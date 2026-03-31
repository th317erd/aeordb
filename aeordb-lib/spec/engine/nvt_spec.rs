use aeordb::engine::scalar_converter::{
  HashConverter, ScalarConverter, U64Converter, StringConverter,
};
use aeordb::engine::nvt::NormalizedVectorTable;

fn make_hash(first_byte: u8) -> Vec<u8> {
  let mut hash = vec![0u8; 32];
  hash[0] = first_byte;
  hash
}

fn make_hash_from_bytes(bytes: &[u8; 8]) -> Vec<u8> {
  let mut hash = vec![0u8; 32];
  hash[..8].copy_from_slice(bytes);
  hash
}

fn make_hash_nvt() -> NormalizedVectorTable {
  NormalizedVectorTable::new(Box::new(HashConverter), 16)
}

// --- HashConverter via NVT tests (regression) ---

#[test]
fn test_hash_converter_scalar_range() {
  let converter = HashConverter;
  // All zeros -> 0.0
  let zero_hash = vec![0u8; 32];
  let scalar = converter.to_scalar(&zero_hash);
  assert!(scalar >= 0.0 && scalar <= 1.0, "scalar {} out of range", scalar);
  assert_eq!(scalar, 0.0);

  // All 0xFF -> ~1.0
  let max_hash = vec![0xFF; 32];
  let scalar = converter.to_scalar(&max_hash);
  assert!(scalar >= 0.0 && scalar <= 1.0, "scalar {} out of range", scalar);
  assert!((scalar - 1.0).abs() < 1e-10, "max hash scalar should be ~1.0, got {}", scalar);
}

#[test]
fn test_hash_converter_uniform() {
  let converter = HashConverter;
  let low_hash = make_hash(0x10);
  let mid_hash = make_hash(0x80);
  let high_hash = make_hash(0xF0);

  let low_scalar = converter.to_scalar(&low_hash);
  let mid_scalar = converter.to_scalar(&mid_hash);
  let high_scalar = converter.to_scalar(&high_hash);

  assert!(low_scalar < mid_scalar, "low {} should be < mid {}", low_scalar, mid_scalar);
  assert!(mid_scalar < high_scalar, "mid {} should be < high {}", mid_scalar, high_scalar);

  assert!(low_scalar < 0.15, "low_scalar {} should be < 0.15", low_scalar);
  assert!(mid_scalar > 0.4 && mid_scalar < 0.6, "mid_scalar {} should be ~0.5", mid_scalar);
  assert!(high_scalar > 0.9, "high_scalar {} should be > 0.9", high_scalar);
}

#[test]
fn test_hash_converter_deterministic() {
  let converter = HashConverter;
  let hash = make_hash_from_bytes(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]);
  let scalar_first = converter.to_scalar(&hash);
  let scalar_second = converter.to_scalar(&hash);
  assert_eq!(scalar_first, scalar_second, "same hash must produce same scalar");
}

// --- NVT bucket tests ---

#[test]
fn test_nvt_bucket_lookup() {
  let nvt = make_hash_nvt();

  let low_hash = make_hash(0x00);
  let bucket_index = nvt.bucket_for_value(&low_hash);
  assert_eq!(bucket_index, 0, "zero hash should map to bucket 0");

  let high_hash = make_hash(0xFF);
  let bucket_index = nvt.bucket_for_value(&high_hash);
  assert!(bucket_index >= 14, "high hash should map to bucket >= 14, got {}", bucket_index);
}

#[test]
fn test_nvt_bucket_count() {
  let nvt = NormalizedVectorTable::new(Box::new(HashConverter), 1024);
  assert_eq!(nvt.bucket_count(), 1024);

  let nvt_small = NormalizedVectorTable::new(Box::new(HashConverter), 4);
  assert_eq!(nvt_small.bucket_count(), 4);
}

#[test]
fn test_nvt_update_bucket() {
  let mut nvt = make_hash_nvt();

  nvt.update_bucket(5, 1000, 42);
  let bucket = nvt.get_bucket(5);
  assert_eq!(bucket.kv_block_offset, 1000);
  assert_eq!(bucket.entry_count, 42);

  let other_bucket = nvt.get_bucket(0);
  assert_eq!(other_bucket.kv_block_offset, 0);
  assert_eq!(other_bucket.entry_count, 0);
}

#[test]
fn test_nvt_resize_doubles_buckets() {
  let mut nvt = NormalizedVectorTable::new(Box::new(HashConverter), 8);
  assert_eq!(nvt.bucket_count(), 8);

  nvt.update_bucket(0, 0, 10);
  nvt.update_bucket(3, 10, 20);
  nvt.update_bucket(7, 30, 5);

  nvt.resize(16);
  assert_eq!(nvt.bucket_count(), 16);

  let total_entries: u32 = (0..16).map(|index| nvt.get_bucket(index).entry_count).sum();
  assert_eq!(total_entries, 35, "total entries should be preserved after resize");
}

#[test]
fn test_nvt_serialize_deserialize_roundtrip() {
  let mut nvt = NormalizedVectorTable::new(Box::new(HashConverter), 8);
  nvt.update_bucket(0, 100, 5);
  nvt.update_bucket(3, 200, 10);
  nvt.update_bucket(7, 300, 15);

  let serialized = nvt.serialize();
  let deserialized = NormalizedVectorTable::deserialize(&serialized)
    .expect("deserialization should succeed");

  assert_eq!(deserialized.bucket_count(), 8);
  assert_eq!(deserialized.version(), 1);

  assert_eq!(deserialized.get_bucket(0).kv_block_offset, 100);
  assert_eq!(deserialized.get_bucket(0).entry_count, 5);
  assert_eq!(deserialized.get_bucket(3).kv_block_offset, 200);
  assert_eq!(deserialized.get_bucket(3).entry_count, 10);
  assert_eq!(deserialized.get_bucket(7).kv_block_offset, 300);
  assert_eq!(deserialized.get_bucket(7).entry_count, 15);

  assert_eq!(deserialized.get_bucket(1).kv_block_offset, 0);
  assert_eq!(deserialized.get_bucket(1).entry_count, 0);
}

#[test]
fn test_nvt_empty() {
  let nvt = NormalizedVectorTable::new(Box::new(HashConverter), 0);
  assert_eq!(nvt.bucket_count(), 0);

  let serialized = nvt.serialize();
  let deserialized = NormalizedVectorTable::deserialize(&serialized)
    .expect("empty NVT deserialization should succeed");
  assert_eq!(deserialized.bucket_count(), 0);
}

// --- Error / edge case tests ---

#[test]
fn test_nvt_deserialize_truncated_header() {
  let data = vec![0x01, 0x01];
  let result = NormalizedVectorTable::deserialize(&data);
  assert!(result.is_err(), "truncated header should fail");
}

#[test]
fn test_nvt_deserialize_truncated_buckets() {
  // Build valid header: version(1) + converter_length(4) + converter(1 byte HashConverter) + bucket_count(4) claiming 10 buckets
  let mut data = Vec::new();
  data.push(1); // version
  data.extend_from_slice(&1u32.to_le_bytes()); // converter_length = 1
  data.push(0x01); // HashConverter type tag
  data.extend_from_slice(&10u32.to_le_bytes()); // bucket_count = 10
  // Missing bucket data

  let result = NormalizedVectorTable::deserialize(&data);
  assert!(result.is_err(), "truncated bucket data should fail");
}

#[test]
fn test_nvt_deserialize_invalid_version() {
  let mut data = Vec::new();
  data.push(0); // invalid version
  data.extend_from_slice(&1u32.to_le_bytes());
  data.push(0x01);
  data.extend_from_slice(&0u32.to_le_bytes());

  let result = NormalizedVectorTable::deserialize(&data);
  assert!(result.is_err(), "version 0 should fail");
}

#[test]
fn test_nvt_bucket_for_value_with_max_hash() {
  let nvt = make_hash_nvt();
  let max_hash = vec![0xFF; 32];
  let bucket_index = nvt.bucket_for_value(&max_hash);
  assert!(bucket_index < 16, "bucket index {} must be < 16", bucket_index);
}

#[test]
fn test_nvt_resize_preserves_empty_state() {
  let mut nvt = NormalizedVectorTable::new(Box::new(HashConverter), 4);
  nvt.resize(8);
  assert_eq!(nvt.bucket_count(), 8);

  for index in 0..8 {
    let bucket = nvt.get_bucket(index);
    assert_eq!(bucket.entry_count, 0, "bucket {} should be empty after resize", index);
  }
}

#[test]
fn test_hash_converter_works_with_longer_hashes() {
  let converter = HashConverter;
  let long_hash = vec![0x80; 64];
  let scalar = converter.to_scalar(&long_hash);
  assert!(scalar > 0.4 && scalar < 0.6, "scalar {} should be ~0.5", scalar);
}

// --- New tests: NVT with different converters ---

#[test]
fn test_nvt_with_hash_converter_regression() {
  // Verify that NVT with HashConverter behaves exactly as the old hash_to_scalar-based NVT.
  let nvt = NormalizedVectorTable::new(Box::new(HashConverter), 1024);

  // Zero hash -> bucket 0
  let zero_hash = vec![0u8; 32];
  assert_eq!(nvt.bucket_for_value(&zero_hash), 0);

  // Max hash -> last bucket
  let max_hash = vec![0xFF; 32];
  let bucket = nvt.bucket_for_value(&max_hash);
  assert_eq!(bucket, 1023, "max hash should map to last bucket");

  // Mid hash -> roughly middle
  let mid_hash = make_hash(0x80);
  let bucket = nvt.bucket_for_value(&mid_hash);
  assert!(bucket > 400 && bucket < 600, "mid hash should map near bucket 512, got {}", bucket);
}

#[test]
fn test_nvt_with_u64_converter() {
  let converter = U64Converter::with_range(0, 1000);
  let nvt = NormalizedVectorTable::new(Box::new(converter), 100);

  // Value 0 -> bucket 0
  let zero_bytes = 0u64.to_be_bytes();
  assert_eq!(nvt.bucket_for_value(&zero_bytes), 0);

  // Value 500 -> bucket ~50
  let mid_bytes = 500u64.to_be_bytes();
  let bucket = nvt.bucket_for_value(&mid_bytes);
  assert!(bucket >= 45 && bucket <= 55, "500/1000 should map near bucket 50, got {}", bucket);

  // Value 1000 -> last bucket
  let max_bytes = 1000u64.to_be_bytes();
  let bucket = nvt.bucket_for_value(&max_bytes);
  assert_eq!(bucket, 99, "max value should map to last bucket, got {}", bucket);

  // Serialization roundtrip preserves converter behavior
  let serialized = nvt.serialize();
  let deserialized = NormalizedVectorTable::deserialize(&serialized)
    .expect("deserialization should succeed");
  assert_eq!(deserialized.bucket_for_value(&mid_bytes), nvt.bucket_for_value(&mid_bytes));
}

#[test]
fn test_nvt_with_string_converter() {
  let converter = StringConverter::new(1024);
  let nvt = NormalizedVectorTable::new(Box::new(converter), 100);

  // Empty string -> bucket 0
  let empty: &[u8] = b"";
  assert_eq!(nvt.bucket_for_value(empty), 0);

  // "a" (0x61) -> some bucket in the lower-mid range
  let a_bytes = b"a";
  let bucket_a = nvt.bucket_for_value(a_bytes);

  // "z" (0x7A) -> should be higher than "a"
  let z_bytes = b"z";
  let bucket_z = nvt.bucket_for_value(z_bytes);
  assert!(bucket_z >= bucket_a, "z bucket {} should be >= a bucket {}", bucket_z, bucket_a);

  // Serialization roundtrip
  let serialized = nvt.serialize();
  let deserialized = NormalizedVectorTable::deserialize(&serialized)
    .expect("deserialization should succeed");
  assert_eq!(deserialized.bucket_for_value(a_bytes), bucket_a);
}
