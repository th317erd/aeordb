use thiserror::Error;

use super::chunk_header::HEADER_SIZE;

#[derive(Debug, Error)]
pub enum ChunkConfigError {
  #[error("chunk size must be a power of two, got {0}")]
  NotPowerOfTwo(usize),

  #[error("chunk size must be at least 1, got {0}")]
  ZeroSize(usize),

  #[error("chunk size {0} is too small to hold the {HEADER_SIZE}-byte header")]
  TooSmallForHeader(usize),
}

#[derive(Debug, Clone, Copy)]
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
    if chunk_size <= HEADER_SIZE {
      return Err(ChunkConfigError::TooSmallForHeader(chunk_size));
    }
    Ok(Self { chunk_size })
  }

  /// Returns the usable data capacity per chunk (chunk_size minus header).
  pub fn data_capacity(&self) -> usize {
    self.chunk_size - HEADER_SIZE
  }

  /// Returns which chunk a byte offset falls in (based on data capacity).
  pub fn chunk_index(&self, byte_offset: u64) -> u64 {
    byte_offset / self.data_capacity() as u64
  }

  /// Returns the offset within the chunk's data region for a given byte offset.
  pub fn offset_within_chunk(&self, byte_offset: u64) -> u64 {
    byte_offset % self.data_capacity() as u64
  }

  /// Returns true if the byte offset is at a chunk data boundary.
  pub fn is_chunk_boundary(&self, byte_offset: u64) -> bool {
    let capacity = self.data_capacity() as u64;
    byte_offset.is_multiple_of(capacity)
  }
}
