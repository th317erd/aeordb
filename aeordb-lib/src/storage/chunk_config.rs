use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChunkConfigError {
  #[error("chunk size must be a power of two, got {0}")]
  NotPowerOfTwo(usize),

  #[error("chunk size must be at least 1, got {0}")]
  ZeroSize(usize),
}

#[derive(Debug, Clone)]
pub struct ChunkConfig {
  pub chunk_size: usize,
}

impl Default for ChunkConfig {
  fn default() -> Self {
    Self {
      chunk_size: 262144, // 256KB
    }
  }
}

impl ChunkConfig {
  pub fn new(chunk_size: usize) -> Result<Self, ChunkConfigError> {
    if chunk_size == 0 {
      return Err(ChunkConfigError::ZeroSize(chunk_size));
    }
    if !chunk_size.is_power_of_two() {
      return Err(ChunkConfigError::NotPowerOfTwo(chunk_size));
    }
    Ok(Self { chunk_size })
  }

  /// Returns which chunk a byte offset falls in.
  pub fn chunk_index(&self, byte_offset: u64) -> u64 {
    byte_offset / self.chunk_size as u64
  }

  /// Returns the offset within the chunk for a given byte offset.
  pub fn offset_within_chunk(&self, byte_offset: u64) -> u64 {
    byte_offset % self.chunk_size as u64
  }

  /// Returns true if the byte offset is at a chunk boundary.
  pub fn is_chunk_boundary(&self, byte_offset: u64) -> bool {
    byte_offset.is_multiple_of(self.chunk_size as u64)
  }
}
