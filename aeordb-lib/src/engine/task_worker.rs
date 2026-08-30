use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engine::backup;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::engine_event::{
  EngineEvent, EVENT_TASKS_CANCELLED, EVENT_TASKS_COMPLETED, EVENT_TASKS_DEFERRED, EVENT_TASKS_FAILED, EVENT_TASKS_STARTED,
};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::event_bus::EventBus;
use crate::engine::gc::{execute_gc_run, GcExecutionRequestV1};
use crate::engine::index_store::{
  IndexManager, IndexWriteBuffer, IndexWriteBufferOptions, DEFAULT_INDEX_BUFFER_FLUSH_INTERVAL, DEFAULT_INDEX_BUFFER_FLUSH_WRITES,
};
use crate::engine::index_config_resolver::{glob_matches, IndexConfigResolver};
use crate::engine::indexing_pipeline::IndexingPipeline;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinatorError, MemoryOwner};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::path_utils::normalize_path;
use crate::engine::request_context::RequestContext;
use crate::engine::run_configuration::MaintenanceRunConfiguration;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::task_queue::{ProgressInfo, TaskOriginV1, TaskQueue, TaskRecord, TaskStatus};
use crate::engine::v4::gc_run::GcRunInvocationV1;
use crate::engine::v4::index_producer_admission::{IndexProducerMaintenanceClassV1, IndexProducerMaintenanceTargetV1};
use crate::plugins::PluginManager;

/// Maximum completed tasks to keep after pruning.
const PRUNE_MAX_COUNT: usize = 100;
/// Maximum age (in milliseconds) of completed tasks before pruning.
const PRUNE_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000; // 24 hours
/// Number of files to process per batch during reindex.
const REINDEX_BATCH_SIZE: usize = 50;
/// Number of consecutive indexing failures before the circuit breaker trips.
const CIRCUIT_BREAKER_THRESHOLD: usize = 10;
/// Retain enough exact examples for diagnosis without allowing task records to
/// grow with the number or size of failed files.
const REINDEX_FAILURE_SAMPLE_LIMIT: usize = 8;
const REINDEX_FAILURE_PATH_LIMIT_BYTES: usize = 768;
const REINDEX_FAILURE_ERROR_LIMIT_BYTES: usize = 512;
/// Number of recent batch times to keep for ETA calculation.
const ROLLING_AVERAGE_WINDOW: usize = 10;
/// Scheduler bookkeeping retained while one task is claimed and dispatched.
/// Individual task implementations reserve their own material work separately.
const TASK_WORKER_ADMISSION_BYTES: u64 = 256 * 1024;
const TASK_DEFERRAL_BASE_DELAY_MS: i64 = 5_000;
const TASK_DEFERRAL_MAX_DELAY_MS: i64 = 5 * 60 * 1_000;
const REINDEX_RETAINED_PATH_OVERHEAD_BYTES: u64 = std::mem::size_of::<String>() as u64 + 32;

fn task_arguments(args: &serde_json::Value) -> EngineResult<&serde_json::Map<String, serde_json::Value>> {
  args.as_object().ok_or_else(|| EngineError::InvalidInput("task arguments must be a JSON object".to_string()))
}

fn required_task_string<'a>(args: &'a serde_json::Value, field: &str) -> EngineResult<&'a str> {
  let value = task_arguments(args)?.get(field).ok_or_else(|| EngineError::InvalidInput(format!("missing '{field}' argument")))?;
  value.as_str().ok_or_else(|| EngineError::InvalidInput(format!("task argument '{field}' must be a string")))
}

fn optional_task_string<'a>(args: &'a serde_json::Value, field: &str) -> EngineResult<Option<&'a str>> {
  match task_arguments(args)?.get(field) {
    None => Ok(None),
    Some(value) => value.as_str().map(Some).ok_or_else(|| EngineError::InvalidInput(format!("task argument '{field}' must be a string"))),
  }
}

fn optional_task_bool(args: &serde_json::Value, field: &str, default: bool) -> EngineResult<bool> {
  match task_arguments(args)?.get(field) {
    None => Ok(default),
    Some(value) => value.as_bool().ok_or_else(|| EngineError::InvalidInput(format!("task argument '{field}' must be a boolean"))),
  }
}

fn optional_task_u64(args: &serde_json::Value, field: &str, default: u64) -> EngineResult<u64> {
  match task_arguments(args)?.get(field) {
    None => Ok(default),
    Some(value) => value.as_u64().ok_or_else(|| EngineError::InvalidInput(format!("task argument '{field}' must be an unsigned integer"))),
  }
}

fn optional_task_usize(args: &serde_json::Value, field: &str, default: usize) -> EngineResult<usize> {
  let value = optional_task_u64(args, field, default as u64)?;
  usize::try_from(value)
    .map_err(|_| EngineError::InvalidInput(format!("task argument '{field}' does not fit this platform's address space")))
}

#[derive(Clone)]
struct ReindexFailureSample {
  phase: &'static str,
  path: String,
  error: String,
}

#[derive(Clone, Default)]
struct ReindexFailureSummary {
  count: usize,
  samples: Vec<ReindexFailureSample>,
}

fn preserve_reindex_partial_on_terminal<T>(
  result: EngineResult<T>,
  failures: &ReindexFailureSummary,
  completed: usize,
  phase: &'static str,
  path: &str,
) -> EngineResult<T> {
  result.map_err(|error| failures.clone().into_terminal_error(completed, phase, path, error))
}

impl ReindexFailureSummary {
  fn is_empty(&self) -> bool {
    self.count == 0
  }

  fn record(&mut self, phase: &'static str, path: &str, error: &EngineError) {
    self.count += 1;
    if self.samples.len() >= REINDEX_FAILURE_SAMPLE_LIMIT {
      return;
    }
    self.samples.push(ReindexFailureSample {
      phase,
      path: bounded_reindex_failure_text(path, REINDEX_FAILURE_PATH_LIMIT_BYTES),
      error: bounded_reindex_failure_text(&error.to_string(), REINDEX_FAILURE_ERROR_LIMIT_BYTES),
    });
  }

  fn into_error(self, completed: usize, circuit_breaker_tripped: bool) -> EngineError {
    let omitted = self.count - self.samples.len();
    let samples =
      self.samples.into_iter().map(|sample| format!("{} {}: {}", sample.phase, sample.path, sample.error)).collect::<Vec<_>>().join(" | ");
    let breaker = if circuit_breaker_tripped { "circuit breaker tripped; " } else { "" };
    EngineError::PartialOperation {
      operation: "reindex".to_string(),
      completed,
      failed: self.count,
      evidence: format!("{breaker}samples=[{samples}]; omitted={omitted}"),
    }
  }

  fn into_terminal_error(mut self, completed: usize, phase: &'static str, path: &str, error: EngineError) -> EngineError {
    if completed == 0 && self.is_empty() {
      return error;
    }
    self.record(phase, path, &error);
    self.into_error(completed, false)
  }
}

fn bounded_reindex_failure_text(value: &str, maximum_bytes: usize) -> String {
  if value.len() <= maximum_bytes {
    return value.to_string();
  }

  let mut boundary = maximum_bytes.saturating_sub(3);
  while boundary > 0 && !value.is_char_boundary(boundary) {
    boundary -= 1;
  }
  format!("{}...", &value[..boundary])
}

fn reindex_error_requires_immediate_failure(error: &EngineError) -> bool {
  matches!(
    error,
    EngineError::IoError(_)
      | EngineError::InvalidMagic
      | EngineError::InvalidHashAlgorithm(_)
      | EngineError::PartialOperation { .. }
      | EngineError::SystemFamilyPolicy { .. }
      | EngineError::DurabilityFailure(_)
      | EngineError::PostMutationDurabilityFailure(_)
      | EngineError::ShuttingDown
      | EngineError::Cancelled(_)
  )
}

struct RunningTaskRecovery<'a> {
  queue: &'a TaskQueue,
  task_id: &'a str,
  armed: bool,
}

impl RunningTaskRecovery<'_> {
  fn disarm(&mut self) {
    self.armed = false;
  }
}

impl Drop for RunningTaskRecovery<'_> {
  fn drop(&mut self) {
    if !self.armed {
      return;
    }
    match self.queue.requeue_running(self.task_id) {
      Ok(true) => tracing::error!(task_id = self.task_id, "task execution unwound unexpectedly; returned task to pending"),
      Ok(false) => {}
      Err(error) => tracing::error!(task_id = self.task_id, %error, "task execution unwound and could not restore pending state"),
    }
  }
}

/// Spawn a background task worker that dequeues and executes tasks in a loop.
///
/// Follows the heartbeat pattern: `tokio::spawn` + loop + sleep.
/// Accepts a [`CancellationToken`](tokio_util::sync::CancellationToken) for
/// graceful shutdown. Long-running task implementations observe the token at
/// their own safe cancellation boundaries before the worker exits.
///
/// Returns a JoinHandle that resolves when the task exits.
pub fn spawn_task_worker(
  queue: Arc<TaskQueue>,
  engine: Arc<StorageEngine>,
  plugin_manager: Arc<PluginManager>,
  event_bus: Arc<EventBus>,
  cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
  let capture_engine = Arc::clone(&engine);
  let runner = Arc::new(move |run_configuration, worker_cancel| {
    process_next_task_internal_with_cancel(
      &queue,
      &engine,
      &plugin_manager,
      &event_bus,
      &worker_cancel,
      Some(run_configuration),
      TaskExecutionHooks { post_dequeue: || {}, pre_execute: || {} },
    )
  });
  tokio::spawn(run_task_scheduler(
    cancel,
    move || capture_engine.capture_maintenance_run_configuration(),
    runner,
    TaskSchedulerTiming::PRODUCTION,
  ))
}

#[derive(Clone, Copy)]
struct TaskSchedulerTiming {
  processed_delay: Duration,
  idle_delay: Duration,
  failure_delay: Duration,
}

impl TaskSchedulerTiming {
  const PRODUCTION: Self =
    Self { processed_delay: Duration::from_secs(1), idle_delay: Duration::from_secs(2), failure_delay: Duration::from_secs(2) };

  #[cfg(test)]
  const TEST: Self =
    Self { processed_delay: Duration::from_millis(1), idle_delay: Duration::from_millis(2), failure_delay: Duration::from_millis(2) };
}

async fn run_task_scheduler<C, R>(
  cancel: tokio_util::sync::CancellationToken,
  capture_configuration: C,
  run_task: Arc<R>,
  timing: TaskSchedulerTiming,
) where
  C: Fn() -> EngineResult<MaintenanceRunConfiguration> + Send + Sync + 'static,
  R: Fn(MaintenanceRunConfiguration, tokio_util::sync::CancellationToken) -> EngineResult<bool> + Send + Sync + 'static,
{
  let mut workers = tokio::task::JoinSet::new();
  let mut next_dispatch = tokio::time::Instant::now();
  loop {
    while let Some(result) = workers.try_join_next() {
      next_dispatch = tokio::time::Instant::now() + task_worker_iteration_delay(result, timing);
    }

    if cancel.is_cancelled() && workers.is_empty() {
      tracing::info!("Task worker shutting down");
      break;
    }
    if cancel.is_cancelled() {
      if let Some(result) = workers.join_next().await {
        let _unused_delay = task_worker_iteration_delay(result, timing);
      }
      continue;
    }

    if tokio::time::Instant::now() >= next_dispatch {
      match capture_configuration() {
        Ok(run_configuration) => {
          let available_slots = run_configuration.max_concurrent_tasks.saturating_sub(workers.len());
          for _ in 0..available_slots {
            let run_task = Arc::clone(&run_task);
            let worker_cancel = cancel.clone();
            workers.spawn_blocking(move || run_task(run_configuration, worker_cancel));
          }
          next_dispatch = tokio::time::Instant::now() + timing.processed_delay;
        }
        Err(error) => {
          tracing::error!(%error, "Task worker cannot capture maintenance policy");
          next_dispatch = tokio::time::Instant::now() + timing.failure_delay;
        }
      }
    }

    tokio::select! {
      _ = cancel.cancelled() => {}
      result = workers.join_next(), if !workers.is_empty() => {
        if let Some(result) = result {
          next_dispatch = tokio::time::Instant::now() + task_worker_iteration_delay(result, timing);
        }
      }
      _ = tokio::time::sleep_until(next_dispatch) => {}
    }
  }
}

fn task_worker_iteration_delay(result: Result<EngineResult<bool>, tokio::task::JoinError>, timing: TaskSchedulerTiming) -> Duration {
  match result {
    Ok(Ok(true)) => timing.processed_delay,
    Ok(Ok(false)) => timing.idle_delay,
    Ok(Err(error)) => {
      tracing::error!(%error, "Task worker iteration failed");
      timing.failure_delay
    }
    Err(error) => {
      tracing::error!(%error, "Task worker blocking execution panicked or was cancelled");
      timing.failure_delay
    }
  }
}

#[cfg(test)]
#[path = "../../spec/engine/task_worker_scheduler_internal_spec.rs"]
mod scheduler_internal_spec;

/// Process the next pending task from the queue. Returns true if a task was processed.
///
/// This is one iteration of the worker loop -- dequeue, execute, update status.
/// Designed for direct use in tests without spawning the infinite loop.
pub fn process_next_task(
  queue: &TaskQueue,
  engine: &StorageEngine,
  plugin_manager: &PluginManager,
  event_bus: &EventBus,
) -> EngineResult<bool> {
  // Tests + sync callers without a cancel token get a dummy "never-cancelled"
  // token. Production goes through process_next_task_internal_with_cancel.
  let dummy_cancel = tokio_util::sync::CancellationToken::new();
  process_next_task_internal_with_cancel(
    queue,
    engine,
    plugin_manager,
    event_bus,
    &dummy_cancel,
    None,
    TaskExecutionHooks { post_dequeue: || {}, pre_execute: || {} },
  )
}

/// Deterministic scheduler-boundary hook used by integration tests to prove
/// pressure and cancellation races immediately after a task is claimed.
#[doc(hidden)]
pub fn process_next_task_with_post_dequeue_hook<F>(
  queue: &TaskQueue,
  engine: &StorageEngine,
  plugin_manager: &PluginManager,
  event_bus: &EventBus,
  post_dequeue_hook: F,
) -> EngineResult<bool>
where
  F: FnOnce(),
{
  let dummy_cancel = tokio_util::sync::CancellationToken::new();
  process_next_task_internal_with_cancel(
    queue,
    engine,
    plugin_manager,
    event_bus,
    &dummy_cancel,
    None,
    TaskExecutionHooks { post_dequeue: post_dequeue_hook, pre_execute: || {} },
  )
}

/// Deterministic execution-boundary hook used by integration tests to prove
/// cancellation after `tasks_started` reaches the real task implementation.
#[doc(hidden)]
pub fn process_next_task_with_pre_execute_hook<F>(
  queue: &TaskQueue,
  engine: &StorageEngine,
  plugin_manager: &PluginManager,
  event_bus: &EventBus,
  pre_execute_hook: F,
) -> EngineResult<bool>
where
  F: FnOnce(),
{
  let dummy_cancel = tokio_util::sync::CancellationToken::new();
  process_next_task_internal_with_cancel(
    queue,
    engine,
    plugin_manager,
    event_bus,
    &dummy_cancel,
    None,
    TaskExecutionHooks { post_dequeue: || {}, pre_execute: pre_execute_hook },
  )
}

/// Deterministic production-cancellation hook used by integration tests to
/// prove worker shutdown reaches an already-started task implementation.
#[doc(hidden)]
pub fn process_next_task_with_cancel_and_pre_execute_hook<F>(
  queue: &TaskQueue,
  engine: &StorageEngine,
  plugin_manager: &PluginManager,
  event_bus: &EventBus,
  cancel: &tokio_util::sync::CancellationToken,
  pre_execute_hook: F,
) -> EngineResult<bool>
where
  F: FnOnce(),
{
  process_next_task_internal_with_cancel(
    queue,
    engine,
    plugin_manager,
    event_bus,
    cancel,
    None,
    TaskExecutionHooks { post_dequeue: || {}, pre_execute: pre_execute_hook },
  )
}

struct TaskExecutionHooks<F, G> {
  post_dequeue: F,
  pre_execute: G,
}

fn process_next_task_internal_with_cancel<F, G>(
  queue: &TaskQueue,
  engine: &StorageEngine,
  plugin_manager: &PluginManager,
  event_bus: &EventBus,
  cancel: &tokio_util::sync::CancellationToken,
  captured_run_configuration: Option<MaintenanceRunConfiguration>,
  hooks: TaskExecutionHooks<F, G>,
) -> EngineResult<bool>
where
  F: FnOnce(),
  G: FnOnce(),
{
  let TaskExecutionHooks { post_dequeue: post_dequeue_hook, pre_execute: pre_execute_hook } = hooks;
  if cancel.is_cancelled() {
    return Ok(false);
  }
  let run_configuration = match captured_run_configuration {
    Some(configuration) => configuration,
    None => engine.capture_maintenance_run_configuration()?,
  };

  let mut task_workspace = match OperationMemoryBudget::new(
    engine,
    "task worker",
    MemoryOwner::Task,
    AdmissionClass::Maintenance,
    TASK_WORKER_ADMISSION_BYTES,
    Some(cancel),
  ) {
    Ok(reservation) => reservation,
    Err(error @ EngineError::ResourceExhausted(_)) => {
      tracing::info!(error = %error, "task worker deferred pending maintenance before dequeue");
      return Ok(false);
    }
    Err(EngineError::Cancelled(_)) => return Ok(false),
    Err(error) => return Err(error),
  };

  if cancel.is_cancelled() {
    return Ok(false);
  }

  // H18: dequeue_next atomically finds the oldest pending task and marks
  // it Running under a lock, preventing double-dequeue.
  let task = match queue.dequeue_next_with_memory(&mut task_workspace) {
    Ok(Some(task)) => task,
    Ok(None) => return Ok(false),
    Err(error @ EngineError::ResourceExhausted(_)) => {
      tracing::info!(error = %error, "task worker deferred pending maintenance during bounded dequeue");
      return Ok(false);
    }
    Err(error) => return Err(error),
  };
  let mut running_recovery = RunningTaskRecovery { queue, task_id: &task.id, armed: true };

  let task_cancellation = queue.register_active_cancellation(&task.id, cancel)?;
  post_dequeue_hook();

  if queue.is_cancelled(&task.id) {
    finish_cancelled_task(queue, &task, event_bus)?;
    running_recovery.disarm();
    return finish_task_iteration(queue, &task.id);
  }

  if task_cancellation.token().is_cancelled() {
    requeue_running_task(queue, &task, event_bus, "task worker is shutting down")?;
    running_recovery.disarm();
    return finish_task_iteration(queue, &task.id);
  }

  if let Err(error) = engine.memory_coordinator().check_admission(MemoryOwner::Task, AdmissionClass::Maintenance) {
    match error {
      MemoryCoordinatorError::SoftPressureDeferred { .. } | MemoryCoordinatorError::HardLimitExceeded { .. } => {
        defer_running_task_with_backoff(queue, &task, event_bus, &error.to_string())?;
        running_recovery.disarm();
        return finish_task_iteration(queue, &task.id);
      }
      error => return Err(task_worker_memory_error(error)),
    }
  }

  // Emit task started event.
  let started_event = EngineEvent::new(
    EVENT_TASKS_STARTED,
    "system",
    serde_json::json!({
        "task_id": task.id,
        "task_type": task.task_type,
        "args": task.args,
        "configuration_generation": run_configuration.generation,
    }),
  );
  event_bus.emit(started_event);
  pre_execute_hook();

  // Execute based on task type.
  let result = match task.task_type.as_str() {
    "reindex" => execute_reindex(queue, &task, engine, plugin_manager, task_cancellation.token()),
    "gc" => execute_gc(queue, &task, engine, event_bus, task_cancellation.token()),
    "backup" => execute_backup(&task, engine, task_cancellation.token()),
    "cleanup" => execute_cleanup(&task, engine, event_bus, task_cancellation.token()),
    unknown => Err(EngineError::InvalidInput(format!("unknown task type: {unknown}"))),
  };

  match result {
    Ok(summary) => {
      if queue.finish_running(&task.id, TaskStatus::Completed, None)? {
        event_bus.emit(EngineEvent::new(
          EVENT_TASKS_COMPLETED,
          "system",
          serde_json::json!({
              "task_id": task.id,
              "task_type": task.task_type,
              "summary": summary,
          }),
        ));
      } else if queue.is_cancelled(&task.id) {
        emit_cancelled_event(&task, event_bus);
      }
    }
    Err(EngineError::Cancelled(_)) if queue.is_cancelled(&task.id) => {
      finish_cancelled_task(queue, &task, event_bus)?;
    }
    Err(error @ EngineError::ResourceExhausted(_)) => {
      defer_running_task_with_backoff(queue, &task, event_bus, &error.to_string())?;
    }
    Err(error @ (EngineError::ShuttingDown | EngineError::Cancelled(_))) => {
      requeue_running_task(queue, &task, event_bus, &error.to_string())?;
    }
    Err(error) => {
      let error_message = error.to_string();
      if queue.finish_running(&task.id, TaskStatus::Failed, Some(error_message.clone()))? {
        event_bus.emit(EngineEvent::new(
          EVENT_TASKS_FAILED,
          "system",
          serde_json::json!({
              "task_id": task.id,
              "task_type": task.task_type,
              "error": error_message,
          }),
        ));
      } else if queue.is_cancelled(&task.id) {
        emit_cancelled_event(&task, event_bus);
      }
    }
  }

  running_recovery.disarm();
  finish_task_iteration(queue, &task.id)
}

fn finish_task_iteration(queue: &TaskQueue, task_id: &str) -> EngineResult<bool> {
  queue.clear_progress(task_id);
  match queue.prune_completed(PRUNE_MAX_AGE_MS, PRUNE_MAX_COUNT) {
    Ok(_) => {}
    Err(EngineError::ResourceExhausted(reason)) => {
      tracing::info!(%reason, "task history pruning deferred under memory pressure");
    }
    Err(error) => return Err(error),
  }
  Ok(true)
}

fn task_deferral_delay_ms(deferral_count: u32) -> i64 {
  let exponent = deferral_count.min(6);
  TASK_DEFERRAL_BASE_DELAY_MS.saturating_mul(1i64 << exponent).min(TASK_DEFERRAL_MAX_DELAY_MS)
}

fn defer_running_task_with_backoff(queue: &TaskQueue, task: &TaskRecord, event_bus: &EventBus, reason: &str) -> EngineResult<()> {
  let retry_after_ms = task_deferral_delay_ms(task.deferral_count);
  let retry_at = chrono::Utc::now().timestamp_millis().saturating_add(retry_after_ms);
  if let Some(deferred) = queue.defer_running_until(&task.id, retry_at)? {
    event_bus.emit(EngineEvent::new(
      EVENT_TASKS_DEFERRED,
      "system",
      serde_json::json!({
          "task_id": task.id,
          "task_type": task.task_type,
          "reason": reason,
          "retryable": true,
          "retry_at": retry_at,
          "retry_after_ms": retry_after_ms,
          "deferral_count": deferred.deferral_count,
      }),
    ));
  } else if queue.is_cancelled(&task.id) {
    emit_cancelled_event(task, event_bus);
  }
  Ok(())
}

fn requeue_running_task(queue: &TaskQueue, task: &TaskRecord, event_bus: &EventBus, reason: &str) -> EngineResult<()> {
  if queue.requeue_running(&task.id)? {
    event_bus.emit(EngineEvent::new(
      EVENT_TASKS_DEFERRED,
      "system",
      serde_json::json!({
          "task_id": task.id,
          "task_type": task.task_type,
          "reason": reason,
          "retryable": true,
          "retry_at": null,
          "retry_after_ms": 0,
          "deferral_count": task.deferral_count,
      }),
    ));
  } else if queue.is_cancelled(&task.id) {
    emit_cancelled_event(task, event_bus);
  }
  Ok(())
}

fn finish_cancelled_task(queue: &TaskQueue, task: &TaskRecord, event_bus: &EventBus) -> EngineResult<()> {
  let transitioned = queue.finish_running(&task.id, TaskStatus::Cancelled, None)?;
  if transitioned || queue.is_cancelled(&task.id) {
    emit_cancelled_event(task, event_bus);
  }
  Ok(())
}

fn emit_cancelled_event(task: &TaskRecord, event_bus: &EventBus) {
  event_bus.emit(EngineEvent::new(
    EVENT_TASKS_CANCELLED,
    "system",
    serde_json::json!({
        "task_id": task.id,
        "task_type": task.task_type,
    }),
  ));
}

fn task_worker_memory_error(error: MemoryCoordinatorError) -> EngineError {
  match error {
    MemoryCoordinatorError::PolicyUnavailable
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. } => {
      EngineError::ResourceExhausted(format!("task worker memory admission failed: {error}"))
    }
    _ => EngineError::IoError(std::io::Error::other(format!("task worker memory admission failed: {error}"))),
  }
}

fn require_forced_migration_memory(migration_memory: &mut Option<OperationMemoryBudget>) -> EngineResult<&mut OperationMemoryBudget> {
  migration_memory
    .as_mut()
    .ok_or_else(|| EngineError::DurabilityFailure("forced reindex migration memory authority is unavailable".to_string()))
}

fn parse_reindex_source_operation_id(task_id: &str) -> EngineResult<[u8; 16]> {
  uuid::Uuid::parse_str(task_id)
    .map(uuid::Uuid::into_bytes)
    .map_err(|error| EngineError::InvalidInput(format!("reindex task ID is not a UUID: {error}")))
}

fn completed_reindex_maintenance_targets<'a>(
  reindex_root: &'a str,
  config_scope: Option<&'a str>,
  force: bool,
) -> EngineResult<Vec<IndexProducerMaintenanceTargetV1<'a>>> {
  let mut targets = Vec::new();
  targets
    .try_reserve_exact(3)
    .map_err(|error| EngineError::ResourceExhausted(format!("reindex maintenance target allocation failed: {error}")))?;
  if let Some(scope) = config_scope {
    targets.push(IndexProducerMaintenanceTargetV1 { class: IndexProducerMaintenanceClassV1::ConfigurationRetirement, scope });
  }
  if force {
    targets.push(IndexProducerMaintenanceTargetV1 { class: IndexProducerMaintenanceClassV1::LegacyMigration, scope: reindex_root });
  }
  targets.push(IndexProducerMaintenanceTargetV1 { class: IndexProducerMaintenanceClassV1::Reindex, scope: reindex_root });
  Ok(targets)
}

fn admit_completed_reindex_maintenance(
  engine: &StorageEngine,
  task_id: &str,
  reindex_root: &str,
  config_scope: Option<&str>,
  force: bool,
) -> EngineResult<()> {
  let source_operation_id = parse_reindex_source_operation_id(task_id)?;
  let targets = completed_reindex_maintenance_targets(reindex_root, config_scope, force)?;
  engine
    .admit_index_maintenance_tasks_v1(source_operation_id, &targets)
    .map(|_| ())
    .map_err(|error| EngineError::DurabilityFailure(format!("completed reindex v4 maintenance admission failed: {error}")))
}

/// Execute a reindex task: re-run the indexing pipeline on all files under a directory.
fn execute_reindex(
  queue: &TaskQueue,
  task: &TaskRecord,
  engine: &StorageEngine,
  plugin_manager: &PluginManager,
  cancel: &tokio_util::sync::CancellationToken,
) -> EngineResult<String> {
  let path = required_task_string(&task.args, "path")?;
  if queue.is_cancelled(&task.id) || cancel.is_cancelled() {
    return Err(EngineError::Cancelled("reindex".to_string()));
  }
  engine.memory_coordinator().check_admission(MemoryOwner::Task, AdmissionClass::Maintenance).map_err(task_worker_memory_error)?;
  let force = optional_task_bool(&task.args, "force", false)?;
  let metadata_only = optional_task_bool(&task.args, "metadata_only", false)?;
  let index_flush_options = reindex_index_buffer_options(&task.args)?;
  let mut task_memory =
    OperationMemoryBudget::new(engine, "reindex task", MemoryOwner::Task, AdmissionClass::Maintenance, 0, Some(cancel))?;
  let mut migration_memory = if force {
    Some(OperationMemoryBudget::new(
      engine,
      "forced reindex migration",
      MemoryOwner::Migration,
      AdmissionClass::Maintenance,
      0,
      Some(cancel),
    )?)
  } else {
    None
  };

  let ops = DirectoryOps::new(engine);
  let resolver = IndexConfigResolver::new(engine);
  let reindex_root = normalize_path(path);

  // Resolve the config owner for this scope. A scoped manual reindex may be
  // governed by an ancestor glob config, just like ordinary file indexing.
  // Forced reindex doubles as the schema-migration path, so it must also be
  // able to run without an indexes.json and simply migrate every file in the
  // subtree.
  let config_path = IndexConfigResolver::config_path_for_directory(&reindex_root);
  let resolved_config = match resolver.find_config_for_reindex_scope(&reindex_root) {
    Ok(Some((config, config_dir))) => Some((config, config_dir)),
    Ok(None) if force => {
      tracing::info!(
        path = %reindex_root,
        config_path = %config_path,
        "forced reindex running migration-only because no index config was found"
      );
      None
    }
    Ok(None) => return Err(EngineError::NotFound(config_path)),
    Err(error) => return Err(error),
  };
  let stale_indexes_deleted = if let Some((config, config_dir)) = resolved_config.as_ref() {
    let deleted = IndexManager::new(engine).delete_indexes_not_in_config_unrouted(config_dir, config)?;
    if deleted > 0 {
      tracing::info!(path = %config_dir, requested_scope = %reindex_root, deleted, "retired stale indexes before reindex");
    }
    deleted
  } else {
    0
  };

  // Build a sorted list of full file paths to reindex.
  let prefix = reindex_root.trim_end_matches('/');
  let mut file_paths: Vec<String> = if force {
    collect_current_file_record_paths(engine, &reindex_root, require_forced_migration_memory(&mut migration_memory)?)?
  } else if let Some((config, config_dir)) = resolved_config.as_ref() {
    if let Some(ref glob_pattern) = config.glob {
      collect_recursive_reindex_paths(engine, &reindex_root, config_dir, glob_pattern, &mut task_memory)?
    } else {
      collect_direct_reindex_paths(&ops, &reindex_root, prefix, &mut task_memory)?
    }
  } else {
    Vec::new()
  };
  file_paths.sort_unstable();

  // If there's a checkpoint, skip paths at or before it.
  if let Some(ref checkpoint) = task.checkpoint {
    file_paths.retain(|p| p.as_str() > checkpoint.as_str());
  }

  let total_count = file_paths.len();
  if total_count == 0 {
    admit_completed_reindex_maintenance(
      engine,
      &task.id,
      &reindex_root,
      resolved_config.as_ref().map(|(_, config_dir)| config_dir.as_str()),
      force,
    )?;
    return Ok("reindexed 0 files".to_string());
  }

  let pipeline = IndexingPipeline::with_plugin_manager(engine, plugin_manager);
  let mut index_buffer = IndexWriteBuffer::new(engine, index_flush_options);

  let mut indexed_count: usize = 0;
  let mut completed_count: usize = 0;
  let mut migrated_count: usize = 0;
  let mut consecutive_failures: usize = 0;
  let mut failures = ReindexFailureSummary::default();
  let mut batch_times: Vec<Duration> = Vec::new();
  let mut checkpoint_path: Option<&str> = None;
  let start = Instant::now();

  macro_rules! flush_before_reindex_exit {
    ($phase:expr, $path:expr) => {
      preserve_reindex_partial_on_terminal(
        flush_reindex_before_retry(queue, task, &mut index_buffer, checkpoint_path),
        &failures,
        completed_count,
        $phase,
        $path,
      )?
    };
  }

  // Process in batches.
  for batch in file_paths.chunks(REINDEX_BATCH_SIZE) {
    let batch_start = Instant::now();

    for file_path in batch {
      if queue.is_cancelled(&task.id) || cancel.is_cancelled() {
        flush_before_reindex_exit!("flush-before-cancel", file_path);
        return Err(EngineError::Cancelled("reindex".to_string()));
      }
      if let Err(error) = engine.memory_coordinator().check_admission(MemoryOwner::Task, AdmissionClass::Maintenance) {
        flush_before_reindex_exit!("flush-before-pressure", file_path);
        return Err(task_worker_memory_error(error));
      }

      let indexable = match pipeline.path_is_indexable(file_path) {
        Ok(indexable) => indexable,
        Err(error) => {
          flush_before_reindex_exit!("flush-before-policy-failure", file_path);
          return Err(failures.into_terminal_error(completed_count, "policy", file_path, error));
        }
      };

      if force {
        match ops.migrate_file_record_to_current_version_with_memory(file_path, require_forced_migration_memory(&mut migration_memory)?) {
          Ok(true) => {
            migrated_count += 1;
          }
          Ok(false) => {}
          Err(error @ (EngineError::ResourceExhausted(_) | EngineError::Cancelled(_) | EngineError::ShuttingDown)) => {
            flush_before_reindex_exit!("flush-before-migration-deferral", file_path);
            return Err(error);
          }
          Err(error) if reindex_error_requires_immediate_failure(&error) => {
            flush_before_reindex_exit!("flush-before-migration-failure", file_path);
            return Err(failures.into_terminal_error(completed_count, "migration-authority", file_path, error));
          }
          Err(error) => {
            tracing::warn!(
              path = %file_path,
              error = %error,
              "forced reindex could not migrate FileRecord"
            );
            failures.record("migration", file_path, &error);
            consecutive_failures += 1;
            indexed_count += 1;
            if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
              flush_before_reindex_exit!("flush-before-circuit-breaker", file_path);
              return Err(failures.into_error(completed_count, true));
            }
            continue;
          }
        }

        if resolved_config.is_none() || !indexable {
          consecutive_failures = 0;
          indexed_count += 1;
          completed_count += 1;
          if failures.is_empty() {
            checkpoint_path = Some(file_path);
          }
          continue;
        }
      }

      if !indexable {
        consecutive_failures = 0;
        indexed_count += 1;
        completed_count += 1;
        if failures.is_empty() {
          checkpoint_path = Some(file_path);
        }
        continue;
      }

      let index_result = if metadata_only {
        pipeline.run_metadata_only_buffered_unrouted_with_outcome(file_path, &mut index_buffer).map(|_| ())
      } else {
        let metadata = match ops.get_metadata(file_path) {
          Ok(metadata) => metadata,
          Err(error @ (EngineError::ResourceExhausted(_) | EngineError::Cancelled(_) | EngineError::ShuttingDown)) => {
            flush_before_reindex_exit!("flush-before-metadata-deferral", file_path);
            return Err(error);
          }
          Err(error) if reindex_error_requires_immediate_failure(&error) => {
            flush_before_reindex_exit!("flush-before-metadata-failure", file_path);
            return Err(failures.into_terminal_error(completed_count, "metadata-authority", file_path, error));
          }
          Err(error) => {
            tracing::warn!(path = %file_path, %error, "reindex could not read file metadata");
            failures.record("metadata", file_path, &error);
            consecutive_failures += 1;
            indexed_count += 1;
            if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
              flush_before_reindex_exit!("flush-before-circuit-breaker", file_path);
              return Err(failures.into_error(completed_count, true));
            }
            continue;
          }
        };
        let content_type = metadata.as_ref().and_then(|record| record.content_type.clone());
        let buffered_body_checkpoint = task_memory.checkpoint();
        let buffered_body_bytes = metadata
          .as_ref()
          .map(|record| {
            record
              .total_size
              .checked_add(std::mem::size_of::<Vec<u8>>() as u64 + 64)
              .ok_or_else(|| EngineError::ResourceExhausted("reindex buffered body estimate overflow".to_string()))
          })
          .transpose()?
          .unwrap_or(0);
        drop(metadata);
        task_memory.reserve(buffered_body_bytes, "reindex buffered file body admission failed")?;
        // Read file content only for full parser/content reindexing. Metadata-only
        // reindexing reads the FileRecord header through the pipeline instead.
        let data = match ops.read_file_buffered(file_path) {
          Ok(data) => data,
          Err(error @ (EngineError::ResourceExhausted(_) | EngineError::Cancelled(_) | EngineError::ShuttingDown)) => {
            task_memory.release_to(buffered_body_checkpoint, "reindex buffered file body release after read refusal")?;
            flush_before_reindex_exit!("flush-before-body-deferral", file_path);
            return Err(error);
          }
          Err(error) if reindex_error_requires_immediate_failure(&error) => {
            task_memory.release_to(buffered_body_checkpoint, "reindex buffered file body release after immediate failure")?;
            flush_before_reindex_exit!("flush-before-body-failure", file_path);
            return Err(failures.into_terminal_error(completed_count, "body-authority", file_path, error));
          }
          Err(error) => {
            task_memory.release_to(buffered_body_checkpoint, "reindex buffered file body release after read failure")?;
            tracing::warn!(path = %file_path, %error, "reindex could not read file body");
            failures.record("body", file_path, &error);
            consecutive_failures += 1;
            indexed_count += 1;
            if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
              flush_before_reindex_exit!("flush-before-circuit-breaker", file_path);
              return Err(failures.into_error(completed_count, true));
            }
            continue;
          }
        };

        let result = pipeline
          .run_buffered_unrouted_with_outcome(file_path, &data, content_type.as_deref(), &mut index_buffer, Some(cancel))
          .map(|_| ());
        drop(data);
        task_memory.release_to(buffered_body_checkpoint, "reindex buffered file body release after parsing")?;
        result
      };

      let indexed_successfully = match index_result {
        Ok(()) => {
          consecutive_failures = 0;
          true
        }
        Err(error) if matches!(error, EngineError::ResourceExhausted(_)) || reindex_error_requires_immediate_failure(&error) => {
          flush_before_reindex_exit!("flush-before-index-failure", file_path);
          if matches!(error, EngineError::ResourceExhausted(_) | EngineError::Cancelled(_) | EngineError::ShuttingDown) {
            return Err(error);
          }
          return Err(failures.into_terminal_error(completed_count, "index-authority", file_path, error));
        }
        Err(error) => {
          tracing::warn!(path = %file_path, %error, "reindex pipeline could not index file");
          failures.record("index", file_path, &error);
          consecutive_failures += 1;
          if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
            flush_before_reindex_exit!("flush-before-circuit-breaker", file_path);
            return Err(failures.into_error(completed_count, true));
          }
          false
        }
      };

      indexed_count += 1;
      if indexed_successfully {
        completed_count += 1;
        if failures.is_empty() {
          checkpoint_path = Some(file_path);
        }
      }

      match index_buffer.flush_if_due() {
        Ok(true) => {
          if let Some(path) = checkpoint_path {
            preserve_reindex_partial_on_terminal(queue.update_checkpoint(&task.id, path), &failures, completed_count, "checkpoint", path)?;
          }
        }
        Ok(false) => {}
        Err(error) => {
          return Err(failures.clone().into_terminal_error(completed_count, "index-flush", file_path, error));
        }
      }
    }

    let batch_duration = batch_start.elapsed();
    batch_times.push(batch_duration);
    if batch_times.len() > ROLLING_AVERAGE_WINDOW {
      batch_times.remove(0);
    }

    // Only advance the checkpoint past buffered index mutations after they have
    // been flushed. If there are no pending index mutations, all completed work
    // is durable and the batch checkpoint is safe.
    let pending_stats =
      preserve_reindex_partial_on_terminal(index_buffer.stats(), &failures, completed_count, "index-stats", &reindex_root)?;
    if pending_stats.pending_mutations == 0 {
      if let Some(path) = checkpoint_path {
        preserve_reindex_partial_on_terminal(queue.update_checkpoint(&task.id, path), &failures, completed_count, "checkpoint", path)?;
      }
    }

    // Compute progress and ETA.
    let progress = indexed_count as f64 / total_count as f64;
    let eta_ms = compute_eta(&batch_times, total_count, indexed_count);

    let index_stats = preserve_reindex_partial_on_terminal(index_buffer.stats(), &failures, completed_count, "index-stats", &reindex_root)?;
    queue.set_progress(
      &task.id,
      ProgressInfo {
        task_id: task.id.clone(),
        task_type: task.task_type.clone(),
        args: task.args.clone(),
        progress,
        eta_ms,
        indexed_count,
        total_count,
        stale_since: None,
        message: Some(format!(
          "processed {}/{} files, completed {}, failed {}, migrated {}, metadata_only={}, index_mutations={}, pending_index_mutations={}, index_flushes={}, cached_indexes={}",
          indexed_count,
          total_count,
          completed_count,
          failures.count,
          migrated_count,
          metadata_only,
          index_stats.mutations,
          index_stats.pending_mutations,
          index_stats.flushes,
          index_stats.cached_indexes
        )),
      },
    );

    // Check for per-task or shutdown cancellation after each batch. The
    // outer cancel covers graceful shutdown — without polling it here the
    // worker can't exit during a long reindex.
    if queue.is_cancelled(&task.id) || cancel.is_cancelled() {
      flush_before_reindex_exit!("flush-before-cancel", &reindex_root);
      return Err(EngineError::Cancelled("reindex".to_string()));
    }
  }

  let flushed_indexes =
    preserve_reindex_partial_on_terminal(index_buffer.flush_all(), &failures, completed_count, "final-index-flush", &reindex_root)?;
  if let Some(path) = checkpoint_path {
    preserve_reindex_partial_on_terminal(queue.update_checkpoint(&task.id, path), &failures, completed_count, "final-checkpoint", path)?;
  }
  if !failures.is_empty() {
    return Err(failures.into_error(completed_count, false));
  }

  admit_completed_reindex_maintenance(
    engine,
    &task.id,
    &reindex_root,
    resolved_config.as_ref().map(|(_, config_dir)| config_dir.as_str()),
    force,
  )?;

  let elapsed_ms = start.elapsed().as_millis();
  let index_stats =
    preserve_reindex_partial_on_terminal(index_buffer.stats(), &failures, completed_count, "final-index-stats", &reindex_root)?;
  let index_summary = format!(
    ", metadata_only={}, stale_indexes_deleted={}, index_mutations={}, index_flushes={}, flushed_indexes={} (+{} final), cached_indexes={}",
    metadata_only,
    stale_indexes_deleted,
    index_stats.mutations,
    index_stats.flushes,
    index_stats.flushed_indexes,
    flushed_indexes,
    index_stats.cached_indexes
  );
  if force {
    Ok(format!("reindexed {} files, migrated {} records in {}ms{}", indexed_count, migrated_count, elapsed_ms, index_summary))
  } else {
    Ok(format!("reindexed {} files in {}ms{}", indexed_count, elapsed_ms, index_summary))
  }
}

fn reindex_path_retained_bytes(path: &str) -> EngineResult<u64> {
  reindex_path_retained_bytes_for_length(path.len())
}

fn reindex_path_retained_bytes_for_length(path_length: usize) -> EngineResult<u64> {
  u64::try_from(path_length)
    .ok()
    .and_then(|bytes| bytes.checked_add(REINDEX_RETAINED_PATH_OVERHEAD_BYTES))
    .ok_or_else(|| EngineError::ResourceExhausted("reindex retained path estimate overflow".to_string()))
}

fn collect_recursive_reindex_paths(
  engine: &StorageEngine,
  reindex_root: &str,
  prefix: &str,
  glob_pattern: &str,
  memory: &mut OperationMemoryBudget,
) -> EngineResult<Vec<String>> {
  let mut path_count = 0usize;
  let mut retained_bytes = 0u64;
  crate::engine::directory_listing::visit_directory_recursive_strict(engine, reindex_root, -1, None, None, |entry| {
    memory.record_work(1)?;
    if !reindex_listing_entry_matches(&entry, prefix, glob_pattern) {
      return Ok(true);
    }
    path_count = path_count.checked_add(1).ok_or_else(|| EngineError::ResourceExhausted("reindex path count overflow".to_string()))?;
    retained_bytes = retained_bytes
      .checked_add(reindex_path_retained_bytes(&entry.path)?)
      .ok_or_else(|| EngineError::ResourceExhausted("reindex path inventory estimate overflow".to_string()))?;
    Ok(true)
  })?;

  memory.reserve(retained_bytes, "reindex retained path inventory admission failed")?;
  let mut paths = Vec::new();
  paths
    .try_reserve_exact(path_count)
    .map_err(|error| EngineError::ResourceExhausted(format!("reindex path inventory allocation failed: {error}")))?;
  let mut populated_bytes = 0u64;
  crate::engine::directory_listing::visit_directory_recursive_strict(engine, reindex_root, -1, None, None, |entry| {
    memory.record_work(1)?;
    if !reindex_listing_entry_matches(&entry, prefix, glob_pattern) {
      return Ok(true);
    }
    let entry_bytes = reindex_path_retained_bytes(&entry.path)?;
    populated_bytes = populated_bytes
      .checked_add(entry_bytes)
      .ok_or_else(|| EngineError::ResourceExhausted("reindex populated path estimate overflow".to_string()))?;
    if paths.len() >= path_count || populated_bytes > retained_bytes {
      return Err(EngineError::ResourceExhausted(
        "reindex namespace grew while its bounded path inventory was being populated; retrying from the durable checkpoint".to_string(),
      ));
    }
    paths.push(entry.path);
    Ok(true)
  })?;
  Ok(paths)
}

fn reindex_listing_entry_matches(entry: &crate::engine::directory_listing::ListingEntry, prefix: &str, glob_pattern: &str) -> bool {
  if entry.entry_type != EntryType::FileRecord.to_u8() {
    return false;
  }
  let relative = entry.path.trim_start_matches(prefix).trim_start_matches('/');
  glob_matches(glob_pattern, relative)
}

fn collect_direct_reindex_paths(
  ops: &DirectoryOps<'_>,
  reindex_root: &str,
  prefix: &str,
  memory: &mut OperationMemoryBudget,
) -> EngineResult<Vec<String>> {
  collect_direct_reindex_paths_with_between_pass_hook(ops, reindex_root, prefix, memory, || {})
}

fn collect_direct_reindex_paths_with_between_pass_hook<F>(
  ops: &DirectoryOps<'_>,
  reindex_root: &str,
  prefix: &str,
  memory: &mut OperationMemoryBudget,
  between_passes: F,
) -> EngineResult<Vec<String>>
where
  F: FnOnce(),
{
  let mut retained_bytes = 0u64;
  let mut path_count = 0usize;
  ops.visit_live_directory_children_strict(reindex_root, |entry| {
    memory.record_work(1)?;
    if !direct_reindex_child_matches(entry) {
      return Ok(true);
    }
    let path_length = prefix
      .len()
      .checked_add(1)
      .and_then(|length| length.checked_add(entry.name.len()))
      .ok_or_else(|| EngineError::ResourceExhausted("direct reindex path length overflow".to_string()))?;
    retained_bytes = retained_bytes
      .checked_add(reindex_path_retained_bytes_for_length(path_length)?)
      .ok_or_else(|| EngineError::ResourceExhausted("direct reindex path inventory estimate overflow".to_string()))?;
    path_count =
      path_count.checked_add(1).ok_or_else(|| EngineError::ResourceExhausted("direct reindex path count overflow".to_string()))?;
    Ok(true)
  })?;
  memory.reserve(retained_bytes, "direct reindex retained path admission failed")?;
  between_passes();
  let mut paths = Vec::new();
  paths
    .try_reserve_exact(path_count)
    .map_err(|error| EngineError::ResourceExhausted(format!("direct reindex path inventory allocation failed: {error}")))?;
  let mut populated_bytes = 0u64;
  ops.visit_live_directory_children_strict(reindex_root, |entry| {
    memory.record_work(1)?;
    if !direct_reindex_child_matches(entry) {
      return Ok(true);
    }
    let path_length = prefix
      .len()
      .checked_add(1)
      .and_then(|length| length.checked_add(entry.name.len()))
      .ok_or_else(|| EngineError::ResourceExhausted("direct reindex path length overflow".to_string()))?;
    populated_bytes = populated_bytes
      .checked_add(reindex_path_retained_bytes_for_length(path_length)?)
      .ok_or_else(|| EngineError::ResourceExhausted("direct reindex populated path estimate overflow".to_string()))?;
    if paths.len() >= path_count || populated_bytes > retained_bytes {
      return Err(EngineError::ResourceExhausted(
        "direct reindex namespace grew while its bounded path inventory was being populated; retrying from the durable checkpoint"
          .to_string(),
      ));
    }
    paths.push(format!("{prefix}/{}", entry.name));
    Ok(true)
  })?;
  Ok(paths)
}

fn direct_reindex_child_matches(entry: &crate::engine::directory_entry::ChildEntry) -> bool {
  entry.entry_type == EntryType::FileRecord.to_u8()
}

fn flush_reindex_before_retry(
  queue: &TaskQueue,
  task: &TaskRecord,
  index_buffer: &mut IndexWriteBuffer<'_>,
  checkpoint_path: Option<&str>,
) -> EngineResult<()> {
  flush_reindex_index_buffer(index_buffer)?;
  if let Some(path) = checkpoint_path {
    return queue.update_checkpoint(&task.id, path);
  }
  Ok(())
}

fn reindex_index_buffer_options(args: &serde_json::Value) -> EngineResult<IndexWriteBufferOptions> {
  let flush_after_writes = optional_task_usize(args, "index_flush_writes", DEFAULT_INDEX_BUFFER_FLUSH_WRITES)?.max(1);
  let flush_after_ms = optional_task_u64(args, "index_flush_ms", DEFAULT_INDEX_BUFFER_FLUSH_INTERVAL.as_millis() as u64)?;

  Ok(IndexWriteBufferOptions::new(flush_after_writes, Duration::from_millis(flush_after_ms)))
}

fn collect_current_file_record_paths(
  engine: &StorageEngine,
  base_path: &str,
  memory: &mut OperationMemoryBudget,
) -> EngineResult<Vec<String>> {
  let normalized_base = crate::engine::path_utils::normalize_path(base_path);
  let mut candidate_count = 0usize;
  let mut retained_bytes = 0u64;
  crate::engine::directory_listing::visit_directory_recursive_strict(engine, &normalized_base, -1, None, None, |entry| {
    memory.record_work(1)?;
    if entry.entry_type != EntryType::FileRecord.to_u8() {
      return Ok(true);
    }
    candidate_count =
      candidate_count.checked_add(1).ok_or_else(|| EngineError::ResourceExhausted("forced reindex path count overflow".to_string()))?;
    retained_bytes = retained_bytes
      .checked_add(reindex_path_retained_bytes(&entry.path)?)
      .ok_or_else(|| EngineError::ResourceExhausted("forced reindex path inventory estimate overflow".to_string()))?;
    Ok(true)
  })?;

  memory.reserve(retained_bytes, "forced reindex retained path admission failed")?;
  let mut paths = Vec::new();
  paths
    .try_reserve_exact(candidate_count)
    .map_err(|error| EngineError::ResourceExhausted(format!("forced reindex path vector allocation failed: {error}")))?;
  let mut populated_bytes = 0u64;
  crate::engine::directory_listing::visit_directory_recursive_strict(engine, &normalized_base, -1, None, None, |entry| {
    memory.record_work(1)?;
    if entry.entry_type != EntryType::FileRecord.to_u8() {
      return Ok(true);
    }
    let entry_bytes = reindex_path_retained_bytes(&entry.path)?;
    populated_bytes = populated_bytes
      .checked_add(entry_bytes)
      .ok_or_else(|| EngineError::ResourceExhausted("forced reindex populated path estimate overflow".to_string()))?;
    if paths.len() >= candidate_count || populated_bytes > retained_bytes {
      return Err(EngineError::ResourceExhausted(
        "forced reindex namespace grew while its bounded path inventory was being populated; retry from the durable checkpoint".to_string(),
      ));
    }
    paths.push(entry.path);
    Ok(true)
  })?;

  // FileRecord is a legacy mixed KV bucket: task records and several system
  // records use the same entry type. Supplement the live namespace with only
  // payloads that decode as FileRecord and whose key proves they are the
  // canonical path locator. A failed decode alone cannot identify corruption
  // in this bucket; malformed live path locators are still surfaced when the
  // namespace path is migrated below.
  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  let snapshot = engine.kv_snapshot.load();
  let mixed_candidate_count = snapshot.count_by_type(crate::engine::KV_TYPE_FILE_RECORD)?;
  let vector_bytes = mixed_candidate_count
    .checked_mul(std::mem::size_of::<String>())
    .and_then(|bytes| u64::try_from(bytes).ok())
    .ok_or_else(|| EngineError::ResourceExhausted("forced reindex supplemental path vector estimate overflow".to_string()))?;
  memory.reserve(vector_bytes, "forced reindex supplemental path vector admission failed")?;
  paths
    .try_reserve(mixed_candidate_count)
    .map_err(|error| EngineError::ResourceExhausted(format!("forced reindex supplemental path allocation failed: {error}")))?;
  snapshot.visit_by_type(crate::engine::KV_TYPE_FILE_RECORD, |entry| {
    memory.record_work(1)?;
    let transient_bytes = u64::from(entry.total_length)
      .checked_mul(2)
      .ok_or_else(|| EngineError::ResourceExhausted("forced reindex supplemental FileRecord estimate overflow".to_string()))?;
    memory.reserve(transient_bytes, "forced reindex supplemental FileRecord admission failed")?;
    let selected = match engine.get_entry_including_deleted(&entry.hash) {
      Ok(Some((header, _key, value))) => {
        match crate::engine::file_record::FileRecord::deserialize(&value, hash_length, header.entry_version) {
          Ok(record) if path_in_reindex_scope(&normalized_base, &record.path) => {
            let path_key = crate::engine::directory_ops::file_path_hash(&record.path, &algo)?;
            (entry.hash == path_key).then_some(record.path)
          }
          Ok(_) => None,
          Err(error) => {
            tracing::trace!(
              offset = entry.offset,
              key = %hex::encode(&entry.hash),
              %error,
              "Skipping a non-FileRecord payload in the legacy mixed FileRecord KV bucket"
            );
            None
          }
        }
      }
      Ok(None) => None,
      Err(error) => {
        return match memory.release(transient_bytes, "forced reindex supplemental FileRecord release after read failure") {
          Ok(()) => Err(error),
          Err(release_error) => Err(release_error),
        };
      }
    };
    memory.release(transient_bytes, "forced reindex supplemental FileRecord release failed")?;
    if let Some(path) = selected {
      memory.reserve(reindex_path_retained_bytes(&path)?, "forced reindex supplemental retained path admission failed")?;
      paths.push(path);
    }
    Ok(true)
  })?;
  drop(snapshot);
  paths.sort();
  paths.dedup();
  Ok(paths)
}

fn path_in_reindex_scope(base_path: &str, candidate_path: &str) -> bool {
  if base_path == "/" {
    return true;
  }

  candidate_path == base_path || candidate_path.strip_prefix(base_path.trim_end_matches('/')).is_some_and(|suffix| suffix.starts_with('/'))
}

/// Execute a garbage collection task.
fn execute_gc(
  _queue: &TaskQueue,
  task: &TaskRecord,
  engine: &StorageEngine,
  event_bus: &EventBus,
  cancel: &tokio_util::sync::CancellationToken,
) -> EngineResult<String> {
  let dry_run = optional_task_bool(&task.args, "dry_run", false)?;
  let invocation = match task.origin {
    TaskOriginV1::Direct => GcRunInvocationV1::Task,
    TaskOriginV1::Scheduled => GcRunInvocationV1::Scheduled,
    TaskOriginV1::RepairFollowUp => GcRunInvocationV1::RepairFollowUp,
  };

  let ctx = RequestContext::with_bus(Arc::new(event_bus.clone()));
  let request = GcExecutionRequestV1::new(invocation, dry_run, cancel.clone()).with_task_id(task.id.clone())?;
  let result = execute_gc_run(engine, &ctx, request)?.result;

  let mut message = format!(
    "gc completed: {} garbage entries, {} bytes reclaimed, dry_run={}",
    result.garbage_entries, result.reclaimed_bytes, result.dry_run
  );
  if !result.cleanup_warnings.is_empty() {
    message.push_str(&format!(", cleanup_warnings={}", result.cleanup_warnings.len()));
  }
  Ok(message)
}

/// Execute a backup task: export HEAD (or a named snapshot) to a timestamped `.aeordb` file.
///
/// Task args:
/// - `backup_dir` (string) -- destination directory, default `"./backups/"`.
/// - `retention_count` (integer) -- keep at most this many `.aeordb` files in
///   `backup_dir`. 0 means unlimited. Default: 0.
/// - `snapshot` (string, optional) -- export a named snapshot instead of HEAD.
fn execute_backup(task: &TaskRecord, engine: &StorageEngine, cancel: &tokio_util::sync::CancellationToken) -> EngineResult<String> {
  let backup_dir = optional_task_string(&task.args, "backup_dir")?.unwrap_or("./backups/");
  let retention_count = optional_task_usize(&task.args, "retention_count", 0)?;
  let snapshot_name = optional_task_string(&task.args, "snapshot")?;

  if cancel.is_cancelled() {
    return Err(EngineError::Cancelled("backup".to_string()));
  }
  engine.memory_coordinator().check_admission(MemoryOwner::BackupRestore, AdmissionClass::Maintenance).map_err(task_worker_memory_error)?;

  // Ensure the backup directory exists.
  std::fs::create_dir_all(backup_dir).map_err(|error| {
    EngineError::IoError(std::io::Error::new(error.kind(), format!("failed to create backup directory '{backup_dir}': {error}")))
  })?;

  // Build a timestamped output filename (with milliseconds to avoid collisions).
  let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
  let filename = match snapshot_name {
    Some(name) => format!("backup-{}-{}.aeordb", name, timestamp),
    None => format!("backup-head-{}.aeordb", timestamp),
  };
  let output_path = std::path::Path::new(backup_dir).join(&filename);
  let output_path_string = output_path.to_string_lossy().to_string();

  // Run the export. Scheduled backups don't include system data —
  // they're for user data history, not credential rotation.
  let result = backup::export_snapshot_with_cancellation(engine, snapshot_name, &output_path_string, false, cancel)?;

  // Enforce retention policy if configured.
  let retention_warning = if retention_count > 0 {
    match enforce_backup_retention(backup_dir, retention_count) {
      Ok(()) => None,
      Err(error) => {
        // The backup is already durable. Preserve that primary success while
        // making the incomplete cleanup visible to operators and task callers.
        crate::metrics::record_system_soft_failure("backup", "retention_cleanup", backup_dir, &error);
        tracing::warn!(
            backup_dir = %backup_dir,
            retention_count = %retention_count,
            error = %error,
            "backup retention enforcement failed"
        );
        Some(error)
      }
    }
  } else {
    None
  };

  let mut summary = format!(
    "backup created: {} ({} chunks, {} files, {} dirs)",
    filename, result.chunks_written, result.files_written, result.directories_written,
  );
  if let Some(warning) = retention_warning {
    summary.push_str(&format!("; retention cleanup incomplete: {warning}"));
  }
  Ok(summary)
}

/// Execute a cleanup task: remove expired refresh tokens and used/expired
/// magic links from the system store. Intended to run on a default hourly cron.
fn execute_cleanup(
  _task: &TaskRecord,
  engine: &StorageEngine,
  event_bus: &EventBus,
  cancellation: &tokio_util::sync::CancellationToken,
) -> EngineResult<String> {
  if cancellation.is_cancelled() {
    return Err(EngineError::Cancelled("expired-token cleanup".to_string()));
  }
  engine.memory_coordinator().check_admission(MemoryOwner::Task, AdmissionClass::Maintenance).map_err(task_worker_memory_error)?;
  let ctx = RequestContext::with_bus(Arc::new(event_bus.clone()));
  let (tokens, links) = crate::engine::system_store::cleanup_expired_tokens_with_cancellation(engine, &ctx, Some(cancellation))?;
  Ok(format!("cleaned {} tokens and {} magic links", tokens, links))
}

/// Remove oldest `.aeordb` files in `backup_dir` until at most `keep` remain.
///
/// Files are sorted by modification time (oldest first), and excess files are deleted.
fn enforce_backup_retention(backup_dir: &str, keep: usize) -> Result<(), String> {
  let mut aeordb_files: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();

  let entries = std::fs::read_dir(backup_dir).map_err(|error| format!("failed to read backup directory: {}", error))?;

  for entry in entries {
    let entry = entry.map_err(|error| format!("failed to read directory entry: {}", error))?;
    let path = entry.path();
    if path.extension().and_then(|ext| ext.to_str()) != Some("aeordb") {
      continue;
    }
    let file_type = entry.file_type().map_err(|error| format!("failed to inspect backup entry '{}': {error}", path.display()))?;
    if !file_type.is_file() {
      continue;
    }
    let metadata = entry.metadata().map_err(|error| format!("failed to read backup metadata for '{}': {error}", path.display()))?;
    let modified = metadata.modified().map_err(|error| format!("failed to read backup timestamp for '{}': {error}", path.display()))?;
    aeordb_files.push((path, modified));
  }

  if aeordb_files.len() <= keep {
    return Ok(());
  }

  // Sort oldest first.
  aeordb_files.sort_by_key(|(_path, modified)| *modified);

  let remove_count = aeordb_files.len() - keep;
  let mut failures = Vec::new();
  for (path, _modified) in aeordb_files.iter().take(remove_count) {
    if let Err(error) = std::fs::remove_file(path) {
      tracing::warn!(path = %path.display(), error = %error, "failed to remove old backup");
      failures.push(format!("'{}': {error}", path.display()));
    }
  }

  if failures.is_empty() {
    Ok(())
  } else {
    Err(format!("failed to remove {} old backup(s): {}", failures.len(), failures.join("; ")))
  }
}

/// Compute estimated time remaining based on rolling average of batch durations.
fn compute_eta(batch_times: &[Duration], total_count: usize, indexed_count: usize) -> Option<i64> {
  if batch_times.is_empty() || indexed_count >= total_count {
    return None;
  }

  let total_batch_ms: u128 = batch_times.iter().map(|d| d.as_millis()).sum();
  let average_batch_ms = total_batch_ms / batch_times.len() as u128;
  let remaining_files = total_count - indexed_count;
  let remaining_batches = remaining_files.div_ceil(REINDEX_BATCH_SIZE);
  let eta_ms = average_batch_ms * remaining_batches as u128;

  Some(eta_ms as i64)
}

#[cfg(test)]
mod direct_reindex_path_tests {
  use super::*;
  use crate::engine::request_context::RequestContext;

  #[test]
  fn direct_inventory_refuses_namespace_growth_between_bounded_passes() {
    let (engine, _temp) = crate::server::create_temp_engine_for_tests();
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.store_file_buffered(&ctx, "/growth/a.txt", b"a", Some("text/plain")).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut memory = OperationMemoryBudget::new(
      &engine,
      "direct reindex inventory test",
      MemoryOwner::Task,
      AdmissionClass::Maintenance,
      0,
      Some(&cancel),
    )
    .unwrap();

    let error = collect_direct_reindex_paths_with_between_pass_hook(&ops, "/growth", "/growth", &mut memory, || {
      ops.store_file_buffered(&ctx, "/growth/b.txt", b"b", Some("text/plain")).unwrap();
    })
    .expect_err("a growing namespace must be retried from its durable task checkpoint");

    assert!(matches!(error, EngineError::ResourceExhausted(_)), "unexpected error: {error}");
  }

  #[test]
  fn completed_reindex_routes_every_intent_under_one_stable_task_identity() {
    let operation_id = parse_reindex_source_operation_id("123e4567-e89b-12d3-a456-426614174000").unwrap();
    assert_eq!(operation_id, *uuid::Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap().as_bytes());
    assert!(parse_reindex_source_operation_id("not-a-task-uuid").is_err());

    let targets = completed_reindex_maintenance_targets("/requested", Some("/configured"), true).unwrap();
    assert_eq!(targets.len(), 3);
    assert_eq!(targets[0].class, IndexProducerMaintenanceClassV1::ConfigurationRetirement);
    assert_eq!(targets[0].scope, "/configured");
    assert_eq!(targets[1].class, IndexProducerMaintenanceClassV1::LegacyMigration);
    assert_eq!(targets[1].scope, "/requested");
    assert_eq!(targets[2].class, IndexProducerMaintenanceClassV1::Reindex);
    assert_eq!(targets[2].scope, "/requested");

    let ordinary = completed_reindex_maintenance_targets("/requested", None, false).unwrap();
    assert_eq!(ordinary.len(), 1);
    assert_eq!(ordinary[0].class, IndexProducerMaintenanceClassV1::Reindex);
  }

  #[test]
  fn every_successful_reindex_exit_uses_the_shared_maintenance_completion_boundary() {
    let source = include_str!("task_worker.rs");
    let call = ["admit_completed_reindex_maintenance", "("].concat();
    assert_eq!(source.matches(&call).count(), 3);
  }
}

#[cfg(test)]
#[path = "../../spec/engine/task_worker_retention_internal_spec.rs"]
mod retention_internal_spec;

fn flush_reindex_index_buffer(index_buffer: &mut IndexWriteBuffer<'_>) -> EngineResult<()> {
  index_buffer.flush_all().map(|_| ())
}
