use std::collections::HashMap;

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::nvt::NormalizedVectorTable;
use crate::engine::request_context::RequestContext;
use crate::engine::scalar_converter::{deserialize_converter, serialize_converter, ScalarConverter};
use crate::engine::storage_engine::StorageEngine;

/// Default number of NVT buckets for a new FieldIndex.
const DEFAULT_NVT_BUCKET_COUNT: usize = 1024;

/// A single entry in a field index: maps a scalar to a file's hash.
#[derive(Debug, Clone)]
pub struct IndexEntry {
  pub scalar: f64,
  pub file_hash: Vec<u8>,
}

/// A field index: converter + sorted entries + NVT bucket index.
pub struct FieldIndex {
  pub field_name: String,
  pub converter: Box<dyn ScalarConverter>,
  pub entries: Vec<IndexEntry>,
  pub nvt: NormalizedVectorTable,
  /// Raw field values keyed by file_hash. Used by fuzzy query recheck
  /// to avoid re-loading files from storage.
  ///
  /// NOTE: The values HashMap grows with every indexed file and is never
  /// pruned. At 1M files with 100-byte values, this is ~100MB per index field.
  /// Consider capping or implementing lazy loading from disk during recheck.
  pub values: HashMap<Vec<u8>, Vec<u8>>,
  dirty: bool,
}

impl FieldIndex {
  /// Create an empty index with the given converter.
  pub fn new(field_name: String, converter: Box<dyn ScalarConverter>) -> Self {
    let nvt_converter = deserialize_converter(&converter.serialize())
      .expect("converter roundtrip for NVT should never fail");
    let nvt = NormalizedVectorTable::new(nvt_converter, DEFAULT_NVT_BUCKET_COUNT);
    FieldIndex {
      field_name,
      converter,
      entries: Vec::new(),
      nvt,
      values: HashMap::new(),
      dirty: false,
    }
  }

  /// Convert value to scalar and insert in sorted position. Marks NVT dirty.
  pub fn insert(&mut self, value: &[u8], file_hash: Vec<u8>) {
    let scalar = self.converter.to_scalar(value);
    let entry = IndexEntry {
      scalar,
      file_hash,
    };
    let position = self.entries
      .binary_search_by(|probe| probe.scalar.partial_cmp(&scalar).unwrap_or(std::cmp::Ordering::Equal))
      .unwrap_or_else(|position| position);
    self.entries.insert(position, entry);
    self.dirty = true;
  }

  /// Expand a value via the converter's expand_value, then insert each expanded
  /// value as a separate index entry. For default converters this inserts one entry.
  /// For trigram/phonetic converters this inserts multiple entries.
  pub fn insert_expanded(&mut self, value: &[u8], file_hash: Vec<u8>) {
    // Store the raw value for recheck lookups
    self.values.insert(file_hash.clone(), value.to_vec());

    let expanded = self.converter.expand_value(value);
    for entry_value in expanded {
      let scalar = self.converter.to_scalar(&entry_value);
      let entry = IndexEntry {
        scalar,
        file_hash: file_hash.clone(),
      };
      let position = self.entries
        .binary_search_by(|probe| probe.scalar.partial_cmp(&scalar).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or_else(|position| position);
      self.entries.insert(position, entry);
      self.dirty = true;
    }
  }

  /// Remove all entries for a given file hash. Marks NVT dirty.
  pub fn remove(&mut self, file_hash: &[u8]) {
    self.values.remove(file_hash);
    let original_length = self.entries.len();
    self.entries.retain(|entry| entry.file_hash != file_hash);
    if self.entries.len() != original_length {
      self.dirty = true;
    }
  }

  /// Get the raw field value for a file hash (for fuzzy query recheck).
  pub fn get_value(&self, file_hash: &[u8]) -> Option<&[u8]> {
    self.values.get(file_hash).map(|v| v.as_slice())
  }

  /// Returns true if entries have changed since the last NVT rebuild.
  pub fn is_dirty(&self) -> bool {
    self.dirty
  }

  /// Ensure the NVT reflects the current entries. Called before any lookup.
  pub fn ensure_nvt_current(&mut self) {
    if !self.dirty {
      return;
    }
    self.rebuild_nvt();
  }

  /// Rebuild the NVT from the sorted entries, assigning bucket ranges.
  pub fn rebuild_nvt(&mut self) {
    let bucket_count = self.nvt.bucket_count();

    // Reset all buckets to empty.
    for bucket_index in 0..bucket_count {
      self.nvt.update_bucket(bucket_index, 0, 0);
    }

    if self.entries.is_empty() {
      self.dirty = false;
      return;
    }

    // Walk sorted entries and assign each to its NVT bucket.
    // Track the start_index and count for each bucket.
    let mut bucket_start_indices: Vec<Option<usize>> = vec![None; bucket_count];
    let mut bucket_counts: Vec<u32> = vec![0; bucket_count];

    for (entry_index, entry) in self.entries.iter().enumerate() {
      let bucket_index = self.scalar_to_bucket(entry.scalar);
      if bucket_start_indices[bucket_index].is_none() {
        bucket_start_indices[bucket_index] = Some(entry_index);
      }
      bucket_counts[bucket_index] += 1;
    }

    for bucket_index in 0..bucket_count {
      let start = bucket_start_indices[bucket_index].unwrap_or(0) as u64;
      let count = bucket_counts[bucket_index];
      self.nvt.update_bucket(bucket_index, start, count);
    }

    self.dirty = false;
  }

  /// Find entries matching the exact value.
  /// Uses NVT for bucket-level lookup, then verifies using raw byte comparison
  /// from the values map (falling back to scalar comparison for entries without
  /// stored values). This avoids false positives when many distinct values map
  /// to the same f64 scalar (e.g. small u64 values in a [0, u64::MAX] range).
  pub fn lookup_exact(&mut self, value: &[u8]) -> Vec<&IndexEntry> {
    self.ensure_nvt_current();

    let target_scalar = self.converter.to_scalar(value);
    let bucket_index = self.nvt.bucket_for_value(value);
    let bucket = self.nvt.get_bucket(bucket_index);

    if bucket.entry_count == 0 {
      return Vec::new();
    }

    let start = bucket.kv_block_offset as usize;
    let end = (start + bucket.entry_count as usize).min(self.entries.len());

    self.entries[start..end]
      .iter()
      .filter(|entry| {
        // First check: scalar must be close (bucket-level filtering)
        if (entry.scalar - target_scalar).abs() >= f64::EPSILON {
          return false;
        }
        // Second check: if we have stored raw values, verify exact byte match.
        // This prevents false positives when distinct values map to the same
        // f64 scalar (e.g. u64 values 100 and 101 with a [0, u64::MAX] range
        // both produce scalars indistinguishable at f64 precision).
        if let Some(stored_value) = self.values.get(&entry.file_hash) {
          stored_value.as_slice() == value
        } else {
          // No stored value — fall back to scalar-only match (legacy indexes
          // without the values map, or indexes where values are not stored).
          true
        }
      })
      .collect()
  }

  /// Range query: find entries with values between min and max.
  /// Uses NVT for bucket-level lookup across the range, then verifies using
  /// raw byte comparison from the values map (falling back to scalar comparison
  /// for entries without stored values). This avoids false negatives when many
  /// distinct values map to nearly identical f64 scalars (e.g. small u64 values
  /// in a [0, u64::MAX] range where scalars are indistinguishable).
  pub fn lookup_range(&mut self, min_value: &[u8], max_value: &[u8]) -> EngineResult<Vec<&IndexEntry>> {
    if !self.converter.is_order_preserving() {
      return Err(EngineError::RangeQueryNotSupported(
        self.converter.name().to_string(),
      ));
    }
    self.ensure_nvt_current();

    let min_scalar = self.converter.to_scalar(min_value);
    let max_scalar = self.converter.to_scalar(max_value);
    let start_bucket = self.scalar_to_bucket(min_scalar);
    let end_bucket = self.scalar_to_bucket(max_scalar);

    // Detect if we have stored raw values for byte-level comparison.
    // When scalars are indistinguishable (e.g. small u64 with [0, u64::MAX] range),
    // the scalar-based filter would reject valid entries. In that case, use raw
    // byte comparison which preserves ordering for big-endian numeric types.
    let has_values = !self.values.is_empty();

    let mut results = Vec::new();
    for bucket_index in start_bucket..=end_bucket {
      if bucket_index >= self.nvt.bucket_count() {
        break;
      }
      let bucket = self.nvt.get_bucket(bucket_index);
      if bucket.entry_count == 0 {
        continue;
      }
      let start = bucket.kv_block_offset as usize;
      let end = (start + bucket.entry_count as usize).min(self.entries.len());
      for entry in &self.entries[start..end] {
        if has_values {
          if let Some(stored_value) = self.values.get(&entry.file_hash) {
            // Raw byte comparison: big-endian bytes preserve numeric ordering.
            if stored_value.as_slice() >= min_value && stored_value.as_slice() <= max_value {
              results.push(entry);
            }
            continue;
          }
        }
        // Fallback: scalar comparison (for entries without stored values).
        if entry.scalar >= min_scalar && entry.scalar <= max_scalar {
          results.push(entry);
        }
      }
    }

    Ok(results)
  }

  /// Greater than query. Uses NVT to skip buckets below the threshold.
  /// When raw values are available, uses byte comparison to avoid f64 precision
  /// issues (same fix as lookup_exact and lookup_range).
  pub fn lookup_gt(&mut self, value: &[u8]) -> EngineResult<Vec<&IndexEntry>> {
    if !self.converter.is_order_preserving() {
      return Err(EngineError::RangeQueryNotSupported(
        self.converter.name().to_string(),
      ));
    }
    self.ensure_nvt_current();

    let target_scalar = self.converter.to_scalar(value);
    let start_bucket = self.scalar_to_bucket(target_scalar);
    let has_values = !self.values.is_empty();

    let mut results = Vec::new();
    for bucket_index in start_bucket..self.nvt.bucket_count() {
      let bucket = self.nvt.get_bucket(bucket_index);
      if bucket.entry_count == 0 {
        continue;
      }
      let start = bucket.kv_block_offset as usize;
      let end = (start + bucket.entry_count as usize).min(self.entries.len());
      for entry in &self.entries[start..end] {
        if has_values {
          if let Some(stored_value) = self.values.get(&entry.file_hash) {
            if stored_value.as_slice() > value {
              results.push(entry);
            }
            continue;
          }
        }
        if entry.scalar > target_scalar {
          results.push(entry);
        }
      }
    }

    Ok(results)
  }

  /// Less than query. Uses NVT to skip buckets above the threshold.
  /// When raw values are available, uses byte comparison to avoid f64 precision
  /// issues (same fix as lookup_exact and lookup_range).
  pub fn lookup_lt(&mut self, value: &[u8]) -> EngineResult<Vec<&IndexEntry>> {
    if !self.converter.is_order_preserving() {
      return Err(EngineError::RangeQueryNotSupported(
        self.converter.name().to_string(),
      ));
    }
    self.ensure_nvt_current();

    let target_scalar = self.converter.to_scalar(value);
    let end_bucket = self.scalar_to_bucket(target_scalar);
    let has_values = !self.values.is_empty();

    let mut results = Vec::new();
    for bucket_index in 0..=end_bucket {
      if bucket_index >= self.nvt.bucket_count() {
        break;
      }
      let bucket = self.nvt.get_bucket(bucket_index);
      if bucket.entry_count == 0 {
        continue;
      }
      let start = bucket.kv_block_offset as usize;
      let end = (start + bucket.entry_count as usize).min(self.entries.len());
      for entry in &self.entries[start..end] {
        if has_values {
          if let Some(stored_value) = self.values.get(&entry.file_hash) {
            if stored_value.as_slice() < value {
              results.push(entry);
            }
            continue;
          }
        }
        if entry.scalar < target_scalar {
          results.push(entry);
        }
      }
    }

    Ok(results)
  }

  /// Direct scalar lookup — O(1) bucket identification, scan within bucket.
  /// Used for Tier 1 simple queries. Bypasses the NVT value conversion.
  pub fn lookup_by_scalar(&mut self, scalar: f64) -> Vec<&IndexEntry> {
    self.ensure_nvt_current();
    let bucket_index = self.nvt.bucket_for_scalar(scalar);
    let bucket = self.nvt.get_bucket(bucket_index);

    if bucket.entry_count == 0 {
      return Vec::new();
    }

    let start = bucket.kv_block_offset as usize;
    let end = (start + bucket.entry_count as usize).min(self.entries.len());

    self.entries[start..end]
      .iter()
      .filter(|entry| (entry.scalar - scalar).abs() < 1e-10)
      .collect()
  }

  /// Direct scalar range — mark start/end buckets, return entries in range.
  /// Used for Tier 1 simple queries. Bypasses the NVT value conversion.
  pub fn lookup_by_scalar_range(&mut self, min_scalar: f64, max_scalar: f64) -> Vec<&IndexEntry> {
    self.ensure_nvt_current();

    let start_bucket = self.scalar_to_bucket(min_scalar);
    let end_bucket = self.scalar_to_bucket(max_scalar);

    let mut results = Vec::new();
    for bucket_index in start_bucket..=end_bucket {
      if bucket_index >= self.nvt.bucket_count() {
        break;
      }
      let bucket = self.nvt.get_bucket(bucket_index);
      if bucket.entry_count == 0 {
        continue;
      }
      let start = bucket.kv_block_offset as usize;
      let end = (start + bucket.entry_count as usize).min(self.entries.len());
      for entry in &self.entries[start..end] {
        if entry.scalar >= min_scalar && entry.scalar <= max_scalar {
          results.push(entry);
        }
      }
    }

    results
  }

  /// Return the number of entries.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Check if the index is empty.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Serialize the index: converter state + NVT data + entry count + sorted entries.
  /// Each entry is: f64 scalar (8 bytes LE) + file_hash (hash_length bytes).
  pub fn serialize(&self, hash_length: usize) -> Vec<u8> {
    let converter_data = serialize_converter(self.converter.as_ref());
    let nvt_data = self.nvt.serialize();
    let field_name_bytes = self.field_name.as_bytes();

    let values_size: usize = self.values.iter()
      .map(|(k, v)| k.len() + 4 + v.len())
      .sum();
    let capacity = 2 + field_name_bytes.len()
      + 4 + converter_data.len()
      + 4 + nvt_data.len()
      + 4
      + self.entries.len() * (8 + hash_length)
      + 4 + values_size;
    let mut buffer = Vec::with_capacity(capacity);

    // Field name
    buffer.extend_from_slice(&(field_name_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(field_name_bytes);

    // Converter section
    buffer.extend_from_slice(&(converter_data.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&converter_data);

    // NVT section
    buffer.extend_from_slice(&(nvt_data.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&nvt_data);

    // Entry count
    buffer.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());

    // Entries: scalar (f64 LE) + file_hash
    for entry in &self.entries {
      buffer.extend_from_slice(&entry.scalar.to_le_bytes());
      buffer.extend_from_slice(&entry.file_hash);
    }

    // Values map: count (u32) + each: file_hash (hash_length bytes) + value_length (u32) + value bytes
    buffer.extend_from_slice(&(self.values.len() as u32).to_le_bytes());
    for (file_hash, value) in &self.values {
      buffer.extend_from_slice(file_hash);
      buffer.extend_from_slice(&(value.len() as u32).to_le_bytes());
      buffer.extend_from_slice(value);
    }

    buffer
  }

  /// Deserialize an index from bytes.
  pub fn deserialize(data: &[u8], hash_length: usize) -> EngineResult<Self> {
    let mut cursor = 0;

    // Field name
    if data.len() < cursor + 2 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "FieldIndex data too short for field name length".to_string(),
      });
    }
    let field_name_length = u16::from_le_bytes([data[cursor], data[cursor + 1]]) as usize;
    cursor += 2;

    if data.len() < cursor + field_name_length {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "FieldIndex data too short for field name".to_string(),
      });
    }
    let field_name = String::from_utf8(data[cursor..cursor + field_name_length].to_vec())
      .map_err(|error| EngineError::CorruptEntry {
        offset: cursor as u64,
        reason: format!("Invalid UTF-8 field name: {}", error),
      })?;
    cursor += field_name_length;

    // Converter section
    if data.len() < cursor + 4 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "FieldIndex data too short for converter length".to_string(),
      });
    }
    let converter_length = u32::from_le_bytes([
      data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3],
    ]) as usize;
    cursor += 4;

    if data.len() < cursor + converter_length {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "FieldIndex data too short for converter data".to_string(),
      });
    }
    let converter = deserialize_converter(&data[cursor..cursor + converter_length])?;
    cursor += converter_length;

    // NVT section (optional for backward compatibility with old format)
    let nvt = if data.len() >= cursor + 4 {
      let nvt_length = u32::from_le_bytes([
        data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3],
      ]) as usize;

      // Heuristic: if nvt_length is unreasonably large or would exceed remaining data
      // minus what we need for entries, this is likely the old format where this u32
      // is actually the entry count. In the new format, NVT data starts with a version
      // byte (>= 1) followed by converter data, so nvt_length will be reasonable.
      // We peek at what follows: if cursor+4+nvt_length leaves room for an entry_count u32,
      // and the data at cursor+4 starts with a valid NVT version byte, it's NVT data.
      let has_nvt_section = nvt_length > 0
        && data.len() >= cursor + 4 + nvt_length + 4
        && data[cursor + 4] >= 1 // valid NVT version byte
        && data[cursor + 4] < 128; // not a huge version number

      if has_nvt_section {
        cursor += 4;
        let nvt_result = NormalizedVectorTable::deserialize(&data[cursor..cursor + nvt_length]);
        cursor += nvt_length;
        nvt_result.ok()
      } else {
        None
      }
    } else {
      None
    };

    // Entry count
    if data.len() < cursor + 4 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "FieldIndex data too short for entry count".to_string(),
      });
    }
    let entry_count = u32::from_le_bytes([
      data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3],
    ]) as usize;
    cursor += 4;

    let entry_size = 8 + hash_length;
    if data.len() < cursor + entry_count * entry_size {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!(
          "FieldIndex data too short for {} entries (need {} bytes, have {})",
          entry_count,
          entry_count * entry_size,
          data.len() - cursor,
        ),
      });
    }

    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
      let scalar = f64::from_le_bytes([
        data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3],
        data[cursor + 4], data[cursor + 5], data[cursor + 6], data[cursor + 7],
      ]);
      cursor += 8;

      let file_hash = data[cursor..cursor + hash_length].to_vec();
      cursor += hash_length;

      entries.push(IndexEntry { scalar, file_hash });
    }

    // Read values map (backward compatible — empty if no data remains)
    let mut values = HashMap::new();
    if data.len() > cursor + 4 {
      let value_count = u32::from_le_bytes([
        data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3],
      ]) as usize;
      cursor += 4;

      for _ in 0..value_count {
        if data.len() < cursor + hash_length + 4 {
          break; // truncated data, stop reading
        }
        let file_hash = data[cursor..cursor + hash_length].to_vec();
        cursor += hash_length;

        let value_length = u32::from_le_bytes([
          data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3],
        ]) as usize;
        cursor += 4;

        if data.len() < cursor + value_length {
          break;
        }
        let value = data[cursor..cursor + value_length].to_vec();
        cursor += value_length;

        values.insert(file_hash, value);
      }
    }

    // Use deserialized NVT if available (preserves bucket count), otherwise build fresh.
    let resolved_nvt = match nvt {
      Some(deserialized_nvt) => deserialized_nvt,
      None => {
        let nvt_converter = deserialize_converter(&converter.serialize())
          .expect("converter roundtrip for NVT should never fail");
        NormalizedVectorTable::new(nvt_converter, DEFAULT_NVT_BUCKET_COUNT)
      }
    };

    // Always rebuild NVT from entries on deserialize, since the serialized NVT
    // may be stale (entries modified after last NVT rebuild before serialization).
    let mut index = FieldIndex {
      field_name,
      converter,
      entries,
      nvt: resolved_nvt,
      values,
      dirty: true,
    };
    index.rebuild_nvt();
    Ok(index)
  }

  // --- Private helpers ---

  /// Map a scalar in [0.0, 1.0] to a bucket index.
  fn scalar_to_bucket(&self, scalar: f64) -> usize {
    let bucket_count = self.nvt.bucket_count();
    let index = (scalar * bucket_count as f64).floor() as usize;
    index.min(bucket_count.saturating_sub(1))
  }
}

/// Manages indexes for paths in the storage engine.
pub struct IndexManager<'a> {
  engine: &'a StorageEngine,
}

impl<'a> IndexManager<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    IndexManager { engine }
  }

  /// Build the index file path for a field at a given path (old format, no strategy).
  fn index_file_path_legacy(path: &str, field_name: &str) -> String {
    let base = if path.ends_with('/') {
      path.to_string()
    } else {
      format!("{}/", path)
    };
    format!("{}.indexes/{}.idx", base, field_name)
  }

  /// Build the index file path for a field at a given path with strategy.
  fn index_file_path(path: &str, field_name: &str, strategy: &str) -> String {
    let base = if path.ends_with('/') {
      path.to_string()
    } else {
      format!("{}/", path)
    };
    format!("{}.indexes/{}.{}.idx", base, field_name, strategy)
  }

  /// Build the indexes directory path for a given path.
  fn indexes_dir_path(path: &str) -> String {
    let base = if path.ends_with('/') {
      path.to_string()
    } else {
      format!("{}/", path)
    };
    format!("{}.indexes", base)
  }

  /// Load an index for a field at the given path.
  /// Tries the old naming format ({field_name}.idx) first for backward compatibility,
  /// then scans for new-format files ({field_name}.{strategy}.idx).
  pub fn load_index(&self, path: &str, field_name: &str) -> EngineResult<Option<FieldIndex>> {
    // Try old format first: {field_name}.idx
    let old_path = Self::index_file_path_legacy(path, field_name);
    let ops = DirectoryOps::new(self.engine);

    match ops.read_file_buffered(&old_path) {
      Ok(data) => {
        let hash_length = self.engine.hash_algo().hash_length();
        let index = FieldIndex::deserialize(&data, hash_length)?;
        return Ok(Some(index));
      }
      Err(EngineError::NotFound(_)) => {} // fall through to scan
      Err(error) => return Err(error),
    }

    // Try new format: scan for {field_name}.{strategy}.idx
    let indexes = self.list_indexes(path)?;
    for index_name in &indexes {
      if index_name.starts_with(&format!("{}.", field_name)) {
        let strategy = index_name.split_once('.').map(|x| x.1).unwrap_or("string");
        return self.load_index_by_strategy(path, field_name, strategy);
      }
    }

    Ok(None)
  }

  /// Load an index by field name and strategy.
  pub fn load_index_by_strategy(&self, path: &str, field_name: &str, strategy: &str) -> EngineResult<Option<FieldIndex>> {
    let index_path = Self::index_file_path(path, field_name, strategy);
    let ops = DirectoryOps::new(self.engine);

    match ops.read_file_buffered(&index_path) {
      Ok(data) => {
        let hash_length = self.engine.hash_algo().hash_length();
        let index = FieldIndex::deserialize(&data, hash_length)?;
        Ok(Some(index))
      }
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(error) => Err(error),
    }
  }

  /// Save an index to `.indexes/{field_name}.{strategy}.idx` at the given path.
  pub fn save_index(&self, path: &str, index: &FieldIndex) -> EngineResult<()> {
    let strategy = index.converter.strategy();
    let index_path = Self::index_file_path(path, &index.field_name, strategy);
    let hash_length = self.engine.hash_algo().hash_length();
    let data = index.serialize(hash_length);
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(self.engine);
    ops.store_file_buffered(&ctx, &index_path, &data, Some("application/octet-stream"))?;
    Ok(())
  }

  /// List index names at this path.
  /// Returns names in the format "field.strategy" (new format) or "field" (old format).
  pub fn list_indexes(&self, path: &str) -> EngineResult<Vec<String>> {
    let indexes_dir = Self::indexes_dir_path(path);
    let ops = DirectoryOps::new(self.engine);

    match ops.list_directory(&indexes_dir) {
      Ok(children) => {
        let field_names: Vec<String> = children
          .iter()
          .filter_map(|child| {
            if child.name.ends_with(".idx") {
              // New format: field.strategy.idx -> return "field.strategy"
              // Old format: field.idx -> return "field"
              Some(child.name.trim_end_matches(".idx").to_string())
            } else {
              None
            }
          })
          .collect();
        Ok(field_names)
      }
      Err(EngineError::NotFound(_)) => Ok(Vec::new()),
      Err(error) => Err(error),
    }
  }

  /// Delete an index for a field and strategy at the given path.
  pub fn delete_index(&self, path: &str, field_name: &str, strategy: &str) -> EngineResult<()> {
    let ctx = RequestContext::system();
    let index_path = Self::index_file_path(path, field_name, strategy);
    let ops = DirectoryOps::new(self.engine);
    ops.delete_file(&ctx, &index_path)
  }

  /// Delete an index using the legacy path format (no strategy).
  pub fn delete_index_legacy(&self, path: &str, field_name: &str) -> EngineResult<()> {
    let ctx = RequestContext::system();
    let index_path = Self::index_file_path_legacy(path, field_name);
    let ops = DirectoryOps::new(self.engine);
    ops.delete_file(&ctx, &index_path)
  }

  /// Create an empty index for a field at the given path.
  pub fn create_index(
    &self,
    path: &str,
    field_name: &str,
    converter: Box<dyn ScalarConverter>,
  ) -> EngineResult<FieldIndex> {
    let index = FieldIndex::new(field_name.to_string(), converter);
    self.save_index(path, &index)?;
    Ok(index)
  }

  /// Discover all directories that contain indexes under `base_path`.
  ///
  /// Scans `base_path` recursively for files whose path includes `/.indexes/`,
  /// extracts the parent directory of each `.indexes` segment, and returns a
  /// deduplicated, sorted list.
  pub fn discover_indexed_directories(
    &self,
    base_path: &str,
  ) -> EngineResult<Vec<String>> {
    use std::collections::BTreeSet;
    use crate::engine::directory_listing::list_directory_recursive;

    let mut indexed_dirs = BTreeSet::new();

    // Check base_path itself: if list_indexes returns any results, include it.
    let base_indexes = self.list_indexes(base_path)?;
    if !base_indexes.is_empty() {
      let normalized = crate::engine::path_utils::normalize_path(base_path);
      indexed_dirs.insert(normalized);
    }

    // Recursively list all files. Files inside .indexes directories have paths
    // like `/some/dir/.indexes/field.trigram.idx`. We extract `/some/dir` from
    // those paths. If the recursive walk fails (e.g., malformed directory entry
    // after KV expansion), log the error and return whatever we found from the
    // base path scan — partial results are better than a total failure.
    match list_directory_recursive(self.engine, base_path, -1, None, None) {
      Ok(entries) => {
        for entry in &entries {
          if let Some(idx_pos) = entry.path.find("/.indexes/") {
            let parent = &entry.path[..idx_pos];
            let dir = if parent.is_empty() { "/" } else { parent };
            indexed_dirs.insert(dir.to_string());
          }
        }
      }
      Err(e) => {
        tracing::warn!(
          base_path,
          "discover_indexed_directories: recursive scan failed ({}). Returning base-path results only.",
          e,
        );
      }
    }

    Ok(indexed_dirs.into_iter().collect())
  }

  /// Load all indexes for a field across all strategies.
  pub fn load_indexes_for_field(&self, path: &str, field_name: &str) -> EngineResult<Vec<FieldIndex>> {
    let indexes = self.list_indexes(path)?;
    let mut result = Vec::new();

    for index_name in &indexes {
      // index_name is either "field" (old) or "field.strategy" (new)
      let is_match = index_name == field_name
        || index_name.starts_with(&format!("{}.", field_name));

      if is_match {
        // Determine strategy from the name
        let strategy = if index_name.contains('.') {
          index_name.split_once('.').map(|x| x.1).unwrap_or("string")
        } else {
          "string" // old format
        };

        if let Some(idx) = self.load_index_by_strategy(path, field_name, strategy)? {
          result.push(idx);
        } else if strategy == "string" {
          // Try old format (no strategy in filename)
          if let Some(idx) = self.load_index(path, field_name)? {
            result.push(idx);
          }
        }
      }
    }

    Ok(result)
  }
}
