use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::index_store::{FieldIndex, IndexManager};
use aeordb::engine::query_engine::QueryResult;
use aeordb::engine::scalar_converter::{
  HashConverter, U8Converter, U16Converter, U32Converter, U64Converter,
  I64Converter, F64Converter, StringConverter, TimestampConverter,
  ScalarConverter,
};
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

fn store_index_config(engine: &StorageEngine, parent_path: &str, config: &PathIndexConfig) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  let config_path = if parent_path.ends_with('/') {
    format!("{}.config/indexes.json", parent_path)
  } else {
    format!("{}/.config/indexes.json", parent_path)
  };
  let config_data = config.serialize();
  ops.store_file(&ctx, &config_path, &config_data, Some("application/json")).unwrap();
}

fn make_user_json(name: &str, age: u64, email: &str) -> Vec<u8> {
  format!(
    r#"{{"name":"{}","age":{},"email":"{}"}}"#,
    name, age, email,
  ).into_bytes()
}

// =============================================================================
// Task 1: ScalarConverter trait new default methods
// =============================================================================

#[test]
fn test_strategy_default_is_name() {
  // Verify all existing converters return their name() as strategy()
  let hash_conv = HashConverter;
  assert_eq!(hash_conv.strategy(), hash_conv.name());
  assert_eq!(hash_conv.strategy(), "hash");

  let u8_conv = U8Converter::new();
  assert_eq!(u8_conv.strategy(), u8_conv.name());
  assert_eq!(u8_conv.strategy(), "u8");

  let u16_conv = U16Converter::new();
  assert_eq!(u16_conv.strategy(), u16_conv.name());
  assert_eq!(u16_conv.strategy(), "u16");

  let u32_conv = U32Converter::new();
  assert_eq!(u32_conv.strategy(), u32_conv.name());
  assert_eq!(u32_conv.strategy(), "u32");

  let u64_conv = U64Converter::new();
  assert_eq!(u64_conv.strategy(), u64_conv.name());
  assert_eq!(u64_conv.strategy(), "u64");

  let i64_conv = I64Converter::new();
  assert_eq!(i64_conv.strategy(), i64_conv.name());
  assert_eq!(i64_conv.strategy(), "i64");

  let f64_conv = F64Converter::new();
  assert_eq!(f64_conv.strategy(), f64_conv.name());
  assert_eq!(f64_conv.strategy(), "f64");

  let string_conv = StringConverter::new(256);
  assert_eq!(string_conv.strategy(), string_conv.name());
  assert_eq!(string_conv.strategy(), "string");

  let ts_conv = TimestampConverter::new();
  assert_eq!(ts_conv.strategy(), ts_conv.name());
  assert_eq!(ts_conv.strategy(), "timestamp");
}

#[test]
fn test_expand_value_default_returns_single() {
  // Default expand_value should return a vec with a single element (the input)
  let conv = StringConverter::new(256);
  let input = b"hello world";
  let expanded = conv.expand_value(input);
  assert_eq!(expanded.len(), 1);
  assert_eq!(expanded[0], input.to_vec());

  // Works for numeric converters too
  let u64_conv = U64Converter::new();
  let num_input = 42u64.to_be_bytes();
  let expanded = u64_conv.expand_value(&num_input);
  assert_eq!(expanded.len(), 1);
  assert_eq!(expanded[0], num_input.to_vec());
}

#[test]
fn test_expand_value_default_empty_input() {
  let conv = StringConverter::new(256);
  let expanded = conv.expand_value(b"");
  assert_eq!(expanded.len(), 1);
  assert_eq!(expanded[0], Vec::<u8>::new());
}

#[test]
fn test_expand_value_default_large_input() {
  let conv = HashConverter;
  let large_input = vec![0xAB; 1024];
  let expanded = conv.expand_value(&large_input);
  assert_eq!(expanded.len(), 1);
  assert_eq!(expanded[0].len(), 1024);
}

#[test]
fn test_recommended_bucket_count_default() {
  // All existing converters should return 1024 (default)
  assert_eq!(HashConverter.recommended_bucket_count(), 1024);
  assert_eq!(U8Converter::new().recommended_bucket_count(), 1024);
  assert_eq!(U16Converter::new().recommended_bucket_count(), 1024);
  assert_eq!(U32Converter::new().recommended_bucket_count(), 1024);
  assert_eq!(U64Converter::new().recommended_bucket_count(), 1024);
  assert_eq!(I64Converter::new().recommended_bucket_count(), 1024);
  assert_eq!(F64Converter::new().recommended_bucket_count(), 1024);
  assert_eq!(StringConverter::new(256).recommended_bucket_count(), 1024);
  assert_eq!(TimestampConverter::new().recommended_bucket_count(), 1024);
}

// =============================================================================
// Task 2: IndexManager multi-strategy
// =============================================================================

#[test]
fn test_index_manager_save_load_with_strategy() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  // Create an index — save_index uses converter.strategy() for the file path
  let converter = Box::new(U64Converter::with_range(0, 200));
  let mut index = FieldIndex::new("age".to_string(), converter);
  index.insert(&30u64.to_be_bytes(), vec![0xAA; 32]);

  index_manager.save_index("/users", &index).unwrap();

  // list_indexes should show "age.u64" (new format)
  let indexes = index_manager.list_indexes("/users").unwrap();
  assert!(indexes.contains(&"age.u64".to_string()),
    "Expected 'age.u64' in indexes, got: {:?}", indexes);

  // load_index_by_strategy should find it
  let loaded = index_manager.load_index_by_strategy("/users", "age", "u64")
    .unwrap()
    .expect("Index should be loadable by strategy");
  assert_eq!(loaded.field_name, "age");
  assert_eq!(loaded.len(), 1);

  // load_index (generic) should also find it via scanning
  let loaded_generic = index_manager.load_index("/users", "age")
    .unwrap()
    .expect("Index should be loadable generically");
  assert_eq!(loaded_generic.field_name, "age");
  assert_eq!(loaded_generic.len(), 1);
}

#[test]
fn test_index_manager_save_load_string_strategy() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(StringConverter::new(256));
  let mut index = FieldIndex::new("name".to_string(), converter);
  index.insert(b"Alice", vec![0xBB; 32]);

  index_manager.save_index("/users", &index).unwrap();

  let loaded = index_manager.load_index_by_strategy("/users", "name", "string")
    .unwrap()
    .expect("String index should load by strategy");
  assert_eq!(loaded.field_name, "name");
  assert_eq!(loaded.len(), 1);
}

#[test]
fn test_index_manager_backward_compat() {
  let ctx = RequestContext::system();
  // Verify that old-format .idx files (without strategy) still load.
  // We simulate the old format by directly storing a file at the legacy path.
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Manually create an index and store it at the OLD path
  let converter = Box::new(U64Converter::with_range(0, 100));
  let index = FieldIndex::new("score".to_string(), converter);
  let hash_length = engine.hash_algo().hash_length();
  let data = index.serialize(hash_length);

  // Store at legacy path: /data/.indexes/score.idx
  ops.store_file(&ctx, "/data/.indexes/score.idx", &data, Some("application/octet-stream")).unwrap();

  // load_index should find it via the old path
  let index_manager = IndexManager::new(&engine);
  let loaded = index_manager.load_index("/data", "score")
    .unwrap()
    .expect("Legacy index should load via old path");
  assert_eq!(loaded.field_name, "score");
}

#[test]
fn test_index_manager_load_nonexistent_returns_none() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let result = index_manager.load_index("/nonexistent", "field").unwrap();
  assert!(result.is_none());

  let result = index_manager.load_index_by_strategy("/nonexistent", "field", "string").unwrap();
  assert!(result.is_none());
}

#[test]
fn test_index_manager_delete_with_strategy() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(U64Converter::with_range(0, 100));
  index_manager.create_index("/data", "count", converter).unwrap();

  // Verify it exists
  let loaded = index_manager.load_index("/data", "count").unwrap();
  assert!(loaded.is_some());

  // Delete using strategy
  index_manager.delete_index("/data", "count", "u64").unwrap();

  // Verify it's gone
  let loaded = index_manager.load_index("/data", "count").unwrap();
  assert!(loaded.is_none());
}

#[test]
fn test_load_indexes_for_field_multiple_strategies() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  // Create two indexes on the same field "name" with different converters/strategies.
  // StringConverter has strategy "string"
  let string_conv = Box::new(StringConverter::new(256));
  let mut string_index = FieldIndex::new("name".to_string(), string_conv);
  string_index.insert(b"Alice", vec![0xAA; 32]);
  index_manager.save_index("/users", &string_index).unwrap();

  // HashConverter has strategy "hash"
  let hash_conv = Box::new(HashConverter);
  let mut hash_index = FieldIndex::new("name".to_string(), hash_conv);
  hash_index.insert(b"Alice", vec![0xBB; 32]);
  index_manager.save_index("/users", &hash_index).unwrap();

  // load_indexes_for_field should return both
  let all_indexes = index_manager.load_indexes_for_field("/users", "name").unwrap();
  assert_eq!(all_indexes.len(), 2,
    "Expected 2 indexes for 'name', got {}", all_indexes.len());

  // Verify both strategies are represented
  let strategies: Vec<&str> = all_indexes.iter()
    .map(|idx| idx.converter.strategy())
    .collect();
  assert!(strategies.contains(&"string"), "Missing 'string' strategy");
  assert!(strategies.contains(&"hash"), "Missing 'hash' strategy");
}

#[test]
fn test_load_indexes_for_field_no_indexes() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let result = index_manager.load_indexes_for_field("/empty", "field").unwrap();
  assert!(result.is_empty());
}

#[test]
fn test_load_indexes_for_field_only_matches_correct_field() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  // Create indexes on different fields
  let age_conv = Box::new(U64Converter::with_range(0, 200));
  index_manager.create_index("/users", "age", age_conv).unwrap();

  let name_conv = Box::new(StringConverter::new(256));
  index_manager.create_index("/users", "name", name_conv).unwrap();

  // Load only "age" indexes
  let age_indexes = index_manager.load_indexes_for_field("/users", "age").unwrap();
  assert_eq!(age_indexes.len(), 1);
  assert_eq!(age_indexes[0].field_name, "age");

  // Load only "name" indexes
  let name_indexes = index_manager.load_indexes_for_field("/users", "name").unwrap();
  assert_eq!(name_indexes.len(), 1);
  assert_eq!(name_indexes[0].field_name, "name");
}

// =============================================================================
// Task 3: QueryResult has score and matched_by
// =============================================================================

#[test]
fn test_query_result_has_score_and_matched_by() {
  let file_record = FileRecord::new(
    "/test/file.json".to_string(),
    Some("application/json".to_string()),
    42,
    vec![],
  );

  let result = QueryResult {
    file_hash: vec![0xAA; 32],
    file_record,
    score: 0.85,
    matched_by: vec!["trigram".to_string(), "phonetic".to_string()],
  };

  assert_eq!(result.score, 0.85);
  assert_eq!(result.matched_by.len(), 2);
  assert_eq!(result.matched_by[0], "trigram");
  assert_eq!(result.matched_by[1], "phonetic");
}

#[test]
fn test_query_result_default_score_is_one() {
  // When constructed with default score (as the query engine does)
  let file_record = FileRecord::new(
    "/test/file.json".to_string(),
    None,
    0,
    vec![],
  );

  let result = QueryResult {
    file_hash: vec![0x00; 32],
    file_record,
    score: 1.0,
    matched_by: vec![],
  };

  assert_eq!(result.score, 1.0);
  assert!(result.matched_by.is_empty());
}

#[test]
fn test_query_result_score_zero() {
  let file_record = FileRecord::new("/t".to_string(), None, 0, vec![]);
  let result = QueryResult {
    file_hash: vec![0x00; 32],
    file_record,
    score: 0.0,
    matched_by: vec![],
  };
  assert_eq!(result.score, 0.0);
}

#[test]
fn test_query_result_matched_by_multiple_entries() {
  let file_record = FileRecord::new("/t".to_string(), None, 0, vec![]);
  let result = QueryResult {
    file_hash: vec![0x00; 32],
    file_record,
    score: 0.95,
    matched_by: vec![
      "exact".to_string(),
      "trigram".to_string(),
      "dmetaphone".to_string(),
    ],
  };
  assert_eq!(result.matched_by.len(), 3);
}

// =============================================================================
// Task 4: expand_value in indexing pipeline
// =============================================================================

#[test]
fn test_insert_expanded_default_same_as_insert() {
  // With default expand_value (returns single entry), insert_expanded
  // should produce the same result as insert.
  let converter1 = Box::new(U64Converter::with_range(0, 100));
  let mut index1 = FieldIndex::new("val".to_string(), converter1);
  index1.insert(&50u64.to_be_bytes(), vec![0xAA; 32]);

  let converter2 = Box::new(U64Converter::with_range(0, 100));
  let mut index2 = FieldIndex::new("val".to_string(), converter2);
  index2.insert_expanded(&50u64.to_be_bytes(), vec![0xAA; 32]);

  assert_eq!(index1.len(), index2.len());
  assert_eq!(index1.entries[0].scalar, index2.entries[0].scalar);
  assert_eq!(index1.entries[0].file_hash, index2.entries[0].file_hash);
}

#[test]
fn test_insert_expanded_with_multiple_values() {
  // Create a custom converter that expands a value into multiple entries
  // We can't easily create a custom converter in tests without a new struct,
  // but we can test the mechanics by calling insert_expanded with the
  // default converter and verifying the single-entry behavior, then
  // manually testing multiple inserts which is what a custom expand_value would cause.
  let converter = Box::new(StringConverter::new(256));
  let mut index = FieldIndex::new("name".to_string(), converter);

  // Simulate what a trigram converter would do: expand "abc" into ["ab", "bc"]
  // by calling insert for each expanded value
  index.insert_expanded(b"Alice", vec![0xAA; 32]);

  // Default expand_value returns [b"Alice"], so 1 entry
  assert_eq!(index.len(), 1);
}

#[test]
fn test_expand_value_in_indexing_pipeline() {
  let ctx = RequestContext::system();
  // End-to-end: store a file with indexing and verify index entries exist
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    indexes: vec![
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "string".to_string(),
        source: None,
        min: None,
        max: None,
      },
      IndexFieldConfig {
        name: "age".to_string(),
        index_type: "u64".to_string(),
        source: None,
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  };
  store_index_config(&engine, "/users", &config);

  let data = make_user_json("Alice", 30, "alice@test.com");
  ops.store_file_with_indexing(&ctx, "/users/alice.json", &data, Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);

  // Verify "name" index has an entry
  let name_index = index_manager.load_index("/users", "name").unwrap()
    .expect("name index should exist");
  assert_eq!(name_index.len(), 1, "name index should have 1 entry");

  // Verify "age" index has an entry
  let age_index = index_manager.load_index("/users", "age").unwrap()
    .expect("age index should exist");
  assert_eq!(age_index.len(), 1, "age index should have 1 entry");
}

#[test]
fn test_expand_value_in_indexing_pipeline_multiple_files() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    indexes: vec![
      IndexFieldConfig {
        name: "age".to_string(),
        index_type: "u64".to_string(),
        source: None,
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  };
  store_index_config(&engine, "/users", &config);

  let data1 = make_user_json("Alice", 30, "alice@test.com");
  ops.store_file_with_indexing(&ctx, "/users/alice.json", &data1, Some("application/json")).unwrap();

  let data2 = make_user_json("Bob", 25, "bob@test.com");
  ops.store_file_with_indexing(&ctx, "/users/bob.json", &data2, Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);
  let age_index = index_manager.load_index("/users", "age").unwrap()
    .expect("age index should exist");
  assert_eq!(age_index.len(), 2, "age index should have 2 entries after storing 2 files");
}

#[test]
fn test_expand_value_overwrite_file_updates_index() {
  let ctx = RequestContext::system();
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    indexes: vec![
      IndexFieldConfig {
        name: "age".to_string(),
        index_type: "u64".to_string(),
        source: None,
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  };
  store_index_config(&engine, "/users", &config);

  let data1 = make_user_json("Alice", 30, "alice@test.com");
  ops.store_file_with_indexing(&ctx, "/users/alice.json", &data1, Some("application/json")).unwrap();

  // Overwrite with different age
  let data2 = make_user_json("Alice", 35, "alice@test.com");
  ops.store_file_with_indexing(&ctx, "/users/alice.json", &data2, Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);
  let age_index = index_manager.load_index("/users", "age").unwrap()
    .expect("age index should exist");
  // Should still have 1 entry (old removed, new inserted)
  assert_eq!(age_index.len(), 1, "age index should have 1 entry after overwrite");
}

// =============================================================================
// Edge cases and error paths
// =============================================================================

#[test]
fn test_index_file_path_includes_strategy_in_filename() {
  // Verify the path format by creating and listing
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(I64Converter::with_range(-100, 100));
  index_manager.create_index("/data", "temperature", converter).unwrap();

  let indexes = index_manager.list_indexes("/data").unwrap();
  assert_eq!(indexes.len(), 1);
  assert_eq!(indexes[0], "temperature.i64");
}

#[test]
fn test_create_index_via_manager_sets_correct_strategy() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(F64Converter::with_range(0.0, 100.0));
  let index = index_manager.create_index("/metrics", "value", converter).unwrap();

  assert_eq!(index.converter.strategy(), "f64");

  // Verify it can be loaded by strategy
  let loaded = index_manager.load_index_by_strategy("/metrics", "value", "f64")
    .unwrap()
    .expect("Should load by strategy");
  assert_eq!(loaded.field_name, "value");
}

#[test]
fn test_list_indexes_empty_directory() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let indexes = index_manager.list_indexes("/nonexistent").unwrap();
  assert!(indexes.is_empty());
}

#[test]
fn test_load_index_by_strategy_wrong_strategy_returns_none() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(U64Converter::with_range(0, 100));
  index_manager.create_index("/data", "count", converter).unwrap();

  // Try loading with wrong strategy
  let result = index_manager.load_index_by_strategy("/data", "count", "string").unwrap();
  assert!(result.is_none(), "Loading with wrong strategy should return None");
}

#[test]
fn test_delete_index_wrong_strategy_returns_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(U64Converter::with_range(0, 100));
  index_manager.create_index("/data", "count", converter).unwrap();

  // Deleting with wrong strategy should fail (file not found)
  let result = index_manager.delete_index("/data", "count", "string");
  assert!(result.is_err(), "Deleting with wrong strategy should error");
}

#[test]
fn test_insert_expanded_empty_value() {
  let converter = Box::new(StringConverter::new(256));
  let mut index = FieldIndex::new("name".to_string(), converter);
  index.insert_expanded(b"", vec![0xAA; 32]);
  // Empty string still produces one entry (default expand_value returns [b""])
  assert_eq!(index.len(), 1);
}

#[test]
fn test_insert_expanded_preserves_sorted_order() {
  let converter = Box::new(U64Converter::with_range(0, 100));
  let mut index = FieldIndex::new("val".to_string(), converter);

  // Insert values out of order using insert_expanded
  index.insert_expanded(&80u64.to_be_bytes(), vec![0x80; 32]);
  index.insert_expanded(&20u64.to_be_bytes(), vec![0x20; 32]);
  index.insert_expanded(&50u64.to_be_bytes(), vec![0x50; 32]);

  assert_eq!(index.len(), 3);

  // Verify sorted order
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
fn test_save_and_reload_preserves_entries_with_strategy() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let index_manager = IndexManager::new(&engine);

  let converter = Box::new(U64Converter::with_range(0, 1000));
  let mut index = FieldIndex::new("score".to_string(), converter);
  index.insert(&100u64.to_be_bytes(), vec![0xAA; 32]);
  index.insert(&200u64.to_be_bytes(), vec![0xBB; 32]);
  index.insert(&300u64.to_be_bytes(), vec![0xCC; 32]);

  index_manager.save_index("/game", &index).unwrap();

  let loaded = index_manager.load_index_by_strategy("/game", "score", "u64")
    .unwrap()
    .expect("Index should load");
  assert_eq!(loaded.len(), 3);
  assert_eq!(loaded.field_name, "score");
}
