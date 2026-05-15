use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

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
use crate::engine::kv_snapshot::ReadSnapshot;
use serde::Serialize;

use crate::engine::kv_store::{
  KVEntry,
  KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY,
  KV_TYPE_SNAPSHOT, KV_TYPE_FORK,
  KV_FLAG_DELETED,
};
use crate::engine::void_manager::VoidManager;

/// A buffered batch of entries to write in one sequential operation.
///
/// Accumulates entries in memory and flushes them all with a single lock
/// acquisition via [`StorageEngine::flush_batch`]. This avoids per-entry
/// lock overhead when writing many entries at once.
pub struct WriteBatch {
    entries: Vec<BatchEntry>,
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
        self.entries.push(BatchEntry {
            entry_type,
            key,
            value,
            kv_type: entry_type.to_kv_type(),
        });
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
  writer: RwLock<AppendWriter>,
  kv_writer: Mutex<DiskKVStore>,
  pub(crate) kv_snapshot: Arc<ArcSwap<ReadSnapshot>>,
  // The VoidManager tracks reclaimable space for future void-reuse optimization.
  // Currently, find_void is not called by any production code -- new entries always
  // append. When void reuse is implemented, store_entry will check find_void
  // before appending, writing into reclaimed space to reduce file growth.
  // TODO: Wire into store_entry/delete_entry to reclaim void space before appending.
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
  pub(crate) last_auto_snapshot_delete: std::sync::atomic::AtomicI64,
  pub(crate) last_auto_snapshot_restore: std::sync::atomic::AtomicI64,
  pub(crate) last_manual_snapshot: std::sync::atomic::AtomicI64,
  /// Cache of directory content keyed by content hash. Content-addressed data
  /// is immutable, so this cache can never serve stale data for a given key.
  /// Populated by update_parent_directories, read by directory lookups.
  pub(crate) dir_content_cache: RwLock<HashMap<Vec<u8>, Vec<u8>>>,
  /// GC recheck queue. While GC mark+sweep runs, every successful write hash
  /// is added here so the sweep phase can avoid clobbering entries that were
  /// written after the mark snapshot was captured. `None` means GC is not
  /// active and writes don't bother recording. See bot-docs/plan/gc-mark-sweep.md.
  pub(crate) gc_recheck: Mutex<Option<HashSet<Vec<u8>>>>,
  #[allow(dead_code)]
  _file_lock: std::fs::File,
}

impl StorageEngine {
  /// Acquire an exclusive advisory file lock. Returns the locked file handle
  /// which must be kept alive for the duration of the engine's lifetime.
  /// If another process already holds the lock, returns an error immediately.
  fn acquire_file_lock(lock_path: &str) -> EngineResult<std::fs::File> {
    let lock_file = OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(false)
      .open(lock_path)
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(format!("Failed to create lock file '{}': {}", lock_path, error)),
      ))?;

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
        let _ = crate::engine::hot_tail::write_hot_tail(&mut f, hot_tail_offset, &empty, hash_length);
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
      writer: RwLock::new(writer),
      kv_writer: Mutex::new(kv_store),
      kv_snapshot,
      void_manager: RwLock::new(void_manager),
      hash_algo,
      counters: ArcSwap::from_pointee(EngineCounters::new()),
      permissions_cache: Arc::new(Cache::new(PermissionsLoader)),
      index_config_cache: Arc::new(Cache::new(IndexConfigLoader)),
      last_auto_snapshot_delete: std::sync::atomic::AtomicI64::new(0),
      last_auto_snapshot_restore: std::sync::atomic::AtomicI64::new(0),
      last_manual_snapshot: std::sync::atomic::AtomicI64::new(0),
      dir_content_cache: RwLock::new(HashMap::new()),
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
  fn open_internal(path: &str, _hot_dir: Option<&Path>) -> EngineResult<Self> {
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
      tracing::info!(
        current_stage, resize_target,
        "Pending KV block expansion detected — expanding before opening"
      );
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
          tracing::warn!(
            hot_tail_offset,
            "Corrupt or missing hot tail — will rebuild KV from WAL (dirty startup)"
          );
          (crate::engine::hot_tail::HotTailPayload::default(), true)
        }
      }
    } else {
      (crate::engine::hot_tail::HotTailPayload::default(), false)
    };
    let hot_entries = hot_payload.writes.clone();
    let hot_voids = hot_payload.voids;

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
        kv_file, hash_algo, kv_block_offset, hot_tail_offset,
        kv_block_stage, hot_entries,
      )?;
      // If any bucket page failed CRC on open, the KV index is unreliable
      // for the buckets involved — trigger dirty startup below so the WAL
      // scan is the source of truth.
      if kv.needs_rebuild {
        detected_kv_corruption = true;
      }

      // Scan for void entries (in-memory, not in KV)
      let scanner = writer.scan_entries()?;
      for scanned_result in scanner {
        let scanned = match scanned_result {
          Ok(entry) => entry,
          Err(e) => {
            tracing::warn!("Skipping corrupt entry during void scan: {}", e);
            continue;
          }
        };
        if scanned.header.entry_type == EntryType::Void {
          void_manager.register_void(scanned.offset, scanned.header.total_length);
        }
      }

      kv
    } else {
      // No valid KV block — create from full WAL scan (dirty startup)
      let kv_block_offset = crate::engine::file_header::HEADER_REGION_SIZE as u64;
      let kv_block_length = crate::engine::kv_stages::initial_block_size();
      let hot_tail_offset = kv_block_offset + kv_block_length;

      let kv_file = OpenOptions::new().read(true).write(true).open(path)?;
      let mut kv = DiskKVStore::create(kv_file, hash_algo, kv_block_offset, hot_tail_offset, 0)?;

      // First pass: rebuild KV store from entry headers, collecting deletion records.
      // Each deletion record stores (path, scan_offset) so we can avoid re-deleting
      // files that were recreated after the deletion.
      let mut deletion_records: Vec<(String, u64)> = Vec::new();
      let scanner = writer.scan_entries()?;
      for scanned_result in scanner {
        let scanned = match scanned_result {
          Ok(entry) => entry,
          Err(e) => {
            tracing::warn!("Skipping corrupt entry during rebuild: {}", e);
            continue;
          }
        };
        // Collect deletion records and register void entries during the scan.
        if scanned.header.entry_type == EntryType::DeletionRecord {
          if let Ok(record) = crate::engine::deletion_record::DeletionRecord::deserialize(&scanned.value) {
            deletion_records.push((record.path, scanned.offset));
          }
        } else if scanned.header.entry_type == EntryType::Void {
          void_manager.register_void(scanned.offset, scanned.header.total_length);
        }
        let kv_type = scanned.header.entry_type.to_kv_type();

        let entry = KVEntry {
          type_flags: kv_type,
          hash: scanned.key.clone(),
          offset: scanned.offset,
          total_length: scanned.header.total_length,
        };
        kv.insert(entry)?;
      }

      // Flush write buffer to disk before deletion replay
      kv.flush()?;

      // Second pass: replay deletions — re-mark entries as deleted.
      // Only applies if the file/entry wasn't recreated after the deletion
      // (i.e., the KV entry's offset is before the deletion record's offset).
      for (path, deletion_offset) in &deletion_records {
        let normalized = crate::engine::path_utils::normalize_path(path);

        // Try file path hash (standard file deletions use "file:" prefix)
        if let Ok(file_key) = hash_algo.compute_hash(format!("file:{}", normalized).as_bytes()) {
          if let Some(entry) = kv.get(&file_key) {
            if entry.offset < *deletion_offset {
              kv.update_flags(&file_key, KV_FLAG_DELETED);
            }
          }
        }

        // Try raw hash (for snapshot/fork deletions that store the domain-prefixed
        // key directly, e.g. "snap:name" or "::aeordb:fork:name").
        // Use the raw path string without normalization since these aren't file paths.
        if let Ok(raw_key) = hash_algo.compute_hash(path.as_bytes()) {
          if let Some(entry) = kv.get(&raw_key) {
            if entry.offset < *deletion_offset {
              kv.update_flags(&raw_key, KV_FLAG_DELETED);
            }
          }
        }
      }

      // Flush any deletion flag updates
      kv.flush()?;

      kv
    };

    // Hot tail entries are already loaded into the DiskKVStore write buffer
    // by DiskKVStore::open() — no separate replay step needed.

    let kv_snapshot = Arc::clone(kv_store.snapshot_handle());

    let engine = StorageEngine {
      writer: RwLock::new(writer),
      kv_writer: Mutex::new(kv_store),
      kv_snapshot,
      void_manager: RwLock::new(void_manager),
      hash_algo,
      counters: ArcSwap::from_pointee(EngineCounters::new()),
      permissions_cache: Arc::new(Cache::new(PermissionsLoader)),
      index_config_cache: Arc::new(Cache::new(IndexConfigLoader)),
      last_auto_snapshot_delete: std::sync::atomic::AtomicI64::new(0),
      last_auto_snapshot_restore: std::sync::atomic::AtomicI64::new(0),
      last_manual_snapshot: std::sync::atomic::AtomicI64::new(0),
      dir_content_cache: RwLock::new(HashMap::new()),
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
      engine.rebuild_kv()?;
      // Re-initialize counters from the freshly rebuilt KV
      let refreshed = Arc::new(EngineCounters::initialize_from_kv(&engine));
      engine.counters.store(refreshed);

      // Dirty rebuild lost the hot tail's void state. Re-derive voids by
      // gap-scanning the rebuilt KV (sorted by offset, ignoring deleted
      // entries) — any byte range not covered by a live KV entry is a void.
      engine.recover_voids_via_gap_scan()?;
    }

    // Seed the DiskKVStore's pending_voids snapshot from the loaded
    // VoidManager state so the next hot tail flush carries it forward.
    engine.sync_voids_to_kv_writer();

    Ok(engine)
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
    let wal_start: u64 = {
      let writer = self.writer.read()
        .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let hdr = writer.file_header();
      hdr.kv_block_offset + hdr.kv_block_length
    };

    // Collect (offset, total_length) of all live (non-deleted) entries.
    let mut ranges: Vec<(u64, u32)> = {
      let snapshot = self.kv_snapshot.load();
      let entries = snapshot.iter_all()?;
      entries.iter()
        .filter(|e| !e.is_deleted())
        .map(|e| (e.offset, e.total_length))
        .collect()
    };
    ranges.sort_by_key(|(offset, _)| *offset);

    let mut vm = self.void_manager.write()
      .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;

    let mut cursor: u64 = wal_start;
    for (offset, total_length) in &ranges {
      if *offset > cursor {
        let gap_size = *offset - cursor;
        let gap_size_u32 = u32::try_from(gap_size).unwrap_or(u32::MAX);
        vm.register_void(cursor, gap_size_u32);
      }
      cursor = offset + *total_length as u64;
    }

    tracing::info!(
      void_count = vm.void_count(),
      total_void_bytes = vm.total_void_space(),
      wal_start,
      "Recovered voids via gap-scan after dirty startup"
    );

    Ok(())
  }

  /// Force an immediate hot tail flush. Used by GC sweep after registering
  /// new voids so the void state is durable without waiting for the normal
  /// threshold trigger.
  pub(crate) fn force_hot_tail_flush(&self) -> EngineResult<()> {
    let mut kv = self.kv_writer.lock()
      .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
    kv.force_flush_hot_buffer()
  }

  /// Mirror VoidManager state into the DiskKVStore's pending_voids so the
  /// next hot tail flush includes the current void snapshot. Call after any
  /// operation that changes the void set (GC sweep, void consumption,
  /// startup population). Also refreshes the void_count + void_space
  /// counters so dashboard metrics stay accurate.
  pub(crate) fn sync_voids_to_kv_writer(&self) {
    let (voids, count, total_bytes) = match self.void_manager.read() {
      Ok(vm) => {
        let collected: Vec<crate::engine::hot_tail::VoidRecord> = vm.iter()
          .map(|(offset, size)| crate::engine::hot_tail::VoidRecord { offset, size })
          .collect();
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
    let engine = Self::open_internal(path, None)?;

    // Guard: refuse to open patch databases as normal databases
    let header = engine.writer.read()
      .map_err(|e| EngineError::IoError(std::io::Error::other(format!("writer lock poisoned: {}", e))))?
      .file_header().clone();
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
    let engine = Self::open_internal(path, hot_dir)?;

    // Guard: refuse to open patch databases as normal databases
    let header = engine.writer.read()
      .map_err(|e| EngineError::IoError(std::io::Error::other(format!("writer lock poisoned: {}", e))))?
      .file_header().clone();
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
    Self::open_internal(path, None)
  }

  /// Store an entry: append to file, register in KV store.
  /// Returns the file offset where the entry was written.
  ///
  /// Both the writer and KV locks are held simultaneously to prevent a
  /// TOCTOU gap where a crash between the disk write and the KV insert
  /// could leave the entry on disk but missing from the index.
  /// Lock order: writer first, then KV (must be consistent everywhere).
  pub fn store_entry(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
  ) -> EngineResult<u64> {
    self.store_entry_internal(entry_type, key, value, 0, CompressionAlgorithm::None)
  }

  pub fn store_entry_with_flags(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    flags: u8,
  ) -> EngineResult<u64> {
    self.store_entry_internal(entry_type, key, value, flags, CompressionAlgorithm::None)
  }

  pub fn store_entry_compressed(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<u64> {
    self.store_entry_internal(entry_type, key, value, 0, compression_algo)
  }

  pub fn store_entry_compressed_with_flags(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    flags: u8,
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<u64> {
    self.store_entry_internal(entry_type, key, value, flags, compression_algo)
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
  ) -> EngineResult<u64> {
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;

    // Try to consume a void first. If we find one that's big enough, write
    // the entry in-place at the void's offset instead of growing the WAL.
    // This is how the GC's freed space gets recycled into new writes.
    //
    // The size is computed from the caller-provided `value` length — for
    // compressed entries, the caller has already compressed the bytes and
    // `value` holds the compressed payload, so compute_total_length gives
    // the right disk size.
    let needed = crate::engine::entry_header::EntryHeader::compute_total_length(
      self.hash_algo, key.len(), value.len(),
    )?;
    let void_slot = if let Ok(mut vm) = self.void_manager.write() {
      vm.find_void(needed)
    } else {
      None
    };
    let voids_changed_via_consume = void_slot.is_some();

    let (offset, total_length) = if let Some((void_offset, _void_size)) = void_slot {
      // In-place write at the void's offset. The void is already removed
      // from void_manager (find_void did it). After this write, the bytes
      // at void_offset belong to the new entry.
      //
      // No explicit fsync here — void-consumption writes ride the same
      // hot-tail-flush durability path as appends. The whole point of this
      // plumbing is to AVOID per-entry random fsyncs.
      let written = writer.write_entry_at_nosync_full(
        void_offset, entry_type, key, value, flags, compression_algo,
      )?;
      (void_offset, written)
    } else {
      writer.append_entry_with_compression(
        entry_type, key, value, flags, compression_algo,
      )?
    };
    kv.set_hot_tail_offset(writer.current_offset());

    let kv_entry = KVEntry {
      type_flags: entry_type.to_kv_type(),
      hash: key.to_vec(),
      offset,
      total_length,
    };
    kv.insert(kv_entry)?;
    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    // If we consumed a void, the pending_voids snapshot in kv_writer is stale
    // — refresh it so the next hot tail flush reflects the consumed state.
    if voids_changed_via_consume {
      if let Ok(vm) = self.void_manager.read() {
        let voids: Vec<crate::engine::hot_tail::VoidRecord> = vm.iter()
          .map(|(o, s)| crate::engine::hot_tail::VoidRecord { offset: o, size: s })
          .collect();
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
  pub fn get_entry(
    &self,
    hash: &[u8],
  ) -> EngineResult<Option<EntryData>> {
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get(hash) {
      Some(entry) if !entry.is_deleted() => entry,
      _ => return Ok(None),
    };

    // Use a READ lock — read_entry_at_shared uses a cloned file handle
    // so it doesn't disturb the writer's seek position.
    let writer = self.writer.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let result = writer.read_entry_at_shared(kv_entry.offset);
    if result.is_err() {
      tracing::debug!(
        offset = kv_entry.offset,
        hash = %hex::encode(&hash[..8.min(hash.len())]),
        type_flags = kv_entry.type_flags,
        kv_block_end = 256 + 65536,
        "get_entry: read failed at KV offset"
      );
    }
    result.map(Some)
  }

  /// Retrieve an entry by hash, including deleted entries.
  /// Used for version history where we need to read files that were
  /// deleted after a snapshot was taken.
  pub fn get_entry_including_deleted(
    &self,
    hash: &[u8],
  ) -> EngineResult<Option<EntryData>> {
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get_raw(hash) {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let writer = self.writer.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let result = writer.read_entry_at_shared(kv_entry.offset);
    result.map(Some)
  }

  /// Retrieve an entry by hash with BLAKE3 hash verification.
  /// Use this for user-facing reads (GET /files/) where integrity matters.
  /// Internal engine reads use `get_entry()` without verification for performance.
  pub fn get_entry_verified(
    &self,
    hash: &[u8],
  ) -> EngineResult<Option<EntryData>> {
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get(hash) {
      Some(entry) if !entry.is_deleted() => entry,
      _ => return Ok(None),
    };

    let writer = self.writer.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let (header, key, value) = writer.read_entry_at_shared_verified(kv_entry.offset)?;

    Ok(Some((header, key, value)))
  }

  /// Like `get_entry_verified` but includes entries marked as deleted.
  /// Needed for reading historical chunk data when streaming files from snapshots.
  pub fn get_entry_verified_including_deleted(
    &self,
    hash: &[u8],
  ) -> EngineResult<Option<EntryData>> {
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get_raw(hash) {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let writer = self.writer.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let (header, key, value) = writer.read_entry_at_shared_verified(kv_entry.offset)?;

    Ok(Some((header, key, value)))
  }

  /// Check if a non-deleted entry exists in the KV store (lock-free).
  pub fn has_entry(&self, hash: &[u8]) -> EngineResult<bool> {
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
    self.writer.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(format!("writer lock poisoned: {}", error)),
      ))
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

  /// Update the HEAD hash in the file header, pointing to a new root directory version.
  pub fn update_head(&self, head_hash: &[u8]) -> EngineResult<()> {
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let mut header = writer.file_header().clone();
    header.head_hash = head_hash.to_vec();
    writer.update_file_header(&header)?;
    Ok(())
  }

  /// Read the current HEAD hash from the file header. HEAD points to the
  /// content-addressed root directory and represents the latest version.
  pub fn head_hash(&self) -> EngineResult<Vec<u8>> {
    let writer = self.writer.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    Ok(writer.file_header().head_hash.clone())
  }

  /// Get the backup metadata from the file header.
  pub fn backup_info(&self) -> EngineResult<(u8, Vec<u8>, Vec<u8>)> {
    let writer = self.writer.read()
      .map_err(|e| EngineError::IoError(std::io::Error::other(format!("writer lock poisoned: {}", e))))?;
    let fh = writer.file_header();
    Ok((fh.backup_type, fh.base_hash.clone(), fh.target_hash.clone()))
  }

  /// Update the backup metadata in the file header.
  pub fn set_backup_info(&self, backup_type: u8, base_hash: &[u8], target_hash: &[u8]) -> EngineResult<()> {
    let mut writer = self.writer.write()
      .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
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
  pub fn store_entry_typed(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    kv_type: u8,
  ) -> EngineResult<u64> {
    // Acquire BOTH locks before any work to close the TOCTOU gap.
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;

    let (offset, total_length) = writer.append_entry(entry_type, key, value, 0)?;
    kv.set_hot_tail_offset(writer.current_offset());

    let kv_entry = KVEntry {
      type_flags: kv_type,
      hash: key.to_vec(),
      offset,
      total_length,
    };
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
    if batch.is_empty() {
      return Ok(Vec::new());
    }

    // Acquire BOTH locks before any work to close the TOCTOU gap.
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;

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
      let kv_entry = KVEntry {
        type_flags: entry.kv_type,
        hash: entry.key.clone(),
        offset: offsets[i],
        total_length: totals[i],
      };
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
    if batch.is_empty() {
      // Still update HEAD even if batch is empty (e.g., system path that skips propagation)
      return self.update_head(head_hash).map(|_| Vec::new());
    }

    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;

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
      let kv_entry = KVEntry {
        type_flags: entry.kv_type,
        hash: entry.key.clone(),
        offset: offsets[i],
        total_length: totals[i],
      };
      kv.insert(kv_entry)?;
    }

    // Update HEAD and hot_tail_offset in the same lock hold.
    // Including hot_tail_offset ensures the header always points to
    // where the hot tail actually is — prevents stale pointer on crash.
    let mut header = writer.file_header().clone();
    header.head_hash = head_hash.to_vec();
    header.hot_tail_offset = writer.current_offset();
    writer.update_file_header(&header)?;

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
    let hash_length = self.hash_algo.hash_length();
    let psize = crate::engine::kv_pages::page_size(hash_length);
    let (_block_size, new_bucket_count) = crate::engine::kv_stages::stage_params(target_stage, psize);
    let new_pages_size = (new_bucket_count as u64) * (psize as u64);

    // Acquire both locks: writer first, then KV
    let mut writer = self.writer.write()
      .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
    let mut kv = self.kv_writer.lock()
      .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;

    let header = writer.file_header().clone();
    let old_kv_end = header.kv_block_offset + header.kv_block_length;
    let new_kv_end = header.kv_block_offset + new_pages_size;
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
      if writer.read_bytes_at(scan_pos, &mut buf).is_err() { break; }
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
      growth_zone_size, old_kv_end, new_kv_end, hot_tail_offset,
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
    let hot_payload = writer.read_hot_tail_payload(hot_tail_offset, hash_length);

    // Copy growth zone [old_kv_end .. new_kv_end] to [hot_tail_offset ..]
    let copy_dst = hot_tail_offset;
    let new_hot_tail = hot_tail_offset + growth_zone_size;

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
      let min_void = (crate::engine::entry_header::EntryHeader::FIXED_HEADER_SIZE
        + self.hash_algo.hash_length()) as u32;
      if tail_size >= min_void {
        writer.write_void_at(new_kv_end, tail_size)?;
      }
    }

    // Update writer's offset to after the relocated data (before new hot tail)
    writer.set_offset(new_hot_tail);

    // Step 4-8: KV finalization (zero pages, rehash, update header, publish snapshot)
    let offset_delta: i64 = copy_dst as i64 - old_kv_end as i64;
    // Use actual_copy_end (not new_kv_end) for offset adjustment range —
    // entries straddling the boundary were also relocated.
    kv.finalize_expansion(target_stage, old_kv_end, actual_copy_end, offset_delta, new_hot_tail)?;

    // Update writer's header to match what KV wrote
    let mut final_header = writer.file_header().clone();
    final_header.kv_block_length = new_pages_size;
    final_header.kv_block_stage = target_stage as u8;
    final_header.resize_in_progress = false;
    final_header.resize_target_stage = 0;
    final_header.hot_tail_offset = new_hot_tail;
    writer.update_file_header(&final_header)?;

    // Sync the writer's file handle to ensure it sees the KV's changes
    writer.sync()?;

    tracing::info!("Online KV block expansion complete");

    Ok(())
  }

  /// Check if a KV entry is marked as deleted.
  pub fn is_entry_deleted(&self, hash: &[u8]) -> EngineResult<bool> {
    let snapshot = self.kv_snapshot.load();
    match snapshot.get_raw(hash) {
      Some(entry) => Ok(entry.is_deleted()),
      None => Ok(false),
    }
  }

  /// Mark a KV entry as deleted by setting the deleted flag.
  pub fn mark_entry_deleted(&self, hash: &[u8]) -> EngineResult<()> {
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let updated = kv.update_flags(hash, KV_FLAG_DELETED);
    if !updated {
      return Err(EngineError::NotFound(
        format!("Entry not found for hash: {}", hex::encode(hash)),
      ));
    }
    Ok(())
  }

  /// Read only the entry header at a given file offset.
  /// Used by GC to determine entry size without reading the full payload.
  pub fn read_entry_header_at(&self, offset: u64) -> EngineResult<EntryHeader> {
    // Use a READ lock — read_entry_at_shared uses a cloned file handle.
    let writer = self.writer.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let (header, _key, _value) = writer.read_entry_at_shared(offset)?;
    Ok(header)
  }

  /// Write a DeletionRecord entry at a specific file offset (in-place).
  /// Returns the total bytes written.
  pub fn write_deletion_at(&self, offset: u64, path: &str) -> EngineResult<u32> {
    let deletion = crate::engine::deletion_record::DeletionRecord::new(
      path.to_string(),
      Some("gc".to_string()),
    );
    let value = deletion.serialize();
    let key = self.compute_hash(
      format!("del:gc:{}:{}", path, deletion.deleted_at).as_bytes(),
    )?;

    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    writer.write_entry_at(offset, EntryType::DeletionRecord, &key, &value)
  }

  /// Write a void entry at a specific file offset (in-place).
  pub fn write_void_at(&self, offset: u64, size: u32) -> EngineResult<()> {
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    writer.write_void_at(offset, size)?;

    let mut vm = self.void_manager.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    vm.register_void(offset, size);

    Ok(())
  }

  /// Write a DeletionRecord in-place WITHOUT syncing. Used by GC batch sweep.
  pub fn write_deletion_at_nosync(&self, offset: u64, path: &str) -> EngineResult<u32> {
    let deletion = crate::engine::deletion_record::DeletionRecord::new(
      path.to_string(),
      Some("gc".to_string()),
    );
    let value = deletion.serialize();
    let key = self.compute_hash(
      format!("del:gc:{}:{}", path, deletion.deleted_at).as_bytes(),
    )?;

    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    writer.write_entry_at_nosync(offset, EntryType::DeletionRecord, &key, &value)
  }

  /// Write a void in-place WITHOUT syncing. Used by GC batch sweep.
  pub fn write_void_at_nosync(&self, offset: u64, size: u32) -> EngineResult<()> {
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    writer.write_void_at_nosync(offset, size)?;

    let mut vm = self.void_manager.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    vm.register_void(offset, size);

    Ok(())
  }

  /// Sync the append writer to disk. Call after batch nosync operations.
  pub fn sync_writer(&self) -> EngineResult<()> {
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    writer.sync()
  }

  /// Batch remove multiple entries from the KV store. Publishes snapshot once at the end.
  pub fn remove_kv_entries_batch(&self, hashes: &[Vec<u8>]) -> EngineResult<()> {
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    kv.mark_deleted_batch(hashes);
    Ok(())
  }

  /// Remove an entry from the KV store (mark deleted). Used by GC sweep.
  pub fn remove_kv_entry(&self, hash: &[u8]) -> EngineResult<()> {
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    kv.mark_deleted(hash);
    Ok(())
  }

  /// Iterate all live KV entries. Used by GC sweep.
  pub fn iter_kv_entries(&self) -> EngineResult<Vec<KVEntry>> {
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
    let entries: Vec<(Vec<u8>, u64)> = {
      let snapshot = self.kv_snapshot.load();
      snapshot.iter_by_type(target_type)
        .into_iter()
        .map(|entry| (entry.hash, entry.offset))
        .collect()
    };

    let mut results = Vec::with_capacity(entries.len());
    let writer = self.writer.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;

    for (hash, offset) in entries {
      let (_header, _key, value) = match writer.read_entry_at_shared(offset) {
        Ok(entry) => entry,
        Err(e) => {
          tracing::warn!("Skipping corrupt entry at offset {} during entries_by_type: {}", offset, e);
          continue;
        }
      };
      results.push((hash, value));
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

    // Type counts from the prebuilt type index — O(1) per type.
    let chunk_count = snapshot.iter_by_type(KV_TYPE_CHUNK).len();
    let file_count = snapshot.iter_by_type(KV_TYPE_FILE_RECORD).len();
    let directory_count = snapshot.iter_by_type(KV_TYPE_DIRECTORY).len();
    let snapshot_count = snapshot.iter_by_type(KV_TYPE_SNAPSHOT).len();
    let fork_count = snapshot.iter_by_type(KV_TYPE_FORK).len();

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
    tracing::info!("Rebuilding KV index from append log...");
    let timer = std::time::Instant::now();

    let hash_algo = self.hash_algo;

    // Scan the append log (needs read lock on writer)
    // For directory entries, we track value_length so we can prefer
    // entries with children over empty entries (e.g., root directory
    // overwritten by ensure_root_directory on a corrupt session).
    // Tuple: (type_flags, hash, offset, value_length, total_length).
    // value_length is used for the empty-directory dedup heuristic;
    // total_length is needed to populate KVEntry.total_length for the
    // void-manager gap-scan that runs on dirty startup.
    let entries: Vec<(u8, Vec<u8>, u64, u32, u32)> = {
      let writer = self.writer.read()
        .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      tracing::debug!(
        writer_offset = writer.current_offset(),
        file_path = %writer.file_path().display(),
        "rebuild_kv: scanning WAL to EOF (dirty recovery)"
      );
      let scanner = writer.scan_entries_dirty_recovery()?;
      let mut collected = Vec::new();
      for result in scanner {
        match result {
          Ok(scanned) => {
            collected.push((
              scanned.header.entry_type.to_kv_type(),
              scanned.key.clone(),
              scanned.offset,
              scanned.header.value_length,
              scanned.header.total_length,
            ));
          }
          Err(e) => {
            tracing::warn!("Skipping corrupt entry during KV rebuild: {}", e);
          }
        }
      }
      collected
    };
    // Writer lock released here
    tracing::debug!(scanned_entries = entries.len(), "rebuild_kv: WAL scan complete");

    // Read layout info from the file header
    let (kv_block_offset, file_path, existing_stage) = {
      let writer = self.writer.read()
        .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let header = writer.file_header();
      (header.kv_block_offset, writer.file_path().to_path_buf(), header.kv_block_stage as usize)
    };

    let hash_length = hash_algo.hash_length();
    let psize = crate::engine::kv_pages::page_size(hash_length);

    // Determine KV block position, size, and stage.
    // hot_tail_offset = end of WAL = writer.current_offset()
    let wal_end = {
      let writer = self.writer.read()
        .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      writer.current_offset()
    };

    let (kv_offset, block_size, hot_offset, rebuild_stage) = if kv_block_offset > 0 {
      // Normal single-file layout: KV at head, hot tail after WAL
      let (bs, _) = crate::engine::kv_stages::stage_params(existing_stage, psize);
      (kv_block_offset, bs, wal_end, existing_stage)
    } else {
      // Legacy database (pre single-file refactor): no KV block on disk.
      // Place KV block at the end of the WAL, sized to fit all entries.
      let writer = self.writer.read()
        .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let wal_end = writer.current_offset();
      let target_stage = crate::engine::kv_pages::stage_for_count(entries.len(), hash_length);
      let (bs, _) = crate::engine::kv_stages::stage_params(target_stage, psize);
      tracing::info!("Legacy database: placing KV block at WAL end (offset {}), stage {} ({}B)", wal_end, target_stage, bs);
      (wal_end, bs, wal_end + bs, target_stage)
    };

    tracing::debug!(
      kv_offset, block_size, hot_offset, rebuild_stage, wal_end,
      kv_block_offset_from_header = kv_block_offset,
      "rebuild_kv: creating new KV store"
    );

    let kv_file = OpenOptions::new().read(true).write(true).open(&file_path)?;
    let mut new_kv = DiskKVStore::create(kv_file, hash_algo, kv_offset, hot_offset, rebuild_stage)?;

    // Insert all entries with auto-flush disabled. We want a single flush
    // at the end so that page writes don't clobber each other across
    // multiple auto-flush cycles (each auto-flush overwrites the same
    // bucket pages, potentially evicting entries from earlier flushes).
    let mut count = 0;
    let dir_type = crate::engine::kv_store::KV_TYPE_DIRECTORY;
    for (type_flags, hash, offset, value_length, total_length) in &entries {
      let kv_type = *type_flags & 0x0F;

      // For directory entries: if we already have a non-empty version,
      // don't overwrite it with an empty one. This prevents
      // ensure_root_directory's empty write from clobbering a valid
      // directory with children.
      if kv_type == dir_type && *value_length == 0 {
        if let Some(existing) = new_kv.get_buffered(hash) {
          if existing.offset != *offset {
            // Already have a (possibly non-empty) version — skip the empty one
            count += 1;
            continue;
          }
        }
      }

      let kv_entry = KVEntry {
        type_flags: *type_flags,
        hash: hash.clone(),
        offset: *offset,
        total_length: *total_length,
      };
      new_kv.buffer_only(kv_entry);
      count += 1;
    }

    tracing::debug!(
      inserted = count,
      write_buffer_len = new_kv.write_buffer_len(),
      "rebuild_kv: all entries inserted, flushing"
    );

    new_kv.flush()?;

    tracing::debug!(
      write_buffer_after_flush = new_kv.write_buffer_len(),
      "rebuild_kv: flush complete"
    );

    // Swap the KV writer. Clear the old KV's write buffer first so its
    // Drop impl doesn't flush stale data over the rebuilt pages.
    let mut kv_lock = self.kv_writer.lock()
      .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
    kv_lock.clear_write_buffer();
    *kv_lock = new_kv;

    // Update the shared snapshot handle to the new KV's snapshot.
    // Without this, readers (including rebuild_directory_tree) would
    // still see the old/empty snapshot.
    let new_snapshot = Arc::clone(&kv_lock.snapshot_handle().load());
    self.kv_snapshot.store(new_snapshot);

    // Update the file header with the current hot_tail_offset so the
    // hot tail entries (overflow from the KV page capacity) are found on reopen.
    {
      let mut writer = self.writer.write()
        .map_err(|e| EngineError::IoError(std::io::Error::other(e.to_string())))?;
      let mut header = writer.file_header().clone();
      let final_stage = kv_lock.stage();
      let (final_block_size, _) = crate::engine::kv_stages::stage_params(final_stage, psize);
      header.kv_block_offset = kv_offset;
      header.kv_block_length = final_block_size;
      // Hot tail goes after the WAL, not after the KV block
      header.hot_tail_offset = wal_end;
      header.entry_count = count as u64;
      header.kv_block_stage = final_stage as u8;
      tracing::debug!(
        kv_block_offset = header.kv_block_offset,
        kv_block_length = header.kv_block_length,
        hot_tail_offset = header.hot_tail_offset,
        kv_block_stage = header.kv_block_stage,
        entry_count = header.entry_count,
        "rebuild_kv: updating file header"
      );
      writer.update_header(&header)?;
    }

    let elapsed = timer.elapsed();
    tracing::info!("KV rebuild complete: {} entries indexed in {:.2}s", count, elapsed.as_secs_f64());

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
  pub fn end_transaction(&self) {
    if let Ok(mut kv) = self.kv_writer.lock() {
      kv.transaction_depth = kv.transaction_depth.saturating_sub(1);
      if kv.transaction_depth == 0 {
        if let Err(e) = kv.flush_hot_buffer() {
          tracing::warn!("Failed to flush hot buffer after transaction: {}", e);
        }
      }
    }
  }

  /// Try to flush the hot buffer if the KV lock is available.
  /// Used by the 100ms timer task — non-blocking, skips if writer is busy.
  pub fn try_flush_hot_buffer(&self) {
    // Sync WAL data to disk first — entries written since last sync are in the
    // OS page cache. This must happen BEFORE writing the hot tail, so that any
    // offsets referenced by the hot tail point to durable data.
    if let Ok(mut writer) = self.writer.try_write() {
      if let Err(e) = writer.sync() {
        tracing::warn!("Timer WAL sync failed: {}", e);
      }
    }

    // Extract KV state under kv lock, then drop it before acquiring writer.
    // This avoids holding kv_writer while acquiring writer (reverse of shutdown's order).
    let header_update = if let Ok(mut kv) = self.kv_writer.try_lock() {
      if kv.hot_buffer_len() > 0 || kv.write_buffer_len() > 0 {
        if let Err(e) = kv.flush_hot_buffer() {
          tracing::warn!("Timer flush failed: {}", e);
        }
        Some((kv.hot_tail_offset(), kv.len() as u64))
      } else {
        None
      }
    } else {
      None
    };

    // Now persist the header with writer lock (kv lock already released)
    if let Some((hot_tail_offset, entry_count)) = header_update {
      if let Ok(mut writer) = self.writer.try_write() {
        let mut header = writer.file_header().clone();
        header.hot_tail_offset = hot_tail_offset;
        header.entry_count = entry_count;
        if let Err(e) = writer.update_header(&header) {
          tracing::warn!("Timer header update failed: {}", e);
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
    tracing::info!("Shutting down storage engine...");

    // Step 1: Flush the KV write buffer to disk pages
    match self.kv_writer.lock() {
      Ok(mut kv) => {
        if let Err(e) = kv.flush() {
          tracing::error!("KV flush failed during shutdown: {}", e);
        }
        // Step 2: Flush the hot file buffer
        if let Err(e) = kv.flush_hot_buffer() {
          tracing::error!("Hot file flush failed during shutdown: {}", e);
        }
      }
      Err(e) => {
        tracing::error!("Could not acquire KV lock during shutdown: {}", e);
      }
    }

    // Step 3: Extract KV metadata, then persist header and sync WAL.
    // Extract values from kv_writer BEFORE acquiring writer to avoid
    // nesting kv_writer inside writer (opposite of the timer's order).
    let (hot_tail_offset, entry_count) = match self.kv_writer.lock() {
      Ok(kv) => (kv.hot_tail_offset(), kv.len() as u64),
      Err(e) => {
        tracing::error!("Could not acquire KV lock for header update during shutdown: {}", e);
        (0, 0)
      }
    };

    match self.writer.write() {
      Ok(mut writer) => {
        let mut header = writer.file_header().clone();
        header.hot_tail_offset = hot_tail_offset;
        header.entry_count = entry_count;
        if let Err(e) = writer.update_header(&header) {
          tracing::error!("Header update failed during shutdown: {}", e);
        }
        if let Err(e) = writer.sync_all() {
          tracing::error!("WAL sync failed during shutdown: {}", e);
        }
      }
      Err(e) => {
        tracing::error!("Could not acquire writer lock during shutdown: {}", e);
      }
    }

    tracing::info!("Storage engine shutdown complete");
    Ok(())
  }
}

impl Drop for StorageEngine {
  fn drop(&mut self) {
    let _ = self.shutdown();
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
    self.engine.end_transaction();
  }
}
