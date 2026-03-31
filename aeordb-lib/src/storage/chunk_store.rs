// LEGACY: Used only by PathResolver -> /fs/ routes.
// Remove once /fs/ routes are migrated to the engine.

use std::collections::HashSet;
use std::sync::Arc;

use redb::Database;

use super::chunk::ChunkHash;
use super::chunk_config::ChunkConfig;
use super::chunk_storage::{ChunkStorage, ChunkStoreError};
use super::hash_map_store::{ContentHashMap, HashMapDiff, HashMapStore};
use super::in_memory_chunk_storage::InMemoryChunkStorage;
use super::redb_chunk_storage::RedbChunkStorage;

/// Statistics about the chunk store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkStoreStats {
  pub total_chunks: u64,
  pub total_bytes: u64,
  pub chunk_size: usize,
}

/// Top-level chunk store combining storage backend, hash map management, and config.
pub struct ChunkStore {
  storage: Arc<dyn ChunkStorage>,
  hash_map_store: HashMapStore,
  config: ChunkConfig,
}

impl ChunkStore {
  /// Create an in-memory chunk store (for testing).
  pub fn new_in_memory() -> Self {
    let config = ChunkConfig::default();
    let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
    let hash_map_store = HashMapStore::new(storage.clone(), config);
    Self {
      storage,
      hash_map_store,
      config,
    }
  }

  /// Create an in-memory chunk store with a custom chunk size (for testing).
  pub fn new_in_memory_with_config(config: ChunkConfig) -> Self {
    let storage: Arc<dyn ChunkStorage> = Arc::new(InMemoryChunkStorage::new());
    let hash_map_store = HashMapStore::new(storage.clone(), config);
    Self {
      storage,
      hash_map_store,
      config,
    }
  }

  /// Create a chunk store backed by a redb database.
  pub fn new_with_redb(database: Arc<Database>) -> Self {
    let config = ChunkConfig::default();
    let storage: Arc<dyn ChunkStorage> = Arc::new(RedbChunkStorage::new(database));
    let hash_map_store = HashMapStore::new(storage.clone(), config);
    Self {
      storage,
      hash_map_store,
      config,
    }
  }

  /// Create a chunk store backed by a redb database with custom config.
  pub fn new_with_redb_and_config(database: Arc<Database>, config: ChunkConfig) -> Self {
    let storage: Arc<dyn ChunkStorage> = Arc::new(RedbChunkStorage::new(database));
    let hash_map_store = HashMapStore::new(storage.clone(), config);
    Self {
      storage,
      hash_map_store,
      config,
    }
  }

  /// Store arbitrary data, returning a ContentHashMap describing the chunks.
  #[tracing::instrument(skip(self, data), fields(size = data.len()))]
  pub fn store(&self, data: &[u8]) -> Result<ContentHashMap, ChunkStoreError> {
    let start = std::time::Instant::now();
    let result = self.hash_map_store.store_data(data);
    let duration = start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::CHUNK_WRITE_DURATION).record(duration);
    if let Ok(ref content_hash_map) = result {
      metrics::counter!(crate::metrics::definitions::CHUNKS_STORED_TOTAL).increment(1);
      metrics::counter!(crate::metrics::definitions::CHUNK_STORE_BYTES).increment(data.len() as u64);
      tracing::trace!(
        size = data.len(),
        chunk_count = content_hash_map.chunk_hashes.len(),
        "Chunks stored"
      );
    }
    result
  }

  /// Retrieve data by its ContentHashMap.
  #[tracing::instrument(skip(self, hash_map))]
  pub fn retrieve(&self, hash_map: &ContentHashMap) -> Result<Vec<u8>, ChunkStoreError> {
    let start = std::time::Instant::now();
    let result = self.hash_map_store.retrieve_data(hash_map);
    let duration = start.elapsed().as_secs_f64();
    metrics::histogram!(crate::metrics::definitions::CHUNK_READ_DURATION).record(duration);
    if result.is_ok() {
      metrics::counter!(crate::metrics::definitions::CHUNKS_READ_TOTAL).increment(1);
      tracing::trace!("Chunks retrieved");
    }
    result
  }

  /// Efficiently update data: only stores new/changed chunks, reuses existing ones.
  pub fn update(
    &self,
    old_map: &ContentHashMap,
    new_data: &[u8],
  ) -> Result<ContentHashMap, ChunkStoreError> {
    self.hash_map_store.update_data(old_map, new_data)
  }

  /// Compute the diff between two content hash maps.
  pub fn diff(&self, old: &ContentHashMap, new: &ContentHashMap) -> HashMapDiff {
    HashMapStore::diff_hash_maps(old, new)
  }

  /// Verify integrity of all chunks in a hash map.
  /// Returns a list of chunk hashes that failed verification (empty if all OK).
  pub fn verify_integrity(
    &self,
    hash_map: &ContentHashMap,
  ) -> Result<Vec<ChunkHash>, ChunkStoreError> {
    let mut corrupt_hashes = Vec::new();

    for chunk_hash in &hash_map.chunk_hashes {
      let chunk = self.storage.get_chunk(chunk_hash)?
        .ok_or(ChunkStoreError::ChunkNotFound(*chunk_hash))?;
      if !chunk.verify() {
        let hash_hex = hex::encode(chunk_hash);
        tracing::error!(
          chunk_hash = %hash_hex,
          "Chunk integrity failure: hash mismatch"
        );
        corrupt_hashes.push(*chunk_hash);
      }
    }

    Ok(corrupt_hashes)
  }

  /// Remove chunks not referenced by any of the provided live hash maps.
  /// Returns the number of chunks removed.
  pub fn garbage_collect(
    &self,
    live_maps: &[ContentHashMap],
  ) -> Result<u64, ChunkStoreError> {
    let live_hashes: HashSet<ChunkHash> = live_maps.iter()
      .flat_map(|map| map.chunk_hashes.iter().copied())
      .collect();

    let all_hashes = self.storage.list_chunk_hashes()?;
    let mut removed_count = 0u64;

    for hash in all_hashes {
      if !live_hashes.contains(&hash)
        && self.storage.remove_chunk(&hash)? {
        removed_count += 1;
      }
    }

    Ok(removed_count)
  }

  /// Get statistics about the chunk store.
  pub fn stats(&self) -> Result<ChunkStoreStats, ChunkStoreError> {
    let all_hashes = self.storage.list_chunk_hashes()?;
    let total_chunks = all_hashes.len() as u64;
    let mut total_bytes = 0u64;

    for hash in &all_hashes {
      if let Some(chunk) = self.storage.get_chunk(hash)? {
        total_bytes += chunk.data.len() as u64;
      }
    }

    Ok(ChunkStoreStats {
      total_chunks,
      total_bytes,
      chunk_size: self.config.chunk_size,
    })
  }

  /// Get a reference to the underlying storage (for testing or advanced use).
  pub fn storage(&self) -> &Arc<dyn ChunkStorage> {
    &self.storage
  }

  /// Get the chunk config.
  pub fn config(&self) -> &ChunkConfig {
    &self.config
  }
}
