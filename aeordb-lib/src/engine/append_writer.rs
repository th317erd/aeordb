use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;
use std::path::Path;

/// Read exactly `buf.len()` bytes at `offset` without modifying the file's
/// seek position. Equivalent to Unix `pread` / Windows `seek_read`. Loops
/// until the buffer is filled to handle short reads on either platform.
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
  let mut total = 0;
  while total < buf.len() {
    #[cfg(unix)]
    let n = file.read_at(&mut buf[total..], offset + total as u64)?;
    #[cfg(windows)]
    let n = file.seek_read(&mut buf[total..], offset + total as u64)?;
    if n == 0 {
      return Err(std::io::Error::new(
        std::io::ErrorKind::UnexpectedEof,
        "early EOF in read_exact_at",
      ));
    }
    total += n;
  }
  Ok(())
}

use crate::engine::compression::CompressionAlgorithm;
use crate::engine::entry_header::{EntryHeader, CURRENT_ENTRY_VERSION};
use crate::engine::entry_scanner::EntryScanner;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_header::{FileHeader, FILE_HEADER_SIZE};
use crate::engine::hash_algorithm::HashAlgorithm;

pub struct AppendWriter {
  file: File,
  reader: File,
  file_path: std::path::PathBuf,
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

    let reader = File::open(path)?;
    let current_offset = FILE_HEADER_SIZE as u64;

    Ok(AppendWriter {
      file,
      reader,
      file_path: path.to_path_buf(),
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

    let reader = File::open(path)?;
    let current_offset = file.seek(SeekFrom::End(0))?;

    Ok(AppendWriter {
      file,
      reader,
      file_path: path.to_path_buf(),
      file_header,
      current_offset,
    })
  }

  pub fn file_path(&self) -> &Path {
    &self.file_path
  }

  /// Set the current write offset. Used after KV block creation to skip
  /// past the reserved space at the head of the file.
  pub fn set_offset(&mut self, offset: u64) {
    self.current_offset = offset;
  }

  /// Current write offset (end of WAL entries, before hot tail).
  pub fn current_offset(&self) -> u64 {
    self.current_offset
  }

  /// Update the file header in place (seek to 0, write 256 bytes, sync).
  pub fn update_header(&mut self, header: &FileHeader) -> EngineResult<()> {
    self.file_header = header.clone();
    let bytes = header.serialize();
    self.file.seek(SeekFrom::Start(0))?;
    self.file.write_all(&bytes)?;
    self.file.sync_data()?;
    Ok(())
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

    // No per-entry fsync — the hot tail (flushed every 250ms) provides crash
    // recovery. WAL data reaches disk via the periodic timer sync in
    // try_flush_hot_buffer(). This eliminates ~4,000 fsync calls per 1GB upload.

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

  /// Sync WAL data to disk. Uses sync_data() (skips metadata fsync).
  pub fn sync(&mut self) -> EngineResult<()> {
    self.file.sync_data()?;
    Ok(())
  }

  /// Full sync including metadata. Use for shutdown.
  pub fn sync_all(&mut self) -> EngineResult<()> {
    self.file.sync_all()?;
    Ok(())
  }

  /// Copy a region of the file from one offset to another.
  /// Used by KV block expansion to relocate WAL entries.
  pub fn copy_region(&mut self, src: u64, dst: u64, size: u64) -> EngineResult<()> {
    const CHUNK: usize = 64 * 1024 * 1024;
    let mut remaining = size;
    let mut read_pos = src;
    let mut write_pos = dst;

    while remaining > 0 {
      let chunk_len = (CHUNK as u64).min(remaining) as usize;
      let mut buf = vec![0u8; chunk_len];
      self.file.seek(SeekFrom::Start(read_pos))?;
      self.file.read_exact(&mut buf)?;
      self.file.seek(SeekFrom::Start(write_pos))?;
      self.file.write_all(&buf)?;
      read_pos += chunk_len as u64;
      write_pos += chunk_len as u64;
      remaining -= chunk_len as u64;
    }
    Ok(())
  }

  /// Write hot tail entries at a specific offset using this writer's file handle.
  pub fn write_hot_tail_at(&mut self, offset: u64, entries: &[crate::engine::kv_store::KVEntry], hash_length: usize) -> EngineResult<u64> {
    let end = crate::engine::hot_tail::write_hot_tail(&mut self.file, offset, entries, hash_length)?;
    Ok(end)
  }

  /// Write bytes at a specific offset (no seek side effects on the main write position).
  pub fn write_at(&mut self, offset: u64, data: &[u8]) -> EngineResult<()> {
    self.file.seek(SeekFrom::Start(offset))?;
    self.file.write_all(data)?;
    Ok(())
  }

  /// Read bytes at a specific offset using the reader handle (no seek side effects).
  pub fn read_bytes_at(&self, offset: u64, buf: &mut [u8]) -> EngineResult<()> {
    read_exact_at(&self.reader, buf, offset)?;
    Ok(())
  }

  /// Read hot tail entries from this writer's reader handle.
  pub fn read_hot_tail_entries(&self, offset: u64, hash_length: usize) -> Vec<crate::engine::kv_store::KVEntry> {
    let mut reader = match self.reader.try_clone() {
      Ok(r) => r,
      Err(_) => return Vec::new(),
    };
    crate::engine::hot_tail::read_hot_tail(&mut reader, offset, hash_length)
      .unwrap_or_default()
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
    self.read_entry_at_opt(offset, false)
  }

  pub fn read_entry_at_verified(&mut self, offset: u64) -> EngineResult<(EntryHeader, Vec<u8>, Vec<u8>)> {
    self.read_entry_at_opt(offset, true)
  }

  fn read_entry_at_opt(&mut self, offset: u64, verify: bool) -> EngineResult<(EntryHeader, Vec<u8>, Vec<u8>)> {
    self.file.seek(SeekFrom::Start(offset))?;
    let header = EntryHeader::deserialize(&mut self.file)?;

    // Validate payload lengths against total_length to prevent unbounded allocation
    // from corrupt headers (H7).
    let header_size = header.header_size() as u64;
    let payload_size = header.key_length as u64 + header.value_length as u64;
    let max_payload = (header.total_length as u64).saturating_sub(header_size);
    if payload_size > max_payload {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!(
          "key_length ({}) + value_length ({}) exceeds total_length ({}) minus header ({})",
          header.key_length, header.value_length, header.total_length, header_size,
        ),
      });
    }

    let mut key = vec![0u8; header.key_length as usize];
    self.file.read_exact(&mut key)?;

    let mut value = vec![0u8; header.value_length as usize];
    self.file.read_exact(&mut value)?;

    // Verify hash integrity — only on user-facing reads.
    // Internal reads (directory ops, KV lookups) skip this for performance.
    // The background integrity scanner and `aeordb verify` catch corruption.
    if verify && !header.verify(&key, &value) {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!(
          "Hash verification failed for entry at offset {}. Data may be corrupt.",
          offset,
        ),
      });
    }

    Ok((header, key, value))
  }

  /// Read an entry at a given offset using a cloned file handle.
  ///
  /// Unlike `read_entry_at`, this takes `&self` and does not disturb the
  /// writer's seek position — allowing callers to hold a READ lock instead
  /// of a WRITE lock on the `RwLock<AppendWriter>`.
  pub fn read_entry_at_shared(&self, offset: u64) -> EngineResult<(EntryHeader, Vec<u8>, Vec<u8>)> {
    self.read_entry_at_shared_opt(offset, false)
  }

  pub fn read_entry_at_shared_verified(&self, offset: u64) -> EngineResult<(EntryHeader, Vec<u8>, Vec<u8>)> {
    self.read_entry_at_shared_opt(offset, true)
  }

  fn read_entry_at_shared_opt(&self, offset: u64, verify: bool) -> EngineResult<(EntryHeader, Vec<u8>, Vec<u8>)> {
    // Use pread (read_at) on a shared reader handle. read_at does not use or
    // modify the file's seek position, so multiple concurrent readers on the
    // same File are safe without any locking.

    // First, read the fixed header to learn hash algorithm and payload sizes.
    let mut fixed_buf = [0u8; EntryHeader::FIXED_HEADER_SIZE];
    read_exact_at(&self.reader, &mut fixed_buf, offset)?;

    // Parse hash algorithm from the fixed header to determine total header size.
    let hash_algo_raw = u16::from_le_bytes([fixed_buf[7], fixed_buf[8]]);
    let hash_algo = crate::engine::hash_algorithm::HashAlgorithm::from_u16(hash_algo_raw)
      .ok_or(EngineError::InvalidHashAlgorithm(hash_algo_raw))?;
    let hash_length = hash_algo.hash_length();
    let full_header_size = EntryHeader::FIXED_HEADER_SIZE + hash_length;

    // Parse total_length from fixed header to know the complete entry size.
    let total_length = u32::from_le_bytes([
      fixed_buf[27], fixed_buf[28], fixed_buf[29], fixed_buf[30],
    ]) as usize;

    // Read the entire entry (header + key + value) in a single pread call.
    let mut entry_buf = vec![0u8; total_length];
    read_exact_at(&self.reader, &mut entry_buf, offset)?;

    // Deserialize the header from the buffer.
    let mut cursor = Cursor::new(&entry_buf[..full_header_size]);
    let header = EntryHeader::deserialize(&mut cursor)?;

    // Validate payload lengths against total_length to prevent unbounded allocation
    // from corrupt headers (H7).
    let header_size = header.header_size() as u64;
    let payload_size = header.key_length as u64 + header.value_length as u64;
    let max_payload = (header.total_length as u64).saturating_sub(header_size);
    if payload_size > max_payload {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!(
          "key_length ({}) + value_length ({}) exceeds total_length ({}) minus header ({})",
          header.key_length, header.value_length, header.total_length, header_size,
        ),
      });
    }

    let key_start = full_header_size;
    let key_end = key_start + header.key_length as usize;
    let value_end = key_end + header.value_length as usize;
    let key = entry_buf[key_start..key_end].to_vec();
    let value = entry_buf[key_end..value_end].to_vec();

    if verify && !header.verify(&key, &value) {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!(
          "Hash verification failed for entry at offset {}. Data may be corrupt.",
          offset,
        ),
      });
    }

    Ok((header, key, value))
  }

  pub fn scan_entries(&self) -> EngineResult<EntryScanner> {
    // Open a fresh file handle — try_clone shares seek position on POSIX,
    // which causes corruption under concurrent access.
    let file = File::open(&self.file_path)?;
    EntryScanner::new(file)
  }

  /// Like scan_entries but yields errors for corrupt entries instead of skipping.
  /// Used by the verify tool to count corruption.
  pub fn scan_entries_reporting(&self) -> EngineResult<EntryScanner> {
    let file = File::open(&self.file_path)?;
    EntryScanner::new_reporting(file)
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
