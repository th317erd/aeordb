use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::first_authority::{
  SweepProposalHardPublicationReceiptV1, SweepProposalHardPublicationRequestV1, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_quarantine::{
  PhysicalQuarantineCandidateClassV1, QuarantineClosureLimitsV1, QuarantineClosureValidatorV1, QuarantineManifestWriteV1,
  decode_quarantine_manifest_v1, encode_quarantine_manifest_v1, quarantine_candidate_records_v1,
};
use aeordb::engine::v4::gc_quarantine_publication::{
  PhysicalQuarantinePublicationQualificationRequestV1, qualify_physical_quarantine_publication_v1,
};
use aeordb::engine::v4::gc_quarantine_transition::{
  PhysicalQuarantineObservationV1, PhysicalQuarantineReachabilityV1, PhysicalQuarantineTransitionContextV1,
  PhysicalQuarantineTransitionModelV1, PhysicalQuarantineTransitionV1,
};
use aeordb::engine::v4::gc_state::{GcStateArtifactV1, GcStateManifestV1, decode_gc_state_artifact};
use aeordb::engine::v4::gc_sweep::{SweepProposalQualificationRequestV1, qualify_sweep_proposal_v1};
use aeordb::engine::v4::gc_void::{SweepVoidArtifactV1, decode_sweep_void_artifact};
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40];

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn rust_sources(root: &Path, sources: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(root).unwrap() {
    let entry = entry.unwrap();
    if entry.file_type().unwrap().is_dir() {
      rust_sources(&entry.path(), sources);
    } else if entry.path().extension().and_then(|extension| extension.to_str()) == Some("rs") {
      sources.push(entry.path());
    }
  }
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("sweep publication proof covers both frozen GC hash widths"),
  }
}

fn sequence(length: usize, start: u8) -> Vec<u8> {
  (0..length).map(|index| start.wrapping_add(index as u8)).collect()
}

fn capabilities() -> [u8; 32] {
  let mut value = [0u8; 32];
  for capability in [12usize, 13, 15, 17] {
    value[capability / 8] |= 1 << (capability % 8);
  }
  value
}

fn lifecycle_manifest(bytes: &[u8], algorithm: HashAlgorithm) -> GcStateManifestV1<'_> {
  let GcStateArtifactV1::Manifest(manifest) = decode_gc_state_artifact(bytes, algorithm).unwrap() else {
    panic!("the lifecycle fixture must decode as a manifest")
  };
  manifest
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap())
}

#[allow(dead_code)]
fn assert_restricted_hard_publisher_contract(
  publisher: &V4FirstAuthorityPublisher,
  request: SweepProposalHardPublicationRequestV1<'_>,
) -> Result<SweepProposalHardPublicationReceiptV1, aeordb::engine::v4::first_authority::SweepProposalHardPublicationErrorV1> {
  publisher.publish_sweep_proposal(request)
}

struct BasisV1 {
  authority: Vec<u8>,
  semantic: Vec<u8>,
  layout: Vec<u8>,
  mark: Vec<u8>,
}

impl BasisV1 {
  fn new(algorithm: HashAlgorithm, seed: u8) -> Self {
    let width = algorithm.hash_length();
    Self {
      authority: sequence(width, seed),
      semantic: sequence(width, seed.wrapping_add(0x20)),
      layout: sequence(width, seed.wrapping_add(0x40)),
      mark: sequence(width, seed.wrapping_add(0x60)),
    }
  }
}

fn manifest_bytes(
  algorithm: HashAlgorithm,
  generation: u64,
  completed_at_ms: u64,
  basis: &BasisV1,
  lifecycle_hash: &[u8],
  candidate_directory_root: &[u8],
  next_candidate_page_id: u64,
  candidate_count: u64,
  eligible_count: u64,
) -> Vec<u8> {
  let record_bytes = u64::try_from(52 + 2 * algorithm.hash_length()).unwrap();
  encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: DATABASE_ID,
    mark_generation: generation,
    completed_at_ms,
    required_capabilities: &capabilities(),
    authority_root_set_digest: &basis.authority,
    semantic_state_digest: &basis.semantic,
    kv_layout_fingerprint: &basis.layout,
    mark_result_digest: &basis.mark,
    candidate_directory_root: Some(candidate_directory_root),
    captured_root_lifecycle_manifest: lifecycle_hash,
    candidate_count,
    candidate_bytes: candidate_count * record_bytes,
    eligible_count_hint: eligible_count,
    eligible_bytes_hint: eligible_count * record_bytes,
    next_candidate_page_id,
    delta_hashes: &[],
  })
  .unwrap()
  .value
}

#[test]
fn exact_eligible_intents_qualify_one_bounded_proposal_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let lifecycle_bytes =
      fs::read(fixture_root().join(format!("agca-{}-root-lifecycle-manifest-populated.bin", algorithm_name(algorithm)))).unwrap();
    let lifecycle = lifecycle_manifest(&lifecycle_bytes, algorithm);
    let page_bytes = fs::read(fixture_root().join(format!("agca-{}-candidate-page-valid.bin", algorithm_name(algorithm)))).unwrap();
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(&page_bytes, algorithm).unwrap() else {
      panic!("the candidate fixture must decode as a page")
    };
    let directory_bytes =
      fs::read(fixture_root().join(format!("agca-{}-candidates-directory-valid.bin", algorithm_name(algorithm)))).unwrap();
    let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(&directory_bytes, algorithm).unwrap() else {
      panic!("the candidate directory fixture must decode as a directory")
    };
    let candidates = quarantine_candidate_records_v1(&page, algorithm).unwrap().map(Result::unwrap).collect::<Vec<_>>();
    assert_eq!(candidates.len(), 2);
    let prior_basis = BasisV1::new(algorithm, 0x51);
    let next_candidate_page_id = directory.maximum_page_id + 1;
    let prior_bytes = manifest_bytes(algorithm, 100, 1_000, &prior_basis, &lifecycle.key, &directory.key, next_candidate_page_id, 2, 0);
    let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
    let next_basis = BasisV1::new(algorithm, 0x52);
    let cancellation = CancellationToken::new();
    let completed_at_ms = candidates
      .iter()
      .map(|candidate| candidate.pending_since_ms.checked_add(candidate.grace_at_pending_ms).unwrap())
      .max()
      .unwrap()
      .max(2_000);
    let mut transition = PhysicalQuarantineTransitionModelV1::new(
      PhysicalQuarantineTransitionContextV1 {
        hash_algorithm: algorithm,
        prior_manifest: &prior,
        mark_generation: 101,
        completed_at_ms,
        current_configured_grace_ms: 0,
        authority_root_set_digest: &next_basis.authority,
        semantic_state_digest: &next_basis.semantic,
        kv_layout_fingerprint: &next_basis.layout,
        mark_result_digest: &next_basis.mark,
        captured_root_lifecycle_manifest: &lifecycle.key,
        maximum_incarnations: 2,
        maximum_candidates: 2,
        mark_complete: true,
        destructive_gc_enabled: true,
        mark_authority_healthy: true,
        physical_inventory_healthy: true,
        root_lifecycle_healthy: true,
      },
      &cancellation,
    )
    .unwrap();
    let mut intents = Vec::new();
    for candidate in &candidates {
      let PhysicalQuarantineTransitionV1::SweepEligible(intent) = transition
        .observe(PhysicalQuarantineObservationV1 {
          incarnation: candidate.incarnation,
          prior_candidate: Some(candidate),
          reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable { class: candidate.class },
        })
        .unwrap()
      else {
        panic!("the second complete mark must emit one exact sweep intent")
      };
      intents.push(intent);
    }
    let transition = transition.finish_for_publication().unwrap();
    assert_ne!(transition.eligible_intent_digest(), vec![0; algorithm.hash_length()]);

    let next_bytes =
      manifest_bytes(algorithm, 101, completed_at_ms, &next_basis, &lifecycle.key, &directory.key, next_candidate_page_id, 2, 2);
    let next = decode_quarantine_manifest_v1(&next_bytes, algorithm).unwrap();
    let mut closure = QuarantineClosureValidatorV1::new(
      &next,
      Some(&directory),
      &lifecycle,
      algorithm,
      cancellation.clone(),
      QuarantineClosureLimitsV1 { maximum_support_artifacts: 2 },
      &memory_coordinator(),
    )
    .unwrap();
    closure.observe_base_page(&page).unwrap();
    let closure = closure.finish().unwrap();
    let quarantine = qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
      prior_manifest: &prior,
      next_manifest: &next,
      support_closure: &closure,
      transition: &transition,
      appended_delta: None,
      cancellation: &cancellation,
    })
    .unwrap();
    assert_eq!(quarantine.eligible_intent_digest(), transition.eligible_intent_digest());

    let batch_id = [0x71; 16];
    let created_at_ms = i64::try_from(completed_at_ms + 1).unwrap();
    let permit = qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
      quarantine_publication: &quarantine,
      quarantine_manifest: &next,
      batch_id: &batch_id,
      created_at_ms,
      intents: &intents,
      cancellation: &cancellation,
    })
    .unwrap();
    let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(&permit.proposal().value, algorithm).unwrap() else {
      panic!("qualified bytes must be one sweep proposal")
    };
    assert_eq!(proposal.key, permit.proposal().key);
    assert_eq!(proposal.quarantine_manifest_hash, next.key);
    assert_eq!(proposal.candidate_count, 2);

    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &[0; 16],
        created_at_ms,
        intents: &intents,
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "sweep_proposal_identity",
    );
    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &batch_id,
        created_at_ms: -1,
        intents: &intents,
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "sweep_proposal_time",
    );
    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &batch_id,
        created_at_ms: i64::try_from(completed_at_ms - 1).unwrap(),
        intents: &intents,
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "sweep_proposal_identity",
    );
    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &batch_id,
        created_at_ms,
        intents: &[],
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "sweep_proposal_limit",
    );

    let mut malformed = intents.clone();
    malformed[0].candidate.incarnation.logical_key.pop();
    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &batch_id,
        created_at_ms,
        intents: &malformed,
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "physical_incarnation_length",
    );

    let mut substituted = intents.clone();
    let original_class = substituted[0].candidate.class;
    substituted[0].candidate.class = if original_class == PhysicalQuarantineCandidateClassV1::ExpiredDerivedArtifact {
      PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation
    } else {
      PhysicalQuarantineCandidateClassV1::ExpiredDerivedArtifact
    };
    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &batch_id,
        created_at_ms,
        intents: &substituted,
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "sweep_proposal_intent_digest",
    );

    let mut reversed = intents.clone();
    reversed.reverse();
    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &batch_id,
        created_at_ms,
        intents: &reversed,
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "sweep_proposal_order",
    );

    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &batch_id,
        created_at_ms,
        intents: &intents[..1],
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "sweep_proposal_aggregate",
    );

    let mut stale = intents.clone();
    stale[0].confirmed_at_ms += 1;
    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &batch_id,
        created_at_ms,
        intents: &stale,
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "sweep_proposal_intent",
    );

    let oversized = vec![intents[0].clone(); 4_097];
    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &batch_id,
        created_at_ms,
        intents: &oversized,
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "sweep_proposal_limit",
    );

    let canceled = CancellationToken::new();
    canceled.cancel();
    assert_eq!(
      qualify_sweep_proposal_v1(SweepProposalQualificationRequestV1 {
        quarantine_publication: &quarantine,
        quarantine_manifest: &next,
        batch_id: &batch_id,
        created_at_ms,
        intents: &intents,
        cancellation: &canceled,
      })
      .unwrap_err()
      .code(),
      "sweep_proposal_canceled",
    );
  }
}

#[test]
fn sweep_proposal_qualification_and_publication_remain_disconnected_from_removal_authority() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let qualifier_source = fs::read_to_string(source_root.join("engine/v4/gc_sweep.rs")).unwrap();
  for forbidden in ["VoidManager", "remove_entry", "remove_locator", "run_gc", "server::", "DirectoryOps", "StorageEngine"] {
    assert!(!qualifier_source.contains(forbidden), "proposal qualification unexpectedly references {forbidden}");
  }

  let mut sources = Vec::new();
  rust_sources(&source_root, &mut sources);
  let mut publisher_owners =
    sources.iter().filter(|path| fs::read_to_string(path).unwrap().contains("pub fn publish_sweep_proposal(")).cloned().collect::<Vec<_>>();
  publisher_owners.sort();
  assert_eq!(publisher_owners, vec![source_root.join("engine/v4/first_authority.rs")]);
  let authority_source = fs::read_to_string(&publisher_owners[0]).unwrap();
  let method_start = authority_source.find("pub fn publish_sweep_proposal(").unwrap();
  let method_end = authority_source[method_start..].find("/// Hard-publish one immutable page or directory").unwrap() + method_start;
  let method = &authority_source[method_start..method_end];
  assert!(method.contains("publish_immutable_gc_artifact_locked"));
  for forbidden in ["VoidManager", "remove_entry", "remove_locator", "run_gc", "DirectoryOps", "StorageEngine"] {
    assert!(!method.contains(forbidden), "proposal publisher unexpectedly references {forbidden}");
  }
}
