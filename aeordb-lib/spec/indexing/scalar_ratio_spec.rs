use aeordb::indexing::{
  F64Mapping, I64Mapping, IndexManager, OffsetTable, ScalarIndex, ScalarMapping, StringMapping,
  U16Mapping, U32Mapping, U64Mapping, U8Mapping,
};

// ─── Scalar Mapping: u8 ─────────────────────────────────────────────────────

#[test]
fn test_u8_maps_to_unit_range() {
  let mapping = U8Mapping;

  let zero_scalar = mapping.map_to_scalar(&[0u8]);
  assert!((zero_scalar - 0.0).abs() < f64::EPSILON, "u8 0 should map to 0.0");

  let max_scalar = mapping.map_to_scalar(&[255u8]);
  assert!((max_scalar - 1.0).abs() < f64::EPSILON, "u8 255 should map to 1.0");

  let mid_scalar = mapping.map_to_scalar(&[128u8]);
  assert!(
    (mid_scalar - 128.0 / 255.0).abs() < f64::EPSILON,
    "u8 128 should map to ~0.502"
  );
}

#[test]
fn test_u8_mapping_empty_input() {
  let mapping = U8Mapping;
  let scalar = mapping.map_to_scalar(&[]);
  assert!((scalar - 0.0).abs() < f64::EPSILON, "empty input should map to 0.0");
}

// ─── Scalar Mapping: u16 ────────────────────────────────────────────────────

#[test]
fn test_u16_maps_to_unit_range() {
  let mapping = U16Mapping;

  let zero_scalar = mapping.map_to_scalar(&0u16.to_be_bytes());
  assert!((zero_scalar - 0.0).abs() < f64::EPSILON, "u16 0 should map to 0.0");

  let max_scalar = mapping.map_to_scalar(&u16::MAX.to_be_bytes());
  assert!((max_scalar - 1.0).abs() < f64::EPSILON, "u16 MAX should map to 1.0");

  let mid_scalar = mapping.map_to_scalar(&32768u16.to_be_bytes());
  let expected = 32768.0 / u16::MAX as f64;
  assert!(
    (mid_scalar - expected).abs() < 1e-10,
    "u16 32768 should map to ~0.500"
  );
}

#[test]
fn test_u16_mapping_short_input() {
  let mapping = U16Mapping;
  let scalar = mapping.map_to_scalar(&[42u8]);
  assert!((scalar - 0.0).abs() < f64::EPSILON, "short input should map to 0.0");
}

// ─── Scalar Mapping: u32 ────────────────────────────────────────────────────

#[test]
fn test_u32_maps_to_unit_range() {
  let mapping = U32Mapping;

  let zero_scalar = mapping.map_to_scalar(&0u32.to_be_bytes());
  assert!((zero_scalar - 0.0).abs() < f64::EPSILON);

  let max_scalar = mapping.map_to_scalar(&u32::MAX.to_be_bytes());
  assert!((max_scalar - 1.0).abs() < f64::EPSILON);

  let mid_scalar = mapping.map_to_scalar(&(u32::MAX / 2).to_be_bytes());
  let expected = (u32::MAX / 2) as f64 / u32::MAX as f64;
  assert!((mid_scalar - expected).abs() < 1e-10);
}

#[test]
fn test_u32_mapping_short_input() {
  let mapping = U32Mapping;
  let scalar = mapping.map_to_scalar(&[1, 2]);
  assert!((scalar - 0.0).abs() < f64::EPSILON);
}

// ─── Scalar Mapping: u64 ────────────────────────────────────────────────────

#[test]
fn test_u64_maps_to_unit_range() {
  let mapping = U64Mapping;

  let zero_scalar = mapping.map_to_scalar(&0u64.to_be_bytes());
  assert!((zero_scalar - 0.0).abs() < f64::EPSILON);

  let max_scalar = mapping.map_to_scalar(&u64::MAX.to_be_bytes());
  assert!((max_scalar - 1.0).abs() < f64::EPSILON);
}

#[test]
fn test_u64_mapping_short_input() {
  let mapping = U64Mapping;
  let scalar = mapping.map_to_scalar(&[1, 2, 3, 4]);
  assert!((scalar - 0.0).abs() < f64::EPSILON);
}

// ─── Scalar Mapping: i64 ────────────────────────────────────────────────────

#[test]
fn test_i64_negative_maps_below_zero() {
  let mapping = I64Mapping;

  let negative_scalar = mapping.map_to_scalar(&(-100i64).to_be_bytes());
  assert!(
    negative_scalar < 0.0,
    "negative i64 should map below 0.0 (in the [-1.0, 0.0) branch), got {}",
    negative_scalar
  );
  assert!(
    negative_scalar > -1.0,
    "negative i64 should be above -1.0, got {}",
    negative_scalar
  );
}

#[test]
fn test_i64_positive_maps_above_zero() {
  let mapping = I64Mapping;

  // Use a large positive value to avoid floating-point precision loss
  let positive_scalar = mapping.map_to_scalar(&(i64::MAX / 2).to_be_bytes());
  assert!(
    positive_scalar > 0.5,
    "positive i64 should map above 0.5, got {}",
    positive_scalar
  );
  assert!(
    positive_scalar <= 1.0,
    "positive i64 should not exceed 1.0, got {}",
    positive_scalar
  );

  // Small positive values should be >= 0.5
  let small_positive = mapping.map_to_scalar(&100i64.to_be_bytes());
  assert!(
    small_positive >= 0.5,
    "small positive i64 should map at or above 0.5, got {}",
    small_positive
  );
}

#[test]
fn test_i64_zero_maps_to_zero_point_five() {
  let mapping = I64Mapping;

  let zero_scalar = mapping.map_to_scalar(&0i64.to_be_bytes());
  assert!(
    (zero_scalar - 0.5).abs() < f64::EPSILON,
    "i64 zero should map to 0.5, got {}",
    zero_scalar
  );
}

#[test]
fn test_i64_min_maps_to_near_zero() {
  let mapping = I64Mapping;

  let min_scalar = mapping.map_to_scalar(&i64::MIN.to_be_bytes());
  // i64::MIN / i64::MIN * -1.0 = -1.0
  assert!(
    (min_scalar - (-1.0)).abs() < f64::EPSILON,
    "i64::MIN should map to -1.0, got {}",
    min_scalar
  );
}

#[test]
fn test_i64_max_maps_to_one() {
  let mapping = I64Mapping;

  let max_scalar = mapping.map_to_scalar(&i64::MAX.to_be_bytes());
  assert!(
    (max_scalar - 1.0).abs() < f64::EPSILON,
    "i64::MAX should map to 1.0, got {}",
    max_scalar
  );
}

#[test]
fn test_i64_preserves_ordering() {
  let mapping = I64Mapping;

  // Use values large enough to distinguish in f64 precision
  // The jump from negative to zero (0.5) and zero to positive is always
  // large, so ordering is preserved across sign boundaries.
  // Within the same sign, values must be large enough to not collapse in f64.
  let values: Vec<i64> = vec![
    i64::MIN,
    i64::MIN / 2,
    -1_000_000_000,
    0,
    1_000_000_000,
    i64::MAX / 2,
    i64::MAX,
  ];
  let scalars: Vec<f64> = values
    .iter()
    .map(|value| mapping.map_to_scalar(&value.to_be_bytes()))
    .collect();

  for (index, window) in scalars.windows(2).enumerate() {
    assert!(
      window[0] < window[1],
      "i64 ordering should be preserved in scalar space: \
       values[{}]={} (scalar {}) should be < values[{}]={} (scalar {})",
      index,
      values[index],
      window[0],
      index + 1,
      values[index + 1],
      window[1]
    );
  }
}

// ─── Scalar Mapping: f64 ────────────────────────────────────────────────────

#[test]
fn test_f64_mapping_basic() {
  let mapping = F64Mapping::new(0.0, 100.0);

  let zero_scalar = mapping.map_to_scalar(&0.0f64.to_be_bytes());
  assert!((zero_scalar - 0.0).abs() < f64::EPSILON);

  let max_scalar = mapping.map_to_scalar(&100.0f64.to_be_bytes());
  assert!((max_scalar - 1.0).abs() < f64::EPSILON);

  let mid_scalar = mapping.map_to_scalar(&50.0f64.to_be_bytes());
  assert!((mid_scalar - 0.5).abs() < f64::EPSILON);
}

#[test]
fn test_f64_mapping_clamps_out_of_range() {
  let mapping = F64Mapping::new(0.0, 100.0);

  let below = mapping.map_to_scalar(&(-50.0f64).to_be_bytes());
  assert!((below - 0.0).abs() < f64::EPSILON, "below range should clamp to 0.0");

  let above = mapping.map_to_scalar(&200.0f64.to_be_bytes());
  assert!((above - 1.0).abs() < f64::EPSILON, "above range should clamp to 1.0");
}

#[test]
fn test_f64_mapping_short_input() {
  let mapping = F64Mapping::new(0.0, 100.0);
  let scalar = mapping.map_to_scalar(&[1, 2, 3]);
  assert!((scalar - 0.0).abs() < f64::EPSILON);
}

#[test]
#[should_panic(expected = "maximum must be greater than minimum")]
fn test_f64_mapping_invalid_range_panics() {
  F64Mapping::new(100.0, 0.0);
}

// ─── Scalar Mapping: String ─────────────────────────────────────────────────

#[test]
fn test_string_mapping_produces_valid_range() {
  let mapping = StringMapping::new(256);

  let test_strings = vec!["", "a", "hello", "ZZZZZZ", "a long string for testing"];
  for string in test_strings {
    let scalar = mapping.map_to_scalar(string.as_bytes());
    assert!(
      scalar >= 0.0 && scalar <= 1.0,
      "string '{}' produced out-of-range scalar: {}",
      string,
      scalar
    );
  }
}

#[test]
fn test_string_mapping_empty_is_zero() {
  let mapping = StringMapping::new(256);
  let scalar = mapping.map_to_scalar(b"");
  assert!((scalar - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_string_mapping_is_order_preserving() {
  let mapping = StringMapping::new(256);

  // Same-length strings: lexicographic order should be preserved
  // because first-byte dominates (70% weight)
  let scalar_a = mapping.map_to_scalar(b"apple");
  let scalar_b = mapping.map_to_scalar(b"banana");
  let scalar_c = mapping.map_to_scalar(b"cherry");

  assert!(
    scalar_a < scalar_b,
    "apple ({}) should map below banana ({})",
    scalar_a,
    scalar_b
  );
  assert!(
    scalar_b < scalar_c,
    "banana ({}) should map below cherry ({})",
    scalar_b,
    scalar_c
  );
}

#[test]
#[should_panic(expected = "max_expected_length must be positive")]
fn test_string_mapping_zero_max_length_panics() {
  StringMapping::new(0);
}

#[test]
fn test_string_mapping_value_type_name() {
  let mapping = StringMapping::new(256);
  assert_eq!(mapping.value_type_name(), "string");
}

// ─── Offset Table ───────────────────────────────────────────────────────────

#[test]
fn test_insert_and_lookup_exact() {
  let mut table = OffsetTable::new(100);
  table.insert(0.5, 42);

  let entry = table.lookup(0.5).expect("should find entry at 0.5");
  assert!((entry.scalar - 0.5).abs() < f64::EPSILON);
  assert_eq!(entry.location, 42);
  assert!(!entry.is_approximate);
}

#[test]
fn test_lookup_range() {
  let mut table = OffsetTable::new(100);
  table.insert(0.1, 10);
  table.insert(0.3, 30);
  table.insert(0.5, 50);
  table.insert(0.7, 70);
  table.insert(0.9, 90);

  let range = table.lookup_range(0.2, 0.6);
  let locations: Vec<u64> = range.iter().map(|entry| entry.location).collect();
  assert_eq!(locations, vec![30, 50]);
}

#[test]
fn test_lookup_range_empty() {
  let table = OffsetTable::new(100);
  let range = table.lookup_range(0.0, 1.0);
  assert!(range.is_empty());
}

#[test]
fn test_lookup_range_no_match() {
  let mut table = OffsetTable::new(100);
  table.insert(0.1, 10);
  table.insert(0.9, 90);

  let range = table.lookup_range(0.4, 0.6);
  assert!(range.is_empty());
}

#[test]
fn test_lookup_greater_than() {
  let mut table = OffsetTable::new(100);
  table.insert(0.2, 20);
  table.insert(0.5, 50);
  table.insert(0.8, 80);

  let greater = table.entries_greater_than(0.5);
  assert_eq!(greater.len(), 1);
  assert_eq!(greater[0].location, 80);
}

#[test]
fn test_lookup_less_than() {
  let mut table = OffsetTable::new(100);
  table.insert(0.2, 20);
  table.insert(0.5, 50);
  table.insert(0.8, 80);

  let less = table.entries_less_than(0.5);
  assert_eq!(less.len(), 1);
  assert_eq!(less[0].location, 20);
}

#[test]
fn test_offset_table_self_corrects_on_write_back() {
  let mut table = OffsetTable::new(100);
  table.insert(0.5, 42);

  // Simulate stale data by resizing
  table.resize(200);
  assert!(table.entries()[0].is_approximate);

  // Self-correcting write-back
  table.correct_entry(0.5, 99);

  let entry = table.lookup(0.5).unwrap();
  assert_eq!(entry.location, 99);
  assert!(!entry.is_approximate, "correction should clear approximate flag");
}

#[test]
fn test_offset_table_resize_marks_entries_approximate() {
  let mut table = OffsetTable::new(100);
  table.insert(0.1, 10);
  table.insert(0.5, 50);
  table.insert(0.9, 90);

  // All entries should be exact before resize
  for entry in table.entries() {
    assert!(!entry.is_approximate);
  }

  table.resize(200);

  // All entries should be approximate after resize
  for entry in table.entries() {
    assert!(entry.is_approximate, "all entries should be approximate after resize");
  }

  assert_eq!(table.capacity(), 200);
}

#[test]
fn test_offset_table_heals_after_corrections() {
  let mut table = OffsetTable::new(100);
  table.insert(0.1, 10);
  table.insert(0.5, 50);
  table.insert(0.9, 90);

  table.resize(200);
  assert!(table.has_approximate_entries());

  // Correct all entries (simulating normal traffic healing)
  table.correct_entry(0.1, 11);
  table.correct_entry(0.5, 51);
  table.correct_entry(0.9, 91);

  assert!(
    !table.has_approximate_entries(),
    "all entries should be healed after corrections"
  );

  // Verify corrected locations
  assert_eq!(table.lookup(0.1).unwrap().location, 11);
  assert_eq!(table.lookup(0.5).unwrap().location, 51);
  assert_eq!(table.lookup(0.9).unwrap().location, 91);
}

#[test]
fn test_insert_many_entries_maintains_sorted_order() {
  let mut table = OffsetTable::new(1000);

  // Insert in random order
  let scalars = vec![0.7, 0.2, 0.9, 0.1, 0.5, 0.3, 0.8, 0.4, 0.6, 0.0];
  for (index, scalar) in scalars.iter().enumerate() {
    table.insert(*scalar, index as u64);
  }

  // Verify sorted order
  let entries = table.entries();
  for window in entries.windows(2) {
    assert!(
      window[0].scalar <= window[1].scalar,
      "entries should be sorted: {} should be <= {}",
      window[0].scalar,
      window[1].scalar
    );
  }
}

#[test]
fn test_empty_table_lookup_returns_none() {
  let table = OffsetTable::new(100);
  assert!(table.lookup(0.5).is_none());
}

#[test]
fn test_offset_table_remove() {
  let mut table = OffsetTable::new(100);
  table.insert(0.5, 42);
  assert_eq!(table.len(), 1);

  let removed = table.remove(0.5, 42);
  assert!(removed);
  assert_eq!(table.len(), 0);
  assert!(table.lookup(0.5).is_none());
}

#[test]
fn test_offset_table_remove_nonexistent() {
  let mut table = OffsetTable::new(100);
  table.insert(0.5, 42);

  let removed = table.remove(0.5, 999);
  assert!(!removed, "should not remove entry with wrong location");
  assert_eq!(table.len(), 1);
}

#[test]
fn test_offset_table_utilization() {
  let mut table = OffsetTable::new(100);
  assert!((table.utilization() - 0.0).abs() < f64::EPSILON);

  for index in 0..50 {
    table.insert(index as f64 / 100.0, index);
  }
  assert!((table.utilization() - 0.5).abs() < f64::EPSILON);
}

#[test]
fn test_offset_table_utilization_zero_capacity() {
  let table = OffsetTable::new(0);
  assert!((table.utilization() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_offset_table_lookup_closest() {
  let mut table = OffsetTable::new(100);
  table.insert(0.2, 20);
  table.insert(0.8, 80);

  // 0.3 is closer to 0.2
  let entry = table.lookup(0.3).unwrap();
  assert_eq!(entry.location, 20);

  // 0.7 is closer to 0.8
  let entry = table.lookup(0.7).unwrap();
  assert_eq!(entry.location, 80);
}

// ─── Scalar Index ───────────────────────────────────────────────────────────

#[test]
fn test_scalar_index_insert_and_lookup_exact() {
  let mut index = ScalarIndex::new(
    "test_index".to_string(),
    Box::new(U8Mapping),
    100,
  );

  index.insert(&[128], 42);
  let result = index.lookup_exact(&[128]);
  assert_eq!(result, Some(42));
}

#[test]
fn test_empty_index_returns_none() {
  let index = ScalarIndex::new(
    "test_index".to_string(),
    Box::new(U8Mapping),
    100,
  );

  assert_eq!(index.lookup_exact(&[128]), None);
}

#[test]
fn test_scalar_index_lookup_range() {
  let mut index = ScalarIndex::new(
    "test_index".to_string(),
    Box::new(U8Mapping),
    100,
  );

  index.insert(&[50], 100);
  index.insert(&[100], 200);
  index.insert(&[150], 300);
  index.insert(&[200], 400);

  // Range from 75 to 175
  let results = index.lookup_range(&[75], &[175]);
  assert!(results.contains(&200), "should contain location for 100");
  assert!(results.contains(&300), "should contain location for 150");
}

#[test]
fn test_scalar_index_lookup_greater_than() {
  let mut index = ScalarIndex::new(
    "test_index".to_string(),
    Box::new(U8Mapping),
    100,
  );

  index.insert(&[50], 100);
  index.insert(&[100], 200);
  index.insert(&[200], 400);

  let results = index.lookup_greater_than(&[100]);
  assert_eq!(results.len(), 1);
  assert!(results.contains(&400));
}

#[test]
fn test_scalar_index_lookup_less_than() {
  let mut index = ScalarIndex::new(
    "test_index".to_string(),
    Box::new(U8Mapping),
    100,
  );

  index.insert(&[50], 100);
  index.insert(&[100], 200);
  index.insert(&[200], 400);

  let results = index.lookup_less_than(&[100]);
  assert_eq!(results.len(), 1);
  assert!(results.contains(&100));
}

#[test]
fn test_remove_entry() {
  let mut index = ScalarIndex::new(
    "test_index".to_string(),
    Box::new(U8Mapping),
    100,
  );

  index.insert(&[128], 42);
  assert_eq!(index.stats().entry_count, 1);

  let removed = index.remove(&[128], 42);
  assert!(removed);
  assert_eq!(index.stats().entry_count, 0);
  assert_eq!(index.lookup_exact(&[128]), None);
}

#[test]
fn test_remove_nonexistent_entry() {
  let mut index = ScalarIndex::new(
    "test_index".to_string(),
    Box::new(U8Mapping),
    100,
  );

  let removed = index.remove(&[128], 42);
  assert!(!removed);
  assert_eq!(index.stats().entry_count, 0);
}

#[test]
fn test_index_stats_accurate() {
  let mut index = ScalarIndex::new(
    "test_index".to_string(),
    Box::new(U8Mapping),
    200,
  );

  let stats = index.stats();
  assert_eq!(stats.entry_count, 0);
  assert_eq!(stats.table_capacity, 200);
  assert!((stats.utilization_percentage - 0.0).abs() < f64::EPSILON);

  for value in 0..100u8 {
    index.insert(&[value], value as u64);
  }

  let stats = index.stats();
  assert_eq!(stats.entry_count, 100);
  assert_eq!(stats.table_capacity, 200);
  assert!((stats.utilization_percentage - 50.0).abs() < f64::EPSILON);
}

#[test]
fn test_scalar_index_correct_writeback() {
  let mut index = ScalarIndex::new(
    "test_index".to_string(),
    Box::new(U8Mapping),
    100,
  );

  index.insert(&[128], 42);

  // Simulate stale data via resize
  index.offset_table_mut().resize(200);

  // Self-correcting write-back
  index.correct(&[128], 99);

  let result = index.lookup_exact(&[128]);
  assert_eq!(result, Some(99));
}

#[test]
fn test_duplicate_values_at_same_scalar() {
  let mut index = ScalarIndex::new(
    "test_index".to_string(),
    Box::new(U8Mapping),
    100,
  );

  // Insert same value pointing to different locations
  index.insert(&[128], 42);
  index.insert(&[128], 99);

  assert_eq!(index.stats().entry_count, 2);

  // lookup_exact finds the first match at that scalar
  let result = index.lookup_exact(&[128]);
  assert!(result.is_some());

  // Range query should find both
  let range_results = index.lookup_range(&[127], &[129]);
  assert_eq!(range_results.len(), 2);
  assert!(range_results.contains(&42));
  assert!(range_results.contains(&99));
}

// ─── Index Manager ──────────────────────────────────────────────────────────

#[test]
fn test_create_and_drop_index_via_manager() {
  let mut manager = IndexManager::new();

  let definition = manager.create_index("users", "age", "u8").expect("should create index");
  assert_eq!(definition.table_name, "users");
  assert_eq!(definition.column_name, "age");
  assert_eq!(definition.mapping_type, "u8");

  let index = manager.get_index("users", "age");
  assert!(index.is_some());

  manager.drop_index("users", "age").expect("should drop index");
  assert!(manager.get_index("users", "age").is_none());
}

#[test]
fn test_create_duplicate_index_fails() {
  let mut manager = IndexManager::new();

  manager.create_index("users", "age", "u8").expect("first create should succeed");
  let result = manager.create_index("users", "age", "u8");
  assert!(result.is_err(), "duplicate index creation should fail");
}

#[test]
fn test_drop_nonexistent_index_fails() {
  let mut manager = IndexManager::new();

  let result = manager.drop_index("users", "age");
  assert!(result.is_err(), "dropping nonexistent index should fail");
}

#[test]
fn test_unsupported_mapping_type_fails() {
  let mut manager = IndexManager::new();

  let result = manager.create_index("users", "data", "blob");
  assert!(result.is_err(), "unsupported mapping type should fail");
}

#[test]
fn test_list_indexes_by_table() {
  let mut manager = IndexManager::new();

  manager.create_index("users", "age", "u8").unwrap();
  manager.create_index("users", "name", "string").unwrap();
  manager.create_index("orders", "total", "f64").unwrap();

  let user_indexes = manager.list_indexes(Some("users"));
  assert_eq!(user_indexes.len(), 2);

  let order_indexes = manager.list_indexes(Some("orders"));
  assert_eq!(order_indexes.len(), 1);

  let all_indexes = manager.list_indexes(None);
  assert_eq!(all_indexes.len(), 3);
}

#[test]
fn test_list_indexes_empty_table() {
  let manager = IndexManager::new();

  let indexes = manager.list_indexes(Some("nonexistent"));
  assert!(indexes.is_empty());
}

#[test]
fn test_get_index_mut_and_insert() {
  let mut manager = IndexManager::new();

  manager.create_index("users", "age", "u8").unwrap();

  let index = manager.get_index_mut("users", "age").expect("should get mutable index");
  index.insert(&[25], 100);
  index.insert(&[30], 200);

  let index = manager.get_index("users", "age").expect("should get index");
  assert_eq!(index.lookup_exact(&[25]), Some(100));
  assert_eq!(index.lookup_exact(&[30]), Some(200));
}

#[test]
fn test_create_index_all_supported_types() {
  let mut manager = IndexManager::new();

  let types = vec!["u8", "u16", "u32", "u64", "i64", "f64", "string"];
  for (index, mapping_type) in types.iter().enumerate() {
    let table_name = format!("table_{}", index);
    let result = manager.create_index(&table_name, "column", mapping_type);
    assert!(
      result.is_ok(),
      "should create index for mapping type: {}",
      mapping_type
    );
  }
}

// ─── Integration: full workflow ─────────────────────────────────────────────

#[test]
fn test_full_index_lifecycle() {
  let mut manager = IndexManager::new();

  // Create index
  manager.create_index("products", "price", "f64").unwrap();

  // Insert data
  let index = manager.get_index_mut("products", "price").unwrap();
  let prices = vec![9.99, 19.99, 49.99, 99.99, 149.99];
  for (offset, price) in prices.iter().enumerate() {
    index.insert(&(*price as f64).to_be_bytes(), offset as u64);
  }

  // Verify stats
  let stats = index.stats();
  assert_eq!(stats.entry_count, 5);

  // Range query
  let index = manager.get_index("products", "price").unwrap();
  let range = index.lookup_range(&10.0f64.to_be_bytes(), &100.0f64.to_be_bytes());
  assert!(range.len() >= 2, "should find products in price range");

  // Drop index
  manager.drop_index("products", "price").unwrap();
  assert!(manager.get_index("products", "price").is_none());
}

#[test]
fn test_self_healing_workflow() {
  let mut index = ScalarIndex::new(
    "healing_test".to_string(),
    Box::new(U64Mapping),
    100,
  );

  // Insert entries
  for value in (0..10u64).map(|value| value * 1000) {
    index.insert(&value.to_be_bytes(), value);
  }

  // Simulate resize (e.g., table grew)
  index.offset_table_mut().resize(200);

  // All entries are now approximate
  assert!(index.offset_table().has_approximate_entries());

  // Normal traffic heals it: each read that discovers stale data writes back
  for value in (0..10u64).map(|value| value * 1000) {
    let correct_location = value + 1; // "corrected" location
    index.correct(&value.to_be_bytes(), correct_location);
  }

  // After healing, no approximate entries
  assert!(!index.offset_table().has_approximate_entries());
}
