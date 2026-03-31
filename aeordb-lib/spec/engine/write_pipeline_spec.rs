use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::index_store::IndexManager;
use aeordb::engine::json_parser::parse_json_fields;
use aeordb::engine::storage_engine::StorageEngine;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory().unwrap();
  engine
}

/// Store an index config at the given parent path.
fn store_index_config(engine: &StorageEngine, parent_path: &str, config: &PathIndexConfig) {
  let ops = DirectoryOps::new(engine);
  let config_path = if parent_path.ends_with('/') {
    format!("{}.config/indexes.json", parent_path)
  } else {
    format!("{}/.config/indexes.json", parent_path)
  };
  let config_data = config.serialize();
  ops.store_file(&config_path, &config_data, Some("application/json")).unwrap();
}

fn make_user_json(name: &str, age: u64, email: &str) -> Vec<u8> {
  format!(
    r#"{{"name":"{}","age":{},"email":"{}"}}"#,
    name, age, email,
  ).into_bytes()
}

#[test]
fn test_store_file_indexes_fields() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Set up index config at /users/
  let config = PathIndexConfig {
    indexes: vec![
      IndexFieldConfig {
        field_name: "age".to_string(),
        converter_type: "u64".to_string(),
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  };
  store_index_config(&engine, "/users", &config);

  // Store a user file with indexing
  let data = make_user_json("Alice", 30, "alice@test.com");
  ops.store_file_with_indexing("/users/alice.json", &data, Some("application/json")).unwrap();

  // Verify the index was created and has an entry
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/users", "age").unwrap().unwrap();
  assert_eq!(index.len(), 1);
}

#[test]
fn test_store_file_no_config_no_indexing() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Store a file without any config — should work fine, no indexing
  let data = make_user_json("Bob", 25, "bob@test.com");
  ops.store_file_with_indexing("/data/bob.json", &data, Some("application/json")).unwrap();

  // No indexes should exist
  let index_manager = IndexManager::new(&engine);
  let indexes = index_manager.list_indexes("/data").unwrap();
  assert!(indexes.is_empty());
}

#[test]
fn test_delete_file_removes_index_entries() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    indexes: vec![
      IndexFieldConfig {
        field_name: "age".to_string(),
        converter_type: "u64".to_string(),
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  };
  store_index_config(&engine, "/users", &config);

  let data = make_user_json("Alice", 30, "alice@test.com");
  ops.store_file_with_indexing("/users/alice.json", &data, Some("application/json")).unwrap();

  // Verify entry exists
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/users", "age").unwrap().unwrap();
  assert_eq!(index.len(), 1);

  // Delete with indexing
  ops.delete_file_with_indexing("/users/alice.json").unwrap();

  // Verify entry is gone
  let index = index_manager.load_index("/users", "age").unwrap().unwrap();
  assert_eq!(index.len(), 0);
}

#[test]
fn test_overwrite_file_updates_index() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    indexes: vec![
      IndexFieldConfig {
        field_name: "age".to_string(),
        converter_type: "u64".to_string(),
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  };
  store_index_config(&engine, "/users", &config);

  // Store initial version
  let data = make_user_json("Alice", 30, "alice@test.com");
  ops.store_file_with_indexing("/users/alice.json", &data, Some("application/json")).unwrap();

  // Overwrite with new age
  let data = make_user_json("Alice", 35, "alice@test.com");
  ops.store_file_with_indexing("/users/alice.json", &data, Some("application/json")).unwrap();

  // Should still have exactly 1 entry (not 2)
  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/users", "age").unwrap().unwrap();
  assert_eq!(index.len(), 1);
}

#[test]
fn test_multiple_indexed_fields() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    indexes: vec![
      IndexFieldConfig {
        field_name: "age".to_string(),
        converter_type: "u64".to_string(),
        min: Some(0.0),
        max: Some(200.0),
      },
      IndexFieldConfig {
        field_name: "name".to_string(),
        converter_type: "string".to_string(),
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/users", &config);

  let data = make_user_json("Alice", 30, "alice@test.com");
  ops.store_file_with_indexing("/users/alice.json", &data, Some("application/json")).unwrap();

  let index_manager = IndexManager::new(&engine);

  let age_index = index_manager.load_index("/users", "age").unwrap().unwrap();
  assert_eq!(age_index.len(), 1);

  let name_index = index_manager.load_index("/users", "name").unwrap().unwrap();
  assert_eq!(name_index.len(), 1);
}

#[test]
fn test_json_parser_extracts_fields() {
  let json = br#"{"name":"Alice","age":30,"email":"alice@test.com"}"#;
  let fields = parse_json_fields(json, &["name", "age", "email"]).unwrap();

  assert_eq!(fields.len(), 3);

  let name = fields.iter().find(|(name, _)| name == "name").unwrap();
  assert_eq!(name.1, b"Alice");

  let age = fields.iter().find(|(name, _)| name == "age").unwrap();
  assert_eq!(age.1, 30u64.to_be_bytes().to_vec());

  let email = fields.iter().find(|(name, _)| name == "email").unwrap();
  assert_eq!(email.1, b"alice@test.com");
}

#[test]
fn test_json_parser_missing_field_skipped() {
  let json = br#"{"name":"Alice","age":30}"#;
  let fields = parse_json_fields(json, &["name", "email", "nonexistent"]).unwrap();

  assert_eq!(fields.len(), 1); // only "name" found
  assert_eq!(fields[0].0, "name");
}

// --- Additional edge case / failure tests ---

#[test]
fn test_json_parser_invalid_utf8() {
  let bad_data = vec![0xFF, 0xFE, 0xFD];
  let result = parse_json_fields(&bad_data, &["name"]);
  assert!(result.is_err());
}

#[test]
fn test_json_parser_invalid_json() {
  let bad_json = b"not json at all";
  let result = parse_json_fields(bad_json, &["name"]);
  assert!(result.is_err());
}

#[test]
fn test_json_parser_non_object_root() {
  let json = b"[1, 2, 3]";
  let result = parse_json_fields(json, &["name"]);
  assert!(result.is_err());
}

#[test]
fn test_json_parser_empty_field_list() {
  let json = br#"{"name":"Alice"}"#;
  let fields = parse_json_fields(json, &[]).unwrap();
  assert!(fields.is_empty());
}

#[test]
fn test_json_parser_boolean_value() {
  let json = br#"{"active":true}"#;
  let fields = parse_json_fields(json, &["active"]).unwrap();
  assert_eq!(fields.len(), 1);
  assert_eq!(fields[0].1, vec![1u8]);
}

#[test]
fn test_json_parser_null_value() {
  let json = br#"{"name":null}"#;
  let fields = parse_json_fields(json, &["name"]).unwrap();
  assert_eq!(fields.len(), 1);
  assert!(fields[0].1.is_empty());
}

#[test]
fn test_json_parser_float_value() {
  let json = br#"{"score":3.14}"#;
  let fields = parse_json_fields(json, &["score"]).unwrap();
  assert_eq!(fields.len(), 1);
  assert_eq!(fields[0].1, 3.14f64.to_be_bytes().to_vec());
}

#[test]
fn test_json_parser_negative_integer() {
  let json = br#"{"temperature":-10}"#;
  let fields = parse_json_fields(json, &["temperature"]).unwrap();
  assert_eq!(fields.len(), 1);
  assert_eq!(fields[0].1, (-10i64).to_be_bytes().to_vec());
}

#[test]
fn test_index_config_serialize_deserialize_roundtrip() {
  let config = PathIndexConfig {
    indexes: vec![
      IndexFieldConfig {
        field_name: "age".to_string(),
        converter_type: "u64".to_string(),
        min: Some(0.0),
        max: Some(200.0),
      },
      IndexFieldConfig {
        field_name: "name".to_string(),
        converter_type: "string".to_string(),
        min: None,
        max: None,
      },
    ],
  };

  let serialized = config.serialize();
  let deserialized = PathIndexConfig::deserialize(&serialized).unwrap();

  assert_eq!(deserialized.indexes.len(), 2);
  assert_eq!(deserialized.indexes[0].field_name, "age");
  assert_eq!(deserialized.indexes[0].converter_type, "u64");
  assert_eq!(deserialized.indexes[0].min, Some(0.0));
  assert_eq!(deserialized.indexes[0].max, Some(200.0));
  assert_eq!(deserialized.indexes[1].field_name, "name");
  assert_eq!(deserialized.indexes[1].converter_type, "string");
  assert!(deserialized.indexes[1].min.is_none());
  assert!(deserialized.indexes[1].max.is_none());
}

#[test]
fn test_index_config_deserialize_invalid_json() {
  let result = PathIndexConfig::deserialize(b"not json");
  assert!(result.is_err());
}

#[test]
fn test_index_config_deserialize_missing_indexes_key() {
  let result = PathIndexConfig::deserialize(br#"{"foo":"bar"}"#);
  assert!(result.is_err());
}

#[test]
fn test_store_non_json_data_with_config_does_not_crash() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    indexes: vec![
      IndexFieldConfig {
        field_name: "age".to_string(),
        converter_type: "u64".to_string(),
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  };
  store_index_config(&engine, "/data", &config);

  // Store binary (non-JSON) data — indexing should be silently skipped
  let data = vec![0xFF, 0xFE, 0xFD, 0x00];
  ops.store_file_with_indexing("/data/binary.dat", &data, Some("application/octet-stream")).unwrap();

  // No index entries should exist
  let index_manager = IndexManager::new(&engine);
  let result = index_manager.load_index("/data", "age").unwrap();
  // Index may not exist or may be empty
  match result {
    Some(index) => assert_eq!(index.len(), 0),
    None => {} // also fine
  }
}

#[test]
fn test_multiple_files_indexed_together() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  let config = PathIndexConfig {
    indexes: vec![
      IndexFieldConfig {
        field_name: "age".to_string(),
        converter_type: "u64".to_string(),
        min: Some(0.0),
        max: Some(200.0),
      },
    ],
  };
  store_index_config(&engine, "/users", &config);

  ops.store_file_with_indexing(
    "/users/alice.json",
    &make_user_json("Alice", 30, "alice@test.com"),
    Some("application/json"),
  ).unwrap();

  ops.store_file_with_indexing(
    "/users/bob.json",
    &make_user_json("Bob", 25, "bob@test.com"),
    Some("application/json"),
  ).unwrap();

  ops.store_file_with_indexing(
    "/users/charlie.json",
    &make_user_json("Charlie", 40, "charlie@test.com"),
    Some("application/json"),
  ).unwrap();

  let index_manager = IndexManager::new(&engine);
  let index = index_manager.load_index("/users", "age").unwrap().unwrap();
  assert_eq!(index.len(), 3);
}
