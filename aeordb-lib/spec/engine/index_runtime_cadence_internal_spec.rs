use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use crate::engine::v4::coverage_runtime::SoftMutationHubOptionsV1;
use crate::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1;
use crate::engine::v4::index_coordinator::{FrozenIndexBatchV1, IndexCoordinatorOptionsV1};
use crate::engine::v4::index_compaction_runtime::{
  IndexArtifactCompactionExecutionOutcomeV1, IndexArtifactCompactionExecutionRequestV1, IndexRuntimeCompactionErrorV1,
  IndexRuntimeCompactionExecutorV1,
};
use crate::engine::v4::index_producer_collector::IndexProducerCollectorOptionsV1;
use crate::engine::v4::index_producer_collector::{
  IndexParserExecutionErrorV1, IndexParserExecutionRequestV1, IndexParserExecutorV1, IndexParserOutcomeV1,
};
use crate::engine::v4::index_producer_coordinator::{
  IndexProducerAdmissionV1, IndexProducerCoordinatorOptionsV1, IndexProducerDurableTaskStoreV1, IndexProducerSpillErrorV1,
  IndexProducerSpillReasonV1, IndexProducerSpillReceiptV1, IndexProducerSpillStoreV1, IndexProducerTaskKindV1, IndexProducerTaskRequestV1,
  IndexProducerTaskViewV1,
};
use crate::engine::v4::index_producer_source::IndexSemanticScopeLimitsV1;
use crate::engine::v4::index_maintenance_scan::{
  IndexMaintenanceScanLimitsV1, IndexMaintenanceScanPageV1, IndexMaintenanceScanReadErrorV1, IndexMaintenanceScanReadV1,
  IndexMaintenanceScanRequestV1, IndexMaintenanceScanSourceV1, index_maintenance_scan_page_retained_bytes_v1,
};
use crate::engine::v4::index_producer_journal_source::{
  IndexProducerJournalReadErrorV1, IndexProducerJournalReadRequestV1, IndexProducerJournalReadV1, IndexProducerJournalSourceV1,
};
use crate::engine::v4::index_producer_source::{
  IndexFileRevisionReadErrorV1, IndexFileRevisionReadV1, IndexFileRevisionSourceV1, IndexSemanticScopeReadErrorV1,
  IndexSemanticScopeReadRequestV1, IndexSemanticScopeReadV1, IndexSemanticScopeSourceV1,
};
use crate::engine::v4::index_runtime_batch_publisher::IndexRuntimeMutationJournalStoreV1;
use crate::engine::v4::index_runtime_owner::{
  IndexRuntimeBatchPublisherV1, IndexRuntimeLifecycleV1, IndexRuntimeOwnerOptionsV1, IndexRuntimeOwnerV1,
  IndexRuntimePublicationErrorClassV1, IndexRuntimePublicationErrorV1, IndexRuntimePublicationReceiptV1, IndexRuntimeRecoveryDecisionV1,
  IndexRuntimeWorkOutcomeV1,
};
use crate::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_mutation_journal,
  encode_mutation_journal,
};
use crate::engine::{HashAlgorithm, MockClock, VirtualClock};

use super::*;

struct UnusedPublisher;

struct UnusedSources;

struct EmptyMaintenanceSource {
  memory: MemoryCoordinator,
  delay: std::time::Duration,
}

impl IndexMaintenanceScanSourceV1 for EmptyMaintenanceSource {
  fn scan(&self, request: IndexMaintenanceScanRequestV1<'_>) -> Result<IndexMaintenanceScanReadV1, IndexMaintenanceScanReadErrorV1> {
    std::thread::sleep(self.delay);
    let mut page = IndexMaintenanceScanPageV1 { documents: Vec::new(), next_resume_after: None, complete: true, retained_bytes: 0 };
    page.retained_bytes = index_maintenance_scan_page_retained_bytes_v1(&page)
      .map_err(|error| IndexMaintenanceScanReadErrorV1::corrupt("test_page_accounting", error.to_string()))?;
    let reservation = self
      .memory
      .reserve(MemoryOwner::Task, page.retained_bytes, AdmissionClass::Maintenance)
      .map_err(|error| IndexMaintenanceScanReadErrorV1::retryable("test_memory", error.to_string()))?;
    IndexMaintenanceScanReadV1::new(HashAlgorithm::Blake3_256, &request, page, reservation)
      .map_err(|error| IndexMaintenanceScanReadErrorV1::corrupt("test_page", error.to_string()))
  }
}

impl IndexProducerJournalSourceV1 for UnusedSources {
  fn load_journal(
    &self,
    _request: IndexProducerJournalReadRequestV1<'_>,
  ) -> Result<IndexProducerJournalReadV1, IndexProducerJournalReadErrorV1> {
    panic!("idle cadence unexpectedly read a mutation journal")
  }
}

impl IndexMaintenanceScanSourceV1 for UnusedSources {
  fn scan(&self, _request: IndexMaintenanceScanRequestV1<'_>) -> Result<IndexMaintenanceScanReadV1, IndexMaintenanceScanReadErrorV1> {
    panic!("idle cadence unexpectedly scanned source authority")
  }
}

impl IndexFileRevisionSourceV1 for UnusedSources {
  fn load_file_revision(
    &self,
    _namespace_root: &[u8],
    _path: &str,
  ) -> Result<Option<IndexFileRevisionReadV1>, IndexFileRevisionReadErrorV1> {
    panic!("idle cadence unexpectedly loaded a file revision")
  }
}

impl IndexSemanticScopeSourceV1 for UnusedSources {
  fn resolve_scopes(
    &self,
    _request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    panic!("idle cadence unexpectedly resolved semantic scopes")
  }
}

impl IndexParserExecutorV1 for UnusedSources {
  fn parse(&self, _request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    panic!("idle cadence unexpectedly invoked a parser")
  }
}

impl IndexRuntimeCompactionExecutorV1 for UnusedSources {
  fn execute(
    &self,
    _request: IndexArtifactCompactionExecutionRequestV1<'_>,
  ) -> Result<IndexArtifactCompactionExecutionOutcomeV1, IndexRuntimeCompactionErrorV1> {
    panic!("idle cadence unexpectedly invoked artifact compaction")
  }
}

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

struct JournalPublisher {
  events: Arc<Mutex<Vec<&'static str>>>,
  refuse_journal: Arc<AtomicBool>,
  refuse_task: Arc<AtomicBool>,
  cancel_after_journal: Option<CancellationToken>,
}

impl IndexRuntimeBatchPublisherV1 for JournalPublisher {
  fn publish(&mut self, _batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    panic!("journal-admission test unexpectedly published a runtime batch")
  }
}

impl IndexRuntimeMutationJournalStoreV1 for JournalPublisher {
  fn persist_mutation_journal(&mut self, _journal: &EncodedImmutableIndexArtifactV1) -> Result<(), IndexRuntimePublicationErrorV1> {
    self.events.lock().unwrap().push("journal");
    if self.refuse_journal.load(Ordering::Acquire) {
      return Err(IndexRuntimePublicationErrorV1::new(
        IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection,
        "injected_journal_refusal",
        "injected mutation-journal durability refusal",
      ));
    }
    if let Some(cancellation) = &self.cancel_after_journal {
      cancellation.cancel();
    }
    Ok(())
  }
}

impl IndexProducerDurableTaskStoreV1 for JournalPublisher {
  fn persist_task(&mut self, task: IndexProducerTaskViewV1<'_>) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    self.events.lock().unwrap().push("task");
    if self.refuse_task.load(Ordering::Acquire) {
      return Err(IndexProducerSpillErrorV1::new("injected_task_refusal", "injected durable-task refusal"));
    }
    IndexProducerSpillReceiptV1::new(task.operation_id(), vec![0x91; 32])
  }
}

impl IndexProducerSpillStoreV1 for JournalPublisher {
  fn spill(
    &mut self,
    _task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    panic!("non-pressure journal admission unexpectedly invoked pressure spill")
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

fn encoded_journal(path: &str) -> EncodedImmutableIndexArtifactV1 {
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

fn unused_service_sources(sources: &UnusedSources) -> IndexRuntimeProducerServiceSourcesV1<'_> {
  IndexRuntimeProducerServiceSourcesV1 {
    journal_source: sources,
    maintenance_source: sources,
    compaction_executor: sources,
    maintenance_limits: IndexMaintenanceScanLimitsV1::new(1, 64 * 1_024, 4 * 1_024).unwrap(),
    revision_source: sources,
    semantic_source: sources,
    parser: sources,
    mapper: None,
  }
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
fn one_cadence_service_attempt_is_idle_without_work_and_cancellation_consumes_nothing() {
  let sources = UnusedSources;
  assert_eq!(
    IndexRuntimeProducerServiceLimitsV1::new(0, std::time::Duration::from_millis(1)).unwrap_err(),
    IndexRuntimeCadenceErrorV1::InvalidServiceLimits
  );
  assert_eq!(
    IndexRuntimeProducerServiceLimitsV1::new(1, std::time::Duration::ZERO).unwrap_err(),
    IndexRuntimeCadenceErrorV1::InvalidServiceLimits
  );
  let limits = IndexRuntimeProducerServiceLimitsV1::new(4, std::time::Duration::from_millis(10)).unwrap();
  let cadence = IndexRuntimeCadenceV1::new(
    owner(),
    UnusedPublisher,
    CancellationToken::new(),
    Arc::new(MockClock::new(13, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  assert_eq!(cadence.service_next_producer(unused_service_sources(&sources)).unwrap(), IndexRuntimeWorkOutcomeV1::Idle);
  assert_eq!(cadence.service_bounded_producers(unused_service_sources(&sources), limits).unwrap(), 0);

  let cancellation = CancellationToken::new();
  let cancelled_owner = owner();
  let cancelled = IndexRuntimeCadenceV1::new(
    Arc::clone(&cancelled_owner),
    UnusedPublisher,
    cancellation.clone(),
    Arc::new(MockClock::new(14, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  cancellation.cancel();
  assert_eq!(
    cancelled.service_bounded_producers(unused_service_sources(&sources), limits).unwrap_err(),
    IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled)
  );
  assert_eq!(cancelled_owner.cached_snapshot().producer.pending_tasks, 0);
}

#[test]
fn bounded_service_stops_at_the_attempt_limit_and_retains_remaining_work() {
  let owner = owner();
  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    UnusedPublisher,
    CancellationToken::new(),
    Arc::new(MockClock::new(15, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let root = [0x51; 32];
  let semantic = [0x52; 32];
  for sequence in 1..=3 {
    assert_eq!(
      cadence.admit_task(maintenance_task([sequence as u8; 16], sequence, &root, &semantic)).unwrap(),
      IndexProducerAdmissionV1::Queued
    );
  }

  let unused = UnusedSources;
  let maintenance = EmptyMaintenanceSource {
    memory: MemoryCoordinator::new(MemoryPolicy::new(4 * 1_024 * 1_024, 8 * 1_024 * 1_024, 1, 1 * 1_024 * 1_024).unwrap()),
    delay: std::time::Duration::ZERO,
  };
  let sources = IndexRuntimeProducerServiceSourcesV1 {
    journal_source: &unused,
    maintenance_source: &maintenance,
    compaction_executor: &unused,
    maintenance_limits: IndexMaintenanceScanLimitsV1::new(1, 64 * 1_024, 4 * 1_024).unwrap(),
    revision_source: &unused,
    semantic_source: &unused,
    parser: &unused,
    mapper: None,
  };
  let limits = IndexRuntimeProducerServiceLimitsV1::new(2, std::time::Duration::from_secs(1)).unwrap();

  assert_eq!(cadence.service_bounded_producers(sources, limits).unwrap(), 2);
  let first = owner.cached_snapshot();
  assert_eq!(first.producer.completed_tasks, 2);
  assert_eq!(first.producer.pending_tasks, 1);
  assert_eq!(first.producer.leased_tasks, 0);

  assert_eq!(cadence.service_bounded_producers(sources, limits).unwrap(), 1);
  let second = owner.cached_snapshot();
  assert_eq!(second.producer.completed_tasks, 3);
  assert_eq!(second.producer.pending_tasks, 0);
  assert_eq!(second.producer.leased_tasks, 0);
}

#[test]
fn bounded_service_stops_at_the_elapsed_limit_between_complete_attempts() {
  let owner = owner();
  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    UnusedPublisher,
    CancellationToken::new(),
    Arc::new(MockClock::new(16, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let root = [0x53; 32];
  let semantic = [0x54; 32];
  for sequence in 1..=2 {
    assert_eq!(
      cadence.admit_task(maintenance_task([sequence as u8; 16], sequence, &root, &semantic)).unwrap(),
      IndexProducerAdmissionV1::Queued
    );
  }

  let unused = UnusedSources;
  let maintenance = EmptyMaintenanceSource {
    memory: MemoryCoordinator::new(MemoryPolicy::new(4 * 1_024 * 1_024, 8 * 1_024 * 1_024, 1, 1 * 1_024 * 1_024).unwrap()),
    delay: std::time::Duration::from_millis(10),
  };
  let sources = IndexRuntimeProducerServiceSourcesV1 {
    journal_source: &unused,
    maintenance_source: &maintenance,
    compaction_executor: &unused,
    maintenance_limits: IndexMaintenanceScanLimitsV1::new(1, 64 * 1_024, 4 * 1_024).unwrap(),
    revision_source: &unused,
    semantic_source: &unused,
    parser: &unused,
    mapper: None,
  };
  let limits = IndexRuntimeProducerServiceLimitsV1::new(2, std::time::Duration::from_millis(1)).unwrap();

  assert_eq!(cadence.service_bounded_producers(sources, limits).unwrap(), 1);
  let first = owner.cached_snapshot();
  assert_eq!(first.producer.completed_tasks, 1);
  assert_eq!(first.producer.pending_tasks, 1);
  assert_eq!(first.producer.leased_tasks, 0);

  assert_eq!(cadence.service_bounded_producers(sources, limits).unwrap(), 1);
  let second = owner.cached_snapshot();
  assert_eq!(second.producer.completed_tasks, 2);
  assert_eq!(second.producer.pending_tasks, 0);
  assert_eq!(second.producer.leased_tasks, 0);
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
fn mutation_journal_is_persisted_before_durable_task_admission_and_queueing() {
  let owner = owner();
  let events = Arc::new(Mutex::new(Vec::new()));
  let refuse_journal = Arc::new(AtomicBool::new(false));
  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    JournalPublisher {
      events: Arc::clone(&events),
      refuse_journal,
      refuse_task: Arc::new(AtomicBool::new(false)),
      cancel_after_journal: None,
    },
    CancellationToken::new(),
    Arc::new(MockClock::new(9, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let encoded = encoded_journal("/docs/a.json");
  let journal = decode_mutation_journal(&encoded.value, HashAlgorithm::Blake3_256).unwrap();

  let admitted = cadence.persist_and_admit_mutation_journal(&encoded, &journal).unwrap();
  assert_eq!(admitted.queued, 1);
  assert_eq!(admitted.duplicates, 0);
  assert_eq!(admitted.spilled, 0);
  assert_eq!(*events.lock().unwrap(), vec!["journal", "task"]);
  assert_eq!(owner.cached_snapshot().producer.pending_tasks, 1);

  let duplicate = cadence.persist_and_admit_mutation_journal(&encoded, &journal).unwrap();
  assert_eq!(duplicate.queued, 0);
  assert_eq!(duplicate.duplicates, 1);
  assert_eq!(duplicate.spilled, 0);
  assert_eq!(*events.lock().unwrap(), vec!["journal", "task", "journal", "task"]);
  assert_eq!(owner.cached_snapshot().producer.pending_tasks, 1);
}

#[test]
fn mutation_journal_persistence_refusal_leaves_the_task_queue_unchanged() {
  let owner = owner();
  let events = Arc::new(Mutex::new(Vec::new()));
  let refuse_journal = Arc::new(AtomicBool::new(true));
  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    JournalPublisher {
      events: Arc::clone(&events),
      refuse_journal,
      refuse_task: Arc::new(AtomicBool::new(false)),
      cancel_after_journal: None,
    },
    CancellationToken::new(),
    Arc::new(MockClock::new(10, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let encoded = encoded_journal("/docs/refused.json");
  let journal = decode_mutation_journal(&encoded.value, HashAlgorithm::Blake3_256).unwrap();

  assert!(matches!(
    cadence.persist_and_admit_mutation_journal(&encoded, &journal),
    Err(IndexRuntimeCadenceErrorV1::Publication(error))
      if error.class() == IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection
  ));
  assert_eq!(*events.lock().unwrap(), vec!["journal"]);
  assert_eq!(owner.cached_snapshot().producer.pending_tasks, 0);
}

#[test]
fn mutation_task_durability_refusal_occurs_after_journal_but_before_queue_admission() {
  let owner = owner();
  let events = Arc::new(Mutex::new(Vec::new()));
  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    JournalPublisher {
      events: Arc::clone(&events),
      refuse_journal: Arc::new(AtomicBool::new(false)),
      refuse_task: Arc::new(AtomicBool::new(true)),
      cancel_after_journal: None,
    },
    CancellationToken::new(),
    Arc::new(MockClock::new(11, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let encoded = encoded_journal("/docs/task-refused.json");
  let journal = decode_mutation_journal(&encoded.value, HashAlgorithm::Blake3_256).unwrap();

  assert!(matches!(
    cadence.persist_and_admit_mutation_journal(&encoded, &journal),
    Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Journal(_)))
  ));
  assert_eq!(*events.lock().unwrap(), vec!["journal", "task"]);
  assert_eq!(owner.cached_snapshot().producer.pending_tasks, 0);
}

#[test]
fn cancellation_after_journal_durability_does_not_admit_a_task() {
  let owner = owner();
  let events = Arc::new(Mutex::new(Vec::new()));
  let cancellation = CancellationToken::new();
  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    JournalPublisher {
      events: Arc::clone(&events),
      refuse_journal: Arc::new(AtomicBool::new(false)),
      refuse_task: Arc::new(AtomicBool::new(false)),
      cancel_after_journal: Some(cancellation.clone()),
    },
    cancellation,
    Arc::new(MockClock::new(12, 100)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let encoded = encoded_journal("/docs/canceled-after-journal.json");
  let journal = decode_mutation_journal(&encoded.value, HashAlgorithm::Blake3_256).unwrap();

  assert_eq!(
    cadence.persist_and_admit_mutation_journal(&encoded, &journal).unwrap_err(),
    IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled)
  );
  assert_eq!(*events.lock().unwrap(), vec!["journal"]);
  assert_eq!(owner.cached_snapshot().producer.pending_tasks, 0);
}
