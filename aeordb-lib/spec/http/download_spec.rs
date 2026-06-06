use std::sync::Arc;
use std::io::Read;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::RequestContext;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<aeordb::engine::StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
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

fn store_test_files(engine: &aeordb::engine::StorageEngine) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  ops.store_file_buffered(&ctx, "/docs/readme.md", b"# Hello", Some("text/markdown")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/notes.txt", b"Some notes", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/images/logo.svg", b"<svg></svg>", Some("image/svg+xml")).unwrap();
}

#[tokio::test]
async fn download_zip_with_valid_paths() {
  let (app, jwt_manager, engine, _temp) = test_app();
  store_test_files(&engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": ["/docs/readme.md", "/docs/notes.txt"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/download")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(response.headers().get("content-type").unwrap(), "application/zip");

  let bytes = response.into_body().collect().await.unwrap().to_bytes();
  let reader = std::io::Cursor::new(bytes.to_vec());
  let mut archive = zip::ZipArchive::new(reader).expect("valid ZIP");
  assert_eq!(archive.len(), 2);

  // Both files are in /docs/ — common prefix stripped, so entries are just filenames
  let mut readme = archive.by_name("readme.md").expect("readme.md in ZIP");
  let mut content = String::new();
  readme.read_to_string(&mut content).unwrap();
  assert_eq!(content, "# Hello");
}

#[tokio::test]
async fn download_zip_skips_missing_paths() {
  let (app, jwt_manager, engine, _temp) = test_app();
  store_test_files(&engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": ["/docs/readme.md", "/nonexistent.txt"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/download")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = response.into_body().collect().await.unwrap().to_bytes();
  let reader = std::io::Cursor::new(bytes.to_vec());
  let archive = zip::ZipArchive::new(reader).expect("valid ZIP");
  assert_eq!(archive.len(), 1, "should only contain the valid file");
}

#[tokio::test]
async fn download_zip_empty_paths_returns_400() {
  let (app, jwt_manager, _engine, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": [] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/download")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn download_zip_includes_folder_contents() {
  let (app, jwt_manager, engine, _temp) = test_app();
  store_test_files(&engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": ["/docs"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/download")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = response.into_body().collect().await.unwrap().to_bytes();
  let reader = std::io::Cursor::new(bytes.to_vec());
  let archive = zip::ZipArchive::new(reader).expect("valid ZIP");
  assert_eq!(archive.len(), 2, "should include both files in /docs/");
}

#[tokio::test]
async fn download_zip_skips_system_paths() {
  let (app, jwt_manager, engine, _temp) = test_app();
  store_test_files(&engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": ["/docs/readme.md", "/.aeordb-system/config"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/download")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let bytes = response.into_body().collect().await.unwrap().to_bytes();
  let reader = std::io::Cursor::new(bytes.to_vec());
  let archive = zip::ZipArchive::new(reader).expect("valid ZIP");
  assert_eq!(archive.len(), 1, "should skip .system/ path");
}

#[tokio::test]
async fn download_zip_requires_auth() {
  let (app, _jwt_manager, _engine, _temp) = test_app();

  let body = serde_json::json!({ "paths": ["/docs/readme.md"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/download")
    .header("content-type", "application/json")
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

async fn response_json(response: axum::response::Response) -> serde_json::Value {
  let bytes = response.into_body().collect().await.unwrap().to_bytes();
  serde_json::from_slice(&bytes).expect("valid JSON response")
}

#[tokio::test]
async fn batch_fetch_returns_file_map_with_metadata_and_content() {
  let (app, jwt_manager, engine, _temp) = test_app();
  store_test_files(&engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": ["/docs/readme.md", "/docs/notes.txt"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = response_json(response).await;
  let readme = &json["/docs/readme.md"];
  assert_eq!(readme["name"], "readme.md");
  assert_eq!(readme["path"], "/docs/readme.md");
  assert_eq!(readme["content_type"], "text/markdown");
  assert_eq!(readme["size"], 7);
  assert!(readme["created_at"].is_number());
  assert!(readme["updated_at"].is_number());
  assert_eq!(readme["content"], "# Hello");

  let notes = &json["/docs/notes.txt"];
  assert_eq!(notes["name"], "notes.txt");
  assert_eq!(notes["content_type"], "text/plain");
  assert_eq!(notes["size"], 10);
  assert_eq!(notes["content"], "Some notes");
}

#[tokio::test]
async fn batch_fetch_empty_paths_returns_400() {
  let (app, jwt_manager, _engine, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": [] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn batch_fetch_missing_path_returns_404() {
  let (app, jwt_manager, engine, _temp) = test_app();
  store_test_files(&engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": ["/docs/readme.md", "/missing.txt"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn batch_fetch_directory_path_returns_404() {
  let (app, jwt_manager, engine, _temp) = test_app();
  store_test_files(&engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": ["/docs"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn batch_fetch_system_path_returns_404() {
  let (app, jwt_manager, engine, _temp) = test_app();
  store_test_files(&engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": ["/docs/readme.md", "/.aeordb-system/config"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn batch_fetch_requires_auth() {
  let (app, _jwt_manager, _engine, _temp) = test_app();

  let body = serde_json::json!({ "paths": ["/docs/readme.md"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn batch_fetch_rejects_too_many_paths() {
  let (app, jwt_manager, _engine, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);
  let paths: Vec<String> = (0..10_001).map(|i| format!("/bulk/{}.txt", i)).collect();

  let body = serde_json::json!({ "paths": paths });
  let request = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn batch_fetch_respects_request_max_bytes_cap() {
  let (app, jwt_manager, engine, _temp) = test_app();
  store_test_files(&engine);
  let auth = bearer_token(&jwt_manager);

  let body = serde_json::json!({ "paths": ["/docs/readme.md"], "max_bytes": 3 });
  let request = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn batch_fetch_binary_content_is_encoded_as_lossy_utf8_string() {
  let (app, jwt_manager, engine, _temp) = test_app();
  let auth = bearer_token(&jwt_manager);
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&ctx, "/bin/data.bin", &[0x66, 0xFF, 0x67], Some("application/octet-stream")).unwrap();

  let body = serde_json::json!({ "paths": ["/bin/data.bin"] });
  let request = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let json = response_json(response).await;
  assert_eq!(json["/bin/data.bin"]["content"], "f\u{FFFD}g");
}
