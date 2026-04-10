//! Guest-side plugin context for calling host functions from WASM.
//!
//! The [`PluginContext`] struct provides methods that call into the AeorDB host
//! runtime via FFI.  On native (non-WASM) targets the same API compiles but
//! every host call returns an error — this allows IDE support and unit testing
//! of plugin logic without requiring an actual WASM runtime.

use serde::{Deserialize, Serialize};

use crate::PluginError;

// ---------------------------------------------------------------------------
// FFI declarations — only available when compiled for wasm32
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "aeordb")]
extern "C" {
    fn aeordb_read_file(ptr: i32, len: i32) -> i64;
    fn aeordb_write_file(ptr: i32, len: i32) -> i64;
    fn aeordb_delete_file(ptr: i32, len: i32) -> i64;
    fn aeordb_file_metadata(ptr: i32, len: i32) -> i64;
    fn aeordb_list_directory(ptr: i32, len: i32) -> i64;
    fn aeordb_query(ptr: i32, len: i32) -> i64;
    fn aeordb_aggregate(ptr: i32, len: i32) -> i64;
}

// ---------------------------------------------------------------------------
// Types returned by host function calls
// ---------------------------------------------------------------------------

/// Raw file data returned by `read_file`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileData {
    /// Decoded file bytes.
    pub data: Vec<u8>,
    /// MIME content type.
    pub content_type: String,
    /// File size in bytes.
    pub size: u64,
}

/// A single directory entry returned by `list_directory`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    /// Entry name (file or directory name, not the full path).
    pub name: String,
    /// `"file"` or `"directory"`.
    #[serde(rename = "type")]
    pub entry_type: String,
    /// Size in bytes (0 for directories).
    #[serde(default)]
    pub size: u64,
}

/// Metadata about a stored file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    /// Full storage path.
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    /// MIME content type (if known).
    pub content_type: Option<String>,
    /// Creation timestamp (ms since epoch).
    pub created_at: i64,
    /// Last update timestamp (ms since epoch).
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// Host function call helpers
// ---------------------------------------------------------------------------

/// Invoke a host function on the WASM target.
///
/// 1. Serialize `args` to JSON bytes.
/// 2. Call the extern FFI function with `(ptr, len)`.
/// 3. Unpack the i64 return as `(response_ptr << 32) | response_len`.
/// 4. Read the response bytes from linear memory.
/// 5. Deserialize as JSON and check for `{"error": "..."}`.
#[cfg(target_arch = "wasm32")]
fn call_host_function(
    host_fn: unsafe extern "C" fn(i32, i32) -> i64,
    args: &serde_json::Value,
) -> Result<serde_json::Value, PluginError> {
    let args_bytes = serde_json::to_vec(args)
        .map_err(|e| PluginError::SerializationFailed(format!("failed to serialize args: {}", e)))?;

    let packed = unsafe { host_fn(args_bytes.as_ptr() as i32, args_bytes.len() as i32) };

    let response_ptr = (packed >> 32) as i32;
    let response_len = (packed & 0xFFFF_FFFF) as i32;

    if response_len <= 0 {
        return Err(PluginError::ExecutionFailed(
            "host function returned empty response".to_string(),
        ));
    }

    let response_bytes = unsafe {
        std::slice::from_raw_parts(response_ptr as *const u8, response_len as usize)
    };

    let value: serde_json::Value = serde_json::from_slice(response_bytes)
        .map_err(|e| PluginError::SerializationFailed(format!("failed to parse response: {}", e)))?;

    // Check for error envelope
    if let Some(error_message) = value.get("error").and_then(|v| v.as_str()) {
        return Err(PluginError::ExecutionFailed(error_message.to_string()));
    }

    Ok(value)
}

/// Stub implementation for non-WASM targets — always returns an error.
#[cfg(not(target_arch = "wasm32"))]
fn call_host_function(
    _host_fn_name: &str,
    _args: &serde_json::Value,
) -> Result<serde_json::Value, PluginError> {
    Err(PluginError::ExecutionFailed(
        "host functions only available in WASM context".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Thin pub(crate) wrappers for QueryBuilder / AggregateBuilder
// ---------------------------------------------------------------------------

/// Call the `aeordb_query` host function with the given JSON arguments.
pub(crate) fn call_query(args: &serde_json::Value) -> Result<serde_json::Value, PluginError> {
    #[cfg(target_arch = "wasm32")]
    {
        call_host_function(aeordb_query, args)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        call_host_function("aeordb_query", args)
    }
}

/// Call the `aeordb_aggregate` host function with the given JSON arguments.
pub(crate) fn call_aggregate(args: &serde_json::Value) -> Result<serde_json::Value, PluginError> {
    #[cfg(target_arch = "wasm32")]
    {
        call_host_function(aeordb_aggregate, args)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        call_host_function("aeordb_aggregate", args)
    }
}

// ---------------------------------------------------------------------------
// PluginContext
// ---------------------------------------------------------------------------

/// Guest-side handle for calling AeorDB host functions.
///
/// Created automatically by the `aeordb_query_plugin!` macro and passed to the
/// plugin handler.  All methods delegate to host FFI on WASM targets and return
/// `PluginError::ExecutionFailed` on native targets.
#[derive(Debug, Clone)]
pub struct PluginContext {
    _private: (),
}

impl PluginContext {
    /// Create a new context.  This is normally called by the macro-generated
    /// `handle` export — plugin authors rarely need to call this directly.
    pub fn new() -> Self {
        Self { _private: () }
    }

    // -- File operations ----------------------------------------------------

    /// Read a file at the given path.
    pub fn read_file(&self, path: &str) -> Result<FileData, PluginError> {
        let args = serde_json::json!({ "path": path });
        let value = self.call("aeordb_read_file", &args)?;

        // The host returns base64-encoded data
        let data_b64 = value
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let data = base64_decode(data_b64)?;

        let content_type = value
            .get("content_type")
            .and_then(|v| v.as_str())
            .unwrap_or("application/octet-stream")
            .to_string();
        let size = value
            .get("size")
            .and_then(|v| v.as_u64())
            .unwrap_or(data.len() as u64);

        Ok(FileData {
            data,
            content_type,
            size,
        })
    }

    /// Write (create or overwrite) a file.
    pub fn write_file(
        &self,
        path: &str,
        data: &[u8],
        content_type: &str,
    ) -> Result<(), PluginError> {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        let args = serde_json::json!({
            "path": path,
            "data": encoded,
            "content_type": content_type,
        });
        self.call("aeordb_write_file", &args)?;
        Ok(())
    }

    /// Delete a file at the given path.
    pub fn delete_file(&self, path: &str) -> Result<(), PluginError> {
        let args = serde_json::json!({ "path": path });
        self.call("aeordb_delete_file", &args)?;
        Ok(())
    }

    /// Retrieve metadata for a file.
    pub fn file_metadata(&self, path: &str) -> Result<FileMetadata, PluginError> {
        let args = serde_json::json!({ "path": path });
        let value = self.call("aeordb_file_metadata", &args)?;
        serde_json::from_value(value).map_err(|e| {
            PluginError::SerializationFailed(format!("failed to parse file metadata: {}", e))
        })
    }

    /// List directory entries at the given path.
    pub fn list_directory(&self, path: &str) -> Result<Vec<DirEntry>, PluginError> {
        let args = serde_json::json!({ "path": path });
        let value = self.call("aeordb_list_directory", &args)?;

        // The host may return { "entries": [...] } or a bare array.
        let entries_value = value
            .get("entries")
            .cloned()
            .unwrap_or(value);

        serde_json::from_value(entries_value).map_err(|e| {
            PluginError::SerializationFailed(format!("failed to parse directory listing: {}", e))
        })
    }

    /// Start building a query against files at the given path.
    pub fn query(&self, path: &str) -> crate::query_builder::QueryBuilder {
        crate::query_builder::QueryBuilder::new(path)
    }

    /// Start building an aggregation against files at the given path.
    pub fn aggregate(&self, path: &str) -> crate::query_builder::AggregateBuilder {
        crate::query_builder::AggregateBuilder::new(path)
    }

    // -- Internal -----------------------------------------------------------

    /// Dispatch a host function call by name.
    #[cfg(target_arch = "wasm32")]
    fn call(&self, function_name: &str, args: &serde_json::Value) -> Result<serde_json::Value, PluginError> {
        let host_fn = match function_name {
            "aeordb_read_file" => aeordb_read_file,
            "aeordb_write_file" => aeordb_write_file,
            "aeordb_delete_file" => aeordb_delete_file,
            "aeordb_file_metadata" => aeordb_file_metadata,
            "aeordb_list_directory" => aeordb_list_directory,
            "aeordb_query" => aeordb_query,
            "aeordb_aggregate" => aeordb_aggregate,
            other => {
                return Err(PluginError::ExecutionFailed(format!(
                    "unknown host function: {}",
                    other
                )));
            }
        };
        call_host_function(host_fn, args)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn call(&self, function_name: &str, args: &serde_json::Value) -> Result<serde_json::Value, PluginError> {
        call_host_function(function_name, args)
    }
}

impl Default for PluginContext {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

fn base64_decode(encoded: &str) -> Result<Vec<u8>, PluginError> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| PluginError::SerializationFailed(format!("base64 decode failed: {}", e)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plugin_context_new() {
        let context = PluginContext::new();
        // Just ensure it constructs without panicking
        let _ = format!("{:?}", context);
    }

    #[test]
    fn test_plugin_context_default() {
        let context = PluginContext::default();
        let _ = format!("{:?}", context);
    }

    // On native targets every host call should return an error.

    #[test]
    fn test_read_file_native_error() {
        let context = PluginContext::new();
        let result = context.read_file("/some/path");
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::ExecutionFailed(message) => {
                assert!(
                    message.contains("WASM context"),
                    "unexpected error: {}",
                    message
                );
            }
            other => panic!("expected ExecutionFailed, got: {:?}", other),
        }
    }

    #[test]
    fn test_write_file_native_error() {
        let context = PluginContext::new();
        let result = context.write_file("/some/path", b"hello", "text/plain");
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::ExecutionFailed(message) => {
                assert!(message.contains("WASM context"));
            }
            other => panic!("expected ExecutionFailed, got: {:?}", other),
        }
    }

    #[test]
    fn test_delete_file_native_error() {
        let context = PluginContext::new();
        let result = context.delete_file("/some/path");
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::ExecutionFailed(message) => {
                assert!(message.contains("WASM context"));
            }
            other => panic!("expected ExecutionFailed, got: {:?}", other),
        }
    }

    #[test]
    fn test_file_metadata_native_error() {
        let context = PluginContext::new();
        let result = context.file_metadata("/some/path");
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::ExecutionFailed(message) => {
                assert!(message.contains("WASM context"));
            }
            other => panic!("expected ExecutionFailed, got: {:?}", other),
        }
    }

    #[test]
    fn test_list_directory_native_error() {
        let context = PluginContext::new();
        let result = context.list_directory("/some/path");
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::ExecutionFailed(message) => {
                assert!(message.contains("WASM context"));
            }
            other => panic!("expected ExecutionFailed, got: {:?}", other),
        }
    }

    // -- Serialization round-trips ------------------------------------------

    #[test]
    fn test_file_data_serialization() {
        let file_data = FileData {
            data: vec![1, 2, 3],
            content_type: "application/octet-stream".to_string(),
            size: 3,
        };
        let json = serde_json::to_value(&file_data).unwrap();
        assert_eq!(json["content_type"], "application/octet-stream");
        assert_eq!(json["size"], 3);

        let roundtripped: FileData = serde_json::from_value(json).unwrap();
        assert_eq!(roundtripped.data, vec![1, 2, 3]);
        assert_eq!(roundtripped.size, 3);
    }

    #[test]
    fn test_dir_entry_serialization() {
        let entry = DirEntry {
            name: "readme.md".to_string(),
            entry_type: "file".to_string(),
            size: 1024,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["name"], "readme.md");
        assert_eq!(json["type"], "file");
        assert_eq!(json["size"], 1024);

        let roundtripped: DirEntry = serde_json::from_value(json).unwrap();
        assert_eq!(roundtripped.name, "readme.md");
        assert_eq!(roundtripped.entry_type, "file");
    }

    #[test]
    fn test_dir_entry_default_size() {
        let json = serde_json::json!({
            "name": "subdir",
            "type": "directory"
        });
        let entry: DirEntry = serde_json::from_value(json).unwrap();
        assert_eq!(entry.size, 0);
        assert_eq!(entry.entry_type, "directory");
    }

    #[test]
    fn test_file_metadata_serialization() {
        let metadata = FileMetadata {
            path: "/docs/file.txt".to_string(),
            size: 4096,
            content_type: Some("text/plain".to_string()),
            created_at: 1700000000000,
            updated_at: 1700000001000,
        };
        let json = serde_json::to_value(&metadata).unwrap();
        assert_eq!(json["path"], "/docs/file.txt");
        assert_eq!(json["size"], 4096);
        assert_eq!(json["content_type"], "text/plain");

        let roundtripped: FileMetadata = serde_json::from_value(json).unwrap();
        assert_eq!(roundtripped.path, "/docs/file.txt");
        assert_eq!(roundtripped.created_at, 1700000000000);
    }

    #[test]
    fn test_file_metadata_optional_content_type() {
        let json = serde_json::json!({
            "path": "/bin/data",
            "size": 512,
            "content_type": null,
            "created_at": 0,
            "updated_at": 0
        });
        let metadata: FileMetadata = serde_json::from_value(json).unwrap();
        assert!(metadata.content_type.is_none());
    }

    #[test]
    fn test_base64_decode_valid() {
        let decoded = base64_decode("SGVsbG8gV29ybGQ=").unwrap();
        assert_eq!(decoded, b"Hello World");
    }

    #[test]
    fn test_base64_decode_empty() {
        let decoded = base64_decode("").unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_base64_decode_invalid() {
        let result = base64_decode("!!!not-base64!!!");
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::SerializationFailed(message) => {
                assert!(message.contains("base64"));
            }
            other => panic!("expected SerializationFailed, got: {:?}", other),
        }
    }

    #[test]
    fn test_call_query_native_error() {
        let args = serde_json::json!({"path": "/users"});
        let result = call_query(&args);
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::ExecutionFailed(message) => {
                assert!(message.contains("WASM context"));
            }
            other => panic!("expected ExecutionFailed, got: {:?}", other),
        }
    }

    #[test]
    fn test_call_aggregate_native_error() {
        let args = serde_json::json!({"path": "/users"});
        let result = call_aggregate(&args);
        assert!(result.is_err());
        match result.unwrap_err() {
            PluginError::ExecutionFailed(message) => {
                assert!(message.contains("WASM context"));
            }
            other => panic!("expected ExecutionFailed, got: {:?}", other),
        }
    }

    #[test]
    fn test_query_builder_from_context() {
        let context = PluginContext::new();
        let builder = context.query("/users");
        // Should serialize cleanly even with no conditions
        let json = builder.to_json();
        assert_eq!(json["path"], "/users");
    }

    #[test]
    fn test_aggregate_builder_from_context() {
        let context = PluginContext::new();
        let builder = context.aggregate("/users");
        let json = builder.to_json();
        assert_eq!(json["path"], "/users");
    }
}
