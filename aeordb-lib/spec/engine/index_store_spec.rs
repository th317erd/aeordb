use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_store::{FieldIndex, IndexManager};
use aeordb::engine::scalar_converter::{
  HashConverter, U64Converter, StringConverter,
};
use aeordb::engine::storage_engine::StorageEngine;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory().unwrap();
  engine
}

#[test]
fn test_create_empty_index() {
  let converter = Box::new(U64Converter::with_range(0, 1000));
  let index = FieldIndex::new("age".to_string(), converter);

  assert_eq!(index.field_name, "age");
  assert_eq!(index.len(), 0);
  assert!(index.is_empty());
}

#[test]
fn test_insert_and_lookup_exact() {
  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  index.insert(&30u64.to_be_bytes(), hash_a.clone());

  let results = index.lookup_exact(&30u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, hash_a);
}

#[test]
fn test_insert_many_sorted() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  // Insert out of order
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  index.insert(&90u64.to_be_bytes(), vec![0x90; 32]);
  index.insert(&30u64.to_be_bytes(), vec![0x30; 32]);

  assert_eq!(index.len(), 4);

  // Verify sorted order by scalar
  for window in index.entries.windows(2) {
    assert!(
      window[0].scalar <= window[1].scalar,
      "Entries not sorted: {} > {}",
      window[0].scalar,
      window[1].scalar,
    );
  }
}

#[test]
fn test_remove_entry() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  let hash_b = vec![0xBB; 32];
  index.insert(&25u64.to_be_bytes(), hash_a.clone());
  index.insert(&30u64.to_be_bytes(), hash_b.clone());

  assert_eq!(index.len(), 2);

  index.remove(&hash_a);
  assert_eq!(index.len(), 1);
  assert_eq!(index.entries[0].file_hash, hash_b);
}

#[test]
fn test_lookup_range() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  index.insert(&20u64.to_be_bytes(), vec![0x20; 32]);
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  index.insert(&80u64.to_be_bytes(), vec![0x80; 32]);

  let results = index.lookup_range(
    &15u64.to_be_bytes(),
    &55u64.to_be_bytes(),
  ).unwrap();

  assert_eq!(results.len(), 2);
  // Should include 20 and 50
  let hashes: Vec<&Vec<u8>> = results.iter().map(|entry| &entry.file_hash).collect();
  assert!(hashes.contains(&&vec![0x20; 32]));
  assert!(hashes.contains(&&vec![0x50; 32]));
}

#[test]
fn test_lookup_gt() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  index.insert(&80u64.to_be_bytes(), vec![0x80; 32]);

  let results = index.lookup_gt(&40u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 2);
}

#[test]
fn test_lookup_lt() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  index.insert(&80u64.to_be_bytes(), vec![0x80; 32]);

  let results = index.lookup_lt(&60u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 2);
}

#[test]
fn test_range_query_on_non_order_preserving_refuses() {
  let converter = Box::new(HashConverter);
  let mut index = FieldIndex::new("hash_field".to_string(), converter);

  index.insert(&[0xAA; 8], vec![0x01; 32]);

  let result = index.lookup_range(&[0x00; 8], &[0xFF; 8]);
  assert!(result.is_err());

  let result = index.lookup_gt(&[0x00; 8]);
  assert!(result.is_err());

  let result = index.lookup_lt(&[0xFF; 8]);
  assert!(result.is_err());
}

#[test]
fn test_serialize_deserialize_roundtrip() {
  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  let hash_b = vec![0xBB; 32];
  index.insert(&25u64.to_be_bytes(), hash_a.clone());
  index.insert(&50u64.to_be_bytes(), hash_b.clone());

  let hash_length = 32;
  let serialized = index.serialize(hash_length);
  let deserialized = FieldIndex::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.field_name, "age");
  assert_eq!(deserialized.len(), 2);
  assert_eq!(deserialized.entries[0].file_hash, hash_a);
  assert_eq!(deserialized.entries[1].file_hash, hash_b);

  // Converter should produce same results
  let original_scalar = index.converter.to_scalar(&25u64.to_be_bytes());
  let deserialized_scalar = deserialized.converter.to_scalar(&25u64.to_be_bytes());
  assert!((original_scalar - deserialized_scalar).abs() < f64::EPSILON);
}

#[test]
fn test_empty_index_lookup_returns_empty() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let results = index.lookup_exact(&42u64.to_be_bytes());
  assert!(results.is_empty());

  let results = index.lookup_range(&0u64.to_be_bytes(), &100u64.to_be_bytes()).unwrap();
  assert!(results.is_empty());

  let results = index.lookup_gt(&0u64.to_be_bytes()).unwrap();
  assert!(results.is_empty());

  let results = index.lookup_lt(&100u64.to_be_bytes()).unwrap();
  assert!(results.is_empty());
}

#[test]
fn test_duplicate_scalars_handled() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  let hash_b = vec![0xBB; 32];

  // Two files with the same age
  index.insert(&30u64.to_be_bytes(), hash_a.clone());
  index.insert(&30u64.to_be_bytes(), hash_b.clone());

  assert_eq!(index.len(), 2);

  let results = index.lookup_exact(&30u64.to_be_bytes());
  assert_eq!(results.len(), 2);

  // Remove one, other remains
  index.remove(&hash_a);
  assert_eq!(index.len(), 1);
  let results = index.lookup_exact(&30u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, hash_b);
}

#[test]
fn test_save_and_load_index_via_engine() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);
  index.insert(&25u64.to_be_bytes(), vec![0xAA; 32]);
  index.insert(&50u64.to_be_bytes(), vec![0xBB; 32]);

  // Save
  index_manager.save_index("/users", &index).unwrap();

  // Load
  let loaded = index_manager.load_index("/users", "age").unwrap().unwrap();
  assert_eq!(loaded.field_name, "age");
  assert_eq!(loaded.len(), 2);
}

#[test]
fn test_list_indexes() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  // Create two indexes
  let converter_age = Box::new(U64Converter::with_range(0, 200));
  index_manager.create_index("/users", "age", converter_age).unwrap();

  let converter_name = Box::new(StringConverter::new(256));
  index_manager.create_index("/users", "name", converter_name).unwrap();

  let indexes = index_manager.list_indexes("/users").unwrap();
  assert_eq!(indexes.len(), 2);
  assert!(indexes.contains(&"age".to_string()));
  assert!(indexes.contains(&"name".to_string()));
}

// --- Additional edge case / failure tests ---

#[test]
fn test_load_nonexistent_index_returns_none() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let result = index_manager.load_index("/nonexistent", "age").unwrap();
  assert!(result.is_none());
}

#[test]
fn test_list_indexes_empty_path_returns_empty() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let indexes = index_manager.list_indexes("/nothing").unwrap();
  assert!(indexes.is_empty());
}

#[test]
fn test_deserialize_corrupt_data_returns_error() {
  let result = FieldIndex::deserialize(&[0x00], 32);
  assert!(result.is_err());
}

#[test]
fn test_deserialize_empty_data_returns_error() {
  let result = FieldIndex::deserialize(&[], 32);
  assert!(result.is_err());
}

#[test]
fn test_serialize_empty_index_roundtrip() {
  let converter = Box::new(StringConverter::new(512));
  let index = FieldIndex::new("email".to_string(), converter);

  let hash_length = 32;
  let serialized = index.serialize(hash_length);
  let deserialized = FieldIndex::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.field_name, "email");
  assert_eq!(deserialized.len(), 0);
}

#[test]
fn test_remove_nonexistent_hash_is_noop() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  index.insert(&30u64.to_be_bytes(), vec![0xAA; 32]);
  assert_eq!(index.len(), 1);

  // Remove a hash that doesn't exist
  index.remove(&vec![0xFF; 32]);
  assert_eq!(index.len(), 1);
}

#[test]
fn test_delete_index() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(U64Converter::with_range(0, 200));
  index_manager.create_index("/users", "age", converter).unwrap();

  // Verify it exists
  let loaded = index_manager.load_index("/users", "age").unwrap();
  assert!(loaded.is_some());

  // Delete it
  index_manager.delete_index("/users", "age").unwrap();

  // Verify it's gone
  let loaded = index_manager.load_index("/users", "age").unwrap();
  assert!(loaded.is_none());
}

#[test]
fn test_overwrite_index_via_save() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);
  index.insert(&25u64.to_be_bytes(), vec![0xAA; 32]);
  index_manager.save_index("/users", &index).unwrap();

  // Modify and save again
  index.insert(&50u64.to_be_bytes(), vec![0xBB; 32]);
  index_manager.save_index("/users", &index).unwrap();

  let loaded = index_manager.load_index("/users", "age").unwrap().unwrap();
  assert_eq!(loaded.len(), 2);
}

// --- NVT-backed lookup tests ---

#[test]
fn test_field_index_nvt_lookup_exact() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  // Insert several values
  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  index.insert(&25u64.to_be_bytes(), vec![0x25; 32]);
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  index.insert(&75u64.to_be_bytes(), vec![0x75; 32]);
  index.insert(&99u64.to_be_bytes(), vec![0x99; 32]);

  // Exact lookup should find the right entry via NVT bucket
  let results = index.lookup_exact(&50u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, vec![0x50; 32]);

  // Lookup a value that doesn't exist
  let results = index.lookup_exact(&42u64.to_be_bytes());
  assert_eq!(results.len(), 0);

  // Lookup at boundaries
  let results = index.lookup_exact(&10u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, vec![0x10; 32]);

  let results = index.lookup_exact(&99u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, vec![0x99; 32]);
}

#[test]
fn test_field_index_nvt_lookup_range() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  for value in (0..=100).step_by(5) {
    let hash_byte = (value & 0xFF) as u8;
    index.insert(&(value as u64).to_be_bytes(), vec![hash_byte; 32]);
  }

  // Range query spanning multiple NVT buckets
  let results = index.lookup_range(
    &20u64.to_be_bytes(),
    &40u64.to_be_bytes(),
  ).unwrap();

  // Should include 20, 25, 30, 35, 40
  assert_eq!(results.len(), 5);
  for entry in &results {
    assert!(entry.scalar >= 0.2 - f64::EPSILON);
    assert!(entry.scalar <= 0.4 + f64::EPSILON);
  }

  // Range at the very start
  let results = index.lookup_range(
    &0u64.to_be_bytes(),
    &5u64.to_be_bytes(),
  ).unwrap();
  assert_eq!(results.len(), 2); // 0 and 5
}

#[test]
fn test_field_index_nvt_rebuild_on_dirty() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  // Initially not dirty (empty, nothing to rebuild)
  assert!(!index.is_dirty());

  // Insert marks dirty
  index.insert(&50u64.to_be_bytes(), vec![0x50; 32]);
  assert!(index.is_dirty());

  // A lookup triggers rebuild, clears dirty
  let _results = index.lookup_exact(&50u64.to_be_bytes());
  assert!(!index.is_dirty());

  // Insert again marks dirty
  index.insert(&25u64.to_be_bytes(), vec![0x25; 32]);
  assert!(index.is_dirty());

  // Another lookup clears dirty and returns correct results
  let result_count = index.lookup_exact(&25u64.to_be_bytes()).len();
  assert!(!index.is_dirty());
  assert_eq!(result_count, 1);
}

#[test]
fn test_field_index_nvt_insert_marks_dirty() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  assert!(!index.is_dirty());

  index.insert(&10u64.to_be_bytes(), vec![0x10; 32]);
  assert!(index.is_dirty());

  // Force rebuild
  index.ensure_nvt_current();
  assert!(!index.is_dirty());

  // Remove marks dirty
  index.remove(&vec![0x10; 32]);
  assert!(index.is_dirty());

  // Removing a non-existent hash does NOT mark dirty
  index.ensure_nvt_current();
  assert!(!index.is_dirty());
  index.remove(&vec![0xFF; 32]);
  assert!(!index.is_dirty());
}

// ===========================================================================
// Task 10: Index serialization with NVT
// ===========================================================================

#[test]
fn test_field_index_serialization_with_nvt() {
  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);

  let hash_a = vec![0xAA; 32];
  let hash_b = vec![0xBB; 32];
  let hash_c = vec![0xCC; 32];
  index.insert(&25u64.to_be_bytes(), hash_a.clone());
  index.insert(&50u64.to_be_bytes(), hash_b.clone());
  index.insert(&75u64.to_be_bytes(), hash_c.clone());

  // Force NVT rebuild before serialize so NVT is current.
  index.ensure_nvt_current();

  let hash_length = 32;
  let serialized = index.serialize(hash_length);

  // The new format should be larger than the old format because it includes NVT data.
  // Minimum NVT overhead: 4 (nvt_length) + version(1) + converter_length(4) + converter_data + bucket_count(4) + buckets
  assert!(serialized.len() > 100);

  let deserialized = FieldIndex::deserialize(&serialized, hash_length).unwrap();
  assert_eq!(deserialized.field_name, "age");
  assert_eq!(deserialized.len(), 3);
  assert_eq!(deserialized.entries[0].file_hash, hash_a);
  assert_eq!(deserialized.entries[1].file_hash, hash_b);
  assert_eq!(deserialized.entries[2].file_hash, hash_c);

  // NVT should be functional after deserialization.
  let mut deserialized = deserialized;
  let results = deserialized.lookup_exact(&50u64.to_be_bytes());
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, hash_b);

  // Converter should produce the same scalars.
  let original_scalar = index.converter.to_scalar(&75u64.to_be_bytes());
  let deserialized_scalar = deserialized.converter.to_scalar(&75u64.to_be_bytes());
  assert!((original_scalar - deserialized_scalar).abs() < f64::EPSILON);
}

#[test]
fn test_field_index_serialization_with_nvt_empty() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let index = FieldIndex::new("score".to_string(), converter);

  let hash_length = 32;
  let serialized = index.serialize(hash_length);
  let deserialized = FieldIndex::deserialize(&serialized, hash_length).unwrap();

  assert_eq!(deserialized.field_name, "score");
  assert_eq!(deserialized.len(), 0);
  assert!(deserialized.is_empty());
}

#[test]
fn test_field_index_serialization_roundtrip_preserves_lookups() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("age".to_string(), converter);

  for value in (0..=100).step_by(10) {
    let hash_byte = (value & 0xFF) as u8;
    index.insert(&(value as u64).to_be_bytes(), vec![hash_byte; 32]);
  }
  index.ensure_nvt_current();

  let hash_length = 32;
  let serialized = index.serialize(hash_length);
  let mut deserialized = FieldIndex::deserialize(&serialized, hash_length).unwrap();

  // Verify all lookups work after roundtrip.
  let results = deserialized.lookup_exact(&50u64.to_be_bytes());
  assert_eq!(results.len(), 1);

  let results = deserialized.lookup_range(&20u64.to_be_bytes(), &60u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 5); // 20, 30, 40, 50, 60

  let results = deserialized.lookup_gt(&70u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 3); // 80, 90, 100

  let results = deserialized.lookup_lt(&30u64.to_be_bytes()).unwrap();
  assert_eq!(results.len(), 3); // 0, 10, 20
}

// ===========================================================================
// Task 3: Direct scalar jump lookups
// ===========================================================================

#[test]
fn test_field_index_scalar_jump_lookup() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  let hash_a = vec![0x10; 32];
  let hash_b = vec![0x50; 32];
  let hash_c = vec![0x99; 32];
  index.insert(&10u64.to_be_bytes(), hash_a.clone());
  index.insert(&50u64.to_be_bytes(), hash_b.clone());
  index.insert(&99u64.to_be_bytes(), hash_c.clone());

  // Compute the scalar for value 50, then look up by scalar directly.
  let scalar_50 = index.converter.to_scalar(&50u64.to_be_bytes());
  let results = index.lookup_by_scalar(scalar_50);
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_hash, hash_b);

  // Scalar for a value that doesn't exist should return empty.
  let scalar_42 = index.converter.to_scalar(&42u64.to_be_bytes());
  let results = index.lookup_by_scalar(scalar_42);
  assert_eq!(results.len(), 0);
}

#[test]
fn test_field_index_scalar_range_lookup() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  for value in (0..=100).step_by(10) {
    let hash_byte = (value & 0xFF) as u8;
    index.insert(&(value as u64).to_be_bytes(), vec![hash_byte; 32]);
  }

  let min_scalar = index.converter.to_scalar(&20u64.to_be_bytes());
  let max_scalar = index.converter.to_scalar(&50u64.to_be_bytes());
  let results = index.lookup_by_scalar_range(min_scalar, max_scalar);

  // Should include 20, 30, 40, 50
  assert_eq!(results.len(), 4);
  for entry in &results {
    assert!(entry.scalar >= min_scalar - f64::EPSILON);
    assert!(entry.scalar <= max_scalar + f64::EPSILON);
  }
}

#[test]
fn test_field_index_scalar_jump_empty_index() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  let results = index.lookup_by_scalar(0.5);
  assert!(results.is_empty());

  let results = index.lookup_by_scalar_range(0.0, 1.0);
  assert!(results.is_empty());
}

#[test]
fn test_field_index_scalar_jump_boundary_values() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("score".to_string(), converter);

  index.insert(&0u64.to_be_bytes(), vec![0x00; 32]);
  index.insert(&100u64.to_be_bytes(), vec![0xFF; 32]);

  // Lookup at the minimum scalar
  let scalar_0 = index.converter.to_scalar(&0u64.to_be_bytes());
  let results = index.lookup_by_scalar(scalar_0);
  assert_eq!(results.len(), 1);

  // Lookup at the maximum scalar
  let scalar_100 = index.converter.to_scalar(&100u64.to_be_bytes());
  let results = index.lookup_by_scalar(scalar_100);
  assert_eq!(results.len(), 1);

  // Full range should return both
  let results = index.lookup_by_scalar_range(scalar_0, scalar_100);
  assert_eq!(results.len(), 2);
}
