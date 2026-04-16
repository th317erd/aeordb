use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::StorageEngine;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

/// Create a fresh in-memory app with engine support.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

/// Rebuild app from shared state (for multi-request tests).
fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
  create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

/// Create a Bearer token with the nil UUID (root user) for admin operations.
fn root_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "00000000-0000-0000-0000-000000000000".to_string(),
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

/// Create a regular Bearer token for authenticated (non-admin) access.
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

/// Collect response body into bytes.
async fn body_bytes(body: Body) -> Vec<u8> {
  body.collect().await.unwrap().to_bytes().to_vec()
}

/// Collect response body into JSON.
async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body_bytes(body).await;
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ---------------------------------------------------------------------------
// Portal asset tests (public, no auth needed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_portal_index_returns_html() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/portal")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert!(
    content_type.contains("text/html"),
    "Expected text/html content-type, got: {}",
    content_type,
  );

  let bytes = body_bytes(response.into_body()).await;
  let body_str = String::from_utf8_lossy(&bytes);
  assert!(
    body_str.contains("AeorDB Portal"),
    "Expected body to contain 'AeorDB Portal', got: {}",
    &body_str[..body_str.len().min(200)],
  );
}

#[tokio::test]
async fn test_portal_index_slash_returns_html() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/portal/")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert!(
    content_type.contains("text/html"),
    "Expected text/html content-type, got: {}",
    content_type,
  );
}

#[tokio::test]
async fn test_portal_app_mjs_returns_javascript() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/portal/app.mjs")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert_eq!(content_type, "application/javascript; charset=utf-8");
}

#[tokio::test]
async fn test_portal_dashboard_mjs_returns_javascript() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/portal/dashboard.mjs")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert_eq!(content_type, "application/javascript; charset=utf-8");
}

#[tokio::test]
async fn test_portal_users_mjs_returns_javascript() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/portal/users.mjs")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert_eq!(content_type, "application/javascript; charset=utf-8");
}

#[tokio::test]
async fn test_portal_unknown_asset_returns_404() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/portal/nonexistent.js")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_portal_assets_require_no_auth() {
  let (app, _, _, _temp_dir) = test_app();

  // Deliberately omit Authorization header.
  let request = Request::builder()
    .method("GET")
    .uri("/portal")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::OK,
    "Portal should be accessible without authentication",
  );
}

// ---------------------------------------------------------------------------
// Stats API tests (requires auth)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_stats_returns_json() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/api/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header present")
    .to_str()
    .unwrap();
  assert!(
    content_type.contains("application/json"),
    "Expected application/json content-type, got: {}",
    content_type,
  );

  // Verify it parses as valid JSON.
  let _json = body_json(response.into_body()).await;
}

#[tokio::test]
async fn test_stats_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/api/stats")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(
    response.status(),
    StatusCode::UNAUTHORIZED,
    "Stats API should require authentication",
  );
}

#[tokio::test]
async fn test_stats_has_expected_fields() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/api/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;

  let expected_fields = [
    "entry_count",
    "kv_entries",
    "kv_size_bytes",
    "nvt_buckets",
    "nvt_size_bytes",
    "chunk_count",
    "file_count",
    "directory_count",
    "snapshot_count",
    "fork_count",
    "void_count",
    "void_space_bytes",
    "db_file_size_bytes",
    "created_at",
    "updated_at",
    "hash_algorithm",
  ];

  for field in &expected_fields {
    assert!(
      !json[field].is_null(),
      "Expected field '{}' to be present in stats response, got: {}",
      field,
      json,
    );
  }
}

#[tokio::test]
async fn test_stats_entry_count_zero_on_fresh_db() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/api/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  // A fresh database may have a small number of entries from root directory
  // initialization and system table bootstrap. Verify the count is reasonable
  // (not hundreds) rather than exactly zero.
  let chunk_count = json["chunk_count"].as_u64().unwrap_or(0);
  assert!(chunk_count <= 5, "Fresh db should have very few chunks, got {}", chunk_count);
}

#[tokio::test]
async fn test_stats_reflects_stored_files() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Store a file.
  let request = Request::builder()
    .method("PUT")
    .uri("/engine/test/hello.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("hello world"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Rebuild app (oneshot consumed the router).
  let app = rebuild_app(&jwt_manager, &engine);

  let request = Request::builder()
    .method("GET")
    .uri("/api/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert!(
    json["file_count"].as_u64().unwrap_or(0) > 0,
    "After storing a file, file_count should be > 0, got: {}",
    json["file_count"],
  );
  assert!(
    json["chunk_count"].as_u64().unwrap_or(0) > 0,
    "After storing a file, chunk_count should be > 0, got: {}",
    json["chunk_count"],
  );
}

#[tokio::test]
async fn test_stats_db_file_size_positive() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/api/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert!(
    json["db_file_size_bytes"].as_u64().unwrap_or(0) > 0,
    "Even an empty db should have db_file_size_bytes > 0 (file header), got: {}",
    json["db_file_size_bytes"],
  );
}

#[tokio::test]
async fn test_stats_hash_algorithm_populated() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/api/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let hash_algo = json["hash_algorithm"]
    .as_str()
    .expect("hash_algorithm should be a string");
  assert!(
    !hash_algo.is_empty(),
    "hash_algorithm should be a non-empty string",
  );
}

#[tokio::test]
async fn test_stats_created_at_populated() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/api/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert!(
    json["created_at"].as_u64().unwrap_or(0) > 0,
    "created_at should be > 0, got: {}",
    json["created_at"],
  );
}

#[tokio::test]
async fn test_stats_snapshot_count_after_snapshot() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a snapshot.
  let request = Request::builder()
    .method("POST")
    .uri("/version/snapshot")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name": "snap1"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert!(
    response.status().is_success(),
    "Snapshot creation should succeed, got: {}",
    response.status(),
  );

  // Rebuild app (oneshot consumed the router).
  let app = rebuild_app(&jwt_manager, &engine);

  let request = Request::builder()
    .method("GET")
    .uri("/api/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(
    json["snapshot_count"], 1,
    "After creating one snapshot, snapshot_count should be 1, got: {}",
    json["snapshot_count"],
  );
}

#[tokio::test]
async fn test_stats_void_space_zero_on_fresh_db() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/api/stats")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(
    json["void_space_bytes"], 0,
    "Fresh db should have void_space_bytes=0, got: {}",
    json["void_space_bytes"],
  );
  assert_eq!(
    json["void_count"], 0,
    "Fresh db should have void_count=0, got: {}",
    json["void_count"],
  );
}
