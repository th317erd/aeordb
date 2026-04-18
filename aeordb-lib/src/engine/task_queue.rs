use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::storage_engine::StorageEngine;

const TASK_PREFIX: &str = "::aeordb:task:";
const TASK_REGISTRY: &str = "::aeordb:task:_registry";

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
    /// Serializes enqueue operations so the load-registry / push / save-registry
    /// sequence is not interleaved by concurrent enqueues (which would lose entries).
    enqueue_lock: Mutex<()>,
}

impl TaskQueue {
    pub fn new(engine: Arc<StorageEngine>) -> Self {
        TaskQueue {
            engine,
            progress: Arc::new(RwLock::new(HashMap::new())),
            cancelled: Arc::new(RwLock::new(HashSet::new())),
            enqueue_lock: Mutex::new(()),
        }
    }

    /// Compute a deterministic hash for a system-table key string.
    fn hash_key(&self, key_string: &str) -> Vec<u8> {
        blake3::hash(key_string.as_bytes()).as_bytes().to_vec()
    }

    /// Create a new task with `status = Pending`, persist it, and add its ID to the registry.
    ///
    /// Returns the created [`TaskRecord`] including the generated UUID.
    pub fn enqueue(&self, task_type: &str, args: serde_json::Value) -> EngineResult<TaskRecord> {
        // Serialize the entire enqueue operation so concurrent enqueues cannot
        // interleave registry reads and writes (which would lose entries).
        let _enqueue_guard = self.enqueue_lock.lock().map_err(|e| {
            EngineError::IoError(std::io::Error::other(
                format!("enqueue lock poisoned: {}", e),
            ))
        })?;

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
        };

        let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
        let json_bytes = serde_json::to_vec(&record)
            .map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
        self.engine.store_entry(EntryType::FileRecord, &hash, &json_bytes)?;

        // Update registry.
        let mut registry = self.load_registry()?;
        registry.push(id);
        self.save_registry(&registry)?;

        Ok(record)
    }

    /// Load all tasks and return the oldest pending one (FIFO order).
    pub fn dequeue_next(&self) -> EngineResult<Option<TaskRecord>> {
        let tasks = self.list_tasks()?;
        let mut oldest: Option<TaskRecord> = None;
        for task in tasks {
            if task.status == TaskStatus::Pending {
                match &oldest {
                    None => oldest = Some(task),
                    Some(current) => {
                        if task.created_at < current.created_at {
                            oldest = Some(task);
                        }
                    }
                }
            }
        }
        Ok(oldest)
    }

    /// Update a task's status and set started_at/completed_at timestamps as appropriate.
    pub fn update_status(
        &self,
        id: &str,
        status: TaskStatus,
        error: Option<String>,
    ) -> EngineResult<()> {
        let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
        let entry = self.engine.get_entry(&hash)?;
        let (_header, _key, value) = entry.ok_or_else(|| {
            EngineError::NotFound(format!("task {id}"))
        })?;

        let mut record: TaskRecord = serde_json::from_slice(&value)
            .map_err(|e| EngineError::InvalidInput(format!("deserialization error: {e}")))?;

        let now = chrono::Utc::now().timestamp_millis();
        match status {
            TaskStatus::Running => {
                record.started_at = Some(now);
            }
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled => {
                record.completed_at = Some(now);
            }
            TaskStatus::Pending => {}
        }

        record.status = status;
        record.error = error;

        let json_bytes = serde_json::to_vec(&record)
            .map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
        self.engine.store_entry(EntryType::FileRecord, &hash, &json_bytes)?;

        Ok(())
    }

    /// Update the checkpoint field on a task.
    pub fn update_checkpoint(&self, id: &str, checkpoint: &str) -> EngineResult<()> {
        let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
        let entry = self.engine.get_entry(&hash)?;
        let (_header, _key, value) = entry.ok_or_else(|| {
            EngineError::NotFound(format!("task {id}"))
        })?;

        let mut record: TaskRecord = serde_json::from_slice(&value)
            .map_err(|e| EngineError::InvalidInput(format!("deserialization error: {e}")))?;

        record.checkpoint = Some(checkpoint.to_string());

        let json_bytes = serde_json::to_vec(&record)
            .map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
        self.engine.store_entry(EntryType::FileRecord, &hash, &json_bytes)?;

        Ok(())
    }

    /// Load a single task by ID.
    pub fn get_task(&self, id: &str) -> EngineResult<Option<TaskRecord>> {
        let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
        match self.engine.get_entry(&hash)? {
            Some((_header, _key, value)) => {
                let record: TaskRecord = serde_json::from_slice(&value)
                    .map_err(|e| EngineError::InvalidInput(format!("deserialization error: {e}")))?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    /// Load all tasks from the registry.
    pub fn list_tasks(&self) -> EngineResult<Vec<TaskRecord>> {
        let registry = self.load_registry()?;
        let mut tasks = Vec::new();
        for id in &registry {
            let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
            if let Some((_header, _key, value)) = self.engine.get_entry(&hash)? {
                let record: TaskRecord = serde_json::from_slice(&value)
                    .map_err(|e| EngineError::InvalidInput(format!("deserialization error: {e}")))?;
                tasks.push(record);
            }
        }
        Ok(tasks)
    }

    /// Cancel a task: mark it as cancelled both in memory and on disk.
    pub fn cancel(&self, id: &str) -> EngineResult<()> {
        {
            let mut cancelled = self.cancelled.write().unwrap_or_else(|e| {
                tracing::warn!("cancelled set write lock poisoned, recovering: {}", e);
                e.into_inner()
            });
            cancelled.insert(id.to_string());
        }
        self.update_status(id, TaskStatus::Cancelled, None)
    }

    /// Mark a task as cancelled in memory only (without updating persisted status).
    /// Useful for testing mid-execution cancellation detection.
    pub fn mark_cancelled_in_memory(&self, id: &str) {
        let mut cancelled = self.cancelled.write().unwrap_or_else(|e| {
            tracing::warn!("cancelled set write lock poisoned, recovering: {}", e);
            e.into_inner()
        });
        cancelled.insert(id.to_string());
    }

    /// Check if a task has been cancelled (in-memory check for speed).
    pub fn is_cancelled(&self, id: &str) -> bool {
        let cancelled = self.cancelled.read().unwrap_or_else(|e| {
            tracing::warn!("cancelled set read lock poisoned, recovering: {}", e);
            e.into_inner()
        });
        cancelled.contains(id)
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
        let now = chrono::Utc::now().timestamp_millis();
        let mut registry = self.load_registry()?;
        let mut tasks: Vec<TaskRecord> = Vec::new();

        // Load all tasks.
        for id in &registry {
            let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
            if let Some((_header, _key, value)) = self.engine.get_entry(&hash)? {
                let record: TaskRecord = serde_json::from_slice(&value)
                    .map_err(|e| EngineError::InvalidInput(format!("deserialization error: {e}")))?;
                tasks.push(record);
            }
        }

        // Identify terminal tasks (completed/failed/cancelled).
        let mut terminal: Vec<&TaskRecord> = tasks.iter().filter(|t| {
            matches!(t.status, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled)
        }).collect();

        // Sort by completed_at descending (newest first) so we keep the newest ones.
        terminal.sort_by(|a, b| {
            let a_time = a.completed_at.unwrap_or(a.created_at);
            let b_time = b.completed_at.unwrap_or(b.created_at);
            b_time.cmp(&a_time)
        });

        let mut to_remove: HashSet<String> = HashSet::new();

        // Remove by age.
        for task in &terminal {
            let task_time = task.completed_at.unwrap_or(task.created_at);
            if now - task_time > max_age_ms {
                to_remove.insert(task.id.clone());
            }
        }

        // Remove by count: if more than max_count terminal tasks remain after age pruning,
        // remove the oldest ones.
        let remaining: Vec<&&TaskRecord> = terminal.iter()
            .filter(|t| !to_remove.contains(&t.id))
            .collect();
        if remaining.len() > max_count {
            for task in remaining.iter().skip(max_count) {
                to_remove.insert(task.id.clone());
            }
        }

        // Delete the entries and update registry.
        let pruned = to_remove.len();
        for id in &to_remove {
            let hash = self.hash_key(&format!("{TASK_PREFIX}{id}"));
            // Overwrite with empty to effectively delete (or just remove from registry).
            let _ = self.engine.mark_entry_deleted(&hash);
        }

        registry.retain(|id| !to_remove.contains(id));
        self.save_registry(&registry)?;

        Ok(pruned)
    }

    // -------------------------------------------------------------------------
    // Registry helpers
    // -------------------------------------------------------------------------

    fn load_registry(&self) -> EngineResult<Vec<String>> {
        let hash = self.hash_key(TASK_REGISTRY);
        match self.engine.get_entry(&hash)? {
            Some((_header, _key, value)) => {
                let registry: Vec<String> = serde_json::from_slice(&value)
                    .map_err(|e| EngineError::InvalidInput(format!("deserialization error: {e}")))?;
                Ok(registry)
            }
            None => Ok(Vec::new()),
        }
    }

    fn save_registry(&self, registry: &[String]) -> EngineResult<()> {
        let hash = self.hash_key(TASK_REGISTRY);
        let encoded = serde_json::to_vec(registry)
            .map_err(|e| EngineError::InvalidInput(format!("serialization error: {e}")))?;
        self.engine.store_entry(EntryType::FileRecord, &hash, &encoded)?;
        Ok(())
    }
}
