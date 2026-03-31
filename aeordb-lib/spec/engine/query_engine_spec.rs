use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::query_engine::{QueryBuilder, QueryEngine, Query, FieldQuery, QueryOp};
use aeordb::engine::storage_engine::StorageEngine;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let path = dir.path().join("test.aeor");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory().unwrap();
  engine
}

fn make_user_json(name: &str, age: u64, email: &str) -> Vec<u8> {
  format!(
    r#"{{"name":"{}","age":{},"email":"{}"}}"#,
    name, age, email,
  ).into_bytes()
}

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

/// Set up an engine with users indexed by age and name.
fn setup_users_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let engine = create_engine(dir);
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

  ops.store_file_with_indexing(
    "/users/diana.json",
    &make_user_json("Diana", 35, "diana@test.com"),
    Some("application/json"),
  ).unwrap();

  engine
}

#[test]
fn test_query_exact_match() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").eq(&30u64.to_be_bytes())
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/users/alice.json");
}

#[test]
fn test_query_range_gt() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").gt(&30u64.to_be_bytes())
    .all()
    .unwrap();

  // Charlie (40) and Diana (35) are > 30
  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/charlie.json"));
  assert!(paths.contains(&"/users/diana.json"));
}

#[test]
fn test_query_range_lt() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").lt(&30u64.to_be_bytes())
    .all()
    .unwrap();

  // Bob (25) is < 30
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/users/bob.json");
}

#[test]
fn test_query_range_between() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").between(&28u64.to_be_bytes(), &36u64.to_be_bytes())
    .all()
    .unwrap();

  // Alice (30) and Diana (35) are in [28, 36]
  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/alice.json"));
  assert!(paths.contains(&"/users/diana.json"));
}

#[test]
fn test_query_multiple_fields_intersection() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // Query: age > 25 AND name matches "Alice"
  // The name index uses StringConverter which is not order-preserving,
  // so we use exact match on name
  let results = QueryBuilder::new(&engine, "/users")
    .field("age").gt(&25u64.to_be_bytes())
    .field("name").eq(b"Alice")
    .all()
    .unwrap();

  // Alice is age 30 (>25) and name matches "Alice"
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/users/alice.json");
}

#[test]
fn test_query_with_limit() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").gt(&0u64.to_be_bytes())
    .limit(2)
    .all()
    .unwrap();

  // All 4 users are > 0, but limit to 2
  assert!(results.len() <= 2);
}

#[test]
fn test_query_first() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let result = QueryBuilder::new(&engine, "/users")
    .field("age").gt(&0u64.to_be_bytes())
    .first()
    .unwrap();

  assert!(result.is_some());
}

#[test]
fn test_query_count() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let count = QueryBuilder::new(&engine, "/users")
    .field("age").gt(&0u64.to_be_bytes())
    .count()
    .unwrap();

  assert_eq!(count, 4);
}

#[test]
fn test_query_empty_results() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").gt(&100u64.to_be_bytes())
    .all()
    .unwrap();

  assert!(results.is_empty());
}

#[test]
fn test_query_builder_chainable() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // Build a complex query and verify it compiles and runs
  let results = QueryBuilder::new(&engine, "/users")
    .field("age").between(&20u64.to_be_bytes(), &40u64.to_be_bytes())
    .limit(10)
    .all()
    .unwrap();

  // All 4 users are in [20, 40]
  assert_eq!(results.len(), 4);
}

// --- Additional edge case / failure tests ---

#[test]
fn test_query_nonexistent_index_returns_error() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let result = QueryBuilder::new(&engine, "/nonexistent")
    .field("age").eq(&30u64.to_be_bytes())
    .all();

  assert!(result.is_err());
}

#[test]
fn test_query_no_field_queries_returns_empty() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  let query = Query {
    path: "/users".to_string(),
    field_queries: Vec::new(),
    limit: None,
  };

  let query_engine = QueryEngine::new(&engine);
  let results = query_engine.execute(&query).unwrap();
  assert!(results.is_empty());
}

#[test]
fn test_query_first_returns_none_when_empty() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let result = QueryBuilder::new(&engine, "/users")
    .field("age").gt(&999u64.to_be_bytes())
    .first()
    .unwrap();

  assert!(result.is_none());
}

#[test]
fn test_query_count_returns_zero_when_empty() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let count = QueryBuilder::new(&engine, "/users")
    .field("age").gt(&999u64.to_be_bytes())
    .count()
    .unwrap();

  assert_eq!(count, 0);
}

#[test]
fn test_query_after_delete() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Delete Alice
  ops.delete_file_with_indexing("/users/alice.json").unwrap();

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").eq(&30u64.to_be_bytes())
    .all()
    .unwrap();

  // Alice was the only one with age 30, now deleted
  assert!(results.is_empty());
}

#[test]
fn test_query_with_overwritten_file() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);
  let ops = DirectoryOps::new(&engine);

  // Update Alice's age from 30 to 50
  ops.store_file_with_indexing(
    "/users/alice.json",
    &make_user_json("Alice", 50, "alice@test.com"),
    Some("application/json"),
  ).unwrap();

  // Old age should not match
  let results = QueryBuilder::new(&engine, "/users")
    .field("age").eq(&30u64.to_be_bytes())
    .all()
    .unwrap();
  assert!(results.is_empty());

  // New age should match
  let results = QueryBuilder::new(&engine, "/users")
    .field("age").eq(&50u64.to_be_bytes())
    .all()
    .unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/users/alice.json");
}

#[test]
fn test_query_via_raw_query_struct() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let query = Query {
    path: "/users".to_string(),
    field_queries: vec![
      FieldQuery {
        field_name: "age".to_string(),
        operation: QueryOp::Gt(25u64.to_be_bytes().to_vec()),
      },
    ],
    limit: Some(2),
  };

  let query_engine = QueryEngine::new(&engine);
  let results = query_engine.execute(&query).unwrap();

  assert!(results.len() <= 2);
}
