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
  // Tests create multiple snapshots back-to-back; bypass the 1-per-60s
  // manual-snapshot throttle in production code.
  // SAFETY: tests run single-threaded per binary; setting env var here is fine.
  unsafe { std::env::set_var("AEORDB_DISABLE_SNAPSHOT_RATE_LIMIT", "1"); }
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

/// Rebuild app from shared state (for multi-request tests).
fn rebuild_app(
  jwt_manager: &Arc<JwtManager>,
  engine: &Arc<StorageEngine>,
) -> axum::Router {
  create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

/// Create a root-user Bearer token value (including "Bearer " prefix).
/// Uses the nil UUID which matches ROOT_USER_ID for root authorization.
fn bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: uuid::Uuid::nil().to_string(),
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
// Engine file store
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_engine_store_file_returns_201() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/docs/readme.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("hello engine"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert!(json["path"].is_string());
  assert_eq!(json["size"], 12);
  assert_eq!(json["content_type"], "text/plain");
}

#[tokio::test]
async fn test_engine_get_file_returns_data() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store a file
  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/data.bin")
    .header("content-type", "application/octet-stream")
    .header("authorization", &auth)
    .body(Body::from(vec![1u8, 2, 3, 4, 5]))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Read it back
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/test/data.bin")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = body_bytes(response.into_body()).await;
  assert_eq!(bytes, vec![1u8, 2, 3, 4, 5]);
}

#[tokio::test]
async fn test_engine_get_file_returns_content_type() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/app/config.json")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"key":"val"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/app/config.json")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(
    response.headers().get("content-type").unwrap().to_str().unwrap(),
    "application/json"
  );
}

#[tokio::test]
async fn test_engine_get_file_404() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/files/nonexistent/file.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_engine_get_directory_returns_listing() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store two files in the same directory
  let request = Request::builder()
    .method("PUT")
    .uri("/files/mydir/file1.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("first"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/files/mydir/file2.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("second"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // List the directory
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/mydir")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let entries = json["items"].as_array().expect("expected items array");
  assert_eq!(entries.len(), 2);

  let names: Vec<&str> = entries
    .iter()
    .map(|entry| entry["name"].as_str().unwrap())
    .collect();
  assert!(names.contains(&"file1.txt"));
  assert!(names.contains(&"file2.txt"));
}

#[tokio::test]
async fn test_engine_delete_file_returns_200() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store first
  let request = Request::builder()
    .method("PUT")
    .uri("/files/todelete/file.txt")
    .header("authorization", &auth)
    .body(Body::from("delete me"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Delete
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("DELETE")
    .uri("/files/todelete/file.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["deleted"], true);
}

#[tokio::test]
async fn test_engine_delete_file_404() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/files/nope/gone.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_engine_head_returns_metadata() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/meta/info.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("metadata test"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("HEAD")
    .uri("/files/meta/info.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(
    response.headers().get("X-AeorDB-Type").unwrap().to_str().unwrap(),
    "file"
  );
  assert_eq!(
    response.headers().get("X-AeorDB-Size").unwrap().to_str().unwrap(),
    "13"
  );
  assert_eq!(
    response.headers().get("content-type").unwrap().to_str().unwrap(),
    "text/plain"
  );

  // Body should be empty for HEAD
  let bytes = body_bytes(response.into_body()).await;
  assert!(bytes.is_empty());
}

// ---------------------------------------------------------------------------
// Additional error / edge-case coverage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_engine_head_on_directory_returns_directory_type() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store a file to create the directory implicitly
  let request = Request::builder()
    .method("PUT")
    .uri("/files/headdir/child.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("content"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // HEAD on the parent directory
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("HEAD")
    .uri("/files/headdir")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(
    response.headers().get("X-AeorDB-Type").unwrap().to_str().unwrap(),
    "directory"
  );

  let bytes = body_bytes(response.into_body()).await;
  assert!(bytes.is_empty(), "HEAD response body should be empty");
}

#[tokio::test]
async fn test_engine_store_without_content_type() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // PUT without content-type header -- should still succeed
  let request = Request::builder()
    .method("PUT")
    .uri("/files/noct/file.bin")
    .header("authorization", &auth)
    .body(Body::from("some data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["size"], 9);
  // content_type should be auto-detected as text/plain (magic byte sniffing for "some data")
  assert_eq!(
    json["content_type"].as_str(), Some("text/plain"),
    "content_type should be auto-detected when not provided"
  );
}

#[tokio::test]
async fn test_engine_get_nonexistent_directory() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/files/totally/does/not/exist")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_engine_delete_nonexistent_deep_path_returns_404() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/files/deep/nested/nonexistent/path.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_engine_put_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("PUT")
    .uri("/files/unauthed/file.txt")
    .header("content-type", "text/plain")
    .body(Body::from("no auth"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_engine_delete_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("DELETE")
    .uri("/files/unauthed/file.txt")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_engine_head_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("HEAD")
    .uri("/files/unauthed/file.txt")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_query_unsupported_value_type_returns_400() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // null is an unsupported value type for json_value_to_bytes
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "eq", "value": null }
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
async fn test_query_unsupported_value2_type_returns_400() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // between with an unsupported value2 type (array)
  let body = serde_json::json!({
    "path": "/myapp/users",
    "where": [
      { "field": "age", "op": "between", "value": 10, "value2": [1, 2, 3] }
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
async fn test_snapshot_create_with_malformed_json_returns_error() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/snapshots")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"not_valid_json"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  assert!(
    status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
    "Expected 400 or 422, got {}",
    status,
  );
}

#[tokio::test]
async fn test_fork_create_with_malformed_json_returns_error() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/forks")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"not json at all"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  assert!(
    status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
    "Expected 400 or 422, got {}",
    status,
  );
}

#[tokio::test]
async fn test_snapshot_restore_with_missing_name_returns_error() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/restore")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  assert!(
    status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
    "Expected 400 or 422, got {}",
    status,
  );
}

#[tokio::test]
async fn test_engine_get_file_returns_metadata_headers() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/headers/test.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("header test"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/headers/test.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Verify metadata headers are present on GET file response
  assert!(response.headers().get("X-AeorDB-Path").is_some());
  assert!(response.headers().get("X-AeorDB-Size").is_some());
  assert!(response.headers().get("X-AeorDB-Created").is_some());
  assert!(response.headers().get("X-AeorDB-Updated").is_some());
  assert_eq!(
    response.headers().get("content-type").unwrap().to_str().unwrap(),
    "text/plain"
  );
}

#[tokio::test]
async fn test_engine_overwrite_existing_file() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store initial file
  let request = Request::builder()
    .method("PUT")
    .uri("/files/overwrite/file.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("version 1"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Overwrite with new content
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/files/overwrite/file.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("version 2 is longer"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Read back should return the new content
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/overwrite/file.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = body_bytes(response.into_body()).await;
  assert_eq!(String::from_utf8(bytes).unwrap(), "version 2 is longer");
}

#[tokio::test]
async fn test_engine_store_creates_intermediate_dirs() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store file at a deeply nested path
  let request = Request::builder()
    .method("PUT")
    .uri("/files/a/b/c/deep.txt")
    .header("authorization", &auth)
    .body(Body::from("deep"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Listing "a/b/c" should show "deep.txt"
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/a/b/c")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let entries = json["items"].as_array().expect("expected items array");
  assert_eq!(entries.len(), 1);
  assert_eq!(entries[0]["name"], "deep.txt");
}

#[tokio::test]
async fn test_engine_store_and_get_roundtrip() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let original_data = "The quick brown fox jumps over the lazy dog.";

  let request = Request::builder()
    .method("PUT")
    .uri("/files/roundtrip/fox.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from(original_data))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/roundtrip/fox.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = body_bytes(response.into_body()).await;
  assert_eq!(String::from_utf8(bytes).unwrap(), original_data);
}

#[tokio::test]
async fn test_engine_routes_require_auth() {
  let (app, _, _, _temp_dir) = test_app();

  // GET without auth should fail
  let request = Request::builder()
    .method("GET")
    .uri("/files/some/path")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Snapshot routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_snapshot_create() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/snapshots")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"v1","metadata":{"env":"test"}}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["name"], "v1");
  assert!(json["root_hash"].is_string());
  assert!(json["created_at"].is_number());
  assert_eq!(json["metadata"]["env"], "test");
}

#[tokio::test]
async fn test_snapshot_list() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Create snap1
  let request = Request::builder()
    .method("POST")
    .uri("/versions/snapshots")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"snap1","metadata":{}}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Write something so snap2 has a different HEAD — without this the
  // dedup path returns the existing snap1 with 200 instead of creating
  // a new snap2 with 201.
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/files/list-test.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("between snapshots"))
    .unwrap();
  let _ = app.oneshot(request).await.unwrap();

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("POST")
    .uri("/versions/snapshots")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"snap2","metadata":{}}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // List snapshots
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/versions/snapshots")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let snapshots = json["items"].as_array().expect("expected items array");
  assert_eq!(snapshots.len(), 2);
}

#[tokio::test]
async fn test_snapshot_restore() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Create a snapshot
  let request = Request::builder()
    .method("POST")
    .uri("/versions/snapshots")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"restore-me","metadata":{}}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Restore it
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("POST")
    .uri("/versions/restore")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"restore-me"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["restored"], true);
  assert_eq!(json["name"], "restore-me");
}

#[tokio::test]
async fn test_snapshot_delete() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Create a snapshot
  let request = Request::builder()
    .method("POST")
    .uri("/versions/snapshots")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"to-delete","metadata":{}}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Delete it
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("DELETE")
    .uri("/versions/snapshots/to-delete")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["deleted"], true);

  // Should be gone from list
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/versions/snapshots")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let json = body_json(response.into_body()).await;
  let snapshots = json["items"].as_array().expect("expected items array");
  assert!(snapshots.is_empty());
}

// ---------------------------------------------------------------------------
// Fork routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_fork_create() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/forks")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"my-batch"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["name"], "my-batch");
  assert!(json["root_hash"].is_string());
  assert!(json["created_at"].is_number());
}

#[tokio::test]
async fn test_fork_list() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Create a fork
  let request = Request::builder()
    .method("POST")
    .uri("/versions/forks")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"fork-a"}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // List forks
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/versions/forks")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let forks = json["items"].as_array().expect("expected items array");
  assert_eq!(forks.len(), 1);
  assert_eq!(forks[0]["name"], "fork-a");
}

#[tokio::test]
async fn test_fork_promote() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Create a fork
  let request = Request::builder()
    .method("POST")
    .uri("/versions/forks")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"promote-me"}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Promote it
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("POST")
    .uri("/versions/forks/promote-me/promote")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["promoted"], true);
  assert_eq!(json["name"], "promote-me");

  // Fork should be gone after promote
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/versions/forks")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let json = body_json(response.into_body()).await;
  let forks = json["items"].as_array().expect("expected items array");
  assert!(forks.is_empty());
}

#[tokio::test]
async fn test_fork_abandon() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Create a fork
  let request = Request::builder()
    .method("POST")
    .uri("/versions/forks")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"abandon-me"}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Abandon it
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("DELETE")
    .uri("/versions/forks/abandon-me")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["abandoned"], true);

  // Fork should be gone
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/versions/forks")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let json = body_json(response.into_body()).await;
  let forks = json["items"].as_array().expect("expected items array");
  assert!(forks.is_empty());
}

// ---------------------------------------------------------------------------
// Large file (multi-chunk)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_engine_large_file() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Create data larger than one chunk (256 KB)
  let large_data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();

  let request = Request::builder()
    .method("PUT")
    .uri("/files/big/largefile.bin")
    .header("content-type", "application/octet-stream")
    .header("authorization", &auth)
    .body(Body::from(large_data.clone()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["size"], 300_000);

  // Read it back and verify roundtrip
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/big/largefile.bin")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = body_bytes(response.into_body()).await;
  assert_eq!(bytes.len(), 300_000);
  assert_eq!(bytes, large_data);
}

// ---------------------------------------------------------------------------
// Error / edge case tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_snapshot_create_duplicate_returns_conflict() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/snapshots")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"dup","metadata":{}}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Duplicate
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("POST")
    .uri("/versions/snapshots")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"dup","metadata":{}}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_snapshot_restore_nonexistent_returns_404() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/restore")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"nonexistent"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_snapshot_delete_nonexistent_returns_404() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/versions/snapshots/nope")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_fork_create_duplicate_returns_conflict() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/forks")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"dup-fork"}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("POST")
    .uri("/versions/forks")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"dup-fork"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_fork_promote_nonexistent_returns_404() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/forks/ghost/promote")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_fork_abandon_nonexistent_returns_404() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/versions/forks/ghost")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_version_routes_require_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/versions/snapshots")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"name":"noauth","metadata":{}}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_engine_head_nonexistent_returns_404() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("HEAD")
    .uri("/files/nope/nothing.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_engine_store_empty_file() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/empty/zero.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["size"], 0);

  // Read it back
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/empty/zero.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = body_bytes(response.into_body()).await;
  assert!(bytes.is_empty());
}

// ---------------------------------------------------------------------------
// Fetch-by-hash routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_by_hash_returns_file_content() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let content = "fetch me by hash!";

  // Store a file
  let request = Request::builder()
    .method("PUT")
    .uri("/files/hashtest/file.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from(content))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let hash = json["hash"].as_str().expect("response should include hash");
  assert!(!hash.is_empty(), "hash should not be empty");

  // Fetch by hash
  let app = rebuild_app(&jwt_manager, &engine);
  let uri = format!("/blobs/{}", hash);
  let request = Request::builder()
    .method("GET")
    .uri(&uri)
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Verify content-type header carried through from the FileRecord
  assert_eq!(
    response.headers().get("content-type").unwrap().to_str().unwrap(),
    "text/plain"
  );
  // Verify X-AeorDB-Hash echo header
  assert_eq!(
    response.headers().get("X-AeorDB-Hash").unwrap().to_str().unwrap(),
    hash
  );
  // Verify X-AeorDB-Type header (FileRecord = 0x02)
  assert_eq!(
    response.headers().get("X-AeorDB-Type").unwrap().to_str().unwrap(),
    "2"
  );

  let bytes = body_bytes(response.into_body()).await;
  assert_eq!(String::from_utf8(bytes).unwrap(), content);
}

#[tokio::test]
async fn test_get_by_hash_not_found() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Fabricate a plausible but nonexistent 32-byte hex hash (64 hex chars)
  let fake_hash = "aa".repeat(32);

  let request = Request::builder()
    .method("GET")
    .uri(&format!("/blobs/{}", fake_hash))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_by_hash_invalid_hex() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/blobs/not-valid-hex-string!")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);

  let json = body_json(response.into_body()).await;
  assert!(
    json["error"].as_str().unwrap().contains("Invalid hex hash"),
    "Error message should mention invalid hex hash"
  );
}

#[tokio::test]
async fn test_get_by_hash_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/blobs/deadbeef")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_get_by_hash_large_file_roundtrip() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Create data larger than one chunk (256 KB) to test multi-chunk streaming
  let large_data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();

  let request = Request::builder()
    .method("PUT")
    .uri("/files/hashtest/large.bin")
    .header("content-type", "application/octet-stream")
    .header("authorization", &auth)
    .body(Body::from(large_data.clone()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let hash = json["hash"].as_str().expect("response should include hash");

  // Fetch the large file by hash
  let app = rebuild_app(&jwt_manager, &engine);
  let uri = format!("/blobs/{}", hash);
  let request = Request::builder()
    .method("GET")
    .uri(&uri)
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = body_bytes(response.into_body()).await;
  assert_eq!(bytes.len(), 300_000);
  assert_eq!(bytes, large_data);
}

#[tokio::test]
async fn test_get_by_hash_empty_file() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store an empty file
  let request = Request::builder()
    .method("PUT")
    .uri("/files/hashtest/empty.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let hash = json["hash"].as_str().expect("response should include hash");

  // Fetch by hash — should return empty body
  let app = rebuild_app(&jwt_manager, &engine);
  let uri = format!("/blobs/{}", hash);
  let request = Request::builder()
    .method("GET")
    .uri(&uri)
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = body_bytes(response.into_body()).await;
  assert!(bytes.is_empty());
}

#[tokio::test]
async fn test_put_response_includes_hash_field() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/hashfield/doc.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("check hash field"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert!(json["hash"].is_string(), "PUT response must include 'hash' field");
  let hash = json["hash"].as_str().unwrap();
  // Content hash should be a valid hex string of reasonable length
  assert!(hash.len() >= 32, "hash should be at least 32 hex chars");
  assert!(
    hash.chars().all(|c| c.is_ascii_hexdigit()),
    "hash should be valid hex"
  );
}

#[tokio::test]
async fn test_get_by_hash_same_content_different_paths() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let content = "identical content";

  // Store same content at two different paths
  let request = Request::builder()
    .method("PUT")
    .uri("/files/dup/a.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from(content))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
  let json1 = body_json(response.into_body()).await;
  let hash1 = json1["hash"].as_str().unwrap().to_string();

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/files/dup/b.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from(content))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
  let json2 = body_json(response.into_body()).await;
  let hash2 = json2["hash"].as_str().unwrap().to_string();

  // Different paths => different FileRecords => different content hashes
  // (the filec: hash includes the serialized FileRecord which includes the path)
  assert_ne!(hash1, hash2, "Different paths should produce different content hashes");

  // Both should be fetchable by their respective hashes
  for (hash, _expected_path) in [(&hash1, "/dup/a.txt"), (&hash2, "/dup/b.txt")] {
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
      .method("GET")
      .uri(&format!("/blobs/{}", hash))
      .header("authorization", &auth)
      .body(Body::empty())
      .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(String::from_utf8(bytes).unwrap(), content);
  }
}
