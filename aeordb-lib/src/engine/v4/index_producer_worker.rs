use thiserror::Error;

use crate::engine::HashAlgorithm;

use super::index_coordinator::IndexCoordinatorV1;
use super::index_producer_collector::{CollectedIndexProducerReportV1, IndexParserExecutorV1};
use super::index_producer_coordinator::{
  IndexProducerCompletionV1, IndexProducerCoordinatorErrorV1, IndexProducerCoordinatorV1, IndexProducerLeaseV1, IndexProducerReportV1,
  IndexProducerSpillStoreV1,
};
use super::index_producer_executor::{IndexProducerExecutionErrorV1, IndexProducerExecutionInputV1, IndexProducerExecutorV1};
use super::index_producer_source::{
  IndexFileRevisionReadErrorClassV1, IndexFileRevisionSourceV1, IndexProducerSourceErrorV1, IndexSemanticScopeLimitsV1,
  IndexSemanticScopeReadErrorClassV1, IndexSemanticScopeResolutionV1, IndexSemanticScopeSourceV1, resolve_leased_mutation_record,
  resolve_mutation_document_transition, resolve_semantic_scope_work,
};
use super::index_source::PluginMapperExecutorV1;
use super::index_task::{MutationJournalV1, MutationRecordV1};

pub struct IndexProducerMutationWorkRequestV1<'request, 'journal> {
  pub lease: &'request IndexProducerLeaseV1,
  pub journal: &'request MutationJournalV1<'journal>,
  pub revision_source: &'request dyn IndexFileRevisionSourceV1,
  pub semantic_source: &'request dyn IndexSemanticScopeSourceV1,
  pub parser: &'request dyn IndexParserExecutorV1,
  pub mapper: Option<&'request dyn PluginMapperExecutorV1>,
  pub now_ms: u64,
  pub is_cancelled: &'request dyn Fn() -> bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IndexProducerWorkerOutcomeV1 {
  Completed(IndexProducerCompletionV1),
  ContentOnly { semantic_state_root: Vec<u8>, completion: IndexProducerCompletionV1 },
  SourceRetry { source: IndexProducerSourceErrorV1, completion: IndexProducerCompletionV1 },
}

pub(crate) struct IndexProducerMutationCollectionV1 {
  lease: IndexProducerLeaseV1,
  result: Result<IndexProducerMutationCollectionOutcomeV1, IndexProducerMutationCollectionErrorV1>,
}

enum IndexProducerMutationCollectionOutcomeV1 {
  Collected(CollectedIndexProducerReportV1),
  ContentOnly { semantic_state_root: Vec<u8> },
}

enum IndexProducerMutationCollectionErrorV1 {
  Source(IndexProducerSourceErrorV1),
  Execution(IndexProducerExecutionErrorV1),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexProducerWorkerErrorV1 {
  #[error("invalid index producer worker options: {0}")]
  InvalidOptions(String),
  #[error("index producer source resolution failed: {0}")]
  Source(IndexProducerSourceErrorV1),
  #[error("index producer execution failed: {0}")]
  Execution(IndexProducerExecutionErrorV1),
  #[error("index producer coordinator failed: {0}")]
  Coordinator(IndexProducerCoordinatorErrorV1),
  #[error("index producer lease release failed after source error {source}: {release}")]
  LeaseReleaseAfterSource { source: Box<IndexProducerSourceErrorV1>, release: IndexProducerCoordinatorErrorV1 },
  #[error("index producer retry coordination failed after source error {source}: {retry}")]
  RetryAfterSource { source: Box<IndexProducerSourceErrorV1>, retry: IndexProducerCoordinatorErrorV1 },
  #[error("index producer lease release failed after source error {source} and retry error {retry}: {release}")]
  LeaseReleaseAfterSourceRetry {
    source: Box<IndexProducerSourceErrorV1>,
    retry: Box<IndexProducerCoordinatorErrorV1>,
    release: IndexProducerCoordinatorErrorV1,
  },
  #[error("index producer lease release failed after coordinator error {source}: {release}")]
  LeaseReleaseAfterCoordinator { source: Box<IndexProducerCoordinatorErrorV1>, release: IndexProducerCoordinatorErrorV1 },
  #[error("index producer worker was cancelled")]
  Cancelled,
}

pub struct IndexProducerMutationWorkerV1 {
  hash_algorithm: HashAlgorithm,
  executor: IndexProducerExecutorV1,
  semantic_limits: IndexSemanticScopeLimitsV1,
  source_retry_after_ms: u64,
}

impl IndexProducerMutationWorkerV1 {
  pub fn new(
    hash_algorithm: HashAlgorithm,
    executor: IndexProducerExecutorV1,
    semantic_limits: IndexSemanticScopeLimitsV1,
    source_retry_after_ms: u64,
  ) -> Result<Self, IndexProducerWorkerErrorV1> {
    if source_retry_after_ms == 0 {
      return Err(IndexProducerWorkerErrorV1::InvalidOptions("source retry delay must be nonzero".to_string()));
    }
    Ok(Self { hash_algorithm, executor, semantic_limits, source_retry_after_ms })
  }

  pub fn execute(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    mutations: &mut IndexCoordinatorV1,
    request: IndexProducerMutationWorkRequestV1<'_, '_>,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    let record = match resolve_leased_mutation_record(self.hash_algorithm, producer, request.lease, request.journal, request.is_cancelled) {
      Ok(record) => record,
      Err(error) => return self.handle_source_failure(producer, request.lease, error, request.now_ms, request.is_cancelled, spill_store),
    };
    let now_ms = request.now_ms;
    let is_cancelled = request.is_cancelled;
    let collection = self.collect_resolved_mutation(record, request);
    self.finish_mutation_collection(producer, mutations, collection, now_ms, is_cancelled, spill_store)
  }

  pub(crate) fn collect_resolved_mutation(
    &self,
    record: MutationRecordV1<'_>,
    request: IndexProducerMutationWorkRequestV1<'_, '_>,
  ) -> IndexProducerMutationCollectionV1 {
    let lease = request.lease.clone();
    let result = self.collect_resolved_mutation_inner(&record, request);
    IndexProducerMutationCollectionV1 { lease, result }
  }

  fn collect_resolved_mutation_inner(
    &self,
    record: &MutationRecordV1<'_>,
    request: IndexProducerMutationWorkRequestV1<'_, '_>,
  ) -> Result<IndexProducerMutationCollectionOutcomeV1, IndexProducerMutationCollectionErrorV1> {
    let transition_read =
      match resolve_mutation_document_transition(self.hash_algorithm, record, request.revision_source, request.is_cancelled) {
        Ok(transition) => transition,
        Err(error) => return Err(IndexProducerMutationCollectionErrorV1::Source(error)),
      };
    let transition = transition_read.transition();
    let scope_read = match resolve_semantic_scope_work(
      self.hash_algorithm,
      request.lease.operation_id(),
      request.lease.publication_sequence(),
      request.journal.semantic_state_root,
      transition,
      request.semantic_source,
      self.semantic_limits,
      request.is_cancelled,
    ) {
      Ok(scopes) => scopes,
      Err(error) => return Err(IndexProducerMutationCollectionErrorV1::Source(error)),
    };
    let (scopes, _scope_reservation) = scope_read.into_parts();

    match scopes {
      IndexSemanticScopeResolutionV1::Complete { semantic_state_root, scope_work } => {
        let collector_scope_work = scope_work.iter().map(|scope| scope.as_collector_scope_work()).collect();
        let collected = self.executor.collect_transition(
          IndexProducerExecutionInputV1 {
            semantic_state_root: &semantic_state_root,
            scope_work: collector_scope_work,
            transition: transition.as_collector_transition(),
          },
          request.parser,
          request.mapper,
          request.is_cancelled,
        );
        collected.map(IndexProducerMutationCollectionOutcomeV1::Collected).map_err(IndexProducerMutationCollectionErrorV1::Execution)
      }
      IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root } => {
        Ok(IndexProducerMutationCollectionOutcomeV1::ContentOnly { semantic_state_root })
      }
    }
  }

  pub(crate) fn finish_mutation_collection(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    mutations: &mut IndexCoordinatorV1,
    collection: IndexProducerMutationCollectionV1,
    now_ms: u64,
    is_cancelled: &dyn Fn() -> bool,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    let lease = collection.lease;
    match collection.result {
      Ok(IndexProducerMutationCollectionOutcomeV1::Collected(collected)) => {
        match self.executor.complete_collected(producer, mutations, &lease, collected, now_ms, is_cancelled, spill_store) {
          Ok(completion) => Ok(IndexProducerWorkerOutcomeV1::Completed(completion)),
          Err(IndexProducerExecutionErrorV1::Cancelled) => Err(IndexProducerWorkerErrorV1::Cancelled),
          Err(error) => Err(IndexProducerWorkerErrorV1::Execution(error)),
        }
      }
      Ok(IndexProducerMutationCollectionOutcomeV1::ContentOnly { semantic_state_root }) => {
        let completion =
          producer.complete(&lease, IndexProducerReportV1 { outcomes: Vec::new() }, mutations, now_ms, is_cancelled(), spill_store);
        match completion {
          Ok(completion) => Ok(IndexProducerWorkerOutcomeV1::ContentOnly { semantic_state_root, completion }),
          Err(IndexProducerCoordinatorErrorV1::Cancelled) => Err(IndexProducerWorkerErrorV1::Cancelled),
          Err(error) => self.coordinator_failure(producer, &lease, error),
        }
      }
      Err(IndexProducerMutationCollectionErrorV1::Source(source)) => {
        self.handle_source_failure(producer, &lease, source, now_ms, is_cancelled, spill_store)
      }
      Err(IndexProducerMutationCollectionErrorV1::Execution(error)) => self.release_after_execution(producer, &lease, error),
    }
  }

  pub(crate) fn finish_source_failure(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    lease: &IndexProducerLeaseV1,
    source: IndexProducerSourceErrorV1,
    now_ms: u64,
    is_cancelled: &dyn Fn() -> bool,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    self.handle_source_failure(producer, lease, source, now_ms, is_cancelled, spill_store)
  }

  fn release_after_execution(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    lease: &IndexProducerLeaseV1,
    error: IndexProducerExecutionErrorV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    let cancelled = error == IndexProducerExecutionErrorV1::Cancelled;
    match producer.cancel(lease) {
      Ok(()) if cancelled => Err(IndexProducerWorkerErrorV1::Cancelled),
      Ok(()) => Err(IndexProducerWorkerErrorV1::Execution(error)),
      Err(source) => Err(IndexProducerWorkerErrorV1::Execution(IndexProducerExecutionErrorV1::LeaseRelease {
        phase: if cancelled { "collection cancellation" } else { "collection failure" },
        source,
      })),
    }
  }

  fn handle_source_failure(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    lease: &IndexProducerLeaseV1,
    source: IndexProducerSourceErrorV1,
    now_ms: u64,
    is_cancelled: &dyn Fn() -> bool,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    if source == IndexProducerSourceErrorV1::Cancelled {
      return match producer.cancel(lease) {
        Ok(()) => Err(IndexProducerWorkerErrorV1::Cancelled),
        Err(release) => Err(IndexProducerWorkerErrorV1::LeaseReleaseAfterSource { source: Box::new(source), release }),
      };
    }
    if source_is_retryable(&source) {
      let completion = producer.retry_task(lease, self.source_retry_after_ms, now_ms, is_cancelled(), spill_store);
      return match completion {
        Ok(completion) => Ok(IndexProducerWorkerOutcomeV1::SourceRetry { source, completion }),
        Err(IndexProducerCoordinatorErrorV1::Cancelled) => Err(IndexProducerWorkerErrorV1::Cancelled),
        Err(retry) => self.retry_failure_after_source(producer, lease, source, retry),
      };
    }
    self.release_after_source(producer, lease, source)
  }

  fn release_after_source(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    lease: &IndexProducerLeaseV1,
    source: IndexProducerSourceErrorV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    match producer.cancel(lease) {
      Ok(()) => Err(IndexProducerWorkerErrorV1::Source(source)),
      Err(release) => Err(IndexProducerWorkerErrorV1::LeaseReleaseAfterSource { source: Box::new(source), release }),
    }
  }

  fn coordinator_failure(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    lease: &IndexProducerLeaseV1,
    source: IndexProducerCoordinatorErrorV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    if matches!(source, IndexProducerCoordinatorErrorV1::ForeignLease | IndexProducerCoordinatorErrorV1::StaleLease) {
      return Err(IndexProducerWorkerErrorV1::Coordinator(source));
    }
    if producer.snapshot().leased_tasks == 0 {
      return Err(IndexProducerWorkerErrorV1::Coordinator(source));
    }
    match producer.cancel(lease) {
      Ok(()) => Err(IndexProducerWorkerErrorV1::Coordinator(source)),
      Err(release) => Err(IndexProducerWorkerErrorV1::LeaseReleaseAfterCoordinator { source: Box::new(source), release }),
    }
  }

  fn retry_failure_after_source(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    lease: &IndexProducerLeaseV1,
    source: IndexProducerSourceErrorV1,
    retry: IndexProducerCoordinatorErrorV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    if producer.snapshot().leased_tasks == 0 {
      return Err(IndexProducerWorkerErrorV1::RetryAfterSource { source: Box::new(source), retry });
    }
    match producer.cancel(lease) {
      Ok(()) => Err(IndexProducerWorkerErrorV1::RetryAfterSource { source: Box::new(source), retry }),
      Err(release) => {
        Err(IndexProducerWorkerErrorV1::LeaseReleaseAfterSourceRetry { source: Box::new(source), retry: Box::new(retry), release })
      }
    }
  }
}

fn source_is_retryable(source: &IndexProducerSourceErrorV1) -> bool {
  matches!(
    source,
    IndexProducerSourceErrorV1::RevisionRead(error) if error.class() == IndexFileRevisionReadErrorClassV1::Retryable
  ) || matches!(
    source,
    IndexProducerSourceErrorV1::SemanticRead(error) if error.class() == IndexSemanticScopeReadErrorClassV1::Retryable
  )
}
