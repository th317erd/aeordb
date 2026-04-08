use std::path::Path;
use std::sync::RwLock;

use crate::engine::append_writer::AppendWriter;
use crate::engine::disk_kv_store::DiskKVStore;
use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_store::{KVEntry, KV_TYPE_CHUNK, KV_FLAG_DELETED};
use crate::engine::void_manager::VoidManager;
use crate::storage::chunk::{Chunk, ChunkHash};
use crate::storage::chunk_header::ChunkHeader;
use crate::storage::chunk_storage::{ChunkStorage, ChunkStoreError};

/// Implementation of the ChunkStorage trait backed by the custom append-only engine.
///
/// The on-disk entry key is the raw ChunkHash (BLAKE3(data)), which is also what
/// the ChunkStorage trait uses for lookups. The entry_type field distinguishes
/// chunks from other entity types in the unified KV store.
///
/// The value stored on disk is: ChunkHeader (33 bytes) + raw chunk data.
pub struct EngineChunkStorage {
  writer: RwLock<AppendWriter>,
  kv_store: RwLock<DiskKVStore>,
  void_manager: RwLock<VoidManager>,
  hash_algo: HashAlgorithm,
}

impl EngineChunkStorage {
  /// Create a new database file at the given path.
  pub fn create(path: &str) -> Result<Self, ChunkStoreError> {
    let writer = AppendWriter::create(Path::new(path))
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let hash_algo = writer.file_header().hash_algo;

    let kv_path = format!("{}.kv", path);
    // Remove stale KV file if it exists
    let _ = std::fs::remove_file(&kv_path);
    let kv_store = DiskKVStore::create(Path::new(&kv_path), hash_algo)
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let void_manager = VoidManager::new(hash_algo);

    Ok(EngineChunkStorage {
      writer: RwLock::new(writer),
      kv_store: RwLock::new(kv_store),
      void_manager: RwLock::new(void_manager),
      hash_algo,
    })
  }

  /// Open an existing database file, rebuilding the KV store from a file scan.
  pub fn open(path: &str) -> Result<Self, ChunkStoreError> {
    let writer = AppendWriter::open(Path::new(path))
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let hash_algo = writer.file_header().hash_algo;
    let mut void_manager = VoidManager::new(hash_algo);

    let kv_path = format!("{}.kv", path);

    // Always rebuild from scan to ensure consistency
    let _ = std::fs::remove_file(&kv_path);

    let kv_store = {
      let mut kv = DiskKVStore::create(Path::new(&kv_path), hash_algo)
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

      // Rebuild KV store by scanning all entries in the file
      let scanner = writer.scan_entries()
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

      for scanned_result in scanner {
        let scanned = scanned_result
          .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

        match scanned.header.entry_type {
          EntryType::Chunk => {
            let entry = KVEntry {
              type_flags: KV_TYPE_CHUNK,
              hash: scanned.key.clone(),
              offset: scanned.offset,
            };
            kv.insert(entry);
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

      kv.flush()
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

      kv
    };

    Ok(EngineChunkStorage {
      writer: RwLock::new(writer),
      kv_store: RwLock::new(kv_store),
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
      let mut kv = self.kv_store.write()
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

    let mut kv = self.kv_store.write()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;
    kv.insert(kv_entry);

    Ok(())
  }

  fn get_chunk(&self, hash: &ChunkHash) -> Result<Option<Chunk>, ChunkStoreError> {
    let kv_entry = {
      let mut kv = self.kv_store.write()
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

      match kv.get(hash.as_slice()) {
        Some(entry) if !entry.is_deleted() => entry,
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
    let mut kv = self.kv_store.write()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    match kv.get(hash.as_slice()) {
      Some(entry) => Ok(!entry.is_deleted()),
      None => Ok(false),
    }
  }

  fn remove_chunk(&self, hash: &ChunkHash) -> Result<bool, ChunkStoreError> {
    let mut kv = self.kv_store.write()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let exists = match kv.get(hash.as_slice()) {
      Some(entry) => !entry.is_deleted(),
      None => false,
    };

    if !exists {
      return Ok(false);
    }

    // Logical delete via flag — void management handles the freed space
    kv.update_flags(hash.as_slice(), KV_FLAG_DELETED);

    Ok(true)
  }

  fn chunk_count(&self) -> Result<u64, ChunkStoreError> {
    let mut kv = self.kv_store.write()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let all_entries = kv.iter_all()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let count = all_entries.iter()
      .filter(|entry| entry.entry_type() == KV_TYPE_CHUNK)
      .count();

    Ok(count as u64)
  }

  fn list_chunk_hashes(&self) -> Result<Vec<ChunkHash>, ChunkStoreError> {
    let mut kv = self.kv_store.write()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let all_entries = kv.iter_all()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;

    let hashes: Vec<ChunkHash> = all_entries.iter()
      .filter(|entry| entry.entry_type() == KV_TYPE_CHUNK)
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
