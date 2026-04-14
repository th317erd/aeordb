use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{RequestContext, StorageEngine};
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

/// Create a root-user Bearer token value (including "Bearer " prefix).
fn bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: uuid::Uuid::nil().to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

/// Collect response body into bytes.
async fn body_bytes(body: Body) -> Vec<u8> {
  body.collect().await.unwrap().to_bytes().to_vec()
}

/// Collect response body into JSON.
#[allow(dead_code)]
async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body_bytes(body).await;
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

/// Helper: PUT a file via HTTP, return the response status.
async fn put_file(
  app: axum::Router,
  auth: &str,
  path: &str,
  content_type: &str,
  body: &[u8],
) -> StatusCode {
  let request = Request::builder()
    .method("PUT")
    .uri(format!("/engine/{}", path))
    .header("content-type", content_type)
    .header("authorization", auth)
    .body(Body::from(body.to_vec()))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  response.status()
}

/// Helper: GET a file via HTTP with optional query string, return (status, headers, body bytes).
async fn get_file(
  app: axum::Router,
  auth: &str,
  uri: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
  let request = Request::builder()
    .method("GET")
    .uri(uri)
    .header("authorization", auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  let headers = response.headers().clone();
  let bytes = body_bytes(response.into_body()).await;
  (status, headers, bytes)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Store v1, snapshot, store v2, GET with ?snapshot= returns v1 content.
#[tokio::test]
async fn test_get_file_at_snapshot() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store v1
  let status = put_file(app, &auth, "file.txt", "text/plain", b"version-one").await;
  assert_eq!(status, StatusCode::CREATED);

  // Create snapshot "snap1" via engine directly
  let ctx = RequestContext::system();
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

  // Store v2 (overwrites file.txt)
  let app = rebuild_app(&jwt_manager, &engine);
  let status = put_file(app, &auth, "file.txt", "text/plain", b"version-two").await;
  assert_eq!(status, StatusCode::CREATED);

  // GET current version should return v2
  let app = rebuild_app(&jwt_manager, &engine);
  let (status, _, bytes) = get_file(app, &auth, "/engine/file.txt").await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(bytes, b"version-two");

  // GET at snapshot should return v1
  let app = rebuild_app(&jwt_manager, &engine);
  let (status, _, bytes) = get_file(app, &auth, "/engine/file.txt?snapshot=snap1").await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(bytes, b"version-one");
}

/// Store v1, snapshot, GET with ?version={hex_hash} returns v1 content.
#[tokio::test]
async fn test_get_file_at_version_hash() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store v1
  let status = put_file(app, &auth, "file.txt", "text/plain", b"version-one").await;
  assert_eq!(status, StatusCode::CREATED);

  // Create snapshot and capture its root_hash
  let ctx = RequestContext::system();
  let vm = VersionManager::new(&engine);
  let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();
  let hex_hash = hex::encode(&snapshot.root_hash);

  // Store v2
  let app = rebuild_app(&jwt_manager, &engine);
  let status = put_file(app, &auth, "file.txt", "text/plain", b"version-two").await;
  assert_eq!(status, StatusCode::CREATED);

  // GET with ?version={hex_hash} should return v1
  let app = rebuild_app(&jwt_manager, &engine);
  let uri = format!("/engine/file.txt?version={}", hex_hash);
  let (status, _, bytes) = get_file(app, &auth, &uri).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(bytes, b"version-one");
}

/// GET with ?snapshot=nonexistent returns 404.
#[tokio::test]
async fn test_get_file_snapshot_not_found() {
  let (app, jwt_manager, _engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let (status, _, _) = get_file(app, &auth, "/engine/file.txt?snapshot=nonexistent").await;
  assert_eq!(status, StatusCode::NOT_FOUND);
}

/// File stored AFTER snapshot, GET at snapshot returns 404 for that file.
#[tokio::test]
async fn test_get_file_not_at_version() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store an initial file so the engine has a valid root
  let status = put_file(app, &auth, "existing.txt", "text/plain", b"exists").await;
  assert_eq!(status, StatusCode::CREATED);

  // Create snapshot before "later.txt" exists
  let ctx = RequestContext::system();
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "before", HashMap::new()).unwrap();

  // Now store a new file after the snapshot
  let app = rebuild_app(&jwt_manager, &engine);
  let status = put_file(app, &auth, "later.txt", "text/plain", b"later content").await;
  assert_eq!(status, StatusCode::CREATED);

  // GET later.txt at snapshot should be 404 (file didn't exist at that version)
  let app = rebuild_app(&jwt_manager, &engine);
  let (status, _, _) = get_file(app, &auth, "/engine/later.txt?snapshot=before").await;
  assert_eq!(status, StatusCode::NOT_FOUND);
}

/// When both ?snapshot= and ?version= are provided, snapshot takes precedence.
#[tokio::test]
async fn test_get_file_snapshot_precedence() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store v1
  let status = put_file(app, &auth, "file.txt", "text/plain", b"snap-content").await;
  assert_eq!(status, StatusCode::CREATED);

  // Create snapshot "snap1"
  let ctx = RequestContext::system();
  let vm = VersionManager::new(&engine);
  let snapshot = vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();
  let _snap_hex = hex::encode(&snapshot.root_hash);

  // Store v2
  let app = rebuild_app(&jwt_manager, &engine);
  let status = put_file(app, &auth, "file.txt", "text/plain", b"v2-content").await;
  assert_eq!(status, StatusCode::CREATED);

  // Create snapshot "snap2" (different root hash)
  let vm = VersionManager::new(&engine);
  let snapshot2 = vm.create_snapshot(&ctx, "snap2", HashMap::new()).unwrap();
  let snap2_hex = hex::encode(&snapshot2.root_hash);

  // Provide both: snapshot=snap1 and version=<snap2 hash>
  // Snapshot should take precedence; we should get v1 "snap-content"
  let app = rebuild_app(&jwt_manager, &engine);
  let uri = format!("/engine/file.txt?snapshot=snap1&version={}", snap2_hex);
  let (status, _, bytes) = get_file(app, &auth, &uri).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(bytes, b"snap-content");

  // Verify the other snapshot would give different content (sanity check)
  let app = rebuild_app(&jwt_manager, &engine);
  let uri2 = format!("/engine/file.txt?snapshot=snap2");
  let (status2, _, bytes2) = get_file(app, &auth, &uri2).await;
  assert_eq!(status2, StatusCode::OK);
  assert_eq!(bytes2, b"v2-content");

  // And verify using snap2 hex hash directly gives v2
  let app = rebuild_app(&jwt_manager, &engine);
  let uri3 = format!("/engine/file.txt?version={}", snap2_hex);
  let (status3, _, bytes3) = get_file(app, &auth, &uri3).await;
  assert_eq!(status3, StatusCode::OK);
  assert_eq!(bytes3, b"v2-content");
}

/// ?version=notahex returns 400 Bad Request.
#[tokio::test]
async fn test_get_file_invalid_version_hash() {
  let (app, jwt_manager, _engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let (status, _, _) = get_file(app, &auth, "/engine/file.txt?version=notahexvalue!!!").await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Normal GET without query params still returns current content (regression test).
#[tokio::test]
async fn test_get_file_no_version_params_still_works() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store a file
  let status = put_file(app, &auth, "hello.txt", "text/plain", b"hello world").await;
  assert_eq!(status, StatusCode::CREATED);

  // GET without any query params
  let app = rebuild_app(&jwt_manager, &engine);
  let (status, _, bytes) = get_file(app, &auth, "/engine/hello.txt").await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(bytes, b"hello world");
}

/// Verify Content-Type header matches historical file's type.
#[tokio::test]
async fn test_get_file_at_version_content_type() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store a JSON file
  let status = put_file(app, &auth, "data.json", "application/json", b"{\"v\":1}").await;
  assert_eq!(status, StatusCode::CREATED);

  // Create snapshot
  let ctx = RequestContext::system();
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "json-snap", HashMap::new()).unwrap();

  // Overwrite with plain text
  let app = rebuild_app(&jwt_manager, &engine);
  let status = put_file(app, &auth, "data.json", "text/plain", b"not json anymore").await;
  assert_eq!(status, StatusCode::CREATED);

  // GET at snapshot should return with original content-type
  let app = rebuild_app(&jwt_manager, &engine);
  let (status, headers, bytes) = get_file(app, &auth, "/engine/data.json?snapshot=json-snap").await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(bytes, b"{\"v\":1}");
  let ct = headers.get("content-type").unwrap().to_str().unwrap();
  assert_eq!(ct, "application/json");
}

/// GET with ?version= using a valid hex string that doesn't correspond to any root returns 404.
#[tokio::test]
async fn test_get_file_version_hash_not_found() {
  let (app, jwt_manager, _engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // A valid hex hash that doesn't exist in the engine
  let fake_hash = "00".repeat(32); // 32-byte zero hash
  let uri = format!("/engine/file.txt?version={}", fake_hash);
  let (status, _, _) = get_file(app, &auth, &uri).await;
  // Should fail - either 404 or 500 depending on engine error
  assert_ne!(status, StatusCode::OK);
}

/// GET at snapshot with nested path works correctly.
#[tokio::test]
async fn test_get_file_at_snapshot_nested_path() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store a nested file
  let status = put_file(app, &auth, "docs/readme.txt", "text/plain", b"original readme").await;
  assert_eq!(status, StatusCode::CREATED);

  // Create snapshot
  let ctx = RequestContext::system();
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "nested-snap", HashMap::new()).unwrap();

  // Overwrite
  let app = rebuild_app(&jwt_manager, &engine);
  let status = put_file(app, &auth, "docs/readme.txt", "text/plain", b"updated readme").await;
  assert_eq!(status, StatusCode::CREATED);

  // GET at snapshot returns original
  let app = rebuild_app(&jwt_manager, &engine);
  let (status, _, bytes) = get_file(app, &auth, "/engine/docs/readme.txt?snapshot=nested-snap").await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(bytes, b"original readme");

  // GET current returns updated
  let app = rebuild_app(&jwt_manager, &engine);
  let (status, _, bytes) = get_file(app, &auth, "/engine/docs/readme.txt").await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(bytes, b"updated readme");
}

/// Verify X-Total-Size and timestamp headers are present on versioned reads.
#[tokio::test]
async fn test_get_file_at_version_response_headers() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let content = b"header test content";
  let status = put_file(app, &auth, "headers.txt", "text/plain", content).await;
  assert_eq!(status, StatusCode::CREATED);

  let ctx = RequestContext::system();
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "hdr-snap", HashMap::new()).unwrap();

  let app = rebuild_app(&jwt_manager, &engine);
  let (status, headers, bytes) = get_file(app, &auth, "/engine/headers.txt?snapshot=hdr-snap").await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(bytes, content);

  // Check expected headers
  assert!(headers.get("X-Path").is_some(), "Missing X-Path header");
  assert!(headers.get("X-Total-Size").is_some(), "Missing X-Total-Size header");
  let size: u64 = headers.get("X-Total-Size").unwrap().to_str().unwrap().parse().unwrap();
  assert_eq!(size, content.len() as u64);
  assert!(headers.get("X-Created-At").is_some(), "Missing X-Created-At header");
  assert!(headers.get("X-Updated-At").is_some(), "Missing X-Updated-At header");
}
