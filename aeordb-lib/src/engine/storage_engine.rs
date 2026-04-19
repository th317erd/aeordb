use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use arc_swap::ArcSwap;

use crate::engine::append_writer::AppendWriter;
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
  /// Path to the `.kv` sidecar file, stored at construction time so that
  /// `stats()` can read its metadata without locking `kv_writer`.
  /// (Used in stats() — dead_code analysis misses it through RwLock indirection.)
  #[allow(dead_code)]
  kv_path: String,
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
}

impl StorageEngine {
  /// Create a new database file at the given path.
  ///
  /// Does not use a hot directory for crash recovery. Suitable for tests and
  /// CLI tools; production servers should use [`create_with_hot_dir`](Self::create_with_hot_dir).
  pub fn create(path: &str) -> EngineResult<Self> {
    Self::create_with_hot_dir(path, None)
  }

  /// Create a new database file at the given path with an optional hot directory
  /// for crash-recovery write-ahead logging.
  pub fn create_with_hot_dir(path: &str, hot_dir: Option<&Path>) -> EngineResult<Self> {
    let writer = AppendWriter::create(Path::new(path))?;
    let hash_algo = writer.file_header().hash_algo;

    let kv_path = format!("{}.kv", path);
    // Remove stale KV file if it exists (e.g. from a previous failed create)
    let _ = std::fs::remove_file(&kv_path);
    let kv_store = DiskKVStore::create(Path::new(&kv_path), hash_algo, hot_dir)?;

    let kv_snapshot = Arc::clone(kv_store.snapshot_handle());

    let void_manager = VoidManager::new(hash_algo);

    let engine = StorageEngine {
      writer: RwLock::new(writer),
      kv_writer: Mutex::new(kv_store),
      kv_snapshot,
      kv_path,
      void_manager: RwLock::new(void_manager),
      hash_algo,
      counters: ArcSwap::from_pointee(EngineCounters::new()),
    };
    let initialized = Arc::new(EngineCounters::initialize_from_kv(&engine));
    engine.counters.store(initialized);
    Ok(engine)
  }

  /// Internal open logic shared by `open` and `open_for_import`.
  ///
  /// If the `.kv` file already exists on disk, it is opened directly — no
  /// full entry scan is needed for the KV index. We still scan for void
  /// entries (they're tracked in-memory, not persisted in the KV file).
  ///
  /// If the `.kv` file does NOT exist, we create it and populate it from a
  /// full scan of the main `.aeordb` file, including deletion replay.
  ///
  /// When `hot_dir` is Some, hot files are replayed into the KV store on open,
  /// then deleted. A new hot file is initialized for ongoing crash recovery.
  fn open_internal(path: &str, hot_dir: Option<&Path>) -> EngineResult<Self> {
    let writer = AppendWriter::open(Path::new(path))?;
    let hash_algo = writer.file_header().hash_algo;

    let kv_path = format!("{}.kv", path);

    let mut void_manager = VoidManager::new(hash_algo);

    // Decide whether to reuse the existing .kv file or rebuild from scan.
    // The .kv is valid if:
    //   1. It exists and has a valid file size (multiple of page_size)
    //   2. It can be opened without errors
    //   3. It's not stale — if the .aeordb has data, the .kv must have entries
    let aeordb_has_entries = writer.file_size() > crate::engine::file_header::FILE_HEADER_SIZE as u64;
    let hash_length = hash_algo.hash_length();
    let min_kv_size = crate::engine::kv_pages::KV_STAGES[0].1 as u64
      * crate::engine::kv_pages::page_size(hash_length) as u64;

    let kv_is_valid = if Path::new(&kv_path).exists() {
      // Check file size first — a valid .kv is at least stage-0 size
      let kv_file_size = std::fs::metadata(&kv_path)
        .map(|m| m.len())
        .unwrap_or(0);

      if kv_file_size < min_kv_size {
        // Too small to be a valid .kv — remove and rebuild
        let _ = std::fs::remove_file(&kv_path);
        false
      } else {
        match DiskKVStore::open(Path::new(&kv_path), hash_algo, None) {
          Ok(kv) => {
            if aeordb_has_entries && kv.len() == 0 {
              // Stale .kv — remove and rebuild
              drop(kv);
              let _ = std::fs::remove_file(&kv_path);
              false
            } else {
              // Valid — close and re-open below
              drop(kv);
              true
            }
          }
          Err(_) => {
            // Corrupt .kv — remove and rebuild
            let _ = std::fs::remove_file(&kv_path);
            false
          }
        }
      }
    } else {
      false
    };

    // Collect hot file entries to replay (before opening the KV store).
    let mut hot_entries_to_replay: Vec<KVEntry> = Vec::new();
    if let Some(hdir) = hot_dir {
      let db_name = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("db");
      let hot_pattern = format!("{}-hot", db_name);
      if let Ok(dir_entries) = std::fs::read_dir(hdir) {
        for dir_entry in dir_entries.flatten() {
          let name = dir_entry.file_name();
          let name_str = name.to_string_lossy();
          if name_str.starts_with(&hot_pattern) {
            let hash_length = hash_algo.hash_length();
            if let Ok(hot_entries) = DiskKVStore::read_hot_file(&dir_entry.path(), hash_length) {
              if !hot_entries.is_empty() {
                hot_entries_to_replay.extend(hot_entries);
              }
            }
            // Delete the hot file after reading
            let _ = std::fs::remove_file(dir_entry.path());
          }
        }
      }
    }

    let kv_store = if kv_is_valid {
      // KV file is valid — open it directly (skip full KV rebuild).
      let kv = DiskKVStore::open(Path::new(&kv_path), hash_algo, hot_dir)?;

      // Still scan for void entries (they're an in-memory optimization,
      // not persisted in the KV file).
      let scanner = writer.scan_entries()?;
      for scanned_result in scanner {
        let scanned = scanned_result?;
        if scanned.header.entry_type == EntryType::Void {
          void_manager.register_void(scanned.header.total_length, scanned.offset);
        }
      }

      kv
    } else {
      // No KV file — create and populate from a full entry scan.
      let mut kv = DiskKVStore::create(Path::new(&kv_path), hash_algo, hot_dir)?;

      // First pass: rebuild KV store from entry headers, collecting deletion records.
      // Each deletion record stores (path, scan_offset) so we can avoid re-deleting
      // files that were recreated after the deletion.
      let mut deletion_records: Vec<(String, u64)> = Vec::new();
      let scanner = writer.scan_entries()?;
      for scanned_result in scanner {
        let scanned = scanned_result?;
        // Collect deletion records and register void entries during the scan.
        if scanned.header.entry_type == EntryType::DeletionRecord {
          if let Ok(record) = crate::engine::deletion_record::DeletionRecord::deserialize(&scanned.value) {
            deletion_records.push((record.path, scanned.offset));
          }
        } else if scanned.header.entry_type == EntryType::Void {
          void_manager.register_void(scanned.header.total_length, scanned.offset);
        }
        let kv_type = scanned.header.entry_type.to_kv_type();

        let entry = KVEntry {
          type_flags: kv_type,
          hash: scanned.key.clone(),
          offset: scanned.offset,
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

    // H13: Warn if the KV store looks significantly stale compared to the
    // .aeordb file header's entry count. This can happen if the .kv file was
    // copied from an older backup or the process crashed after writing entries
    // but before flushing the KV. The hot file mechanism should recover recent
    // writes, but a large gap suggests the .kv file should be deleted to force
    // a full rebuild.
    if aeordb_has_entries {
      let file_entry_count = writer.file_header().entry_count;
      if file_entry_count > 100 && kv_store.len() < (file_entry_count as usize / 4) {
        tracing::warn!(
          "KV store may be stale: {} entries vs ~{} in .aeordb file header. \
           The hot file should recover recent writes, but consider deleting \
           the .kv file to force a full rebuild if data appears missing.",
          kv_store.len(), file_entry_count
        );
      }
    }

    // Replay hot entries into the KV store, then flush
    let mut kv_store = kv_store;
    if !hot_entries_to_replay.is_empty() {
      for entry in hot_entries_to_replay {
        kv_store.insert(entry)?;
      }
      kv_store.flush()?;
    }

    let kv_snapshot = Arc::clone(kv_store.snapshot_handle());

    let engine = StorageEngine {
      writer: RwLock::new(writer),
      kv_writer: Mutex::new(kv_store),
      kv_snapshot,
      kv_path,
      void_manager: RwLock::new(void_manager),
      hash_algo,
      counters: ArcSwap::from_pointee(EngineCounters::new()),
    };
    let initialized = Arc::new(EngineCounters::initialize_from_kv(&engine));
    engine.counters.store(initialized);
    Ok(engine)
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
    // Acquire BOTH locks before any work to close the TOCTOU gap.
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;

    let offset = writer.append_entry(entry_type, key, value, 0)?;

    let kv_entry = KVEntry {
      type_flags: entry_type.to_kv_type(),
      hash: key.to_vec(),
      offset,
    };
    kv.insert(kv_entry)?;
    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    Ok(offset)
  }

  /// Store an entry with custom flags: append to file, register in KV store.
  /// Returns the file offset where the entry was written.
  ///
  /// Both the writer and KV locks are held simultaneously to prevent a
  /// TOCTOU gap where a crash between the disk write and the KV insert
  /// could leave the entry on disk but missing from the index.
  /// Lock order: writer first, then KV (must be consistent everywhere).
  pub fn store_entry_with_flags(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    flags: u8,
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

    let offset = writer.append_entry_with_compression(entry_type, key, value, flags, CompressionAlgorithm::None)?;

    let kv_entry = KVEntry {
      type_flags: entry_type.to_kv_type(),
      hash: key.to_vec(),
      offset,
    };
    kv.insert(kv_entry)?;
    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    Ok(offset)
  }

  /// Store an entry with compression: append to file, register in KV store.
  /// The hash is computed on the UNCOMPRESSED value (for dedup).
  /// The compressed value is what gets written to disk.
  /// Returns the file offset where the entry was written.
  ///
  /// Both the writer and KV locks are held simultaneously to prevent a
  /// TOCTOU gap where a crash between the disk write and the KV insert
  /// could leave the entry on disk but missing from the index.
  /// Lock order: writer first, then KV (must be consistent everywhere).
  pub fn store_entry_compressed(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    compression_algo: CompressionAlgorithm,
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

    let offset = writer.append_entry_with_compression(
      entry_type,
      key,
      value,
      0,
      compression_algo,
    )?;

    let kv_entry = KVEntry {
      type_flags: entry_type.to_kv_type(),
      hash: key.to_vec(),
      offset,
    };
    kv.insert(kv_entry)?;
    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    Ok(offset)
  }

  /// Store an entry with both compression and custom flags.
  /// The hash is computed on the UNCOMPRESSED value (for dedup).
  /// The compressed value is what gets written to disk.
  pub fn store_entry_compressed_with_flags(
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

    let offset = writer.append_entry_with_compression(
      entry_type,
      key,
      value,
      flags,
      compression_algo,
    )?;

    let kv_entry = KVEntry {
      type_flags: entry_type.to_kv_type(),
      hash: key.to_vec(),
      offset,
    };
    kv.insert(kv_entry)?;
    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    Ok(offset)
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
    let (header, key, value) = writer.read_entry_at_shared(kv_entry.offset)?;

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

    let offset = writer.append_entry(entry_type, key, value, 0)?;

    let kv_entry = KVEntry {
      type_flags: kv_type,
      hash: key.to_vec(),
      offset,
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

    for entry in &batch.entries {
      let offset = writer.append_entry(
        entry.entry_type,
        &entry.key,
        &entry.value,
        0, // flags
      )?;
      offsets.push(offset);
    }

    for (i, entry) in batch.entries.iter().enumerate() {
      let kv_entry = KVEntry {
        type_flags: entry.kv_type,
        hash: entry.key.clone(),
        offset: offsets[i],
      };
      kv.insert(kv_entry)?;
    }
    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    Ok(offsets)
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
    vm.register_void(size, offset);

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
    vm.register_void(size, offset);

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
  /// Reads each entry's value from disk. Includes deleted entries in the
  /// result — callers should check `is_entry_deleted` if needed.
  pub fn entries_by_type(&self, target_type: u8) -> EngineResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let hashes: Vec<(Vec<u8>, u64)> = {
      let snapshot = self.kv_snapshot.load();
      snapshot.iter_all()?
        .into_iter()
        .filter(|entry| entry.entry_type() == target_type)
        .map(|entry| (entry.hash, entry.offset))
        .collect()
    };

    let mut results = Vec::with_capacity(hashes.len());
    // Use a READ lock — read_entry_at_shared uses a cloned file handle.
    let writer = self.writer.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;

    for (hash, offset) in hashes {
      let (_header, _key, value) = writer.read_entry_at_shared(offset)?;
      results.push((hash, value));
    }

    Ok(results)
  }

  /// Return aggregate statistics about the database including entry counts
  /// by type, file sizes, void space, and timestamps.
  pub fn stats(&self) -> DatabaseStats {
    // 1. Lock writer for file header info and file size
    let (entry_count, created_at, updated_at, db_file_size_bytes) = match self.writer.read() {
      Ok(writer) => {
        let fh = writer.file_header();
        (fh.entry_count, fh.created_at, fh.updated_at, writer.file_size())
      }
      Err(e) => {
        tracing::error!("writer lock poisoned in stats(): {}", e);
        (0, 0, 0, 0)
      }
    };

    // 2. Use snapshot for entry counts (lock-free)
    let snapshot = self.kv_snapshot.load();
    let kv_entries = snapshot.len();
    let nvt_buckets = snapshot.bucket_count();

    // L9: Use stored kv_path instead of locking kv_writer just to get the path.
    let kv_size_bytes = std::fs::metadata(&self.kv_path)
      .map(|m| m.len())
      .unwrap_or(0);

    // PERF(M4): This iter_all() + type counting is O(n) over all KV entries.
    // For databases with millions of entries, consider maintaining atomic counters
    // per entry type (updated on insert/delete) or caching this result with a
    // short TTL (e.g., 1 second). Currently acceptable because the snapshot is
    // fully in-memory and n is bounded by the number of stored objects.
    let all_entries = snapshot.iter_all().unwrap_or_default();

    let mut chunk_count = 0usize;
    let mut file_count = 0usize;
    let mut directory_count = 0usize;
    let mut snapshot_count = 0usize;
    let mut fork_count = 0usize;

    for entry in &all_entries {
      match entry.entry_type() {
        KV_TYPE_CHUNK => chunk_count += 1,
        KV_TYPE_FILE_RECORD => file_count += 1,
        KV_TYPE_DIRECTORY => directory_count += 1,
        KV_TYPE_SNAPSHOT => snapshot_count += 1,
        KV_TYPE_FORK => fork_count += 1,
        _ => {}
      }
    }

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

    // Step 3: Sync the WAL file
    match self.writer.write() {
      Ok(mut writer) => {
        if let Err(e) = writer.sync() {
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
