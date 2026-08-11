use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::gc::{GcArtifactKindV1, ImmutableGcArtifactWriteV1, decode_gc_artifact_envelope, encode_immutable_gc_artifact};
use aeordb::engine::v4::gc_audit::{AuditArtifactV1, decode_audit_artifact};
use aeordb::engine::v4::gc_state::{
  GcStateArtifactV1, RetirementJournalReferenceModelV1, RetirementReasonV1, decode_gc_state_artifact, decode_retirement_journal_segment_v1,
  retirement_journal_records_v1,
};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(relative_path: &str) -> Vec<u8> {
  fs::read(fixture_root().join(relative_path)).unwrap()
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("retirement fixtures cover the two frozen hash widths"),
  }
}

fn journal_fixture(algorithm: HashAlgorithm) -> Vec<u8> {
  fixture(&format!("agca-{}-retirement-journal-segment-valid.bin", algorithm_name(algorithm)))
}

fn corrupt_evidence_fixture(algorithm: HashAlgorithm) -> Vec<u8> {
  fixture(&format!("agca-{}-corrupt-gc-evidence.bin", algorithm_name(algorithm)))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn record_from_fixture(algorithm: HashAlgorithm, sequence: u64, old_key_first_byte: u8) -> Vec<u8> {
  let bytes = journal_fixture(algorithm);
  let envelope = decode_gc_artifact_envelope(&bytes).unwrap();
  let hash_width = algorithm.hash_length();
  let mut record = envelope.body[32 + hash_width..].to_vec();
  put_u64(&mut record, 8, sequence);
  record[24] = old_key_first_byte;
  record
}

fn journal_segment(
  algorithm: HashAlgorithm,
  ordinal: u64,
  generation: u64,
  chain_reset: bool,
  previous_hash: Option<&[u8]>,
  records: &[Vec<u8>],
) -> Vec<u8> {
  let fixture = journal_fixture(algorithm);
  let fixture_envelope = decode_gc_artifact_envelope(&fixture).unwrap();
  let hash_width = algorithm.hash_length();
  let records_length: usize = records.iter().map(Vec::len).sum();
  let mut body = vec![0u8; 32 + hash_width + records_length];
  put_u32(&mut body, 0, u32::from(chain_reset));
  put_u16(&mut body, 4, 1);
  put_u64(&mut body, 8, u64::from_le_bytes(records[0][8..16].try_into().unwrap()));
  put_u64(&mut body, 16, u64::from_le_bytes(records.last().unwrap()[8..16].try_into().unwrap()));
  put_u32(&mut body, 24, records.len() as u32);
  put_u32(&mut body, 28, records_length as u32);
  if let Some(previous_hash) = previous_hash {
    body[32..32 + hash_width].copy_from_slice(previous_hash);
  }
  let mut cursor = 32 + hash_width;
  for record in records {
    body[cursor..cursor + record.len()].copy_from_slice(record);
    cursor += record.len();
  }
  let mut identity = fixture_envelope.identity[..16].to_vec();
  identity.extend_from_slice(&ordinal.to_le_bytes());
  encode_immutable_gc_artifact(&ImmutableGcArtifactWriteV1 {
    kind: GcArtifactKindV1::RetirementJournalSegment,
    hash_algorithm: algorithm,
    generation,
    identity: &identity,
    body: &body,
  })
  .unwrap()
  .value
}

#[test]
fn typed_retirement_segment_and_records_match_both_independent_fixtures() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let bytes = journal_fixture(algorithm);
    let segment = decode_retirement_journal_segment_v1(&bytes, algorithm).unwrap();
    assert_eq!(segment.database_id, (0x31u8..=0x40).collect::<Vec<_>>());
    assert_eq!(segment.segment_ordinal, 1);
    assert_eq!(segment.generation, 401);
    assert!(segment.chain_reset);
    assert_eq!(segment.first_replacement_sequence, 5_000);
    assert_eq!(segment.last_replacement_sequence, 5_000);
    assert_eq!(segment.record_count, 1);
    assert!(segment.previous_segment_hash.is_none());

    let records: Vec<_> = retirement_journal_records_v1(&segment, algorithm).unwrap().map(Result::unwrap).collect();
    assert_eq!(records.len(), 1);
    let record = records[0];
    assert_eq!(record.reason, RetirementReasonV1::StableKeyReplace);
    assert_eq!(record.replacement_publication_sequence, 5_000);
    assert_eq!(record.retired_at_ms, 1_700_000_050_000);
    assert_ne!(record.old, record.replacement);
    assert_eq!(record.encoded.as_ptr(), segment.records.as_ptr());

    let GcStateArtifactV1::RetirementJournal { record_count, .. } = decode_gc_state_artifact(&bytes, algorithm).unwrap() else {
      panic!("fixture must remain compatible with the generic GC-state reader");
    };
    assert_eq!(record_count, 1);
  }
}

#[test]
fn retirement_reference_model_closes_a_canonical_segment_chain_without_collecting_records() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let first_bytes = journal_fixture(algorithm);
    let first = decode_retirement_journal_segment_v1(&first_bytes, algorithm).unwrap();
    let records = [record_from_fixture(algorithm, 5_001, 0x31), record_from_fixture(algorithm, 5_001, 0x32)];
    let second_bytes = journal_segment(algorithm, 2, 402, false, Some(&first.key), &records);
    let second = decode_retirement_journal_segment_v1(&second_bytes, algorithm).unwrap();
    let cancellation = CancellationToken::new();
    let mut model = RetirementJournalReferenceModelV1::new(algorithm, &cancellation, 3);

    model.observe_segment(&first).unwrap();
    model.observe_segment(&second).unwrap();
    let summary = model.finish().unwrap();
    assert_eq!(summary.segment_count, 2);
    assert_eq!(summary.record_count, 3);
    assert_eq!(summary.first_replacement_sequence, 5_000);
    assert_eq!(summary.last_replacement_sequence, 5_001);
    assert_eq!(summary.last_segment_ordinal, 2);
    assert_eq!(summary.last_segment_hash, second.key);
  }
}

#[test]
fn retirement_reference_model_fails_closed_on_chain_gaps_or_wrong_predecessors() {
  let algorithm = HashAlgorithm::Blake3_256;
  let first_bytes = journal_fixture(algorithm);
  let first = decode_retirement_journal_segment_v1(&first_bytes, algorithm).unwrap();
  let records = [record_from_fixture(algorithm, 5_001, 0x31)];
  let wrong_hash = vec![0xAA; algorithm.hash_length()];
  let bad_bytes = journal_segment(algorithm, 3, 402, false, Some(&wrong_hash), &records);
  let bad = decode_retirement_journal_segment_v1(&bad_bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let mut model = RetirementJournalReferenceModelV1::new(algorithm, &cancellation, 2);
  model.observe_segment(&first).unwrap();
  assert_eq!(model.observe_segment(&bad).unwrap_err().code(), "retirement_journal_segment_ordinal");
  assert_eq!(model.finish().unwrap_err().code(), "retirement_journal_model_failed");

  let bad_bytes = journal_segment(algorithm, 2, 402, false, Some(&wrong_hash), &records);
  let bad = decode_retirement_journal_segment_v1(&bad_bytes, algorithm).unwrap();
  let mut model = RetirementJournalReferenceModelV1::new(algorithm, &cancellation, 2);
  model.observe_segment(&first).unwrap();
  assert_eq!(model.observe_segment(&bad).unwrap_err().code(), "retirement_journal_previous_hash");
}

#[test]
fn retirement_reference_model_enforces_record_bounds_cancellation_and_reset_position() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bytes = journal_fixture(algorithm);
  let segment = decode_retirement_journal_segment_v1(&bytes, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  assert_eq!(
    RetirementJournalReferenceModelV1::new(algorithm, &cancellation, 0).observe_segment(&segment).unwrap_err().code(),
    "retirement_journal_record_limit"
  );

  cancellation.cancel();
  assert_eq!(
    RetirementJournalReferenceModelV1::new(algorithm, &cancellation, 1).observe_segment(&segment).unwrap_err().code(),
    "retirement_journal_cancelled"
  );

  let cancellation = CancellationToken::new();
  let continuation =
    journal_segment(algorithm, 1, 402, false, Some(&vec![0x11; algorithm.hash_length()]), &[record_from_fixture(algorithm, 5_000, 0x30)]);
  let continuation = decode_retirement_journal_segment_v1(&continuation, algorithm).unwrap();
  assert_eq!(
    RetirementJournalReferenceModelV1::new(algorithm, &cancellation, 1).observe_segment(&continuation).unwrap_err().code(),
    "retirement_journal_initial_reset"
  );

  let mut detached = segment.clone();
  detached.key.clear();
  let mut model = RetirementJournalReferenceModelV1::new(algorithm, &cancellation, 1);
  assert_eq!(model.observe_segment(&detached).unwrap_err().code(), "retirement_journal_segment_shape");
  assert_eq!(model.finish().unwrap_err().code(), "retirement_journal_model_failed");
}

#[test]
fn malformed_retirement_records_are_rejected_before_typed_iteration() {
  let algorithm = HashAlgorithm::Blake3_256;
  let bad_reason_record = {
    let mut record = record_from_fixture(algorithm, 5_000, 0x30);
    put_u16(&mut record, 4, 6);
    record
  };
  let bytes = journal_segment(algorithm, 1, 401, true, None, &[bad_reason_record]);
  assert_eq!(decode_retirement_journal_segment_v1(&bytes, algorithm).unwrap_err().code(), "retirement_record_fields");

  let mut first = record_from_fixture(algorithm, 5_000, 0x32);
  let mut second = record_from_fixture(algorithm, 5_000, 0x31);
  put_u16(&mut first, 4, 2);
  put_u16(&mut second, 4, 3);
  let bytes = journal_segment(algorithm, 1, 401, true, None, &[first, second]);
  assert_eq!(decode_retirement_journal_segment_v1(&bytes, algorithm).unwrap_err().code(), "retirement_record_order");
}

#[test]
fn existing_corrupt_evidence_reader_is_the_single_typed_evidence_contract() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let bytes = corrupt_evidence_fixture(algorithm);
    let AuditArtifactV1::CorruptEvidence(evidence) = decode_audit_artifact(&bytes, algorithm).unwrap() else {
      panic!("fixture must decode as typed corrupt evidence");
    };
    assert_eq!(evidence.database_id, (0x31u8..=0x40).collect::<Vec<_>>());
    assert!(evidence.detected_at_ms > 0);
    assert!(!evidence.context.is_empty());
    assert!(evidence.evidence_count > 0);
    assert_eq!(evidence.evidence_hashes.len(), evidence.evidence_count as usize * algorithm.hash_length());
  }
}
