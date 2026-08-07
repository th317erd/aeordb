use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::Duration;

use fs2::FileExt;

use arc_swap::ArcSwap;

use crate::engine::append_writer::{read_span_at, AppendWriter};
use crate::engine::cache::{Cache, CleanCache};
use crate::engine::cache_loaders::{PermissionsLoader, IndexConfigLoader};
use crate::engine::compression::CompressionAlgorithm;
use crate::engine::disk_kv_store::DiskKVStore;
use crate::engine::durability_coordinator::{
  DurabilityCoordinator, DurabilityCoordinatorError, DurabilityFailureDisposition, DurabilityHardTurn, DurabilityOperation,
  DurabilityTicket, DurabilityWaiterState, NativeFileBarrierKind, OsErrorClass, RetryClass,
};
use crate::engine::engine_counters::EngineCounters;
use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::file_header::{FILE_HEADER_SIZE, v3_header_commit_plan};
use crate::engine::hot_tail::VoidRecord;
use crate::engine::index_store::{IndexManager, IndexMemoryPolicy, IndexWriteBufferOptions, SharedIndexWriteBuffer};
use crate::engine::kv_rebuild_workspace::{KvRebuildWorkspace, RebuildOrder as WorkspaceRebuildOrder};
use crate::engine::kv_snapshot::ReadSnapshot;
use crate::engine::memory_coordinator::{
  AdmissionClass, CriticalMemoryPurpose, HostMemorySample, MemoryCoordinator, MemoryCoordinatorError, MemoryCoordinatorSnapshot,
  MemoryObservation, MemoryOwner, MemoryPolicy, MemoryReservation,
};
use crate::engine::native_durability::sync_file_all_native;
use crate::engine::operation_memory::OperationMemoryBudget;
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
  pub offset: u64,
  pub total_length: u32,
  pub stored_value_length: u64,
  pub raw_value_length: Option<u64>,
  pub compression_algo: CompressionAlgorithm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkReadLocation {
  pub hash: Vec<u8>,
  pub offset: u64,
  pub total_length: u32,
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
  pub database_id: String,
  pub incident_id: String,
  pub source_location_class: Option<u16>,
  pub creation_sequence: u64,
  pub first_failure_at_ms: i64,
  pub latest_failure_at_ms: i64,
  pub failed_operation: u16,
  pub os_error_class: u16,
  pub os_error_code: i32,
  pub last_selected_header_sequence: u64,
  pub last_durable_write_sequence: u64,
  pub last_durable_publication_sequence: u64,
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

const EMERGENCY_SPILL_BASE_WORKSPACE_BYTES: u64 = 2 * 1024 * 1024;
const SHUTDOWN_BASE_WORKSPACE_BYTES: u64 = 256 * 1024;
const EMERGENCY_SPILL_TEXT_MAX_BYTES: usize = 16 * 1024;
const EMERGENCY_SPILL_ERROR_MAX_COUNT: usize = 32;

struct EmergencyComponentWriter<'a> {
  file: &'a mut std::fs::File,
  hasher: blake3::Hasher,
  length: u64,
}

impl std::io::Write for EmergencyComponentWriter<'_> {
  fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
    let written = self.file.write(buffer)?;
    self.hasher.update(&buffer[..written]);
    self.length =
      self.length.checked_add(written as u64).ok_or_else(|| std::io::Error::other("emergency spill component length overflow"))?;
    Ok(written)
  }

  fn flush(&mut self) -> std::io::Result<()> {
    self.file.flush()
  }
}

#[derive(Debug, Clone, Serialize)]
pub struct DurabilityFailureState {
  pub database_id: [u8; 16],
  pub incident_id: [u8; 16],
  pub creation_sequence: u64,
  pub first_failure_at_ms: i64,
  pub latest_failure_at_ms: i64,
  pub failed_operation: u16,
  pub os_error_class: u16,
  pub os_error_code: i32,
  pub last_selected_header_sequence: u64,
  pub last_durable_write_sequence: u64,
  pub last_durable_publication_sequence: u64,
  pub first_failure: String,
  pub latest_failure: String,
  pub occurrence_count: u64,
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
enum KvRebuildScanBoundary {
  /// Recover uncheckpointed WAL bytes after a missing/corrupt hot tail.
  PhysicalEof,
  /// Trust the selected, validated WAL frontier and ignore later bytes.
  SelectedWal,
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
  maintenance_in_progress: bool,
  active_operations: usize,
  operations: HashMap<&'static str, usize>,
}

pub(crate) struct EngineOperationGuard<'a> {
  tracker: &'a EngineOperationTracker,
  operation: &'static str,
  engine_id: usize,
  counted: bool,
  _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

struct EngineMaintenanceGuard<'a> {
  tracker: &'a EngineOperationTracker,
}

impl EngineOperationTracker {
  fn begin(&self, engine_id: usize, operation: &'static str) -> EngineResult<EngineOperationGuard<'_>> {
    let nested = ENGINE_OPERATION_STACK.with(|stack| stack.borrow().iter().any(|held| *held == engine_id));
    ENGINE_OPERATION_STACK.with(|stack| stack.borrow_mut().push(engine_id));
    if nested {
      return Ok(EngineOperationGuard { tracker: self, operation, engine_id, counted: false, _thread_bound: std::marker::PhantomData });
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
    while state.maintenance_in_progress && !state.shutting_down {
      state = match self.idle.wait(state) {
        Ok(state) => state,
        Err(error) => {
          ENGINE_OPERATION_STACK.with(|stack| {
            stack.borrow_mut().pop();
          });
          return Err(EngineError::IoError(std::io::Error::other(error.to_string())));
        }
      };
    }
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
    Ok(EngineOperationGuard { tracker: self, operation, engine_id, counted: true, _thread_bound: std::marker::PhantomData })
  }

  fn begin_shutdown(&self) {
    if let Ok(mut state) = self.state.lock() {
      state.shutting_down = true;
      self.idle.notify_all();
      if state.active_operations == 0 {
        self.idle.notify_all();
      }
    }
  }

  fn begin_maintenance(&self, engine_id: usize, timeout: std::time::Duration) -> EngineResult<EngineMaintenanceGuard<'_>> {
    let admitted = ENGINE_OPERATION_STACK.with(|stack| stack.borrow().iter().any(|held| *held == engine_id));
    if !admitted {
      return Err(EngineError::InvalidInput("KV layout maintenance requires an admitted engine operation".to_string()));
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut state = self.state.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    if state.shutting_down {
      return Err(EngineError::ShuttingDown);
    }
    if state.maintenance_in_progress {
      return Err(EngineError::InvalidInput("another engine maintenance operation is already in progress".to_string()));
    }
    state.maintenance_in_progress = true;
    while state.active_operations > 1 {
      let now = std::time::Instant::now();
      if now >= deadline {
        state.maintenance_in_progress = false;
        self.idle.notify_all();
        return Err(EngineError::InvalidInput("timed out waiting for active operations before KV layout maintenance".to_string()));
      }
      let remaining = deadline.saturating_duration_since(now);
      let (next, result) =
        self.idle.wait_timeout(state, remaining).map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
      state = next;
      if state.shutting_down {
        state.maintenance_in_progress = false;
        self.idle.notify_all();
        return Err(EngineError::ShuttingDown);
      }
      if result.timed_out() && state.active_operations > 1 {
        state.maintenance_in_progress = false;
        self.idle.notify_all();
        return Err(EngineError::InvalidInput("timed out waiting for active operations before KV layout maintenance".to_string()));
      }
    }
    Ok(EngineMaintenanceGuard { tracker: self })
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
    if state.active_operations == 0 || state.maintenance_in_progress {
      self.tracker.idle.notify_all();
    }
  }
}

impl Drop for EngineMaintenanceGuard<'_> {
  fn drop(&mut self) {
    if let Ok(mut state) = self.tracker.state.lock() {
      state.maintenance_in_progress = false;
      self.tracker.idle.notify_all();
    }
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
  pub evictions: usize,
  pub evicted_indexes: usize,
  pub evicted_bytes: u64,
  pub entries: usize,
  pub values: usize,
  pub estimated_bytes: u64,
  pub estimated_clean_bytes: u64,
  pub estimated_dirty_bytes: u64,
  pub clean_reserved_bytes: u64,
  pub dirty_reserved_bytes: u64,
  pub flush_reserved_bytes: u64,
  pub flushing_indexes: usize,
  pub max_bytes: u64,
  pub mutation_max_bytes: u64,
  pub publication_batch_max_bytes: u64,
  pub clean_ttl_ms: u64,
  pub reservation_owned: bool,
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

fn memory_policy_from_config_shadow(report: &crate::engine::config_resolver::ConfigShadowReport) -> Result<MemoryPolicy, String> {
  let resolution = report
    .resolution
    .as_ref()
    .ok_or_else(|| report.context_error.clone().unwrap_or_else(|| "startup configuration resolution is unavailable".to_string()))?;
  let paths = ["memory.soft_limit_bytes", "memory.hard_limit_bytes", "memory.host_available_floor_bytes", "memory.emergency_reserve_bytes"];
  let blocking = resolution
    .issues
    .iter()
    .filter(|issue| issue.blocking && issue.property.as_deref().is_some_and(|property| paths.contains(&property)))
    .map(|issue| issue.message.as_str())
    .collect::<Vec<_>>();
  if !blocking.is_empty() {
    return Err(format!("memory configuration is unresolved: {}", blocking.join("; ")));
  }
  let unsigned = |path: &str| match resolution.property(path).and_then(|property| property.value.as_ref()) {
    Some(crate::engine::config_resolver::ConfigValue::Unsigned(value)) => Ok(*value),
    Some(value) => Err(format!("{path} resolved to unexpected value {value:?}")),
    None => Err(format!("{path} has no resolved value")),
  };
  MemoryPolicy::new(unsigned(paths[0])?, unsigned(paths[1])?, unsigned(paths[2])?, unsigned(paths[3])?).map_err(|error| error.to_string())
}

fn observation_failed(owner: MemoryOwner, message: impl Into<String>) -> MemoryCoordinatorError {
  MemoryCoordinatorError::ObservationFailed { owner, message: message.into() }
}

fn durability_coordinator_engine_error(error: DurabilityCoordinatorError) -> EngineError {
  match error {
    DurabilityCoordinatorError::ResourceExhausted(message) => EngineError::ResourceExhausted(message),
    other => EngineError::DurabilityFailure(other.to_string()),
  }
}

fn gc_recheck_memory_error(error: MemoryCoordinatorError) -> EngineError {
  match error {
    MemoryCoordinatorError::PolicyUnavailable
    | MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. } => {
      EngineError::ResourceExhausted(format!("GC recheck memory admission failed: {error}"))
    }
    _ => EngineError::IoError(std::io::Error::other(format!("GC recheck memory admission failed: {error}"))),
  }
}

const GC_RECHECK_BYTES_PER_HASH: u64 = 128;

struct GcRecheckState {
  hashes: HashSet<Vec<u8>>,
  reservation: MemoryReservation,
  failure: Option<String>,
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
  database_path: PathBuf,
  configuration_authority: OnceLock<Arc<crate::engine::configuration_authority::ConfigurationAuthority>>,
  memory_coordinator: OnceLock<Arc<MemoryCoordinator>>,
  operation_tracker: EngineOperationTracker,
  shutdown_started: Arc<AtomicBool>,
  shutdown_flush_started: AtomicBool,
  shutdown_complete: AtomicBool,
  durability_failure: Mutex<Option<DurabilityFailureState>>,
  persistent_durability_recovery: Mutex<Option<crate::engine::v4::durability_recovery::PersistentDurabilityRecoveryState>>,
  durability_repair_owner: Mutex<Option<std::thread::ThreadId>>,
  emergency_spill: Mutex<Option<EmergencySpillReport>>,
  last_published_hot_tail_offset: AtomicU64,
  durability_coordinator: Arc<DurabilityCoordinator>,
  namespace_write_lock: Mutex<()>,
  writer: RwLock<AppendWriter>,
  kv_writer: Mutex<DiskKVStore>,
  pub(crate) kv_snapshot: Arc<ArcSwap<ReadSnapshot>>,
  // The VoidManager tracks reclaimable WAL space that can be reused by new
  // writes. Every void must remain outside the file header, KV block, and hot
  // tail; violating that invariant can overwrite storage metadata.
  #[allow(dead_code)]
  pub(crate) void_manager: RwLock<VoidManager>,
  /// Set when a transaction consumes reusable void space and the DiskKVStore
  /// pending-void snapshot needs one deferred refresh before commit.
  void_snapshot_dirty: AtomicBool,
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
  pub(crate) dir_content_cache: CleanCache<Vec<u8>, Vec<u8>>,
  /// Shared in-memory index write buffer. All index mutations pass through
  /// this state and are flushed to disk by write-count/time policy.
  pub(crate) index_write_buffer: Mutex<SharedIndexWriteBuffer>,
  pub(crate) index_flush_guard: Mutex<()>,
  /// GC recheck queue. While GC mark+sweep runs, every successful write hash
  /// is added here so the sweep phase can avoid clobbering entries that were
  /// written after the mark snapshot was captured. `None` means GC is not
  /// active and writes don't bother recording. See bot-docs/plan/gc-mark-sweep.md.
  gc_recheck: Mutex<Option<GcRecheckState>>,
  #[allow(dead_code)]
  _file_lock: std::fs::File,
}

impl StorageEngine {
  fn initialize_configuration_authority(&self) -> EngineResult<()> {
    let startup_state = crate::engine::config_resolver::build_startup_configuration(
      self,
      &self.database_path,
      crate::engine::directory_ops::DEFAULT_CHUNK_SIZE as u64,
    );
    let report = &startup_state.report;
    let complete = report.complete();
    let degraded = report.degraded();
    let blocking_issues =
      report.resolution.as_ref().map(|resolution| resolution.issues.iter().filter(|issue| issue.blocking).count()).unwrap_or(1);
    self
      .configuration_authority
      .set(Arc::new(crate::engine::configuration_authority::ConfigurationAuthority::new(startup_state)))
      .map_err(|_| EngineError::InvalidInput("configuration authority was initialized more than once".to_string()))?;
    tracing::info!(complete, degraded, blocking_issues, "Initialized configuration authority from startup diagnostics");
    Ok(())
  }

  pub fn configuration_shadow(&self) -> Arc<crate::engine::config_resolver::ConfigShadowReport> {
    self.configuration_authority().startup_report()
  }

  pub fn configuration_snapshot(&self) -> Arc<crate::engine::configuration_authority::ConfigurationAuthoritySnapshot> {
    self.configuration_authority().snapshot()
  }

  pub fn replace_configuration_document(
    &self,
    family: crate::engine::config_resolver::ConfigurationFamily,
    bytes: &[u8],
  ) -> EngineResult<Arc<crate::engine::configuration_authority::ConfigurationAuthoritySnapshot>> {
    let authority = self.configuration_authority();
    authority.replace_document(family, bytes, |validated| {
      crate::engine::directory_ops::DirectoryOps::new(self).store_file_buffered(
        &crate::engine::request_context::RequestContext::system(),
        family.path(),
        validated,
        Some("application/json"),
      )?;
      Ok(())
    })
  }

  fn configuration_authority(&self) -> Arc<crate::engine::configuration_authority::ConfigurationAuthority> {
    Arc::clone(self.configuration_authority.get().expect("StorageEngine constructors initialize the configuration authority"))
  }

  fn initialize_memory_coordinator(&self, inherited: Option<Arc<MemoryCoordinator>>) -> EngineResult<()> {
    let coordinator = match inherited {
      Some(coordinator) => {
        tracing::debug!("Storage engine inherited the process memory coordinator");
        coordinator
      }
      None => {
        let report = self.configuration_shadow();
        Arc::new(match memory_policy_from_config_shadow(&report) {
          Ok(policy) => MemoryCoordinator::new(policy),
          Err(reason) => {
            tracing::warn!(%reason, "Memory coordinator started in observation-only mode");
            MemoryCoordinator::without_policy_reason(reason)
          }
        })
      }
    };
    self
      .memory_coordinator
      .set(Arc::clone(&coordinator))
      .map_err(|_| EngineError::InvalidInput("memory coordinator was initialized more than once".to_string()))?;
    self.durability_coordinator.activate_memory_coordinator(coordinator).map_err(durability_coordinator_engine_error)?;
    Ok(())
  }

  fn activate_bounded_kv_pages(&self) -> EngineResult<()> {
    let report = self.configuration_shadow();
    let Some(resolution) = report.resolution.as_ref() else {
      tracing::warn!("Bounded KV residency remains inactive because startup configuration resolution is unavailable");
      return Ok(());
    };
    let max_resident_bytes = match resolution.property("cache.kv_resident_max_bytes").and_then(|property| property.value.as_ref()) {
      Some(crate::engine::config_resolver::ConfigValue::Unsigned(value)) => *value,
      _ => {
        tracing::warn!("Bounded KV residency remains inactive because cache.kv_resident_max_bytes is unresolved");
        return Ok(());
      }
    };
    let coordinator = self.memory_coordinator();
    let policy_available =
      coordinator.snapshot().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?.policy.is_some();
    if !policy_available {
      tracing::warn!("Bounded KV residency remains inactive because the process memory policy is unavailable");
      return Ok(());
    }
    self
      .kv_writer
      .lock()
      .map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?
      .activate_bounded_pages((*coordinator).clone(), max_resident_bytes)
  }

  pub(crate) fn resolved_unsigned_config(&self, path: &str) -> Option<u64> {
    self.configuration_snapshot().resolved_unsigned(path)
  }

  fn activate_bounded_clean_caches(&self) -> EngineResult<()> {
    let directory_max_bytes = self.resolved_unsigned_config("cache.directory_max_bytes").unwrap_or_else(|| {
      tracing::warn!("Directory cache retention is disabled because cache.directory_max_bytes is unresolved");
      0
    });
    let coordinator = self.memory_coordinator();
    let server_cache_max_bytes = coordinator
      .snapshot()
      .map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?
      .policy
      .map(|policy| policy.soft_limit_bytes)
      .unwrap_or_else(|| {
        tracing::warn!("Generic clean-cache retention is disabled because the process memory policy is unavailable");
        0
      });

    self.dir_content_cache.activate_bounded((*coordinator).clone(), MemoryOwner::DirectoryCache, directory_max_bytes)?;
    self.permissions_cache.activate_bounded((*coordinator).clone(), server_cache_max_bytes)?;
    self.index_config_cache.activate_bounded((*coordinator).clone(), server_cache_max_bytes)?;
    self.grants_index_cache.activate_bounded((*coordinator).clone(), server_cache_max_bytes)?;
    self.activate_bounded_index_cache()?;
    Ok(())
  }

  fn activate_bounded_index_cache(&self) -> EngineResult<()> {
    let coordinator = self.memory_coordinator();
    let policy_available =
      coordinator.snapshot().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?.policy.is_some();
    if !policy_available {
      tracing::warn!("Bounded index residency remains inactive because the process memory policy is unavailable");
      return Ok(());
    }
    let required = |path: &str| {
      self
        .resolved_unsigned_config(path)
        .ok_or_else(|| EngineError::InvalidInput(format!("required resolved configuration is unavailable: {path}")))
    };
    let clean_max_bytes = required("cache.index_clean_max_bytes")?;
    let mutation_max_bytes = required("index.mutation_buffer_max_bytes")?;
    let publication_batch_max_bytes = required("index.publication_batch_max_bytes")?;
    let clean_ttl = Duration::from_secs(required("cache.index_clean_ttl_seconds")?);
    let flush_after_writes = usize::try_from(required("index.flush_after_mutations")?)
      .map_err(|_| EngineError::InvalidInput("index.flush_after_mutations does not fit this platform".to_string()))?;
    let flush_after = Duration::from_secs(required("index.flush_after_seconds")?);
    let policy =
      IndexMemoryPolicy::new((*coordinator).clone(), clean_max_bytes, mutation_max_bytes, publication_batch_max_bytes, clean_ttl);
    self
      .index_write_buffer
      .lock()
      .map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?
      .activate_memory_policy(policy, IndexWriteBufferOptions::new(flush_after_writes, flush_after))
  }

  pub fn memory_coordinator(&self) -> Arc<MemoryCoordinator> {
    self.memory_coordinator_if_initialized().expect("StorageEngine constructors initialize the memory coordinator")
  }

  /// Return the process memory coordinator when startup has reached memory
  /// policy activation. Early startup must read configuration and persistent
  /// recovery controls before that policy can be resolved.
  pub(crate) fn memory_coordinator_if_initialized(&self) -> Option<Arc<MemoryCoordinator>> {
    self.memory_coordinator.get().map(Arc::clone)
  }

  pub(crate) fn database_path(&self) -> &Path {
    &self.database_path
  }

  pub fn kv_page_provider_stats(&self) -> EngineResult<Option<crate::engine::kv_page_provider::KvPageProviderStats>> {
    self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?.kv_page_provider_stats()
  }

  pub fn memory_coordinator_snapshot(&self) -> Result<MemoryCoordinatorSnapshot, MemoryCoordinatorError> {
    let coordinator = self.memory_coordinator();
    let process = crate::engine::rss_sampler::read_process_memory();
    coordinator.update_host_sample(HostMemorySample {
      rss_bytes: process.resident_kb.saturating_mul(1024),
      private_bytes: None,
      mapped_bytes: None,
      allocator_bytes: None,
      host_available_bytes: crate::engine::rss_sampler::read_host_available_bytes(),
    })?;
    for (owner, observation) in self.current_memory_observations()? {
      coordinator.observe_legacy(owner, observation)?;
    }
    coordinator.snapshot()
  }

  fn current_memory_observations(&self) -> Result<Vec<(MemoryOwner, MemoryObservation)>, MemoryCoordinatorError> {
    let mut observations =
      MemoryOwner::ALL.into_iter().map(|owner| (owner, MemoryObservation::default())).collect::<std::collections::BTreeMap<_, _>>();

    let snapshot = self.kv_snapshot.load();
    let snapshot_memory = snapshot.memory_stats();
    observations.insert(
      MemoryOwner::KvResidentPages,
      MemoryObservation {
        resident_bytes: snapshot_memory.resident_page_bytes,
        clean_bytes: snapshot_memory.resident_page_bytes,
        pinned_bytes: snapshot_memory.resident_page_bytes,
        items: snapshot.bucket_count() as u64,
        ..Default::default()
      },
    );
    let snapshot_generation_bytes =
      snapshot_memory.snapshot_metadata_bytes.saturating_add(snapshot_memory.buffer_bytes).saturating_add(snapshot_memory.nvt_bytes);
    observations.insert(
      MemoryOwner::KvSnapshotGenerations,
      MemoryObservation {
        resident_bytes: snapshot_generation_bytes,
        clean_bytes: snapshot_memory.snapshot_metadata_bytes.saturating_add(snapshot_memory.nvt_bytes),
        dirty_bytes: snapshot_memory.buffer_bytes,
        pinned_bytes: snapshot_generation_bytes,
        items: 1,
        ..Default::default()
      },
    );
    drop(snapshot);

    let kv_memory =
      self.kv_writer.lock().map_err(|error| observation_failed(MemoryOwner::KvWriteBuffers, error.to_string()))?.memory_stats();
    observations.insert(
      MemoryOwner::KvWriteBuffers,
      MemoryObservation {
        resident_bytes: kv_memory.total_bytes(),
        dirty_bytes: kv_memory.total_bytes(),
        pinned_bytes: kv_memory.total_bytes(),
        items: self.counters.load().snapshot().write_buffer_depth,
        ..Default::default()
      },
    );

    let durability =
      self.durability_coordinator.snapshot().map_err(|error| observation_failed(MemoryOwner::DurabilityWaiters, error.to_string()))?;
    let durability_reservation_owned = self
      .durability_coordinator
      .memory_reservations_active()
      .map_err(|error| observation_failed(MemoryOwner::DurabilityWaiters, error.to_string()))?;
    let durability_bytes = if durability_reservation_owned {
      0
    } else {
      std::mem::size_of_val(&durability).saturating_add(
        durability.ledger.capacity().saturating_mul(std::mem::size_of::<crate::engine::durability_coordinator::DurabilityLedgerEntry>()),
      ) as u64
    };
    observations.insert(
      MemoryOwner::DurabilityWaiters,
      MemoryObservation {
        resident_bytes: durability_bytes,
        dirty_bytes: durability_bytes,
        pinned_bytes: durability_bytes,
        items: durability.ledger.len().saturating_add(durability.pending_hard) as u64,
        ..Default::default()
      },
    );

    let directory = self.dir_content_cache.stats().map_err(|error| observation_failed(MemoryOwner::DirectoryCache, error.to_string()))?;
    let directory_legacy_bytes = if directory.max_bytes.is_some() { 0 } else { directory.resident_bytes };
    observations.insert(
      MemoryOwner::DirectoryCache,
      MemoryObservation {
        resident_bytes: directory_legacy_bytes,
        clean_bytes: directory_legacy_bytes,
        evictable_bytes: directory_legacy_bytes,
        items: directory.entries as u64,
        hits: directory.hits,
        misses: directory.misses,
        evictions: directory.evictions,
        ..Default::default()
      },
    );

    let index =
      self.index_write_buffer.lock().map_err(|error| observation_failed(MemoryOwner::IndexDirtyBuffers, error.to_string()))?.stats();
    let index_clean_legacy_bytes = if index.reservation_owned { 0 } else { index.estimated_clean_bytes };
    let index_dirty_legacy_bytes = if index.reservation_owned { 0 } else { index.estimated_dirty_bytes };
    observations.insert(
      MemoryOwner::IndexCleanCache,
      MemoryObservation {
        resident_bytes: index_clean_legacy_bytes,
        clean_bytes: index_clean_legacy_bytes,
        evictable_bytes: index_clean_legacy_bytes,
        items: index.cached_indexes.saturating_sub(index.dirty_indexes) as u64,
        evictions: index.evictions as u64,
        ..Default::default()
      },
    );
    observations.insert(
      MemoryOwner::IndexDirtyBuffers,
      MemoryObservation {
        resident_bytes: index_dirty_legacy_bytes,
        dirty_bytes: index_dirty_legacy_bytes,
        pinned_bytes: index_dirty_legacy_bytes,
        items: index.dirty_indexes as u64,
        evictions: index.evictions as u64,
        ..Default::default()
      },
    );

    let gc_recheck = self.gc_recheck.lock().map_err(|error| observation_failed(MemoryOwner::GarbageCollection, error.to_string()))?;
    if let Some(state) = gc_recheck.as_ref() {
      observations.insert(
        MemoryOwner::GarbageCollection,
        MemoryObservation {
          // Recheck memory is reservation-owned; reporting it as legacy
          // resident memory here would count it twice.
          items: state.hashes.len() as u64,
          ..Default::default()
        },
      );
    }
    drop(gc_recheck);

    let voids = self.void_manager.read().map_err(|error| observation_failed(MemoryOwner::VoidManager, error.to_string()))?;
    let void_bytes = voids.estimated_memory_bytes();
    observations.insert(
      MemoryOwner::VoidManager,
      MemoryObservation {
        resident_bytes: void_bytes,
        dirty_bytes: void_bytes,
        pinned_bytes: void_bytes,
        items: voids.void_count() as u64,
        ..Default::default()
      },
    );
    drop(voids);

    let permission_cache =
      self.permissions_cache.stats().map_err(|error| observation_failed(MemoryOwner::ServerCaches, error.to_string()))?;
    let index_config_cache =
      self.index_config_cache.stats().map_err(|error| observation_failed(MemoryOwner::ServerCaches, error.to_string()))?;
    let grants_cache = self.grants_index_cache.stats().map_err(|error| observation_failed(MemoryOwner::ServerCaches, error.to_string()))?;
    let server_cache_bytes = [&permission_cache, &index_config_cache, &grants_cache]
      .into_iter()
      .filter(|cache| cache.max_bytes.is_none())
      .fold(0u64, |total, cache| total.saturating_add(cache.resident_bytes));
    observations.insert(
      MemoryOwner::ServerCaches,
      MemoryObservation {
        resident_bytes: server_cache_bytes,
        clean_bytes: server_cache_bytes,
        evictable_bytes: server_cache_bytes,
        items: permission_cache.entries.saturating_add(index_config_cache.entries).saturating_add(grants_cache.entries) as u64,
        hits: permission_cache.hits.saturating_add(index_config_cache.hits).saturating_add(grants_cache.hits),
        misses: permission_cache.misses.saturating_add(index_config_cache.misses).saturating_add(grants_cache.misses),
        evictions: permission_cache.evictions.saturating_add(index_config_cache.evictions).saturating_add(grants_cache.evictions),
        ..Default::default()
      },
    );

    Ok(observations.into_iter().collect())
  }

  fn operation_guard(&self, operation: &'static str) -> EngineResult<EngineOperationGuard<'_>> {
    let engine_id = self as *const StorageEngine as usize;
    self.operation_tracker.begin(engine_id, operation)
  }

  pub(crate) fn query_operation_guard(&self) -> EngineResult<EngineOperationGuard<'_>> {
    self.operation_guard("query")
  }

  pub(crate) fn repair_cancellation(&self) -> Arc<AtomicBool> {
    Arc::clone(&self.shutdown_started)
  }

  pub(crate) fn with_repair_maintenance<T, F>(&self, operation: &'static str, action: F) -> EngineResult<T>
  where
    F: FnOnce() -> EngineResult<T>,
  {
    let _operation = self.operation_guard(operation)?;
    let engine_id = self as *const StorageEngine as usize;
    let _maintenance = self.operation_tracker.begin_maintenance(engine_id, std::time::Duration::from_secs(300))?;
    action()
  }

  fn internal_operation_scope(&self, operation: &'static str) -> EngineOperationGuard<'_> {
    let engine_id = self as *const StorageEngine as usize;
    ENGINE_OPERATION_STACK.with(|stack| stack.borrow_mut().push(engine_id));
    EngineOperationGuard { tracker: &self.operation_tracker, operation, engine_id, counted: false, _thread_bound: std::marker::PhantomData }
  }

  /// Stop accepting new top-level engine operations. Existing operations are
  /// allowed to finish so shutdown can avoid closing under an active DB read
  /// or write.
  pub fn begin_shutdown(&self) {
    self.operation_tracker.begin_shutdown();
  }

  pub fn durability_failure(&self) -> Option<String> {
    let runtime_failure = self
      .durability_failure
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .as_ref()
      .map(|failure| failure.latest_failure.clone());
    runtime_failure
      .or_else(|| self.persistent_durability_recovery().filter(|recovery| recovery.blocks_writes).map(|recovery| recovery.reason))
  }

  pub fn durability_failure_state(&self) -> Option<DurabilityFailureState> {
    self.durability_failure.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
  }

  pub fn persistent_durability_recovery(&self) -> Option<crate::engine::v4::durability_recovery::PersistentDurabilityRecoveryState> {
    self.persistent_durability_recovery.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
  }

  pub fn begin_explicit_durability_repair(&self) -> EngineResult<crate::engine::v4::durability_recovery::ExplicitDurabilityRepair<'_>> {
    crate::engine::v4::durability_recovery::ExplicitDurabilityRepair::begin(self)
  }

  pub fn seed_durability_recovery_from_spills(
    &self,
    artifacts: &[crate::engine::emergency_spill::EmergencySpillArtifact],
  ) -> EngineResult<crate::engine::v4::durability_recovery::DurabilityRecoverySeed> {
    crate::engine::v4::durability_recovery::seed_from_external_spills(self, artifacts)
  }

  pub(crate) fn refresh_persistent_durability_recovery(&self) -> EngineResult<()> {
    let recovery = crate::engine::v4::durability_recovery::inspect_persistent_durability_recovery(self)?;
    *self.persistent_durability_recovery.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = recovery;
    Ok(())
  }

  pub(crate) fn acquire_durability_repair_authority(&self) -> EngineResult<DurabilityRepairAuthorityGuard<'_>> {
    let owner = std::thread::current().id();
    let mut active = self.durability_repair_owner.lock().map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
    if active.is_some() {
      return Err(EngineError::DurabilityFailure("an explicit durability repair session is already active".to_string()));
    }
    *active = Some(owner);
    Ok(DurabilityRepairAuthorityGuard { engine: self, owner })
  }

  fn current_thread_has_durability_repair_authority(&self) -> bool {
    let owner = std::thread::current().id();
    self.durability_repair_owner.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref() == Some(&owner)
  }

  pub fn durability_snapshot(&self) -> EngineResult<crate::engine::durability_coordinator::DurabilityCoordinatorSnapshot> {
    self.durability_coordinator.snapshot().map_err(|error| EngineError::DurabilityFailure(error.to_string()))
  }

  pub fn emergency_spill_report(&self) -> Option<EmergencySpillReport> {
    self.emergency_spill.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
  }

  pub(crate) fn ensure_writable(&self) -> EngineResult<()> {
    let has_repair_authority = self.current_thread_has_durability_repair_authority();
    if let Some(recovery) = self.persistent_durability_recovery() {
      if recovery.blocks_writes && !has_repair_authority {
        return Err(EngineError::DurabilityFailure(format!("database is read-only until explicit repair completes: {}", recovery.reason)));
      }
    }
    if let Some(failure) = self.durability_failure.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref() {
      return Err(EngineError::DurabilityFailure(format!(
        "database is read-only after serious durability failure: {}",
        failure.latest_failure
      )));
    }
    Ok(())
  }

  fn record_durability_failure(&self, operation: DurabilityOperation, context: &str, error: impl std::fmt::Display) -> EngineError {
    let message = format!("{}: {}", context, error);
    let failure_at_ms = chrono::Utc::now().timestamp_millis();
    let coordinator_snapshot = self.durability_coordinator.snapshot().ok();
    let hard_failure = self.durability_coordinator.hard_failure().ok().flatten();
    let creation_sequence = coordinator_snapshot.as_ref().map(|snapshot| snapshot.next_sequence.max(1)).unwrap_or(1);
    let failed_operation = hard_failure.as_ref().map(|failure| failure.operation).unwrap_or(operation).stable_id();
    let os_error_class =
      hard_failure.as_ref().and_then(|failure| failure.os_error_class).unwrap_or(OsErrorClass::OtherPersistentIo).stable_id();
    let os_error_code = -1;
    let last_selected_header_sequence = self.writer.try_read().map(|writer| writer.file_header().sequence.max(1)).unwrap_or(1);
    let durable_sequence = coordinator_snapshot.as_ref().map(|snapshot| snapshot.hard_frontier.max(1)).unwrap_or(1);
    let candidate_database_id = uuid::Uuid::new_v4().into_bytes();
    let candidate_incident_id = uuid::Uuid::new_v4().into_bytes();
    let (first_failure, failure_state) = {
      let mut failure = self.durability_failure.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
      match failure.as_mut() {
        Some(state) => {
          state.latest_failure_at_ms = state.latest_failure_at_ms.max(failure_at_ms);
          state.latest_failure = message.clone();
          state.occurrence_count = state.occurrence_count.saturating_add(1);
          (false, state.clone())
        }
        None => {
          let state = DurabilityFailureState {
            database_id: candidate_database_id,
            incident_id: candidate_incident_id,
            creation_sequence,
            first_failure_at_ms: failure_at_ms,
            latest_failure_at_ms: failure_at_ms,
            failed_operation,
            os_error_class,
            os_error_code,
            last_selected_header_sequence,
            last_durable_write_sequence: durable_sequence,
            last_durable_publication_sequence: durable_sequence,
            first_failure: message.clone(),
            latest_failure: message.clone(),
            occurrence_count: 1,
          };
          *failure = Some(state.clone());
          (true, state)
        }
      }
    };
    if first_failure {
      let spill = self.attempt_emergency_spill(
        context,
        &message,
        failure_state.database_id,
        failure_state.incident_id,
        failure_state.creation_sequence,
        failure_state.first_failure_at_ms,
        failure_state.latest_failure_at_ms,
        failure_state.failed_operation,
        failure_state.os_error_class,
        failure_state.os_error_code,
        failure_state.last_selected_header_sequence,
        failure_state.last_durable_write_sequence,
        failure_state.last_durable_publication_sequence,
      );
      *self.emergency_spill.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(spill.clone());
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
      let manifest_path = self
        .emergency_spill
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(|report| report.manifest_path.clone());
      if let Some(manifest_path) = manifest_path {
        let update = crate::engine::emergency_spill::update_v2_manifest_latest(
          Path::new(&manifest_path),
          failure_state.database_id,
          failure_state.incident_id,
          failure_state.latest_failure_at_ms,
          &failure_state.latest_failure,
        );
        let mut spill = self.emergency_spill.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(report) = spill.as_mut() {
          match update {
            Ok(()) => {
              report.latest_failure_at_ms = failure_state.latest_failure_at_ms;
              report.failure = failure_state.latest_failure.clone();
            }
            Err(error) => Self::push_spill_error(report, format!("failed to update latest durability evidence: {error}")),
          }
        }
      } else {
        let retry = self.attempt_emergency_spill(
          context,
          &failure_state.latest_failure,
          failure_state.database_id,
          failure_state.incident_id,
          failure_state.creation_sequence,
          failure_state.first_failure_at_ms,
          failure_state.latest_failure_at_ms,
          failure_state.failed_operation,
          failure_state.os_error_class,
          failure_state.os_error_code,
          failure_state.last_selected_header_sequence,
          failure_state.last_durable_write_sequence,
          failure_state.last_durable_publication_sequence,
        );
        *self.emergency_spill.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(retry);
      }
      tracing::error!(context, error = %message, "Critical durability failure");
    }
    EngineError::DurabilityFailure(message)
  }

  fn normalize_runtime_write_error(&self, operation: DurabilityOperation, context: &str, error: EngineError) -> EngineError {
    match error {
      EngineError::PostMutationDurabilityFailure(message) => self.record_durability_failure(operation, context, message),
      other => other,
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn new_emergency_spill_report(
    &self,
    context: &str,
    failure: &str,
    database_id: [u8; 16],
    incident_id: [u8; 16],
    creation_sequence: u64,
    first_failure_at_ms: i64,
    latest_failure_at_ms: i64,
    failed_operation: u16,
    os_error_class: u16,
    os_error_code: i32,
    last_selected_header_sequence: u64,
    last_durable_write_sequence: u64,
    last_durable_publication_sequence: u64,
  ) -> EmergencySpillReport {
    EmergencySpillReport {
      database_id: hex::encode(database_id),
      incident_id: hex::encode(incident_id),
      source_location_class: None,
      creation_sequence,
      first_failure_at_ms,
      latest_failure_at_ms,
      failed_operation,
      os_error_class,
      os_error_code,
      last_selected_header_sequence,
      last_durable_write_sequence,
      last_durable_publication_sequence,
      attempted_at: chrono::Utc::now().to_rfc3339(),
      context: Self::bounded_spill_text(context),
      failure: Self::bounded_spill_text(failure),
      succeeded: false,
      spill_directory: None,
      manifest_path: None,
      hot_tail_path: None,
      wal_tail_path: None,
      index_buffer_path: None,
      db_path: Some(self.database_path.display().to_string()),
      hot_tail_writes: 0,
      hot_tail_voids: 0,
      index_pending_mutations: 0,
      index_dirty_saves: 0,
      index_deletes: 0,
      wal_tail_original_start: None,
      wal_tail_copy_start: None,
      wal_tail_end: None,
      wal_tail_bytes: 0,
      wal_tail_truncated: false,
      errors: Vec::new(),
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn attempt_emergency_spill(
    &self,
    context: &str,
    failure: &str,
    database_id: [u8; 16],
    incident_id: [u8; 16],
    creation_sequence: u64,
    first_failure_at_ms: i64,
    latest_failure_at_ms: i64,
    failed_operation: u16,
    os_error_class: u16,
    os_error_code: i32,
    last_selected_header_sequence: u64,
    last_durable_write_sequence: u64,
    last_durable_publication_sequence: u64,
  ) -> EmergencySpillReport {
    let mut report = self.new_emergency_spill_report(
      context,
      failure,
      database_id,
      incident_id,
      creation_sequence,
      first_failure_at_ms,
      latest_failure_at_ms,
      failed_operation,
      os_error_class,
      os_error_code,
      last_selected_header_sequence,
      last_durable_write_sequence,
      last_durable_publication_sequence,
    );
    let mut memory = match OperationMemoryBudget::new(
      self,
      "emergency spill",
      MemoryOwner::EmergencySpill,
      AdmissionClass::Critical(CriticalMemoryPurpose::EmergencySpill),
      EMERGENCY_SPILL_BASE_WORKSPACE_BYTES,
      None,
    ) {
      Ok(memory) => memory,
      Err(error) => {
        Self::push_spill_error(&mut report, format!("emergency spill memory admission failed before volatile-state capture: {error}"));
        return report;
      }
    };
    let db_path = self.database_path.clone();
    let mut wal_tail_original_start = None;
    let mut wal_tail_end = None;
    let mut wal_source = None;

    let current_offset = match self.writer.try_read() {
      Ok(writer) => {
        let header = writer.file_header();
        let wal_start = header.kv_block_offset.saturating_add(header.kv_block_length);
        let current_offset = writer.current_offset();
        let boundary = self.last_published_hot_tail_offset.load(Ordering::Acquire);
        wal_tail_original_start = Some(boundary.max(wal_start).min(current_offset));
        wal_tail_end = Some(current_offset);
        match writer.clone_emergency_reader() {
          Ok(reader) => wal_source = Some(reader),
          Err(error) => Self::push_spill_error(&mut report, format!("failed to clone database handle for emergency WAL spill: {error}")),
        }
        current_offset
      }
      Err(error) => {
        Self::push_spill_error(&mut report, format!("writer lock unavailable for emergency spill: {error}"));
        0
      }
    };

    let payload = match self.kv_writer.try_lock() {
      Ok(mut kv) => {
        let payload_bytes = kv.emergency_hot_tail_payload_memory_bytes();
        match memory.reserve(payload_bytes, "emergency hot-tail snapshot admission failed") {
          Ok(()) => Some(kv.emergency_hot_tail_payload()),
          Err(error) => {
            Self::push_spill_error(&mut report, error.to_string());
            None
          }
        }
      }
      Err(error) => {
        Self::push_spill_error(&mut report, format!("KV lock unavailable for emergency spill: {error}"));
        None
      }
    };
    report.hot_tail_writes = payload.as_ref().map_or(0, |payload| payload.writes.len());
    report.hot_tail_voids = payload.as_ref().map_or(0, |payload| payload.voids.len());
    report.wal_tail_original_start = wal_tail_original_start;
    report.wal_tail_end = wal_tail_end;

    let db_label = db_path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| "unknown-db".to_string());
    let dir_name = format!(
      "{}-{}-{}-{}",
      Self::sanitize_spill_component(&db_label),
      first_failure_at_ms,
      hex::encode(incident_id),
      uuid::Uuid::new_v4().simple()
    );

    for location in crate::engine::emergency_spill::emergency_spill_locations() {
      let spill_dir = location.path.join(&dir_name);
      if let Err(error) = crate::engine::emergency_spill::create_private_dir_all(&location.path) {
        Self::push_spill_error(&mut report, format!("failed to create spill base directory {}: {error}", location.path.display()));
        continue;
      }
      if let Err(error) = crate::engine::emergency_spill::create_private_dir(&spill_dir) {
        Self::push_spill_error(&mut report, format!("failed to create new spill directory {}: {error}", spill_dir.display()));
        continue;
      }
      if let Err(error) = crate::engine::durability::sync_parent_dir(&spill_dir) {
        Self::push_spill_error(&mut report, format!("failed to sync spill parent for {}: {error}", spill_dir.display()));
        continue;
      }

      match self.write_emergency_spill_files(
        &spill_dir,
        location.class,
        Some(&db_path),
        wal_source.as_ref(),
        current_offset,
        payload.as_ref(),
        &mut report,
        &mut memory,
      ) {
        Ok(()) => {
          report.source_location_class = Some(location.class as u16);
          report.succeeded = true;
          return report;
        }
        Err(error) => {
          Self::push_spill_error(&mut report, format!("failed to write emergency spill in {}: {error}", spill_dir.display()));
        }
      }
    }

    report
  }

  fn write_emergency_spill_files(
    &self,
    spill_dir: &Path,
    source_location_class: crate::engine::emergency_spill::SpillLocationClass,
    db_path: Option<&Path>,
    wal_source: Option<&std::fs::File>,
    current_offset: u64,
    payload: Option<&crate::engine::hot_tail::HotTailPayload>,
    report: &mut EmergencySpillReport,
    memory: &mut OperationMemoryBudget,
  ) -> Result<(), String> {
    let db_path = db_path.ok_or_else(|| "database path unavailable for emergency spill identity".to_string())?;
    report.spill_directory = None;
    report.manifest_path = None;
    report.hot_tail_path = None;
    report.wal_tail_path = None;
    report.index_buffer_path = None;
    report.index_pending_mutations = 0;
    report.index_dirty_saves = 0;
    report.index_deletes = 0;
    report.wal_tail_copy_start = None;
    report.wal_tail_bytes = 0;
    report.wal_tail_truncated = false;
    let pending_path = spill_dir.join("pending.json");
    let pending = serde_json::json!({
      "format": crate::engine::emergency_spill::EMERGENCY_SPILL_PENDING_FORMAT_V2,
      "database_id": &report.database_id,
      "incident_id": &report.incident_id,
      "source_location_class": source_location_class as u16,
      "path_encoding": crate::engine::emergency_spill::native_path_encoding(),
      "creation_sequence": report.creation_sequence,
      "first_failure_at_ms": report.first_failure_at_ms,
      "db_path": db_path.display().to_string(),
      "db_path_bytes": hex::encode(crate::engine::emergency_spill::native_path_bytes(db_path)),
    });
    let pending_bytes = serde_json::to_vec_pretty(&pending).map_err(|error| error.to_string())?;
    Self::write_durable_file(&pending_path, &pending_bytes)?;

    let mut components = Vec::new();
    if let Some(payload) = payload {
      let hot_tail_path = spill_dir.join("hot-tail.bin");
      let (length, digest) = Self::write_durable_stream(&hot_tail_path, |writer| {
        crate::engine::hot_tail::write_hot_tail_payload(writer, payload, self.hash_algo.hash_length()).map_err(|error| error.to_string())
      })?;
      components.push(serde_json::json!({
        "kind": "hot_tail",
        "file_name": "hot-tail.bin",
        "length": length,
        "blake3": hex::encode(digest),
      }));
      report.hot_tail_path = Some(hot_tail_path.display().to_string());
    }

    match self.index_write_buffer.try_lock() {
      Ok(mut buffer) => {
        let stats = buffer.emergency_snapshot_stats();
        report.index_pending_mutations = stats.pending_mutations;
        report.index_dirty_saves = stats.dirty_saves;
        report.index_deletes = stats.deletes;
        if stats.pending_mutations > 0 || stats.dirty_saves > 0 || stats.deletes > 0 {
          let index_buffer_path = spill_dir.join("index-buffer.json");
          match Self::write_durable_stream(&index_buffer_path, |writer| {
            buffer.write_emergency_snapshot(writer, self.hash_algo.hash_length(), memory).map(|_| ()).map_err(|error| error.to_string())
          }) {
            Ok((length, digest)) => {
              components.push(serde_json::json!({
                "kind": "index_buffer",
                "file_name": "index-buffer.json",
                "length": length,
                "blake3": hex::encode(digest),
              }));
              report.index_buffer_path = Some(index_buffer_path.display().to_string());
            }
            Err(error) => {
              let cleanup = std::fs::remove_file(&index_buffer_path).err();
              Self::push_spill_error(report, format!("failed to preserve buffered indexes: {error}; partial-file cleanup: {cleanup:?}"));
            }
          }
        }
      }
      Err(error) => Self::push_spill_error(report, format!("index buffer lock unavailable for emergency spill: {error}")),
    }

    if let (Some(original_start), Some(end)) = (report.wal_tail_original_start, report.wal_tail_end) {
      if end > original_start {
        let wal_tail_path = spill_dir.join("wal-tail.bin");
        let Some(wal_source) = wal_source else {
          Self::push_spill_error(
            report,
            format!(
              "failed to copy WAL tail from {} at {}..{}: pinned database handle unavailable",
              db_path.display(),
              original_start,
              end
            ),
          );
          return self.finish_emergency_spill_manifest(spill_dir, source_location_class, db_path, report, components, &pending_path);
        };
        match Self::copy_wal_tail_to_file(wal_source, &wal_tail_path, original_start, end) {
          Ok((copy_start, copied, truncated, digest)) => {
            components.push(serde_json::json!({
              "kind": "wal_tail",
              "file_name": "wal-tail.bin",
              "length": copied,
              "blake3": hex::encode(digest),
            }));
            report.wal_tail_path = Some(wal_tail_path.display().to_string());
            report.wal_tail_copy_start = Some(copy_start);
            report.wal_tail_bytes = copied;
            report.wal_tail_truncated = truncated;
          }
          Err(error) => {
            Self::push_spill_error(
              report,
              format!("failed to copy WAL tail from {} at {}..{}: {error}", db_path.display(), original_start, end),
            );
          }
        }
      } else {
        report.wal_tail_copy_start = Some(current_offset);
      }
    }

    self.finish_emergency_spill_manifest(spill_dir, source_location_class, db_path, report, components, &pending_path)
  }

  fn finish_emergency_spill_manifest(
    &self,
    spill_dir: &Path,
    source_location_class: crate::engine::emergency_spill::SpillLocationClass,
    db_path: &Path,
    report: &mut EmergencySpillReport,
    components: Vec<serde_json::Value>,
    pending_path: &Path,
  ) -> Result<(), String> {
    report.spill_directory = Some(spill_dir.display().to_string());
    let manifest_path = spill_dir.join("manifest.json");
    let manifest = serde_json::json!({
      "format": crate::engine::emergency_spill::EMERGENCY_SPILL_FORMAT_V2,
      "database_id": &report.database_id,
      "incident_id": &report.incident_id,
      "source_location_class": source_location_class as u16,
      "path_encoding": if cfg!(windows) { 2 } else { 1 },
      "creation_sequence": report.creation_sequence,
      "first_failure_at_ms": report.first_failure_at_ms,
      "latest_failure_at_ms": report.latest_failure_at_ms,
      "failed_operation": report.failed_operation,
      "os_error_class": report.os_error_class,
      "os_error_code": report.os_error_code,
      "last_selected_header_sequence": report.last_selected_header_sequence,
      "last_durable_write_sequence": report.last_durable_write_sequence,
      "last_durable_publication_sequence": report.last_durable_publication_sequence,
      "attempted_at": &report.attempted_at,
      "pid": std::process::id(),
      "context": &report.context,
      "failure": &report.failure,
      "first_failure": &report.failure,
      "latest_failure": &report.failure,
      "db_path": &report.db_path,
      "db_path_bytes": hex::encode(crate::engine::emergency_spill::native_path_bytes(db_path)),
      "hash_algorithm": format!("{:?}", self.hash_algo),
      "components": components,
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
    if manifest_bytes.len() as u64 > crate::engine::emergency_spill::MANIFEST_SIZE_CAP {
      return Err(format!(
        "emergency spill manifest is {} bytes, exceeding the {}-byte startup scanner cap",
        manifest_bytes.len(),
        crate::engine::emergency_spill::MANIFEST_SIZE_CAP
      ));
    }
    Self::write_durable_file(&manifest_path, &manifest_bytes)?;
    report.manifest_path = Some(manifest_path.display().to_string());
    if let Err(error) = std::fs::remove_file(&pending_path) {
      Self::push_spill_error(
        report,
        format!("failed to remove completed emergency spill pending record {}: {error}", pending_path.display()),
      );
    } else if let Err(error) = crate::engine::durability::sync_parent_dir(&pending_path) {
      Self::push_spill_error(report, format!("failed to sync pending-record removal for {}: {error}", pending_path.display()));
    }
    Ok(())
  }

  fn bounded_spill_text(value: &str) -> String {
    if value.len() <= EMERGENCY_SPILL_TEXT_MAX_BYTES {
      return value.to_string();
    }
    let mut end = EMERGENCY_SPILL_TEXT_MAX_BYTES;
    while !value.is_char_boundary(end) {
      end -= 1;
    }
    value[..end].to_string()
  }

  fn push_spill_error(report: &mut EmergencySpillReport, error: String) {
    if report.errors.len() >= EMERGENCY_SPILL_ERROR_MAX_COUNT {
      return;
    }
    report.errors.push(Self::bounded_spill_text(&error));
  }

  fn write_durable_stream<F>(path: &Path, write_component: F) -> Result<(u64, [u8; 32]), String>
  where
    F: FnOnce(&mut EmergencyComponentWriter<'_>) -> Result<(), String>,
  {
    let mut file = crate::engine::emergency_spill::create_new_regular_file_no_follow(path).map_err(|error| error.to_string())?;
    let (length, digest) = {
      let mut writer = EmergencyComponentWriter { file: &mut file, hasher: blake3::Hasher::new(), length: 0 };
      write_component(&mut writer)?;
      writer.flush().map_err(|error| error.to_string())?;
      (writer.length, *writer.hasher.finalize().as_bytes())
    };
    sync_file_all_native(&file).map_err(|error| error.to_string())?;
    crate::engine::durability::sync_parent_dir(path).map_err(|error| error.to_string())?;
    Ok((length, digest))
  }

  fn write_durable_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = crate::engine::emergency_spill::create_new_regular_file_no_follow(path).map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    sync_file_all_native(&file).map_err(|error| error.to_string())?;
    crate::engine::durability::sync_parent_dir(path).map_err(|error| error.to_string())
  }

  fn copy_wal_tail_to_file(source: &std::fs::File, destination: &Path, start: u64, end: u64) -> Result<(u64, u64, bool, [u8; 32]), String> {
    if end <= start {
      Self::write_durable_file(destination, &[])?;
      return Ok((start, 0, false, *blake3::hash(&[]).as_bytes()));
    }

    let range = end - start;
    let max_bytes = Self::emergency_wal_spill_max_bytes();
    let (copy_start, truncated) = if max_bytes > 0 && range > max_bytes { (end - max_bytes, true) } else { (start, false) };

    let mut output = crate::engine::emergency_spill::create_new_regular_file_no_follow(destination).map_err(|error| error.to_string())?;

    let mut remaining = end - copy_start;
    let mut copied = 0u64;
    let mut hasher = blake3::Hasher::new();
    while remaining > 0 {
      let read_len = remaining.min(1024 * 1024) as usize;
      let bytes = read_span_at(source, copy_start + copied, read_len).map_err(|error| error.to_string())?;
      output.write_all(&bytes).map_err(|error| error.to_string())?;
      hasher.update(&bytes);
      copied += read_len as u64;
      remaining -= read_len as u64;
    }
    sync_file_all_native(&output).map_err(|error| error.to_string())?;
    crate::engine::durability::sync_parent_dir(destination).map_err(|error| error.to_string())?;
    Ok((copy_start, copied, truncated, *hasher.finalize().as_bytes()))
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

  pub(crate) fn entry_overlaps_current_void(&self, offset: u64, size: u32) -> EngineResult<bool> {
    let voids = self.void_manager.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Ok(voids.overlaps_range(offset, size))
  }

  pub(crate) fn visit_current_voids_for_repair<F>(&self, mut visitor: F) -> EngineResult<()>
  where
    F: FnMut(u64, u32) -> EngineResult<()>,
  {
    let voids = self.void_manager.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    for (offset, size) in voids.iter() {
      visitor(offset, size)?;
    }
    Ok(())
  }

  /// Serialize namespace-level mutations that publish mutable path keys and
  /// directory/HEAD state. The lower writer/KV locks make individual appends
  /// safe, but they do not make a whole file/directory publish atomic against
  /// another namespace writer.
  pub(crate) fn namespace_write_guard(&self) -> EngineResult<NamespaceWriteGuard<'_>> {
    let engine_id = self as *const StorageEngine as usize;
    // Maintenance closes top-level operation admission before it drains the
    // current set. Namespace authority must therefore be admitted before the
    // mutex is acquired: a holder can finish nested work, and every waiter is
    // visible to maintenance before it can become a lock dependency.
    let operation = self.operation_guard("namespace_authority")?;
    let already_held = NAMESPACE_WRITE_STACK.with(|stack| stack.borrow().iter().any(|held| *held == engine_id));
    if already_held {
      NAMESPACE_WRITE_STACK.with(|stack| stack.borrow_mut().push(engine_id));
      return Ok(NamespaceWriteGuard { engine_id, _guard: None, _operation: operation });
    }

    let guard = self.namespace_write_lock.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    NAMESPACE_WRITE_STACK.with(|stack| stack.borrow_mut().push(engine_id));
    Ok(NamespaceWriteGuard { engine_id, _guard: Some(guard), _operation: operation })
  }

  /// Acquire namespace authority for a header publication that creates its
  /// own hard-authority ticket. A transaction admits its ticket before
  /// releasing namespace authority, so a direct publisher must let the
  /// existing hard frontier drain before it can admit another ticket.
  pub(crate) fn direct_hard_authority_guard(&self) -> EngineResult<NamespaceWriteGuard<'_>> {
    let engine_id = self as *const StorageEngine as usize;
    let already_held = NAMESPACE_WRITE_STACK.with(|stack| stack.borrow().iter().any(|held| *held == engine_id));
    if already_held {
      let transaction_active =
        self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?.transaction_depth > 0;
      if !transaction_active {
        let snapshot = self.durability_coordinator.snapshot().map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
        if snapshot.pending_hard > 0 || snapshot.driver_active || snapshot.admitted > 0 || snapshot.executing > 0 {
          return Err(EngineError::DurabilityFailure(
            "direct header publication cannot publish while an earlier hard-authority ticket is pending".to_string(),
          ));
        }
      }
      return self.namespace_write_guard();
    }

    let deadline = std::time::Instant::now() + Self::shutdown_operation_wait_timeout();
    loop {
      self.ensure_writable()?;
      let now = std::time::Instant::now();
      if now >= deadline {
        return Err(EngineError::DurabilityFailure(
          "timed out waiting for the existing hard-authority frontier before direct header publication".to_string(),
        ));
      }
      let remaining = deadline.saturating_duration_since(now);
      let snapshot =
        self.durability_coordinator.wait_until_idle(remaining).map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
      if snapshot.pending_hard > 0 || snapshot.driver_active || snapshot.admitted > 0 || snapshot.executing > 0 {
        return Err(EngineError::DurabilityFailure(
          "timed out waiting for the existing hard-authority frontier before direct header publication".to_string(),
        ));
      }

      let guard = self.namespace_write_guard()?;
      self.ensure_writable()?;
      let snapshot = self.durability_coordinator.snapshot().map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
      if snapshot.pending_hard == 0 && !snapshot.driver_active && snapshot.admitted == 0 && snapshot.executing == 0 {
        return Ok(guard);
      }
      drop(guard);
    }
  }

  /// Non-blocking timer form of [`direct_hard_authority_guard`]. Contention
  /// is normal: the admitted transaction owns the next hard publication, so
  /// the timer simply defers to a later tick.
  fn try_direct_hard_authority_guard(&self) -> EngineResult<Option<NamespaceWriteGuard<'_>>> {
    let engine_id = self as *const StorageEngine as usize;
    let already_held = NAMESPACE_WRITE_STACK.with(|stack| stack.borrow().iter().any(|held| *held == engine_id));
    if already_held {
      let transaction_active =
        self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?.transaction_depth > 0;
      if !transaction_active {
        let snapshot = self.durability_coordinator.snapshot().map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
        if snapshot.pending_hard > 0 || snapshot.driver_active || snapshot.admitted > 0 || snapshot.executing > 0 {
          return Ok(None);
        }
      }
      return self.namespace_write_guard().map(Some);
    }

    let operation = self.operation_guard("namespace_authority")?;
    let guard = match self.namespace_write_lock.try_lock() {
      Ok(guard) => guard,
      Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
      Err(std::sync::TryLockError::Poisoned(error)) => {
        return Err(EngineError::IoError(std::io::Error::other(error.to_string())));
      }
    };
    NAMESPACE_WRITE_STACK.with(|stack| stack.borrow_mut().push(engine_id));
    let authority = NamespaceWriteGuard { engine_id, _guard: Some(guard), _operation: operation };
    self.ensure_writable()?;
    let snapshot = self.durability_coordinator.snapshot().map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
    if snapshot.pending_hard > 0 || snapshot.driver_active || snapshot.admitted > 0 || snapshot.executing > 0 {
      drop(authority);
      return Ok(None);
    }
    Ok(Some(authority))
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

  fn shifted_expansion_offset(offset: u64, offset_delta: i64) -> EngineResult<u64> {
    let shifted = i128::from(offset) + i128::from(offset_delta);
    u64::try_from(shifted).map_err(|_| EngineError::InvalidInput(format!("relocated offset {shifted} cannot be represented as u64")))
  }

  fn expansion_relocation_end(writer: &AppendWriter, old_kv_end: u64, new_kv_end: u64, wal_end: u64) -> EngineResult<u64> {
    if old_kv_end > wal_end {
      return Err(EngineError::CorruptEntry {
        offset: old_kv_end,
        reason: format!("KV block ends after the active WAL frontier at {wal_end}"),
      });
    }

    let overlap_end = new_kv_end.min(wal_end);
    let mut entry_offset = old_kv_end;
    while entry_offset < overlap_end {
      let header = writer.read_entry_header_at_shared(entry_offset).map_err(|error| match error {
        EngineError::CorruptEntry { reason, .. } => {
          EngineError::CorruptEntry { offset: entry_offset, reason: format!("cannot establish KV expansion boundary: {reason}") }
        }
        other => other,
      })?;
      let total_length = u64::from(header.total_length);
      let minimum_length = header.header_size() as u64;
      if total_length < minimum_length {
        return Err(EngineError::CorruptEntry {
          offset: entry_offset,
          reason: format!("WAL entry length {total_length} is smaller than its {minimum_length}-byte header"),
        });
      }
      let entry_end = entry_offset.checked_add(total_length).ok_or_else(|| EngineError::CorruptEntry {
        offset: entry_offset,
        reason: format!("WAL entry length {total_length} overflows its file offset"),
      })?;
      if entry_end > wal_end {
        return Err(EngineError::CorruptEntry {
          offset: entry_offset,
          reason: format!("WAL entry ends at {entry_end}, beyond the active WAL frontier {wal_end}"),
        });
      }
      entry_offset = entry_end;
    }
    Ok(entry_offset)
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

  pub(crate) fn create_with_memory_coordinator(path: &str, coordinator: Arc<MemoryCoordinator>) -> EngineResult<Self> {
    Self::create_internal(path, None, Some(coordinator))
  }

  /// Create a new database file at the given path with an optional hot directory
  /// for crash-recovery write-ahead logging.
  ///
  /// NOTE: `hot_dir` is ignored — hot data is stored in the hot tail at the end
  /// of the main .aeordb file. The parameter is kept for API backward compat.
  pub fn create_with_hot_dir(path: &str, _hot_dir: Option<&Path>) -> EngineResult<Self> {
    Self::create_internal(path, _hot_dir, None)
  }

  fn create_internal(path: &str, _hot_dir: Option<&Path>, inherited_memory: Option<Arc<MemoryCoordinator>>) -> EngineResult<Self> {
    let lock_path = format!("{}.lock", path);
    let lock_file = Self::acquire_file_lock(&lock_path)?;

    let mut writer = AppendWriter::create(Path::new(path))?;
    let hash_algo = writer.file_header().hash_algo;
    let durability_coordinator = writer.durability_coordinator();

    // Open a second file handle for the KV store (same .aeordb file)
    let kv_file = OpenOptions::new().read(true).write(true).open(path)?;
    // v3 layout: data starts after BOTH header slots (HEADER_REGION_SIZE), not
    // just the first one. The two slots make up the A/B double-buffer.
    let kv_block_offset = crate::engine::file_header::HEADER_REGION_SIZE as u64;
    let hash_length = hash_algo.hash_length();
    let kv_block_length = crate::engine::kv_stages::initial_block_size();
    // hot_tail_offset = after header + KV block
    let hot_tail_offset = kv_block_offset + kv_block_length;

    let kv_store =
      DiskKVStore::create_with_coordinator(kv_file, hash_algo, kv_block_offset, hot_tail_offset, 0, Arc::clone(&durability_coordinator))?;

    // Set the append writer's offset past the KV block so WAL entries
    // don't overwrite the KV pages.
    writer.set_offset(hot_tail_offset);

    // Write empty hot tail
    {
      let mut f = OpenOptions::new().read(true).write(true).open(path)?;
      let empty = crate::engine::hot_tail::HotTailPayload::default();
      let end = crate::engine::hot_tail::write_hot_tail(&mut f, hot_tail_offset, &empty, hash_length)?;
      f.set_len(end)?;
      durability_coordinator
        .execute_recoverable_file_barrier(&f, crate::engine::durability_coordinator::NativeFileBarrierKind::Data, end - hot_tail_offset)
        .map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
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
      database_path: PathBuf::from(path),
      configuration_authority: OnceLock::new(),
      memory_coordinator: OnceLock::new(),
      operation_tracker: EngineOperationTracker::default(),
      shutdown_started: Arc::new(AtomicBool::new(false)),
      shutdown_flush_started: AtomicBool::new(false),
      shutdown_complete: AtomicBool::new(false),
      durability_failure: Mutex::new(None),
      persistent_durability_recovery: Mutex::new(None),
      durability_repair_owner: Mutex::new(None),
      emergency_spill: Mutex::new(None),
      last_published_hot_tail_offset: AtomicU64::new(hot_tail_offset),
      durability_coordinator,
      namespace_write_lock: Mutex::new(()),
      writer: RwLock::new(writer),
      kv_writer: Mutex::new(kv_store),
      kv_snapshot,
      void_manager: RwLock::new(void_manager),
      void_snapshot_dirty: AtomicBool::new(false),
      hash_algo,
      counters: ArcSwap::from_pointee(EngineCounters::new()),
      permissions_cache: Arc::new(Cache::new(PermissionsLoader)),
      index_config_cache: Arc::new(Cache::new(IndexConfigLoader)),
      grants_index_cache: Arc::new(Cache::new(crate::engine::grants_index::GrantsIndexLoader)),
      last_auto_snapshot_delete: std::sync::atomic::AtomicI64::new(0),
      last_auto_snapshot_restore: std::sync::atomic::AtomicI64::new(0),
      last_manual_snapshot: std::sync::atomic::AtomicI64::new(0),
      dir_content_cache: CleanCache::new(),
      index_write_buffer: Mutex::new(SharedIndexWriteBuffer::default()),
      index_flush_guard: Mutex::new(()),
      gc_recheck: Mutex::new(None),
      _file_lock: lock_file,
    };
    let initialized = Arc::new(EngineCounters::initialize_from_kv(&engine)?);
    engine.counters.store(initialized);
    engine.initialize_configuration_authority()?;
    engine.initialize_memory_coordinator(inherited_memory)?;
    engine.activate_bounded_clean_caches()?;
    engine.activate_bounded_kv_pages()?;
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
  fn open_internal(
    path: &str,
    _hot_dir: Option<&Path>,
    progress_callback: Option<EngineStartupProgressCallback>,
    inherited_memory: Option<Arc<MemoryCoordinator>>,
  ) -> EngineResult<Self> {
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
    let mut needs_kv_rebuild = false;

    // Sidecar-era databases have no in-file KV block. A short-lived repair
    // path also placed KV pages after the WAL. Normalize either legacy layout
    // through the crash-recoverable resize journal before any KV page handle is
    // opened; creating pages at HEADER_REGION_SIZE here would overwrite the
    // first legacy WAL records before they could be scanned.
    let header_end = crate::engine::file_header::HEADER_REGION_SIZE as u64;
    let needs_initial_kv = !file_header.resize_in_progress
      && ((file_header.kv_block_offset == 0 && file_header.kv_block_length == 0)
        || (file_header.kv_block_offset > header_end && file_header.kv_block_length > 0));
    if needs_initial_kv {
      tracing::info!(
        kv_block_offset = file_header.kv_block_offset,
        kv_block_length = file_header.kv_block_length,
        "Legacy KV layout detected; migrating to the standard in-file layout"
      );
      drop(writer);
      crate::engine::kv_expand::bootstrap_initial_kv_block(path, hash_length)?;
      needs_kv_rebuild = true;
      writer = AppendWriter::open(Path::new(path))?;
      file_header = writer.file_header().clone();
    }

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
    let resize_target = file_header.resize_target_stage as usize;
    let current_stage = file_header.kv_block_stage as usize;
    if file_header.resize_in_progress || resize_target > current_stage {
      tracing::info!(current_stage, resize_target, "Pending KV block expansion detected — expanding before opening");
      // Drop the writer to release the file handle during expansion
      drop(writer);
      let (new_length, new_stage, delta) =
        crate::engine::kv_expand::expand_kv_block(path, resize_target, hash_length).map_err(|error| {
          EngineError::DurabilityFailure(format!(
            "interrupted KV expansion recovery failed; database remains closed and requires explicit repair: {error}"
          ))
        })?;
      tracing::info!(new_length, new_stage, delta, "KV block expansion recovered successfully — will rebuild KV index");
      needs_kv_rebuild = true;
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
    let kv_block_end = kv_block_offset
      .checked_add(file_header.kv_block_length)
      .ok_or_else(|| EngineError::CorruptEntry { offset: kv_block_offset, reason: "KV block end overflows u64".to_string() })?;
    if kv_block_offset != header_end || file_header.kv_block_length == 0 || hot_tail_offset < kv_block_end {
      return Err(EngineError::CorruptEntry {
        offset: kv_block_offset,
        reason: format!(
          "unsupported or overlapping KV layout: expected block at {header_end}, got offset {kv_block_offset}, length {}, hot tail {hot_tail_offset}",
          file_header.kv_block_length
        ),
      });
    }
    let durability_coordinator = writer.durability_coordinator();

    tracing::debug!(
      kv_block_offset,
      kv_block_length = file_header.kv_block_length,
      kv_block_stage,
      hot_tail_offset,
      kv_block_valid = true,
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

    // Open only the validated standard layout. Legacy layouts have already
    // passed through the crash-recoverable bootstrap above, so there is no
    // second startup rebuild path that can bypass bounded external sorting.
    let kv_file = OpenOptions::new().read(true).write(true).open(path)?;
    let kv_store = DiskKVStore::open_with_layout_and_coordinator(
      kv_file,
      hash_algo,
      kv_block_offset,
      file_header.kv_block_length,
      hot_tail_offset,
      kv_block_stage,
      hot_entries,
      hot_voids.clone(),
      file_header.kv_block_version,
      Arc::clone(&durability_coordinator),
    )?;
    // If any bucket page failed CRC on open, the KV index is unreliable for
    // the affected buckets and the WAL becomes the source of truth below.
    let detected_kv_corruption = kv_store.needs_rebuild;

    // Hot tail entries are already loaded into the DiskKVStore write buffer
    // by DiskKVStore::open() — no separate replay step needed.

    let kv_snapshot = Arc::clone(kv_store.snapshot_handle());

    let engine = StorageEngine {
      database_path: PathBuf::from(path),
      configuration_authority: OnceLock::new(),
      memory_coordinator: OnceLock::new(),
      operation_tracker: EngineOperationTracker::default(),
      shutdown_started: Arc::new(AtomicBool::new(false)),
      shutdown_flush_started: AtomicBool::new(false),
      shutdown_complete: AtomicBool::new(false),
      durability_failure: Mutex::new(None),
      persistent_durability_recovery: Mutex::new(None),
      durability_repair_owner: Mutex::new(None),
      emergency_spill: Mutex::new(None),
      last_published_hot_tail_offset: AtomicU64::new(file_header.hot_tail_offset),
      durability_coordinator,
      namespace_write_lock: Mutex::new(()),
      writer: RwLock::new(writer),
      kv_writer: Mutex::new(kv_store),
      kv_snapshot,
      void_manager: RwLock::new(void_manager),
      void_snapshot_dirty: AtomicBool::new(false),
      hash_algo,
      counters: ArcSwap::from_pointee(EngineCounters::new()),
      permissions_cache: Arc::new(Cache::new(PermissionsLoader)),
      index_config_cache: Arc::new(Cache::new(IndexConfigLoader)),
      grants_index_cache: Arc::new(Cache::new(crate::engine::grants_index::GrantsIndexLoader)),
      last_auto_snapshot_delete: std::sync::atomic::AtomicI64::new(0),
      last_auto_snapshot_restore: std::sync::atomic::AtomicI64::new(0),
      last_manual_snapshot: std::sync::atomic::AtomicI64::new(0),
      dir_content_cache: CleanCache::new(),
      index_write_buffer: Mutex::new(SharedIndexWriteBuffer::default()),
      index_flush_guard: Mutex::new(()),
      gc_recheck: Mutex::new(None),
      _file_lock: lock_file,
    };
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
      let scan_boundary = if needs_dirty_startup { KvRebuildScanBoundary::PhysicalEof } else { KvRebuildScanBoundary::SelectedWal };
      engine.rebuild_kv_with_progress_boundary(progress_callback.clone(), scan_boundary)?;
      // Re-initialize counters from the freshly rebuilt KV
      let refreshed = Arc::new(EngineCounters::initialize_from_kv(&engine)?);
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
    } else {
      let initialized = Arc::new(EngineCounters::initialize_from_kv(&engine)?);
      engine.counters.store(initialized);
    }

    // Seed the DiskKVStore's pending_voids snapshot from the loaded
    // VoidManager state so the next hot tail flush carries it forward.
    engine.sync_voids_to_kv_writer()?;
    engine.refresh_persistent_durability_recovery()?;
    engine.initialize_configuration_authority()?;
    engine.initialize_memory_coordinator(inherited_memory)?;
    engine.activate_bounded_clean_caches()?;
    engine.activate_bounded_kv_pages()?;
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
    self.sync_voids_to_kv_writer()?;
    self.force_hot_tail_flush()
  }

  /// Force an immediate hot tail flush. Used by GC sweep after registering
  /// new voids so the void state is durable without waiting for the normal
  /// threshold trigger.
  pub(crate) fn force_hot_tail_flush(&self) -> EngineResult<()> {
    self.ensure_writable()?;
    let _authority = self.direct_hard_authority_guard()?;
    let commit_result = {
      let mut writer = self.writer.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let mut kv = self.kv_writer.lock().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      Self::publish_hot_tail_authority(&mut writer, &mut kv, true)
    };
    match commit_result {
      Ok(Some((hot_tail_offset, _))) => self.last_published_hot_tail_offset.store(hot_tail_offset, Ordering::Release),
      Ok(None) => {}
      Err(error) => return Err(self.record_durability_failure(DurabilityOperation::HeaderAb, "Forced hot-tail hard commit failed", error)),
    }
    Ok(())
  }

  fn publish_hot_tail_authority(writer: &mut AppendWriter, kv: &mut DiskKVStore, force: bool) -> EngineResult<Option<(u64, u64)>> {
    if kv.transaction_depth != 0 || (!force && kv.hot_buffer_len() == 0) {
      return Ok(None);
    }

    let hot_tail_offset = kv.hot_tail_offset();
    let entry_count = kv.len() as u64;
    let estimated_dependency_bytes = kv.pending_hot_tail_bytes();
    let mut header = writer.file_header().clone();
    header.hot_tail_offset = hot_tail_offset;
    header.entry_count = entry_count;
    writer.update_header_with_dependency(&header, estimated_dependency_bytes, || kv.prepare_hot_tail_dependency(force).map(|_| ()))?;
    kv.complete_hot_tail_dependency();
    Ok(Some((hot_tail_offset, entry_count)))
  }

  fn publish_hot_tail_authority_group(
    writer: &mut AppendWriter,
    kv: &mut DiskKVStore,
    tickets: &[DurabilityTicket],
  ) -> EngineResult<Option<(u64, u64)>> {
    if kv.transaction_depth != 0 {
      return Err(EngineError::DurabilityFailure(
        "grouped transaction authority cannot publish while a namespace transaction is active".to_string(),
      ));
    }

    let hot_tail_offset = kv.hot_tail_offset();
    let entry_count = kv.len() as u64;
    let mut header = writer.file_header().clone();
    header.hot_tail_offset = hot_tail_offset;
    header.entry_count = entry_count;
    writer.update_header_group_with_dependency(&header, tickets, || kv.prepare_hot_tail_dependency(true).map(|_| ()))?;
    kv.complete_hot_tail_dependency();
    Ok(Some((hot_tail_offset, entry_count)))
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

  pub fn index_buffer_stats(&self) -> EngineResult<crate::engine::index_store::IndexWriteBufferStats> {
    IndexManager::new(self).buffered_index_stats()
  }

  pub fn evict_clean_index_cache(&self) -> EngineResult<usize> {
    IndexManager::new(self).evict_clean_indexes()
  }

  /// Mirror VoidManager state into the DiskKVStore's pending_voids so the
  /// next hot tail flush includes the current void snapshot. Call after any
  /// operation that changes the void set (GC sweep, void consumption,
  /// startup population). Also refreshes the void_count + void_space
  /// counters so dashboard metrics stay accurate.
  pub(crate) fn sync_voids_to_kv_writer(&self) -> EngineResult<()> {
    let (voids, count, total_bytes) = self.collect_void_snapshot()?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    kv.set_pending_voids(voids);
    self.counters.load().set_void_stats(count, total_bytes);
    Ok(())
  }

  fn collect_void_snapshot(&self) -> EngineResult<(Vec<crate::engine::hot_tail::VoidRecord>, u64, u64)> {
    let vm = self.void_manager.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let count = vm.void_count();
    let mut voids = Vec::new();
    voids.try_reserve_exact(count).map_err(|error| EngineError::ResourceExhausted(format!("void snapshot allocation failed: {error}")))?;
    voids.extend(vm.iter().map(|(offset, size)| crate::engine::hot_tail::VoidRecord { offset, size }));
    Ok((voids, count as u64, vm.total_void_space()))
  }

  /// Open an existing database file.
  ///
  /// Rebuilds the KV index from a full file scan if the `.kv` sidecar is
  /// missing or stale. Does not use a hot directory. Refuses to open patch
  /// databases (`backup_type > 1`).
  pub fn open(path: &str) -> EngineResult<Self> {
    let engine = Self::open_internal(path, None, None, None)?;

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
    let engine = Self::open_internal(path, hot_dir, progress_callback, None)?;

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
    Self::open_internal(path, None, None, None)
  }

  pub(crate) fn open_for_import_with_memory_coordinator(path: &str, coordinator: Arc<MemoryCoordinator>) -> EngineResult<Self> {
    Self::open_internal(path, None, None, Some(coordinator))
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
    let mut void_manager = if Self::can_reuse_void_for_entry(entry_type) {
      Some(self.void_manager.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?)
    } else {
      None
    };
    let void_slot = if let Some(vm) = void_manager.as_mut() {
      loop {
        match vm.find_void(needed) {
          Some((void_offset, void_size)) => {
            voids_changed_via_consume = true;
            if Self::valid_reusable_range(void_offset, void_size, wal_start, wal_end) {
              break Some((void_offset, void_size));
            }
            if void_size > needed {
              vm.remove_void(void_offset.saturating_add(needed as u64));
            }
            tracing::warn!(void_offset, void_size, wal_start, wal_end, "Discarding invalid void outside current WAL region before reuse");
          }
          None => break None,
        }
      }
    } else {
      None
    };

    let (offset, total_length) = if let Some((void_offset, void_size)) = void_slot {
      // In-place write at the void's offset. The void is already removed
      // from void_manager (find_void did it). After this write, the bytes
      // at void_offset belong to the new entry and any remainder must become
      // a complete physical Void entry so sequential WAL scans remain valid.
      //
      // No explicit fsync here — void-consumption writes ride the same
      // hot-tail-flush durability path as appends. The whole point of this
      // plumbing is to AVOID per-entry random fsyncs.
      let write_result = (|| -> EngineResult<u32> {
        let written =
          writer.write_entry_at_nosync_full_with_version(void_offset, entry_type, key, value, flags, compression_algo, entry_version)?;
        if written != needed {
          return Err(EngineError::PostMutationDurabilityFailure(format!(
            "reusable void write encoded {written} bytes but preflight required {needed}"
          )));
        }
        let remainder = void_size.checked_sub(written).ok_or_else(|| {
          EngineError::PostMutationDurabilityFailure(format!(
            "reusable void write of {written} bytes exceeded selected extent size {void_size}"
          ))
        })?;
        if remainder != 0 {
          writer.write_void_at_nosync(void_offset + written as u64, remainder)?;
        }
        Ok(written)
      })();

      match write_result {
        Ok(written) => (void_offset, written),
        Err(error) => {
          if let Some(vm) = void_manager.as_mut() {
            if void_size > needed {
              vm.remove_void(void_offset.saturating_add(needed as u64));
            }
            vm.register_void(void_offset, void_size);
          }
          drop(void_manager);
          drop(kv);
          drop(writer);
          return Err(self.record_durability_failure(
            DurabilityOperation::DataBarrier,
            "Reusable void write failed after in-place mutation may have begun",
            error,
          ));
        }
      }
    } else {
      writer.append_entry_with_compression_and_version(entry_type, key, value, flags, compression_algo, entry_version)?
    };
    kv.set_hot_tail_offset(writer.current_offset());

    // KV insertion may flush at its write/hot-buffer threshold. Publish the
    // updated reusable-space snapshot before that operation so no flush can
    // serialize the old full extent after its prefix became a live entry.
    // Transactions defer the complete snapshot until their outer commit,
    // where all void mutations are collected together.
    if voids_changed_via_consume {
      if kv.transaction_depth > 0 {
        self.void_snapshot_dirty.store(true, Ordering::Release);
      } else if let Some(vm) = void_manager.as_ref() {
        let voids: Vec<crate::engine::hot_tail::VoidRecord> =
          vm.iter().map(|(offset, size)| crate::engine::hot_tail::VoidRecord { offset, size }).collect();
        kv.set_pending_voids(voids);
      }
    }

    let kv_entry = KVEntry { type_flags: entry_type.to_kv_type(), hash: key.to_vec(), offset, total_length };
    if let Err(error) = kv.insert(kv_entry) {
      drop(void_manager);
      drop(kv);
      drop(writer);
      return Err(self.normalize_runtime_write_error(DurabilityOperation::DataBarrier, "KV page flush failed", error));
    }
    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);
    self.record_gc_recheck(key);
    drop(void_manager);

    // Check if KV block needs expansion (set during insert → flush → resize)
    let pending_expansion = Self::take_ready_kv_expansion(&mut kv);

    // Drop locks before expansion (expansion acquires them itself)
    drop(kv);
    drop(writer);

    if let Some(target_stage) = pending_expansion {
      self.execute_kv_expansion_request(target_stage)?;
    }

    Ok(offset)
  }

  fn can_reuse_void_for_entry(entry_type: EntryType) -> bool {
    matches!(entry_type, EntryType::Chunk)
  }

  fn take_ready_kv_expansion(kv: &mut DiskKVStore) -> Option<usize> {
    if kv.transaction_depth == 0 {
      kv.needs_expansion.take()
    } else {
      None
    }
  }

  fn run_ready_kv_expansion(&self) -> EngineResult<()> {
    let pending_expansion = {
      let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
      Self::take_ready_kv_expansion(&mut kv)
    };
    if let Some(target_stage) = pending_expansion {
      self.execute_kv_expansion_request(target_stage)?;
    }
    Ok(())
  }

  fn execute_kv_expansion_request(&self, target_stage: usize) -> EngineResult<()> {
    match self.expand_kv_block_online(target_stage) {
      Ok(()) => Ok(()),
      Err(error) => {
        // Failures after the first layout marker latch the engine read-only and
        // startup recovery owns the unfinished transition. A read-only
        // preflight refusal leaves the current layout authoritative, so retain
        // the request for a later write/transaction boundary instead of losing
        // the only signal that the current pages remain overfull.
        if self.durability_failure().is_none() && target_stage < crate::engine::kv_stages::KV_STAGE_SIZES.len() {
          let mut kv = self.kv_writer.lock().map_err(|lock_error| {
            EngineError::IoError(std::io::Error::other(format!(
              "KV expansion preflight failed ({error}) and its retry request could not be restored: {lock_error}"
            )))
          })?;
          if target_stage > kv.stage() {
            kv.needs_expansion = Some(kv.needs_expansion.map_or(target_stage, |pending| pending.max(target_stage)));
          }
        }
        Err(error)
      }
    }
  }

  /// Record a write into the GC recheck set if GC mark+sweep is active.
  /// No-op otherwise. Cheap: one Mutex acquisition + an Option check.
  fn record_gc_recheck(&self, hash: &[u8]) {
    let mut guard = match self.gc_recheck.lock() {
      Ok(guard) => guard,
      Err(error) => {
        tracing::error!(%error, "GC recheck lock failed after a committed write; active GC will fail closed");
        return;
      }
    };
    let Some(state) = guard.as_mut() else {
      return;
    };
    if state.failure.is_some() || state.hashes.contains(hash) {
      return;
    }
    if let Err(error) = state.reservation.grow(GC_RECHECK_BYTES_PER_HASH) {
      let message = format!("GC recheck memory admission failed after a committed write: {error}");
      tracing::warn!(%error, "GC recheck stopped accepting hashes; sweep will abort without rejecting the write");
      state.failure = Some(message);
      return;
    }
    state.hashes.insert(hash.to_vec());
  }

  /// Begin GC recheck tracking. Subsequent writes have their hashes recorded
  /// into the recheck set. The caller (GC) reads + clears the set via
  /// `take_gc_recheck` between mark and sweep, and again after sweep.
  pub fn begin_gc_recheck(&self) -> EngineResult<()> {
    let reservation =
      self.memory_coordinator().reserve(MemoryOwner::GarbageCollection, 0, AdmissionClass::Maintenance).map_err(gc_recheck_memory_error)?;
    let mut guard = self.gc_recheck.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    if guard.is_some() {
      return Err(EngineError::AlreadyExists("garbage collection recheck tracking is already active".to_string()));
    }
    *guard = Some(GcRecheckState { hashes: HashSet::new(), reservation, failure: None });
    Ok(())
  }

  /// Drain the GC recheck set. Returns the hashes accumulated since the last
  /// call (or since `begin_gc_recheck`). Leaves an empty set in place so
  /// recording continues.
  pub fn take_gc_recheck(&self) -> EngineResult<HashSet<Vec<u8>>> {
    let mut guard = self.gc_recheck.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let Some(state) = guard.as_mut() else {
      return Ok(HashSet::new());
    };
    if let Some(failure) = &state.failure {
      return Err(EngineError::ResourceExhausted(failure.clone()));
    }
    Ok(std::mem::take(&mut state.hashes))
  }

  /// Peek at the GC recheck set without draining. Used during sweep to spare
  /// in-flight writes (writers can still add while we read).
  pub fn gc_recheck_contains(&self, hash: &[u8]) -> EngineResult<bool> {
    let guard = self.gc_recheck.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let Some(state) = guard.as_ref() else {
      return Ok(false);
    };
    if let Some(failure) = &state.failure {
      return Err(EngineError::ResourceExhausted(failure.clone()));
    }
    Ok(state.hashes.contains(hash))
  }

  /// End GC recheck tracking. Writes will no longer record.
  pub fn end_gc_recheck(&self) -> EngineResult<()> {
    match self.gc_recheck.lock() {
      Ok(mut guard) => {
        *guard = None;
        Ok(())
      }
      Err(poisoned) => {
        let mut guard = poisoned.into_inner();
        *guard = None;
        Err(EngineError::IoError(std::io::Error::other("GC recheck lock is poisoned")))
      }
    }
  }

  /// Retrieve an entry by its hash key via a lock-free snapshot read.
  ///
  /// Returns `(header, key, value)` if a non-deleted entry exists.
  pub fn get_entry(&self, hash: &[u8]) -> EngineResult<Option<EntryData>> {
    let _operation = self.operation_guard("get_entry")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get(hash)? {
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
    let kv_entry = match snapshot.get(hash)? {
      Some(entry) if !entry.is_deleted() => entry,
      _ => return Ok(None),
    };

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry_header")?;
    writer.read_entry_header_at_shared(kv_entry.offset).map(Some)
  }

  /// Retrieve only an entry header by key, including entries marked deleted.
  /// Maintenance readers use the encoded lengths to reserve before loading a
  /// historical value.
  pub fn get_entry_header_including_deleted(&self, hash: &[u8]) -> EngineResult<Option<EntryHeader>> {
    let _operation = self.operation_guard("get_entry_header_including_deleted")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get_raw(hash)? {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry_header_including_deleted")?;
    writer.read_entry_header_at_shared(kv_entry.offset).map(Some)
  }

  /// Retrieve an entry by hash, including deleted entries.
  /// Used for version history where we need to read files that were
  /// deleted after a snapshot was taken.
  pub fn get_entry_including_deleted(&self, hash: &[u8]) -> EngineResult<Option<EntryData>> {
    let _operation = self.operation_guard("get_entry_including_deleted")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get_raw(hash)? {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry_including_deleted")?;
    let result = writer.read_entry_at_shared(kv_entry.offset);
    result.map(Some)
  }

  /// Read a live or deleted historical entry only when its value still fits a
  /// caller-owned allocation reservation. A concurrent mutable-key update can
  /// therefore invalidate the first header probe without causing unaccounted
  /// growth.
  pub fn get_entry_including_deleted_bounded(&self, hash: &[u8], maximum_value_length: u32) -> EngineResult<Option<EntryData>> {
    let _operation = self.operation_guard("get_entry_including_deleted_bounded")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get_raw(hash)? {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry_including_deleted_bounded")?;
    let header = writer.read_entry_header_at_shared(kv_entry.offset)?;
    if header.value_length > maximum_value_length {
      return Err(EngineError::ResourceExhausted(format!(
        "historical entry value length {} exceeds reserved bound {}",
        header.value_length, maximum_value_length
      )));
    }
    writer.read_entry_at_shared(kv_entry.offset).map(Some)
  }

  /// Read and verify a live or deleted historical entry only when its value
  /// fits a caller-owned allocation reservation.
  pub(crate) fn get_entry_including_deleted_verified_bounded(
    &self,
    hash: &[u8],
    maximum_value_length: u32,
  ) -> EngineResult<Option<EntryData>> {
    let _operation = self.operation_guard("get_entry_including_deleted_verified_bounded")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get_raw(hash)? {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry_including_deleted_verified_bounded")?;
    let header = writer.read_entry_header_at_shared(kv_entry.offset)?;
    if header.value_length > maximum_value_length {
      return Err(EngineError::ResourceExhausted(format!(
        "historical entry value length {} exceeds reserved bound {}",
        header.value_length, maximum_value_length
      )));
    }
    writer.read_entry_at_shared_verified(kv_entry.offset).map(Some)
  }

  /// Retrieve an entry by hash with BLAKE3 hash verification.
  /// Use this for user-facing reads (GET /files/) where integrity matters.
  /// Internal engine reads use `get_entry()` without verification for performance.
  pub fn get_entry_verified(&self, hash: &[u8]) -> EngineResult<Option<EntryData>> {
    self.get_entry_verified_bounded(hash, u32::MAX)
  }

  /// Retrieve and verify an entry only when its encoded value is within a
  /// caller-owned allocation bound. The header check and value read share one
  /// writer read lock, so a concurrent append cannot invalidate the bound.
  pub fn get_entry_verified_bounded(&self, hash: &[u8], maximum_value_length: u32) -> EngineResult<Option<EntryData>> {
    let _operation = self.operation_guard("get_entry_verified")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get(hash)? {
      Some(entry) if !entry.is_deleted() => entry,
      _ => return Ok(None),
    };

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_entry_verified")?;
    let bounded_header = writer.read_entry_header_at_shared(kv_entry.offset)?;
    if bounded_header.value_length > maximum_value_length {
      return Err(EngineError::InvalidInput(format!(
        "entry value length {} exceeds caller bound {}",
        bounded_header.value_length, maximum_value_length
      )));
    }
    let (header, key, value) = writer.read_entry_at_shared_verified(kv_entry.offset)?;

    Ok(Some((header, key, value)))
  }

  /// Like `get_entry_verified` but includes entries marked as deleted.
  /// Needed for reading historical chunk data when streaming files from snapshots.
  pub fn get_entry_verified_including_deleted(&self, hash: &[u8]) -> EngineResult<Option<EntryData>> {
    let _operation = self.operation_guard("get_entry_verified_including_deleted")?;
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get_raw(hash)? {
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

  fn decode_chunk_entry_bounded(
    &self,
    requested_hash: &[u8],
    header: EntryHeader,
    value: Vec<u8>,
    maximum_decoded_length: usize,
  ) -> EngineResult<Vec<u8>> {
    if header.entry_type != EntryType::Chunk {
      return Err(EngineError::InvalidInput(format!("Hash {} is not a chunk entry", hex::encode(requested_hash))));
    }
    crate::engine::compression::decompress_bounded(&value, header.compression_algo, maximum_decoded_length)
  }

  fn decode_verified_chunk_entry_from_buffer(
    &self,
    requested_hash: &[u8],
    entry_offset: u64,
    entry_buffer: &[u8],
  ) -> EngineResult<Vec<u8>> {
    if entry_buffer.len() < EntryHeader::FIXED_HEADER_SIZE {
      return Err(EngineError::CorruptEntry {
        offset: entry_offset,
        reason: format!("Entry buffer too short: {} bytes", entry_buffer.len()),
      });
    }

    let hash_algo_raw = u16::from_le_bytes([entry_buffer[7], entry_buffer[8]]);
    let hash_algo = HashAlgorithm::from_u16(hash_algo_raw).ok_or(EngineError::InvalidHashAlgorithm(hash_algo_raw))?;
    let full_header_size = EntryHeader::FIXED_HEADER_SIZE + hash_algo.hash_length();
    if entry_buffer.len() < full_header_size {
      return Err(EngineError::CorruptEntry {
        offset: entry_offset,
        reason: format!("Entry buffer too short for full header: {} bytes", entry_buffer.len()),
      });
    }

    let mut cursor = Cursor::new(&entry_buffer[..full_header_size]);
    let header = EntryHeader::deserialize(&mut cursor)?;
    if header.total_length as usize != entry_buffer.len() {
      return Err(EngineError::CorruptEntry {
        offset: entry_offset,
        reason: format!("Entry total_length {} does not match buffered length {}", header.total_length, entry_buffer.len()),
      });
    }

    let header_size = header.header_size() as u64;
    let payload_size = header.key_length as u64 + header.value_length as u64;
    let max_payload = (header.total_length as u64).saturating_sub(header_size);
    if payload_size > max_payload {
      return Err(EngineError::CorruptEntry {
        offset: entry_offset,
        reason: format!(
          "key_length ({}) + value_length ({}) exceeds total_length ({}) minus header ({})",
          header.key_length, header.value_length, header.total_length, header_size,
        ),
      });
    }

    let key_start = full_header_size;
    let key_end = key_start + header.key_length as usize;
    let value_end = key_end + header.value_length as usize;
    if value_end > entry_buffer.len() {
      return Err(EngineError::CorruptEntry {
        offset: entry_offset,
        reason: format!("Entry payload extends past buffered length {}", entry_buffer.len()),
      });
    }

    let key = entry_buffer[key_start..key_end].to_vec();
    let value = entry_buffer[key_end..value_end].to_vec();
    if key.as_slice() != requested_hash {
      return Err(EngineError::CorruptEntry {
        offset: entry_offset,
        reason: format!("Chunk key mismatch: expected {}, found {}", hex::encode(requested_hash), hex::encode(&key)),
      });
    }

    if !header.verify(&key, &value) {
      return Err(EngineError::CorruptEntry {
        offset: entry_offset,
        reason: format!("Hash verification failed for entry at offset {}. Data may be corrupt.", entry_offset),
      });
    }

    self.decode_chunk_entry(requested_hash, header, value)
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
    self.get_chunk_metadata_internal(hash, false)
  }

  /// Return header-accurate metadata for read planning. Unlike the commit-side
  /// metadata shortcut, this reports an unknown decoded length for compressed
  /// chunks so callers cannot mistake stored bytes for logical file bytes.
  pub(crate) fn get_chunk_stream_metadata(&self, hash: &[u8], include_deleted: bool) -> EngineResult<Option<ChunkEntryMetadata>> {
    let _operation = self.operation_guard("get_chunk_stream_metadata")?;
    let snapshot = self.kv_snapshot.load();
    let Some(kv_entry) = snapshot.get(hash)? else {
      return Ok(None);
    };
    if kv_entry.is_deleted() && !include_deleted {
      return Ok(None);
    }
    if kv_entry.entry_type() != KV_TYPE_CHUNK {
      return Err(EngineError::InvalidInput(format!("Hash {} is not a chunk entry", hex::encode(hash))));
    }

    let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    Self::validate_kv_entry_offset(&writer, &kv_entry, hash, "get_chunk_stream_metadata")?;
    let header = writer.read_entry_header_at_shared(kv_entry.offset)?;
    if header.entry_type != EntryType::Chunk {
      return Err(EngineError::CorruptEntry {
        offset: kv_entry.offset,
        reason: format!("Chunk KV row points to {:?} entry", header.entry_type),
      });
    }
    if header.total_length != kv_entry.total_length {
      return Err(EngineError::CorruptEntry {
        offset: kv_entry.offset,
        reason: format!("Chunk header length {} does not match KV length {}", header.total_length, kv_entry.total_length),
      });
    }

    let stored_value_length = header.value_length as u64;
    let raw_value_length = (header.compression_algo == CompressionAlgorithm::None).then_some(stored_value_length);
    Ok(Some(ChunkEntryMetadata {
      offset: kv_entry.offset,
      total_length: kv_entry.total_length,
      stored_value_length,
      raw_value_length,
      compression_algo: header.compression_algo,
    }))
  }

  fn get_chunk_metadata_internal(&self, hash: &[u8], include_deleted: bool) -> EngineResult<Option<ChunkEntryMetadata>> {
    let _operation = self.operation_guard("get_chunk_metadata")?;
    let snapshot = self.kv_snapshot.load();
    let Some(kv_entry) = snapshot.get(hash)? else {
      return Ok(None);
    };
    if kv_entry.is_deleted() && !include_deleted {
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
      offset: kv_entry.offset,
      total_length: kv_entry.total_length,
      stored_value_length,
      raw_value_length: Some(stored_value_length),
      compression_algo: CompressionAlgorithm::None,
    }))
  }

  /// Read multiple live chunks from one WAL span and verify each entry.
  ///
  /// The caller decides which chunks are close enough to coalesce. This method
  /// revalidates that each chunk is still the live KV entry at the expected
  /// offset, then performs one offset-based read over the covering span.
  pub fn read_chunk_span_verified(&self, locations: &[ChunkReadLocation]) -> EngineResult<Vec<Vec<u8>>> {
    let _operation = self.operation_guard("read_chunk_span_verified")?;
    if locations.is_empty() {
      return Ok(Vec::new());
    }

    let snapshot = self.kv_snapshot.load();
    let (reader, span_start, span_len) = {
      let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
      let mut span_start = u64::MAX;
      let mut span_end = 0u64;

      for location in locations {
        let kv_entry = match snapshot.get(&location.hash)? {
          Some(entry) if !entry.is_deleted() => entry,
          _ => return Err(EngineError::NotFound(format!("Chunk not found: {}", hex::encode(&location.hash)))),
        };
        if kv_entry.entry_type() != KV_TYPE_CHUNK {
          return Err(EngineError::InvalidInput(format!("Hash {} is not a chunk entry", hex::encode(&location.hash))));
        }
        if kv_entry.offset != location.offset || kv_entry.total_length != location.total_length {
          return Err(EngineError::CorruptEntry {
            offset: location.offset,
            reason: format!(
              "Chunk KV entry moved while planning span: expected offset {} length {}, found offset {} length {}",
              location.offset, location.total_length, kv_entry.offset, kv_entry.total_length,
            ),
          });
        }
        Self::validate_kv_entry_offset(&writer, &kv_entry, &location.hash, "read_chunk_span_verified")?;

        let entry_end = location
          .offset
          .checked_add(location.total_length as u64)
          .ok_or_else(|| EngineError::InvalidInput("Chunk span offset overflowed".to_string()))?;
        span_start = span_start.min(location.offset);
        span_end = span_end.max(entry_end);
      }

      let span_len =
        span_end.checked_sub(span_start).ok_or_else(|| EngineError::InvalidInput("Chunk span length underflowed".to_string()))?;
      let span_len: usize =
        span_len.try_into().map_err(|_| EngineError::InvalidInput(format!("Chunk span too large to buffer: {} bytes", span_len)))?;
      (writer.open_shared_reader()?, span_start, span_len)
    };

    let span = read_span_at(&reader, span_start, span_len).map_err(EngineError::IoError)?;
    let mut chunks = Vec::with_capacity(locations.len());
    for location in locations {
      let relative_start = location
        .offset
        .checked_sub(span_start)
        .ok_or_else(|| EngineError::InvalidInput("Chunk span relative offset underflowed".to_string()))?;
      let relative_start: usize =
        relative_start.try_into().map_err(|_| EngineError::InvalidInput(format!("Chunk relative offset too large: {}", relative_start)))?;
      let relative_end = relative_start
        .checked_add(location.total_length as usize)
        .ok_or_else(|| EngineError::InvalidInput("Chunk span relative end overflowed".to_string()))?;
      if relative_end > span.len() {
        return Err(EngineError::CorruptEntry {
          offset: location.offset,
          reason: format!("Chunk entry extends past buffered span: {} > {}", relative_end, span.len()),
        });
      }
      chunks.push(self.decode_verified_chunk_entry_from_buffer(&location.hash, location.offset, &span[relative_start..relative_end])?);
    }

    Ok(chunks)
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

  /// Read and verify a live chunk while bounding both its stored representation
  /// and decoded output before either can grow without caller control.
  pub fn read_chunk_verified_bounded(&self, hash: &[u8], maximum_decoded_length: usize) -> EngineResult<Option<Vec<u8>>> {
    self.read_chunk_verified_bounded_internal(hash, maximum_decoded_length, false)
  }

  /// Read and verify a live or deleted historical chunk while bounding both
  /// its stored representation and decoded output before allocation.
  pub(crate) fn read_chunk_verified_including_deleted_bounded(
    &self,
    hash: &[u8],
    maximum_decoded_length: usize,
  ) -> EngineResult<Option<Vec<u8>>> {
    self.read_chunk_verified_bounded_internal(hash, maximum_decoded_length, true)
  }

  fn read_chunk_verified_bounded_internal(
    &self,
    hash: &[u8],
    maximum_decoded_length: usize,
    include_deleted: bool,
  ) -> EngineResult<Option<Vec<u8>>> {
    let bounded_decoded_length = maximum_decoded_length.min(u32::MAX as usize);
    let maximum_stored_length =
      zstd::zstd_safe::compress_bound(bounded_decoded_length).max(maximum_decoded_length).try_into().unwrap_or(u32::MAX);
    let entry = if include_deleted {
      self.get_entry_including_deleted_verified_bounded(hash, maximum_stored_length)?
    } else {
      self.get_entry_verified_bounded(hash, maximum_stored_length)?
    };
    match entry {
      Some((header, _key, value)) => self.decode_chunk_entry_bounded(hash, header, value, maximum_decoded_length).map(Some),
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
    match snapshot.get(hash)? {
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
  pub fn reconcile_counters_from_kv(&self) -> EngineResult<()> {
    let current = self.counters.load().snapshot();
    let mut refreshed = EngineCounters::initialize_from_kv(self)?.snapshot();
    refreshed.writes_total = current.writes_total;
    refreshed.reads_total = current.reads_total;
    refreshed.bytes_written_total = current.bytes_written_total;
    refreshed.bytes_read_total = current.bytes_read_total;
    refreshed.chunks_deduped_total = current.chunks_deduped_total;
    refreshed.write_buffer_depth = current.write_buffer_depth;
    self.counters.load().reconcile(&refreshed);
    Ok(())
  }

  /// Update the HEAD hash in the file header, pointing to a new root directory version.
  pub fn update_head(&self, head_hash: &[u8]) -> EngineResult<()> {
    let _operation = self.operation_guard("update_head")?;
    self.ensure_writable()?;
    let _namespace = self.direct_hard_authority_guard()?;
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
    if !head_hash.is_empty() {
      self.record_gc_recheck(head_hash);
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
    let _namespace = self.direct_hard_authority_guard()?;
    let in_transaction =
      self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?.transaction_depth > 0;
    let mut writer = self.writer.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
    let mut header = writer.file_header().clone();
    header.backup_type = backup_type;
    header.base_hash = base_hash.to_vec();
    header.target_hash = target_hash.to_vec();
    if in_transaction {
      writer.set_header_in_memory(header);
    } else {
      writer.update_file_header(&header)?;
    }
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
    if let Err(error) = kv.insert(kv_entry) {
      drop(kv);
      drop(writer);
      return Err(self.normalize_runtime_write_error(DurabilityOperation::DataBarrier, "Typed KV page flush failed", error));
    }
    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);
    self.record_gc_recheck(key);

    let pending_expansion = Self::take_ready_kv_expansion(&mut kv);
    drop(kv);
    drop(writer);

    if let Some(target_stage) = pending_expansion {
      self.execute_kv_expansion_request(target_stage)?;
    }

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
      if let Err(error) = kv.insert(kv_entry) {
        drop(kv);
        drop(writer);
        return Err(self.normalize_runtime_write_error(DurabilityOperation::DataBarrier, "Batched KV page flush failed", error));
      }
    }

    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);
    for entry in &batch.entries {
      self.record_gc_recheck(&entry.key);
    }

    let pending_expansion = Self::take_ready_kv_expansion(&mut kv);
    drop(kv);
    drop(writer);

    if let Some(target_stage) = pending_expansion {
      self.execute_kv_expansion_request(target_stage)?;
    }

    Ok(offsets)
  }

  /// Flush a write batch AND update HEAD atomically in a single lock hold.
  /// This avoids separate lock acquisitions for the batch and the head update.
  pub fn flush_batch_and_update_head(&self, batch: WriteBatch, head_hash: &[u8]) -> EngineResult<Vec<u64>> {
    let _operation = self.operation_guard("flush_batch_and_update_head")?;
    self.ensure_writable()?;
    let _namespace = self.direct_hard_authority_guard()?;
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
      if let Err(error) = kv.insert(kv_entry) {
        drop(kv);
        drop(writer);
        return Err(self.normalize_runtime_write_error(DurabilityOperation::DataBarrier, "Batched HEAD KV page flush failed", error));
      }
    }
    for entry in &batch.entries {
      self.record_gc_recheck(&entry.key);
    }
    if !head_hash.is_empty() {
      self.record_gc_recheck(head_hash);
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

    let pending_expansion = Self::take_ready_kv_expansion(&mut kv);
    drop(kv);
    drop(writer);

    if let Some(target_stage) = pending_expansion {
      self.execute_kv_expansion_request(target_stage)?;
    }

    Ok(offsets)
  }

  /// Get directory content from cache by content hash.
  pub(crate) fn get_cached_dir_content(&self, content_key: &[u8]) -> EngineResult<Option<Vec<u8>>> {
    self.dir_content_cache.get(&content_key.to_vec())
  }

  /// Cache directory content by content hash.
  pub(crate) fn cache_dir_content(&self, content_key: Vec<u8>, value: Vec<u8>) -> EngineResult<()> {
    let weight = std::mem::size_of::<(Vec<u8>, Vec<u8>)>().saturating_add(content_key.capacity()).saturating_add(value.capacity()) as u64;
    self.dir_content_cache.insert_with_weight(content_key, value, weight)?;
    Ok(())
  }

  /// Clear the directory content cache (called on snapshot restore).
  pub fn clear_dir_content_cache(&self) -> EngineResult<()> {
    self.dir_content_cache.clear()
  }

  /// Best-effort sizes of the engine's in-memory caches. Returns
  /// (permissions, index_config, dir_content) entry counts. Used by
  /// soak-test instrumentation to attribute RSS growth to specific caches.
  pub fn engine_cache_sizes(&self) -> (usize, usize, usize) {
    let perms = self.permissions_cache.len();
    let idx = self.index_config_cache.len();
    let dirc = self.dir_content_cache.len();
    (perms, idx, dirc)
  }

  pub fn memory_stats(&self) -> EngineResult<EngineMemoryStats> {
    let process = crate::engine::rss_sampler::read_process_memory();
    let index_stats = self.index_buffer_stats()?;
    let directory_cache = self.directory_cache_memory_stats()?;
    let permission_cache = self.permissions_cache.stats()?;
    let index_config_cache = self.index_config_cache.stats()?;
    let grants_index_cache = self.grants_index_cache.stats()?;
    let caches = EngineCacheMemoryStats {
      permissions_entries: permission_cache.entries,
      index_config_entries: index_config_cache.entries,
      grants_index_entries: grants_index_cache.entries,
    };
    let index_cache = IndexCacheMemoryStats {
      cached_indexes: index_stats.cached_indexes,
      dirty_indexes: index_stats.dirty_indexes,
      deleted_indexes: index_stats.deleted_indexes,
      pending_mutations: index_stats.pending_mutations,
      total_mutations: index_stats.mutations,
      flushes: index_stats.flushes,
      flushed_indexes: index_stats.flushed_indexes,
      evictions: index_stats.evictions,
      evicted_indexes: index_stats.evicted_indexes,
      evicted_bytes: index_stats.evicted_bytes,
      entries: index_stats.entries,
      values: index_stats.values,
      estimated_bytes: index_stats.estimated_bytes,
      estimated_clean_bytes: index_stats.estimated_clean_bytes,
      estimated_dirty_bytes: index_stats.estimated_dirty_bytes,
      clean_reserved_bytes: index_stats.clean_reserved_bytes,
      dirty_reserved_bytes: index_stats.dirty_reserved_bytes,
      flush_reserved_bytes: index_stats.flush_reserved_bytes,
      flushing_indexes: index_stats.flushing_indexes,
      max_bytes: index_stats.max_bytes,
      mutation_max_bytes: index_stats.mutation_max_bytes,
      publication_batch_max_bytes: index_stats.publication_batch_max_bytes,
      clean_ttl_ms: index_stats.clean_ttl_ms,
      reservation_owned: index_stats.reservation_owned,
      top_cached_indexes: index_stats.top_cached_indexes,
    };
    let estimated_engine_owned_bytes =
      index_cache.estimated_bytes.saturating_add(index_cache.flush_reserved_bytes).saturating_add(directory_cache.estimated_bytes);

    Ok(EngineMemoryStats {
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
    })
  }

  fn directory_cache_memory_stats(&self) -> EngineResult<DirectoryCacheMemoryStats> {
    let cache = self.dir_content_cache.stats()?;
    Ok(DirectoryCacheMemoryStats { entries: cache.entries, estimated_bytes: cache.resident_bytes })
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
    let _operation = self.operation_guard("expand_kv_block_online")?;
    self.ensure_writable()?;
    let engine_id = self as *const StorageEngine as usize;
    let _maintenance = self.operation_tracker.begin_maintenance(engine_id, std::time::Duration::from_secs(300))?;
    let _authority = self.direct_hard_authority_guard()?;
    if self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?.transaction_depth > 0 {
      return Err(EngineError::InvalidInput("KV layout expansion cannot publish while a namespace transaction is active".to_string()));
    }
    let hash_length = self.hash_algo.hash_length();
    let psize = crate::engine::kv_pages::page_size(hash_length);
    if target_stage >= crate::engine::kv_stages::KV_STAGE_SIZES.len() {
      return Err(EngineError::InvalidInput(format!("KV target stage {target_stage} is outside the supported stage table")));
    }
    let (minimum_block_size, _new_bucket_count) = crate::engine::kv_stages::stage_params(target_stage, psize);

    // Acquire both locks: writer first, then KV
    let mut writer = self.writer.write().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
    let mut kv = self.kv_writer.lock().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
    let all_entries = kv.iter_all()?;

    let header = writer.file_header().clone();
    if target_stage <= header.kv_block_stage as usize {
      return Err(EngineError::InvalidInput(format!(
        "KV target stage {target_stage} must be greater than current stage {}",
        header.kv_block_stage
      )));
    }
    let old_kv_end = header
      .kv_block_offset
      .checked_add(header.kv_block_length)
      .ok_or_else(|| EngineError::InvalidInput("current KV block end overflows u64".to_string()))?;
    let minimum_kv_end = header
      .kv_block_offset
      .checked_add(minimum_block_size)
      .ok_or_else(|| EngineError::InvalidInput("expanded KV block end overflows u64".to_string()))?;
    // Use the writer's current offset (end of WAL) as the actual hot tail position,
    // NOT the header's hot_tail_offset which may be stale.
    let hot_tail_offset = writer.current_offset();

    // Walk validated entry boundaries from the current WAL start. Looking for
    // magic bytes inside payloads is ambiguous, and a final straddling entry
    // has no later magic marker. The first boundary at or beyond the overlap
    // is the exact byte through which relocation must copy.
    let actual_copy_end = Self::expansion_relocation_end(&writer, old_kv_end, minimum_kv_end, hot_tail_offset)?;
    // A page block may need a small amount of slack so its WAL boundary lands
    // between complete entries. This avoids overwriting a straddling entry tail
    // with a synthetic marker and makes the relocation-durable phase directly
    // scannable after a crash.
    let new_kv_end = minimum_kv_end.max(actual_copy_end);
    let new_block_length = new_kv_end
      .checked_sub(header.kv_block_offset)
      .ok_or_else(|| EngineError::InvalidInput("expanded KV block length underflows its offset".to_string()))?;
    let growth_zone_size = actual_copy_end - old_kv_end;
    // If the boundary-aligned block overtakes the old WAL frontier, append the
    // relocated bytes after it. Otherwise the old frontier remains the first
    // non-overlapping destination and avoids growing the file unnecessarily.
    let copy_dst = hot_tail_offset.max(new_kv_end);
    let new_hot_tail = copy_dst
      .checked_add(growth_zone_size)
      .ok_or_else(|| EngineError::InvalidInput("expanded hot-tail offset overflows u64".to_string()))?;
    let raw_offset_delta = i128::from(copy_dst) - i128::from(old_kv_end);
    let offset_delta = i64::try_from(raw_offset_delta)
      .map_err(|_| EngineError::InvalidInput(format!("KV relocation delta {raw_offset_delta} cannot be represented as i64")))?;
    let current_voids = self
      .void_manager
      .read()
      .map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?
      .iter()
      .map(|(offset, size)| VoidRecord { offset, size })
      .collect::<Vec<_>>();
    let adjusted_voids =
      Self::adjust_voids_for_expansion(current_voids, old_kv_end, actual_copy_end, offset_delta, new_kv_end, new_hot_tail);
    let mut relocation_hot_payload = kv.emergency_hot_tail_payload();
    for entry in &mut relocation_hot_payload.writes {
      if entry.offset >= old_kv_end && entry.offset < actual_copy_end {
        entry.offset = Self::shifted_expansion_offset(entry.offset, offset_delta)?;
      }
    }
    relocation_hot_payload.voids = adjusted_voids.clone();

    tracing::info!(
      growth_zone_size,
      old_kv_end,
      minimum_kv_end,
      new_kv_end,
      new_block_length,
      hot_tail_offset,
      "Online KV expansion: relocating {} bytes of WAL data",
      growth_zone_size,
    );

    // Boundary validation is read-only. Keep the current provider published
    // until every preflight check has succeeded, then drain old snapshot
    // generations immediately before the first durable layout mutation.
    kv.suspend_bounded_pages_for_layout_rewrite(std::time::Duration::from_secs(30))?;

    let expansion_result = (|| -> EngineResult<()> {
      // Step 1: Mark resize in progress. From this call onward any failure is
      // durability-critical because the selected header or relocated bytes
      // may already differ from the in-memory preflight view.
      let mut h = header.clone();
      h.resize_in_progress = true;
      h.resize_target_stage = target_stage as u8;
      writer.update_file_header(&h)?;

      // Step 2: Copy the complete overlapping WAL entries to the old frontier
      // and publish the adjusted recoverable hot tail at the new frontier.
      writer.copy_region(old_kv_end, copy_dst, growth_zone_size)?;
      writer.write_hot_tail_at(new_hot_tail, &relocation_hot_payload, hash_length)?;

      // Step 3: Fsync. Two complete copies now exist.
      writer.sync()?;

      // Publish a second, distinct phase only after the relocated WAL and hot
      // tail are durable. Its block length ends on a validated entry boundary,
      // so startup can finish by zeroing/rebuilding without relocating again.
      let mut relocated_header = writer.file_header().clone();
      relocated_header.kv_block_length = new_block_length;
      relocated_header.resize_in_progress = false;
      relocated_header.resize_target_stage = target_stage as u8;
      relocated_header.hot_tail_offset = new_hot_tail;
      writer.update_file_header(&relocated_header)?;
      self.last_published_hot_tail_offset.store(new_hot_tail, Ordering::Release);

      // Step 4-8: rehash and durably publish the final pages/hot tail, then
      // replace the resize marker with the completed layout header.
      writer.set_offset(new_hot_tail);
      kv.finalize_expansion_with_block_length(
        target_stage,
        new_block_length,
        old_kv_end,
        actual_copy_end,
        offset_delta,
        new_hot_tail,
        adjusted_voids.clone(),
        all_entries,
      )?;

      let mut final_header = writer.file_header().clone();
      final_header.kv_block_length = new_block_length;
      final_header.kv_block_stage = target_stage as u8;
      final_header.resize_in_progress = false;
      final_header.resize_target_stage = 0;
      final_header.hot_tail_offset = new_hot_tail;
      writer.update_file_header(&final_header)?;
      self.last_published_hot_tail_offset.store(final_header.hot_tail_offset, Ordering::Release);
      writer.sync()?;

      self
        .void_manager
        .write()
        .map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?
        .replace_all(adjusted_voids.iter().map(|void| (void.offset, void.size)));
      Ok(())
    })();

    if let Err(error) = expansion_result {
      drop(kv);
      drop(writer);
      return Err(self.record_durability_failure(
        DurabilityOperation::AuthorityWrite,
        "KV layout expansion failed after mutation began",
        error,
      ));
    }

    tracing::info!("Online KV block expansion complete");
    Ok(())
  }

  /// Check if a KV entry is marked as deleted.
  pub fn is_entry_deleted(&self, hash: &[u8]) -> EngineResult<bool> {
    let _operation = self.operation_guard("is_entry_deleted")?;
    let snapshot = self.kv_snapshot.load();
    match snapshot.get_raw(hash)? {
      Some(entry) => Ok(entry.is_deleted()),
      None => Ok(false),
    }
  }

  /// Mark a KV entry as deleted by setting the deleted flag.
  pub fn mark_entry_deleted(&self, hash: &[u8]) -> EngineResult<()> {
    let _operation = self.operation_guard("mark_entry_deleted")?;
    self.ensure_writable()?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    let updated = kv.update_flags(hash, KV_FLAG_DELETED)?;
    if !updated {
      return Err(EngineError::NotFound(format!("Entry not found for hash: {}", hex::encode(hash))));
    }
    Ok(())
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
      return Err(self.record_durability_failure(DurabilityOperation::DataBarrier, "Explicit WAL sync failed", error));
    }
    Ok(())
  }

  /// Batch remove multiple entries from the KV store. Publishes snapshot once at the end.
  pub fn remove_kv_entries_batch(&self, hashes: &[Vec<u8>]) -> EngineResult<()> {
    let _operation = self.operation_guard("remove_kv_entries_batch")?;
    self.ensure_writable()?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    kv.mark_deleted_batch(hashes)?;
    Ok(())
  }

  /// Remove an entry from the KV store (mark deleted). Used by GC sweep.
  pub fn remove_kv_entry(&self, hash: &[u8]) -> EngineResult<()> {
    let _operation = self.operation_guard("remove_kv_entry")?;
    self.ensure_writable()?;
    let mut kv = self.kv_writer.lock().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
    kv.mark_deleted(hash)?;
    Ok(())
  }

  /// Iterate all live KV entries. Used by GC sweep.
  pub fn iter_kv_entries(&self) -> EngineResult<Vec<KVEntry>> {
    let _operation = self.operation_guard("iter_kv_entries")?;
    let snapshot = self.kv_snapshot.load();
    snapshot.iter_all()
  }

  pub(crate) fn visit_kv_entries_for_repair<F>(&self, visitor: F) -> EngineResult<bool>
  where
    F: FnMut(&KVEntry) -> EngineResult<bool>,
  {
    let snapshot = self.kv_snapshot.load();
    snapshot.visit_all(visitor)
  }

  /// Return the number of rows in the current immutable KV read view without
  /// cloning those rows. Maintenance owners use this for pre-allocation
  /// admission before materializing a full scan.
  pub fn kv_entry_count(&self) -> EngineResult<usize> {
    let _operation = self.operation_guard("kv_entry_count")?;
    Ok(self.kv_snapshot.load().len())
  }

  pub(crate) fn kv_entries_by_type_admitted<F>(&self, target_type: u8, admit: F) -> EngineResult<Vec<KVEntry>>
  where
    F: FnOnce(usize) -> EngineResult<()>,
  {
    let _operation = self.operation_guard("kv_entries_by_type_admitted")?;
    let snapshot = self.kv_snapshot.load();
    let count = snapshot.count_by_type(target_type)?;
    admit(count)?;
    snapshot.iter_by_type(target_type)
  }

  /// Lightweight single-hash lookup in the KV snapshot.
  /// Returns `None` for deleted or missing entries.
  pub fn get_kv_entry(&self, hash: &[u8]) -> EngineResult<Option<KVEntry>> {
    let _operation = self.operation_guard("get_kv_entry")?;
    let snapshot = self.kv_snapshot.load();
    snapshot.get(hash)
  }

  /// Return all (key_hash, value) pairs for entries matching a KV type.
  /// Scans KV pages through the active snapshot backend, then reads each
  /// matching entry's value from the WAL.
  pub fn entries_by_type(&self, target_type: u8) -> EngineResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let _operation = self.operation_guard("entries_by_type")?;
    let entries: Vec<KVEntry> = {
      let snapshot = self.kv_snapshot.load();
      snapshot.iter_by_type(target_type)?
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
  pub fn stats(&self) -> EngineResult<DatabaseStats> {
    // 1. Lock writer for file header info and file size
    let (entry_count, created_at, updated_at, db_file_size_bytes, kv_size_bytes) = {
      let writer = self.writer.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
      let fh = writer.file_header();
      (fh.entry_count, fh.created_at, fh.updated_at, writer.file_size(), fh.kv_block_length)
    };

    // 2. Use snapshot for entry counts (lock-free)
    let snapshot = self.kv_snapshot.load();
    let kv_entries = snapshot.len();
    let nvt_buckets = snapshot.bucket_count();

    // Type counts are backed by compact snapshot counters and adjusted for
    // the small live write buffer without cloning every entry of that type.
    let chunk_count = snapshot.count_by_type(KV_TYPE_CHUNK)?;
    let file_count = snapshot.count_by_type(KV_TYPE_FILE_RECORD)?;
    let directory_count = snapshot.count_by_type(KV_TYPE_DIRECTORY)?;
    let snapshot_count = snapshot.count_by_type(KV_TYPE_SNAPSHOT)?;
    let fork_count = snapshot.count_by_type(KV_TYPE_FORK)?;

    // 3. Lock void_manager for void stats
    let (void_count, void_space_bytes) = {
      let vm = self.void_manager.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
      (vm.void_count(), vm.total_void_space())
    };

    Ok(DatabaseStats {
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
    })
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
    self.rebuild_kv_with_progress_boundary(progress_callback, KvRebuildScanBoundary::PhysicalEof)
  }

  fn rebuild_kv_with_progress_boundary(
    &self,
    progress_callback: Option<EngineStartupProgressCallback>,
    scan_boundary: KvRebuildScanBoundary,
  ) -> EngineResult<()> {
    let _operation = self.operation_guard("rebuild_kv")?;
    self.ensure_writable()?;
    let engine_id = self as *const StorageEngine as usize;
    let _maintenance = self.operation_tracker.begin_maintenance(engine_id, std::time::Duration::from_secs(300))?;
    let _mem = crate::engine::rss_sampler::PhaseSampler::start("rebuild_kv", std::time::Duration::from_millis(50));
    tracing::info!("Rebuilding KV index from append log...");
    let timer = std::time::Instant::now();

    let hash_algo = self.hash_algo;

    let memory_coordinator = self.memory_coordinator_if_initialized();
    let mut rebuild_workspace = KvRebuildWorkspace::new(
      &self.database_path,
      hash_algo,
      memory_coordinator.as_deref(),
      AdmissionClass::Critical(CriticalMemoryPurpose::BoundedRecovery),
      Some(Arc::clone(&self.shutdown_started)),
    )?;

    // Scan the append log under the exclusive maintenance gate. The external
    // workspace spills sorted runs beside the database, so WAL cardinality no
    // longer determines resident memory. Entry chronology remains
    // (timestamp, offset) because GC can reuse lower physical offsets.
    let (scanned_count, dirty_max_end) = {
      let writer = self.writer.read().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let voids = self.void_manager.read().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
      tracing::debug!(
        writer_offset = writer.current_offset(),
        file_path = %writer.file_path().display(),
        ?scan_boundary,
        "rebuild_kv: scanning authoritative WAL"
      );
      let mut scanner = match scan_boundary {
        KvRebuildScanBoundary::PhysicalEof => writer.scan_entries_dirty_recovery()?,
        KvRebuildScanBoundary::SelectedWal => writer.scan_entries_reporting_current_wal(Some(Arc::clone(&self.shutdown_started)))?,
      };
      let scan_start_offset = scanner.current_offset();
      let scan_total_bytes = scanner.file_length().saturating_sub(scan_start_offset);
      let mut last_progress_log = std::time::Instant::now();
      let scan_timer = std::time::Instant::now();
      let mut scanned_count = 0u64;
      let mut deletion_count = 0u64;
      let mut corrupt_entry_count = 0u64;
      let mut dirty_max_end = 0u64;
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
            let order = WorkspaceRebuildOrder { timestamp: scanned.header.timestamp, offset: scanned.offset };
            scanned_count = scanned_count
              .checked_add(1)
              .ok_or_else(|| EngineError::ResourceExhausted("KV rebuild scanned-entry count overflow".to_string()))?;
            let entry_end = scanned.offset.checked_add(scanned.header.total_length as u64).ok_or_else(|| EngineError::CorruptEntry {
              offset: scanned.offset,
              reason: "KV rebuild entry end overflows u64".to_string(),
            })?;
            dirty_max_end = dirty_max_end.max(entry_end);
            if matches!(scanned.header.entry_type, EntryType::Chunk | EntryType::Void) {
              skipped_payload_bytes = skipped_payload_bytes.saturating_add(scanned.header.value_length as u64);
            }
            if scanned.header.entry_type == EntryType::Void || voids.overlaps_range(scanned.offset, scanned.header.total_length) {
              continue;
            }
            if scanned.header.entry_type == EntryType::DeletionRecord {
              let value = scanned.value.as_ref().ok_or_else(|| EngineError::CorruptEntry {
                offset: scanned.offset,
                reason: "deletion record payload was omitted during KV rebuild".to_string(),
              })?;
              let record = crate::engine::deletion_record::DeletionRecord::deserialize(value, scanned.header.entry_version)?;
              rebuild_workspace.push_deletion_path(&record.path, order)?;
              deletion_count = deletion_count.saturating_add(1);
            }
            rebuild_workspace.push_value(
              scanned.header.entry_type.to_kv_type(),
              &scanned.key,
              scanned.offset,
              scanned.header.value_length,
              scanned.header.total_length,
              order,
            )?;
          }
          Err(e) => {
            tracing::warn!("Skipping corrupt entry during KV rebuild: {}", e);
            corrupt_entry_count = corrupt_entry_count.saturating_add(1);
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
            entries_collected = scanned_count,
            deletion_records = deletion_count,
            corrupt_entries = corrupt_entry_count,
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
        entries_collected = scanned_count,
        deletion_records = deletion_count,
        corrupt_entries = corrupt_entry_count,
        skipped_payload_bytes,
        duration_ms = scan_timer.elapsed().as_millis() as u64,
        "rebuild_kv: WAL scan complete"
      );
      (scanned_count, dirty_max_end)
    };
    // Writer lock released here
    rebuild_workspace.finish()?;
    let workspace_record_count = rebuild_workspace.raw_record_count();
    let resolved_count = rebuild_workspace.resolved_record_count()?;
    tracing::info!(scanned_count, workspace_record_count, resolved_count, "rebuild_kv: external resolution complete");
    Self::report_startup_progress(
      &progress_callback,
      EngineStartupProgress {
        phase: "rebuild_kv_resolve".to_string(),
        message: "Resolving latest WAL records for the rebuilt KV index".to_string(),
        current: resolved_count,
        total: Some(resolved_count),
        progress: Some(0.82),
        eta_seconds: None,
      },
    );

    // Read layout info from the file header
    let (kv_block_offset, file_path, existing_stage, durability_coordinator) = {
      let writer = self.writer.read().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let header = writer.file_header();
      (header.kv_block_offset, writer.file_path().to_path_buf(), header.kv_block_stage as usize, writer.durability_coordinator())
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
      let resolved_count_usize = usize::try_from(resolved_count).map_err(|_| {
        EngineError::ResourceExhausted(format!("KV rebuild resolved count {resolved_count} exceeds platform address space"))
      })?;
      let target_stage = crate::engine::kv_pages::stage_for_count(resolved_count_usize, hash_length);
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

    let bounded_page_config = {
      let mut current = self.kv_writer.lock().map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let config = current.bounded_page_configuration();
      current.suspend_bounded_pages_for_layout_rewrite(std::time::Duration::from_secs(30))?;
      config
    };
    let mut inserted_count = 0u64;
    let mut deleted_count = 0u64;
    let rebuild_result = (|| -> EngineResult<DiskKVStore> {
      // A valid hot tail tells startup that KV pages represent a clean
      // checkpoint. Remove that claim durably before the first page is
      // overwritten. A crash from here until flush restores a valid tail will
      // therefore re-enter WAL recovery instead of accepting partial pages.
      let marker_file = OpenOptions::new().read(true).write(true).open(&file_path)?;
      marker_file.set_len(wal_end)?;
      durability_coordinator
        .execute_recoverable_file_barrier(&marker_file, NativeFileBarrierKind::Data, 0)
        .map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;

      let kv_file = OpenOptions::new().read(true).write(true).open(&file_path)?;
      let mut new_kv = DiskKVStore::create_with_coordinator(
        kv_file,
        hash_algo,
        kv_offset,
        hot_offset,
        rebuild_stage,
        Arc::clone(&durability_coordinator),
      )?;
      let configure_bootstrap_after_flush = bounded_page_config.is_none();
      if let Some((coordinator, max_resident_bytes)) = bounded_page_config {
        new_kv.activate_bounded_pages(coordinator, max_resident_bytes)?;
      }

      // Stream the resolved run through DiskKVStore's
      // prepare-before-overwrite bulk path. The write buffer flushes at its
      // fixed threshold, so rebuild memory is independent of WAL cardinality.
      let mut last_insert_progress = std::time::Instant::now();
      Self::report_startup_progress(
        &progress_callback,
        EngineStartupProgress {
          phase: "rebuild_kv_insert".to_string(),
          message: "Buffering resolved KV records".to_string(),
          current: 0,
          total: Some(resolved_count),
          progress: Some(0.86),
          eta_seconds: None,
        },
      );
      rebuild_workspace.visit_resolved(|record| {
        if record.is_deleted() {
          deleted_count = deleted_count.saturating_add(1);
        }
        let entry = record.to_kv_entry();
        new_kv.bulk_insert(std::slice::from_ref(&entry))?;
        inserted_count = inserted_count
          .checked_add(1)
          .ok_or_else(|| EngineError::ResourceExhausted("KV rebuild inserted-entry count overflow".to_string()))?;
        if last_insert_progress.elapsed() >= std::time::Duration::from_secs(5) {
          let progress = if resolved_count == 0 { 0.92 } else { 0.86 + (inserted_count as f64 / resolved_count as f64) * 0.06 };
          Self::report_startup_progress(
            &progress_callback,
            EngineStartupProgress {
              phase: "rebuild_kv_insert".to_string(),
              message: "Writing resolved KV records in bounded batches".to_string(),
              current: inserted_count,
              total: Some(resolved_count),
              progress: Some(progress.clamp(0.86, 0.92)),
              eta_seconds: None,
            },
          );
          last_insert_progress = std::time::Instant::now();
        }
        Ok(())
      })?;
      if inserted_count != resolved_count {
        return Err(EngineError::CorruptEntry {
          offset: kv_offset,
          reason: format!("KV rebuild resolved {resolved_count} records but emitted {inserted_count}"),
        });
      }

      tracing::debug!(
        inserted = inserted_count,
        write_buffer_len = new_kv.write_buffer_len(),
        deleted_entries = deleted_count,
        "rebuild_kv: all resolved entries inserted; flushing"
      );

      Self::report_startup_progress(
        &progress_callback,
        EngineStartupProgress {
          phase: "rebuild_kv_flush".to_string(),
          message: "Flushing rebuilt KV pages to disk".to_string(),
          current: inserted_count,
          total: Some(resolved_count),
          progress: Some(0.92),
          eta_seconds: None,
        },
      );
      new_kv.flush()?;
      let (voids, void_count, void_bytes) = self.collect_void_snapshot()?;
      new_kv.set_pending_voids(voids);
      self.counters.load().set_void_stats(void_count, void_bytes);
      new_kv.force_flush_hot_buffer()?;
      if configure_bootstrap_after_flush {
        new_kv.activate_bootstrap_page_provider()?;
      }
      new_kv.adopt_snapshot_handle(Arc::clone(&self.kv_snapshot))?;
      Ok(new_kv)
    })();
    let new_kv = match rebuild_result {
      Ok(kv) => kv,
      Err(error) => {
        return Err(self.record_durability_failure(
          DurabilityOperation::AuthorityWrite,
          "KV rebuild failed after dirty-startup marker publication",
          error,
        ));
      }
    };

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
        current: inserted_count,
        total: Some(resolved_count),
        progress: Some(0.95),
        eta_seconds: Some(0),
      },
    );

    Ok(())
  }

  /// Begin a transaction: increment the KV store's transaction depth so that
  /// `flush()` skips hot-file truncation until the transaction ends.
  fn begin_transaction(&self) -> EngineResult<()> {
    let mut kv = self
      .kv_writer
      .lock()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Failed to begin transaction: {error}"))))?;
    kv.transaction_depth += 1;
    Ok(())
  }

  fn prepare_transaction_completion(&self) -> EngineResult<Option<DurabilityTicket>> {
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
      return Ok(None);
    }

    if self.void_snapshot_dirty.swap(false, Ordering::AcqRel) {
      if let Err(error) = self.sync_voids_to_kv_writer() {
        self.void_snapshot_dirty.store(true, Ordering::Release);
        return Err(error);
      }
    }

    let estimated_dependency_bytes = self
      .kv_writer
      .lock()
      .map_err(|error| EngineError::IoError(std::io::Error::other(format!("Failed to estimate transaction hot tail: {error}"))))?
      .pending_hot_tail_bytes();
    let plan = v3_header_commit_plan().map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
    let ticket = self
      .durability_coordinator
      .admit_sized(plan, estimated_dependency_bytes.saturating_add(FILE_HEADER_SIZE as u64))
      .map_err(durability_coordinator_engine_error)?;
    Ok(Some(ticket))
  }

  fn complete_transaction_ticket(&self, ticket: DurabilityTicket) -> EngineResult<()> {
    let coordinator = Arc::clone(&self.durability_coordinator);

    loop {
      match coordinator.wait_for_hard_turn(ticket).map_err(durability_coordinator_engine_error)? {
        DurabilityHardTurn::Complete(_) => {
          return match coordinator.take_waiter_state(ticket).map_err(durability_coordinator_engine_error)? {
            DurabilityWaiterState::Succeeded(_) => Ok(()),
            DurabilityWaiterState::Failed(failure) => Err(EngineError::DurabilityFailure(failure.message)),
            DurabilityWaiterState::Pending => {
              Err(EngineError::DurabilityFailure("transaction waiter remained pending after completion".to_string()))
            }
          };
        }
        DurabilityHardTurn::Drive(permit) => {
          let namespace = match self.namespace_write_guard() {
            Ok(namespace) => namespace,
            Err(error) => {
              permit.release();
              return Err(self.fail_transaction_ticket(&coordinator, ticket, error));
            }
          };
          let group = match coordinator.select_ready_hard_group(true) {
            Ok(group) => group,
            Err(error) => {
              drop(namespace);
              permit.release();
              return Err(self.fail_transaction_ticket(&coordinator, ticket, durability_coordinator_engine_error(error)));
            }
          };
          if group.is_empty() {
            drop(namespace);
            permit.release();
            continue;
          }

          let commit_result = (|| -> EngineResult<_> {
            let mut writer = self.writer.write().map_err(|error| EngineError::IoError(std::io::Error::other(error.to_string())))?;
            let mut kv = self.kv_writer.lock().map_err(|error| {
              EngineError::IoError(std::io::Error::other(format!("Failed to flush grouped transaction hot tail: {error}")))
            })?;
            Self::publish_hot_tail_authority_group(&mut writer, &mut kv, &group)
          })();
          drop(namespace);

          match commit_result {
            Ok(Some((hot_tail_offset, _entry_count))) => {
              self.last_published_hot_tail_offset.store(hot_tail_offset, Ordering::Release);
            }
            Ok(None) => {}
            Err(error) => {
              permit.release();
              return Err(self.fail_transaction_ticket(&coordinator, ticket, error));
            }
          }
          permit.release();
        }
      }
    }
  }

  fn fail_transaction_ticket(&self, coordinator: &DurabilityCoordinator, ticket: DurabilityTicket, error: EngineError) -> EngineError {
    let _ = coordinator.fail_pending_hard(
      DurabilityOperation::DependencyAppend,
      error.to_string(),
      DurabilityFailureDisposition::serious(OsErrorClass::OtherPersistentIo, RetryClass::AfterRepair),
      1,
    );
    let _ = coordinator.take_waiter_state(ticket);
    error
  }

  /// End a transaction and wait until its exact hard-authority ticket is at or
  /// below the proven durability frontier.
  pub fn end_transaction(&self) -> EngineResult<()> {
    let namespace = self.namespace_write_guard()?;
    self.end_transaction_after(namespace)
  }

  fn end_transaction_after(&self, namespace: NamespaceWriteGuard<'_>) -> EngineResult<()> {
    let prepared = self.prepare_transaction_completion();
    drop(namespace);
    match prepared {
      Ok(Some(ticket)) => {
        if let Err(error) = self.complete_transaction_ticket(ticket) {
          return Err(self.classify_transaction_completion_error(error));
        }
        self.run_ready_kv_expansion()
      }
      Ok(None) => Ok(()),
      Err(error) => Err(self.classify_transaction_completion_error(error)),
    }
  }

  fn classify_transaction_completion_error(&self, error: EngineError) -> EngineError {
    match error {
      EngineError::ResourceExhausted(_) => error,
      other => self.record_durability_failure(DurabilityOperation::DependencyAppend, "Transaction hard completion failed", other),
    }
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

    let _authority = match self.try_direct_hard_authority_guard() {
      Ok(Some(authority)) => authority,
      Ok(None) => return,
      Err(error) => {
        self.record_durability_failure(DurabilityOperation::HeaderAb, "Timer namespace authority failed", error);
        return;
      }
    };

    // Acquire in the engine's canonical writer -> KV order and execute the
    // hot-tail write as the dependency step of the same hard plan that
    // publishes the header. A transaction owner performs this work when its
    // outermost guard exits, so the timer leaves active transactions alone.
    let commit_result = {
      let mut writer = match self.writer.try_write() {
        Ok(writer) => writer,
        Err(_) => return,
      };
      let mut kv = match self.kv_writer.try_lock() {
        Ok(kv) => kv,
        Err(_) => return,
      };
      if kv.hot_buffer_len() == 0 || kv.transaction_depth != 0 {
        return;
      }

      Self::publish_hot_tail_authority(&mut writer, &mut kv, false).map(|commit| commit.map(|(hot_tail_offset, _)| hot_tail_offset))
    };

    match commit_result {
      Ok(Some(hot_tail_offset)) => self.last_published_hot_tail_offset.store(hot_tail_offset, Ordering::Release),
      Ok(None) => {}
      Err(error) => {
        if !matches!(error, EngineError::ResourceExhausted(_)) {
          self.record_durability_failure(DurabilityOperation::HeaderAb, "Timer hard commit failed", error);
        }
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
    let drain_deadline = std::time::Instant::now() + drain_timeout;
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

    let durability_wait = drain_deadline.saturating_duration_since(std::time::Instant::now());
    let durability =
      self.durability_coordinator.wait_until_idle(durability_wait).map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
    if durability.admitted > 0 || durability.executing > 0 || durability.pending_hard > 0 || durability.driver_active {
      tracing::error!(
        admitted = durability.admitted,
        executing = durability.executing,
        pending_hard = durability.pending_hard,
        driver_active = durability.driver_active,
        oldest_pending_age_ms = durability.oldest_pending_age_ms,
        wait_milliseconds = durability_wait.as_millis(),
        repeated_attempt = previous_attempt,
        "Storage engine shutdown blocked by durability work"
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

    let mut shutdown_memory = match OperationMemoryBudget::new(
      self,
      "graceful shutdown",
      MemoryOwner::Shutdown,
      AdmissionClass::Critical(CriticalMemoryPurpose::Shutdown),
      SHUTDOWN_BASE_WORKSPACE_BYTES,
      None,
    ) {
      Ok(memory) => memory,
      Err(error) => {
        self.shutdown_flush_started.store(false, Ordering::Release);
        return Err(error);
      }
    };

    // Keep shutdown workspace and engine locks out of the emergency-spill
    // path. A failed flush is first captured as bounded process state; only
    // after every shutdown resource is released do we latch the engine and
    // preserve the remaining volatile state.
    let mut failures: Vec<(&'static str, String)> = Vec::new();

    if let Err(e) = self.flush_index_buffer() {
      failures.push(("Index buffer flush failed during shutdown", e.to_string()));
    }

    // Step 1: Flush the KV write buffer to disk pages. Defer durability
    // latching until after the KV mutex is released so emergency spill can
    // snapshot volatile KV state.
    match self.kv_writer.lock() {
      Ok(mut kv) => {
        let checkpoint = shutdown_memory.checkpoint();
        let workspace_bytes = kv.shutdown_flush_workspace_bytes();
        match shutdown_memory.reserve(workspace_bytes, "shutdown KV flush workspace admission failed") {
          Ok(()) => {
            if let Err(e) = kv.flush_for_shutdown() {
              failures.push(("KV flush failed during shutdown", e.to_string()));
            }
            // Step 2: Flush the hot file buffer while the admitted clone and
            // page-assembly workspace remains held.
            if let Err(e) = kv.flush_hot_buffer() {
              failures.push(("Hot file flush failed during shutdown", e.to_string()));
            }
            if let Err(error) = shutdown_memory.release_to(checkpoint, "shutdown KV flush workspace accounting failed") {
              failures.push(("KV flush memory accounting failed during shutdown", error.to_string()));
            }
          }
          Err(error) => failures.push(("KV flush memory admission failed during shutdown", error.to_string())),
        }
      }
      Err(e) => {
        failures.push(("Could not acquire KV lock during shutdown", e.to_string()));
      }
    }

    // Step 3: Extract KV metadata, then persist header and sync WAL.
    // Extract values from kv_writer BEFORE acquiring writer to avoid
    // nesting kv_writer inside writer (opposite of the timer's order).
    let (hot_tail_offset, entry_count) = match self.kv_writer.lock() {
      Ok(kv) => (kv.hot_tail_offset(), kv.len() as u64),
      Err(e) => {
        failures.push(("Could not acquire KV lock for header update during shutdown", e.to_string()));
        (0, 0)
      }
    };

    match self.writer.write() {
      Ok(mut writer) => {
        let mut header = writer.file_header().clone();
        header.hot_tail_offset = hot_tail_offset;
        header.entry_count = entry_count;
        if let Err(e) = writer.update_header(&header) {
          failures.push(("Header update failed during shutdown", e.to_string()));
        } else {
          self.last_published_hot_tail_offset.store(hot_tail_offset, Ordering::Release);
        }
      }
      Err(e) => {
        failures.push(("Could not acquire writer lock during shutdown", e.to_string()));
      }
    }

    drop(shutdown_memory);

    let mut first_failure: Option<String> = None;
    for (context, error) in failures {
      let error = self.record_durability_failure(DurabilityOperation::ShutdownFlush, context, error);
      if first_failure.is_none() {
        first_failure = Some(error.to_string());
      }
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
  use std::io::{Seek, SeekFrom};
  use std::time::{Duration, Instant};

  #[test]
  fn layout_maintenance_drains_existing_operations_and_blocks_new_admission() {
    let tracker = Arc::new(EngineOperationTracker::default());
    let engine_id = Arc::as_ptr(&tracker) as usize;
    let owner = tracker.begin(engine_id, "layout_owner").unwrap();

    std::thread::scope(|scope| {
      let (active_tx, active_rx) = std::sync::mpsc::channel();
      let (release_tx, release_rx) = std::sync::mpsc::channel();
      let worker_tracker = Arc::clone(&tracker);
      scope.spawn(move || {
        let worker = worker_tracker.begin(engine_id, "existing_reader").unwrap();
        active_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        drop(worker);
      });
      active_rx.recv().unwrap();

      scope.spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        release_tx.send(()).unwrap();
      });
      let started = Instant::now();
      let maintenance = tracker.begin_maintenance(engine_id, Duration::from_secs(1)).unwrap();
      assert!(started.elapsed() >= Duration::from_millis(30), "maintenance returned before the admitted reader drained");

      let (admitted_tx, admitted_rx) = std::sync::mpsc::channel();
      let blocked_tracker = Arc::clone(&tracker);
      scope.spawn(move || {
        let blocked = blocked_tracker.begin(engine_id, "new_reader").unwrap();
        admitted_tx.send(()).unwrap();
        drop(blocked);
      });
      assert!(admitted_rx.recv_timeout(Duration::from_millis(40)).is_err(), "new work entered during exclusive maintenance");
      drop(maintenance);
      admitted_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    });
    drop(owner);
  }

  #[test]
  fn layout_maintenance_timeout_reopens_normal_admission() {
    let tracker = Arc::new(EngineOperationTracker::default());
    let engine_id = Arc::as_ptr(&tracker) as usize;
    let owner = tracker.begin(engine_id, "layout_owner").unwrap();

    std::thread::scope(|scope| {
      let (active_tx, active_rx) = std::sync::mpsc::channel();
      let (release_tx, release_rx) = std::sync::mpsc::channel();
      let worker_tracker = Arc::clone(&tracker);
      scope.spawn(move || {
        let worker = worker_tracker.begin(engine_id, "slow_reader").unwrap();
        active_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        drop(worker);
      });
      active_rx.recv().unwrap();

      assert!(tracker.begin_maintenance(engine_id, Duration::from_millis(10)).is_err());
      release_tx.send(()).unwrap();
    });
    let admitted = tracker.begin(engine_id, "after_timeout").unwrap();
    drop(admitted);
    drop(owner);
  }

  #[test]
  fn namespace_authority_is_operation_admitted_before_lock_ownership() {
    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("namespace-maintenance-order.aeordb");
    let engine = Arc::new(StorageEngine::create(engine_path.to_str().unwrap()).unwrap());
    let namespace = engine.namespace_write_guard().unwrap();

    let held = engine.active_operations_snapshot();
    assert_eq!(held.active_operations, 1, "namespace ownership was invisible to exclusive maintenance");
    assert_eq!(held.operations, vec![("namespace_authority".to_string(), 1)]);

    std::thread::scope(|scope| {
      let maintenance_engine = Arc::clone(&engine);
      let (result_tx, result_rx) = std::sync::mpsc::channel();
      scope.spawn(move || {
        let operation = maintenance_engine.operation_guard("layout_owner").unwrap();
        let engine_id = Arc::as_ptr(&maintenance_engine) as usize;
        let result = maintenance_engine.operation_tracker.begin_maintenance(engine_id, Duration::from_secs(1));
        result_tx.send(result.map(drop)).unwrap();
        drop(operation);
      });

      let deadline = Instant::now() + Duration::from_secs(1);
      loop {
        let maintenance_started = engine.operation_tracker.state.lock().unwrap().maintenance_in_progress;
        if maintenance_started {
          break;
        }
        assert!(Instant::now() < deadline, "maintenance did not close admission");
        std::thread::yield_now();
      }
      assert!(result_rx.recv_timeout(Duration::from_millis(30)).is_err(), "maintenance ignored a held namespace authority");

      drop(namespace);
      result_rx.recv_timeout(Duration::from_secs(1)).expect("maintenance did not resume after namespace release").unwrap();
    });

    let try_namespace = engine.try_direct_hard_authority_guard().unwrap().expect("uncontended try-lock must succeed");
    let held = engine.active_operations_snapshot();
    assert_eq!(held.active_operations, 1, "try-lock namespace ownership was invisible to exclusive maintenance");
    assert_eq!(held.operations, vec![("namespace_authority".to_string(), 1)]);
    drop(try_namespace);
    assert_eq!(engine.active_operations_snapshot().active_operations, 0);
  }

  #[test]
  fn durability_waiter_pressure_refuses_transaction_without_latching_the_engine() {
    let temp = tempfile::tempdir().unwrap();
    let engine = StorageEngine::create(temp.path().join("waiter-pressure.aeordb").to_str().unwrap()).unwrap();
    let memory = engine.memory_coordinator();
    let snapshot = memory.snapshot().unwrap();
    let policy = snapshot.policy.unwrap();
    let remaining = policy.emergency_reserve_bytes - snapshot.critical_reserved_bytes;
    let pressure = memory
      .reserve(
        MemoryOwner::DurabilityWaiters,
        remaining.saturating_sub(1),
        AdmissionClass::Critical(crate::engine::memory_coordinator::CriticalMemoryPurpose::DurableWrite),
      )
      .unwrap();

    let error = TransactionGuard::new(&engine).unwrap().commit().unwrap_err();
    assert!(matches!(error, EngineError::ResourceExhausted(_)));
    assert!(engine.durability_failure().is_none(), "pre-mutation waiter refusal must not latch the database");

    drop(pressure);
    TransactionGuard::new(&engine).unwrap().commit().unwrap();
    assert!(engine.durability_failure().is_none());
  }

  #[test]
  fn timer_waiter_pressure_defers_without_latching_or_consuming_the_hot_tail() {
    let temp = tempfile::tempdir().unwrap();
    let engine = StorageEngine::create(temp.path().join("timer-waiter-pressure.aeordb").to_str().unwrap()).unwrap();
    engine.store_entry(EntryType::Chunk, b"timer-pressure-key", b"staged dependency").unwrap();
    assert!(engine.kv_writer.lock().unwrap().hot_buffer_len() > 0);

    let memory = engine.memory_coordinator();
    let snapshot = memory.snapshot().unwrap();
    let policy = snapshot.policy.unwrap();
    let remaining = policy.emergency_reserve_bytes - snapshot.critical_reserved_bytes;
    let pressure = memory
      .reserve(
        MemoryOwner::DurabilityWaiters,
        remaining.saturating_sub(1),
        AdmissionClass::Critical(crate::engine::memory_coordinator::CriticalMemoryPurpose::DurableWrite),
      )
      .unwrap();

    engine.try_flush_hot_buffer();
    assert!(engine.durability_failure().is_none(), "pre-dependency timer refusal must not latch the database");
    assert!(engine.kv_writer.lock().unwrap().hot_buffer_len() > 0, "refused timer publication consumed recoverable dependency state");

    drop(pressure);
    engine.try_flush_hot_buffer();
    assert!(engine.durability_failure().is_none());
    assert_eq!(engine.kv_writer.lock().unwrap().hot_buffer_len(), 0);
  }

  #[test]
  #[serial]
  fn layout_mutation_failure_latches_engine_read_only() {
    let temp = tempfile::tempdir().unwrap();
    let _env = SpillTestEnv::new(&temp.path().join("spill"));
    let db_path = temp.path().join("layout-failure.aeordb");
    let engine = StorageEngine::create(db_path.to_str().unwrap()).unwrap();
    let key = vec![0x83u8; engine.hash_algo().hash_length()];
    let value = vec![0x5Au8; 600 * 1024];
    let offset = engine.store_entry(EntryType::DirectoryIndex, &key, &value).unwrap();
    let header_size = engine.read_entry_header_at(offset).unwrap().header_size() as u64;

    // Leave a valid header for read-only boundary preflight, but make the
    // later region copy fail after resize_in_progress has been published.
    {
      let file = OpenOptions::new().write(true).open(&db_path).unwrap();
      file.set_len(offset + header_size).unwrap();
      crate::engine::native_durability::sync_file_all_native(&file).unwrap();
    }

    let error = engine.expand_kv_block_online(1).expect_err("truncated relocation source must fail expansion");
    assert!(matches!(error, EngineError::DurabilityFailure(_)), "post-marker failure must be classified as durability-critical: {error}");
    let failure = engine.durability_failure().expect("post-marker failure must latch write authority");
    assert!(failure.contains("KV layout expansion failed after mutation began"));
    assert!(matches!(engine.ensure_writable(), Err(EngineError::DurabilityFailure(_))));
  }

  struct SpillTestEnv {
    spill_dir: Option<std::ffi::OsString>,
    spill_max: Option<std::ffi::OsString>,
    config_only: Option<std::ffi::OsString>,
  }

  struct FailAfterWriter {
    written: usize,
    fail_at: usize,
  }

  impl std::io::Write for FailAfterWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
      if self.written >= self.fail_at {
        return Err(std::io::Error::new(std::io::ErrorKind::StorageFull, "injected index spill write failure"));
      }
      let width = buffer.len().min(self.fail_at - self.written);
      self.written += width;
      Ok(width)
    }

    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }

  impl SpillTestEnv {
    fn new(spill_dir: &Path) -> Self {
      let state = Self {
        spill_dir: std::env::var_os("AEORDB_EMERGENCY_SPILL_DIR"),
        spill_max: std::env::var_os("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES"),
        config_only: std::env::var_os("AEORDB_EMERGENCY_SPILL_TEST_CONFIG_ONLY"),
      };
      state.set_spill_dir(spill_dir);
      unsafe {
        std::env::set_var("AEORDB_EMERGENCY_WAL_SPILL_MAX_BYTES", "1048576");
        std::env::set_var("AEORDB_EMERGENCY_SPILL_TEST_CONFIG_ONLY", "1");
      }
      state
    }

    fn set_spill_dir(&self, spill_dir: &Path) {
      unsafe { std::env::set_var("AEORDB_EMERGENCY_SPILL_DIR", spill_dir) }
    }
  }

  impl Drop for SpillTestEnv {
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
        match &self.config_only {
          Some(value) => std::env::set_var("AEORDB_EMERGENCY_SPILL_TEST_CONFIG_ONLY", value),
          None => std::env::remove_var("AEORDB_EMERGENCY_SPILL_TEST_CONFIG_ONLY"),
        }
      }
    }
  }

  fn admit_pending_namespace_transaction(engine: &StorageEngine, key: &[u8]) -> DurabilityTicket {
    let namespace = engine.namespace_write_guard().unwrap();
    engine.begin_transaction().unwrap();
    engine.store_entry(EntryType::Chunk, key, b"pending transaction bytes").unwrap();
    let ticket = engine.prepare_transaction_completion().unwrap().expect("outer transaction must admit a hard-authority ticket");
    drop(namespace);
    ticket
  }

  #[test]
  #[serial]
  fn timer_defers_to_an_earlier_transaction_hard_authority_ticket() {
    let temp_dir = tempfile::tempdir().unwrap();
    let _restore = SpillTestEnv::new(&temp_dir.path().join("spill"));
    let engine_path = temp_dir.path().join("timer-hard-frontier-race.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let ticket = admit_pending_namespace_transaction(&engine, b"timer-race-key");

    engine.try_flush_hot_buffer();
    let premature_failure = engine.durability_failure();
    engine.complete_transaction_ticket(ticket).unwrap();

    assert!(premature_failure.is_none(), "timer treated coordinator contention as a serious storage failure: {premature_failure:?}");
    assert_eq!(engine.durability_snapshot().unwrap().pending_hard, 0);
    assert!(engine.get_entry(b"timer-race-key").unwrap().is_some());
  }

  #[test]
  #[serial]
  fn direct_header_publication_waits_for_the_existing_hard_frontier() {
    let temp_dir = tempfile::tempdir().unwrap();
    let _restore = SpillTestEnv::new(&temp_dir.path().join("spill"));
    let engine_path = temp_dir.path().join("direct-hard-frontier-race.aeordb");
    let engine = Arc::new(StorageEngine::create(engine_path.to_str().unwrap()).unwrap());
    let ticket = admit_pending_namespace_transaction(&engine, b"direct-race-key");
    let new_head = vec![0x5au8; engine.hash_algo().hash_length()];

    let (result_sender, result_receiver) = std::sync::mpsc::channel();
    let publishing_engine = Arc::clone(&engine);
    let publishing_head = new_head.clone();
    let publisher = std::thread::spawn(move || {
      result_sender.send(publishing_engine.update_head(&publishing_head)).unwrap();
    });

    let early_result = result_receiver.recv_timeout(Duration::from_millis(50));
    let completed_early = early_result.is_ok();
    engine.complete_transaction_ticket(ticket).unwrap();
    let publication_result = match early_result {
      Ok(result) => result,
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => result_receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
      Err(error) => panic!("direct publisher result channel failed: {error}"),
    };
    publisher.join().unwrap();

    assert!(!completed_early, "direct publication did not wait for the prior ticket");
    publication_result.unwrap();
    assert_eq!(engine.head_hash().unwrap(), new_head);
    assert!(engine.durability_failure().is_none());
  }

  #[test]
  #[serial]
  fn reentrant_non_transaction_header_publication_refuses_an_older_hard_ticket() {
    let temp_dir = tempfile::tempdir().unwrap();
    let _restore = SpillTestEnv::new(&temp_dir.path().join("spill"));
    let engine_path = temp_dir.path().join("reentrant-hard-frontier-race.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let original_head = engine.head_hash().unwrap();
    let ticket = admit_pending_namespace_transaction(&engine, b"reentrant-race-key");
    let namespace = engine.namespace_write_guard().unwrap();
    let new_head = vec![0x6bu8; engine.hash_algo().hash_length()];

    let error = engine.update_head(&new_head).expect_err("reentrant direct publication must not leapfrog an older ticket");

    assert!(error.to_string().contains("cannot publish while an earlier hard-authority ticket is pending"), "unexpected refusal: {error}");
    assert_eq!(engine.head_hash().unwrap(), original_head);
    assert!(engine.durability_failure().is_none());
    drop(namespace);
    engine.complete_transaction_ticket(ticket).unwrap();
  }

  #[test]
  #[serial]
  fn backup_header_changes_remain_in_memory_until_transaction_hard_completion() {
    let temp_dir = tempfile::tempdir().unwrap();
    let _restore = SpillTestEnv::new(&temp_dir.path().join("spill"));
    let engine_path = temp_dir.path().join("transaction-backup-header.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let initial_sequence = engine.writer.read().unwrap().file_header().sequence;
    let namespace = engine.namespace_write_guard().unwrap();
    let transaction = TransactionGuard::new(&engine).unwrap();
    let base_hash = vec![0x71; engine.hash_algo().hash_length()];
    let target_hash = vec![0x72; engine.hash_algo().hash_length()];

    engine.set_backup_info(1, &base_hash, &target_hash).unwrap();

    let header = engine.writer.read().unwrap().file_header().clone();
    assert_eq!(header.sequence, initial_sequence, "transactional backup metadata published a header before hard completion");
    assert_eq!(header.backup_type, 1);
    assert_eq!(header.base_hash, base_hash);
    assert_eq!(header.target_hash, target_hash);

    transaction.commit_after(namespace).unwrap();
    assert!(engine.writer.read().unwrap().file_header().sequence > initial_sequence);
    drop(engine);

    let reopened = StorageEngine::open(engine_path.to_str().unwrap()).unwrap();
    assert_eq!(reopened.backup_info().unwrap(), (1, base_hash, target_hash));
  }

  #[test]
  #[serial]
  fn kv_expansion_requested_inside_a_transaction_waits_for_hard_completion() {
    let temp_dir = tempfile::tempdir().unwrap();
    let _restore = SpillTestEnv::new(&temp_dir.path().join("spill"));
    let engine_path = temp_dir.path().join("transaction-expansion.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let namespace = engine.namespace_write_guard().unwrap();
    let transaction = TransactionGuard::new(&engine).unwrap();
    engine.kv_writer.lock().unwrap().needs_expansion = Some(1);

    engine.store_entry(EntryType::Chunk, &[0x81; 32], b"transaction expansion payload").unwrap();

    assert_eq!(
      engine.writer.read().unwrap().file_header().kv_block_stage,
      0,
      "online expansion published layout headers before transaction hard completion"
    );
    assert_eq!(engine.kv_writer.lock().unwrap().needs_expansion, Some(1));

    transaction.commit_after(namespace).unwrap();
    assert_eq!(engine.writer.read().unwrap().file_header().kv_block_stage, 1);
    assert_eq!(engine.kv_writer.lock().unwrap().needs_expansion, None);
    assert!(engine.get_entry(&[0x81; 32]).unwrap().is_some());
  }

  #[test]
  #[serial]
  fn kv_expansion_preflight_failure_keeps_the_request_queued_for_retry() {
    let temp_dir = tempfile::tempdir().unwrap();
    let _restore = SpillTestEnv::new(&temp_dir.path().join("spill"));
    let engine_path = temp_dir.path().join("expansion-preflight-retry.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let wal_start = engine.store_entry(EntryType::Chunk, &[0x91; 32], b"entry in expansion growth zone").unwrap();
    {
      let mut file = OpenOptions::new().write(true).open(&engine_path).unwrap();
      file.seek(SeekFrom::Start(wal_start)).unwrap();
      file.write_all(&[0u8; 4]).unwrap();
      crate::engine::native_durability::sync_file_all_native(&file).unwrap();
    }
    engine.kv_writer.lock().unwrap().needs_expansion = Some(1);

    let error = engine.run_ready_kv_expansion().expect_err("corrupt WAL boundary must refuse expansion preflight");

    assert!(matches!(error, EngineError::InvalidMagic), "unexpected preflight error: {error}");
    assert!(engine.durability_failure().is_none(), "read-only preflight refusal must not latch the engine");
    assert_eq!(engine.kv_writer.lock().unwrap().needs_expansion, Some(1), "retryable expansion request was lost");
    let header = engine.writer.read().unwrap().file_header().clone();
    assert!(!header.resize_in_progress);
    assert_eq!(header.resize_target_stage, 0);
  }

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
  fn shutdown_refuses_pending_durability_work_without_active_operations() {
    struct NoopExecutor;

    impl crate::engine::durability_coordinator::DurabilityExecutor for NoopExecutor {
      type Error = std::convert::Infallible;

      fn execute(&mut self, _sequence: u64, _operation: DurabilityOperation) -> Result<(), Self::Error> {
        Ok(())
      }
    }

    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("shutdown-pending-durability.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let ticket = engine.durability_coordinator.admit(v3_header_commit_plan().unwrap()).unwrap();
    assert_eq!(engine.active_operations_snapshot().active_operations, 0);

    let blocked = engine.shutdown_with_drain_timeout(Duration::ZERO);
    assert!(matches!(blocked, Err(EngineError::ShuttingDown)));
    let pending = engine.durability_snapshot().unwrap();
    assert_eq!(pending.admitted, 1);
    assert_eq!(pending.pending_hard, 1);

    engine.durability_coordinator.execute(ticket, &mut NoopExecutor).unwrap();
    assert!(matches!(engine.durability_coordinator.take_waiter_state(ticket).unwrap(), DurabilityWaiterState::Succeeded(_)));
    engine.shutdown_with_drain_timeout(Duration::ZERO).unwrap();
  }

  #[test]
  #[serial]
  fn durability_failure_latches_read_only_and_spills_volatile_state() {
    let temp_dir = tempfile::tempdir().unwrap();
    let spill_dir = temp_dir.path().join("spill");
    let _restore = SpillTestEnv::new(&spill_dir);

    let engine_path = temp_dir.path().join("durability-spill.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    engine.store_entry(EntryType::Chunk, b"chunk-1", b"hello").unwrap();

    let error = engine.record_durability_failure(DurabilityOperation::AuthorityBarrier, "test forced durability failure", "synthetic EIO");
    assert!(matches!(error, EngineError::DurabilityFailure(_)));
    assert!(engine.durability_failure().is_some());
    let first_state = engine.durability_failure_state().expect("failure state");
    assert_eq!(first_state.occurrence_count, 1);

    let later = engine.record_durability_failure(DurabilityOperation::DataBarrier, "later durability failure", "synthetic ENOSPC");
    assert!(matches!(later, EngineError::DurabilityFailure(_)));
    let latest_state = engine.durability_failure_state().expect("updated failure state");
    assert_eq!(latest_state.database_id, first_state.database_id);
    assert_eq!(latest_state.incident_id, first_state.incident_id);
    assert_eq!(latest_state.first_failure, first_state.first_failure);
    assert_eq!(latest_state.failed_operation, first_state.failed_operation);
    assert_eq!(latest_state.os_error_class, first_state.os_error_class);
    assert_eq!(latest_state.os_error_code, first_state.os_error_code);
    assert_eq!(latest_state.last_selected_header_sequence, first_state.last_selected_header_sequence);
    assert_eq!(latest_state.last_durable_write_sequence, first_state.last_durable_write_sequence);
    assert_eq!(latest_state.last_durable_publication_sequence, first_state.last_durable_publication_sequence);
    assert!(latest_state.latest_failure.contains("later durability failure"));
    assert!(latest_state.latest_failure_at_ms >= latest_state.first_failure_at_ms);
    assert_eq!(latest_state.occurrence_count, 2);

    let spill = engine.emergency_spill_report().expect("spill report");
    assert!(spill.succeeded, "spill failed: {:?}", spill.errors);
    assert_eq!(spill.hot_tail_writes, 1);
    assert!(spill.hot_tail_path.as_ref().is_some_and(|path| std::fs::metadata(path).is_ok()));
    assert!(spill.manifest_path.as_ref().is_some_and(|path| std::fs::metadata(path).is_ok()));
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let spill_directory_mode = std::fs::metadata(spill.spill_directory.as_ref().unwrap()).unwrap().permissions().mode() & 0o777;
      let component_mode = std::fs::metadata(spill.hot_tail_path.as_ref().unwrap()).unwrap().permissions().mode() & 0o777;
      assert_eq!(spill_directory_mode, 0o700);
      assert_eq!(component_mode, 0o600);
    }

    let manifest_path = spill.manifest_path.as_ref().unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
    assert_eq!(manifest.get("format").and_then(|value| value.as_str()), Some("aeordb-emergency-spill-v2"));
    assert_eq!(manifest.get("database_id").and_then(|value| value.as_str()).map(str::len), Some(32));
    assert_eq!(manifest.get("incident_id").and_then(|value| value.as_str()).map(str::len), Some(32));
    assert_eq!(manifest.get("source_location_class").and_then(|value| value.as_u64()), Some(2));
    assert!(manifest.get("creation_sequence").and_then(|value| value.as_u64()).is_some_and(|value| value > 0));
    assert_eq!(manifest.get("first_failure_at_ms").and_then(|value| value.as_i64()), Some(latest_state.first_failure_at_ms));
    assert_eq!(manifest.get("latest_failure_at_ms").and_then(|value| value.as_i64()), Some(latest_state.latest_failure_at_ms));
    assert_eq!(manifest.get("failed_operation").and_then(|value| value.as_u64()), Some(latest_state.failed_operation as u64));
    assert_eq!(manifest.get("os_error_class").and_then(|value| value.as_u64()), Some(latest_state.os_error_class as u64));
    assert_eq!(manifest.get("os_error_code").and_then(|value| value.as_i64()), Some(latest_state.os_error_code as i64));
    assert_eq!(
      manifest.get("last_selected_header_sequence").and_then(|value| value.as_u64()),
      Some(latest_state.last_selected_header_sequence)
    );
    assert_eq!(manifest.get("latest_failure").and_then(|value| value.as_str()), Some(latest_state.latest_failure.as_str()));
    assert!(manifest.get("components").and_then(|value| value.as_array()).is_some_and(|components| components
      .iter()
      .any(|component| component.get("kind").and_then(|value| value.as_str()) == Some("hot_tail"))));

    let artifacts = crate::engine::emergency_spill::scan_unapplied_for_database(&engine_path).unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].database_id.map(hex::encode).as_deref(), Some(spill.database_id.as_str()));
    assert_eq!(artifacts[0].incident_id.map(hex::encode).as_deref(), Some(spill.incident_id.as_str()));
    assert_eq!(artifacts[0].creation_sequence, spill.creation_sequence);

    assert!(engine.get_entry(b"chunk-1").unwrap().is_some());
    let rejected = engine.store_entry(EntryType::Chunk, b"chunk-2", b"blocked");
    assert!(matches!(rejected, Err(EngineError::DurabilityFailure(_))));
  }

  #[test]
  #[serial]
  fn durability_failure_spill_uses_critical_headroom_under_ordinary_hard_pressure() {
    let temp_dir = tempfile::tempdir().unwrap();
    let spill_dir = temp_dir.path().join("spill");
    let _restore = SpillTestEnv::new(&spill_dir);
    let engine_path = temp_dir.path().join("durability-spill-hard-pressure.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    engine.store_entry(EntryType::Chunk, b"critical-spill-chunk", b"preserve me").unwrap();
    let coordinator = engine.memory_coordinator();
    let policy = coordinator.snapshot().unwrap().policy.unwrap();
    coordinator
      .update_host_sample(HostMemorySample {
        rss_bytes: policy.hard_limit_bytes,
        host_available_bytes: Some(policy.host_available_floor_bytes.saturating_sub(1)),
        ..HostMemorySample::default()
      })
      .unwrap();

    engine.record_durability_failure(DurabilityOperation::AuthorityBarrier, "hard-pressure spill", "synthetic EIO");

    let report = engine.emergency_spill_report().expect("spill report");
    assert!(report.succeeded, "critical spill failed under ordinary hard pressure: {:?}", report.errors);
    let snapshot = coordinator.snapshot().unwrap();
    let owner = snapshot.owner(MemoryOwner::EmergencySpill).unwrap();
    assert!(owner.peak_reserved_bytes > 0, "spill bypassed its critical memory owner");
    assert_eq!(owner.reserved_bytes, 0);
    assert_eq!(owner.critical_reserved_bytes, 0);
    assert_eq!(owner.active_reservations, 0);
  }

  #[test]
  #[serial]
  fn durability_failure_streams_a_parseable_dirty_index_snapshot() {
    let temp_dir = tempfile::tempdir().unwrap();
    let spill_dir = temp_dir.path().join("spill");
    let _restore = SpillTestEnv::new(&spill_dir);
    let engine_path = temp_dir.path().join("durability-index-spill.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let config = crate::engine::index_config::IndexFieldConfig {
      name: "title".to_string(),
      index_type: "string".to_string(),
      source: None,
      min: None,
      max: None,
    };
    crate::engine::index_store::IndexManager::new(&engine)
      .update_index("/docs", "title", &config, &[b"bounded spill".to_vec()], &[0x5a; 32])
      .unwrap();

    engine.record_durability_failure(DurabilityOperation::AuthorityBarrier, "dirty-index spill", "synthetic EIO");

    let report = engine.emergency_spill_report().expect("spill report");
    assert!(report.succeeded, "dirty-index spill failed: {:?}", report.errors);
    assert_eq!(report.index_pending_mutations, 1);
    assert_eq!(report.index_dirty_saves, 1);
    let bytes = std::fs::read(report.index_buffer_path.expect("dirty index component")).unwrap();
    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(snapshot.get("format").and_then(serde_json::Value::as_str), Some("aeordb-index-buffer-spill-v1"));
    let saves = snapshot.get("dirty_saves").and_then(serde_json::Value::as_array).unwrap();
    assert_eq!(saves.len(), 1);
    let encoded = saves[0].pointer("/bytes_base64/data").and_then(serde_json::Value::as_str).unwrap();
    let serialized = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).unwrap();
    let restored = crate::engine::index_store::FieldIndex::deserialize(&serialized, engine.hash_algo().hash_length()).unwrap();
    assert_eq!(restored.field_name, "title");
    assert_eq!(restored.len(), 1);
  }

  #[test]
  fn failed_index_spill_write_releases_transient_emergency_memory() {
    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("durability-index-spill-write-failure.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let config = crate::engine::index_config::IndexFieldConfig {
      name: "title".to_string(),
      index_type: "string".to_string(),
      source: None,
      min: None,
      max: None,
    };
    crate::engine::index_store::IndexManager::new(&engine)
      .update_index("/docs", "title", &config, &[b"bounded spill".to_vec()], &[0x5a; 32])
      .unwrap();
    let mut memory = OperationMemoryBudget::new(
      &engine,
      "test emergency spill",
      MemoryOwner::EmergencySpill,
      AdmissionClass::Critical(CriticalMemoryPurpose::EmergencySpill),
      EMERGENCY_SPILL_BASE_WORKSPACE_BYTES,
      None,
    )
    .unwrap();
    let baseline = memory.checkpoint();
    let mut writer = FailAfterWriter { written: 0, fail_at: 256 };

    let error = engine
      .index_write_buffer
      .lock()
      .unwrap()
      .write_emergency_snapshot(&mut writer, engine.hash_algo().hash_length(), &mut memory)
      .unwrap_err();

    assert!(matches!(error, EngineError::IoError(ref source) if source.kind() == std::io::ErrorKind::StorageFull));
    assert_eq!(memory.checkpoint(), baseline, "failed serialization stranded transient emergency memory");
    drop(memory);
    let snapshot = engine.memory_coordinator().snapshot().unwrap();
    let owner = snapshot.owner(MemoryOwner::EmergencySpill).unwrap();
    assert_eq!(owner.reserved_bytes, 0);
    assert_eq!(owner.active_reservations, 0);
  }

  #[test]
  #[serial]
  fn optional_index_spill_refusal_preserves_authoritative_hot_tail_and_wal() {
    let temp_dir = tempfile::tempdir().unwrap();
    let spill_dir = temp_dir.path().join("spill");
    let _restore = SpillTestEnv::new(&spill_dir);
    let engine_path = temp_dir.path().join("durability-partial-spill.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    engine.store_entry(EntryType::Chunk, b"authoritative-hot-entry", b"must survive").unwrap();
    let config = crate::engine::index_config::IndexFieldConfig {
      name: "title".to_string(),
      index_type: "string".to_string(),
      source: None,
      min: None,
      max: None,
    };
    crate::engine::index_store::IndexManager::new(&engine)
      .update_index("/docs", "title", &config, &[b"rebuildable index".to_vec()], &[0x6b; 32])
      .unwrap();
    let coordinator = engine.memory_coordinator();
    let snapshot = coordinator.snapshot().unwrap();
    let policy = snapshot.policy.unwrap();
    let pressure_bytes = policy
      .emergency_reserve_bytes
      .saturating_sub(snapshot.critical_reserved_bytes)
      .saturating_sub(EMERGENCY_SPILL_BASE_WORKSPACE_BYTES)
      .saturating_sub(32 * 1024);
    let pressure = coordinator
      .reserve(MemoryOwner::HealthStatus, pressure_bytes, AdmissionClass::Critical(CriticalMemoryPurpose::HealthStatus))
      .unwrap();

    engine.record_durability_failure(DurabilityOperation::AuthorityBarrier, "partial spill", "synthetic EIO");

    let report = engine.emergency_spill_report().expect("spill report");
    assert!(report.succeeded, "authoritative spill components should survive optional index refusal: {:?}", report.errors);
    assert!(report.hot_tail_path.as_ref().is_some_and(|path| std::fs::metadata(path).is_ok()));
    assert!(report.manifest_path.as_ref().is_some_and(|path| std::fs::metadata(path).is_ok()));
    assert!(report.index_buffer_path.is_none());
    assert!(report.errors.iter().any(|error| error.contains("emergency index NVT rebuild admission failed")));
    drop(pressure);
    let released = coordinator.snapshot().unwrap();
    let owner = released.owner(MemoryOwner::EmergencySpill).unwrap();
    assert_eq!(owner.reserved_bytes, 0);
    assert_eq!(owner.active_reservations, 0);
  }

  #[test]
  fn graceful_shutdown_uses_critical_headroom_under_ordinary_hard_pressure() {
    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("shutdown-hard-pressure.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    engine.store_entry(EntryType::Chunk, b"shutdown-pressure-chunk", b"durable bytes").unwrap();
    let coordinator = engine.memory_coordinator();
    let policy = coordinator.snapshot().unwrap().policy.unwrap();
    coordinator
      .update_host_sample(HostMemorySample {
        rss_bytes: policy.hard_limit_bytes,
        host_available_bytes: Some(policy.host_available_floor_bytes.saturating_sub(1)),
        ..HostMemorySample::default()
      })
      .unwrap();

    engine.shutdown_with_drain_timeout(Duration::ZERO).unwrap();

    let snapshot = coordinator.snapshot().unwrap();
    let owner = snapshot.owner(MemoryOwner::Shutdown).unwrap();
    assert!(owner.peak_reserved_bytes > 0, "shutdown bypassed its critical memory owner");
    assert_eq!(owner.reserved_bytes, 0);
    assert_eq!(owner.critical_reserved_bytes, 0);
    assert_eq!(owner.active_reservations, 0);
  }

  #[test]
  #[serial]
  fn failed_shutdown_releases_critical_memory_before_emergency_spill() {
    let temp_dir = tempfile::tempdir().unwrap();
    let spill_dir = temp_dir.path().join("spill");
    let _restore = SpillTestEnv::new(&spill_dir);
    let engine_path = temp_dir.path().join("shutdown-failure-spill-order.aeordb");
    let engine = Arc::new(StorageEngine::create(engine_path.to_str().unwrap()).unwrap());
    engine.store_entry(EntryType::Chunk, b"shutdown-spill-order", b"authoritative bytes").unwrap();
    let poisoned_engine = Arc::clone(&engine);
    let poison = std::thread::spawn(move || {
      let _guard = poisoned_engine.index_write_buffer.lock().unwrap();
      panic!("inject index-buffer mutex poison");
    });
    assert!(poison.join().is_err());

    let coordinator = engine.memory_coordinator();
    let snapshot = coordinator.snapshot().unwrap();
    let policy = snapshot.policy.unwrap();
    let spill_only_headroom = EMERGENCY_SPILL_BASE_WORKSPACE_BYTES + 128 * 1024;
    let pressure_bytes =
      policy.emergency_reserve_bytes.saturating_sub(snapshot.critical_reserved_bytes).saturating_sub(spill_only_headroom);
    let pressure = coordinator
      .reserve(MemoryOwner::HealthStatus, pressure_bytes, AdmissionClass::Critical(CriticalMemoryPurpose::HealthStatus))
      .unwrap();

    let error = engine.shutdown_with_drain_timeout(Duration::ZERO).unwrap_err();

    assert!(matches!(error, EngineError::DurabilityFailure(_)));
    let report = engine.emergency_spill_report().expect("shutdown failure must attempt emergency spill");
    assert!(report.succeeded, "shutdown memory overlapped and starved emergency spill: {:?}", report.errors);
    let after_failure = coordinator.snapshot().unwrap();
    let shutdown = after_failure.owner(MemoryOwner::Shutdown).unwrap();
    assert_eq!(shutdown.reserved_bytes, 0);
    assert_eq!(shutdown.active_reservations, 0);
    assert!(!engine.shutdown_flush_started.load(Ordering::Acquire));

    drop(pressure);
    engine.index_write_buffer.clear_poison();
  }

  #[test]
  fn shutdown_admission_refusal_releases_state_and_allows_retry() {
    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("shutdown-admission-retry.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    engine.store_entry(EntryType::Chunk, b"shutdown-retry-chunk", b"durable bytes").unwrap();
    let coordinator = engine.memory_coordinator();
    let snapshot = coordinator.snapshot().unwrap();
    let policy = snapshot.policy.unwrap();
    let pressure_bytes = policy.emergency_reserve_bytes.saturating_sub(snapshot.critical_reserved_bytes).saturating_sub(1);
    let pressure = coordinator
      .reserve(MemoryOwner::HealthStatus, pressure_bytes, AdmissionClass::Critical(CriticalMemoryPurpose::HealthStatus))
      .unwrap();

    let refused = engine.shutdown_with_drain_timeout(Duration::ZERO).unwrap_err();

    assert!(matches!(refused, EngineError::ResourceExhausted(_)));
    let refused_snapshot = coordinator.snapshot().unwrap();
    let shutdown = refused_snapshot.owner(MemoryOwner::Shutdown).unwrap();
    assert_eq!(shutdown.reserved_bytes, 0);
    assert_eq!(shutdown.active_reservations, 0);
    drop(pressure);
    engine.shutdown_with_drain_timeout(Duration::ZERO).unwrap();
  }

  #[test]
  #[serial]
  fn repair_authority_never_bypasses_a_new_runtime_durability_failure() {
    let temp_dir = tempfile::tempdir().unwrap();
    let spill_dir = temp_dir.path().join("spill");
    let _restore = SpillTestEnv::new(&spill_dir);
    let engine_path = temp_dir.path().join("repair-runtime-failure.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let _authority = engine.acquire_durability_repair_authority().unwrap();

    engine.record_durability_failure(DurabilityOperation::AuthorityBarrier, "failure during explicit repair", "synthetic EIO");

    let error = engine.ensure_writable().unwrap_err();
    assert!(error.to_string().contains("failure during explicit repair"), "{error}");
  }

  #[test]
  #[serial]
  fn later_failure_retries_external_spill_with_the_original_incident_identity() {
    let temp_dir = tempfile::tempdir().unwrap();
    let blocked_root = temp_dir.path().join("blocked-root");
    std::fs::write(&blocked_root, b"not a directory").unwrap();
    let restore = SpillTestEnv::new(&blocked_root);
    let engine_path = temp_dir.path().join("spill-retry.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    engine.store_entry(EntryType::Chunk, b"chunk-1", b"hello").unwrap();

    engine.record_durability_failure(DurabilityOperation::AuthorityBarrier, "first durability failure", "synthetic EIO");
    let first_state = engine.durability_failure_state().unwrap();
    assert!(!engine.emergency_spill_report().unwrap().succeeded);

    let recovered_root = temp_dir.path().join("recovered-root");
    restore.set_spill_dir(&recovered_root);
    engine.record_durability_failure(DurabilityOperation::DataBarrier, "second durability failure", "synthetic ENOSPC");
    let latest_state = engine.durability_failure_state().unwrap();
    let report = engine.emergency_spill_report().unwrap();
    assert!(report.succeeded, "retry failed: {:?}", report.errors);
    assert_eq!(latest_state.database_id, first_state.database_id);
    assert_eq!(latest_state.incident_id, first_state.incident_id);
    assert_eq!(report.database_id, hex::encode(first_state.database_id));
    assert_eq!(report.incident_id, hex::encode(first_state.incident_id));
    assert_eq!(report.latest_failure_at_ms, latest_state.latest_failure_at_ms);
    assert_eq!(crate::engine::emergency_spill::scan_unapplied_for_database(&engine_path).unwrap().len(), 1);
  }

  #[cfg(unix)]
  #[test]
  fn emergency_spill_writer_refuses_existing_or_symlinked_components() {
    use std::os::unix::fs::symlink;

    let temp_dir = tempfile::tempdir().unwrap();
    let existing = temp_dir.path().join("existing.bin");
    std::fs::write(&existing, b"original").unwrap();
    assert!(StorageEngine::write_durable_file(&existing, b"replacement").is_err());
    assert_eq!(std::fs::read(&existing).unwrap(), b"original");

    let outside = temp_dir.path().join("outside.bin");
    std::fs::write(&outside, b"outside").unwrap();
    let linked = temp_dir.path().join("linked.bin");
    symlink(&outside, &linked).unwrap();
    assert!(StorageEngine::write_durable_file(&linked, b"replacement").is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
  }

  #[test]
  fn emergency_wal_copy_uses_the_pinned_database_handle_after_path_replacement() {
    let temp_dir = tempfile::tempdir().unwrap();
    let database = temp_dir.path().join("database.aeordb");
    let original = temp_dir.path().join("original.aeordb");
    let destination = temp_dir.path().join("wal-tail.bin");
    std::fs::write(&database, b"abcdef").unwrap();
    let pinned = std::fs::File::open(&database).unwrap();
    std::fs::rename(&database, &original).unwrap();
    std::fs::write(&database, b"BADBAD").unwrap();

    let (_, copied, _, digest) = StorageEngine::copy_wal_tail_to_file(&pinned, &destination, 3, 6).unwrap();
    assert_eq!(copied, 3);
    assert_eq!(std::fs::read(&destination).unwrap(), b"def");
    assert_eq!(digest, *blake3::hash(b"def").as_bytes());
  }

  #[test]
  #[serial]
  fn emergency_spill_keeps_database_identity_when_the_writer_lock_is_unavailable() {
    let temp_dir = tempfile::tempdir().unwrap();
    let spill_dir = temp_dir.path().join("spill");
    let _restore = SpillTestEnv::new(&spill_dir);
    let engine_path = temp_dir.path().join("writer-busy-spill.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();

    let writer = engine.writer.write().unwrap();
    engine.record_durability_failure(DurabilityOperation::AuthorityWrite, "writer unavailable", "synthetic EIO");
    drop(writer);

    let report = engine.emergency_spill_report().unwrap();
    assert!(report.succeeded, "spill failed: {:?}", report.errors);
    assert_eq!(report.db_path.as_deref(), Some(engine_path.display().to_string().as_str()));
    assert_eq!(crate::engine::emergency_spill::scan_unapplied_for_database(&engine_path).unwrap().len(), 1);
  }

  #[test]
  fn transaction_guard_commit_surfaces_completion_failure() {
    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("transaction-completion-error.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let transaction = TransactionGuard::new(&engine).unwrap();

    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let _guard = engine.kv_writer.lock().unwrap();
      panic!("poison transaction state");
    }));

    let error = transaction.commit().expect_err("transaction completion failure must reach the caller");
    assert!(error.to_string().contains("Failed to end transaction"));
  }

  #[test]
  fn grouped_driver_setup_failure_halts_its_admitted_waiter() {
    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("grouped-driver-setup-failure.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let coordinator = engine.writer.read().unwrap().durability_coordinator();

    engine.begin_transaction().unwrap();
    let ticket = engine.prepare_transaction_completion().unwrap().expect("hard ticket");
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let _guard = engine.writer.write().unwrap();
      panic!("poison grouped writer authority");
    }));

    assert!(engine.complete_transaction_ticket(ticket).is_err());
    let snapshot = coordinator.snapshot().unwrap();
    assert_eq!(snapshot.pending_hard, 0);
    assert_eq!(snapshot.failed, 0, "the failed driver must retire its own terminal waiter record");
  }

  #[test]
  fn grouped_namespace_setup_failure_halts_and_retires_its_waiter() {
    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("grouped-namespace-setup-failure.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let coordinator = Arc::clone(&engine.durability_coordinator);

    engine.begin_transaction().unwrap();
    let ticket = engine.prepare_transaction_completion().unwrap().expect("hard ticket");
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let _guard = engine.namespace_write_lock.lock().unwrap();
      panic!("poison grouped namespace authority");
    }));

    assert!(engine.complete_transaction_ticket(ticket).is_err());
    let snapshot = coordinator.snapshot().unwrap();
    assert_eq!(snapshot.pending_hard, 0);
    assert_eq!(snapshot.failed, 0, "the failed driver must retire its own terminal waiter record");
  }

  #[test]
  fn gc_sweep_refuses_poisoned_void_manager_before_kv_removal() {
    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("gc-poisoned-void-manager.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();
    let garbage_key = engine.compute_hash(b"gc-poisoned-void-garbage").unwrap();
    engine.store_entry(EntryType::Chunk, &garbage_key, b"garbage").unwrap();
    let live = crate::engine::gc::gc_mark(&engine).unwrap();
    assert!(!live.contains(&garbage_key));

    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let _guard = engine.void_manager.write().unwrap();
      panic!("poison void manager for GC refusal test");
    }));

    let error = crate::engine::gc::gc_sweep(&engine, &live, false)
      .expect_err("GC must not remove KV rows when void registration authority is unavailable");
    assert!(matches!(error, EngineError::IoError(_)));
    assert!(engine.has_entry(&garbage_key).unwrap(), "preflight refusal must leave the candidate in the KV view");
  }

  #[test]
  fn syncing_void_snapshot_surfaces_poisoned_void_manager() {
    let temp_dir = tempfile::tempdir().unwrap();
    let engine_path = temp_dir.path().join("poisoned-void-snapshot.aeordb");
    let engine = StorageEngine::create(engine_path.to_str().unwrap()).unwrap();

    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      let _guard = engine.void_manager.write().unwrap();
      panic!("poison void manager for snapshot propagation test");
    }));

    let error = engine.sync_voids_to_kv_writer().expect_err("void snapshot synchronization failures must never become success");
    assert!(matches!(error, EngineError::IoError(_)));
  }
}

thread_local! {
  static NAMESPACE_WRITE_STACK: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
  static ENGINE_OPERATION_STACK: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct DurabilityRepairAuthorityGuard<'a> {
  engine: &'a StorageEngine,
  owner: std::thread::ThreadId,
}

impl Drop for DurabilityRepairAuthorityGuard<'_> {
  fn drop(&mut self) {
    let mut active = self.engine.durability_repair_owner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if active.as_ref() == Some(&self.owner) {
      *active = None;
    }
  }
}

pub(crate) struct NamespaceWriteGuard<'a> {
  engine_id: usize,
  _guard: Option<MutexGuard<'a, ()>>,
  _operation: EngineOperationGuard<'a>,
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

/// Guard that begins a transaction and requires explicit fallible completion
/// before a successful mutation may be acknowledged.
///
/// While this guard is alive, `DiskKVStore::flush()` will skip truncating the
/// hot file, ensuring crash recovery can replay all entries written during
/// the transaction. [`commit`](Self::commit) surfaces the hard-commit result to
/// the caller. Construction is fallible because shutdown rejects new top-level
/// transactions. The guard also keeps the transaction visible to shutdown until
/// its hard durability ticket reaches a terminal state. `Drop` remains a
/// best-effort cleanup for early error paths, where there is no successful
/// acknowledgement to protect.
pub struct TransactionGuard<'a> {
  engine: &'a StorageEngine,
  _operation: EngineOperationGuard<'a>,
  completed: bool,
}

impl<'a> TransactionGuard<'a> {
  pub fn new(engine: &'a StorageEngine) -> EngineResult<Self> {
    let operation = engine.operation_guard("namespace_transaction")?;
    engine.begin_transaction()?;
    Ok(TransactionGuard { engine, _operation: operation, completed: false })
  }

  pub fn commit(mut self) -> EngineResult<()> {
    self.completed = true;
    self.engine.end_transaction()
  }

  pub(crate) fn commit_after(mut self, namespace: NamespaceWriteGuard<'a>) -> EngineResult<()> {
    self.completed = true;
    self.engine.end_transaction_after(namespace)
  }

  pub fn finish<T>(self, result: EngineResult<T>) -> EngineResult<T> {
    let engine = self.engine;
    let completion = self.commit();
    match (result, completion) {
      (_, Err(error)) => Err(error),
      (Err(error), Ok(())) => Err(engine.normalize_runtime_write_error(
        DurabilityOperation::DataBarrier,
        "Transaction failed after storage mutation began",
        error,
      )),
      (Ok(value), Ok(())) => Ok(value),
    }
  }

  pub(crate) fn finish_after<T>(self, result: EngineResult<T>, namespace: NamespaceWriteGuard<'a>) -> EngineResult<T> {
    let engine = self.engine;
    let completion = self.commit_after(namespace);
    match (result, completion) {
      (_, Err(error)) => Err(error),
      (Err(error), Ok(())) => Err(engine.normalize_runtime_write_error(
        DurabilityOperation::DataBarrier,
        "Namespace transaction failed after storage mutation began",
        error,
      )),
      (Ok(value), Ok(())) => Ok(value),
    }
  }
}

impl<'a> Drop for TransactionGuard<'a> {
  fn drop(&mut self) {
    if self.completed {
      return;
    }
    if let Err(error) = self.engine.end_transaction() {
      tracing::error!("Transaction guard failed to clean up an incomplete operation: {}", error);
    }
  }
}
