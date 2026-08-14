use thiserror::Error;

use super::index_coordinator::IndexCoordinatorV1;
use super::index_producer_collector::{
  IndexCollectorDocumentRevisionTransitionV1, IndexCollectorScopeWorkV1, IndexParserExecutorV1, IndexProducerCollectorErrorV1,
  IndexProducerCollectorV1,
};
use super::index_producer_coordinator::{
  IndexProducerCompletionV1, IndexProducerCoordinatorErrorV1, IndexProducerCoordinatorV1, IndexProducerLeaseV1, IndexProducerSpillStoreV1,
};
use super::index_source::PluginMapperExecutorV1;

pub struct IndexProducerExecutionInputV1<'a> {
  pub semantic_state_root: &'a [u8],
  pub scope_work: Vec<IndexCollectorScopeWorkV1<'a>>,
  pub transition: IndexCollectorDocumentRevisionTransitionV1<'a>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexProducerExecutionErrorV1 {
  #[error("index producer leased task mismatch: {0}")]
  TaskMismatch(String),
  #[error("index producer collection failed: {0}")]
  Collector(#[from] IndexProducerCollectorErrorV1),
  #[error("index producer completion failed: {0}")]
  Completion(IndexProducerCoordinatorErrorV1),
  #[error("index producer lease release failed after {phase}: {source}")]
  LeaseRelease { phase: &'static str, source: IndexProducerCoordinatorErrorV1 },
  #[error("index producer execution was cancelled")]
  Cancelled,
}

/// Executes one exact document transition through the sole leased producer
/// path. Runtime source resolution remains outside this storage-neutral owner;
/// every supplied root and semantic definition set is rebound to the retained
/// task before parser or index work begins.
pub struct IndexProducerExecutorV1 {
  collector: IndexProducerCollectorV1,
}

impl IndexProducerExecutorV1 {
  pub fn new(collector: IndexProducerCollectorV1) -> Self {
    Self { collector }
  }

  #[allow(clippy::too_many_arguments)]
  pub fn execute_transition(
    &self,
    producer: &mut IndexProducerCoordinatorV1,
    mutations: &mut IndexCoordinatorV1,
    lease: &IndexProducerLeaseV1,
    input: IndexProducerExecutionInputV1<'_>,
    parser: &dyn IndexParserExecutorV1,
    mapper: Option<&dyn PluginMapperExecutorV1>,
    now_ms: u64,
    is_cancelled: &dyn Fn() -> bool,
    spill_store: &mut dyn IndexProducerSpillStoreV1,
  ) -> Result<IndexProducerCompletionV1, IndexProducerExecutionErrorV1> {
    if is_cancelled() {
      release_lease(producer, lease, "pre-collection cancellation")?;
      return Err(IndexProducerExecutionErrorV1::Cancelled);
    }
    if let Err(error) = validate_execution_input(producer, lease, &input) {
      release_lease(producer, lease, "task validation")?;
      return Err(error);
    }

    let collected = match self.collector.collect_scopes(input.scope_work, input.transition, parser, mapper, is_cancelled) {
      Ok(collected) => collected,
      Err(IndexProducerCollectorErrorV1::Cancelled) => {
        release_lease(producer, lease, "collection cancellation")?;
        return Err(IndexProducerExecutionErrorV1::Cancelled);
      }
      Err(error) => {
        release_lease(producer, lease, "collection failure")?;
        return Err(IndexProducerExecutionErrorV1::Collector(error));
      }
    };
    let (report, _report_reservation) = collected.into_parts();
    match producer.complete(lease, report, mutations, now_ms, is_cancelled(), spill_store) {
      Ok(completion) => Ok(completion),
      Err(IndexProducerCoordinatorErrorV1::Cancelled) => Err(IndexProducerExecutionErrorV1::Cancelled),
      Err(error) => {
        if producer.snapshot().leased_tasks != 0 {
          release_lease(producer, lease, "completion failure")?;
        }
        Err(IndexProducerExecutionErrorV1::Completion(error))
      }
    }
  }
}

fn validate_execution_input(
  producer: &IndexProducerCoordinatorV1,
  lease: &IndexProducerLeaseV1,
  input: &IndexProducerExecutionInputV1<'_>,
) -> Result<(), IndexProducerExecutionErrorV1> {
  let task = producer.leased_task(lease).map_err(IndexProducerExecutionErrorV1::Completion)?;
  if input.semantic_state_root != task.semantic_state_root() {
    return Err(IndexProducerExecutionErrorV1::TaskMismatch(
      "definition bundle does not belong to the task semantic-state root".to_string(),
    ));
  }
  if let Some(before) = input.transition.before {
    if before.namespace_root != task.namespace_root_before() {
      return Err(IndexProducerExecutionErrorV1::TaskMismatch(
        "before document does not belong to the task's exact source root".to_string(),
      ));
    }
    validate_task_scope(task.scope(), &before.file_record.path)?;
  }
  if let Some(after) = input.transition.after {
    if after.namespace_root != task.namespace_root_after() {
      return Err(IndexProducerExecutionErrorV1::TaskMismatch(
        "after document does not belong to the task's exact target root".to_string(),
      ));
    }
    validate_task_scope(task.scope(), &after.file_record.path)?;
  }
  Ok(())
}

fn validate_task_scope(scope: Option<&str>, path: &str) -> Result<(), IndexProducerExecutionErrorV1> {
  let Some(scope) = scope else {
    return Ok(());
  };
  if scope == "/" || path == scope || path.strip_prefix(scope).is_some_and(|suffix| suffix.starts_with('/')) {
    return Ok(());
  }
  Err(IndexProducerExecutionErrorV1::TaskMismatch(format!("document path '{path}' is outside leased maintenance scope '{scope}'")))
}

fn release_lease(
  producer: &mut IndexProducerCoordinatorV1,
  lease: &IndexProducerLeaseV1,
  phase: &'static str,
) -> Result<(), IndexProducerExecutionErrorV1> {
  producer.cancel(lease).map_err(|source| IndexProducerExecutionErrorV1::LeaseRelease { phase, source })
}
