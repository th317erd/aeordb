use crate::engine::errors::{EngineError, EngineResult};

// --- Converter type tags for serialization ---

pub const CONVERTER_TYPE_HASH: u8 = 0x01;
pub const CONVERTER_TYPE_U8: u8 = 0x02;
pub const CONVERTER_TYPE_U16: u8 = 0x03;
pub const CONVERTER_TYPE_U32: u8 = 0x04;
pub const CONVERTER_TYPE_U64: u8 = 0x05;
pub const CONVERTER_TYPE_I64: u8 = 0x06;
pub const CONVERTER_TYPE_F64: u8 = 0x07;
pub const CONVERTER_TYPE_STRING: u8 = 0x08;
pub const CONVERTER_TYPE_TIMESTAMP: u8 = 0x09;
pub const CONVERTER_TYPE_WASM: u8 = 0x0A;

/// Converts any value to a normalized scalar in [0.0, 1.0].
pub trait ScalarConverter: Send + Sync {
  /// Convert raw bytes to a scalar in [0.0, 1.0].
  fn to_scalar(&self, value: &[u8]) -> f64;

  /// Is this converter order-preserving?
  /// Required for range queries (gt, lt, between).
  fn is_order_preserving(&self) -> bool;

  /// Human-readable name.
  fn name(&self) -> &str;

  /// Serialize this converter's state (type tag + config).
  fn serialize(&self) -> Vec<u8>;

  /// Type tag identifying this converter variant.
  fn type_tag(&self) -> u8;
}

/// Serialize any converter to bytes (type tag + state).
pub fn serialize_converter(converter: &dyn ScalarConverter) -> Vec<u8> {
  converter.serialize()
}

/// Deserialize a converter from bytes.
pub fn deserialize_converter(data: &[u8]) -> EngineResult<Box<dyn ScalarConverter>> {
  if data.is_empty() {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: "Converter data is empty".to_string(),
    });
  }

  let type_tag = data[0];
  let payload = &data[1..];

  match type_tag {
    CONVERTER_TYPE_HASH => {
      Ok(Box::new(HashConverter))
    }
    CONVERTER_TYPE_U8 => {
      if payload.len() < 2 {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: "U8Converter data too short".to_string(),
        });
      }
      Ok(Box::new(U8Converter::with_range(payload[0], payload[1])))
    }
    CONVERTER_TYPE_U16 => {
      if payload.len() < 4 {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: "U16Converter data too short".to_string(),
        });
      }
      let min = u16::from_le_bytes([payload[0], payload[1]]);
      let max = u16::from_le_bytes([payload[2], payload[3]]);
      Ok(Box::new(U16Converter::with_range(min, max)))
    }
    CONVERTER_TYPE_U32 => {
      if payload.len() < 8 {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: "U32Converter data too short".to_string(),
        });
      }
      let min = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
      let max = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
      Ok(Box::new(U32Converter::with_range(min, max)))
    }
    CONVERTER_TYPE_U64 => {
      if payload.len() < 16 {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: "U64Converter data too short".to_string(),
        });
      }
      let min = u64::from_le_bytes(payload[0..8].try_into().unwrap());
      let max = u64::from_le_bytes(payload[8..16].try_into().unwrap());
      Ok(Box::new(U64Converter::with_range(min, max)))
    }
    CONVERTER_TYPE_I64 => {
      if payload.len() < 16 {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: "I64Converter data too short".to_string(),
        });
      }
      let min = i64::from_le_bytes(payload[0..8].try_into().unwrap());
      let max = i64::from_le_bytes(payload[8..16].try_into().unwrap());
      Ok(Box::new(I64Converter::with_range(min, max)))
    }
    CONVERTER_TYPE_F64 => {
      if payload.len() < 16 {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: "F64Converter data too short".to_string(),
        });
      }
      let min = f64::from_le_bytes(payload[0..8].try_into().unwrap());
      let max = f64::from_le_bytes(payload[8..16].try_into().unwrap());
      Ok(Box::new(F64Converter::with_range(min, max)))
    }
    CONVERTER_TYPE_STRING => {
      if payload.len() < 4 {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: "StringConverter data too short".to_string(),
        });
      }
      let max_length = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
      Ok(Box::new(StringConverter::new(max_length)))
    }
    CONVERTER_TYPE_TIMESTAMP => {
      if payload.len() < 16 {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: "TimestampConverter data too short".to_string(),
        });
      }
      let min = i64::from_le_bytes(payload[0..8].try_into().unwrap());
      let max = i64::from_le_bytes(payload[8..16].try_into().unwrap());
      Ok(Box::new(TimestampConverter::with_range(min, max)))
    }
    CONVERTER_TYPE_WASM => {
      // Deserialize: 1 byte order_preserving flag + 2 bytes name length + name + wasm_bytes
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
          reason: format!("Invalid UTF-8 name in WasmConverter: {}", error),
        })?;
      cursor += name_length;
      let wasm_bytes = payload[cursor..].to_vec();
      Ok(Box::new(super::wasm_converter::WasmConverter::from_parts(
        name,
        order_preserving,
        wasm_bytes,
      )))
    }
    unknown => {
      Err(EngineError::CorruptEntry {
        offset: 0,
        reason: format!("Unknown converter type tag: 0x{:02X}", unknown),
      })
    }
  }
}

// ============================================================================
// HashConverter
// ============================================================================

/// Converts hash bytes to a scalar via first 8 bytes as big-endian u64 / u64::MAX.
/// Used for KVS hash lookups. Not order-preserving.
#[derive(Debug, Clone)]
pub struct HashConverter;

impl ScalarConverter for HashConverter {
  fn to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 8 {
      return 0.0;
    }
    let bytes: [u8; 8] = value[..8].try_into().unwrap();
    let numeric = u64::from_be_bytes(bytes);
    numeric as f64 / u64::MAX as f64
  }

  fn is_order_preserving(&self) -> bool {
    false
  }

  fn name(&self) -> &str {
    "hash"
  }

  fn serialize(&self) -> Vec<u8> {
    vec![CONVERTER_TYPE_HASH]
  }

  fn type_tag(&self) -> u8 {
    CONVERTER_TYPE_HASH
  }
}

// ============================================================================
// U8Converter
// ============================================================================

/// Converts u8 values (1 byte big-endian) to [0.0, 1.0] with range tracking.
#[derive(Debug, Clone)]
pub struct U8Converter {
  min: u8,
  max: u8,
}

impl Default for U8Converter {
  fn default() -> Self {
    Self::new()
  }
}

impl U8Converter {
  pub fn new() -> Self {
    Self { min: 0, max: u8::MAX }
  }

  pub fn with_range(min: u8, max: u8) -> Self {
    Self { min, max }
  }

  pub fn update_range(&mut self, min: u8, max: u8) {
    self.min = min;
    self.max = max;
  }
}

impl ScalarConverter for U8Converter {
  fn to_scalar(&self, value: &[u8]) -> f64 {
    if value.is_empty() {
      return 0.0;
    }
    if self.min == self.max {
      return 0.5;
    }
    let numeric = value[0];
    (numeric.saturating_sub(self.min)) as f64 / (self.max - self.min) as f64
  }

  fn is_order_preserving(&self) -> bool {
    true
  }

  fn name(&self) -> &str {
    "u8"
  }

  fn serialize(&self) -> Vec<u8> {
    vec![CONVERTER_TYPE_U8, self.min, self.max]
  }

  fn type_tag(&self) -> u8 {
    CONVERTER_TYPE_U8
  }
}

// ============================================================================
// U16Converter
// ============================================================================

/// Converts u16 values (2 bytes big-endian) to [0.0, 1.0] with range tracking.
#[derive(Debug, Clone)]
pub struct U16Converter {
  min: u16,
  max: u16,
}

impl Default for U16Converter {
  fn default() -> Self {
    Self::new()
  }
}

impl U16Converter {
  pub fn new() -> Self {
    Self { min: 0, max: u16::MAX }
  }

  pub fn with_range(min: u16, max: u16) -> Self {
    Self { min, max }
  }

  pub fn update_range(&mut self, min: u16, max: u16) {
    self.min = min;
    self.max = max;
  }
}

impl ScalarConverter for U16Converter {
  fn to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 2 {
      return 0.0;
    }
    if self.min == self.max {
      return 0.5;
    }
    let numeric = u16::from_be_bytes([value[0], value[1]]);
    (numeric.saturating_sub(self.min)) as f64 / (self.max - self.min) as f64
  }

  fn is_order_preserving(&self) -> bool {
    true
  }

  fn name(&self) -> &str {
    "u16"
  }

  fn serialize(&self) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(5);
    buffer.push(CONVERTER_TYPE_U16);
    buffer.extend_from_slice(&self.min.to_le_bytes());
    buffer.extend_from_slice(&self.max.to_le_bytes());
    buffer
  }

  fn type_tag(&self) -> u8 {
    CONVERTER_TYPE_U16
  }
}

// ============================================================================
// U32Converter
// ============================================================================

/// Converts u32 values (4 bytes big-endian) to [0.0, 1.0] with range tracking.
#[derive(Debug, Clone)]
pub struct U32Converter {
  min: u32,
  max: u32,
}

impl Default for U32Converter {
  fn default() -> Self {
    Self::new()
  }
}

impl U32Converter {
  pub fn new() -> Self {
    Self { min: 0, max: u32::MAX }
  }

  pub fn with_range(min: u32, max: u32) -> Self {
    Self { min, max }
  }

  pub fn update_range(&mut self, min: u32, max: u32) {
    self.min = min;
    self.max = max;
  }
}

impl ScalarConverter for U32Converter {
  fn to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 4 {
      return 0.0;
    }
    if self.min == self.max {
      return 0.5;
    }
    let numeric = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
    (numeric.saturating_sub(self.min)) as f64 / (self.max - self.min) as f64
  }

  fn is_order_preserving(&self) -> bool {
    true
  }

  fn name(&self) -> &str {
    "u32"
  }

  fn serialize(&self) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(9);
    buffer.push(CONVERTER_TYPE_U32);
    buffer.extend_from_slice(&self.min.to_le_bytes());
    buffer.extend_from_slice(&self.max.to_le_bytes());
    buffer
  }

  fn type_tag(&self) -> u8 {
    CONVERTER_TYPE_U32
  }
}

// ============================================================================
// U64Converter
// ============================================================================

/// Converts u64 values (8 bytes big-endian) to [0.0, 1.0] with range tracking.
#[derive(Debug, Clone)]
pub struct U64Converter {
  min: u64,
  max: u64,
}

impl Default for U64Converter {
  fn default() -> Self {
    Self::new()
  }
}

impl U64Converter {
  pub fn new() -> Self {
    Self { min: 0, max: u64::MAX }
  }

  pub fn with_range(min: u64, max: u64) -> Self {
    Self { min, max }
  }

  pub fn update_range(&mut self, min: u64, max: u64) {
    self.min = min;
    self.max = max;
  }
}

impl ScalarConverter for U64Converter {
  fn to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 8 {
      return 0.0;
    }
    if self.min == self.max {
      return 0.5;
    }
    let numeric = u64::from_be_bytes(value[..8].try_into().unwrap());
    (numeric.saturating_sub(self.min)) as f64 / (self.max - self.min) as f64
  }

  fn is_order_preserving(&self) -> bool {
    true
  }

  fn name(&self) -> &str {
    "u64"
  }

  fn serialize(&self) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(17);
    buffer.push(CONVERTER_TYPE_U64);
    buffer.extend_from_slice(&self.min.to_le_bytes());
    buffer.extend_from_slice(&self.max.to_le_bytes());
    buffer
  }

  fn type_tag(&self) -> u8 {
    CONVERTER_TYPE_U64
  }
}

// ============================================================================
// I64Converter
// ============================================================================

/// Converts i64 values (8 bytes big-endian) to [0.0, 1.0] with range tracking.
/// Shifts signed range to unsigned before normalizing.
#[derive(Debug, Clone)]
pub struct I64Converter {
  min: i64,
  max: i64,
}

impl Default for I64Converter {
  fn default() -> Self {
    Self::new()
  }
}

impl I64Converter {
  pub fn new() -> Self {
    Self { min: i64::MIN, max: i64::MAX }
  }

  pub fn with_range(min: i64, max: i64) -> Self {
    Self { min, max }
  }

  pub fn update_range(&mut self, min: i64, max: i64) {
    self.min = min;
    self.max = max;
  }
}

impl ScalarConverter for I64Converter {
  fn to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 8 {
      return 0.0;
    }
    if self.min == self.max {
      return 0.5;
    }
    let numeric = i64::from_be_bytes(value[..8].try_into().unwrap());
    // Shift to unsigned space to avoid sign issues:
    // (value - min) / (max - min), computed in u64 space
    let shifted_value = (numeric as i128 - self.min as i128) as f64;
    let shifted_range = (self.max as i128 - self.min as i128) as f64;
    (shifted_value / shifted_range).clamp(0.0, 1.0)
  }

  fn is_order_preserving(&self) -> bool {
    true
  }

  fn name(&self) -> &str {
    "i64"
  }

  fn serialize(&self) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(17);
    buffer.push(CONVERTER_TYPE_I64);
    buffer.extend_from_slice(&self.min.to_le_bytes());
    buffer.extend_from_slice(&self.max.to_le_bytes());
    buffer
  }

  fn type_tag(&self) -> u8 {
    CONVERTER_TYPE_I64
  }
}

// ============================================================================
// F64Converter
// ============================================================================

/// Converts f64 values (8 bytes big-endian) to [0.0, 1.0] with range tracking.
/// NaN returns 0.0. Infinity is clamped.
#[derive(Debug, Clone)]
pub struct F64Converter {
  min: f64,
  max: f64,
}

impl Default for F64Converter {
  fn default() -> Self {
    Self::new()
  }
}

impl F64Converter {
  pub fn new() -> Self {
    Self { min: 0.0, max: 1.0 }
  }

  pub fn with_range(min: f64, max: f64) -> Self {
    Self { min, max }
  }

  pub fn update_range(&mut self, min: f64, max: f64) {
    self.min = min;
    self.max = max;
  }
}

impl ScalarConverter for F64Converter {
  fn to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 8 {
      return 0.0;
    }
    let numeric = f64::from_be_bytes(value[..8].try_into().unwrap());
    if numeric.is_nan() {
      return 0.0;
    }
    if self.min == self.max {
      return 0.5;
    }
    let normalized = (numeric - self.min) / (self.max - self.min);
    normalized.clamp(0.0, 1.0)
  }

  fn is_order_preserving(&self) -> bool {
    true
  }

  fn name(&self) -> &str {
    "f64"
  }

  fn serialize(&self) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(17);
    buffer.push(CONVERTER_TYPE_F64);
    buffer.extend_from_slice(&self.min.to_le_bytes());
    buffer.extend_from_slice(&self.max.to_le_bytes());
    buffer
  }

  fn type_tag(&self) -> u8 {
    CONVERTER_TYPE_F64
  }
}

// ============================================================================
// StringConverter
// ============================================================================

/// Converts strings to [0.0, 1.0] via multi-stage decomposition.
/// Stage 1: First byte normalized (weight 0.7).
/// Stage 2: Length normalized against max_length (weight 0.3).
/// Roughly order-preserving (lexicographic approximation, not exact).
#[derive(Debug, Clone)]
pub struct StringConverter {
  max_length: usize,
}

impl StringConverter {
  pub fn new(max_length: usize) -> Self {
    let max_length = if max_length == 0 { 1024 } else { max_length };
    Self { max_length }
  }
}

impl ScalarConverter for StringConverter {
  fn to_scalar(&self, value: &[u8]) -> f64 {
    if value.is_empty() {
      return 0.0;
    }
    let first_byte_scalar = value[0] as f64 / 255.0;
    let length_scalar = (value.len() as f64 / self.max_length as f64).min(1.0);
    let combined = first_byte_scalar * 0.7 + length_scalar * 0.3;
    combined.clamp(0.0, 1.0)
  }

  fn is_order_preserving(&self) -> bool {
    false // only roughly lexicographic, not exact
  }

  fn name(&self) -> &str {
    "string"
  }

  fn serialize(&self) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(5);
    buffer.push(CONVERTER_TYPE_STRING);
    buffer.extend_from_slice(&(self.max_length as u32).to_le_bytes());
    buffer
  }

  fn type_tag(&self) -> u8 {
    CONVERTER_TYPE_STRING
  }
}

// ============================================================================
// TimestampConverter
// ============================================================================

/// Converts UTC millisecond timestamps (i64, 8 bytes big-endian) to [0.0, 1.0].
/// Semantically distinct from I64Converter, same underlying math.
#[derive(Debug, Clone)]
pub struct TimestampConverter {
  min: i64,
  max: i64,
}

impl Default for TimestampConverter {
  fn default() -> Self {
    Self::new()
  }
}

impl TimestampConverter {
  pub fn new() -> Self {
    // Default range: Unix epoch (0) to ~year 2100 (4102444800000ms)
    Self {
      min: 0,
      max: 4_102_444_800_000,
    }
  }

  pub fn with_range(min: i64, max: i64) -> Self {
    Self { min, max }
  }

  pub fn update_range(&mut self, min: i64, max: i64) {
    self.min = min;
    self.max = max;
  }
}

impl ScalarConverter for TimestampConverter {
  fn to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 8 {
      return 0.0;
    }
    if self.min == self.max {
      return 0.5;
    }
    let numeric = i64::from_be_bytes(value[..8].try_into().unwrap());
    let shifted_value = (numeric as i128 - self.min as i128) as f64;
    let shifted_range = (self.max as i128 - self.min as i128) as f64;
    (shifted_value / shifted_range).clamp(0.0, 1.0)
  }

  fn is_order_preserving(&self) -> bool {
    true
  }

  fn name(&self) -> &str {
    "timestamp"
  }

  fn serialize(&self) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(17);
    buffer.push(CONVERTER_TYPE_TIMESTAMP);
    buffer.extend_from_slice(&self.min.to_le_bytes());
    buffer.extend_from_slice(&self.max.to_le_bytes());
    buffer
  }

  fn type_tag(&self) -> u8 {
    CONVERTER_TYPE_TIMESTAMP
  }
}
