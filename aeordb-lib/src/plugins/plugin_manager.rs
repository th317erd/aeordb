use chrono::{DateTime, Utc};
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{PluginMetadata, PluginType};
use super::wasm_runtime::WasmPluginRuntime;

/// Persistent record for a deployed plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRecord {
  pub plugin_id: Uuid,
  pub name: String,
  pub path: String,
  pub plugin_type: PluginType,
  pub wasm_bytes: Vec<u8>,
  pub created_at: DateTime<Utc>,
}

impl PluginRecord {
  /// Convert to lightweight metadata (strips the WASM bytes).
  pub fn to_metadata(&self) -> PluginMetadata {
    PluginMetadata {
      plugin_id: self.plugin_id,
      name: self.name.clone(),
      path: self.path.clone(),
      plugin_type: self.plugin_type.clone(),
      created_at: self.created_at,
    }
  }
}

/// Manages the lifecycle of deployed plugins backed by redb storage.
pub struct PluginManager {
  database: std::sync::Arc<redb::Database>,
}

const PLUGINS_TABLE: TableDefinition<'static, &str, &[u8]> =
  TableDefinition::new("_system:plugins");

impl PluginManager {
  /// Create a new PluginManager sharing the given redb Database handle.
  pub fn new(database: std::sync::Arc<redb::Database>) -> Self {
    Self { database }
  }

  /// Deploy (or overwrite) a plugin at the given path.
  ///
  /// For WASM plugins, the bytes are validated before storage.
  pub fn deploy_plugin(
    &self,
    name: &str,
    path: &str,
    plugin_type: PluginType,
    wasm_bytes: Vec<u8>,
  ) -> Result<PluginRecord, PluginManagerError> {
    // Validate WASM bytes if this is a WASM plugin.
    if plugin_type == PluginType::Wasm {
      WasmPluginRuntime::new(&wasm_bytes).map_err(|error| {
        PluginManagerError::InvalidPlugin(format!("WASM validation failed: {}", error))
      })?;
    }

    // Check if a plugin already exists at this path — reuse its ID if so.
    let existing = self.get_plugin(path)?;
    let plugin_id = existing
      .as_ref()
      .map(|record| record.plugin_id)
      .unwrap_or_else(Uuid::new_v4);

    let record = PluginRecord {
      plugin_id,
      name: name.to_string(),
      path: path.to_string(),
      plugin_type,
      wasm_bytes,
      created_at: existing
        .map(|record| record.created_at)
        .unwrap_or_else(Utc::now),
    };

    let encoded = serde_json::to_vec(&record)
      .map_err(|error| PluginManagerError::Storage(format!("serialization failed: {}", error)))?;

    let write_transaction = self
      .database
      .begin_write()
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;
    {
      let mut table = write_transaction
        .open_table(PLUGINS_TABLE)
        .map_err(|error| PluginManagerError::Storage(error.to_string()))?;
      table
        .insert(path, encoded.as_slice())
        .map_err(|error| PluginManagerError::Storage(error.to_string()))?;
    }
    write_transaction
      .commit()
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;

    Ok(record)
  }

  /// Retrieve a deployed plugin by its path.
  pub fn get_plugin(&self, path: &str) -> Result<Option<PluginRecord>, PluginManagerError> {
    let read_transaction = self
      .database
      .begin_read()
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;

    let table = match read_transaction.open_table(PLUGINS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
      Err(error) => return Err(PluginManagerError::Storage(error.to_string())),
    };

    let guard = match table
      .get(path)
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?
    {
      Some(guard) => guard,
      None => return Ok(None),
    };

    let record: PluginRecord = serde_json::from_slice(guard.value())
      .map_err(|error| PluginManagerError::Storage(format!("deserialization failed: {}", error)))?;

    Ok(Some(record))
  }

  /// List metadata for all deployed plugins.
  pub fn list_plugins(&self) -> Result<Vec<PluginMetadata>, PluginManagerError> {
    let read_transaction = self
      .database
      .begin_read()
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;

    let table = match read_transaction.open_table(PLUGINS_TABLE) {
      Ok(table) => table,
      Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
      Err(error) => return Err(PluginManagerError::Storage(error.to_string())),
    };

    let mut plugins = Vec::new();
    for result in table
      .iter()
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?
    {
      let (_, value_guard) =
        result.map_err(|error| PluginManagerError::Storage(error.to_string()))?;
      let record: PluginRecord = serde_json::from_slice(value_guard.value()).map_err(|error| {
        PluginManagerError::Storage(format!("deserialization failed: {}", error))
      })?;
      plugins.push(record.to_metadata());
    }

    Ok(plugins)
  }

  /// Remove a deployed plugin by its path.
  ///
  /// Returns true if the plugin existed and was removed, false if not found.
  pub fn remove_plugin(&self, path: &str) -> Result<bool, PluginManagerError> {
    let write_transaction = self
      .database
      .begin_write()
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;

    let removed = {
      let mut table = write_transaction
        .open_table(PLUGINS_TABLE)
        .map_err(|error| PluginManagerError::Storage(error.to_string()))?;
      let existed = table
        .remove(path)
        .map_err(|error| PluginManagerError::Storage(error.to_string()))?
        .is_some();
      existed
    };

    write_transaction
      .commit()
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;

    Ok(removed)
  }

  /// Instantiate and invoke a deployed WASM plugin.
  pub fn invoke_wasm_plugin(
    &self,
    path: &str,
    request_bytes: &[u8],
  ) -> Result<Vec<u8>, PluginManagerError> {
    let record = self
      .get_plugin(path)?
      .ok_or_else(|| PluginManagerError::NotFound(path.to_string()))?;

    if record.plugin_type != PluginType::Wasm {
      return Err(PluginManagerError::InvalidPlugin(format!(
        "plugin at '{}' is not a WASM plugin",
        path
      )));
    }

    let runtime = WasmPluginRuntime::new(&record.wasm_bytes).map_err(|error| {
      PluginManagerError::ExecutionFailed(format!("failed to load WASM module: {}", error))
    })?;

    runtime.call_handle(request_bytes).map_err(|error| {
      PluginManagerError::ExecutionFailed(format!("WASM execution failed: {}", error))
    })
  }
}

/// Errors specific to plugin management operations.
#[derive(Debug, thiserror::Error)]
pub enum PluginManagerError {
  #[error("plugin not found: {0}")]
  NotFound(String),

  #[error("invalid plugin: {0}")]
  InvalidPlugin(String),

  #[error("plugin execution failed: {0}")]
  ExecutionFailed(String),

  #[error("storage error: {0}")]
  Storage(String),
}
