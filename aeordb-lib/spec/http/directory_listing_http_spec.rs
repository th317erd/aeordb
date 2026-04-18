use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
  create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

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

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON")
}

async fn body_bytes(body: Body) -> Vec<u8> {
  body.collect().await.unwrap().to_bytes().to_vec()
}

fn store_file(engine: &StorageEngine, path: &str, content: &[u8]) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  ops.store_file(&ctx, path, content, None).unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Default listing (no depth/glob) includes hash and path fields.
#[tokio::test]
async fn test_default_listing_includes_hash_and_path() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  store_file(&engine, "/dir/a.txt", b"hello");

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/dir/")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let entries = json.as_array().expect("listing is array");
  assert_eq!(entries.len(), 1);

  let entry = &entries[0];
  assert_eq!(entry["name"], "a.txt");
  assert!(entry["hash"].is_string(), "hash field should be a string");
  assert!(!entry["hash"].as_str().unwrap().is_empty(), "hash should be non-empty");
  assert_eq!(entry["path"], "/dir/a.txt");
}

/// depth=-1 returns all files recursively as a flat list.
#[tokio::test]
async fn test_recursive_unlimited() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  store_file(&engine, "/dir/a.txt", b"aaa");
  store_file(&engine, "/dir/sub/b.txt", b"bbb");
  store_file(&engine, "/dir/sub/deep/c.txt", b"ccc");

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/dir/?depth=-1")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let entries = json.as_array().expect("listing is array");
  assert_eq!(entries.len(), 3, "should return all 3 files recursively");

  let names: Vec<&str> = entries.iter().map(|e| e["name"].as_str().unwrap()).collect();
  assert!(names.contains(&"a.txt"));
  assert!(names.contains(&"b.txt"));
  assert!(names.contains(&"c.txt"));
}

/// depth=1 returns only immediate children and one level deep.
#[tokio::test]
async fn test_recursive_depth_1() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  store_file(&engine, "/dir/a.txt", b"aaa");
  store_file(&engine, "/dir/sub/b.txt", b"bbb");
  store_file(&engine, "/dir/sub/deep/c.txt", b"ccc");

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/dir/?depth=1")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let entries = json.as_array().expect("listing is array");

  let names: Vec<&str> = entries.iter().map(|e| e["name"].as_str().unwrap()).collect();
  assert!(names.contains(&"a.txt"), "should include immediate child a.txt");
  assert!(names.contains(&"b.txt"), "should include one-level-deep b.txt");
  assert!(!names.contains(&"c.txt"), "should NOT include two-levels-deep c.txt");
}

/// glob filter returns only matching files.
#[tokio::test]
async fn test_glob_filter() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  store_file(&engine, "/dir/a.txt", b"aaa");
  store_file(&engine, "/dir/b.psd", b"bbb");

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/dir/?glob=*.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let entries = json.as_array().expect("listing is array");
  assert_eq!(entries.len(), 1);
  assert_eq!(entries[0]["name"], "a.txt");
}

/// glob combined with depth=-1 filters recursively.
#[tokio::test]
async fn test_glob_with_depth() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  store_file(&engine, "/dir/a.txt", b"aaa");
  store_file(&engine, "/dir/sub/b.txt", b"bbb");
  store_file(&engine, "/dir/sub/c.psd", b"ccc");

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/dir/?depth=-1&glob=*.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let entries = json.as_array().expect("listing is array");
  assert_eq!(entries.len(), 2);

  let names: Vec<&str> = entries.iter().map(|e| e["name"].as_str().unwrap()).collect();
  assert!(names.contains(&"a.txt"));
  assert!(names.contains(&"b.txt"));
  assert!(!names.contains(&"c.psd"));
}

/// Recursive listing excludes directory entries (entry_type 3).
#[tokio::test]
async fn test_directories_excluded_recursive() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  store_file(&engine, "/dir/a.txt", b"aaa");
  store_file(&engine, "/dir/sub/b.txt", b"bbb");

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/dir/?depth=-1")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let entries = json.as_array().expect("listing is array");

  // EntryType::DirectoryIndex = 3; all entries should be files (entry_type 2)
  for entry in entries {
    let et = entry["entry_type"].as_u64().unwrap();
    assert_ne!(et, 3, "recursive listing should not include directory entries");
    assert_eq!(et, 2, "all entries should be FileRecord (entry_type 2)");
  }
}

/// Nonexistent directory returns 404.
#[tokio::test]
async fn test_nonexistent_directory_404() {
  let (app, jwt_manager, _engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/files/nonexistent/?depth=-1")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// GET on a file path still returns file content, not a JSON listing.
#[tokio::test]
async fn test_file_get_unaffected() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  store_file(&engine, "/file.txt", b"raw content here");

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/file.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = body_bytes(response.into_body()).await;
  assert_eq!(bytes, b"raw content here");
}

/// Version query params (snapshot) still work after the refactor.
#[tokio::test]
async fn test_version_query_still_works() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store v1
  store_file(&engine, "/file.txt", b"version-one");

  // Create snapshot
  let ctx = RequestContext::system();
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "snap1", HashMap::new()).unwrap();

  // Store v2 (overwrite)
  store_file(&engine, "/file.txt", b"version-two");

  // GET at snapshot should return v1
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/files/file.txt?snapshot=snap1")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = body_bytes(response.into_body()).await;
  assert_eq!(bytes, b"version-one");
}
