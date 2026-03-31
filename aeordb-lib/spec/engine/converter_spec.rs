use aeordb::engine::scalar_converter::{
  ScalarConverter,
  HashConverter, U8Converter, U16Converter, U32Converter, U64Converter,
  I64Converter, F64Converter, StringConverter, TimestampConverter,
  serialize_converter, deserialize_converter,
};

// ============================================================================
// HashConverter
// ============================================================================

#[test]
fn test_hash_converter_range() {
  let converter = HashConverter;

  // All zeros -> 0.0
  let zero = vec![0u8; 32];
  assert_eq!(converter.to_scalar(&zero), 0.0);

  // All 0xFF -> ~1.0
  let max = vec![0xFF; 32];
  let scalar = converter.to_scalar(&max);
  assert!(scalar >= 0.0 && scalar <= 1.0, "scalar {} out of [0,1]", scalar);
  assert!((scalar - 1.0).abs() < 1e-10, "max hash should be ~1.0, got {}", scalar);

  // Various values always in range
  for byte in [0x10, 0x40, 0x80, 0xC0, 0xF0] {
    let mut hash = vec![0u8; 32];
    hash[0] = byte;
    let scalar = converter.to_scalar(&hash);
    assert!(scalar >= 0.0 && scalar <= 1.0, "scalar {} out of [0,1] for byte 0x{:02X}", scalar, byte);
  }
}

#[test]
fn test_hash_converter_deterministic() {
  let converter = HashConverter;
  let hash = vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE,
                  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
  let first = converter.to_scalar(&hash);
  let second = converter.to_scalar(&hash);
  assert_eq!(first, second, "same input must produce same scalar");
}

#[test]
fn test_hash_converter_is_not_order_preserving() {
  let converter = HashConverter;
  assert!(!converter.is_order_preserving());
}

#[test]
fn test_hash_converter_short_input() {
  let converter = HashConverter;
  // Less than 8 bytes returns 0.0
  assert_eq!(converter.to_scalar(&[]), 0.0);
  assert_eq!(converter.to_scalar(&[0xFF; 4]), 0.0);
  assert_eq!(converter.to_scalar(&[0xFF; 7]), 0.0);
}

// ============================================================================
// U64Converter
// ============================================================================

#[test]
fn test_u64_converter_preserves_order() {
  let converter = U64Converter::new();
  assert!(converter.is_order_preserving());

  let low = 100u64.to_be_bytes();
  let mid = 500u64.to_be_bytes();
  let high = 1000u64.to_be_bytes();

  let scalar_low = converter.to_scalar(&low);
  let scalar_mid = converter.to_scalar(&mid);
  let scalar_high = converter.to_scalar(&high);

  assert!(scalar_low < scalar_mid, "low {} should be < mid {}", scalar_low, scalar_mid);
  assert!(scalar_mid < scalar_high, "mid {} should be < high {}", scalar_mid, scalar_high);
}

#[test]
fn test_u64_converter_range_full() {
  let converter = U64Converter::new();

  let zero = 0u64.to_be_bytes();
  assert_eq!(converter.to_scalar(&zero), 0.0);

  let max = u64::MAX.to_be_bytes();
  let scalar = converter.to_scalar(&max);
  assert!((scalar - 1.0).abs() < 1e-10, "u64::MAX should map to ~1.0, got {}", scalar);
}

#[test]
fn test_u64_converter_range_custom() {
  let converter = U64Converter::with_range(100, 200);

  let at_min = 100u64.to_be_bytes();
  assert_eq!(converter.to_scalar(&at_min), 0.0);

  let at_max = 200u64.to_be_bytes();
  let scalar = converter.to_scalar(&at_max);
  assert!((scalar - 1.0).abs() < 1e-10, "at max should be ~1.0, got {}", scalar);

  let at_mid = 150u64.to_be_bytes();
  let scalar = converter.to_scalar(&at_mid);
  assert!((scalar - 0.5).abs() < 1e-10, "midpoint should be ~0.5, got {}", scalar);
}

#[test]
fn test_u64_converter_min_equals_max_returns_0_5() {
  let converter = U64Converter::with_range(42, 42);
  let value = 42u64.to_be_bytes();
  assert_eq!(converter.to_scalar(&value), 0.5);

  let other = 99u64.to_be_bytes();
  assert_eq!(converter.to_scalar(&other), 0.5);
}

#[test]
fn test_u64_converter_empty_input() {
  let converter = U64Converter::new();
  assert_eq!(converter.to_scalar(&[]), 0.0);
}

#[test]
fn test_u64_converter_wrong_size_input() {
  let converter = U64Converter::new();
  // Too short
  assert_eq!(converter.to_scalar(&[0xFF; 4]), 0.0);
  assert_eq!(converter.to_scalar(&[0xFF; 7]), 0.0);
}

#[test]
fn test_u64_converter_update_range() {
  let mut converter = U64Converter::new();
  converter.update_range(10, 20);

  let value = 15u64.to_be_bytes();
  let scalar = converter.to_scalar(&value);
  assert!((scalar - 0.5).abs() < 1e-10, "midpoint should be ~0.5, got {}", scalar);
}

// ============================================================================
// U8Converter
// ============================================================================

#[test]
fn test_u8_converter_full_range() {
  let converter = U8Converter::new();
  assert_eq!(converter.to_scalar(&[0]), 0.0);

  let scalar = converter.to_scalar(&[255]);
  assert!((scalar - 1.0).abs() < 1e-10);

  let scalar = converter.to_scalar(&[128]);
  assert!(scalar > 0.4 && scalar < 0.6);
}

#[test]
fn test_u8_converter_empty_input() {
  let converter = U8Converter::new();
  assert_eq!(converter.to_scalar(&[]), 0.0);
}

// ============================================================================
// U16Converter
// ============================================================================

#[test]
fn test_u16_converter_full_range() {
  let converter = U16Converter::new();
  assert_eq!(converter.to_scalar(&0u16.to_be_bytes()), 0.0);

  let scalar = converter.to_scalar(&u16::MAX.to_be_bytes());
  assert!((scalar - 1.0).abs() < 1e-10);
}

#[test]
fn test_u16_converter_short_input() {
  let converter = U16Converter::new();
  assert_eq!(converter.to_scalar(&[0xFF]), 0.0);
}

// ============================================================================
// U32Converter
// ============================================================================

#[test]
fn test_u32_converter_full_range() {
  let converter = U32Converter::new();
  assert_eq!(converter.to_scalar(&0u32.to_be_bytes()), 0.0);

  let scalar = converter.to_scalar(&u32::MAX.to_be_bytes());
  assert!((scalar - 1.0).abs() < 1e-10);
}

#[test]
fn test_u32_converter_short_input() {
  let converter = U32Converter::new();
  assert_eq!(converter.to_scalar(&[0xFF; 3]), 0.0);
}

// ============================================================================
// I64Converter
// ============================================================================

#[test]
fn test_i64_converter_negative_values() {
  let converter = I64Converter::new();

  // i64::MIN -> 0.0
  let min_bytes = i64::MIN.to_be_bytes();
  let scalar = converter.to_scalar(&min_bytes);
  assert!(scalar.abs() < 1e-10, "i64::MIN should map to ~0.0, got {}", scalar);

  // -1 with full i64 range: the difference from midpoint is negligible
  // at floating point precision (1 / 2^64 is below f64 epsilon at 0.5).
  // So -1 maps to ~0.5 which is expected and correct.
  let neg_one = (-1i64).to_be_bytes();
  let scalar = converter.to_scalar(&neg_one);
  assert!((scalar - 0.5).abs() < 1e-10, "i64(-1) with full range should be ~0.5, got {}", scalar);
}

#[test]
fn test_i64_converter_crosses_zero() {
  let converter = I64Converter::with_range(-100, 100);

  let neg = (-50i64).to_be_bytes();
  let zero = 0i64.to_be_bytes();
  let pos = 50i64.to_be_bytes();

  let scalar_neg = converter.to_scalar(&neg);
  let scalar_zero = converter.to_scalar(&zero);
  let scalar_pos = converter.to_scalar(&pos);

  assert!(scalar_neg < scalar_zero, "negative {} should be < zero {}", scalar_neg, scalar_zero);
  assert!(scalar_zero < scalar_pos, "zero {} should be < positive {}", scalar_zero, scalar_pos);

  // Zero should be at midpoint
  assert!((scalar_zero - 0.5).abs() < 1e-10, "zero should map to 0.5, got {}", scalar_zero);
}

#[test]
fn test_i64_converter_preserves_order() {
  // Use a constrained range where f64 precision can distinguish values
  let converter = I64Converter::with_range(-1_000_000, 1_000_000);
  assert!(converter.is_order_preserving());

  let values: Vec<i64> = vec![-1000, -100, -1, 0, 1, 100, 1000];
  let scalars: Vec<f64> = values.iter()
    .map(|value| converter.to_scalar(&value.to_be_bytes()))
    .collect();

  for window in scalars.windows(2) {
    assert!(window[0] < window[1], "scalars should be monotonically increasing: {} >= {}", window[0], window[1]);
  }
}

#[test]
fn test_i64_converter_full_range_extremes_ordered() {
  // With full range, widely spaced values should still be ordered
  let converter = I64Converter::new();

  let values: Vec<i64> = vec![i64::MIN, -1_000_000_000, 0, 1_000_000_000, i64::MAX];
  let scalars: Vec<f64> = values.iter()
    .map(|value| converter.to_scalar(&value.to_be_bytes()))
    .collect();

  for window in scalars.windows(2) {
    assert!(window[0] <= window[1], "scalars should be non-decreasing: {} > {}", window[0], window[1]);
  }
}

#[test]
fn test_i64_converter_empty_input() {
  let converter = I64Converter::new();
  assert_eq!(converter.to_scalar(&[]), 0.0);
}

// ============================================================================
// F64Converter
// ============================================================================

#[test]
fn test_f64_converter_within_range() {
  let converter = F64Converter::with_range(0.0, 100.0);

  let at_min = 0.0f64.to_be_bytes();
  assert_eq!(converter.to_scalar(&at_min), 0.0);

  let at_max = 100.0f64.to_be_bytes();
  let scalar = converter.to_scalar(&at_max);
  assert!((scalar - 1.0).abs() < 1e-10, "at max should be 1.0, got {}", scalar);

  let at_mid = 50.0f64.to_be_bytes();
  let scalar = converter.to_scalar(&at_mid);
  assert!((scalar - 0.5).abs() < 1e-10, "midpoint should be 0.5, got {}", scalar);
}

#[test]
fn test_f64_converter_clamps_outside() {
  let converter = F64Converter::with_range(10.0, 20.0);

  let below = 5.0f64.to_be_bytes();
  assert_eq!(converter.to_scalar(&below), 0.0, "below min should clamp to 0.0");

  let above = 25.0f64.to_be_bytes();
  assert_eq!(converter.to_scalar(&above), 1.0, "above max should clamp to 1.0");
}

#[test]
fn test_f64_converter_nan_returns_zero() {
  let converter = F64Converter::with_range(0.0, 100.0);
  let nan = f64::NAN.to_be_bytes();
  assert_eq!(converter.to_scalar(&nan), 0.0, "NaN should return 0.0");
}

#[test]
fn test_f64_converter_infinity_handling() {
  let converter = F64Converter::with_range(0.0, 100.0);

  let pos_inf = f64::INFINITY.to_be_bytes();
  assert_eq!(converter.to_scalar(&pos_inf), 1.0, "+Infinity should clamp to 1.0");

  let neg_inf = f64::NEG_INFINITY.to_be_bytes();
  assert_eq!(converter.to_scalar(&neg_inf), 0.0, "-Infinity should clamp to 0.0");
}

#[test]
fn test_f64_converter_min_equals_max() {
  let converter = F64Converter::with_range(42.0, 42.0);
  let value = 42.0f64.to_be_bytes();
  assert_eq!(converter.to_scalar(&value), 0.5);
}

#[test]
fn test_f64_converter_empty_input() {
  let converter = F64Converter::new();
  assert_eq!(converter.to_scalar(&[]), 0.0);
}

#[test]
fn test_f64_converter_short_input() {
  let converter = F64Converter::new();
  assert_eq!(converter.to_scalar(&[0xFF; 4]), 0.0);
}

// ============================================================================
// StringConverter
// ============================================================================

#[test]
fn test_string_converter_rough_order() {
  let converter = StringConverter::new(1024);

  let a = b"apple";
  let m = b"mango";
  let z = b"zebra";

  let scalar_a = converter.to_scalar(a);
  let scalar_m = converter.to_scalar(m);
  let scalar_z = converter.to_scalar(z);

  // "a" < "m" < "z" in first byte, so scalars should roughly follow
  assert!(scalar_a < scalar_m, "apple {} should be < mango {}", scalar_a, scalar_m);
  assert!(scalar_m < scalar_z, "mango {} should be < zebra {}", scalar_m, scalar_z);
}

#[test]
fn test_string_converter_empty_string() {
  let converter = StringConverter::new(1024);
  assert_eq!(converter.to_scalar(b""), 0.0);
}

#[test]
fn test_string_converter_long_string() {
  let converter = StringConverter::new(100);

  // String longer than max_length: length component should clamp to 1.0
  let long_string = vec![b'a'; 200];
  let scalar = converter.to_scalar(&long_string);

  // first_byte = 'a' (0x61) / 255.0 * 0.7 ~= 0.266
  // length = 1.0 (clamped) * 0.3 = 0.3
  // total ~= 0.566
  assert!(scalar > 0.5 && scalar < 0.6, "long string scalar {} should be ~0.566", scalar);
  assert!(scalar >= 0.0 && scalar <= 1.0, "scalar must be in [0,1]");
}

#[test]
fn test_string_converter_zero_max_length_uses_default() {
  // Passing 0 for max_length should not panic, should default to 1024
  let converter = StringConverter::new(0);
  let scalar = converter.to_scalar(b"hello");
  assert!(scalar >= 0.0 && scalar <= 1.0);
}

#[test]
fn test_string_converter_not_order_preserving() {
  let converter = StringConverter::new(1024);
  assert!(!converter.is_order_preserving());
}

// ============================================================================
// TimestampConverter
// ============================================================================

#[test]
fn test_timestamp_converter() {
  let converter = TimestampConverter::with_range(1000, 2000);
  assert!(converter.is_order_preserving());

  let at_min = 1000i64.to_be_bytes();
  assert_eq!(converter.to_scalar(&at_min), 0.0);

  let at_max = 2000i64.to_be_bytes();
  let scalar = converter.to_scalar(&at_max);
  assert!((scalar - 1.0).abs() < 1e-10);

  let at_mid = 1500i64.to_be_bytes();
  let scalar = converter.to_scalar(&at_mid);
  assert!((scalar - 0.5).abs() < 1e-10, "midpoint should be 0.5, got {}", scalar);
}

#[test]
fn test_timestamp_converter_empty_input() {
  let converter = TimestampConverter::new();
  assert_eq!(converter.to_scalar(&[]), 0.0);
}

#[test]
fn test_timestamp_converter_default_range() {
  let converter = TimestampConverter::new();
  // Unix epoch should map to 0.0
  let epoch = 0i64.to_be_bytes();
  assert_eq!(converter.to_scalar(&epoch), 0.0);

  // A timestamp in the middle of the default range should be ~0.5
  let mid = converter.to_scalar(&(2_051_222_400_000i64).to_be_bytes());
  assert!(mid > 0.4 && mid < 0.6, "mid-range timestamp should be ~0.5, got {}", mid);
}

// ============================================================================
// Serialization roundtrip — all converter types
// ============================================================================

#[test]
fn test_converter_serialization_roundtrip() {
  let converters: Vec<Box<dyn ScalarConverter>> = vec![
    Box::new(HashConverter),
    Box::new(U8Converter::with_range(10, 200)),
    Box::new(U16Converter::with_range(100, 60000)),
    Box::new(U32Converter::with_range(1000, 4000000)),
    Box::new(U64Converter::with_range(500, 999999)),
    Box::new(I64Converter::with_range(-1000, 1000)),
    Box::new(F64Converter::with_range(-3.14, 3.14)),
    Box::new(StringConverter::new(512)),
    Box::new(TimestampConverter::with_range(1000000, 2000000)),
  ];

  let test_values: Vec<Vec<u8>> = vec![
    vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
         0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    vec![100],
    30000u16.to_be_bytes().to_vec(),
    2000000u32.to_be_bytes().to_vec(),
    750000u64.to_be_bytes().to_vec(),
    500i64.to_be_bytes().to_vec(),
    1.5f64.to_be_bytes().to_vec(),
    b"hello world".to_vec(),
    1500000i64.to_be_bytes().to_vec(),
  ];

  for (index, converter) in converters.iter().enumerate() {
    let serialized = serialize_converter(converter.as_ref());
    let deserialized = deserialize_converter(&serialized)
      .unwrap_or_else(|error| panic!("roundtrip failed for {}: {:?}", converter.name(), error));

    assert_eq!(
      deserialized.name(), converter.name(),
      "name mismatch after roundtrip for converter index {}", index
    );
    assert_eq!(
      deserialized.is_order_preserving(), converter.is_order_preserving(),
      "order_preserving mismatch for {}", converter.name()
    );

    let original_scalar = converter.to_scalar(&test_values[index]);
    let roundtrip_scalar = deserialized.to_scalar(&test_values[index]);
    assert!(
      (original_scalar - roundtrip_scalar).abs() < 1e-15,
      "scalar mismatch for {}: original={}, roundtrip={}",
      converter.name(), original_scalar, roundtrip_scalar
    );
  }
}

#[test]
fn test_deserialize_empty_data() {
  let result = deserialize_converter(&[]);
  assert!(result.is_err(), "empty data should fail deserialization");
}

#[test]
fn test_deserialize_unknown_type_tag() {
  let result = deserialize_converter(&[0xFF]);
  assert!(result.is_err(), "unknown type tag should fail");
}

#[test]
fn test_deserialize_truncated_payload() {
  // U64 converter needs 16 bytes of payload after the type tag
  let result = deserialize_converter(&[0x05, 0x01, 0x02]);
  assert!(result.is_err(), "truncated U64Converter payload should fail");
}
