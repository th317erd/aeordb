use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::engine::entry_header::EntryHeader;
use crate::engine::errors::EngineResult;
use crate::engine::file_header::FILE_HEADER_SIZE;

#[derive(Debug)]
pub struct ScannedEntry {
  pub offset: u64,
  pub header: EntryHeader,
  pub key: Vec<u8>,
  pub value: Vec<u8>,
}

pub struct EntryScanner {
  file: File,
  current_offset: u64,
  file_length: u64,
}

impl EntryScanner {
  pub fn new(mut file: File) -> EngineResult<Self> {
    let file_length = file.seek(SeekFrom::End(0))?;
    let start_offset = FILE_HEADER_SIZE as u64;
    file.seek(SeekFrom::Start(start_offset))?;

    Ok(EntryScanner {
      file,
      current_offset: start_offset,
      file_length,
    })
  }
}

impl Iterator for EntryScanner {
  type Item = EngineResult<ScannedEntry>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.current_offset >= self.file_length {
      return None;
    }

    let entry_offset = self.current_offset;

    // Try to seek to current offset
    if let Err(error) = self.file.seek(SeekFrom::Start(entry_offset)) {
      return Some(Err(error.into()));
    }

    // Try to read the header
    let header = match EntryHeader::deserialize(&mut self.file) {
      Ok(header) => header,
      Err(crate::engine::errors::EngineError::UnexpectedEof) => return None,
      Err(error) => {
        // Corrupt entry — log warning and skip
        tracing::warn!(
          "Corrupt entry at offset {}: {}. Skipping.",
          entry_offset,
          error
        );
        // We can't reliably skip without total_length, so we stop iteration
        return None;
      }
    };

    // Validate payload lengths against total_length to prevent unbounded allocation
    // from corrupt headers (H7).
    let header_size = header.header_size() as u64;
    let payload_size = header.key_length as u64 + header.value_length as u64;
    let max_payload = (header.total_length as u64).saturating_sub(header_size);
    if payload_size > max_payload {
      tracing::warn!(
        "Corrupt entry at offset {}: key_length ({}) + value_length ({}) exceeds total_length ({}) minus header ({}). Skipping.",
        entry_offset, header.key_length, header.value_length, header.total_length, header_size,
      );
      self.current_offset = entry_offset + header.total_length as u64;
      return self.next();
    }

    // Read key
    let mut key = vec![0u8; header.key_length as usize];
    if let Err(error) = self.file.read_exact(&mut key) {
      return Some(Err(error.into()));
    }

    // Read value
    let mut value = vec![0u8; header.value_length as usize];
    if let Err(error) = self.file.read_exact(&mut value) {
      return Some(Err(error.into()));
    }

    // Verify hash integrity
    if !header.verify(&key, &value) {
      tracing::warn!(
        "Hash verification failed for entry at offset {}. Skipping.",
        entry_offset
      );
      // Jump to next entry using total_length
      self.current_offset = entry_offset + header.total_length as u64;
      return self.next();
    }

    // Advance to next entry using total_length
    self.current_offset = entry_offset + header.total_length as u64;

    Some(Ok(ScannedEntry {
      offset: entry_offset,
      header,
      key,
      value,
    }))
  }
}
