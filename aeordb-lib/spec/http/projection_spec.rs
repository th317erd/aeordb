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
    key_id: None,
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
    format!("{}/.aeordb-config/indexes.json", parent_path)
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

async fn query_with_select(
  app: axum::Router,
  auth: &str,
  select: Option<serde_json::Value>,
) -> serde_json::Value {
  let mut body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "gt", "value": 20 }
    ]
  });
  if let Some(s) = select {
    body["select"] = s;
  }

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
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
async fn test_select_filters_response() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let json = query_with_select(
    app,
    &auth,
    Some(serde_json::json!(["path", "score"])),
  ).await;

  let results = json["items"].as_array().unwrap();
  assert!(!results.is_empty());
  for result in results {
    let obj = result.as_object().unwrap();
    // Should only have "path" and "score"
    assert!(obj.contains_key("path"), "expected 'path' field");
    assert!(obj.contains_key("score"), "expected 'score' field");
    assert!(!obj.contains_key("size"), "should not have 'size'");
    assert!(!obj.contains_key("content_type"), "should not have 'content_type'");
    assert!(!obj.contains_key("created_at"), "should not have 'created_at'");
    assert!(!obj.contains_key("updated_at"), "should not have 'updated_at'");
    assert!(!obj.contains_key("matched_by"), "should not have 'matched_by'");
  }
}

#[tokio::test]
async fn test_select_virtual_fields() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Use @-prefixed virtual field names
  let json = query_with_select(
    app,
    &auth,
    Some(serde_json::json!(["@path", "@score"])),
  ).await;

  let results = json["items"].as_array().unwrap();
  assert!(!results.is_empty());
  for result in results {
    let obj = result.as_object().unwrap();
    // @path maps to "path", @score maps to "score"
    assert!(obj.contains_key("path"), "expected 'path' field");
    assert!(obj.contains_key("score"), "expected 'score' field");
    assert!(!obj.contains_key("size"), "should not have 'size'");
    assert!(!obj.contains_key("matched_by"), "should not have 'matched_by'");
  }
}

#[tokio::test]
async fn test_select_no_filter_without_select() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // No select -> full response
  let json = query_with_select(app, &auth, None).await;

  let results = json["items"].as_array().unwrap();
  assert!(!results.is_empty());
  for result in results {
    let obj = result.as_object().unwrap();
    // Should have all standard fields
    assert!(obj.contains_key("path"));
    assert!(obj.contains_key("score"));
    assert!(obj.contains_key("size"));
    assert!(obj.contains_key("content_type"));
    assert!(obj.contains_key("created_at"));
    assert!(obj.contains_key("updated_at"));
    assert!(obj.contains_key("matched_by"));
  }
}

#[tokio::test]
async fn test_select_empty_array() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Empty select -> no filtering (treated same as no select)
  let json = query_with_select(
    app,
    &auth,
    Some(serde_json::json!([])),
  ).await;

  let results = json["items"].as_array().unwrap();
  assert!(!results.is_empty());
  for result in results {
    let obj = result.as_object().unwrap();
    // All fields should be present
    assert!(obj.contains_key("path"));
    assert!(obj.contains_key("score"));
    assert!(obj.contains_key("size"));
  }
}

#[tokio::test]
async fn test_select_preserves_envelope() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let json = query_with_select(
    app,
    &auth,
    Some(serde_json::json!(["path"])),
  ).await;

  // Envelope fields are never stripped
  assert!(json.get("items").is_some(), "envelope 'items' should be present");
  assert!(json.get("has_more").is_some(), "envelope 'has_more' should be present");
}

#[tokio::test]
async fn test_select_on_aggregate_response() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);

  // Aggregate query with select — select should have no effect on aggregate envelope
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "gt", "value": 20 }
    ],
    "aggregate": { "count": true },
    "select": ["path"]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &bearer_token(&jwt_manager))
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  // Aggregate fields should still be present (not filtered)
  assert!(json.get("count").is_some(), "aggregate 'count' should be present");
}

#[tokio::test]
async fn test_select_unknown_field() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Select a field that doesn't exist in results
  let json = query_with_select(
    app,
    &auth,
    Some(serde_json::json!(["nonexistent"])),
  ).await;

  let results = json["items"].as_array().unwrap();
  assert!(!results.is_empty());
  for result in results {
    let obj = result.as_object().unwrap();
    // All known fields should be stripped, leaving empty objects
    assert!(!obj.contains_key("path"));
    assert!(!obj.contains_key("score"));
    assert!(!obj.contains_key("size"));
    assert!(obj.is_empty(), "objects should be empty when selecting nonexistent field");
  }
}

#[tokio::test]
async fn test_select_size_virtual_field() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // @size maps to size
  let json = query_with_select(
    app,
    &auth,
    Some(serde_json::json!(["@size"])),
  ).await;

  let results = json["items"].as_array().unwrap();
  assert!(!results.is_empty());
  for result in results {
    let obj = result.as_object().unwrap();
    assert!(obj.contains_key("size"), "expected 'size' from @size mapping");
    assert_eq!(obj.len(), 1, "should only have size");
  }
}

#[tokio::test]
async fn test_select_mixed_virtual_and_regular() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Mix virtual (@path) and regular (score) field names
  let json = query_with_select(
    app,
    &auth,
    Some(serde_json::json!(["@path", "score", "@content_type"])),
  ).await;

  let results = json["items"].as_array().unwrap();
  assert!(!results.is_empty());
  for result in results {
    let obj = result.as_object().unwrap();
    assert!(obj.contains_key("path"));
    assert!(obj.contains_key("score"));
    assert!(obj.contains_key("content_type"));
    assert_eq!(obj.len(), 3, "should only have the 3 selected fields");
  }
}
