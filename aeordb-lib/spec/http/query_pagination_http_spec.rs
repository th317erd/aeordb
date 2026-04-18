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

fn setup_people(engine: &StorageEngine, count: usize) {
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
  store_index_config(engine, "/people", &config);

  for i in 0..count {
    let age = 20 + i as u64;
    let name = format!("person_{:02}", i);
    let path = format!("/people/{}.json", name);
    ops.store_file_with_indexing(
      &ctx, &path, &make_person_json(&name, age), Some("application/json"),
    ).unwrap();
  }
}

async fn query_post(
  app: axum::Router,
  auth: &str,
  body: &serde_json::Value,
) -> (StatusCode, serde_json::Value) {
  let request = Request::builder()
    .method("POST")
    .uri("/files/query")
    .header("content-type", "application/json")
    .header("authorization", auth)
    .body(Body::from(serde_json::to_vec(body).unwrap()))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  let json = body_json(response.into_body()).await;
  (status, json)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_query_response_has_envelope() {
  let (_, jwt_manager, engine, _dir) = test_app();
  setup_people(&engine, 5);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "limit": 10
  });

  let (status, json) = query_post(app, &auth, &body).await;
  assert_eq!(status, StatusCode::OK);

  // Envelope fields
  assert!(json["results"].is_array(), "Response must have results array");
  assert!(json["has_more"].is_boolean(), "Response must have has_more boolean");
  assert_eq!(json["results"].as_array().unwrap().len(), 5);
  assert_eq!(json["has_more"], false);
}

#[tokio::test]
async fn test_query_with_order_by() {
  let (_, jwt_manager, engine, _dir) = test_app();
  setup_people(&engine, 10);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "order_by": [{ "field": "@path", "direction": "asc" }],
    "limit": 10
  });

  let (status, json) = query_post(app, &auth, &body).await;
  assert_eq!(status, StatusCode::OK);

  let results = json["results"].as_array().unwrap();
  assert_eq!(results.len(), 10);

  let paths: Vec<&str> = results.iter().map(|r| r["path"].as_str().unwrap()).collect();
  for i in 1..paths.len() {
    assert!(paths[i - 1] <= paths[i], "Not sorted: {} > {}", paths[i - 1], paths[i]);
  }
}

#[tokio::test]
async fn test_query_with_offset() {
  let (_, jwt_manager, engine, _dir) = test_app();
  setup_people(&engine, 10);
  let app1 = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body1 = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "order_by": [{ "field": "@path", "direction": "asc" }],
    "limit": 5
  });
  let (_, json1) = query_post(app1, &auth, &body1).await;
  let paths1: Vec<&str> = json1["results"].as_array().unwrap()
    .iter().map(|r| r["path"].as_str().unwrap()).collect();

  let app2 = rebuild_app(&jwt_manager, &engine);
  let body2 = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "order_by": [{ "field": "@path", "direction": "asc" }],
    "limit": 5,
    "offset": 5
  });
  let (_, json2) = query_post(app2, &auth, &body2).await;
  let paths2: Vec<&str> = json2["results"].as_array().unwrap()
    .iter().map(|r| r["path"].as_str().unwrap()).collect();

  assert_eq!(paths1.len(), 5);
  assert_eq!(paths2.len(), 5);
  // No overlap
  for p in &paths1 {
    assert!(!paths2.contains(p), "Overlap found: {}", p);
  }
}

#[tokio::test]
async fn test_query_with_cursor() {
  let (_, jwt_manager, engine, _dir) = test_app();
  setup_people(&engine, 15);
  let auth = bearer_token(&jwt_manager);

  // Page 1
  let app1 = rebuild_app(&jwt_manager, &engine);
  let body1 = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "order_by": [{ "field": "@path", "direction": "asc" }],
    "limit": 5
  });
  let (_, json1) = query_post(app1, &auth, &body1).await;
  assert_eq!(json1["has_more"], true);
  let next_cursor = json1["next_cursor"].as_str().unwrap();

  // Page 2
  let app2 = rebuild_app(&jwt_manager, &engine);
  let body2 = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "order_by": [{ "field": "@path", "direction": "asc" }],
    "limit": 5,
    "after": next_cursor
  });
  let (_, json2) = query_post(app2, &auth, &body2).await;
  assert_eq!(json2["results"].as_array().unwrap().len(), 5);

  // No overlap
  let paths1: Vec<&str> = json1["results"].as_array().unwrap()
    .iter().map(|r| r["path"].as_str().unwrap()).collect();
  let paths2: Vec<&str> = json2["results"].as_array().unwrap()
    .iter().map(|r| r["path"].as_str().unwrap()).collect();
  for p in &paths1 {
    assert!(!paths2.contains(p), "Overlap: {}", p);
  }
}

#[tokio::test]
async fn test_query_default_limit_in_response() {
  let (_, jwt_manager, engine, _dir) = test_app();
  setup_people(&engine, 30);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  // No explicit limit => default limit hit
  let body = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }]
  });
  let (status, json) = query_post(app, &auth, &body).await;
  assert_eq!(status, StatusCode::OK);

  assert_eq!(json["has_more"], true);
  assert_eq!(json["default_limit_hit"], true);
  assert!(json["default_limit"].is_number());
  assert_eq!(json["results"].as_array().unwrap().len(), json["default_limit"].as_u64().unwrap() as usize);
}

#[tokio::test]
async fn test_query_include_total() {
  let (_, jwt_manager, engine, _dir) = test_app();
  setup_people(&engine, 15);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "limit": 5,
    "include_total": true
  });
  let (status, json) = query_post(app, &auth, &body).await;
  assert_eq!(status, StatusCode::OK);

  assert_eq!(json["total_count"], 15);
  assert_eq!(json["results"].as_array().unwrap().len(), 5);
  assert_eq!(json["has_more"], true);
}

#[tokio::test]
async fn test_query_sort_direction() {
  let (_, jwt_manager, engine, _dir) = test_app();
  setup_people(&engine, 10);
  let auth = bearer_token(&jwt_manager);

  // ASC
  let app1 = rebuild_app(&jwt_manager, &engine);
  let body_asc = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "order_by": [{ "field": "@path", "direction": "asc" }],
    "limit": 10
  });
  let (_, json_asc) = query_post(app1, &auth, &body_asc).await;
  let paths_asc: Vec<&str> = json_asc["results"].as_array().unwrap()
    .iter().map(|r| r["path"].as_str().unwrap()).collect();

  // DESC
  let app2 = rebuild_app(&jwt_manager, &engine);
  let body_desc = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "order_by": [{ "field": "@path", "direction": "desc" }],
    "limit": 10
  });
  let (_, json_desc) = query_post(app2, &auth, &body_desc).await;
  let paths_desc: Vec<&str> = json_desc["results"].as_array().unwrap()
    .iter().map(|r| r["path"].as_str().unwrap()).collect();

  // Verify they are reversed
  let mut reversed_desc = paths_desc.clone();
  reversed_desc.reverse();
  assert_eq!(paths_asc, reversed_desc);
}

#[tokio::test]
async fn test_query_virtual_field_sort() {
  let (_, jwt_manager, engine, _dir) = test_app();
  setup_people(&engine, 5);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "order_by": [{ "field": "@path", "direction": "asc" }],
    "limit": 5
  });
  let (status, json) = query_post(app, &auth, &body).await;
  assert_eq!(status, StatusCode::OK);

  let paths: Vec<&str> = json["results"].as_array().unwrap()
    .iter().map(|r| r["path"].as_str().unwrap()).collect();
  for i in 1..paths.len() {
    assert!(paths[i - 1] <= paths[i]);
  }
}

#[tokio::test]
async fn test_query_invalid_cursor_returns_400() {
  let (_, jwt_manager, engine, _dir) = test_app();
  setup_people(&engine, 5);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "after": "not-valid-base64!!!"
  });
  let (status, _json) = query_post(app, &auth, &body).await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_query_explicit_limit_no_default_hit() {
  let (_, jwt_manager, engine, _dir) = test_app();
  setup_people(&engine, 30);
  let app = rebuild_app(&jwt_manager, &engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "path": "/people",
    "where": [{ "field": "age", "op": "gt", "value": 0 }],
    "limit": 5
  });
  let (status, json) = query_post(app, &auth, &body).await;
  assert_eq!(status, StatusCode::OK);

  assert_eq!(json["results"].as_array().unwrap().len(), 5);
  assert_eq!(json["has_more"], true);
  // default_limit_hit should NOT be present (explicit limit used)
  assert!(json.get("default_limit_hit").is_none() || json["default_limit_hit"].is_null(),
    "default_limit_hit should not be present when explicit limit is used");
}
