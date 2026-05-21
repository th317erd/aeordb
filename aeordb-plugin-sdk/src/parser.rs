//! Parser plugin support for AeorDB.
//!
//! # Writing a parser
//!
//! ```rust,no_run
//! use aeordb_plugin_sdk::aeordb_parser;
//! use aeordb_plugin_sdk::parser::*;
//!
//! aeordb_parser!(parse);
//!
//! fn parse(input: ParserInput) -> Result<serde_json::Value, String> {
//!     let text = std::str::from_utf8(&input.data).map_err(|e| e.to_string())?;
//!     Ok(serde_json::json!({
//!         "text": text,
//!         "line_count": text.lines().count(),
//!     }))
//! }
//! ```

use serde::{Deserialize, Serialize};

/// Metadata about the file being parsed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    /// File name only (e.g., "report.pdf")
    pub filename: String,
    /// Full storage path (e.g., "/docs/reports/report.pdf")
    pub path: String,
    /// MIME type
    pub content_type: String,
    /// Raw file size in bytes
    pub size: u64,
    /// Hex-encoded content hash (optional, may be empty)
    #[serde(default)]
    pub hash: String,
    /// Hash algorithm (e.g., "blake3_256")
    #[serde(default)]
    pub hash_algorithm: String,
    /// Creation timestamp (ms since epoch)
    #[serde(default)]
    pub created_at: i64,
    /// Last update timestamp (ms since epoch)
    #[serde(default)]
    pub updated_at: i64,
}

/// Input to a parser function. The SDK handles envelope deserialization
/// and base64 decoding — the parser receives raw bytes and metadata.
#[derive(Debug, Clone)]
pub struct ParserInput {
    /// Raw file bytes (base64-decoded from the envelope)
    pub data: Vec<u8>,
    /// File metadata
    pub meta: FileMeta,
}

/// The raw envelope format sent by the AeorDB host.
/// Users don't interact with this directly — the macro handles it.
#[derive(Debug, Deserialize)]
pub(crate) struct ParserEnvelope {
    /// Base64-encoded file data
    pub data: String,
    /// File metadata
    pub meta: FileMeta,
}

impl ParserEnvelope {
    /// Decode the envelope into a ParserInput.
    pub fn into_input(self) -> Result<ParserInput, String> {
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD
            .decode(&self.data)
            .map_err(|e| format!("base64 decode failed: {}", e))?;
        Ok(ParserInput {
            data,
            meta: self.meta,
        })
    }
}

/// Deserialize the raw envelope bytes into a ParserInput.
/// Called by the `aeordb_parser!` macro — not typically used directly.
pub fn decode_envelope(raw_bytes: &[u8]) -> Result<ParserInput, String> {
    let envelope: ParserEnvelope = serde_json::from_slice(raw_bytes)
        .map_err(|e| format!("envelope parse failed: {}", e))?;
    envelope.into_input()
}

/// Serialize a JSON value into response bytes.
/// Called by the `aeordb_parser!` macro — not typically used directly.
pub fn encode_response(value: &serde_json::Value) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value).map_err(|e| format!("response serialization failed: {}", e))
}

/// Serialize an error into response bytes (as a JSON error object).
pub fn encode_error(message: &str) -> Vec<u8> {
    let error = serde_json::json!({"error": message});
    serde_json::to_vec(&error).unwrap_or_else(|_| b"{}".to_vec())
}

/// Generate the WASM `handle` export function that deserializes the parser
/// envelope, calls the user's parse function, and returns the serialized response.
///
/// # Usage
///
/// ```rust,no_run
/// use aeordb_plugin_sdk::aeordb_parser;
/// use aeordb_plugin_sdk::parser::*;
///
/// aeordb_parser!(my_parse_fn);
///
/// fn my_parse_fn(input: ParserInput) -> Result<serde_json::Value, String> {
///     Ok(serde_json::json!({"hello": "world"}))
/// }
/// ```
#[macro_export]
macro_rules! aeordb_parser {
    ($parse_fn:ident) => {
        // Global allocator for WASM — required for dynamic allocation
        #[cfg(target_arch = "wasm32")]
        #[global_allocator]
        static ALLOC: std::alloc::System = std::alloc::System;

        /// WASM export: handle(request_ptr, request_len) -> i64
        /// Returns packed (response_ptr << 32) | response_len
        #[no_mangle]
        pub extern "C" fn handle(ptr: i32, len: i32) -> i64 {
            // Read request bytes from linear memory
            let request_bytes = unsafe {
                std::slice::from_raw_parts(ptr as *const u8, len as usize)
            };

            // Decode envelope and call the user's parse function
            let response_bytes = match $crate::parser::decode_envelope(request_bytes) {
                Ok(input) => {
                    match $parse_fn(input) {
                        Ok(value) => {
                            match $crate::parser::encode_response(&value) {
                                Ok(bytes) => bytes,
                                Err(e) => $crate::parser::encode_error(&e),
                            }
                        }
                        Err(e) => $crate::parser::encode_error(&e),
                    }
                }
                Err(e) => $crate::parser::encode_error(&e),
            };

            // Allocate response in WASM memory and return packed pointer
            let response_len = response_bytes.len();
            let response_ptr = response_bytes.as_ptr() as i64;
            std::mem::forget(response_bytes); // Don't deallocate — host will read it
            (response_ptr << 32) | (response_len as i64)
        }
    };
}

// Re-export serde_json for parser authors
pub use serde_json;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_envelope_valid() {
        use base64::Engine as _;
        let data = b"Hello World";
        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        let envelope = serde_json::json!({
            "data": encoded,
            "meta": {
                "filename": "test.txt",
                "path": "/docs/test.txt",
                "content_type": "text/plain",
                "size": 11
            }
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let input = decode_envelope(&bytes).unwrap();
        assert_eq!(input.data, b"Hello World");
        assert_eq!(input.meta.filename, "test.txt");
        assert_eq!(input.meta.path, "/docs/test.txt");
        assert_eq!(input.meta.content_type, "text/plain");
        assert_eq!(input.meta.size, 11);
    }

    #[test]
    fn test_decode_envelope_invalid_json() {
        let result = decode_envelope(b"not json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("envelope parse failed"), "error was: {}", err);
    }

    #[test]
    fn test_decode_envelope_invalid_base64() {
        let envelope = serde_json::json!({
            "data": "!!!not-base64!!!",
            "meta": {"filename":"x","path":"/x","content_type":"text/plain","size":0}
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let result = decode_envelope(&bytes);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("base64 decode failed"), "error was: {}", err);
    }

    #[test]
    fn test_decode_envelope_missing_required_fields() {
        // Missing 'meta' entirely
        let envelope = serde_json::json!({"data": "aGVsbG8="});
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let result = decode_envelope(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_envelope_missing_data_field() {
        let envelope = serde_json::json!({
            "meta": {"filename":"x","path":"/x","content_type":"text/plain","size":0}
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let result = decode_envelope(&bytes);
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_envelope_empty_data() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"");
        let envelope = serde_json::json!({
            "data": encoded,
            "meta": {"filename":"empty.txt","path":"/empty.txt","content_type":"text/plain","size":0}
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let input = decode_envelope(&bytes).unwrap();
        assert!(input.data.is_empty());
        assert_eq!(input.meta.filename, "empty.txt");
    }

    #[test]
    fn test_decode_envelope_binary_data() {
        use base64::Engine as _;
        let binary: Vec<u8> = (0..=255).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&binary);
        let envelope = serde_json::json!({
            "data": encoded,
            "meta": {"filename":"bin.dat","path":"/bin.dat","content_type":"application/octet-stream","size":256}
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let input = decode_envelope(&bytes).unwrap();
        assert_eq!(input.data, binary);
    }

    #[test]
    fn test_decode_envelope_large_data() {
        use base64::Engine as _;
        let large_data = vec![0x42u8; 100_000];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&large_data);
        let envelope = serde_json::json!({
            "data": encoded,
            "meta": {"filename":"large.bin","path":"/large.bin","content_type":"application/octet-stream","size":100000}
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let input = decode_envelope(&bytes).unwrap();
        assert_eq!(input.data.len(), 100_000);
        assert!(input.data.iter().all(|&b| b == 0x42));
    }

    #[test]
    fn test_encode_response() {
        let value = serde_json::json!({"key": "value"});
        let bytes = encode_response(&value).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn test_encode_response_complex_json() {
        let value = serde_json::json!({
            "text": "hello",
            "metadata": {"line_count": 5, "word_count": 10},
            "tags": ["a", "b", "c"],
            "nested": {"deep": {"value": true}}
        });
        let bytes = encode_response(&value).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["metadata"]["line_count"], 5);
        assert_eq!(parsed["tags"][1], "b");
        assert_eq!(parsed["nested"]["deep"]["value"], true);
    }

    #[test]
    fn test_encode_response_null() {
        let value = serde_json::Value::Null;
        let bytes = encode_response(&value).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(parsed.is_null());
    }

    #[test]
    fn test_encode_error() {
        let bytes = encode_error("something broke");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["error"], "something broke");
    }

    #[test]
    fn test_encode_error_empty_message() {
        let bytes = encode_error("");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["error"], "");
    }

    #[test]
    fn test_encode_error_special_chars() {
        let bytes = encode_error("error with \"quotes\" and \\ backslash and \n newline");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(parsed["error"].as_str().unwrap().contains("quotes"));
        assert!(parsed["error"].as_str().unwrap().contains("backslash"));
    }

    #[test]
    fn test_file_meta_defaults() {
        let envelope = serde_json::json!({
            "data": "aGVsbG8=",
            "meta": {
                "filename": "test.txt",
                "path": "/test.txt",
                "content_type": "text/plain",
                "size": 5
            }
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let input = decode_envelope(&bytes).unwrap();
        // Optional fields should default
        assert_eq!(input.meta.hash, "");
        assert_eq!(input.meta.hash_algorithm, "");
        assert_eq!(input.meta.created_at, 0);
        assert_eq!(input.meta.updated_at, 0);
    }

    #[test]
    fn test_file_meta_with_all_fields() {
        let envelope = serde_json::json!({
            "data": "aGVsbG8=",
            "meta": {
                "filename": "test.txt",
                "path": "/test.txt",
                "content_type": "text/plain",
                "size": 5,
                "hash": "abc123",
                "hash_algorithm": "blake3_256",
                "created_at": 1700000000000_i64,
                "updated_at": 1700000001000_i64,
            }
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let input = decode_envelope(&bytes).unwrap();
        assert_eq!(input.meta.hash, "abc123");
        assert_eq!(input.meta.hash_algorithm, "blake3_256");
        assert_eq!(input.meta.created_at, 1700000000000);
        assert_eq!(input.meta.updated_at, 1700000001000);
    }

    #[test]
    fn test_file_meta_serializes_correctly() {
        let meta = FileMeta {
            filename: "test.txt".to_string(),
            path: "/docs/test.txt".to_string(),
            content_type: "text/plain".to_string(),
            size: 42,
            hash: "deadbeef".to_string(),
            hash_algorithm: "blake3_256".to_string(),
            created_at: 1000,
            updated_at: 2000,
        };
        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(json["filename"], "test.txt");
        assert_eq!(json["size"], 42);
        assert_eq!(json["hash"], "deadbeef");
    }

    #[test]
    fn test_decode_envelope_empty_bytes() {
        let result = decode_envelope(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_roundtrip_encode_decode() {
        
        // Encode a response, then wrap it in an envelope and decode
        let original = serde_json::json!({"text": "round trip", "count": 7});
        let response_bytes = encode_response(&original).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&response_bytes).unwrap();
        assert_eq!(original, parsed);
    }
}
