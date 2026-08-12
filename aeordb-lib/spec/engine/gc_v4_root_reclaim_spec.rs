use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc_lifecycle::{
  RootExpiryRecordWriteV1, RootRetirementCommitWriteV1, decode_root_object_reclaim_proof_v1, decode_root_retirement_commit_v1,
  encode_root_expiry_record_v1, encode_root_retirement_commit_v1, validate_root_expiry_reclaim_proof,
};
use aeordb::engine::v4::gc_root_reclaim::{
  RootObjectReclaimEvidenceVerificationErrorV1, RootObjectReclaimEvidenceVerificationRequestV1, RootObjectReclaimEvidenceVerifierV1,
  RootObjectReclaimQualificationRequestV1, qualify_root_object_reclaim_v1,
};
use aeordb::engine::v4::gc_state::{RootExpiryStateV1, decode_physical_inventory_manifest_v1, decode_root_expiry_record_v1};
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

fn reclaim_case(algorithm: HashAlgorithm) -> ReclaimCase {
  let inventory_bytes = fixture(algorithm, "physical-inventory-manifest-populated");
  let inventory = decode_physical_inventory_manifest_v1(&inventory_bytes, algorithm).unwrap();
  let namespace_root_hash = digest_parts(algorithm, &[b"qualified reclaimed namespace root"]);
  let authority_root_set_digest = digest_parts(algorithm, &[b"retirement authority roots"]);
  let admission_commit_payload_hash = digest_parts(algorithm, &[b"retirement admission payload"]);
  let prior_lifecycle_manifest_hash = digest_parts(algorithm, &[b"prior selected lifecycle"]);
  let retired_at_ms = i64::try_from(inventory.completed_at_ms).unwrap() - 10_000;
  let retirement_bytes = encode_root_retirement_commit_v1(&RootRetirementCommitWriteV1 {
    hash_algorithm: algorithm,
    database_id: inventory.database_id,
    namespace_root_hash: &namespace_root_hash,
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
    namespace_root_hash: &namespace_root_hash,
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
