//! # AeorDB Plugin SDK
//!
//! SDK for building WASM plugins that run inside AeorDB.
//!
//! ## Plugin Types
//!
//! - **Parsers** — transform non-JSON files into queryable JSON on ingest
//! - **Query Plugins** — server-side functions with full database access
//!
//! ## Parser Example
//!
//! ```rust,no_run
//! use aeordb_plugin_sdk::aeordb_parser;
//! use aeordb_plugin_sdk::parser::*;
//!
//! aeordb_parser!(parse);
//!
//! fn parse(input: ParserInput) -> Result<serde_json::Value, String> {
//!     Ok(serde_json::json!({"text": std::str::from_utf8(&input.data).unwrap_or("")}))
//! }
//! ```
//!
//! ## Query Plugin Example
//!
//! ```rust,no_run
//! use aeordb_plugin_sdk::prelude::*;
//! use aeordb_plugin_sdk::aeordb_query_plugin;
//!
//! aeordb_query_plugin!(handle);
//!
//! fn handle(ctx: PluginContext, req: PluginRequest) -> Result<PluginResponse, PluginError> {
//!     let results = ctx.query("/users").field("name").contains("Alice").execute()?;
//!     PluginResponse::json(200, &results).map_err(|e| PluginError::SerializationFailed(e.to_string()))
//! }
//! ```

pub mod context;
pub mod parser;
pub mod query_builder;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// Re-export serde_json so macros can reference it as `$crate::serde_json`.
pub use serde_json;

/// Request passed to a plugin when it is invoked.
///
/// Contains the raw argument bytes from the HTTP request body and
/// key-value metadata about the invocation context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRequest {
  /// Raw argument bytes (e.g. the HTTP request body forwarded to the plugin).
  pub arguments: Vec<u8>,
  /// Arbitrary key-value metadata about the invocation context.
  pub metadata: HashMap<String, String>,
}

/// Response returned by a plugin after handling a request.
///
/// Use the convenience constructors [`PluginResponse::json`],
/// [`PluginResponse::text`], or [`PluginResponse::error`] to build responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginResponse {
  /// HTTP-style status code indicating the outcome.
  pub status_code: u16,
  /// Raw response body bytes.
  pub body: Vec<u8>,
  /// Optional MIME content type of the body.
  pub content_type: Option<String>,
  /// Arbitrary response headers the plugin wants surfaced.
  pub headers: HashMap<String, String>,
}

impl PluginResponse {
  /// Build a JSON response from a serializable body.
  pub fn json<T: Serialize>(status_code: u16, body: &T) -> Result<Self, serde_json::Error> {
    let serialized = serde_json::to_vec(body)?;
    Ok(Self {
      status_code,
      body: serialized,
      content_type: Some("application/json".to_string()),
      headers: HashMap::new(),
    })
  }

  /// Build a plain-text response.
  pub fn text(status_code: u16, body: impl Into<String>) -> Self {
    Self {
      status_code,
      body: body.into().into_bytes(),
      content_type: Some("text/plain".to_string()),
      headers: HashMap::new(),
    }
  }

  /// Build a JSON error response with a `{"error": "<message>"}` body.
  pub fn error(status_code: u16, message: impl Into<String>) -> Self {
    let error_body = serde_json::json!({ "error": message.into() });
    Self {
      status_code,
      body: serde_json::to_vec(&error_body).unwrap_or_default(),
      content_type: Some("application/json".to_string()),
      headers: HashMap::new(),
    }
  }
}

/// Errors that can occur within the plugin system.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PluginError {
  /// The plugin could not be found.
  NotFound(String),
  /// The plugin failed during execution.
  ExecutionFailed(String),
  /// The plugin request or response could not be serialized/deserialized.
  SerializationFailed(String),
  /// The plugin exceeded its resource limits (memory, fuel, etc.).
  ResourceLimitExceeded(String),
  /// An invalid or corrupt WASM module was provided.
  InvalidModule(String),
  /// A generic internal error.
  Internal(String),
}

impl std::fmt::Display for PluginError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      PluginError::NotFound(message) => write!(formatter, "plugin not found: {}", message),
      PluginError::ExecutionFailed(message) => {
        write!(formatter, "plugin execution failed: {}", message)
      }
      PluginError::SerializationFailed(message) => {
        write!(formatter, "serialization failed: {}", message)
      }
      PluginError::ResourceLimitExceeded(message) => {
        write!(formatter, "resource limit exceeded: {}", message)
      }
      PluginError::InvalidModule(message) => write!(formatter, "invalid module: {}", message),
      PluginError::Internal(message) => write!(formatter, "internal error: {}", message),
    }
  }
}

impl std::error::Error for PluginError {}

/// Generate WASM exports (`alloc` + `handle`) for a query plugin.
///
/// The macro creates:
/// - A global allocator for WASM targets
/// - An `alloc(size) -> ptr` export for the host to allocate guest memory
/// - A `handle(ptr, len) -> i64` export that deserializes the request, calls
///   the user function with a [`context::PluginContext`], and returns a packed
///   pointer+length to the serialized response.
///
/// # Usage
///
/// ```rust,no_run
/// use aeordb_plugin_sdk::prelude::*;
/// use aeordb_plugin_sdk::aeordb_query_plugin;
///
/// aeordb_query_plugin!(handle_query);
///
/// fn handle_query(
///     ctx: PluginContext,
///     request: PluginRequest,
/// ) -> Result<PluginResponse, PluginError> {
///     let results = ctx.query("/users")
///         .field("name").contains("Wyatt")
///         .limit(10)
///         .execute()?;
///     PluginResponse::json(200, &results).map_err(|e| {
///         PluginError::SerializationFailed(e.to_string())
///     })
/// }
/// ```
#[macro_export]
macro_rules! aeordb_query_plugin {
    ($handler_fn:ident) => {
        #[cfg(target_arch = "wasm32")]
        #[global_allocator]
        static ALLOC: std::alloc::System = std::alloc::System;

        /// WASM export: allocate `size` bytes in guest memory and return the pointer.
        /// Used by the host to write request data into guest linear memory.
        #[no_mangle]
        pub extern "C" fn alloc(size: i32) -> i32 {
            let mut buffer = Vec::<u8>::with_capacity(size as usize);
            let ptr = buffer.as_mut_ptr();
            std::mem::forget(buffer);
            ptr as i32
        }

        /// WASM export: handle a plugin request.
        ///
        /// The host writes request JSON at `(ptr, len)` in guest memory.
        /// Returns a packed i64: `(response_ptr << 32) | response_len`.
        #[no_mangle]
        pub extern "C" fn handle(ptr: i32, len: i32) -> i64 {
            let request_bytes = unsafe {
                std::slice::from_raw_parts(ptr as *const u8, len as usize)
            };

            let request: $crate::PluginRequest = match $crate::serde_json::from_slice(request_bytes) {
                Ok(req) => req,
                Err(e) => {
                    let response = $crate::PluginResponse::error(
                        400,
                        &format!("Invalid request: {}", e),
                    );
                    return _aeordb_encode_plugin_response(&response);
                }
            };

            let ctx = $crate::context::PluginContext::new();

            let response = match $handler_fn(ctx, request) {
                Ok(resp) => resp,
                Err(e) => $crate::PluginResponse::error(500, &e.to_string()),
            };

            _aeordb_encode_plugin_response(&response)
        }

        /// Internal helper: serialize a PluginResponse and return packed ptr+len.
        fn _aeordb_encode_plugin_response(response: &$crate::PluginResponse) -> i64 {
            let bytes = $crate::serde_json::to_vec(response).unwrap_or_default();
            let len = bytes.len();
            let ptr = bytes.as_ptr() as i64;
            std::mem::forget(bytes);
            (ptr << 32) | (len as i64)
        }
    };
}

/// Prelude module for convenient imports.
pub mod prelude {
  pub use super::{PluginError, PluginRequest, PluginResponse};
  pub use super::parser::{ParserInput, FileMeta};
  pub use super::context::{PluginContext, FileData, DirEntry, FileMetadata};
  pub use super::query_builder::{QueryResult, AggregateResult, SortDirection};
}
