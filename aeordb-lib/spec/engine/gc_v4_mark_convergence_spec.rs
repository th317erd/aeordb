use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::gc_mark::{
  GcMarkArtifactV1, MarkMutationJournalSegmentWriteV1, MarkMutationOperationV1, MarkMutationRecordWriteV1, decode_gc_mark_artifact,
  encode_mark_mutation_journal_segment, mark_mutation_journal_records_v1, validate_mark_mutation_journal_chain,
};
use aeordb::engine::v4::gc_mark_convergence::{
  MarkMutationJournalBufferOptionsV1, MarkMutationJournalChainStartV1, MarkMutationJournalDurabilityReceiptV1,
  MarkMutationJournalDurableSinkV1, MarkMutationJournalOwnerErrorV1, MarkMutationJournalOwnerV1, MarkMutationJournalSinkErrorV1,
  MarkMutationObservationV1, PreparedMarkMutationJournalSegmentV1,
};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture_label(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("mark convergence fixtures use only frozen hash profiles"),
  }
}

fn sequence<const N: usize>(start: u8) -> [u8; N] {
  let mut bytes = [0u8; N];
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = start.wrapping_add(u8::try_from(index).unwrap());
  }
  bytes
}

fn repeated_hash(algorithm: HashAlgorithm, value: u8) -> Vec<u8> {
  vec![value; algorithm.hash_length()]
}

fn physical_incarnation(algorithm: HashAlgorithm, value: u8, write_sequence: u64) -> Vec<u8> {
  let width = algorithm.hash_length();
  let mut bytes = Vec::with_capacity(24 + 2 * width);
  bytes.extend_from_slice(&repeated_hash(algorithm, value));
  bytes.extend_from_slice(&repeated_hash(algorithm, value.wrapping_add(1)));
  bytes.extend_from_slice(&(4096 + write_sequence * 512).to_le_bytes());
  bytes.extend_from_slice(&write_sequence.to_le_bytes());
  bytes.extend_from_slice(&256u32.to_le_bytes());
  bytes.push(2);
  bytes.push(1);
  bytes.extend_from_slice(&0u16.to_le_bytes());
  bytes
}

struct MutationValues {
  mutation_id: Vec<u8>,
  root_before: Vec<u8>,
  root_after: Vec<u8>,
  logical_key: Vec<u8>,
  incarnation: Vec<u8>,
}

fn mutation_values(algorithm: HashAlgorithm, value: u8, write_sequence: u64) -> MutationValues {
  MutationValues {
    mutation_id: repeated_hash(algorithm, value),
    root_before: repeated_hash(algorithm, value.wrapping_add(1)),
    root_after: repeated_hash(algorithm, value.wrapping_add(2)),
    logical_key: repeated_hash(algorithm, value.wrapping_add(3)),
    incarnation: physical_incarnation(algorithm, value.wrapping_add(4), write_sequence),
  }
}

fn mutation_record<'a>(
  publication_sequence: u64,
  values: &'a MutationValues,
  operation: MarkMutationOperationV1,
) -> MarkMutationRecordWriteV1<'a> {
  MarkMutationRecordWriteV1 {
    publication_sequence,
    mutation_id: &values.mutation_id,
    root_before: &values.root_before,
    root_after: &values.root_after,
    published_logical_key: &values.logical_key,
    new_incarnation: &values.incarnation,
    operation,
  }
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap())
}

#[derive(Debug)]
struct InjectedSinkFailure;

impl std::fmt::Display for InjectedSinkFailure {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("injected mark mutation sink failure")
  }
}

impl std::error::Error for InjectedSinkFailure {}

#[derive(Default)]
struct RecordingSink {
  attempts: Vec<Vec<u8>>,
  publications: Vec<Vec<u8>>,
  next_publication_sequence: u64,
  fail_next: bool,
  dishonest_receipt: bool,
  receipt_sequence_override: Option<u64>,
}

impl MarkMutationJournalDurableSinkV1 for RecordingSink {
  fn publish_mark_mutation_segment_synced(
    &mut self,
    segment: &PreparedMarkMutationJournalSegmentV1<'_>,
  ) -> Result<MarkMutationJournalDurabilityReceiptV1, MarkMutationJournalSinkErrorV1> {
    self.attempts.push(segment.value.to_vec());
    if self.fail_next {
      self.fail_next = false;
      return Err(MarkMutationJournalSinkErrorV1::new("injected_mark_sink", InjectedSinkFailure));
    }
    self.next_publication_sequence += 1;
    self.publications.push(segment.value.to_vec());
    Ok(MarkMutationJournalDurabilityReceiptV1 {
      artifact_key: if self.dishonest_receipt { vec![0xa5; segment.artifact_key.len()] } else { segment.artifact_key.to_vec() },
      stored_value_length: segment.value.len() as u32,
      hard_publication_sequence: self.receipt_sequence_override.unwrap_or(self.next_publication_sequence),
    })
  }
}

fn journal_owner<'a>(
  algorithm: HashAlgorithm,
  cancellation: &'a CancellationToken,
  memory: &MemoryCoordinator,
  options: MarkMutationJournalBufferOptionsV1,
) -> MarkMutationJournalOwnerV1<'a> {
  MarkMutationJournalOwnerV1::new_chain(
    MarkMutationJournalChainStartV1 {
      algorithm,
      database_id: sequence(0x31),
      run_id: sequence(0x51),
      generation: 77,
      captured_publication_sequence: 10,
      options,
      cancellation,
    },
    memory,
  )
  .unwrap()
}

#[test]
fn mutation_writer_matches_independent_both_width_fixtures_and_iterator_fields() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let fixture = fs::read(fixture_root().join(format!("agca-{}-mark-mutation-journal-reset.bin", fixture_label(algorithm)))).unwrap();
    let GcMarkArtifactV1::MutationJournal(journal) = decode_gc_mark_artifact(&fixture, algorithm).unwrap() else {
      panic!("expected mutation journal fixture");
    };
    let records = mark_mutation_journal_records_v1(&journal, algorithm).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].publication_sequence, 800);
    assert_eq!(records[1].publication_sequence, 801);
    assert_eq!(records[0].operation, MarkMutationOperationV1::Create);
    assert_eq!(records[1].operation, MarkMutationOperationV1::Replace);
    assert_eq!(records[0].new_incarnation.write_sequence, 701);
    assert_eq!(records[1].new_incarnation.write_sequence, 702);

    let writes = records
      .iter()
      .map(|record| MarkMutationRecordWriteV1 {
        publication_sequence: record.publication_sequence,
        mutation_id: record.mutation_id,
        root_before: record.root_before,
        root_after: record.root_after,
        published_logical_key: record.published_logical_key,
        new_incarnation: record.new_incarnation_bytes,
        operation: record.operation,
      })
      .collect::<Vec<_>>();
    let database_id: [u8; 16] = journal.database_id.try_into().unwrap();
    let run_id: [u8; 16] = journal.run_id.try_into().unwrap();
    let encoded = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
      hash_algorithm: algorithm,
      database_id: &database_id,
      run_id: &run_id,
      generation: journal.generation,
      segment_ordinal: journal.segment_sequence,
      previous_segment_hash: None,
      records: &writes,
    })
    .unwrap();
    assert_eq!(encoded.value, fixture);
    assert_eq!(encoded.key, journal.key);
  }
}

#[test]
fn segment_chain_allows_a_publication_to_span_bounded_segments_but_not_regress() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = sequence(0x31);
  let run_id = sequence(0x51);
  let first_values = mutation_values(algorithm, 0x11, 10);
  let second_values = mutation_values(algorithm, 0x21, 11);
  let first_record = mutation_record(20, &first_values, MarkMutationOperationV1::Create);
  let second_record = mutation_record(20, &second_values, MarkMutationOperationV1::Replace);
  let first = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 40,
    segment_ordinal: 1,
    previous_segment_hash: None,
    records: &[first_record],
  })
  .unwrap();
  let second = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 40,
    segment_ordinal: 2,
    previous_segment_hash: Some(&first.key),
    records: &[second_record],
  })
  .unwrap();
  let GcMarkArtifactV1::MutationJournal(first) = decode_gc_mark_artifact(&first.value, algorithm).unwrap() else {
    panic!("expected first mutation segment");
  };
  let GcMarkArtifactV1::MutationJournal(second) = decode_gc_mark_artifact(&second.value, algorithm).unwrap() else {
    panic!("expected second mutation segment");
  };
  validate_mark_mutation_journal_chain(&first, &second).unwrap();

  let regressing_values = mutation_values(algorithm, 0x10, 12);
  let regressing_record = mutation_record(20, &regressing_values, MarkMutationOperationV1::Repair);
  let regressing = encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 40,
    segment_ordinal: 2,
    previous_segment_hash: Some(&first.key),
    records: &[regressing_record],
  })
  .unwrap();
  let GcMarkArtifactV1::MutationJournal(regressing) = decode_gc_mark_artifact(&regressing.value, algorithm).unwrap() else {
    panic!("expected regressing mutation segment");
  };
  assert!(validate_mark_mutation_journal_chain(&first, &regressing).is_err());
}

#[test]
fn mutation_writer_rejects_empty_malformed_or_noncanonical_input() {
  let algorithm = HashAlgorithm::Blake3_256;
  let database_id = sequence(0x31);
  let run_id = sequence(0x51);
  assert!(encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 1,
    segment_ordinal: 1,
    previous_segment_hash: None,
    records: &[],
  })
  .is_err());

  let values = mutation_values(algorithm, 0x11, 10);
  let mut wrong_width = mutation_record(1, &values, MarkMutationOperationV1::Create);
  wrong_width.mutation_id = &values.mutation_id[..31];
  assert!(encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 1,
    segment_ordinal: 1,
    previous_segment_hash: None,
    records: &[wrong_width],
  })
  .is_err());

  let first = mutation_record(2, &values, MarkMutationOperationV1::Create);
  let second = mutation_record(1, &values, MarkMutationOperationV1::Create);
  assert!(encode_mark_mutation_journal_segment(&MarkMutationJournalSegmentWriteV1 {
    hash_algorithm: algorithm,
    database_id: &database_id,
    run_id: &run_id,
    generation: 1,
    segment_ordinal: 1,
    previous_segment_hash: None,
    records: &[first, second],
  })
  .is_err());
}

#[test]
fn acknowledged_mutations_only_buffer_and_background_flush_publishes_exact_segments() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let options = MarkMutationJournalBufferOptionsV1::new(2, 1024, 4096, 10).unwrap();
    let mut owner = journal_owner(algorithm, &cancellation, &memory, options);
    let mut sink = RecordingSink::default();
    let first = mutation_values(algorithm, 0x11, 11);
    let second = mutation_values(algorithm, 0x21, 12);

    assert_eq!(
      owner.observe_committed(mutation_record(11, &first, MarkMutationOperationV1::Create), 1),
      MarkMutationObservationV1::Buffered { flush_due: false }
    );
    assert_eq!(
      owner.observe_committed(mutation_record(12, &second, MarkMutationOperationV1::Replace), 2),
      MarkMutationObservationV1::Buffered { flush_due: true }
    );
    assert!(sink.attempts.is_empty(), "writer-side observation performed journal I/O");
    assert!(owner.poll(2, &mut sink).unwrap());
    assert_eq!(sink.publications.len(), 1);

    let status = owner.status();
    assert_eq!(status.pending_records, 0);
    assert_eq!(status.durable_records, 2);
    assert_eq!(status.durable_through_publication_sequence, 12);
    assert!(!status.incomplete);
    assert!(!status.failed);
    let GcMarkArtifactV1::MutationJournal(segment) = decode_gc_mark_artifact(&sink.publications[0], algorithm).unwrap() else {
      panic!("expected published mutation segment");
    };
    assert_eq!(segment.record_count, 2);
    assert_eq!(segment.first_sequence, 11);
    assert_eq!(segment.last_sequence, 12);
  }
}

#[test]
fn same_publication_tail_time_flush_and_capacity_are_bounded_exactly() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let options = MarkMutationJournalBufferOptionsV1::new(8, 2048, 4096, 10).unwrap();
  let mut owner = journal_owner(algorithm, &cancellation, &memory, options);
  let first = mutation_values(algorithm, 0x11, 11);
  let same_publication_tail = mutation_values(algorithm, 0x21, 12);
  assert!(matches!(
    owner.observe_committed(mutation_record(11, &first, MarkMutationOperationV1::Create), 1),
    MarkMutationObservationV1::Buffered { flush_due: false }
  ));
  assert!(matches!(
    owner.observe_committed(mutation_record(11, &same_publication_tail, MarkMutationOperationV1::Replace), 2),
    MarkMutationObservationV1::Buffered { flush_due: false }
  ));
  let mut sink = RecordingSink::default();
  assert!(!owner.poll(10, &mut sink).unwrap());
  assert!(owner.poll(11, &mut sink).unwrap());
  let GcMarkArtifactV1::MutationJournal(segment) = decode_gc_mark_artifact(&sink.publications[0], algorithm).unwrap() else {
    panic!("expected time-flushed mutation segment");
  };
  assert_eq!(segment.first_sequence, 11);
  assert_eq!(segment.last_sequence, 11);

  let capacity_options = MarkMutationJournalBufferOptionsV1::new(8, 400, 400, 30_000).unwrap();
  let capacity_cancellation = CancellationToken::new();
  let mut capacity_owner = journal_owner(algorithm, &capacity_cancellation, &memory, capacity_options);
  assert!(matches!(
    capacity_owner.observe_committed(mutation_record(11, &first, MarkMutationOperationV1::Create), 1),
    MarkMutationObservationV1::Buffered { .. }
  ));
  assert!(matches!(
    capacity_owner.observe_committed(mutation_record(11, &same_publication_tail, MarkMutationOperationV1::Replace), 2),
    MarkMutationObservationV1::RunIncomplete { code: "mark_mutation_capacity", .. }
  ));
  assert_eq!(capacity_owner.status().pending_records, 1);
}

#[test]
fn bounded_owner_splits_same_publication_across_exactly_chained_segments() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let options = MarkMutationJournalBufferOptionsV1::new(8, 400, 1024, 30_000).unwrap();
  let mut owner = journal_owner(algorithm, &cancellation, &memory, options);
  let first = mutation_values(algorithm, 0x11, 11);
  let second = mutation_values(algorithm, 0x21, 12);
  let third = mutation_values(algorithm, 0x31, 13);
  for (record, monotonic_now_ms) in [
    (mutation_record(11, &first, MarkMutationOperationV1::Create), 1),
    (mutation_record(11, &second, MarkMutationOperationV1::Replace), 2),
    (mutation_record(12, &third, MarkMutationOperationV1::Promote), 3),
  ] {
    assert!(matches!(owner.observe_committed(record, monotonic_now_ms), MarkMutationObservationV1::Buffered { .. }));
  }
  let mut sink = RecordingSink::default();
  assert!(owner.flush(&mut sink).unwrap());
  assert_eq!(owner.status().pending_records, 2);
  assert!(owner.flush(&mut sink).unwrap());
  assert_eq!(owner.status().pending_records, 1);
  assert!(owner.flush(&mut sink).unwrap());
  assert_eq!(owner.status().pending_records, 0);
  assert_eq!(sink.publications.len(), 3);

  let decoded = sink
    .publications
    .iter()
    .map(|bytes| match decode_gc_mark_artifact(bytes, algorithm).unwrap() {
      GcMarkArtifactV1::MutationJournal(segment) => segment,
      GcMarkArtifactV1::Checkpoint(_) => panic!("owner emitted a checkpoint"),
    })
    .collect::<Vec<_>>();
  validate_mark_mutation_journal_chain(&decoded[0], &decoded[1]).unwrap();
  validate_mark_mutation_journal_chain(&decoded[1], &decoded[2]).unwrap();
  assert_eq!((decoded[0].first_sequence, decoded[0].last_sequence), (11, 11));
  assert_eq!((decoded[1].first_sequence, decoded[1].last_sequence), (11, 11));
  assert_eq!((decoded[2].first_sequence, decoded[2].last_sequence), (12, 12));
}

#[test]
fn owner_construction_rejects_invalid_bounds_identity_cancellation_and_memory() {
  assert!(MarkMutationJournalBufferOptionsV1::new(0, 1024, 4096, 1).is_err());
  assert!(MarkMutationJournalBufferOptionsV1::new(1, 0, 4096, 1).is_err());
  assert!(MarkMutationJournalBufferOptionsV1::new(1, 1024, 512, 1).is_err());
  assert!(MarkMutationJournalBufferOptionsV1::new(1, 1024, 17 * 1024 * 1024, 1).is_err());

  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory_coordinator();
  let options = MarkMutationJournalBufferOptionsV1::new(1, 1024, 4096, 1).unwrap();
  for (database_id, run_id, generation, captured_publication_sequence) in [
    ([0; 16], sequence(0x51), 1, 1),
    (sequence(0x31), [0; 16], 1, 1),
    (sequence(0x31), sequence(0x51), 0, 1),
    (sequence(0x31), sequence(0x51), 1, 0),
  ] {
    let cancellation = CancellationToken::new();
    assert!(MarkMutationJournalOwnerV1::new_chain(
      MarkMutationJournalChainStartV1 {
        algorithm,
        database_id,
        run_id,
        generation,
        captured_publication_sequence,
        options,
        cancellation: &cancellation,
      },
      &memory,
    )
    .is_err());
  }
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  assert!(matches!(
    MarkMutationJournalOwnerV1::new_chain(
      MarkMutationJournalChainStartV1 {
        algorithm,
        database_id: sequence(0x31),
        run_id: sequence(0x51),
        generation: 1,
        captured_publication_sequence: 1,
        options,
        cancellation: &cancellation,
      },
      &memory,
    ),
    Err(MarkMutationJournalOwnerErrorV1::Canceled)
  ));
}

#[test]
fn malformed_records_and_clock_regression_preserve_the_first_failure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory_coordinator();
  let options = MarkMutationJournalBufferOptionsV1::new(8, 1024, 4096, 30_000).unwrap();
  let cancellation = CancellationToken::new();
  let mut malformed_owner = journal_owner(algorithm, &cancellation, &memory, options);
  let values = mutation_values(algorithm, 0x11, 11);
  let mut malformed = mutation_record(11, &values, MarkMutationOperationV1::Create);
  malformed.mutation_id = &values.mutation_id[..31];
  let malformed_observation = malformed_owner.observe_committed(malformed, 1);
  let (first_code, first_message) = match malformed_observation {
    MarkMutationObservationV1::RunIncomplete { code, message } => (code, message),
    MarkMutationObservationV1::Buffered { .. } => panic!("malformed mutation was admitted"),
  };
  assert_eq!(malformed_owner.status().pending_records, 0);
  assert_eq!(malformed_owner.status().incomplete_code, Some(first_code));
  assert_eq!(
    malformed_owner.observe_committed(mutation_record(11, &values, MarkMutationOperationV1::Create), 2),
    MarkMutationObservationV1::RunIncomplete { code: first_code, message: first_message }
  );

  let clock_cancellation = CancellationToken::new();
  let mut clock_owner = journal_owner(algorithm, &clock_cancellation, &memory, options);
  assert!(matches!(
    clock_owner.observe_committed(mutation_record(11, &values, MarkMutationOperationV1::Create), 2),
    MarkMutationObservationV1::Buffered { .. }
  ));
  let mut sink = RecordingSink::default();
  assert!(matches!(clock_owner.poll(1, &mut sink), Err(MarkMutationJournalOwnerErrorV1::ClockRegression)));
  assert_eq!(clock_owner.status().incomplete_code, Some("mark_mutation_clock_regression"));
  assert!(sink.attempts.is_empty());
}

#[test]
fn soft_sink_failure_retains_exact_bytes_and_never_restores_mark_completeness() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let options = MarkMutationJournalBufferOptionsV1::new(1, 1024, 4096, 30_000).unwrap();
  let mut owner = journal_owner(algorithm, &cancellation, &memory, options);
  let values = mutation_values(algorithm, 0x11, 11);
  assert_eq!(
    owner.observe_committed(mutation_record(11, &values, MarkMutationOperationV1::Create), 1),
    MarkMutationObservationV1::Buffered { flush_due: true }
  );
  let mut sink = RecordingSink { fail_next: true, ..RecordingSink::default() };
  assert!(matches!(owner.flush(&mut sink), Err(MarkMutationJournalOwnerErrorV1::Sink { .. })));
  let retained = sink.attempts[0].clone();
  assert_eq!(owner.status().pending_records, 1);
  assert!(owner.status().incomplete);
  assert!(!owner.status().failed);

  let later = mutation_values(algorithm, 0x21, 12);
  assert!(matches!(
    owner.observe_committed(mutation_record(12, &later, MarkMutationOperationV1::Replace), 2),
    MarkMutationObservationV1::RunIncomplete { code: "mark_mutation_sink", .. }
  ));
  assert!(owner.flush(&mut sink).unwrap());
  assert_eq!(sink.attempts[1], retained);
  assert_eq!(sink.publications, [retained]);
  assert_eq!(owner.status().pending_records, 0);
  assert!(owner.status().incomplete, "retry must not turn an incomplete mark back into reclaim authority");
}

#[test]
fn background_memory_pressure_retains_evidence_and_permanently_invalidates_reclaim() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let normal_policy = MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap();
  let options = MarkMutationJournalBufferOptionsV1::new(1, 1024, 4096, 30_000).unwrap();
  let mut owner = journal_owner(algorithm, &cancellation, &memory, options);
  let values = mutation_values(algorithm, 0x11, 11);
  assert!(matches!(
    owner.observe_committed(mutation_record(11, &values, MarkMutationOperationV1::Create), 1),
    MarkMutationObservationV1::Buffered { flush_due: true }
  ));
  memory.reconfigure_policy(MemoryPolicy::new(1, 2 * 1024 * 1024, 1, 1024 * 1024).unwrap()).unwrap();
  let mut sink = RecordingSink::default();
  assert!(matches!(owner.flush(&mut sink), Err(MarkMutationJournalOwnerErrorV1::Memory(_))));
  assert_eq!(owner.status().pending_records, 1);
  assert_eq!(owner.status().incomplete_code, Some("mark_mutation_memory"));
  assert!(sink.attempts.is_empty());

  memory.reconfigure_policy(normal_policy).unwrap();
  assert!(owner.flush(&mut sink).unwrap());
  assert_eq!(owner.status().pending_records, 0);
  assert!(owner.status().incomplete);
  assert!(matches!(
    owner.observe_committed(mutation_record(12, &mutation_values(algorithm, 0x21, 12), MarkMutationOperationV1::Replace), 2),
    MarkMutationObservationV1::RunIncomplete { code: "mark_mutation_memory", .. }
  ));
}

#[test]
fn gaps_cancellation_pressure_and_dishonest_receipts_fail_the_run_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let options = MarkMutationJournalBufferOptionsV1::new(1, 1024, 1024, 30_000).unwrap();
  let mut owner = journal_owner(algorithm, &cancellation, &memory, options);
  let skipped = mutation_values(algorithm, 0x11, 12);
  assert!(matches!(
    owner.observe_committed(mutation_record(12, &skipped, MarkMutationOperationV1::Create), 1),
    MarkMutationObservationV1::RunIncomplete { code: "mark_mutation_gap", .. }
  ));
  assert_eq!(owner.status().pending_records, 0);

  let cancellation = CancellationToken::new();
  let mut owner = journal_owner(algorithm, &cancellation, &memory, options);
  cancellation.cancel();
  let values = mutation_values(algorithm, 0x11, 11);
  assert!(matches!(
    owner.observe_committed(mutation_record(11, &values, MarkMutationOperationV1::Create), 1),
    MarkMutationObservationV1::RunIncomplete { code: "mark_mutation_cancelled", .. }
  ));

  let cancellation = CancellationToken::new();
  let mut owner = journal_owner(algorithm, &cancellation, &memory, options);
  assert!(matches!(
    owner.observe_committed(mutation_record(11, &values, MarkMutationOperationV1::Create), 1),
    MarkMutationObservationV1::Buffered { .. }
  ));
  let mut dishonest = RecordingSink { dishonest_receipt: true, ..RecordingSink::default() };
  assert!(matches!(owner.flush(&mut dishonest), Err(MarkMutationJournalOwnerErrorV1::ReceiptMismatch)));
  assert!(owner.status().failed);

  let cancellation = CancellationToken::new();
  let mut owner = journal_owner(algorithm, &cancellation, &memory, options);
  let mut stale_receipt = RecordingSink::default();
  assert!(matches!(
    owner.observe_committed(mutation_record(11, &values, MarkMutationOperationV1::Create), 1),
    MarkMutationObservationV1::Buffered { .. }
  ));
  assert!(owner.flush(&mut stale_receipt).unwrap());
  assert!(matches!(
    owner.observe_committed(mutation_record(12, &mutation_values(algorithm, 0x21, 12), MarkMutationOperationV1::Replace), 2,),
    MarkMutationObservationV1::Buffered { .. }
  ));
  stale_receipt.receipt_sequence_override = Some(owner.status().last_hard_publication_sequence);
  assert!(matches!(owner.flush(&mut stale_receipt), Err(MarkMutationJournalOwnerErrorV1::ReceiptMismatch)));
  assert!(owner.status().failed);

  let tiny_memory = MemoryCoordinator::new(MemoryPolicy::new(1024, 2048, 1, 512).unwrap());
  let tiny_cancellation = CancellationToken::new();
  assert!(MarkMutationJournalOwnerV1::new_chain(
    MarkMutationJournalChainStartV1 {
      algorithm,
      database_id: sequence(0x31),
      run_id: sequence(0x51),
      generation: 77,
      captured_publication_sequence: 10,
      options,
      cancellation: &tiny_cancellation,
    },
    &tiny_memory,
  )
  .is_err());
}

#[test]
fn mark_mutation_runtime_remains_disconnected_from_live_v3_and_service_paths() {
  let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  for relative in [
    "engine/gc.rs",
    "engine/namespace_mutation.rs",
    "engine/storage_engine.rs",
    "engine/task_worker.rs",
    "server/mod.rs",
    "server/routes.rs",
  ] {
    let source = fs::read_to_string(root.join(relative)).unwrap();
    assert!(!source.contains("encode_mark_mutation_journal_segment"), "mark writer escaped into {relative}");
    assert!(!source.contains("mark_mutation_journal_records_v1"), "mark iterator escaped into {relative}");
    assert!(!source.contains("MarkMutationJournalOwnerV1"), "mark owner escaped into {relative}");
  }
}
