use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{PluginMetadata, PluginType};
use super::wasm_runtime::WasmPluginRuntime;
use crate::engine::RequestContext;
use crate::engine::StorageEngine;
use crate::engine::SystemTables;

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

/// Manages the lifecycle of deployed plugins backed by the StorageEngine.
pub struct PluginManager {
  engine: std::sync::Arc<StorageEngine>,
}

impl PluginManager {
  /// Create a new PluginManager sharing the given StorageEngine.
  pub fn new(engine: std::sync::Arc<StorageEngine>) -> Self {
    Self { engine }
  }

  fn system_tables(&self) -> SystemTables<'_> {
    SystemTables::new(&self.engine)
  }

  /// Deploy (or overwrite) a plugin at the given path.
  ///
  /// For WASM plugins, the bytes are validated before storage.
  #[tracing::instrument(skip(self, wasm_bytes), fields(path = %path, plugin_type = ?plugin_type))]
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

    let ctx = RequestContext::system();
    self.system_tables()
      .store_plugin(&ctx, path, &encoded)
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;

    tracing::info!(
      path = %path,
      plugin_type = ?record.plugin_type,
      plugin_id = %record.plugin_id,
      "Plugin deployed"
    );

    Ok(record)
  }

  /// Retrieve a deployed plugin by its path.
  pub fn get_plugin(&self, path: &str) -> Result<Option<PluginRecord>, PluginManagerError> {
    let data = self.system_tables()
      .get_plugin(path)
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;

    match data {
      Some(bytes) => {
        let record: PluginRecord = serde_json::from_slice(&bytes)
          .map_err(|error| PluginManagerError::Storage(format!("deserialization failed: {}", error)))?;
        Ok(Some(record))
      }
      None => Ok(None),
    }
  }

  /// List metadata for all deployed plugins.
  pub fn list_plugins(&self) -> Result<Vec<PluginMetadata>, PluginManagerError> {
    let entries = self.system_tables()
      .list_plugins()
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;

    let mut plugins = Vec::new();
    for (_path, bytes) in entries {
      let record: PluginRecord = serde_json::from_slice(&bytes).map_err(|error| {
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
    let ctx = RequestContext::system();
    self.system_tables()
      .remove_plugin(&ctx, path)
      .map_err(|error| PluginManagerError::Storage(error.to_string()))
  }

  /// Instantiate and invoke a deployed WASM plugin.
  #[tracing::instrument(skip(self, request_bytes), fields(path = %path, request_size = request_bytes.len()))]
  pub fn invoke_wasm_plugin(
    &self,
    path: &str,
    request_bytes: &[u8],
  ) -> Result<Vec<u8>, PluginManagerError> {
    let start = std::time::Instant::now();

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
      tracing::error!(path = %path, error = %error, "Failed to load WASM module");
      metrics::counter!(crate::metrics::definitions::PLUGIN_ERRORS_TOTAL, "error_type" => "load_failed").increment(1);
      PluginManagerError::ExecutionFailed(format!("failed to load WASM module: {}", error))
    })?;

    let result = runtime.call_handle(request_bytes).map_err(|error| {
      tracing::error!(path = %path, error = %error, "WASM execution failed");
      metrics::counter!(crate::metrics::definitions::PLUGIN_ERRORS_TOTAL, "error_type" => "execution_failed").increment(1);
      PluginManagerError::ExecutionFailed(format!("WASM execution failed: {}", error))
    });

    let duration = start.elapsed().as_secs_f64();
    metrics::counter!(crate::metrics::definitions::PLUGIN_INVOCATIONS_TOTAL).increment(1);
    metrics::histogram!(crate::metrics::definitions::PLUGIN_DURATION).record(duration);

    tracing::info!(
      path = %path,
      duration_ms = duration * 1000.0,
      "Plugin invoked"
    );

    result
  }

  /// Invoke a WASM plugin with custom memory limits (for parser plugins).
  pub fn invoke_wasm_plugin_with_limits(
    &self,
    path: &str,
    request_bytes: &[u8],
    memory_limit_bytes: usize,
  ) -> Result<Vec<u8>, PluginManagerError> {
    let record = self
      .get_plugin(path)?
      .ok_or_else(|| PluginManagerError::NotFound(path.to_string()))?;

    if record.plugin_type != PluginType::Wasm {
      return Err(PluginManagerError::InvalidPlugin(format!(
        "plugin at '{}' is not a WASM plugin", path
      )));
    }

    let runtime = WasmPluginRuntime::with_limits(
      &record.wasm_bytes,
      memory_limit_bytes,
      1_000_000, // default fuel limit
    ).map_err(|error| {
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
