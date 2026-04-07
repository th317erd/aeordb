use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::query_engine::{
    QueryEngine, Query, QueryNode, FieldQuery, QueryOp, QueryStrategy,
    SortField, SortDirection, DEFAULT_QUERY_LIMIT,
};
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

fn make_person_json(name: &str, age: u64) -> Vec<u8> {
    format!(r#"{{"name":"{}","age":{}}}"#, name, age).into_bytes()
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
fn test_paginated_cursors_are_none() {
    // Cursors are not implemented yet (Task 4), so verify they are None
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_30_people(&dir);
    let qe = QueryEngine::new(&engine);

    let query = make_query_all_people();
    let paginated = qe.execute_paginated(&query).unwrap();

    assert!(paginated.next_cursor.is_none());
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
    };
    let paginated = qe.execute_paginated(&query).unwrap();

    assert!(paginated.results.is_empty());
    assert_eq!(paginated.total_count, Some(0));
    assert!(!paginated.has_more);
    assert!(!paginated.default_limit_hit);
}
