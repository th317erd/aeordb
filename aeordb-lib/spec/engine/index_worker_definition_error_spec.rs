use super::*;
use crate::engine::memory_coordinator::MemoryPolicy;
use crate::engine::v4::index_producer_collector::{IndexProducerCollectorOptionsV1, IndexProducerCollectorV1};
use crate::engine::v4::index_producer_coordinator::{
  IndexProducerCoordinatorOptionsV1, IndexProducerSpillErrorV1, IndexProducerSpillReasonV1, IndexProducerSpillReceiptV1,
  IndexProducerTaskKindV1, IndexProducerTaskRequestV1, IndexProducerTaskViewV1,
};
use crate::engine::v4::reader::{FormatError, MalformedInputClass};

struct NoSpill;
impl IndexProducerSpillStoreV1 for NoSpill {
  fn spill(
    &mut self,
    _: IndexProducerTaskViewV1<'_>,
    _: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    panic!("first retry must remain bounded in memory")
  }
}

fn fixture() -> (IndexProducerMutationWorkerV1, IndexProducerCoordinatorV1, IndexProducerLeaseV1) {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = MemoryCoordinator::new(MemoryPolicy::new(16 << 20, 32 << 20, 1, 4 << 20).unwrap());
  let collector = IndexProducerCollectorV1::new(
    algorithm,
    memory.clone(),
    IndexProducerCollectorOptionsV1::new(8, 16, 32, 2 << 20, 256, 2 << 20, 50).unwrap(),
  )
  .unwrap();
  let worker = IndexProducerMutationWorkerV1::new(
    algorithm,
    memory.clone(),
    IndexProducerExecutorV1::new(collector),
    IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 << 20).unwrap(),
    25,
  )
  .unwrap();
  let mut producer = IndexProducerCoordinatorV1::new(
    algorithm,
    memory,
    IndexProducerCoordinatorOptionsV1::new(8, 2 << 20, 3, 10, 1000, 16, 256, 2 << 20).unwrap(),
  )
  .unwrap();
  producer
    .admit(
      IndexProducerTaskRequestV1 {
        operation_id: [1; 16],
        kind: IndexProducerTaskKindV1::Build,
        publication_sequence: 1,
        namespace_root_before: &[2; 32],
        namespace_root_after: &[2; 32],
        semantic_state_root: &[4; 32],
        journal_head: None,
        scope: Some("/"),
      },
      100,
    )
    .unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  (worker, producer, lease)
}

#[test]
fn retained_definition_allocation_error_schedules_worker_backoff() {
  let (worker, mut producer, lease) = fixture();
  let source = IndexProducerSourceErrorV1::Format(FormatError::allocation_failure("selector_decode_allocation", "test host refusal"));
  let outcome = worker.finish_source_failure(&mut producer, &lease, source, 101, &|| false, &mut NoSpill).unwrap();
  assert!(matches!(
    outcome,
    IndexProducerWorkerOutcomeV1::SourceRetry {
      completion: IndexProducerCompletionV1::RetryScheduled { attempt: 1, next_retry_at_ms: 126, .. },
      ..
    }
  ));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert!(producer.lease_next(125, false).unwrap().is_none());
  let retry = producer.lease_next(126, false).unwrap().unwrap();
  producer.cancel(&retry).unwrap();
}

#[test]
fn retained_definition_limits_remain_terminal_and_release_worker_lease() {
  let (worker, mut producer, lease) = fixture();
  let source = IndexProducerSourceErrorV1::Format(FormatError::new(
    MalformedInputClass::AllocationAmplification,
    "selector_decode_allocation",
    "same diagnostic text, deterministic malformed bytes",
  ));
  let error = worker.finish_source_failure(&mut producer, &lease, source, 101, &|| false, &mut NoSpill).unwrap_err();
  assert!(matches!(error, IndexProducerWorkerErrorV1::Source(IndexProducerSourceErrorV1::Format(_))));
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(producer.snapshot().pending_tasks, 1);
}

#[test]
fn retained_definition_resource_retry_respects_cancellation() {
  let (worker, mut producer, lease) = fixture();
  let source = IndexProducerSourceErrorV1::Format(FormatError::allocation_failure("selector_decode_allocation", "test host refusal"));
  let error = worker.finish_source_failure(&mut producer, &lease, source, 101, &|| true, &mut NoSpill).unwrap_err();
  assert!(matches!(error, IndexProducerWorkerErrorV1::Cancelled));
  assert_eq!(producer.snapshot().leased_tasks, 0);
}
