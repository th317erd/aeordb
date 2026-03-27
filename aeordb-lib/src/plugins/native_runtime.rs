use std::path::Path;

/// Error type for native plugin operations.
#[derive(Debug, thiserror::Error)]
pub enum NativeRuntimeError {
  #[error("failed to load native library: {0}")]
  LoadFailed(String),

  #[error("symbol not found in native library: {0}")]
  SymbolNotFound(String),

  #[error("native plugin execution failed: {0}")]
  ExecutionFailed(String),
}

/// C ABI function signature expected from native plugins.
///
/// The plugin receives a pointer to the request bytes and their length,
/// then writes a response into `response_out` (caller-provided buffer)
/// and returns the number of bytes written. Returns a negative value on error.
///
/// ```c
/// int32_t aeordb_handle(
///     const uint8_t* request_ptr,
///     uint32_t request_len,
///     uint8_t* response_ptr,
///     uint32_t response_capacity
/// ) -> int32_t;
/// ```
type AeordbHandleFn = unsafe extern "C" fn(
  request_ptr: *const u8,
  request_len: u32,
  response_ptr: *mut u8,
  response_capacity: u32,
) -> i32;

/// Default maximum response buffer size (4 MB).
const DEFAULT_RESPONSE_CAPACITY: usize = 4 * 1024 * 1024;

/// A runtime for loading and invoking native (shared library) plugins.
#[derive(Debug)]
pub struct NativePluginRuntime {
  library: libloading::Library,
}

impl NativePluginRuntime {
  /// Load a native plugin from the given shared library path.
  ///
  /// The library must export an `aeordb_handle` symbol with the expected C ABI.
  pub fn load(library_path: &Path) -> Result<Self, NativeRuntimeError> {
    if !library_path.exists() {
      return Err(NativeRuntimeError::LoadFailed(format!(
        "library not found: {}",
        library_path.display()
      )));
    }

    let library = unsafe {
      libloading::Library::new(library_path)
        .map_err(|error| NativeRuntimeError::LoadFailed(error.to_string()))?
    };

    // Verify the expected symbol exists before returning.
    unsafe {
      library
        .get::<AeordbHandleFn>(b"aeordb_handle\0")
        .map_err(|error| NativeRuntimeError::SymbolNotFound(error.to_string()))?;
    }

    Ok(Self { library })
  }

  /// Invoke the plugin's `aeordb_handle` function with the given request bytes.
  pub fn call_handle(&self, request_bytes: &[u8]) -> Result<Vec<u8>, NativeRuntimeError> {
    let handle_function: libloading::Symbol<AeordbHandleFn> = unsafe {
      self
        .library
        .get(b"aeordb_handle\0")
        .map_err(|error| NativeRuntimeError::SymbolNotFound(error.to_string()))?
    };

    let mut response_buffer = vec![0u8; DEFAULT_RESPONSE_CAPACITY];

    let bytes_written = unsafe {
      handle_function(
        request_bytes.as_ptr(),
        request_bytes.len() as u32,
        response_buffer.as_mut_ptr(),
        response_buffer.len() as u32,
      )
    };

    if bytes_written < 0 {
      return Err(NativeRuntimeError::ExecutionFailed(format!(
        "plugin returned error code: {}",
        bytes_written
      )));
    }

    response_buffer.truncate(bytes_written as usize);
    Ok(response_buffer)
  }
}
