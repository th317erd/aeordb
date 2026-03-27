use std::collections::HashMap;
use std::sync::RwLock;

use super::chunk::{Chunk, ChunkHash};
use super::chunk_storage::{ChunkStorage, ChunkStoreError};

/// In-memory implementation of ChunkStorage for testing.
pub struct InMemoryChunkStorage {
  chunks: RwLock<HashMap<ChunkHash, Vec<u8>>>,
}

impl InMemoryChunkStorage {
  pub fn new() -> Self {
    Self {
      chunks: RwLock::new(HashMap::new()),
    }
  }
}

impl Default for InMemoryChunkStorage {
  fn default() -> Self {
    Self::new()
  }
}

impl ChunkStorage for InMemoryChunkStorage {
  fn store_chunk(&self, chunk: &Chunk) -> Result<(), ChunkStoreError> {
    let mut chunks = self.chunks.write().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    chunks.entry(chunk.hash).or_insert_with(|| chunk.data.clone());
    Ok(())
  }

  fn get_chunk(&self, hash: &ChunkHash) -> Result<Option<Chunk>, ChunkStoreError> {
    let chunks = self.chunks.read().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    Ok(chunks.get(hash).map(|data| Chunk {
      hash: *hash,
      data: data.clone(),
    }))
  }

  fn has_chunk(&self, hash: &ChunkHash) -> Result<bool, ChunkStoreError> {
    let chunks = self.chunks.read().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    Ok(chunks.contains_key(hash))
  }

  fn remove_chunk(&self, hash: &ChunkHash) -> Result<bool, ChunkStoreError> {
    let mut chunks = self.chunks.write().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    Ok(chunks.remove(hash).is_some())
  }

  fn chunk_count(&self) -> Result<u64, ChunkStoreError> {
    let chunks = self.chunks.read().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    Ok(chunks.len() as u64)
  }

  fn list_chunk_hashes(&self) -> Result<Vec<ChunkHash>, ChunkStoreError> {
    let chunks = self.chunks.read().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    Ok(chunks.keys().copied().collect())
  }
}
