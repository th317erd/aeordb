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

/// Create a fresh app with engine support.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

/// Rebuild app from shared state (multi-request tests).
fn rebuild_app(
  jwt_manager: &Arc<JwtManager>,
  engine: &Arc<StorageEngine>,
) -> axum::Router {
  create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

/// Create an admin Bearer token.
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

/// Collect response body into JSON.
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

/// Set up the engine with indexed user data.
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

  ops.store_file_with_indexing(&ctx,
    "/myapp/users/diana.json",
    &make_user_json("Diana", 35, "diana@test.com"),
    Some("application/json"),
  ).unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_query_exact_match() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "eq", "value": 30 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/myapp/users/alice.json");
}

#[tokio::test]
async fn test_query_gt() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "gt", "value": 30 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 2);

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/myapp/users/charlie.json"));
  assert!(paths.contains(&"/myapp/users/diana.json"));
}

#[tokio::test]
async fn test_query_lt() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "lt", "value": 30 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/myapp/users/bob.json");
}

#[tokio::test]
async fn test_query_between() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "between", "value": 28, "value2": 36 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 2);

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/myapp/users/alice.json"));
  assert!(paths.contains(&"/myapp/users/diana.json"));
}

#[tokio::test]
async fn test_query_multiple_fields() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "gt", "value": 25 },
      { "field": "name", "op": "eq", "value": "Alice" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/myapp/users/alice.json");
}

#[tokio::test]
async fn test_query_with_limit() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "gt", "value": 0 }
    ],
    "limit": 2
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert!(results.len() <= 2);
}

#[tokio::test]
async fn test_query_empty_results() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "gt", "value": 999 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert!(results.is_empty());
}

#[tokio::test]
async fn test_query_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "eq", "value": 30 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    // No authorization header
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_query_invalid_body_returns_400() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"this is not valid"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  // axum returns 422 for deserialization failures by default
  let status = response.status();
  assert!(
    status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
    "Expected 400 or 422, got {}",
    status,
  );
}

#[tokio::test]
async fn test_query_nonexistent_path() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/nonexistent/path",
    "where": [
      { "field": "age", "op": "eq", "value": 30 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  // Should be 404 since the index does not exist
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_query_between_missing_value2_returns_400() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "between", "value": 20 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_query_unknown_op_returns_400() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "like", "value": 30 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_query_response_contains_metadata_fields() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "eq", "value": 30 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);

  let result = &results[0];
  assert!(result["path"].is_string());
  assert!(result["size"].is_number());
  assert!(result["created_at"].is_number());
  assert!(result["updated_at"].is_number());
  // content_type may be null or a string
  assert!(result["content_type"].is_null() || result["content_type"].is_string());
}

#[tokio::test]
async fn test_query_with_string_value() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "name", "op": "eq", "value": "Bob" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/myapp/users/bob.json");
}

#[tokio::test]
async fn test_query_with_boolean_value() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Boolean values should not crash the endpoint even though no index exists
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "active", "op": "eq", "value": true }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  // We expect 404 because the index for "active" does not exist, not a crash
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_query_empty_where_returns_empty_array() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": []
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert!(results.is_empty());
}

// ===========================================================================
// Task 8: HTTP Query API with Boolean Logic
// ===========================================================================

#[tokio::test]
async fn test_query_json_boolean_and() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": {
      "and": [
        { "field": "age", "op": "gt", "value": 25 },
        { "field": "name", "op": "eq", "value": "Alice" }
      ]
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/myapp/users/alice.json");
}

#[tokio::test]
async fn test_query_json_boolean_or() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": {
      "or": [
        { "field": "age", "op": "eq", "value": 25 },
        { "field": "age", "op": "eq", "value": 40 }
      ]
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 2);

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/myapp/users/bob.json"));
  assert!(paths.contains(&"/myapp/users/charlie.json"));
}

#[tokio::test]
async fn test_query_json_boolean_not() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": {
      "not": { "field": "age", "op": "eq", "value": 30 }
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 3);

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(!paths.contains(&"/myapp/users/alice.json"));
  assert!(paths.contains(&"/myapp/users/bob.json"));
  assert!(paths.contains(&"/myapp/users/charlie.json"));
  assert!(paths.contains(&"/myapp/users/diana.json"));
}

#[tokio::test]
async fn test_query_json_nested_boolean() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // (age > 25) AND (name == "Alice" OR name == "Charlie") AND NOT(age == 40)
  // Result: Alice only
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": {
      "and": [
        { "field": "age", "op": "gt", "value": 25 },
        { "or": [
          { "field": "name", "op": "eq", "value": "Alice" },
          { "field": "name", "op": "eq", "value": "Charlie" }
        ]},
        { "not": { "field": "age", "op": "eq", "value": 40 } }
      ]
    },
    "limit": 100
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/myapp/users/alice.json");
}

#[tokio::test]
async fn test_query_json_backward_compatible_array() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Legacy array format still works.
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "gt", "value": 30 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 2);

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/myapp/users/charlie.json"));
  assert!(paths.contains(&"/myapp/users/diana.json"));
}

#[tokio::test]
async fn test_query_json_in_operation() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "in", "value": [25, 40] }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 2);

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/myapp/users/bob.json"));
  assert!(paths.contains(&"/myapp/users/charlie.json"));
}

#[tokio::test]
async fn test_query_json_invalid_boolean_structure() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Invalid: where is an object with no recognized keys.
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": {
      "invalid_key": true
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_query_json_in_with_string_values() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_users(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "name", "op": "in", "value": ["Bob", "Diana"] }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 2);

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/myapp/users/bob.json"));
  assert!(paths.contains(&"/myapp/users/diana.json"));
}

#[tokio::test]
async fn test_query_json_in_non_array_returns_400() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // "in" requires array value, not a scalar.
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "in", "value": 30 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_query_json_or_missing_field_returns_error() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // OR with a clause missing "field" key.
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": {
      "or": [
        { "op": "eq", "value": 30 }
      ]
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Virtual field query tests
// ---------------------------------------------------------------------------

/// Store files with varied names, sizes, and content types for virtual field tests.
fn setup_virtual_field_files(engine: &StorageEngine) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);

  ops.store_file(&ctx, "/docs/quarterly-report.pdf", b"fake pdf data for quarterly report",
    Some("application/pdf")).unwrap();
  ops.store_file(&ctx, "/docs/annual-report.pdf", b"fake pdf data for annual report document",
    Some("application/pdf")).unwrap();
  ops.store_file(&ctx, "/docs/notes.txt", b"some plain text notes",
    Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/images/photo.jpeg", b"fake jpeg image data bytes here",
    Some("image/jpeg")).unwrap();
  ops.store_file(&ctx, "/images/logo.png", b"ab",
    Some("image/png")).unwrap();
}

#[tokio::test]
async fn test_virtual_field_filename_contains() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/docs",
    "where": [
      { "field": "@filename", "op": "contains", "value": "report" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 2, "Expected 2 files with 'report' in filename, got: {:?}",
    results.iter().map(|r| r["path"].as_str().unwrap()).collect::<Vec<_>>());

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/docs/quarterly-report.pdf"));
  assert!(paths.contains(&"/docs/annual-report.pdf"));
}

#[tokio::test]
async fn test_virtual_field_extension_eq() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/docs",
    "where": [
      { "field": "@extension", "op": "eq", "value": "pdf" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 2, "Expected 2 PDF files, got: {:?}",
    results.iter().map(|r| r["path"].as_str().unwrap()).collect::<Vec<_>>());

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/docs/quarterly-report.pdf"));
  assert!(paths.contains(&"/docs/annual-report.pdf"));
}

#[tokio::test]
async fn test_virtual_field_content_type_eq() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/images",
    "where": [
      { "field": "@content_type", "op": "eq", "value": "image/jpeg" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/images/photo.jpeg");
}

#[tokio::test]
async fn test_virtual_field_size_gt() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "fake jpeg image data bytes here" = 31 bytes
  // "ab" = 2 bytes
  // Query for files > 10 bytes should find photo.jpeg but not logo.png
  let body = serde_json::json!({
    "path": "/images",
    "where": [
      { "field": "@size", "op": "gt", "value": 10 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1, "Expected 1 file > 10 bytes, got: {:?}",
    results.iter().map(|r| r["path"].as_str().unwrap()).collect::<Vec<_>>());
  assert_eq!(results[0]["path"], "/images/photo.jpeg");
}

#[tokio::test]
async fn test_virtual_field_path_contains() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Search from root for files whose path contains "/docs/"
  let body = serde_json::json!({
    "path": "/",
    "where": [
      { "field": "@path", "op": "contains", "value": "/docs/" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 3, "Expected 3 files under /docs/, got: {:?}",
    results.iter().map(|r| r["path"].as_str().unwrap()).collect::<Vec<_>>());
}

#[tokio::test]
async fn test_virtual_field_created_at_gt() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Use a timestamp from the past — all files created "now" should be newer.
  let body = serde_json::json!({
    "path": "/docs",
    "where": [
      { "field": "@created_at", "op": "gt", "value": 1000000000000_i64 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  // All 3 docs files were just created, so they should all be newer than 2001.
  assert_eq!(results.len(), 3, "Expected all 3 /docs/ files with created_at > 1000000000000, got: {:?}",
    results.iter().map(|r| r["path"].as_str().unwrap()).collect::<Vec<_>>());
}

#[tokio::test]
async fn test_virtual_field_combined_and() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Combine two virtual fields via AND:
  // @extension eq "pdf" AND @filename contains "annual"
  // Should match only annual-report.pdf
  let body = serde_json::json!({
    "path": "/docs",
    "where": [
      { "field": "@extension", "op": "eq", "value": "pdf" },
      { "field": "@filename", "op": "contains", "value": "annual" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1, "Expected only annual-report.pdf, got: {:?}",
    results.iter().map(|r| r["path"].as_str().unwrap()).collect::<Vec<_>>());
  assert_eq!(results[0]["path"], "/docs/annual-report.pdf");
}

#[tokio::test]
async fn test_virtual_field_boolean_or() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // OR: @extension eq "txt" OR @extension eq "pdf"
  // Should match all 3 files under /docs
  let body = serde_json::json!({
    "path": "/docs",
    "where": {
      "or": [
        { "field": "@extension", "op": "eq", "value": "txt" },
        { "field": "@extension", "op": "eq", "value": "pdf" }
      ]
    }
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 3, "Expected all 3 /docs/ files (2 pdf + 1 txt), got: {:?}",
    results.iter().map(|r| r["path"].as_str().unwrap()).collect::<Vec<_>>());
}

#[tokio::test]
async fn test_virtual_field_empty_directory_returns_empty() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();

  // Create an empty directory.
  let ops = DirectoryOps::new(&engine);
  ops.create_directory(&RequestContext::system(), "/empty").unwrap();

  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/empty",
    "where": [
      { "field": "@filename", "op": "contains", "value": "anything" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 0);
}

#[tokio::test]
async fn test_virtual_field_unknown_field_returns_error() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/docs",
    "where": [
      { "field": "@nonexistent", "op": "eq", "value": "test" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  // Invalid virtual field should produce an error response (400 or 404).
  assert!(
    response.status() == StatusCode::BAD_REQUEST
      || response.status() == StatusCode::INTERNAL_SERVER_ERROR
      || response.status() == StatusCode::NOT_FOUND,
    "Expected error status for unknown virtual field, got: {}",
    response.status(),
  );
}

#[tokio::test]
async fn test_virtual_field_nonexistent_directory_returns_empty() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/nonexistent",
    "where": [
      { "field": "@filename", "op": "eq", "value": "anything" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 0);
}

#[tokio::test]
async fn test_virtual_field_size_between() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "some plain text notes" = 21 bytes, fits between 10 and 25
  // "fake pdf data for quarterly report" = 34 bytes, too large
  // "fake pdf data for annual report document" = 40 bytes, too large
  let body = serde_json::json!({
    "path": "/docs",
    "where": [
      { "field": "@size", "op": "between", "value": 10, "value2": 25 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1, "Expected 1 file between 10-25 bytes, got: {:?}",
    results.iter().map(|r| r["path"].as_str().unwrap()).collect::<Vec<_>>());
  assert_eq!(results[0]["path"], "/docs/notes.txt");
}

#[tokio::test]
async fn test_virtual_field_content_type_contains() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Find all image files (content_type starts with "image/")
  let body = serde_json::json!({
    "path": "/",
    "where": [
      { "field": "@content_type", "op": "contains", "value": "image/" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 2, "Expected 2 image files, got: {:?}",
    results.iter().map(|r| r["path"].as_str().unwrap()).collect::<Vec<_>>());

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  assert!(paths.contains(&"/images/photo.jpeg"));
  assert!(paths.contains(&"/images/logo.png"));
}

#[tokio::test]
async fn test_virtual_field_filename_eq() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/docs",
    "where": [
      { "field": "@filename", "op": "eq", "value": "notes.txt" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/docs/notes.txt");
}

#[tokio::test]
async fn test_virtual_field_size_eq() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "ab" = 2 bytes exactly
  let body = serde_json::json!({
    "path": "/images",
    "where": [
      { "field": "@size", "op": "eq", "value": 2 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/images/logo.png");
}

#[tokio::test]
async fn test_virtual_field_size_lt() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // "ab" = 2 bytes, should be the only file < 10 bytes in /images
  let body = serde_json::json!({
    "path": "/images",
    "where": [
      { "field": "@size", "op": "lt", "value": 10 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/images/logo.png");
}

#[tokio::test]
async fn test_virtual_field_updated_at_gt() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // Use a far-future timestamp — no files should match.
  let body = serde_json::json!({
    "path": "/docs",
    "where": [
      { "field": "@updated_at", "op": "gt", "value": 9999999999999_i64 }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 0, "No files should have updated_at in the far future");
}

#[tokio::test]
async fn test_virtual_field_path_eq() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/",
    "where": [
      { "field": "@path", "op": "eq", "value": "/images/photo.jpeg" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 1);
  assert_eq!(results[0]["path"], "/images/photo.jpeg");
}

#[tokio::test]
async fn test_virtual_field_no_match_returns_empty() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  setup_virtual_field_files(&engine);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/docs",
    "where": [
      { "field": "@filename", "op": "contains", "value": "zzz_no_match_zzz" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let results = json["items"].as_array().unwrap();
  assert_eq!(results.len(), 0);
}


// ---------------------------------------------------------------------------
// Virtual field fuzzy/phonetic/similar query tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_virtual_field_filename_similar() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_virtual_field_files(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/docs",
        "where": [{ "field": "@filename", "op": "similar", "value": "report", "threshold": 0.2 }]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/files/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let results = json["items"].as_array().unwrap();
    assert!(!results.is_empty(), "similar search should find files with names similar to 'report'");
}

#[tokio::test]
async fn test_virtual_field_filename_fuzzy() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_virtual_field_files(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/docs",
        "where": [{ "field": "@filename", "op": "fuzzy", "value": "notes.txt", "fuzziness": 2 }]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/files/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let results = json["items"].as_array().unwrap();
    assert!(!results.is_empty(), "fuzzy search should find 'notes.txt' within edit distance 2");
}

#[tokio::test]
async fn test_virtual_field_filename_match() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_virtual_field_files(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/docs",
        "where": [{ "field": "@filename", "op": "match", "value": "report" }]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/files/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let results = json["items"].as_array().unwrap();
    assert!(!results.is_empty(), "match search should find files matching 'report'");
}

#[tokio::test]
async fn test_virtual_field_path_similar() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    setup_virtual_field_files(&engine);
    let app = rebuild_app(&jwt_manager, &engine);
    let auth = bearer_token(&jwt_manager);

    let body = serde_json::json!({
        "path": "/docs",
        "where": [{ "field": "@path", "op": "similar", "value": "/docs/report.pdf", "threshold": 0.3 }]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/files/query")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let results = json["items"].as_array().unwrap();
    assert!(!results.is_empty(), "similar search on @path should find matches");
}
