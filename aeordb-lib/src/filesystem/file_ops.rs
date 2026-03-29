use std::sync::Arc;

use crate::storage::{Chunk, ChunkConfig, ChunkHash, ChunkStorage, ChunkStoreError};

use super::index_entry::ChunkList;

/// Maximum file size for the `read_file_to_vec` convenience method (10 MB).
const MAX_READ_TO_VEC_SIZE: u64 = 10 * 1024 * 1024;

/// Threshold for inline vs overflow chunk lists.
/// If the serialized chunk hash list exceeds this many hashes, use overflow.
const INLINE_HASH_LIMIT: usize = 32;

pub struct FileOperations {
  storage: Arc<dyn ChunkStorage>,
  chunk_config: ChunkConfig,
}

impl FileOperations {
  pub fn new(storage: Arc<dyn ChunkStorage>, chunk_config: ChunkConfig) -> Self {
    Self {
      storage,
      chunk_config,
    }
  }

  /// Store file data by splitting it into chunks.
  /// Returns the chunk list and total size in bytes.
  pub fn store_file(&self, data: &[u8]) -> Result<(ChunkList, u64), ChunkStoreError> {
    let total_size = data.len() as u64;
    let data_capacity = self.chunk_config.data_capacity();

    if data.is_empty() {
      return Ok((ChunkList::Inline(Vec::new()), 0));
    }

    let mut chunk_hashes: Vec<ChunkHash> = Vec::new();

    let mut offset = 0;
    while offset < data.len() {
      let end = std::cmp::min(offset + data_capacity, data.len());
      let chunk_data = data[offset..end].to_vec();
      let chunk = Chunk::new(chunk_data);
      let hash = chunk.hash;
      self.storage.store_chunk(&chunk)?;
      chunk_hashes.push(hash);
      offset = end;
    }

    let chunk_list = if chunk_hashes.len() <= INLINE_HASH_LIMIT {
      ChunkList::Inline(chunk_hashes)
    } else {
      // Store the hash list as a separate overflow chunk.
      let packed_hashes = self.pack_chunk_hashes(&chunk_hashes);
      let overflow_chunk = Chunk::new(packed_hashes);
      let overflow_hash = overflow_chunk.hash;
      self.storage.store_chunk(&overflow_chunk)?;
      ChunkList::Overflow(overflow_hash)
    };

    Ok((chunk_list, total_size))
  }

  /// Resolve a ChunkList to a vector of chunk hashes.
  /// For Inline, returns the hashes directly. For Overflow, reads and unpacks the overflow chunk.
  pub fn resolve_chunk_list(
    &self,
    chunk_list: &ChunkList,
  ) -> Result<Vec<ChunkHash>, ChunkStoreError> {
    match chunk_list {
      ChunkList::Inline(hashes) => Ok(hashes.clone()),
      ChunkList::Overflow(overflow_hash) => {
        let overflow_chunk = self.storage.get_chunk(overflow_hash)?
          .ok_or(ChunkStoreError::ChunkNotFound(*overflow_hash))?;
        self.unpack_chunk_hashes(&overflow_chunk.data)
      }
    }
  }

  /// Read file data as a streaming iterator, yielding one chunk at a time.
  pub fn read_file_streaming(
    &self,
    chunk_list: &ChunkList,
  ) -> Result<FileStream, ChunkStoreError> {
    let chunk_hashes = self.resolve_chunk_list(chunk_list)?;
    Ok(FileStream {
      chunk_hashes,
      current_index: 0,
      storage: self.storage.clone(),
    })
  }

  /// Convenience method to read the entire file into a Vec<u8>.
  /// Refuses files larger than MAX_READ_TO_VEC_SIZE.
  pub fn read_file_to_vec(
    &self,
    chunk_list: &ChunkList,
    total_size: u64,
  ) -> Result<Vec<u8>, ChunkStoreError> {
    if total_size > MAX_READ_TO_VEC_SIZE {
      return Err(ChunkStoreError::IoError(format!(
        "file too large for read_file_to_vec: {} bytes exceeds limit of {} bytes",
        total_size, MAX_READ_TO_VEC_SIZE,
      )));
    }

    let stream = self.read_file_streaming(chunk_list)?;
    let mut result = Vec::with_capacity(total_size as usize);
    for chunk_data in stream {
      result.extend_from_slice(&chunk_data?);
    }
    Ok(result)
  }

  /// Calculate file size from the chunk list without reading all chunk data.
  /// This requires reading each chunk to get its data length, since we don't store
  /// individual chunk sizes in the hash list.
  pub fn file_size(&self, chunk_list: &ChunkList) -> Result<u64, ChunkStoreError> {
    let chunk_hashes = self.resolve_chunk_list(chunk_list)?;
    let mut total = 0u64;
    for hash in &chunk_hashes {
      let chunk = self.storage.get_chunk(hash)?
        .ok_or(ChunkStoreError::ChunkNotFound(*hash))?;
      total += chunk.data.len() as u64;
    }
    Ok(total)
  }

  /// Pack chunk hashes into a byte vector (32 bytes per hash, contiguous).
  fn pack_chunk_hashes(&self, hashes: &[ChunkHash]) -> Vec<u8> {
    let mut packed = Vec::with_capacity(hashes.len() * 32);
    for hash in hashes {
      packed.extend_from_slice(hash);
    }
    packed
  }

  /// Unpack chunk hashes from a byte vector.
  fn unpack_chunk_hashes(&self, data: &[u8]) -> Result<Vec<ChunkHash>, ChunkStoreError> {
    if !data.len().is_multiple_of(32) {
      return Err(ChunkStoreError::SerializationError(format!(
        "overflow chunk data length {} is not a multiple of 32",
        data.len(),
      )));
    }

    let count = data.len() / 32;
    let mut hashes = Vec::with_capacity(count);
    for index in 0..count {
      let start = index * 32;
      let mut hash = [0u8; 32];
      hash.copy_from_slice(&data[start..start + 32]);
      hashes.push(hash);
    }
    Ok(hashes)
  }
}

/// A streaming file reader that yields one chunk's data at a time.
pub struct FileStream {
  chunk_hashes: Vec<ChunkHash>,
  current_index: usize,
  storage: Arc<dyn ChunkStorage>,
}

impl Iterator for FileStream {
  type Item = Result<Vec<u8>, ChunkStoreError>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.current_index >= self.chunk_hashes.len() {
      return None;
    }

    let hash = &self.chunk_hashes[self.current_index];
    self.current_index += 1;

    let result = self.storage.get_chunk(hash).and_then(|maybe_chunk| {
      maybe_chunk
        .map(|chunk| chunk.data)
        .ok_or(ChunkStoreError::ChunkNotFound(*hash))
    });

    Some(result)
  }
}

impl FileStream {

  /// Return the total number of chunks.
  pub fn chunk_count(&self) -> usize {
    self.chunk_hashes.len()
  }

  /// Return the number of remaining chunks.
  pub fn remaining(&self) -> usize {
    self.chunk_hashes.len() - self.current_index
  }
}
