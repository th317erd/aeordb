use std::path::Path;
use std::sync::RwLock;

use crate::engine::append_writer::AppendWriter;
use crate::engine::compression::CompressionAlgorithm;
use crate::engine::disk_kv_store::DiskKVStore;
use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use serde::Serialize;

use crate::engine::kv_store::{
  KVEntry,
  KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY,
  KV_TYPE_DELETION, KV_TYPE_SNAPSHOT, KV_TYPE_VOID,
  KV_FLAG_DELETED, KV_TYPE_FORK,
};
use crate::engine::void_manager::VoidManager;

/// A buffered batch of entries to write in one sequential operation.
/// Accumulates entries in memory and flushes them all with a single
/// lock acquisition on both the writer and KV store.
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
        let kv_type = match entry_type {
            EntryType::Chunk => KV_TYPE_CHUNK,
            EntryType::FileRecord => KV_TYPE_FILE_RECORD,
            EntryType::DirectoryIndex => KV_TYPE_DIRECTORY,
            EntryType::DeletionRecord => KV_TYPE_DELETION,
            EntryType::Snapshot => KV_TYPE_SNAPSHOT,
            EntryType::Void => KV_TYPE_VOID,
            EntryType::Fork => KV_TYPE_FORK,
        };
        self.entries.push(BatchEntry {
            entry_type,
            key,
            value,
            kv_type,
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

#[derive(Debug, Clone, Serialize)]
pub struct DatabaseStats {
  pub entry_count: u64,
  pub kv_entries: usize,
  pub kv_size_bytes: u64,
  pub nvt_buckets: usize,
  pub nvt_size_bytes: u64,
  pub chunk_count: usize,
  pub file_count: usize,
  pub directory_count: usize,
  pub snapshot_count: usize,
  pub fork_count: usize,
  pub void_count: usize,
  pub void_space_bytes: u64,
  pub db_file_size_bytes: u64,
  pub created_at: i64,
  pub updated_at: i64,
  pub hash_algorithm: String,
}

/// Top-level storage engine combining append writer, KV index, and void manager.
///
/// Provides low-level entry storage and retrieval. Higher-level operations
/// (directory ops, file ops) are built on top via `DirectoryOps`.
pub struct StorageEngine {
  writer: RwLock<AppendWriter>,
  kv_store: RwLock<DiskKVStore>,
  #[allow(dead_code)] // Used for future void reuse optimization
  void_manager: RwLock<VoidManager>,
  hash_algo: HashAlgorithm,
}

impl StorageEngine {
  /// Create a new database file at the given path.
  pub fn create(path: &str) -> EngineResult<Self> {
    let writer = AppendWriter::create(Path::new(path))?;
    let hash_algo = writer.file_header().hash_algo;

    let kv_path = format!("{}.kv", path);
    // Remove stale KV file if it exists (e.g. from a previous failed create)
    let _ = std::fs::remove_file(&kv_path);
    let kv_store = DiskKVStore::create(Path::new(&kv_path), hash_algo)?;

    let void_manager = VoidManager::new(hash_algo);

    Ok(StorageEngine {
      writer: RwLock::new(writer),
      kv_store: RwLock::new(kv_store),
      void_manager: RwLock::new(void_manager),
      hash_algo,
    })
  }

  /// Internal open logic shared by `open` and `open_for_import`.
  fn open_internal(path: &str) -> EngineResult<Self> {
    let writer = AppendWriter::open(Path::new(path))?;
    let hash_algo = writer.file_header().hash_algo;

    let kv_path = format!("{}.kv", path);

    let mut void_manager = VoidManager::new(hash_algo);

    // Always rebuild the KV file from a scan of the main .aeordb file.
    // This ensures consistency even if the previous .kv file was stale
    // (e.g., unflushed write buffer from a concurrent or prior open).
    let _ = std::fs::remove_file(&kv_path);

    let kv_store = {
      let mut kv = DiskKVStore::create(Path::new(&kv_path), hash_algo)?;

      // First pass: rebuild KV store from entry headers, collecting deletion records.
      // Each deletion record stores (path, scan_offset) so we can avoid re-deleting
      // files that were recreated after the deletion.
      let mut deletion_records: Vec<(String, u64)> = Vec::new();
      let scanner = writer.scan_entries()?;
      for scanned_result in scanner {
        let scanned = scanned_result?;
        let kv_type = match scanned.header.entry_type {
          EntryType::Chunk => KV_TYPE_CHUNK,
          EntryType::FileRecord => KV_TYPE_FILE_RECORD,
          EntryType::DirectoryIndex => KV_TYPE_DIRECTORY,
          EntryType::DeletionRecord => {
            // Collect deletion record paths for the post-scan pass.
            if let Ok(record) = crate::engine::deletion_record::DeletionRecord::deserialize(&scanned.value) {
              deletion_records.push((record.path, scanned.offset));
            }
            KV_TYPE_DELETION
          }
          EntryType::Void => {
            void_manager.register_void(scanned.header.total_length, scanned.offset);
            KV_TYPE_VOID
          }
          EntryType::Snapshot => KV_TYPE_SNAPSHOT,
          EntryType::Fork => KV_TYPE_FORK,
        };

        let entry = KVEntry {
          type_flags: kv_type,
          hash: scanned.key.clone(),
          offset: scanned.offset,
        };
        kv.insert(entry);
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

    Ok(StorageEngine {
      writer: RwLock::new(writer),
      kv_store: RwLock::new(kv_store),
      void_manager: RwLock::new(void_manager),
      hash_algo,
    })
  }

  /// Open an existing database file, rebuilding the KV store from a file scan.
  /// Refuses to open patch databases (backup_type > 1).
  pub fn open(path: &str) -> EngineResult<Self> {
    let engine = Self::open_internal(path)?;

    // Guard: refuse to open patch databases as normal databases
    let header = engine.writer.read().expect("writer lock").file_header().clone();
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
    Self::open_internal(path)
  }

  /// Store an entry: append to file, register in KV store.
  /// Returns the file offset where the entry was written.
  pub fn store_entry(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
  ) -> EngineResult<u64> {
    let offset = {
      let mut writer = self.writer.write()
        .map_err(|error| EngineError::IoError(
          std::io::Error::other(error.to_string()),
        ))?;
      writer.append_entry(entry_type, key, value, 0)?
    };

    let kv_type = match entry_type {
      EntryType::Chunk => KV_TYPE_CHUNK,
      EntryType::FileRecord => KV_TYPE_FILE_RECORD,
      EntryType::DirectoryIndex => KV_TYPE_DIRECTORY,
      EntryType::DeletionRecord => KV_TYPE_DELETION,
      EntryType::Snapshot => KV_TYPE_SNAPSHOT,
      EntryType::Void => KV_TYPE_VOID,
      EntryType::Fork => KV_TYPE_FORK,
    };

    let kv_entry = KVEntry {
      type_flags: kv_type,
      hash: key.to_vec(),
      offset,
    };

    let mut kv = self.kv_store.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    kv.insert(kv_entry);

    Ok(offset)
  }

  /// Store an entry with compression: append to file, register in KV store.
  /// The hash is computed on the UNCOMPRESSED value (for dedup).
  /// The compressed value is what gets written to disk.
  /// Returns the file offset where the entry was written.
  pub fn store_entry_compressed(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<u64> {
    let offset = {
      let mut writer = self.writer.write()
        .map_err(|error| EngineError::IoError(
          std::io::Error::other(error.to_string()),
        ))?;
      writer.append_entry_with_compression(
        entry_type,
        key,
        value,
        0,
        compression_algo,
      )?
    };

    let kv_type = match entry_type {
      EntryType::Chunk => KV_TYPE_CHUNK,
      EntryType::FileRecord => KV_TYPE_FILE_RECORD,
      EntryType::DirectoryIndex => KV_TYPE_DIRECTORY,
      EntryType::DeletionRecord => KV_TYPE_DELETION,
      EntryType::Snapshot => KV_TYPE_SNAPSHOT,
      EntryType::Void => KV_TYPE_VOID,
      EntryType::Fork => KV_TYPE_FORK,
    };

    let kv_entry = KVEntry {
      type_flags: kv_type,
      hash: key.to_vec(),
      offset,
    };

    let mut kv = self.kv_store.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    kv.insert(kv_entry);

    Ok(offset)
  }

  /// Retrieve an entry by its hash key.
  /// Returns (header, key, value) if found.
  pub fn get_entry(
    &self,
    hash: &[u8],
  ) -> EngineResult<Option<EntryData>> {
    let kv_entry = {
      let mut kv = self.kv_store.write()
        .map_err(|error| EngineError::IoError(
          std::io::Error::other(error.to_string()),
        ))?;
      match kv.get(hash) {
        Some(entry) if !entry.is_deleted() => entry,
        _ => return Ok(None),
      }
    };

    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let (header, key, value) = writer.read_entry_at(kv_entry.offset)?;

    Ok(Some((header, key, value)))
  }

  /// Check if a non-deleted entry exists in the KV store.
  pub fn has_entry(&self, hash: &[u8]) -> EngineResult<bool> {
    let mut kv = self.kv_store.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    match kv.get(hash) {
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

  /// Update the HEAD hash in the file header.
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

  /// Read the current HEAD hash from the file header.
  pub fn head_hash(&self) -> EngineResult<Vec<u8>> {
    let writer = self.writer.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    Ok(writer.file_header().head_hash.clone())
  }

  /// Get the backup metadata from the file header.
  pub fn backup_info(&self) -> (u8, Vec<u8>, Vec<u8>) {
    let writer = self.writer.read().expect("writer lock");
    let fh = writer.file_header();
    (fh.backup_type, fh.base_hash.clone(), fh.target_hash.clone())
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
  pub fn store_entry_typed(
    &self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    kv_type: u8,
  ) -> EngineResult<u64> {
    let offset = {
      let mut writer = self.writer.write()
        .map_err(|error| EngineError::IoError(
          std::io::Error::other(error.to_string()),
        ))?;
      writer.append_entry(entry_type, key, value, 0)?
    };

    let kv_entry = KVEntry {
      type_flags: kv_type,
      hash: key.to_vec(),
      offset,
    };

    let mut kv = self.kv_store.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    kv.insert(kv_entry);

    Ok(offset)
  }

  /// Write all entries in a batch with a single lock acquisition.
  /// Each entry is appended sequentially, then all are registered in the KV store.
  /// Returns the file offsets where entries were written.
  pub fn flush_batch(&self, batch: WriteBatch) -> EngineResult<Vec<u64>> {
    if batch.is_empty() {
      return Ok(Vec::new());
    }

    let mut offsets = Vec::with_capacity(batch.entries.len());

    // Single write lock acquisition for all entries
    {
      let mut writer = self.writer.write()
        .map_err(|error| EngineError::IoError(
          std::io::Error::other(error.to_string()),
        ))?;

      for entry in &batch.entries {
        let offset = writer.append_entry(
          entry.entry_type,
          &entry.key,
          &entry.value,
          0, // flags
        )?;
        offsets.push(offset);
      }
    }

    // Single KV lock acquisition for all KV inserts
    {
      let mut kv = self.kv_store.write()
        .map_err(|error| EngineError::IoError(
          std::io::Error::other(error.to_string()),
        ))?;

      for (i, entry) in batch.entries.iter().enumerate() {
        let kv_entry = KVEntry {
          type_flags: entry.kv_type,
          hash: entry.key.clone(),
          offset: offsets[i],
        };
        kv.insert(kv_entry);
      }
    }

    Ok(offsets)
  }

  /// Check if a KV entry is marked as deleted.
  pub fn is_entry_deleted(&self, hash: &[u8]) -> EngineResult<bool> {
    let mut kv = self.kv_store.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    match kv.get(hash) {
      Some(entry) => Ok(entry.is_deleted()),
      None => Ok(false),
    }
  }

  /// Mark a KV entry as deleted by setting the deleted flag.
  pub fn mark_entry_deleted(&self, hash: &[u8]) -> EngineResult<()> {
    let mut kv = self.kv_store.write()
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

  /// Return all (key_hash, value) pairs for entries matching a KV type.
  /// Reads each entry's value from disk. Includes deleted entries in the
  /// result — callers should check `is_entry_deleted` if needed.
  pub fn entries_by_type(&self, target_type: u8) -> EngineResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let hashes: Vec<(Vec<u8>, u64)> = {
      let mut kv = self.kv_store.write()
        .map_err(|error| EngineError::IoError(
          std::io::Error::other(error.to_string()),
        ))?;
      kv.iter_all()?
        .into_iter()
        .filter(|entry| entry.entry_type() == target_type)
        .map(|entry| (entry.hash, entry.offset))
        .collect()
    };

    let mut results = Vec::with_capacity(hashes.len());
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;

    for (hash, offset) in hashes {
      let (_header, _key, value) = writer.read_entry_at(offset)?;
      results.push((hash, value));
    }

    Ok(results)
  }

  /// Return aggregate statistics about the database.
  pub fn stats(&self) -> DatabaseStats {
    // 1. Lock writer for file header info and file size
    let (entry_count, created_at, updated_at, db_file_size_bytes) = {
      let writer = self.writer.read().expect("writer lock poisoned");
      let fh = writer.file_header();
      (
        fh.entry_count,
        fh.created_at,
        fh.updated_at,
        writer.file_size(),
      )
    };

    // 2. Lock kv_store for entry counts
    let (kv_entries, kv_size_bytes, nvt_buckets, chunk_count, file_count, directory_count, snapshot_count, fork_count) = {
      let mut kv = self.kv_store.write().expect("kv_store lock poisoned");
      let kv_entries = kv.len();
      let nvt_buckets = kv.bucket_count();

      // Get the KV file size for stats
      let kv_size_bytes = std::fs::metadata(kv.path())
        .map(|m| m.len())
        .unwrap_or(0);

      let all_entries = kv.iter_all().unwrap_or_default();

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

      (kv_entries, kv_size_bytes, nvt_buckets, chunk_count, file_count, directory_count, snapshot_count, fork_count)
    };

    // 3. Lock void_manager for void stats
    let (void_count, void_space_bytes) = {
      let vm = self.void_manager.read().expect("void_manager lock poisoned");
      (vm.void_count(), vm.total_void_space())
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
}
