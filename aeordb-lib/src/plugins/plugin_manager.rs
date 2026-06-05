use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{PluginMetadata, PluginType};
use super::wasm_runtime::WasmPluginRuntime;
use crate::engine::cache::Cache;
use crate::engine::cache_loaders::{ApiKeyLoader, GroupLoader};
use crate::engine::RequestContext;
use crate::engine::StorageEngine;
use crate::engine::system_store;

/// A first-party plugin embedded into the AeorDB binary.
#[derive(Debug, Clone, Copy)]
pub struct BundledPlugin {
  pub plugin_id: &'static str,
  pub name: &'static str,
  pub path: &'static str,
  pub version: &'static str,
  pub author: &'static str,
  pub wasm_bytes: &'static [u8],
}

/// WASM query plugins installed into user-accessible `/plugins/{name}` paths
/// when the server starts.
pub const BUNDLED_PLUGINS: &[BundledPlugin] = &[
  BundledPlugin {
    plugin_id: "/org/aeordev/aeordb/plugins/extract",
    name: "extract",
    path: "extract",
    version: "0.1.0",
    author: "AeorDB",
    wasm_bytes: include_bytes!("bundled/extract.wasm"),
  },
  BundledPlugin {
    plugin_id: "/org/aeordev/aeordb/plugins/jq",
    name: "jq",
    path: "jq",
    version: "0.1.0",
    author: "AeorDB",
    wasm_bytes: include_bytes!("bundled/jq.wasm"),
  },
];

fn checksum_for_bytes(bytes: &[u8]) -> String {
  format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn default_updated_at() -> DateTime<Utc> {
  Utc::now()
}

fn bundled_version_can_replace(bundled_version: &str, current_version: Option<&str>) -> bool {
  let Ok(bundled) = Version::parse(bundled_version) else {
    return false;
  };

  match current_version {
    Some(current_version) => Version::parse(current_version)
      .map(|current| bundled >= current)
      .unwrap_or(true),
    None => true,
  }
}

/// Persistent record for a deployed plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRecord {
  pub plugin_id: String,
  pub name: String,
  pub path: String,
  pub plugin_type: PluginType,
  pub wasm_bytes: Vec<u8>,
  pub created_at: DateTime<Utc>,
  #[serde(default)]
  pub version: Option<String>,
  #[serde(default)]
  pub author: Option<String>,
  #[serde(default)]
  pub checksum: String,
  #[serde(default = "default_updated_at")]
  pub updated_at: DateTime<Utc>,
}

impl PluginRecord {
  fn normalize_metadata(&mut self) {
    if self.checksum.is_empty() {
      self.checksum = checksum_for_bytes(&self.wasm_bytes);
    }
  }

  /// Convert to lightweight metadata (strips the WASM bytes).
  pub fn to_metadata(&self) -> PluginMetadata {
    PluginMetadata {
      plugin_id: self.plugin_id.clone(),
      name: self.name.clone(),
      path: self.path.clone(),
      plugin_type: self.plugin_type.clone(),
      created_at: self.created_at,
      version: self.version.clone(),
      author: self.author.clone(),
      checksum: if self.checksum.is_empty() {
        checksum_for_bytes(&self.wasm_bytes)
      } else {
        self.checksum.clone()
      },
      updated_at: self.updated_at,
    }
  }
}

/// A cached compiled WASM runtime keyed by plugin path.
///
/// The `WasmPluginRuntime` holds a wasmi `Engine` + `Module`. The `Module`
/// is the parsed/validated WASM — that parsing is the expensive step we want
/// to avoid on every invocation. The runtime is reusable because `call_handle`
/// creates a fresh `Store` per invocation (no shared mutable state).
struct PluginCache {
  entries: HashMap<String, Arc<WasmPluginRuntime>>,
}

impl PluginCache {
  fn new() -> Self {
    PluginCache {
      entries: HashMap::new(),
    }
  }

  /// Get a cached runtime, or compile + cache it from the given WASM bytes.
  fn get_or_compile(
    &mut self,
    path: &str,
    wasm_bytes: &[u8],
  ) -> Result<Arc<WasmPluginRuntime>, super::wasm_runtime::WasmRuntimeError> {
    if let Some(runtime) = self.entries.get(path) {
      return Ok(Arc::clone(runtime));
    }
    let runtime = Arc::new(WasmPluginRuntime::new(wasm_bytes)?);
    self.entries.insert(path.to_string(), Arc::clone(&runtime));
    Ok(runtime)
  }

  /// Get a cached runtime with custom limits, or compile + cache it.
  /// Custom-limit runtimes are NOT cached (limits may differ per call).
  fn compile_with_limits(
    wasm_bytes: &[u8],
    memory_limit_bytes: usize,
    fuel_limit: u64,
  ) -> Result<WasmPluginRuntime, super::wasm_runtime::WasmRuntimeError> {
    WasmPluginRuntime::with_limits(wasm_bytes, memory_limit_bytes, fuel_limit)
  }

  /// Invalidate the cache entry for a given path.
  fn invalidate(&mut self, path: &str) {
    self.entries.remove(path);
  }
}

/// Manages the lifecycle of deployed plugins backed by the StorageEngine.
pub struct PluginManager {
  engine: std::sync::Arc<StorageEngine>,
  /// Cache of compiled WASM runtimes keyed by plugin path.
  /// Invalidated on deploy and remove.
  cache: Mutex<PluginCache>,
}

impl PluginManager {
  /// Create a new PluginManager sharing the given StorageEngine.
  pub fn new(engine: std::sync::Arc<StorageEngine>) -> Self {
    Self {
      engine,
      cache: Mutex::new(PluginCache::new()),
    }
  }

  /// Install or update all bundled first-party plugins.
  ///
  /// Bundled plugins are stored at their public plugin path, so `extract`
  /// becomes available at `/plugins/extract/invoke`. Existing records are only
  /// overwritten when they carry the bundled plugin ID and the bundled version
  /// is not older than the stored version.
  pub fn install_bundled_plugins(&self) -> Result<Vec<PluginMetadata>, PluginManagerError> {
    let mut installed_or_updated = Vec::new();

    for bundled in BUNDLED_PLUGINS {
      let bundled_plugin_id = bundled.plugin_id.to_string();
      let bundled_checksum = checksum_for_bytes(bundled.wasm_bytes);

      match self.get_plugin(bundled.path)? {
        Some(existing) => {
          let is_current_bundled_plugin = existing.plugin_id == bundled_plugin_id;
          let version_allows_replace =
            bundled_version_can_replace(bundled.version, existing.version.as_deref());
          let bytes_or_metadata_differ = existing.checksum != bundled_checksum
            || existing.version.as_deref() != Some(bundled.version)
            || existing.author.as_deref() != Some(bundled.author)
            || existing.name != bundled.name
            || existing.plugin_type != PluginType::Wasm;

          if is_current_bundled_plugin && version_allows_replace && bytes_or_metadata_differ {
            let record = self.deploy_plugin_with_metadata_and_id(
              bundled.name,
              bundled.path,
              PluginType::Wasm,
              bundled.wasm_bytes.to_vec(),
              Some(bundled.version.to_string()),
              Some(bundled.author.to_string()),
              Some(bundled_plugin_id),
            )?;
            installed_or_updated.push(record.to_metadata());
          } else if !is_current_bundled_plugin
            && existing.author.as_deref() == Some(bundled.author)
            && existing.name == bundled.name
            && existing.version.as_deref() == Some(bundled.version)
            && existing.checksum == bundled_checksum
            && existing.plugin_type == PluginType::Wasm
          {
            let record = self.deploy_plugin_with_metadata_and_id(
              bundled.name,
              bundled.path,
              PluginType::Wasm,
              bundled.wasm_bytes.to_vec(),
              Some(bundled.version.to_string()),
              Some(bundled.author.to_string()),
              Some(bundled_plugin_id),
            )?;
            installed_or_updated.push(record.to_metadata());
          } else if is_current_bundled_plugin && !version_allows_replace {
            tracing::warn!(
              path = %bundled.path,
              bundled_version = %bundled.version,
              current_version = ?existing.version,
              "Bundled plugin version is older than stored plugin version; leaving stored plugin untouched"
            );
          } else if !is_current_bundled_plugin {
            tracing::warn!(
              path = %bundled.path,
              bundled_plugin_id = %bundled_plugin_id,
              current_plugin_id = %existing.plugin_id,
              "Bundled plugin path is occupied by a different plugin ID; leaving it untouched"
            );
          }
        }
        None => {
          let record = self.deploy_plugin_with_metadata_and_id(
            bundled.name,
            bundled.path,
            PluginType::Wasm,
            bundled.wasm_bytes.to_vec(),
            Some(bundled.version.to_string()),
            Some(bundled.author.to_string()),
            Some(bundled_plugin_id),
          )?;
          installed_or_updated.push(record.to_metadata());
        }
      }
    }

    Ok(installed_or_updated)
  }

  /// Deploy (or overwrite) a plugin at the given path.
  ///
  /// For WASM plugins, the bytes are validated before storage.
  /// Invalidates any cached runtime for this path.
  #[tracing::instrument(skip(self, wasm_bytes), fields(path = %path, plugin_type = ?plugin_type))]
  pub fn deploy_plugin(
    &self,
    name: &str,
    path: &str,
    plugin_type: PluginType,
    wasm_bytes: Vec<u8>,
  ) -> Result<PluginRecord, PluginManagerError> {
    self.deploy_plugin_with_metadata(name, path, plugin_type, wasm_bytes, None, None)
  }

  /// Deploy (or overwrite) a plugin with optional package metadata.
  pub fn deploy_plugin_with_metadata(
    &self,
    name: &str,
    path: &str,
    plugin_type: PluginType,
    wasm_bytes: Vec<u8>,
    version: Option<String>,
    author: Option<String>,
  ) -> Result<PluginRecord, PluginManagerError> {
    self.deploy_plugin_with_metadata_and_id(
      name,
      path,
      plugin_type,
      wasm_bytes,
      version,
      author,
      None,
    )
  }

  fn deploy_plugin_with_metadata_and_id(
    &self,
    name: &str,
    path: &str,
    plugin_type: PluginType,
    wasm_bytes: Vec<u8>,
    version: Option<String>,
    author: Option<String>,
    plugin_id_override: Option<String>,
  ) -> Result<PluginRecord, PluginManagerError> {
    // Validate WASM bytes if this is a WASM plugin.
    if plugin_type == PluginType::Wasm {
      WasmPluginRuntime::new(&wasm_bytes).map_err(|error| {
        PluginManagerError::InvalidPlugin(format!("WASM validation failed: {}", error))
      })?;
    }

    // Invalidate cached runtime for this path (new WASM bytes).
    if let Ok(mut cache) = self.cache.lock() {
      cache.invalidate(path);
    }

    // Check if a plugin already exists at this path — reuse its ID if so.
    let existing = self.get_plugin(path)?;
    let plugin_id = plugin_id_override
      .or_else(|| existing.as_ref().map(|record| record.plugin_id.clone()))
      .unwrap_or_else(|| Uuid::new_v4().to_string());

    let created_at = existing.as_ref().map(|record| record.created_at);

    let now = Utc::now();
    let mut record = PluginRecord {
      plugin_id,
      name: name.to_string(),
      path: path.to_string(),
      plugin_type,
      wasm_bytes,
      created_at: created_at.unwrap_or(now),
      version,
      author,
      checksum: String::new(),
      updated_at: now,
    };
    record.normalize_metadata();

    let encoded = serde_json::to_vec(&record)
      .map_err(|error| PluginManagerError::Storage(format!("serialization failed: {}", error)))?;

    let ctx = RequestContext::system();
    system_store::store_plugin(&self.engine, &ctx, path, &encoded)
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
    let data = system_store::get_plugin(&self.engine, path)
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;

    match data {
      Some(bytes) => {
        let mut record: PluginRecord = serde_json::from_slice(&bytes)
          .map_err(|error| PluginManagerError::Storage(format!("deserialization failed: {}", error)))?;
        record.normalize_metadata();
        Ok(Some(record))
      }
      None => Ok(None),
    }
  }

  /// List metadata for all deployed plugins.
  pub fn list_plugins(&self) -> Result<Vec<PluginMetadata>, PluginManagerError> {
    let entries = system_store::list_plugins(&self.engine)
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?;

    let mut plugins = Vec::new();
    for (_path, bytes) in entries {
      let mut record: PluginRecord = serde_json::from_slice(&bytes).map_err(|error| {
        PluginManagerError::Storage(format!("deserialization failed: {}", error))
      })?;
      record.normalize_metadata();
      plugins.push(record.to_metadata());
    }

    Ok(plugins)
  }

  /// Remove a deployed plugin by its path.
  ///
  /// Returns true if the plugin existed and was removed, false if not found.
  /// Invalidates any cached runtime for this path.
  pub fn remove_plugin(&self, path: &str) -> Result<bool, PluginManagerError> {
    // Invalidate cached runtime.
    if let Ok(mut cache) = self.cache.lock() {
      cache.invalidate(path);
    }

    let ctx = RequestContext::system();
    system_store::remove_plugin(&self.engine, &ctx, path)
      .map_err(|error| PluginManagerError::Storage(error.to_string()))
  }

  /// Get a cached compiled runtime for a plugin, or compile and cache it.
  fn get_cached_runtime(
    &self,
    path: &str,
    wasm_bytes: &[u8],
  ) -> Result<Arc<WasmPluginRuntime>, PluginManagerError> {
    let mut cache = self.cache.lock()
      .map_err(|e| PluginManagerError::ExecutionFailed(
        format!("plugin cache lock poisoned: {}", e),
      ))?;
    cache.get_or_compile(path, wasm_bytes).map_err(|error| {
      tracing::error!(path = %path, error = %error, "Failed to load WASM module");
      metrics::counter!(crate::metrics::definitions::PLUGIN_ERRORS_TOTAL, "error_type" => "load_failed").increment(1);
      PluginManagerError::ExecutionFailed(format!("failed to load WASM module: {}", error))
    })
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
        "plugin at '{}' is not a WASM plugin", path
      )));
    }

    let runtime = self.get_cached_runtime(path, &record.wasm_bytes)?;

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

  /// Instantiate and invoke a deployed WASM plugin with engine context.
  ///
  /// Same as `invoke_wasm_plugin` but provides the `StorageEngine` and
  /// `RequestContext` to the WASM runtime, enabling the 7 database host
  /// functions to perform real operations. Used for query plugins.
  #[tracing::instrument(skip(self, request_bytes, engine, ctx), fields(path = %path, request_size = request_bytes.len()))]
  pub fn invoke_wasm_plugin_with_context(
    &self,
    path: &str,
    request_bytes: &[u8],
    engine: std::sync::Arc<StorageEngine>,
    ctx: RequestContext,
  ) -> Result<Vec<u8>, PluginManagerError> {
    self.invoke_wasm_plugin_with_auth(
      path,
      request_bytes,
      engine,
      ctx,
      Arc::new(Cache::new(GroupLoader)),
      Arc::new(Cache::new(ApiKeyLoader)),
    )
  }

  /// Instantiate and invoke a deployed WASM plugin with authenticated engine context.
  ///
  /// Provides the same permission caches used by HTTP middleware so host
  /// functions can enforce per-path authorization for paths supplied inside
  /// plugin request bodies.
  #[tracing::instrument(skip(self, request_bytes, engine, ctx, group_cache, api_key_cache), fields(path = %path, request_size = request_bytes.len()))]
  pub fn invoke_wasm_plugin_with_auth(
    &self,
    path: &str,
    request_bytes: &[u8],
    engine: std::sync::Arc<StorageEngine>,
    ctx: RequestContext,
    group_cache: Arc<Cache<GroupLoader>>,
    api_key_cache: Arc<Cache<ApiKeyLoader>>,
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

    let runtime = self.get_cached_runtime(path, &record.wasm_bytes)?;

    let result = runtime.call_handle_with_context(request_bytes, engine, ctx, group_cache, api_key_cache).map_err(|error| {
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
      "Plugin invoked with context"
    );

    result
  }

  /// Invoke a WASM plugin with custom memory limits (for parser plugins).
  /// Custom-limit invocations bypass the cache since limits may differ per call.
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
        "plugin at '{}' is not a WASM plugin",
        path
      )));
    }

    let runtime = PluginCache::compile_with_limits(
      &record.wasm_bytes,
      memory_limit_bytes,
      1_000_000, // default fuel limit
    )
    .map_err(|error| {
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
