use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::scalar_converter::{ScalarConverter, CONVERTER_TYPE_WASM};

/// A `ScalarConverter` backed by a WASM plugin.
///
/// The WASM module is expected to export:
///   `convert(ptr: i32, len: i32) -> f64`
///
/// For now this is a stub -- actual WASM execution will integrate with
/// `WasmPluginRuntime` from `src/plugins/wasm_runtime.rs` in the future.
#[derive(Debug, Clone)]
pub struct WasmConverter {
  name: String,
  order_preserving: bool,
  wasm_bytes: Vec<u8>,
}

impl WasmConverter {
  /// Create a new WASM converter from raw WASM bytes.
  pub fn new(name: String, order_preserving: bool, wasm_bytes: Vec<u8>) -> Self {
    WasmConverter {
      name,
      order_preserving,
      wasm_bytes,
    }
  }

  /// Re-create from deserialized parts (used by `deserialize_converter`).
  pub fn from_parts(name: String, order_preserving: bool, wasm_bytes: Vec<u8>) -> Self {
    Self::new(name, order_preserving, wasm_bytes)
  }

  /// Return a reference to the stored WASM bytes.
  pub fn wasm_bytes(&self) -> &[u8] {
    &self.wasm_bytes
  }

  /// Serialize to bytes: type_tag + order_preserving (1) + name_len (2) + name + wasm_bytes.
  pub fn serialize(&self) -> Vec<u8> {
    let name_bytes = self.name.as_bytes();
    let capacity = 1 + 1 + 2 + name_bytes.len() + self.wasm_bytes.len();
    let mut buffer = Vec::with_capacity(capacity);
    buffer.push(CONVERTER_TYPE_WASM);
    buffer.push(if self.order_preserving { 1 } else { 0 });
    buffer.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(name_bytes);
    buffer.extend_from_slice(&self.wasm_bytes);
    buffer
  }

  /// Deserialize from bytes produced by `serialize()`.
  pub fn deserialize(data: &[u8]) -> EngineResult<Self> {
    if data.is_empty() || data[0] != CONVERTER_TYPE_WASM {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "Not a WasmConverter (wrong type tag or empty)".to_string(),
      });
    }
    let payload = &data[1..];
    if payload.is_empty() {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "WasmConverter data too short for order_preserving flag".to_string(),
      });
    }
    let order_preserving = payload[0] != 0;
    let mut cursor = 1;
    if payload.len() < cursor + 2 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "WasmConverter data too short for name length".to_string(),
      });
    }
    let name_length = u16::from_le_bytes([payload[cursor], payload[cursor + 1]]) as usize;
    cursor += 2;
    if payload.len() < cursor + name_length {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "WasmConverter data too short for name".to_string(),
      });
    }
    let name = String::from_utf8(payload[cursor..cursor + name_length].to_vec())
      .map_err(|error| EngineError::CorruptEntry {
        offset: cursor as u64,
        reason: format!("Invalid UTF-8 name: {}", error),
      })?;
    cursor += name_length;
    let wasm_bytes = payload[cursor..].to_vec();
    Ok(WasmConverter::new(name, order_preserving, wasm_bytes))
  }
}

impl ScalarConverter for WasmConverter {
  fn to_scalar(&self, _value: &[u8]) -> f64 {
    // TODO: Load WASM module via WasmPluginRuntime, call `convert(ptr, len) -> f64`.
    // For now, return 0.5 (stub).
    0.5
  }

  fn is_order_preserving(&self) -> bool {
    self.order_preserving
  }

  fn name(&self) -> &str {
    &self.name
  }

  fn serialize(&self) -> Vec<u8> {
    WasmConverter::serialize(self)
  }

  fn type_tag(&self) -> u8 {
    CONVERTER_TYPE_WASM
  }
}

// ============================================================================
// WasmBatchConverter
// ============================================================================

/// Batch converter for WASM plugins.
///
/// The WASM module is expected to export:
///   `convert_batch(ptr: i32, count: i32, lengths_ptr: i32) -> i32`
///
/// where `ptr` points to concatenated values, `count` is the number of values,
/// `lengths_ptr` points to an array of i32 lengths, and the return value is a
/// pointer to `count` f64 results in WASM linear memory.
///
/// This is a stub -- actual batch execution deferred to WASM runtime integration.
#[derive(Debug, Clone)]
pub struct WasmBatchConverter {
  name: String,
  order_preserving: bool,
  wasm_bytes: Vec<u8>,
}

impl WasmBatchConverter {
  /// Create a new batch WASM converter.
  pub fn new(name: String, order_preserving: bool, wasm_bytes: Vec<u8>) -> Self {
    WasmBatchConverter {
      name,
      order_preserving,
      wasm_bytes,
    }
  }

  /// Return the converter name.
  pub fn name(&self) -> &str {
    &self.name
  }

  /// Whether this converter is order-preserving.
  pub fn is_order_preserving(&self) -> bool {
    self.order_preserving
  }

  /// Return a reference to the stored WASM bytes.
  pub fn wasm_bytes(&self) -> &[u8] {
    &self.wasm_bytes
  }

  /// Convert a batch of byte-slice values to f64 scalars.
  ///
  /// TODO: Load WASM module, allocate concatenated buffer + lengths array
  /// in WASM linear memory, call `convert_batch`, read back f64 results.
  pub fn convert_batch(&self, values: &[&[u8]]) -> EngineResult<Vec<f64>> {
    // Stub: return 0.5 for every value.
    Ok(values.iter().map(|_| 0.5).collect())
  }
}
