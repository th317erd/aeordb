// Audit tests for ALL query operators with u64 indexes.
//
// Verifies that Eq, Gt, Lt, Between, and In all return correct results
// when u64 indexes use the default [0, u64::MAX] range, which causes
// small values to map to nearly identical f64 scalars. The fix uses raw
// byte comparison (big-endian bytes preserve u64 ordering) instead of
// relying on f64 scalar precision.
//
// Contains/Similar/Phonetic/Fuzzy/Match use trigram/phonetic indexes
// (not scalar/u64 indexes) and do not suffer from the f64 precision issue.

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::query_engine::QueryBuilder;
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let path = dir.path().join("test.aeordb");
  let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
  let ops = DirectoryOps::new(&engine);
  ops.ensure_root_directory(&ctx).unwrap();
  engine
}

/// Set up an engine with 50 JSON files at /items/, each with {"value": i*10}
/// for i in 1..=50 (values: 10, 20, 30, ..., 500).
/// Uses u64 index with DEFAULT range (no min/max) — this is the buggy case
/// where small values all map to nearly identical f64 scalars.
fn setup_u64_default_range_engine(dir: &tempfile::TempDir) -> StorageEngine {
  let ctx = RequestContext::system();
  let engine = create_engine(dir);
  let ops = DirectoryOps::new(&engine);

  // u64 index with NO min/max — defaults to [0, u64::MAX]
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,

    indexes: vec![
      IndexFieldConfig {
        name: "value".to_string(),
        index_type: "u64".to_string(),
        source: None,
        min: None,
        max: None,
      },
    ],
  };
  store_index_config(&engine, "/items", &config);

  for i in 1..=50 {
    let value = i * 10;
    let json = format!(r#"{{"value":{}}}"#, value);
    let path = format!("/items/item_{:03}.json", i);
    ops.store_file_with_indexing(&ctx, &path, json.as_bytes(), Some("application/json"))
      .unwrap();
  }

  engine
}

fn store_index_config(engine: &StorageEngine, parent_path: &str, config: &PathIndexConfig) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  let config_path = if parent_path.ends_with('/') {
    format!("{}.config/indexes.json", parent_path)
  } else {
    format!("{}/.aeordb-config/indexes.json", parent_path)
  };
  let config_data = config.serialize();
  ops.store_file(&ctx, &config_path, &config_data, Some("application/json")).unwrap();
}

// ============================================================================
// Test 1: Eq on u64 with default range
// ============================================================================

#[test]
fn test_u64_eq() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").eq(&100u64.to_be_bytes())
    .all()
    .unwrap();

  assert_eq!(results.len(), 1, "Eq(100) should return exactly 1 result, got {}", results.len());
}

// ============================================================================
// Test 2: Gt on u64 with default range
// ============================================================================

#[test]
fn test_u64_gt() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").gt(&400u64.to_be_bytes())
    .all()
    .unwrap();

  // Values > 400: 410, 420, 430, 440, 450, 460, 470, 480, 490, 500 = 10 items
  assert_eq!(results.len(), 10, "Gt(400) should return 10 results, got {}", results.len());
}

// ============================================================================
// Test 3: Lt on u64 with default range
// ============================================================================

#[test]
fn test_u64_lt() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").lt(&100u64.to_be_bytes())
    .all()
    .unwrap();

  // Values < 100: 10, 20, 30, 40, 50, 60, 70, 80, 90 = 9 items
  assert_eq!(results.len(), 9, "Lt(100) should return 9 results, got {}", results.len());
}

// ============================================================================
// Test 4: Between on u64 with default range
// ============================================================================

#[test]
fn test_u64_between() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").between(&100u64.to_be_bytes(), &200u64.to_be_bytes())
    .all()
    .unwrap();

  // Values in [100, 200]: 100, 110, 120, 130, 140, 150, 160, 170, 180, 190, 200 = 11 items
  assert_eq!(results.len(), 11, "Between(100, 200) should return 11 results, got {}", results.len());
}

// ============================================================================
// Test 5: In on u64 with default range
// ============================================================================

#[test]
fn test_u64_in() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let values: Vec<Vec<u8>> = vec![
    100u64.to_be_bytes().to_vec(),
    200u64.to_be_bytes().to_vec(),
    300u64.to_be_bytes().to_vec(),
  ];
  let results = QueryBuilder::new(&engine, "/items")
    .field("value").in_values(values)
    .all()
    .unwrap();

  assert_eq!(results.len(), 3, "In([100, 200, 300]) should return 3 results, got {}", results.len());
}

// ============================================================================
// Test 6: Eq(0) edge case — value not in dataset
// ============================================================================

#[test]
fn test_u64_eq_zero() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").eq(&0u64.to_be_bytes())
    .all()
    .unwrap();

  // 0 is not in the dataset (values start at 10)
  assert_eq!(results.len(), 0, "Eq(0) should return 0 results (not in dataset), got {}", results.len());
}

// ============================================================================
// Test 7: Eq on a larger value
// ============================================================================

#[test]
fn test_u64_eq_large_value() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").eq(&490u64.to_be_bytes())
    .all()
    .unwrap();

  assert_eq!(results.len(), 1, "Eq(490) should return exactly 1 result, got {}", results.len());
}

// ============================================================================
// Test 8: Between wide range covering all items
// ============================================================================

#[test]
fn test_u64_between_wide_range() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").between(&0u64.to_be_bytes(), &500u64.to_be_bytes())
    .limit(100)
    .all()
    .unwrap();

  // All 50 items have values in [10, 500], all within [0, 500]
  assert_eq!(results.len(), 50, "Between(0, 500) should return all 50 results, got {}", results.len());
}

// ============================================================================
// Test 9: Gt(0) should return all items (all values > 0)
// ============================================================================

#[test]
fn test_u64_gt_zero() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").gt(&0u64.to_be_bytes())
    .limit(100)
    .all()
    .unwrap();

  assert_eq!(results.len(), 50, "Gt(0) should return all 50 results, got {}", results.len());
}

// ============================================================================
// Test 10: Lt(501) should return all items (all values < 501)
// ============================================================================

#[test]
fn test_u64_lt_above_max() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").lt(&501u64.to_be_bytes())
    .limit(100)
    .all()
    .unwrap();

  assert_eq!(results.len(), 50, "Lt(501) should return all 50 results, got {}", results.len());
}

// ============================================================================
// Test 11: Between with tight range (single value)
// ============================================================================

#[test]
fn test_u64_between_single_value() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").between(&250u64.to_be_bytes(), &250u64.to_be_bytes())
    .all()
    .unwrap();

  assert_eq!(results.len(), 1, "Between(250, 250) should return exactly 1 result, got {}", results.len());
}

// ============================================================================
// Test 12: Eq for a value not in the dataset
// ============================================================================

#[test]
fn test_u64_eq_nonexistent() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").eq(&105u64.to_be_bytes())
    .all()
    .unwrap();

  // 105 is not in the dataset (only multiples of 10)
  assert_eq!(results.len(), 0, "Eq(105) should return 0 results, got {}", results.len());
}

// ============================================================================
// Test 13: Between returning empty range
// ============================================================================

#[test]
fn test_u64_between_empty_range() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").between(&501u64.to_be_bytes(), &600u64.to_be_bytes())
    .all()
    .unwrap();

  assert_eq!(results.len(), 0, "Between(501, 600) should return 0 results, got {}", results.len());
}

// ============================================================================
// Test 14: Gt on the max value in the dataset
// ============================================================================

#[test]
fn test_u64_gt_max_value() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").gt(&500u64.to_be_bytes())
    .all()
    .unwrap();

  assert_eq!(results.len(), 0, "Gt(500) should return 0 results, got {}", results.len());
}

// ============================================================================
// Test 15: Lt on the min value in the dataset
// ============================================================================

#[test]
fn test_u64_lt_min_value() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_u64_default_range_engine(&dir);

  let results = QueryBuilder::new(&engine, "/items")
    .field("value").lt(&10u64.to_be_bytes())
    .all()
    .unwrap();

  assert_eq!(results.len(), 0, "Lt(10) should return 0 results, got {}", results.len());
}
