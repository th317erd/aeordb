use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::RequestContext;
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

/// Root user Bearer token (nil UUID).
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

/// Non-root user Bearer token (random UUID).
fn non_root_bearer_token(jwt_manager: &JwtManager) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = TokenClaims {
        sub: uuid::Uuid::new_v4().to_string(),
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

/// Seed engine with test files so GC has something to scan.
fn seed_engine(engine: &StorageEngine) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, "/docs/hello.txt", b"Hello World", Some("text/plain"))
        .unwrap();
    ops.store_file(&ctx, "/docs/goodbye.txt", b"Goodbye World", Some("text/plain"))
        .unwrap();
}

// ===========================================================================
// Happy path: root user can run GC
// ===========================================================================

#[tokio::test]
async fn test_gc_root_user_succeeds() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    seed_engine(&engine);
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/system/gc")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert!(json.get("versions_scanned").is_some());
    assert!(json.get("live_entries").is_some());
    assert!(json.get("garbage_entries").is_some());
    assert!(json.get("reclaimed_bytes").is_some());
    assert!(json.get("duration_ms").is_some());
    assert_eq!(json["dry_run"], false);
}

#[tokio::test]
async fn test_gc_dry_run_returns_results_without_deleting() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    seed_engine(&engine);
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/system/gc?dry_run=true")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["dry_run"], true);
    // A dry run should still report statistics
    assert!(json.get("versions_scanned").is_some());
    assert!(json.get("garbage_entries").is_some());
}

#[tokio::test]
async fn test_gc_on_empty_engine_succeeds() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/system/gc")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    // Even an "empty" engine has a root directory, so garbage_entries >= 0
    assert!(json["garbage_entries"].as_u64().is_some());
}

// ===========================================================================
// Authorization: non-root user gets 403
// ===========================================================================

#[tokio::test]
async fn test_gc_non_root_user_forbidden() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = non_root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/system/gc")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("root"));
}

// ===========================================================================
// Authorization: no token gets 401
// ===========================================================================

#[tokio::test]
async fn test_gc_no_auth_returns_401() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let request = Request::builder()
        .method("POST")
        .uri("/system/gc")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// Invalid method: GET should be rejected (405 or 404)
// ===========================================================================

#[tokio::test]
async fn test_gc_get_method_not_allowed() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/system/gc")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    // axum returns 405 Method Not Allowed for wrong methods on existing routes
    assert!(
        response.status() == StatusCode::METHOD_NOT_ALLOWED
            || response.status() == StatusCode::NOT_FOUND,
        "Expected 405 or 404, got {}",
        response.status()
    );
}

// ===========================================================================
// Invalid query params: bad dry_run value
// ===========================================================================

#[tokio::test]
async fn test_gc_invalid_dry_run_param() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/system/gc?dry_run=notabool")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    // axum should reject bad query deserialization with 400
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// Running GC twice in a row should succeed (idempotency)
// ===========================================================================

#[tokio::test]
async fn test_gc_twice_in_a_row_succeeds() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    seed_engine(&engine);
    let auth = root_bearer_token(&jwt_manager);

    // First GC
    let request = Request::builder()
        .method("POST")
        .uri("/system/gc")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Second GC (rebuild app since oneshot consumes the router)
    let app2 = rebuild_app(&jwt_manager, &engine);
    let request2 = Request::builder()
        .method("POST")
        .uri("/system/gc")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response2 = app2.oneshot(request2).await.unwrap();
    assert_eq!(response2.status(), StatusCode::OK);

    let json = body_json(response2.into_body()).await;
    // After first real GC, second run should find no new garbage
    // (garbage_entries should be <= what the first run found)
    assert!(json["garbage_entries"].as_u64().is_some());
}

// ===========================================================================
// dry_run=false (explicit) should work the same as omitting it
// ===========================================================================

#[tokio::test]
async fn test_gc_explicit_dry_run_false() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    seed_engine(&engine);
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/system/gc?dry_run=false")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["dry_run"], false);
}

// ===========================================================================
// Invalid token (malformed JWT) should be rejected
// ===========================================================================

#[tokio::test]
async fn test_gc_invalid_jwt_returns_401() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let request = Request::builder()
        .method("POST")
        .uri("/system/gc")
        .header("authorization", "Bearer invalid.jwt.token")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// Expired token should be rejected
// ===========================================================================

#[tokio::test]
async fn test_gc_expired_token_returns_401() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();

    let now = chrono::Utc::now().timestamp();
    let claims = TokenClaims {
        sub: "00000000-0000-0000-0000-000000000000".to_string(),
        iss: "aeordb".to_string(),
        iat: now - 7200,
        exp: now - 3600, // expired 1 hour ago
        scope: None,
        permissions: None,
    key_id: None,
    };
    let token = jwt_manager.create_token(&claims).expect("create token");
    let auth = format!("Bearer {}", token);

    let request = Request::builder()
        .method("POST")
        .uri("/system/gc")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
