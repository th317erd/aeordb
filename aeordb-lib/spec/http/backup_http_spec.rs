use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
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
    ops.store_file(&ctx, "/docs/hello.txt", b"Hello World", Some("text/plain"))
        .unwrap();
    ops.store_file(&ctx, "/docs/goodbye.txt", b"Goodbye World", Some("text/plain"))
        .unwrap();
}

// ─── 1. test_export_head_returns_aeordb ─────────────────────────────────

#[tokio::test]
async fn test_export_head_returns_aeordb() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    seed_engine(&engine);
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/versions/export")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_type, "application/octet-stream");

    let disposition = response
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap();
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
    assert!(
        json["error"].as_str().unwrap().contains("Invalid hash"),
        "error should mention invalid hash, got: {}",
        json
    );
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
    // Should fail with 500 (the engine error gets wrapped)
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

// ─── 4. test_import_export_round_trip ───────────────────────────────────

#[tokio::test]
async fn test_import_export_round_trip() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    seed_engine(&engine);
    let auth = bearer_token(&jwt_manager);

    // Export
    let export_request = Request::builder()
        .method("POST")
        .uri("/versions/export")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

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
    assert!(
        json["error"].as_str().unwrap().contains("Invalid hash"),
        "error should mention invalid hash"
    );
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
    assert!(
        json["error"].as_str().unwrap().contains("not found"),
        "error should mention not found"
    );
}

// ─── 8. test_import_without_auth_fails ──────────────────────────────────

#[tokio::test]
async fn test_import_without_auth_fails() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let request = Request::builder()
        .method("POST")
        .uri("/versions/import")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── 9. test_export_without_auth_fails ──────────────────────────────────

#[tokio::test]
async fn test_export_without_auth_fails() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let request = Request::builder()
        .method("POST")
        .uri("/versions/export")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── 10. test_promote_without_auth_fails ────────────────────────────────

#[tokio::test]
async fn test_promote_without_auth_fails() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let request = Request::builder()
        .method("POST")
        .uri("/versions/promote?hash=abc123")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── 11. test_diff_without_auth_fails ───────────────────────────────────

#[tokio::test]
async fn test_diff_without_auth_fails() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let request = Request::builder()
        .method("POST")
        .uri("/versions/diff?from=abc")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ─── 12. test_import_empty_body ─────────────────────────────────────────

#[tokio::test]
async fn test_import_empty_body() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/versions/import")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    // An empty body won't produce a valid .aeordb file
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ─── 13. test_import_with_force_param ───────────────────────────────────

#[tokio::test]
async fn test_import_with_force_and_promote_params() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    seed_engine(&engine);
    let auth = bearer_token(&jwt_manager);

    // First export to get valid data
    let export_request = Request::builder()
        .method("POST")
        .uri("/versions/export")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

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
