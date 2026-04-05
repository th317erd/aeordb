pub mod parser;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Request passed to a plugin when it is invoked.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRequest {
  /// Raw argument bytes (e.g. the HTTP request body forwarded to the plugin).
  pub arguments: Vec<u8>,
  /// Arbitrary key-value metadata about the invocation context.
  pub metadata: HashMap<String, String>,
}

/// Response returned by a plugin after handling a request.
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

/// Placeholder trait for host functions that plugins can call back into.
///
/// These will be the bridge between the WASM sandbox and the database engine.
/// Each method represents an operation the plugin can request from the host.
pub trait HostFunctions {
  // TODO: fn db_read(&self, table: &str, key: &[u8]) -> Result<Option<Vec<u8>>, PluginError>;
  // TODO: fn db_write(&self, table: &str, key: &[u8], value: &[u8]) -> Result<(), PluginError>;
  // TODO: fn db_delete(&self, table: &str, key: &[u8]) -> Result<(), PluginError>;
  // TODO: fn db_list(&self, table: &str) -> Result<Vec<Vec<u8>>, PluginError>;
  // TODO: fn log(&self, level: &str, message: &str);
}

/// Prelude module for convenient imports.
pub mod prelude {
  pub use super::{HostFunctions, PluginError, PluginRequest, PluginResponse};
  pub use super::parser::{ParserInput, FileMeta};
}
