use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::gc_retirement::{
  PreparedRetirementJournalSegmentV1, RetirementJournalBufferOptionsV1, RetirementJournalDurabilityReceiptV1,
  RetirementJournalDurableSinkV1, RetirementJournalOwnerErrorV1, RetirementJournalOwnerV1, RetirementJournalRecordWriteV1,
  RetirementJournalSinkErrorV1,
};
use aeordb::engine::v4::gc_state::{RetirementJournalReferenceModelV1, RetirementReasonV1, decode_retirement_journal_segment_v1};
use tokio_util::sync::CancellationToken;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/gc-artifact-v1")
}

fn fixture(algorithm: HashAlgorithm) -> Vec<u8> {
  let algorithm_name = match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => unreachable!("the persistent v4 contract has exactly two hash widths"),
  };
  fs::read(fixture_root().join(format!("agca-{algorithm_name}-retirement-journal-segment-valid.bin"))).unwrap()
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TestSinkFailure;

impl Display for TestSinkFailure {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter.write_str("injected retirement-journal sink failure")
  }
}

impl Error for TestSinkFailure {}

#[derive(Default)]
struct RecordingSink {
  attempts: Vec<Vec<u8>>,
  publications: Vec<Vec<u8>>,
  next_publication_sequence: u64,
  fail_next: bool,
  wrong_receipt_key: bool,
  wrong_value_length: bool,
  zero_publication_sequence: bool,
}

impl RetirementJournalDurableSinkV1 for RecordingSink {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    self.attempts.push(segment.value.to_vec());
    if self.fail_next {
      self.fail_next = false;
      return Err(RetirementJournalSinkErrorV1::new("injected_sink_failure", TestSinkFailure));
    }
    self.next_publication_sequence += 1;
    self.publications.push(segment.value.to_vec());
    let artifact_key = if self.wrong_receipt_key { vec![0xA5; segment.artifact_key.len()] } else { segment.artifact_key.to_vec() };
    Ok(RetirementJournalDurabilityReceiptV1 {
      artifact_key,
      stored_value_length: if self.wrong_value_length { 0 } else { segment.value.len() as u32 },
      hard_publication_sequence: if self.zero_publication_sequence { 0 } else { self.next_publication_sequence },
    })
  }
}

fn fixture_record<'a>(bytes: &'a [u8], algorithm: HashAlgorithm) -> RetirementJournalRecordWriteV1<'a> {
  let hash_width = algorithm.hash_length();
  let record_start = 32 + 24 + 32 + hash_width;
  let physical_length = 24 + 2 * hash_width;
  RetirementJournalRecordWriteV1 {
    reason: RetirementReasonV1::StableKeyReplace,
    replacement_publication_sequence: u64::from_le_bytes(bytes[record_start + 8..record_start + 16].try_into().unwrap()),
    retired_at_ms: u64::from_le_bytes(bytes[record_start + 16..record_start + 24].try_into().unwrap()),
    old_incarnation: &bytes[record_start + 24..record_start + 24 + physical_length],
    replacement_incarnation: &bytes[record_start + 24 + physical_length..record_start + 24 + 2 * physical_length],
  }
}

fn owner(
  algorithm: HashAlgorithm,
  cancellation: &CancellationToken,
  options: RetirementJournalBufferOptionsV1,
  memory: &MemoryCoordinator,
) -> RetirementJournalOwnerV1 {
  RetirementJournalOwnerV1::new_chain(
    algorithm,
    (0x31u8..=0x40).collect::<Vec<_>>().try_into().unwrap(),
    1,
    401,
    options,
    cancellation,
    memory,
  )
  .unwrap()
}

#[test]
fn writer_matches_the_independent_frozen_fixture_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let expected = fixture(algorithm);
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let options = RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000);
    let mut owner = owner(algorithm, &cancellation, options, &memory);
    let mut sink = RecordingSink::default();

    owner.append(fixture_record(&expected, algorithm), 10, &mut sink).unwrap();

    assert_eq!(sink.publications, [expected]);
    let status = owner.status();
    assert_eq!(status.pending_records, 0);
    assert_eq!(status.durable_segments, 1);
    assert_eq!(status.durable_records, 1);
    assert_eq!(status.durable_through_replacement_sequence, 5_000);
    assert_eq!(status.last_hard_publication_sequence, 1);
    assert!(!status.failed);
  }
}

#[test]
fn count_and_time_thresholds_publish_one_immutable_predecessor_chain() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let options = RetirementJournalBufferOptionsV1::new(2, 1024 * 1024, 1_000);
  let mut owner = owner(algorithm, &cancellation, options, &memory);
  let mut sink = RecordingSink::default();
  let first = fixture_record(&expected, algorithm);
  let mut second_old = first.old_incarnation.to_vec();
  second_old[0] += 1;
  let second = RetirementJournalRecordWriteV1 { old_incarnation: &second_old, ..first };

  owner.append(first, 100, &mut sink).unwrap();
  assert!(sink.publications.is_empty());
  owner.append(second, 100, &mut sink).unwrap();
  assert_eq!(sink.publications.len(), 1);

  let mut third_old = second_old;
  third_old[0] += 1;
  let third = RetirementJournalRecordWriteV1 { replacement_publication_sequence: 5_001, old_incarnation: &third_old, ..first };
  owner.append(third, 200, &mut sink).unwrap();
  assert!(!owner.poll(1_199, &mut sink).unwrap());
  assert!(owner.poll(1_200, &mut sink).unwrap());
  assert_eq!(sink.publications.len(), 2);

  let first_segment = decode_retirement_journal_segment_v1(&sink.publications[0], algorithm).unwrap();
  let second_segment = decode_retirement_journal_segment_v1(&sink.publications[1], algorithm).unwrap();
  assert_eq!(second_segment.segment_ordinal, 2);
  assert_eq!(second_segment.generation, 402);
  assert_eq!(second_segment.previous_segment_hash, Some(first_segment.key.as_slice()));
  assert!(!second_segment.chain_reset);
}

#[test]
fn byte_threshold_flushes_without_exceeding_the_frozen_segment_target() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let record_length = 72 + 4 * algorithm.hash_length();
  let complete_fixed_length = 92 + algorithm.hash_length();
  let options = RetirementJournalBufferOptionsV1::new(100, complete_fixed_length + 2 * record_length, 30_000);
  let mut owner = owner(algorithm, &cancellation, options, &memory);
  let mut sink = RecordingSink::default();
  let first = fixture_record(&expected, algorithm);
  let mut second_old = first.old_incarnation.to_vec();
  second_old[0] += 1;

  owner.append(first, 1, &mut sink).unwrap();
  owner.append(RetirementJournalRecordWriteV1 { old_incarnation: &second_old, ..first }, 2, &mut sink).unwrap();

  assert_eq!(sink.publications.len(), 1);
  assert_eq!(sink.publications[0].len(), complete_fixed_length + 2 * record_length);
}

#[test]
fn sink_failure_retains_the_exact_buffer_for_idempotent_retry() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000), &memory);
  let mut sink = RecordingSink { fail_next: true, ..RecordingSink::default() };

  let error = owner.append(fixture_record(&expected, algorithm), 1, &mut sink).unwrap_err();
  assert_eq!(error.code(), "retirement_journal_sink");
  assert!(error.incoming_record_retained());
  assert_eq!(owner.status().pending_records, 1);
  assert!(!owner.status().failed);

  assert!(owner.flush(&mut sink).unwrap());
  assert_eq!(sink.attempts.len(), 2);
  assert_eq!(sink.attempts[0], sink.attempts[1]);
  assert_eq!(sink.publications, [expected]);
}

#[test]
fn preappend_flush_failure_does_not_accept_the_incoming_record() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(10, 1024 * 1024, 1_000), &memory);
  let mut sink = RecordingSink::default();
  let first = fixture_record(&expected, algorithm);
  owner.append(first, 1, &mut sink).unwrap();

  let mut second_old = first.old_incarnation.to_vec();
  second_old[0] += 1;
  sink.fail_next = true;
  let error = owner
    .append(
      RetirementJournalRecordWriteV1 { replacement_publication_sequence: 5_001, old_incarnation: &second_old, ..first },
      1_001,
      &mut sink,
    )
    .unwrap_err();
  assert_eq!(error.code(), "retirement_journal_sink");
  assert!(!error.incoming_record_retained());
  assert_eq!(owner.status().pending_records, 1);
  assert_eq!(sink.attempts.len(), 1);
}

#[test]
fn validated_reference_summary_resumes_the_chain_after_restart() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let first_segment = decode_retirement_journal_segment_v1(&expected, algorithm).unwrap();
  let cancellation = CancellationToken::new();
  let mut model = RetirementJournalReferenceModelV1::new(algorithm, &cancellation, 1);
  model.observe_segment(&first_segment).unwrap();
  let summary = model.finish().unwrap();
  let memory = memory_coordinator();
  let wrong_database_id = [0xAA; 16];
  assert_eq!(
    RetirementJournalOwnerV1::resume_chain(
      algorithm,
      wrong_database_id,
      &summary,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .err()
    .unwrap()
    .code(),
    "retirement_journal_options"
  );
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0);
  let mut owner = RetirementJournalOwnerV1::resume_chain(
    algorithm,
    (0x31u8..=0x40).collect::<Vec<_>>().try_into().unwrap(),
    &summary,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    &cancellation,
    &memory,
  )
  .unwrap();
  let first = fixture_record(&expected, algorithm);
  let mut next_old = first.old_incarnation.to_vec();
  next_old[0] += 1;
  let mut sink = RecordingSink::default();
  owner
    .append(RetirementJournalRecordWriteV1 { replacement_publication_sequence: 5_001, old_incarnation: &next_old, ..first }, 1, &mut sink)
    .unwrap();

  let resumed = decode_retirement_journal_segment_v1(&sink.publications[0], algorithm).unwrap();
  assert_eq!(resumed.segment_ordinal, 2);
  assert_eq!(resumed.generation, 402);
  assert_eq!(resumed.previous_segment_hash, Some(first_segment.key.as_slice()));
  assert_eq!(owner.status().durable_segments, 2);
  assert_eq!(owner.status().durable_records, 2);
}

#[test]
fn invalid_options_clock_regression_and_exhausted_sequences_refuse_before_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let database_id = (0x31u8..=0x40).collect::<Vec<_>>().try_into().unwrap();
  let valid = RetirementJournalBufferOptionsV1::new(10, 1024 * 1024, 30_000);
  for options in [
    RetirementJournalBufferOptionsV1::new(0, 1024 * 1024, 30_000),
    RetirementJournalBufferOptionsV1::new(1, 1, 30_000),
    RetirementJournalBufferOptionsV1::new(1, 16 * 1024 * 1024 + 1, 30_000),
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 0),
  ] {
    assert_eq!(
      RetirementJournalOwnerV1::new_chain(algorithm, database_id, 1, 1, options, &cancellation, &memory).err().unwrap().code(),
      "retirement_journal_options"
    );
  }
  assert_eq!(
    RetirementJournalOwnerV1::new_chain(algorithm, database_id, u64::MAX, 1, valid, &cancellation, &memory).err().unwrap().code(),
    "retirement_journal_options"
  );
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0);

  let expected = fixture(algorithm);
  let mut owner = owner(algorithm, &cancellation, valid, &memory);
  let mut sink = RecordingSink::default();
  assert!(!owner.flush(&mut sink).unwrap());
  assert!(!owner.poll(10, &mut sink).unwrap());
  owner.append(fixture_record(&expected, algorithm), 10, &mut sink).unwrap();
  assert_eq!(owner.poll(9, &mut sink).unwrap_err().code(), "retirement_journal_clock_regression");
  assert_eq!(owner.status().pending_records, 1);
  assert!(sink.publications.is_empty());
}

#[test]
fn memory_pressure_refuses_before_allocating_or_publishing() {
  let algorithm = HashAlgorithm::Blake3_256;
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(1024 * 1024, 2 * 1024 * 1024, 1, 512 * 1024).unwrap());
  let error =
    RetirementJournalOwnerV1::new_chain(algorithm, [0x31; 16], 1, 1, RetirementJournalBufferOptionsV1::default(), &cancellation, &memory)
      .err()
      .unwrap();
  assert_eq!(error.code(), "retirement_journal_memory");
  let owner = memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
  assert_eq!(owner.rejections, 1);
}

#[test]
fn a_false_durability_receipt_latches_the_owner_without_forgetting_pending_evidence() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000), &memory);
  let mut sink = RecordingSink { wrong_receipt_key: true, ..RecordingSink::default() };

  let error = owner.append(fixture_record(&expected, algorithm), 1, &mut sink).unwrap_err();
  assert_eq!(error.code(), "retirement_journal_receipt");
  assert!(error.incoming_record_retained());
  assert_eq!(owner.status().pending_records, 1);
  assert!(owner.status().failed);
  assert!(matches!(owner.flush(&mut sink), Err(RetirementJournalOwnerErrorV1::Failed)));
}

#[test]
fn every_unbound_durability_receipt_shape_fails_closed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  for mut sink in [
    RecordingSink { wrong_value_length: true, ..RecordingSink::default() },
    RecordingSink { zero_publication_sequence: true, ..RecordingSink::default() },
  ] {
    let cancellation = CancellationToken::new();
    let memory = memory_coordinator();
    let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000), &memory);
    let error = owner.append(fixture_record(&expected, algorithm), 1, &mut sink).unwrap_err();
    assert_eq!(error.code(), "retirement_journal_receipt");
    assert!(owner.status().failed);
    assert_eq!(owner.status().pending_records, 1);
  }
}

#[test]
fn malformed_out_of_order_canceled_and_unadmitted_inputs_change_no_state() {
  let algorithm = HashAlgorithm::Blake3_256;
  let expected = fixture(algorithm);
  let cancellation = CancellationToken::new();
  let memory = memory_coordinator();
  let mut owner = owner(algorithm, &cancellation, RetirementJournalBufferOptionsV1::new(10, 1024 * 1024, 30_000), &memory);
  let mut sink = RecordingSink::default();
  let first = fixture_record(&expected, algorithm);
  owner.append(first, 1, &mut sink).unwrap();

  assert_eq!(owner.append(first, 2, &mut sink).unwrap_err().code(), "retirement_journal_record_order");
  let mut malformed = first.old_incarnation.to_vec();
  malformed[..algorithm.hash_length()].fill(0);
  assert_eq!(
    owner.append(RetirementJournalRecordWriteV1 { old_incarnation: &malformed, ..first }, 3, &mut sink).unwrap_err().code(),
    "physical_incarnation_fields"
  );
  assert_eq!(owner.status().pending_records, 1);

  cancellation.cancel();
  assert_eq!(owner.poll(30_001, &mut sink).unwrap_err().code(), "retirement_journal_cancelled");
  assert_eq!(owner.status().pending_records, 1);
  assert!(sink.publications.is_empty());

  let snapshot = memory.snapshot().unwrap();
  assert!(snapshot.owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes > 0);
  drop(owner);
  let snapshot = memory.snapshot().unwrap();
  assert_eq!(snapshot.owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0);
}

fn rust_sources(root: &Path, sources: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(root).unwrap() {
    let entry = entry.unwrap();
    let path = entry.path();
    if path.is_dir() {
      rust_sources(&path, sources);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      sources.push(path);
    }
  }
}

#[test]
fn writer_has_only_reviewed_authority_callers_and_no_independent_watermark_control() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let writer_source = fs::read_to_string(source_root.join("engine/v4/gc_retirement.rs")).unwrap();
  assert!(!writer_source.contains("V4ControlStore"));
  assert!(!writer_source.contains("PhysicalInventoryActiveControl"));
  assert!(!writer_source.contains("publish_mutable"));
  let migration_source_gc = fs::read_to_string(source_root.join("engine/v4/migration_source_gc.rs")).unwrap();
  assert!(migration_source_gc.contains("MigrationStateOwnerV1"));
  assert!(!migration_source_gc.contains("V4FirstAuthorityPublisher"));
  assert!(!migration_source_gc.contains("retirement_owner."));
  let migration_capture_runtime = fs::read_to_string(source_root.join("engine/v4/migration_capture_runtime.rs")).unwrap();
  assert!(migration_capture_runtime.contains("MigrationStateOwnerV1"));
  assert!(!migration_capture_runtime.contains("V4FirstAuthorityPublisher"));
  assert!(!migration_capture_runtime.contains("retirement_owner."));
  let migration_capture_replay = fs::read_to_string(source_root.join("engine/v4/migration_capture_replay.rs")).unwrap();
  assert!(migration_capture_replay.contains("MigrationStateOwnerV1"));
  assert!(!migration_capture_replay.contains("retirement_owner."));
  let migration_root_map_owner = fs::read_to_string(source_root.join("engine/v4/migration_root_map_owner.rs")).unwrap();
  assert!(migration_root_map_owner.contains("V4FirstAuthorityPublisher"));
  assert!(migration_root_map_owner.contains("publish_mutable_system_control_with_authority_expectation"));
  assert!(!migration_root_map_owner.contains("retirement_owner."));
  let index_runtime_installation = fs::read_to_string(source_root.join("engine/v4/index_runtime_installation.rs")).unwrap();
  assert!(index_runtime_installation.contains("IndexScopeOrdinalStoreRegistryV1::new"));
  assert!(index_runtime_installation.contains("request.retirement_owner,"));
  assert!(!index_runtime_installation.contains("RetirementJournalOwnerV1::"));

  let mut callers = Vec::new();
  let mut sources = Vec::new();
  rust_sources(&source_root, &mut sources);
  for path in sources {
    if path == source_root.join("engine/v4/gc_retirement.rs") || path == source_root.join("engine/v4/gc_lineage_recovery.rs") {
      continue;
    }
    let source = fs::read_to_string(&path).unwrap();
    if source.contains("RetirementJournalOwnerV1") {
      callers.push(path.strip_prefix(&source_root).unwrap().to_owned());
    }
  }
  callers.sort();
  assert_eq!(
    callers,
    [
      PathBuf::from("engine/v4/first_authority.rs"),
      PathBuf::from("engine/v4/index_recovery_store.rs"),
      PathBuf::from("engine/v4/index_runtime_installation.rs"),
      PathBuf::from("engine/v4/migration_capture_replay.rs"),
      PathBuf::from("engine/v4/migration_capture_runtime.rs"),
      PathBuf::from("engine/v4/migration_owner.rs"),
      PathBuf::from("engine/v4/migration_root_map_owner.rs"),
      PathBuf::from("engine/v4/migration_source_gc.rs"),
    ],
    "retirement owner must remain confined to reviewed physical-authority and fenced migration owners"
  );
}
