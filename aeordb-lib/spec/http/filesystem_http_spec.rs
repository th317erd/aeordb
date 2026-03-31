use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::StorageEngine;
use aeordb::server::{create_app_with_jwt, create_temp_engine_for_tests};
use aeordb::storage::RedbStorage;

/// Create a fresh in-memory app with filesystem support.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<RedbStorage>, Arc<StorageEngine>, tempfile::TempDir) {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt(storage.clone(), jwt_manager.clone(), engine.clone());
  (app, jwt_manager, storage, engine, temp_dir)
}

/// Rebuild app from shared state (for multi-request tests).
fn rebuild_app(storage: &Arc<RedbStorage>, jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
  create_app_with_jwt(storage.clone(), jwt_manager.clone(), engine.clone())
}

/// Create an admin Bearer token value (including "Bearer " prefix).
fn bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "test-admin".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    roles: vec!["admin".to_string()],
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
async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body_bytes(body).await;
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ---------------------------------------------------------------------------
// Store file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_store_file_returns_201() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/config.json")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"key":"value"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_store_file_returns_metadata_with_document_id() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/data.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("hello world"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert!(json["document_id"].is_string(), "missing document_id");
  let document_id = json["document_id"].as_str().unwrap();
  uuid::Uuid::parse_str(document_id).expect("document_id should be a valid UUID");
  assert!(json["name"].is_string(), "missing name");
  assert_eq!(json["name"], "data.txt");
  assert_eq!(json["entry_type"], "file");
  assert!(json["created_at"].is_string(), "missing created_at");
  assert!(json["updated_at"].is_string(), "missing updated_at");
  assert_eq!(json["total_size"], 11);
}

#[tokio::test]
async fn test_store_file_preserves_content_type() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/image.png")
    .header("content-type", "image/png")
    .header("authorization", &auth)
    .body(Body::from(vec![0x89, 0x50, 0x4e, 0x47]))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["content_type"], "image/png");

  // Fetch the file and verify content-type header.
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri("/fs/myapp/image.png")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);
  let content_type = get_response
    .headers()
    .get("content-type")
    .expect("content-type header should be present")
    .to_str()
    .unwrap();
  assert_eq!(content_type, "image/png");
}

// ---------------------------------------------------------------------------
// Get file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_file_returns_data() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store a file.
  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/hello.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("hello world"))
    .unwrap();
  app.oneshot(request).await.unwrap();

  // Get it back.
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri("/fs/myapp/hello.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let body = body_bytes(get_response.into_body()).await;
  assert_eq!(body, b"hello world");
}

#[tokio::test]
async fn test_get_file_returns_correct_content_type() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/data.xml")
    .header("content-type", "application/xml")
    .header("authorization", &auth)
    .body(Body::from("<root/>"))
    .unwrap();
  app.oneshot(request).await.unwrap();

  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri("/fs/myapp/data.xml")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let content_type = get_response
    .headers()
    .get("content-type")
    .expect("content-type header expected")
    .to_str()
    .unwrap();
  assert_eq!(content_type, "application/xml");
}

#[tokio::test]
async fn test_get_file_returns_404_for_missing() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/fs/nonexistent/file.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Directory listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_directory_returns_listing() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store two files in the same directory.
  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/file1.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("content1"))
    .unwrap();
  app.oneshot(request).await.unwrap();

  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let request2 = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/file2.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("content2"))
    .unwrap();
  app2.oneshot(request2).await.unwrap();

  // List the directory.
  let app3 = rebuild_app(&storage, &jwt_manager, &engine);
  let list_request = Request::builder()
    .uri("/fs/myapp")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let list_response = app3.oneshot(list_request).await.unwrap();
  assert_eq!(list_response.status(), StatusCode::OK);

  let json = body_json(list_response.into_body()).await;
  let array = json.as_array().expect("response should be an array");
  assert_eq!(array.len(), 2, "should list 2 files");

  // Entries should have metadata.
  for entry in array {
    assert!(entry["name"].is_string());
    assert!(entry["document_id"].is_string());
    assert!(entry["entry_type"].is_string());
  }
}

#[tokio::test]
async fn test_get_directory_empty_returns_empty_array() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store a file in a subdirectory so "emptydir" parent gets created.
  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/emptydir/subdir/file.txt")
    .header("authorization", &auth)
    .body(Body::from("data"))
    .unwrap();
  app.oneshot(request).await.unwrap();

  // List "emptydir" which should have only the "subdir" entry.
  // But let's create a truly empty dir by storing a file and then listing
  // its sibling empty dir. Actually, "emptydir" will contain "subdir".
  // Let's just create a directory by storing a file directly in it,
  // then create a separate empty subdirectory.
  // The simplest approach: create a file at /fs/parent/child/file.txt
  // and then list /fs/parent/child which has only file.txt.
  // We want an empty directory. Let's use the fact that storing a file
  // at /fs/a/b/c.txt creates dirs /a, /a/b. But /a/b only has c.txt.
  // So if we create /fs/withempty/populated/f.txt, then /fs/withempty
  // has only "populated". Not empty either.
  // Actually, the simplest way: if we store /fs/root/file.txt, the root
  // dir "/" has entry "root". Then /root has "file.txt". But there is
  // no empty dir naturally. However, if we just list a directory that
  // has no entries but exists as a table, it should return [].
  // Actually, mkdir -p creates the table. So let's check:
  // /fs/hasempty/empty is created as a directory when we store
  // /fs/hasempty/empty/deep/file.txt. Then /fs/hasempty/empty has "deep".
  // The only way to get truly empty is to never put anything in it.
  // But our API only creates dirs implicitly. The parent of a stored file
  // always has at least one entry.
  //
  // Simple approach: just verify the dir has entries and count them.
  // For a truly empty check, let's just assert that listing a newly
  // created parent dir (before adding children) works. But we can't
  // do that via HTTP alone.
  //
  // Alternative: create /fs/solo/file.txt which creates /solo with
  // file.txt in it. Then delete /fs/solo/file.txt. Now /solo is empty.
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let store_request = Request::builder()
    .method("PUT")
    .uri("/fs/emptytest/temp.txt")
    .header("authorization", &auth)
    .body(Body::from("temporary"))
    .unwrap();
  app2.oneshot(store_request).await.unwrap();

  // Delete the file.
  let app3 = rebuild_app(&storage, &jwt_manager, &engine);
  let delete_request = Request::builder()
    .method("DELETE")
    .uri("/fs/emptytest/temp.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  app3.oneshot(delete_request).await.unwrap();

  // List the now-empty directory.
  let app4 = rebuild_app(&storage, &jwt_manager, &engine);
  let list_request = Request::builder()
    .uri("/fs/emptytest")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let list_response = app4.oneshot(list_request).await.unwrap();
  assert_eq!(list_response.status(), StatusCode::OK);

  let json = body_json(list_response.into_body()).await;
  let array = json.as_array().expect("response should be an array");
  assert!(array.is_empty(), "directory should be empty after deleting its only file");
}

// ---------------------------------------------------------------------------
// Delete file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_file_returns_200() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store a file.
  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/deleteme.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("to be deleted"))
    .unwrap();
  app.oneshot(request).await.unwrap();

  // Delete it.
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let delete_request = Request::builder()
    .method("DELETE")
    .uri("/fs/myapp/deleteme.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let delete_response = app2.oneshot(delete_request).await.unwrap();
  assert_eq!(delete_response.status(), StatusCode::OK);

  let json = body_json(delete_response.into_body()).await;
  assert_eq!(json["name"], "deleteme.txt");
  assert_eq!(json["entry_type"], "file");
  assert!(json["document_id"].is_string());
}

#[tokio::test]
async fn test_delete_file_returns_404_for_missing() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/fs/nonexistent/file.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Intermediate directories
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_store_creates_intermediate_directories() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store at a deep path.
  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/deep/nested/path/file.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("deep data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Verify we can list intermediate directories.
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let list_request = Request::builder()
    .uri("/fs/deep/nested")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let list_response = app2.oneshot(list_request).await.unwrap();
  assert_eq!(list_response.status(), StatusCode::OK);

  let json = body_json(list_response.into_body()).await;
  let array = json.as_array().unwrap();
  assert_eq!(array.len(), 1);
  assert_eq!(array[0]["name"], "path");
  assert_eq!(array[0]["entry_type"], "directory");
}

// ---------------------------------------------------------------------------
// Roundtrip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_store_and_get_roundtrip() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let data = "roundtrip test data with special chars: <>&\"'";

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/roundtrip/test.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from(data))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri("/fs/roundtrip/test.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let body = body_bytes(get_response.into_body()).await;
  assert_eq!(String::from_utf8(body).unwrap(), data);
}

// ---------------------------------------------------------------------------
// Overwrite existing file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_overwrite_existing_file() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store initial version.
  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/overwrite.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("version 1"))
    .unwrap();
  app.oneshot(request).await.unwrap();

  // Overwrite with new content.
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let request2 = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/overwrite.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("version 2"))
    .unwrap();

  let response2 = app2.oneshot(request2).await.unwrap();
  assert_eq!(response2.status(), StatusCode::CREATED);

  // Verify the overwritten content.
  let app3 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri("/fs/myapp/overwrite.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app3.oneshot(get_request).await.unwrap();
  let body = body_bytes(get_response.into_body()).await;
  assert_eq!(String::from_utf8(body).unwrap(), "version 2");
}

// ---------------------------------------------------------------------------
// HEAD request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_head_returns_metadata_headers() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Store a file.
  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/headtest.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("head test data"))
    .unwrap();
  app.oneshot(request).await.unwrap();

  // HEAD request.
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let head_request = Request::builder()
    .method("HEAD")
    .uri("/fs/myapp/headtest.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let head_response = app2.oneshot(head_request).await.unwrap();
  assert_eq!(head_response.status(), StatusCode::OK);

  assert!(head_response.headers().get("X-Document-Id").is_some());
  assert!(head_response.headers().get("X-Created-At").is_some());
  assert!(head_response.headers().get("X-Updated-At").is_some());
  assert!(head_response.headers().get("X-Total-Size").is_some());
  assert_eq!(
    head_response.headers().get("X-Entry-Type").unwrap().to_str().unwrap(),
    "file"
  );
  assert_eq!(
    head_response.headers().get("content-type").unwrap().to_str().unwrap(),
    "text/plain"
  );
  assert_eq!(
    head_response.headers().get("X-Name").unwrap().to_str().unwrap(),
    "headtest.txt"
  );

  // Body should be empty for HEAD.
  let body = body_bytes(head_response.into_body()).await;
  assert!(body.is_empty(), "HEAD response body should be empty");
}

#[tokio::test]
async fn test_head_returns_404_for_missing() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("HEAD")
    .uri("/fs/nonexistent/file.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Auth required
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_filesystem_routes_require_auth() {
  let (app, _, _, _, _temp_dir) = test_app();

  // PUT without auth.
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/file.txt")
    .body(Body::from("data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Binary data
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_store_file_with_binary_data() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let binary_data: Vec<u8> = (0u8..=255).collect();

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/binary.bin")
    .header("content-type", "application/octet-stream")
    .header("authorization", &auth)
    .body(Body::from(binary_data.clone()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Fetch and verify.
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri("/fs/myapp/binary.bin")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let body = body_bytes(get_response.into_body()).await;
  assert_eq!(body, binary_data);
}

// ---------------------------------------------------------------------------
// Large file (multi-chunk)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_store_and_get_large_file() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Default chunk size is 64KB. Create data larger than one chunk.
  let large_data: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/largefile.bin")
    .header("content-type", "application/octet-stream")
    .header("authorization", &auth)
    .body(Body::from(large_data.clone()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["total_size"], 200_000);

  // Fetch and verify all data matches.
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri("/fs/myapp/largefile.bin")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let body = body_bytes(get_response.into_body()).await;
  assert_eq!(body.len(), large_data.len());
  assert_eq!(body, large_data);
}

// ---------------------------------------------------------------------------
// Dot-prefix paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_dot_prefix_paths_work() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/myapp/.config")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"hidden":true}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["name"], ".config");

  // Fetch it back.
  let app2 = rebuild_app(&storage, &jwt_manager, &engine);
  let get_request = Request::builder()
    .uri("/fs/myapp/.config")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let get_response = app2.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let body = body_bytes(get_response.into_body()).await;
  assert_eq!(String::from_utf8(body).unwrap(), r#"{"hidden":true}"#);
}

// ---------------------------------------------------------------------------
// List directory after multiple stores
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_directory_after_multiple_stores() {
  let (_, jwt_manager, storage, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let file_names = ["alpha.txt", "beta.txt", "gamma.txt", "delta.txt"];
  for file_name in &file_names {
    let app = rebuild_app(&storage, &jwt_manager, &engine);
    let request = Request::builder()
      .method("PUT")
      .uri(format!("/fs/listing/{}", file_name))
      .header("content-type", "text/plain")
      .header("authorization", &auth)
      .body(Body::from(format!("content of {}", file_name)))
      .unwrap();
    app.oneshot(request).await.unwrap();
  }

  // List the directory.
  let app = rebuild_app(&storage, &jwt_manager, &engine);
  let list_request = Request::builder()
    .uri("/fs/listing")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let list_response = app.oneshot(list_request).await.unwrap();
  assert_eq!(list_response.status(), StatusCode::OK);

  let json = body_json(list_response.into_body()).await;
  let array = json.as_array().expect("response should be an array");
  assert_eq!(array.len(), 4, "should have 4 files");

  // Entries should be sorted by name (redb sorts &str keys).
  let names: Vec<&str> = array.iter().map(|entry| entry["name"].as_str().unwrap()).collect();
  assert_eq!(names, vec!["alpha.txt", "beta.txt", "delta.txt", "gamma.txt"]);
}
