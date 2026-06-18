use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};

use fs2::FileExt;

use arc_swap::ArcSwap;

use crate::engine::append_writer::AppendWriter;
use crate::engine::cache::Cache;
use crate::engine::cache_loaders::{PermissionsLoader, IndexConfigLoader};
use crate::engine::compression::CompressionAlgorithm;
use crate::engine::disk_kv_store::DiskKVStore;
use crate::engine::engine_counters::EngineCounters;
use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::hot_tail::VoidRecord;
use crate::engine::index_store::{IndexManager, SharedIndexWriteBuffer};
use crate::engine::kv_snapshot::ReadSnapshot;
use serde::Serialize;

use crate::engine::kv_store::{KVEntry, KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY, KV_TYPE_SNAPSHOT, KV_TYPE_FORK, KV_FLAG_DELETED};
use crate::engine::void_manager::VoidManager;

/// A buffered batch of entries to write in one sequential operation.
///
/// Accumulates entries in memory and flushes them all with a single lock
/// acquisition via [`StorageEngine::flush_batch`]. This avoids per-entry
/// lock overhead when writing many entries at once.
pub struct WriteBatch {
  entries: Vec<BatchEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkEntryMetadata {
  pub stored_value_length: u64,
  pub raw_value_length: Option<u64>,
  pub compression_algo: CompressionAlgorithm,
}

#[derive(Debug, Clone)]
pub struct EngineStartupProgress {
  pub phase: String,
  pub message: String,
  pub current: u64,
  pub total: Option<u64>,
  /// Phase-local progress, where 0.0 is just started and 1.0 is complete.
  pub progress: Option<f64>,
  pub eta_seconds: Option<u64>,
}

pub type EngineStartupProgressCallback = Arc<dyn Fn(EngineStartupProgress) + Send + Sync + 'static>;

#[derive(Debug, Clone, Serialize)]
pub struct EmergencySpillReport {
  pub attempted_at: String,
  pub context: String,
  pub failure: String,
  pub succeeded: bool,
  pub spill_directory: Option<String>,
  pub manifest_path: Option<String>,
  pub hot_tail_path: Option<String>,
  pub wal_tail_path: Option<String>,
  pub index_buffer_path: Option<String>,
  pub db_path: Option<String>,
  pub hot_tail_writes: usize,
  pub hot_tail_voids: usize,
  pub index_pending_mutations: usize,
  pub index_dirty_saves: usize,
  pub index_deletes: usize,
  pub wal_tail_original_start: Option<u64>,
  pub wal_tail_copy_start: Option<u64>,
  pub wal_tail_end: Option<u64>,
  pub wal_tail_bytes: u64,
  pub wal_tail_truncated: bool,
  pub errors: Vec<String>,
}

struct BatchEntry {
  entry_type: EntryType,
  key: Vec<u8>,
  value: Vec<u8>,
  kv_type: u8,
}

impl Default for WriteBatch {
  fn default() -> Self {
    Self::new()
  }
}

impl WriteBatch {
  pub fn new() -> Self {
    WriteBatch { entries: Vec::new() }
  }

  /// Add an entry to the batch.
  pub fn add(&mut self, entry_type: EntryType, key: Vec<u8>, value: Vec<u8>) {
    self.entries.push(BatchEntry { entry_type, key, value, kv_type: entry_type.to_kv_type() });
  }

  /// Number of entries in the batch.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }
}

/// Result type for entry retrieval: (header, key, value).
pub type EntryData = (EntryHeader, Vec<u8>, Vec<u8>);

fn estimate_remaining_seconds(elapsed: std::time::Duration, current: u64, total: u64) -> Option<u64> {
  if total == 0 {
    return None;
  }
  if current >= total {
    return Some(0);
  }
  if current == 0 {
    return None;
  }
  let elapsed_secs = elapsed.as_secs_f64();
  if elapsed_secs <= 0.0 {
    return None;
  }
  let bytes_per_second = current as f64 / elapsed_secs;
  if bytes_per_second <= 0.0 {
    return None;
  }
  let remaining = total.saturating_sub(current) as f64;
  Some((remaining / bytes_per_second).ceil() as u64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RebuildOrder {
  timestamp: i64,
  offset: u64,
}

impl RebuildOrder {
  fn is_after(self, other: Self) -> bool {
    (self.timestamp, self.offset) > (other.timestamp, other.offset)
  }
}

#[derive(Debug, Clone)]
struct RebuildKvRecord {
  type_flags: u8,
  hash: Vec<u8>,
  offset: u64,
  value_length: u32,
  total_length: u32,
  order: RebuildOrder,
}

#[derive(Debug, Clone)]
pub struct EngineOperationSnapshot {
  pub shutting_down: bool,
  pub active_operations: usize,
  pub operations: Vec<(String, usize)>,
}

#[derive(Default)]
struct EngineOperationTracker {
  state: Mutex<EngineOperationState>,
  idle: Condvar,
}

#[derive(Default)]
struct EngineOperationState {
  shutting_down: bool,
  active_operations: usize,
  operations: HashMap<&'static str, usize>,
}

struct EngineOperationGuard<'a> {
  tracker: &'a EngineOperationTracker,
  operation: &'static str,
  engine_id: usize,
  counted: bool,
}

impl EngineOperationTracker {
  fn begin(&self, engine_id: usize, operation: &'static str) -> EngineResult<EngineOperationGuard<'_>> {
    let nested = ENGINE_OPERATION_STACK.with(|stack| stack.borrow().iter().any(|held| *held == engine_id));
    ENGINE_OPERATION_STACK.with(|stack| stack.borrow_mut().push(engine_id));
    if nested {
      return Ok(EngineOperationGuard { tracker: self, operation, engine_id, counted: false });
    }

    let mut state = match self.state.lock() {
      Ok(state) => state,
      Err(error) => {
        ENGINE_OPERATION_STACK.with(|stack| {
          let mut stack = stack.borrow_mut();
          let popped = stack.pop();
          debug_assert_eq!(popped, Some(engine_id));
        });
        return Err(EngineError::IoError(std::io::Error::other(error.to_string())));
      }
    };
    if state.shutting_down {
      ENGINE_OPERATION_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        let popped = stack.pop();
        debug_assert_eq!(popped, Some(engine_id));
      });
      return Err(EngineError::ShuttingDown);
    }
    state.active_operations += 1;
    *state.operations.entry(operation).or_insert(0) += 1;
    Ok(EngineOperationGuard { tracker: self, operation, engine_id, counted: true })
  }

  fn begin_shutdown(&self) {
    if let Ok(mut state) = self.state.lock() {
      state.shutting_down = true;
      if state.active_operations == 0 {
        self.idle.notify_all();
      }
    }
  }

  fn snapshot(&self) -> EngineOperationSnapshot {
    let Ok(state) = self.state.lock() else {
      return EngineOperationSnapshot { shutting_down: true, active_operations: 0, operations: Vec::new() };
    };
    let mut operations: Vec<(String, usize)> = state.operations.iter().map(|(name, count)| ((*name).to_string(), *count)).collect();
    operations.sort_by(|a, b| a.0.cmp(&b.0));
    EngineOperationSnapshot { shutting_down: state.shutting_down, active_operations: state.active_operations, operations }
  }

  fn wait_until_idle(&self, timeout: std::time::Duration) -> EngineOperationSnapshot {
    let deadline = std::time::Instant::now() + timeout;
    let mut state = match self.state.lock() {
      Ok(state) => state,
      Err(_) => return self.snapshot(),
    };
    while state.active_operations > 0 {
      let now = std::time::Instant::now();
      if now >= deadline {
        break;
      }
      let remaining = deadline.saturating_duration_since(now);
      match self.idle.wait_timeout(state, remaining) {
        Ok((next_state, result)) => {
          state = next_state;
          if result.timed_out() {
            break;
          }
        }
        Err(_) => return self.snapshot(),
      }
    }
    let mut operations: Vec<(String, usize)> = state.operations.iter().map(|(name, count)| ((*name).to_string(), *count)).collect();
    operations.sort_by(|a, b| a.0.cmp(&b.0));
    EngineOperationSnapshot { shutting_down: state.shutting_down, active_operations: state.active_operations, operations }
  }
}

impl Drop for EngineOperationGuard<'_> {
  fn drop(&mut self) {
    ENGINE_OPERATION_STACK.with(|stack| {
      let mut stack = stack.borrow_mut();
      let popped = stack.pop();
      debug_assert_eq!(popped, Some(self.engine_id));
    });

    if !self.counted {
      return;
    }

    let Ok(mut state) = self.tracker.state.lock() else {
      return;
    };
    state.active_operations = state.active_operations.saturating_sub(1);
    if let Some(count) = state.operations.get_mut(self.operation) {
      *count = count.saturating_sub(1);
      if *count == 0 {
        state.operations.remove(self.operation);
      }
    }
    if state.active_operations == 0 {
      self.tracker.idle.notify_all();
    }
  }
}

impl RebuildKvRecord {
  fn to_kv_entry(&self) -> KVEntry {
    KVEntry { type_flags: self.type_flags, hash: self.hash.clone(), offset: self.offset, total_length: self.total_length }
  }
}

/// Aggregate statistics about the database, returned by [`StorageEngine::stats`].
#[derive(Debug, Clone, Serialize)]
pub struct DatabaseStats {
  /// Total number of entries ever appended to the WAL (from the file header).
  pub entry_count: u64,
  /// Number of live entries in the KV index.
  pub kv_entries: usize,
  /// Size of the `.kv` sidecar file in bytes.
  pub kv_size_bytes: u64,
  /// Number of NVT hash-table buckets.
  pub nvt_buckets: usize,
  /// Reserved; currently always 0.
  pub nvt_size_bytes: u64,
  /// Number of stored data chunks.
  pub chunk_count: usize,
  /// Number of stored file records.
  pub file_count: usize,
  /// Number of stored directory entries.
  pub directory_count: usize,
  /// Number of named snapshots.
  pub snapshot_count: usize,
  /// Number of named forks.
  pub fork_count: usize,
  /// Number of reclaimable void entries.
  pub void_count: usize,
  /// Total bytes occupied by void entries.
  pub void_space_bytes: u64,
  /// Size of the main `.aeordb` file in bytes.
  pub db_file_size_bytes: u64,
  /// Database creation timestamp (ms since epoch).
  pub created_at: i64,
  /// Last-modified timestamp (ms since epoch).
  pub updated_at: i64,
  /// Hash algorithm name (e.g. `"Blake3_256"`).
  pub hash_algorithm: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct EngineMemoryStats {
  pub process: ProcessMemoryStats,
  pub index_cache: IndexCacheMemoryStats,
  pub directory_cache: DirectoryCacheMemoryStats,
  pub caches: EngineCacheMemoryStats,
  pub estimated_engine_owned_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ProcessMemoryStats {
  pub rss_bytes: u64,
  pub peak_rss_bytes: u64,
  pub virtual_bytes: u64,
  pub data_bytes: u64,
  pub swap_bytes: u64,
  pub thread_count: u64,
  pub fd_count: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct IndexCacheMemoryStats {
  pub cached_indexes: usize,
  pub dirty_indexes: usize,
  pub deleted_indexes: usize,
  pub pending_mutations: usize,
  pub total_mutations: usize,
  pub flushes: usize,
  pub flushed_indexes: usize,
  pub entries: usize,
  pub values: usize,
  pub estimated_bytes: u64,
  pub top_cached_indexes: Vec<crate::engine::index_store::CachedIndexMemoryStats>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DirectoryCacheMemoryStats {
  pub entries: usize,
  pub estimated_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct EngineCacheMemoryStats {
  pub permissions_entries: usize,
  pub index_config_entries: usize,
  pub grants_index_entries: usize,
}

/// Top-level storage engine combining an append-only WAL, a disk-backed KV index,
/// and a void manager for reclaimable space tracking.
///
/// `StorageEngine` is the foundation of AeorDB. It stores content-addressed
/// entries on disk and indexes them in a memory-mapped KV store for O(1) lookups.
/// Higher-level operations (file CRUD, directories, queries) are built on top
/// via [`DirectoryOps`](crate::engine::directory_ops::DirectoryOps) and
/// [`QueryEngine`](crate::engine::query_engine::QueryEngine).
///
/// Lock-free snapshot reads allow concurrent readers while a single writer
/// appends new entries.
pub struct StorageEngine {
  operation_tracker: EngineOperationTracker,
  shutdown_started: AtomicBool,
  shutdown_flush_started: AtomicBool,
  shutdown_complete: AtomicBool,
  durability_failure: Mutex<Option<String>>,
  emergency_spill: Mutex<Option<EmergencySpillReport>>,
  last_published_hot_tail_offset: AtomicU64,
  namespace_write_lock: Mutex<()>,
  writer: RwLock<AppendWriter>,
  kv_writer: Mutex<DiskKVStore>,
  pub(crate) kv_snapshot: Arc<ArcSwap<ReadSnapshot>>,
  // The VoidManager tracks reclaimable WAL space that can be reused by new
  // writes. Every void must remain outside the file header, KV block, and hot
  // tail; violating that invariant can overwrite storage metadata.
  #[allow(dead_code)]
  pub(crate) void_manager: RwLock<VoidManager>,
  hash_algo: HashAlgorithm,
  /// Atomic counters for O(1) database statistics, maintained in-memory.
  counters: ArcSwap<EngineCounters>,
  /// Advisory file lock on the database file. Held for the lifetime of the
  /// engine to prevent multiple processes from opening the same file
  /// simultaneously, which would cause corruption (in-process RwLock does
  /// not protect across process boundaries).
  /// Separate rate-limit lanes for auto-snapshots. Each lane has its own
  /// throttle so delete/restore/manual operations don't block each other.
  pub permissions_cache: Arc<Cache<PermissionsLoader>>,
  pub index_config_cache: Arc<Cache<IndexConfigLoader>>,
  pub grants_index_cache: Arc<Cache<crate::engine::grants_index::GrantsIndexLoader>>,
  pub(crate) last_auto_snapshot_delete: std::sync::atomic::AtomicI64,
  pub(crate) last_auto_snapshot_restore: std::sync::atomic::AtomicI64,
  pub(crate) last_manual_snapshot: std::sync::atomic::AtomicI64,
  /// Cache of directory content keyed by content hash. Content-addressed data
  /// is immutable, so this cache can never serve stale data for a given key.
  /// Populated by update_parent_directories, read by directory lookups.
  pub(crate) dir_content_cache: RwLock<HashMap<Vec<u8>, Vec<u8>>>,
  /// Shared in-memory index write buffer. All index mutations pass through
  /// this state and are flushed to disk by write-count/time policy.
  pub(crate) index_write_buffer: Mutex<SharedIndexWriteBuffer>,
  /// GC recheck queue. While GC mark+sweep runs, every successful write hash
  /// is added here so the sweep phase can avoid clobbering entries that were
  /// written after the mark snapshot was captured. `None` means GC is not
  /// active and writes don't bother recording. See bot-docs/plan/gc-mark-sweep.md.
  pub(crate) gc_recheck: Mutex<Option<HashSet<Vec<u8>>>>,
  #[allow(dead_code)]
  _file_lock: std::fs::File,
}

impl StorageEngine {
  fn operation_guard(&self, operation: &'static str) -> EngineResult<EngineOperationGuard<'_>> {
    let engine_id = self as *const StorageEngine as usize;
    self.operation_tracker.begin(engine_id, operation)
  }

  fn internal_operation_scope(&self, operation: &'static str) -> EngineOperationGuard<'_> {
    let engine_id = self as *const StorageEngine as usize;
    ENGINE_OPERATION_STACK.with(|stack| stack.borrow_mut().push(engine_id));
    EngineOperationGuard { tracker: &self.operation_tracker, operation, engine_id, counted: false }
  }

  /// Stop accepting new top-level engine operations. Existing operations are
  /// allowed to finish so shutdown can avoid closing under an active DB read
  /// or write.
  pub fn begin_shutdown(&self) {
    self.operation_tracker.begin_shutdown();
  }

  pub fn durability_failure(&self) -> Option<String> {
    self.durability_failure.lock().ok().and_then(|failure| failure.clone())
  }

  pub fn emergency_spill_report(&self) -> Option<EmergencySpillReport> {
    self.emergency_spill.lock().ok().and_then(|spill| spill.clone())
  }

  pub(crate) fn ensure_writable(&self) -> EngineResult<()> {
    if let Some(failure) = self.durability_failure() {
      return Err(EngineError::DurabilityFailure(format!("database is read-only after serious durability failure: {}", failure)));
    }
    Ok(())
  }

  fn record_durability_failure(&self, context: &str, error: impl std::fmt::Display) -> EngineError {
    let message = format!("{}: {}", context, error);
    let mut first_failure = false;
    if let Ok(mut failure) = self.durability_failure.lock() {
      if failure.is_none() {
        *failure = Some(message.clone());
        first_failure = true;
      }
    }
    if first_failure {
      let spill = self.attempt_emergency_spill(context, &message);
      if let Ok(mut slot) = self.emergency_spill.lock() {
        *slot = Some(spill.clone());
      }
      if spill.succeeded {
        tracing::error!(
          context,
          error = %message,
          spill_directory = ?spill.spill_directory,
          wal_tail_bytes = spill.wal_tail_bytes,
          hot_tail_writes = spill.hot_tail_writes,
          hot_tail_voids = spill.hot_tail_voids,
          "Critical durability failure; database latched read-only and emergency spill created"
        );
      } else {
        tracing::error!(
          context,
          error = %message,
          spill_errors = ?spill.errors,
          "Critical durability failure; database latched read-only and emergency spill failed"
        );
      }
    } else {
      tracing::error!(context, error = %message, "Critical durability failure");
    }
    EngineError::DurabilityFailure(message)
  }

  fn attempt_emergency_spill(&self, context: &str, failure: &str) -> EmergencySpillReport {
    let attempted_at = chrono::Utc::now().to_rfc3339();
    let mut errors = Vec::new();
    let mut db_path: Option<PathBuf> = None;
    let mut wal_tail_original_start = None;
    let mut wal_tail_end = None;

    let current_offset = match self.writer.try_read() {
      Ok(writer) => {
        db_path = Some(writer.file_path().to_path_buf());
        let header = writer.file_header();
        let wal_start = header.kv_block_offset.saturating_add(header.kv_block_length);
        let current_offset = writer.current_offset();
        let boundary = self.last_published_hot_tail_offset.load(Ordering::Acquire);
        wal_tail_original_start = Some(boundary.max(wal_start).min(current_offset));
        wal_tail_end = Some(current_offset);
        current_offset
      }
      Err(error) => {
        errors.push(format!("writer lock unavailable for emergency spill: {}", error));
        0
      }
    };

    let payload = match self.kv_writer.try_lock() {
      Ok(mut kv) => kv.emergency_hot_tail_payload(),
      Err(error) => {
        errors.push(format!("KV lock unavailable for emergency spill: {}", error));
        crate::engine::hot_tail::HotTailPayload::default()
      }
    };

    let index_snapshot = match self.index_write_buffer.try_lock() {
      Ok(mut buffer) => match buffer.emergency_snapshot_json(self.hash_algo.hash_length()) {
        Ok(snapshot) => Some(snapshot),
        Err(error) => {
          errors.push(format!("failed to snapshot buffered indexes for emergency spill: {}", error));
          None
        }
      },
      Err(error) => {
        errors.push(format!("index buffer lock unavailable for emergency spill: {}", error));
        None
      }
    };
    let index_pending_mutations =
      index_snapshot.as_ref().and_then(|snapshot| snapshot.get("pending_mutations")).and_then(|value| value.as_u64()).unwrap_or(0) as usize;
    let index_dirty_saves = index_snapshot
      .as_ref()
      .and_then(|snapshot| snapshot.get("dirty_saves"))
      .and_then(|value| value.as_array())
      .map(|values| values.len())
      .unwrap_or(0);
    let index_deletes = index_snapshot
      .as_ref()
      .and_then(|snapshot| snapshot.get("deletes"))
      .and_then(|value| value.as_array())
      .map(|values| values.len())
      .unwrap_or(0);

    let mut report = EmergencySpillReport {
      attempted_at,
      context: context.to_string(),
      failure: failure.to_string(),
      succeeded: false,
      spill_directory: None,
      manifest_path: None,
      hot_tail_path: None,
      wal_tail_path: None,
      index_buffer_path: None,
      db_path: db_path.as_ref().map(|path| path.display().to_string()),
      hot_tail_writes: payload.writes.len(),
      hot_tail_voids: payload.voids.len(),
      index_pending_mutations,
      index_dirty_saves,
      index_deletes,
      wal_tail_original_start,
      wal_tail_copy_start: None,
      wal_tail_end,
      wal_tail_bytes: 0,
      wal_tail_truncated: false,
      errors,
    };

    let db_label = db_path
      .as_ref()
      .and_then(|path| path.file_name())
      .map(|name| name.to_string_lossy().to_string())
      .unwrap_or_else(|| "unknown-db".to_string());
    let dir_name =
      format!("{}-{}-{}", Self::sanitize_spill_component(&db_label), chrono::Utc::now().timestamp_millis(), std::process::id());

    for base_dir in crate::engine::emergency_spill::emergency_spill_base_dirs() {
      let spill_dir = base_dir.join(&dir_name);
      if let Err(error) = std::fs::create_dir_all(&spill_dir) {
        report.errors.push(format!("failed to create spill directory {}: {}", spill_dir.display(), error));
        continue;
      }
      if let Err(error) = crate::engine::durability::sync_parent_dir(&spill_dir) {
        report.errors.push(format!("failed to sync spill parent for {}: {}", spill_dir.display(), error));
      }

      match self.write_emergency_spill_files(&spill_dir, db_path.as_deref(), current_offset, &payload, index_snapshot.as_ref(), &mut report)
      {
        Ok(()) => {
          report.succeeded = true;
          return report;
        }
        Err(error) => {
          report.errors.push(format!("failed to write emergency spill in {}: {}", spill_dir.display(), error));
        }
      }
    }

    report
  }

  fn write_emergency_spill_files(
    &self,
    spill_dir: &Path,
    db_path: Option<&Path>,
    current_offset: u64,
    payload: &crate::engine::hot_tail::HotTailPayload,
    index_snapshot: Option<&serde_json::Value>,
    report: &mut EmergencySpillReport,
  ) -> Result<(), String> {
    let hot_tail_path = spill_dir.join("hot-tail.bin");
    let hot_tail_bytes = crate::engine::hot_tail::serialize_hot_tail(payload, self.hash_algo.hash_length());
    Self::write_durable_file(&hot_tail_path, &hot_tail_bytes)?;
    report.hot_tail_path = Some(hot_tail_path.display().to_string());

    if let Some(snapshot) = index_snapshot {
      if report.index_pending_mutations > 0 || report.index_dirty_saves > 0 || report.index_deletes > 0 {
        let index_buffer_path = spill_dir.join("index-buffer.json");
        let index_bytes = serde_json::to_vec_pretty(snapshot).map_err(|error| error.to_string())?;
        Self::write_durable_file(&index_buffer_path, &index_bytes)?;
        report.index_buffer_path = Some(index_buffer_path.display().to_string());
      }
    }

    if let (Some(source), Some(original_start), Some(end)) = (db_path, report.wal_tail_original_start, report.wal_tail_end) {
      if end > original_start {
        let wal_tail_path = spill_dir.join("wal-tail.bin");
        match Self::copy_wal_tail_to_file(source, &wal_tail_path, original_start, end) {
          Ok((copy_start, copied, truncated)) => {
            report.wal_tail_path = Some(wal_tail_path.display().to_string());
            report.wal_tail_copy_start = Some(copy_start);
            report.wal_tail_bytes = copied;
            report.wal_tail_truncated = truncated;
          }
          Err(error) => {
            report.errors.push(format!("failed to copy WAL tail from {} at {}..{}: {}", source.display(), original_start, end, error));
          }
        }
      } else {
        report.wal_tail_copy_start = Some(current_offset);
      }
    }

    report.spill_directory = Some(spill_dir.display().to_string());
    let manifest_path = spill_dir.join("manifest.json");
    let manifest = serde_json::json!({
      "format": "aeordb-emergency-spill-v1",
      "attempted_at": &report.attempted_at,
      "pid": std::process::id(),
      "context": &report.context,
      "failure": &report.failure,
      "db_path": &report.db_path,
      "hash_algorithm": format!("{:?}", self.hash_algo),
      "hot_tail_path": &report.hot_tail_path,
      "hot_tail_writes": report.hot_tail_writes,
      "hot_tail_voids": report.hot_tail_voids,
      "index_buffer_path": &report.index_buffer_path,
      "index_pending_mutations": report.index_pending_mutations,
      "index_dirty_saves": report.index_dirty_saves,
      "index_deletes": report.index_deletes,
      "wal_tail_path": &report.wal_tail_path,
      "wal_tail_original_start": report.wal_tail_original_start,
      "wal_tail_copy_start": report.wal_tail_copy_start,
      "wal_tail_end": report.wal_tail_end,
      "wal_tail_bytes": report.wal_tail_bytes,
      "wal_tail_truncated": report.wal_tail_truncated,
      "wal_tail_max_bytes": Self::emergency_wal_spill_max_bytes(),
      "notes": [
        "Best-effort emergency preservation after a serious durability failure.",
        "WAL bytes are copied from the filesystem view available to this process; OS/page-cache state may still determine what was recoverable."
      ],
      "errors": &report.errors,
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    Self::write_durable_file(&manifest_path, &manifest_bytes)?;
    report.manifest_path = Some(manifest_path.display().to_string());
    Ok(())
  }

  fn write_durable_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = std::fs::File::create(path).map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    crate::engine::durability::sync_parent_dir(path).map_err(|error| error.to_string())
  }

  fn copy_wal_tail_to_file(source: &Path, destination: &Path, start: u64, end: u64) -> Result<(u64, u64, bool), String> {
    if end <= start {
      Self::write_durable_file(destination, &[])?;
      return Ok((start, 0, false));
    }

    let range = end - start;
    let max_bytes = Self::emergency_wal_spill_max_bytes();
    let (copy_start, truncated) = if max_bytes > 0 && range > max_bytes { (end - max_bytes, true) } else { (start, false) };

    let mut input = std::fs::File::open(source).map_err(|error| error.to_string())?;
    let mut output = std::fs::File::create(destination).map_err(|error| error.to_string())?;
    input.seek(SeekFrom::Start(copy_start)).map_err(|error| error.to_string())?;

    let mut remaining = end - copy_start;
    let mut copied = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    while remaining > 0 {
      let read_len = remaining.min(buffer.len() as u64) as usize;
      input.read_exact(&mut buffer[..read_len]).map_err(|error| error.to_string())?;
      output.write_all(&buffer[..read_len]).map_err(|error| error.to_string())?;
      copied += read_len as u64;
      remaining -= read_len as u64;
    }
    output.sync_all().map_err(|error| error.to_string())?;
    crate::engine::durability::sync_parent_dir(destination).map_err(|error| error.to_string())?;
    Ok((copy_start, copied, truncated))
  }

  fn emergency_wal_spill_max_bytes() -> u64 {
    std::env::var("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES").ok().and_then(|value| value.parse::<u64>().ok()).unwrap_or(4 * 1024 * 1024 * 1024)
  }

  fn sanitize_spill_component(component: &str) -> String {
    let sanitized: String =
      component.chars().map(|ch| if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' { ch } else { '_' }).collect();
    if sanitized.is_empty() {
      "unknown".to_string()
    } else {
      sanitized
    }
  }

  /// Wait for currently active top-level engine operations to drain.
  pub fn wait_for_active_operations(&self, timeout: std::time::Duration) -> EngineOperationSnapshot {
    self.operation_tracker.wait_until_idle(timeout)
  }

  pub fn active_operations_snapshot(&self) -> EngineOperationSnapshot {
    self.operation_tracker.snapshot()
  }

  fn shutdown_operation_wait_timeout() -> std::time::Duration {
    let seconds = std::env::var("AEORDB_SHUTDOWN_OPERATION_WAIT_SECS").ok().and_then(|value| value.parse::<u64>().ok()).unwrap_or(600);
    std::time::Duration::from_secs(seconds)
  }

  pub(crate) fn valid_reusable_range(offset: u64, size: u32, wal_start: u64, wal_end: u64) -> bool {
    if size == 0 || wal_end < wal_start || offset < wal_start {
      return false;
    }
    let Some(end) = offset.checked_add(size as u64) else {
      return false;
    };
    end <= wal_end
  }

  fn writer_wal_bounds(writer: &AppendWriter) -> (u64, u64) {
    let header = writer.file_header();
    let wal_start = header.kv_block_offset.saturating_add(header.kv_block_length);
    let wal_end = writer.current_offset();
    (wal_start, wal_end)
  }

  pub(crate) fn is_current_reusable_range(&self, offset: u64, size: u32) -> EngineResult<bool> {
    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let (wal_start, wal_end) = Self::writer_wal_bounds(&writer);
    Ok(Self::valid_reusable_range(offset, size, wal_start, wal_end))
  }

  /// Serialize namespace-level mutations that publish mutable path keys and
  /// directory/HEAD state. The lower writer/KV locks make individual appends
  /// safe, but they do not make a whole file/directory publish atomic against
  /// another namespace writer.
  pub(crate) fn namespace_write_guard(&self) -> EngineResult<NamespaceWriteGuard<'_>> {
    let engine_id = self as *const StorageEngine as usize;
    let already_held = NAMESPACE_WRITE_STACK.with(|stack| stack.borrow().iter().any(|held| *held == engine_id));
    if already_held {
      NAMESPACE_WRITE_STACK.with(|stack| stack.borrow_mut().push(engine_id));
      return Ok(NamespaceWriteGuard { engine_id, _guard: None });
    }

    let guard = self.namespace_write_lock.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    NAMESPACE_WRITE_STACK.with(|stack| stack.borrow_mut().push(engine_id));
    Ok(NamespaceWriteGuard { engine_id, _guard: Some(guard) })
  }

  fn validate_kv_entry_offset(writer: &AppendWriter, kv_entry: &KVEntry, hash: &[u8], context: &str) -> EngineResult<()> {
    let (wal_start, wal_end) = Self::writer_wal_bounds(writer);
    if Self::valid_reusable_range(kv_entry.offset, kv_entry.total_length, wal_start, wal_end) {
      return Ok(());
    }

    tracing::warn!(
      context,
      offset = kv_entry.offset,
      total_length = kv_entry.total_length,
      hash = %hex::encode(&hash[..8.min(hash.len())]),
      wal_start,
      wal_end,
      "KV entry points outside current WAL region"
    );
    Err(EngineError::CorruptEntry {
      offset: kv_entry.offset,
      reason: format!("KV entry points outside current WAL region {}..{} for hash {}", wal_start, wal_end, hex::encode(hash)),
    })
  }

  fn filter_voids_for_bounds(voids: impl IntoIterator<Item = VoidRecord>, wal_start: u64, wal_end: u64, context: &str) -> Vec<VoidRecord> {
    let mut kept = Vec::new();
    let mut dropped = 0usize;
    for void in voids {
      if Self::valid_reusable_range(void.offset, void.size, wal_start, wal_end) {
        kept.push(void);
      } else {
        dropped += 1;
      }
    }
    if dropped > 0 {
      tracing::warn!(context, dropped, wal_start, wal_end, "Dropped invalid void records outside the current WAL region");
    }
    kept
  }

  fn adjust_voids_for_expansion(
    voids: impl IntoIterator<Item = VoidRecord>,
    old_kv_end: u64,
    relocated_end: u64,
    offset_delta: i64,
    new_wal_start: u64,
    new_wal_end: u64,
  ) -> Vec<VoidRecord> {
    let mut adjusted = Vec::new();
    let mut dropped = 0usize;

    for mut void in voids {
      let Some(end) = void.offset.checked_add(void.size as u64) else {
        dropped += 1;
        continue;
      };

      if void.offset >= old_kv_end && end <= relocated_end {
        let shifted = (void.offset as i128) + (offset_delta as i128);
        if shifted < 0 || shifted > u64::MAX as i128 {
          dropped += 1;
          continue;
        }
        void.offset = shifted as u64;
      } else if void.offset < new_wal_start {
        dropped += 1;
        continue;
      }

      if Self::valid_reusable_range(void.offset, void.size, new_wal_start, new_wal_end) {
        adjusted.push(void);
      } else {
        dropped += 1;
      }
    }

    if dropped > 0 {
      tracing::warn!(
        dropped,
        old_kv_end,
        relocated_end,
        new_wal_start,
        new_wal_end,
        "Dropped invalid void records while adjusting for KV expansion"
      );
    }

    adjusted
  }

  /// Acquire an exclusive advisory file lock. Returns the locked file handle
  /// which must be kept alive for the duration of the engine's lifetime.
  /// If another process already holds the lock, returns an error immediately.
  fn acquire_file_lock(lock_path: &str) -> EngineResult<std::fs::File> {
    let lock_file = OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(false)
      .open(lock_path)
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Failed to create lock file '{}': {}", lock_path, error))))?;

    lock_file.try_lock_exclusive().map_err(|_| {
      EngineError::IoError(std::io::Error::other(format!(
        "Database '{}' is locked by another process. Only one process can open a database at a time.",
        lock_path.trim_end_matches(".lock"),
      )))
    })?;

    Ok(lock_file)
  }

  /// Create a new database file at the given path.
  ///
  /// Does not use a hot directory for crash recovery. Suitable for tests and
  /// CLI tools; production servers should use [`create_with_hot_dir`](Self::create_with_hot_dir).
  pub fn create(path: &str) -> EngineResult<Self> {
    Self::create_with_hot_dir(path, None)
  }

  /// Create a new database file at the given path with an optional hot directory
  /// for crash-recovery write-ahead logging.
  ///
  /// NOTE: `hot_dir` is ignored — hot data is stored in the hot tail at the end
  /// of the main .aeordb file. The parameter is kept for API backward compat.
  pub fn create_with_hot_dir(path: &str, _hot_dir: Option<&Path>) -> EngineResult<Self> {
    let lock_path = format!("{}.lock", path);
    let lock_file = Self::acquire_file_lock(&lock_path)?;

    let mut writer = AppendWriter::create(Path::new(path))?;
    let hash_algo = writer.file_header().hash_algo;

    // Open a second file handle for the KV store (same .aeordb file)
    let kv_file = OpenOptions::new().read(true).write(true).open(path)?;
    // v3 layout: data starts after BOTH header slots (HEADER_REGION_SIZE), not
    // just the first one. The two slots make up the A/B double-buffer.
    let kv_block_offset = crate::engine::file_header::HEADER_REGION_SIZE as u64;
    let hash_length = hash_algo.hash_length();
    let kv_block_length = crate::engine::kv_stages::initial_block_size();
    // hot_tail_offset = after header + KV block
    let hot_tail_offset = kv_block_offset + kv_block_length;

    let kv_store = DiskKVStore::create(kv_file, hash_algo, kv_block_offset, hot_tail_offset, 0)?;

    // Set the append writer's offset past the KV block so WAL entries
    // don't overwrite the KV pages.
    writer.set_offset(hot_tail_offset);

    // Write empty hot tail
    {
      let mut f = OpenOptions::new().read(true).write(true).open(path)?;
      let empty = crate::engine::hot_tail::HotTailPayload::default();
      let end = crate::engine::hot_tail::write_hot_tail(&mut f, hot_tail_offset, &empty, hash_length)?;
      f.set_len(end)?;
      f.sync_data()?;
    }

    // Update file header with KV layout info
    {
      let mut header = writer.file_header().clone();
      header.kv_block_offset = kv_block_offset;
      header.kv_block_length = kv_block_length;
      header.kv_block_stage = 0;
      header.hot_tail_offset = hot_tail_offset;
      writer.update_header(&header)?;
    }

    let kv_snapshot = Arc::clone(kv_store.snapshot_handle());

    let void_manager = VoidManager::new(hash_algo);

    let engine = StorageEngine {
      operation_tracker: EngineOperationTracker::default(),
      shutdown_started: AtomicBool::new(false),
      shutdown_flush_started: AtomicBool::new(false),
      shutdown_complete: AtomicBool::new(false),
      durability_failure: Mutex::new(None),
      emergency_spill: Mutex::new(None),
      last_published_hot_tail_offset: AtomicU64::new(hot_tail_offset),
      namespace_write_lock: Mutex::new(()),
      writer: RwLock::new(writer),
      kv_writer: Mutex::new(kv_store),
      kv_snapshot,
      void_manager: RwLock::new(void_manager),
      hash_algo,
      counters: ArcSwap::from_pointee(EngineCounters::new()),
      permissions_cache: Arc::new(Cache::new(PermissionsLoader)),
      index_config_cache: Arc::new(Cache::new(IndexConfigLoader)),
      grants_index_cache: Arc::new(Cache::new(crate::engine::grants_index::GrantsIndexLoader)),
      last_auto_snapshot_delete: std::sync::atomic::AtomicI64::new(0),
      last_auto_snapshot_restore: std::sync::atomic::AtomicI64::new(0),
      last_manual_snapshot: std::sync::atomic::AtomicI64::new(0),
      dir_content_cache: RwLock::new(HashMap::new()),
      index_write_buffer: Mutex::new(SharedIndexWriteBuffer::default()),
      gc_recheck: Mutex::new(None),
      _file_lock: lock_file,
    };
    let initialized = Arc::new(EngineCounters::initialize_from_kv(&engine));
    engine.counters.store(initialized);
    Ok(engine)
  }

  /// Internal open logic shared by `open` and `open_for_import`.
  ///
  /// The KV block and hot tail are inside the .aeordb file. On open:
  /// 1. Read file header for KV/hot tail offsets
  /// 2. Read hot tail entries (crash recovery buffer)
  /// 3. Open KV store from in-file bucket pages
  /// 4. Scan WAL for void entries (in-memory optimization)
  ///
  /// If the hot tail is corrupt, falls back to a full WAL scan rebuild.
  fn open_internal(path: &str, _hot_dir: Option<&Path>, progress_callback: Option<EngineStartupProgressCallback>) -> EngineResult<Self> {
    Self::report_startup_progress(
      &progress_callback,
      EngineStartupProgress {
        phase: "opening_file".to_string(),
        message: "Opening database file".to_string(),
        current: 0,
        total: None,
        progress: Some(0.0),
        eta_seconds: None,
      },
    );
    let lock_path = format!("{}.lock", path);
    let lock_file = Self::acquire_file_lock(&lock_path)?;

    let mut writer = AppendWriter::open(Path::new(path))?;
    let hash_algo = writer.file_header().hash_algo;
    let hash_length = hash_algo.hash_length();
    let mut file_header = writer.file_header().clone();

    // Set writer offset to hot_tail_offset so new entries go before the hot tail
    if file_header.hot_tail_offset > 0 {
      writer.set_offset(file_header.hot_tail_offset);
    }

    let mut void_manager = VoidManager::new(hash_algo);

    // Voids loaded from the hot tail (clean startup path). On dirty startup
    // these will be empty and we'll populate via gap-scan later.
    // (Populated further down once `hot_voids` is read.)

    // Check for pending KV block expansion (resize was blocked at runtime).
    // expand_kv_block relocates WAL entries forward and zero-fills the KV block.
    // After expansion, the engine opens normally and then rebuild_kv() is called
    // to repopulate the KV index from a full WAL scan with correct new offsets.
    let mut needs_kv_rebuild = false;
    let resize_target = file_header.resize_target_stage as usize;
    let current_stage = file_header.kv_block_stage as usize;
    if resize_target > current_stage {
      tracing::info!(current_stage, resize_target, "Pending KV block expansion detected — expanding before opening");
      // Drop the writer to release the file handle during expansion
      drop(writer);
      match crate::engine::kv_expand::expand_kv_block(path, resize_target, hash_length) {
        Ok((new_length, new_stage, delta)) => {
          tracing::info!(new_length, new_stage, delta, "KV block expanded successfully — will rebuild KV index");
          needs_kv_rebuild = true;
        }
        Err(e) => {
          tracing::error!("KV block expansion failed: {}. Continuing with overflow buffer.", e);
        }
      }
      // Re-open writer and re-read the (possibly updated) header
      writer = AppendWriter::open(Path::new(path))?;
      file_header = writer.file_header().clone();
      if file_header.hot_tail_offset > 0 {
        writer.set_offset(file_header.hot_tail_offset);
      }
    }

    let kv_block_offset = file_header.kv_block_offset;
    let kv_block_stage = file_header.kv_block_stage as usize;
    let hot_tail_offset = file_header.hot_tail_offset;
    let kv_block_valid = kv_block_offset > 0 && hot_tail_offset > kv_block_offset;

    tracing::debug!(
      kv_block_offset,
      kv_block_length = file_header.kv_block_length,
      kv_block_stage,
      hot_tail_offset,
      kv_block_valid,
      entry_count = file_header.entry_count,
      writer_offset = writer.current_offset(),
      "open_internal: file header loaded"
    );

    // Read hot tail payload (writes + voids) from end of file
    let (hot_payload, needs_dirty_startup) = if hot_tail_offset > 0 {
      let mut f = OpenOptions::new().read(true).open(path)?;
      match crate::engine::hot_tail::read_hot_tail(&mut f, hot_tail_offset, hash_length) {
        Some(payload) => {
          tracing::debug!(
            hot_writes_loaded = payload.writes.len(),
            hot_voids_loaded = payload.voids.len(),
            "open_internal: hot tail loaded",
          );
          (payload, false)
        }
        None => {
          tracing::warn!(hot_tail_offset, "Corrupt or missing hot tail — will rebuild KV from WAL (dirty startup)");
          (crate::engine::hot_tail::HotTailPayload::default(), true)
        }
      }
    } else {
      (crate::engine::hot_tail::HotTailPayload::default(), false)
    };
    let hot_entries = hot_payload.writes.clone();
    let hot_voids = hot_payload.voids;
    let wal_start = file_header.kv_block_offset.saturating_add(file_header.kv_block_length);
    let hot_voids = Self::filter_voids_for_bounds(hot_voids, wal_start, hot_tail_offset, "startup hot-tail load");

    // Populate void_manager from the hot tail's void section (clean startup).
    // On dirty startup hot_voids is empty; we re-derive via gap-scan later.
    for v in &hot_voids {
      void_manager.register_void(v.offset, v.size);
    }

    let mut detected_kv_corruption = false;
    let kv_store = if kv_block_valid {
      // KV block is in the file — open from in-file pages
      let kv_file = OpenOptions::new().read(true).write(true).open(path)?;
      let kv = DiskKVStore::open(
        kv_file,
        hash_algo,
        kv_block_offset,
        hot_tail_offset,
        kv_block_stage,
        hot_entries,
        hot_voids.clone(),
        file_header.kv_block_version,
      )?;
      // If any bucket page failed CRC on open, the KV index is unreliable
      // for the buckets involved — trigger dirty startup below so the WAL
      // scan is the source of truth.
      if kv.needs_rebuild {
        detected_kv_corruption = true;
      }

      // Voids live in the hot tail (already loaded into void_manager above).
      // No WAL scan needed on clean startup — pre-refactor we walked the
      // entire WAL here to find EntryType::Void records, but those records
      // no longer represent the source of truth for void state. On a 60 GB
      // DB on a rotational disk that scan was a >50 minute startup cost
      // finding zero useful records.

      kv
    } else {
      // No valid KV block — create from full WAL scan (dirty startup)
      let kv_block_offset = crate::engine::file_header::HEADER_REGION_SIZE as u64;
      let kv_block_length = crate::engine::kv_stages::initial_block_size();
      let hot_tail_offset = kv_block_offset + kv_block_length;

      let kv_file = OpenOptions::new().read(true).write(true).open(path)?;
      let mut kv = DiskKVStore::create(kv_file, hash_algo, kv_block_offset, hot_tail_offset, 0)?;

      // First pass: rebuild KV store from entry headers, collecting deletion records.
      // Entry offsets are not chronology once GC starts reusing voids, so
      // duplicate mutable keys are resolved by entry timestamp before flush.
      let mut deletion_records: Vec<(String, RebuildOrder)> = Vec::new();
      let mut rebuild_records = Vec::new();
      let scanner = writer.scan_entries()?;
      for scanned_result in scanner {
        let scanned = match scanned_result {
          Ok(entry) => entry,
          Err(e) => {
            tracing::warn!("Skipping corrupt entry during rebuild: {}", e);
            continue;
          }
        };
        // Collect deletion records. Voids are recovered via gap-scan
        // (recover_voids_via_gap_scan) on dirty startup — the WAL void
        // entries we used to register here are not the source of truth.
        let order = RebuildOrder { timestamp: scanned.header.timestamp, offset: scanned.offset };
        if scanned.header.entry_type == EntryType::DeletionRecord {
          if let Ok(record) = crate::engine::deletion_record::DeletionRecord::deserialize(&scanned.value, scanned.header.entry_version) {
            deletion_records.push((record.path, order));
          }
        }
        let kv_type = scanned.header.entry_type.to_kv_type();

        rebuild_records.push(RebuildKvRecord {
          type_flags: kv_type,
          hash: scanned.key.clone(),
          offset: scanned.offset,
          value_length: scanned.header.value_length,
          total_length: scanned.header.total_length,
          order,
        });
      }

      let resolved = Self::resolve_rebuild_records(rebuild_records, hash_algo, &deletion_records)?;
      kv.bulk_insert(&resolved);
      kv.flush()?;

      kv
    };

    // Hot tail entries are already loaded into the DiskKVStore write buffer
    // by DiskKVStore::open() — no separate replay step needed.

    let kv_snapshot = Arc::clone(kv_store.snapshot_handle());

    let engine = StorageEngine {
      operation_tracker: EngineOperationTracker::default(),
      shutdown_started: AtomicBool::new(false),
      shutdown_flush_started: AtomicBool::new(false),
      shutdown_complete: AtomicBool::new(false),
      durability_failure: Mutex::new(None),
      emergency_spill: Mutex::new(None),
      last_published_hot_tail_offset: AtomicU64::new(file_header.hot_tail_offset),
      namespace_write_lock: Mutex::new(()),
      writer: RwLock::new(writer),
      kv_writer: Mutex::new(kv_store),
      kv_snapshot,
      void_manager: RwLock::new(void_manager),
      hash_algo,
      counters: ArcSwap::from_pointee(EngineCounters::new()),
      permissions_cache: Arc::new(Cache::new(PermissionsLoader)),
      index_config_cache: Arc::new(Cache::new(IndexConfigLoader)),
      grants_index_cache: Arc::new(Cache::new(crate::engine::grants_index::GrantsIndexLoader)),
      last_auto_snapshot_delete: std::sync::atomic::AtomicI64::new(0),
      last_auto_snapshot_restore: std::sync::atomic::AtomicI64::new(0),
      last_manual_snapshot: std::sync::atomic::AtomicI64::new(0),
      dir_content_cache: RwLock::new(HashMap::new()),
      index_write_buffer: Mutex::new(SharedIndexWriteBuffer::default()),
      gc_recheck: Mutex::new(None),
      _file_lock: lock_file,
    };
    let initialized = Arc::new(EngineCounters::initialize_from_kv(&engine));
    engine.counters.store(initialized);

    // After KV block expansion, rebuild the entire KV index from WAL.
    // The expansion zeroed the KV pages, so only hot tail entries are loaded.
    // A full rebuild repopulates all entries at their new offsets.
    let did_dirty_rebuild = needs_kv_rebuild || needs_dirty_startup || detected_kv_corruption;
    if did_dirty_rebuild {
      if needs_kv_rebuild {
        tracing::info!("Rebuilding KV index after block expansion...");
      }
      if needs_dirty_startup {
        tracing::warn!("Dirty startup: rebuilding KV index from full WAL scan...");
      }
      engine.rebuild_kv_with_progress(progress_callback.clone())?;
      // Re-initialize counters from the freshly rebuilt KV
      let refreshed = Arc::new(EngineCounters::initialize_from_kv(&engine));
      engine.counters.store(refreshed);

      // Dirty rebuild lost the hot tail's void state. Re-derive voids by
      // gap-scanning the rebuilt KV (sorted by offset, ignoring deleted
      // entries) — any byte range not covered by a live KV entry is a void.
      Self::report_startup_progress(
        &progress_callback,
        EngineStartupProgress {
          phase: "recovering_voids".to_string(),
          message: "Recovering reusable WAL gaps after dirty startup".to_string(),
          current: 0,
          total: None,
          progress: Some(0.96),
          eta_seconds: None,
        },
      );
      engine.recover_voids_via_gap_scan()?;
    }

    // Seed the DiskKVStore's pending_voids snapshot from the loaded
    // VoidManager state so the next hot tail flush carries it forward.
    engine.sync_voids_to_kv_writer();
    Self::report_startup_progress(
      &progress_callback,
      EngineStartupProgress {
        phase: "engine_ready".to_string(),
        message: "Storage engine is open".to_string(),
        current: 1,
        total: Some(1),
        progress: Some(1.0),
        eta_seconds: Some(0),
      },
    );

    Ok(engine)
  }

  fn report_startup_progress(callback: &Option<EngineStartupProgressCallback>, progress: EngineStartupProgress) {
    if let Some(callback) = callback {
      callback(progress);
    }
  }

  /// Gap-scan the live KV index and register each gap (between consecutive
  /// non-deleted entries' offset ranges) as a void in VoidManager. Used
  /// after dirty startup when the hot tail's void section was lost.
  ///
  /// The cursor starts at the WAL's start offset (immediately after the KV
  /// block), so any gap between the KV block boundary and the first live
  /// entry is captured. Previously this started at `ranges.first()` which
  /// missed the very first void if it lived between kv_block_end and the
  /// first entry.
  pub(crate) fn recover_voids_via_gap_scan(&self) -> EngineResult<()> {
    // WAL begins immediately after the KV block.
    let (wal_start, wal_end): (u64, u64) = {
      let writer = self.writer.read().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      Self::writer_wal_bounds(&writer)
    };

    // Collect (offset, total_length) of all live (non-deleted) entries.
    let mut ranges: Vec<(u64, u32)> = {
      let snapshot = self.kv_snapshot.load();
      let entries = snapshot.iter_all()?;
      entries
        .iter()
        .filter(|e| !e.is_deleted())
        .filter_map(|e| {
          if Self::valid_reusable_range(e.offset, e.total_length, wal_start, wal_end) {
            Some((e.offset, e.total_length))
          } else {
            tracing::warn!(
              offset = e.offset,
              total_length = e.total_length,
              wal_start,
              wal_end,
              "Skipping live KV entry outside current WAL region during void gap-scan"
            );
            None
          }
        })
        .collect()
    };
    ranges.sort_by_key(|(offset, _)| *offset);

    let mut vm = self.void_manager.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;

    let mut recovered = Vec::new();
    let mut cursor: u64 = wal_start;
    for (offset, total_length) in &ranges {
      if *offset > cursor {
        let gap_size = *offset - cursor;
        let gap_size_u32 = u32::try_from(gap_size).unwrap_or(u32::MAX);
        recovered.push(VoidRecord { offset: cursor, size: gap_size_u32 });
      }
      cursor = offset.saturating_add(*total_length as u64).max(cursor);
    }
    vm.replace_all(recovered.into_iter().map(|void| (void.offset, void.size)));

    tracing::info!(
      void_count = vm.void_count(),
      total_void_bytes = vm.total_void_space(),
      wal_start,
      "Recovered voids via gap-scan after dirty startup"
    );

    Ok(())
  }

  /// Repair path used after external emergency spill artifacts have restored
  /// any missing WAL-tail bytes. This deliberately uses the dirty-recovery
  /// scanner so entries beyond a stale `hot_tail_offset` are indexed, then
  /// reconstructs reusable void state from WAL gaps and publishes a fresh
  /// hot-tail snapshot.
  pub fn recover_after_emergency_spill_replay(&self) -> EngineResult<()> {
    self.rebuild_kv()?;
    self.recover_voids_via_gap_scan()?;
    self.sync_voids_to_kv_writer();
    self.force_hot_tail_flush()
  }

  /// Force an immediate hot tail flush. Used by GC sweep after registering
  /// new voids so the void state is durable without waiting for the normal
  /// threshold trigger.
  pub(crate) fn force_hot_tail_flush(&self) -> EngineResult<()> {
    self.ensure_writable()?;
    let sync_result = {
      let mut writer = self.writer.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      writer.sync()
    };
    if let Err(error) = sync_result {
      return Err(self.record_durability_failure("Forced hot-tail WAL sync failed", error));
    }

    let flush_result = {
      let mut kv = self.kv_writer.lock().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      kv.force_flush_hot_buffer().map(|_| (kv.hot_tail_offset(), kv.len() as u64, kv.transaction_depth > 0))
    };
    let (hot_tail_offset, entry_count, in_transaction) = match flush_result {
      Ok(result) => result,
      Err(error) => return Err(self.record_durability_failure("Forced hot-tail flush failed", error)),
    };

    if in_transaction {
      return Ok(());
    }

    let header_result = {
      let mut writer = self.writer.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let mut header = writer.file_header().clone();
      header.hot_tail_offset = hot_tail_offset;
      header.entry_count = entry_count;
      writer.update_header(&header)
    };
    if let Err(error) = header_result {
      return Err(self.record_durability_failure("Forced hot-tail header update failed", error));
    }
    self.last_published_hot_tail_offset.store(hot_tail_offset, Ordering::Release);
    Ok(())
  }

  /// Flush buffered index mutations if their shared write-count/time policy
  /// says they are due.
  pub fn flush_index_buffer_if_due(&self) -> EngineResult<bool> {
    self.ensure_writable()?;
    IndexManager::new(self).flush_buffered_indexes_if_due()
  }

  /// Force all buffered index mutations to disk.
  pub fn flush_index_buffer(&self) -> EngineResult<usize> {
    self.ensure_writable()?;
    IndexManager::new(self).flush_buffered_indexes()
  }

  pub fn index_buffer_stats(&self) -> crate::engine::index_store::IndexWriteBufferStats {
    IndexManager::new(self).buffered_index_stats()
  }

  /// Mirror VoidManager state into the DiskKVStore's pending_voids so the
  /// next hot tail flush includes the current void snapshot. Call after any
  /// operation that changes the void set (GC sweep, void consumption,
  /// startup population). Also refreshes the void_count + void_space
  /// counters so dashboard metrics stay accurate.
  pub(crate) fn sync_voids_to_kv_writer(&self) {
    let (voids, count, total_bytes) = match self.void_manager.read() {
      Ok(vm) => {
        let collected: Vec<crate::engine::hot_tail::VoidRecord> =
          vm.iter().map(|(offset, size)| crate::engine::hot_tail::VoidRecord { offset, size }).collect();
        let total = vm.total_void_space();
        let count = vm.void_count() as u64;
        (collected, count, total)
      }
      Err(_) => return,
    };
    if let Ok(mut kv) = self.kv_writer.lock() {
      kv.set_pending_voids(voids);
    }
    self.counters.load().set_void_stats(count, total_bytes);
  }

  /// Open an existing database file.
  ///
  /// Rebuilds the KV index from a full file scan if the `.kv` sidecar is
  /// missing or stale. Does not use a hot directory. Refuses to open patch
  /// databases (`backup_type > 1`).
  pub fn open(path: &str) -> EngineResult<Self> {
    let engine = Self::open_internal(path, None, None)?;

    // Guard: refuse to open patch databases as normal databases
    let header = engine
      .writer
      .read()
      .map_err(|e| EngineError::IoError(std::io::Error::other(format!("writer lock poisoned: {}", e))))?
      .file_header()
      .clone();
    if header.backup_type > 1 {
      let base = hex::encode(&header.base_hash);
      let target = hex::encode(&header.target_hash);
      return Err(EngineError::PatchDatabase(format!(
        "This is a patch export and cannot be used as a standalone database.\n\n\
         Base version:   {}\n\
         Target version: {}\n\n\
         To apply this patch, import it into a database at the base version:\n\
         aeordb import --database <your.aeordb> --file {}",
        base, target, path
      )));
    }

    Ok(engine)
  }

  /// Open an existing database with a hot directory for crash recovery.
  ///
  /// Replays any existing hot files on startup, then initializes a new hot
  /// file for ongoing writes. This is the recommended open path for production
  /// servers.
  pub fn open_with_hot_dir(path: &str, hot_dir: Option<&Path>) -> EngineResult<Self> {
    Self::open_with_hot_dir_and_progress(path, hot_dir, None)
  }

  pub fn open_with_hot_dir_and_progress(
    path: &str,
    hot_dir: Option<&Path>,
    progress_callback: Option<EngineStartupProgressCallback>,
  ) -> EngineResult<Self> {
    let engine = Self::open_internal(path, hot_dir, progress_callback)?;

    // Guard: refuse to open patch databases as normal databases
    let header = engine
      .writer
      .read()
      .map_err(|e| EngineError::IoError(std::io::Error::other(format!("writer lock poisoned: {}", e))))?
      .file_header()
      .clone();
    if header.backup_type > 1 {
      let base = hex::encode(&header.base_hash);
      let target = hex::encode(&header.target_hash);
      return Err(EngineError::PatchDatabase(format!(
        "This is a patch export and cannot be used as a standalone database.\n\n\
         Base version:   {}\n\
         Target version: {}\n\n\
         To apply this patch, import it into a database at the base version:\n\
         aeordb import --database <your.aeordb> --file {}",
        base, target, path
      )));
    }

    Ok(engine)
  }

  /// Open a database file for import purposes, allowing patch databases.
  pub fn open_for_import(path: &str) -> EngineResult<Self> {
    Self::open_internal(path, None, None)
  }

  /// Store an entry: append to file, register in KV store.
  /// Returns the file offset where the entry was written.
  ///
  /// Both the writer and KV locks are held simultaneously to prevent a
  /// TOCTOU gap where a crash between the disk write and the KV insert
  /// could leave the entry on disk but missing from the index.
  /// Lock order: writer first, then KV (must be consistent everywhere).
  pub fn store_entry(&self, entry_type: EntryType, key: &[u8], value: &[u8]) -> EngineResult<u64> {
    self.store_entry_internal(entry_type, key, value, 0, CompressionAlgorithm::None, crate::engine::entry_header::CURRENT_ENTRY_VERSION)
  }

  pub fn store_entry_with_flags(&self, entry_type: EntryType, key: &[u8], value: &[u8], flags: u8) -> EngineResult<u64> {
    self.store_entry_internal(entry_type, key, value, flags, CompressionAlgorithm::None, crate::engine::entry_header::CURRENT_ENTRY_VERSION)
  }

  pub fn store_entry_with_version(&self, entry_type: EntryType, key: &[u8], value: &[u8], entry_version: u8) -> EngineResult<u64> {
    self.store_entry_internal(entry_type, key, value, 0, CompressionAlgorithm::None, entry_version)
  }

  pub fn store_entry_with_flags_and_version(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    flags: u8,
    entry_version: u8,
  ) -> EngineResult<u64> {
    self.store_entry_internal(entry_type, key, value, flags, CompressionAlgorithm::None, entry_version)
  }

  pub fn store_entry_compressed(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<u64> {
    self.store_entry_internal(entry_type, key, value, 0, compression_algo, crate::engine::entry_header::CURRENT_ENTRY_VERSION)
  }

  pub fn store_entry_compressed_with_flags(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    flags: u8,
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<u64> {
    self.store_entry_internal(entry_type, key, value, flags, compression_algo, crate::engine::entry_header::CURRENT_ENTRY_VERSION)
  }

  /// Core store_entry implementation. Acquires writer + KV locks, appends
  /// entry to WAL, and registers in KV index.
  /// Lock order: writer first, then KV (must be consistent everywhere).
  fn store_entry_internal(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    flags: u8,
    compression_algo: CompressionAlgorithm,
    entry_version: u8,
  ) -> EngineResult<u64> {
    let _operation = self.operation_guard("store_entry")?;
    self.ensure_writable()?;
    let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;

    // Try to consume a void first. If we find one that's big enough, write
    // the entry in-place at the void's offset instead of growing the WAL.
    // This is how the GC's freed space gets recycled into new writes.
    //
    // The size is computed from the caller-provided `value` length — for
    // compressed entries, the caller has already compressed the bytes and
    // `value` holds the compressed payload, so compute_total_length gives
    // the right disk size.
    let needed = crate::engine::entry_header::EntryHeader::compute_total_length(self.hash_algo, key.len(), value.len())?;
    let (wal_start, wal_end) = Self::writer_wal_bounds(&writer);
    let mut voids_changed_via_consume = false;
    let void_slot = if Self::can_reuse_void_for_entry(entry_type) {
      if let Ok(mut vm) = self.void_manager.write() {
        loop {
          match vm.find_void(needed) {
            Some((void_offset, void_size)) => {
              voids_changed_via_consume = true;
              if Self::valid_reusable_range(void_offset, void_size, wal_start, wal_end) {
                break Some((void_offset, void_size));
              }
              tracing::warn!(void_offset, void_size, wal_start, wal_end, "Discarding invalid void outside current WAL region before reuse");
            }
            None => break None,
          }
        }
      } else {
        None
      }
    } else {
      None
    };

    let (offset, total_length) = if let Some((void_offset, _void_size)) = void_slot {
      // In-place write at the void's offset. The void is already removed
      // from void_manager (find_void did it). After this write, the bytes
      // at void_offset belong to the new entry.
      //
      // No explicit fsync here — void-consumption writes ride the same
      // hot-tail-flush durability path as appends. The whole point of this
      // plumbing is to AVOID per-entry random fsyncs.
      let written =
        writer.write_entry_at_nosync_full_with_version(void_offset, entry_type, key, value, flags, compression_algo, entry_version)?;
      (void_offset, written)
    } else {
      writer.append_entry_with_compression_and_version(entry_type, key, value, flags, compression_algo, entry_version)?
    };
    kv.set_hot_tail_offset(writer.current_offset());

    let kv_entry = KVEntry { type_flags: entry_type.to_kv_type(), hash: key.to_vec(), offset, total_length };
    kv.insert(kv_entry)?;
    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    // If we consumed a void, the pending_voids snapshot in kv_writer is stale
    // — refresh it so the next hot tail flush reflects the consumed state.
    if voids_changed_via_consume {
      if let Ok(vm) = self.void_manager.read() {
        let voids: Vec<crate::engine::hot_tail::VoidRecord> =
          vm.iter().map(|(o, s)| crate::engine::hot_tail::VoidRecord { offset: o, size: s }).collect();
        kv.set_pending_voids(voids);
      }
    }

    // Check if KV block needs expansion (set during insert → flush → resize)
    let pending_expansion = kv.needs_expansion.take();

    // Drop locks before expansion (expansion acquires them itself)
    drop(kv);
    drop(writer);

    if let Some(target_stage) = pending_expansion {
      if let Err(e) = self.expand_kv_block_online(target_stage) {
        tracing::error!("Online KV expansion failed: {}. Will retry on next overflow.", e);
      }
    }

    // If GC is running, record the hash so the sweep phase can spare it.
    self.record_gc_recheck(key);

    Ok(offset)
  }

  fn can_reuse_void_for_entry(entry_type: EntryType) -> bool {
    matches!(entry_type, EntryType::Chunk)
  }

  /// Record a write into the GC recheck set if GC mark+sweep is active.
  /// No-op otherwise. Cheap: one Mutex acquisition + an Option check.
  fn record_gc_recheck(&self, hash: &[u8]) {
    if let Ok(mut guard) = self.gc_recheck.lock() {
      if let Some(set) = guard.as_mut() {
        set.insert(hash.to_vec());
      }
    }
  }

  /// Begin GC recheck tracking. Subsequent writes have their hashes recorded
  /// into the recheck set. The caller (GC) reads + clears the set via
  /// `take_gc_recheck` between mark and sweep, and again after sweep.
  pub fn begin_gc_recheck(&self) {
    if let Ok(mut guard) = self.gc_recheck.lock() {
      *guard = Some(HashSet::new());
    }
  }

  /// Drain the GC recheck set. Returns the hashes accumulated since the last
  /// call (or since `begin_gc_recheck`). Leaves an empty set in place so
  /// recording continues.
  pub fn take_gc_recheck(&self) -> HashSet<Vec<u8>> {
    if let Ok(mut guard) = self.gc_recheck.lock() {
      if let Some(set) = guard.as_mut() {
        return std::mem::take(set);
      }
    }
    HashSet::new()
  }

  /// Peek at the GC recheck set without draining. Used during sweep to spare
  /// in-flight writes (writers can still add while we read).
  pub fn gc_recheck_contains(&self, hash: &[u8]) -> bool {
    if let Ok(guard) = self.gc_recheck.lock() {
      if let Some(set) = guard.as_ref() {
        return set.contains(hash);
      }
    }
    false
  }

  /// End GC recheck tracking. Writes will no longer record.
  pub fn end_gc_recheck(&self) {
    if let Ok(mut guard) = self.gc_recheck.lock() {
      *guard = None;
    }
  }

  /// Retrieve an entry by its hash key via a lock-free snapshot read.
  ///
  /// Returns `(header, key, value)` if a non-deleted entry exists.
  pub fn get_entry(&self, hash: &[u8]) -> EngineResult<Option<EntryData>> {
    let _operation = self.operation_guard("get_entry")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get(hash) {
      Some(entry) if !entry.is_deleted() => entry,
      _ => return Ok(None),
    };

    // Use a READ lock — read_entry_at_shared uses a cloned file handle
    // so it doesn't disturb the writer's seek position.
    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry")?;
    let result = writer.read_entry_at_shared(kv_entry.offset);
    if result.is_err() {
      let kv_block_end = {
        let header = writer.file_header();
        header.kv_block_offset.saturating_add(header.kv_block_length)
      };
      tracing::debug!(
        offset = kv_entry.offset,
        hash = %hex::encode(&hash[..8.min(hash.len())]),
        type_flags = kv_entry.type_flags,
        kv_block_end,
        "get_entry: read failed at KV offset"
      );
    }
    result.map(Some)
  }

  /// Retrieve only an entry header by key without reading the entry value.
  pub fn get_entry_header(&self, hash: &[u8]) -> EngineResult<Option<EntryHeader>> {
    let _operation = self.operation_guard("get_entry_header")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get(hash) {
      Some(entry) if !entry.is_deleted() => entry,
      _ => return Ok(None),
    };

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry_header")?;
    writer.read_entry_header_at_shared(kv_entry.offset).map(Some)
  }

  /// Retrieve an entry by hash, including deleted entries.
  /// Used for version history where we need to read files that were
  /// deleted after a snapshot was taken.
  pub fn get_entry_including_deleted(&self, hash: &[u8]) -> EngineResult<Option<EntryData>> {
    let _operation = self.operation_guard("get_entry_including_deleted")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get_raw(hash) {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry_including_deleted")?;
    let result = writer.read_entry_at_shared(kv_entry.offset);
    result.map(Some)
  }

  /// Retrieve an entry by hash with BLAKE3 hash verification.
  /// Use this for user-facing reads (GET /files/) where integrity matters.
  /// Internal engine reads use `get_entry()` without verification for performance.
  pub fn get_entry_verified(&self, hash: &[u8]) -> EngineResult<Option<EntryData>> {
    let _operation = self.operation_guard("get_entry_verified")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get(hash) {
      Some(entry) if !entry.is_deleted() => entry,
      _ => return Ok(None),
    };

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry_verified")?;
    let (header, key, value) = writer.read_entry_at_shared_verified(kv_entry.offset)?;

    Ok(Some((header, key, value)))
  }

  /// Like `get_entry_verified` but includes entries marked as deleted.
  /// Needed for reading historical chunk data when streaming files from snapshots.
  pub fn get_entry_verified_including_deleted(&self, hash: &[u8]) -> EngineResult<Option<EntryData>> {
    let _operation = self.operation_guard("get_entry_verified_including_deleted")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get_raw(hash) {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry_verified_including_deleted")?;
    let (header, key, value) = writer.read_entry_at_shared_verified(kv_entry.offset)?;

    Ok(Some((header, key, value)))
  }

  fn decode_chunk_entry(&self, requested_hash: &[u8], header: EntryHeader, value: Vec<u8>) -> EngineResult<Vec<u8>> {
    if header.entry_type != EntryType::Chunk {
      return Err(EngineError::InvalidInput(format!("Hash {} is not a chunk entry", hex::encode(requested_hash))));
    }

    if header.compression_algo != CompressionAlgorithm::None {
      crate::engine::compression::decompress(&value, header.compression_algo)
    } else {
      Ok(value)
    }
  }

  /// Read a non-deleted chunk and return its decompressed bytes.
  pub fn read_chunk(&self, hash: &[u8]) -> EngineResult<Option<Vec<u8>>> {
    match self.get_entry(hash)? {
      Some((header, _key, value)) => self.decode_chunk_entry(hash, header, value).map(Some),
      None => Ok(None),
    }
  }

  /// Return metadata for a live chunk without loading its value.
  pub fn get_chunk_metadata(&self, hash: &[u8]) -> EngineResult<Option<ChunkEntryMetadata>> {
    let _operation = self.operation_guard("get_chunk_metadata")?;
    let snapshot = self.kv_snapshot.load();
    let Some(kv_entry) = snapshot.get(hash) else {
      return Ok(None);
    };
    if kv_entry.is_deleted() {
      return Ok(None);
    }

    if kv_entry.entry_type() != KV_TYPE_CHUNK {
      return Err(EngineError::InvalidInput(format!("Hash {} is not a chunk entry", hex::encode(hash))));
    }

    // Chunks are stored with the chunk hash as the key and no compression.
    // Derive the stored/raw value length from the KV entry's total length so
    // blob commits do not perform one random WAL header read per chunk.
    let hash_length = self.hash_algo.hash_length() as u64;
    let overhead = EntryHeader::FIXED_HEADER_SIZE as u64 + hash_length + hash_length;
    let Some(stored_value_length) = (kv_entry.total_length as u64).checked_sub(overhead) else {
      return Err(EngineError::CorruptEntry {
        offset: kv_entry.offset,
        reason: format!("chunk KV entry total_length {} is smaller than header+key overhead {}", kv_entry.total_length, overhead),
      });
    };

    Ok(Some(ChunkEntryMetadata {
      stored_value_length,
      raw_value_length: Some(stored_value_length),
      compression_algo: CompressionAlgorithm::None,
    }))
  }

  /// Read a chunk including deleted entries and return its decompressed bytes.
  pub fn read_chunk_including_deleted(&self, hash: &[u8]) -> EngineResult<Option<Vec<u8>>> {
    match self.get_entry_including_deleted(hash)? {
      Some((header, _key, value)) => self.decode_chunk_entry(hash, header, value).map(Some),
      None => Ok(None),
    }
  }

  /// Read a non-deleted chunk with entry hash verification.
  pub fn read_chunk_verified(&self, hash: &[u8]) -> EngineResult<Option<Vec<u8>>> {
    match self.get_entry_verified(hash)? {
      Some((header, _key, value)) => self.decode_chunk_entry(hash, header, value).map(Some),
      None => Ok(None),
    }
  }

  /// Read a chunk including deleted entries with entry hash verification.
  pub fn read_chunk_verified_including_deleted(&self, hash: &[u8]) -> EngineResult<Option<Vec<u8>>> {
    match self.get_entry_verified_including_deleted(hash)? {
      Some((header, _key, value)) => self.decode_chunk_entry(hash, header, value).map(Some),
      None => Ok(None),
    }
  }

  /// Check if a non-deleted entry exists in the KV store (lock-free).
  pub fn has_entry(&self, hash: &[u8]) -> EngineResult<bool> {
    let _operation = self.operation_guard("has_entry")?;
    let snapshot = self.kv_snapshot.load();
    match snapshot.get(hash) {
      Some(entry) => Ok(!entry.is_deleted()),
      None => Ok(false),
    }
  }

  /// Acquire a read lock on the append writer.
  ///
  /// Used by the verify module and background integrity scanner to scan
  /// entries without blocking concurrent reads. Returns a read guard that
  /// provides access to `scan_entries()` and `read_entry_at_shared()`.
  pub fn writer_read_lock(&self) -> EngineResult<std::sync::RwLockReadGuard<'_, AppendWriter>> {
    self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(format!("writer lock poisoned: {}", error))))
  }

  /// Return the database's hash algorithm.
  pub fn hash_algo(&self) -> HashAlgorithm {
    self.hash_algo
  }

  /// Convenience wrapper to compute a hash using the database's algorithm.
  pub fn compute_hash(&self, data: &[u8]) -> EngineResult<Vec<u8>> {
    self.hash_algo.compute_hash(data)
  }

  /// Return a reference to the atomic engine counters.
  pub fn counters(&self) -> arc_swap::Guard<Arc<EngineCounters>> {
    self.counters.load()
  }

  /// Reconcile live count counters from the authoritative KV snapshot while
  /// preserving monotonic throughput counters.
  pub fn reconcile_counters_from_kv(&self) {
    let current = self.counters.load().snapshot();
    let mut refreshed = EngineCounters::initialize_from_kv(self).snapshot();
    refreshed.writes_total = current.writes_total;
    refreshed.reads_total = current.reads_total;
    refreshed.bytes_written_total = current.bytes_written_total;
    refreshed.bytes_read_total = current.bytes_read_total;
    refreshed.chunks_deduped_total = current.chunks_deduped_total;
    refreshed.write_buffer_depth = current.write_buffer_depth;
    self.counters.load().reconcile(&refreshed);
  }

  /// Update the HEAD hash in the file header, pointing to a new root directory version.
  pub fn update_head(&self, head_hash: &[u8]) -> EngineResult<()> {
    let _operation = self.operation_guard("update_head")?;
    self.ensure_writable()?;
    let _namespace = self.namespace_write_guard()?;
    let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let in_transaction =
      self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?.transaction_depth > 0;
    let mut header = writer.file_header().clone();
    header.head_hash = head_hash.to_vec();
    header.hot_tail_offset = writer.current_offset();
    header.updated_at = chrono::Utc::now().timestamp_millis();
    if in_transaction {
      writer.set_header_in_memory(header);
    } else {
      writer.update_file_header(&header)?;
      self.last_published_hot_tail_offset.store(header.hot_tail_offset, Ordering::Release);
    }
    Ok(())
  }

  /// Read the current HEAD hash from the file header. HEAD points to the
  /// content-addressed root directory and represents the latest version.
  pub fn head_hash(&self) -> EngineResult<Vec<u8>> {
    let _operation = self.operation_guard("head_hash")?;
    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Ok(writer.file_header().head_hash.clone())
  }

  /// Get the backup metadata from the file header.
  pub fn backup_info(&self) -> EngineResult<(u8, Vec<u8>, Vec<u8>)> {
    let _operation = self.operation_guard("backup_info")?;
    let writer = self.writer.read().map_err(|e| EngineError::IoError(std::io::Error::other(format!("writer lock poisoned: {}", e))))?;
    let fh = writer.file_header();
    Ok((fh.backup_type, fh.base_hash.clone(), fh.target_hash.clone()))
  }

  /// Update the backup metadata in the file header.
  pub fn set_backup_info(&self, backup_type: u8, base_hash: &[u8], target_hash: &[u8]) -> EngineResult<()> {
    let _operation = self.operation_guard("set_backup_info")?;
    self.ensure_writable()?;
    let mut writer = self.writer.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
    let mut header = writer.file_header().clone();
    header.backup_type = backup_type;
    header.base_hash = base_hash.to_vec();
    header.target_hash = target_hash.to_vec();
    writer.update_file_header(&header)?;
    Ok(())
  }

  /// Store an entry with an explicit KV type (for versioning entries
  /// where the EntryType on disk doesn't map 1:1 to the KV type).
  ///
  /// Both the writer and KV locks are held simultaneously to prevent a
  /// TOCTOU gap where a crash between the disk write and the KV insert
  /// could leave the entry on disk but missing from the index.
  /// Lock order: writer first, then KV (must be consistent everywhere).
  pub fn store_entry_typed(&self, entry_type: EntryType, key: &[u8], value: &[u8], kv_type: u8) -> EngineResult<u64> {
    let _operation = self.operation_guard("store_entry_typed")?;
    self.ensure_writable()?;
    // Acquire BOTH locks before any work to close the TOCTOU gap.
    let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;

    let (offset, total_length) = writer.append_entry(entry_type, key, value, 0)?;
    kv.set_hot_tail_offset(writer.current_offset());

    let kv_entry = KVEntry { type_flags: kv_type, hash: key.to_vec(), offset, total_length };
    kv.insert(kv_entry)?;
    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    Ok(offset)
  }

  /// Write all entries in a batch with a single lock acquisition.
  /// Each entry is appended sequentially, then all are registered in the KV store.
  /// Returns the file offsets where entries were written.
  ///
  /// Both the writer and KV locks are held simultaneously for the entire
  /// batch to prevent a TOCTOU gap where a crash between disk writes and
  /// KV inserts could leave entries on disk but missing from the index.
  /// Lock order: writer first, then KV (must be consistent everywhere).
  pub fn flush_batch(&self, batch: WriteBatch) -> EngineResult<Vec<u64>> {
    let _operation = self.operation_guard("flush_batch")?;
    self.ensure_writable()?;
    if batch.is_empty() {
      return Ok(Vec::new());
    }

    // Acquire BOTH locks before any work to close the TOCTOU gap.
    let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;

    let mut offsets = Vec::with_capacity(batch.entries.len());
    let mut totals = Vec::with_capacity(batch.entries.len());

    for entry in &batch.entries {
      let (offset, total_length) = writer.append_entry(
        entry.entry_type,
        &entry.key,
        &entry.value,
        0, // flags
      )?;
      kv.set_hot_tail_offset(writer.current_offset());
      offsets.push(offset);
      totals.push(total_length);
    }

    for (i, entry) in batch.entries.iter().enumerate() {
      let kv_entry = KVEntry { type_flags: entry.kv_type, hash: entry.key.clone(), offset: offsets[i], total_length: totals[i] };
      kv.insert(kv_entry)?;
    }

    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    let pending_expansion = kv.needs_expansion.take();
    drop(kv);
    drop(writer);

    if let Some(target_stage) = pending_expansion {
      if let Err(e) = self.expand_kv_block_online(target_stage) {
        tracing::error!("Online KV expansion failed: {}. Will retry on next overflow.", e);
      }
    }

    Ok(offsets)
  }

  /// Flush a write batch AND update HEAD atomically in a single lock hold.
  /// This avoids separate lock acquisitions for the batch and the head update.
  pub fn flush_batch_and_update_head(&self, batch: WriteBatch, head_hash: &[u8]) -> EngineResult<Vec<u64>> {
    let _operation = self.operation_guard("flush_batch_and_update_head")?;
    self.ensure_writable()?;
    let _namespace = self.namespace_write_guard()?;
    if batch.is_empty() {
      // Still update HEAD even if batch is empty (e.g., system path that skips propagation)
      return self.update_head(head_hash).map(|_| Vec::new());
    }

    let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;

    let mut offsets = Vec::with_capacity(batch.entries.len());
    let mut totals = Vec::with_capacity(batch.entries.len());

    for entry in &batch.entries {
      let (offset, total_length) = writer.append_entry(
        entry.entry_type,
        &entry.key,
        &entry.value,
        0, // flags
      )?;
      kv.set_hot_tail_offset(writer.current_offset());
      offsets.push(offset);
      totals.push(total_length);
    }

    for (i, entry) in batch.entries.iter().enumerate() {
      let kv_entry = KVEntry { type_flags: entry.kv_type, hash: entry.key.clone(), offset: offsets[i], total_length: totals[i] };
      kv.insert(kv_entry)?;
    }

    // Update HEAD and hot_tail_offset in the same lock hold. Inside a
    // transaction this is in-memory only: the durable A/B header must not
    // advertise the new root until the outer transaction has synced WAL and
    // flushed the hot tail. Otherwise a SIGKILL can leave HEAD pointing at
    // FileRecords whose chunks never reached recoverable storage.
    let in_transaction = kv.transaction_depth > 0;
    let mut header = writer.file_header().clone();
    header.head_hash = head_hash.to_vec();
    header.hot_tail_offset = writer.current_offset();
    header.updated_at = chrono::Utc::now().timestamp_millis();
    if in_transaction {
      writer.set_header_in_memory(header);
    } else {
      writer.update_file_header(&header)?;
      self.last_published_hot_tail_offset.store(header.hot_tail_offset, Ordering::Release);
    }

    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    let pending_expansion = kv.needs_expansion.take();
    drop(kv);
    drop(writer);

    if let Some(target_stage) = pending_expansion {
      if let Err(e) = self.expand_kv_block_online(target_stage) {
        tracing::error!("Online KV expansion failed: {}. Will retry on next overflow.", e);
      }
    }

    Ok(offsets)
  }

  /// Get directory content from cache by content hash.
  pub(crate) fn get_cached_dir_content(&self, content_key: &[u8]) -> Option<Vec<u8>> {
    self.dir_content_cache.read().ok()?.get(content_key).cloned()
  }

  /// Cache directory content by content hash.
  pub(crate) fn cache_dir_content(&self, content_key: Vec<u8>, value: Vec<u8>) {
    if let Ok(mut cache) = self.dir_content_cache.write() {
      cache.insert(content_key, value);
    }
  }

  /// Clear the directory content cache (called on snapshot restore).
  pub fn clear_dir_content_cache(&self) {
    if let Ok(mut cache) = self.dir_content_cache.write() {
      cache.clear();
    }
  }

  /// Best-effort sizes of the engine's in-memory caches. Returns
  /// (permissions, index_config, dir_content) entry counts. Used by
  /// soak-test instrumentation to attribute RSS growth to specific caches.
  pub fn engine_cache_sizes(&self) -> (usize, usize, usize) {
    let perms = self.permissions_cache.len();
    let idx = self.index_config_cache.len();
    let dirc = self.dir_content_cache.read().map(|m| m.len()).unwrap_or(0);
    (perms, idx, dirc)
  }

  pub fn memory_stats(&self) -> EngineMemoryStats {
    let process = crate::engine::rss_sampler::read_process_memory();
    let index_stats = self.index_buffer_stats();
    let directory_cache = self.directory_cache_memory_stats();
    let caches = EngineCacheMemoryStats {
      permissions_entries: self.permissions_cache.len(),
      index_config_entries: self.index_config_cache.len(),
      grants_index_entries: self.grants_index_cache.len(),
    };
    let index_cache = IndexCacheMemoryStats {
      cached_indexes: index_stats.cached_indexes,
      dirty_indexes: index_stats.dirty_indexes,
      deleted_indexes: index_stats.deleted_indexes,
      pending_mutations: index_stats.pending_mutations,
      total_mutations: index_stats.mutations,
      flushes: index_stats.flushes,
      flushed_indexes: index_stats.flushed_indexes,
      entries: index_stats.entries,
      values: index_stats.values,
      estimated_bytes: index_stats.estimated_bytes,
      top_cached_indexes: index_stats.top_cached_indexes,
    };
    let estimated_engine_owned_bytes = index_cache.estimated_bytes.saturating_add(directory_cache.estimated_bytes);

    EngineMemoryStats {
      process: ProcessMemoryStats {
        rss_bytes: process.resident_kb.saturating_mul(1024),
        peak_rss_bytes: process.peak_resident_kb.saturating_mul(1024),
        virtual_bytes: process.virtual_kb.saturating_mul(1024),
        data_bytes: process.data_kb.saturating_mul(1024),
        swap_bytes: process.swap_kb.saturating_mul(1024),
        thread_count: process.thread_count,
        fd_count: process.fd_count,
      },
      index_cache,
      directory_cache,
      caches,
      estimated_engine_owned_bytes,
    }
  }

  fn directory_cache_memory_stats(&self) -> DirectoryCacheMemoryStats {
    let Ok(cache) = self.dir_content_cache.read() else {
      return DirectoryCacheMemoryStats::default();
    };
    let entries = cache.len();
    let sample_bytes = cache.iter().next().map(|(key, value)| key.len().saturating_add(value.len())).unwrap_or(64);
    let per_entry = std::mem::size_of::<(Vec<u8>, Vec<u8>)>().saturating_add(sample_bytes);
    DirectoryCacheMemoryStats {
      entries,
      estimated_bytes: std::mem::size_of::<HashMap<Vec<u8>, Vec<u8>>>().saturating_add(entries.saturating_mul(per_entry)) as u64,
    }
  }

  /// Best-effort O(1) metrics for the in-file KV block.
  ///
  /// Returns `(kv_block_size_bytes, kv_fill_ratio)`. The ratio is based on the
  /// current snapshot's live KV entries against the current bucket-page
  /// capacity, so it avoids the old full stats scan while still reflecting
  /// resize pressure.
  pub fn kv_layout_metrics(&self) -> (u64, f64) {
    let kv_size_bytes = match self.writer.read() {
      Ok(writer) => writer.file_header().kv_block_length,
      Err(e) => {
        tracing::error!("writer lock poisoned in kv_layout_metrics(): {}", e);
        0
      }
    };

    let snapshot = self.kv_snapshot.load();
    let capacity = snapshot.bucket_count().saturating_mul(crate::engine::kv_pages::MAX_ENTRIES_PER_PAGE);
    let fill_ratio = if capacity > 0 { snapshot.len() as f64 / capacity as f64 } else { 0.0 };

    (kv_size_bytes, fill_ratio)
  }

  /// Perform online KV block expansion. Called after a KV flush detects
  /// that the block needs to grow (kv.needs_expansion is Some).
  ///
  /// This method acquires BOTH locks and:
  /// 1. Marks resize_in_progress in the file header
  /// 2. Copies WAL entries from the growth zone to end of WAL via the writer
  /// 3. Fsyncs (crash-safe: two copies exist)
  /// 4. Tells the KV store to finalize: zero pages, rehash, update header
  /// 5. Updates the writer's offset to reflect the new file layout
  pub fn expand_kv_block_online(&self, target_stage: usize) -> EngineResult<()> {
    self.ensure_writable()?;
    let hash_length = self.hash_algo.hash_length();
    let psize = crate::engine::kv_pages::page_size(hash_length);
    let (new_block_size, _new_bucket_count) = crate::engine::kv_stages::stage_params(target_stage, psize);

    // Acquire both locks: writer first, then KV
    let mut writer = self.writer.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
    let mut kv = self.kv_writer.lock().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;

    let header = writer.file_header().clone();
    let old_kv_end = header.kv_block_offset + header.kv_block_length;
    let new_kv_end = header.kv_block_offset + new_block_size;
    // Use the writer's current offset (end of WAL) as the actual hot tail position,
    // NOT the header's hot_tail_offset which may be stale.
    let hot_tail_offset = writer.current_offset();

    // The growth zone is [old_kv_end..new_kv_end], but entries may straddle
    // the new_kv_end boundary. We must copy the straddling entry's tail too,
    // otherwise the relocated copy is truncated. Scan forward from new_kv_end
    // to find the first valid entry header — everything before it is the
    // straddling tail that must be included in the copy.
    let magic_bytes = crate::engine::entry_header::ENTRY_MAGIC.to_le_bytes();
    let mut actual_copy_end = new_kv_end;
    let scan_limit = new_kv_end + 1024 * 1024; // 1MB max scan for boundary
    let mut scan_pos = new_kv_end;
    while scan_pos < scan_limit && scan_pos < hot_tail_offset {
      let mut buf = [0u8; 4];
      if writer.read_bytes_at(scan_pos, &mut buf).is_err() {
        break;
      }
      if buf == magic_bytes {
        actual_copy_end = scan_pos;
        break;
      }
      scan_pos += 1;
    }
    if actual_copy_end == new_kv_end {
      // No straddling entry found, or couldn't scan — just use new_kv_end
      actual_copy_end = new_kv_end;
    }
    let growth_zone_size = actual_copy_end - old_kv_end;

    tracing::info!(
      growth_zone_size,
      old_kv_end,
      new_kv_end,
      hot_tail_offset,
      "Online KV expansion: relocating {} bytes of WAL data",
      growth_zone_size,
    );

    // Step 1: Mark resize in progress
    {
      let mut h = header.clone();
      h.resize_in_progress = true;
      h.resize_target_stage = target_stage as u8;
      writer.update_file_header(&h)?;
    }

    // Step 2: Read hot tail into memory, then copy growth zone data to
    // where the hot tail was. The hot tail gets rewritten after.
    let mut hot_payload = writer.read_hot_tail_payload(hot_tail_offset, hash_length);

    // Copy growth zone [old_kv_end .. actual_copy_end] to [hot_tail_offset ..]
    let copy_dst = hot_tail_offset;
    let new_hot_tail = hot_tail_offset + growth_zone_size;
    let offset_delta: i64 = copy_dst as i64 - old_kv_end as i64;

    let adjusted_voids = {
      let mut vm = self.void_manager.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let current_voids = vm.iter().map(|(offset, size)| VoidRecord { offset, size }).collect::<Vec<_>>();
      let adjusted = Self::adjust_voids_for_expansion(current_voids, old_kv_end, actual_copy_end, offset_delta, new_kv_end, new_hot_tail);
      vm.replace_all(adjusted.iter().map(|void| (void.offset, void.size)));
      adjusted
    };
    hot_payload.voids = adjusted_voids.clone();

    writer.copy_region(old_kv_end, copy_dst, growth_zone_size)?;

    // Rewrite hot tail at new position
    writer.write_hot_tail_at(new_hot_tail, &hot_payload, hash_length)?;

    // Step 3: Fsync — two copies exist, crash-safe
    writer.sync()?;

    // The straddling tail region [new_kv_end..actual_copy_end] was relocated
    // to the end of the WAL. Write a void entry over the dead original so
    // the entry scanner doesn't trip over it on restart.
    if actual_copy_end > new_kv_end {
      let tail_size = (actual_copy_end - new_kv_end) as u32;
      let min_void = (crate::engine::entry_header::EntryHeader::FIXED_HEADER_SIZE + self.hash_algo.hash_length()) as u32;
      if tail_size >= min_void {
        writer.write_void_at(new_kv_end, tail_size)?;
      }
    }

    // Update writer's offset to after the relocated data (before new hot tail)
    writer.set_offset(new_hot_tail);

    // Step 4-8: KV finalization (zero pages, rehash, update header, publish snapshot)
    // Use actual_copy_end (not new_kv_end) for offset adjustment range —
    // entries straddling the boundary were also relocated.
    kv.finalize_expansion(target_stage, old_kv_end, actual_copy_end, offset_delta, new_hot_tail, adjusted_voids)?;

    // Update writer's header to match what KV wrote
    let mut final_header = writer.file_header().clone();
    final_header.kv_block_length = new_block_size;
    final_header.kv_block_stage = target_stage as u8;
    final_header.resize_in_progress = false;
    final_header.resize_target_stage = 0;
    final_header.hot_tail_offset = new_hot_tail;
    writer.update_file_header(&final_header)?;
    self.last_published_hot_tail_offset.store(final_header.hot_tail_offset, Ordering::Release);

    // Sync the writer's file handle to ensure it sees the KV's changes
    writer.sync()?;

    tracing::info!("Online KV block expansion complete");

    Ok(())
  }

  /// Check if a KV entry is marked as deleted.
  pub fn is_entry_deleted(&self, hash: &[u8]) -> EngineResult<bool> {
    let _operation = self.operation_guard("is_entry_deleted")?;
    let snapshot = self.kv_snapshot.load();
    match snapshot.get_raw(hash) {
      Some(entry) => Ok(entry.is_deleted()),
      None => Ok(false),
    }
  }

  /// Mark a KV entry as deleted by setting the deleted flag.
  pub fn mark_entry_deleted(&self, hash: &[u8]) -> EngineResult<()> {
    let _operation = self.operation_guard("mark_entry_deleted")?;
    self.ensure_writable()?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let updated = kv.update_flags(hash, KV_FLAG_DELETED);
    if !updated {
      return Err(EngineError::NotFound(format!("Entry not found for hash: {}", hex::encode(hash))));
    }
    Ok(())
  }

  fn resolve_rebuild_records(
    records: Vec<RebuildKvRecord>,
    hash_algo: HashAlgorithm,
    deletion_records: &[(String, RebuildOrder)],
  ) -> EngineResult<Vec<KVEntry>> {
    let mut resolved: HashMap<Vec<u8>, RebuildKvRecord> = HashMap::new();

    for record in records {
      let replace = match resolved.get(&record.hash) {
        Some(existing) => Self::should_replace_rebuild_record(existing, &record),
        None => true,
      };
      if replace {
        resolved.insert(record.hash.clone(), record);
      }
    }

    Self::replay_deletion_records_on_rebuild_records(&mut resolved, hash_algo, deletion_records)?;
    Ok(resolved.into_values().map(|record| record.to_kv_entry()).collect())
  }

  fn should_replace_rebuild_record(existing: &RebuildKvRecord, candidate: &RebuildKvRecord) -> bool {
    let existing_type = existing.type_flags & 0x0F;
    let candidate_type = candidate.type_flags & 0x0F;
    if existing_type == KV_TYPE_DIRECTORY && candidate_type == KV_TYPE_DIRECTORY {
      if candidate.value_length == 0 && existing.value_length > 0 {
        return false;
      }
      if candidate.value_length > 0 && existing.value_length == 0 {
        return true;
      }
    }

    candidate.order.is_after(existing.order)
  }

  fn replay_deletion_records_on_rebuild_records(
    records: &mut HashMap<Vec<u8>, RebuildKvRecord>,
    hash_algo: HashAlgorithm,
    deletion_records: &[(String, RebuildOrder)],
  ) -> EngineResult<()> {
    for (path, deletion_order) in deletion_records {
      let normalized = crate::engine::path_utils::normalize_path(path);

      // File, directory, and symlink deletes all write DeletionRecords keyed
      // by user path. Re-mark only if the current path entry predates the
      // deletion, so delete-then-recreate remains live after rebuild.
      let file_key = crate::engine::directory_ops::file_path_hash(&normalized, &hash_algo)?;
      Self::mark_rebuild_record_deleted_if_older(records, &file_key, *deletion_order);

      let dir_key = crate::engine::directory_ops::directory_path_hash(&normalized, &hash_algo)?;
      Self::mark_rebuild_record_deleted_if_older(records, &dir_key, *deletion_order);

      let symlink_key = crate::engine::symlink_record::symlink_path_hash(&normalized, &hash_algo)?;
      Self::mark_rebuild_record_deleted_if_older(records, &symlink_key, *deletion_order);

      // Some system records (snapshots/forks/GC records) store a domain
      // key string directly in the DeletionRecord path. Preserve that legacy
      // replay path without normalizing the string.
      let raw_key = hash_algo.compute_hash(path.as_bytes())?;
      Self::mark_rebuild_record_deleted_if_older(records, &raw_key, *deletion_order);
    }

    Ok(())
  }

  fn mark_rebuild_record_deleted_if_older(records: &mut HashMap<Vec<u8>, RebuildKvRecord>, key: &[u8], deletion_order: RebuildOrder) {
    if let Some(entry) = records.get_mut(key) {
      if deletion_order.is_after(entry.order) {
        entry.type_flags = (entry.type_flags & 0x0F) | KV_FLAG_DELETED;
      }
    }
  }

  /// Read only the entry header at a given file offset.
  /// Used by GC to determine entry size without reading the full payload.
  pub fn read_entry_header_at(&self, offset: u64) -> EngineResult<EntryHeader> {
    let _operation = self.operation_guard("read_entry_header_at")?;
    // Use a READ lock — read_entry_at_shared uses a cloned file handle.
    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let (wal_start, wal_end) = Self::writer_wal_bounds(&writer);
    if offset < wal_start || offset >= wal_end {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!("entry header offset is outside current WAL region {}..{}", wal_start, wal_end),
      });
    }
    let header = writer.read_entry_header_at_shared(offset)?;
    if !Self::valid_reusable_range(offset, header.total_length, wal_start, wal_end) {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!("entry range is outside current WAL region {}..{}", wal_start, wal_end),
      });
    }
    Ok(header)
  }

  /// Write a DeletionRecord entry at a specific file offset (in-place).
  /// Returns the total bytes written.
  pub fn write_deletion_at(&self, offset: u64, path: &str) -> EngineResult<u32> {
    let _operation = self.operation_guard("write_deletion_at")?;
    self.ensure_writable()?;
    let deletion = crate::engine::deletion_record::DeletionRecord::new(path.to_string(), Some("gc".to_string()));
    let value = deletion.serialize();
    let key = self.compute_hash(format!("del:gc:{}:{}", path, deletion.deleted_at).as_bytes())?;
    let needed = EntryHeader::compute_total_length(self.hash_algo, key.len(), value.len())?;

    let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let (wal_start, wal_end) = Self::writer_wal_bounds(&writer);
    if !Self::valid_reusable_range(offset, needed, wal_start, wal_end) {
      return Err(EngineError::InvalidInput(format!(
        "deletion range {}..{} is outside current WAL region {}..{}",
        offset,
        offset.saturating_add(needed as u64),
        wal_start,
        wal_end
      )));
    }
    writer.write_entry_at(offset, EntryType::DeletionRecord, &key, &value)
  }

  /// Write a void entry at a specific file offset (in-place).
  pub fn write_void_at(&self, offset: u64, size: u32) -> EngineResult<()> {
    let _operation = self.operation_guard("write_void_at")?;
    self.ensure_writable()?;
    let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let (wal_start, wal_end) = Self::writer_wal_bounds(&writer);
    if !Self::valid_reusable_range(offset, size, wal_start, wal_end) {
      return Err(EngineError::InvalidInput(format!(
        "void range {}..{} is outside current WAL region {}..{}",
        offset,
        offset.saturating_add(size as u64),
        wal_start,
        wal_end
      )));
    }
    writer.write_void_at(offset, size)?;

    let mut vm = self.void_manager.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    vm.register_void(offset, size);

    Ok(())
  }

  /// Write a DeletionRecord in-place WITHOUT syncing. Used by GC batch sweep.
  pub fn write_deletion_at_nosync(&self, offset: u64, path: &str) -> EngineResult<u32> {
    let _operation = self.operation_guard("write_deletion_at_nosync")?;
    self.ensure_writable()?;
    let deletion = crate::engine::deletion_record::DeletionRecord::new(path.to_string(), Some("gc".to_string()));
    let value = deletion.serialize();
    let key = self.compute_hash(format!("del:gc:{}:{}", path, deletion.deleted_at).as_bytes())?;
    let needed = EntryHeader::compute_total_length(self.hash_algo, key.len(), value.len())?;

    let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let (wal_start, wal_end) = Self::writer_wal_bounds(&writer);
    if !Self::valid_reusable_range(offset, needed, wal_start, wal_end) {
      return Err(EngineError::InvalidInput(format!(
        "deletion range {}..{} is outside current WAL region {}..{}",
        offset,
        offset.saturating_add(needed as u64),
        wal_start,
        wal_end
      )));
    }
    writer.write_entry_at_nosync(offset, EntryType::DeletionRecord, &key, &value)
  }

  /// Write a void in-place WITHOUT syncing. Used by GC batch sweep.
  pub fn write_void_at_nosync(&self, offset: u64, size: u32) -> EngineResult<()> {
    let _operation = self.operation_guard("write_void_at_nosync")?;
    self.ensure_writable()?;
    let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let (wal_start, wal_end) = Self::writer_wal_bounds(&writer);
    if !Self::valid_reusable_range(offset, size, wal_start, wal_end) {
      return Err(EngineError::InvalidInput(format!(
        "void range {}..{} is outside current WAL region {}..{}",
        offset,
        offset.saturating_add(size as u64),
        wal_start,
        wal_end
      )));
    }
    writer.write_void_at_nosync(offset, size)?;

    let mut vm = self.void_manager.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    vm.register_void(offset, size);

    Ok(())
  }

  /// Sync the append writer to disk. Call after batch nosync operations.
  pub fn sync_writer(&self) -> EngineResult<()> {
    let _operation = self.operation_guard("sync_writer")?;
    self.ensure_writable()?;
    let sync_result = {
      let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
      writer.sync()
    };
    if let Err(error) = sync_result {
      return Err(self.record_durability_failure("Explicit WAL sync failed", error));
    }
    Ok(())
  }

  /// Batch remove multiple entries from the KV store. Publishes snapshot once at the end.
  pub fn remove_kv_entries_batch(&self, hashes: &[Vec<u8>]) -> EngineResult<()> {
    let _operation = self.operation_guard("remove_kv_entries_batch")?;
    self.ensure_writable()?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    kv.mark_deleted_batch(hashes);
    Ok(())
  }

  /// Remove an entry from the KV store (mark deleted). Used by GC sweep.
  pub fn remove_kv_entry(&self, hash: &[u8]) -> EngineResult<()> {
    let _operation = self.operation_guard("remove_kv_entry")?;
    self.ensure_writable()?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    kv.mark_deleted(hash);
    Ok(())
  }

  /// Iterate all live KV entries. Used by GC sweep.
  pub fn iter_kv_entries(&self) -> EngineResult<Vec<KVEntry>> {
    let _operation = self.operation_guard("iter_kv_entries")?;
    let snapshot = self.kv_snapshot.load();
    snapshot.iter_all()
  }

  /// Lightweight single-hash lookup in the KV snapshot.
  /// Returns `None` for deleted or missing entries.
  pub fn get_kv_entry(&self, hash: &[u8]) -> Option<KVEntry> {
    let snapshot = self.kv_snapshot.load();
    snapshot.get(hash)
  }

  /// Return all (key_hash, value) pairs for entries matching a KV type.
  /// Uses the prebuilt type index for O(k) lookup where k is the number of
  /// entries of the target type. Reads each entry's value from disk.
  pub fn entries_by_type(&self, target_type: u8) -> EngineResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let _operation = self.operation_guard("entries_by_type")?;
    let entries: Vec<KVEntry> = {
      let snapshot = self.kv_snapshot.load();
      snapshot.iter_by_type(target_type)
    };

    let mut results = Vec::with_capacity(entries.len());
    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;

    for entry in entries {
      if let Err(e) = Self::validate_kv_entry_offset(&writer, &entry, &entry.hash, "entries_by_type") {
        tracing::warn!("Skipping corrupt KV entry during entries_by_type: {}", e);
        continue;
      }
      let (_header, _key, value) = match writer.read_entry_at_shared(entry.offset) {
        Ok(entry) => entry,
        Err(e) => {
          tracing::warn!("Skipping corrupt entry at offset {} during entries_by_type: {}", entry.offset, e);
          continue;
        }
      };
      results.push((entry.hash, value));
    }

    Ok(results)
  }

  /// Return aggregate statistics about the database including entry counts
  /// by type, file sizes, void space, and timestamps.
  pub fn stats(&self) -> DatabaseStats {
    // 1. Lock writer for file header info and file size
    let (entry_count, created_at, updated_at, db_file_size_bytes, kv_size_bytes) = match self.writer.read() {
      Ok(writer) => {
        let fh = writer.file_header();
        (fh.entry_count, fh.created_at, fh.updated_at, writer.file_size(), fh.kv_block_length)
      }
      Err(e) => {
        tracing::error!("writer lock poisoned in stats(): {}", e);
        (0, 0, 0, 0, 0)
      }
    };

    // 2. Use snapshot for entry counts (lock-free)
    let snapshot = self.kv_snapshot.load();
    let kv_entries = snapshot.len();
    let nvt_buckets = snapshot.bucket_count();

    // Type counts are backed by compact snapshot counters and adjusted for
    // the small live write buffer without cloning every entry of that type.
    let chunk_count = snapshot.count_by_type(KV_TYPE_CHUNK);
    let file_count = snapshot.count_by_type(KV_TYPE_FILE_RECORD);
    let directory_count = snapshot.count_by_type(KV_TYPE_DIRECTORY);
    let snapshot_count = snapshot.count_by_type(KV_TYPE_SNAPSHOT);
    let fork_count = snapshot.count_by_type(KV_TYPE_FORK);

    // 3. Lock void_manager for void stats
    let (void_count, void_space_bytes) = match self.void_manager.read() {
      Ok(vm) => (vm.void_count(), vm.total_void_space()),
      Err(e) => {
        tracing::error!("void_manager lock poisoned in stats(): {}", e);
        (0, 0)
      }
    };

    DatabaseStats {
      entry_count,
      kv_entries,
      kv_size_bytes,
      nvt_buckets,
      nvt_size_bytes: 0, // NVT is internal to DiskKVStore
      chunk_count,
      file_count,
      directory_count,
      snapshot_count,
      fork_count,
      void_count,
      void_space_bytes,
      db_file_size_bytes,
      created_at,
      updated_at,
      hash_algorithm: format!("{:?}", self.hash_algo),
    }
  }

  /// Rebuild the KV index from a full scan of the append log.
  ///
  /// Deletes the existing `.kv` file and creates a fresh one populated from
  /// every entry in the `.aeordb` file. Corrupt entries are skipped with a
  /// warning. The rebuilt KV store is swapped in atomically.
  pub fn rebuild_kv(&self) -> EngineResult<()> {
    self.rebuild_kv_with_progress(None)
  }

  pub fn rebuild_kv_with_progress(&self, progress_callback: Option<EngineStartupProgressCallback>) -> EngineResult<()> {
    self.ensure_writable()?;
    let _mem = crate::engine::rss_sampler::PhaseSampler::start("rebuild_kv", std::time::Duration::from_millis(50));
    tracing::info!("Rebuilding KV index from append log...");
    let timer = std::time::Instant::now();

    let hash_algo = self.hash_algo;

    // Scan the append log (needs read lock on writer)
    // For directory entries, we track value_length so we can prefer
    // entries with children over empty entries (e.g., root directory
    // overwritten by ensure_root_directory on a corrupt session). We also
    // track entry timestamp because WAL offset order stops being chronology
    // once GC reuses lower void ranges for newer writes.
    let (scanned_records, deletion_records): (Vec<RebuildKvRecord>, Vec<(String, RebuildOrder)>) = {
      let writer = self.writer.read().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      tracing::debug!(
        writer_offset = writer.current_offset(),
        file_path = %writer.file_path().display(),
        "rebuild_kv: scanning WAL to EOF (dirty recovery)"
      );
      let mut scanner = writer.scan_entries_dirty_recovery()?;
      let scan_start_offset = scanner.current_offset();
      let scan_total_bytes = scanner.file_length().saturating_sub(scan_start_offset);
      let mut last_progress_log = std::time::Instant::now();
      let scan_timer = std::time::Instant::now();
      let mut collected = Vec::new();
      let mut deletions = Vec::new();
      Self::report_startup_progress(
        &progress_callback,
        EngineStartupProgress {
          phase: "rebuild_kv_scan".to_string(),
          message: "Scanning WAL entries for dirty startup recovery".to_string(),
          current: 0,
          total: Some(scan_total_bytes),
          progress: Some(0.0),
          eta_seconds: None,
        },
      );
      let mut skipped_payload_bytes = 0u64;
      while let Some(result) = scanner.next_rebuild_entry() {
        match result {
          Ok(scanned) => {
            let order = RebuildOrder { timestamp: scanned.header.timestamp, offset: scanned.offset };
            if scanned.header.entry_type == EntryType::DeletionRecord {
              if let Some(value) = scanned.value.as_ref() {
                if let Ok(record) = crate::engine::deletion_record::DeletionRecord::deserialize(value, scanned.header.entry_version) {
                  deletions.push((record.path, order));
                }
              }
            }
            if matches!(scanned.header.entry_type, EntryType::Chunk | EntryType::Void) {
              skipped_payload_bytes = skipped_payload_bytes.saturating_add(scanned.header.value_length as u64);
            }
            collected.push(RebuildKvRecord {
              type_flags: scanned.header.entry_type.to_kv_type(),
              hash: scanned.key.clone(),
              offset: scanned.offset,
              value_length: scanned.header.value_length,
              total_length: scanned.header.total_length,
              order,
            });
          }
          Err(e) => {
            tracing::warn!("Skipping corrupt entry during KV rebuild: {}", e);
          }
        }
        if last_progress_log.elapsed() >= std::time::Duration::from_secs(5) {
          let current = scanner.current_offset();
          let scanned_bytes = current.saturating_sub(scan_start_offset);
          let progress_pct = if scan_total_bytes > 0 { (scanned_bytes as f64 / scan_total_bytes as f64) * 100.0 } else { 100.0 };
          let phase_progress =
            if scan_total_bytes > 0 { ((scanned_bytes as f64 / scan_total_bytes as f64) * 0.80).clamp(0.0, 0.80) } else { 0.80 };
          let eta_seconds = estimate_remaining_seconds(scan_timer.elapsed(), scanned_bytes, scan_total_bytes);
          tracing::info!(
            current_offset = current,
            scanned_bytes,
            total_scan_bytes = scan_total_bytes,
            progress_pct,
            entries_collected = collected.len(),
            deletion_records = deletions.len(),
            skipped_payload_bytes,
            "rebuild_kv: WAL scan progress"
          );
          Self::report_startup_progress(
            &progress_callback,
            EngineStartupProgress {
              phase: "rebuild_kv_scan".to_string(),
              message: "Scanning WAL entries for dirty startup recovery".to_string(),
              current: scanned_bytes,
              total: Some(scan_total_bytes),
              progress: Some(phase_progress),
              eta_seconds,
            },
          );
          last_progress_log = std::time::Instant::now();
        }
      }
      tracing::info!(
        scanned_bytes = scanner.current_offset().saturating_sub(scan_start_offset),
        total_scan_bytes = scan_total_bytes,
        entries_collected = collected.len(),
        deletion_records = deletions.len(),
        skipped_payload_bytes,
        duration_ms = scan_timer.elapsed().as_millis() as u64,
        "rebuild_kv: WAL scan complete"
      );
      (collected, deletions)
    };
    // Writer lock released here
    let scanned_count = scanned_records.len() as u64;
    Self::report_startup_progress(
      &progress_callback,
      EngineStartupProgress {
        phase: "rebuild_kv_resolve".to_string(),
        message: "Resolving latest WAL records for the rebuilt KV index".to_string(),
        current: scanned_count,
        total: Some(scanned_count),
        progress: Some(0.82),
        eta_seconds: None,
      },
    );

    // Read layout info from the file header
    let (kv_block_offset, file_path, existing_stage) = {
      let writer = self.writer.read().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let header = writer.file_header();
      (header.kv_block_offset, writer.file_path().to_path_buf(), header.kv_block_stage as usize)
    };

    let hash_length = hash_algo.hash_length();
    let psize = crate::engine::kv_pages::page_size(hash_length);

    // Determine the true end of the WAL after dirty recovery. We CANNOT
    // trust `writer.current_offset()` here: on a dirty open, it was seeded
    // from the stale on-disk `header.hot_tail_offset`, which is updated
    // only every 100 ms by the hot tail flush timer. Any entry written
    // between the last flush and the crash sits PAST that offset and was
    // just discovered by `scan_entries_dirty_recovery`. If we set
    // hot_tail_offset = writer.current_offset(), header lies about where
    // valid data ends and the next append clobbers the dirty-recovered
    // entries — leaving the KV pointing at offsets whose data has been
    // overwritten (stale KV pattern observed in S2 14-crash soak).
    //
    // The real end of the WAL is one byte past the last byte of the
    // furthest-out entry the scanner returned.
    let dirty_max_end: u64 = scanned_records.iter().map(|record| record.offset + record.total_length as u64).max().unwrap_or(0);
    let wal_end = {
      let writer = self.writer.read().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      writer.current_offset().max(dirty_max_end)
    };

    let (kv_offset, block_size, hot_offset, rebuild_stage) = if kv_block_offset > 0 {
      // Normal single-file layout: KV at head, hot tail after WAL
      let (bs, _) = crate::engine::kv_stages::stage_params(existing_stage, psize);
      (kv_block_offset, bs, wal_end, existing_stage)
    } else {
      // Legacy database (pre single-file refactor): no KV block on disk.
      // Place KV block at the end of the WAL, sized to fit all entries.
      let writer = self.writer.read().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let wal_end = writer.current_offset();
      let target_stage = crate::engine::kv_pages::stage_for_count(scanned_records.len(), hash_length);
      let (bs, _) = crate::engine::kv_stages::stage_params(target_stage, psize);
      tracing::info!("Legacy database: placing KV block at WAL end (offset {}), stage {} ({}B)", wal_end, target_stage, bs);
      (wal_end, bs, wal_end + bs, target_stage)
    };

    tracing::debug!(
      kv_offset,
      block_size,
      hot_offset,
      rebuild_stage,
      wal_end,
      kv_block_offset_from_header = kv_block_offset,
      "rebuild_kv: creating new KV store"
    );

    let kv_file = OpenOptions::new().read(true).write(true).open(&file_path)?;
    let mut new_kv = DiskKVStore::create(kv_file, hash_algo, kv_offset, hot_offset, rebuild_stage)?;

    // Insert all entries with auto-flush disabled. We want a single flush
    // at the end so that page writes don't clobber each other across
    // multiple auto-flush cycles (each auto-flush overwrites the same
    // bucket pages, potentially evicting entries from earlier flushes).
    let resolved_entries = Self::resolve_rebuild_records(scanned_records, hash_algo, &deletion_records)?;
    let inserted_count = resolved_entries.len();
    Self::report_startup_progress(
      &progress_callback,
      EngineStartupProgress {
        phase: "rebuild_kv_insert".to_string(),
        message: "Buffering resolved KV records".to_string(),
        current: 0,
        total: Some(inserted_count as u64),
        progress: Some(0.86),
        eta_seconds: None,
      },
    );
    for kv_entry in &resolved_entries {
      new_kv.buffer_only(kv_entry.clone());
    }

    tracing::debug!(
      inserted = inserted_count,
      write_buffer_len = new_kv.write_buffer_len(),
      deletion_records = deletion_records.len(),
      "rebuild_kv: all entries inserted, replaying deletions and flushing"
    );

    Self::report_startup_progress(
      &progress_callback,
      EngineStartupProgress {
        phase: "rebuild_kv_flush".to_string(),
        message: "Flushing rebuilt KV pages to disk".to_string(),
        current: inserted_count as u64,
        total: Some(inserted_count as u64),
        progress: Some(0.92),
        eta_seconds: None,
      },
    );
    new_kv.flush()?;
    new_kv.adopt_snapshot_handle(Arc::clone(&self.kv_snapshot));

    tracing::debug!(write_buffer_after_flush = new_kv.write_buffer_len(), "rebuild_kv: flush complete");

    // Swap the KV writer. Clear the old KV's write buffer first so its
    // Drop impl doesn't flush stale data over the rebuilt pages.
    let mut kv_lock = self.kv_writer.lock().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
    kv_lock.clear_write_buffer();
    *kv_lock = new_kv;

    // Update the file header with the current hot_tail_offset so the
    // hot tail entries (overflow from the KV page capacity) are found on reopen.
    // ALSO update the writer's in-memory current_offset to match — otherwise
    // the next append starts at the stale pre-crash hot_tail_offset and
    // overwrites the dirty-recovered entries the rebuild just installed
    // into the KV (the "462 stale entries after 14 SIGKILLs" S2 pattern).
    {
      let mut writer = self.writer.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let mut header = writer.file_header().clone();
      let final_stage = kv_lock.stage();
      let (final_block_size, _) = crate::engine::kv_stages::stage_params(final_stage, psize);
      header.kv_block_offset = kv_offset;
      header.kv_block_length = final_block_size;
      // Hot tail goes after the WAL, not after the KV block
      header.hot_tail_offset = wal_end;
      header.entry_count = scanned_count;
      header.kv_block_stage = final_stage as u8;
      writer.set_offset(wal_end);
      tracing::debug!(
        kv_block_offset = header.kv_block_offset,
        kv_block_length = header.kv_block_length,
        hot_tail_offset = header.hot_tail_offset,
        kv_block_stage = header.kv_block_stage,
        entry_count = header.entry_count,
        "rebuild_kv: updating file header"
      );
      writer.update_header(&header)?;
      self.last_published_hot_tail_offset.store(header.hot_tail_offset, Ordering::Release);
    }

    let elapsed = timer.elapsed();
    tracing::info!("KV rebuild complete: {} entries indexed in {:.2}s", inserted_count, elapsed.as_secs_f64());
    Self::report_startup_progress(
      &progress_callback,
      EngineStartupProgress {
        phase: "rebuild_kv_complete".to_string(),
        message: "KV rebuild complete".to_string(),
        current: inserted_count as u64,
        total: Some(inserted_count as u64),
        progress: Some(0.95),
        eta_seconds: Some(0),
      },
    );

    Ok(())
  }

  /// Begin a transaction: increment the KV store's transaction depth so that
  /// `flush()` skips hot-file truncation until the transaction ends.
  pub fn begin_transaction(&self) {
    if let Ok(mut kv) = self.kv_writer.lock() {
      kv.transaction_depth += 1;
    }
  }

  /// End a transaction: decrement the KV store's transaction depth and, when
  /// it reaches zero, truncate the hot file (completing the deferred work
  /// that `flush()` skipped while the transaction was active).
  pub fn end_transaction(&self) -> EngineResult<()> {
    let should_commit = match self.kv_writer.lock() {
      Ok(mut kv) => {
        kv.transaction_depth = kv.transaction_depth.saturating_sub(1);
        kv.transaction_depth == 0
      }
      Err(e) => {
        return Err(EngineError::IoError(std::io::Error::other(format!("Failed to end transaction: {}", e))));
      }
    };

    if !should_commit {
      return Ok(());
    }

    {
      let mut writer = self.writer.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      if let Err(e) = writer.sync() {
        return Err(self.record_durability_failure("Transaction WAL sync failed", e));
      }
    }

    let header_update = match self.kv_writer.lock() {
      Ok(mut kv) => {
        if kv.transaction_depth != 0 {
          Ok(None)
        } else {
          kv.force_flush_hot_buffer().map(|_| Some((kv.hot_tail_offset(), kv.len() as u64)))
        }
      }
      Err(e) => {
        return Err(EngineError::IoError(std::io::Error::other(format!("Failed to flush transaction hot tail: {}", e))));
      }
    };
    let header_update = match header_update {
      Ok(header_update) => header_update,
      Err(e) => return Err(self.record_durability_failure("Failed to flush hot buffer after transaction", e)),
    };

    if let Some((hot_tail_offset, entry_count)) = header_update {
      let header_result = {
        let mut writer = self.writer.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
        let mut header = writer.file_header().clone();
        header.hot_tail_offset = hot_tail_offset;
        header.entry_count = entry_count;
        writer.update_header(&header)
      };
      if let Err(e) = header_result {
        return Err(self.record_durability_failure("Failed to update header after transaction hot-tail flush", e));
      }
      self.last_published_hot_tail_offset.store(hot_tail_offset, Ordering::Release);
    }

    Ok(())
  }

  /// Try to flush the hot buffer if the KV lock is available.
  /// Used by the 100ms timer task — non-blocking, skips if writer is busy.
  ///
  /// Cheap-path early-exit: if the hot buffer is empty, return immediately
  /// without acquiring the writer lock or calling fsync. Without this the
  /// timer was issuing fdatasync 10× per second on an otherwise-idle DB,
  /// which kept spinning HDDs from ever idling down.
  ///
  /// Subtle: the FIRST cut of this gate also checked `write_buffer_len()`,
  /// which is wrong — `write_buffer` lifecycle is independent of hot-tail
  /// durability. `kv.insert()` puts entries into BOTH buffers; the hot
  /// buffer clears every 512 entries (or on this timer), but the write
  /// buffer only flushes to KV pages when it hits `WRITE_BUFFER_THRESHOLD`
  /// (which is much higher) or on explicit flush calls. So after any
  /// past activity, `hot_buffer.is_empty() && write_buffer.len() > 0` is
  /// a normal idle state. Gating on the OR meant we re-wrote the file
  /// header (3 fdatasyncs per cycle) 10×/s indefinitely for any DB that
  /// had ever been written to — kept HDDs spun up forever. Gate ONLY on
  /// the hot buffer: that's what this timer is responsible for.
  pub fn try_flush_hot_buffer(&self) {
    if self.durability_failure().is_some() {
      return;
    }

    // 1. Cheap probe: hot buffer empty? Nothing for this timer to do.
    //    The lock is released before we proceed so the writer is
    //    available for any concurrent write that arrives next.
    let has_pending = match self.kv_writer.try_lock() {
      Ok(kv) => kv.hot_buffer_len() > 0,
      // Couldn't get the lock — a writer is busy; let them finish and we'll
      // pick it up on the next tick.
      Err(_) => return,
    };
    if !has_pending {
      return;
    }

    // 2. Sync WAL data to disk first — entries written since last sync are
    //    in the OS page cache. This must happen BEFORE writing the hot tail,
    //    so that any offsets referenced by the hot tail point to durable data.
    let wal_sync_error = match self.writer.try_write() {
      Ok(mut writer) => writer.sync().err().map(|error| error.to_string()),
      Err(_) => return,
    };
    if let Some(error) = wal_sync_error {
      self.record_durability_failure("Timer WAL sync failed", error);
      return;
    }

    // 3. Flush the hot buffer (re-takes the kv lock; the cheap probe above
    //    has already been released by here). Re-check hot_buffer_len in case
    //    another path flushed between the probe and here.
    let mut flush_error = None;
    let header_update = if let Ok(mut kv) = self.kv_writer.try_lock() {
      if kv.hot_buffer_len() > 0 {
        if let Err(e) = kv.flush_hot_buffer() {
          flush_error = Some(e.to_string());
          None
        } else if kv.transaction_depth == 0 {
          Some((kv.hot_tail_offset(), kv.len() as u64))
        } else {
          None
        }
      } else {
        None
      }
    } else {
      None
    };
    if let Some(error) = flush_error {
      self.record_durability_failure("Timer hot-tail flush failed", error);
      return;
    }

    // Now persist the header with writer lock (kv lock already released)
    if let Some((hot_tail_offset, entry_count)) = header_update {
      let mut header_error = None;
      if let Ok(mut writer) = self.writer.try_write() {
        let mut header = writer.file_header().clone();
        header.hot_tail_offset = hot_tail_offset;
        header.entry_count = entry_count;
        if let Err(e) = writer.update_header(&header) {
          header_error = Some(e.to_string());
        } else {
          self.last_published_hot_tail_offset.store(hot_tail_offset, Ordering::Release);
        }
      }
      if let Some(error) = header_error {
        self.record_durability_failure("Timer header update failed", error);
      }
    }
  }

  /// Gracefully shut down the engine: flush all buffers and sync to disk.
  ///
  /// This is a best-effort operation. Errors during individual flush steps
  /// are logged but do not prevent subsequent steps from executing. The
  /// ordered shutdown sequence is:
  ///
  /// 1. Flush the KV write buffer to disk pages
  /// 2. Flush the hot file buffer (crash-recovery journal)
  /// 3. Sync the WAL file to ensure all OS-buffered writes are durable
  pub fn shutdown(&self) -> EngineResult<()> {
    self.shutdown_with_drain_timeout(Self::shutdown_operation_wait_timeout())
  }

  fn shutdown_with_drain_timeout(&self, initial_drain_timeout: std::time::Duration) -> EngineResult<()> {
    if self.shutdown_complete.load(Ordering::Acquire) {
      tracing::debug!("Storage engine shutdown already complete");
      return Ok(());
    }

    let previous_attempt = self.shutdown_started.swap(true, Ordering::AcqRel);
    tracing::info!("Shutting down storage engine...");
    self.begin_shutdown();
    let _shutdown_operation = self.internal_operation_scope("shutdown");

    let drain_timeout = if previous_attempt { std::time::Duration::ZERO } else { initial_drain_timeout };
    let snapshot = self.wait_for_active_operations(drain_timeout);
    if snapshot.active_operations > 0 {
      tracing::error!(
        active_operations = snapshot.active_operations,
        operations = ?snapshot.operations,
        wait_seconds = drain_timeout.as_secs(),
        repeated_attempt = previous_attempt,
        "Storage engine shutdown blocked by active operations"
      );
      return Err(EngineError::ShuttingDown);
    }

    if self.shutdown_flush_started.swap(true, Ordering::AcqRel) {
      if self.shutdown_complete.load(Ordering::Acquire) {
        return Ok(());
      }
      tracing::warn!("Storage engine shutdown flush is already in progress");
      return Err(EngineError::ShuttingDown);
    }

    let mut first_failure: Option<String> = None;
    let mut record_failure = |context: &str, error: String| {
      let error = self.record_durability_failure(context, error);
      if first_failure.is_none() {
        first_failure = Some(error.to_string());
      }
    };

    if let Err(e) = self.flush_index_buffer() {
      record_failure("Index buffer flush failed during shutdown", e.to_string());
    }

    // Step 1: Flush the KV write buffer to disk pages. Defer durability
    // latching until after the KV mutex is released so emergency spill can
    // snapshot volatile KV state.
    let mut deferred_failures: Vec<(&'static str, String)> = Vec::new();
    match self.kv_writer.lock() {
      Ok(mut kv) => {
        if let Err(e) = kv.flush() {
          deferred_failures.push(("KV flush failed during shutdown", e.to_string()));
        }
        // Step 2: Flush the hot file buffer
        if let Err(e) = kv.flush_hot_buffer() {
          deferred_failures.push(("Hot file flush failed during shutdown", e.to_string()));
        }
      }
      Err(e) => {
        deferred_failures.push(("Could not acquire KV lock during shutdown", e.to_string()));
      }
    }
    for (context, error) in deferred_failures {
      record_failure(context, error);
    }

    // Step 3: Extract KV metadata, then persist header and sync WAL.
    // Extract values from kv_writer BEFORE acquiring writer to avoid
    // nesting kv_writer inside writer (opposite of the timer's order).
    let (hot_tail_offset, entry_count) = match self.kv_writer.lock() {
      Ok(kv) => (kv.hot_tail_offset(), kv.len() as u64),
      Err(e) => {
        record_failure("Could not acquire KV lock for header update during shutdown", e.to_string());
        (0, 0)
      }
    };

    let mut writer_failures: Vec<(&'static str, String)> = Vec::new();
    match self.writer.write() {
      Ok(mut writer) => {
        let mut header = writer.file_header().clone();
        header.hot_tail_offset = hot_tail_offset;
        header.entry_count = entry_count;
        if let Err(e) = writer.update_header(&header) {
          writer_failures.push(("Header update failed during shutdown", e.to_string()));
        } else {
          self.last_published_hot_tail_offset.store(hot_tail_offset, Ordering::Release);
        }
        if let Err(e) = writer.sync_all() {
          writer_failures.push(("WAL sync failed during shutdown", e.to_string()));
        }
      }
      Err(e) => {
        writer_failures.push(("Could not acquire writer lock during shutdown", e.to_string()));
      }
    }
    for (context, error) in writer_failures {
      record_failure(context, error);
    }

    if let Some(failure) = first_failure {
      self.shutdown_flush_started.store(false, Ordering::Release);
      return Err(EngineError::DurabilityFailure(failure));
    }

    tracing::info!("Storage engine shutdown complete");
    self.shutdown_complete.store(true, Ordering::Release);
    Ok(())
  }
}

impl Drop for StorageEngine {
  fn drop(&mut self) {
    if let Err(error) = self.shutdown() {
      tracing::error!("Storage engine drop shutdown failed: {}", error);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serial_test::serial;
  use std::time::{Duration, Instant};

  #[test]
  fn repeated_blocked_shutdown_attempt_does_not_wait_again() {
    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("shutdown-repeat.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let _active_operation = engine.operation_guard("test_active_operation").unwrap();

    let first = engine.shutdown_with_drain_timeout(Duration::ZERO);
    assert!(matches!(first, Err(EngineError::ShuttingDown)));

    let started = Instant::now();
    let second = engine.shutdown_with_drain_timeout(Duration::from_secs(60));
    assert!(matches!(second, Err(EngineError::ShuttingDown)));
    assert!(started.elapsed() < Duration::from_millis(100), "repeated blocked shutdown waited too long");
  }

  #[test]
  #[serial]
  fn durability_failure_latches_read_only_and_spills_volatile_state() {
    let temp_dir = tempfile::tempdir().unwrap();
    let spill_dir = temp_dir.path().join("spill");
    let old_spill_dir = std::env::var_os("AEORDB_EMERGENCY_SPILL_DIR");
    let old_spill_max = std::env::var_os("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES");
    unsafe {
      std::env::set_var("AEORDB_EMERGENCY_SPILL_DIR", &spill_dir);
      std::env::set_var("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES", "1048576");
    }

    struct EnvRestore {
      spill_dir: Option<std::ffi::OsString>,
      spill_max: Option<std::ffi::OsString>,
    }
    impl Drop for EnvRestore {
      fn drop(&mut self) {
        unsafe {
          match &self.spill_dir {
            Some(value) => std::env::set_var("AEORDB_EMERGENCY_SPILL_DIR", value),
            None => std::env::remove_var("AEORDB_EMERGENCY_SPILL_DIR"),
          }
          match &self.spill_max {
            Some(value) => std::env::set_var("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES", value),
            None => std::env::remove_var("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES"),
          }
        }
      }
    }
    let _restore = EnvRestore { spill_dir: old_spill_dir, spill_max: old_spill_max };

    let engine_path = temp_dir.path().join("durability-spill.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    engine.store_entry(EntryType::Chunk, b"chunk-1", b"hello").unwrap();

    let error = engine.record_durability_failure("test forced durability failure", "synthetic EIO");
    assert!(matches!(error, EngineError::DurabilityFailure(_)));
    assert!(engine.durability_failure().is_some());

    let spill = engine.emergency_spill_report().expect("spill report");
    assert!(spill.succeeded, "spill failed: {:?}", spill.errors);
    assert_eq!(spill.hot_tail_writes, 1);
    assert!(spill.hot_tail_path.as_ref().is_some_and(|path| std::fs::metadata(path).is_ok()));
    assert!(spill.manifest_path.as_ref().is_some_and(|path| std::fs::metadata(path).is_ok()));

    let manifest_path = spill.manifest_path.as_ref().unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
    assert_eq!(manifest.get("format").and_then(|value| value.as_str()), Some("aeordb-emergency-spill-v1"));

    assert!(engine.get_entry(b"chunk-1").unwrap().is_some());
    let rejected = engine.store_entry(EntryType::Chunk, b"chunk-2", b"blocked");
    assert!(matches!(rejected, Err(EngineError::DurabilityFailure(_))));
  }
}

thread_local! {
  static NAMESPACE_WRITE_STACK: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
  static ENGINE_OPERATION_STACK: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct NamespaceWriteGuard<'a> {
  engine_id: usize,
  _guard: Option<MutexGuard<'a, ()>>,
}

impl Drop for NamespaceWriteGuard<'_> {
  fn drop(&mut self) {
    NAMESPACE_WRITE_STACK.with(|stack| {
      let mut stack = stack.borrow_mut();
      let popped = stack.pop();
      if popped != Some(self.engine_id) {
        debug_assert_eq!(popped, Some(self.engine_id), "namespace write guard stack out of order");
        if let Some(other_engine_id) = popped {
          stack.push(other_engine_id);
        }
        if let Some(pos) = stack.iter().rposition(|held| *held == self.engine_id) {
          stack.remove(pos);
        }
      }
    });
  }
}

/// RAII guard that begins a transaction on creation and ends it on drop.
///
/// While this guard is alive, `DiskKVStore::flush()` will skip truncating the
/// hot file, ensuring crash recovery can replay all entries written during
/// the transaction. When the guard is dropped, `end_transaction()` is called,
/// which decrements the depth and truncates the hot file if depth reaches 0.
pub struct TransactionGuard<'a> {
  engine: &'a StorageEngine,
}

impl<'a> TransactionGuard<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    engine.begin_transaction();
    TransactionGuard { engine }
  }
}

impl<'a> Drop for TransactionGuard<'a> {
  fn drop(&mut self) {
    if let Err(error) = self.engine.end_transaction() {
      tracing::error!("Transaction guard failed to complete cleanly: {}", error);
    }
  }
}
