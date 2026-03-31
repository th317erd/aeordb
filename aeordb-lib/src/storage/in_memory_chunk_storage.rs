// LEGACY: In-memory backend for ChunkStore, used only by legacy /fs/ route tests.
// Remove once /fs/ routes are migrated to the engine.

use std::collections::HashMap;
use std::sync::RwLock;

use super::chunk::{Chunk, ChunkHash};
use super::chunk_header::{ChunkHeader, HEADER_SIZE};
use super::chunk_storage::{ChunkStorage, ChunkStoreError};

/// In-memory implementation of ChunkStorage for testing.
/// Stores chunks as serialized bytes: [header][data].
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

/// Serialize a chunk (header + data) into a single byte vector.
fn serialize_chunk(chunk: &Chunk) -> Vec<u8> {
  let header_bytes = chunk.header.serialize();
  let mut buffer = Vec::with_capacity(HEADER_SIZE + chunk.data.len());
  buffer.extend_from_slice(&header_bytes);
  buffer.extend_from_slice(&chunk.data);
  buffer
}

/// Deserialize a chunk from stored bytes (header + data).
fn deserialize_chunk(hash: &ChunkHash, stored: &[u8]) -> Result<Chunk, ChunkStoreError> {
  if stored.len() < HEADER_SIZE {
    return Err(ChunkStoreError::SerializationError(format!(
      "stored chunk too short: {} bytes, need at least {HEADER_SIZE}",
      stored.len(),
    )));
  }

  let header = ChunkHeader::deserialize_from_slice(stored).map_err(|error| {
    ChunkStoreError::SerializationError(format!("chunk header: {error}"))
  })?;
  let data = stored[HEADER_SIZE..].to_vec();

  Ok(Chunk {
    hash: *hash,
    data,
    header,
  })
}

impl ChunkStorage for InMemoryChunkStorage {
  fn store_chunk(&self, chunk: &Chunk) -> Result<(), ChunkStoreError> {
    let mut chunks = self.chunks.write().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    use std::collections::hash_map::Entry;
    match chunks.entry(chunk.hash) {
      Entry::Occupied(_) => {
        tracing::debug!(
          hash_prefix = %hex::encode(&chunk.hash[..8]),
          "Chunk dedup: already exists, skipped write"
        );
        metrics::counter!(crate::metrics::definitions::CHUNKS_DEDUPLICATED_TOTAL).increment(1);
      }
      Entry::Vacant(entry) => {
        tracing::trace!(
          hash_prefix = %hex::encode(&chunk.hash[..8]),
          size = chunk.data.len(),
          "Chunk stored (in-memory)"
        );
        entry.insert(serialize_chunk(chunk));
      }
    }
    Ok(())
  }

  fn get_chunk(&self, hash: &ChunkHash) -> Result<Option<Chunk>, ChunkStoreError> {
    let chunks = self.chunks.read().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    match chunks.get(hash) {
      Some(stored) => Ok(Some(deserialize_chunk(hash, stored)?)),
      None => Ok(None),
    }
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
