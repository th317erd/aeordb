use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The kind of plugin deployed into the system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
  Wasm,
  Native,
  Rule,
}

impl std::fmt::Display for PluginType {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      PluginType::Wasm => write!(formatter, "wasm"),
      PluginType::Native => write!(formatter, "native"),
      PluginType::Rule => write!(formatter, "rule"),
    }
  }
}

impl std::str::FromStr for PluginType {
  type Err = String;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    match value {
      "wasm" => Ok(PluginType::Wasm),
      "native" => Ok(PluginType::Native),
      "rule" => Ok(PluginType::Rule),
      other => Err(format!("unknown plugin type: {}", other)),
    }
  }
}

/// Lightweight metadata about a deployed plugin (excludes the WASM bytes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMetadata {
  pub plugin_id: Uuid,
  pub name: String,
  pub path: String,
  pub plugin_type: PluginType,
  pub created_at: DateTime<Utc>,
}

/// Serialize a value to JSON bytes for FFI transfer.
pub fn serialize_for_ffi<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
  serde_json::to_vec(value)
}

/// Deserialize a value from JSON bytes received via FFI.
pub fn deserialize_from_ffi<T: for<'de> Deserialize<'de>>(
  bytes: &[u8],
) -> Result<T, serde_json::Error> {
  serde_json::from_slice(bytes)
}
