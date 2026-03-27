/// Trait for mapping arbitrary byte-encoded values to a scalar in [0.0, 1.0].
///
/// Every indexable value is normalized to a position within the known domain
/// of that type. This is the foundation of scalar ratio indexing.
pub trait ScalarMapping: Send + Sync {
  /// Map a raw byte-encoded value to a scalar in [0.0, 1.0].
  /// For signed types, negative values may map to [-1.0, 0.0).
  fn map_to_scalar(&self, value: &[u8]) -> f64;

  /// Human-readable name for the value type this mapping handles.
  fn value_type_name(&self) -> &str;
}

/// Maps u8 values (0-255) to [0.0, 1.0].
pub struct U8Mapping;

impl ScalarMapping for U8Mapping {
  fn map_to_scalar(&self, value: &[u8]) -> f64 {
    if value.is_empty() {
      return 0.0;
    }
    value[0] as f64 / 255.0
  }

  fn value_type_name(&self) -> &str {
    "u8"
  }
}

/// Maps u16 values to [0.0, 1.0]. Reads big-endian.
pub struct U16Mapping;

impl ScalarMapping for U16Mapping {
  fn map_to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 2 {
      return 0.0;
    }
    let raw = u16::from_be_bytes([value[0], value[1]]);
    raw as f64 / u16::MAX as f64
  }

  fn value_type_name(&self) -> &str {
    "u16"
  }
}

/// Maps u32 values to [0.0, 1.0]. Reads big-endian.
pub struct U32Mapping;

impl ScalarMapping for U32Mapping {
  fn map_to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 4 {
      return 0.0;
    }
    let raw = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
    raw as f64 / u32::MAX as f64
  }

  fn value_type_name(&self) -> &str {
    "u32"
  }
}

/// Maps u64 values to [0.0, 1.0]. Reads big-endian.
pub struct U64Mapping;

impl ScalarMapping for U64Mapping {
  fn map_to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 8 {
      return 0.0;
    }
    let raw = u64::from_be_bytes([
      value[0], value[1], value[2], value[3],
      value[4], value[5], value[6], value[7],
    ]);
    raw as f64 / u64::MAX as f64
  }

  fn value_type_name(&self) -> &str {
    "u64"
  }
}

/// Maps i64 values to scalars. Negative values map to [-1.0, 0.0),
/// zero maps to 0.5 (midpoint of normalized range), positive values
/// map to (0.5, 1.0]. This gives the negative branch its own index space.
///
/// Reads big-endian.
pub struct I64Mapping;

impl ScalarMapping for I64Mapping {
  fn map_to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 8 {
      return 0.0;
    }
    let raw = i64::from_be_bytes([
      value[0], value[1], value[2], value[3],
      value[4], value[5], value[6], value[7],
    ]);
    if raw < 0 {
      // Map [i64::MIN, -1] to [-1.0, 0.0)
      -(raw as f64 / i64::MIN as f64)
    } else if raw == 0 {
      0.5
    } else {
      // Map [1, i64::MAX] to (0.5, 1.0]
      0.5 + (raw as f64 / i64::MAX as f64) * 0.5
    }
  }

  fn value_type_name(&self) -> &str {
    "i64"
  }
}

/// Maps f64 values to [0.0, 1.0] via normalization against a configurable
/// min/max range. Values outside the range are clamped.
pub struct F64Mapping {
  pub minimum: f64,
  pub maximum: f64,
}

impl F64Mapping {
  pub fn new(minimum: f64, maximum: f64) -> Self {
    assert!(maximum > minimum, "maximum must be greater than minimum");
    Self { minimum, maximum }
  }
}

impl ScalarMapping for F64Mapping {
  fn map_to_scalar(&self, value: &[u8]) -> f64 {
    if value.len() < 8 {
      return 0.0;
    }
    let raw = f64::from_be_bytes([
      value[0], value[1], value[2], value[3],
      value[4], value[5], value[6], value[7],
    ]);
    let normalized = (raw - self.minimum) / (self.maximum - self.minimum);
    normalized.clamp(0.0, 1.0)
  }

  fn value_type_name(&self) -> &str {
    "f64"
  }
}

/// Maps strings to [0.0, 1.0] via multi-stage decomposition.
///
/// Stage 1: First byte normalized to [0.0, 1.0] (weighted at 70%)
/// Stage 2: Length normalized against a max expected length (weighted at 30%)
///
/// This is a simple initial implementation. The full multi-dimensional
/// vector approach described in the design doc comes later.
pub struct StringMapping {
  pub max_expected_length: usize,
}

impl StringMapping {
  pub fn new(max_expected_length: usize) -> Self {
    assert!(max_expected_length > 0, "max_expected_length must be positive");
    Self { max_expected_length }
  }
}

impl ScalarMapping for StringMapping {
  fn map_to_scalar(&self, value: &[u8]) -> f64 {
    if value.is_empty() {
      return 0.0;
    }

    // Stage 1: first byte normalized (weight: 0.7)
    let first_byte_scalar = value[0] as f64 / 255.0;

    // Stage 2: length normalized (weight: 0.3)
    let length_scalar = (value.len() as f64 / self.max_expected_length as f64).min(1.0);

    let combined = first_byte_scalar * 0.7 + length_scalar * 0.3;
    combined.clamp(0.0, 1.0)
  }

  fn value_type_name(&self) -> &str {
    "string"
  }
}
