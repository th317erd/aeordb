use std::sync::Arc;

use base64::Engine as _;
use wasmi::{
  Caller, Config, Engine, Extern, Linker, Memory, MemoryType, Module, Store,
};

use crate::engine::directory_ops::DirectoryOps;
use crate::engine::entry_type::EntryType;
use crate::engine::query_engine::{
  AggregateQuery, ExplainMode, Query, QueryEngine, QueryStrategy, SortDirection, SortField,
};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;

/// Default maximum memory in bytes (16 MB).
const DEFAULT_MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Default fuel budget for execution metering.
const DEFAULT_FUEL_LIMIT: u64 = 1_000_000;

/// Fixed offset in guest memory where host function responses are written.
/// The guest SDK reads response data from this offset.
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
  pub fn with_limits(
    wasm_bytes: &[u8],
    memory_limit_bytes: usize,
    fuel_limit: u64,
  ) -> Result<Self, WasmRuntimeError> {
    let mut config = Config::default();
    config.consume_fuel(true);

    let engine = Engine::new(&config);
    let module = Module::new(&engine, wasm_bytes)
      .map_err(|error| WasmRuntimeError::CompilationFailed(error.to_string()))?;

    Ok(Self {
      engine,
      module,
      memory_limit_bytes,
      fuel_limit,
    })
  }

  /// Invoke the plugin's exported `handle` function.
  ///
  /// The convention is:
  ///   - The host writes the request bytes into the guest's memory.
  ///   - The host calls `handle(request_ptr, request_len)` which returns a
  ///     packed i64: high 32 bits = response pointer, low 32 bits = response length.
  ///   - The host reads the response bytes from the guest's memory.
  pub fn call_handle(&self, request_bytes: &[u8]) -> Result<Vec<u8>, WasmRuntimeError> {
    let mut store = Store::new(&self.engine, HostState {
      memory: None,
      engine: None,
      request_context: None,
    });
    store
      .set_fuel(self.fuel_limit)
      .map_err(|error| WasmRuntimeError::Trap(error.to_string()))?;

    let mut linker = <Linker<HostState>>::new(&self.engine);
    self.register_host_functions(&mut linker)?;

    // Provide a default "env" memory if the module imports one.
    let memory_pages = (self.memory_limit_bytes / (64 * 1024)).max(1) as u32;
    let memory_type = MemoryType::new(1, Some(memory_pages))
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;
    let memory = Memory::new(&mut store, memory_type)
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;
    linker
      .define("env", "memory", Extern::Memory(memory))
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    let instance = linker
      .instantiate(&mut store, &self.module)
      .and_then(|pre_instance| pre_instance.start(&mut store))
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // Resolve guest memory — prefer the instance's own export, fall back to the one we created.
    let guest_memory = instance
      .get_memory(&store, "memory")
      .unwrap_or(memory);

    store.data_mut().memory = Some(guest_memory);

    // Write request bytes into guest memory starting at offset 0.
    let request_length = request_bytes.len();
    let memory_size = guest_memory.data_size(&store);
    if request_length > memory_size {
      return Err(WasmRuntimeError::MemoryLimitExceeded);
    }
    guest_memory
      .write(&mut store, 0, request_bytes)
      .map_err(|_| WasmRuntimeError::MemoryOutOfBounds)?;

    // Call the exported `handle` function.
    let handle_function = instance
      .get_func(&store, "handle")
      .ok_or_else(|| WasmRuntimeError::ExportNotFound("handle".to_string()))?;

    let handle_typed = handle_function
      .typed::<(i32, i32), i64>(&store)
      .map_err(|error| WasmRuntimeError::ExportNotFound(format!("handle type mismatch: {}", error)))?;

    let result = handle_typed
      .call(&mut store, (0i32, request_length as i32))
      .map_err(|error| {
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
    guest_memory
      .read(&store, response_pointer, &mut response_buffer)
      .map_err(|_| WasmRuntimeError::MemoryOutOfBounds)?;

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
  ) -> Result<Vec<u8>, WasmRuntimeError> {
    let mut store = Store::new(&self.engine, HostState {
      memory: None,
      engine: Some(engine),
      request_context: Some(ctx),
    });
    store
      .set_fuel(self.fuel_limit)
      .map_err(|error| WasmRuntimeError::Trap(error.to_string()))?;

    let mut linker = <Linker<HostState>>::new(&self.engine);
    self.register_host_functions(&mut linker)?;

    // Provide a default "env" memory if the module imports one.
    let memory_pages = (self.memory_limit_bytes / (64 * 1024)).max(1) as u32;
    let memory_type = MemoryType::new(1, Some(memory_pages))
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;
    let memory = Memory::new(&mut store, memory_type)
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;
    linker
      .define("env", "memory", Extern::Memory(memory))
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    let instance = linker
      .instantiate(&mut store, &self.module)
      .and_then(|pre_instance| pre_instance.start(&mut store))
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // Resolve guest memory — prefer the instance's own export, fall back to the one we created.
    let guest_memory = instance
      .get_memory(&store, "memory")
      .unwrap_or(memory);

    store.data_mut().memory = Some(guest_memory);

    // Write request bytes into guest memory starting at offset 0.
    let request_length = request_bytes.len();
    let memory_size = guest_memory.data_size(&store);
    if request_length > memory_size {
      return Err(WasmRuntimeError::MemoryLimitExceeded);
    }
    guest_memory
      .write(&mut store, 0, request_bytes)
      .map_err(|_| WasmRuntimeError::MemoryOutOfBounds)?;

    // Call the exported `handle` function.
    let handle_function = instance
      .get_func(&store, "handle")
      .ok_or_else(|| WasmRuntimeError::ExportNotFound("handle".to_string()))?;

    let handle_typed = handle_function
      .typed::<(i32, i32), i64>(&store)
      .map_err(|error| WasmRuntimeError::ExportNotFound(format!("handle type mismatch: {}", error)))?;

    let result = handle_typed
      .call(&mut store, (0i32, request_length as i32))
      .map_err(|error| {
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
    guest_memory
      .read(&store, response_pointer, &mut response_buffer)
      .map_err(|_| WasmRuntimeError::MemoryOutOfBounds)?;

    Ok(response_buffer)
  }

  /// Register host functions that the WASM module can import.
  ///
  /// Includes the 7 database host functions and the log_message function.
  fn register_host_functions(
    &self,
    linker: &mut Linker<HostState>,
  ) -> Result<(), WasmRuntimeError> {
    // -----------------------------------------------------------------------
    // aeordb_read_file(ptr, len) -> i64
    // Reads a file from the database. Args: {"path": "/..."}
    // Returns: {"data": "<base64>", "content_type": "...", "size": N}
    // -----------------------------------------------------------------------
    linker
      .func_wrap(
        "aeordb",
        "aeordb_read_file",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
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

          let dir_ops = DirectoryOps::new(&engine);

          // Read file content
          let data = match dir_ops.read_file(&path) {
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
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_write_file(ptr, len) -> i64
    // Writes a file to the database.
    // Args: {"path": "/...", "data": "<base64>", "content_type": "..."}
    // Returns: {"ok": true, "size": N}
    // -----------------------------------------------------------------------
    linker
      .func_wrap(
        "aeordb",
        "aeordb_write_file",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
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

          let dir_ops = DirectoryOps::new(&engine);
          let size = data.len();

          match dir_ops.store_file(&ctx, &path, &data, content_type.as_deref()) {
            Ok(_) => {
              let response = serde_json::json!({
                "ok": true,
                "size": size,
              });
              write_json_response(&mut caller, &response)
            }
            Err(e) => write_error_response(&mut caller, &format!("Write failed: {}", e)),
          }
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_delete_file(ptr, len) -> i64
    // Deletes a file from the database. Args: {"path": "/..."}
    // Returns: {"ok": true}
    // -----------------------------------------------------------------------
    linker
      .func_wrap(
        "aeordb",
        "aeordb_delete_file",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
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

          let dir_ops = DirectoryOps::new(&engine);

          match dir_ops.delete_file(&ctx, &path) {
            Ok(()) => {
              let response = serde_json::json!({"ok": true});
              write_json_response(&mut caller, &response)
            }
            Err(e) => write_error_response(&mut caller, &format!("Delete failed: {}", e)),
          }
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_file_metadata(ptr, len) -> i64
    // Gets file metadata. Args: {"path": "/..."}
    // Returns: {"path": "...", "size": N, "content_type": "...", "created_at": N, "updated_at": N}
    // -----------------------------------------------------------------------
    linker
      .func_wrap(
        "aeordb",
        "aeordb_file_metadata",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
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
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_list_directory(ptr, len) -> i64
    // Lists directory contents. Args: {"path": "/..."}
    // Returns: {"entries": [{"name": "...", "type": "file"|"directory", "size": N}, ...]}
    // -----------------------------------------------------------------------
    linker
      .func_wrap(
        "aeordb",
        "aeordb_list_directory",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
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

          let dir_ops = DirectoryOps::new(&engine);

          match dir_ops.list_directory(&path) {
            Ok(children) => {
              let entries: Vec<serde_json::Value> = children.iter().map(|child| {
                let entry_type = if child.entry_type == EntryType::DirectoryIndex.to_u8() {
                  "directory"
                } else {
                  "file"
                };
                serde_json::json!({
                  "name": child.name,
                  "type": entry_type,
                  "size": child.total_size,
                })
              }).collect();

              let response = serde_json::json!({"entries": entries});
              write_json_response(&mut caller, &response)
            }
            Err(e) => write_error_response(&mut caller, &format!("List failed: {}", e)),
          }
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_query(ptr, len) -> i64
    // Executes a query. Args: same JSON format as POST /query.
    // Returns: {"results": [...], "total_count": N, "has_more": bool}
    // -----------------------------------------------------------------------
    linker
      .func_wrap(
        "aeordb",
        "aeordb_query",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
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

          let query_engine = QueryEngine::new(&engine);
          match query_engine.execute_paginated(&query) {
            Ok(paginated) => {
              let result_items: Vec<serde_json::Value> = paginated.results
                .iter()
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

              let mut response = serde_json::json!({
                "results": result_items,
                "has_more": paginated.has_more,
              });

              if let Some(total) = paginated.total_count {
                response["total_count"] = serde_json::json!(total);
              }

              write_json_response(&mut caller, &response)
            }
            Err(e) => write_error_response(&mut caller, &format!("Query failed: {}", e)),
          }
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // aeordb_aggregate(ptr, len) -> i64
    // Executes an aggregate query. Args: same JSON format as POST /query with aggregate.
    // Returns: the aggregate result as JSON.
    // -----------------------------------------------------------------------
    linker
      .func_wrap(
        "aeordb",
        "aeordb_aggregate",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
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

          if query.aggregate.is_none() {
            return write_error_response(&mut caller, "Missing 'aggregate' section in query");
          }

          let query_engine = QueryEngine::new(&engine);
          match query_engine.execute_aggregate(&query) {
            Ok(result) => {
              match serde_json::to_value(&result) {
                Ok(v) => write_json_response(&mut caller, &v),
                Err(e) => write_error_response(&mut caller, &format!("Serialization failed: {}", e)),
              }
            }
            Err(e) => write_error_response(&mut caller, &format!("Aggregate failed: {}", e)),
          }
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // -----------------------------------------------------------------------
    // log_message(level_ptr, level_len, msg_ptr, msg_len)
    // Reads level and message strings from guest memory and emits a tracing event.
    // -----------------------------------------------------------------------
    linker
      .func_wrap(
        "aeordb",
        "log_message",
        |caller: Caller<'_, HostState>,
         level_ptr: i32,
         level_len: i32,
         msg_ptr: i32,
         msg_len: i32| {
          let memory = match caller.data().memory {
            Some(mem) => mem,
            None => {
              tracing::warn!("log_message called before memory was set");
              return;
            }
          };

          let level_str = {
            let mut buf = vec![0u8; level_len as usize];
            if memory.read(&caller, level_ptr as usize, &mut buf).is_ok() {
              String::from_utf8_lossy(&buf).to_string()
            } else {
              "unknown".to_string()
            }
          };

          let msg_str = {
            let mut buf = vec![0u8; msg_len as usize];
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
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    Ok(())
  }
}

// ---------------------------------------------------------------------------
// Helper functions for host function implementations
// ---------------------------------------------------------------------------

/// Get the engine and a RequestContext from the host state.
/// Uses the stored request_context's user_id to build the context, falling
/// back to "system" when no context is stored.
fn get_engine_and_context(
  caller: &Caller<'_, HostState>,
) -> Result<(Arc<StorageEngine>, RequestContext), String> {
  let engine = caller.data().engine.as_ref()
    .ok_or_else(|| "Database access not available in this plugin context".to_string())?;

  // Build a RequestContext using the user_id from the stored context if available.
  // We can't clone the stored context (it holds an Arc<EventBus>), so we create
  // a system context. The user_id is preserved for auditing.
  let _user_id = caller.data().request_context.as_ref()
    .map(|ctx| ctx.user_id.as_str())
    .unwrap_or("system");

  let ctx = RequestContext::system();
  Ok((Arc::clone(engine), ctx))
}

/// Read JSON arguments from guest memory at the given (ptr, len).
fn read_guest_json(
  caller: &Caller<'_, HostState>,
  ptr: i32,
  len: i32,
) -> Result<serde_json::Value, String> {
  let memory = caller.data().memory
    .ok_or_else(|| "Memory not available".to_string())?;

  let mut buf = vec![0u8; len as usize];
  memory
    .read(caller, ptr as usize, &mut buf)
    .map_err(|_| "Failed to read from guest memory".to_string())?;

  serde_json::from_slice(&buf)
    .map_err(|e| format!("Failed to parse JSON arguments: {}", e))
}

/// Write a JSON response into guest memory at HOST_RESPONSE_OFFSET.
/// Returns packed i64: (ptr << 32) | len.
fn write_json_response(
  caller: &mut Caller<'_, HostState>,
  value: &serde_json::Value,
) -> i64 {
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
fn write_error_response(
  caller: &mut Caller<'_, HostState>,
  message: &str,
) -> i64 {
  let response = serde_json::json!({"error": message});
  write_json_response(caller, &response)
}

// ---------------------------------------------------------------------------
// Query JSON parsing — mirrors the logic from engine_routes.rs
// ---------------------------------------------------------------------------

/// Parse a Query struct from JSON in the same format as POST /query.
fn parse_query_from_json(json: &serde_json::Value) -> Result<Query, String> {
  let path = json.get("path")
    .and_then(|v| v.as_str())
    .ok_or_else(|| "Missing 'path' in query".to_string())?
    .to_string();

  let where_clause = json.get("where").cloned().unwrap_or(serde_json::json!([]));
  let query_node = parse_where_clause(&where_clause)?;
  let is_empty = matches!(&query_node, crate::engine::query_engine::QueryNode::And(children) if children.is_empty());

  let limit = json.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
  let offset = json.get("offset").and_then(|v| v.as_u64()).map(|v| v as usize);
  let after = json.get("after").and_then(|v| v.as_str()).map(|s| s.to_string());
  let before = json.get("before").and_then(|v| v.as_str()).map(|s| s.to_string());
  let include_total = json.get("include_total").and_then(|v| v.as_bool()).unwrap_or(false);

  // Parse order_by
  let order_by: Vec<SortField> = json.get("order_by")
    .and_then(|v| v.as_array())
    .map(|fields| {
      fields.iter().filter_map(|f| {
        let field = f.get("field")?.as_str()?.to_string();
        let direction = match f.get("direction").and_then(|d| d.as_str()) {
          Some("desc") => SortDirection::Desc,
          _ => SortDirection::Asc,
        };
        Some(SortField { field, direction })
      }).collect()
    })
    .unwrap_or_default();

  // Parse aggregate section
  let aggregate = json.get("aggregate").map(|agg| {
    AggregateQuery {
      count: agg.get("count").and_then(|v| v.as_bool()).unwrap_or(false),
      sum: parse_string_array(agg.get("sum")),
      avg: parse_string_array(agg.get("avg")),
      min: parse_string_array(agg.get("min")),
      max: parse_string_array(agg.get("max")),
      group_by: parse_string_array(agg.get("group_by")),
    }
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
    .map(|arr| {
      arr.iter()
        .filter_map(|item| item.as_str().map(|s| s.to_string()))
        .collect()
    })
    .unwrap_or_default()
}

/// Convert a JSON value to the byte representation used by converters.
fn json_value_to_bytes(value: &serde_json::Value) -> Result<Vec<u8>, String> {
  match value {
    serde_json::Value::Number(number) => {
      if let Some(unsigned) = number.as_u64() {
        Ok(unsigned.to_be_bytes().to_vec())
      } else if let Some(signed) = number.as_i64() {
        Ok((signed as u64).to_be_bytes().to_vec())
      } else if let Some(float) = number.as_f64() {
        Ok((float as u64).to_be_bytes().to_vec())
      } else {
        Err("Unsupported number format".to_string())
      }
    }
    serde_json::Value::String(text) => Ok(text.as_bytes().to_vec()),
    serde_json::Value::Bool(flag) => Ok(vec![if *flag { 1 } else { 0 }]),
    other => Err(format!("Unsupported value type: {}", other)),
  }
}

/// Parse a single field-level where clause JSON object into a QueryNode::Field.
fn parse_single_field_query(value: &serde_json::Value) -> Result<crate::engine::query_engine::QueryNode, String> {
  use crate::engine::query_engine::*;

  let field = value.get("field")
    .and_then(|v| v.as_str())
    .ok_or_else(|| "Missing 'field' in where clause".to_string())?;
  let op = value.get("op")
    .and_then(|v| v.as_str())
    .ok_or_else(|| format!("Missing 'op' in where clause for field '{}'", field))?;
  let raw_value = value.get("value")
    .ok_or_else(|| format!("Missing 'value' in where clause for field '{}'", field))?;

  let operation = match op {
    "eq" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|msg| format!("Invalid value for field '{}': {}", field, msg))?;
      QueryOp::Eq(bytes)
    }
    "gt" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|msg| format!("Invalid value for field '{}': {}", field, msg))?;
      QueryOp::Gt(bytes)
    }
    "lt" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|msg| format!("Invalid value for field '{}': {}", field, msg))?;
      QueryOp::Lt(bytes)
    }
    "between" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|msg| format!("Invalid value for field '{}': {}", field, msg))?;
      let raw_value2 = value.get("value2")
        .ok_or_else(|| format!("Missing value2 for 'between' operation on field '{}'", field))?;
      let bytes2 = json_value_to_bytes(raw_value2)
        .map_err(|msg| format!("Invalid value2 for field '{}': {}", field, msg))?;
      QueryOp::Between(bytes, bytes2)
    }
    "in" => {
      let array = raw_value.as_array()
        .ok_or_else(|| format!("'in' operation requires array value for field '{}'", field))?;
      let mut byte_values = Vec::with_capacity(array.len());
      for item in array {
        let bytes = json_value_to_bytes(item)
          .map_err(|msg| format!("Invalid value in 'in' array for field '{}': {}", field, msg))?;
        byte_values.push(bytes);
      }
      QueryOp::In(byte_values)
    }
    "contains" => {
      let s = raw_value.as_str()
        .ok_or_else(|| format!("'contains' requires string value for field '{}'", field))?;
      QueryOp::Contains(s.to_string())
    }
    "similar" => {
      let s = raw_value.as_str()
        .ok_or_else(|| format!("'similar' requires string value for field '{}'", field))?;
      let threshold = value.get("threshold")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.3);
      QueryOp::Similar(s.to_string(), threshold)
    }
    "phonetic" => {
      let s = raw_value.as_str()
        .ok_or_else(|| format!("'phonetic' requires string value for field '{}'", field))?;
      QueryOp::Phonetic(s.to_string())
    }
    "fuzzy" => {
      let s = raw_value.as_str()
        .ok_or_else(|| format!("'fuzzy' requires string value for field '{}'", field))?;

      let fuzziness = match value.get("fuzziness") {
        Some(v) if v.is_string() && v.as_str() == Some("auto") => Fuzziness::Auto,
        Some(v) if v.is_u64() => Fuzziness::Fixed(v.as_u64().unwrap() as usize),
        Some(v) if v.is_i64() => Fuzziness::Fixed(v.as_i64().unwrap().max(0) as usize),
        _ => Fuzziness::Auto,
      };

      let algorithm = match value.get("algorithm").and_then(|v| v.as_str()) {
        Some("jaro_winkler") => FuzzyAlgorithm::JaroWinkler,
        _ => FuzzyAlgorithm::DamerauLevenshtein,
      };

      QueryOp::Fuzzy(s.to_string(), FuzzyOptions { fuzziness, algorithm })
    }
    "match" => {
      let s = raw_value.as_str()
        .ok_or_else(|| format!("'match' requires string value for field '{}'", field))?;
      QueryOp::Match(s.to_string())
    }
    unknown => {
      return Err(format!("Unknown operation: '{}'", unknown));
    }
  };

  Ok(QueryNode::Field(FieldQuery {
    field_name: field.to_string(),
    operation,
  }))
}

/// Recursively parse a where clause JSON value into a QueryNode tree.
fn parse_where_clause(value: &serde_json::Value) -> Result<crate::engine::query_engine::QueryNode, String> {
  use crate::engine::query_engine::QueryNode;

  if value.is_array() {
    let array = value.as_array().unwrap();
    let children: Result<Vec<QueryNode>, String> = array.iter()
      .map(parse_where_clause)
      .collect();
    return Ok(QueryNode::And(children?));
  }

  if let Some(and_array) = value.get("and") {
    let array = and_array.as_array()
      .ok_or_else(|| "'and' must be an array".to_string())?;
    let children: Result<Vec<QueryNode>, String> = array.iter()
      .map(parse_where_clause)
      .collect();
    return Ok(QueryNode::And(children?));
  }

  if let Some(or_array) = value.get("or") {
    let array = or_array.as_array()
      .ok_or_else(|| "'or' must be an array".to_string())?;
    let children: Result<Vec<QueryNode>, String> = array.iter()
      .map(parse_where_clause)
      .collect();
    return Ok(QueryNode::Or(children?));
  }

  if let Some(not_value) = value.get("not") {
    let child = parse_where_clause(not_value)?;
    return Ok(QueryNode::Not(Box::new(child)));
  }

  if value.get("field").is_some() {
    return parse_single_field_query(value);
  }

  Err(format!("Invalid where clause structure: {}", value))
}
