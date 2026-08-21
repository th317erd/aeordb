use thiserror::Error;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};

use super::index_coordinator::IndexCoordinatorV1;
use super::index_maintenance_scan::{
  IndexMaintenanceScanDocumentV1, IndexMaintenanceScanReadErrorClassV1, IndexProducerServiceModeV1,
  derive_index_maintenance_document_operation_id_v1, index_producer_service_mode_v1,
};
use super::index_producer_collector::{CollectedIndexProducerReportV1, IndexParserExecutorV1};
use super::index_producer_coordinator::{
  IndexProducerCompletionV1, IndexProducerCoordinatorErrorV1, IndexProducerCoordinatorV1, IndexProducerLeaseV1,
  IndexProducerMaintenanceDocumentRequestV1, IndexProducerMaintenanceProgressV1, IndexProducerReportV1, IndexProducerSpillStoreV1,
};
use super::index_producer_executor::{IndexProducerExecutionErrorV1, IndexProducerExecutionInputV1, IndexProducerExecutorV1};
use super::index_producer_journal_source::IndexProducerJournalReadErrorClassV1;
use super::index_producer_source::{
  IndexFileRevisionReadErrorClassV1, IndexFileRevisionSourceV1, IndexProducerSourceErrorV1, IndexSemanticScopeLimitsV1,
  IndexSemanticScopeReadErrorClassV1, IndexSemanticScopeResolutionV1, IndexSemanticScopeSourceV1, resolve_leased_mutation_record,
  resolve_mutation_document_transition, resolve_semantic_scope_work, ResolvedIndexDocumentTransitionV1, ResolvedIndexDocumentV1,
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

pub struct IndexProducerMaintenanceWorkRequestV1<'request> {
  pub lease: &'request IndexProducerLeaseV1,
  pub namespace_root: &'request [u8],
  pub semantic_state_root: &'request [u8],
  pub document: IndexMaintenanceScanDocumentV1,
  pub semantic_source: &'request dyn IndexSemanticScopeSourceV1,
  pub parser: &'request dyn IndexParserExecutorV1,
  pub mapper: Option<&'request dyn PluginMapperExecutorV1>,
  pub is_cancelled: &'request dyn Fn() -> bool,
}

pub(crate) struct IndexProducerMaintenancePageRequestV1<'request> {
  pub lease: &'request IndexProducerLeaseV1,
  pub processed_documents: u32,
  pub complete: bool,
  pub now_ms: u64,
  pub is_cancelled: &'request dyn Fn() -> bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IndexProducerWorkerOutcomeV1 {
  Completed(IndexProducerCompletionV1),
  ContentOnly { semantic_state_root: Vec<u8>, completion: IndexProducerCompletionV1 },
  SourceRetry { source: IndexProducerSourceErrorV1, completion: IndexProducerCompletionV1 },
  MaintenanceDocument(IndexProducerMaintenanceProgressV1),
  MaintenancePage { processed_documents: u32, complete: bool, completion: Option<IndexProducerCompletionV1> },
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

pub(crate) struct IndexProducerMaintenanceCollectionV1 {
  lease: IndexProducerLeaseV1,
  revision_hash: Vec<u8>,
  path: String,
  result: Result<IndexProducerMutationCollectionOutcomeV1, IndexProducerMutationCollectionErrorV1>,
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
  memory: MemoryCoordinator,
  executor: IndexProducerExecutorV1,
  semantic_limits: IndexSemanticScopeLimitsV1,
  source_retry_after_ms: u64,
}

impl IndexProducerMutationWorkerV1 {
  pub fn new(
    hash_algorithm: HashAlgorithm,
    memory: MemoryCoordinator,
    executor: IndexProducerExecutorV1,
    semantic_limits: IndexSemanticScopeLimitsV1,
    source_retry_after_ms: u64,
  ) -> Result<Self, IndexProducerWorkerErrorV1> {
    if source_retry_after_ms == 0 {
      return Err(IndexProducerWorkerErrorV1::InvalidOptions("source retry delay must be nonzero".to_string()));
    }
    Ok(Self { hash_algorithm, memory, executor, semantic_limits, source_retry_after_ms })
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

  pub(crate) fn collect_maintenance_document(
    &self,
    request: IndexProducerMaintenanceWorkRequestV1<'_>,
  ) -> IndexProducerMaintenanceCollectionV1 {
    let lease = request.lease.clone();
    let (namespace_root, _root_reservation) = match self.clone_maintenance_root(request.namespace_root) {
      Ok(root) => root,
      Err(error) => {
        return IndexProducerMaintenanceCollectionV1 {
          lease,
          revision_hash: request.document.revision_hash,
          path: request.document.file_record.path,
          result: Err(IndexProducerMutationCollectionErrorV1::Source(error)),
        };
      }
    };
    let document_operation_id = match derive_index_maintenance_document_operation_id_v1(
      self.hash_algorithm,
      request.lease.operation_id(),
      request.lease.kind(),
      request.namespace_root,
      &request.document.revision_hash,
      &request.document.file_record.path,
    ) {
      Ok(operation_id) => operation_id,
      Err(error) => {
        return IndexProducerMaintenanceCollectionV1 {
          lease,
          revision_hash: request.document.revision_hash,
          path: request.document.file_record.path,
          result: Err(IndexProducerMutationCollectionErrorV1::Source(IndexProducerSourceErrorV1::TaskMismatch(error.to_string()))),
        };
      }
    };
    let resolved =
      ResolvedIndexDocumentV1 { namespace_root, revision_hash: request.document.revision_hash, file_record: request.document.file_record };
    let mut transition = match index_producer_service_mode_v1(request.lease.kind()) {
      IndexProducerServiceModeV1::AuthoritativeUpsertScan => ResolvedIndexDocumentTransitionV1 { before: None, after: Some(resolved) },
      IndexProducerServiceModeV1::AuthoritativeRetirementScan => ResolvedIndexDocumentTransitionV1 { before: Some(resolved), after: None },
      IndexProducerServiceModeV1::JournalTransition | IndexProducerServiceModeV1::ArtifactCompaction => {
        let revision_hash = resolved.revision_hash;
        let path = resolved.file_record.path;
        return IndexProducerMaintenanceCollectionV1 {
          lease,
          revision_hash,
          path,
          result: Err(IndexProducerMutationCollectionErrorV1::Source(IndexProducerSourceErrorV1::TaskMismatch(
            "leased task is not authoritative document-scan work".to_string(),
          ))),
        };
      }
    };
    let result = self.collect_resolved_transition_inner(
      document_operation_id,
      request.lease.publication_sequence(),
      request.semantic_state_root,
      &transition,
      request.semantic_source,
      request.parser,
      request.mapper,
      request.is_cancelled,
    );
    let resolved = match transition.before.take() {
      Some(resolved) => Some(resolved),
      None => transition.after.take(),
    };
    let Some(resolved) = resolved else {
      return IndexProducerMaintenanceCollectionV1 {
        lease,
        revision_hash: Vec::new(),
        path: String::new(),
        result: Err(IndexProducerMutationCollectionErrorV1::Source(IndexProducerSourceErrorV1::TaskMismatch(
          "maintenance transition lost its exact document".to_string(),
        ))),
      };
    };
    IndexProducerMaintenanceCollectionV1 { lease, revision_hash: resolved.revision_hash, path: resolved.file_record.path, result }
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
    self.collect_resolved_transition_inner(
      request.lease.operation_id(),
      request.lease.publication_sequence(),
      request.journal.semantic_state_root,
      transition_read.transition(),
      request.semantic_source,
      request.parser,
      request.mapper,
      request.is_cancelled,
    )
  }

  #[allow(clippy::too_many_arguments)]
  fn collect_resolved_transition_inner(
    &self,
    operation_id: [u8; 16],
    publication_sequence: u64,
    semantic_state_root: &[u8],
    transition: &ResolvedIndexDocumentTransitionV1,
    semantic_source: &dyn IndexSemanticScopeSourceV1,
    parser: &dyn IndexParserExecutorV1,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    is_cancelled: &dyn Fn() -> bool,
  ) -> Result<IndexProducerMutationCollectionOutcomeV1, IndexProducerMutationCollectionErrorV1> {
    let scope_read = match resolve_semantic_scope_work(
      self.hash_algorithm,
      operation_id,
      publication_sequence,
      semantic_state_root,
      transition,
      semantic_source,
      self.semantic_limits,
      is_cancelled,
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
          parser,
          mapper,
          is_cancelled,
        );
        collected.map(IndexProducerMutationCollectionOutcomeV1::Collected).map_err(IndexProducerMutationCollectionErrorV1::Execution)
      }
      IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root } => {
        Ok(IndexProducerMutationCollectionOutcomeV1::ContentOnly { semantic_state_root })
      }
    }
  }

  fn clone_maintenance_root(&self, namespace_root: &[u8]) -> Result<(Vec<u8>, MemoryReservation), IndexProducerSourceErrorV1> {
    let requested_bytes = u64::try_from(namespace_root.len())
      .map_err(|error| IndexProducerSourceErrorV1::Allocation(format!("maintenance namespace-root bytes exceed u64: {error}")))?;
    let mut reservation = self
      .memory
      .reserve(MemoryOwner::Task, requested_bytes, AdmissionClass::Workload)
      .map_err(|error| IndexProducerSourceErrorV1::Allocation(error.to_string()))?;
    let mut root = Vec::new();
    root
      .try_reserve_exact(namespace_root.len())
      .map_err(|error| IndexProducerSourceErrorV1::Allocation(format!("cannot reserve maintenance namespace root: {error}")))?;
    root.extend_from_slice(namespace_root);
    let retained_bytes = u64::try_from(root.capacity())
      .map_err(|error| IndexProducerSourceErrorV1::Allocation(format!("maintenance namespace-root capacity exceeds u64: {error}")))?;
    if retained_bytes > requested_bytes {
      reservation.grow(retained_bytes - requested_bytes).map_err(|error| IndexProducerSourceErrorV1::Allocation(error.to_string()))?;
    }
    Ok((root, reservation))
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

  pub(crate) fn finish_maintenance_collection(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    mutations: &mut IndexCoordinatorV1,
    collection: IndexProducerMaintenanceCollectionV1,
    now_ms: u64,
    is_cancelled: &dyn Fn() -> bool,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    let lease = collection.lease;
    match collection.result {
      Ok(IndexProducerMutationCollectionOutcomeV1::Collected(collected)) => {
        let (report, _report_reservation) = collected.into_parts();
        self.advance_maintenance_report(
          producer,
          mutations,
          &lease,
          &collection.revision_hash,
          &collection.path,
          report,
          now_ms,
          is_cancelled,
          spill_store,
        )
      }
      Ok(IndexProducerMutationCollectionOutcomeV1::ContentOnly { .. }) => self.advance_maintenance_report(
        producer,
        mutations,
        &lease,
        &collection.revision_hash,
        &collection.path,
        IndexProducerReportV1 { outcomes: Vec::new() },
        now_ms,
        is_cancelled,
        spill_store,
      ),
      Err(IndexProducerMutationCollectionErrorV1::Source(source)) => {
        self.handle_source_failure(producer, &lease, source, now_ms, is_cancelled, spill_store)
      }
      Err(IndexProducerMutationCollectionErrorV1::Execution(error)) => self.release_after_execution(producer, &lease, error),
    }
  }

  pub(crate) fn finish_maintenance_page(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    mutations: &mut IndexCoordinatorV1,
    request: IndexProducerMaintenancePageRequestV1<'_>,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    if (request.is_cancelled)() {
      return match producer.cancel(request.lease) {
        Ok(()) => Err(IndexProducerWorkerErrorV1::Cancelled),
        Err(source) => Err(IndexProducerWorkerErrorV1::Coordinator(source)),
      };
    }
    if !request.complete {
      if request.processed_documents == 0 {
        return self.release_after_source(
          producer,
          request.lease,
          IndexProducerSourceErrorV1::TaskMismatch("an incomplete maintenance page made no document progress".to_string()),
        );
      }
      return match producer.cancel(request.lease) {
        Ok(()) => Ok(IndexProducerWorkerOutcomeV1::MaintenancePage {
          processed_documents: request.processed_documents,
          complete: false,
          completion: None,
        }),
        Err(source) => Err(IndexProducerWorkerErrorV1::Coordinator(source)),
      };
    }
    match producer.complete(request.lease, IndexProducerReportV1 { outcomes: Vec::new() }, mutations, request.now_ms, false, spill_store) {
      Ok(completion) => Ok(IndexProducerWorkerOutcomeV1::MaintenancePage {
        processed_documents: request.processed_documents,
        complete: true,
        completion: Some(completion),
      }),
      Err(IndexProducerCoordinatorErrorV1::Cancelled) => Err(IndexProducerWorkerErrorV1::Cancelled),
      Err(error) => self.coordinator_failure(producer, request.lease, error),
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn advance_maintenance_report(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    mutations: &mut IndexCoordinatorV1,
    lease: &IndexProducerLeaseV1,
    revision_hash: &[u8],
    path: &str,
    report: IndexProducerReportV1,
    now_ms: u64,
    is_cancelled: &dyn Fn() -> bool,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerWorkerOutcomeV1, IndexProducerWorkerErrorV1> {
    match producer.advance_maintenance_document(
      lease,
      IndexProducerMaintenanceDocumentRequestV1 { revision_hash, path, report },
      mutations,
      now_ms,
      is_cancelled(),
      spill_store,
    ) {
      Ok(progress) => Ok(IndexProducerWorkerOutcomeV1::MaintenanceDocument(progress)),
      Err(IndexProducerCoordinatorErrorV1::Cancelled) => Err(IndexProducerWorkerErrorV1::Cancelled),
      Err(error) => self.coordinator_failure(producer, lease, error),
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
    if source_is_cancelled(&source) {
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
  matches!(source, IndexProducerSourceErrorV1::Allocation(_))
    || matches!(
      source,
      IndexProducerSourceErrorV1::Coordinator(
        IndexProducerCoordinatorErrorV1::SpillRequired { .. }
          | IndexProducerCoordinatorErrorV1::MemoryAuthority(_)
          | IndexProducerCoordinatorErrorV1::Allocation(_)
      )
    )
    || matches!(
      source,
      IndexProducerSourceErrorV1::RevisionRead(error) if error.class() == IndexFileRevisionReadErrorClassV1::Retryable
    )
    || matches!(
      source,
      IndexProducerSourceErrorV1::SemanticRead(error) if error.class() == IndexSemanticScopeReadErrorClassV1::Retryable
    )
    || matches!(
      source,
      IndexProducerSourceErrorV1::MaintenanceScan(error) if error.class() == IndexMaintenanceScanReadErrorClassV1::Retryable
    )
    || matches!(
      source,
      IndexProducerSourceErrorV1::JournalRead(error) if error.class() == IndexProducerJournalReadErrorClassV1::Retryable
    )
}

fn source_is_cancelled(source: &IndexProducerSourceErrorV1) -> bool {
  matches!(source, IndexProducerSourceErrorV1::Cancelled)
    || matches!(
      source,
      IndexProducerSourceErrorV1::MaintenanceScan(error) if error.class() == IndexMaintenanceScanReadErrorClassV1::Cancelled
    )
    || matches!(
      source,
      IndexProducerSourceErrorV1::JournalRead(error) if error.class() == IndexProducerJournalReadErrorClassV1::Cancelled
    )
}
