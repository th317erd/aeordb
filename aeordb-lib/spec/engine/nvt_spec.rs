use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::nvt::{hash_to_scalar, NormalizedVectorTable};

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

// --- hash_to_scalar tests ---

#[test]
fn test_hash_to_scalar_range() {
  // All zeros -> 0.0
  let zero_hash = vec![0u8; 32];
  let scalar = hash_to_scalar(&zero_hash);
  assert!(scalar >= 0.0 && scalar <= 1.0, "scalar {} out of range", scalar);
  assert_eq!(scalar, 0.0);

  // All 0xFF -> ~1.0
  let max_hash = vec![0xFF; 32];
  let scalar = hash_to_scalar(&max_hash);
  assert!(scalar >= 0.0 && scalar <= 1.0, "scalar {} out of range", scalar);
  assert!((scalar - 1.0).abs() < 1e-10, "max hash scalar should be ~1.0, got {}", scalar);
}

#[test]
fn test_hash_to_scalar_uniform() {
  // Different first bytes should produce different scalars spread across range
  let low_hash = make_hash(0x10);
  let mid_hash = make_hash(0x80);
  let high_hash = make_hash(0xF0);

  let low_scalar = hash_to_scalar(&low_hash);
  let mid_scalar = hash_to_scalar(&mid_hash);
  let high_scalar = hash_to_scalar(&high_hash);

  assert!(low_scalar < mid_scalar, "low {} should be < mid {}", low_scalar, mid_scalar);
  assert!(mid_scalar < high_scalar, "mid {} should be < high {}", mid_scalar, high_scalar);

  // They should be in roughly the right ranges
  assert!(low_scalar < 0.15, "low_scalar {} should be < 0.15", low_scalar);
  assert!(mid_scalar > 0.4 && mid_scalar < 0.6, "mid_scalar {} should be ~0.5", mid_scalar);
  assert!(high_scalar > 0.9, "high_scalar {} should be > 0.9", high_scalar);
}

#[test]
fn test_hash_to_scalar_deterministic() {
  let hash = make_hash_from_bytes(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE]);
  let scalar_first = hash_to_scalar(&hash);
  let scalar_second = hash_to_scalar(&hash);
  assert_eq!(scalar_first, scalar_second, "same hash must produce same scalar");
}

// --- NVT tests ---

#[test]
fn test_nvt_bucket_lookup() {
  let nvt = NormalizedVectorTable::new(HashAlgorithm::Blake3_256, 16);

  // Low hash should map to a low bucket
  let low_hash = make_hash(0x00);
  let bucket_index = nvt.bucket_for_hash(&low_hash);
  assert_eq!(bucket_index, 0, "zero hash should map to bucket 0");

  // High hash should map to a high bucket
  let high_hash = make_hash(0xFF);
  let bucket_index = nvt.bucket_for_hash(&high_hash);
  assert!(bucket_index >= 14, "high hash should map to bucket >= 14, got {}", bucket_index);
}

#[test]
fn test_nvt_bucket_count() {
  let nvt = NormalizedVectorTable::new(HashAlgorithm::Blake3_256, 1024);
  assert_eq!(nvt.bucket_count(), 1024);

  let nvt_small = NormalizedVectorTable::new(HashAlgorithm::Blake3_256, 4);
  assert_eq!(nvt_small.bucket_count(), 4);
}

#[test]
fn test_nvt_update_bucket() {
  let mut nvt = NormalizedVectorTable::new(HashAlgorithm::Blake3_256, 16);

  nvt.update_bucket(5, 1000, 42);
  let bucket = nvt.get_bucket(5);
  assert_eq!(bucket.kv_block_offset, 1000);
  assert_eq!(bucket.entry_count, 42);

  // Verify other buckets are unchanged
  let other_bucket = nvt.get_bucket(0);
  assert_eq!(other_bucket.kv_block_offset, 0);
  assert_eq!(other_bucket.entry_count, 0);
}

#[test]
fn test_nvt_resize_doubles_buckets() {
  let mut nvt = NormalizedVectorTable::new(HashAlgorithm::Blake3_256, 8);
  assert_eq!(nvt.bucket_count(), 8);

  // Populate some buckets with data
  nvt.update_bucket(0, 0, 10);
  nvt.update_bucket(3, 10, 20);
  nvt.update_bucket(7, 30, 5);

  nvt.resize(16);
  assert_eq!(nvt.bucket_count(), 16);

  // Total entries should be preserved
  let total_entries: u32 = (0..16).map(|index| nvt.get_bucket(index).entry_count).sum();
  assert_eq!(total_entries, 35, "total entries should be preserved after resize");
}

#[test]
fn test_nvt_serialize_deserialize_roundtrip() {
  let mut nvt = NormalizedVectorTable::new(HashAlgorithm::Blake3_256, 8);
  nvt.update_bucket(0, 100, 5);
  nvt.update_bucket(3, 200, 10);
  nvt.update_bucket(7, 300, 15);

  let serialized = nvt.serialize();
  let deserialized = NormalizedVectorTable::deserialize(&serialized)
    .expect("deserialization should succeed");

  assert_eq!(deserialized.bucket_count(), 8);
  assert_eq!(deserialized.version(), 1);

  // Verify bucket contents
  assert_eq!(deserialized.get_bucket(0).kv_block_offset, 100);
  assert_eq!(deserialized.get_bucket(0).entry_count, 5);
  assert_eq!(deserialized.get_bucket(3).kv_block_offset, 200);
  assert_eq!(deserialized.get_bucket(3).entry_count, 10);
  assert_eq!(deserialized.get_bucket(7).kv_block_offset, 300);
  assert_eq!(deserialized.get_bucket(7).entry_count, 15);

  // Unset buckets should remain zero
  assert_eq!(deserialized.get_bucket(1).kv_block_offset, 0);
  assert_eq!(deserialized.get_bucket(1).entry_count, 0);
}

#[test]
fn test_nvt_empty() {
  let nvt = NormalizedVectorTable::new(HashAlgorithm::Blake3_256, 0);
  assert_eq!(nvt.bucket_count(), 0);

  let serialized = nvt.serialize();
  let deserialized = NormalizedVectorTable::deserialize(&serialized)
    .expect("empty NVT deserialization should succeed");
  assert_eq!(deserialized.bucket_count(), 0);
}

// --- Error / edge case tests ---

#[test]
fn test_nvt_deserialize_truncated_header() {
  let data = vec![0x01, 0x01]; // too short
  let result = NormalizedVectorTable::deserialize(&data);
  assert!(result.is_err(), "truncated header should fail");
}

#[test]
fn test_nvt_deserialize_truncated_buckets() {
  // Valid header claiming 10 buckets, but no bucket data
  let mut data = Vec::new();
  data.push(1); // version
  data.extend_from_slice(&1u16.to_le_bytes()); // hash_algo
  data.extend_from_slice(&10u32.to_le_bytes()); // bucket_count = 10
  // Missing bucket data

  let result = NormalizedVectorTable::deserialize(&data);
  assert!(result.is_err(), "truncated bucket data should fail");
}

#[test]
fn test_nvt_deserialize_invalid_version() {
  let mut data = Vec::new();
  data.push(0); // invalid version
  data.extend_from_slice(&1u16.to_le_bytes());
  data.extend_from_slice(&0u32.to_le_bytes());

  let result = NormalizedVectorTable::deserialize(&data);
  assert!(result.is_err(), "version 0 should fail");
}

#[test]
fn test_nvt_deserialize_invalid_hash_algorithm() {
  let mut data = Vec::new();
  data.push(1); // version
  data.extend_from_slice(&0xFFFFu16.to_le_bytes()); // invalid hash algo
  data.extend_from_slice(&0u32.to_le_bytes());

  let result = NormalizedVectorTable::deserialize(&data);
  assert!(result.is_err(), "invalid hash algo should fail");
}

#[test]
fn test_nvt_bucket_for_hash_with_max_hash() {
  // Ensure max hash (all 0xFF) doesn't panic or go out of bounds
  let nvt = NormalizedVectorTable::new(HashAlgorithm::Blake3_256, 16);
  let max_hash = vec![0xFF; 32];
  let bucket_index = nvt.bucket_for_hash(&max_hash);
  assert!(bucket_index < 16, "bucket index {} must be < 16", bucket_index);
}

#[test]
fn test_nvt_resize_preserves_empty_state() {
  let mut nvt = NormalizedVectorTable::new(HashAlgorithm::Blake3_256, 4);
  nvt.resize(8);
  assert_eq!(nvt.bucket_count(), 8);

  for index in 0..8 {
    let bucket = nvt.get_bucket(index);
    assert_eq!(bucket.entry_count, 0, "bucket {} should be empty after resize", index);
  }
}

#[test]
fn test_nvt_serialize_size() {
  let nvt = NormalizedVectorTable::new(HashAlgorithm::Blake3_256, 1024);
  let serialized = nvt.serialize();
  // version(1) + hash_algo(2) + bucket_count(4) + 1024 * 12 = 7 + 12288 = 12295
  assert_eq!(serialized.len(), 7 + 1024 * 12);
}

#[test]
fn test_hash_to_scalar_works_with_longer_hashes() {
  // SHA-512 produces 64-byte hashes. hash_to_scalar should still work.
  let mut long_hash = vec![0x80; 64];
  long_hash[0] = 0x80;
  let scalar = hash_to_scalar(&long_hash);
  assert!(scalar > 0.4 && scalar < 0.6, "scalar {} should be ~0.5", scalar);
}
