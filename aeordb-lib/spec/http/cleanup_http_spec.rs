use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::magic_link::MagicLinkRecord;
use aeordb::auth::refresh::RefreshTokenRecord;
use aeordb::engine::system_store;
use aeordb::engine::{RequestContext, StorageEngine, TaskQueue};
use aeordb::server::{create_app_with_jwt_engine_and_task_queue, create_temp_engine_for_tests};

/// Create a fresh in-memory app with engine and task queue support.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, Arc<TaskQueue>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let task_queue = Arc::new(TaskQueue::new(engine.clone()));
    let app = create_app_with_jwt_engine_and_task_queue(jwt_manager.clone(), engine.clone(), task_queue.clone());
    (app, jwt_manager, engine, task_queue, temp_dir)
}

/// Rebuild app from shared state (for multi-request tests).
fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>, task_queue: &Arc<TaskQueue>) -> axum::Router {
    create_app_with_jwt_engine_and_task_queue(jwt_manager.clone(), engine.clone(), task_queue.clone())
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

/// Collect response body into JSON.
async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ===========================================================================
// 1. test_trigger_cleanup_endpoint — POST /admin/tasks/cleanup returns 200
// ===========================================================================

#[tokio::test]
async fn test_trigger_cleanup_endpoint() {
    let (app, jwt_manager, engine, _task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);
    let ctx = RequestContext::system();

    // Seed some expired tokens and used magic links
    let expired_token = RefreshTokenRecord {
        token_hash: "cleanup-test-token".to_string(),
        user_subject: "test-user".to_string(),
        created_at: Utc::now() - Duration::hours(2),
        expires_at: Utc::now() - Duration::hours(1),
        is_revoked: false,
    };
    system_store::store_refresh_token(&engine, &ctx, &expired_token).unwrap();

    let used_link = MagicLinkRecord {
        code_hash: "cleanup-test-link".to_string(),
        email: "test@example.com".to_string(),
        created_at: Utc::now() - Duration::hours(1),
        expires_at: Utc::now() + Duration::minutes(10),
        is_used: true,
    };
    system_store::store_magic_link(&engine, &ctx, &used_link).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/admin/tasks/cleanup")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["tokens_cleaned"], 1, "should report 1 token cleaned");
    assert_eq!(json["links_cleaned"], 1, "should report 1 link cleaned");
}

// ===========================================================================
// 2. test_cleanup_endpoint_empty — nothing to clean returns 200 with zeros
// ===========================================================================

#[tokio::test]
async fn test_cleanup_endpoint_empty() {
    let (app, jwt_manager, _engine, _task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/admin/tasks/cleanup")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["tokens_cleaned"], 0);
    assert_eq!(json["links_cleaned"], 0);
}

// ===========================================================================
// 3. test_cleanup_endpoint_requires_auth
// ===========================================================================

#[tokio::test]
async fn test_cleanup_endpoint_requires_auth() {
    let (app, _jwt_manager, _engine, _task_queue, _temp_dir) = test_app();

    let request = Request::builder()
        .method("POST")
        .uri("/admin/tasks/cleanup")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// 4. test_cleanup_endpoint_requires_root
// ===========================================================================

#[tokio::test]
async fn test_cleanup_endpoint_requires_root() {
    let (app, jwt_manager, _engine, _task_queue, _temp_dir) = test_app();

    // Non-root user
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
    let auth = format!("Bearer {}", token);

    let request = Request::builder()
        .method("POST")
        .uri("/admin/tasks/cleanup")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// 5. test_cleanup_endpoint_preserves_valid — verify valid tokens survive
// ===========================================================================

#[tokio::test]
async fn test_cleanup_endpoint_preserves_valid() {
    let (app, jwt_manager, engine, _task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);
    let ctx = RequestContext::system();

    // Store a valid token and a valid link
    let valid_token = RefreshTokenRecord {
        token_hash: "valid-token".to_string(),
        user_subject: "test-user".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::days(30),
        is_revoked: false,
    };
    system_store::store_refresh_token(&engine, &ctx, &valid_token).unwrap();

    let valid_link = MagicLinkRecord {
        code_hash: "valid-link".to_string(),
        email: "test@example.com".to_string(),
        created_at: Utc::now(),
        expires_at: Utc::now() + Duration::minutes(10),
        is_used: false,
    };
    system_store::store_magic_link(&engine, &ctx, &valid_link).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/admin/tasks/cleanup")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["tokens_cleaned"], 0, "valid tokens should not be cleaned");
    assert_eq!(json["links_cleaned"], 0, "valid links should not be cleaned");

    // Verify they still exist
    assert!(system_store::get_refresh_token(&engine, "valid-token").unwrap().is_some());
    assert!(system_store::get_magic_link(&engine, "valid-link").unwrap().is_some());
}

// ===========================================================================
// 6. test_cleanup_endpoint_idempotent — calling twice yields zeros on second
// ===========================================================================

#[tokio::test]
async fn test_cleanup_endpoint_idempotent() {
    let (app, jwt_manager, engine, task_queue, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);
    let ctx = RequestContext::system();

    // Seed an expired token
    let expired_token = RefreshTokenRecord {
        token_hash: "idempotent-test".to_string(),
        user_subject: "test-user".to_string(),
        created_at: Utc::now() - Duration::hours(48),
        expires_at: Utc::now() - Duration::hours(24),
        is_revoked: false,
    };
    system_store::store_refresh_token(&engine, &ctx, &expired_token).unwrap();

    // First call
    let request = Request::builder()
        .method("POST")
        .uri("/admin/tasks/cleanup")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    assert_eq!(json["tokens_cleaned"], 1);

    // Second call
    let app2 = rebuild_app(&jwt_manager, &engine, &task_queue);
    let request = Request::builder()
        .method("POST")
        .uri("/admin/tasks/cleanup")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app2.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    assert_eq!(json["tokens_cleaned"], 0, "second cleanup should find nothing");
    assert_eq!(json["links_cleaned"], 0);
}
