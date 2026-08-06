//! Low-level header repair for databases that won't open cleanly.
//!
//! The normal [`StorageEngine::open`] path fails fast on:
//!   - unknown header version (we expect v3; v1 and v2 files refused)
//!   - header CRC mismatch (torn write)
//!   - hot_tail_offset past EOF (the fsync-ordering corruption that hit
//!     xenocept on 2026-05-11)
//!
//! For real-world recovery we need to make the file openable WITHOUT trusting
//! every header field. This module reads the raw header bytes, identifies
//! recoverable conditions, applies the minimal fixes needed to let
//! `StorageEngine::open` proceed (which then triggers dirty startup), and
//! writes a new v3 header back to disk.
//!
//! For v1 and v2 files, the data region also needs to shift forward 256 bytes
//! to make room for slot B. `repair_header_in_place` handles that shift
//! transparently.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

use crate::engine::durability_coordinator::{DurabilityCoordinator, NativeFileBarrierKind};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_header::{write_initial_header_coordinated, FileHeader, FILE_HEADER_SIZE, FILE_MAGIC, SUPPORTED_HEADER_VERSION};
use crate::engine::hash_algorithm::HashAlgorithm;

/// Pre-open legacy-header repair cannot use the runtime memory coordinator.
/// Keep its only material allocation fixed and small so repairing a large
/// database does not scale resident memory with database size.
const HEADER_REPAIR_COPY_BUFFER_BYTES: usize = 256 * 1024;

/// What the inspection found wrong with the header.
#[derive(Debug, Clone, Default)]
pub struct HeaderRepairReport {
  /// The header was already valid for this build — no repair needed.
  pub already_ok: bool,
  /// File magic was wrong (not an AeorDB file).
  pub bad_magic: bool,
  /// Header version was older than this build supports.
  pub upgraded_version: Option<(u8, u8)>,
  /// hot_tail_offset pointed past the actual file size and was reset.
  pub hot_tail_past_eof: Option<HotTailMismatch>,
  /// Header CRC didn't match (v2 only).
  pub crc_failed: bool,
  /// The new header was written and fsynced.
  pub repaired: bool,
}

#[derive(Debug, Clone)]
pub struct HotTailMismatch {
  pub recorded_offset: u64,
  pub actual_file_size: u64,
  pub bytes_past_eof: u64,
}

impl HeaderRepairReport {
  pub fn needs_repair(&self) -> bool {
    self.upgraded_version.is_some() || self.hot_tail_past_eof.is_some() || self.crc_failed
  }
}

/// Inspect the header at `db_path` without going through `StorageEngine::open`.
/// Returns a description of what's wrong without modifying anything.
pub fn inspect_header(db_path: &str) -> EngineResult<HeaderRepairReport> {
  let mut report = HeaderRepairReport::default();

  let mut file = OpenOptions::new().read(true).open(db_path)?;
  let file_size = file.metadata()?.len();

  let mut bytes = [0u8; FILE_HEADER_SIZE];
  if let Err(error) = file.read_exact(&mut bytes) {
    return Err(EngineError::IoError(error));
  }

  if &bytes[0..4] != FILE_MAGIC {
    report.bad_magic = true;
    return Ok(report);
  }

  let header_version = bytes[4];
  if header_version != SUPPORTED_HEADER_VERSION {
    report.upgraded_version = Some((header_version, SUPPORTED_HEADER_VERSION));
  }

  // For v2+ check CRC; for v1 there's no CRC to check.
  if header_version >= 2 {
    let stored = u32::from_le_bytes(bytes[FILE_HEADER_SIZE - 4..].try_into().unwrap());
    let computed = crc32fast::hash(&bytes[..FILE_HEADER_SIZE - 4]);
    if stored != computed {
      report.crc_failed = true;
    }
  }

  // Parse hot_tail_offset directly. v1/v2 has it at bytes 114..122. v3
  // inserts a u64 sequence at byte 7, shifting hot_tail_offset to 122..130.
  let hot_tail_pos = if header_version >= 3 { 122 } else { 114 };
  let hot_tail_offset = u64::from_le_bytes(bytes[hot_tail_pos..hot_tail_pos + 8].try_into().unwrap());
  if hot_tail_offset > file_size {
    report.hot_tail_past_eof =
      Some(HotTailMismatch { recorded_offset: hot_tail_offset, actual_file_size: file_size, bytes_past_eof: hot_tail_offset - file_size });
  }

  if !report.needs_repair() && !report.bad_magic {
    report.already_ok = true;
  }

  Ok(report)
}

/// Repair the header in place. Reads it as v1/v2, applies fixes, writes a
/// fresh v2 header back to disk with the safe fsync ordering. After this
/// returns successfully, `StorageEngine::open` on the same path will succeed
/// (triggering dirty startup to rebuild the KV from a full WAL scan).
///
/// Caller MUST close any open handle to the file before invoking this.
/// Returns `(report, repaired_header)` so the caller can decide what to do
/// next (e.g. trigger dirty startup, rebuild_kv, etc.).
pub fn repair_header_in_place(db_path: &str) -> EngineResult<HeaderRepairReport> {
  let mut report = inspect_header(db_path)?;

  if report.already_ok {
    return Ok(report);
  }
  if report.bad_magic {
    return Err(EngineError::InvalidMagic);
  }

  let mut file = OpenOptions::new().read(true).write(true).open(db_path)?;
  let file_size = file.metadata()?.len();

  let mut bytes = [0u8; FILE_HEADER_SIZE];
  file.seek(SeekFrom::Start(0))?;
  file.read_exact(&mut bytes)?;

  // Parse the fields we trust into a fresh FileHeader. v1 and v2 share the
  // same field layout from byte 7 onwards (v2 adds a tail CRC). v3 inserts
  // a u64 sequence at byte 7, shifting every subsequent field by 8.
  let source_version = bytes[4];
  let needs_data_shift = source_version <= 2;
  let sequence_size: usize = if source_version >= 3 { 8 } else { 0 };

  let hash_algo_raw = u16::from_le_bytes([bytes[5], bytes[6]]);
  let hash_algo = HashAlgorithm::from_u16(hash_algo_raw).ok_or(EngineError::InvalidHashAlgorithm(hash_algo_raw))?;
  let hash_length = hash_algo.hash_length();

  let mut pos = 7 + sequence_size; // skip sequence in v3
  let created_at = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
  pos += 8;
  let updated_at = i64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
  pos += 8;
  let kv_block_offset = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
  pos += 8;
  let kv_block_length = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
  pos += 8;
  let kv_block_version = bytes[pos];
  pos += 1;
  let nvt_offset = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
  pos += 8;
  let nvt_length = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
  pos += 8;
  let nvt_version = bytes[pos];
  pos += 1;

  let head_hash = bytes[pos..pos + hash_length].to_vec();
  pos += hash_length;

  let entry_count = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
  pos += 8;
  let resize_in_progress = bytes[pos] != 0;
  pos += 1;
  let buffer_kvs_offset = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
  pos += 8;
  let buffer_nvt_offset = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
  pos += 8;
  let mut hot_tail_offset = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
  pos += 8;
  let kv_block_stage = bytes[pos];
  pos += 1;
  let resize_target_stage = bytes[pos];
  pos += 1;
  let backup_type = bytes[pos];
  pos += 1;

  let base_hash = bytes[pos..pos + hash_length].to_vec();
  pos += hash_length;
  let target_hash = bytes[pos..pos + hash_length].to_vec();

  // For v1 and v2 → v3 migration, the data region needs to shift forward
  // 256 bytes (one slot's worth) to make room for slot B. Bump every
  // header-stored offset that refers to a location inside the data region.
  let mut kv_block_offset = kv_block_offset;
  let mut nvt_offset = nvt_offset;
  let mut buffer_kvs_offset = buffer_kvs_offset;
  let mut buffer_nvt_offset = buffer_nvt_offset;
  let durability_coordinator = DurabilityCoordinator::new();
  if needs_data_shift {
    kv_block_offset = if kv_block_offset > 0 { kv_block_offset + FILE_HEADER_SIZE as u64 } else { 0 };
    if hot_tail_offset > 0 {
      hot_tail_offset += FILE_HEADER_SIZE as u64;
    }
    if nvt_offset > 0 {
      nvt_offset += FILE_HEADER_SIZE as u64;
    }
    if buffer_kvs_offset > 0 {
      buffer_kvs_offset += FILE_HEADER_SIZE as u64;
    }
    if buffer_nvt_offset > 0 {
      buffer_nvt_offset += FILE_HEADER_SIZE as u64;
    }
  }

  // Apply the fix: if hot_tail_offset points past EOF, reset it to the
  // current file size. The reset uses the POST-shift file size when we're
  // also migrating versions, since the shift adds 256 bytes to the file.
  if let Some(ref mismatch) = report.hot_tail_past_eof {
    let target_size = if needs_data_shift { mismatch.actual_file_size + FILE_HEADER_SIZE as u64 } else { mismatch.actual_file_size };
    tracing::warn!(
      recorded = mismatch.recorded_offset,
      file_size = mismatch.actual_file_size,
      past_eof = mismatch.bytes_past_eof,
      "hot_tail_offset is past EOF — resetting to {} to trigger dirty startup",
      target_size,
    );
    hot_tail_offset = target_size;
  }

  let mut new_header = FileHeader {
    header_version: SUPPORTED_HEADER_VERSION,
    hash_algo,
    sequence: 0, // write_initial_header sets to 1
    created_at,
    updated_at,
    kv_block_offset,
    kv_block_length,
    kv_block_version,
    nvt_offset,
    nvt_length,
    nvt_version,
    head_hash,
    entry_count,
    resize_in_progress,
    buffer_kvs_offset,
    buffer_nvt_offset,
    hot_tail_offset,
    kv_block_stage,
    resize_target_stage,
    backup_type,
    base_hash,
    target_hash,
  };

  // If we're migrating v1/v2 → v3, shift the data region forward by
  // FILE_HEADER_SIZE bytes (the size of one slot). Copy from the end
  // backwards to avoid overwriting source data before we've read it.
  if needs_data_shift {
    tracing::info!(
      file_size,
      source_version,
      "Migrating v{} → v3: shifting data region forward {} bytes",
      source_version,
      FILE_HEADER_SIZE,
    );
    shift_data_region_forward(&mut file, file_size, FILE_HEADER_SIZE as u64, &durability_coordinator)?;
  }

  // Write the new v3 header to slot A and zero slot B. The barrier inside
  // write_initial_header fsyncs both writes.
  write_initial_header_coordinated(&mut file, &mut new_header, &durability_coordinator)?;

  report.repaired = true;
  Ok(report)
}

/// Shift `[FILE_HEADER_SIZE..file_size]` forward by `shift` bytes. The file
/// grows by `shift`. Copy chunks back-to-front to avoid corrupting bytes we
/// haven't read yet. fsyncs at the end.
fn shift_data_region_forward(
  file: &mut std::fs::File,
  file_size: u64,
  shift: u64,
  durability_coordinator: &DurabilityCoordinator,
) -> EngineResult<()> {
  if file_size <= FILE_HEADER_SIZE as u64 {
    return Ok(()); // nothing to shift
  }
  let data_start = FILE_HEADER_SIZE as u64;
  let data_size = file_size - data_start;

  let buffer_length = usize::try_from((HEADER_REPAIR_COPY_BUFFER_BYTES as u64).min(data_size))
    .map_err(|_| EngineError::ResourceExhausted("header repair copy buffer exceeds this platform".to_string()))?;
  let mut buffer = Vec::new();
  buffer
    .try_reserve_exact(buffer_length)
    .map_err(|error| EngineError::ResourceExhausted(format!("header repair copy buffer allocation failed: {error}")))?;
  buffer.resize(buffer_length, 0);

  copy_data_region_forward(file, file_size, shift, &mut buffer)?;
  durability_coordinator
    .execute_recoverable_file_barrier(file, NativeFileBarrierKind::Full, data_size)
    .map_err(|error| EngineError::DurabilityFailure(error.to_string()))?;
  Ok(())
}

fn copy_data_region_forward<T: Read + Seek + Write>(file: &mut T, file_size: u64, shift: u64, buffer: &mut [u8]) -> EngineResult<()> {
  if file_size <= FILE_HEADER_SIZE as u64 {
    return Ok(());
  }
  if buffer.is_empty() {
    return Err(EngineError::InvalidInput("header repair copy buffer must not be empty".to_string()));
  }

  let data_start = FILE_HEADER_SIZE as u64;
  let mut remaining = file_size - data_start;
  while remaining > 0 {
    let chunk_len = usize::try_from((buffer.len() as u64).min(remaining))
      .map_err(|_| EngineError::ResourceExhausted("header repair copy window exceeds this platform".to_string()))?;
    let src = data_start + remaining - chunk_len as u64;
    let dst = src.checked_add(shift).ok_or_else(|| EngineError::InvalidInput("header repair destination offset overflow".to_string()))?;
    file.seek(SeekFrom::Start(src))?;
    file.read_exact(&mut buffer[..chunk_len])?;
    file.seek(SeekFrom::Start(dst))?;
    file.write_all(&buffer[..chunk_len])?;
    remaining -= chunk_len as u64;
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::io::Cursor;
  use std::sync::Arc;

  use super::*;
  use crate::engine::durability_coordinator::RecoverableFileBarrier;
  use crate::engine::native_durability::{NativeDurabilityError, NativeDurabilityOperation};

  struct FaultingIo {
    cursor: Cursor<Vec<u8>>,
    fail_read: bool,
    fail_write: bool,
  }

  impl Read for FaultingIo {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
      if self.fail_read {
        Err(std::io::Error::other("injected header-repair read failure"))
      } else {
        self.cursor.read(buffer)
      }
    }
  }

  impl Write for FaultingIo {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
      if self.fail_write {
        Err(std::io::Error::other("injected header-repair write failure"))
      } else {
        self.cursor.write(buffer)
      }
    }

    fn flush(&mut self) -> std::io::Result<()> {
      Ok(())
    }
  }

  impl Seek for FaultingIo {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
      self.cursor.seek(position)
    }
  }

  #[derive(Debug)]
  struct FailingBarrier;

  impl RecoverableFileBarrier for FailingBarrier {
    fn execute_file_barrier(&self, _file: &std::fs::File, _barrier_kind: NativeFileBarrierKind) -> Result<(), NativeDurabilityError> {
      Err(NativeDurabilityError::from_io(
        NativeDurabilityOperation::FileBarrier,
        std::io::Error::other("injected header-repair barrier failure"),
      ))
    }
  }

  #[test]
  fn legacy_copy_buffer_is_fixed_and_small() {
    assert_eq!(HEADER_REPAIR_COPY_BUFFER_BYTES, 256 * 1024);
  }

  #[test]
  fn legacy_copy_propagates_read_failure() {
    let mut io = FaultingIo { cursor: Cursor::new(vec![0u8; FILE_HEADER_SIZE * 2]), fail_read: true, fail_write: false };
    let mut buffer = vec![0u8; 64];
    let error = copy_data_region_forward(&mut io, (FILE_HEADER_SIZE * 2) as u64, FILE_HEADER_SIZE as u64, &mut buffer).unwrap_err();
    assert!(matches!(error, EngineError::IoError(_)));
  }

  #[test]
  fn legacy_copy_propagates_write_failure() {
    let mut io = FaultingIo { cursor: Cursor::new(vec![0u8; FILE_HEADER_SIZE * 2]), fail_read: false, fail_write: true };
    let mut buffer = vec![0u8; 64];
    let error = copy_data_region_forward(&mut io, (FILE_HEADER_SIZE * 2) as u64, FILE_HEADER_SIZE as u64, &mut buffer).unwrap_err();
    assert!(matches!(error, EngineError::IoError(_)));
  }

  #[test]
  fn legacy_copy_rejects_empty_buffer_and_offset_overflow() {
    let mut io = FaultingIo { cursor: Cursor::new(vec![0u8; FILE_HEADER_SIZE * 2]), fail_read: false, fail_write: false };
    let error = copy_data_region_forward(&mut io, (FILE_HEADER_SIZE * 2) as u64, FILE_HEADER_SIZE as u64, &mut []).unwrap_err();
    assert!(matches!(error, EngineError::InvalidInput(message) if message.contains("must not be empty")));

    let mut buffer = vec![0u8; 64];
    let error = copy_data_region_forward(&mut io, (FILE_HEADER_SIZE * 2) as u64, u64::MAX, &mut buffer).unwrap_err();
    assert!(matches!(error, EngineError::InvalidInput(message) if message.contains("offset overflow")));
  }

  #[test]
  fn legacy_shift_propagates_barrier_failure() {
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(&vec![0x5au8; FILE_HEADER_SIZE * 2]).unwrap();
    let coordinator = DurabilityCoordinator::with_recoverable_file_barrier(Arc::new(FailingBarrier));
    let error = shift_data_region_forward(&mut file, (FILE_HEADER_SIZE * 2) as u64, FILE_HEADER_SIZE as u64, &coordinator).unwrap_err();
    assert!(matches!(error, EngineError::DurabilityFailure(_)));
  }
}
