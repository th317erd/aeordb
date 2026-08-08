use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use futures_util::stream;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryOwner};
use aeordb::engine::StorageEngine;
use aeordb::engine::RequestContext;
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

/// Seed engine with test files.
fn seed_engine(engine: &StorageEngine) {
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(engine);
  ops.store_file_buffered(&ctx, "/docs/hello.txt", b"Hello World", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/goodbye.txt", b"Goodbye World", Some("text/plain")).unwrap();
}

fn backup_artifacts_in(directory: &std::path::Path, prefix: &str) -> Vec<std::path::PathBuf> {
  std::fs::read_dir(directory)
    .unwrap()
    .filter_map(Result::ok)
    .map(|entry| entry.path())
    .filter(|path| path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with(prefix)))
    .collect()
}

// ─── 1. test_export_head_returns_aeordb ─────────────────────────────────

#[tokio::test]
async fn test_export_head_returns_aeordb() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  seed_engine(&engine);
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder().method("POST").uri("/versions/export").header("authorization", &auth).body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response.headers().get("content-type").unwrap().to_str().unwrap();
  assert_eq!(content_type, "application/octet-stream");

  let disposition = response.headers().get("content-disposition").unwrap().to_str().unwrap();
  assert!(
    disposition.contains("export-") && disposition.contains(".aeordb"),
    "content-disposition should have export filename, got: {}",
    disposition
  );

  let data = body_bytes(response.into_body()).await;
  assert!(!data.is_empty(), "export body should not be empty");
}

// ─── 2. test_export_invalid_hash ────────────────────────────────────────

#[tokio::test]
async fn test_export_invalid_hash() {
  let (app, jwt_manager, _engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/export?hash=not_valid_hex_zzz")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);

  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("Invalid hash"), "error should mention invalid hash, got: {}", json);
}

#[tokio::test]
async fn test_export_empty_hash_does_not_fall_back_to_head() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  seed_engine(&engine);
  let request = Request::builder()
    .method("POST")
    .uri("/versions/export?hash=")
    .header("authorization", bearer_token(&jwt_manager))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();

  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ─── 3. test_export_nonexistent_snapshot ────────────────────────────────

#[tokio::test]
async fn test_export_nonexistent_snapshot() {
  let (app, jwt_manager, _engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/export?snapshot=nonexistent")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  // The shared engine error mapper preserves the missing-resource status.
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ─── 4. test_import_export_round_trip ───────────────────────────────────

#[tokio::test]
async fn test_import_export_round_trip() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  seed_engine(&engine);
  let auth = bearer_token(&jwt_manager);

  // Export
  let export_request =
    Request::builder().method("POST").uri("/versions/export").header("authorization", &auth).body(Body::empty()).unwrap();

  let export_response = app.oneshot(export_request).await.unwrap();
  assert_eq!(export_response.status(), StatusCode::OK);

  let exported_data = body_bytes(export_response.into_body()).await;
  assert!(!exported_data.is_empty());

  // Import into the same engine (with promote)
  let app2 = rebuild_app(&jwt_manager, &engine);
  let import_request = Request::builder()
    .method("POST")
    .uri("/versions/import?promote=true")
    .header("authorization", &auth)
    .header("content-type", "application/octet-stream")
    .body(Body::from(exported_data))
    .unwrap();

  let import_response = app2.oneshot(import_request).await.unwrap();
  assert_eq!(import_response.status(), StatusCode::OK);

  let json = body_json(import_response.into_body()).await;
  assert_eq!(json["status"], "success");
  assert_eq!(json["backup_type"], "export");
  assert!(json["head_promoted"].as_bool().unwrap());
}

// ─── 5. test_promote_hash ───────────────────────────────────────────────

#[tokio::test]
async fn test_promote_hash() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  seed_engine(&engine);
  let auth = bearer_token(&jwt_manager);

  let head_hash = hex::encode(engine.head_hash().unwrap());

  let request = Request::builder()
    .method("POST")
    .uri(format!("/versions/promote?hash={}", head_hash))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["status"], "success");
  assert_eq!(json["head"], head_hash);
}

// ─── 6. test_promote_invalid_hash ───────────────────────────────────────

#[tokio::test]
async fn test_promote_invalid_hash() {
  let (app, jwt_manager, _engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/versions/promote?hash=zzzz_not_hex")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);

  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("Invalid hash"), "error should mention invalid hash");
}

// ─── 7. test_promote_nonexistent_hash ───────────────────────────────────

#[tokio::test]
async fn test_promote_nonexistent_hash() {
  let (app, jwt_manager, _engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  // Valid hex but won't exist in the DB
  let bogus = hex::encode(vec![0xFF; 32]);

  let request = Request::builder()
    .method("POST")
    .uri(format!("/versions/promote?hash={}", bogus))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);

  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("not found"), "error should mention not found");
}

#[tokio::test]
async fn test_promote_rejects_existing_non_directory_hash() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let head_before = engine.head_hash().unwrap();
  let chunk_key = engine.compute_hash(b"not a namespace root").unwrap();
  engine.store_entry(aeordb::engine::EntryType::Chunk, &chunk_key, b"payload").unwrap();

  let request = Request::builder()
    .method("POST")
    .uri(format!("/versions/promote?hash={}", hex::encode(&chunk_key)))
    .header("authorization", bearer_token(&jwt_manager))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();

  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  assert_eq!(engine.head_hash().unwrap(), head_before, "wrong-type promotion must not move HEAD");
}

#[tokio::test]
async fn test_promote_rejects_a_noncanonical_directory_locator_as_corrupt_storage() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let head_before = engine.head_hash().unwrap();
  let noncanonical_key = vec![0xB4; engine.hash_algo().hash_length()];
  engine.store_entry(aeordb::engine::EntryType::DirectoryIndex, &noncanonical_key, &[]).unwrap();

  let request = Request::builder()
    .method("POST")
    .uri(format!("/versions/promote?hash={}", hex::encode(&noncanonical_key)))
    .header("authorization", bearer_token(&jwt_manager))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();

  assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
  let body = body_json(response.into_body()).await;
  assert_eq!(body["error"], "Promote failed", "corruption details must remain server-side");
  assert_eq!(body["code"], "INTERNAL_ERROR");
  assert_eq!(engine.head_hash().unwrap(), head_before, "noncanonical promotion must not move HEAD");
}

// ─── 8. test_import_without_auth_fails ──────────────────────────────────

#[tokio::test]
async fn test_import_without_auth_fails() {
  let (app, _jwt_manager, _engine, _temp_dir) = test_app();

  let request = Request::builder().method("POST").uri("/versions/import").body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── 9. test_export_without_auth_fails ──────────────────────────────────

#[tokio::test]
async fn test_export_without_auth_fails() {
  let (app, _jwt_manager, _engine, _temp_dir) = test_app();

  let request = Request::builder().method("POST").uri("/versions/export").body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── 10. test_promote_without_auth_fails ────────────────────────────────

#[tokio::test]
async fn test_promote_without_auth_fails() {
  let (app, _jwt_manager, _engine, _temp_dir) = test_app();

  let request = Request::builder().method("POST").uri("/versions/promote?hash=abc123").body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── 11. test_diff_without_auth_fails ───────────────────────────────────

#[tokio::test]
async fn test_diff_without_auth_fails() {
  let (app, _jwt_manager, _engine, _temp_dir) = test_app();

  let request = Request::builder().method("POST").uri("/versions/diff?from=abc").body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── 12. test_import_empty_body ─────────────────────────────────────────

#[tokio::test]
async fn test_import_empty_body() {
  let (app, jwt_manager, _engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder().method("POST").uri("/versions/import").header("authorization", &auth).body(Body::empty()).unwrap();

  let response = app.oneshot(request).await.unwrap();
  // An empty body won't produce a valid .aeordb file
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_import_malformed_nonempty_body_is_bad_request() {
  let (app, jwt_manager, _engine, temp_dir) = test_app();
  let request = Request::builder()
    .method("POST")
    .uri("/versions/import")
    .header("authorization", bearer_token(&jwt_manager))
    .header("content-type", "application/octet-stream")
    .body(Body::from("not an aeordb backup"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();

  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  let json = body_json(response.into_body()).await;
  assert_eq!(json["code"], "INVALID_INPUT");
  assert!(backup_artifacts_in(temp_dir.path(), "aeordb-import-").is_empty());
}

#[tokio::test]
async fn test_import_rejects_declared_body_over_ten_gibibytes_without_artifact() {
  let (app, jwt_manager, _engine, temp_dir) = test_app();
  let request = Request::builder()
    .method("POST")
    .uri("/versions/import")
    .header("authorization", bearer_token(&jwt_manager))
    .header("content-type", "application/octet-stream")
    .header("content-length", (10_u64 * 1024 * 1024 * 1024 + 1).to_string())
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();

  assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
  assert!(backup_artifacts_in(temp_dir.path(), "aeordb-import-").is_empty());
}

// ─── 13. test_import_with_force_param ───────────────────────────────────

#[tokio::test]
async fn test_import_with_force_and_promote_params() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  seed_engine(&engine);
  let auth = bearer_token(&jwt_manager);

  // First export to get valid data
  let export_request =
    Request::builder().method("POST").uri("/versions/export").header("authorization", &auth).body(Body::empty()).unwrap();

  let export_response = app.oneshot(export_request).await.unwrap();
  let exported_data = body_bytes(export_response.into_body()).await;

  // Import with force=true and promote=true
  let app2 = rebuild_app(&jwt_manager, &engine);
  let import_request = Request::builder()
    .method("POST")
    .uri("/versions/import?force=true&promote=true")
    .header("authorization", &auth)
    .body(Body::from(exported_data))
    .unwrap();

  let import_response = app2.oneshot(import_request).await.unwrap();
  assert_eq!(import_response.status(), StatusCode::OK);

  let json = body_json(import_response.into_body()).await;
  assert_eq!(json["status"], "success");
  assert!(json["head_promoted"].as_bool().unwrap());
}

#[tokio::test]
async fn test_import_streams_backup_larger_than_legacy_ten_megabyte_limit() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let mut content = Vec::with_capacity(11 * 1024 * 1024);
  let mut value = 0xD1CE_BA5Eu32;
  for _ in 0..content.capacity() {
    value = value.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    content.push((value >> 24) as u8);
  }
  DirectoryOps::new(&engine)
    .store_file_buffered(&RequestContext::system(), "/large/random.bin", &content, Some("application/octet-stream"))
    .unwrap();

  let export_request =
    Request::builder().method("POST").uri("/versions/export").header("authorization", &auth).body(Body::empty()).unwrap();
  let export_response = app.oneshot(export_request).await.unwrap();
  assert_eq!(export_response.status(), StatusCode::OK);
  let exported_data = body_bytes(export_response.into_body()).await;
  assert!(exported_data.len() > 10 * 1024 * 1024, "fixture must cross the legacy body cap");

  let chunks = exported_data
    .chunks(64 * 1024)
    .map(|chunk| Ok::<_, std::convert::Infallible>(axum::body::Bytes::copy_from_slice(chunk)))
    .collect::<Vec<_>>();
  let import_request = Request::builder()
    .method("POST")
    .uri("/versions/import?force=true&promote=true")
    .header("authorization", &auth)
    .header("content-type", "application/octet-stream")
    .body(Body::from_stream(stream::iter(chunks)))
    .unwrap();

  let import_response = rebuild_app(&jwt_manager, &engine).oneshot(import_request).await.unwrap();
  let status = import_response.status();
  let response_body = body_bytes(import_response.into_body()).await;
  assert_eq!(status, StatusCode::OK, "response: {}", String::from_utf8_lossy(&response_body));
}

#[tokio::test]
async fn test_export_pressure_returns_retryable_service_unavailable() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  seed_engine(&engine);
  let coordinator = engine.memory_coordinator();
  let snapshot = coordinator.snapshot().unwrap();
  let policy = snapshot.policy.unwrap();
  let remaining = policy.ordinary_limit_bytes().saturating_sub(snapshot.accounted_bytes);
  let _pressure = coordinator.reserve(MemoryOwner::Task, remaining.saturating_sub(2 * 1024), AdmissionClass::Workload).unwrap();
  let request = Request::builder()
    .method("POST")
    .uri("/versions/export")
    .header("authorization", bearer_token(&jwt_manager))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();

  assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_export_stream_holds_and_releases_streaming_memory() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  seed_engine(&engine);
  let request = Request::builder()
    .method("POST")
    .uri("/versions/export")
    .header("authorization", bearer_token(&jwt_manager))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let active = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
  assert_eq!(active.active_reservations, 1);
  assert_eq!(active.reserved_bytes, 64 * 1024);

  drop(response);
  let released = engine.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::StreamingRead).unwrap().clone();
  assert_eq!(released.active_reservations, 0);
  assert_eq!(released.reserved_bytes, 0);
}

#[tokio::test]
async fn test_export_stream_uses_database_filesystem_and_cleans_up_on_drop() {
  let (app, jwt_manager, engine, temp_dir) = test_app();
  seed_engine(&engine);
  let request = Request::builder()
    .method("POST")
    .uri("/versions/export")
    .header("authorization", bearer_token(&jwt_manager))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let artifacts = backup_artifacts_in(temp_dir.path(), "aeordb-export-");
  assert_eq!(artifacts.len(), 1, "the active response artifact must live beside the database");

  drop(response);
  assert!(backup_artifacts_in(temp_dir.path(), "aeordb-export-").is_empty(), "dropping the response must remove its artifact");
}
