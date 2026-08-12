use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::first_authority::{V4FirstAuthorityPublisher, VoidReusableStateReconstructionRequestV1};
use aeordb::engine::v4::gc_void::{
  SweepVoidArtifactV1, VoidClaimExtentV1, VoidClaimWriteV1, VoidExtentRecordV1, decode_sweep_void_artifact, encode_void_claim_v1,
};
use aeordb::engine::v4::gc_void_runtime::{
  VoidReclaimReceiptAuthorityErrorV1, VoidReclaimReceiptAuthorityRequestV1, VoidReclaimReceiptAuthoritySnapshotV1,
  VoidReclaimReceiptAuthorityV1, VoidReusableSpaceStateV1, VoidReusableStateErrorV1, VoidReusableStateIdentityV1,
  VoidReusableStateLimitsV1, VoidReusableStateValidatorV1,
};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(profile: &str, name: &str) -> Vec<u8> {
  fs::read(fixture_root().join(format!("agca-{profile}-{name}.bin"))).unwrap()
}

fn algorithm(profile: &str) -> HashAlgorithm {
  match profile {
    "blake3-256" => HashAlgorithm::Blake3_256,
    "sha512" => HashAlgorithm::Sha512,
    _ => panic!("unexpected fixture profile {profile}"),
  }
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap())
}

fn identity<'a>(manifest_key: &'a [u8], control_key: &'a [u8]) -> VoidReusableStateIdentityV1<'a> {
  VoidReusableStateIdentityV1 {
    selected_manifest_key: manifest_key,
    selected_control_key: control_key,
    selected_control_sequence: 31,
    selected_control_write_sequence: 47,
    selected_control_slot: 1,
  }
}

fn limits(maximum_candidate_extents: u32) -> VoidReusableStateLimitsV1 {
  VoidReusableStateLimitsV1 { maximum_support_artifacts: 16, maximum_outstanding_claim_extents: 16, maximum_candidate_extents }
}

fn as_claim_extents<'a>(extents: &[VoidExtentRecordV1<'a>]) -> Vec<VoidClaimExtentV1<'a>> {
  extents
    .iter()
    .map(|extent| VoidClaimExtentV1 {
      offset: extent.offset,
      length: extent.length,
      origin_sweep_proposal_hash: extent.origin_sweep_proposal_hash,
    })
    .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ReceiptMutation {
  #[default]
  None,
  Fail,
  Identity,
  Range,
  Locator,
  Lineage,
  Search,
  Repair,
  Cancel,
}

#[derive(Default)]
struct ExactReceiptAuthority {
  observations: usize,
  omit_receipt: bool,
  conflicts: u32,
  mutation: ReceiptMutation,
}

impl VoidReclaimReceiptAuthorityV1 for ExactReceiptAuthority {
  fn recheck_void_reclaim_receipt_authority(
    &mut self,
    request: VoidReclaimReceiptAuthorityRequestV1<'_>,
  ) -> Result<VoidReclaimReceiptAuthoritySnapshotV1, VoidReclaimReceiptAuthorityErrorV1> {
    self.observations += 1;
    if self.mutation == ReceiptMutation::Fail {
      return Err(VoidReclaimReceiptAuthorityErrorV1::new("void_runtime_test_authority", "injected receipt authority failure"));
    }
    let mut snapshot = VoidReclaimReceiptAuthoritySnapshotV1 {
      database_id: request.database_id.try_into().unwrap(),
      selected_manifest_key: request.selected_manifest_key.to_vec(),
      selected_generation: request.selected_generation,
      origin_sweep_proposal_hash: request.extent.origin_sweep_proposal_hash.to_vec(),
      origin_quarantine_manifest_hash: request.extent.origin_quarantine_manifest_hash.to_vec(),
      reclaimed_incarnation_digest: request.extent.reclaimed_incarnation_digest.to_vec(),
      proposal_write_sequence: 41,
      receipt_hash: vec![0x91; request.hash_algorithm.hash_length()],
      receipt_write_sequence: 43,
      reclaim_commit_sequence: request.extent.reclaim_commit_sequence,
      receipt_reclaimed_offset: request.extent.offset,
      receipt_reclaimed_length: request.extent.length,
      exact_proposal_receipt_current: !self.omit_receipt,
      locator_removal_durable: true,
      replacement_lineage_complete: true,
      receipt_search_complete: true,
      conflicting_receipt_count: self.conflicts,
      repair_latch_clear: true,
    };
    match self.mutation {
      ReceiptMutation::None | ReceiptMutation::Fail => {}
      ReceiptMutation::Identity => snapshot.selected_generation += 1,
      ReceiptMutation::Range => snapshot.receipt_reclaimed_length = snapshot.receipt_reclaimed_length.saturating_sub(1),
      ReceiptMutation::Locator => snapshot.locator_removal_durable = false,
      ReceiptMutation::Lineage => snapshot.replacement_lineage_complete = false,
      ReceiptMutation::Search => snapshot.receipt_search_complete = false,
      ReceiptMutation::Repair => snapshot.repair_latch_clear = false,
      ReceiptMutation::Cancel => request.cancellation.cancel(),
    }
    Ok(snapshot)
  }
}

#[test]
fn selected_free_space_is_receipt_proven_and_bounded_at_both_hash_widths() {
  for profile in ["blake3-256", "sha512"] {
    let algorithm = algorithm(profile);
    let manifest_bytes = fixture(profile, "void-catalog-source");
    let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(&manifest_bytes, algorithm).unwrap() else {
      panic!("expected source Void catalog")
    };
    let control_key = vec![0x83; algorithm.hash_length()];
    let memory = memory_coordinator();
    let mut authority = ExactReceiptAuthority::default();
    let mut validator = VoidReusableStateValidatorV1::new(
      &manifest,
      algorithm,
      identity(&manifest.key, &control_key),
      CancellationToken::new(),
      limits(1),
      &memory,
    )
    .unwrap();

    validator.finish_claims().unwrap();
    validator.observe_free_encoded(&fixture(profile, "void-extent-page-source"), &mut authority).unwrap();
    validator.observe_free_encoded(&fixture(profile, "void-free-directory-source"), &mut authority).unwrap();
    let state = validator.finish().unwrap();

    assert_eq!(authority.observations as u64, manifest.free_count);
    assert_eq!(state.selected_manifest_key(), manifest.key);
    assert_eq!(state.selected_control_key(), control_key);
    assert_eq!(state.selected_control_sequence(), 31);
    assert_eq!(state.selected_control_write_sequence(), 47);
    assert_eq!(state.selected_control_slot(), 1);
    assert_eq!(state.free_count(), manifest.free_count);
    assert_eq!(state.free_bytes(), manifest.free_bytes);
    assert_eq!(state.outstanding_claim_count(), 0);
    assert_eq!(state.candidate_extents().len(), 1);
    assert!(state.candidate_window_truncated());
  }
}

#[test]
fn outstanding_claims_are_removed_before_free_candidates_are_retained() {
  for profile in ["blake3-256", "sha512"] {
    let algorithm = algorithm(profile);
    let manifest_bytes = fixture(profile, "void-catalog-outstanding");
    let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(&manifest_bytes, algorithm).unwrap() else {
      panic!("expected outstanding-claim Void catalog")
    };
    let control_key = vec![0x84; algorithm.hash_length()];
    let memory = memory_coordinator();
    let mut authority = ExactReceiptAuthority::default();
    let mut validator = VoidReusableStateValidatorV1::new(
      &manifest,
      algorithm,
      identity(&manifest.key, &control_key),
      CancellationToken::new(),
      limits(8),
      &memory,
    )
    .unwrap();

    validator.observe_claim_encoded(&fixture(profile, "void-claim")).unwrap();
    validator.observe_claim_encoded(&fixture(profile, "void-claims-directory")).unwrap();
    validator.finish_claims().unwrap();
    validator.observe_free_encoded(&fixture(profile, "void-extent-page-remaining"), &mut authority).unwrap();
    validator.observe_free_encoded(&fixture(profile, "void-free-directory-remaining"), &mut authority).unwrap();
    let state = validator.finish().unwrap();

    assert_eq!(state.free_count(), manifest.free_count);
    assert_eq!(state.free_bytes(), manifest.free_bytes);
    assert_eq!(state.outstanding_claim_count(), manifest.claim_count);
    assert_eq!(state.claimed_bytes(), manifest.claimed_bytes);
    assert_eq!(state.candidate_extents().len() as u64, manifest.free_count);
    assert!(!state.candidate_window_truncated());
  }
}

#[test]
fn overlapping_claims_and_missing_or_conflicting_receipts_fail_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = fixture("blake3-256", "void-catalog-source");
  let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(&manifest_bytes, algorithm).unwrap() else {
    panic!("expected source Void catalog")
  };
  let page_bytes = fixture("blake3-256", "void-extent-page-source");
  let SweepVoidArtifactV1::VoidExtentPage(page) = decode_sweep_void_artifact(&page_bytes, algorithm).unwrap() else {
    panic!("expected source extent page")
  };
  let first_extent = page.extent_records().unwrap().next().unwrap().unwrap();
  let database_id: [u8; 16] = manifest.database_id.try_into().unwrap();
  let claim_id = [0x11; 16];
  let boot_id = [0x22; 16];
  let task_id = [0x33; 16];
  let claim = encode_void_claim_v1(&VoidClaimWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    claim_id: &claim_id,
    generation: manifest.generation,
    created_at_ms: manifest.published_at_ms,
    requesting_boot_id: &boot_id,
    requesting_task_or_batch_id: &task_id,
    source_manifest_hash: manifest.key.as_slice(),
    extents: &[VoidClaimExtentV1 {
      offset: first_extent.offset,
      length: first_extent.length,
      origin_sweep_proposal_hash: first_extent.origin_sweep_proposal_hash,
    }],
  })
  .unwrap();
  let control_key = vec![0x85; algorithm.hash_length()];
  let memory = memory_coordinator();
  let mut overlap = VoidReusableStateValidatorV1::new(
    &manifest,
    algorithm,
    identity(&manifest.key, &control_key),
    CancellationToken::new(),
    limits(8),
    &memory,
  )
  .unwrap();
  overlap.observe_claim_encoded(&claim.value).unwrap();
  overlap.finish_claims().unwrap();
  let mut authority = ExactReceiptAuthority::default();
  assert_eq!(overlap.observe_free_encoded(&page_bytes, &mut authority).unwrap_err().code(), "void_runtime_claim_free_overlap");
  assert_eq!(authority.observations, 0);

  let receipt_failures = [
    (true, 0, ReceiptMutation::None, "void_runtime_receipt_authority_incomplete"),
    (false, 1, ReceiptMutation::None, "void_runtime_receipt_authority_incomplete"),
    (false, 0, ReceiptMutation::Identity, "void_runtime_receipt_authority_incomplete"),
    (false, 0, ReceiptMutation::Range, "void_runtime_receipt_authority_incomplete"),
    (false, 0, ReceiptMutation::Locator, "void_runtime_receipt_authority_incomplete"),
    (false, 0, ReceiptMutation::Lineage, "void_runtime_receipt_authority_incomplete"),
    (false, 0, ReceiptMutation::Search, "void_runtime_receipt_authority_incomplete"),
    (false, 0, ReceiptMutation::Repair, "void_runtime_receipt_authority_incomplete"),
    (false, 0, ReceiptMutation::Cancel, "void_runtime_canceled"),
    (false, 0, ReceiptMutation::Fail, "void_runtime_test_authority"),
  ];
  for (omit_receipt, conflicts, mutation, expected_code) in receipt_failures {
    let mut validator = VoidReusableStateValidatorV1::new(
      &manifest,
      algorithm,
      identity(&manifest.key, &control_key),
      CancellationToken::new(),
      limits(8),
      &memory,
    )
    .unwrap();
    validator.finish_claims().unwrap();
    let mut authority = ExactReceiptAuthority { omit_receipt, conflicts, mutation, ..Default::default() };
    assert_eq!(validator.observe_free_encoded(&page_bytes, &mut authority).unwrap_err().code(), expected_code);
    assert_eq!(validator.finish().unwrap_err().code(), "void_runtime_failed");
  }
}

#[test]
fn claim_overlap_limits_phase_order_and_malformed_support_latch_failure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let manifest_bytes = fixture("blake3-256", "void-catalog-source");
  let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(&manifest_bytes, algorithm).unwrap() else {
    panic!("expected source Void catalog")
  };
  let page_bytes = fixture("blake3-256", "void-extent-page-source");
  let SweepVoidArtifactV1::VoidExtentPage(page) = decode_sweep_void_artifact(&page_bytes, algorithm).unwrap() else {
    panic!("expected source extent page")
  };
  let page_extents = page.extent_records().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
  assert!(page_extents.len() >= 2);
  let database_id: [u8; 16] = manifest.database_id.try_into().unwrap();
  let boot_id = [0x22; 16];
  let task_id = [0x33; 16];
  let encode_claim = |claim_id: &[u8; 16], extents: &[VoidClaimExtentV1<'_>]| {
    encode_void_claim_v1(&VoidClaimWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      claim_id,
      generation: manifest.generation,
      created_at_ms: manifest.published_at_ms,
      requesting_boot_id: &boot_id,
      requesting_task_or_batch_id: &task_id,
      source_manifest_hash: manifest.key.as_slice(),
      extents,
    })
    .unwrap()
  };
  let first_claim_extents = as_claim_extents(&page_extents[..1]);
  let first_claim = encode_claim(&[0x10; 16], &first_claim_extents);
  let second_claim = encode_claim(&[0x11; 16], &first_claim_extents);
  let control_key = vec![0x87; algorithm.hash_length()];
  let memory = memory_coordinator();
  let mut overlap = VoidReusableStateValidatorV1::new(
    &manifest,
    algorithm,
    identity(&manifest.key, &control_key),
    CancellationToken::new(),
    limits(8),
    &memory,
  )
  .unwrap();
  overlap.observe_claim_encoded(&first_claim.value).unwrap();
  overlap.observe_claim_encoded(&second_claim.value).unwrap();
  assert_eq!(overlap.finish_claims().unwrap_err().code(), "void_runtime_claim_overlap");
  assert_eq!(overlap.finish_claims().unwrap_err().code(), "void_runtime_failed");

  let two_claim_extents = as_claim_extents(&page_extents[..2]);
  let oversized_claim = encode_claim(&[0x12; 16], &two_claim_extents);
  let mut limited = VoidReusableStateValidatorV1::new(
    &manifest,
    algorithm,
    identity(&manifest.key, &control_key),
    CancellationToken::new(),
    VoidReusableStateLimitsV1 { maximum_support_artifacts: 8, maximum_outstanding_claim_extents: 1, maximum_candidate_extents: 8 },
    &memory,
  )
  .unwrap();
  assert_eq!(limited.observe_claim_encoded(&oversized_claim.value).unwrap_err().code(), "void_runtime_claim_extent_limit");
  assert_eq!(limited.finish_claims().unwrap_err().code(), "void_runtime_failed");

  let mut wrong_phase = VoidReusableStateValidatorV1::new(
    &manifest,
    algorithm,
    identity(&manifest.key, &control_key),
    CancellationToken::new(),
    limits(8),
    &memory,
  )
  .unwrap();
  let mut authority = ExactReceiptAuthority::default();
  assert_eq!(wrong_phase.observe_free_encoded(&page_bytes, &mut authority).unwrap_err().code(), "void_runtime_phase");
  assert_eq!(wrong_phase.finish_claims().unwrap_err().code(), "void_runtime_failed");
  assert_eq!(authority.observations, 0);

  let mut malformed = VoidReusableStateValidatorV1::new(
    &manifest,
    algorithm,
    identity(&manifest.key, &control_key),
    CancellationToken::new(),
    limits(8),
    &memory,
  )
  .unwrap();
  assert!(malformed.observe_claim_encoded(b"not a GC artifact").is_err());
  assert_eq!(malformed.finish_claims().unwrap_err().code(), "void_runtime_failed");
}

#[test]
fn empty_canceled_and_resource_refused_reconstruction_never_grants_candidates() {
  let algorithm = HashAlgorithm::Blake3_256;
  let empty_bytes = fixture("blake3-256", "void-catalog-empty");
  let SweepVoidArtifactV1::VoidCatalog(empty) = decode_sweep_void_artifact(&empty_bytes, algorithm).unwrap() else {
    panic!("expected empty Void catalog")
  };
  let control_key = vec![0x86; algorithm.hash_length()];
  let memory = memory_coordinator();
  let mut empty_validator =
    VoidReusableStateValidatorV1::new(&empty, algorithm, identity(&empty.key, &control_key), CancellationToken::new(), limits(1), &memory)
      .unwrap();
  empty_validator.finish_claims().unwrap();
  let state = empty_validator.finish().unwrap();
  assert!(state.candidate_extents().is_empty());
  assert!(!state.candidate_window_truncated());
  drop(state);
  assert_eq!(
    memory.snapshot().unwrap().owner(aeordb::engine::memory_coordinator::MemoryOwner::GarbageCollection).unwrap().reserved_bytes,
    0
  );

  let cancellation = CancellationToken::new();
  cancellation.cancel();
  assert_eq!(
    VoidReusableStateValidatorV1::new(&empty, algorithm, identity(&empty.key, &control_key), cancellation, limits(1), &memory,)
      .unwrap_err()
      .code(),
    "void_runtime_canceled"
  );

  assert_eq!(
    VoidReusableStateValidatorV1::new(
      &empty,
      algorithm,
      identity(&empty.key, &control_key),
      CancellationToken::new(),
      VoidReusableStateLimitsV1 { maximum_support_artifacts: 0, maximum_outstanding_claim_extents: 1, maximum_candidate_extents: 1 },
      &memory,
    )
    .unwrap_err()
    .code(),
    "void_runtime_limits"
  );

  let constrained_memory = MemoryCoordinator::new(MemoryPolicy::new(128, 192, 1, 64).unwrap());
  assert_eq!(
    VoidReusableStateValidatorV1::new(
      &empty,
      algorithm,
      identity(&empty.key, &control_key),
      CancellationToken::new(),
      limits(16),
      &constrained_memory,
    )
    .unwrap_err()
    .code(),
    "void_runtime_memory"
  );
  assert_eq!(
    constrained_memory
      .snapshot()
      .unwrap()
      .owner(aeordb::engine::memory_coordinator::MemoryOwner::GarbageCollection)
      .unwrap()
      .reserved_bytes,
    0
  );
}

fn reconstruct_selected_void_state(
  publisher: &V4FirstAuthorityPublisher,
  request: VoidReusableStateReconstructionRequestV1<'_>,
  authority: &mut dyn VoidReclaimReceiptAuthorityV1,
) -> Result<Option<VoidReusableSpaceStateV1>, VoidReusableStateErrorV1> {
  publisher.reconstruct_void_reusable_state(request, authority)
}

#[test]
fn restart_reconstruction_has_one_disconnected_first_authority_owner() {
  let pointer: fn(
    &V4FirstAuthorityPublisher,
    VoidReusableStateReconstructionRequestV1<'_>,
    &mut dyn VoidReclaimReceiptAuthorityV1,
  ) -> Result<Option<VoidReusableSpaceStateV1>, VoidReusableStateErrorV1> = reconstruct_selected_void_state;
  assert_eq!(std::mem::size_of_val(&pointer), std::mem::size_of::<usize>());

  let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine/v4/first_authority.rs")).unwrap();
  let method_start = source.find("pub fn reconstruct_void_reusable_state(").unwrap();
  let method_tail = &source[method_start..];
  let method_end = method_tail.find("\n  ///").unwrap_or(method_tail.len());
  let method = &method_tail[..method_end];
  for required in ["select_void_catalog_control", "VoidCatalogSupportReadContextV1", "finish_claims"] {
    assert!(method.contains(required), "restart owner must call {required}");
  }
  for forbidden in ["VoidManager", "replace_all", "find_void", "hot_tail", "gap", "run_gc", "server::", "StorageEngine"] {
    assert!(!method.contains(forbidden), "restart owner must not consult live v3 {forbidden}");
  }
}
