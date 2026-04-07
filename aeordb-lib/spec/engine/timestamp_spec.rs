use aeordb::engine::scalar_converter::{ScalarConverter, TimestampConverter};

// ============================================================================
// Timestamp parsing tests
// ============================================================================

#[test]
fn test_timestamp_i64_bytes() {
  let converter = TimestampConverter::new();
  let millis: i64 = 1_775_577_960_000; // some timestamp in millis
  let bytes = millis.to_be_bytes();
  assert_eq!(converter.parse_timestamp(&bytes), millis);
}

#[test]
fn test_timestamp_iso8601_utc() {
  let converter = TimestampConverter::new();
  let input = b"2026-04-07T15:30:00Z";
  let result = converter.parse_timestamp(input);
  // 2026-04-07T15:30:00Z in millis
  let expected = chrono::DateTime::parse_from_rfc3339("2026-04-07T15:30:00Z")
    .unwrap()
    .timestamp_millis();
  assert_eq!(result, expected);
}

#[test]
fn test_timestamp_iso8601_offset() {
  let converter = TimestampConverter::new();
  let utc_input = b"2026-04-07T15:30:00Z";
  let offset_input = b"2026-04-07T10:30:00-05:00";
  let utc_result = converter.parse_timestamp(utc_input);
  let offset_result = converter.parse_timestamp(offset_input);
  // Both represent the same instant
  assert_eq!(utc_result, offset_result);
}

#[test]
fn test_timestamp_iso8601_no_tz() {
  let converter = TimestampConverter::new();
  let input = b"2026-04-07T15:30:00";
  let result = converter.parse_timestamp(input);
  // Should be treated as UTC
  let expected = chrono::NaiveDateTime::parse_from_str("2026-04-07T15:30:00", "%Y-%m-%dT%H:%M:%S")
    .unwrap()
    .and_utc()
    .timestamp_millis();
  assert_eq!(result, expected);
}

#[test]
fn test_timestamp_iso8601_fractional() {
  let converter = TimestampConverter::new();
  let input = b"2026-04-07T15:30:00.123Z";
  let result = converter.parse_timestamp(input);
  let expected = chrono::DateTime::parse_from_rfc3339("2026-04-07T15:30:00.123Z")
    .unwrap()
    .timestamp_millis();
  assert_eq!(result, expected);
  // The millis portion should be 123
  assert_eq!(result % 1000, 123);
}

#[test]
fn test_timestamp_date_only() {
  let converter = TimestampConverter::new();
  let input = b"2026-04-07";
  let result = converter.parse_timestamp(input);
  let expected = chrono::NaiveDate::parse_from_str("2026-04-07", "%Y-%m-%d")
    .unwrap()
    .and_hms_opt(0, 0, 0)
    .unwrap()
    .and_utc()
    .timestamp_millis();
  assert_eq!(result, expected);
}

#[test]
fn test_timestamp_numeric_string() {
  let converter = TimestampConverter::new();
  let input = b"1775577960000";
  let result = converter.parse_timestamp(input);
  assert_eq!(result, 1_775_577_960_000);
}

#[test]
fn test_timestamp_offset_normalizes_to_utc() {
  let converter = TimestampConverter::with_range(0, 4_102_444_800_000);
  let input_a = b"2026-04-07T15:30:00Z" as &[u8];
  let input_b = b"2026-04-07T10:30:00-05:00" as &[u8];
  let scalar_a = converter.to_scalar(input_a);
  let scalar_b = converter.to_scalar(input_b);
  assert!(
    (scalar_a - scalar_b).abs() < f64::EPSILON,
    "Same instant with different offsets should produce identical scalars: {} vs {}",
    scalar_a,
    scalar_b
  );
}

#[test]
fn test_timestamp_empty_returns_zero() {
  let converter = TimestampConverter::new();
  let result = converter.parse_timestamp(b"");
  assert_eq!(result, 0);
}

#[test]
fn test_timestamp_garbage_returns_zero() {
  let converter = TimestampConverter::new();
  let result = converter.parse_timestamp(b"not a date");
  assert_eq!(result, 0);
}

#[test]
fn test_timestamp_scalar_ordering() {
  let converter = TimestampConverter::with_range(0, 4_102_444_800_000);
  let earlier = b"2020-01-01T00:00:00Z" as &[u8];
  let later = b"2026-04-07T15:30:00Z" as &[u8];
  let scalar_early = converter.to_scalar(earlier);
  let scalar_late = converter.to_scalar(later);
  assert!(
    scalar_early < scalar_late,
    "Earlier date should produce lower scalar: {} vs {}",
    scalar_early,
    scalar_late
  );
}

#[test]
fn test_timestamp_converter_order_preserving() {
  let converter = TimestampConverter::new();
  assert!(converter.is_order_preserving());
}

// ============================================================================
// Internal timestamp standardization tests
// ============================================================================

#[test]
fn test_file_header_timestamps_millis() {
  use aeordb::engine::file_header::FileHeader;
  use aeordb::engine::hash_algorithm::HashAlgorithm;

  let header = FileHeader::new(HashAlgorithm::Blake3_256);

  // Millis timestamps should be in a reasonable range.
  // As of 2024, millis since epoch are ~1.7 trillion.
  // Seconds since epoch are ~1.7 billion.
  // If created_at > 1_000_000_000_000, it's definitely millis (not seconds).
  assert!(
    header.created_at > 1_000_000_000_000,
    "created_at should be in milliseconds, got: {}",
    header.created_at
  );
  assert!(
    header.updated_at > 1_000_000_000_000,
    "updated_at should be in milliseconds, got: {}",
    header.updated_at
  );

  // Should be close to now (within 5 seconds)
  let now_millis = chrono::Utc::now().timestamp_millis();
  assert!(
    (header.created_at - now_millis).abs() < 5_000,
    "created_at should be close to now: {} vs {}",
    header.created_at,
    now_millis
  );
}

#[test]
fn test_file_record_timestamps_millis() {
  use aeordb::engine::append_writer::AppendWriter;
  use aeordb::engine::entry_type::EntryType;

  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("test.aeor");
  let mut writer = AppendWriter::create(&path).unwrap();

  // Write an entry and check the timestamp on the file header
  writer.append_entry(EntryType::FileRecord, b"key", b"value", 0).unwrap();

  let header = writer.file_header();
  assert!(
    header.updated_at > 1_000_000_000_000,
    "updated_at after write should be in milliseconds, got: {}",
    header.updated_at
  );

  // Read the entry back and check its timestamp
  let (_entry_header, _key, _value) = writer.read_entry_at(256).unwrap();
  assert!(
    _entry_header.timestamp > 1_000_000_000_000,
    "entry timestamp should be in milliseconds, got: {}",
    _entry_header.timestamp
  );
}

#[test]
fn test_event_timestamp_millis() {
  use aeordb::engine::append_writer::AppendWriter;
  use aeordb::engine::entry_type::EntryType;

  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("test.aeor");
  let mut writer = AppendWriter::create(&path).unwrap();

  let offset = writer.append_entry(EntryType::Chunk, b"chunk_key", b"chunk_data", 0).unwrap();
  let (entry_header, _key, _value) = writer.read_entry_at(offset).unwrap();

  let now_millis = chrono::Utc::now().timestamp_millis();
  assert!(
    entry_header.timestamp > 1_000_000_000_000,
    "event timestamp should be in milliseconds, got: {}",
    entry_header.timestamp
  );
  assert!(
    (entry_header.timestamp - now_millis).abs() < 5_000,
    "event timestamp should be close to now: {} vs {}",
    entry_header.timestamp,
    now_millis
  );
}

// ============================================================================
// Edge cases and additional coverage
// ============================================================================

#[test]
fn test_timestamp_min_equals_max() {
  let converter = TimestampConverter::with_range(1000, 1000);
  let bytes = 1000_i64.to_be_bytes();
  let scalar = converter.to_scalar(&bytes);
  assert!((scalar - 0.5).abs() < f64::EPSILON, "min == max should return 0.5");
}

#[test]
fn test_timestamp_whitespace_trimmed() {
  let converter = TimestampConverter::new();
  let input = b"  2026-04-07T15:30:00Z  ";
  let result = converter.parse_timestamp(input);
  let expected = chrono::DateTime::parse_from_rfc3339("2026-04-07T15:30:00Z")
    .unwrap()
    .timestamp_millis();
  assert_eq!(result, expected);
}

#[test]
fn test_timestamp_short_bytes_returns_zero_scalar() {
  // Fewer than 8 bytes that aren't valid UTF-8 date strings
  let converter = TimestampConverter::new();
  let result = converter.parse_timestamp(&[0xFF, 0xFE]);
  assert_eq!(result, 0);
}

#[test]
fn test_timestamp_negative_millis() {
  // Timestamps before epoch (e.g., 1969)
  let converter = TimestampConverter::with_range(-1_000_000_000, 1_000_000_000);
  let negative_millis: i64 = -500_000_000;
  let bytes = negative_millis.to_be_bytes();
  let result = converter.parse_timestamp(&bytes);
  assert_eq!(result, negative_millis);
  let scalar = converter.to_scalar(&bytes);
  assert!(scalar > 0.0 && scalar < 1.0);
}

#[test]
fn test_timestamp_scalar_boundary_values() {
  let converter = TimestampConverter::with_range(1000, 2000);
  // Value at min
  let min_bytes = 1000_i64.to_be_bytes();
  let scalar_min = converter.to_scalar(&min_bytes);
  assert!((scalar_min - 0.0).abs() < f64::EPSILON);
  // Value at max
  let max_bytes = 2000_i64.to_be_bytes();
  let scalar_max = converter.to_scalar(&max_bytes);
  assert!((scalar_max - 1.0).abs() < f64::EPSILON);
  // Value at midpoint
  let mid_bytes = 1500_i64.to_be_bytes();
  let scalar_mid = converter.to_scalar(&mid_bytes);
  assert!((scalar_mid - 0.5).abs() < f64::EPSILON);
}

#[test]
fn test_timestamp_serialization_roundtrip() {
  use aeordb::engine::scalar_converter::{deserialize_converter, serialize_converter};

  let converter = TimestampConverter::with_range(100_000, 200_000);
  let serialized = serialize_converter(&converter);
  let deserialized = deserialize_converter(&serialized).unwrap();
  assert_eq!(deserialized.name(), "timestamp");
  assert!(deserialized.is_order_preserving());

  // Same scalar for same input
  let input = 150_000_i64.to_be_bytes();
  let scalar_original = converter.to_scalar(&input);
  let scalar_deserialized = deserialized.to_scalar(&input);
  assert!((scalar_original - scalar_deserialized).abs() < f64::EPSILON);
}
