use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::StorageEngine;
use aeordb::engine::RequestContext;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

fn rebuild_app(
  jwt_manager: &Arc<JwtManager>,
  engine: &Arc<StorageEngine>,
) -> axum::Router {
  create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

fn bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "test-admin".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

fn make_user_json(name: &str, age: u64, email: &str) -> Vec<u8> {
  format!(
    r#"{{"name":"{}","age":{},"email":"{}"}}"#,
    name, age, email,
  ).into_bytes()
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

fn setup_users(engine: &StorageEngine) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);

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
  store_index_config(engine, "/myapp/users", &config);

  ops.store_file_with_indexing(&ctx,
    "/myapp/users/alice.json",
    &make_user_json("Alice", 30, "alice@test.com"),
    Some("application/json"),
  ).unwrap();

  ops.store_file_with_indexing(&ctx,
    "/myapp/users/bob.json",
    &make_user_json("Bob", 25, "bob@test.com"),
    Some("application/json"),
  ).unwrap();

  ops.store_file_with_indexing(&ctx,
    "/myapp/users/charlie.json",
    &make_user_json("Charlie", 40, "charlie@test.com"),
    Some("application/json"),
  ).unwrap();
}

async fn query_with_explain(
  app: axum::Router,
  auth: &str,
  where_clause: serde_json::Value,
  explain: serde_json::Value,
) -> serde_json::Value {
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": where_clause,
    "explain": explain,
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  body_json(response.into_body()).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_explain_plan_returns_plan() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let json = query_with_explain(
    app,
    &auth,
    serde_json::json!([{ "field": "age", "op": "gt", "value": 20 }]),
    serde_json::json!(true),
  ).await;

  // Should have plan, no execution, no results
  assert!(json.get("plan").is_some(), "should have 'plan'");
  assert!(json.get("execution").is_none(), "plan mode should not have 'execution'");
  assert!(json.get("results").is_none(), "plan mode should not have 'results'");
}

#[tokio::test]
async fn test_explain_analyze_returns_plan_and_results() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let json = query_with_explain(
    app,
    &auth,
    serde_json::json!([{ "field": "age", "op": "gt", "value": 20 }]),
    serde_json::json!("analyze"),
  ).await;

  // Should have plan, execution, and results
  assert!(json.get("plan").is_some(), "should have 'plan'");
  assert!(json.get("execution").is_some(), "analyze mode should have 'execution'");
  assert!(json.get("results").is_some(), "analyze mode should have 'results'");
}

#[tokio::test]
async fn test_explain_shows_indexes() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let json = query_with_explain(
    app,
    &auth,
    serde_json::json!({ "field": "age", "op": "eq", "value": 30 }),
    serde_json::json!(true),
  ).await;

  let plan = json.get("plan").unwrap();
  let tree = plan.get("query_tree").unwrap();
  let indexes = tree.get("indexes").unwrap().as_array().unwrap();

  assert!(!indexes.is_empty(), "should list at least one index");
  let idx = &indexes[0];
  assert!(idx.get("strategy").is_some(), "index info should have 'strategy'");
  assert!(idx.get("type").is_some(), "index info should have 'type'");
  assert!(idx.get("entries").is_some(), "index info should have 'entries'");
}

#[tokio::test]
async fn test_explain_shows_operation() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let json = query_with_explain(
    app,
    &auth,
    serde_json::json!({ "field": "age", "op": "gt", "value": 20 }),
    serde_json::json!(true),
  ).await;

  let plan = json.get("plan").unwrap();
  let tree = plan.get("query_tree").unwrap();
  assert_eq!(tree.get("operation").unwrap(), "gt");
  assert_eq!(tree.get("field").unwrap(), "age");
}

#[tokio::test]
async fn test_explain_shows_recheck_false_for_exact() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let json = query_with_explain(
    app,
    &auth,
    serde_json::json!({ "field": "age", "op": "eq", "value": 30 }),
    serde_json::json!(true),
  ).await;

  let tree = json["plan"]["query_tree"].clone();
  assert_eq!(tree["recheck"], false, "exact query should not need recheck");
}

#[tokio::test]
async fn test_explain_shows_bitmap_compositing_for_or() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let json = query_with_explain(
    app,
    &auth,
    serde_json::json!({
      "or": [
        { "field": "age", "op": "eq", "value": 25 },
        { "field": "age", "op": "eq", "value": 40 }
      ]
    }),
    serde_json::json!(true),
  ).await;

  let plan = json.get("plan").unwrap();
  assert_eq!(plan["bitmap_compositing"], true, "OR query should use bitmap compositing");
}

#[tokio::test]
async fn test_explain_shows_order_by() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [{ "field": "age", "op": "gt", "value": 20 }],
    "order_by": [{ "field": "@score", "direction": "desc" }],
    "explain": true,
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &bearer_token(&jwt_manager))
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let plan = json.get("plan").unwrap();
  let order_by = plan.get("order_by").unwrap().as_array().unwrap();
  assert_eq!(order_by.len(), 1);
  assert_eq!(order_by[0]["field"], "@score");
  assert_eq!(order_by[0]["direction"], "desc");
}

#[tokio::test]
async fn test_explain_shows_aggregate() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [{ "field": "age", "op": "gt", "value": 20 }],
    "aggregate": { "count": true, "sum": ["age"] },
    "explain": true,
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &bearer_token(&jwt_manager))
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let plan = json.get("plan").unwrap();
  let agg = plan.get("aggregate").unwrap();
  assert_eq!(agg["count"], true);
  assert_eq!(agg["sum"], serde_json::json!(["age"]));
}

#[tokio::test]
async fn test_explain_analyze_timing() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let json = query_with_explain(
    app,
    &auth,
    serde_json::json!([{ "field": "age", "op": "gt", "value": 20 }]),
    serde_json::json!("analyze"),
  ).await;

  let execution = json.get("execution").unwrap();
  assert!(execution.get("total_duration_ms").is_some(), "should have total_duration_ms");
  let duration = execution["total_duration_ms"].as_f64().unwrap();
  assert!(duration >= 0.0, "duration should be non-negative");
  assert!(execution.get("candidates_generated").is_some());
  assert!(execution.get("results_returned").is_some());
}

#[tokio::test]
async fn test_explain_off_returns_normal() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // No explain field — normal response
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [{ "field": "age", "op": "gt", "value": 20 }],
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  // Normal response has results array, not plan
  assert!(json.get("results").is_some());
  assert!(json.get("plan").is_none());
}

#[tokio::test]
async fn test_explain_plan_string_mode() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "plan" string should also trigger plan mode
  let json = query_with_explain(
    app,
    &auth,
    serde_json::json!([{ "field": "age", "op": "eq", "value": 30 }]),
    serde_json::json!("plan"),
  ).await;

  assert!(json.get("plan").is_some());
  assert!(json.get("execution").is_none());
  assert!(json.get("results").is_none());
}

#[tokio::test]
async fn test_explain_false_returns_normal() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // explain: false should be treated as Off
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [{ "field": "age", "op": "gt", "value": 20 }],
    "explain": false,
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert!(json.get("results").is_some(), "explain:false should return normal results");
  assert!(json.get("plan").is_none(), "explain:false should not return plan");
}

#[tokio::test]
async fn test_explain_plan_shows_limit() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [{ "field": "age", "op": "gt", "value": 20 }],
    "limit": 5,
    "explain": true,
  });

  let request = Request::builder()
    .method("POST")
    .uri("/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let plan = json.get("plan").unwrap();
  assert_eq!(plan["limit"], 5);
}

#[tokio::test]
async fn test_explain_and_query_tree() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // AND query
  let json = query_with_explain(
    app,
    &auth,
    serde_json::json!([
      { "field": "age", "op": "gt", "value": 20 },
      { "field": "name", "op": "eq", "value": "Alice" }
    ]),
    serde_json::json!(true),
  ).await;

  let tree = &json["plan"]["query_tree"];
  assert_eq!(tree["type"], "and");
  let children = tree["children"].as_array().unwrap();
  assert_eq!(children.len(), 2);
  assert_eq!(children[0]["type"], "field");
  assert_eq!(children[1]["type"], "field");
}

#[tokio::test]
async fn test_explain_not_query_tree() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let json = query_with_explain(
    app,
    &auth,
    serde_json::json!({
      "not": { "field": "age", "op": "eq", "value": 30 }
    }),
    serde_json::json!(true),
  ).await;

  let tree = &json["plan"]["query_tree"];
  assert_eq!(tree["type"], "not");
  assert!(tree.get("child").is_some());
  assert_eq!(tree["child"]["type"], "field");
}
