use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};

const MAX_RECORDED_SKIPPED_REGIONS: usize = 1024;
const VERIFY_IO_BUFFER_BYTES: usize = 64 * 1024;
const MAX_DELETION_RECORD_BYTES: usize = 2 + u16::MAX as usize + 8 + 2 + u16::MAX as usize;

#[derive(Debug)]
pub struct ScannedEntry {
  pub offset: u64,
  pub header: EntryHeader,
  pub key: Vec<u8>,
  pub value: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct ScannedRebuildEntry {
  pub offset: u64,
  pub header: EntryHeader,
  pub key: Vec<u8>,
  pub value: Option<Vec<u8>>,
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
  skipped_region_count: u64,
  skipped_region_bytes: u64,
  cancellation: Option<Arc<AtomicBool>>,
}

impl EntryScanner {
  pub fn new(file: File) -> EngineResult<Self> {
    Self::new_internal(file, false, false)
  }

  /// Create a scanner that reports errors for corrupt entries instead of skipping.
  pub fn new_reporting(file: File) -> EngineResult<Self> {
    Self::new_internal(file, true, false)
  }

  pub(crate) fn new_reporting_to(file: File, wal_end: u64, cancellation: Option<Arc<AtomicBool>>) -> EngineResult<Self> {
    let physical_length = file.metadata()?.len();
    let mut scanner = Self::new_internal(file, true, false)?;
    if wal_end < scanner.current_offset || wal_end > physical_length {
      return Err(EngineError::InvalidInput(format!(
        "verification WAL end {wal_end} is outside {}..{physical_length}",
        scanner.current_offset
      )));
    }
    scanner.file_length = wal_end;
    scanner.cancellation = cancellation;
    Ok(scanner)
  }

  /// Construct a scanner for dirty-startup recovery: ignore the stale
  /// `header.hot_tail_offset` boundary and scan to EOF.
  ///
  /// **Why this matters.** `header.hot_tail_offset` is updated only by the
  /// 100ms flush timer. Any WAL entry written between the last header update
  /// and the crash sits PAST `hot_tail_offset` but before EOF. Using the
  /// stale offset as the scan end silently drops those entries during
  /// `rebuild_kv`. Scanning to EOF lets `scan_for_next_magic` skip any
  /// torn-write garbage at the tail and recover the real entries beyond it.
  pub fn new_dirty_recovery(file: File) -> EngineResult<Self> {
    Self::new_internal(file, true, true)
  }

  fn new_internal(mut file: File, report_errors: bool, dirty_recovery: bool) -> EngineResult<Self> {
    // Read the active header slot (v3 A/B layout).
    let (header, _slot) = crate::engine::file_header::read_active_header(&mut file)?;

    // Determine WAL scan range based on layout.
    // Standard layout: [Header A/B] [KV block] [WAL] [Hot tail]
    //   → start = kv_block_offset + kv_block_length, end = hot_tail_offset
    // Legacy layout:   [Header A/B] [WAL] [KV block] [Hot tail]
    //   → start = HEADER_REGION_SIZE, end = kv_block_offset (KV is after WAL)
    // No KV layout:    [Header A/B] [WAL]
    //   → start = HEADER_REGION_SIZE, end = EOF
    //
    // For `dirty_recovery == true`, every standard/no-KV branch falls back
    // to EOF regardless of `hot_tail_offset`; the legacy branch is unaffected
    // because the KV block (not the hot tail) bounds the WAL there.
    let header_end = crate::engine::file_header::HEADER_REGION_SIZE as u64;
    let (start_offset, file_length) = if header.kv_block_offset > 0 && header.kv_block_length > 0 {
      if header.kv_block_offset == header_end {
        // Standard: KV at head, WAL after
        let start = header.kv_block_offset + header.kv_block_length;
        let end = if dirty_recovery {
          file.seek(SeekFrom::End(0))?
        } else if header.hot_tail_offset > start {
          header.hot_tail_offset
        } else {
          file.seek(SeekFrom::End(0))?
        };
        (start, end)
      } else {
        // Legacy repair: KV placed after WAL
        let end = header.kv_block_offset; // WAL ends where KV block starts
        (header_end, end)
      }
    } else {
      // No KV block at all
      let end = if dirty_recovery {
        file.seek(SeekFrom::End(0))?
      } else if header.hot_tail_offset > 0 {
        header.hot_tail_offset
      } else {
        file.seek(SeekFrom::End(0))?
      };
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
      skipped_region_count: 0,
      skipped_region_bytes: 0,
      cancellation: None,
    })
  }

  /// Scan forward from `start` looking for the 4-byte entry magic (0x0AE012DB LE).
  /// The scan walks overlapping 1 MiB windows so memory stays bounded without
  /// discarding valid records after a large corrupt region.
  /// Returns Some((offset, bytes_skipped)) if found, None if not.
  fn scan_for_next_magic(&mut self, start: u64) -> EngineResult<Option<(u64, u64)>> {
    use crate::engine::entry_header::ENTRY_MAGIC;
    let magic_bytes = ENTRY_MAGIC.to_le_bytes();
    let mut buffer = Vec::new();
    let mut window_start = start;
    while window_start < self.file_length {
      self.check_cancelled()?;
      let window_end = window_start.saturating_add(1_048_576).min(self.file_length);
      let window_size = usize::try_from(window_end.saturating_sub(window_start))
        .map_err(|_| EngineError::ResourceExhausted("entry recovery scan window exceeds platform address space".to_string()))?;
      if window_size < 4 {
        return Ok(None);
      }

      buffer.clear();
      buffer
        .try_reserve_exact(window_size)
        .map_err(|error| EngineError::ResourceExhausted(format!("entry recovery scan allocation failed: {error}")))?;
      buffer.resize(window_size, 0);
      self.file.seek(SeekFrom::Start(window_start))?;
      self.file.read_exact(&mut buffer)?;

      for i in 0..buffer.len().saturating_sub(3) {
        if i % 4_096 == 0 {
          self.check_cancelled()?;
        }
        if buffer[i..i + 4] == magic_bytes {
          let candidate_offset = window_start.checked_add(i as u64).ok_or_else(|| EngineError::CorruptEntry {
            offset: window_start,
            reason: "entry recovery candidate offset overflow".to_string(),
          })?;

          self.file.seek(SeekFrom::Start(candidate_offset))?;
          match EntryHeader::deserialize(&mut self.file) {
            Ok(header) => {
              if Self::validated_entry_end(&header, candidate_offset, self.file_length).is_ok() {
                return Ok(Some((candidate_offset, candidate_offset.saturating_sub(start).saturating_add(1))));
              }
            }
            Err(EngineError::IoError(error)) => return Err(EngineError::IoError(error)),
            Err(_) => {}
          }
        }
      }

      if window_end == self.file_length {
        return Ok(None);
      }
      window_start = window_end.saturating_sub(3);
    }

    Ok(None)
  }

  fn validated_entry_end(header: &EntryHeader, entry_offset: u64, file_length: u64) -> EngineResult<u64> {
    let expected_total = EntryHeader::compute_total_length(header.hash_algo, header.key_length as usize, header.value_length as usize)
      .map_err(|error| EngineError::CorruptEntry { offset: entry_offset, reason: format!("invalid entry lengths: {error}") })?;
    if header.total_length != expected_total {
      return Err(EngineError::CorruptEntry {
        offset: entry_offset,
        reason: format!("entry total length {} does not match encoded length {expected_total}", header.total_length),
      });
    }
    let entry_end = entry_offset
      .checked_add(u64::from(expected_total))
      .ok_or_else(|| EngineError::CorruptEntry { offset: entry_offset, reason: "entry end offset overflow".to_string() })?;
    if entry_end > file_length {
      return Err(EngineError::CorruptEntry {
        offset: entry_offset,
        reason: format!("entry end {entry_end} exceeds WAL boundary {file_length}"),
      });
    }
    Ok(entry_end)
  }

  fn allocate_buffer(length: usize, context: &str) -> EngineResult<Vec<u8>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(length).map_err(|error| EngineError::ResourceExhausted(format!("{context} allocation failed: {error}")))?;
    buffer.resize(length, 0);
    Ok(buffer)
  }

  fn recover_after_malformed_header(&mut self, entry_offset: u64) -> EngineResult<()> {
    match self.scan_for_next_magic(entry_offset.saturating_add(1))? {
      Some((next_offset, skipped_bytes)) => {
        tracing::warn!(entry_offset, next_offset, skipped_bytes, "Recovered at the next validated entry header");
        self.record_skipped_region(entry_offset, skipped_bytes as usize);
        self.current_offset = next_offset;
      }
      None => {
        let skipped = self.file_length.saturating_sub(entry_offset);
        self.record_skipped_region(entry_offset, skipped as usize);
        self.current_offset = self.file_length;
      }
    }
    Ok(())
  }

  fn check_cancelled(&self) -> EngineResult<()> {
    if self.cancellation.as_ref().is_some_and(|cancelled| cancelled.load(AtomicOrdering::Acquire)) {
      return Err(EngineError::ShuttingDown);
    }
    Ok(())
  }

  fn record_skipped_region(&mut self, offset: u64, length: usize) {
    self.last_skipped_region = Some((offset, length));
    self.skipped_region_count = self.skipped_region_count.saturating_add(1);
    self.skipped_region_bytes = self.skipped_region_bytes.saturating_add(length as u64);
    if self.skipped_regions.len() < MAX_RECORDED_SKIPPED_REGIONS {
      self.skipped_regions.push((offset, length));
    }
  }

  pub fn skipped_region_count(&self) -> u64 {
    self.skipped_region_count
  }

  pub fn skipped_region_bytes(&self) -> u64 {
    self.skipped_region_bytes
  }

  pub fn current_offset(&self) -> u64 {
    self.current_offset
  }

  pub fn file_length(&self) -> u64 {
    self.file_length
  }

  /// Scan one entry for KV rebuild. Chunk and void payloads are skipped
  /// because rebuild only needs the key/header metadata for those large
  /// records. Mutable metadata records are still read and hash-verified.
  pub(crate) fn next_rebuild_entry(&mut self) -> Option<EngineResult<ScannedRebuildEntry>> {
    self.next_bounded_entry(false)
  }

  /// Scan one entry for verification without materializing its value. Every
  /// payload is hash-verified through a fixed-size buffer; only the bounded
  /// deletion-record value is retained because KV resolution needs its path.
  pub(crate) fn next_verify_entry(&mut self) -> Option<EngineResult<ScannedRebuildEntry>> {
    self.next_bounded_entry(true)
  }

  fn next_bounded_entry(&mut self, verify_large_payloads: bool) -> Option<EngineResult<ScannedRebuildEntry>> {
    loop {
      if let Err(error) = self.check_cancelled() {
        return Some(Err(error));
      }
      if self.current_offset >= self.file_length {
        return None;
      }

      let entry_offset = self.current_offset;
      if let Err(error) = self.file.seek(SeekFrom::Start(entry_offset)) {
        return Some(Err(error.into()));
      }

      let header = match EntryHeader::deserialize(&mut self.file) {
        Ok(header) => header,
        Err(EngineError::UnexpectedEof) => {
          let length = self.file_length.saturating_sub(entry_offset) as usize;
          self.record_skipped_region(entry_offset, length);
          self.current_offset = self.file_length;
          if self.report_errors {
            return Some(Err(EngineError::CorruptEntry { offset: entry_offset, reason: "truncated entry header".to_string() }));
          }
          return None;
        }
        Err(EngineError::IoError(error)) => return Some(Err(EngineError::IoError(error))),
        Err(error) => {
          tracing::warn!("Corrupt entry header at offset {}: {}. Scanning for next valid entry...", entry_offset, error);
          if let Err(recovery_error) = self.recover_after_malformed_header(entry_offset) {
            return Some(Err(recovery_error));
          }
          if self.report_errors {
            return Some(Err(EngineError::CorruptEntry { offset: entry_offset, reason: format!("Corrupt header: {}", error) }));
          }
          continue;
        }
      };

      let entry_end = match Self::validated_entry_end(&header, entry_offset, self.file_length) {
        Ok(entry_end) => entry_end,
        Err(error) => {
          if let Err(recovery_error) = self.recover_after_malformed_header(entry_offset) {
            return Some(Err(recovery_error));
          }
          if self.report_errors {
            return Some(Err(error));
          }
          continue;
        }
      };

      let expected_key_length = if header.entry_type == EntryType::Void { 0 } else { header.hash_algo.hash_length() };
      if header.key_length as usize != expected_key_length {
        let error = EngineError::CorruptEntry {
          offset: entry_offset,
          reason: format!(
            "database entry key length {} does not match the expected length {} for {:?}",
            header.key_length, expected_key_length, header.entry_type
          ),
        };
        if let Err(recovery_error) = self.recover_after_malformed_header(entry_offset) {
          return Some(Err(recovery_error));
        }
        if self.report_errors {
          return Some(Err(error));
        }
        continue;
      }

      let mut key = match Self::allocate_buffer(header.key_length as usize, "entry key scan") {
        Ok(key) => key,
        Err(error) => return Some(Err(error)),
      };
      if let Err(error) = self.file.read_exact(&mut key) {
        if error.kind() != std::io::ErrorKind::UnexpectedEof {
          return Some(Err(error.into()));
        }
        tracing::warn!("IO error reading key at offset {}: {}. Skipping entry.", entry_offset, error);
        self.record_skipped_region(entry_offset, header.total_length as usize);
        self.current_offset = entry_end;
        if self.report_errors {
          return Some(Err(EngineError::CorruptEntry { offset: entry_offset, reason: format!("IO error reading key: {}", error) }));
        }
        continue;
      }

      let should_verify = verify_large_payloads || !matches!(header.entry_type, EntryType::Chunk | EntryType::Void);
      if !should_verify {
        if let Err(error) = self.file.seek(SeekFrom::Start(entry_end)) {
          return Some(Err(error.into()));
        }
        self.current_offset = entry_end;
        return Some(Ok(ScannedRebuildEntry { offset: entry_offset, header, key, value: None }));
      }

      let retain_value = header.entry_type == EntryType::DeletionRecord;
      if retain_value && header.value_length as usize > MAX_DELETION_RECORD_BYTES {
        self.current_offset = entry_end;
        return Some(Err(EngineError::CorruptEntry {
          offset: entry_offset,
          reason: format!("deletion record value length {} exceeds format maximum {}", header.value_length, MAX_DELETION_RECORD_BYTES),
        }));
      }
      let mut retained = if retain_value {
        let mut value = Vec::new();
        if let Err(error) = value.try_reserve_exact(header.value_length as usize) {
          self.current_offset = entry_end;
          return Some(Err(EngineError::ResourceExhausted(format!("verification deletion-record allocation failed: {error}"))));
        }
        Some(value)
      } else {
        None
      };
      let mut hasher = match header.hash_algo.incremental_hasher() {
        Ok(hasher) => hasher,
        Err(error) => return Some(Err(error)),
      };
      hasher.update(&[header.entry_type.to_u8()]);
      hasher.update(&key);
      let mut remaining = header.value_length as usize;
      let mut buffer = [0u8; VERIFY_IO_BUFFER_BYTES];
      while remaining > 0 {
        if let Err(error) = self.check_cancelled() {
          return Some(Err(error));
        }
        let width = remaining.min(buffer.len());
        if let Err(error) = self.file.read_exact(&mut buffer[..width]) {
          if error.kind() != std::io::ErrorKind::UnexpectedEof {
            return Some(Err(error.into()));
          }
          self.record_skipped_region(entry_offset, header.total_length as usize);
          self.current_offset = entry_end;
          return Some(Err(EngineError::CorruptEntry { offset: entry_offset, reason: format!("IO error reading value: {error}") }));
        }
        hasher.update(&buffer[..width]);
        if let Some(value) = retained.as_mut() {
          value.extend_from_slice(&buffer[..width]);
        }
        remaining -= width;
      }
      if hasher.finalize() != header.hash {
        self.current_offset = entry_end;
        if self.report_errors {
          return Some(Err(EngineError::CorruptEntry { offset: entry_offset, reason: "Hash verification failed".to_string() }));
        }
        continue;
      }

      self.current_offset = entry_end;
      return Some(Ok(ScannedRebuildEntry { offset: entry_offset, header, key, value: retained }));
    }
  }
}

impl Iterator for EntryScanner {
  type Item = EngineResult<ScannedEntry>;

  fn next(&mut self) -> Option<Self::Item> {
    loop {
      if let Err(error) = self.check_cancelled() {
        return Some(Err(error));
      }
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
        Err(EngineError::UnexpectedEof) => {
          let length = self.file_length.saturating_sub(entry_offset) as usize;
          self.record_skipped_region(entry_offset, length);
          self.current_offset = self.file_length;
          if self.report_errors {
            return Some(Err(EngineError::CorruptEntry { offset: entry_offset, reason: "truncated entry header".to_string() }));
          }
          return None;
        }
        Err(EngineError::IoError(error)) => return Some(Err(EngineError::IoError(error))),
        Err(error) => {
          // Corrupt entry header — can't use total_length to skip.
          // Scan forward looking for the next valid entry magic bytes.
          tracing::warn!("Corrupt entry header at offset {}: {}. Scanning for next valid entry...", entry_offset, error);
          if let Err(recovery_error) = self.recover_after_malformed_header(entry_offset) {
            return Some(Err(recovery_error));
          }
          if self.report_errors {
            return Some(Err(EngineError::CorruptEntry { offset: entry_offset, reason: format!("Corrupt header: {}", error) }));
          }
          continue;
        }
      };

      let entry_end = match Self::validated_entry_end(&header, entry_offset, self.file_length) {
        Ok(entry_end) => entry_end,
        Err(error) => {
          if let Err(recovery_error) = self.recover_after_malformed_header(entry_offset) {
            return Some(Err(recovery_error));
          }
          if self.report_errors {
            return Some(Err(error));
          }
          continue;
        }
      };

      // Read key
      let mut key = match Self::allocate_buffer(header.key_length as usize, "entry key scan") {
        Ok(key) => key,
        Err(error) => return Some(Err(error)),
      };
      if let Err(error) = self.file.read_exact(&mut key) {
        tracing::warn!("IO error reading key at offset {}: {}. Skipping entry.", entry_offset, error);
        self.record_skipped_region(entry_offset, header.total_length as usize);
        self.current_offset = entry_end;

        if self.report_errors {
          return Some(Err(EngineError::CorruptEntry { offset: entry_offset, reason: format!("IO error reading key: {}", error) }));
        }
        continue;
      }

      // Read value
      let mut value = match Self::allocate_buffer(header.value_length as usize, "entry value scan") {
        Ok(value) => value,
        Err(error) => return Some(Err(error)),
      };
      if let Err(error) = self.file.read_exact(&mut value) {
        tracing::warn!("IO error reading value at offset {}: {}. Skipping entry.", entry_offset, error);
        self.record_skipped_region(entry_offset, header.total_length as usize);
        self.current_offset = entry_end;

        if self.report_errors {
          return Some(Err(EngineError::CorruptEntry { offset: entry_offset, reason: format!("IO error reading value: {}", error) }));
        }
        continue;
      }

      // Verify hash integrity
      if !header.verify(&key, &value) {
        tracing::warn!("Hash verification failed for entry at offset {}. Skipping.", entry_offset);
        self.current_offset = entry_end;

        if self.report_errors {
          return Some(Err(EngineError::CorruptEntry { offset: entry_offset, reason: "Hash verification failed".to_string() }));
        }
        continue;
      }

      // Advance to next entry using total_length
      self.current_offset = entry_end;

      return Some(Ok(ScannedEntry { offset: entry_offset, header, key, value }));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::engine::append_writer::AppendWriter;

  #[test]
  fn bounded_verification_scan_honors_preexisting_cancellation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("cancelled-scan.aeordb");
    let mut writer = AppendWriter::create(&path).unwrap();
    let key = [0x5a; 32];
    writer.append_entry(EntryType::Chunk, &key, b"payload", 0).unwrap();
    let cancellation = Arc::new(AtomicBool::new(true));
    let mut scanner = EntryScanner::new_reporting_to(File::open(path).unwrap(), writer.current_offset(), Some(cancellation)).unwrap();

    assert!(matches!(scanner.next_verify_entry(), Some(Err(EngineError::ShuttingDown))));
  }

  #[test]
  fn scanners_recover_after_a_header_with_mismatched_total_length() {
    use std::io::{Seek, SeekFrom, Write};

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mismatched-total.aeordb");
    let mut writer = AppendWriter::create(&path).unwrap();
    let first_key = [0x11; 32];
    let second_key = [0x22; 32];
    let (first_offset, first_length) = writer.append_entry(EntryType::Chunk, &first_key, b"first", 0).unwrap();
    writer.set_offset(writer.current_offset() + 1_048_576 + 4_096);
    writer.append_entry(EntryType::Chunk, &second_key, b"second", 0).unwrap();
    let wal_end = writer.current_offset();

    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(first_offset + 27)).unwrap();
    file.write_all(&first_length.saturating_add(7).to_le_bytes()).unwrap();
    crate::engine::native_durability::sync_file_data_native(&file).unwrap();
    drop(file);

    let mut scanner = EntryScanner::new_reporting_to(File::open(&path).unwrap(), wal_end, None).unwrap();
    assert!(matches!(scanner.next_verify_entry(), Some(Err(EngineError::CorruptEntry { .. }))));
    let recovered = scanner.next_verify_entry().unwrap().unwrap();
    assert_eq!(recovered.key, second_key);
    assert!(scanner.next_verify_entry().is_none());

    let mut buffered = EntryScanner::new_reporting_to(File::open(&path).unwrap(), wal_end, None).unwrap();
    assert!(matches!(buffered.next(), Some(Err(EngineError::CorruptEntry { .. }))));
    let recovered = buffered.next().unwrap().unwrap();
    assert_eq!(recovered.key, second_key);
    assert!(buffered.next().is_none());
  }

  #[test]
  fn skipped_region_diagnostics_saturate_without_losing_totals() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bounded-skipped-regions.aeordb");
    AppendWriter::create(&path).unwrap();
    let mut scanner = EntryScanner::new_reporting(File::open(path).unwrap()).unwrap();
    let observed = MAX_RECORDED_SKIPPED_REGIONS + 17;

    for index in 0..observed {
      scanner.record_skipped_region(index as u64 * 11, 11);
    }

    assert_eq!(scanner.skipped_regions.len(), MAX_RECORDED_SKIPPED_REGIONS);
    assert_eq!(scanner.skipped_region_count(), observed as u64);
    assert_eq!(scanner.skipped_region_bytes(), observed as u64 * 11);
    assert_eq!(scanner.last_skipped_region, Some(((observed as u64 - 1) * 11, 11)));
  }
}
