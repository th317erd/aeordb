use super::chunk::{Chunk, ChunkHash};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChunkStoreError {
  #[error("storage I/O error: {0}")]
  IoError(String),

  #[error("chunk not found: {hash}", hash = hex::encode(.0))]
  ChunkNotFound(ChunkHash),

  #[error("chunk integrity error: expected {expected}, got {actual}",
    expected = hex::encode(.expected),
    actual = hex::encode(.actual))]
  IntegrityError {
    expected: ChunkHash,
    actual: ChunkHash,
  },

  #[error("serialization error: {0}")]
  SerializationError(String),
}

/// Trait for physical storage of content-addressed chunks.
pub trait ChunkStorage: Send + Sync {
  /// Store a chunk. If a chunk with the same hash already exists, this is a no-op.
  fn store_chunk(&self, chunk: &Chunk) -> Result<(), ChunkStoreError>;

  /// Retrieve a chunk by its hash. Returns None if not found.
  fn get_chunk(&self, hash: &ChunkHash) -> Result<Option<Chunk>, ChunkStoreError>;

  /// Check whether a chunk exists in storage.
  fn has_chunk(&self, hash: &ChunkHash) -> Result<bool, ChunkStoreError>;

  /// Remove a chunk by hash. Returns true if the chunk existed and was removed.
  fn remove_chunk(&self, hash: &ChunkHash) -> Result<bool, ChunkStoreError>;

  /// Return the total number of chunks in storage.
  fn chunk_count(&self) -> Result<u64, ChunkStoreError>;

  /// List all chunk hashes in storage.
  fn list_chunk_hashes(&self) -> Result<Vec<ChunkHash>, ChunkStoreError>;
}
