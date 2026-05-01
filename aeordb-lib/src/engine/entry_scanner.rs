use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::engine::entry_header::EntryHeader;
use crate::engine::errors::{EngineError, EngineResult};
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
  /// When true, yield errors for corrupt entries instead of silently skipping.
  /// Used by the verify tool to count corruption.
  report_errors: bool,
  /// After a corrupt header is encountered, stores (offset, length) of the skipped region.
  pub last_skipped_region: Option<(u64, usize)>,
  /// All skipped regions accumulated during the scan.
  pub skipped_regions: Vec<(u64, usize)>,
}

impl EntryScanner {
  pub fn new(file: File) -> EngineResult<Self> {
    Self::new_internal(file, false)
  }

  /// Create a scanner that reports errors for corrupt entries instead of skipping.
  pub fn new_reporting(file: File) -> EngineResult<Self> {
    Self::new_internal(file, true)
  }

  fn new_internal(mut file: File, report_errors: bool) -> EngineResult<Self> {
    // Read header to determine where the WAL starts (after KV block)
    file.seek(SeekFrom::Start(0))?;
    let mut header_bytes = [0u8; FILE_HEADER_SIZE];
    file.read_exact(&mut header_bytes)?;
    let header = crate::engine::file_header::FileHeader::deserialize(&header_bytes)?;

    // Determine WAL scan range based on layout.
    // Standard layout: [Header] [KV block] [WAL] [Hot tail]
    //   → start = kv_block_offset + kv_block_length, end = hot_tail_offset
    // Legacy layout:   [Header] [WAL] [KV block] [Hot tail]
    //   → start = FILE_HEADER_SIZE, end = kv_block_offset (KV is after WAL)
    // No KV layout:    [Header] [WAL]
    //   → start = FILE_HEADER_SIZE, end = EOF
    let header_end = FILE_HEADER_SIZE as u64;
    let (start_offset, file_length) = if header.kv_block_offset > 0 && header.kv_block_length > 0 {
      if header.kv_block_offset == header_end {
        // Standard: KV at head, WAL after
        let start = header.kv_block_offset + header.kv_block_length;
        let end = if header.hot_tail_offset > start { header.hot_tail_offset } else { file.seek(SeekFrom::End(0))? };
        (start, end)
      } else {
        // Legacy repair: KV placed after WAL
        let end = header.kv_block_offset; // WAL ends where KV block starts
        (header_end, end)
      }
    } else {
      // No KV block at all
      let end = if header.hot_tail_offset > 0 { header.hot_tail_offset } else { file.seek(SeekFrom::End(0))? };
      (header_end, end)
    };
    file.seek(SeekFrom::Start(start_offset))?;

    Ok(EntryScanner {
      file,
      current_offset: start_offset,
      file_length,
      report_errors,
      last_skipped_region: None,
      skipped_regions: Vec::new(),
    })
  }

  /// Scan forward from `start` looking for the 4-byte entry magic (0x0AE012DB LE).
  /// Caps the search at 1MB to avoid scanning the entire file.
  /// Returns Some((offset, bytes_skipped)) if found, None if not.
  fn scan_for_next_magic(&mut self, start: u64) -> Option<(u64, u64)> {
    use crate::engine::entry_header::ENTRY_MAGIC;
    let magic_bytes = ENTRY_MAGIC.to_le_bytes();
    let max_scan = 1_048_576u64; // 1MB search window
    let end = (start + max_scan).min(self.file_length);

    // Read the search window into memory
    let window_size = (end - start) as usize;
    if window_size < 4 {
      return None;
    }

    if self.file.seek(SeekFrom::Start(start)).is_err() {
      return None;
    }

    let mut buffer = vec![0u8; window_size];
    if let Err(_) = self.file.read_exact(&mut buffer) {
      // Partial read — truncate to what we actually have
      if self.file.seek(SeekFrom::Start(start)).is_err() {
        return None;
      }
      let actual = self.file.read(&mut buffer).unwrap_or(0);
      buffer.truncate(actual);
    }

    // Search for magic bytes
    for i in 0..buffer.len().saturating_sub(3) {
      if buffer[i..i + 4] == magic_bytes {
        let candidate_offset = start + i as u64;

        // Validate: try to deserialize a header at this offset
        if self.file.seek(SeekFrom::Start(candidate_offset)).is_ok() {
          if let Ok(header) = EntryHeader::deserialize(&mut self.file) {
            // Sanity check: total_length should be reasonable
            let remaining = self.file_length - candidate_offset;
            if (header.total_length as u64) <= remaining && header.total_length > 0 {
              return Some((candidate_offset, candidate_offset - start + 1));
            }
          }
        }
      }
    }

    None
  }

  fn record_skipped_region(&mut self, offset: u64, length: usize) {
    self.last_skipped_region = Some((offset, length));
    self.skipped_regions.push((offset, length));
  }
}

impl Iterator for EntryScanner {
  type Item = EngineResult<ScannedEntry>;

  fn next(&mut self) -> Option<Self::Item> {
    loop {
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
        Err(EngineError::UnexpectedEof) => return None,
        Err(error) => {
          // Corrupt entry header — can't use total_length to skip.
          // Scan forward looking for the next valid entry magic bytes.
          tracing::warn!(
            "Corrupt entry header at offset {}: {}. Scanning for next valid entry...",
            entry_offset,
            error
          );

          match self.scan_for_next_magic(entry_offset + 1) {
            Some((next_offset, skipped_bytes)) => {
              tracing::warn!(
                "Found next valid entry at offset {} (skipped {} bytes from {})",
                next_offset, skipped_bytes, entry_offset,
              );
              self.record_skipped_region(entry_offset, skipped_bytes as usize);
              self.current_offset = next_offset;

              if self.report_errors {
                return Some(Err(EngineError::CorruptEntry {
                  offset: entry_offset,
                  reason: format!("Corrupt header: {}", error),
                }));
              }
              continue;
            }
            None => {
              tracing::warn!(
                "No valid entry found after offset {}. Stopping scan.",
                entry_offset,
              );
              self.record_skipped_region(entry_offset, (self.file_length - entry_offset) as usize);
              return None;
            }
          }
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

        if self.report_errors {
          return Some(Err(EngineError::CorruptEntry {
            offset: entry_offset,
            reason: format!(
              "Corrupt header: key_length ({}) + value_length ({}) exceeds total_length ({})",
              header.key_length, header.value_length, header.total_length,
            ),
          }));
        }
        continue;
      }

      // Read key
      let mut key = vec![0u8; header.key_length as usize];
      if let Err(error) = self.file.read_exact(&mut key) {
        tracing::warn!(
          "IO error reading key at offset {}: {}. Skipping entry.",
          entry_offset, error
        );
        self.record_skipped_region(entry_offset, header.total_length as usize);
        self.current_offset = entry_offset + header.total_length as u64;

        if self.report_errors {
          return Some(Err(EngineError::CorruptEntry {
            offset: entry_offset,
            reason: format!("IO error reading key: {}", error),
          }));
        }
        continue;
      }

      // Read value
      let mut value = vec![0u8; header.value_length as usize];
      if let Err(error) = self.file.read_exact(&mut value) {
        tracing::warn!(
          "IO error reading value at offset {}: {}. Skipping entry.",
          entry_offset, error
        );
        self.record_skipped_region(entry_offset, header.total_length as usize);
        self.current_offset = entry_offset + header.total_length as u64;

        if self.report_errors {
          return Some(Err(EngineError::CorruptEntry {
            offset: entry_offset,
            reason: format!("IO error reading value: {}", error),
          }));
        }
        continue;
      }

      // Verify hash integrity
      if !header.verify(&key, &value) {
        tracing::warn!(
          "Hash verification failed for entry at offset {}. Skipping.",
          entry_offset
        );
        self.current_offset = entry_offset + header.total_length as u64;

        if self.report_errors {
          return Some(Err(EngineError::CorruptEntry {
            offset: entry_offset,
            reason: "Hash verification failed".to_string(),
          }));
        }
        continue;
      }

      // Advance to next entry using total_length
      self.current_offset = entry_offset + header.total_length as u64;

      return Some(Ok(ScannedEntry {
        offset: entry_offset,
        header,
        key,
        value,
      }));
    }
  }
}
