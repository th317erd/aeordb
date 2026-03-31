use std::path::Path;
use std::sync::RwLock;

use crate::engine::append_writer::AppendWriter;
use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_resize::KVResizeManager;
use crate::engine::kv_store::{KVEntry, KVStore, KV_TYPE_CHUNK, KV_FLAG_DELETED};
use crate::engine::void_manager::VoidManager;
use crate::storage::chunk::{Chunk, ChunkHash};
use crate::storage::chunk_header::ChunkHeader;
use crate::storage::chunk_storage::{ChunkStorage, ChunkStoreError};

const DEFAULT_NVT_BUCKETS: usize = 1024;

/// Implementation of the ChunkStorage trait backed by the custom append-only engine.
///
/// The on-disk entry key is the raw ChunkHash (BLAKE3(data)), which is also what
/// the ChunkStorage trait uses for lookups. The entry_type field distinguishes
/// chunks from other entity types in the unified KV store.
///
/// The value stored on disk is: ChunkHeader (33 bytes) + raw chunk data.
pub struct EngineChunkStorage {
  writer: RwLock<AppendWriter>,
  kv_manager: RwLock<KVResizeManager>,
  void_manager: RwLock<VoidManager>,
  hash_algo: HashAlgorithm,
}

impl EngineChunkStorage {
  /// Create a new database file at the given path.
  pub fn create(path: &str) -> Result<Self, ChunkStoreError> {
    let writer = AppendWriter::create(Path::new(path))
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let hash_algo = writer.file_header().hash_algo;
    let kv_store = KVStore::new(hash_algo, DEFAULT_NVT_BUCKETS);
    let kv_manager = KVResizeManager::new(kv_store);
    let void_manager = VoidManager::new(hash_algo);

    Ok(EngineChunkStorage {
      writer: RwLock::new(writer),
      kv_manager: RwLock::new(kv_manager),
      void_manager: RwLock::new(void_manager),
      hash_algo,
    })
  }

  /// Open an existing database file, rebuilding the KV store from a file scan.
  pub fn open(path: &str) -> Result<Self, ChunkStoreError> {
    let writer = AppendWriter::open(Path::new(path))
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let hash_algo = writer.file_header().hash_algo;
    let mut kv_store = KVStore::new(hash_algo, DEFAULT_NVT_BUCKETS);
    let mut void_manager = VoidManager::new(hash_algo);

    // Rebuild KV store by scanning all entries in the file
    let scanner = writer.scan_entries()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    for scanned_result in scanner {
      let scanned = scanned_result
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

      match scanned.header.entry_type {
        EntryType::Chunk => {
          // The on-disk key IS the raw ChunkHash
          let entry = KVEntry {
            type_flags: KV_TYPE_CHUNK,
            hash: scanned.key.clone(),
            offset: scanned.offset,
          };
          kv_store.insert(entry);
        }
        EntryType::Void => {
          void_manager.register_void(
            scanned.header.total_length,
            scanned.offset,
          );
        }
        _ => {
          // Other entry types not relevant for ChunkStorage
        }
      }
    }

    let kv_manager = KVResizeManager::new(kv_store);

    Ok(EngineChunkStorage {
      writer: RwLock::new(writer),
      kv_manager: RwLock::new(kv_manager),
      void_manager: RwLock::new(void_manager),
      hash_algo,
    })
  }

  /// Serialize chunk header + data into the on-disk value blob.
  fn serialize_chunk_value(chunk: &Chunk) -> Vec<u8> {
    let header_bytes = chunk.header.serialize();
    let mut value = Vec::with_capacity(header_bytes.len() + chunk.data.len());
    value.extend_from_slice(&header_bytes);
    value.extend_from_slice(&chunk.data);
    value
  }
}

impl ChunkStorage for EngineChunkStorage {
  fn store_chunk(&self, chunk: &Chunk) -> Result<(), ChunkStoreError> {
    let chunk_hash = chunk.hash.to_vec();

    // Dedup: if this hash already exists and is not deleted, skip
    {
      let kv = self.kv_manager.read()
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;
      if let Some(existing) = kv.get(&chunk_hash) {
        if !existing.is_deleted() {
          return Ok(());
        }
      }
    }

    let value = Self::serialize_chunk_value(chunk);
    let entry_total_length = EntryHeader::compute_total_length(
      self.hash_algo,
      chunk_hash.len() as u32,
      value.len() as u32,
    );

    // Try to reuse a void
    {
      let mut vm = self.void_manager.write()
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;
      vm.find_void(entry_total_length);
      // Note: void reuse (seek-write at arbitrary offset) is a future optimization.
      // For now we always append. The void tracking is in place for when we add
      // seek-write support to AppendWriter.
    }

    // Append to end of file
    let offset = {
      let mut writer = self.writer.write()
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;
      writer.append_entry(EntryType::Chunk, &chunk_hash, &value, 0)
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?
    };

    // Register in KV store
    let kv_entry = KVEntry {
      type_flags: KV_TYPE_CHUNK,
      hash: chunk_hash,
      offset,
    };

    let mut kv = self.kv_manager.write()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;
    kv.insert(kv_entry);

    Ok(())
  }

  fn get_chunk(&self, hash: &ChunkHash) -> Result<Option<Chunk>, ChunkStoreError> {
    let kv_entry = {
      let kv = self.kv_manager.read()
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

      match kv.get(hash.as_slice()) {
        Some(entry) if !entry.is_deleted() => entry.clone(),
        _ => return Ok(None),
      }
    };

    // Read the entry from disk at the stored offset
    let (header, _key, value) = {
      let mut writer = self.writer.write()
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;
      writer.read_entry_at(kv_entry.offset)
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?
    };

    // Verify on-disk entry integrity
    if !header.verify(&_key, &value) {
      return Err(ChunkStoreError::IntegrityError {
        expected: *hash,
        actual: [0u8; 32],
      });
    }

    // Deserialize value: ChunkHeader (33 bytes) + raw data
    if value.len() < crate::storage::chunk_header::HEADER_SIZE {
      return Err(ChunkStoreError::SerializationError(
        "Stored chunk value too short for chunk header".to_string(),
      ));
    }

    let chunk_header = ChunkHeader::deserialize_from_slice(&value)
      .map_err(|error| ChunkStoreError::SerializationError(error.to_string()))?;

    let data = value[crate::storage::chunk_header::HEADER_SIZE..].to_vec();

    let chunk = Chunk {
      hash: *hash,
      data,
      header: chunk_header,
    };

    // Verify chunk data integrity (BLAKE3(data) == hash)
    if !chunk.verify() {
      return Err(ChunkStoreError::IntegrityError {
        expected: *hash,
        actual: crate::storage::chunk::hash_data(&chunk.data),
      });
    }

    Ok(Some(chunk))
  }

  fn has_chunk(&self, hash: &ChunkHash) -> Result<bool, ChunkStoreError> {
    let kv = self.kv_manager.read()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    match kv.get(hash.as_slice()) {
      Some(entry) => Ok(!entry.is_deleted()),
      None => Ok(false),
    }
  }

  fn remove_chunk(&self, hash: &ChunkHash) -> Result<bool, ChunkStoreError> {
    let mut kv = self.kv_manager.write()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let exists = match kv.get(hash.as_slice()) {
      Some(entry) => !entry.is_deleted(),
      None => false,
    };

    if !exists {
      return Ok(false);
    }

    // Logical delete via flag — void management handles the freed space
    kv.primary_mut().update_flags(hash.as_slice(), KV_FLAG_DELETED);

    Ok(true)
  }

  fn chunk_count(&self) -> Result<u64, ChunkStoreError> {
    let kv = self.kv_manager.read()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let count = kv.primary().iter()
      .filter(|entry| entry.entry_type() == KV_TYPE_CHUNK && !entry.is_deleted())
      .count();

    Ok(count as u64)
  }

  fn list_chunk_hashes(&self) -> Result<Vec<ChunkHash>, ChunkStoreError> {
    let kv = self.kv_manager.read()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let hashes: Vec<ChunkHash> = kv.primary().iter()
      .filter(|entry| entry.entry_type() == KV_TYPE_CHUNK && !entry.is_deleted())
      .filter_map(|entry| {
        if entry.hash.len() == 32 {
          let mut hash = [0u8; 32];
          hash.copy_from_slice(&entry.hash);
          Some(hash)
        } else {
          None
        }
      })
      .collect();

    Ok(hashes)
  }
}
