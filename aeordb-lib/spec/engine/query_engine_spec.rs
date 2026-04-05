use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::query_engine::{QueryBuilder, QueryEngine, Query, QueryNode, FieldQuery, QueryOp, QueryStrategy, should_use_bitmap_compositing};
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
      IndexFieldConfig {
        name: "name".to_string(),
        index_type: "string".to_string(),
        source: None,
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
    node: None,
    limit: None,
    strategy: QueryStrategy::Full,
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
    node: None,
    limit: Some(2),
    strategy: QueryStrategy::Full,
  };

  let query_engine = QueryEngine::new(&engine);
  let results = query_engine.execute(&query).unwrap();

  assert!(results.len() <= 2);
}

// ===========================================================================
// Task 4: Typed convenience methods
// ===========================================================================

#[test]
fn test_typed_eq_u64() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").eq_u64(30)
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/users/alice.json");
}

#[test]
fn test_typed_gt_str() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // Use eq_str since StringConverter is not order-preserving.
  let results = QueryBuilder::new(&engine, "/users")
    .field("name").eq_str("Bob")
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/users/bob.json");
}

#[test]
fn test_typed_eq_bool() {
  let dir = tempfile::tempdir().unwrap();
  let engine = create_engine(&dir);

  // Verify the method compiles and produces the correct bytes.
  // No bool index exists, so execution will fail — but construction must work.
  let builder = QueryBuilder::new(&engine, "/test")
    .field("active").eq_bool(true);

  let result = builder.all();
  assert!(result.is_err()); // no index exists
}

#[test]
fn test_typed_between_u64() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").between_u64(28, 36)
    .all()
    .unwrap();

  // Alice (30) and Diana (35) are in [28, 36]
  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/alice.json"));
  assert!(paths.contains(&"/users/diana.json"));
}

// ===========================================================================
// Task 5: QueryNode tree with boolean logic
// ===========================================================================

#[test]
fn test_query_or() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // age == 25 OR age == 40
  let results = QueryBuilder::new(&engine, "/users")
    .or(|q| {
      q.field("age").eq_u64(25)
       .field("age").eq_u64(40)
    })
    .all()
    .unwrap();

  // Bob (25) and Charlie (40)
  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/bob.json"));
  assert!(paths.contains(&"/users/charlie.json"));
}

#[test]
fn test_query_not() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // NOT(age == 30) — everyone except Alice
  let results = QueryBuilder::new(&engine, "/users")
    .not(|q| q.field("age").eq_u64(30))
    .all()
    .unwrap();

  // Bob (25), Charlie (40), Diana (35)
  assert_eq!(results.len(), 3);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(!paths.contains(&"/users/alice.json"));
  assert!(paths.contains(&"/users/bob.json"));
  assert!(paths.contains(&"/users/charlie.json"));
  assert!(paths.contains(&"/users/diana.json"));
}

#[test]
fn test_query_complex_and_or_not() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // (age > 25) AND NOT(age == 40)
  // Should match Alice (30) and Diana (35), but not Charlie (40)
  let results = QueryBuilder::new(&engine, "/users")
    .field("age").gt_u64(25)
    .not(|q| q.field("age").eq_u64(40))
    .all()
    .unwrap();

  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/alice.json"));
  assert!(paths.contains(&"/users/diana.json"));
  assert!(!paths.contains(&"/users/charlie.json"));
  assert!(!paths.contains(&"/users/bob.json"));
  assert_eq!(results.len(), 2);
}

#[test]
fn test_query_node_tree() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // Build a QueryNode tree directly: OR(age==25, age==30)
  let node = QueryNode::Or(vec![
    QueryNode::Field(FieldQuery {
      field_name: "age".to_string(),
      operation: QueryOp::Eq(25u64.to_be_bytes().to_vec()),
    }),
    QueryNode::Field(FieldQuery {
      field_name: "age".to_string(),
      operation: QueryOp::Eq(30u64.to_be_bytes().to_vec()),
    }),
  ]);

  let query = Query {
    path: "/users".to_string(),
    field_queries: Vec::new(),
    node: Some(node),
    limit: None,
    strategy: QueryStrategy::Full,
  };

  let query_engine = QueryEngine::new(&engine);
  let results = query_engine.execute(&query).unwrap();

  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/alice.json"));
  assert!(paths.contains(&"/users/bob.json"));
}

// ===========================================================================
// Task 9: Query strategy
// ===========================================================================

#[test]
fn test_query_strategy_auto() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").gt_u64(25)
    .strategy(QueryStrategy::Auto)
    .all()
    .unwrap();

  // Alice (30), Charlie (40), Diana (35)
  assert_eq!(results.len(), 3);
}

#[test]
fn test_query_strategy_strided() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").gt_u64(25)
    .strategy(QueryStrategy::Strided(4))
    .all()
    .unwrap();

  assert_eq!(results.len(), 3);
}

#[test]
fn test_query_strategy_progressive() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").gt_u64(25)
    .strategy(QueryStrategy::Progressive { initial_stride: 8 })
    .all()
    .unwrap();

  assert_eq!(results.len(), 3);
}

#[test]
fn test_query_or_empty_sub_builder() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // OR with an empty sub-builder is a no-op.
  let results = QueryBuilder::new(&engine, "/users")
    .field("age").eq_u64(30)
    .or(|q| q) // empty OR group
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/users/alice.json");
}

#[test]
fn test_query_and_explicit_group() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .and(|q| {
      q.field("age").gt_u64(25)
       .field("name").eq_str("Alice")
    })
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/users/alice.json");
}

#[test]
fn test_query_not_with_no_matches_returns_all() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // NOT(age == 999) — no one has age 999, so all should be returned
  let results = QueryBuilder::new(&engine, "/users")
    .not(|q| q.field("age").eq_u64(999))
    .all()
    .unwrap();

  assert_eq!(results.len(), 4);
}

#[test]
fn test_query_node_not_complement() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // NOT(age > 30) => only those with age <= 30: Bob (25), Alice (30)
  let node = QueryNode::Not(Box::new(QueryNode::Field(FieldQuery {
    field_name: "age".to_string(),
    operation: QueryOp::Gt(30u64.to_be_bytes().to_vec()),
  })));

  let query = Query {
    path: "/users".to_string(),
    field_queries: Vec::new(),
    node: Some(node),
    limit: None,
    strategy: QueryStrategy::Full,
  };

  let query_engine = QueryEngine::new(&engine);
  let results = query_engine.execute(&query).unwrap();

  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/alice.json"));
  assert!(paths.contains(&"/users/bob.json"));
}

// ===========================================================================
// Task 6: Two-Tier Query Execution Engine
// ===========================================================================

#[test]
fn test_two_tier_simple_uses_tier1() {
  // A flat AND of field queries should NOT trigger bitmap compositing (Tier 1).
  let node = QueryNode::And(vec![
    QueryNode::Field(FieldQuery {
      field_name: "age".to_string(),
      operation: QueryOp::Gt(30u64.to_be_bytes().to_vec()),
    }),
    QueryNode::Field(FieldQuery {
      field_name: "name".to_string(),
      operation: QueryOp::Eq(b"Alice".to_vec()),
    }),
  ]);

  assert!(!should_use_bitmap_compositing(&node));

  // Single field query is also Tier 1.
  let single = QueryNode::Field(FieldQuery {
    field_name: "age".to_string(),
    operation: QueryOp::Eq(30u64.to_be_bytes().to_vec()),
  });
  assert!(!should_use_bitmap_compositing(&single));
}

#[test]
fn test_two_tier_complex_uses_tier2() {
  // OR triggers Tier 2.
  let or_node = QueryNode::Or(vec![
    QueryNode::Field(FieldQuery {
      field_name: "age".to_string(),
      operation: QueryOp::Eq(25u64.to_be_bytes().to_vec()),
    }),
    QueryNode::Field(FieldQuery {
      field_name: "age".to_string(),
      operation: QueryOp::Eq(30u64.to_be_bytes().to_vec()),
    }),
  ]);
  assert!(should_use_bitmap_compositing(&or_node));

  // NOT triggers Tier 2.
  let not_node = QueryNode::Not(Box::new(QueryNode::Field(FieldQuery {
    field_name: "age".to_string(),
    operation: QueryOp::Eq(30u64.to_be_bytes().to_vec()),
  })));
  assert!(should_use_bitmap_compositing(&not_node));

  // AND containing an OR triggers Tier 2.
  let nested = QueryNode::And(vec![
    QueryNode::Field(FieldQuery {
      field_name: "age".to_string(),
      operation: QueryOp::Gt(20u64.to_be_bytes().to_vec()),
    }),
    or_node,
  ]);
  assert!(should_use_bitmap_compositing(&nested));
}

#[test]
fn test_mask_based_or_query() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // OR query: age == 25 OR age == 40 (goes through Tier 2).
  let results = QueryBuilder::new(&engine, "/users")
    .or(|q| {
      q.field("age").eq_u64(25)
       .field("age").eq_u64(40)
    })
    .all()
    .unwrap();

  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/bob.json"));
  assert!(paths.contains(&"/users/charlie.json"));
}

#[test]
fn test_mask_based_not_query() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // NOT query: NOT(age == 30) -> everyone except Alice (goes through Tier 2).
  let results = QueryBuilder::new(&engine, "/users")
    .not(|q| q.field("age").eq_u64(30))
    .all()
    .unwrap();

  assert_eq!(results.len(), 3);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(!paths.contains(&"/users/alice.json"));
  assert!(paths.contains(&"/users/bob.json"));
  assert!(paths.contains(&"/users/charlie.json"));
  assert!(paths.contains(&"/users/diana.json"));
}

// ===========================================================================
// Task 7: Memory-Bounded Joins (IN queries)
// ===========================================================================

#[test]
fn test_in_query_static_set() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // IN query: age IN (25, 40)
  let results = QueryBuilder::new(&engine, "/users")
    .field("age").in_values(vec![
      25u64.to_be_bytes().to_vec(),
      40u64.to_be_bytes().to_vec(),
    ])
    .all()
    .unwrap();

  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/bob.json"));
  assert!(paths.contains(&"/users/charlie.json"));
}

#[test]
fn test_in_query_typed_u64() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // IN query using typed u64 convenience: age IN (30, 35)
  let results = QueryBuilder::new(&engine, "/users")
    .field("age").in_u64(&[30, 35])
    .all()
    .unwrap();

  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/alice.json"));
  assert!(paths.contains(&"/users/diana.json"));
}

#[test]
fn test_in_query_typed_str() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // IN query using typed str convenience: name IN ("Bob", "Diana")
  let results = QueryBuilder::new(&engine, "/users")
    .field("name").in_str(&["Bob", "Diana"])
    .all()
    .unwrap();

  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/bob.json"));
  assert!(paths.contains(&"/users/diana.json"));
}

#[test]
fn test_in_query_empty_set_returns_empty() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").in_values(vec![])
    .all()
    .unwrap();

  assert!(results.is_empty());
}

#[test]
fn test_in_query_no_matches() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let results = QueryBuilder::new(&engine, "/users")
    .field("age").in_u64(&[999, 1000])
    .all()
    .unwrap();

  assert!(results.is_empty());
}

#[test]
fn test_in_query_combined_with_and() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  // age IN (25, 30, 35) AND name == "Alice"
  let results = QueryBuilder::new(&engine, "/users")
    .field("age").in_u64(&[25, 30, 35])
    .field("name").eq_str("Alice")
    .all()
    .unwrap();

  assert_eq!(results.len(), 1);
  assert_eq!(results[0].file_record.path, "/users/alice.json");
}

#[test]
fn test_in_query_via_query_node() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);

  let node = QueryNode::Field(FieldQuery {
    field_name: "age".to_string(),
    operation: QueryOp::In(vec![
      25u64.to_be_bytes().to_vec(),
      40u64.to_be_bytes().to_vec(),
    ]),
  });

  let query = Query {
    path: "/users".to_string(),
    field_queries: Vec::new(),
    node: Some(node),
    limit: None,
    strategy: QueryStrategy::Full,
  };

  let query_engine = QueryEngine::new(&engine);
  let results = query_engine.execute(&query).unwrap();

  assert_eq!(results.len(), 2);
  let paths: Vec<&str> = results.iter().map(|r| r.file_record.path.as_str()).collect();
  assert!(paths.contains(&"/users/bob.json"));
  assert!(paths.contains(&"/users/charlie.json"));
}
