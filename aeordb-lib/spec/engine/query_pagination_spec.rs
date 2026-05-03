use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::query_engine::{
    QueryEngine, QueryBuilder, Query, QueryNode, FieldQuery, QueryOp, QueryStrategy,
    SortField, SortDirection, DEFAULT_QUERY_LIMIT, ExplainMode,
};
use aeordb::engine::storage_engine::StorageEngine;
use aeordb::engine::RequestContext;
use std::collections::HashSet;

fn create_engine(dir: &tempfile::TempDir) -> StorageEngine {
    let ctx = RequestContext::system();
    let path = dir.path().join("test.aeor");
    let engine = StorageEngine::create(path.to_str().unwrap()).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    engine
}

fn make_person_json(name: &str, age: u64) -> Vec<u8> {
    format!(r#"{{"name":"{}","age":{}}}"#, name, age).into_bytes()
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

/// Create an engine with 30 people (ages 20..49), indexed by "name" (u64-type won't work
/// for string sort, but name is stored as a value) and "age" (u64, order-preserving).
fn setup_30_people(dir: &tempfile::TempDir) -> StorageEngine {
    let ctx = RequestContext::system();
    let engine = create_engine(dir);
    let ops = DirectoryOps::new(&engine);

    let config = PathIndexConfig {
        parser: None,
        parser_memory_limit: None,
        logging: false,
        glob: None,

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
    store_index_config(&engine, "/people", &config);

    // Create 30 people with ages 20..49
    for i in 0..30 {
        let age = 20 + i as u64;
        let name = format!("person_{:02}", i);
        let path = format!("/people/{}.json", name);
        ops.store_file_with_indexing(
            &ctx,
            &path,
            &make_person_json(&name, age),
            Some("application/json"),
        ).unwrap();
    }

    engine
}

/// Create an engine with a trigram-indexed field (non-order-preserving) plus a sortable age field.
fn setup_trigram_indexed(dir: &tempfile::TempDir) -> StorageEngine {
    let ctx = RequestContext::system();
    let engine = create_engine(dir);
    let ops = DirectoryOps::new(&engine);

    let config = PathIndexConfig {
        parser: None,
        parser_memory_limit: None,
        logging: false,
        glob: None,

        indexes: vec![
            IndexFieldConfig {
                name: "title".to_string(),
                index_type: "trigram".to_string(),
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
    store_index_config(&engine, "/items", &config);

    ops.store_file_with_indexing(
        &ctx,
        "/items/a.json",
        &r#"{"title":"hello world","age":25}"#.as_bytes(),
        Some("application/json"),
    ).unwrap();

    engine
}

fn make_query_all_people() -> Query {
    // Query: age > 0 (matches all 30 people)
    Query {
        path: "/people".to_string(),
        field_queries: Vec::new(),
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(0u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: None,
        explain: ExplainMode::Off,
    }
}

// ============================================================================
// Pagination tests
// ============================================================================

#[test]
fn test_default_limit_applied_when_no_limit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let query = make_query_all_people();
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), DEFAULT_QUERY_LIMIT);
    assert!(paginated.has_more);
    assert!(paginated.default_limit_hit);
}

#[test]
fn test_explicit_limit_overrides_default() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(5);
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 5);
    assert!(!paginated.default_limit_hit);
    assert!(paginated.has_more); // 30 > 5
}

#[test]
fn test_offset_skips_results() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    // Sort by @path to get deterministic ordering
    let mut query1 = make_query_all_people();
    query1.limit = Some(5);
    query1.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    let page1 = qe.execute_paginated(&query1).unwrap();

    let mut query2 = make_query_all_people();
    query2.limit = Some(5);
    query2.offset = Some(5);
    query2.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    let page2 = qe.execute_paginated(&query2).unwrap();

    assert_eq!(page1.results.len(), 5);
    assert_eq!(page2.results.len(), 5);

    // Ensure no overlap
    let paths1: Vec<&str> = page1.results.iter().map(|r| r.file_record.path.as_str()).collect();
    let paths2: Vec<&str> = page2.results.iter().map(|r| r.file_record.path.as_str()).collect();
    for p in &paths1 {
        assert!(!paths2.contains(p), "Overlap found: {}", p);
    }
}

#[test]
fn test_has_more_false_when_all_fit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(100);
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 30);
    assert!(!paginated.has_more);
    assert!(!paginated.default_limit_hit);
}

#[test]
fn test_include_total_returns_count() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.include_total = true;
    query.limit = Some(5);
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.total_count, Some(30));
    assert_eq!(paginated.results.len(), 5);
}

#[test]
fn test_include_total_none_when_not_requested() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.include_total = false;
    let paginated = qe.execute_paginated(&query).unwrap();

    assert!(paginated.total_count.is_none());
}

#[test]
fn test_empty_result_set() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    // Query for age > 999 (no matches)
    let query = Query {
        path: "/people".to_string(),
        field_queries: Vec::new(),
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(999u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: true,
        strategy: QueryStrategy::Full,
        aggregate: None,
        explain: ExplainMode::Off,
    };
    let paginated = qe.execute_paginated(&query).unwrap();

    assert!(paginated.results.is_empty());
    assert!(!paginated.has_more);
    assert!(!paginated.default_limit_hit);
    assert_eq!(paginated.total_count, Some(0));
}

// ============================================================================
// Sorting tests
// ============================================================================

#[test]
fn test_sort_by_field_asc() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    query.order_by = vec![SortField { field: "age".to_string(), direction: SortDirection::Asc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 30);
    // Verify ascending order by checking paths (person_00 has age 20, person_29 has age 49)
    let paths: Vec<String> = paginated.results.iter().map(|r| r.file_record.path.clone()).collect();
    for i in 1..paths.len() {
        assert!(paths[i - 1] <= paths[i], "Not sorted ascending: {} > {}", paths[i - 1], paths[i]);
    }
}

#[test]
fn test_sort_by_field_desc() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    query.order_by = vec![SortField { field: "age".to_string(), direction: SortDirection::Desc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 30);
    // Verify descending: first result should be person_29 (age 49), last person_00 (age 20)
    let paths: Vec<String> = paginated.results.iter().map(|r| r.file_record.path.clone()).collect();
    for i in 1..paths.len() {
        assert!(paths[i - 1] >= paths[i], "Not sorted descending: {} < {}", paths[i - 1], paths[i]);
    }
}

#[test]
fn test_sort_by_virtual_path() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    query.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    let paths: Vec<String> = paginated.results.iter().map(|r| r.file_record.path.clone()).collect();
    for i in 1..paths.len() {
        assert!(paths[i - 1] <= paths[i], "Paths not sorted ascending: {} > {}", paths[i - 1], paths[i]);
    }
}

#[test]
fn test_sort_by_virtual_score() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    query.order_by = vec![SortField { field: "@score".to_string(), direction: SortDirection::Desc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    // All exact-match queries get score 1.0, so all should be equal
    for r in &paginated.results {
        assert!((r.score - 1.0).abs() < f64::EPSILON);
    }
}

#[test]
fn test_sort_by_virtual_created_at() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    query.order_by = vec![SortField { field: "@created_at".to_string(), direction: SortDirection::Asc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    // Just verify it runs without error and returns sorted results
    assert_eq!(paginated.results.len(), 30);
    let timestamps: Vec<i64> = paginated.results.iter().map(|r| r.file_record.created_at).collect();
    for i in 1..timestamps.len() {
        assert!(timestamps[i - 1] <= timestamps[i], "created_at not sorted ascending");
    }
}

#[test]
fn test_sort_by_virtual_updated_at() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    query.order_by = vec![SortField { field: "@updated_at".to_string(), direction: SortDirection::Desc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 30);
    let timestamps: Vec<i64> = paginated.results.iter().map(|r| r.file_record.updated_at).collect();
    for i in 1..timestamps.len() {
        assert!(timestamps[i - 1] >= timestamps[i], "updated_at not sorted descending");
    }
}

#[test]
fn test_sort_by_virtual_size() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    query.order_by = vec![SortField { field: "@size".to_string(), direction: SortDirection::Asc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 30);
    let sizes: Vec<u64> = paginated.results.iter().map(|r| r.file_record.total_size).collect();
    for i in 1..sizes.len() {
        assert!(sizes[i - 1] <= sizes[i], "sizes not sorted ascending: {} > {}", sizes[i - 1], sizes[i]);
    }
}

#[test]
fn test_sort_nonexistent_field_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(5);
    query.order_by = vec![SortField { field: "missing_field".to_string(), direction: SortDirection::Asc }];
    let result = qe.execute_paginated(&query);

    assert!(result.is_err(), "Expected error for nonexistent sort field");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("missing_field"), "Error should mention the field name: {}", err_msg);
}

#[test]
fn test_sort_non_order_preserving_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_trigram_indexed(&dir);
    let qe = QueryEngine::new(&engine);

    // Query by age (u64, order-preserving) but sort by title (trigram, NOT order-preserving)
    let query = Query {
        path: "/items".to_string(),
        field_queries: Vec::new(),
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(0u64.to_be_bytes().to_vec()),
        })),
        limit: Some(10),
        offset: None,
        order_by: vec![SortField { field: "title".to_string(), direction: SortDirection::Asc }],
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: None,
        explain: ExplainMode::Off,
    };

    let result = qe.execute_paginated(&query);
    assert!(result.is_err(), "Expected error for non-order-preserving sort field");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("order-preserving"), "Error should mention order-preserving: {}", err_msg);
}

#[test]
fn test_no_order_by_preserves_existing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    // No order_by
    let paginated = qe.execute_paginated(&query).unwrap();

    // Should return all 30 results in whatever order the engine produces
    assert_eq!(paginated.results.len(), 30);
}

#[test]
fn test_sort_then_offset() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    query.offset = Some(10);
    query.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 20); // 30 - 10 offset
    // Verify still sorted
    let paths: Vec<String> = paginated.results.iter().map(|r| r.file_record.path.clone()).collect();
    for i in 1..paths.len() {
        assert!(paths[i - 1] <= paths[i], "Not sorted after offset");
    }
}

#[test]
fn test_sort_then_limit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(5);
    query.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 5);
    assert!(paginated.has_more);
    // Should be the first 5 paths in sorted order
    let paths: Vec<String> = paginated.results.iter().map(|r| r.file_record.path.clone()).collect();
    for i in 1..paths.len() {
        assert!(paths[i - 1] <= paths[i], "Not sorted");
    }
    // First should be person_00
    assert!(paths[0].contains("person_00"), "First sorted path should be person_00, got {}", paths[0]);
}

#[test]
fn test_multi_field_sort() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    query.order_by = vec![
        SortField { field: "age".to_string(), direction: SortDirection::Asc },
        SortField { field: "@path".to_string(), direction: SortDirection::Desc },
    ];
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 30);
    // Since each person has a unique age, the secondary sort (@path desc) won't change anything,
    // but the primary sort by age ascending should work.
    // person_00 (age 20) should be first, person_29 (age 49) should be last.
    assert!(
        paginated.results[0].file_record.path.contains("person_00"),
        "Expected person_00 first, got {}",
        paginated.results[0].file_record.path
    );
    assert!(
        paginated.results[29].file_record.path.contains("person_29"),
        "Expected person_29 last, got {}",
        paginated.results[29].file_record.path
    );
}

// ============================================================================
// Backward compatibility tests
// ============================================================================

#[test]
fn test_execute_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(5);
    let results = qe.execute(&query).unwrap();

    assert_eq!(results.len(), 5);
}

#[test]
fn test_execute_applies_default_limit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let query = make_query_all_people();
    // No explicit limit
    let results = qe.execute(&query).unwrap();

    assert!(results.len() <= DEFAULT_QUERY_LIMIT, "execute() should apply default limit, got {}", results.len());
}

// ============================================================================
// Edge case tests
// ============================================================================

#[test]
fn test_offset_beyond_results() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.offset = Some(999);
    query.limit = Some(10);
    let paginated = qe.execute_paginated(&query).unwrap();

    assert!(paginated.results.is_empty());
    assert!(!paginated.has_more);
    assert!(!paginated.default_limit_hit);
}

#[test]
fn test_offset_exactly_at_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    // Offset = 30 (exactly the count of results)
    let mut query = make_query_all_people();
    query.offset = Some(30);
    query.limit = Some(10);
    let paginated = qe.execute_paginated(&query).unwrap();

    assert!(paginated.results.is_empty());
}

#[test]
fn test_offset_one_before_end() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.offset = Some(29);
    query.limit = Some(10);
    query.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 1);
    assert!(!paginated.has_more);
    assert!(paginated.results[0].file_record.path.contains("person_29"));
}

#[test]
fn test_zero_limit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(0);
    let paginated = qe.execute_paginated(&query).unwrap();

    assert!(paginated.results.is_empty());
    assert!(!paginated.default_limit_hit); // explicit limit was provided
    assert!(paginated.has_more); // 30 > 0
}

#[test]
fn test_paginated_cursors_present_when_has_more() {
    // When there are more results than the limit, next_cursor should be Some
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let query = make_query_all_people();
    let paginated = qe.execute_paginated(&query).unwrap();

    // Default limit 20 with 30 results => has_more = true, next_cursor = Some
    assert!(paginated.has_more);
    assert!(paginated.next_cursor.is_some());
    // No after cursor was used and offset is 0, so prev_cursor should be None
    assert!(paginated.prev_cursor.is_none());
}

#[test]
fn test_default_query_limit_constant() {
    assert_eq!(DEFAULT_QUERY_LIMIT, 20);
}

#[test]
fn test_sort_unknown_virtual_field_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(30);
    query.order_by = vec![SortField { field: "@nonexistent".to_string(), direction: SortDirection::Asc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    // Unknown virtual fields compare as Equal, so order is unspecified but no error
    assert_eq!(paginated.results.len(), 30);
}

#[test]
fn test_include_total_with_offset() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.include_total = true;
    query.offset = Some(10);
    query.limit = Some(5);
    let paginated = qe.execute_paginated(&query).unwrap();

    // total_count should reflect ALL results before offset/limit
    assert_eq!(paginated.total_count, Some(30));
    assert_eq!(paginated.results.len(), 5);
    assert!(paginated.has_more); // 20 remaining after offset > 5
}

#[test]
fn test_execute_paginated_with_no_node() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let query = Query {
        path: "/people".to_string(),
        field_queries: Vec::new(),
        node: None,
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: true,
        strategy: QueryStrategy::Full,
        aggregate: None,
        explain: ExplainMode::Off,
    };
    let paginated = qe.execute_paginated(&query).unwrap();

    assert!(paginated.results.is_empty());
    assert_eq!(paginated.total_count, Some(0));
    assert!(!paginated.has_more);
    assert!(!paginated.default_limit_hit);
}

// ============================================================================
// Cursor-based pagination tests (Task 4)
// ============================================================================

#[test]
fn test_cursor_pagination_next_page() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    // Page 1: limit=5, sorted by @path asc
    let mut query1 = make_query_all_people();
    query1.limit = Some(5);
    query1.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    let page1 = qe.execute_paginated(&query1).unwrap();

    assert_eq!(page1.results.len(), 5);
    assert!(page1.has_more);
    assert!(page1.next_cursor.is_some());

    // Page 2: use after cursor from page 1
    let mut query2 = make_query_all_people();
    query2.limit = Some(5);
    query2.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    query2.after = page1.next_cursor.clone();
    let page2 = qe.execute_paginated(&query2).unwrap();

    assert_eq!(page2.results.len(), 5);

    // Ensure no overlap
    let paths1: Vec<&str> = page1.results.iter().map(|r| r.file_record.path.as_str()).collect();
    let paths2: Vec<&str> = page2.results.iter().map(|r| r.file_record.path.as_str()).collect();
    for p in &paths1 {
        assert!(!paths2.contains(p), "Overlap: {}", p);
    }

    // Verify page2 continues from where page1 left off
    assert!(paths1.last().unwrap() < paths2.first().unwrap(),
        "Page 2 should start after page 1 ended: {:?} vs {:?}", paths1.last(), paths2.first());

    // Page 2 should have prev_cursor since we used after
    assert!(page2.prev_cursor.is_some());
}

#[test]
fn test_cursor_contains_version() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(5);
    query.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    let paginated = qe.execute_paginated(&query).unwrap();

    let cursor_str = paginated.next_cursor.unwrap();
    // Decode cursor
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&cursor_str).unwrap();
    let cursor_json: serde_json::Value = serde_json::from_slice(&decoded).unwrap();

    assert!(cursor_json.get("_version").is_some(), "Cursor should contain _version");
    assert!(cursor_json.get("_hash").is_some(), "Cursor should contain _hash");
    assert!(cursor_json["_version"].as_str().unwrap().len() > 0, "Version should be non-empty hex");
    assert!(cursor_json["_hash"].as_str().unwrap().len() > 0, "Hash should be non-empty hex");
}

#[test]
fn test_invalid_cursor_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    // Bad base64
    let mut query = make_query_all_people();
    query.limit = Some(5);
    query.after = Some("not-valid-base64!!!".to_string());
    let result = qe.execute_paginated(&query);
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("Invalid cursor"), "Error should mention invalid cursor: {}", err);

    // Valid base64 but missing _hash
    let bad_json = serde_json::json!({"foo": "bar"});
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_vec(&bad_json).unwrap());
    let mut query2 = make_query_all_people();
    query2.limit = Some(5);
    query2.after = Some(encoded);
    let result2 = qe.execute_paginated(&query2);
    assert!(result2.is_err());
    let err2 = format!("{}", result2.unwrap_err());
    assert!(err2.contains("_hash"), "Error should mention missing _hash: {}", err2);

    // Valid base64 but not valid JSON
    let bad_b64 = base64::engine::general_purpose::STANDARD.encode(b"not json");
    let mut query3 = make_query_all_people();
    query3.limit = Some(5);
    query3.after = Some(bad_b64);
    let result3 = qe.execute_paginated(&query3);
    assert!(result3.is_err());
}

#[test]
fn test_no_more_results_no_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut query = make_query_all_people();
    query.limit = Some(100); // All results fit
    let paginated = qe.execute_paginated(&query).unwrap();

    assert_eq!(paginated.results.len(), 30);
    assert!(!paginated.has_more);
    assert!(paginated.next_cursor.is_none(), "next_cursor should be None when all results fit");
}

#[test]
fn test_cursor_three_pages() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let mut all_paths: Vec<String> = Vec::new();

    // Page 1
    let mut query = make_query_all_people();
    query.limit = Some(10);
    query.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    let page1 = qe.execute_paginated(&query).unwrap();
    assert_eq!(page1.results.len(), 10);
    assert!(page1.has_more);
    assert!(page1.next_cursor.is_some());
    for r in &page1.results {
        all_paths.push(r.file_record.path.clone());
    }

    // Page 2
    let mut query2 = make_query_all_people();
    query2.limit = Some(10);
    query2.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    query2.after = page1.next_cursor.clone();
    let page2 = qe.execute_paginated(&query2).unwrap();
    assert_eq!(page2.results.len(), 10);
    assert!(page2.has_more);
    assert!(page2.next_cursor.is_some());
    for r in &page2.results {
        all_paths.push(r.file_record.path.clone());
    }

    // Page 3
    let mut query3 = make_query_all_people();
    query3.limit = Some(10);
    query3.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    query3.after = page2.next_cursor.clone();
    let page3 = qe.execute_paginated(&query3).unwrap();
    assert_eq!(page3.results.len(), 10);
    assert!(!page3.has_more);
    assert!(page3.next_cursor.is_none());
    for r in &page3.results {
        all_paths.push(r.file_record.path.clone());
    }

    // Verify complete: 30 unique paths, no duplicates, no gaps
    assert_eq!(all_paths.len(), 30);
    let unique: HashSet<&String> = all_paths.iter().collect();
    assert_eq!(unique.len(), 30, "Expected 30 unique paths, got {}", unique.len());

    // Verify sorted
    for i in 1..all_paths.len() {
        assert!(all_paths[i - 1] < all_paths[i],
            "Not sorted at index {}: {} >= {}", i, all_paths[i - 1], all_paths[i]);
    }
}

#[test]
fn test_before_cursor_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    // Get page 1 to obtain a cursor
    let mut query1 = make_query_all_people();
    query1.limit = Some(10);
    query1.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    let page1 = qe.execute_paginated(&query1).unwrap();

    // Page 2 to get its first element's cursor (prev_cursor)
    let mut query2 = make_query_all_people();
    query2.limit = Some(10);
    query2.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    query2.after = page1.next_cursor.clone();
    let page2 = qe.execute_paginated(&query2).unwrap();
    assert!(page2.prev_cursor.is_some());

    // Use before cursor with the first result of page 2 to get results before it
    let mut query_before = make_query_all_people();
    query_before.limit = Some(100);
    query_before.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    query_before.before = page2.prev_cursor.clone();
    let before_page = qe.execute_paginated(&query_before).unwrap();

    // The "before" results should not include any of page2's results
    let page2_paths: Vec<&str> = page2.results.iter().map(|r| r.file_record.path.as_str()).collect();
    for r in &before_page.results {
        assert!(!page2_paths.contains(&r.file_record.path.as_str()),
            "before results should not overlap with page2");
    }
}

#[test]
fn test_cursor_with_nonexistent_hash_skips_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    // Build a cursor with a hash that doesn't exist in results
    let fake_cursor = serde_json::json!({
        "_hash": "deadbeef00000000000000000000000000000000000000000000000000000000",
        "_version": "0000000000000000000000000000000000000000000000000000000000000000"
    });
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_vec(&fake_cursor).unwrap());

    let mut query = make_query_all_people();
    query.limit = Some(100);
    query.order_by = vec![SortField { field: "@path".to_string(), direction: SortDirection::Asc }];
    query.after = Some(encoded);
    let paginated = qe.execute_paginated(&query).unwrap();

    // Hash not found => no skip, all 30 results returned
    assert_eq!(paginated.results.len(), 30);
}

#[test]
fn test_cursor_invalid_hex_hash_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let bad_cursor = serde_json::json!({
        "_hash": "not-valid-hex",
        "_version": "0000"
    });
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_vec(&bad_cursor).unwrap());

    let mut query = make_query_all_people();
    query.limit = Some(5);
    query.after = Some(encoded);
    let result = qe.execute_paginated(&query);
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("Invalid cursor hash"), "Error: {}", err);
}

// ============================================================================
// QueryBuilder tests (Task 7)
// ============================================================================

#[test]
fn test_query_builder_order_by() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);

    let paginated = QueryBuilder::new(&engine, "/people")
        .field("age").gt_u64(0)
        .order_by("@path", SortDirection::Asc)
        .limit(5)
        .execute_paginated()
        .unwrap();

    assert_eq!(paginated.results.len(), 5);
    assert!(paginated.has_more);
    let paths: Vec<String> = paginated.results.iter().map(|r| r.file_record.path.clone()).collect();
    for i in 1..paths.len() {
        assert!(paths[i - 1] <= paths[i], "Not sorted: {} > {}", paths[i - 1], paths[i]);
    }
}

#[test]
fn test_query_builder_offset() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);

    let page1 = QueryBuilder::new(&engine, "/people")
        .field("age").gt_u64(0)
        .order_by("@path", SortDirection::Asc)
        .limit(5)
        .execute_paginated()
        .unwrap();

    let page2 = QueryBuilder::new(&engine, "/people")
        .field("age").gt_u64(0)
        .order_by("@path", SortDirection::Asc)
        .limit(5)
        .offset(5)
        .execute_paginated()
        .unwrap();

    // No overlap
    let paths1: Vec<&str> = page1.results.iter().map(|r| r.file_record.path.as_str()).collect();
    let paths2: Vec<&str> = page2.results.iter().map(|r| r.file_record.path.as_str()).collect();
    for p in &paths1 {
        assert!(!paths2.contains(p));
    }
}

#[test]
fn test_query_builder_include_total() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);

    let paginated = QueryBuilder::new(&engine, "/people")
        .field("age").gt_u64(0)
        .include_total()
        .limit(5)
        .execute_paginated()
        .unwrap();

    assert_eq!(paginated.total_count, Some(30));
    assert_eq!(paginated.results.len(), 5);
}

#[test]
fn test_query_builder_cursor_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);

    let page1 = QueryBuilder::new(&engine, "/people")
        .field("age").gt_u64(0)
        .order_by("@path", SortDirection::Asc)
        .limit(10)
        .execute_paginated()
        .unwrap();

    assert!(page1.next_cursor.is_some());

    let page2 = QueryBuilder::new(&engine, "/people")
        .field("age").gt_u64(0)
        .order_by("@path", SortDirection::Asc)
        .limit(10)
        .after(page1.next_cursor.as_ref().unwrap())
        .execute_paginated()
        .unwrap();

    assert_eq!(page2.results.len(), 10);

    // Verify no overlap
    let paths1: HashSet<String> = page1.results.iter().map(|r| r.file_record.path.clone()).collect();
    let paths2: HashSet<String> = page2.results.iter().map(|r| r.file_record.path.clone()).collect();
    assert!(paths1.is_disjoint(&paths2), "Pages should not overlap");
}

#[test]
fn test_query_builder_before_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);

    // Get page 1 cursor
    let page1 = QueryBuilder::new(&engine, "/people")
        .field("age").gt_u64(0)
        .order_by("@path", SortDirection::Asc)
        .limit(10)
        .execute_paginated()
        .unwrap();

    // Get page 2 and its prev cursor
    let page2 = QueryBuilder::new(&engine, "/people")
        .field("age").gt_u64(0)
        .order_by("@path", SortDirection::Asc)
        .limit(10)
        .after(page1.next_cursor.as_ref().unwrap())
        .execute_paginated()
        .unwrap();

    assert!(page2.prev_cursor.is_some());

    // Use before cursor
    let before_results = QueryBuilder::new(&engine, "/people")
        .field("age").gt_u64(0)
        .order_by("@path", SortDirection::Asc)
        .limit(100)
        .before(page2.prev_cursor.as_ref().unwrap())
        .execute_paginated()
        .unwrap();

    // Should not contain any of page2's items
    let page2_paths: HashSet<String> = page2.results.iter().map(|r| r.file_record.path.clone()).collect();
    for r in &before_results.results {
        assert!(!page2_paths.contains(&r.file_record.path));
    }
}

use base64::Engine as _;
