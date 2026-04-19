use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::query_engine::{
    QueryEngine, Query, QueryNode, FieldQuery, QueryOp, QueryStrategy, ExplainMode, ExplainResult,
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

fn make_user_json(name: &str, age: u64) -> Vec<u8> {
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

fn setup_users_engine(dir: &tempfile::TempDir) -> StorageEngine {
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
  store_index_config(&engine, "/users", &config);

  ops.store_file_with_indexing(&ctx,
    "/users/alice.json",
    &make_user_json("Alice", 30),
    Some("application/json"),
  ).unwrap();

  ops.store_file_with_indexing(&ctx,
    "/users/bob.json",
    &make_user_json("Bob", 25),
    Some("application/json"),
  ).unwrap();

  ops.store_file_with_indexing(&ctx,
    "/users/charlie.json",
    &make_user_json("Charlie", 40),
    Some("application/json"),
  ).unwrap();

  engine
}

#[test]
fn test_execute_explain_plan_mode() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);
  let qe = QueryEngine::new(&engine);

  let query = Query {
    path: "/users/".to_string(),
    field_queries: vec![],
    node: Some(QueryNode::Field(FieldQuery {
      field_name: "age".to_string(),
      operation: QueryOp::Gt(25u64.to_be_bytes().to_vec()),
    })),
    limit: Some(10),
    offset: None,
    order_by: vec![],
    after: None,
    before: None,
    include_total: false,
    strategy: QueryStrategy::Full,
    aggregate: None,
    explain: ExplainMode::Plan,
  };

  let result = qe.execute_explain(&query).unwrap();

  // Plan should be present
  assert!(result.plan.is_object(), "plan should be an object");
  assert!(result.plan.get("query_tree").is_some(), "plan should have query_tree");
  assert!(result.plan.get("limit").is_some(), "plan should have limit");
  assert_eq!(result.plan["limit"], 10);

  // No execution or results in Plan mode
  assert!(result.execution.is_none(), "Plan mode should not have execution");
  assert!(result.results.is_none(), "Plan mode should not have results");
}

#[test]
fn test_execute_explain_analyze_mode() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);
  let qe = QueryEngine::new(&engine);

  let query = Query {
    path: "/users/".to_string(),
    field_queries: vec![],
    node: Some(QueryNode::Field(FieldQuery {
      field_name: "age".to_string(),
      operation: QueryOp::Gt(25u64.to_be_bytes().to_vec()),
    })),
    limit: Some(10),
    offset: None,
    order_by: vec![],
    after: None,
    before: None,
    include_total: false,
    strategy: QueryStrategy::Full,
    aggregate: None,
    explain: ExplainMode::Analyze,
  };

  let result = qe.execute_explain(&query).unwrap();

  // All three sections should be present
  assert!(result.plan.is_object(), "plan should be present");
  assert!(result.execution.is_some(), "Analyze mode should have execution");
  assert!(result.results.is_some(), "Analyze mode should have results");

  let execution = result.execution.unwrap();
  assert!(execution.get("total_duration_ms").is_some());
  assert!(execution["total_duration_ms"].as_f64().unwrap() >= 0.0);
  assert!(execution.get("results_returned").is_some());
}

#[test]
fn test_explain_result_serializes() {
  let result = ExplainResult {
    plan: serde_json::json!({"type": "test"}),
    execution: Some(serde_json::json!({"total_duration_ms": 1.5})),
    results: Some(serde_json::json!({"items": []})),
  };

  let json = serde_json::to_value(&result).unwrap();
  assert!(json.is_object());
  assert!(json.get("plan").is_some());
  assert!(json.get("execution").is_some());
  assert!(json.get("results").is_some());
}

#[test]
fn test_explain_result_serializes_without_optional_fields() {
  let result = ExplainResult {
    plan: serde_json::json!({"type": "test"}),
    execution: None,
    results: None,
  };

  let json = serde_json::to_value(&result).unwrap();
  assert!(json.is_object());
  assert!(json.get("plan").is_some());
  // Optional fields should be absent (skip_serializing_if)
  assert!(json.get("execution").is_none());
  assert!(json.get("results").is_none());
}

#[test]
fn test_explain_plan_shows_query_tree_structure() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);
  let qe = QueryEngine::new(&engine);

  let query = Query {
    path: "/users/".to_string(),
    field_queries: vec![],
    node: Some(QueryNode::Or(vec![
      QueryNode::Field(FieldQuery {
        field_name: "age".to_string(),
        operation: QueryOp::Eq(25u64.to_be_bytes().to_vec()),
      }),
      QueryNode::Field(FieldQuery {
        field_name: "age".to_string(),
        operation: QueryOp::Eq(30u64.to_be_bytes().to_vec()),
      }),
    ])),
    limit: None,
    offset: None,
    order_by: vec![],
    after: None,
    before: None,
    include_total: false,
    strategy: QueryStrategy::Full,
    aggregate: None,
    explain: ExplainMode::Plan,
  };

  let result = qe.execute_explain(&query).unwrap();
  let tree = result.plan.get("query_tree").unwrap();
  assert_eq!(tree["type"], "or");
  assert_eq!(tree["children"].as_array().unwrap().len(), 2);

  // OR should trigger bitmap compositing
  assert_eq!(result.plan["bitmap_compositing"], true);
}

#[test]
fn test_explain_plan_shows_index_info() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);
  let qe = QueryEngine::new(&engine);

  let query = Query {
    path: "/users/".to_string(),
    field_queries: vec![],
    node: Some(QueryNode::Field(FieldQuery {
      field_name: "age".to_string(),
      operation: QueryOp::Eq(30u64.to_be_bytes().to_vec()),
    })),
    limit: None,
    offset: None,
    order_by: vec![],
    after: None,
    before: None,
    include_total: false,
    strategy: QueryStrategy::Full,
    aggregate: None,
    explain: ExplainMode::Plan,
  };

  let result = qe.execute_explain(&query).unwrap();
  let tree = result.plan.get("query_tree").unwrap();
  let indexes = tree["indexes"].as_array().unwrap();
  assert!(!indexes.is_empty(), "should have index information");

  let idx = &indexes[0];
  assert!(idx.get("strategy").is_some());
  assert!(idx.get("type").is_some());
  assert!(idx.get("entries").is_some());
  assert!(idx.get("order_preserving").is_some());
  assert!(idx.get("values_stored").is_some());
}

#[test]
fn test_explain_plan_no_node_returns_plan() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);
  let qe = QueryEngine::new(&engine);

  // Query with no node (empty where clause)
  let query = Query {
    path: "/users/".to_string(),
    field_queries: vec![],
    node: None,
    limit: Some(5),
    offset: None,
    order_by: vec![],
    after: None,
    before: None,
    include_total: false,
    strategy: QueryStrategy::Full,
    aggregate: None,
    explain: ExplainMode::Plan,
  };

  let result = qe.execute_explain(&query).unwrap();
  // Should still return a plan, just without query_tree
  assert!(result.plan.is_object());
  assert_eq!(result.plan["limit"], 5);
  assert!(result.plan.get("query_tree").is_none());
}

#[test]
fn test_explain_analyze_returns_actual_results() {
  let dir = tempfile::tempdir().unwrap();
  let engine = setup_users_engine(&dir);
  let qe = QueryEngine::new(&engine);

  let query = Query {
    path: "/users/".to_string(),
    field_queries: vec![],
    node: Some(QueryNode::Field(FieldQuery {
      field_name: "age".to_string(),
      operation: QueryOp::Eq(30u64.to_be_bytes().to_vec()),
    })),
    limit: None,
    offset: None,
    order_by: vec![],
    after: None,
    before: None,
    include_total: false,
    strategy: QueryStrategy::Full,
    aggregate: None,
    explain: ExplainMode::Analyze,
  };

  let result = qe.execute_explain(&query).unwrap();
  let results = result.results.unwrap();
  let result_array = results["items"].as_array().unwrap();
  assert_eq!(result_array.len(), 1, "Alice (age 30) should be the only match");
  assert_eq!(result_array[0]["path"], "/users/alice.json");
}
