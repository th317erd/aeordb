use std::sync::Arc;

use crate::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use crate::engine::v4::coverage_runtime::SoftMutationHubOptionsV1;
use crate::engine::v4::index_coordinator::{FrozenIndexBatchV1, IndexCoordinatorOptionsV1};
use crate::engine::v4::index_producer_collector::IndexProducerCollectorOptionsV1;
use crate::engine::v4::index_producer_coordinator::IndexProducerCoordinatorOptionsV1;
use crate::engine::v4::index_producer_source::IndexSemanticScopeLimitsV1;
use crate::engine::v4::index_runtime_owner::{
  IndexRuntimeBatchPublisherV1, IndexRuntimeLifecycleV1, IndexRuntimeOwnerOptionsV1, IndexRuntimeOwnerV1, IndexRuntimePublicationErrorV1,
  IndexRuntimePublicationReceiptV1, IndexRuntimeRecoveryDecisionV1,
};
use crate::engine::{HashAlgorithm, MockClock, VirtualClock};

use super::*;

struct UnusedPublisher;

impl IndexRuntimeBatchPublisherV1 for UnusedPublisher {
  fn publish(&mut self, _batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    panic!("idle cadence unexpectedly invoked its publisher")
  }
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
