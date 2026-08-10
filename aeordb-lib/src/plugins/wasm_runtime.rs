use std::sync::Arc;

use base64::Engine as _;
use wasmi::{Caller, Config, Engine, Extern, Linker, Memory, MemoryType, Module, Store, StoreLimits, StoreLimitsBuilder};

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineError;
use crate::engine::api_key_rules::{check_operation_permitted, is_ancestor_of_any_rule, match_rules, operation_to_flag_char};
use crate::engine::cache::Cache;
use crate::engine::cache_loaders::{ApiKeyLoader, GroupLoader};
use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};
use crate::engine::query_engine::{
  parse_where_clause, AggregateQuery, ExplainMode, Query, QueryEngine, QueryStrategy, SortDirection, SortField,
};
use crate::engine::range_extract::{extract_range_by_path, RangeExtractionRequest, RangeMode};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::SystemFamilyPolicyResolver;

/// Default maximum memory in bytes (16 MB).
pub(crate) const DEFAULT_MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Default fuel budget for execution metering.
const DEFAULT_FUEL_LIMIT: u64 = 10_000_000;

/// Fixed offset in guest memory where host function responses are written.
/// The guest SDK reads response data from this offset.
///
/// **Overlap constraint**: The request bytes are also written starting at
/// offset 0 (see `call_handle` / `call_handle_with_context`). This means
/// the host response overwrites the request region. Guests MUST finish
/// reading and parsing the request before calling any host function,
/// because the first host function response will clobber the request data
/// at this offset. The guest SDK guarantees this by parsing the request
/// JSON into owned structures before invoking any host calls.
const HOST_RESPONSE_OFFSET: usize = 0;

/// Error type for WASM plugin operations.
#[derive(Debug, thiserror::Error)]
pub enum WasmRuntimeError {
  #[error("failed to compile WASM module: {0}")]
  CompilationFailed(String),

  #[error("failed to instantiate WASM module: {0}")]
  InstantiationFailed(String),

  #[error("WASM execution trapped: {0}")]
  Trap(String),

  #[error("exported function not found: {0}")]
  ExportNotFound(String),

  #[error("memory limit exceeded")]
  MemoryLimitExceeded,

  #[error("fuel limit exceeded (execution too long)")]
  FuelLimitExceeded,

  #[error("memory access out of bounds")]
  MemoryOutOfBounds,

  #[error("serialization error: {0}")]
  Serialization(String),
}

fn map_memory_instantiation_error(error: impl std::fmt::Display) -> WasmRuntimeError {
  let message = error.to_string();
  let normalized = message.to_ascii_lowercase();
  if normalized.contains("resource limiter") && normalized.contains("memory") {
    WasmRuntimeError::MemoryLimitExceeded
  } else {
    WasmRuntimeError::InstantiationFailed(message)
  }
}

/// Host state passed into the WASM Store.
struct HostState {
  /// Reference to the guest's linear memory (set after instantiation).
  memory: Option<Memory>,
  /// Storage engine for database operations (set for query plugins, None for parsers).
  engine: Option<Arc<StorageEngine>>,
  /// Storage engine that owns API-key authority. This can differ from the
  /// database engine when the server uses `file://` authentication.
  api_key_engine: Option<Arc<StorageEngine>>,
  /// Request context for permission-checked operations.
  request_context: Option<RequestContext>,
  /// Group cache for request-scoped permission checks.
  group_cache: Option<Arc<Cache<GroupLoader>>>,
  /// API key cache for scoped-key path checks.
  api_key_cache: Option<Arc<Cache<ApiKeyLoader>>>,
  /// Enforces the configured ceiling for imported and module-defined memory.
  limits: StoreLimits,
}

/// A sandboxed WASM plugin runtime powered by wasmi.
#[derive(Debug)]
pub struct WasmPluginRuntime {
  engine: Engine,
  module: Module,
  memory_limit_bytes: usize,
  fuel_limit: u64,
}

impl WasmPluginRuntime {
  fn store_limits(&self) -> StoreLimits {
    StoreLimitsBuilder::new().memory_size(self.memory_limit_bytes).build()
  }

  /// Load and validate a WASM binary, preparing it for execution.
  pub fn new(wasm_bytes: &[u8]) -> Result<Self, WasmRuntimeError> {
    Self::with_limits(wasm_bytes, DEFAULT_MEMORY_LIMIT_BYTES, DEFAULT_FUEL_LIMIT)
  }

  /// Load and validate a WASM binary with custom resource limits.
  pub fn with_limits(wasm_bytes: &[u8], memory_limit_bytes: usize, fuel_limit: u64) -> Result<Self, WasmRuntimeError> {
    let mut config = Config::default();
    config.consume_fuel(true);

    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm_bytes).map_err(|error| WasmRuntimeError::CompilationFailed(error.to_string()))?;

    Ok(Self { engine, module, memory_limit_bytes, fuel_limit })
  }

  /// Invoke the plugin's exported `handle` function.
  ///
  /// The convention is:
  ///   - The host writes the request bytes into the guest's memory.
  ///   - The host calls `handle(request_ptr, request_len)` which returns a
  ///     packed i64: high 32 bits = response pointer, low 32 bits = response length.
  ///   - The host reads the response bytes from the guest's memory.
  pub fn call_handle(&self, request_bytes: &[u8]) -> Result<Vec<u8>, WasmRuntimeError> {
    self.call_handle_inner(request_bytes, None, None, None, None, None)
  }

  /// Invoke the plugin's exported `handle` function with engine access.
  ///
  /// Same as `call_handle` but provides the `StorageEngine` and `RequestContext`
  /// to the host state, enabling the 7 database host functions to perform real
  /// operations. Used by query plugins (not parsers).
  pub fn call_handle_with_context(
    &self,
    request_bytes: &[u8],
    engine: Arc<StorageEngine>,
    ctx: RequestContext,
    group_cache: Arc<Cache<GroupLoader>>,
    api_key_cache: Arc<Cache<ApiKeyLoader>>,
  ) -> Result<Vec<u8>, WasmRuntimeError> {
    self.call_handle_with_authority_engines(request_bytes, engine.clone(), engine, ctx, group_cache, api_key_cache)
  }

  /// Invoke with separate data and API-key authority engines.
  pub fn call_handle_with_authority_engines(
    &self,
    request_bytes: &[u8],
    engine: Arc<StorageEngine>,
    api_key_engine: Arc<StorageEngine>,
    ctx: RequestContext,
    group_cache: Arc<Cache<GroupLoader>>,
    api_key_cache: Arc<Cache<ApiKeyLoader>>,
  ) -> Result<Vec<u8>, WasmRuntimeError> {
    self.call_handle_inner(request_bytes, Some(engine), Some(api_key_engine), Some(ctx), Some(group_cache), Some(api_key_cache))
  }

  fn call_handle_inner(
    &self,
    request_bytes: &[u8],
    engine: Option<Arc<StorageEngine>>,
    api_key_engine: Option<Arc<StorageEngine>>,
    request_context: Option<RequestContext>,
    group_cache: Option<Arc<Cache<GroupLoader>>>,
    api_key_cache: Option<Arc<Cache<ApiKeyLoader>>>,
  ) -> Result<Vec<u8>, WasmRuntimeError> {
    let mut store = Store::new(
      &self.engine,
      HostState { memory: None, engine, api_key_engine, request_context, group_cache, api_key_cache, limits: self.store_limits() },
    );
    store.limiter(|state| &mut state.limits);
    store.set_fuel(self.fuel_limit).map_err(|error| WasmRuntimeError::Trap(error.to_string()))?;

    let mut linker = <Linker<HostState>>::new(&self.engine);
    self.register_host_functions(&mut linker)?;

    // Provide a default "env" memory if the module imports one.
    let memory_pages = (self.memory_limit_bytes / (64 * 1024)).max(1) as u32;
    let memory_type = MemoryType::new(1, Some(memory_pages)).map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;
    let memory = Memory::new(&mut store, memory_type).map_err(map_memory_instantiation_error)?;
    linker.define("env", "memory", Extern::Memory(memory)).map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    let instance = linker
      .instantiate(&mut store, &self.module)
      .and_then(|pre_instance| pre_instance.start(&mut store))
      .map_err(map_memory_instantiation_error)?;

    // Resolve guest memory — prefer the instance's own export, fall back to the one we created.
    let guest_memory = instance.get_memory(&store, "memory").unwrap_or(memory);

    store.data_mut().memory = Some(guest_memory);

    // Write request bytes into guest memory starting at offset 0.
    let request_length = request_bytes.len();
    let memory_size = guest_memory.data_size(&store);
    if request_length > memory_size || request_length > i32::MAX as usize {
      return Err(WasmRuntimeError::MemoryLimitExceeded);
    }
    guest_memory.write(&mut store, 0, request_bytes).map_err(|_| WasmRuntimeError::MemoryOutOfBounds)?;

    // Call the exported `handle` function.
    let handle_function = instance.get_func(&store, "handle").ok_or_else(|| WasmRuntimeError::ExportNotFound("handle".to_string()))?;

    let handle_typed = handle_function
      .typed::<(i32, i32), i64>(&store)
      .map_err(|error| WasmRuntimeError::ExportNotFound(format!("handle type mismatch: {}", error)))?;

    // NOTE: Fuel exhaustion is detected via string matching on the wasmi error
    // message. This is brittle -- if wasmi changes the message format, fuel
    // exhaustion would be reported as a generic trap. Consider checking for
    // specific wasmi error variants when the wasmi API supports it.
    let result = handle_typed.call(&mut store, (0i32, request_length as i32)).map_err(|error| {
      let message = error.to_string();
      if message.contains("fuel") {
        WasmRuntimeError::FuelLimitExceeded
      } else {
        WasmRuntimeError::Trap(message)
      }
    })?;

    // Unpack the response pointer and length from the i64 result.
    let response_pointer = (result >> 32) as u32 as usize;
    let response_length = (result & 0xFFFF_FFFF) as u32 as usize;

    if response_length == 0 {
      return Ok(Vec::new());
    }

    let current_memory_size = guest_memory.data_size(&store);
    let response_end = response_pointer.checked_add(response_length).ok_or(WasmRuntimeError::MemoryOutOfBounds)?;
    if response_end > current_memory_size {
      return Err(WasmRuntimeError::MemoryOutOfBounds);
    }

    let mut response_buffer = vec![0u8; response_length];
    guest_memory.read(&store, response_pointer, &mut response_buffer).map_err(|_| WasmRuntimeError::MemoryOutOfBounds)?;

    Ok(response_buffer)
  }

  /// Register host functions that the WASM module can import.
  ///
  /// Includes the database host functions and the log_message function.
  ///
  /// Path-bearing host functions authorize the caller's request context and
  /// scoped API-key rules before touching storage.
  fn register_host_functions(&self, linker: &mut Linker<HostState>) -> Result<(), WasmRuntimeError> {
    // -----------------------------------------------------------------------
    // aeordb_read_file(ptr, len) -> i64
    // Reads a file from the database. Args: {"path": "/..."}
    // Returns: {"data": "<base64>", "content_type": "...", "size": N}
    // -----------------------------------------------------------------------
    linker
      .func_wrap("aeordb", "aeordb_read_file", |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
        let args_json = match read_guest_json(&caller, ptr, len) {
          Ok(v) => v,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        let path = match args_json.get("path").and_then(|v| v.as_str()) {
          Some(p) => p.to_string(),
          None => return write_error_response(&mut caller, "Missing 'path' argument"),
        };

        let engine = match caller.data().engine.as_ref() {
          Some(e) => Arc::clone(e),
          None => return write_error_response(&mut caller, "Database access not available in this plugin context"),
        };

        if let Err(error) = authorize_plugin_path(&caller, &path, CrudlifyOp::Read) {
          return write_error_response(&mut caller, &error.to_string());
        }

        let dir_ops = DirectoryOps::new(&engine);
        let record = match dir_ops.get_metadata(&path) {
          Ok(Some(record)) => record,
          Ok(None) => return write_error_response(&mut caller, &format!("File not found: {path}")),
          Err(error) => return write_error_response(&mut caller, &format!("Metadata failed: {error}")),
        };
        let content_type = record.content_type.unwrap_or_default();
        let response_capacity = host_response_capacity(&caller);
        let encoded_length = record
          .total_size
          .checked_add(2)
          .and_then(|bytes| bytes.checked_div(3))
          .and_then(|bytes| bytes.checked_mul(4));
        let response_length = encoded_length
          .and_then(|bytes| bytes.checked_add((content_type.len() as u64).saturating_mul(6)))
          .and_then(|bytes| bytes.checked_add(256));
        if response_length.is_none_or(|bytes| bytes > response_capacity as u64) {
          return write_error_response(
            &mut caller,
            &format!(
              "File response is too large for guest memory (source bytes: {}, response capacity: {}); use aeordb_extract_file for bounded ranges",
              record.total_size, response_capacity
            ),
          );
        }

        let data = match dir_ops.read_file_buffered(&path) {
          Ok(data) => data,
          Err(error) => return write_error_response(&mut caller, &format!("Read failed: {error}")),
        };

        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        let size = data.len();

        let response = serde_json::json!({
          "data": encoded,
          "content_type": content_type,
          "size": size,
        });

        write_json_response(&mut caller, &response)
      })
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_write_file(ptr, len) -> i64
    // Writes a file to the database.
    // Args: {"path": "/...", "data": "<base64>", "content_type": "..."}
    // Returns: {"ok": true, "size": N}
    // -----------------------------------------------------------------------
    linker
      .func_wrap("aeordb", "aeordb_write_file", |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
        let args_json = match read_guest_json(&caller, ptr, len) {
          Ok(v) => v,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        let path = match args_json.get("path").and_then(|v| v.as_str()) {
          Some(p) => p.to_string(),
          None => return write_error_response(&mut caller, "Missing 'path' argument"),
        };

        let data_b64 = match args_json.get("data").and_then(|v| v.as_str()) {
          Some(d) => d,
          None => return write_error_response(&mut caller, "Missing 'data' argument"),
        };

        let content_type = args_json.get("content_type").and_then(|v| v.as_str()).map(|s| s.to_string());

        let data = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
          Ok(d) => d,
          Err(e) => return write_error_response(&mut caller, &format!("Base64 decode failed: {}", e)),
        };

        let (engine, ctx) = match get_engine_and_context(&caller) {
          Ok(pair) => pair,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        if let Err(error) = authorize_plugin_path(&caller, &path, CrudlifyOp::Create) {
          return write_error_response(&mut caller, &error.to_string());
        }

        let dir_ops = DirectoryOps::new(&engine);
        let size = data.len();

        match dir_ops.store_file_buffered(&ctx, &path, &data, content_type.as_deref()) {
          Ok(_) => {
            let response = serde_json::json!({
              "ok": true,
              "size": size,
            });
            write_json_response(&mut caller, &response)
          }
          Err(e) => write_error_response(&mut caller, &format!("Write failed: {}", e)),
        }
      })
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_extract_file(ptr, len) -> i64
    // Extracts text ranges without buffering the full file.
    // Args: {"path": "/...", "mode": "lines"|"chars", "start": N, "end": N, "max_bytes": N}
    // Returns: {"text": "...", "content_type": "...", "source_size": N, "truncated": bool}
    // -----------------------------------------------------------------------
    linker
      .func_wrap("aeordb", "aeordb_extract_file", |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
        let args_json = match read_guest_json(&caller, ptr, len) {
          Ok(v) => v,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        let path = match args_json.get("path").and_then(|v| v.as_str()) {
          Some(p) => p.to_string(),
          None => return write_error_response(&mut caller, "Missing 'path' argument"),
        };

        if let Err(error) = authorize_plugin_path(&caller, &path, CrudlifyOp::Read) {
          return write_error_response(&mut caller, &error.to_string());
        }

        let engine = match caller.data().engine.as_ref() {
          Some(e) => Arc::clone(e),
          None => return write_error_response(&mut caller, "Database access not available in this plugin context"),
        };

        let safe_text_bytes = host_response_capacity(&caller).saturating_sub(1024) / 6;
        if safe_text_bytes == 0 {
          return write_error_response(&mut caller, "Guest memory has no capacity for an extract response");
        }

        match extract_file_text(&engine, &path, &args_json, safe_text_bytes) {
          Ok(response) => write_json_response(&mut caller, &response),
          Err(e) => write_error_response(&mut caller, &e),
        }
      })
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_delete_file(ptr, len) -> i64
    // Deletes a file from the database. Args: {"path": "/..."}
    // Returns: {"ok": true}
    // -----------------------------------------------------------------------
    linker
      .func_wrap("aeordb", "aeordb_delete_file", |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
        let args_json = match read_guest_json(&caller, ptr, len) {
          Ok(v) => v,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        let path = match args_json.get("path").and_then(|v| v.as_str()) {
          Some(p) => p.to_string(),
          None => return write_error_response(&mut caller, "Missing 'path' argument"),
        };

        let (engine, ctx) = match get_engine_and_context(&caller) {
          Ok(pair) => pair,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        if let Err(error) = authorize_plugin_path(&caller, &path, CrudlifyOp::Delete) {
          return write_error_response(&mut caller, &error.to_string());
        }

        let dir_ops = DirectoryOps::new(&engine);

        match dir_ops.delete_file(&ctx, &path) {
          Ok(()) => {
            let response = serde_json::json!({"ok": true});
            write_json_response(&mut caller, &response)
          }
          Err(e) => write_error_response(&mut caller, &format!("Delete failed: {}", e)),
        }
      })
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_file_metadata(ptr, len) -> i64
    // Gets file metadata. Args: {"path": "/..."}
    // Returns: {"path": "...", "size": N, "content_type": "...", "created_at": N, "updated_at": N}
    // -----------------------------------------------------------------------
    linker
      .func_wrap("aeordb", "aeordb_file_metadata", |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
        let args_json = match read_guest_json(&caller, ptr, len) {
          Ok(v) => v,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        let path = match args_json.get("path").and_then(|v| v.as_str()) {
          Some(p) => p.to_string(),
          None => return write_error_response(&mut caller, "Missing 'path' argument"),
        };

        let engine = match caller.data().engine.as_ref() {
          Some(e) => Arc::clone(e),
          None => return write_error_response(&mut caller, "Database access not available in this plugin context"),
        };

        if let Err(error) = authorize_plugin_path(&caller, &path, CrudlifyOp::Read) {
          return write_error_response(&mut caller, &error.to_string());
        }

        let dir_ops = DirectoryOps::new(&engine);

        match dir_ops.get_metadata(&path) {
          Ok(Some(record)) => {
            let response = serde_json::json!({
              "path": record.path,
              "size": record.total_size,
              "content_type": record.content_type,
              "created_at": record.created_at,
              "updated_at": record.updated_at,
            });
            write_json_response(&mut caller, &response)
          }
          Ok(None) => write_error_response(&mut caller, &format!("File not found: {}", path)),
          Err(e) => write_error_response(&mut caller, &format!("Metadata failed: {}", e)),
        }
      })
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_list_directory(ptr, len) -> i64
    // Lists directory contents. Args: {"path": "/..."}
    // Returns: {"entries": [{"name": "...", "type": "file"|"directory", "size": N}, ...]}
    // -----------------------------------------------------------------------
    linker
      .func_wrap("aeordb", "aeordb_list_directory", |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
        let args_json = match read_guest_json(&caller, ptr, len) {
          Ok(v) => v,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        let path = match args_json.get("path").and_then(|v| v.as_str()) {
          Some(p) => p.to_string(),
          None => return write_error_response(&mut caller, "Missing 'path' argument"),
        };

        let engine = match caller.data().engine.as_ref() {
          Some(e) => Arc::clone(e),
          None => return write_error_response(&mut caller, "Database access not available in this plugin context"),
        };

        if let Err(error) = authorize_plugin_path(&caller, &path, CrudlifyOp::List) {
          return write_error_response(&mut caller, &error.to_string());
        }

        let dir_ops = DirectoryOps::new(&engine);
        let family_policy = match SystemFamilyPolicyResolver::new(engine.hash_algo()) {
          Ok(policy) => policy,
          Err(error) => return write_error_response(&mut caller, &format!("System family policy failed: {error}")),
        };
        let normalized_directory = crate::engine::path_utils::normalize_path(&path);
        let response_capacity = host_response_capacity(&caller);
        let offset = match optional_json_usize(&args_json, "offset") {
          Ok(value) => value.unwrap_or(0),
          Err(error) => return write_error_response(&mut caller, &error),
        };
        let capacity_limit = response_capacity.saturating_sub(256) / 128;
        let requested_limit = match optional_json_usize(&args_json, "limit") {
          Ok(value) => value,
          Err(error) => return write_error_response(&mut caller, &error),
        };
        let limit = requested_limit.unwrap_or(capacity_limit).min(capacity_limit);

        match dir_ops.list_directory_window_strict(&path, offset, limit) {
          Ok(window) => {
            let mut response_bytes = 64usize;
            let mut entries = Vec::with_capacity(window.entries.len());
            let mut capacity_truncated = false;
            for child in &window.entries {
              let child_path =
                if normalized_directory == "/" { format!("/{}", child.name) } else { format!("{}/{}", normalized_directory, child.name) };
              match family_policy.generic_data_path_is_visible(&child_path) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                  return write_error_response(&mut caller, &format!("System family policy failed for '{}': {}", child_path, error));
                }
              }
              let estimated = child.name.len().saturating_mul(6).saturating_add(96);
              if response_bytes.saturating_add(estimated) > response_capacity {
                capacity_truncated = true;
                break;
              }
              response_bytes = response_bytes.saturating_add(estimated);
              let entry_type = if child.entry_type == EntryType::DirectoryIndex.to_u8() { "directory" } else { "file" };
              entries.push(serde_json::json!({
                "name": child.name,
                "type": entry_type,
                "size": child.total_size,
              }));
            }
            let has_more = window.has_more || capacity_truncated;
            if requested_limit.is_none() && has_more {
              return write_error_response(
                &mut caller,
                "Directory response is too large for guest memory; request a bounded limit and offset",
              );
            }
            let response = serde_json::json!({"entries": entries, "has_more": has_more});
            write_json_response(&mut caller, &response)
          }
          Err(e) => write_error_response(&mut caller, &format!("List failed: {}", e)),
        }
      })
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_query(ptr, len) -> i64
    // Executes a query. Args: same JSON format as POST /query.
    // Returns: {"items": [...], "total": N, "has_more": bool}
    // -----------------------------------------------------------------------
    linker
      .func_wrap("aeordb", "aeordb_query", |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
        let args_json = match read_guest_json(&caller, ptr, len) {
          Ok(v) => v,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        let engine = match caller.data().engine.as_ref() {
          Some(e) => Arc::clone(e),
          None => return write_error_response(&mut caller, "Database access not available in this plugin context"),
        };

        // Parse the query from JSON
        let mut query = match parse_query_from_json(&args_json) {
          Ok(q) => q,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        if let Err(error) = authorize_plugin_path(&caller, &query.path, CrudlifyOp::List) {
          return write_error_response(&mut caller, &error.to_string());
        }

        let response_capacity = host_response_capacity(&caller);
        let response_item_limit = (response_capacity.saturating_sub(512) / 256).max(1);
        query.limit = Some(query.limit.unwrap_or(response_item_limit).min(response_item_limit));

        let family_policy = match SystemFamilyPolicyResolver::new(engine.hash_algo()) {
          Ok(policy) => policy,
          Err(error) => return write_error_response(&mut caller, &format!("System family policy failed: {error}")),
        };
        let query_engine = QueryEngine::new(&engine);
        match query_engine.execute_paginated_filtered(&query, |result| {
          if !family_policy.generic_data_path_is_visible(&result.file_record.path)? {
            return Ok(false);
          }
          match authorize_plugin_path(&caller, &result.file_record.path, CrudlifyOp::Read) {
            Ok(()) => Ok(true),
            Err(PluginPathAuthorizationError::Denied(_)) => Ok(false),
            Err(error) => Err(error.into_engine_error()),
          }
        }) {
          Ok(paginated) => {
            let mut result_items = Vec::new();
            let mut response_bytes = 256usize;
            let mut capacity_truncated = false;
            for result in &paginated.results {
              let matched_bytes = result.matched_by.iter().fold(0usize, |total, value| total.saturating_add(value.len().saturating_mul(6)));
              let estimated = result
                .file_record
                .path
                .len()
                .saturating_mul(6)
                .saturating_add(result.file_record.content_type.as_ref().map_or(0, |value| value.len().saturating_mul(6)))
                .saturating_add(matched_bytes)
                .saturating_add(256);
              if response_bytes.saturating_add(estimated) > response_capacity {
                capacity_truncated = true;
                break;
              }
              response_bytes = response_bytes.saturating_add(estimated);
              result_items.push(serde_json::json!({
                "path": result.file_record.path,
                "score": result.score,
                "total_size": result.file_record.total_size,
                "content_type": result.file_record.content_type,
                "created_at": result.file_record.created_at,
                "updated_at": result.file_record.updated_at,
                "matched_by": result.matched_by,
              }));
            }

            let mut response = serde_json::json!({
              "items": result_items,
              "has_more": paginated.has_more || capacity_truncated,
            });

            if is_unrestricted_plugin_context(&caller) {
              if let Some(total) = paginated.total_count {
                response["total"] = serde_json::json!(total);
              }
            }

            write_json_response(&mut caller, &response)
          }
          Err(e) => write_error_response(&mut caller, &format!("Query failed: {}", e)),
        }
      })
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_aggregate(ptr, len) -> i64
    // Executes an aggregate query. Args: same JSON format as POST /query with aggregate.
    // Returns: the aggregate result as JSON.
    // -----------------------------------------------------------------------
    linker
      .func_wrap("aeordb", "aeordb_aggregate", |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
        let args_json = match read_guest_json(&caller, ptr, len) {
          Ok(v) => v,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        let engine = match caller.data().engine.as_ref() {
          Some(e) => Arc::clone(e),
          None => return write_error_response(&mut caller, "Database access not available in this plugin context"),
        };

        // Parse the query from JSON (must include aggregate section)
        let query = match parse_query_from_json(&args_json) {
          Ok(q) => q,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        if let Err(error) = authorize_plugin_path(&caller, &query.path, CrudlifyOp::List) {
          return write_error_response(&mut caller, &error.to_string());
        }

        if query.aggregate.is_none() {
          return write_error_response(&mut caller, "Missing 'aggregate' section in query");
        }
        if !is_unrestricted_plugin_context(&caller) {
          return write_error_response(&mut caller, "Aggregate host function requires root or system context");
        }

        let family_policy = match SystemFamilyPolicyResolver::new(engine.hash_algo()) {
          Ok(policy) => policy,
          Err(error) => return write_error_response(&mut caller, &format!("System family policy failed: {error}")),
        };
        let query_engine = QueryEngine::new(&engine);
        match query_engine.execute_aggregate_filtered(&query, |result| family_policy.generic_data_path_is_visible(&result.file_record.path))
        {
          Ok(result) => match serde_json::to_value(&result) {
            Ok(v) => write_json_response(&mut caller, &v),
            Err(e) => write_error_response(&mut caller, &format!("Serialization failed: {}", e)),
          },
          Err(e) => write_error_response(&mut caller, &format!("Aggregate failed: {}", e)),
        }
      })
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // log_message(level_ptr, level_len, msg_ptr, msg_len)
    // Reads level and message strings from guest memory and emits a tracing event.
    // -----------------------------------------------------------------------
    linker
      .func_wrap("aeordb", "log_message", |caller: Caller<'_, HostState>, level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32| {
        // M12: Reject negative pointer or length values.
        if level_ptr < 0 || level_len < 0 || msg_ptr < 0 || msg_len < 0 {
          tracing::warn!(
            "log_message: negative ptr/len (level_ptr={}, level_len={}, msg_ptr={}, msg_len={})",
            level_ptr,
            level_len,
            msg_ptr,
            msg_len
          );
          return;
        }

        // M13: Clamp lengths to prevent unbounded allocations from a buggy guest.
        let level_len_clamped = (level_len as usize).min(MAX_GUEST_MESSAGE_SIZE);
        let msg_len_clamped = (msg_len as usize).min(MAX_GUEST_MESSAGE_SIZE);

        let memory = match caller.data().memory {
          Some(mem) => mem,
          None => {
            tracing::warn!("log_message called before memory was set");
            return;
          }
        };

        let level_str = {
          let mut buf = vec![0u8; level_len_clamped];
          if memory.read(&caller, level_ptr as usize, &mut buf).is_ok() {
            String::from_utf8_lossy(&buf).to_string()
          } else {
            "unknown".to_string()
          }
        };

        let msg_str = {
          let mut buf = vec![0u8; msg_len_clamped];
          if memory.read(&caller, msg_ptr as usize, &mut buf).is_ok() {
            String::from_utf8_lossy(&buf).to_string()
          } else {
            "<unreadable>".to_string()
          }
        };

        match level_str.to_lowercase().as_str() {
          "error" => tracing::error!(target: "wasm_plugin", "{}", msg_str),
          "warn" | "warning" => tracing::warn!(target: "wasm_plugin", "{}", msg_str),
          "debug" => tracing::debug!(target: "wasm_plugin", "{}", msg_str),
          "trace" => tracing::trace!(target: "wasm_plugin", "{}", msg_str),
          _ => tracing::info!(target: "wasm_plugin", "{}", msg_str),
        }
      })
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    Ok(())
  }
}

// ---------------------------------------------------------------------------
// Helper functions for host function implementations
// ---------------------------------------------------------------------------

/// Get the engine and a RequestContext from the host state.
/// Uses the stored request_context's user_id and event_bus to build a proper
/// context that preserves the caller's identity for auditing and permissions.
fn get_engine_and_context(caller: &Caller<'_, HostState>) -> Result<(Arc<StorageEngine>, RequestContext), String> {
  let engine = caller.data().engine.as_ref().ok_or_else(|| "Database access not available in this plugin context".to_string())?;

  // Build a RequestContext preserving the caller's user_id and event_bus.
  // Falls back to a system context only when no request context is stored.
  let ctx = match caller.data().request_context.as_ref() {
    Some(stored_ctx) => {
      match stored_ctx.event_bus() {
        Some(bus) => RequestContext::from_claims(&stored_ctx.user_id, Arc::clone(bus)),
        None => {
          // Has user_id but no event bus — construct without bus.
          // This preserves the user identity for auditing even without events.
          RequestContext::from_claims(&stored_ctx.user_id, std::sync::Arc::new(crate::engine::EventBus::new()))
        }
      }
    }
    None => RequestContext::system(),
  };
  Ok((Arc::clone(engine), ctx))
}

#[derive(Debug, thiserror::Error)]
enum PluginPathAuthorizationError {
  #[error("{0}")]
  Denied(String),
  #[error("{0}")]
  Operational(#[source] EngineError),
  #[error("{0}")]
  Context(String),
}

impl PluginPathAuthorizationError {
  fn into_engine_error(self) -> EngineError {
    match self {
      Self::Operational(error) => error,
      Self::Denied(message) | Self::Context(message) => EngineError::InvalidInput(format!("plugin authorization failed: {message}")),
    }
  }
}

fn authorize_plugin_path(caller: &Caller<'_, HostState>, path: &str, operation: CrudlifyOp) -> Result<(), PluginPathAuthorizationError> {
  let engine = caller
    .data()
    .engine
    .as_ref()
    .ok_or_else(|| PluginPathAuthorizationError::Context("Database access not available in this plugin context".to_string()))?;
  let ctx = caller
    .data()
    .request_context
    .as_ref()
    .ok_or_else(|| PluginPathAuthorizationError::Context("Request context not available in this plugin context".to_string()))?;

  let normalized = if path.starts_with('/') { path.to_string() } else { format!("/{}", path) };
  let visible = SystemFamilyPolicyResolver::new(engine.hash_algo())
    .and_then(|resolver| resolver.generic_data_path_is_visible(&normalized))
    .map_err(PluginPathAuthorizationError::Operational)?;
  if !visible {
    return Err(PluginPathAuthorizationError::Denied(format!("Permission denied: {}", normalized)));
  }

  if ctx.user_id == "system" {
    return Ok(());
  }

  if let Some(key_id) = ctx.key_id.as_ref() {
    let api_key_cache = caller
      .data()
      .api_key_cache
      .as_ref()
      .ok_or_else(|| PluginPathAuthorizationError::Context("API key cache not available in this plugin context".to_string()))?;
    let api_key_engine = caller
      .data()
      .api_key_engine
      .as_ref()
      .ok_or_else(|| PluginPathAuthorizationError::Context("API key authority not available in this plugin context".to_string()))?;
    let key_record = api_key_cache
      .get(key_id, api_key_engine)
      .map_err(PluginPathAuthorizationError::Operational)?
      .ok_or_else(|| PluginPathAuthorizationError::Denied("API key not found".to_string()))?;

    if key_record.is_revoked {
      return Err(PluginPathAuthorizationError::Denied("API key has been revoked".to_string()));
    }
    if key_record.expires_at <= chrono::Utc::now().timestamp_millis() {
      return Err(PluginPathAuthorizationError::Denied("API key expired".to_string()));
    }

    if !key_record.rules.is_empty() {
      let flag_char = operation_to_flag_char(&operation);
      let is_ancestor = is_ancestor_of_any_rule(&key_record.rules, &normalized);
      let ancestor_allowed = is_ancestor && matches!(operation, CrudlifyOp::Read | CrudlifyOp::List);

      if !ancestor_allowed {
        match match_rules(&key_record.rules, &normalized) {
          Some(rule) if check_operation_permitted(&rule.permitted, flag_char) => {}
          _ => return Err(PluginPathAuthorizationError::Denied(format!("Permission denied: {}", normalized))),
        }
      }
    }

    if ctx.user_id.starts_with("share:") {
      return Ok(());
    }
  }

  let user_id =
    uuid::Uuid::parse_str(&ctx.user_id).map_err(|_| PluginPathAuthorizationError::Denied("Invalid user identity".to_string()))?;
  let group_cache = caller
    .data()
    .group_cache
    .as_ref()
    .ok_or_else(|| PluginPathAuthorizationError::Context("Group cache not available in this plugin context".to_string()))?;
  let resolver = PermissionResolver::new(engine, group_cache);
  let allowed = resolver.check_path_permission(&user_id, &normalized, operation).map_err(PluginPathAuthorizationError::Operational)?;

  if allowed {
    Ok(())
  } else {
    Err(PluginPathAuthorizationError::Denied(format!("Permission denied: {}", normalized)))
  }
}

fn is_unrestricted_plugin_context(caller: &Caller<'_, HostState>) -> bool {
  let Some(ctx) = caller.data().request_context.as_ref() else {
    return false;
  };
  if ctx.user_id == "system" {
    return true;
  }
  if ctx.key_id.is_some() {
    return false;
  }
  uuid::Uuid::parse_str(&ctx.user_id).map(|user_id| user_id.is_nil()).unwrap_or(false)
}

fn extract_file_text(
  engine: &StorageEngine,
  path: &str,
  args_json: &serde_json::Value,
  safe_text_bytes: usize,
) -> Result<serde_json::Value, String> {
  let mode = match args_json.get("mode").and_then(|v| v.as_str()) {
    Some("lines") => RangeMode::Lines,
    Some("chars") => RangeMode::Chars,
    Some("bytes") => RangeMode::Bytes,
    Some("json_pointer") => RangeMode::JsonPointer,
    Some(_) => return Err("Unsupported extract mode; expected 'lines', 'chars', 'bytes', or 'json_pointer'".to_string()),
    None => return Err("Missing 'mode' argument".to_string()),
  };

  let request = RangeExtractionRequest {
    mode,
    start: args_json.get("start").and_then(|v| v.as_u64()),
    end: args_json.get("end").and_then(|v| v.as_u64()),
    pointer: args_json.get("pointer").and_then(|v| v.as_str()).map(str::to_string),
    max_bytes: Some(
      args_json
        .get("max_bytes")
        .and_then(|v| v.as_u64())
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(safe_text_bytes)
        .min(safe_text_bytes),
    ),
  };

  let extracted = extract_range_by_path(engine, path, &request).map_err(|error| error.to_string())?;

  Ok(serde_json::json!({
    "text": extracted.content,
    "content_type": extracted.content_type,
    "source_size": extracted.source_size,
    "mode": extracted.mode.as_str(),
    "start": extracted.start,
    "end": extracted.end,
    "pointer": extracted.pointer,
    "truncated": extracted.truncated,
  }))
}

/// Maximum size for a single guest message read (16 MB).
/// Prevents a malicious or buggy guest from causing a huge allocation.
const MAX_GUEST_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// Read JSON arguments from guest memory at the given (ptr, len).
///
/// Validates that ptr and len are non-negative and that len does not exceed
/// `MAX_GUEST_MESSAGE_SIZE` to prevent unbounded allocations from a buggy or
/// malicious guest module.
fn read_guest_json(caller: &Caller<'_, HostState>, ptr: i32, len: i32) -> Result<serde_json::Value, String> {
  // M12: Reject negative pointer or length (i32 -> usize cast would wrap).
  if ptr < 0 || len < 0 {
    return Err(format!("Invalid guest memory access: ptr={}, len={} (negative values not allowed)", ptr, len));
  }

  let len_usize = len as usize;

  // M13: Reject unreasonably large allocations.
  if len_usize > MAX_GUEST_MESSAGE_SIZE {
    return Err(format!("Guest message too large: {} bytes (max {} bytes)", len_usize, MAX_GUEST_MESSAGE_SIZE));
  }

  let memory = caller.data().memory.ok_or_else(|| "Memory not available".to_string())?;

  let mut buf = vec![0u8; len_usize];
  memory.read(caller, ptr as usize, &mut buf).map_err(|_| "Failed to read from guest memory".to_string())?;

  serde_json::from_slice(&buf).map_err(|e| format!("Failed to parse JSON arguments: {}", e))
}

fn host_response_capacity(caller: &Caller<'_, HostState>) -> usize {
  caller.data().memory.map_or(0, |memory| memory.data_size(caller).saturating_sub(HOST_RESPONSE_OFFSET))
}

/// Write a JSON response into guest memory at HOST_RESPONSE_OFFSET.
/// Returns packed i64: (ptr << 32) | len.
fn write_json_response(caller: &mut Caller<'_, HostState>, value: &serde_json::Value) -> i64 {
  let bytes = match serde_json::to_vec(value) {
    Ok(b) => b,
    Err(error) => return write_error_response(caller, &format!("Host response serialization failed: {error}")),
  };

  if bytes.len() > host_response_capacity(caller) || bytes.len() > u32::MAX as usize {
    return write_error_response(caller, "Host response exceeds guest memory capacity; request a smaller bounded result");
  }
  write_response_bytes(caller, &bytes)
}

fn write_response_bytes(caller: &mut Caller<'_, HostState>, bytes: &[u8]) -> i64 {
  if bytes.len() > host_response_capacity(caller) || bytes.len() > u32::MAX as usize {
    return 0;
  }
  let memory = match caller.data().memory {
    Some(mem) => mem,
    None => return 0,
  };

  if memory.write(caller, HOST_RESPONSE_OFFSET, &bytes).is_err() {
    return 0;
  }

  ((HOST_RESPONSE_OFFSET as i64) << 32) | (bytes.len() as i64)
}

/// Write an error response as {"error": "message"} into guest memory.
fn write_error_response(caller: &mut Caller<'_, HostState>, message: &str) -> i64 {
  if let Ok(bytes) = serde_json::to_vec(&serde_json::json!({"error": message})) {
    if bytes.len() <= host_response_capacity(caller) {
      return write_response_bytes(caller, &bytes);
    }
  }
  write_response_bytes(caller, br#"{"error":"Host response exceeds guest memory capacity"}"#)
}

// ---------------------------------------------------------------------------
// Query JSON parsing — mirrors the logic from engine_routes.rs
// ---------------------------------------------------------------------------

/// Parse a Query struct from JSON in the same format as POST /query.
fn parse_query_from_json(json: &serde_json::Value) -> Result<Query, String> {
  let path = json.get("path").and_then(|v| v.as_str()).ok_or_else(|| "Missing 'path' in query".to_string())?.to_string();

  let where_clause = json.get("where").cloned().unwrap_or(serde_json::json!([]));
  let query_node = parse_where_clause(&where_clause)?;
  let is_empty = matches!(&query_node, crate::engine::query_engine::QueryNode::And(children) if children.is_empty());

  let limit = optional_json_usize(json, "limit")?;
  let offset = optional_json_usize(json, "offset")?;
  let after = optional_query_string(json, "after")?;
  let before = optional_query_string(json, "before")?;
  let include_total = optional_query_bool(json, "include_total")?.unwrap_or(false);
  let order_by = parse_query_order_by(json.get("order_by"))?;

  let aggregate = match json.get("aggregate") {
    None => None,
    Some(value) => {
      let aggregate = value.as_object().ok_or_else(|| "'aggregate' must be an object".to_string())?;
      Some(AggregateQuery {
        count: optional_query_bool(value, "count")?.unwrap_or(false),
        sum: parse_string_array(aggregate.get("sum"), "aggregate.sum")?,
        avg: parse_string_array(aggregate.get("avg"), "aggregate.avg")?,
        min: parse_string_array(aggregate.get("min"), "aggregate.min")?,
        max: parse_string_array(aggregate.get("max"), "aggregate.max")?,
        group_by: parse_string_array(aggregate.get("group_by"), "aggregate.group_by")?,
      })
    }
  };

  Ok(Query {
    path,
    field_queries: Vec::new(),
    node: if is_empty { None } else { Some(query_node) },
    limit,
    offset,
    order_by,
    after,
    before,
    include_total,
    strategy: QueryStrategy::Full,
    aggregate,
    explain: ExplainMode::Off,
  })
}

fn optional_json_usize(json: &serde_json::Value, field: &str) -> Result<Option<usize>, String> {
  let Some(value) = json.get(field) else {
    return Ok(None);
  };
  let value = value.as_u64().ok_or_else(|| format!("'{field}' must be an unsigned integer"))?;
  usize::try_from(value).map(Some).map_err(|_| format!("'{field}' exceeds this platform's address space"))
}

fn optional_query_string(json: &serde_json::Value, field: &str) -> Result<Option<String>, String> {
  let Some(value) = json.get(field) else {
    return Ok(None);
  };
  value.as_str().map(|value| Some(value.to_string())).ok_or_else(|| format!("'{field}' must be a string"))
}

fn optional_query_bool(json: &serde_json::Value, field: &str) -> Result<Option<bool>, String> {
  let Some(value) = json.get(field) else {
    return Ok(None);
  };
  value.as_bool().map(Some).ok_or_else(|| format!("'{field}' must be a boolean"))
}

fn parse_query_order_by(value: Option<&serde_json::Value>) -> Result<Vec<SortField>, String> {
  let Some(value) = value else {
    return Ok(Vec::new());
  };
  let fields = value.as_array().ok_or_else(|| "'order_by' must be an array".to_string())?;
  let mut order_by = Vec::with_capacity(fields.len());
  for (index, value) in fields.iter().enumerate() {
    let field = value
      .get("field")
      .and_then(serde_json::Value::as_str)
      .ok_or_else(|| format!("'order_by[{index}].field' must be a string"))?
      .to_string();
    let direction = match value.get("direction") {
      None => SortDirection::Asc,
      Some(value) if value.as_str() == Some("asc") => SortDirection::Asc,
      Some(value) if value.as_str() == Some("desc") => SortDirection::Desc,
      Some(_) => return Err(format!("'order_by[{index}].direction' must be 'asc' or 'desc'")),
    };
    order_by.push(SortField { field, direction });
  }
  Ok(order_by)
}

/// Parse an absent value as an empty list; a present value must be an array of strings.
fn parse_string_array(value: Option<&serde_json::Value>, field: &str) -> Result<Vec<String>, String> {
  let Some(value) = value else {
    return Ok(Vec::new());
  };
  let values = value.as_array().ok_or_else(|| format!("'{field}' must be an array"))?;
  values
    .iter()
    .enumerate()
    .map(|(index, value)| value.as_str().map(str::to_string).ok_or_else(|| format!("'{field}[{index}]' must be a string")))
    .collect()
}

#[cfg(test)]
#[path = "../../spec/plugins/wasm_runtime_query_internal_spec.rs"]
mod wasm_runtime_query_internal_spec;
