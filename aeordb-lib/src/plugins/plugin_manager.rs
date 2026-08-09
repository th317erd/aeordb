use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{PluginMetadata, PluginType};
use super::wasm_runtime::{DEFAULT_MEMORY_LIMIT_BYTES, WasmPluginRuntime};
use crate::engine::cache::{Cache, CleanCache};
use crate::engine::cache_loaders::{ApiKeyLoader, GroupLoader};
use crate::engine::directory_ops::BufferedFileTransform;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::{DirectoryOps, EngineError, NamespaceMutationKind, RequestContext, StorageEngine};
use crate::engine::system_store;

pub const PLUGIN_BINARY_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const PLUGIN_RECORD_MAX_BYTES: u64 = 48 * 1024 * 1024;
const PLUGIN_METADATA_FIELD_MAX_BYTES: usize = 4096;

/// A first-party plugin embedded into the AeorDB binary.
#[derive(Debug, Clone, Copy)]
pub struct BundledPlugin {
  pub plugin_id: &'static str,
  pub name: &'static str,
  pub path: &'static str,
  pub version: &'static str,
  pub author: &'static str,
  /// Canonical release time used for both persistent plugin timestamps.
  pub released_at_millis: i64,
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
    released_at_millis: 1_780_621_005_000,
    wasm_bytes: include_bytes!("bundled/extract.wasm"),
  },
  BundledPlugin {
    plugin_id: "/org/aeordev/aeordb/plugins/jq",
    name: "jq",
    path: "jq",
    version: "0.1.0",
    author: "AeorDB",
    released_at_millis: 1_780_621_005_000,
    wasm_bytes: include_bytes!("bundled/jq.wasm"),
  },
];

fn checksum_for_bytes(bytes: &[u8]) -> String {
  format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn default_updated_at() -> DateTime<Utc> {
  Utc::now()
}

fn validate_text_field(field: &str, value: &str, allow_empty: bool) -> Result<(), String> {
  if !allow_empty && value.is_empty() {
    return Err(format!("plugin {field} must not be empty"));
  }
  if value.len() > PLUGIN_METADATA_FIELD_MAX_BYTES {
    return Err(format!("plugin {field} is {} bytes, exceeding the {PLUGIN_METADATA_FIELD_MAX_BYTES}-byte limit", value.len(),));
  }
  if value.chars().any(char::is_control) {
    return Err(format!("plugin {field} contains control characters"));
  }
  Ok(())
}

fn validate_plugin_key(path: &str) -> Result<(), String> {
  validate_text_field("path", path, false)?;
  if path == "." || path == ".." {
    return Err(format!("plugin path '{path}' is reserved"));
  }
  if path.contains("::") {
    return Err("plugin path contains the reserved '::' storage escape".to_string());
  }
  Ok(())
}

fn validate_plugin_input(
  name: &str,
  path: &str,
  wasm_bytes: &[u8],
  version: Option<&str>,
  author: Option<&str>,
) -> Result<(), PluginManagerError> {
  validate_text_field("name", name, false).map_err(PluginManagerError::InvalidPlugin)?;
  validate_plugin_key(path).map_err(PluginManagerError::InvalidPlugin)?;
  if let Some(version) = version {
    validate_text_field("version", version, false).map_err(PluginManagerError::InvalidPlugin)?;
  }
  if let Some(author) = author {
    validate_text_field("author", author, false).map_err(PluginManagerError::InvalidPlugin)?;
  }
  if wasm_bytes.is_empty() {
    return Err(PluginManagerError::InvalidPlugin("plugin body must not be empty".to_string()));
  }
  if wasm_bytes.len() > PLUGIN_BINARY_MAX_BYTES {
    return Err(PluginManagerError::ResourceExhausted(format!(
      "plugin body is {} bytes, exceeding the {PLUGIN_BINARY_MAX_BYTES}-byte limit",
      wasm_bytes.len(),
    )));
  }
  Ok(())
}

fn bundled_version_can_replace(bundled_version: &str, current_version: Option<&str>) -> bool {
  let Ok(bundled) = Version::parse(bundled_version) else {
    return false;
  };

  match current_version {
    Some(current_version) => Version::parse(current_version).map(|current| bundled >= current).unwrap_or(false),
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

  fn validate_for_path(&mut self, expected_path: &str, allow_legacy_empty_checksum: bool) -> Result<(), String> {
    validate_text_field("ID", &self.plugin_id, false)?;
    validate_text_field("name", &self.name, false)?;
    validate_plugin_key(&self.path)?;
    if self.path != expected_path {
      return Err(format!("plugin record path '{}' does not match storage key '{expected_path}'", self.path));
    }
    if let Some(version) = self.version.as_deref() {
      validate_text_field("version", version, false)?;
    }
    if let Some(author) = self.author.as_deref() {
      validate_text_field("author", author, false)?;
    }
    if self.wasm_bytes.is_empty() {
      return Err("plugin body must not be empty".to_string());
    }
    if self.wasm_bytes.len() > PLUGIN_BINARY_MAX_BYTES {
      return Err(format!("plugin body is {} bytes, exceeding the {PLUGIN_BINARY_MAX_BYTES}-byte limit", self.wasm_bytes.len(),));
    }

    let expected_checksum = checksum_for_bytes(&self.wasm_bytes);
    if self.checksum.is_empty() && allow_legacy_empty_checksum {
      self.checksum = expected_checksum;
    } else if self.checksum != expected_checksum {
      return Err(format!("plugin checksum '{}' does not match its stored body", self.checksum));
    }
    Ok(())
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
      checksum: if self.checksum.is_empty() { checksum_for_bytes(&self.wasm_bytes) } else { self.checksum.clone() },
      updated_at: self.updated_at,
    }
  }
}

fn decode_stored_plugin_record(expected_path: &str, encoded: &[u8]) -> Result<PluginRecord, EngineError> {
  if encoded.len() as u64 > PLUGIN_RECORD_MAX_BYTES {
    return Err(EngineError::ResourceExhausted(format!(
      "stored plugin record for '{expected_path}' is {} bytes, exceeding the {PLUGIN_RECORD_MAX_BYTES}-byte limit",
      encoded.len(),
    )));
  }
  let mut record: PluginRecord = serde_json::from_slice(encoded)
    .map_err(|error| EngineError::JsonParseError(format!("plugin record '{expected_path}' is malformed: {error}")))?;
  record
    .validate_for_path(expected_path, true)
    .map_err(|reason| EngineError::JsonParseError(format!("plugin record '{expected_path}' is invalid: {reason}")))?;
  Ok(record)
}

fn encode_plugin_record(record: &PluginRecord) -> Result<Vec<u8>, EngineError> {
  let encoded = serde_json::to_vec(record).map_err(|error| EngineError::JsonParseError(format!("plugin serialization failed: {error}")))?;
  if encoded.len() as u64 > PLUGIN_RECORD_MAX_BYTES {
    return Err(EngineError::ResourceExhausted(format!(
      "plugin record for '{}' is {} bytes, exceeding the {PLUGIN_RECORD_MAX_BYTES}-byte limit",
      record.path,
      encoded.len(),
    )));
  }
  Ok(encoded)
}

fn plugin_storage_error(error: EngineError) -> PluginManagerError {
  match error {
    EngineError::ResourceExhausted(message) => PluginManagerError::ResourceExhausted(message),
    error => PluginManagerError::Storage(error.to_string()),
  }
}

/// A cached compiled WASM runtime keyed by plugin path and verified checksum.
///
/// The `WasmPluginRuntime` holds a wasmi `Engine` + `Module`. The `Module`
/// is the parsed/validated WASM — that parsing is the expensive step we want
/// to avoid on every invocation. The runtime is reusable because `call_handle`
/// creates a fresh `Store` per invocation (no shared mutable state).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PluginCacheKey {
  path: String,
  checksum: String,
}

struct PluginCache {
  entries: CleanCache<PluginCacheKey, Arc<WasmPluginRuntime>>,
}

impl PluginCache {
  fn new(engine: &StorageEngine) -> Self {
    // Compiled modules are parser/plugin-owned clean derived state, not query plans.
    // Keep their internal retention ceiling conservative without inventing a
    // public configuration property outside the frozen v4 registry.
    let max_bytes = engine
      .memory_coordinator()
      .snapshot()
      .ok()
      .and_then(|snapshot| snapshot.policy)
      .map_or(0, |policy| (256 * 1024 * 1024).min(policy.hard_limit_bytes / 32));
    PluginCache { entries: CleanCache::new_bounded((*engine.memory_coordinator()).clone(), MemoryOwner::ParserPlugin, max_bytes) }
  }

  fn compiled_weight(wasm_bytes: usize) -> Result<u64, PluginManagerError> {
    wasm_bytes
      .checked_mul(16)
      .and_then(|bytes| bytes.checked_add(1024 * 1024))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| PluginManagerError::ResourceExhausted("compiled plugin cache estimate overflow".to_string()))
  }

  /// Get a cached runtime, or compile + cache it from the given WASM bytes.
  fn get_or_compile(&mut self, path: &str, checksum: &str, wasm_bytes: &[u8]) -> Result<Arc<WasmPluginRuntime>, PluginManagerError> {
    let key = PluginCacheKey { path: path.to_string(), checksum: checksum.to_string() };
    if let Some(runtime) =
      self.entries.get(&key).map_err(|error| PluginManagerError::ExecutionFailed(format!("plugin cache read failed: {error}")))?
    {
      return Ok(runtime);
    }
    let runtime = Arc::new(
      WasmPluginRuntime::new(wasm_bytes)
        .map_err(|error| PluginManagerError::ExecutionFailed(format!("failed to load WASM module: {error}")))?,
    );
    let weight = Self::compiled_weight(wasm_bytes.len())?;
    self
      .entries
      .insert_with_weight(key, Arc::clone(&runtime), weight)
      .map_err(|error| PluginManagerError::ExecutionFailed(format!("plugin cache insertion failed: {error}")))?;
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
  fn invalidate(&mut self, path: &str) -> Result<(), PluginManagerError> {
    self
      .entries
      .remove_where(|key| key.path == path)
      .map(|_| ())
      .map_err(|error| PluginManagerError::ExecutionFailed(format!("plugin cache invalidation failed: {error}")))
  }
}

/// Manages the lifecycle of deployed plugins backed by the StorageEngine.
pub struct PluginManager {
  engine: std::sync::Arc<StorageEngine>,
  /// Cache of compiled WASM runtimes keyed by plugin path and content identity.
  /// Post-ack invalidation releases obsolete derived state eagerly, while the
  /// checksum key prevents stale execution across independent managers.
  cache: Mutex<PluginCache>,
}

pub(crate) struct AccountedPluginOutput {
  bytes: Vec<u8>,
  _reservation: MemoryReservation,
}

pub(crate) struct AccountedPluginList {
  items: Vec<PluginMetadata>,
  reservation: MemoryReservation,
}

enum BundledPluginInstallOutcome {
  Stored(PluginRecord),
  Unchanged,
  NeedsStore,
  OlderStoredVersion(Option<String>),
  OccupiedByDifferentID(String),
}

impl AccountedPluginList {
  pub(crate) fn as_slice(&self) -> &[PluginMetadata] {
    &self.items
  }

  pub(crate) fn into_parts(self) -> (Vec<PluginMetadata>, MemoryReservation) {
    (self.items, self.reservation)
  }

  fn into_items(self) -> Vec<PluginMetadata> {
    self.items
  }
}

impl AccountedPluginOutput {
  pub(crate) fn as_slice(&self) -> &[u8] {
    &self.bytes
  }

  pub(crate) fn into_parts(self) -> (Vec<u8>, MemoryReservation) {
    (self.bytes, self._reservation)
  }

  fn into_bytes(self) -> Vec<u8> {
    self.bytes
  }
}

impl PluginManager {
  /// Create a new PluginManager sharing the given StorageEngine.
  pub fn new(engine: std::sync::Arc<StorageEngine>) -> Self {
    let cache = Mutex::new(PluginCache::new(&engine));
    Self { engine, cache }
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
      match self.install_bundled_plugin(bundled)? {
        BundledPluginInstallOutcome::Stored(record) => installed_or_updated.push(record.to_metadata()),
        BundledPluginInstallOutcome::Unchanged => {}
        BundledPluginInstallOutcome::NeedsStore => {
          return Err(PluginManagerError::Storage(
            format!("bundled plugin '{}' remained undecided after validated storage", bundled.path,),
          ));
        }
        BundledPluginInstallOutcome::OlderStoredVersion(current_version) => {
          tracing::warn!(
            path = %bundled.path,
            bundled_version = %bundled.version,
            current_version = ?current_version,
            "Bundled plugin version is older than stored plugin version; leaving stored plugin untouched"
          );
        }
        BundledPluginInstallOutcome::OccupiedByDifferentID(current_plugin_id) => {
          tracing::warn!(
            path = %bundled.path,
            bundled_plugin_id = %bundled.plugin_id,
            current_plugin_id = %current_plugin_id,
            "Bundled plugin path is occupied by a different plugin ID; leaving it untouched"
          );
        }
      }
    }

    Ok(installed_or_updated)
  }

  fn install_bundled_plugin(&self, bundled: &BundledPlugin) -> Result<BundledPluginInstallOutcome, PluginManagerError> {
    validate_plugin_input(bundled.name, bundled.path, bundled.wasm_bytes, Some(bundled.version), Some(bundled.author))?;
    let bundled_released_at = DateTime::<Utc>::from_timestamp_millis(bundled.released_at_millis)
      .ok_or_else(|| PluginManagerError::InvalidPlugin(format!("bundled plugin '{}' has an invalid release timestamp", bundled.path)))?;
    let outcome = self.apply_bundled_plugin(bundled, bundled_released_at, false)?;
    if !matches!(&outcome, BundledPluginInstallOutcome::NeedsStore) {
      return Ok(outcome);
    }

    let _plugin_memory = self.validate_and_reserve_deployment(PluginType::Wasm, bundled.wasm_bytes)?;
    let outcome = self.apply_bundled_plugin(bundled, bundled_released_at, true)?;
    if matches!(&outcome, BundledPluginInstallOutcome::Stored(_)) {
      self.invalidate_cached_runtime_after_ack(bundled.path, "bundled plugin deployment");
    }
    Ok(outcome)
  }

  fn apply_bundled_plugin(
    &self,
    bundled: &BundledPlugin,
    bundled_released_at: DateTime<Utc>,
    allow_store: bool,
  ) -> Result<BundledPluginInstallOutcome, PluginManagerError> {
    let bundled_plugin_id = bundled.plugin_id.to_string();
    validate_text_field("ID", &bundled_plugin_id, false).map_err(PluginManagerError::InvalidPlugin)?;
    let bundled_name = bundled.name.to_string();
    let bundled_path = bundled.path.to_string();
    let bundled_version = bundled.version.to_string();
    let bundled_author = bundled.author.to_string();
    let bundled_wasm_bytes = bundled.wasm_bytes;
    let bundled_checksum = checksum_for_bytes(bundled_wasm_bytes);
    let storage_path = system_store::plugin_storage_path(&bundled_path);
    let ctx = RequestContext::system();

    let outcome = DirectoryOps::new(&self.engine)
      .transform_file_buffered(
        &ctx,
        &storage_path,
        Some("application/octet-stream"),
        PLUGIN_RECORD_MAX_BYTES,
        NamespaceMutationKind::SystemWrite,
        move |current| {
          let existing = current.map(|encoded| decode_stored_plugin_record(&bundled_path, encoded)).transpose()?;
          if let Some(existing) = existing.as_ref() {
            let is_current_bundled_plugin = existing.plugin_id == bundled_plugin_id;
            let version_allows_replace = bundled_version_can_replace(&bundled_version, existing.version.as_deref());
            let bytes_or_metadata_differ = existing.checksum != bundled_checksum
              || existing.version.as_deref() != Some(bundled_version.as_str())
              || existing.author.as_deref() != Some(bundled_author.as_str())
              || existing.name != bundled_name
              || existing.plugin_type != PluginType::Wasm
              || existing.created_at != bundled_released_at
              || existing.updated_at != bundled_released_at;
            let is_legacy_canonical_bundle = !is_current_bundled_plugin
              && existing.author.as_deref() == Some(bundled_author.as_str())
              && existing.name == bundled_name
              && existing.version.as_deref() == Some(bundled_version.as_str())
              && existing.checksum == bundled_checksum
              && existing.plugin_type == PluginType::Wasm;

            if is_current_bundled_plugin && !version_allows_replace {
              return Ok(BufferedFileTransform::Keep(BundledPluginInstallOutcome::OlderStoredVersion(existing.version.clone())));
            }
            if !is_current_bundled_plugin && !is_legacy_canonical_bundle {
              return Ok(BufferedFileTransform::Keep(BundledPluginInstallOutcome::OccupiedByDifferentID(existing.plugin_id.clone())));
            }
            if is_current_bundled_plugin && !bytes_or_metadata_differ {
              return Ok(BufferedFileTransform::Keep(BundledPluginInstallOutcome::Unchanged));
            }
          }
          if !allow_store {
            return Ok(BufferedFileTransform::Keep(BundledPluginInstallOutcome::NeedsStore));
          }

          let mut record = PluginRecord {
            plugin_id: bundled_plugin_id,
            name: bundled_name,
            path: bundled_path.clone(),
            plugin_type: PluginType::Wasm,
            wasm_bytes: bundled_wasm_bytes.to_vec(),
            created_at: bundled_released_at,
            version: Some(bundled_version),
            author: Some(bundled_author),
            checksum: bundled_checksum,
            updated_at: bundled_released_at,
          };
          record
            .validate_for_path(&bundled_path, false)
            .map_err(|reason| EngineError::InvalidInput(format!("bundled plugin record is invalid: {reason}")))?;
          let data = encode_plugin_record(&record)?;
          Ok(BufferedFileTransform::Replace { data, output: BundledPluginInstallOutcome::Stored(record) })
        },
      )
      .map_err(plugin_storage_error)?;
    Ok(outcome)
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
    self.deploy_plugin_record(name, path, plugin_type, wasm_bytes, version, author)
  }

  fn deploy_plugin_record(
    &self,
    name: &str,
    path: &str,
    plugin_type: PluginType,
    wasm_bytes: Vec<u8>,
    version: Option<String>,
    author: Option<String>,
  ) -> Result<PluginRecord, PluginManagerError> {
    validate_plugin_input(name, path, &wasm_bytes, version.as_deref(), author.as_deref())?;
    let _plugin_memory = self.validate_and_reserve_deployment(plugin_type.clone(), &wasm_bytes)?;
    let candidate_plugin_id = Uuid::new_v4().to_string();
    let now = Utc::now();
    let name = name.to_string();
    let path = path.to_string();
    let storage_path = system_store::plugin_storage_path(&path);
    let ctx = RequestContext::system();
    let record = DirectoryOps::new(&self.engine)
      .transform_file_buffered(
        &ctx,
        &storage_path,
        Some("application/octet-stream"),
        PLUGIN_RECORD_MAX_BYTES,
        NamespaceMutationKind::SystemWrite,
        move |current| {
          let existing = current.map(|encoded| decode_stored_plugin_record(&path, encoded)).transpose()?;
          let mut record = PluginRecord {
            plugin_id: existing.as_ref().map(|record| record.plugin_id.clone()).unwrap_or(candidate_plugin_id),
            name,
            path: path.clone(),
            plugin_type,
            wasm_bytes,
            created_at: existing.as_ref().map(|record| record.created_at).unwrap_or(now),
            version,
            author,
            checksum: String::new(),
            updated_at: now,
          };
          record.normalize_metadata();
          record
            .validate_for_path(&path, false)
            .map_err(|reason| EngineError::InvalidInput(format!("plugin record is invalid: {reason}")))?;
          let data = encode_plugin_record(&record)?;
          Ok(BufferedFileTransform::Replace { data, output: record })
        },
      )
      .map_err(plugin_storage_error)?;

    self.invalidate_cached_runtime_after_ack(&record.path, "plugin deployment");

    tracing::info!(
      path = %record.path,
      plugin_type = ?record.plugin_type,
      plugin_id = %record.plugin_id,
      "Plugin deployed"
    );

    Ok(record)
  }

  /// Retrieve a deployed plugin by its path.
  pub fn get_plugin(&self, path: &str) -> Result<Option<PluginRecord>, PluginManagerError> {
    validate_plugin_key(path).map_err(PluginManagerError::InvalidPlugin)?;
    let storage_path = system_store::plugin_storage_path(path);
    match DirectoryOps::new(&self.engine).read_file_buffered_bounded(&storage_path, PLUGIN_RECORD_MAX_BYTES) {
      Ok(encoded) => decode_stored_plugin_record(path, &encoded).map(Some).map_err(plugin_storage_error),
      Err(EngineError::NotFound(_)) => Ok(None),
      Err(error) => Err(plugin_storage_error(error)),
    }
  }

  /// List metadata for all deployed plugins.
  pub fn list_plugins(&self) -> Result<Vec<PluginMetadata>, PluginManagerError> {
    self.list_plugins_accounted().map(AccountedPluginList::into_items)
  }

  pub(crate) fn list_plugins_accounted(&self) -> Result<AccountedPluginList, PluginManagerError> {
    const PAGE_SIZE: usize = 64;
    const LIST_WORKSPACE_BYTES: u64 = 256 * 1024;
    let coordinator = self.engine.memory_coordinator();
    let mut reservation =
      coordinator.reserve(MemoryOwner::ParserPlugin, LIST_WORKSPACE_BYTES, AdmissionClass::Workload).map_err(plugin_memory_error)?;
    let mut plugins = Vec::new();
    let mut offset = 0usize;
    loop {
      let (keys, has_more) = system_store::list_plugin_keys_window(&self.engine, offset, PAGE_SIZE)
        .map_err(|error| PluginManagerError::Storage(error.to_string()))?;
      if keys.is_empty() {
        break;
      }
      offset = offset.saturating_add(keys.len());
      for key in keys {
        let storage_path = system_store::plugin_storage_path(&key);
        let Some(metadata) =
          DirectoryOps::new(&self.engine).get_metadata(&storage_path).map_err(|error| PluginManagerError::Storage(error.to_string()))?
        else {
          continue;
        };
        if metadata.total_size > PLUGIN_RECORD_MAX_BYTES {
          return Err(PluginManagerError::ResourceExhausted(format!(
            "stored plugin record for '{key}' is {} bytes, exceeding the {PLUGIN_RECORD_MAX_BYTES}-byte limit",
            metadata.total_size,
          )));
        }
        let stored_bytes = usize::try_from(metadata.total_size)
          .map_err(|_| PluginManagerError::ResourceExhausted("stored plugin size exceeds this platform's address space".to_string()))?;
        let record_work_bytes = stored_bytes
          .checked_mul(2)
          .and_then(|bytes| bytes.checked_add(64 * 1024))
          .and_then(|bytes| u64::try_from(bytes).ok())
          .ok_or_else(|| PluginManagerError::ResourceExhausted("plugin listing record estimate overflow".to_string()))?;
        let _record_memory =
          coordinator.reserve(MemoryOwner::ParserPlugin, record_work_bytes, AdmissionClass::Workload).map_err(plugin_memory_error)?;
        let Some(mut record) = self.get_plugin(&key)? else {
          continue;
        };
        record.normalize_metadata();
        let metadata = record.to_metadata();
        reservation.grow(plugin_metadata_retained_bytes(&metadata)?).map_err(plugin_memory_error)?;
        plugins.push(metadata);
      }
      if !has_more {
        break;
      }
    }
    Ok(AccountedPluginList { items: plugins, reservation })
  }

  /// Remove a deployed plugin by its path.
  ///
  /// Returns true if the plugin existed and was removed, false if not found.
  /// Invalidates any cached runtime for this path.
  pub fn remove_plugin(&self, path: &str) -> Result<bool, PluginManagerError> {
    validate_plugin_key(path).map_err(PluginManagerError::InvalidPlugin)?;
    let ctx = RequestContext::system();
    let storage_path = system_store::plugin_storage_path(path);
    let removed = match DirectoryOps::new(&self.engine).delete_file(&ctx, &storage_path) {
      Ok(()) => true,
      Err(EngineError::NotFound(_)) => false,
      Err(error) => return Err(plugin_storage_error(error)),
    };
    if removed {
      self.invalidate_cached_runtime_after_ack(path, "plugin removal");
    }
    Ok(removed)
  }

  /// Get a cached compiled runtime for a plugin, or compile and cache it.
  fn get_cached_runtime(&self, path: &str, checksum: &str, wasm_bytes: &[u8]) -> Result<Arc<WasmPluginRuntime>, PluginManagerError> {
    let mut cache = self.cache.lock().map_err(|e| PluginManagerError::ExecutionFailed(format!("plugin cache lock poisoned: {}", e)))?;
    cache.get_or_compile(path, checksum, wasm_bytes).map_err(|error| {
      tracing::error!(path = %path, error = %error, "Failed to load WASM module");
      metrics::counter!(crate::metrics::definitions::PLUGIN_ERRORS_TOTAL, "error_type" => "load_failed").increment(1);
      error
    })
  }

  fn invalidate_cached_runtime(&self, path: &str) -> Result<(), PluginManagerError> {
    self.cache.lock().map_err(|error| PluginManagerError::ExecutionFailed(format!("plugin cache lock poisoned: {error}")))?.invalidate(path)
  }

  fn invalidate_cached_runtime_after_ack(&self, path: &str, operation: &str) {
    if let Err(error) = self.invalidate_cached_runtime(path) {
      tracing::warn!(path = %path, operation, error = %error, "Acknowledged plugin mutation could not evict derived compiled state");
      metrics::counter!(crate::metrics::definitions::PLUGIN_ERRORS_TOTAL, "error_type" => "cache_invalidation_failed").increment(1);
    }
  }

  fn validate_and_reserve_deployment(&self, plugin_type: PluginType, plugin_bytes: &[u8]) -> Result<MemoryReservation, PluginManagerError> {
    let encoded_upper_bound = plugin_bytes
      .len()
      .checked_mul(4)
      .and_then(|bytes| bytes.checked_add(128 * 1024))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| PluginManagerError::ResourceExhausted("plugin record size estimate overflow".to_string()))?
      .min(PLUGIN_RECORD_MAX_BYTES);
    let publication_workspace = PLUGIN_RECORD_MAX_BYTES
      .checked_add(PLUGIN_BINARY_MAX_BYTES as u64)
      .and_then(|bytes| encoded_upper_bound.checked_mul(2).and_then(|encoded| bytes.checked_add(encoded)))
      .and_then(|bytes| bytes.checked_add(1024 * 1024))
      .ok_or_else(|| PluginManagerError::ResourceExhausted("plugin publication memory estimate overflow".to_string()))?;
    let compile_workspace = plugin_bytes
      .len()
      .checked_mul(16)
      .and_then(|bytes| bytes.checked_add(1024 * 1024))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| PluginManagerError::ResourceExhausted("compiled plugin memory estimate overflow".to_string()))?;
    let reservation = self
      .engine
      .memory_coordinator()
      .reserve(MemoryOwner::ParserPlugin, publication_workspace.max(compile_workspace), AdmissionClass::Workload)
      .map_err(plugin_memory_error)?;

    if plugin_type == PluginType::Wasm {
      WasmPluginRuntime::new(plugin_bytes)
        .map_err(|error| PluginManagerError::InvalidPlugin(format!("WASM validation failed: {error}")))?;
    }
    Ok(reservation)
  }

  fn reserve_invocation_memory(
    &self,
    wasm_bytes: usize,
    request_bytes: usize,
    guest_memory_limit: usize,
  ) -> Result<MemoryReservation, PluginManagerError> {
    let bytes = guest_memory_limit
      .checked_mul(4)
      .and_then(|bytes| wasm_bytes.checked_mul(16).and_then(|compiled| bytes.checked_add(compiled)))
      .and_then(|bytes| request_bytes.checked_mul(2).and_then(|request| bytes.checked_add(request)))
      .and_then(|bytes| bytes.checked_add(1024 * 1024))
      .and_then(|bytes| u64::try_from(bytes).ok())
      .ok_or_else(|| PluginManagerError::ResourceExhausted("plugin invocation memory estimate overflow".to_string()))?;
    self.engine.memory_coordinator().reserve(MemoryOwner::ParserPlugin, bytes, AdmissionClass::Workload).map_err(plugin_memory_error)
  }

  fn load_plugin_for_invocation(
    &self,
    path: &str,
    request_bytes: usize,
    guest_memory_limit: usize,
  ) -> Result<(PluginRecord, MemoryReservation), PluginManagerError> {
    let storage_path = system_store::plugin_storage_path(path);
    let metadata = DirectoryOps::new(&self.engine)
      .get_metadata(&storage_path)
      .map_err(|error| PluginManagerError::Storage(error.to_string()))?
      .ok_or_else(|| PluginManagerError::NotFound(path.to_string()))?;
    if metadata.total_size > PLUGIN_RECORD_MAX_BYTES {
      return Err(PluginManagerError::ResourceExhausted(format!(
        "stored plugin record for '{path}' is {} bytes, exceeding the {PLUGIN_RECORD_MAX_BYTES}-byte limit",
        metadata.total_size,
      )));
    }
    let stored_bytes = usize::try_from(metadata.total_size)
      .map_err(|_| PluginManagerError::ResourceExhausted("stored plugin size exceeds this platform's address space".to_string()))?;
    let reservation = self.reserve_invocation_memory(stored_bytes, request_bytes, guest_memory_limit)?;
    let record = self.get_plugin(path)?.ok_or_else(|| PluginManagerError::NotFound(path.to_string()))?;
    if record.plugin_type != PluginType::Wasm {
      return Err(PluginManagerError::InvalidPlugin(format!("plugin at '{}' is not a WASM plugin", path)));
    }
    Ok((record, reservation))
  }

  fn retain_plugin_output(mut reservation: MemoryReservation, bytes: Vec<u8>) -> Result<AccountedPluginOutput, PluginManagerError> {
    let retained_bytes = bytes
      .len()
      .checked_mul(2)
      .and_then(|value| value.checked_add(64 * 1024))
      .and_then(|value| u64::try_from(value).ok())
      .ok_or_else(|| PluginManagerError::ResourceExhausted("plugin output memory estimate overflow".to_string()))?
      .max(1);
    if retained_bytes < reservation.bytes() {
      reservation
        .shrink(reservation.bytes() - retained_bytes)
        .map_err(|error| PluginManagerError::ExecutionFailed(format!("plugin output memory accounting failed: {error}")))?;
    } else if retained_bytes > reservation.bytes() {
      reservation.grow(retained_bytes - reservation.bytes()).map_err(plugin_memory_error)?;
    }
    Ok(AccountedPluginOutput { bytes, _reservation: reservation })
  }

  /// Instantiate and invoke a deployed WASM plugin.
  #[tracing::instrument(skip(self, request_bytes), fields(path = %path, request_size = request_bytes.len()))]
  pub fn invoke_wasm_plugin(&self, path: &str, request_bytes: &[u8]) -> Result<Vec<u8>, PluginManagerError> {
    self.invoke_wasm_plugin_accounted(path, request_bytes).map(AccountedPluginOutput::into_bytes)
  }

  pub(crate) fn invoke_wasm_plugin_accounted(&self, path: &str, request_bytes: &[u8]) -> Result<AccountedPluginOutput, PluginManagerError> {
    let start = std::time::Instant::now();

    let (record, memory) = self.load_plugin_for_invocation(path, request_bytes.len(), DEFAULT_MEMORY_LIMIT_BYTES)?;
    let runtime = self.get_cached_runtime(path, &record.checksum, &record.wasm_bytes)?;

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

    let bytes = result?;
    Self::retain_plugin_output(memory, bytes)
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
    let group_cache = engine.group_cache.clone();
    let api_key_cache = engine.api_key_cache.clone();
    self.invoke_wasm_plugin_with_auth(path, request_bytes, engine, ctx, group_cache, api_key_cache)
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
    self.invoke_wasm_plugin_with_authority_engines(path, request_bytes, engine.clone(), engine, ctx, group_cache, api_key_cache)
  }

  /// Invoke a plugin when API-key authority belongs to a different engine.
  #[tracing::instrument(skip(self, request_bytes, engine, api_key_engine, ctx, group_cache, api_key_cache), fields(path = %path, request_size = request_bytes.len()))]
  pub fn invoke_wasm_plugin_with_authority_engines(
    &self,
    path: &str,
    request_bytes: &[u8],
    engine: std::sync::Arc<StorageEngine>,
    api_key_engine: std::sync::Arc<StorageEngine>,
    ctx: RequestContext,
    group_cache: Arc<Cache<GroupLoader>>,
    api_key_cache: Arc<Cache<ApiKeyLoader>>,
  ) -> Result<Vec<u8>, PluginManagerError> {
    self
      .invoke_wasm_plugin_with_auth_accounted(path, request_bytes, engine, api_key_engine, ctx, group_cache, api_key_cache)
      .map(AccountedPluginOutput::into_bytes)
  }

  pub(crate) fn invoke_wasm_plugin_with_auth_accounted(
    &self,
    path: &str,
    request_bytes: &[u8],
    engine: std::sync::Arc<StorageEngine>,
    api_key_engine: std::sync::Arc<StorageEngine>,
    ctx: RequestContext,
    group_cache: Arc<Cache<GroupLoader>>,
    api_key_cache: Arc<Cache<ApiKeyLoader>>,
  ) -> Result<AccountedPluginOutput, PluginManagerError> {
    let start = std::time::Instant::now();

    let (record, memory) = self.load_plugin_for_invocation(path, request_bytes.len(), DEFAULT_MEMORY_LIMIT_BYTES)?;
    let runtime = self.get_cached_runtime(path, &record.checksum, &record.wasm_bytes)?;

    let result =
      runtime.call_handle_with_authority_engines(request_bytes, engine, api_key_engine, ctx, group_cache, api_key_cache).map_err(|error| {
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

    Self::retain_plugin_output(memory, result?)
  }

  /// Invoke a WASM plugin with custom memory limits (for parser plugins).
  /// Custom-limit invocations bypass the cache since limits may differ per call.
  pub fn invoke_wasm_plugin_with_limits(
    &self,
    path: &str,
    request_bytes: &[u8],
    memory_limit_bytes: usize,
  ) -> Result<Vec<u8>, PluginManagerError> {
    self.invoke_wasm_plugin_with_limits_accounted(path, request_bytes, memory_limit_bytes).map(AccountedPluginOutput::into_bytes)
  }

  pub(crate) fn invoke_wasm_plugin_with_limits_accounted(
    &self,
    path: &str,
    request_bytes: &[u8],
    memory_limit_bytes: usize,
  ) -> Result<AccountedPluginOutput, PluginManagerError> {
    let (record, memory) = self.load_plugin_for_invocation(path, request_bytes.len(), memory_limit_bytes)?;
    let runtime = PluginCache::compile_with_limits(
      &record.wasm_bytes,
      memory_limit_bytes,
      1_000_000, // default fuel limit
    )
    .map_err(|error| PluginManagerError::ExecutionFailed(format!("failed to load WASM module: {}", error)))?;

    let bytes = runtime
      .call_handle(request_bytes)
      .map_err(|error| PluginManagerError::ExecutionFailed(format!("WASM execution failed: {}", error)))?;
    Self::retain_plugin_output(memory, bytes)
  }
}

fn plugin_memory_error(error: MemoryCoordinatorError) -> PluginManagerError {
  match error {
    MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
    | MemoryCoordinatorError::PolicyUnavailable => PluginManagerError::ResourceExhausted(error.to_string()),
    _ => PluginManagerError::ExecutionFailed(format!("plugin memory accounting failed: {error}")),
  }
}

fn plugin_metadata_retained_bytes(metadata: &PluginMetadata) -> Result<u64, PluginManagerError> {
  let strings = metadata
    .plugin_id
    .len()
    .checked_add(metadata.name.len())
    .and_then(|bytes| bytes.checked_add(metadata.path.len()))
    .and_then(|bytes| bytes.checked_add(metadata.version.as_ref().map_or(0, String::len)))
    .and_then(|bytes| bytes.checked_add(metadata.author.as_ref().map_or(0, String::len)))
    .and_then(|bytes| bytes.checked_add(metadata.checksum.len()))
    .ok_or_else(|| PluginManagerError::ResourceExhausted("plugin metadata size overflow".to_string()))?;
  strings
    .checked_mul(8)
    .and_then(|bytes| bytes.checked_add(std::mem::size_of::<PluginMetadata>()))
    .and_then(|bytes| bytes.checked_add(4096))
    .and_then(|bytes| u64::try_from(bytes).ok())
    .ok_or_else(|| PluginManagerError::ResourceExhausted("plugin metadata retained estimate overflow".to_string()))
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

  #[error("resource exhausted: {0}")]
  ResourceExhausted(String),

  #[error("storage error: {0}")]
  Storage(String),
}
