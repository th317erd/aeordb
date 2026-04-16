use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::engine::compression::CompressionAlgorithm;
use crate::engine::entry_header::{EntryHeader, CURRENT_ENTRY_VERSION};
use crate::engine::entry_scanner::EntryScanner;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_header::{FileHeader, FILE_HEADER_SIZE};
use crate::engine::hash_algorithm::HashAlgorithm;

pub struct AppendWriter {
  file: File,
  file_header: FileHeader,
  current_offset: u64,
}

impl AppendWriter {
  pub fn create(path: &Path) -> EngineResult<Self> {
    let mut file = OpenOptions::new()
      .read(true)
      .write(true)
      .create_new(true)
      .open(path)?;

    let file_header = FileHeader::new(HashAlgorithm::Blake3_256);
    let header_bytes = file_header.serialize();
    file.write_all(&header_bytes)?;
    file.sync_all()?;

    let current_offset = FILE_HEADER_SIZE as u64;

    Ok(AppendWriter {
      file,
      file_header,
      current_offset,
    })
  }

  pub fn open(path: &Path) -> EngineResult<Self> {
    let mut file = OpenOptions::new()
      .read(true)
      .write(true)
      .open(path)?;

    let mut header_bytes = [0u8; FILE_HEADER_SIZE];
    file.read_exact(&mut header_bytes)?;
    let file_header = FileHeader::deserialize(&header_bytes)?;

    let current_offset = file.seek(SeekFrom::End(0))?;

    Ok(AppendWriter {
      file,
      file_header,
      current_offset,
    })
  }

  pub fn append_entry(
    &mut self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    flags: u8,
  ) -> EngineResult<u64> {
    self.append_entry_with_compression(
      entry_type,
      key,
      value,
      flags,
      CompressionAlgorithm::None,
    )
  }

  pub fn append_entry_with_compression(
    &mut self,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
    flags: u8,
    compression_algo: CompressionAlgorithm,
  ) -> EngineResult<u64> {
    let hash_algo = self.file_header.hash_algo;
    let hash = EntryHeader::compute_hash(entry_type, key, value, hash_algo)?;
    let total_length =
      EntryHeader::compute_total_length(hash_algo, key.len() as u32, value.len() as u32);

    let now = chrono::Utc::now().timestamp_millis();

    let header = EntryHeader {
      entry_version: CURRENT_ENTRY_VERSION,
      entry_type,
      flags,
      hash_algo,
      compression_algo,
      encryption_algo: 0,
      key_length: key.len() as u32,
      value_length: value.len() as u32,
      timestamp: now,
      total_length,
      hash,
    };

    let entry_offset = self.current_offset;

    self.file.seek(SeekFrom::Start(entry_offset))?;

    let header_bytes = header.serialize();
    self.file.write_all(&header_bytes)?;
    self.file.write_all(key)?;
    self.file.write_all(value)?;

    // Flush data to disk. We use sync_data() instead of sync_all() because we only
    // need data durability — not metadata (timestamps, file size). sync_data() skips
    // the metadata fsync, saving one syscall per write. The metadata is non-critical
    // for crash recovery since we rebuild state from entry contents, not file metadata.
    //
    // PERF(H14): For further throughput gains, consider group commit (batch fsync
    // across multiple entries) or skipping per-entry fsync entirely when a hot file
    // provides crash recovery journaling.
    self.file.sync_data()?;

    self.current_offset = entry_offset + total_length as u64;
    self.file_header.entry_count += 1;
    self.file_header.updated_at = now;

    Ok(entry_offset)
  }

  pub fn write_void(&mut self, size: u32) -> EngineResult<u64> {
    let hash_algo = self.file_header.hash_algo;
    let header_size = 31 + hash_algo.hash_length(); // fixed header + hash

    if (size as usize) < header_size {
      return Err(EngineError::CorruptEntry {
        offset: self.current_offset,
        reason: format!(
          "Void size {} is smaller than minimum entry size {}",
          size, header_size
        ),
      });
    }

    let key = b"";
    let value_length = size as usize - header_size;
    let value = vec![0u8; value_length];

    self.append_entry(EntryType::Void, key, &value, 0)
  }

  /// Write an entry at a specific file offset (in-place overwrite).
  /// Does NOT update current_offset or entry_count — this overwrites existing space.
  /// Calls sync_all after writing. For batch operations, use `write_entry_at_nosync`.
  pub fn write_entry_at(
    &mut self,
    offset: u64,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
  ) -> EngineResult<u32> {
    let total_length = self.write_entry_at_nosync(offset, entry_type, key, value)?;
    self.file.sync_all()?;
    Ok(total_length)
  }

  /// Write an entry at a specific offset WITHOUT syncing.
  /// Caller is responsible for calling `sync()` after all writes are done.
  /// Used by GC sweep for batch in-place overwrites.
  pub fn write_entry_at_nosync(
    &mut self,
    offset: u64,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
  ) -> EngineResult<u32> {
    let hash_algo = self.file_header.hash_algo;
    let hash = EntryHeader::compute_hash(entry_type, key, value, hash_algo)?;
    let total_length =
      EntryHeader::compute_total_length(hash_algo, key.len() as u32, value.len() as u32);

    let now = chrono::Utc::now().timestamp_millis();

    let header = EntryHeader {
      entry_version: CURRENT_ENTRY_VERSION,
      entry_type,
      flags: 0,
      hash_algo,
      compression_algo: CompressionAlgorithm::None,
      encryption_algo: 0,
      key_length: key.len() as u32,
      value_length: value.len() as u32,
      timestamp: now,
      total_length,
      hash,
    };

    self.file.seek(SeekFrom::Start(offset))?;
    let header_bytes = header.serialize();
    self.file.write_all(&header_bytes)?;
    self.file.write_all(key)?;
    self.file.write_all(value)?;

    Ok(total_length)
  }

  /// Sync the file to disk. Call after batch nosync operations.
  pub fn sync(&mut self) -> EngineResult<()> {
    self.file.sync_all()?;
    Ok(())
  }

  /// Write a void entry at a specific file offset (in-place overwrite).
  /// The void fills exactly `size` bytes starting at `offset`.
  pub fn write_void_at(&mut self, offset: u64, size: u32) -> EngineResult<()> {
    self.write_void_at_nosync(offset, size)?;
    self.file.sync_all()?;
    Ok(())
  }

  /// Write a void at a specific offset WITHOUT syncing.
  pub fn write_void_at_nosync(&mut self, offset: u64, size: u32) -> EngineResult<()> {
    let hash_algo = self.file_header.hash_algo;
    let header_size = 31 + hash_algo.hash_length();

    if (size as usize) < header_size {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!(
          "Void size {} is smaller than minimum entry size {}",
          size, header_size
        ),
      });
    }

    let key = b"";
    let value_length = size as usize - header_size;
    let value = vec![0u8; value_length];

    self.write_entry_at_nosync(offset, EntryType::Void, key, &value)?;
    Ok(())
  }

  pub fn read_entry_at(&mut self, offset: u64) -> EngineResult<(EntryHeader, Vec<u8>, Vec<u8>)> {
    self.file.seek(SeekFrom::Start(offset))?;
    let header = EntryHeader::deserialize(&mut self.file)?;

    let mut key = vec![0u8; header.key_length as usize];
    self.file.read_exact(&mut key)?;

    let mut value = vec![0u8; header.value_length as usize];
    self.file.read_exact(&mut value)?;

    Ok((header, key, value))
  }

  /// Read an entry at a given offset using a cloned file handle.
  ///
  /// Unlike `read_entry_at`, this takes `&self` and does not disturb the
  /// writer's seek position — allowing callers to hold a READ lock instead
  /// of a WRITE lock on the `RwLock<AppendWriter>`.
  pub fn read_entry_at_shared(&self, offset: u64) -> EngineResult<(EntryHeader, Vec<u8>, Vec<u8>)> {
    let mut file = self.file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    let header = EntryHeader::deserialize(&mut file)?;

    let mut key = vec![0u8; header.key_length as usize];
    file.read_exact(&mut key)?;

    let mut value = vec![0u8; header.value_length as usize];
    file.read_exact(&mut value)?;

    Ok((header, key, value))
  }

  pub fn scan_entries(&self) -> EngineResult<EntryScanner> {
    let file_copy = self.file.try_clone()?;
    EntryScanner::new(file_copy)
  }

  pub fn file_header(&self) -> &FileHeader {
    &self.file_header
  }

  pub fn file_size(&self) -> u64 {
    self.current_offset
  }

  pub fn update_file_header(&mut self, header: &FileHeader) -> EngineResult<()> {
    self.file_header = header.clone();
    let header_bytes = self.file_header.serialize();
    self.file.seek(SeekFrom::Start(0))?;
    self.file.write_all(&header_bytes)?;
    self.file.sync_all()?;
    Ok(())
  }
}
