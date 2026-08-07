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
use crate::engine::gc::run_gc_with_cancellation;
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
use crate::engine::task_queue::{ProgressInfo, TaskQueue, TaskRecord, TaskStatus};
use crate::plugins::PluginManager;

/// Maximum completed tasks to keep after pruning.
const PRUNE_MAX_COUNT: usize = 100;
/// Maximum age (in milliseconds) of completed tasks before pruning.
const PRUNE_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000; // 24 hours
/// Number of files to process per batch during reindex.
const REINDEX_BATCH_SIZE: usize = 50;
/// Number of consecutive indexing failures before the circuit breaker trips.
const CIRCUIT_BREAKER_THRESHOLD: usize = 10;
/// Number of recent batch times to keep for ETA calculation.
const ROLLING_AVERAGE_WINDOW: usize = 10;
/// Scheduler bookkeeping retained while one task is claimed and dispatched.
/// Individual task implementations reserve their own material work separately.
const TASK_WORKER_ADMISSION_BYTES: u64 = 256 * 1024;
const TASK_DEFERRAL_BASE_DELAY_MS: i64 = 5_000;
const TASK_DEFERRAL_MAX_DELAY_MS: i64 = 5 * 60 * 1_000;
const REINDEX_RETAINED_PATH_OVERHEAD_BYTES: u64 = std::mem::size_of::<String>() as u64 + 32;

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
      || {},
      || {},
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
        let _ = task_worker_iteration_delay(result, timing);
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
  process_next_task_internal_with_cancel(queue, engine, plugin_manager, event_bus, &dummy_cancel, None, || {}, || {})
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
  process_next_task_internal_with_cancel(queue, engine, plugin_manager, event_bus, &dummy_cancel, None, post_dequeue_hook, || {})
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
  process_next_task_internal_with_cancel(queue, engine, plugin_manager, event_bus, &dummy_cancel, None, || {}, pre_execute_hook)
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
  process_next_task_internal_with_cancel(queue, engine, plugin_manager, event_bus, cancel, None, || {}, pre_execute_hook)
}

fn process_next_task_internal_with_cancel<F, G>(
  queue: &TaskQueue,
  engine: &StorageEngine,
  plugin_manager: &PluginManager,
  event_bus: &EventBus,
  cancel: &tokio_util::sync::CancellationToken,
  captured_run_configuration: Option<MaintenanceRunConfiguration>,
  post_dequeue_hook: F,
  pre_execute_hook: G,
) -> EngineResult<bool>
where
  F: FnOnce(),
  G: FnOnce(),
{
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

  let task_cancellation = queue.register_active_cancellation(&task.id, cancel);
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
    "gc" => execute_gc(queue, &task, engine, task_cancellation.token()),
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

/// Execute a reindex task: re-run the indexing pipeline on all files under a directory.
fn execute_reindex(
  queue: &TaskQueue,
  task: &TaskRecord,
  engine: &StorageEngine,
  plugin_manager: &PluginManager,
  cancel: &tokio_util::sync::CancellationToken,
) -> EngineResult<String> {
  let path = task
    .args
    .get("path")
    .and_then(|value| value.as_str())
    .ok_or_else(|| EngineError::InvalidInput("missing 'path' argument".to_string()))?;
  if queue.is_cancelled(&task.id) || cancel.is_cancelled() {
    return Err(EngineError::Cancelled("reindex".to_string()));
  }
  engine.memory_coordinator().check_admission(MemoryOwner::Task, AdmissionClass::Maintenance).map_err(task_worker_memory_error)?;
  let force = task.args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
  let metadata_only = task.args.get("metadata_only").and_then(|v| v.as_bool()).unwrap_or(false);
  let index_flush_options = reindex_index_buffer_options(&task.args);
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
  let (config, config_dir) = match resolver.find_config_for_reindex_scope(&reindex_root) {
    Ok(Some((config, config_dir))) => (Some(config), Some(config_dir)),
    Ok(None) if force => {
      tracing::info!(
        path = %reindex_root,
        config_path = %config_path,
        "forced reindex running migration-only because no index config was found"
      );
      (None, None)
    }
    Ok(None) => return Err(EngineError::NotFound(config_path)),
    Err(error) => return Err(error),
  };
  let stale_indexes_deleted = if let Some(ref config) = config {
    let config_dir = config_dir.as_deref().expect("resolved config has an owner directory");
    let deleted = IndexManager::new(engine).delete_indexes_not_in_config(config_dir, config)?;
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
    collect_current_file_record_paths(engine, &reindex_root, migration_memory.as_mut().expect("force creates migration memory"))?
  } else if let Some(ref config) = config {
    if let Some(ref glob_pattern) = config.glob {
      collect_recursive_reindex_paths(
        engine,
        &reindex_root,
        config_dir.as_deref().expect("resolved glob config has an owner directory"),
        glob_pattern,
        &mut task_memory,
      )?
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
    return Ok("reindexed 0 files".to_string());
  }

  let pipeline = IndexingPipeline::with_plugin_manager(engine, plugin_manager);
  let ctx = RequestContext::system();
  let mut index_buffer = IndexWriteBuffer::new(engine, index_flush_options);

  let mut indexed_count: usize = 0;
  let mut migrated_count: usize = 0;
  let mut consecutive_failures: usize = 0;
  let mut batch_times: Vec<Duration> = Vec::new();
  let mut last_processed_path: Option<&str> = None;
  let start = Instant::now();

  // Process in batches.
  for batch in file_paths.chunks(REINDEX_BATCH_SIZE) {
    let batch_start = Instant::now();

    for file_path in batch {
      if queue.is_cancelled(&task.id) || cancel.is_cancelled() {
        flush_reindex_before_retry(queue, task, &mut index_buffer, last_processed_path)?;
        return Err(EngineError::Cancelled("reindex".to_string()));
      }
      if let Err(error) = engine.memory_coordinator().check_admission(MemoryOwner::Task, AdmissionClass::Maintenance) {
        flush_reindex_before_retry(queue, task, &mut index_buffer, last_processed_path)?;
        return Err(task_worker_memory_error(error));
      }

      let indexable = match pipeline.path_is_indexable(file_path) {
        Ok(indexable) => indexable,
        Err(error) => {
          flush_reindex_before_retry(queue, task, &mut index_buffer, last_processed_path)?;
          return Err(error);
        }
      };

      if force {
        match ops
          .migrate_file_record_to_current_version_with_memory(file_path, migration_memory.as_mut().expect("force creates migration memory"))
        {
          Ok(true) => {
            migrated_count += 1;
          }
          Ok(false) => {}
          Err(error @ (EngineError::ResourceExhausted(_) | EngineError::Cancelled(_) | EngineError::ShuttingDown)) => {
            flush_reindex_before_retry(queue, task, &mut index_buffer, last_processed_path)?;
            return Err(error);
          }
          Err(error) => {
            tracing::warn!(
              path = %file_path,
              error = %error,
              "forced reindex could not migrate FileRecord"
            );
            consecutive_failures += 1;
            if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
              return Err(reindex_circuit_breaker_error());
            }
            indexed_count += 1;
            last_processed_path = Some(file_path);
            continue;
          }
        }

        if config.is_none() || !indexable {
          consecutive_failures = 0;
          indexed_count += 1;
          last_processed_path = Some(file_path);
          continue;
        }
      }

      if !indexable {
        consecutive_failures = 0;
        indexed_count += 1;
        last_processed_path = Some(file_path);
        continue;
      }

      let index_result = if metadata_only {
        pipeline.run_metadata_only_buffered(&ctx, file_path, &mut index_buffer)
      } else {
        let metadata = match ops.get_metadata(file_path) {
          Ok(metadata) => metadata,
          Err(error @ (EngineError::ResourceExhausted(_) | EngineError::Cancelled(_) | EngineError::ShuttingDown)) => {
            flush_reindex_before_retry(queue, task, &mut index_buffer, last_processed_path)?;
            return Err(error);
          }
          Err(error) => {
            tracing::warn!(path = %file_path, %error, "reindex could not read file metadata");
            consecutive_failures += 1;
            if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
              return Err(reindex_circuit_breaker_error());
            }
            indexed_count += 1;
            last_processed_path = Some(file_path);
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
            flush_reindex_before_retry(queue, task, &mut index_buffer, last_processed_path)?;
            return Err(error);
          }
          Err(error) => {
            task_memory.release_to(buffered_body_checkpoint, "reindex buffered file body release after read failure")?;
            tracing::warn!(path = %file_path, %error, "reindex could not read file body");
            consecutive_failures += 1;
            if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
              return Err(reindex_circuit_breaker_error());
            }
            indexed_count += 1;
            last_processed_path = Some(file_path);
            continue;
          }
        };

        let result = pipeline.run_buffered_with_cancellation(&ctx, file_path, &data, content_type.as_deref(), &mut index_buffer, cancel);
        drop(data);
        task_memory.release_to(buffered_body_checkpoint, "reindex buffered file body release after parsing")?;
        result
      };

      match index_result {
        Ok(()) => {
          consecutive_failures = 0;
        }
        Err(
          error @ (EngineError::ResourceExhausted(_)
          | EngineError::Cancelled(_)
          | EngineError::ShuttingDown
          | EngineError::SystemFamilyPolicy { .. }),
        ) => {
          flush_reindex_before_retry(queue, task, &mut index_buffer, last_processed_path)?;
          return Err(error);
        }
        Err(error) => {
          tracing::warn!(path = %file_path, %error, "reindex pipeline could not index file");
          consecutive_failures += 1;
          if consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
            return Err(reindex_circuit_breaker_error());
          }
        }
      }

      indexed_count += 1;
      last_processed_path = Some(file_path);

      match index_buffer.flush_if_due() {
        Ok(true) => {
          queue.update_checkpoint(&task.id, file_path)?;
        }
        Ok(false) => {}
        Err(error) => return Err(error),
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
    if index_buffer.stats()?.pending_mutations == 0 {
      if let Some(last_path) = last_processed_path {
        queue.update_checkpoint(&task.id, last_path)?;
      }
    }

    // Compute progress and ETA.
    let progress = indexed_count as f64 / total_count as f64;
    let eta_ms = compute_eta(&batch_times, total_count, indexed_count);

    let index_stats = index_buffer.stats()?;
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
          "indexed {}/{} files, migrated {}, metadata_only={}, index_mutations={}, pending_index_mutations={}, index_flushes={}, cached_indexes={}",
          indexed_count,
          total_count,
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
      flush_reindex_before_retry(queue, task, &mut index_buffer, last_processed_path)?;
      return Err(EngineError::Cancelled("reindex".to_string()));
    }
  }

  let flushed_indexes = index_buffer.flush_all()?;
  if let Some(last_path) = file_paths.last() {
    queue.update_checkpoint(&task.id, last_path)?;
  }

  let elapsed_ms = start.elapsed().as_millis();
  let index_stats = index_buffer.stats()?;
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

fn reindex_circuit_breaker_error() -> EngineError {
  EngineError::InvalidInput(format!("circuit breaker: {CIRCUIT_BREAKER_THRESHOLD} consecutive indexing failures"))
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
  crate::engine::directory_listing::visit_directory_recursive(engine, reindex_root, -1, None, None, |entry| {
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
  crate::engine::directory_listing::visit_directory_recursive(engine, reindex_root, -1, None, None, |entry| {
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
  ops.visit_live_directory_children(reindex_root, |entry| {
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
  ops.visit_live_directory_children(reindex_root, |entry| {
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
  last_processed_path: Option<&str>,
) -> EngineResult<()> {
  index_buffer.flush_all()?;
  if let Some(path) = last_processed_path {
    queue.update_checkpoint(&task.id, path)?;
  }
  Ok(())
}

fn reindex_index_buffer_options(args: &serde_json::Value) -> IndexWriteBufferOptions {
  let flush_after_writes = args
    .get("index_flush_writes")
    .and_then(|value| value.as_u64())
    .and_then(|value| usize::try_from(value).ok())
    .unwrap_or(DEFAULT_INDEX_BUFFER_FLUSH_WRITES)
    .max(1);

  let flush_after =
    args.get("index_flush_ms").and_then(|value| value.as_u64()).map(Duration::from_millis).unwrap_or(DEFAULT_INDEX_BUFFER_FLUSH_INTERVAL);

  IndexWriteBufferOptions::new(flush_after_writes, flush_after)
}

fn collect_current_file_record_paths(
  engine: &StorageEngine,
  base_path: &str,
  memory: &mut OperationMemoryBudget,
) -> EngineResult<Vec<String>> {
  let normalized_base = crate::engine::path_utils::normalize_path(base_path);
  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  let snapshot = engine.kv_snapshot.load();
  let candidate_count = snapshot.count_by_type(crate::engine::KV_TYPE_FILE_RECORD)?;
  let vector_bytes = candidate_count
    .checked_mul(std::mem::size_of::<String>())
    .and_then(|bytes| u64::try_from(bytes).ok())
    .ok_or_else(|| EngineError::ResourceExhausted("forced reindex path vector estimate overflow".to_string()))?;
  memory.reserve(vector_bytes, "forced reindex path vector admission failed")?;
  let mut paths = Vec::new();
  paths
    .try_reserve_exact(candidate_count)
    .map_err(|error| EngineError::ResourceExhausted(format!("forced reindex path vector allocation failed: {error}")))?;

  snapshot.visit_by_type(crate::engine::KV_TYPE_FILE_RECORD, |entry| {
    memory.record_work(1)?;
    let transient_bytes = u64::from(entry.total_length)
      .checked_mul(2)
      .ok_or_else(|| EngineError::ResourceExhausted("forced reindex FileRecord estimate overflow".to_string()))?;
    memory.reserve(transient_bytes, "forced reindex FileRecord admission failed")?;
    let read_result = engine.get_entry_including_deleted(&entry.hash);
    let selected = match read_result {
      Ok(Some((header, _key, value))) => {
        match crate::engine::file_record::FileRecord::deserialize(&value, hash_length, header.entry_version) {
          Ok(record) if path_in_reindex_scope(&normalized_base, &record.path) => {
            let path_key = crate::engine::directory_ops::file_path_hash(&record.path, &algo)?;
            if entry.hash == path_key {
              let retained_bytes = u64::try_from(record.path.capacity())
                .map_err(|_| EngineError::ResourceExhausted("forced reindex path estimate overflow".to_string()))?;
              memory.reserve(retained_bytes, "forced reindex retained path admission failed")?;
              Some(record.path)
            } else {
              None
            }
          }
          Ok(_) | Err(_) => None,
        }
      }
      Ok(None) => None,
      Err(error) => {
        return match memory.release(transient_bytes, "forced reindex FileRecord release after read failure") {
          Ok(()) => Err(error),
          Err(release_error) => Err(release_error),
        };
      }
    };
    memory.release(transient_bytes, "forced reindex FileRecord release failed")?;
    if let Some(path) = selected {
      paths.push(path);
    }
    Ok(true)
  })?;
  drop(snapshot);
  paths.sort();
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
  cancel: &tokio_util::sync::CancellationToken,
) -> EngineResult<String> {
  let dry_run = task.args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);

  let ctx = RequestContext::system();
  let result = run_gc_with_cancellation(engine, &ctx, dry_run, cancel)?;

  Ok(format!(
    "gc completed: {} garbage entries, {} bytes reclaimed, dry_run={}",
    result.garbage_entries, result.reclaimed_bytes, result.dry_run
  ))
}

/// Execute a backup task: export HEAD (or a named snapshot) to a timestamped `.aeordb` file.
///
/// Task args:
/// - `backup_dir` (string) -- destination directory, default `"./backups/"`.
/// - `retention_count` (integer) -- keep at most this many `.aeordb` files in
///   `backup_dir`. 0 means unlimited. Default: 0.
/// - `snapshot` (string, optional) -- export a named snapshot instead of HEAD.
fn execute_backup(task: &TaskRecord, engine: &StorageEngine, cancel: &tokio_util::sync::CancellationToken) -> EngineResult<String> {
  let backup_dir = task.args.get("backup_dir").and_then(|v| v.as_str()).unwrap_or("./backups/");

  let retention_count = task.args.get("retention_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

  let snapshot_name = task.args.get("snapshot").and_then(|v| v.as_str());

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
  if retention_count > 0 {
    if let Err(error) = enforce_backup_retention(backup_dir, retention_count) {
      // Retention failure is not fatal -- log but do not fail the task.
      tracing::warn!(
          backup_dir = %backup_dir,
          retention_count = %retention_count,
          error = %error,
          "backup retention enforcement failed"
      );
    }
  }

  Ok(format!(
    "backup created: {} ({} chunks, {} files, {} dirs)",
    filename, result.chunks_written, result.files_written, result.directories_written,
  ))
}

/// Execute a cleanup task: remove expired refresh tokens and used/expired
/// magic links from the system store. Intended to run on a default hourly cron.
fn execute_cleanup(
  _task: &TaskRecord,
  engine: &StorageEngine,
  _event_bus: &EventBus,
  cancellation: &tokio_util::sync::CancellationToken,
) -> EngineResult<String> {
  if cancellation.is_cancelled() {
    return Err(EngineError::Cancelled("expired-token cleanup".to_string()));
  }
  engine.memory_coordinator().check_admission(MemoryOwner::Task, AdmissionClass::Maintenance).map_err(task_worker_memory_error)?;
  let ctx = RequestContext::system();
  let (tokens, links) = crate::engine::system_store::cleanup_expired_tokens(engine, &ctx)?;
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
    if path.extension().and_then(|ext| ext.to_str()) == Some("aeordb") {
      let modified = entry.metadata().and_then(|metadata| metadata.modified()).unwrap_or(std::time::UNIX_EPOCH);
      aeordb_files.push((path, modified));
    }
  }

  if aeordb_files.len() <= keep {
    return Ok(());
  }

  // Sort oldest first.
  aeordb_files.sort_by_key(|(_path, modified)| *modified);

  let remove_count = aeordb_files.len() - keep;
  for (path, _modified) in aeordb_files.iter().take(remove_count) {
    if let Err(error) = std::fs::remove_file(path) {
      tracing::warn!(path = %path.display(), error = %error, "failed to remove old backup");
    }
  }

  Ok(())
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
}
