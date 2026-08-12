use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::gc::PhysicalIncarnationV1;
use aeordb::engine::v4::gc_quarantine::{
  CandidateDeltaRecordWriteV1, CandidateDeltaWriteV1, PhysicalQuarantineCandidateClassV1, QuarantineClosureLimitsV1,
  QuarantineClosureValidatorV1, QuarantineManifestWriteV1, decode_quarantine_manifest_v1, encode_candidate_delta_v1,
  encode_quarantine_manifest_v1,
};
use aeordb::engine::v4::gc_quarantine_publication::{
  PhysicalQuarantinePublicationQualificationRequestV1, qualify_physical_quarantine_publication_v1,
};
use aeordb::engine::v4::gc_quarantine_transition::{
  PhysicalQuarantineObservationV1, PhysicalQuarantineReachabilityV1, PhysicalQuarantineTransitionContextV1,
  PhysicalQuarantineTransitionModelV1, PhysicalQuarantineTransitionV1,
};
use aeordb::engine::v4::gc_state::{GcStateArtifactV1, GcStateManifestV1, decode_gc_state_artifact};
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40];

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("publication qualification covers both frozen GC hash widths"),
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

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap())
}

fn lifecycle_bytes(algorithm: HashAlgorithm) -> Vec<u8> {
  fs::read(fixture_root().join(format!("agca-{}-root-lifecycle-manifest-populated.bin", algorithm_name(algorithm)))).unwrap()
}

fn lifecycle_manifest(bytes: &[u8], algorithm: HashAlgorithm) -> GcStateManifestV1<'_> {
  let GcStateArtifactV1::Manifest(manifest) = decode_gc_state_artifact(bytes, algorithm).unwrap() else {
    panic!("the lifecycle fixture must decode as a manifest")
  };
  manifest
}

struct BasisV1 {
  authority: Vec<u8>,
  semantic: Vec<u8>,
  layout: Vec<u8>,
  mark: Vec<u8>,
}

impl BasisV1 {
  fn new(algorithm: HashAlgorithm) -> Self {
    let width = algorithm.hash_length();
    Self { authority: sequence(width, 0x51), semantic: sequence(width, 0x71), layout: sequence(width, 0x91), mark: sequence(width, 0xb1) }
  }
}

fn manifest_bytes(
  algorithm: HashAlgorithm,
  generation: u64,
  completed_at_ms: u64,
  basis: &BasisV1,
  lifecycle_hash: &[u8],
  candidate_count: u64,
  eligible_count: u64,
  delta_hashes: &[u8],
) -> Vec<u8> {
  let required_capabilities = capabilities();
  let record_bytes = u64::try_from(52 + 2 * algorithm.hash_length()).unwrap();
  encode_quarantine_manifest_v1(&QuarantineManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id: DATABASE_ID,
    mark_generation: generation,
    completed_at_ms,
    required_capabilities: &required_capabilities,
    authority_root_set_digest: &basis.authority,
    semantic_state_digest: &basis.semantic,
    kv_layout_fingerprint: &basis.layout,
    mark_result_digest: &basis.mark,
    candidate_directory_root: None,
    captured_root_lifecycle_manifest: lifecycle_hash,
    candidate_count,
    candidate_bytes: candidate_count * record_bytes,
    eligible_count_hint: eligible_count,
    eligible_bytes_hint: eligible_count * record_bytes,
    next_candidate_page_id: 1,
    delta_hashes,
  })
  .unwrap()
  .value
}

#[test]
fn completed_transition_qualifies_only_its_exact_incremental_delta_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let lifecycle_bytes = lifecycle_bytes(algorithm);
    let lifecycle = lifecycle_manifest(&lifecycle_bytes, algorithm);
    let prior_basis = BasisV1::new(algorithm);
    let prior_bytes = manifest_bytes(algorithm, 100, 1_000, &prior_basis, &lifecycle.key, 0, 0, &[]);
    let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
    let next_basis = BasisV1 {
      authority: sequence(algorithm.hash_length(), 0x52),
      semantic: sequence(algorithm.hash_length(), 0x72),
      layout: sequence(algorithm.hash_length(), 0x92),
      mark: sequence(algorithm.hash_length(), 0xb2),
    };
    let cancellation = CancellationToken::new();
    let mut model = PhysicalQuarantineTransitionModelV1::new(
      PhysicalQuarantineTransitionContextV1 {
        hash_algorithm: algorithm,
        prior_manifest: &prior,
        mark_generation: 101,
        completed_at_ms: 2_000,
        current_configured_grace_ms: 60_000,
        authority_root_set_digest: &next_basis.authority,
        semantic_state_digest: &next_basis.semantic,
        kv_layout_fingerprint: &next_basis.layout,
        mark_result_digest: &next_basis.mark,
        captured_root_lifecycle_manifest: &lifecycle.key,
        maximum_incarnations: 1,
        maximum_candidates: 1,
        mark_complete: true,
        destructive_gc_enabled: true,
        mark_authority_healthy: true,
        physical_inventory_healthy: true,
        root_lifecycle_healthy: true,
      },
      &cancellation,
    )
    .unwrap();
    let logical_key = sequence(algorithm.hash_length(), 0x11);
    let integrity = sequence(algorithm.hash_length(), 0x31);
    let transition = model
      .observe(PhysicalQuarantineObservationV1 {
        incarnation: PhysicalIncarnationV1 {
          logical_key: &logical_key,
          integrity_or_legacy_digest: &integrity,
          wal_offset: 4_096,
          write_sequence: 7,
          entity_length: 512,
          entry_type: 1,
          entity_version: 1,
        },
        prior_candidate: None,
        reachability: PhysicalQuarantineReachabilityV1::ConfirmedUnreachable {
          class: PhysicalQuarantineCandidateClassV1::RetiredLowerIncarnation,
        },
      })
      .unwrap();
    let PhysicalQuarantineTransitionV1::CandidateStarted(candidate) = transition else {
      panic!("the first unreachable mark must emit one candidate set")
    };
    let records = [candidate.as_delta_write_request()];
    let delta = encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
      hash_algorithm: algorithm,
      database_id: DATABASE_ID,
      mark_generation: 101,
      delta_ordinal: 1,
      previous_delta_hash: None,
      records: &records,
    })
    .unwrap();
    let transition_permit = model.finish_for_publication().unwrap();
    let next_bytes = manifest_bytes(algorithm, 101, 2_000, &next_basis, &lifecycle.key, 1, 0, &delta.key);
    let next = decode_quarantine_manifest_v1(&next_bytes, algorithm).unwrap();
    let mut closure = QuarantineClosureValidatorV1::new(
      &next,
      None,
      &lifecycle,
      algorithm,
      cancellation.clone(),
      QuarantineClosureLimitsV1 { maximum_support_artifacts: 4 },
      &memory_coordinator(),
    )
    .unwrap();
    closure.observe_delta(&delta.value).unwrap();
    let closure = closure.finish().unwrap();
    let permit = qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
      prior_manifest: &prior,
      next_manifest: &next,
      support_closure: &closure,
      transition: &transition_permit,
      appended_delta: Some(&delta.value),
      cancellation: &cancellation,
    })
    .unwrap();
    assert_eq!(permit.next_manifest_hash(), next.key);
    assert_eq!(permit.prior_manifest_hash(), prior.key);
    assert_eq!((permit.mutation_count(), permit.resulting_candidate_count(), permit.eligible_count()), (1, 1, 0));

    let substituted_record = CandidateDeltaRecordWriteV1 {
      operation: records[0].operation,
      candidate: aeordb::engine::v4::gc_quarantine::PhysicalQuarantineCandidateWriteV1 {
        class: PhysicalQuarantineCandidateClassV1::ExpiredDerivedArtifact,
        ..records[0].candidate
      },
    };
    let substituted = encode_candidate_delta_v1(&CandidateDeltaWriteV1 {
      hash_algorithm: algorithm,
      database_id: DATABASE_ID,
      mark_generation: 101,
      delta_ordinal: 1,
      previous_delta_hash: None,
      records: &[substituted_record],
    })
    .unwrap();
    let substituted_manifest_bytes = manifest_bytes(algorithm, 101, 2_000, &next_basis, &lifecycle.key, 1, 0, &substituted.key);
    let substituted_manifest = decode_quarantine_manifest_v1(&substituted_manifest_bytes, algorithm).unwrap();
    let mut substituted_closure = QuarantineClosureValidatorV1::new(
      &substituted_manifest,
      None,
      &lifecycle,
      algorithm,
      cancellation.clone(),
      QuarantineClosureLimitsV1 { maximum_support_artifacts: 4 },
      &memory_coordinator(),
    )
    .unwrap();
    substituted_closure.observe_delta(&substituted.value).unwrap();
    let substituted_closure = substituted_closure.finish().unwrap();
    assert_eq!(
      qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
        prior_manifest: &prior,
        next_manifest: &substituted_manifest,
        support_closure: &substituted_closure,
        transition: &transition_permit,
        appended_delta: Some(&substituted.value),
        cancellation: &cancellation,
      })
      .unwrap_err()
      .code(),
      "quarantine_publication_mutations",
    );
  }
}

#[test]
fn omitted_delta_aggregate_drift_and_cancellation_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let lifecycle_bytes = lifecycle_bytes(algorithm);
  let lifecycle = lifecycle_manifest(&lifecycle_bytes, algorithm);
  let basis = BasisV1::new(algorithm);
  let prior_bytes = manifest_bytes(algorithm, 200, 1_000, &basis, &lifecycle.key, 0, 0, &[]);
  let prior = decode_quarantine_manifest_v1(&prior_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let model = PhysicalQuarantineTransitionModelV1::new(
    PhysicalQuarantineTransitionContextV1 {
      hash_algorithm: algorithm,
      prior_manifest: &prior,
      mark_generation: 201,
      completed_at_ms: 2_000,
      current_configured_grace_ms: 0,
      authority_root_set_digest: &basis.authority,
      semantic_state_digest: &basis.semantic,
      kv_layout_fingerprint: &basis.layout,
      mark_result_digest: &basis.mark,
      captured_root_lifecycle_manifest: &lifecycle.key,
      maximum_incarnations: 1,
      maximum_candidates: 1,
      mark_complete: true,
      destructive_gc_enabled: true,
      mark_authority_healthy: true,
      physical_inventory_healthy: true,
      root_lifecycle_healthy: true,
    },
    &cancellation,
  )
  .unwrap();
  let transition = model.finish_for_publication().unwrap();
  let next_bytes = manifest_bytes(algorithm, 201, 2_000, &basis, &lifecycle.key, 0, 0, &[]);
  let mut next = decode_quarantine_manifest_v1(&next_bytes, algorithm).unwrap();
  let closure = QuarantineClosureValidatorV1::new(
    &next,
    None,
    &lifecycle,
    algorithm,
    cancellation.clone(),
    QuarantineClosureLimitsV1 { maximum_support_artifacts: 1 },
    &memory_coordinator(),
  )
  .unwrap()
  .finish()
  .unwrap();
  next.candidate_count = 1;
  assert_eq!(
    qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
      prior_manifest: &prior,
      next_manifest: &next,
      support_closure: &closure,
      transition: &transition,
      appended_delta: None,
      cancellation: &cancellation,
    })
    .unwrap_err()
    .code(),
    "quarantine_publication_closure",
  );

  let next = decode_quarantine_manifest_v1(&next_bytes, algorithm).unwrap();
  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    qualify_physical_quarantine_publication_v1(PhysicalQuarantinePublicationQualificationRequestV1 {
      prior_manifest: &prior,
      next_manifest: &next,
      support_closure: &closure,
      transition: &transition,
      appended_delta: None,
      cancellation: &canceled,
    })
    .unwrap_err()
    .code(),
    "quarantine_publication_canceled",
  );
}
