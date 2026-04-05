use wasmi::{
  Caller, Config, Engine, Extern, Linker, Memory, MemoryType, Module, Store,
};

/// Default maximum memory in bytes (16 MB).
const DEFAULT_MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Default fuel budget for execution metering.
const DEFAULT_FUEL_LIMIT: u64 = 1_000_000;

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
    let mut store = Store::new(&self.engine, HostState { memory: None });
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

  /// Register stub host functions that the WASM module can import.
  ///
  /// These are placeholders — the actual database interaction functions
  /// will be implemented in a future phase.
  fn register_host_functions(
    &self,
    linker: &mut Linker<HostState>,
  ) -> Result<(), WasmRuntimeError> {
    // -----------------------------------------------------------------------
    // TODO: Host function stubs
    //
    // These stubs allow WASM modules that import these functions to link
    // successfully. They do NOT perform real database operations yet.
    // -----------------------------------------------------------------------

    // TODO: db_read(table_ptr, table_len, key_ptr, key_len) -> i64
    //   Returns a packed pointer+length to the value, or 0 if not found.
    linker
      .func_wrap(
        "aeordb",
        "db_read",
        |_caller: Caller<'_, HostState>,
         _table_ptr: i32,
         _table_len: i32,
         _key_ptr: i32,
         _key_len: i32|
         -> i64 {
          tracing::warn!("TODO: db_read host function called but not yet implemented");
          0i64
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // TODO: db_write(table_ptr, table_len, key_ptr, key_len, value_ptr, value_len) -> i32
    //   Returns 0 on success, negative on error.
    linker
      .func_wrap(
        "aeordb",
        "db_write",
        |_caller: Caller<'_, HostState>,
         _table_ptr: i32,
         _table_len: i32,
         _key_ptr: i32,
         _key_len: i32,
         _value_ptr: i32,
         _value_len: i32|
         -> i32 {
          tracing::warn!("TODO: db_write host function called but not yet implemented");
          -1i32
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // TODO: db_delete(table_ptr, table_len, key_ptr, key_len) -> i32
    //   Returns 0 on success, negative on error.
    linker
      .func_wrap(
        "aeordb",
        "db_delete",
        |_caller: Caller<'_, HostState>,
         _table_ptr: i32,
         _table_len: i32,
         _key_ptr: i32,
         _key_len: i32|
         -> i32 {
          tracing::warn!("TODO: db_delete host function called but not yet implemented");
          -1i32
        },
      )
      .map_err(|error| WasmRuntimeError::InstantiationFailed(error.to_string()))?;

    // log_message(level_ptr, level_len, msg_ptr, msg_len)
    // Reads level and message strings from guest memory and emits a tracing event.
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
