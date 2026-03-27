use std::collections::HashMap;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::scalar_index::ScalarIndex;
use super::scalar_mapping::{
  F64Mapping, I64Mapping, StringMapping, U16Mapping, U32Mapping, U64Mapping, U8Mapping,
};

const DEFAULT_INITIAL_CAPACITY: usize = 1024;
const DEFAULT_STRING_MAX_LENGTH: usize = 256;
const DEFAULT_F64_MIN: f64 = 0.0;
const DEFAULT_F64_MAX: f64 = 1_000_000.0;

/// Metadata describing an index.
#[derive(Debug, Clone)]
pub struct IndexDefinition {
  pub index_id: Uuid,
  pub table_name: String,
  pub column_name: String,
  pub mapping_type: String,
  pub created_at: DateTime<Utc>,
}

/// Error type for index manager operations.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
  #[error("unsupported mapping type: {0}")]
  UnsupportedMappingType(String),

  #[error("index already exists for {0}:{1}")]
  IndexAlreadyExists(String, String),

  #[error("index not found for {0}:{1}")]
  IndexNotFound(String, String),
}

/// Manages multiple scalar indexes across tables and columns.
pub struct IndexManager {
  indexes: HashMap<String, ScalarIndex>,
  definitions: HashMap<String, IndexDefinition>,
}

impl IndexManager {
  /// Create a new, empty index manager.
  pub fn new() -> Self {
    Self {
      indexes: HashMap::new(),
      definitions: HashMap::new(),
    }
  }

  /// Build the key used for index lookup: "table:column".
  fn make_key(table_name: &str, column_name: &str) -> String {
    format!("{}:{}", table_name, column_name)
  }

  /// Create a new index for a table/column with the specified mapping type.
  ///
  /// Supported mapping types: "u8", "u16", "u32", "u64", "i64", "f64", "string".
  pub fn create_index(
    &mut self,
    table_name: &str,
    column_name: &str,
    mapping_type: &str,
  ) -> Result<IndexDefinition, IndexError> {
    let key = Self::make_key(table_name, column_name);

    if self.indexes.contains_key(&key) {
      return Err(IndexError::IndexAlreadyExists(
        table_name.to_string(),
        column_name.to_string(),
      ));
    }

    let mapping: Box<dyn super::scalar_mapping::ScalarMapping> = match mapping_type {
      "u8" => Box::new(U8Mapping),
      "u16" => Box::new(U16Mapping),
      "u32" => Box::new(U32Mapping),
      "u64" => Box::new(U64Mapping),
      "i64" => Box::new(I64Mapping),
      "f64" => Box::new(F64Mapping::new(DEFAULT_F64_MIN, DEFAULT_F64_MAX)),
      "string" => Box::new(StringMapping::new(DEFAULT_STRING_MAX_LENGTH)),
      other => return Err(IndexError::UnsupportedMappingType(other.to_string())),
    };

    let index_name = format!("idx_{}_{}", table_name, column_name);
    let scalar_index = ScalarIndex::new(index_name, mapping, DEFAULT_INITIAL_CAPACITY);

    let definition = IndexDefinition {
      index_id: Uuid::new_v4(),
      table_name: table_name.to_string(),
      column_name: column_name.to_string(),
      mapping_type: mapping_type.to_string(),
      created_at: Utc::now(),
    };

    self.indexes.insert(key.clone(), scalar_index);
    self.definitions.insert(key, definition.clone());

    Ok(definition)
  }

  /// Drop an index for a table/column.
  pub fn drop_index(
    &mut self,
    table_name: &str,
    column_name: &str,
  ) -> Result<(), IndexError> {
    let key = Self::make_key(table_name, column_name);

    if self.indexes.remove(&key).is_none() {
      return Err(IndexError::IndexNotFound(
        table_name.to_string(),
        column_name.to_string(),
      ));
    }

    self.definitions.remove(&key);
    Ok(())
  }

  /// Get an immutable reference to an index.
  pub fn get_index(&self, table_name: &str, column_name: &str) -> Option<&ScalarIndex> {
    let key = Self::make_key(table_name, column_name);
    self.indexes.get(&key)
  }

  /// Get a mutable reference to an index.
  pub fn get_index_mut(
    &mut self,
    table_name: &str,
    column_name: &str,
  ) -> Option<&mut ScalarIndex> {
    let key = Self::make_key(table_name, column_name);
    self.indexes.get_mut(&key)
  }

  /// List all index definitions, optionally filtered by table name.
  pub fn list_indexes(&self, table_name: Option<&str>) -> Vec<&IndexDefinition> {
    self.definitions
      .values()
      .filter(|definition| {
        table_name
          .map(|name| definition.table_name == name)
          .unwrap_or(true)
      })
      .collect()
  }
}

impl Default for IndexManager {
  fn default() -> Self {
    Self::new()
  }
}
