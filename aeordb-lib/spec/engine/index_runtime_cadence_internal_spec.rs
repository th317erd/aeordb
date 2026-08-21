use std::sync::Arc;

use crate::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use crate::engine::v4::coverage_runtime::SoftMutationHubOptionsV1;
use crate::engine::v4::index_coordinator::{FrozenIndexBatchV1, IndexCoordinatorOptionsV1};
use crate::engine::v4::index_producer_collector::IndexProducerCollectorOptionsV1;
use crate::engine::v4::index_producer_coordinator::{
  IndexProducerAdmissionV1, IndexProducerCoordinatorOptionsV1, IndexProducerSpillErrorV1, IndexProducerSpillReasonV1,
  IndexProducerSpillReceiptV1, IndexProducerSpillStoreV1, IndexProducerTaskKindV1, IndexProducerTaskRequestV1, IndexProducerTaskViewV1,
};
use crate::engine::v4::index_producer_source::IndexSemanticScopeLimitsV1;
use crate::engine::v4::index_runtime_owner::{
  IndexRuntimeBatchPublisherV1, IndexRuntimeLifecycleV1, IndexRuntimeOwnerOptionsV1, IndexRuntimeOwnerV1, IndexRuntimePublicationErrorV1,
  IndexRuntimePublicationReceiptV1, IndexRuntimeRecoveryDecisionV1,
};
use crate::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_mutation_journal,
  encode_mutation_journal,
};
use crate::engine::{HashAlgorithm, MockClock, VirtualClock};

use super::*;

struct UnusedPublisher;

impl IndexRuntimeBatchPublisherV1 for UnusedPublisher {
  fn publish(&mut self, _batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    panic!("idle cadence unexpectedly invoked its publisher")
  }
}

impl IndexProducerSpillStoreV1 for UnusedPublisher {
  fn spill(
    &mut self,
    _task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    panic!("non-pressure cadence admission unexpectedly invoked its spill store")
  }
}

struct RecordingPublisher {
  spilled: Arc<std::sync::Mutex<Vec<([u8; 16], IndexProducerSpillReasonV1)>>>,
}

impl IndexRuntimeBatchPublisherV1 for RecordingPublisher {
  fn publish(&mut self, _batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    panic!("producer-admission test unexpectedly published a runtime batch")
  }
}

impl IndexProducerSpillStoreV1 for RecordingPublisher {
  fn spill(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    self.spilled.lock().unwrap().push((task.operation_id(), reason));
    IndexProducerSpillReceiptV1::new(task.operation_id(), vec![0x91; 32])
  }
}

fn maintenance_task<'a>(
  operation_id: [u8; 16],
  publication_sequence: u64,
  root: &'a [u8],
  semantic: &'a [u8],
) -> IndexProducerTaskRequestV1<'a> {
  IndexProducerTaskRequestV1 {
    operation_id,
    kind: IndexProducerTaskKindV1::Rebuild,
    publication_sequence,
    namespace_root_before: root,
    namespace_root_after: root,
    semantic_state_root: semantic,
    journal_head: None,
    scope: Some("/docs"),
  }
}

fn encoded_journal(path: &str) -> Vec<u8> {
  let hash = |label: &[u8]| crate::engine::v4::hash::digest_parts(HashAlgorithm::Blake3_256, &[b"cadence-journal:", label]);
  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    owner_id: [0x61; 16],
    owner_kind: JournalOwnerKindV1::Task,
    generation: 1,
    segment_ordinal: 0,
    chain_reset: true,
    previous_segment: &[0; 32],
    semantic_state_root: &hash(b"semantic"),
    runtime_boot_id: [0x62; 16],
    records: &[MutationRecordWriteV1 {
      kind: MutationKindV1::Create,
      sequence: 7,
      mutation_id: &hash(b"mutation"),
      batch_ordinal: 0,
      batch_count: 1,
      root_before: &hash(b"before-root"),
      root_after: &hash(b"after-root"),
      before: None,
      after: Some(MutationSideWriteV1 { path, revision: &hash(b"revision") }),
      committed_at_ms: 100,
    }],
  })
  .unwrap()
  .value
}

fn owner() -> Arc<IndexRuntimeOwnerV1> {
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 * 1_024 * 1_024, 32 * 1_024 * 1_024, 1, 4 * 1_024 * 1_024).unwrap());
  let options = IndexRuntimeOwnerOptionsV1 {
    soft_hub: SoftMutationHubOptionsV1::new(8, 64 * 1_024, 16 * 1_024).unwrap(),
    producer: IndexProducerCoordinatorOptionsV1::new(8, 512 * 1_024, 2, 10, 1_000, 4, 64, 512 * 1_024).unwrap(),
    mutations: IndexCoordinatorOptionsV1::new(512 * 1_024, 64, 30_000, 128 * 1_024).unwrap(),
    collector: IndexProducerCollectorOptionsV1::new(4, 8, 16, 512 * 1_024, 64, 512 * 1_024, 25).unwrap(),
    semantic: IndexSemanticScopeLimitsV1::new(4, 8, 16, 512 * 1_024).unwrap(),
    source_retry_after_ms: 25,
    publication_retry_after_ms: 100,
  };
  let owner = Arc::new(IndexRuntimeOwnerV1::new([0x41; 16], HashAlgorithm::Blake3_256, memory, options, 1).unwrap());
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 0, highest_checkpoint_sequence: 0 }).unwrap();
  owner
}

#[test]
fn zero_clock_is_rejected_at_construction_and_at_each_tick() {
  let zero_clock: Arc<dyn VirtualClock> = Arc::new(MockClock::new(1, 0));
  assert!(matches!(
    IndexRuntimeCadenceV1::new(owner(), UnusedPublisher, CancellationToken::new(), zero_clock),
    Err(IndexRuntimeCadenceErrorV1::InvalidClock)
  ));

  let clock = Arc::new(MockClock::new(2, 10));
  let tick_owner = owner();
  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&tick_owner),
    UnusedPublisher,
    CancellationToken::new(),
    Arc::clone(&clock) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  clock.set_time(0);
  assert!(matches!(cadence.flush_if_due(), Err(IndexRuntimeCadenceErrorV1::InvalidClock)));
  let degraded = tick_owner.cached_snapshot();
  assert_eq!(degraded.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(degraded.degraded.as_ref().unwrap().code, "cadence_invalid_clock");
}

#[test]
fn poisoned_publisher_lock_fails_closed_without_entering_the_owner() {
  let cadence = IndexRuntimeCadenceV1::new(
    owner(),
    UnusedPublisher,
    CancellationToken::new(),
    Arc::new(MockClock::new(3, 10)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let injected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _publisher = cadence.publisher.lock().unwrap();
    panic!("inject publisher lock poison");
  }));
  assert!(injected.is_err());
  assert!(matches!(cadence.flush_if_due(), Err(IndexRuntimeCadenceErrorV1::PublisherPoisoned)));
  let degraded = cadence.owner.cached_snapshot();
  assert_eq!(degraded.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(degraded.degraded.as_ref().unwrap().code, "cadence_publisher_poisoned");
  assert_eq!(degraded.mutations.active_records, 0);
}

#[test]
fn graceful_drain_closes_and_stops_the_same_owner_idempotently() {
  let owner = owner();
  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    UnusedPublisher,
    CancellationToken::new(),
    Arc::new(MockClock::new(4, 20)) as Arc<dyn VirtualClock>,
  )
  .unwrap();

  let first = cadence.drain_and_stop().unwrap();
  assert_eq!(first.published_batches, 0);
  assert_eq!(first.published_records, 0);
  assert_eq!(first.publication_bytes, 0);
  assert_eq!(first.highest_checkpoint_sequence, 0);
  assert_eq!(owner.cached_snapshot().lifecycle, IndexRuntimeLifecycleV1::Stopped);

  let repeated = cadence.drain_and_stop().unwrap();
  assert_eq!(repeated, first);
  assert_eq!(owner.cached_snapshot().lifecycle, IndexRuntimeLifecycleV1::Stopped);
}

#[test]
fn task_admission_uses_the_cadence_owned_publisher_for_queue_duplicate_and_pressure_spill() {
  let owner = owner();
  let spilled = Arc::new(std::sync::Mutex::new(Vec::new()));
  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    RecordingPublisher { spilled: Arc::clone(&spilled) },
    CancellationToken::new(),
    Arc::new(MockClock::new(5, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let root = [0x31; 32];
  let semantic = [0x41; 32];

  assert_eq!(cadence.admit_task(maintenance_task([1; 16], 1, &root, &semantic)).unwrap(), IndexProducerAdmissionV1::Queued);
  assert_eq!(cadence.admit_task(maintenance_task([1; 16], 1, &root, &semantic)).unwrap(), IndexProducerAdmissionV1::Duplicate);
  for sequence in 2..=8 {
    assert_eq!(
      cadence.admit_task(maintenance_task([sequence as u8; 16], sequence, &root, &semantic)).unwrap(),
      IndexProducerAdmissionV1::Queued
    );
  }
  let pressure_operation = [9; 16];
  let admission = cadence.admit_task(maintenance_task(pressure_operation, 9, &root, &semantic)).unwrap();
  assert!(matches!(admission, IndexProducerAdmissionV1::Spilled { .. }));
  assert_eq!(*spilled.lock().unwrap(), vec![(pressure_operation, IndexProducerSpillReasonV1::AdmissionPressure)]);
  let snapshot = owner.cached_snapshot();
  assert_eq!(snapshot.producer.pending_tasks, 8);
  assert_eq!(snapshot.producer.spilled_tasks, 1);
}

#[test]
fn task_admission_fails_closed_on_cancellation_zero_clock_and_publisher_poison() {
  let root = [0x32; 32];
  let semantic = [0x42; 32];

  let cancellation = CancellationToken::new();
  let cancelled_owner = owner();
  let cancelled = IndexRuntimeCadenceV1::new(
    Arc::clone(&cancelled_owner),
    UnusedPublisher,
    cancellation.clone(),
    Arc::new(MockClock::new(6, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  cancellation.cancel();
  assert!(matches!(
    cancelled.admit_task(maintenance_task([0x11; 16], 1, &root, &semantic)),
    Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled))
  ));
  assert_eq!(cancelled_owner.cached_snapshot().producer.pending_tasks, 0);

  let zero_clock = Arc::new(MockClock::new(7, 100));
  let zero_owner = owner();
  let zero = IndexRuntimeCadenceV1::new(
    Arc::clone(&zero_owner),
    UnusedPublisher,
    CancellationToken::new(),
    Arc::clone(&zero_clock) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  zero_clock.set_time(0);
  assert!(matches!(zero.admit_task(maintenance_task([0x12; 16], 1, &root, &semantic)), Err(IndexRuntimeCadenceErrorV1::InvalidClock)));
  assert_eq!(zero_owner.cached_snapshot().lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(zero_owner.cached_snapshot().producer.pending_tasks, 0);

  let poisoned_owner = owner();
  let poisoned = IndexRuntimeCadenceV1::new(
    Arc::clone(&poisoned_owner),
    UnusedPublisher,
    CancellationToken::new(),
    Arc::new(MockClock::new(8, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let injected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _publisher = poisoned.publisher.lock().unwrap();
    panic!("inject producer admission publisher lock poison");
  }));
  assert!(injected.is_err());
  assert!(matches!(
    poisoned.admit_task(maintenance_task([0x13; 16], 1, &root, &semantic)),
    Err(IndexRuntimeCadenceErrorV1::PublisherPoisoned)
  ));
  assert_eq!(poisoned_owner.cached_snapshot().lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(poisoned_owner.cached_snapshot().producer.pending_tasks, 0);
}

#[test]
fn mutation_journal_admission_uses_the_same_cadence_owned_spill_boundary() {
  let owner = owner();
  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    UnusedPublisher,
    CancellationToken::new(),
    Arc::new(MockClock::new(9, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let encoded = encoded_journal("/docs/a.json");
  let journal = decode_mutation_journal(&encoded, HashAlgorithm::Blake3_256).unwrap();

  let admitted = cadence.admit_mutation_journal(&journal).unwrap();
  assert_eq!(admitted.queued, 1);
  assert_eq!(admitted.duplicates, 0);
  assert_eq!(admitted.spilled, 0);
  assert_eq!(owner.cached_snapshot().producer.pending_tasks, 1);

  let duplicate = cadence.admit_mutation_journal(&journal).unwrap();
  assert_eq!(duplicate.queued, 0);
  assert_eq!(duplicate.duplicates, 1);
  assert_eq!(duplicate.spilled, 0);
  assert_eq!(owner.cached_snapshot().producer.pending_tasks, 1);
}
