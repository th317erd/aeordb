use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::gc::{GcArtifactKindV1, decode_physical_incarnation};
use aeordb::engine::v4::gc_audit::{
  AuditArtifactV1, CorruptGcEvidenceDurabilityReceiptV1, CorruptGcEvidenceDurableSinkV1, CorruptGcEvidenceSinkErrorV1,
  CorruptGcEvidenceWriteV1, GcErrorClassV1, decode_audit_artifact, encode_corrupt_gc_evidence_v1,
};
use aeordb::engine::v4::gc_lineage_recovery::{
  RetirementLineageRecoveryContextV1, RetirementLineageRecoveryDispositionV1, RetirementLineageRecoveryGroupV1,
  RetirementLineageRecoveryIssueV1, RetirementLineageRecoveryObservationV1, RetirementLineageRecoveryReconcilerV1,
};
use aeordb::engine::v4::gc_retirement::{
  PreparedRetirementJournalSegmentV1, RetirementJournalBufferOptionsV1, RetirementJournalDurabilityReceiptV1,
  RetirementJournalDurableSinkV1, RetirementJournalOwnerV1, RetirementJournalSinkErrorV1,
};
use aeordb::engine::v4::gc_state::{RetirementReasonV1, decode_retirement_journal_segment_v1, retirement_journal_records_v1};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("the frozen v4 contract has exactly two hash widths"),
  }
}

fn corrupt_evidence_fixture(algorithm: HashAlgorithm) -> Vec<u8> {
  fs::read(fixture_root().join(format!("agca-{}-corrupt-gc-evidence.bin", algorithm_name(algorithm)))).unwrap()
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap())
}

fn database_id() -> [u8; 16] {
  (0x31u8..=0x40).collect::<Vec<_>>().try_into().unwrap()
}

fn physical_incarnation(algorithm: HashAlgorithm, logical_key_byte: u8, digest_byte: u8, wal_offset: u64, write_sequence: u64) -> Vec<u8> {
  let hash_width = algorithm.hash_length();
  let mut bytes = vec![0u8; 24 + 2 * hash_width];
  bytes[..hash_width].fill(logical_key_byte);
  bytes[hash_width..2 * hash_width].fill(digest_byte);
  bytes[2 * hash_width..2 * hash_width + 8].copy_from_slice(&wal_offset.to_le_bytes());
  bytes[2 * hash_width + 8..2 * hash_width + 16].copy_from_slice(&write_sequence.to_le_bytes());
  bytes[2 * hash_width + 16..2 * hash_width + 20].copy_from_slice(&128u32.to_le_bytes());
  bytes[2 * hash_width + 20] = 3;
  bytes[2 * hash_width + 21] = u8::from(write_sequence > 0);
  bytes
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TestSinkFailure(&'static str);

impl Display for TestSinkFailure {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.write_str(self.0)
  }
}

impl Error for TestSinkFailure {}

#[derive(Default)]
struct RecordingJournalSink {
  publications: Vec<Vec<u8>>,
  next_publication_sequence: u64,
  fail_next: bool,
}

impl RetirementJournalDurableSinkV1 for RecordingJournalSink {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    if self.fail_next {
      self.fail_next = false;
      return Err(RetirementJournalSinkErrorV1::new("injected_journal_failure", TestSinkFailure("journal sink failed")));
    }
    self.next_publication_sequence += 1;
    self.publications.push(segment.value.to_vec());
    Ok(RetirementJournalDurabilityReceiptV1 {
      artifact_key: segment.artifact_key.to_vec(),
      stored_value_length: segment.value.len() as u32,
      hard_publication_sequence: self.next_publication_sequence,
    })
  }
}

#[derive(Default)]
struct RecordingEvidenceSink {
  attempts: Vec<Vec<u8>>,
  publications: Vec<Vec<u8>>,
  next_publication_sequence: u64,
  fail_next: bool,
  wrong_receipt: bool,
}

impl CorruptGcEvidenceDurableSinkV1 for RecordingEvidenceSink {
  fn publish_corrupt_evidence_synced(
    &mut self,
    artifact_key: &[u8],
    value: &[u8],
  ) -> Result<CorruptGcEvidenceDurabilityReceiptV1, CorruptGcEvidenceSinkErrorV1> {
    self.attempts.push(value.to_vec());
    if self.fail_next {
      self.fail_next = false;
      return Err(CorruptGcEvidenceSinkErrorV1::new("injected_evidence_failure", TestSinkFailure("evidence sink failed")));
    }
    self.next_publication_sequence += 1;
    self.publications.push(value.to_vec());
    Ok(CorruptGcEvidenceDurabilityReceiptV1 {
      artifact_key: if self.wrong_receipt { vec![0xA5; artifact_key.len()] } else { artifact_key.to_vec() },
      stored_value_length: value.len() as u32,
      hard_publication_sequence: self.next_publication_sequence,
    })
  }
}

#[derive(Default)]
struct RecordingRecoverySink {
  journal: RecordingJournalSink,
  evidence: RecordingEvidenceSink,
}

impl RetirementJournalDurableSinkV1 for RecordingRecoverySink {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    self.journal.publish_synced(segment)
  }
}

impl CorruptGcEvidenceDurableSinkV1 for RecordingRecoverySink {
  fn publish_corrupt_evidence_synced(
    &mut self,
    artifact_key: &[u8],
    value: &[u8],
  ) -> Result<CorruptGcEvidenceDurabilityReceiptV1, CorruptGcEvidenceSinkErrorV1> {
    self.evidence.publish_corrupt_evidence_synced(artifact_key, value)
  }
}

fn new_owner<'a>(
  algorithm: HashAlgorithm,
  cancellation: &'a CancellationToken,
  memory: &MemoryCoordinator,
) -> RetirementJournalOwnerV1<'a> {
  RetirementJournalOwnerV1::new_chain(
    algorithm,
    database_id(),
    1,
    401,
    RetirementJournalBufferOptionsV1::new(4_096, 1024 * 1024, 30_000),
    cancellation,
    memory,
  )
  .unwrap()
}

fn context() -> RetirementLineageRecoveryContextV1 {
  RetirementLineageRecoveryContextV1 {
    database_id: database_id(),
    run_id: [0x71; 16],
    generation: 500,
    detected_at_ms: 1_700_000_500_000,
    recovery_publication_sequence: 9_000,
  }
}

#[test]
fn corrupt_evidence_writer_matches_the_independent_fixtures_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let expected = corrupt_evidence_fixture(algorithm);
    let AuditArtifactV1::CorruptEvidence(decoded) = decode_audit_artifact(&expected, algorithm).unwrap() else {
      panic!("fixture must remain corrupt-GC evidence");
    };
    let encoded = encode_corrupt_gc_evidence_v1(
      &CorruptGcEvidenceWriteV1 {
        database_id: decoded.database_id.try_into().unwrap(),
        evidence_id: decoded.evidence_id.try_into().unwrap(),
        generation: decoded.generation,
        detected_at_ms: decoded.detected_at_ms,
        error_class: decoded.error_class,
        observed_entry_type: decoded.observed_entry_type,
        observed_artifact_kind: decoded.observed_artifact_kind,
        physical_range: decoded.physical_range,
        write_sequence: decoded.write_sequence,
        expected_hash: decoded.expected_hash,
        observed_hash: decoded.observed_hash,
        run_id: decoded.run_id.map(|value| value.try_into().unwrap()),
        control_kind: decoded.control_kind,
        control_identity_digest: decoded.control_identity_digest,
        context: decoded.context,
        evidence_hashes: decoded.evidence_hashes,
      },
      algorithm,
    )
    .unwrap();
    assert_eq!(encoded.value, expected);
    assert_eq!(encoded.key, decoded.key);
  }
}

#[test]
fn corrupt_evidence_writer_rejects_unbound_noncanonical_and_amplified_fields() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = corrupt_evidence_fixture(algorithm);
  let AuditArtifactV1::CorruptEvidence(decoded) = decode_audit_artifact(&expected, algorithm).unwrap() else {
    panic!("fixture must remain corrupt-GC evidence");
  };
  let base = CorruptGcEvidenceWriteV1 {
    database_id: decoded.database_id.try_into().unwrap(),
    evidence_id: decoded.evidence_id.try_into().unwrap(),
    generation: decoded.generation,
    detected_at_ms: decoded.detected_at_ms,
    error_class: decoded.error_class,
    observed_entry_type: decoded.observed_entry_type,
    observed_artifact_kind: decoded.observed_artifact_kind,
    physical_range: decoded.physical_range,
    write_sequence: decoded.write_sequence,
    expected_hash: decoded.expected_hash,
    observed_hash: decoded.observed_hash,
    run_id: decoded.run_id.map(|value| value.try_into().unwrap()),
    control_kind: decoded.control_kind,
    control_identity_digest: decoded.control_identity_digest,
    context: decoded.context,
    evidence_hashes: decoded.evidence_hashes,
  };
  assert_eq!(
    encode_corrupt_gc_evidence_v1(&CorruptGcEvidenceWriteV1 { context: &[], ..base }, algorithm).unwrap_err().code(),
    "corrupt_evidence_context"
  );
  let mut reversed_hashes = decoded.evidence_hashes.to_vec();
  let hash_width = algorithm.hash_length();
  let (first, second) = reversed_hashes.split_at_mut(hash_width);
  first.swap_with_slice(second);
  assert_eq!(
    encode_corrupt_gc_evidence_v1(&CorruptGcEvidenceWriteV1 { evidence_hashes: &reversed_hashes, ..base }, algorithm).unwrap_err().code(),
    "corrupt_evidence_order"
  );
  assert_eq!(
    encode_corrupt_gc_evidence_v1(&CorruptGcEvidenceWriteV1 { expected_hash: Some(&[1; 3]), ..base }, algorithm).unwrap_err().code(),
    "corrupt_evidence_hash"
  );
  assert_eq!(
    encode_corrupt_gc_evidence_v1(
      &CorruptGcEvidenceWriteV1 { control_kind: Some(GcArtifactKindV1::QuarantineActiveControl), control_identity_digest: None, ..base },
      algorithm,
    )
    .unwrap_err()
    .code(),
    "corrupt_evidence_fields"
  );
  assert_eq!(
    encode_corrupt_gc_evidence_v1(&CorruptGcEvidenceWriteV1 { physical_range: Some((u64::MAX, 1)), ..base }, algorithm).unwrap_err().code(),
    "corrupt_evidence_fields"
  );
  assert_eq!(
    encode_corrupt_gc_evidence_v1(&CorruptGcEvidenceWriteV1 { run_id: Some([0; 16]), ..base }, algorithm).unwrap_err().code(),
    "corrupt_evidence_run"
  );
  let mut amplified_hashes = Vec::new();
  for value in 1..=65u8 {
    amplified_hashes.extend_from_slice(&[value; 32]);
  }
  assert_eq!(
    encode_corrupt_gc_evidence_v1(&CorruptGcEvidenceWriteV1 { evidence_hashes: &amplified_hashes, ..base }, algorithm).unwrap_err().code(),
    "corrupt_evidence_count"
  );
}

#[test]
fn missing_lower_lineage_is_hard_published_as_repair_without_reclaim_authority() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut owner = new_owner(algorithm, &cancellation, &memory);
    let mut sink = RecordingRecoverySink::default();
    let old_a = physical_incarnation(algorithm, 0x41, 0x11, 10_000, 7);
    let old_b = physical_incarnation(algorithm, 0x41, 0x12, 20_000, 8);
    let selected = physical_incarnation(algorithm, 0x41, 0x13, 30_000, 9);
    let observations = [
      RetirementLineageRecoveryObservationV1 { incarnation: &old_a, retirement_present: false },
      RetirementLineageRecoveryObservationV1 { incarnation: &old_b, retirement_present: false },
      RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: false },
    ];
    let mut recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context(), &cancellation).unwrap();

    let outcome = recovery
      .recover_group(
        RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
        100,
        &mut owner,
        &mut sink,
      )
      .unwrap();

    assert_eq!(outcome.disposition, RetirementLineageRecoveryDispositionV1::Synthesized { record_count: 2 });
    assert!(!outcome.authorizes_reclaim());
    assert!(outcome.evidence_receipt.is_some());
    assert_eq!(sink.journal.publications.len(), 1);
    assert_eq!(sink.evidence.publications.len(), 1);
    let segment = decode_retirement_journal_segment_v1(&sink.journal.publications[0], algorithm).unwrap();
    let records: Vec<_> = retirement_journal_records_v1(&segment, algorithm).unwrap().map(Result::unwrap).collect();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| record.reason == RetirementReasonV1::Repair));
    let selected_decoded = decode_physical_incarnation(&selected, algorithm).unwrap();
    assert!(records.iter().all(|record| record.replacement == selected_decoded));
    let AuditArtifactV1::CorruptEvidence(evidence) = decode_audit_artifact(&sink.evidence.publications[0], algorithm).unwrap() else {
      panic!("recovery evidence must use the existing typed contract");
    };
    assert_eq!(evidence.error_class, GcErrorClassV1::MissingEdge);
  }
}

#[test]
fn already_covered_lineage_is_a_noop_and_still_never_authorizes_reclaim() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = new_owner(algorithm, &cancellation, &memory);
  let mut sink = RecordingRecoverySink::default();
  let old = physical_incarnation(algorithm, 0x41, 0x11, 10_000, 7);
  let selected = physical_incarnation(algorithm, 0x41, 0x12, 20_000, 8);
  let observations = [
    RetirementLineageRecoveryObservationV1 { incarnation: &old, retirement_present: true },
    RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: false },
  ];
  let mut recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context(), &cancellation).unwrap();
  let outcome = recovery
    .recover_group(
      RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
      100,
      &mut owner,
      &mut sink,
    )
    .unwrap();
  assert_eq!(outcome.disposition, RetirementLineageRecoveryDispositionV1::AlreadyComplete);
  assert!(!outcome.authorizes_reclaim());
  assert!(sink.journal.publications.is_empty());
  assert!(sink.evidence.publications.is_empty());
}

#[test]
fn selected_authority_disagreement_equal_sequence_and_overlap_are_protected() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cases = [
    (10_000, 7, 20_000, 8, RetirementLineageRecoveryIssueV1::SelectedAuthorityMismatch, GcErrorClassV1::WrongIdentity),
    (10_000, 8, 20_000, 8, RetirementLineageRecoveryIssueV1::AmbiguousHighestSequence, GcErrorClassV1::AmbiguousControl),
    (10_000, 8, 10_064, 7, RetirementLineageRecoveryIssueV1::OverlappingExtent, GcErrorClassV1::BoundsOrOverlap),
  ];
  for (selected_offset, selected_sequence, other_offset, other_sequence, issue, error_class) in cases {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut owner = new_owner(algorithm, &cancellation, &memory);
    let mut sink = RecordingRecoverySink::default();
    let selected = physical_incarnation(algorithm, 0x41, 0x11, selected_offset, selected_sequence);
    let other = physical_incarnation(algorithm, 0x41, 0x12, other_offset, other_sequence);
    let observations = [
      RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: false },
      RetirementLineageRecoveryObservationV1 { incarnation: &other, retirement_present: false },
    ];
    let mut recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context(), &cancellation).unwrap();
    let outcome = recovery
      .recover_group(
        RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
        100,
        &mut owner,
        &mut sink,
      )
      .unwrap();
    assert_eq!(outcome.disposition, RetirementLineageRecoveryDispositionV1::Protected { issue });
    assert!(!outcome.authorizes_reclaim());
    assert!(sink.journal.publications.is_empty());
    let AuditArtifactV1::CorruptEvidence(evidence) = decode_audit_artifact(&sink.evidence.publications[0], algorithm).unwrap() else {
      panic!("protected lineage must emit typed evidence");
    };
    assert_eq!(evidence.error_class, error_class);
  }
}

#[test]
fn malformed_order_identity_and_group_limits_emit_evidence_without_journal_mutation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let selected = physical_incarnation(algorithm, 0x41, 0x11, 20_000, 8);
  let malformed = vec![0xFF; 7];
  let wrong_key = physical_incarnation(algorithm, 0x42, 0x12, 30_000, 9);
  let older = physical_incarnation(algorithm, 0x41, 0x10, 10_000, 7);
  let scenarios: Vec<(Vec<RetirementLineageRecoveryObservationV1<'_>>, RetirementLineageRecoveryIssueV1)> = vec![
    (
      vec![
        RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: false },
        RetirementLineageRecoveryObservationV1 { incarnation: &malformed, retirement_present: false },
      ],
      RetirementLineageRecoveryIssueV1::MalformedIncarnation,
    ),
    (
      vec![
        RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: false },
        RetirementLineageRecoveryObservationV1 { incarnation: &wrong_key, retirement_present: false },
      ],
      RetirementLineageRecoveryIssueV1::WrongLogicalIdentity,
    ),
    (
      vec![
        RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: false },
        RetirementLineageRecoveryObservationV1 { incarnation: &older, retirement_present: false },
      ],
      RetirementLineageRecoveryIssueV1::NoncanonicalObservationOrder,
    ),
    (
      vec![RetirementLineageRecoveryObservationV1 { incarnation: &older, retirement_present: false }],
      RetirementLineageRecoveryIssueV1::SelectedIncarnationMissing,
    ),
    (
      vec![RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: true }],
      RetirementLineageRecoveryIssueV1::SelectedIncarnationRetired,
    ),
  ];
  for (observations, expected_issue) in scenarios {
    let mut owner = new_owner(algorithm, &cancellation, &memory);
    let mut sink = RecordingRecoverySink::default();
    let mut recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context(), &cancellation).unwrap();
    let outcome = recovery
      .recover_group(
        RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
        100,
        &mut owner,
        &mut sink,
      )
      .unwrap();
    assert_eq!(outcome.disposition, RetirementLineageRecoveryDispositionV1::Protected { issue: expected_issue });
    assert!(sink.journal.publications.is_empty());
    assert_eq!(sink.evidence.publications.len(), 1);
  }

  let too_many: Vec<_> =
    (0..65).map(|index| physical_incarnation(algorithm, 0x41, index as u8 + 1, 40_000 + index * 256, 1 + index)).collect();
  let too_many_observations: Vec<_> =
    too_many.iter().map(|incarnation| RetirementLineageRecoveryObservationV1 { incarnation, retirement_present: false }).collect();
  let mut owner = new_owner(algorithm, &cancellation, &memory);
  let mut sink = RecordingRecoverySink::default();
  let mut recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context(), &cancellation).unwrap();
  let outcome = recovery
    .recover_group(
      RetirementLineageRecoveryGroupV1 { selected_incarnation: too_many.last().unwrap(), observations: &too_many_observations },
      100,
      &mut owner,
      &mut sink,
    )
    .unwrap();
  assert_eq!(
    outcome.disposition,
    RetirementLineageRecoveryDispositionV1::Protected { issue: RetirementLineageRecoveryIssueV1::IncarnationLimit }
  );
  assert!(sink.journal.publications.is_empty());
  assert_eq!(sink.evidence.publications.len(), 1);
}

#[test]
fn cancellation_and_sink_failures_never_turn_incomplete_recovery_into_success() {
  let algorithm = HashAlgorithm::Blake3_256;
  let old = physical_incarnation(algorithm, 0x41, 0x11, 10_000, 7);
  let selected = physical_incarnation(algorithm, 0x41, 0x12, 20_000, 8);
  let observations = [
    RetirementLineageRecoveryObservationV1 { incarnation: &old, retirement_present: false },
    RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: false },
  ];

  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let memory = memory_coordinator();
  let mut owner = new_owner(algorithm, &cancellation, &memory);
  let mut sink = RecordingRecoverySink::default();
  let mut recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context(), &cancellation).unwrap();
  assert_eq!(
    recovery
      .recover_group(
        RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
        100,
        &mut owner,
        &mut sink,
      )
      .unwrap_err()
      .code(),
    "retirement_lineage_recovery_cancelled"
  );
  assert!(sink.journal.publications.is_empty());
  assert!(sink.evidence.publications.is_empty());

  let cancellation = CancellationToken::new();
  let mut owner = new_owner(algorithm, &cancellation, &memory);
  let mut sink = RecordingRecoverySink {
    evidence: RecordingEvidenceSink { fail_next: true, ..RecordingEvidenceSink::default() },
    ..RecordingRecoverySink::default()
  };
  let mut recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context(), &cancellation).unwrap();
  assert_eq!(
    recovery
      .recover_group(
        RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
        100,
        &mut owner,
        &mut sink,
      )
      .unwrap_err()
      .code(),
    "corrupt_gc_evidence_sink"
  );
  assert!(sink.journal.publications.is_empty());

  let mut owner = new_owner(algorithm, &cancellation, &memory);
  let mut sink = RecordingRecoverySink {
    journal: RecordingJournalSink { fail_next: true, ..RecordingJournalSink::default() },
    ..RecordingRecoverySink::default()
  };
  let mut recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context(), &cancellation).unwrap();
  let error = recovery
    .recover_group(
      RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
      100,
      &mut owner,
      &mut sink,
    )
    .unwrap_err();
  assert_eq!(error.code(), "retirement_journal_sink");
  assert_eq!(error.admitted_records(), 1);
  assert_eq!(sink.evidence.publications.len(), 1);
  assert_eq!(owner.status().pending_records, 1);
}

#[test]
fn dishonest_evidence_receipt_latches_recovery_before_any_retirement_append() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = new_owner(algorithm, &cancellation, &memory);
  let mut sink = RecordingRecoverySink {
    evidence: RecordingEvidenceSink { wrong_receipt: true, ..RecordingEvidenceSink::default() },
    ..RecordingRecoverySink::default()
  };
  let old = physical_incarnation(algorithm, 0x41, 0x11, 10_000, 7);
  let selected = physical_incarnation(algorithm, 0x41, 0x12, 20_000, 8);
  let observations = [
    RetirementLineageRecoveryObservationV1 { incarnation: &old, retirement_present: false },
    RetirementLineageRecoveryObservationV1 { incarnation: &selected, retirement_present: false },
  ];
  let mut recovery = RetirementLineageRecoveryReconcilerV1::new(algorithm, context(), &cancellation).unwrap();
  assert_eq!(
    recovery
      .recover_group(
        RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
        100,
        &mut owner,
        &mut sink,
      )
      .unwrap_err()
      .code(),
    "corrupt_gc_evidence_receipt"
  );
  assert!(sink.journal.publications.is_empty());
  assert_eq!(
    recovery
      .recover_group(
        RetirementLineageRecoveryGroupV1 { selected_incarnation: &selected, observations: &observations },
        100,
        &mut owner,
        &mut sink,
      )
      .unwrap_err()
      .code(),
    "retirement_lineage_recovery_failed"
  );
}

fn rust_sources(root: &Path, sources: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(root).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      rust_sources(&path, sources);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      sources.push(path);
    }
  }
}

#[test]
fn recovery_reconciler_remains_disconnected_from_live_gc_service_and_reclaim_paths() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let recovery_path = source_root.join("engine/v4/gc_lineage_recovery.rs");
  let recovery_source = fs::read_to_string(&recovery_path).unwrap();
  for forbidden in ["engine::gc", "VoidManager", "V4ControlStore", "publish_mutable", "candidate", "sweep", "authorizes_reclaim: true"] {
    assert!(!recovery_source.contains(forbidden), "recovery source contains forbidden live/reclaim token {forbidden}");
  }
  let mut sources = Vec::new();
  rust_sources(&source_root, &mut sources);
  let callers: Vec<_> = sources
    .into_iter()
    .filter(|path| path != &recovery_path)
    .filter(|path| fs::read_to_string(path).unwrap_or_default().contains("RetirementLineageRecoveryReconcilerV1"))
    .map(|path| path.strip_prefix(&source_root).unwrap().to_owned())
    .collect();
  assert!(callers.is_empty(), "recovery reconciler activated outside disconnected P4-2: {callers:?}");
}
