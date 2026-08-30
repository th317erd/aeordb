use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc_lifecycle::{
  RootExpiryManifestWriteV1, RootExpiryRecordWriteV1, RootLifecycleManifestWriteV1, RootLifecycleSupportClosureBuilderV1,
  RootLifecycleSupportLimitsV1, RootRetirementCommitWriteV1, decode_root_expiry_manifest_v1, decode_root_lifecycle_manifest_v1,
  decode_root_object_reclaim_proof_v1, decode_root_retirement_commit_v1, encode_root_expiry_manifest_v1, encode_root_expiry_record_v1,
  encode_root_lifecycle_manifest_v1, encode_root_retirement_commit_v1, validate_root_expiry_reclaim_proof,
};
use aeordb::engine::v4::gc_root_reclaim::{
  RootExpiryRetentionActionV1, RootExpiryRetentionContextV1, RootExpiryRetentionCutoffV1, RootExpiryRetentionModelV1,
  RootExpiryRetentionSelectionV1, RootObjectReclaimEvidenceVerificationErrorV1, RootObjectReclaimEvidenceVerificationRequestV1,
  RootObjectReclaimEvidenceVerifierV1, RootObjectReclaimQualificationRequestV1, qualify_root_object_reclaim_v1,
};
use aeordb::engine::v4::gc_lifecycle::{RootExpiryManifestV1, RootLifecycleManifestV1};
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::gc_state::{
  GcDirectoryRoleV1, GcPhysicalHintV1, GcStateDirectoryEntryWriteV1, GcStateDirectoryWriteV1, GcStatePageWriteV1, RootExpiryRecordV1,
  RootExpiryStateV1, decode_gc_state_artifact, decode_physical_inventory_manifest_v1, decode_root_expiry_record_v1,
  encode_gc_state_directory_v1, encode_gc_state_page_v1,
};
use aeordb::engine::v4::hash::digest_parts;
use tokio_util::sync::CancellationToken;

const RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(algorithm: HashAlgorithm, name: &str) -> Vec<u8> {
  let algorithm_name = match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("root reclaim proof covers both frozen hash widths"),
  };
  fs::read(fixture_root().join(format!("agca-{algorithm_name}-{name}.bin"))).unwrap()
}

struct RecordingVerifier {
  calls: usize,
  fail: bool,
  cancel_after_verify: Option<CancellationToken>,
}

impl RootObjectReclaimEvidenceVerifierV1 for RecordingVerifier {
  fn verify_root_object_reclaim(
    &mut self,
    request: RootObjectReclaimEvidenceVerificationRequestV1<'_>,
  ) -> Result<(), RootObjectReclaimEvidenceVerificationErrorV1> {
    self.calls += 1;
    assert!(request.final_physical_inventory_generation > request.latest_sweep_receipt_generation);
    assert_eq!(request.root_object_incarnation_count, 2);
    assert_eq!(request.sweep_receipt_count, 3);
    if self.fail {
      return Err(RootObjectReclaimEvidenceVerificationErrorV1::new("test_receipt_gap", "a required sweep receipt is absent"));
    }
    if let Some(cancellation) = &self.cancel_after_verify {
      cancellation.cancel();
    }
    Ok(())
  }
}

struct ReclaimCase {
  inventory_bytes: Vec<u8>,
  retirement_bytes: Vec<u8>,
  prior_expiry_bytes: Vec<u8>,
  proof_id: [u8; 16],
  root_incarnation_digest: Vec<u8>,
  sweep_receipt_merkle_root: Vec<u8>,
  absence_digest: Vec<u8>,
  reclaimed_at_ms: i64,
}

struct ReclaimSupport {
  expiry_page: aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1,
  expiry_directory: aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1,
  expiry_manifest: aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1,
  lifecycle_manifest: aeordb::engine::v4::gc::EncodedImmutableGcArtifactV1,
}

fn reclaim_support(
  algorithm: HashAlgorithm,
  database_id: &[u8],
  authority_root_set_digest: &[u8],
  generation: u64,
  published_at_ms: i64,
  retention: (u64, u64),
  expiry_record: &[u8],
) -> ReclaimSupport {
  let (retention_ms, optional_byte_budget) = retention;
  let hash_width = algorithm.hash_length();
  let decoded_record = decode_root_expiry_record_v1(expiry_record, algorithm).unwrap();
  let catalog_id = [0xb1; 16];
  let expiry_page = encode_gc_state_page_v1(&GcStatePageWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id,
    catalog_id: &catalog_id,
    generation,
    page_id: 1,
    records: &[expiry_record],
  })
  .unwrap();
  let page = match decode_gc_state_artifact(&expiry_page.value, algorithm).unwrap() {
    aeordb::engine::v4::gc_state::GcStateArtifactV1::Page(page) => page,
    _ => unreachable!(),
  };
  let directory_entries = [GcStateDirectoryEntryWriteV1 {
    lower_fence: page.lower_fence,
    upper_fence: page.upper_fence,
    child_hash: &expiry_page.key,
    child_generation: page.generation,
    live_count: u64::from(page.record_count),
    tombstone_count: 0,
    page_count: 1,
    logical_bytes: page.logical_bytes,
    minimum_page_id: page.page_id,
    maximum_page_id: page.page_id,
    physical_hint: GcPhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
  }];
  let expiry_directory = encode_gc_state_directory_v1(&GcStateDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role: GcDirectoryRoleV1::RootExpiry,
    database_id,
    catalog_id: &catalog_id,
    generation,
    level: 0,
    entries: &directory_entries,
  })
  .unwrap();
  let row_bytes = u64::try_from(40 + 3 * hash_width).unwrap();
  let (mandatory_count, optional_count) = match decoded_record.state {
    RootExpiryStateV1::LogicallyRetired => (1, 0),
    RootExpiryStateV1::PhysicallyReclaimed => (0, 1),
  };
  let expiry_manifest = encode_root_expiry_manifest_v1(&RootExpiryManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id,
    generation,
    retention_ms,
    optional_byte_budget,
    directory_root_hash: Some(&expiry_directory.key),
    next_page_id: 2,
    record_count: 1,
    logical_bytes: row_bytes,
    mandatory_count,
    mandatory_bytes: mandatory_count * row_bytes,
    optional_count,
    optional_bytes: optional_count * row_bytes,
    oldest_retired_at_ms: Some(decoded_record.retired_at_ms),
    newest_retired_at_ms: Some(decoded_record.retired_at_ms),
  })
  .unwrap();
  let lifecycle_manifest = encode_root_lifecycle_manifest_v1(&RootLifecycleManifestWriteV1 {
    hash_algorithm: algorithm,
    database_id,
    generation,
    published_at_ms,
    source_complete_mark_generation: decoded_record.final_mark_generation,
    authority_root_set_digest,
    candidate_directory_hash: None,
    root_expiry_manifest_hash: Some(&expiry_manifest.key),
    next_page_id: 1,
    candidate_count: 0,
    pending_count: 0,
    retired_evidence_count: 1,
    candidate_bytes: 0,
    expiry_bytes: row_bytes,
  })
  .unwrap();
  ReclaimSupport { expiry_page, expiry_directory, expiry_manifest, lifecycle_manifest }
}

fn reclaim_case(algorithm: HashAlgorithm) -> ReclaimCase {
  let namespace_root_hash = digest_parts(algorithm, &[b"qualified reclaimed namespace root"]);
  reclaim_case_for_root(algorithm, &namespace_root_hash)
}

fn reclaim_case_for_root(algorithm: HashAlgorithm, namespace_root_hash: &[u8]) -> ReclaimCase {
  let inventory_bytes = fixture(algorithm, "physical-inventory-manifest-populated");
  let inventory = decode_physical_inventory_manifest_v1(&inventory_bytes, algorithm).unwrap();
  let authority_root_set_digest = digest_parts(algorithm, &[b"retirement authority roots"]);
  let admission_commit_payload_hash = digest_parts(algorithm, &[b"retirement admission payload"]);
  let prior_lifecycle_manifest_hash = digest_parts(algorithm, &[b"prior selected lifecycle"]);
  let retired_at_ms = i64::try_from(inventory.completed_at_ms).unwrap() - 10_000;
  let retirement_bytes = encode_root_retirement_commit_v1(&RootRetirementCommitWriteV1 {
    hash_algorithm: algorithm,
    database_id: inventory.database_id,
    namespace_root_hash,
    retirement_id: &[0x91; 16],
    committed_at_ms: retired_at_ms,
    pending_since_ms: retired_at_ms - 86_400_000,
    grace_at_pending_ms: 86_400_000,
    final_mark_generation: 41,
    reason: 1,
    prior_lifecycle_manifest_hash: &prior_lifecycle_manifest_hash,
    authority_root_set_digest: &authority_root_set_digest,
    admission_commit_payload_hash: &admission_commit_payload_hash,
  })
  .unwrap()
  .value;
  let retirement = decode_root_retirement_commit_v1(&retirement_bytes, algorithm).unwrap();
  let prior_expiry_bytes = encode_root_expiry_record_v1(&RootExpiryRecordWriteV1 {
    hash_algorithm: algorithm,
    namespace_root_hash,
    retired_at_ms,
    last_pending_since_ms: retirement.pending_since_ms,
    final_mark_generation: retirement.final_mark_generation,
    reason: retirement.reason,
    state: RootExpiryStateV1::LogicallyRetired,
    retirement_commit_hash: &retirement.key,
    root_object_reclaim_proof_hash: None,
    evidence_expires_at_ms: None,
  })
  .unwrap();
  let reclaimed_at_ms = i64::try_from(inventory.completed_at_ms).unwrap() + 1_000;
  ReclaimCase {
    inventory_bytes,
    retirement_bytes,
    prior_expiry_bytes,
    proof_id: [0xa1; 16],
    root_incarnation_digest: digest_parts(algorithm, &[b"canonical root incarnation set"]),
    sweep_receipt_merkle_root: digest_parts(algorithm, &[b"canonical sweep receipt set"]),
    absence_digest: digest_parts(algorithm, &[b"final root object absence proof"]),
    reclaimed_at_ms,
  }
}

fn expiry_row(
  algorithm: HashAlgorithm,
  namespace_root_hash: &[u8],
  retired_at_ms: i64,
  state: RootExpiryStateV1,
  retirement_commit_hash: &[u8],
  proof_hash: Option<&[u8]>,
  evidence_expires_at_ms: Option<i64>,
) -> Vec<u8> {
  encode_root_expiry_record_v1(&RootExpiryRecordWriteV1 {
    hash_algorithm: algorithm,
    namespace_root_hash,
    retired_at_ms,
    last_pending_since_ms: retired_at_ms - 1_000,
    final_mark_generation: 41,
    reason: 1,
    state,
    retirement_commit_hash,
    root_object_reclaim_proof_hash: proof_hash,
    evidence_expires_at_ms,
  })
  .unwrap()
}

fn retention_manifests<'a>(
  algorithm: HashAlgorithm,
  database_id: &'a [u8],
  rows: &[RootExpiryRecordV1<'_>],
  directory_root_hash: &'a [u8],
  authority_root_set_digest: &'a [u8],
  expiry_key: &'a [u8],
  lifecycle_key: Vec<u8>,
) -> (RootExpiryManifestV1<'a>, RootLifecycleManifestV1<'a>) {
  let row_bytes = u64::try_from(40 + 3 * algorithm.hash_length()).unwrap();
  let mandatory_count = u64::try_from(rows.iter().filter(|row| row.state == RootExpiryStateV1::LogicallyRetired).count()).unwrap();
  let optional_count = u64::try_from(rows.len()).unwrap() - mandatory_count;
  let record_count = u64::try_from(rows.len()).unwrap();
  let logical_bytes = record_count * row_bytes;
  let oldest_retired_at_ms = rows.iter().map(|row| row.retired_at_ms).min();
  let newest_retired_at_ms = rows.iter().map(|row| row.retired_at_ms).max();
  let expiry = RootExpiryManifestV1 {
    database_id,
    generation: 17,
    retention_ms: RETENTION_MS,
    optional_byte_budget: 16 * row_bytes,
    directory_root_hash: Some(directory_root_hash),
    next_page_id: 2,
    record_count,
    logical_bytes,
    mandatory_count,
    mandatory_bytes: mandatory_count * row_bytes,
    optional_count,
    optional_bytes: optional_count * row_bytes,
    oldest_retired_at_ms,
    newest_retired_at_ms,
    key: expiry_key.to_vec(),
  };
  let lifecycle = RootLifecycleManifestV1 {
    database_id,
    generation: 17,
    published_at_ms: oldest_retired_at_ms.unwrap() - 1,
    source_complete_mark_generation: 41,
    authority_root_set_digest,
    candidate_directory_hash: None,
    root_expiry_manifest_hash: Some(expiry_key),
    next_page_id: 0,
    candidate_count: 0,
    pending_count: 0,
    retired_evidence_count: record_count,
    candidate_bytes: 0,
    expiry_bytes: logical_bytes,
    key: lifecycle_key,
  };
  (expiry, lifecycle)
}

#[test]
fn qualified_reclaim_binds_exact_inventory_receipts_proof_and_optional_row_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let values = reclaim_case(algorithm);
    let inventory = decode_physical_inventory_manifest_v1(&values.inventory_bytes, algorithm).unwrap();
    let retirement = decode_root_retirement_commit_v1(&values.retirement_bytes, algorithm).unwrap();
    let prior_expiry = decode_root_expiry_record_v1(&values.prior_expiry_bytes, algorithm).unwrap();
    let cancellation = CancellationToken::new();
    let mut verifier = RecordingVerifier { calls: 0, fail: false, cancel_after_verify: None };
    let qualified = qualify_root_object_reclaim_v1(
      RootObjectReclaimQualificationRequestV1 {
        hash_algorithm: algorithm,
        prior_expiry: &prior_expiry,
        retirement: &retirement,
        final_physical_inventory: &inventory,
        proof_id: &values.proof_id,
        reclaimed_at_ms: values.reclaimed_at_ms,
        latest_sweep_receipt_generation: inventory.generation - 1,
        root_object_incarnation_digest: &values.root_incarnation_digest,
        root_object_incarnation_count: 2,
        sweep_receipt_merkle_root: &values.sweep_receipt_merkle_root,
        sweep_receipt_count: 3,
        absence_digest: &values.absence_digest,
        retention_ms: RETENTION_MS,
      },
      &cancellation,
      &mut verifier,
    )
    .unwrap();

    assert_eq!(verifier.calls, 1);
    let proof = decode_root_object_reclaim_proof_v1(qualified.encoded_proof().value.as_slice(), algorithm).unwrap();
    let replacement = decode_root_expiry_record_v1(qualified.encoded_expiry_record(), algorithm).unwrap();
    assert_eq!(proof.generation, inventory.generation);
    assert_eq!(proof.physical_inventory_manifest_hash, inventory.key);
    assert_eq!(proof.retirement_commit_hash, retirement.key);
    assert_eq!(replacement.state, RootExpiryStateV1::PhysicallyReclaimed);
    assert_eq!(replacement.retired_at_ms, prior_expiry.retired_at_ms);
    assert_eq!(replacement.evidence_expires_at_ms, Some(values.reclaimed_at_ms + i64::try_from(RETENTION_MS).unwrap()));
    validate_root_expiry_reclaim_proof(&replacement, &proof).unwrap();
  }
}

#[test]
fn reclaim_support_closure_binds_the_exact_proof_and_rebuilt_expiry_graph_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let values = reclaim_case(algorithm);
    let inventory = decode_physical_inventory_manifest_v1(&values.inventory_bytes, algorithm).unwrap();
    let retirement = decode_root_retirement_commit_v1(&values.retirement_bytes, algorithm).unwrap();
    let prior_expiry = decode_root_expiry_record_v1(&values.prior_expiry_bytes, algorithm).unwrap();
    let cancellation = CancellationToken::new();
    let mut verifier = RecordingVerifier { calls: 0, fail: false, cancel_after_verify: None };
    let qualified = qualify_root_object_reclaim_v1(
      RootObjectReclaimQualificationRequestV1 {
        hash_algorithm: algorithm,
        prior_expiry: &prior_expiry,
        retirement: &retirement,
        final_physical_inventory: &inventory,
        proof_id: &values.proof_id,
        reclaimed_at_ms: values.reclaimed_at_ms,
        latest_sweep_receipt_generation: inventory.generation - 1,
        root_object_incarnation_digest: &values.root_incarnation_digest,
        root_object_incarnation_count: 2,
        sweep_receipt_merkle_root: &values.sweep_receipt_merkle_root,
        sweep_receipt_count: 3,
        absence_digest: &values.absence_digest,
        retention_ms: RETENTION_MS,
      },
      &cancellation,
      &mut verifier,
    )
    .unwrap();
    let authority_root_set_digest = digest_parts(algorithm, &[b"reclaim support authority roots"]);
    let support = reclaim_support(
      algorithm,
      inventory.database_id,
      &authority_root_set_digest,
      42,
      values.reclaimed_at_ms + 1,
      (RETENTION_MS, 16 * u64::try_from(40 + 3 * algorithm.hash_length()).unwrap()),
      qualified.encoded_expiry_record(),
    );
    let lifecycle = decode_root_lifecycle_manifest_v1(&support.lifecycle_manifest.value, algorithm).unwrap();
    let expiry = decode_root_expiry_manifest_v1(&support.expiry_manifest.value, algorithm).unwrap();
    let proof = decode_root_object_reclaim_proof_v1(&qualified.encoded_proof().value, algorithm).unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap());
    let mut builder = RootLifecycleSupportClosureBuilderV1::new_for_reclaim(
      &lifecycle,
      &expiry,
      &proof,
      algorithm,
      &cancellation,
      RootLifecycleSupportLimitsV1 { maximum_candidate_records: 0, maximum_expiry_records: 1, maximum_support_artifacts: 2 },
      &memory,
    )
    .unwrap();
    builder.observe_encoded(&support.expiry_page.value).unwrap();
    builder.observe_encoded(&support.expiry_directory.value).unwrap();
    let closure = builder.finish().unwrap();

    assert_eq!(closure.lifecycle_manifest_hash(), support.lifecycle_manifest.key);
    assert_eq!(closure.expiry_manifest_hash(), Some(support.expiry_manifest.key.as_slice()));
    assert_eq!(closure.root_object_reclaim_proof_hash(), Some(qualified.encoded_proof().key.as_slice()));
  }
}

#[test]
fn reclaim_support_closure_rejects_missing_mandatory_or_mismatched_target_evidence() {
  let algorithm = HashAlgorithm::Blake3_256;
  let values = reclaim_case(algorithm);
  let inventory = decode_physical_inventory_manifest_v1(&values.inventory_bytes, algorithm).unwrap();
  let retirement = decode_root_retirement_commit_v1(&values.retirement_bytes, algorithm).unwrap();
  let prior_expiry = decode_root_expiry_record_v1(&values.prior_expiry_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let mut verifier = RecordingVerifier { calls: 0, fail: false, cancel_after_verify: None };
  let qualified = qualify_root_object_reclaim_v1(
    RootObjectReclaimQualificationRequestV1 {
      hash_algorithm: algorithm,
      prior_expiry: &prior_expiry,
      retirement: &retirement,
      final_physical_inventory: &inventory,
      proof_id: &values.proof_id,
      reclaimed_at_ms: values.reclaimed_at_ms,
      latest_sweep_receipt_generation: inventory.generation - 1,
      root_object_incarnation_digest: &values.root_incarnation_digest,
      root_object_incarnation_count: 2,
      sweep_receipt_merkle_root: &values.sweep_receipt_merkle_root,
      sweep_receipt_count: 3,
      absence_digest: &values.absence_digest,
      retention_ms: RETENTION_MS,
    },
    &cancellation,
    &mut verifier,
  )
  .unwrap();
  let authority_root_set_digest = digest_parts(algorithm, &[b"reclaim support authority roots"]);
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 * 1024 * 1024, 32 * 1024 * 1024, 1, 1024 * 1024).unwrap());
  let proof = decode_root_object_reclaim_proof_v1(&qualified.encoded_proof().value, algorithm).unwrap();
  let assert_reclaim_closure_error = |support: &ReclaimSupport, proof, expected_code| {
    let lifecycle = decode_root_lifecycle_manifest_v1(&support.lifecycle_manifest.value, algorithm).unwrap();
    let expiry = decode_root_expiry_manifest_v1(&support.expiry_manifest.value, algorithm).unwrap();
    let mut builder = RootLifecycleSupportClosureBuilderV1::new_for_reclaim(
      &lifecycle,
      &expiry,
      proof,
      algorithm,
      &cancellation,
      RootLifecycleSupportLimitsV1 { maximum_candidate_records: 0, maximum_expiry_records: 1, maximum_support_artifacts: 2 },
      &memory,
    )
    .unwrap();
    if let Err(error) = builder.observe_encoded(&support.expiry_page.value) {
      assert_eq!(error.code(), expected_code);
      return;
    }
    builder.observe_encoded(&support.expiry_directory.value).unwrap();
    assert_eq!(builder.finish().unwrap_err().code(), expected_code);
  };

  let mandatory_support = reclaim_support(
    algorithm,
    inventory.database_id,
    &authority_root_set_digest,
    42,
    values.reclaimed_at_ms + 1,
    (RETENTION_MS, 16 * u64::try_from(40 + 3 * algorithm.hash_length()).unwrap()),
    &values.prior_expiry_bytes,
  );
  assert_reclaim_closure_error(&mandatory_support, &proof, "root_expiry_reclaim_proof");

  let other_root = vec![0xe1; algorithm.hash_length()];
  let other_row = expiry_row(
    algorithm,
    &other_root,
    prior_expiry.retired_at_ms,
    RootExpiryStateV1::PhysicallyReclaimed,
    prior_expiry.retirement_commit_hash,
    Some(&qualified.encoded_proof().key),
    Some(qualified.evidence_expires_at_ms()),
  );
  let missing_support = reclaim_support(
    algorithm,
    inventory.database_id,
    &authority_root_set_digest,
    42,
    values.reclaimed_at_ms + 1,
    (RETENTION_MS, 16 * u64::try_from(40 + 3 * algorithm.hash_length()).unwrap()),
    &other_row,
  );
  assert_reclaim_closure_error(&missing_support, &proof, "root_lifecycle_support_reclaim_closure");

  let valid_support = reclaim_support(
    algorithm,
    inventory.database_id,
    &authority_root_set_digest,
    42,
    values.reclaimed_at_ms + 1,
    (RETENTION_MS, 16 * u64::try_from(40 + 3 * algorithm.hash_length()).unwrap()),
    qualified.encoded_expiry_record(),
  );
  let mut mismatched_proof = proof.clone();
  mismatched_proof.key = digest_parts(algorithm, &[b"mismatched root-object reclaim proof"]);
  assert_reclaim_closure_error(&valid_support, &mismatched_proof, "root_expiry_reclaim_proof");
}

#[test]
fn qualification_fails_closed_on_unverified_stale_replayed_or_overflowed_evidence() {
  let algorithm = HashAlgorithm::Blake3_256;
  let values = reclaim_case(algorithm);
  let inventory = decode_physical_inventory_manifest_v1(&values.inventory_bytes, algorithm).unwrap();
  let retirement = decode_root_retirement_commit_v1(&values.retirement_bytes, algorithm).unwrap();
  let prior_expiry = decode_root_expiry_record_v1(&values.prior_expiry_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let request = || RootObjectReclaimQualificationRequestV1 {
    hash_algorithm: algorithm,
    prior_expiry: &prior_expiry,
    retirement: &retirement,
    final_physical_inventory: &inventory,
    proof_id: &values.proof_id,
    reclaimed_at_ms: values.reclaimed_at_ms,
    latest_sweep_receipt_generation: inventory.generation - 1,
    root_object_incarnation_digest: &values.root_incarnation_digest,
    root_object_incarnation_count: 2,
    sweep_receipt_merkle_root: &values.sweep_receipt_merkle_root,
    sweep_receipt_count: 3,
    absence_digest: &values.absence_digest,
    retention_ms: RETENTION_MS,
  };

  let mut verifier = RecordingVerifier { calls: 0, fail: true, cancel_after_verify: None };
  let error = qualify_root_object_reclaim_v1(request(), &cancellation, &mut verifier).unwrap_err();
  assert_eq!(error.code(), "test_receipt_gap");
  assert_eq!(verifier.calls, 1);

  let mut stale = request();
  stale.latest_sweep_receipt_generation = inventory.generation;
  let mut verifier = RecordingVerifier { calls: 0, fail: false, cancel_after_verify: None };
  assert_eq!(qualify_root_object_reclaim_v1(stale, &cancellation, &mut verifier).unwrap_err().code(), "root_reclaim_inventory_order");
  assert_eq!(verifier.calls, 0);

  let mut overflowed = request();
  overflowed.reclaimed_at_ms = i64::MAX - 1;
  overflowed.retention_ms = 10;
  assert_eq!(qualify_root_object_reclaim_v1(overflowed, &cancellation, &mut verifier).unwrap_err().code(), "root_reclaim_time");
  assert_eq!(verifier.calls, 0);

  let mut empty_receipts = request();
  empty_receipts.sweep_receipt_count = 0;
  assert_eq!(qualify_root_object_reclaim_v1(empty_receipts, &cancellation, &mut verifier).unwrap_err().code(), "root_reclaim_evidence");
  assert_eq!(verifier.calls, 0);

  let existing_proof_hash = digest_parts(algorithm, &[b"existing reclaim proof"]);
  let optional_bytes = encode_root_expiry_record_v1(&RootExpiryRecordWriteV1 {
    hash_algorithm: algorithm,
    namespace_root_hash: prior_expiry.namespace_root_hash,
    retired_at_ms: prior_expiry.retired_at_ms,
    last_pending_since_ms: prior_expiry.last_pending_since_ms,
    final_mark_generation: prior_expiry.final_mark_generation,
    reason: prior_expiry.reason,
    state: RootExpiryStateV1::PhysicallyReclaimed,
    retirement_commit_hash: prior_expiry.retirement_commit_hash,
    root_object_reclaim_proof_hash: Some(&existing_proof_hash),
    evidence_expires_at_ms: Some(values.reclaimed_at_ms + 1),
  })
  .unwrap();
  let optional = decode_root_expiry_record_v1(&optional_bytes, algorithm).unwrap();
  let mut already_reclaimed = request();
  already_reclaimed.prior_expiry = &optional;
  assert_eq!(
    qualify_root_object_reclaim_v1(already_reclaimed, &cancellation, &mut verifier).unwrap_err().code(),
    "root_reclaim_lifecycle_state",
  );
  assert_eq!(verifier.calls, 0);

  let mut mismatched_retirement = retirement.clone();
  mismatched_retirement.committed_at_ms += 1;
  let mut wrong_timestamp = request();
  wrong_timestamp.retirement = &mismatched_retirement;
  assert_eq!(
    qualify_root_object_reclaim_v1(wrong_timestamp, &cancellation, &mut verifier).unwrap_err().code(),
    "root_reclaim_lifecycle_state"
  );
  assert_eq!(verifier.calls, 0);

  let callback_cancellation = CancellationToken::new();
  let mut canceling_verifier = RecordingVerifier { calls: 0, fail: false, cancel_after_verify: Some(callback_cancellation.clone()) };
  assert_eq!(
    qualify_root_object_reclaim_v1(request(), &callback_cancellation, &mut canceling_verifier).unwrap_err().code(),
    "root_reclaim_canceled",
  );
  assert_eq!(canceling_verifier.calls, 1);

  cancellation.cancel();
  assert_eq!(qualify_root_object_reclaim_v1(request(), &cancellation, &mut verifier).unwrap_err().code(), "root_reclaim_canceled");
  assert_eq!(verifier.calls, 0);
}

#[test]
fn retention_stream_drops_expired_and_oldest_optional_rows_while_preserving_every_mandatory_row() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let hash_width = algorithm.hash_length();
    let target_root = vec![0x40; hash_width];
    let target = reclaim_case_for_root(algorithm, &target_root);
    let inventory = decode_physical_inventory_manifest_v1(&target.inventory_bytes, algorithm).unwrap();
    let retirement = decode_root_retirement_commit_v1(&target.retirement_bytes, algorithm).unwrap();
    let target_prior = decode_root_expiry_record_v1(&target.prior_expiry_bytes, algorithm).unwrap();
    let cancellation = CancellationToken::new();
    let mut verifier = RecordingVerifier { calls: 0, fail: false, cancel_after_verify: None };
    let qualified = qualify_root_object_reclaim_v1(
      RootObjectReclaimQualificationRequestV1 {
        hash_algorithm: algorithm,
        prior_expiry: &target_prior,
        retirement: &retirement,
        final_physical_inventory: &inventory,
        proof_id: &target.proof_id,
        reclaimed_at_ms: target.reclaimed_at_ms,
        latest_sweep_receipt_generation: inventory.generation - 1,
        root_object_incarnation_digest: &target.root_incarnation_digest,
        root_object_incarnation_count: 2,
        sweep_receipt_merkle_root: &target.sweep_receipt_merkle_root,
        sweep_receipt_count: 3,
        absence_digest: &target.absence_digest,
        retention_ms: RETENTION_MS,
      },
      &cancellation,
      &mut verifier,
    )
    .unwrap();
    let target_expires_at_ms = qualified.evidence_expires_at_ms();
    let completed_at_ms = target.reclaimed_at_ms + 10;
    let retirement_hash = digest_parts(algorithm, &[b"other retirement"]);
    let proof_hash = digest_parts(algorithm, &[b"other reclaim proof"]);
    let row_bytes = u64::try_from(40 + 3 * hash_width).unwrap();
    let rows = [
      expiry_row(
        algorithm,
        &vec![0x10; hash_width],
        completed_at_ms - 50_000,
        RootExpiryStateV1::LogicallyRetired,
        &retirement_hash,
        None,
        None,
      ),
      expiry_row(
        algorithm,
        &vec![0x20; hash_width],
        completed_at_ms - 40_000,
        RootExpiryStateV1::PhysicallyReclaimed,
        &retirement_hash,
        Some(&proof_hash),
        Some(completed_at_ms),
      ),
      expiry_row(
        algorithm,
        &vec![0x30; hash_width],
        completed_at_ms - 30_000,
        RootExpiryStateV1::PhysicallyReclaimed,
        &retirement_hash,
        Some(&proof_hash),
        Some(target_expires_at_ms - 1_000),
      ),
      target.prior_expiry_bytes.clone(),
      expiry_row(
        algorithm,
        &vec![0x50; hash_width],
        completed_at_ms - 10_000,
        RootExpiryStateV1::PhysicallyReclaimed,
        &retirement_hash,
        Some(&proof_hash),
        Some(target_expires_at_ms + 1_000),
      ),
    ];
    let decoded_rows: Vec<_> = rows.iter().map(|row| decode_root_expiry_record_v1(row, algorithm).unwrap()).collect();
    let expiry_key = digest_parts(algorithm, &[b"selected prior expiry manifest"]);
    let lifecycle_key = digest_parts(algorithm, &[b"selected prior lifecycle manifest"]);
    let directory_root_hash = digest_parts(algorithm, &[b"prior expiry directory"]);
    let authority_root_set_digest = digest_parts(algorithm, &[b"root lifecycle authority set"]);
    let (expiry, lifecycle) = retention_manifests(
      algorithm,
      inventory.database_id,
      &decoded_rows,
      &directory_root_hash,
      &authority_root_set_digest,
      &expiry_key,
      lifecycle_key,
    );
    let cutoff = RootExpiryRetentionCutoffV1 { evidence_expires_at_ms: target_expires_at_ms, namespace_root_hash: target_root };
    let mut model = RootExpiryRetentionModelV1::new(
      RootExpiryRetentionContextV1 {
        hash_algorithm: algorithm,
        prior_lifecycle: &lifecycle,
        prior_expiry: &expiry,
        lifecycle_generation: lifecycle.generation + 1,
        completed_at_ms,
        retention_ms: RETENTION_MS,
        optional_byte_budget: 2 * row_bytes,
        maximum_records: 5,
        selection: RootExpiryRetentionSelectionV1::AtOrAfter(cutoff),
        qualified_reclaim: &qualified,
      },
      &cancellation,
    )
    .unwrap();
    let actions: Vec<_> = decoded_rows.iter().map(|row| model.observe(row).unwrap()).collect();
    assert_eq!(
      actions,
      [
        RootExpiryRetentionActionV1::RetainedMandatory,
        RootExpiryRetentionActionV1::DroppedExpired,
        RootExpiryRetentionActionV1::DroppedForBudget,
        RootExpiryRetentionActionV1::ReclaimedAndRetained,
        RootExpiryRetentionActionV1::RetainedOptional,
      ],
    );
    let permit = model.finish().unwrap();
    let summary = permit.summary();
    assert_eq!((summary.prior_count, summary.resulting_count), (5, 3));
    assert_eq!((summary.resulting_mandatory_count, summary.resulting_optional_count), (1, 2));
    assert_eq!((summary.expired_count, summary.budget_evicted_count, summary.reclaimed_count), (1, 1, 1));
    assert_eq!(summary.resulting_optional_bytes, 2 * row_bytes);
    assert_eq!(permit.hash_algorithm(), algorithm);
    assert_eq!(permit.database_id().as_slice(), inventory.database_id);
    assert_eq!(permit.prior_lifecycle_manifest_hash(), lifecycle.key);
    assert_eq!(permit.prior_expiry_manifest_hash(), expiry.key);
    assert_eq!(permit.namespace_root_hash(), target_prior.namespace_root_hash);
    assert_eq!(permit.lifecycle_generation(), lifecycle.generation + 1);
    assert_eq!(permit.completed_at_ms(), completed_at_ms);
    assert_eq!(permit.retention_ms(), RETENTION_MS);
    assert_eq!(permit.optional_byte_budget(), 2 * row_bytes);
    assert_eq!(permit.root_object_reclaim_proof_hash(), qualified.encoded_proof().key);
  }
}

#[test]
fn retention_stream_rejects_nonmaximal_cutoffs_missing_targets_ordering_and_cancellation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let hash_width = algorithm.hash_length();
  let target_root = vec![0x40; hash_width];
  let target = reclaim_case_for_root(algorithm, &target_root);
  let inventory = decode_physical_inventory_manifest_v1(&target.inventory_bytes, algorithm).unwrap();
  let retirement = decode_root_retirement_commit_v1(&target.retirement_bytes, algorithm).unwrap();
  let target_prior = decode_root_expiry_record_v1(&target.prior_expiry_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let mut verifier = RecordingVerifier { calls: 0, fail: false, cancel_after_verify: None };
  let qualified = qualify_root_object_reclaim_v1(
    RootObjectReclaimQualificationRequestV1 {
      hash_algorithm: algorithm,
      prior_expiry: &target_prior,
      retirement: &retirement,
      final_physical_inventory: &inventory,
      proof_id: &target.proof_id,
      reclaimed_at_ms: target.reclaimed_at_ms,
      latest_sweep_receipt_generation: inventory.generation - 1,
      root_object_incarnation_digest: &target.root_incarnation_digest,
      root_object_incarnation_count: 2,
      sweep_receipt_merkle_root: &target.sweep_receipt_merkle_root,
      sweep_receipt_count: 3,
      absence_digest: &target.absence_digest,
      retention_ms: RETENTION_MS,
    },
    &cancellation,
    &mut verifier,
  )
  .unwrap();
  let completed_at_ms = target.reclaimed_at_ms + 10;
  let target_expires_at_ms = qualified.evidence_expires_at_ms();
  let retirement_hash = digest_parts(algorithm, &[b"other retirement"]);
  let proof_hash = digest_parts(algorithm, &[b"other reclaim proof"]);
  let rows = [
    expiry_row(
      algorithm,
      &vec![0x10; hash_width],
      completed_at_ms - 30_000,
      RootExpiryStateV1::LogicallyRetired,
      &retirement_hash,
      None,
      None,
    ),
    expiry_row(
      algorithm,
      &vec![0x30; hash_width],
      completed_at_ms - 20_000,
      RootExpiryStateV1::PhysicallyReclaimed,
      &retirement_hash,
      Some(&proof_hash),
      Some(target_expires_at_ms - 1_000),
    ),
    target.prior_expiry_bytes.clone(),
    expiry_row(
      algorithm,
      &vec![0x50; hash_width],
      completed_at_ms - 10_000,
      RootExpiryStateV1::PhysicallyReclaimed,
      &retirement_hash,
      Some(&proof_hash),
      Some(target_expires_at_ms + 1_000),
    ),
  ];
  let decoded_rows: Vec<_> = rows.iter().map(|row| decode_root_expiry_record_v1(row, algorithm).unwrap()).collect();
  let expiry_key = digest_parts(algorithm, &[b"selected prior expiry manifest"]);
  let lifecycle_key = digest_parts(algorithm, &[b"selected prior lifecycle manifest"]);
  let directory_root_hash = digest_parts(algorithm, &[b"prior expiry directory"]);
  let authority_root_set_digest = digest_parts(algorithm, &[b"root lifecycle authority set"]);
  let (expiry, lifecycle) = retention_manifests(
    algorithm,
    inventory.database_id,
    &decoded_rows,
    &directory_root_hash,
    &authority_root_set_digest,
    &expiry_key,
    lifecycle_key,
  );
  let row_bytes = u64::try_from(40 + 3 * hash_width).unwrap();
  let context = |selection, optional_byte_budget| RootExpiryRetentionContextV1 {
    hash_algorithm: algorithm,
    prior_lifecycle: &lifecycle,
    prior_expiry: &expiry,
    lifecycle_generation: lifecycle.generation + 1,
    completed_at_ms,
    retention_ms: RETENTION_MS,
    optional_byte_budget,
    maximum_records: 4,
    selection,
    qualified_reclaim: &qualified,
  };

  let mut keep_all =
    RootExpiryRetentionModelV1::new(context(RootExpiryRetentionSelectionV1::KeepAll, 2 * row_bytes), &cancellation).unwrap();
  for row in &decoded_rows {
    keep_all.observe(row).unwrap();
  }
  assert_eq!(keep_all.finish().unwrap_err().code(), "root_expiry_retention_budget");

  let too_new = RootExpiryRetentionCutoffV1 { evidence_expires_at_ms: target_expires_at_ms, namespace_root_hash: target_root.clone() };
  let mut nonmaximal =
    RootExpiryRetentionModelV1::new(context(RootExpiryRetentionSelectionV1::AtOrAfter(too_new), 3 * row_bytes), &cancellation).unwrap();
  for row in &decoded_rows {
    nonmaximal.observe(row).unwrap();
  }
  assert_eq!(nonmaximal.finish().unwrap_err().code(), "root_expiry_retention_nonmaximal");

  let target_excluding =
    RootExpiryRetentionCutoffV1 { evidence_expires_at_ms: target_expires_at_ms + 1, namespace_root_hash: vec![0x01; hash_width] };
  let mut excludes_target =
    RootExpiryRetentionModelV1::new(context(RootExpiryRetentionSelectionV1::AtOrAfter(target_excluding), 2 * row_bytes), &cancellation)
      .unwrap();
  assert_eq!(excludes_target.observe(&decoded_rows[0]).unwrap(), RootExpiryRetentionActionV1::RetainedMandatory);
  excludes_target.observe(&decoded_rows[1]).unwrap();
  assert_eq!(excludes_target.observe(&decoded_rows[2]).unwrap_err().code(), "root_expiry_retention_target");
  assert_eq!(excludes_target.observe(&decoded_rows[3]).unwrap_err().code(), "root_expiry_retention_failed");

  let ambiguous_cutoff =
    RootExpiryRetentionCutoffV1 { evidence_expires_at_ms: target_expires_at_ms - 500, namespace_root_hash: vec![0x01; hash_width] };
  let mut ambiguous =
    RootExpiryRetentionModelV1::new(context(RootExpiryRetentionSelectionV1::AtOrAfter(ambiguous_cutoff), 2 * row_bytes), &cancellation)
      .unwrap();
  for row in &decoded_rows {
    ambiguous.observe(row).unwrap();
  }
  assert_eq!(ambiguous.finish().unwrap_err().code(), "root_expiry_retention_nonmaximal");

  let mut missing_target =
    RootExpiryRetentionModelV1::new(context(RootExpiryRetentionSelectionV1::KeepAll, 3 * row_bytes), &cancellation).unwrap();
  for row in [&decoded_rows[0], &decoded_rows[1], &decoded_rows[3]] {
    missing_target.observe(row).unwrap();
  }
  assert_eq!(missing_target.finish().unwrap_err().code(), "root_expiry_retention_target");

  let mut out_of_order =
    RootExpiryRetentionModelV1::new(context(RootExpiryRetentionSelectionV1::KeepAll, 3 * row_bytes), &cancellation).unwrap();
  out_of_order.observe(&decoded_rows[1]).unwrap();
  assert_eq!(out_of_order.observe(&decoded_rows[0]).unwrap_err().code(), "root_expiry_retention_order");
  assert_eq!(out_of_order.observe(&decoded_rows[2]).unwrap_err().code(), "root_expiry_retention_failed");

  let replacement_mandatory_bytes = expiry_row(
    algorithm,
    decoded_rows[0].namespace_root_hash,
    decoded_rows[0].retired_at_ms,
    RootExpiryStateV1::PhysicallyReclaimed,
    &retirement_hash,
    Some(&proof_hash),
    Some(target_expires_at_ms + 2_000),
  );
  let replacement_mandatory = decode_root_expiry_record_v1(&replacement_mandatory_bytes, algorithm).unwrap();
  let mut aggregate_lie =
    RootExpiryRetentionModelV1::new(context(RootExpiryRetentionSelectionV1::KeepAll, 4 * row_bytes), &cancellation).unwrap();
  for row in [&replacement_mandatory, &decoded_rows[1], &decoded_rows[2], &decoded_rows[3]] {
    aggregate_lie.observe(row).unwrap();
  }
  assert_eq!(aggregate_lie.finish().unwrap_err().code(), "root_expiry_retention_manifest");

  let extra_bytes = expiry_row(
    algorithm,
    &vec![0x60; hash_width],
    completed_at_ms - 5_000,
    RootExpiryStateV1::PhysicallyReclaimed,
    &retirement_hash,
    Some(&proof_hash),
    Some(target_expires_at_ms + 2_000),
  );
  let extra = decode_root_expiry_record_v1(&extra_bytes, algorithm).unwrap();
  let mut limited =
    RootExpiryRetentionModelV1::new(context(RootExpiryRetentionSelectionV1::KeepAll, 4 * row_bytes), &cancellation).unwrap();
  for row in &decoded_rows {
    limited.observe(row).unwrap();
  }
  assert_eq!(limited.observe(&extra).unwrap_err().code(), "root_expiry_retention_limit");

  let invalid_cutoff = RootExpiryRetentionCutoffV1 { evidence_expires_at_ms: completed_at_ms, namespace_root_hash: vec![0; hash_width] };
  assert_eq!(
    RootExpiryRetentionModelV1::new(context(RootExpiryRetentionSelectionV1::AtOrAfter(invalid_cutoff), 3 * row_bytes), &cancellation,)
      .unwrap_err()
      .code(),
    "root_expiry_retention_configuration",
  );

  let mut canceled =
    RootExpiryRetentionModelV1::new(context(RootExpiryRetentionSelectionV1::KeepAll, 2 * row_bytes), &cancellation).unwrap();
  canceled.observe(&decoded_rows[0]).unwrap();
  cancellation.cancel();
  assert_eq!(canceled.observe(&decoded_rows[1]).unwrap_err().code(), "root_expiry_retention_canceled");
  assert_eq!(canceled.observe(&decoded_rows[2]).unwrap_err().code(), "root_expiry_retention_failed");
}
