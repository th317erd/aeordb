//! Bounded recovery of prior runtime and lifecycle configuration revisions.

use std::fs::File;

use crate::engine::compression::CompressionAlgorithm;
use crate::engine::config_resolver::{ConfigFallback, ConfigurationFamily, MAX_CONFIG_DOCUMENT_BYTES};
use crate::engine::directory_ops::file_path_hash;
use crate::engine::entry_header::{ENTRY_MAGIC, EntryHeader, FLAG_SYSTEM};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::file_record::FileRecord;
use crate::engine::native_durability::read_exact_at_platform;
use crate::engine::{HashAlgorithm, StorageEngine};

const HISTORY_SCAN_WINDOW_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_HISTORY_SCAN_BYTES: u64 = 1024 * 1024 * 1024;
pub(crate) const MAX_HISTORY_CANDIDATES: usize = 32;
const MAX_CONFIG_FILE_RECORD_BYTES: u32 = 64 * 1024;

#[derive(Debug)]
pub(crate) struct ConfigurationHistoryRecord {
  offset: u64,
  header: EntryHeader,
  value: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct ConfigurationHistoryScan {
  records: Vec<ConfigurationHistoryRecord>,
  wal_start: u64,
  wal_end: u64,
  scan_start: u64,
  candidate_limit_reached: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ConfigurationHistoryLoad {
  pub candidates: Vec<ConfigFallback>,
  pub issues: Vec<String>,
}

pub(crate) fn load_configuration_history(engine: &StorageEngine, family: ConfigurationFamily) -> ConfigurationHistoryLoad {
  load_configuration_history_with_limits(engine, family, MAX_HISTORY_SCAN_BYTES, MAX_HISTORY_CANDIDATES)
}

pub(crate) fn load_configuration_history_with_limits(
  engine: &StorageEngine,
  family: ConfigurationFamily,
  maximum_scan_bytes: u64,
  maximum_candidates: usize,
) -> ConfigurationHistoryLoad {
  let scan = (|| -> EngineResult<ConfigurationHistoryScan> {
    let writer = engine.writer_read_lock()?;
    let header = writer.file_header();
    let wal_start = header
      .kv_block_offset
      .checked_add(header.kv_block_length)
      .ok_or_else(|| EngineError::InvalidInput("configuration history WAL start overflows u64".to_string()))?;
    let wal_end = writer.current_offset();
    let file = File::open(writer.file_path())?;
    drop(writer);
    scan_configuration_history_records(&file, wal_start, wal_end, engine.hash_algo(), family.path(), maximum_scan_bytes, maximum_candidates)
  })();

  let scan = match scan {
    Ok(scan) => scan,
    Err(error) => {
      return ConfigurationHistoryLoad {
        candidates: Vec::new(),
        issues: vec![format!("{} append-history scan failed: {error}", family.name())],
      };
    }
  };
  materialize_configuration_history(scan, family, engine.hash_algo(), |chunk_hash, maximum_decoded_length| {
    engine.read_chunk_verified_including_deleted_bounded(chunk_hash, maximum_decoded_length)
  })
}

pub(crate) fn scan_configuration_history_records(
  file: &File,
  wal_start: u64,
  wal_end: u64,
  hash_algorithm: HashAlgorithm,
  path: &str,
  maximum_scan_bytes: u64,
  maximum_candidates: usize,
) -> EngineResult<ConfigurationHistoryScan> {
  let file_length = file.metadata()?.len();
  if wal_start > wal_end || wal_end > file_length {
    return Err(EngineError::InvalidInput(format!(
      "configuration history WAL range {wal_start}..{wal_end} is outside file length {file_length}"
    )));
  }
  let scan_start = wal_end.saturating_sub(maximum_scan_bytes).max(wal_start);
  let target_key = file_path_hash(path, &hash_algorithm)?;
  let magic = ENTRY_MAGIC.to_le_bytes();
  let mut records = Vec::new();
  let mut logical_end = wal_end;
  let mut candidate_limit_reached = false;

  while logical_end > scan_start && records.len() < maximum_candidates {
    let window_start = logical_end.saturating_sub(HISTORY_SCAN_WINDOW_BYTES).max(scan_start);
    let read_end = logical_end.saturating_add(3).min(wal_end);
    let window_length = usize::try_from(read_end.saturating_sub(window_start))
      .map_err(|_| EngineError::ResourceExhausted("configuration history scan window exceeds platform address space".to_string()))?;
    let mut window = Vec::new();
    window
      .try_reserve_exact(window_length)
      .map_err(|error| EngineError::ResourceExhausted(format!("configuration history scan window allocation failed: {error}")))?;
    window.resize(window_length, 0);
    read_exact_at_platform(file, window_start, &mut window)?;

    if window.len() >= magic.len() {
      for index in (0..=window.len() - magic.len()).rev() {
        let offset = window_start.saturating_add(index as u64);
        if offset >= logical_end || window[index..index + magic.len()] != magic {
          continue;
        }
        if let Some(record) = read_matching_record(file, offset, wal_start, wal_end, hash_algorithm, &target_key)? {
          records.push(record);
          if records.len() == maximum_candidates {
            candidate_limit_reached = window_start > scan_start || index > 0;
            break;
          }
        }
      }
    }
    logical_end = window_start;
  }

  Ok(ConfigurationHistoryScan { records, wal_start, wal_end, scan_start, candidate_limit_reached })
}

fn read_matching_record(
  file: &File,
  offset: u64,
  wal_start: u64,
  wal_end: u64,
  hash_algorithm: HashAlgorithm,
  target_key: &[u8],
) -> EngineResult<Option<ConfigurationHistoryRecord>> {
  let fixed_end = match offset.checked_add(EntryHeader::FIXED_HEADER_SIZE as u64) {
    Some(end) if offset >= wal_start && end <= wal_end => end,
    _ => return Ok(None),
  };
  let mut fixed = [0u8; EntryHeader::FIXED_HEADER_SIZE];
  read_exact_at_platform(file, offset, &mut fixed)?;
  if fixed[..4] != ENTRY_MAGIC.to_le_bytes() {
    return Ok(None);
  }
  let encoded_hash_algorithm = u16::from_le_bytes([fixed[7], fixed[8]]);
  if HashAlgorithm::from_u16(encoded_hash_algorithm) != Some(hash_algorithm) {
    return Ok(None);
  }
  let header_length = EntryHeader::FIXED_HEADER_SIZE + hash_algorithm.hash_length();
  if offset.checked_add(header_length as u64).is_none_or(|end| end > wal_end) {
    return Ok(None);
  }
  let mut encoded_header = vec![0u8; header_length];
  encoded_header[..EntryHeader::FIXED_HEADER_SIZE].copy_from_slice(&fixed);
  read_exact_at_platform(file, fixed_end, &mut encoded_header[EntryHeader::FIXED_HEADER_SIZE..])?;
  let header = match EntryHeader::deserialize(&mut encoded_header.as_slice()) {
    Ok(header) => header,
    Err(_) => return Ok(None),
  };
  if header.entry_type != EntryType::FileRecord
    || header.flags & FLAG_SYSTEM == 0
    || header.compression_algo != CompressionAlgorithm::None
    || header.encryption_algo != 0
    || header.key_length as usize != hash_algorithm.hash_length()
    || header.value_length > MAX_CONFIG_FILE_RECORD_BYTES
  {
    return Ok(None);
  }
  let expected_total = EntryHeader::compute_total_length(hash_algorithm, header.key_length as usize, header.value_length as usize)?;
  if header.total_length != expected_total {
    return Ok(None);
  }
  let entry_end = match offset.checked_add(u64::from(expected_total)) {
    Some(end) if end <= wal_end => end,
    _ => return Ok(None),
  };
  let entry_length = usize::try_from(entry_end - offset)
    .map_err(|_| EngineError::ResourceExhausted("configuration history entry exceeds platform address space".to_string()))?;
  let mut encoded_entry = vec![0u8; entry_length];
  read_exact_at_platform(file, offset, &mut encoded_entry)?;
  let key_start = header_length;
  let key_end = key_start + header.key_length as usize;
  let value_end = key_end + header.value_length as usize;
  let key = &encoded_entry[key_start..key_end];
  let value = &encoded_entry[key_end..value_end];
  if key != target_key || !header.verify(key, value) {
    return Ok(None);
  }
  Ok(Some(ConfigurationHistoryRecord { offset, header, value: value.to_vec() }))
}

pub(crate) fn materialize_configuration_history<F>(
  scan: ConfigurationHistoryScan,
  family: ConfigurationFamily,
  hash_algorithm: HashAlgorithm,
  mut read_chunk: F,
) -> ConfigurationHistoryLoad
where
  F: FnMut(&[u8], usize) -> EngineResult<Option<Vec<u8>>>,
{
  let mut load = ConfigurationHistoryLoad::default();
  if scan.scan_start > scan.wal_start {
    load.issues.push(format!(
      "{} append-history scan inspected only the newest {} bytes of the {}-byte WAL",
      family.name(),
      scan.wal_end.saturating_sub(scan.scan_start),
      scan.wal_end.saturating_sub(scan.wal_start)
    ));
  }
  if scan.candidate_limit_reached {
    load.issues.push(format!("{} append-history scan reached its {}-candidate bound", family.name(), scan.records.len()));
  }

  let mut recovered = Vec::new();
  for historical in scan.records {
    let identity =
      format!("append-history:{}:offset={:020}:entry={}", family.name(), historical.offset, hex::encode(&historical.header.hash));
    let candidate = (|| -> EngineResult<(i64, Vec<u8>)> {
      let record = FileRecord::deserialize(&historical.value, hash_algorithm.hash_length(), historical.header.entry_version)?;
      if record.path != family.path() {
        return Err(EngineError::InvalidInput(format!("historical FileRecord path {} does not match {}", record.path, family.path())));
      }
      if record.updated_at <= 0 {
        return Err(EngineError::InvalidInput("historical FileRecord has a non-positive update time".to_string()));
      }
      let content_length = usize::try_from(record.total_size)
        .map_err(|_| EngineError::ResourceExhausted("historical configuration length exceeds platform address space".to_string()))?;
      if content_length > MAX_CONFIG_DOCUMENT_BYTES {
        return Err(EngineError::ResourceExhausted(format!(
          "historical configuration length {content_length} exceeds {MAX_CONFIG_DOCUMENT_BYTES} bytes"
        )));
      }
      let mut content = Vec::new();
      content
        .try_reserve_exact(content_length)
        .map_err(|error| EngineError::ResourceExhausted(format!("historical configuration allocation failed: {error}")))?;
      let mut content_hasher = hash_algorithm.incremental_hasher()?;
      for chunk_hash in &record.chunk_hashes {
        let remaining = content_length
          .checked_sub(content.len())
          .ok_or_else(|| EngineError::InvalidInput("historical configuration exceeded its declared length".to_string()))?;
        if remaining == 0 {
          return Err(EngineError::InvalidInput("historical configuration contains excess chunks".to_string()));
        }
        let chunk = read_chunk(chunk_hash, remaining)?
          .ok_or_else(|| EngineError::NotFound(format!("historical configuration chunk {}", hex::encode(chunk_hash))))?;
        if chunk.len() > remaining {
          return Err(EngineError::InvalidInput("historical configuration chunk exceeds its remaining length".to_string()));
        }
        content_hasher.update(&chunk);
        content.extend_from_slice(&chunk);
      }
      if content.len() != content_length {
        return Err(EngineError::InvalidInput(format!(
          "historical configuration expected {content_length} bytes but read {}",
          content.len()
        )));
      }
      if !record.content_hash.is_empty() && content_hasher.finalize() != record.content_hash {
        return Err(EngineError::InvalidInput("historical configuration whole-file hash mismatch".to_string()));
      }
      Ok((record.updated_at, content))
    })();

    match candidate {
      Ok((recorded_at_ms, bytes)) => {
        recovered.push((recorded_at_ms, historical.offset, ConfigFallback { bytes, identity, recorded_at_ms }))
      }
      Err(error) => load.issues.push(format!("{} {identity} is unusable: {error}", family.name())),
    }
  }
  recovered.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
  load.candidates = recovered.into_iter().map(|(_, _, fallback)| fallback).collect();
  load
}

#[cfg(test)]
#[path = "../../spec/engine/configuration_history_internal_spec.rs"]
mod configuration_history_internal_spec;
