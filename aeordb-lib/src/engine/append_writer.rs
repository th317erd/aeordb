use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::engine::compression::CompressionAlgorithm;
use crate::engine::entry_header::EntryHeader;
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

    let now = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .expect("System clock before Unix epoch")
      .as_millis() as i64;

    let header = EntryHeader {
      entry_version: 1,
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

    // fsync for truth entities (chunks, file records, deletions, voids)
    match entry_type {
      EntryType::Chunk
      | EntryType::FileRecord
      | EntryType::DeletionRecord
      | EntryType::Void => {
        self.file.sync_all()?;
      }
      _ => {
        self.file.sync_data()?;
      }
    }

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

  pub fn read_entry_at(&mut self, offset: u64) -> EngineResult<(EntryHeader, Vec<u8>, Vec<u8>)> {
    self.file.seek(SeekFrom::Start(offset))?;
    let header = EntryHeader::deserialize(&mut self.file)?;

    let mut key = vec![0u8; header.key_length as usize];
    self.file.read_exact(&mut key)?;

    let mut value = vec![0u8; header.value_length as usize];
    self.file.read_exact(&mut value)?;

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
