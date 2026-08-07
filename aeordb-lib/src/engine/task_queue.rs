use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use crate::engine::operation_memory::OperationMemoryBudget;
use crate::engine::storage_engine::StorageEngine;

const TASK_PREFIX: &str = "::aeordb:task:";
const TASK_REGISTRY: &str = "::aeordb:task:_registry";
const TASK_JSON_ALLOCATION_MULTIPLIER: u64 = 3;
const TASK_JSON_ALLOCATION_OVERHEAD: u64 = 512;

fn task_storage_hash(key: &str) -> Vec<u8> {
  blake3::hash(key.as_bytes()).as_bytes().to_vec()
}

/// Recognize and validate the task queue's sanctioned reuse of the low-level
/// FileRecord tag. `None` means the row belongs to another FileRecord producer.
pub(crate) fn validate_task_storage_record(hash: &[u8], value: &[u8]) -> Option<EngineResult<()>> {
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

/// A persisted background task record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
  /// Unique task identifier (UUID v4).
  pub id: String,
  /// Task type name (e.g. `"reindex"`, `"gc"`).
  pub task_type: String,
  /// Arbitrary JSON arguments for the task.
  pub args: serde_json::Value,
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
    let mut active = self.queue.active_cancellations.write().unwrap_or_else(|error| {
      tracing::warn!("active task cancellation write lock poisoned, recovering: {}", error);
      error.into_inner()
    });
    if active.get(&self.task_id).is_some_and(|current| Arc::ptr_eq(current, &self.token)) {
      active.remove(&self.task_id);
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

  /// Compute a deterministic hash for a system-table key string.
  fn hash_key(&self, key_string: &str) -> Vec<u8> {
    task_storage_hash(key_string)
  }

  /// Create a new task with `status = Pending`, persist it, and add its ID to the registry.
  ///
  /// Returns the created [`TaskRecord`] including the generated UUID.
  pub fn enqueue(&self, task_type: &str, args: serde_json::Value) -> EngineResult<TaskRecord> {
    // Serialize the entire enqueue operation so concurrent enqueues cannot
    // interleave registry reads and writes (which would lose entries).
    let _state_guard = self.lock_state("enqueue")?;

    let id = uuid::Uuid::new_v4().to_string();
    let record = TaskRecord {
      id: id.clone(),
      task_type: task_type.to_string(),
      args,
      status: TaskStatus::Pending,
      created_at: chrono::Utc::now().timestamp_millis(),
      started_at: None,
      completed_at: None,
      error: None,
      checkpoint: None,
      retry_at: None,
      deferral_count: 0,
    };

    let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
    let json_bytes = serde_json::to_vec(&record).map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &json_bytes)?;

    // Update registry.
    let mut registry = self.load_registry()?;
    registry.push(id);
    self.save_registry(&registry)?;

    Ok(record)
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

    let (registry, registry_charge) = self.load_registry_with_memory(memory)?;
    let mut oldest: Option<TaskRecord> = None;
    let mut oldest_charge = 0u64;
    let now = chrono::Utc::now().timestamp_millis();
    for id in &registry {
      memory.record_work(1)?;
      let candidate_checkpoint = memory.checkpoint();
      let (task, task_charge) = self.load_task_with_memory(id, memory)?;
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

    // Atomically mark as Running before returning — no one else can
    // see this task as Pending while we hold the lock.
    if let Some(ref mut task) = oldest {
      let now = chrono::Utc::now().timestamp_millis();
      task.status = TaskStatus::Running;
      task.started_at = Some(now);
      task.completed_at = None;
      task.error = None;
      task.retry_at = None;
      self.save_task_unlocked(task)?;
    }

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
    let mut record = self.load_task_unlocked(id)?;
    record.checkpoint = Some(checkpoint.to_string());
    self.save_task_unlocked(&record)
  }

  /// Return a running task to the pending queue without losing its durable checkpoint.
  /// A cancellation or terminal transition that won the state lock is never overwritten.
  pub fn requeue_running(&self, id: &str) -> EngineResult<bool> {
    let _state_guard = self.lock_state("task requeue")?;
    let mut record = self.load_task_unlocked(id)?;
    if record.status != TaskStatus::Running {
      return Ok(false);
    }

    record.status = TaskStatus::Pending;
    record.started_at = None;
    record.completed_at = None;
    record.error = None;
    record.retry_at = None;
    self.save_task_unlocked(&record)?;
    Ok(true)
  }

  /// Return a running task to Pending while making it ineligible until the
  /// supplied wall-clock time. Cancellation and terminal states retain
  /// precedence because the transition is serialized by the task state lock.
  pub fn defer_running_until(&self, id: &str, retry_at: i64) -> EngineResult<Option<TaskRecord>> {
    let _state_guard = self.lock_state("task deferral")?;
    let mut record = self.load_task_unlocked(id)?;
    if record.status != TaskStatus::Running {
      return Ok(None);
    }

    let now = chrono::Utc::now().timestamp_millis();
    if retry_at <= now {
      return Err(EngineError::InvalidInput("task retry_at must be in the future".to_string()));
    }
    record.status = TaskStatus::Pending;
    record.started_at = None;
    record.completed_at = None;
    record.error = None;
    record.retry_at = Some(retry_at);
    record.deferral_count = record.deferral_count.saturating_add(1);
    self.save_task_unlocked(&record)?;
    Ok(Some(record))
  }

  /// Finish a task only while it is still running. This is the worker's
  /// compare-and-transition operation and prevents stale completion/failure
  /// writes from replacing a concurrent cancellation.
  pub fn finish_running(&self, id: &str, status: TaskStatus, error: Option<String>) -> EngineResult<bool> {
    if !matches!(status, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled) {
      return Err(EngineError::InvalidInput("finish_running requires a terminal task status".to_string()));
    }

    let _state_guard = self.lock_state("task completion")?;
    let record = self.load_task_unlocked(id)?;
    if record.status != TaskStatus::Running {
      return Ok(false);
    }
    self.update_status_unlocked(id, status, error)?;
    Ok(true)
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
      let (mut task, _task_charge) = self.load_task_with_memory(id, &mut memory)?;
      if task.status != TaskStatus::Running {
        drop(task);
        memory.release_to(record_checkpoint, "startup task recovery released inactive record")?;
        continue;
      }
      task.status = TaskStatus::Pending;
      task.started_at = None;
      task.completed_at = None;
      task.error = None;
      task.retry_at = None;
      self.save_task_unlocked(&task)?;
      recovered = recovered.saturating_add(1);
      drop(task);
      memory.release_to(record_checkpoint, "startup task recovery released recovered record")?;
    }
    Ok(recovered)
  }

  /// Load a single task by ID.
  pub fn get_task(&self, id: &str) -> EngineResult<Option<TaskRecord>> {
    let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
    match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => Ok(Some(decode_task_record(id, &value)?)),
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
      let mut cancelled = self.cancelled.write().unwrap_or_else(|e| {
        tracing::warn!("cancelled set write lock poisoned, recovering: {}", e);
        e.into_inner()
      });
      cancelled.insert(id.to_string());
    }
    let active = self.active_cancellations.read().unwrap_or_else(|error| {
      tracing::warn!("active task cancellation read lock poisoned, recovering: {}", error);
      error.into_inner()
    });
    if let Some(token) = active.get(id) {
      token.cancel();
    }
    Ok(())
  }

  /// Mark a task as cancelled in memory only (without updating persisted status).
  /// Useful for testing mid-execution cancellation detection.
  pub fn mark_cancelled_in_memory(&self, id: &str) {
    let mut cancelled = self.cancelled.write().unwrap_or_else(|e| {
      tracing::warn!("cancelled set write lock poisoned, recovering: {}", e);
      e.into_inner()
    });
    cancelled.insert(id.to_string());
    drop(cancelled);
    let active = self.active_cancellations.read().unwrap_or_else(|error| {
      tracing::warn!("active task cancellation read lock poisoned, recovering: {}", error);
      error.into_inner()
    });
    if let Some(token) = active.get(id) {
      token.cancel();
    }
  }

  /// Check if a task has been cancelled (in-memory check for speed).
  pub fn is_cancelled(&self, id: &str) -> bool {
    let cancelled = self.cancelled.read().unwrap_or_else(|e| {
      tracing::warn!("cancelled set read lock poisoned, recovering: {}", e);
      e.into_inner()
    });
    cancelled.contains(id)
  }

  pub(crate) fn register_active_cancellation<'a>(&'a self, id: &str, parent: &CancellationToken) -> ActiveTaskCancellation<'a> {
    let token = Arc::new(parent.child_token());
    {
      let mut active = self.active_cancellations.write().unwrap_or_else(|error| {
        tracing::warn!("active task cancellation write lock poisoned, recovering: {}", error);
        error.into_inner()
      });
      active.insert(id.to_string(), Arc::clone(&token));
    }
    if self.is_cancelled(id) {
      token.cancel();
    }
    ActiveTaskCancellation { queue: self, task_id: id.to_string(), token }
  }

  /// Set in-memory progress info for a task.
  pub fn set_progress(&self, id: &str, info: ProgressInfo) {
    let mut progress = self.progress.write().unwrap_or_else(|e| {
      tracing::warn!("progress map write lock poisoned, recovering: {}", e);
      e.into_inner()
    });
    progress.insert(id.to_string(), info);
  }

  /// Get in-memory progress info for a task.
  pub fn get_progress(&self, id: &str) -> Option<ProgressInfo> {
    let progress = self.progress.read().unwrap_or_else(|e| {
      tracing::warn!("progress map read lock poisoned, recovering: {}", e);
      e.into_inner()
    });
    progress.get(id).cloned()
  }

  /// Find any running reindex task whose args.path is a prefix of the given path.
  pub fn get_reindex_progress_for_path(&self, path: &str) -> Option<ProgressInfo> {
    let progress = self.progress.read().unwrap_or_else(|e| {
      tracing::warn!("progress map read lock poisoned, recovering: {}", e);
      e.into_inner()
    });
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
    let mut progress = self.progress.write().unwrap_or_else(|e| {
      tracing::warn!("progress map write lock poisoned, recovering: {}", e);
      e.into_inner()
    });
    progress.remove(id);
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

    // Delete the entries and update registry.
    let pruned = remove.iter().filter(|remove| **remove).count();
    for (index, id) in registry.iter().enumerate() {
      if !remove[index] {
        continue;
      }
      let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
      self.engine.mark_entry_deleted(&hash)?;
    }

    {
      let mut cancelled = self.cancelled.write().unwrap_or_else(|error| {
        tracing::warn!("cancelled set write lock poisoned, recovering: {}", error);
        error.into_inner()
      });
      for (index, id) in registry.iter().enumerate() {
        if remove[index] {
          cancelled.remove(id);
        }
      }
    }

    let mut index = 0usize;
    registry.retain(|_| {
      let retain = !remove[index];
      index += 1;
      retain
    });
    self.save_registry(&registry)?;

    Ok(pruned)
  }

  // -------------------------------------------------------------------------
  // Registry helpers
  // -------------------------------------------------------------------------

  fn load_registry(&self) -> EngineResult<Vec<String>> {
    let hash = self.hash_key(TASK_REGISTRY);
    match self.engine.get_entry(&hash)? {
      Some((_header, _key, value)) => decode_task_registry(&value),
      None => Ok(Vec::new()),
    }
  }

  fn load_registry_with_memory(&self, memory: &mut OperationMemoryBudget) -> EngineResult<(Vec<String>, u64)> {
    let hash = self.hash_key(TASK_REGISTRY);
    let Some(header) = self.engine.get_entry_header(&hash)? else {
      return Ok((Vec::new(), 0));
    };
    let charge = task_json_allocation_charge(header.value_length, "task registry")?;
    memory.reserve(charge, "task registry admission failed")?;
    let Some((_header, _key, value)) = self.engine.get_entry_verified_bounded(&hash, header.value_length)? else {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "task registry disappeared during dequeue".to_string() });
    };
    Ok((decode_task_registry(&value)?, charge))
  }

  fn load_task_with_memory(&self, id: &str, memory: &mut OperationMemoryBudget) -> EngineResult<(TaskRecord, u64)> {
    let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
    let header = self
      .engine
      .get_entry_header(&hash)?
      .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: format!("task registry references missing task '{id}'") })?;
    let charge = task_json_allocation_charge(header.value_length, "task record")?;
    memory.reserve(charge, "task record admission failed")?;
    let Some((_header, _key, value)) = self.engine.get_entry_verified_bounded(&hash, header.value_length)? else {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("task registry references missing task '{id}'") });
    };
    Ok((decode_task_record(id, &value)?, charge))
  }

  fn lock_state(&self, operation: &str) -> EngineResult<std::sync::MutexGuard<'_, ()>> {
    self
      .state_lock
      .lock()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("task state lock poisoned during {operation}: {error}"))))
  }

  fn load_task_unlocked(&self, id: &str) -> EngineResult<TaskRecord> {
    let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
    let entry = self.engine.get_entry(&hash)?;
    let (_header, _key, value) = entry.ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
    decode_task_record(id, &value)
  }

  fn save_task_unlocked(&self, record: &TaskRecord) -> EngineResult<()> {
    let hash = self.hash_key(&format!("{TASK_PREFIX}{}", record.id));
    let json_bytes = serde_json::to_vec(record).map_err(|error| EngineError::InvalidInput(format!("serialization error: {error}")))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &json_bytes)?;
    Ok(())
  }

  fn list_tasks_unlocked(&self) -> EngineResult<Vec<TaskRecord>> {
    let registry = self.load_registry()?;
    let mut tasks = Vec::new();
    for id in &registry {
      let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
      let Some((_header, _key, value)) = self.engine.get_entry(&hash)? else {
        return Err(EngineError::CorruptEntry { offset: 0, reason: format!("task registry references missing task '{id}'") });
      };
      tasks.push(decode_task_record(id, &value)?);
    }
    Ok(tasks)
  }

  fn update_status_unlocked(&self, id: &str, status: TaskStatus, error: Option<String>) -> EngineResult<()> {
    let mut record = self.load_task_unlocked(id)?;
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
    self.save_task_unlocked(&record)
  }

  fn save_registry(&self, registry: &[String]) -> EngineResult<()> {
    let hash = self.hash_key(TASK_REGISTRY);
    let encoded = serde_json::to_vec(registry).map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
    self.engine.store_entry(EntryType::FileRecord, &hash, &encoded)?;
    Ok(())
  }
}

fn task_json_allocation_charge(value_length: u32, label: &str) -> EngineResult<u64> {
  u64::from(value_length)
    .checked_mul(TASK_JSON_ALLOCATION_MULTIPLIER)
    .and_then(|bytes| bytes.checked_add(TASK_JSON_ALLOCATION_OVERHEAD))
    .ok_or_else(|| EngineError::ResourceExhausted(format!("{label} memory estimate overflow")))
}
