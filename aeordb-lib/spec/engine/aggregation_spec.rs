use std::collections::HashSet;

use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::query_engine::{
    AggregateQuery, QueryEngine, Query, QueryNode, FieldQuery,
    QueryOp, QueryStrategy, bytes_to_f64, bytes_to_json_value, is_numeric_type, ExplainMode,
};
use aeordb::engine::scalar_converter::{
    CONVERTER_TYPE_U8, CONVERTER_TYPE_U16, CONVERTER_TYPE_U32, CONVERTER_TYPE_U64,
    CONVERTER_TYPE_I64, CONVERTER_TYPE_F64, CONVERTER_TYPE_STRING, CONVERTER_TYPE_TIMESTAMP,
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

fn make_person_json(age: u64, department: &str, salary: u64) -> Vec<u8> {
    format!(
        r#"{{"age":{},"department":"{}","salary":{}}}"#,
        age, department, salary,
    ).into_bytes()
}

/// Set up an engine with 20 people indexed by age, department, and salary.
/// person_00: age=20, department="engineering", salary=50000
/// person_01: age=21, department="sales", salary=55000
/// ... alternating departments, incrementing age and salary
fn setup_people_engine(dir: &tempfile::TempDir) -> StorageEngine {
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
                name: "department".to_string(),
                index_type: "string".to_string(),
                source: None,
                min: None,
                max: None,
            },
            IndexFieldConfig {
                name: "salary".to_string(),
                index_type: "u64".to_string(),
                source: None,
                min: Some(0.0),
                max: Some(200000.0),
            },
        ],
    };
    store_index_config(&engine, "/people", &config);

    for i in 0..20u64 {
        let age = 20 + i;
        let department = if i % 2 == 0 { "engineering" } else { "sales" };
        let salary = 50000 + i * 5000;
        let path = format!("/people/person_{:02}.json", i);
        let data = make_person_json(age, department, salary);
        ops.store_file_with_indexing(&ctx, &path, &data, Some("application/json")).unwrap();
    }

    engine
}

/// Helper: build a query that matches all people (age > 0).
fn make_all_people_query(agg: AggregateQuery, limit: Option<usize>) -> Query {
    Query {
        path: "/people".to_string(),
        field_queries: Vec::new(),
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(0u64.to_be_bytes().to_vec()),
        })),
        limit,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: Some(agg),
        explain: ExplainMode::Off,
    }
}

// ============================================================================
// 1. test_count
// ============================================================================
#[test]
fn test_count() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery { count: true, ..Default::default() };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    assert_eq!(result.count, Some(20));
    assert!(!result.has_more);
}

// ============================================================================
// 2. test_count_with_filter
// ============================================================================
#[test]
fn test_count_with_filter() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    // age > 30 means ages 31..39 = 9 people (indices 11..19)
    let agg = AggregateQuery { count: true, ..Default::default() };
    let query = Query {
        path: "/people".to_string(),
        field_queries: Vec::new(),
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(30u64.to_be_bytes().to_vec()),
        })),
        limit: None,
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: Some(agg),
        explain: ExplainMode::Off,
    };
    let result = qe.execute_aggregate(&query).unwrap();

    let count = result.count.unwrap();
    assert!(count < 20, "filtered count should be less than 20, got {}", count);
    assert!(count > 0, "filtered count should be > 0");
    // ages 31-39 = 9 people
    assert_eq!(count, 9);
}

// ============================================================================
// 3. test_sum
// ============================================================================
#[test]
fn test_sum() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        sum: vec!["salary".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    // salary = 50000 + i*5000 for i in 0..20
    // sum = 20*50000 + 5000*(0+1+2+...+19) = 1000000 + 5000*190 = 1000000 + 950000 = 1950000
    let sum = result.sum.get("salary").unwrap();
    assert_eq!(*sum, 1_950_000.0);
}

// ============================================================================
// 4. test_avg
// ============================================================================
#[test]
fn test_avg() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        avg: vec!["age".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    // ages 20..39, avg = (20+21+...+39)/20 = 590/20 = 29.5
    let avg = result.avg.get("age").unwrap();
    assert!((avg - 29.5).abs() < 0.01, "avg age should be 29.5, got {}", avg);
}

// ============================================================================
// 5. test_min
// ============================================================================
#[test]
fn test_min() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        min: vec!["age".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    let min_val = result.min.get("age").unwrap();
    assert_eq!(*min_val, serde_json::json!(20u64));
}

// ============================================================================
// 6. test_max
// ============================================================================
#[test]
fn test_max() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        max: vec!["salary".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    // max salary = 50000 + 19*5000 = 145000
    let max_val = result.max.get("salary").unwrap();
    assert_eq!(*max_val, serde_json::json!(145000u64));
}

// ============================================================================
// 7. test_min_max_string
// ============================================================================
#[test]
fn test_min_max_string() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        min: vec!["department".to_string()],
        max: vec!["department".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    // lexicographic: "engineering" < "sales"
    let min_val = result.min.get("department").unwrap();
    let max_val = result.max.get("department").unwrap();
    assert_eq!(*min_val, serde_json::json!("engineering"));
    assert_eq!(*max_val, serde_json::json!("sales"));
}

// ============================================================================
// 8. test_sum_non_numeric_errors
// ============================================================================
#[test]
fn test_sum_non_numeric_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        sum: vec!["department".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query);

    assert!(result.is_err(), "SUM on string field should error");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("SUM"), "error should mention SUM: {}", err_msg);
}

// ============================================================================
// 9. test_avg_non_numeric_errors
// ============================================================================
#[test]
fn test_avg_non_numeric_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        avg: vec!["department".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query);

    assert!(result.is_err(), "AVG on string field should error");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("AVG"), "error should mention AVG: {}", err_msg);
}

// ============================================================================
// 10. test_field_not_indexed_errors
// ============================================================================
#[test]
fn test_field_not_indexed_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        sum: vec!["nonexistent_field".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query);

    assert!(result.is_err(), "aggregate on non-indexed field should error");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("No index found"), "error should mention missing index: {}", err_msg);
}

// ============================================================================
// 11. test_group_by_single_field
// ============================================================================
#[test]
fn test_group_by_single_field() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        count: true,
        group_by: vec!["department".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    let groups = result.groups.as_ref().unwrap();
    assert_eq!(groups.len(), 2, "should have 2 groups (engineering, sales)");

    // Check that both departments are present
    let dept_names: HashSet<String> = groups.iter()
        .map(|g| g.key.get("department").unwrap().as_str().unwrap().to_string())
        .collect();
    assert!(dept_names.contains("engineering"));
    assert!(dept_names.contains("sales"));
}

// ============================================================================
// 12. test_group_by_with_count
// ============================================================================
#[test]
fn test_group_by_with_count() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        count: true,
        group_by: vec!["department".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    let groups = result.groups.as_ref().unwrap();
    // 10 engineering (even indices), 10 sales (odd indices)
    for group in groups {
        assert_eq!(group.count, 10, "each department should have 10 people");
    }
    // Total count
    assert_eq!(result.count, Some(20));
}

// ============================================================================
// 13. test_group_by_with_avg
// ============================================================================
#[test]
fn test_group_by_with_avg() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        avg: vec!["age".to_string()],
        group_by: vec!["department".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    let groups = result.groups.as_ref().unwrap();
    for group in groups {
        let dept = group.key.get("department").unwrap().as_str().unwrap();
        let avg_age = group.avg.get("age").unwrap();

        if dept == "engineering" {
            // Engineering: even indices 0,2,4,...,18 -> ages 20,22,24,...,38
            // avg = (20+22+24+26+28+30+32+34+36+38)/10 = 290/10 = 29.0
            assert!((avg_age - 29.0).abs() < 0.01, "engineering avg age should be 29.0, got {}", avg_age);
        } else {
            // Sales: odd indices 1,3,5,...,19 -> ages 21,23,25,...,39
            // avg = (21+23+25+27+29+31+33+35+37+39)/10 = 300/10 = 30.0
            assert!((avg_age - 30.0).abs() < 0.01, "sales avg age should be 30.0, got {}", avg_age);
        }
    }
}

// ============================================================================
// 14. test_group_by_multi_field
// ============================================================================
#[test]
fn test_group_by_multi_field() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    // Group by department + age -- should produce many groups (each unique dept+age combo)
    let agg = AggregateQuery {
        count: true,
        group_by: vec!["department".to_string(), "age".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, Some(100));
    let result = qe.execute_aggregate(&query).unwrap();

    let groups = result.groups.as_ref().unwrap();
    // Each person has a unique age, so 20 groups
    assert_eq!(groups.len(), 20, "should have 20 unique dept+age groups");

    // Each group should have count=1 (unique age per person)
    for group in groups {
        assert_eq!(group.count, 1);
        assert!(group.key.contains_key("department"));
        assert!(group.key.contains_key("age"));
    }
}

// ============================================================================
// 15. test_group_by_default_limit
// ============================================================================
#[test]
fn test_group_by_default_limit() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = RequestContext::system();
    let engine = create_engine(&dir);
    let ops = DirectoryOps::new(&engine);

    // Create 25 different departments so groups exceed default limit of 20
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
                name: "department".to_string(),
                index_type: "string".to_string(),
                source: None,
                min: None,
                max: None,
            },
        ],
    };
    store_index_config(&engine, "/many", &config);

    for i in 0..25u64 {
        let dept = format!("dept_{:02}", i);
        let json = format!(r#"{{"age":{},"department":"{}"}}"#, 20 + i, dept);
        let path = format!("/many/person_{:02}.json", i);
        ops.store_file_with_indexing(&ctx, &path, json.as_bytes(), Some("application/json")).unwrap();
    }

    let qe = QueryEngine::new(&engine);
    let agg = AggregateQuery {
        count: true,
        group_by: vec!["department".to_string()],
        ..Default::default()
    };
    let query = Query {
        path: "/many".to_string(),
        field_queries: Vec::new(),
        node: Some(QueryNode::Field(FieldQuery {
            field_name: "age".to_string(),
            operation: QueryOp::Gt(0u64.to_be_bytes().to_vec()),
        })),
        limit: None,  // default limit
        offset: None,
        order_by: Vec::new(),
        after: None,
        before: None,
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: Some(agg),
        explain: ExplainMode::Off,
    };
    let result = qe.execute_aggregate(&query).unwrap();

    let groups = result.groups.as_ref().unwrap();
    assert_eq!(groups.len(), 20, "should be capped at default limit of 20");
    assert!(result.has_more, "should indicate more groups available");
    assert!(result.default_limit_hit, "should indicate default limit was hit");
}

// ============================================================================
// 16. test_group_by_explicit_limit
// ============================================================================
#[test]
fn test_group_by_explicit_limit() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    // Group by age (20 unique) but limit to 5
    let agg = AggregateQuery {
        count: true,
        group_by: vec!["age".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, Some(5));
    let result = qe.execute_aggregate(&query).unwrap();

    let groups = result.groups.as_ref().unwrap();
    assert_eq!(groups.len(), 5, "should be limited to 5 groups");
    assert!(result.has_more, "should indicate more groups available");
    assert!(!result.default_limit_hit, "explicit limit, not default");
}

// ============================================================================
// 17. test_empty_result_set
// ============================================================================
#[test]
fn test_empty_result_set() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    // age > 999 matches nobody
    let agg = AggregateQuery {
        count: true,
        sum: vec!["salary".to_string()],
        avg: vec!["age".to_string()],
        min: vec!["age".to_string()],
        max: vec!["salary".to_string()],
        ..Default::default()
    };
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
        include_total: false,
        strategy: QueryStrategy::Full,
        aggregate: Some(agg),
        explain: ExplainMode::Off,
    };
    let result = qe.execute_aggregate(&query).unwrap();

    assert_eq!(result.count, Some(0));
    assert!(result.sum.is_empty(), "sum should be empty for no results");
    assert!(result.avg.is_empty(), "avg should be empty for no results");
    assert!(result.min.is_empty(), "min should be empty for no results");
    assert!(result.max.is_empty(), "max should be empty for no results");
}

// ============================================================================
// 18. test_multiple_aggregates
// ============================================================================
#[test]
fn test_multiple_aggregates() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        count: true,
        sum: vec!["salary".to_string()],
        avg: vec!["age".to_string()],
        min: vec!["age".to_string()],
        max: vec!["salary".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    assert_eq!(result.count, Some(20));
    assert_eq!(*result.sum.get("salary").unwrap(), 1_950_000.0);
    assert!((result.avg.get("age").unwrap() - 29.5).abs() < 0.01);
    assert_eq!(*result.min.get("age").unwrap(), serde_json::json!(20u64));
    assert_eq!(*result.max.get("salary").unwrap(), serde_json::json!(145000u64));
}

// ============================================================================
// 19. test_group_by_not_indexed_errors
// ============================================================================
#[test]
fn test_group_by_not_indexed_errors() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        count: true,
        group_by: vec!["nonexistent_field".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query);

    assert!(result.is_err(), "group by non-indexed field should error");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("No index found"), "error should mention missing index: {}", err_msg);
}

// ============================================================================
// 20. test_bytes_to_f64_u64
// ============================================================================
#[test]
fn test_bytes_to_f64_u64() {
    let value: u64 = 12345;
    let bytes = value.to_be_bytes();
    let result = bytes_to_f64(&bytes, CONVERTER_TYPE_U64);
    assert_eq!(result, Some(12345.0));
}

// ============================================================================
// 21. test_bytes_to_f64_i64
// ============================================================================
#[test]
fn test_bytes_to_f64_i64() {
    let value: i64 = -42;
    let bytes = value.to_be_bytes();
    let result = bytes_to_f64(&bytes, CONVERTER_TYPE_I64);
    assert_eq!(result, Some(-42.0));

    // Test positive i64
    let value2: i64 = 100;
    let bytes2 = value2.to_be_bytes();
    let result2 = bytes_to_f64(&bytes2, CONVERTER_TYPE_I64);
    assert_eq!(result2, Some(100.0));
}

// ============================================================================
// 22. test_bytes_to_f64_f64
// ============================================================================
#[test]
fn test_bytes_to_f64_f64() {
    let value: f64 = 3.14159;
    let bytes = value.to_be_bytes();
    let result = bytes_to_f64(&bytes, CONVERTER_TYPE_F64);
    assert!((result.unwrap() - 3.14159).abs() < 1e-10);
}

// ============================================================================
// 23. test_bytes_to_json_value_string
// ============================================================================
#[test]
fn test_bytes_to_json_value_string() {
    let value = "hello world";
    let bytes = value.as_bytes();
    let result = bytes_to_json_value(bytes, CONVERTER_TYPE_STRING);
    assert_eq!(result, serde_json::json!("hello world"));
}

// ============================================================================
// 24. test_bytes_to_json_value_number
// ============================================================================
#[test]
fn test_bytes_to_json_value_number() {
    // u8
    let result_u8 = bytes_to_json_value(&[42], CONVERTER_TYPE_U8);
    assert_eq!(result_u8, serde_json::json!(42));

    // u16
    let bytes_u16 = 1000u16.to_be_bytes();
    let result_u16 = bytes_to_json_value(&bytes_u16, CONVERTER_TYPE_U16);
    assert_eq!(result_u16, serde_json::json!(1000));

    // u32
    let bytes_u32 = 100000u32.to_be_bytes();
    let result_u32 = bytes_to_json_value(&bytes_u32, CONVERTER_TYPE_U32);
    assert_eq!(result_u32, serde_json::json!(100000));

    // u64
    let bytes_u64 = 1000000u64.to_be_bytes();
    let result_u64 = bytes_to_json_value(&bytes_u64, CONVERTER_TYPE_U64);
    assert_eq!(result_u64, serde_json::json!(1000000u64));

    // i64
    let bytes_i64 = (-500i64).to_be_bytes();
    let result_i64 = bytes_to_json_value(&bytes_i64, CONVERTER_TYPE_I64);
    assert_eq!(result_i64, serde_json::json!(-500i64));

    // f64
    let bytes_f64 = (2.718f64).to_be_bytes();
    let result_f64 = bytes_to_json_value(&bytes_f64, CONVERTER_TYPE_F64);
    assert_eq!(result_f64, serde_json::json!(2.718));
}

// ============================================================================
// Additional edge case tests
// ============================================================================

#[test]
fn test_bytes_to_f64_too_short() {
    // Empty bytes for u64 should return None
    assert_eq!(bytes_to_f64(&[], CONVERTER_TYPE_U64), None);
    assert_eq!(bytes_to_f64(&[1, 2, 3], CONVERTER_TYPE_U64), None);
    assert_eq!(bytes_to_f64(&[], CONVERTER_TYPE_U8), None);
    assert_eq!(bytes_to_f64(&[], CONVERTER_TYPE_F64), None);
}

#[test]
fn test_bytes_to_f64_u8() {
    let result = bytes_to_f64(&[255], CONVERTER_TYPE_U8);
    assert_eq!(result, Some(255.0));
}

#[test]
fn test_bytes_to_f64_u16() {
    let bytes = 65535u16.to_be_bytes();
    let result = bytes_to_f64(&bytes, CONVERTER_TYPE_U16);
    assert_eq!(result, Some(65535.0));
}

#[test]
fn test_bytes_to_f64_u32() {
    let bytes = 1_000_000u32.to_be_bytes();
    let result = bytes_to_f64(&bytes, CONVERTER_TYPE_U32);
    assert_eq!(result, Some(1_000_000.0));
}

#[test]
fn test_bytes_to_f64_timestamp() {
    let ts: i64 = 1712500000000; // a timestamp in millis
    let bytes = ts.to_be_bytes();
    let result = bytes_to_f64(&bytes, CONVERTER_TYPE_TIMESTAMP);
    assert_eq!(result, Some(1712500000000.0));
}

#[test]
fn test_bytes_to_f64_unknown_type() {
    let result = bytes_to_f64(&[1, 2, 3, 4, 5, 6, 7, 8], 0xFF);
    assert_eq!(result, None);
}

#[test]
fn test_is_numeric_type() {
    assert!(is_numeric_type(CONVERTER_TYPE_U8));
    assert!(is_numeric_type(CONVERTER_TYPE_U16));
    assert!(is_numeric_type(CONVERTER_TYPE_U32));
    assert!(is_numeric_type(CONVERTER_TYPE_U64));
    assert!(is_numeric_type(CONVERTER_TYPE_I64));
    assert!(is_numeric_type(CONVERTER_TYPE_F64));
    assert!(!is_numeric_type(CONVERTER_TYPE_STRING));
    assert!(!is_numeric_type(CONVERTER_TYPE_TIMESTAMP));
    assert!(!is_numeric_type(0xFF));
}

#[test]
fn test_bytes_to_json_value_null_on_too_short() {
    assert_eq!(bytes_to_json_value(&[], CONVERTER_TYPE_U8), serde_json::Value::Null);
    assert_eq!(bytes_to_json_value(&[1], CONVERTER_TYPE_U16), serde_json::Value::Null);
    assert_eq!(bytes_to_json_value(&[1, 2], CONVERTER_TYPE_U32), serde_json::Value::Null);
    assert_eq!(bytes_to_json_value(&[1, 2, 3, 4], CONVERTER_TYPE_U64), serde_json::Value::Null);
}

#[test]
fn test_bytes_to_json_value_unknown_type_utf8() {
    // Unknown type tag, valid UTF-8
    let result = bytes_to_json_value("hello".as_bytes(), 0xFF);
    assert_eq!(result, serde_json::json!("hello"));
}

#[test]
fn test_bytes_to_json_value_unknown_type_non_utf8() {
    // Unknown type tag, invalid UTF-8 -> hex encoded
    let result = bytes_to_json_value(&[0xFF, 0xFE, 0xFD], 0xFF);
    assert_eq!(result, serde_json::json!("fffefd"));
}

#[test]
fn test_aggregate_no_aggregate_query() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    // Query without aggregate field set
    let query = Query {
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
    };
    let result = qe.execute_aggregate(&query);
    assert!(result.is_err(), "should error when no aggregate query is set");
}

#[test]
fn test_group_by_sorted_by_count_desc() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    // Both groups have 10 people each, so order doesn't matter for equality,
    // but the sort should still work without panicking
    let agg = AggregateQuery {
        count: true,
        group_by: vec!["department".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    let groups = result.groups.as_ref().unwrap();
    assert_eq!(groups.len(), 2);
    // Both have count=10, verify sort is stable (no crash)
    assert!(groups[0].count >= groups[1].count);
}

#[test]
fn test_group_by_with_sum_and_min_max() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    let agg = AggregateQuery {
        count: true,
        sum: vec!["salary".to_string()],
        min: vec!["age".to_string()],
        max: vec!["age".to_string()],
        group_by: vec!["department".to_string()],
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    let groups = result.groups.as_ref().unwrap();
    for group in groups {
        let dept = group.key.get("department").unwrap().as_str().unwrap();
        assert!(group.sum.contains_key("salary"));
        assert!(group.min.contains_key("age"));
        assert!(group.max.contains_key("age"));

        if dept == "engineering" {
            // Engineering: indices 0,2,4,...,18 -> salaries 50000,60000,...,140000
            // sum = 50000+60000+70000+80000+90000+100000+110000+120000+130000+140000 = 950000
            assert_eq!(*group.sum.get("salary").unwrap(), 950_000.0);
            assert_eq!(*group.min.get("age").unwrap(), serde_json::json!(20u64));
            assert_eq!(*group.max.get("age").unwrap(), serde_json::json!(38u64));
        } else {
            // Sales: indices 1,3,5,...,19 -> salaries 55000,65000,...,145000
            // sum = 55000+65000+75000+85000+95000+105000+115000+125000+135000+145000 = 1000000
            assert_eq!(*group.sum.get("salary").unwrap(), 1_000_000.0);
            assert_eq!(*group.min.get("age").unwrap(), serde_json::json!(21u64));
            assert_eq!(*group.max.get("age").unwrap(), serde_json::json!(39u64));
        }
    }
}

#[test]
fn test_count_only_no_fields() {
    let dir = tempfile::tempdir().unwrap();
    let engine = setup_people_engine(&dir);
    let qe = QueryEngine::new(&engine);

    // Just count, no sum/avg/min/max/group_by
    let agg = AggregateQuery {
        count: true,
        ..Default::default()
    };
    let query = make_all_people_query(agg, None);
    let result = qe.execute_aggregate(&query).unwrap();

    assert_eq!(result.count, Some(20));
    assert!(result.sum.is_empty());
    assert!(result.avg.is_empty());
    assert!(result.min.is_empty());
    assert!(result.max.is_empty());
    assert!(result.groups.is_none());
}

#[test]
fn test_aggregate_result_serialization() {
    use serde_json;

    let result = aeordb::engine::query_engine::AggregateResult {
        count: Some(10),
        sum: {
            let mut m = std::collections::HashMap::new();
            m.insert("salary".to_string(), 500000.0);
            m
        },
        avg: std::collections::HashMap::new(),
        min: std::collections::HashMap::new(),
        max: std::collections::HashMap::new(),
        groups: None,
        has_more: false,
        default_limit_hit: false,
    };

    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["count"], serde_json::json!(10));
    assert_eq!(json["sum"]["salary"], serde_json::json!(500000.0));
    assert_eq!(json["has_more"], serde_json::json!(false));
    // Empty maps should be skipped
    assert!(json.get("avg").is_none());
    assert!(json.get("min").is_none());
    assert!(json.get("max").is_none());
    assert!(json.get("groups").is_none());
    // default_limit_hit is false, should be skipped
    assert!(json.get("default_limit_hit").is_none());
}
