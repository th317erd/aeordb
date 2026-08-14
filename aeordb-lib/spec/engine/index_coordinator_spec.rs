use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{HostMemorySample, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coordinator::{
  IndexCoordinatorErrorV1, IndexCoordinatorLifecycleV1, IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1,
  IndexMutationAdmissionV1, IndexMutationRequestV1,
};
use aeordb::engine::v4::index_page::{OrderedIndexRoleV1, decode_ordered_record, ordered_record_order_key};
use aeordb::engine::v4::index_record::{ScopeReverseRecordV1, encode_scope_reverse_record};

const HASH_ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn memory(hard_limit_bytes: u64) -> MemoryCoordinator {
  let emergency_reserve_bytes = (hard_limit_bytes / 3).max(1);
  let ordinary_limit_bytes = hard_limit_bytes - emergency_reserve_bytes;
  MemoryCoordinator::new(
    MemoryPolicy::new(ordinary_limit_bytes.saturating_sub(1).max(1), hard_limit_bytes, 1, emergency_reserve_bytes).unwrap(),
  )
}

fn options(mutation_bytes: u64, flush_mutations: u64, flush_ms: u64, publication_bytes: u64) -> IndexCoordinatorOptionsV1 {
  IndexCoordinatorOptionsV1::new(mutation_bytes, flush_mutations, flush_ms, publication_bytes).unwrap()
}

fn index_id(label: &[u8]) -> Vec<u8> {
  digest_parts(HASH_ALGORITHM, &[b"index:", label])
}

fn record(document_ordinal: u64) -> Vec<u8> {
  let file_key = digest_parts(HASH_ALGORITHM, &[b"file:", &document_ordinal.to_le_bytes()]);
  encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal, file_key: &file_key }, HASH_ALGORITHM).unwrap()
}

fn request<'a>(index_id: &'a [u8], sequence: u64, operation_byte: u8, encoded_record: &'a [u8]) -> IndexMutationRequestV1<'a> {
  IndexMutationRequestV1 {
    index_id,
    role: OrderedIndexRoleV1::ScopeReverse,
    publication_sequence: sequence,
    operation_id: [operation_byte; 16],
    encoded_record,
  }
}

fn coordinator(memory: MemoryCoordinator, options: IndexCoordinatorOptionsV1) -> IndexCoordinatorV1 {
  IndexCoordinatorV1::new([7; 16], HASH_ALGORITHM, memory, options, 1_000).unwrap()
}

#[test]
fn options_and_mutation_identity_fail_closed_before_state_changes() {
  assert!(IndexCoordinatorOptionsV1::new(0, 1, 1, 1).is_err());
  assert!(IndexCoordinatorOptionsV1::new(1, 0, 1, 1).is_err());
  assert!(IndexCoordinatorOptionsV1::new(1, 1, 0, 1).is_err());
  assert!(IndexCoordinatorOptionsV1::new(1, 1, 1, 0).is_err());
  assert!(IndexCoordinatorV1::new([0; 16], HASH_ALGORITHM, memory(100_000), options(20_000, 10, 1_000, 10_000), 1).is_err());

  let memory = memory(100_000);
  let mut coordinator = coordinator(memory.clone(), options(20_000, 10, 1_000, 10_000));
  let valid_id = index_id(b"valid");
  let encoded = record(1);
  for invalid in [
    IndexMutationRequestV1 { index_id: &[], ..request(&valid_id, 1, 1, &encoded) },
    IndexMutationRequestV1 { publication_sequence: 0, ..request(&valid_id, 1, 1, &encoded) },
    IndexMutationRequestV1 { operation_id: [0; 16], ..request(&valid_id, 1, 1, &encoded) },
    IndexMutationRequestV1 { role: OrderedIndexRoleV1::NvtTile, ..request(&valid_id, 1, 1, &encoded) },
    IndexMutationRequestV1 { encoded_record: b"bad", ..request(&valid_id, 1, 1, &encoded) },
  ] {
    assert!(coordinator.admit(invalid, 1_001).is_err());
    assert_eq!(coordinator.snapshot().active_records, 0);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn admission_reserves_exactly_and_collapses_duplicates_and_replacements() {
  let memory = memory(100_000);
  let mut coordinator = coordinator(memory.clone(), options(20_000, 10, 1_000, 10_000));
  let id = index_id(b"names");
  let encoded = record(1);

  assert_eq!(coordinator.admit(request(&id, 1, 1, &encoded), 1_001).unwrap(), IndexMutationAdmissionV1::Inserted);
  let inserted = coordinator.snapshot();
  assert_eq!(inserted.active_records, 1);
  assert_eq!(inserted.active_mutations, 1);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, inserted.active_bytes);

  assert_eq!(coordinator.admit(request(&id, 1, 1, &encoded), 1_002).unwrap(), IndexMutationAdmissionV1::Duplicate);
  assert_eq!(coordinator.snapshot(), inserted);

  assert_eq!(coordinator.admit(request(&id, 2, 2, &encoded), 1_003).unwrap(), IndexMutationAdmissionV1::Replaced);
  let replaced = coordinator.snapshot();
  assert_eq!(replaced.active_records, 1);
  assert_eq!(replaced.active_mutations, 2);
  assert_eq!(replaced.active_bytes, inserted.active_bytes);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, replaced.active_bytes);
}

#[test]
fn exact_fit_global_budget_does_not_require_provisional_clone_headroom() {
  let id = index_id(b"exact-fit");
  let encoded = record(1);
  let probe_memory = memory(100_000);
  let mut probe = coordinator(probe_memory, options(20_000, 10, 1_000, 10_000));
  probe.admit(request(&id, 1, 1, &encoded), 1_001).unwrap();
  let exact_bytes = probe.snapshot().active_bytes;
  drop(probe);

  let exact_memory = MemoryCoordinator::new(MemoryPolicy::new(exact_bytes, exact_bytes + 100, 1, 100).unwrap());
  let mut exact = coordinator(exact_memory.clone(), options(exact_bytes, 10, 1_000, 10_000));
  assert_eq!(exact.admit(request(&id, 1, 1, &encoded), 1_001).unwrap(), IndexMutationAdmissionV1::Inserted);
  assert_eq!(exact_memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, exact_bytes);
}

#[test]
fn stale_and_conflicting_mutations_do_not_change_retained_state() {
  let memory = memory(100_000);
  let mut coordinator = coordinator(memory, options(20_000, 10, 1_000, 10_000));
  let id = index_id(b"names");
  let encoded = record(1);
  coordinator.admit(request(&id, 2, 2, &encoded), 1_001).unwrap();
  let before = coordinator.snapshot();

  assert!(matches!(coordinator.admit(request(&id, 1, 1, &encoded), 1_002), Err(IndexCoordinatorErrorV1::StaleMutation { .. })));
  assert!(matches!(coordinator.admit(request(&id, 2, 3, &encoded), 1_003), Err(IndexCoordinatorErrorV1::ConflictingMutation { .. })));
  assert_eq!(coordinator.snapshot(), before);
}

#[test]
fn count_age_and_process_pressure_each_request_a_flush() {
  let coordinator_memory = memory(100_000);
  let mut buffered = coordinator(coordinator_memory, options(20_000, 2, 100, 10_000));
  let id = index_id(b"names");
  let first = record(1);
  let second = record(2);
  buffered.admit(request(&id, 1, 1, &first), 1_001).unwrap();
  assert_eq!(buffered.flush_reason(1_050).unwrap(), None);
  assert_eq!(buffered.flush_reason(1_101).unwrap(), Some(IndexFlushReasonV1::Age));
  buffered.admit(request(&id, 2, 2, &second), 1_102).unwrap();
  assert_eq!(buffered.flush_reason(1_102).unwrap(), Some(IndexFlushReasonV1::MutationCount));

  let pressure_memory = memory(100_000);
  let mut pressured = coordinator(pressure_memory.clone(), options(20_000, 10, 1_000, 10_000));
  pressured.admit(request(&id, 1, 1, &first), 1_001).unwrap();
  pressure_memory.update_host_sample(HostMemorySample { rss_bytes: 98_000, ..Default::default() }).unwrap();
  assert_eq!(pressured.flush_reason(1_002).unwrap(), Some(IndexFlushReasonV1::MemoryPressure));
}

#[test]
fn local_and_global_pressure_refuse_without_retaining_partial_state() {
  let id = index_id(b"names");
  let encoded = record(1);
  let local_memory = memory(100_000);
  let mut local = coordinator(local_memory.clone(), options(1, 10, 1_000, 10_000));
  assert!(matches!(local.admit(request(&id, 1, 1, &encoded), 1_001), Err(IndexCoordinatorErrorV1::SpillRequired { .. })));
  assert_eq!(local.snapshot().active_records, 0);
  assert_eq!(local_memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, 0);

  let global_memory = memory(300);
  let mut global = coordinator(global_memory.clone(), options(20_000, 10, 1_000, 10_000));
  assert!(matches!(global.admit(request(&id, 1, 1, &encoded), 1_001), Err(IndexCoordinatorErrorV1::SpillRequired { .. })));
  assert_eq!(global.snapshot().active_records, 0);
  assert_eq!(global_memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, 0);
}

#[test]
fn one_frozen_batch_can_publish_while_new_active_work_continues() {
  let memory = memory(100_000);
  let mut coordinator = coordinator(memory.clone(), options(20_000, 1, 1_000, 10_000));
  let id = index_id(b"names");
  let first = record(1);
  let second = record(2);
  coordinator.admit(request(&id, 1, 1, &first), 1_001).unwrap();
  let batch = coordinator.begin_flush(1_002, None, false).unwrap().expect("due batch");
  assert_eq!(batch.records().len(), 1);
  assert_eq!(coordinator.snapshot().frozen_records, 1);

  coordinator.admit(request(&id, 2, 2, &second), 1_003).unwrap();
  let snapshot = coordinator.snapshot();
  assert_eq!(snapshot.active_records, 1);
  assert_eq!(snapshot.frozen_records, 1);
  assert!(matches!(
    coordinator.begin_flush(1_004, Some(IndexFlushReasonV1::Explicit), false),
    Err(IndexCoordinatorErrorV1::FlushInProgress { .. })
  ));

  let reserved = memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes;
  assert_eq!(reserved, snapshot.active_bytes + snapshot.frozen_bytes + batch.publication_bytes());
  coordinator.complete_success(&batch).unwrap();
  drop(batch);
  let completed = coordinator.snapshot();
  assert_eq!(completed.active_records, 1);
  assert_eq!(completed.frozen_records, 0);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, completed.active_bytes);
}

#[test]
fn failed_completion_restores_exact_state_and_newer_active_mutation_wins() {
  let memory = memory(100_000);
  let mut coordinator = coordinator(memory.clone(), options(20_000, 1, 1_000, 10_000));
  let id = index_id(b"names");
  let encoded = record(1);
  coordinator.admit(request(&id, 1, 1, &encoded), 1_001).unwrap();
  let batch = coordinator.begin_flush(1_002, None, false).unwrap().unwrap();
  coordinator.admit(request(&id, 2, 2, &encoded), 1_003).unwrap();
  coordinator.complete_failure(&batch, 1_004).unwrap();
  drop(batch);

  let snapshot = coordinator.snapshot();
  assert_eq!(snapshot.active_records, 1);
  assert_eq!(snapshot.active_mutations, 2);
  assert_eq!(snapshot.frozen_records, 0);
  assert_eq!(snapshot.restored_flushes, 1);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, snapshot.active_bytes);
}

#[test]
fn abandoned_frozen_batch_can_be_reissued_without_accepting_the_stale_handle() {
  let memory = memory(100_000);
  let mut coordinator = coordinator(memory.clone(), options(20_000, 1, 1_000, 10_000));
  let id = index_id(b"names");
  let encoded = record(1);
  coordinator.admit(request(&id, 1, 1, &encoded), 1_001).unwrap();
  let abandoned = coordinator.begin_flush(1_002, None, false).unwrap().unwrap();
  let before_retry = coordinator.snapshot();

  assert!(matches!(coordinator.retry_frozen(true), Err(IndexCoordinatorErrorV1::Cancelled)));
  assert_eq!(coordinator.snapshot(), before_retry);
  let replacement = coordinator.retry_frozen(false).unwrap();
  assert!(matches!(coordinator.complete_success(&abandoned), Err(IndexCoordinatorErrorV1::StaleBatch)));
  assert_eq!(coordinator.snapshot(), before_retry);

  coordinator.complete_failure(&replacement, 1_003).unwrap();
  drop(abandoned);
  drop(replacement);
  let restored = coordinator.snapshot();
  assert_eq!(restored.active_records, 1);
  assert_eq!(restored.frozen_records, 0);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, restored.active_bytes);
}

#[test]
fn cancellation_and_foreign_batch_tokens_cannot_consume_dirty_state() {
  let shared_memory = memory(200_000);
  let mut first = coordinator(shared_memory.clone(), options(20_000, 1, 1_000, 10_000));
  let mut second = IndexCoordinatorV1::new([8; 16], HASH_ALGORITHM, shared_memory, options(20_000, 1, 1_000, 10_000), 1_000).unwrap();
  let id = index_id(b"names");
  let encoded = record(1);
  first.admit(request(&id, 1, 1, &encoded), 1_001).unwrap();
  let before = first.snapshot();
  assert!(matches!(first.begin_flush(1_002, None, true), Err(IndexCoordinatorErrorV1::Cancelled)));
  assert_eq!(first.snapshot(), before);

  second.admit(request(&id, 1, 1, &encoded), 1_001).unwrap();
  let foreign = second.begin_flush(1_002, None, false).unwrap().unwrap();
  assert!(matches!(first.complete_success(&foreign), Err(IndexCoordinatorErrorV1::ForeignBatch)));
  assert_eq!(first.snapshot(), before);
  second.complete_failure(&foreign, 1_003).unwrap();
  assert_eq!(second.snapshot().active_records, 1, "foreign-batch refusal must preserve the rightful owner's recovery handle");
  drop(foreign);
}

#[test]
fn shutdown_drains_before_stopping_and_rejects_new_mutations() {
  let memory = memory(100_000);
  let mut coordinator = coordinator(memory, options(20_000, 10, 1_000, 10_000));
  let id = index_id(b"names");
  let encoded = record(1);
  coordinator.admit(request(&id, 1, 1, &encoded), 1_001).unwrap();
  coordinator.begin_draining().unwrap();
  assert_eq!(coordinator.snapshot().lifecycle, IndexCoordinatorLifecycleV1::Draining);
  assert!(matches!(coordinator.admit(request(&id, 2, 2, &encoded), 1_002), Err(IndexCoordinatorErrorV1::NotRunning { .. })));
  assert!(coordinator.finish_draining().is_err());

  let batch = coordinator.begin_flush(1_003, Some(IndexFlushReasonV1::Shutdown), false).unwrap().unwrap();
  coordinator.complete_success(&batch).unwrap();
  drop(batch);
  coordinator.finish_draining().unwrap();
  assert_eq!(coordinator.snapshot().lifecycle, IndexCoordinatorLifecycleV1::Stopped);
  assert!(coordinator.begin_draining().is_err());
}

#[test]
fn clock_regression_and_not_yet_due_flush_leave_dirty_state_unchanged() {
  let memory = memory(100_000);
  let mut coordinator = coordinator(memory, options(20_000, 10, 1_000, 10_000));
  let id = index_id(b"names");
  let encoded = record(1);
  coordinator.admit(request(&id, 1, 1, &encoded), 1_100).unwrap();
  let before = coordinator.snapshot();

  assert!(matches!(coordinator.admit(request(&id, 2, 2, &encoded), 1_099), Err(IndexCoordinatorErrorV1::ClockRegressed { .. })));
  assert_eq!(coordinator.snapshot(), before);
  assert!(coordinator.begin_flush(1_101, None, false).unwrap().is_none());
  assert_eq!(coordinator.snapshot(), before);
}

#[test]
fn publication_batches_are_deterministic_and_bounded() {
  let memory = memory(100_000);
  let mut coordinator = coordinator(memory, options(20_000, 1, 1_000, 300));
  let id = index_id(b"names");
  let records = [record(3), record(1), record(2)];
  let mut expected = records.clone();
  expected.sort_by_key(|encoded| {
    let decoded = decode_ordered_record(encoded, HASH_ALGORITHM, OrderedIndexRoleV1::ScopeReverse).unwrap();
    ordered_record_order_key(&decoded).unwrap()
  });
  for (offset, encoded) in records.iter().enumerate() {
    coordinator.admit(request(&id, (offset + 1) as u64, (offset + 1) as u8, encoded), 1_001 + offset as u64).unwrap();
  }

  let first = coordinator.begin_flush(1_010, None, false).unwrap().unwrap();
  assert_eq!(first.records().len(), 1);
  assert!(first.publication_bytes() <= 300);
  assert_eq!(first.records()[0].encoded_record(), expected[0]);
  coordinator.complete_success(&first).unwrap();
  drop(first);

  let second = coordinator.begin_flush(1_011, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  assert_eq!(second.records().len(), 1);
  assert_eq!(second.records()[0].encoded_record(), expected[1]);
  coordinator.complete_success(&second).unwrap();
  drop(second);

  let third = coordinator.begin_flush(1_012, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  assert_eq!(third.records().len(), 1);
  assert_eq!(third.records()[0].encoded_record(), expected[2]);
  coordinator.complete_success(&third).unwrap();
  drop(third);
  assert_eq!(coordinator.snapshot().active_records, 0);
}
