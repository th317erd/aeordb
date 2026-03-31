use std::path::Path;
use std::sync::RwLock;

use crate::engine::append_writer::AppendWriter;
use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_resize::KVResizeManager;
use crate::engine::kv_store::{
  KVEntry, KVStore,
  KV_TYPE_CHUNK, KV_TYPE_FILE_RECORD, KV_TYPE_DIRECTORY,
  KV_TYPE_DELETION, KV_TYPE_SNAPSHOT, KV_TYPE_VOID,
  KV_FLAG_DELETED,
};
use crate::engine::void_manager::VoidManager;

const DEFAULT_NVT_BUCKETS: usize = 1024;

/// Result type for entry retrieval: (header, key, value).
pub type EntryData = (EntryHeader, Vec<u8>, Vec<u8>);

/// Top-level storage engine combining append writer, KV index, and void manager.
///
/// Provides low-level entry storage and retrieval. Higher-level operations
/// (directory ops, file ops) are built on top via `DirectoryOps`.
pub struct StorageEngine {
  writer: RwLock<AppendWriter>,
  kv_manager: RwLock<KVResizeManager>,
  #[allow(dead_code)] // Used for future void reuse optimization
  void_manager: RwLock<VoidManager>,
  hash_algo: HashAlgorithm,
}

impl StorageEngine {
  /// Create a new database file at the given path.
  pub fn create(path: &str) -> EngineResult<Self> {
    let writer = AppendWriter::create(Path::new(path))?;
    let hash_algo = writer.file_header().hash_algo;
    let kv_store = KVStore::new(hash_algo, DEFAULT_NVT_BUCKETS);
    let kv_manager = KVResizeManager::new(kv_store);
    let void_manager = VoidManager::new(hash_algo);

    Ok(StorageEngine {
      writer: RwLock::new(writer),
      kv_manager: RwLock::new(kv_manager),
      void_manager: RwLock::new(void_manager),
      hash_algo,
    })
  }

  /// Open an existing database file, rebuilding the KV store from a file scan.
  pub fn open(path: &str) -> EngineResult<Self> {
    let writer = AppendWriter::open(Path::new(path))?;
    let hash_algo = writer.file_header().hash_algo;
    let mut kv_store = KVStore::new(hash_algo, DEFAULT_NVT_BUCKETS);
    let mut void_manager = VoidManager::new(hash_algo);

    let scanner = writer.scan_entries()?;
    for scanned_result in scanner {
      let scanned = scanned_result?;
      let kv_type = match scanned.header.entry_type {
        EntryType::Chunk => KV_TYPE_CHUNK,
        EntryType::FileRecord => KV_TYPE_FILE_RECORD,
        EntryType::DirectoryIndex => KV_TYPE_DIRECTORY,
        EntryType::DeletionRecord => KV_TYPE_DELETION,
        EntryType::Void => {
          void_manager.register_void(scanned.header.total_length, scanned.offset);
          KV_TYPE_VOID
        }
        _ => continue,
      };

      let entry = KVEntry {
        type_flags: kv_type,
        hash: scanned.key.clone(),
        offset: scanned.offset,
      };
      kv_store.insert(entry);
    }

    let kv_manager = KVResizeManager::new(kv_store);

    Ok(StorageEngine {
      writer: RwLock::new(writer),
      kv_manager: RwLock::new(kv_manager),
      void_manager: RwLock::new(void_manager),
      hash_algo,
    })
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
    };

    let kv_entry = KVEntry {
      type_flags: kv_type,
      hash: key.to_vec(),
      offset,
    };

    let mut kv = self.kv_manager.write()
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
      let kv = self.kv_manager.read()
        .map_err(|error| EngineError::IoError(
          std::io::Error::other(error.to_string()),
        ))?;
      match kv.get(hash) {
        Some(entry) => entry.clone(),
        None => return Ok(None),
      }
    };

    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let (header, key, value) = writer.read_entry_at(kv_entry.offset)?;

    Ok(Some((header, key, value)))
  }

  /// Check if an entry exists in the KV store.
  pub fn has_entry(&self, hash: &[u8]) -> EngineResult<bool> {
    let kv = self.kv_manager.read()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    Ok(kv.contains(hash))
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

    let mut kv = self.kv_manager.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    kv.insert(kv_entry);

    Ok(offset)
  }

  /// Check if a KV entry is marked as deleted.
  pub fn is_entry_deleted(&self, hash: &[u8]) -> EngineResult<bool> {
    let kv = self.kv_manager.read()
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
    let mut kv = self.kv_manager.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let updated = kv.primary_mut().update_flags(hash, KV_FLAG_DELETED);
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
      let kv = self.kv_manager.read()
        .map_err(|error| EngineError::IoError(
          std::io::Error::other(error.to_string()),
        ))?;
      kv.primary()
        .iter()
        .filter(|entry| entry.entry_type() == target_type)
        .map(|entry| (entry.hash.clone(), entry.offset))
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
}
