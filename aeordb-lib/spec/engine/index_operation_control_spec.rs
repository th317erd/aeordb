use std::path::Path;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::index_operation_control::{
  IndexOperationControlV1, IndexOperationControlWriteV1, IndexOperationKindV1, IndexOperationStateV1, RetryClassV1, StableReasonV1,
  decode_index_operation_control, encode_index_operation_control,
};
use aeordb::engine::v4::reader::MalformedInputClass;

fn hash(algorithm: HashAlgorithm, fill: u8) -> Vec<u8> {
  vec![fill; algorithm.hash_length()]
}

fn write_request(algorithm: HashAlgorithm) -> IndexOperationControlWriteV1<'static> {
  let index_id = Box::leak(hash(algorithm, 0x21).into_boxed_slice());
  let requested_namespace_root = Box::leak(hash(algorithm, 0x31).into_boxed_slice());
  let definition_id = Box::leak(hash(algorithm, 0x41).into_boxed_slice());
  let base_manifest = Box::leak(hash(algorithm, 0x51).into_boxed_slice());
  let target_manifest = Box::leak(hash(algorithm, 0x61).into_boxed_slice());
  let checkpoint_artifact = Box::leak(hash(algorithm, 0x71).into_boxed_slice());
  let error_evidence_hash = Box::leak(hash(algorithm, 0x81).into_boxed_slice());
  IndexOperationControlWriteV1 {
    database_id: [0x11; 16],
    index_id,
    operation_id: [0x22; 16],
    operation_kind: IndexOperationKindV1::Reconcile,
    state: IndexOperationStateV1::Checkpointed,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_123,
    requested_namespace_root,
    definition_id,
    base_manifest: Some(base_manifest),
    target_manifest: Some(target_manifest),
    checkpoint_artifact: Some(checkpoint_artifact),
    captured_runtime_sequence: 101,
    reconciled_through_sequence: 102,
    completed_work: 103,
    total_work_hint: 104,
    stable_reason: StableReasonV1::Requested,
    retry_class: RetryClassV1::BoundedBackoff,
    error_evidence_hash: Some(error_evidence_hash),
  }
}

fn assert_round_trip(algorithm: HashAlgorithm) {
  let request = write_request(algorithm);
  let encoded = encode_index_operation_control(7, &request, algorithm).unwrap();
  let decoded = decode_index_operation_control(&encoded, algorithm).unwrap();
  assert_eq!(decoded, IndexOperationControlV1::from_write(7, &request));
  assert_eq!(encode_index_operation_control(decoded.control_sequence, &decoded.as_write(), algorithm).unwrap(), encoded);
}

#[test]
fn typed_index_operation_round_trips_every_supported_hash_profile() {
  for algorithm in
    [HashAlgorithm::Blake3_256, HashAlgorithm::Sha256, HashAlgorithm::Sha512, HashAlgorithm::Sha3_256, HashAlgorithm::Sha3_512]
  {
    assert_round_trip(algorithm);
  }
}

#[test]
fn typed_reader_round_trips_the_independent_golden_fixtures() {
  for (algorithm, file) in [
    (HashAlgorithm::Blake3_256, "control-blake3-256-index-operation-valid.bin"),
    (HashAlgorithm::Sha512, "control-sha512-index-operation-valid.bin"),
  ] {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/system-control-v1").join(file);
    let bytes = std::fs::read(path).unwrap();
    let decoded = decode_index_operation_control(&bytes, algorithm).unwrap();
    assert_eq!(decoded.control_sequence, 7);
    assert_eq!(decoded.index_id.len(), algorithm.hash_length());
    assert_eq!(encode_index_operation_control(decoded.control_sequence, &decoded.as_write(), algorithm).unwrap(), bytes);
  }
}

#[test]
fn optional_hashes_are_canonical_zeroes_and_required_hashes_are_nonzero() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut request = write_request(algorithm);
  request.base_manifest = None;
  request.target_manifest = None;
  request.checkpoint_artifact = None;
  request.error_evidence_hash = None;
  let encoded = encode_index_operation_control(1, &request, algorithm).unwrap();
  let decoded = decode_index_operation_control(&encoded, algorithm).unwrap();
  assert_eq!(decoded.base_manifest, None);
  assert_eq!(decoded.target_manifest, None);
  assert_eq!(decoded.checkpoint_artifact, None);
  assert_eq!(decoded.error_evidence_hash, None);

  request.definition_id = Box::leak(vec![0; algorithm.hash_length()].into_boxed_slice());
  let error = encode_index_operation_control(1, &request, algorithm).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::IdentityKeyOrGenerationMismatch);
}

#[test]
fn writer_rejects_wrong_width_time_order_and_counter_order() {
  let algorithm = HashAlgorithm::Blake3_256;
  let mut request = write_request(algorithm);
  request.index_id = &[0x11; 31];
  assert_eq!(
    encode_index_operation_control(1, &request, algorithm).unwrap_err().class(),
    MalformedInputClass::LengthCountOrArithmeticOverflow
  );

  let mut request = write_request(algorithm);
  request.updated_at_ms = request.created_at_ms - 1;
  assert_eq!(encode_index_operation_control(1, &request, algorithm).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);

  let mut request = write_request(algorithm);
  request.reconciled_through_sequence = request.captured_runtime_sequence - 1;
  assert_eq!(encode_index_operation_control(1, &request, algorithm).unwrap_err().class(), MalformedInputClass::CrossRecordClosureMismatch);
}

#[test]
fn typed_reader_rejects_another_control_kind() {
  let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/system-control-v1/control-blake3-256-index-registry-valid.bin");
  let bytes = std::fs::read(path).unwrap();
  let error = decode_index_operation_control(&bytes, HashAlgorithm::Blake3_256).unwrap_err();
  assert_eq!(error.class(), MalformedInputClass::UnknownTypeKindOrEnum);
}
