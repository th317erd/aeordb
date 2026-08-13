use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use crate::engine::namespace_mutation::{
  NamespaceMutationBatch, NamespaceMutationCoordinator, NamespaceMutationKind, NamespaceMutationSourceIdentity,
};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::storage_engine::StorageEngine;

const TASK_PREFIX: &str = "::aeordb:task:";
const TASK_REGISTRY: &str = "::aeordb:task:_registry";
const TASK_NAMESPACE_ROOT: &str = "/.aeordb-system/tasks";
const TASK_JSON_ALLOCATION_MULTIPLIER: u64 = 3;
const TASK_JSON_ALLOCATION_OVERHEAD: u64 = 512;
const TASK_PRUNE_BATCH_MAXIMUM: usize = 256;

fn task_storage_hash(key: &str) -> Vec<u8> {
  blake3::hash(key.as_bytes()).as_bytes().to_vec()
}

/// Recognize and validate the task queue's sanctioned reuse of the low-level
/// FileRecord tag. `None` means the row belongs to another FileRecord producer.
#[doc(hidden)]
pub fn validate_task_storage_record(hash: &[u8], value: &[u8]) -> Option<EngineResult<()>> {
  if hash == task_storage_hash(TASK_REGISTRY) {
    return Some(decode_task_registry(value).map(|_| ()));
  }

  let record = serde_json::from_slice::<TaskRecord>(value).ok()?;
  (hash == task_storage_hash(&format!("{TASK_PREFIX}{}", record.id))).then_some(Ok(()))
}

fn decode_task_registry(value: &[u8]) -> EngineResult<Vec<String>> {
  let registry: Vec<String> = serde_json::from_slice(value).map_err(|error| EngineError::CorruptEntry {
    offset: 0,
    reason: format!("task registry is malformed: deserialization error: {error}"),
  })?;
  let mut unique = HashSet::with_capacity(registry.len());
  for id in &registry {
    if id.is_empty() {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "task registry contains an empty id".to_string() });
    }
    if !unique.insert(id.as_str()) {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("task registry contains duplicate id '{id}'") });
    }
  }
  Ok(registry)
}

fn decode_task_record(expected_id: &str, value: &[u8]) -> EngineResult<TaskRecord> {
  let record: TaskRecord = serde_json::from_slice(value)
    .map_err(|error| EngineError::CorruptEntry { offset: 0, reason: format!("task '{expected_id}' deserialization error: {error}") })?;
  if record.id != expected_id {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!("task record id '{}' does not match registry id '{expected_id}'", record.id),
    });
  }
  Ok(record)
}

/// Lifecycle status of a background task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
  /// Waiting to be picked up by the task runner.
  Pending,
  /// Currently executing.
  Running,
  /// Finished successfully.
  Completed,
  /// Finished with an error.
  Failed,
  /// Cancelled by the user.
  Cancelled,
}

/// Persisted provenance for task execution. Legacy records predate this field
/// and decode as direct queue submissions.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskOriginV1 {
  #[default]
  Direct,
  Scheduled,
  RepairFollowUp,
}

/// A persisted background task record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
  /// Unique task identifier (UUID v4).
  pub id: String,
  /// Task type name (e.g. `"reindex"`, `"gc"`).
  pub task_type: String,
  /// Arbitrary JSON arguments for the task.
  pub args: serde_json::Value,
  /// Doorway that durably submitted this task.
  #[serde(default)]
  pub origin: TaskOriginV1,
  /// Current lifecycle status.
  pub status: TaskStatus,
  /// When the task was enqueued (ms since epoch).
  pub created_at: i64,
  /// When the task began executing (ms since epoch).
  pub started_at: Option<i64>,
  /// When the task finished (ms since epoch).
  pub completed_at: Option<i64>,
  /// Error message if the task failed.
  pub error: Option<String>,
  /// Opaque checkpoint string for resumable tasks.
  pub checkpoint: Option<String>,
  /// Earliest wall-clock time when a deferred task may be claimed again.
  #[serde(default)]
  pub retry_at: Option<i64>,
  /// Number of retryable execution deferrals retained for backoff and diagnostics.
  #[serde(default)]
  pub deferral_count: u32,
}

/// In-memory progress information for a running task.
#[derive(Debug, Clone)]
pub struct ProgressInfo {
  /// Task identifier.
  pub task_id: String,
  /// Task type name.
  pub task_type: String,
  /// Task arguments.
  pub args: serde_json::Value,
  /// Progress as a fraction (0.0 to 1.0).
  pub progress: f64,
  /// Estimated time remaining in milliseconds.
  pub eta_ms: Option<i64>,
  /// Number of items processed so far.
  pub indexed_count: usize,
  /// Total number of items to process.
  pub total_count: usize,
  /// Timestamp (ms since epoch) when data became stale.
  pub stale_since: Option<i64>,
  /// Human-readable status message.
  pub message: Option<String>,
}

/// Background task queue backed by the storage engine.
///
/// Tasks are persisted as JSON entries keyed by deterministic hashes and
/// tracked via a registry entry. Supports enqueue, dequeue, cancel,
/// in-memory progress tracking, and automatic pruning of completed tasks.
///
/// NOTE: Task records are stored using `EntryType::FileRecord`, which means
/// they are counted in `stats().file_count` and could theoretically be swept
/// by GC. Task records use deterministic hashes from `"::aeordb:task:{id}"`
/// which do NOT appear in the directory tree. To protect tasks from GC,
/// `gc_mark` explicitly marks task hashes as live (see `mark_task_entries`).
pub struct TaskQueue {
  engine: Arc<StorageEngine>,
  progress: Arc<RwLock<HashMap<String, ProgressInfo>>>,
  cancelled: Arc<RwLock<HashSet<String>>>,
  active_cancellations: Arc<RwLock<HashMap<String, Arc<CancellationToken>>>>,
  /// Serializes every persisted task/registry read-modify-write transition.
  /// Without one authority, checkpoint, cancel, completion, and recovery can
  /// overwrite one another with stale records.
  state_lock: Mutex<()>,
}

pub(crate) struct ActiveTaskCancellation<'a> {
  queue: &'a TaskQueue,
  task_id: String,
  token: Arc<CancellationToken>,
}

impl ActiveTaskCancellation<'_> {
  pub(crate) fn token(&self) -> &CancellationToken {
    &self.token
  }
}

impl Drop for ActiveTaskCancellation<'_> {
  fn drop(&mut self) {
    match self.queue.active_cancellations.write() {
      Ok(mut active) => {
        if active.get(&self.task_id).is_some_and(|current| Arc::ptr_eq(current, &self.token)) {
          active.remove(&self.task_id);
        }
      }
      Err(error) => {
        crate::metrics::record_system_soft_failure("task_queue", "active_cancellation_drop", &self.task_id, error);
      }
    }
  }
}

impl TaskQueue {
  pub fn new(engine: Arc<StorageEngine>) -> Self {
    TaskQueue {
      engine,
      progress: Arc::new(RwLock::new(HashMap::new())),
      cancelled: Arc::new(RwLock::new(HashSet::new())),
      active_cancellations: Arc::new(RwLock::new(HashMap::new())),
      state_lock: Mutex::new(()),
    }
  }

  /// Create a new task with `status = Pending`, persist it, and add its ID to the registry.
  ///
  /// Returns the created [`TaskRecord`] including the generated UUID.
  pub fn enqueue(&self, task_type: &str, args: serde_json::Value) -> EngineResult<TaskRecord> {
    self.enqueue_with_origin(task_type, args, TaskOriginV1::Direct)
  }

  /// Create a task with explicit persisted invocation provenance.
  pub fn enqueue_with_origin(&self, task_type: &str, args: serde_json::Value, origin: TaskOriginV1) -> EngineResult<TaskRecord> {
    // Serialize the entire enqueue operation so concurrent enqueues cannot
    // interleave registry reads and writes (which would lose entries).
    let _state_guard = self.lock_state("enqueue")?;

    let id = uuid::Uuid::new_v4().to_string();
    let record = TaskRecord {
      id: id.clone(),
      task_type: task_type.to_string(),
      args,
      origin,
      status: TaskStatus::Pending,
      created_at: chrono::Utc::now().timestamp_millis(),
      started_at: None,
      completed_at: None,
      error: None,
      checkpoint: None,
      retry_at: None,
      deferral_count: 0,
    };

    let record_for_plan = record.clone();
    NamespaceMutationCoordinator::new(&self.engine).prepare_and_execute(|planning_engine| {
      let mut registry = load_task_registry(planning_engine)?;
      if registry.iter().any(|existing| existing == &record_for_plan.id) {
        return Err(EngineError::AlreadyExists(format!("task {}", record_for_plan.id)));
      }
      let task_key = task_record_hash(&record_for_plan.id);
      if validate_task_locator_type(planning_engine, &task_key, &format!("task '{}'", record_for_plan.id))? {
        return Err(EngineError::AlreadyExists(format!("task {}", record_for_plan.id)));
      }
      registry.push(record_for_plan.id.clone());

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      append_task_replacement(planning_engine, &mut batch, &record_for_plan)?;
      append_registry_replacement(planning_engine, &mut batch, &registry)?;
      Ok((batch, ()))
    })?;

    Ok(record)
  }

  /// Queue the required non-destructive GC proof after an explicit repair.
  pub fn enqueue_gc_repair_follow_up(&self) -> EngineResult<TaskRecord> {
    self.enqueue_with_origin("gc", serde_json::json!({"dry_run": true}), TaskOriginV1::RepairFollowUp)
  }

  /// Load all tasks and return the oldest pending one (FIFO order).
  /// H18: Atomically find the oldest pending task AND mark it as Running.
  /// Uses the task state lock to prevent concurrent dequeues from claiming
  /// the same task or racing another lifecycle transition.
  pub fn dequeue_next(&self) -> EngineResult<Option<TaskRecord>> {
    let mut memory = OperationMemoryBudget::new(&self.engine, "task dequeue", MemoryOwner::Task, AdmissionClass::Maintenance, 0, None)?;
    self.dequeue_next_with_memory(&mut memory)
  }

  pub(crate) fn dequeue_next_with_memory(&self, memory: &mut OperationMemoryBudget) -> EngineResult<Option<TaskRecord>> {
    let _state_guard = self.lock_state("dequeue")?;
    let (_acknowledgement, oldest) = NamespaceMutationCoordinator::new(&self.engine).prepare_and_maybe_execute(|planning_engine| {
      let (registry, registry_charge) = load_task_registry_with_memory(planning_engine, memory)?;
      let mut oldest: Option<TaskRecord> = None;
      let mut oldest_charge = 0u64;
      let now = chrono::Utc::now().timestamp_millis();
      for id in &registry {
        memory.record_work(1)?;
        let candidate_checkpoint = memory.checkpoint();
        let (task, task_charge) = load_task_with_memory(planning_engine, id, memory)?;
        let eligible = task.status == TaskStatus::Pending && task.retry_at.is_none_or(|retry_at| retry_at <= now);
        let replace = eligible && oldest.as_ref().is_none_or(|current| task.created_at < current.created_at);
        if replace {
          let previous = oldest.replace(task);
          drop(previous);
          memory.release(oldest_charge, "task dequeue replaced retained FIFO candidate")?;
          oldest_charge = task_charge;
        } else {
          drop(task);
          memory.release_to(candidate_checkpoint, "task dequeue released non-selected record")?;
        }
      }
      drop(registry);
      memory.release(registry_charge, "task dequeue released registry inventory")?;

      let Some(ref mut task) = oldest else {
        return Ok((None, None));
      };
      task.status = TaskStatus::Running;
      task.started_at = Some(chrono::Utc::now().timestamp_millis());
      task.completed_at = None;
      task.error = None;
      task.retry_at = None;
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      append_task_replacement(planning_engine, &mut batch, task)?;
      Ok((Some(batch), oldest))
    })?;
    Ok(oldest)
  }

  /// Update a task's status and set started_at/completed_at timestamps as appropriate.
  pub fn update_status(&self, id: &str, status: TaskStatus, error: Option<String>) -> EngineResult<()> {
    let _state_guard = self.lock_state("status update")?;
    self.update_status_unlocked(id, status, error)
  }

  /// Update the checkpoint field on a task.
  pub fn update_checkpoint(&self, id: &str, checkpoint: &str) -> EngineResult<()> {
    let _state_guard = self.lock_state("checkpoint update")?;
    let checkpoint = checkpoint.to_string();
    self.transition_task_unlocked(id, |record| {
      record.checkpoint = Some(checkpoint);
      Ok((true, ()))
    })
  }

  /// Return a running task to the pending queue without losing its durable checkpoint.
  /// A cancellation or terminal transition that won the state lock is never overwritten.
  pub fn requeue_running(&self, id: &str) -> EngineResult<bool> {
    let _state_guard = self.lock_state("task requeue")?;
    self.transition_task_unlocked(id, |record| {
      if record.status != TaskStatus::Running {
        return Ok((false, false));
      }
      record.status = TaskStatus::Pending;
      record.started_at = None;
      record.completed_at = None;
      record.error = None;
      record.retry_at = None;
      Ok((true, true))
    })
  }

  /// Return a running task to Pending while making it ineligible until the
  /// supplied wall-clock time. Cancellation and terminal states retain
  /// precedence because the transition is serialized by the task state lock.
  pub fn defer_running_until(&self, id: &str, retry_at: i64) -> EngineResult<Option<TaskRecord>> {
    let _state_guard = self.lock_state("task deferral")?;
    let now = chrono::Utc::now().timestamp_millis();
    if retry_at <= now {
      return Err(EngineError::InvalidInput("task retry_at must be in the future".to_string()));
    }
    self.transition_task_unlocked(id, |record| {
      if record.status != TaskStatus::Running {
        return Ok((false, None));
      }
      record.status = TaskStatus::Pending;
      record.started_at = None;
      record.completed_at = None;
      record.error = None;
      record.retry_at = Some(retry_at);
      record.deferral_count = record.deferral_count.saturating_add(1);
      Ok((true, Some(record.clone())))
    })
  }

  /// Finish a task only while it is still running. This is the worker's
  /// compare-and-transition operation and prevents stale completion/failure
  /// writes from replacing a concurrent cancellation.
  pub fn finish_running(&self, id: &str, status: TaskStatus, error: Option<String>) -> EngineResult<bool> {
    if !matches!(status, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled) {
      return Err(EngineError::InvalidInput("finish_running requires a terminal task status".to_string()));
    }

    let _state_guard = self.lock_state("task completion")?;
    self.transition_task_unlocked(id, |record| {
      if record.status != TaskStatus::Running {
        return Ok((false, false));
      }
      apply_task_status(record, status, error);
      Ok((true, true))
    })
  }

  /// Recover crash-interrupted tasks without discarding their last durable checkpoint.
  pub fn recover_interrupted_tasks(&self) -> EngineResult<usize> {
    let _state_guard = self.lock_state("startup task recovery")?;
    let mut memory =
      OperationMemoryBudget::new(&self.engine, "startup task recovery", MemoryOwner::Task, AdmissionClass::Maintenance, 0, None)?;
    let (registry, _registry_charge) = self.load_registry_with_memory(&mut memory)?;
    let mut recovered = 0usize;
    for id in &registry {
      memory.record_work(1)?;
      let record_checkpoint = memory.checkpoint();
      let (task, _task_charge) = self.load_task_with_memory(id, &mut memory)?;
      if task.status != TaskStatus::Running {
        drop(task);
        memory.release_to(record_checkpoint, "startup task recovery released inactive record")?;
        continue;
      }
      if self.transition_task_unlocked(id, |current| {
        if current.status != TaskStatus::Running {
          return Ok((false, false));
        }
        current.status = TaskStatus::Pending;
        current.started_at = None;
        current.completed_at = None;
        current.error = None;
        current.retry_at = None;
        Ok((true, true))
      })? {
        recovered = recovered.saturating_add(1);
      }
      drop(task);
      memory.release_to(record_checkpoint, "startup task recovery released recovered record")?;
    }
    Ok(recovered)
  }

  /// Load a single task by ID.
  pub fn get_task(&self, id: &str) -> EngineResult<Option<TaskRecord>> {
    let hash = task_record_hash(id);
    match self.engine.get_entry_verified(&hash)? {
      Some((header, key, value)) => {
        validate_task_entry(&hash, &header, &key, id)?;
        Ok(Some(decode_task_record(id, &value)?))
      }
      None => Ok(None),
    }
  }

  /// Load all tasks from the registry.
  pub fn list_tasks(&self) -> EngineResult<Vec<TaskRecord>> {
    self.list_tasks_unlocked()
  }

  /// Cancel a task: mark it as cancelled both in memory and on disk.
  pub fn cancel(&self, id: &str) -> EngineResult<()> {
    let _state_guard = self.lock_state("task cancellation")?;
    self.update_status_unlocked(id, TaskStatus::Cancelled, None)?;
    {
      let mut cancelled = self.cancelled.write().map_err(|error| {
        EngineError::IoError(std::io::Error::other(format!("task cancellation authority is poisoned during cancellation: {error}")))
      })?;
      cancelled.insert(id.to_string());
    }
    let active = self.active_cancellations.read().map_err(|error| {
      EngineError::IoError(std::io::Error::other(format!("active task cancellation authority is poisoned during cancellation: {error}")))
    })?;
    if let Some(token) = active.get(id) {
      token.cancel();
    }
    Ok(())
  }

  /// Mark a task as cancelled in memory only (without updating persisted status).
  /// Useful for testing mid-execution cancellation detection.
  pub fn mark_cancelled_in_memory(&self, id: &str) -> EngineResult<()> {
    {
      let mut cancelled = self.cancelled.write().map_err(|error| {
        EngineError::IoError(std::io::Error::other(format!(
          "task cancellation authority is poisoned during in-memory cancellation: {error}"
        )))
      })?;
      cancelled.insert(id.to_string());
    }
    let active = self.active_cancellations.read().map_err(|error| {
      EngineError::IoError(std::io::Error::other(format!(
        "active task cancellation authority is poisoned during in-memory cancellation: {error}"
      )))
    })?;
    if let Some(token) = active.get(id) {
      token.cancel();
    }
    Ok(())
  }

  /// Check if a task has been cancelled (in-memory check for speed).
  pub fn is_cancelled(&self, id: &str) -> bool {
    match self.cancelled.read() {
      Ok(cancelled) => cancelled.contains(id),
      Err(error) => {
        tracing::error!(%error, task_id = id, "task cancellation authority is poisoned; treating task as cancelled");
        true
      }
    }
  }

  pub(crate) fn register_active_cancellation<'a>(
    &'a self,
    id: &str,
    parent: &CancellationToken,
  ) -> EngineResult<ActiveTaskCancellation<'a>> {
    let token = Arc::new(parent.child_token());
    {
      let mut active = self.active_cancellations.write().map_err(|error| {
        EngineError::IoError(std::io::Error::other(format!("active task cancellation authority is poisoned during registration: {error}")))
      })?;
      active.insert(id.to_string(), Arc::clone(&token));
    }
    if self.is_cancelled(id) {
      token.cancel();
    }
    Ok(ActiveTaskCancellation { queue: self, task_id: id.to_string(), token })
  }

  /// Set in-memory progress info for a task.
  pub fn set_progress(&self, id: &str, info: ProgressInfo) {
    match self.progress.write() {
      Ok(mut progress) => {
        progress.insert(id.to_string(), info);
      }
      Err(error) => crate::metrics::record_system_soft_failure("task_queue", "progress_write", id, error),
    }
  }

  /// Get in-memory progress info for a task.
  pub fn get_progress(&self, id: &str) -> Option<ProgressInfo> {
    match self.progress.read() {
      Ok(progress) => progress.get(id).cloned(),
      Err(error) => {
        crate::metrics::record_system_soft_failure("task_queue", "progress_read", id, error);
        None
      }
    }
  }

  /// Find any running reindex task whose args.path is a prefix of the given path.
  pub fn get_reindex_progress_for_path(&self, path: &str) -> Option<ProgressInfo> {
    let progress = match self.progress.read() {
      Ok(progress) => progress,
      Err(error) => {
        crate::metrics::record_system_soft_failure("task_queue", "reindex_progress_read", path, error);
        return None;
      }
    };
    for info in progress.values() {
      if info.task_type == "reindex" {
        if let Some(task_path) = info.args.get("path").and_then(|v| v.as_str()) {
          if path.starts_with(task_path) {
            return Some(info.clone());
          }
        }
      }
    }
    None
  }

  /// Remove in-memory progress info for a task.
  pub fn clear_progress(&self, id: &str) {
    match self.progress.write() {
      Ok(mut progress) => {
        progress.remove(id);
      }
      Err(error) => crate::metrics::record_system_soft_failure("task_queue", "progress_clear", id, error),
    }
  }

  /// Remove completed/failed/cancelled tasks exceeding age or count limits.
  /// Returns the number of tasks pruned.
  pub fn prune_completed(&self, max_age_ms: i64, max_count: usize) -> EngineResult<usize> {
    let _state_guard = self.lock_state("task pruning")?;
    let mut memory = OperationMemoryBudget::new(&self.engine, "task pruning", MemoryOwner::Task, AdmissionClass::Maintenance, 0, None)?;
    let now = chrono::Utc::now().timestamp_millis();
    let (mut registry, _registry_charge) = self.load_registry_with_memory(&mut memory)?;
    let workspace_bytes = u64::try_from(registry.len())
      .ok()
      .and_then(|count| count.checked_mul((std::mem::size_of::<(usize, i64)>() + 1) as u64))
      .ok_or_else(|| EngineError::ResourceExhausted("task pruning workspace estimate overflow".to_string()))?;
    memory.reserve(workspace_bytes, "task pruning workspace admission failed")?;
    let mut remove = Vec::new();
    remove
      .try_reserve_exact(registry.len())
      .map_err(|error| EngineError::ResourceExhausted(format!("task pruning removal bitmap allocation failed: {error}")))?;
    remove.resize(registry.len(), false);
    let mut retained_terminal = Vec::new();
    retained_terminal
      .try_reserve_exact(registry.len())
      .map_err(|error| EngineError::ResourceExhausted(format!("task pruning candidate allocation failed: {error}")))?;

    for (index, id) in registry.iter().enumerate() {
      memory.record_work(1)?;
      let record_checkpoint = memory.checkpoint();
      let (task, _task_charge) = self.load_task_with_memory(id, &mut memory)?;
      if matches!(task.status, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled) {
        let task_time = task.completed_at.unwrap_or(task.created_at);
        if now.saturating_sub(task_time) > max_age_ms {
          remove[index] = true;
        } else {
          retained_terminal.push((index, task_time));
        }
      }
      drop(task);
      memory.release_to(record_checkpoint, "task pruning released scanned record")?;
    }

    retained_terminal.sort_unstable_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    for (index, _completed_at) in retained_terminal.into_iter().skip(max_count) {
      remove[index] = true;
    }

    let mut selected_for_pruning = 0usize;
    for should_remove in &mut remove {
      if !*should_remove {
        continue;
      }
      if selected_for_pruning == TASK_PRUNE_BATCH_MAXIMUM {
        *should_remove = false;
        continue;
      }
      selected_for_pruning += 1;
    }

    let pruned = remove.iter().filter(|remove| **remove).count();
    if pruned == 0 {
      return Ok(0);
    }

    let expected_registry_digest = task_registry_digest(&registry);
    let removed_ids = registry.iter().enumerate().filter_map(|(index, id)| remove[index].then_some(id.clone())).collect::<Vec<_>>();

    let mut index = 0usize;
    registry.retain(|_| {
      let retain = !remove[index];
      index += 1;
      retain
    });
    NamespaceMutationCoordinator::new(&self.engine).prepare_and_execute(|planning_engine| {
      let current_registry = load_task_registry(planning_engine)?;
      if task_registry_digest(&current_registry) != expected_registry_digest {
        return Err(EngineError::AlreadyExists("task registry changed while pruning was prepared".to_string()));
      }
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      for id in &removed_ids {
        let key = task_record_hash(id);
        validate_task_locator_type(planning_engine, &key, &format!("task '{id}'"))?;
        batch.retire_locator(key.clone())?;
        batch.add_source_identity(NamespaceMutationSourceIdentity {
          path: task_namespace_path(id),
          entry_type: Some(EntryType::FileRecord.to_u8()),
          previous_identity: Some(key),
          new_identity: None,
        })?;
      }
      append_registry_replacement(planning_engine, &mut batch, &registry)?;
      Ok((batch, ()))
    })?;

    match self.cancelled.write() {
      Ok(mut cancelled) => {
        for id in &removed_ids {
          cancelled.remove(id);
        }
      }
      Err(error) => crate::metrics::record_system_soft_failure("task_queue", "prune_cancellation_cleanup", removed_ids.len(), error),
    }

    Ok(pruned)
  }

  // -------------------------------------------------------------------------
  // Registry helpers
  // -------------------------------------------------------------------------

  fn load_registry(&self) -> EngineResult<Vec<String>> {
    load_task_registry(&self.engine)
  }

  fn load_registry_with_memory(&self, memory: &mut OperationMemoryBudget) -> EngineResult<(Vec<String>, u64)> {
    load_task_registry_with_memory(&self.engine, memory)
  }

  fn load_task_with_memory(&self, id: &str, memory: &mut OperationMemoryBudget) -> EngineResult<(TaskRecord, u64)> {
    load_task_with_memory(&self.engine, id, memory)
  }

  fn lock_state(&self, operation: &str) -> EngineResult<std::sync::MutexGuard<'_, ()>> {
    self
      .state_lock
      .lock()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("task state lock poisoned during {operation}: {error}"))))
  }

  fn list_tasks_unlocked(&self) -> EngineResult<Vec<TaskRecord>> {
    let registry = self.load_registry()?;
    let mut tasks = Vec::new();
    for id in &registry {
      let hash = task_record_hash(id);
      let Some((header, key, value)) = self.engine.get_entry_verified(&hash)? else {
        return Err(EngineError::CorruptEntry { offset: 0, reason: format!("task registry references missing task '{id}'") });
      };
      validate_task_entry(&hash, &header, &key, id)?;
      tasks.push(decode_task_record(id, &value)?);
    }
    Ok(tasks)
  }

  fn update_status_unlocked(&self, id: &str, status: TaskStatus, error: Option<String>) -> EngineResult<()> {
    self.transition_task_unlocked(id, |record| {
      apply_task_status(record, status, error);
      Ok((true, ()))
    })
  }

  fn transition_task_unlocked<T, F>(&self, id: &str, transition: F) -> EngineResult<T>
  where
    F: FnOnce(&mut TaskRecord) -> EngineResult<(bool, T)>,
  {
    let id = id.to_string();
    let (_acknowledgement, output) = NamespaceMutationCoordinator::new(&self.engine).prepare_and_maybe_execute(|planning_engine| {
      let mut record = load_task_record(planning_engine, &id)?;
      let (changed, output) = transition(&mut record)?;
      if !changed {
        return Ok((None, output));
      }
      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      append_task_replacement(planning_engine, &mut batch, &record)?;
      Ok((Some(batch), output))
    })?;
    Ok(output)
  }
}

fn task_record_hash(id: &str) -> Vec<u8> {
  task_storage_hash(&format!("{TASK_PREFIX}{id}"))
}

fn task_namespace_path(id: &str) -> String {
  format!("{TASK_NAMESPACE_ROOT}/{id}")
}

fn task_registry_digest(registry: &[String]) -> blake3::Hash {
  let mut hasher = blake3::Hasher::new();
  for id in registry {
    hasher.update(&(id.len() as u64).to_le_bytes());
    hasher.update(id.as_bytes());
  }
  hasher.finalize()
}

fn validate_task_entry(
  expected_key: &[u8],
  header: &crate::engine::entry_header::EntryHeader,
  stored_key: &[u8],
  role: &str,
) -> EngineResult<()> {
  if header.entry_type != EntryType::FileRecord || stored_key != expected_key {
    return Err(EngineError::CorruptEntry { offset: 0, reason: format!("task {role} locator is not an exact FileRecord") });
  }
  Ok(())
}

fn load_task_registry(engine: &StorageEngine) -> EngineResult<Vec<String>> {
  let hash = task_storage_hash(TASK_REGISTRY);
  match engine.get_entry_verified(&hash)? {
    Some((header, key, value)) => {
      if header.entry_type != EntryType::FileRecord || key != hash {
        return Err(EngineError::CorruptEntry { offset: 0, reason: "task registry locator is not an exact FileRecord".to_string() });
      }
      decode_task_registry(&value)
    }
    None => Ok(Vec::new()),
  }
}

fn load_task_record(engine: &StorageEngine, id: &str) -> EngineResult<TaskRecord> {
  let hash = task_record_hash(id);
  let (header, key, value) = engine.get_entry_verified(&hash)?.ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
  validate_task_entry(&hash, &header, &key, id)?;
  decode_task_record(id, &value)
}

fn load_task_registry_with_memory(engine: &StorageEngine, memory: &mut OperationMemoryBudget) -> EngineResult<(Vec<String>, u64)> {
  let hash = task_storage_hash(TASK_REGISTRY);
  let Some(header) = engine.get_entry_header(&hash)? else {
    return Ok((Vec::new(), 0));
  };
  if header.entry_type != EntryType::FileRecord {
    return Err(EngineError::CorruptEntry { offset: 0, reason: "task registry locator is not a FileRecord".to_string() });
  }
  let charge = task_json_allocation_charge(header.value_length, "task registry")?;
  memory.reserve(charge, "task registry admission failed")?;
  let Some((read_header, key, value)) = engine.get_entry_verified_bounded(&hash, header.value_length)? else {
    return Err(EngineError::CorruptEntry { offset: 0, reason: "task registry disappeared during dequeue".to_string() });
  };
  validate_task_entry(&hash, &read_header, &key, "_registry")?;
  Ok((decode_task_registry(&value)?, charge))
}

fn load_task_with_memory(engine: &StorageEngine, id: &str, memory: &mut OperationMemoryBudget) -> EngineResult<(TaskRecord, u64)> {
  let hash = task_record_hash(id);
  let header = engine
    .get_entry_header(&hash)?
    .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: format!("task registry references missing task '{id}'") })?;
  if header.entry_type != EntryType::FileRecord {
    return Err(EngineError::CorruptEntry { offset: 0, reason: format!("task '{id}' locator is not a FileRecord") });
  }
  let charge = task_json_allocation_charge(header.value_length, "task record")?;
  memory.reserve(charge, "task record admission failed")?;
  let Some((read_header, key, value)) = engine.get_entry_verified_bounded(&hash, header.value_length)? else {
    return Err(EngineError::CorruptEntry { offset: 0, reason: format!("task registry references missing task '{id}'") });
  };
  validate_task_entry(&hash, &read_header, &key, id)?;
  Ok((decode_task_record(id, &value)?, charge))
}

fn apply_task_status(record: &mut TaskRecord, status: TaskStatus, error: Option<String>) {
  let now = chrono::Utc::now().timestamp_millis();
  match status {
    TaskStatus::Pending => {
      record.started_at = None;
      record.completed_at = None;
      record.retry_at = None;
    }
    TaskStatus::Running => {
      record.started_at = Some(now);
      record.completed_at = None;
      record.retry_at = None;
    }
    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled => {
      record.completed_at = Some(now);
      record.retry_at = None;
    }
  }
  record.status = status;
  record.error = error;
}

fn validate_task_locator_type(engine: &StorageEngine, key: &[u8], role: &str) -> EngineResult<bool> {
  let Some(entry) = engine.get_kv_entry(key)? else {
    return Ok(false);
  };
  if entry.entry_type() != EntryType::FileRecord.to_kv_type() {
    return Err(EngineError::CorruptEntry { offset: entry.offset, reason: format!("{role} locator is not a FileRecord") });
  }
  Ok(true)
}

fn append_task_replacement(engine: &StorageEngine, batch: &mut NamespaceMutationBatch, record: &TaskRecord) -> EngineResult<()> {
  let key = task_record_hash(&record.id);
  let existed = validate_task_locator_type(engine, &key, &format!("task '{}'", record.id))?;
  let bytes = serde_json::to_vec(record).map_err(|error| EngineError::InvalidInput(format!("serialization error: {error}")))?;
  batch.replace_locator(EntryType::FileRecord, key.clone(), bytes, 0)?;
  batch.add_source_identity(NamespaceMutationSourceIdentity {
    path: task_namespace_path(&record.id),
    entry_type: Some(EntryType::FileRecord.to_u8()),
    previous_identity: existed.then(|| key.clone()),
    new_identity: Some(key),
  })
}

fn append_registry_replacement(engine: &StorageEngine, batch: &mut NamespaceMutationBatch, registry: &[String]) -> EngineResult<()> {
  let key = task_storage_hash(TASK_REGISTRY);
  let existed = validate_task_locator_type(engine, &key, "task registry")?;
  let bytes = serde_json::to_vec(registry).map_err(|error| EngineError::InvalidInput(format!("serialization error: {error}")))?;
  batch.replace_locator(EntryType::FileRecord, key.clone(), bytes, 0)?;
  batch.add_source_identity(NamespaceMutationSourceIdentity {
    path: format!("{TASK_NAMESPACE_ROOT}/_registry"),
    entry_type: Some(EntryType::FileRecord.to_u8()),
    previous_identity: existed.then(|| key.clone()),
    new_identity: Some(key),
  })
}

fn task_json_allocation_charge(value_length: u32, label: &str) -> EngineResult<u64> {
  u64::from(value_length)
    .checked_mul(TASK_JSON_ALLOCATION_MULTIPLIER)
    .and_then(|bytes| bytes.checked_add(TASK_JSON_ALLOCATION_OVERHEAD))
    .ok_or_else(|| EngineError::ResourceExhausted(format!("{label} memory estimate overflow")))
}

#[cfg(test)]
#[path = "../../spec/engine/task_queue_poison_internal_spec.rs"]
mod task_queue_poison_internal_spec;
