use std::sync::Arc;

use base64::Engine as _;
use wasmi::{Caller, Config, Engine, Extern, Linker, Memory, MemoryType, Module, Store};

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::entry_type::EntryType;
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

/// Default maximum memory in bytes (16 MB).
const DEFAULT_MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

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

/// Host state passed into the WASM Store.
struct HostState {
  /// Reference to the guest's linear memory (set after instantiation).
  memory: Option<Memory>,
  /// Storage engine for database operations (set for query plugins, None for parsers).
  engine: Option<Arc<StorageEngine>>,
  /// Request context for permission-checked operations.
  request_context: Option<RequestContext>,
  /// Group cache for request-scoped permission checks.
  group_cache: Option<Arc<Cache<GroupLoader>>>,
  /// API key cache for scoped-key path checks.
  api_key_cache: Option<Arc<Cache<ApiKeyLoader>>>,
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
    let mut store =
      Store::new(&self.engine, HostState { memory: None, engine: None, request_context: None, group_cache: None, api_key_cache: None });
    store.set_fuel(self.fuel_limit).map_err(|error| WasmRuntimeError::Trap(error.to_string()))?;

    let mut linker = <Linker<HostState>>::new(&self.engine);
    self.register_host_functions(&mut linker)?;

    // Provide a default "env" memory if the module imports one.
    let memory_pages = (self.memory_limit_bytes / (64 * 1024)).max(1) as u32;
    let memory_type = MemoryType::new(1, Some(memory_pages)).map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;
    let memory = Memory::new(&mut store, memory_type).map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;
    linker.define("env", "memory", Extern::Memory(memory)).map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    let instance = linker
      .instantiate(&mut store, &self.module)
      .and_then(|pre_instance| pre_instance.start(&mut store))
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // Resolve guest memory — prefer the instance's own export, fall back to the one we created.
    let guest_memory = instance.get_memory(&store, "memory").unwrap_or(memory);

    store.data_mut().memory = Some(guest_memory);

    // Write request bytes into guest memory starting at offset 0.
    let request_length = request_bytes.len();
    let memory_size = guest_memory.data_size(&store);
    if request_length > memory_size {
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
    if response_pointer + response_length > current_memory_size {
      return Err(WasmRuntimeError::MemoryOutOfBounds);
    }

    let mut response_buffer = vec![0u8; response_length];
    guest_memory.read(&store, response_pointer, &mut response_buffer).map_err(|_| WasmRuntimeError::MemoryOutOfBounds)?;

    Ok(response_buffer)
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
    let mut store = Store::new(
      &self.engine,
      HostState {
        memory: None,
        engine: Some(engine),
        request_context: Some(ctx),
        group_cache: Some(group_cache),
        api_key_cache: Some(api_key_cache),
      },
    );
    store.set_fuel(self.fuel_limit).map_err(|error| WasmRuntimeError::Trap(error.to_string()))?;

    let mut linker = <Linker<HostState>>::new(&self.engine);
    self.register_host_functions(&mut linker)?;

    // Provide a default "env" memory if the module imports one.
    let memory_pages = (self.memory_limit_bytes / (64 * 1024)).max(1) as u32;
    let memory_type = MemoryType::new(1, Some(memory_pages)).map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;
    let memory = Memory::new(&mut store, memory_type).map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;
    linker.define("env", "memory", Extern::Memory(memory)).map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    let instance = linker
      .instantiate(&mut store, &self.module)
      .and_then(|pre_instance| pre_instance.start(&mut store))
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // Resolve guest memory — prefer the instance's own export, fall back to the one we created.
    let guest_memory = instance.get_memory(&store, "memory").unwrap_or(memory);

    store.data_mut().memory = Some(guest_memory);

    // Write request bytes into guest memory starting at offset 0.
    let request_length = request_bytes.len();
    let memory_size = guest_memory.data_size(&store);
    if request_length > memory_size {
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
    if response_pointer + response_length > current_memory_size {
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
  /// **H4 — Permission gap**: These host functions currently do NOT enforce
  /// per-operation permission checks beyond what `DirectoryOps` and the
  /// `RequestContext` provide. Full permission enforcement requires threading
  /// `PermissionResolver` (which depends on `GroupCache` + `PermissionsCache`)
  /// into `HostState`. Until that refactor is done, WASM plugins operate
  /// with the permissions of the request that invoked them, validated only
  /// at the HTTP middleware level. See the TODO in `get_engine_and_context`.
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

        if let Err(e) = authorize_plugin_path(&caller, &path, CrudlifyOp::Read) {
          return write_error_response(&mut caller, &e);
        }

        let dir_ops = DirectoryOps::new(&engine);

        // Read file content
        let data = match dir_ops.read_file_buffered(&path) {
          Ok(d) => d,
          Err(e) => return write_error_response(&mut caller, &format!("Read failed: {}", e)),
        };

        // Get metadata for content_type
        let content_type = match dir_ops.get_metadata(&path) {
          Ok(Some(record)) => record.content_type.unwrap_or_default(),
          _ => String::new(),
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
          Some(d) => d.to_string(),
          None => return write_error_response(&mut caller, "Missing 'data' argument"),
        };

        let content_type = args_json.get("content_type").and_then(|v| v.as_str()).map(|s| s.to_string());

        let data = match base64::engine::general_purpose::STANDARD.decode(&data_b64) {
          Ok(d) => d,
          Err(e) => return write_error_response(&mut caller, &format!("Base64 decode failed: {}", e)),
        };

        let (engine, ctx) = match get_engine_and_context(&caller) {
          Ok(pair) => pair,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        if let Err(e) = authorize_plugin_path(&caller, &path, CrudlifyOp::Create) {
          return write_error_response(&mut caller, &e);
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

        if let Err(e) = authorize_plugin_path(&caller, &path, CrudlifyOp::Read) {
          return write_error_response(&mut caller, &e);
        }

        let engine = match caller.data().engine.as_ref() {
          Some(e) => Arc::clone(e),
          None => return write_error_response(&mut caller, "Database access not available in this plugin context"),
        };

        match extract_file_text(&engine, &path, &args_json) {
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

        if let Err(e) = authorize_plugin_path(&caller, &path, CrudlifyOp::Delete) {
          return write_error_response(&mut caller, &e);
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

        if let Err(e) = authorize_plugin_path(&caller, &path, CrudlifyOp::Read) {
          return write_error_response(&mut caller, &e);
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

        if let Err(e) = authorize_plugin_path(&caller, &path, CrudlifyOp::List) {
          return write_error_response(&mut caller, &e);
        }

        let dir_ops = DirectoryOps::new(&engine);

        match dir_ops.list_directory(&path) {
          Ok(children) => {
            let entries: Vec<serde_json::Value> = children
              .iter()
              .map(|child| {
                let entry_type = if child.entry_type == EntryType::DirectoryIndex.to_u8() { "directory" } else { "file" };
                serde_json::json!({
                  "name": child.name,
                  "type": entry_type,
                  "size": child.total_size,
                })
              })
              .collect();

            let response = serde_json::json!({"entries": entries});
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
        let query = match parse_query_from_json(&args_json) {
          Ok(q) => q,
          Err(e) => return write_error_response(&mut caller, &e),
        };

        if let Err(e) = authorize_plugin_path(&caller, &query.path, CrudlifyOp::List) {
          return write_error_response(&mut caller, &e);
        }

        let query_engine = QueryEngine::new(&engine);
        match query_engine.execute_paginated(&query) {
          Ok(paginated) => {
            let result_items: Vec<serde_json::Value> = paginated
              .results
              .iter()
              .filter(|r| authorize_plugin_path(&caller, &r.file_record.path, CrudlifyOp::Read).is_ok())
              .map(|r| {
                serde_json::json!({
                  "path": r.file_record.path,
                  "score": r.score,
                  "total_size": r.file_record.total_size,
                  "content_type": r.file_record.content_type,
                  "created_at": r.file_record.created_at,
                  "updated_at": r.file_record.updated_at,
                  "matched_by": r.matched_by,
                })
              })
              .collect();
            let visible_count = result_items.len();

            let mut response = serde_json::json!({
              "items": result_items,
              "has_more": paginated.has_more,
            });

            if let Some(total) = paginated.total_count {
              response["total"] = serde_json::json!(std::cmp::min(total, visible_count as u64));
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

        if let Err(e) = authorize_plugin_path(&caller, &query.path, CrudlifyOp::List) {
          return write_error_response(&mut caller, &e);
        }

        if query.aggregate.is_none() {
          return write_error_response(&mut caller, "Missing 'aggregate' section in query");
        }
        if !is_unrestricted_plugin_context(&caller) {
          return write_error_response(&mut caller, "Aggregate host function requires root or system context");
        }

        let query_engine = QueryEngine::new(&engine);
        match query_engine.execute_aggregate(&query) {
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

fn authorize_plugin_path(caller: &Caller<'_, HostState>, path: &str, operation: CrudlifyOp) -> Result<(), String> {
  let engine = caller.data().engine.as_ref().ok_or_else(|| "Database access not available in this plugin context".to_string())?;
  let ctx = caller.data().request_context.as_ref().ok_or_else(|| "Request context not available in this plugin context".to_string())?;

  if ctx.user_id == "system" {
    return Ok(());
  }

  let normalized = if path.starts_with('/') { path.to_string() } else { format!("/{}", path) };

  if crate::engine::directory_ops::is_system_path(&normalized) {
    return Err(format!("Permission denied: {}", normalized));
  }

  if let Some(key_id) = ctx.key_id.as_ref() {
    let api_key_cache =
      caller.data().api_key_cache.as_ref().ok_or_else(|| "API key cache not available in this plugin context".to_string())?;
    let key_record = api_key_cache
      .get(key_id, engine)
      .map_err(|error| format!("Failed to verify API key: {}", error))?
      .ok_or_else(|| "API key not found".to_string())?;

    if key_record.is_revoked {
      return Err("API key has been revoked".to_string());
    }
    if key_record.expires_at <= chrono::Utc::now().timestamp_millis() {
      return Err("API key expired".to_string());
    }

    if !key_record.rules.is_empty() {
      let flag_char = operation_to_flag_char(&operation);
      let is_ancestor = is_ancestor_of_any_rule(&key_record.rules, &normalized);
      let ancestor_allowed = is_ancestor && matches!(operation, CrudlifyOp::Read | CrudlifyOp::List);

      if !ancestor_allowed {
        match match_rules(&key_record.rules, &normalized) {
          Some(rule) if check_operation_permitted(&rule.permitted, flag_char) => {}
          _ => return Err(format!("Permission denied: {}", normalized)),
        }
      }
    }

    if ctx.user_id.starts_with("share:") {
      return Ok(());
    }
  }

  let user_id = uuid::Uuid::parse_str(&ctx.user_id).map_err(|_| "Invalid user identity".to_string())?;
  let group_cache = caller.data().group_cache.as_ref().ok_or_else(|| "Group cache not available in this plugin context".to_string())?;
  let resolver = PermissionResolver::new(engine, group_cache);
  let allowed =
    resolver.check_path_permission(&user_id, &normalized, operation).map_err(|error| format!("Permission check failed: {}", error))?;

  if allowed {
    Ok(())
  } else {
    Err(format!("Permission denied: {}", normalized))
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

fn extract_file_text(engine: &StorageEngine, path: &str, args_json: &serde_json::Value) -> Result<serde_json::Value, String> {
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
    max_bytes: args_json.get("max_bytes").and_then(|v| v.as_u64()).map(|v| v as usize),
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

/// Write a JSON response into guest memory at HOST_RESPONSE_OFFSET.
/// Returns packed i64: (ptr << 32) | len.
fn write_json_response(caller: &mut Caller<'_, HostState>, value: &serde_json::Value) -> i64 {
  let bytes = match serde_json::to_vec(value) {
    Ok(b) => b,
    Err(_) => return 0i64,
  };

  let memory = match caller.data().memory {
    Some(mem) => mem,
    None => return 0i64,
  };

  let response_len = bytes.len();
  if memory.write(caller, HOST_RESPONSE_OFFSET, &bytes).is_err() {
    return 0i64;
  }

  ((HOST_RESPONSE_OFFSET as i64) << 32) | (response_len as i64)
}

/// Write an error response as {"error": "message"} into guest memory.
fn write_error_response(caller: &mut Caller<'_, HostState>, message: &str) -> i64 {
  let response = serde_json::json!({"error": message});
  write_json_response(caller, &response)
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

  let limit = json.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
  let offset = json.get("offset").and_then(|v| v.as_u64()).map(|v| v as usize);
  let after = json.get("after").and_then(|v| v.as_str()).map(|s| s.to_string());
  let before = json.get("before").and_then(|v| v.as_str()).map(|s| s.to_string());
  let include_total = json.get("include_total").and_then(|v| v.as_bool()).unwrap_or(false);

  // Parse order_by
  let order_by: Vec<SortField> = json
    .get("order_by")
    .and_then(|v| v.as_array())
    .map(|fields| {
      fields
        .iter()
        .filter_map(|f| {
          let field = f.get("field")?.as_str()?.to_string();
          let direction = match f.get("direction").and_then(|d| d.as_str()) {
            Some("desc") => SortDirection::Desc,
            _ => SortDirection::Asc,
          };
          Some(SortField { field, direction })
        })
        .collect()
    })
    .unwrap_or_default();

  // Parse aggregate section
  let aggregate = json.get("aggregate").map(|agg| AggregateQuery {
    count: agg.get("count").and_then(|v| v.as_bool()).unwrap_or(false),
    sum: parse_string_array(agg.get("sum")),
    avg: parse_string_array(agg.get("avg")),
    min: parse_string_array(agg.get("min")),
    max: parse_string_array(agg.get("max")),
    group_by: parse_string_array(agg.get("group_by")),
  });

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

/// Parse an optional JSON value as an array of strings.
fn parse_string_array(value: Option<&serde_json::Value>) -> Vec<String> {
  value
    .and_then(|v| v.as_array())
    .map(|arr| arr.iter().filter_map(|item| item.as_str().map(|s| s.to_string())).collect())
    .unwrap_or_default()
}
