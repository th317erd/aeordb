use crate::engine::directory_ops::DirectoryOps;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::scalar_converter::{deserialize_converter, serialize_converter, ScalarConverter};
use crate::engine::storage_engine::StorageEngine;

/// A single entry in a field index: maps a scalar to a file's hash.
#[derive(Debug, Clone)]
pub struct IndexEntry {
  pub scalar: f64,
  pub file_hash: Vec<u8>,
}

/// A field index: converter + sorted entries.
pub struct FieldIndex {
  pub field_name: String,
  pub converter: Box<dyn ScalarConverter>,
  pub entries: Vec<IndexEntry>,
}

impl FieldIndex {
  /// Create an empty index with the given converter.
  pub fn new(field_name: String, converter: Box<dyn ScalarConverter>) -> Self {
    FieldIndex {
      field_name,
      converter,
      entries: Vec::new(),
    }
  }

  /// Convert value to scalar and insert in sorted position.
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
  }

  /// Remove all entries for a given file hash.
  pub fn remove(&mut self, file_hash: &[u8]) {
    self.entries.retain(|entry| entry.file_hash != file_hash);
  }

  /// Find entries matching the scalar for this value (approximate match).
  pub fn lookup_exact(&self, value: &[u8]) -> Vec<&IndexEntry> {
    let target_scalar = self.converter.to_scalar(value);
    self.entries
      .iter()
      .filter(|entry| (entry.scalar - target_scalar).abs() < f64::EPSILON)
      .collect()
  }

  /// Range query: find entries with scalars between min and max values.
  /// Returns error if converter is not order-preserving.
  pub fn lookup_range(&self, min_value: &[u8], max_value: &[u8]) -> EngineResult<Vec<&IndexEntry>> {
    if !self.converter.is_order_preserving() {
      return Err(EngineError::RangeQueryNotSupported(
        self.converter.name().to_string(),
      ));
    }
    let min_scalar = self.converter.to_scalar(min_value);
    let max_scalar = self.converter.to_scalar(max_value);
    Ok(
      self.entries
        .iter()
        .filter(|entry| entry.scalar >= min_scalar && entry.scalar <= max_scalar)
        .collect(),
    )
  }

  /// Greater than query.
  pub fn lookup_gt(&self, value: &[u8]) -> EngineResult<Vec<&IndexEntry>> {
    if !self.converter.is_order_preserving() {
      return Err(EngineError::RangeQueryNotSupported(
        self.converter.name().to_string(),
      ));
    }
    let target_scalar = self.converter.to_scalar(value);
    Ok(
      self.entries
        .iter()
        .filter(|entry| entry.scalar > target_scalar)
        .collect(),
    )
  }

  /// Less than query.
  pub fn lookup_lt(&self, value: &[u8]) -> EngineResult<Vec<&IndexEntry>> {
    if !self.converter.is_order_preserving() {
      return Err(EngineError::RangeQueryNotSupported(
        self.converter.name().to_string(),
      ));
    }
    let target_scalar = self.converter.to_scalar(value);
    Ok(
      self.entries
        .iter()
        .filter(|entry| entry.scalar < target_scalar)
        .collect(),
    )
  }

  /// Return the number of entries.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Check if the index is empty.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Serialize the index: converter state + entry count + sorted entries.
  /// Each entry is: f64 scalar (8 bytes LE) + file_hash (hash_length bytes).
  pub fn serialize(&self, hash_length: usize) -> Vec<u8> {
    let converter_data = serialize_converter(self.converter.as_ref());
    let field_name_bytes = self.field_name.as_bytes();

    let capacity = 2 + field_name_bytes.len()
      + 4 + converter_data.len()
      + 4
      + self.entries.len() * (8 + hash_length);
    let mut buffer = Vec::with_capacity(capacity);

    // Field name
    buffer.extend_from_slice(&(field_name_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(field_name_bytes);

    // Converter section
    buffer.extend_from_slice(&(converter_data.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&converter_data);

    // Entry count
    buffer.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());

    // Entries: scalar (f64 LE) + file_hash
    for entry in &self.entries {
      buffer.extend_from_slice(&entry.scalar.to_le_bytes());
      buffer.extend_from_slice(&entry.file_hash);
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

    Ok(FieldIndex {
      field_name,
      converter,
      entries,
    })
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

  /// Build the index file path for a field at a given path.
  fn index_file_path(path: &str, field_name: &str) -> String {
    let base = if path.ends_with('/') {
      path.to_string()
    } else {
      format!("{}/", path)
    };
    format!("{}.indexes/{}.idx", base, field_name)
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

  /// Load an index from `.indexes/{field_name}.idx` at the given path.
  pub fn load_index(&self, path: &str, field_name: &str) -> EngineResult<Option<FieldIndex>> {
    let index_path = Self::index_file_path(path, field_name);
    let ops = DirectoryOps::new(self.engine);

    match ops.read_file(&index_path) {
      Ok(data) => {
        let hash_length = self.engine.hash_algo().hash_length();
        let index = FieldIndex::deserialize(&data, hash_length)?;
        Ok(Some(index))
      }
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(error) => Err(error),
    }
  }

  /// Save an index to `.indexes/{field_name}.idx` at the given path.
  pub fn save_index(&self, path: &str, index: &FieldIndex) -> EngineResult<()> {
    let index_path = Self::index_file_path(path, &index.field_name);
    let hash_length = self.engine.hash_algo().hash_length();
    let data = index.serialize(hash_length);
    let ops = DirectoryOps::new(self.engine);
    ops.store_file(&index_path, &data, Some("application/octet-stream"))?;
    Ok(())
  }

  /// List field names with indexes at this path.
  pub fn list_indexes(&self, path: &str) -> EngineResult<Vec<String>> {
    let indexes_dir = Self::indexes_dir_path(path);
    let ops = DirectoryOps::new(self.engine);

    match ops.list_directory(&indexes_dir) {
      Ok(children) => {
        let field_names: Vec<String> = children
          .iter()
          .filter_map(|child| {
            if child.name.ends_with(".idx") {
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

  /// Delete an index for a field at the given path.
  pub fn delete_index(&self, path: &str, field_name: &str) -> EngineResult<()> {
    let index_path = Self::index_file_path(path, field_name);
    let ops = DirectoryOps::new(self.engine);
    ops.delete_file(&index_path)
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
}
