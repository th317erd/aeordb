use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use crate::engine::namespace_mutation::{NamespaceMutationAcknowledgement, NamespaceMutationKind};

use super::*;

fn options() -> IndexRuntimeOwnerOptionsV1 {
  IndexRuntimeOwnerOptionsV1 {
    soft_hub: SoftMutationHubOptionsV1::engine_default(),
    producer: IndexProducerCoordinatorOptionsV1::new(32, 2 * 1_024 * 1_024, 3, 10, 1_000, 16, 256, 2 * 1_024 * 1_024).unwrap(),
    mutations: IndexCoordinatorOptionsV1::new(4 * 1_024 * 1_024, 262_144, 30_000, 256 * 1_024).unwrap(),
    collector: IndexProducerCollectorOptionsV1::new(8, 16, 32, 2 * 1_024 * 1_024, 256, 2 * 1_024 * 1_024, 50).unwrap(),
    semantic: IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 * 1_024 * 1_024).unwrap(),
    source_retry_after_ms: 25,
    publication_retry_after_ms: 100,
  }
}

fn owner_with_hub() -> (IndexRuntimeOwnerV1, Arc<SoftMutationHubV1>) {
  let options = options();
  let hub = Arc::new(SoftMutationHubV1::new(options.soft_hub).unwrap());
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 * 1_024 * 1_024, 64 * 1_024 * 1_024, 1, 4 * 1_024 * 1_024).unwrap());
  let owner = IndexRuntimeOwnerV1::new_with_soft_hub([0x51; 16], HashAlgorithm::Blake3_256, memory, options, 1, Arc::clone(&hub)).unwrap();
  (owner, hub)
}

fn acknowledgement(sequence: u64) -> NamespaceMutationAcknowledgement {
  NamespaceMutationAcknowledgement {
    operation_id: uuid::Uuid::from_bytes([0x61; 16]),
    kind: NamespaceMutationKind::FileWrite,
    publication_sequence: sequence,
    previous_root_hash: vec![0x41; 32],
    root_hash: vec![0x42; 32],
    source_identities: Vec::new(),
    locator_replacements: Vec::new(),
  }
}

#[test]
fn contended_shared_hub_updates_cached_loss_without_waiting() {
  let (owner, hub) = owner_with_hub();
  let queue = hub.lock_queue_for_test().unwrap();
  let started = Instant::now();
  assert_eq!(
    owner.offer_acknowledgement(&acknowledgement(7)),
    SoftMutationAdmissionV1::ReconciliationRequired(SoftMutationLossReasonV1::QueueContended),
  );
  assert!(started.elapsed() < Duration::from_millis(250));
  drop(queue);
  let snapshot = owner.cached_snapshot();
  assert!(snapshot.soft_hub.reconciliation_required);
  assert_eq!(snapshot.soft_hub.lost_through_sequence, Some(7));
  assert!(snapshot.soft_hub.loss_reasons.contains(&SoftMutationLossReasonV1::QueueContended));
}

#[test]
fn poisoned_shared_hub_is_cached_as_queue_unavailable() {
  let (owner, hub) = owner_with_hub();
  let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    let _queue = hub.lock_queue_for_test().unwrap();
    panic!("inject queue poison");
  }));
  assert!(poisoned.is_err());
  assert_eq!(
    owner.offer_acknowledgement(&acknowledgement(8)),
    SoftMutationAdmissionV1::ReconciliationRequired(SoftMutationLossReasonV1::QueueUnavailable),
  );
  let snapshot = owner.cached_snapshot();
  assert!(snapshot.soft_hub.reconciliation_required);
  assert_eq!(snapshot.soft_hub.lost_through_sequence, Some(8));
  assert!(snapshot.soft_hub.loss_reasons.contains(&SoftMutationLossReasonV1::QueueUnavailable));
  hub.clear_queue_poison_for_test();
  let authoritative = hub.snapshot().unwrap();
  assert_eq!(authoritative.dropped_notices, 1, "one lost acknowledgement must be counted once");
  assert_eq!(authoritative.loss_epoch, 1, "one lost acknowledgement must advance one loss epoch");
}

#[test]
fn installation_refresh_observes_pre_install_queue_and_loss() {
  let (owner, hub) = owner_with_hub();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 0, highest_checkpoint_sequence: 0 }).unwrap();
  assert_eq!(owner.cached_snapshot().soft_hub.queued_notices, 0);

  assert_eq!(hub.offer_acknowledgement(&acknowledgement(9)), SoftMutationAdmissionV1::Accepted);
  assert_eq!(owner.cached_snapshot().soft_hub.queued_notices, 0, "disconnected owner cache should still be stale before installation");
  assert!(owner.has_pending_soft_mutations(), "scheduler admission must not depend on stale cached telemetry");
  owner.refresh_for_installation().unwrap();
  let queued = owner.cached_snapshot();
  assert_eq!(queued.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(queued.soft_hub.queued_notices, 1);

  assert_eq!(
    hub.force_reconciliation_required(10, SoftMutationLossReasonV1::QueueFull),
    SoftMutationAdmissionV1::ReconciliationRequired(SoftMutationLossReasonV1::QueueFull),
  );
  owner.refresh_for_installation().unwrap();
  let degraded = owner.cached_snapshot();
  assert_eq!(degraded.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(degraded.degraded.as_ref().unwrap().code, "soft_mutation_loss");
  assert_eq!(degraded.soft_hub.lost_through_sequence, Some(10));
}
