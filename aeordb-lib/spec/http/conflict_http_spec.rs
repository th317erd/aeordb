use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::conflict_store;
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::merge::{ConflictEntry, ConflictType, ConflictVersion};
use aeordb::engine::RequestContext;
use aeordb::engine::StorageEngine;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
    (app, jwt_manager, engine, temp_dir)
}

fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
    create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

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

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

/// Seed a conflict into the engine for testing.
fn seed_conflict(engine: &StorageEngine, path: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);

    // Store winner at the real path
    ops.store_file(&ctx, path, b"winner-content", Some("text/plain"))
        .unwrap();

    // Store loser at a temp path
    let loser_path = format!("/tmp/loser{}", path);
    ops.store_file(&ctx, &loser_path, b"loser-content", Some("text/plain"))
        .unwrap();

    let algo = engine.hash_algo();
    let winner_hash = aeordb::engine::directory_ops::file_identity_hash(
        path,
        Some("text/plain"),
        &[],
        &algo,
    )
    .unwrap();
    let loser_hash = aeordb::engine::directory_ops::file_identity_hash(
        &loser_path,
        Some("text/plain"),
        &[],
        &algo,
    )
    .unwrap();

    let conflict = ConflictEntry {
        path: path.to_string(),
        conflict_type: ConflictType::ConcurrentModify,
        winner: ConflictVersion {
            hash: winner_hash,
            virtual_time: 200,
            node_id: 1,
            size: 14,
            content_type: Some("text/plain".to_string()),
        },
        loser: ConflictVersion {
            hash: loser_hash,
            virtual_time: 100,
            node_id: 2,
            size: 13,
            content_type: Some("text/plain".to_string()),
        },
    };

    conflict_store::store_conflict(engine, &ctx, &conflict).unwrap();
}

// ===========================================================================
// GET /admin/conflicts — list empty
// ===========================================================================

#[tokio::test]
async fn test_list_conflicts_empty() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/sync/conflicts")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert!(json["items"].as_array().unwrap().is_empty());
}

// ===========================================================================
// GET /admin/conflicts — list with conflicts
// ===========================================================================

#[tokio::test]
async fn test_list_conflicts() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    seed_conflict(&engine, "/docs/a.txt");
    seed_conflict(&engine, "/docs/b.txt");

    let auth = root_bearer_token(&jwt_manager);
    let app = rebuild_app(&jwt_manager, &engine);

    let request = Request::builder()
        .method("GET")
        .uri("/sync/conflicts")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let arr = json["items"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

// ===========================================================================
// GET /admin/conflicts/{path} — get specific conflict
// ===========================================================================

#[tokio::test]
async fn test_get_conflict() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    seed_conflict(&engine, "/docs/target.txt");

    let auth = root_bearer_token(&jwt_manager);
    let app = rebuild_app(&jwt_manager, &engine);

    let request = Request::builder()
        .method("GET")
        .uri("/sync/conflicts/docs/target.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["path"], "/docs/target.txt");
    assert_eq!(json["conflict_type"], "ConcurrentModify");
}

// ===========================================================================
// GET /admin/conflicts/{path} — not found
// ===========================================================================

#[tokio::test]
async fn test_get_conflict_not_found() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/sync/conflicts/nonexistent.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// POST /admin/conflict-dismiss/{path} — dismiss conflict
// ===========================================================================

#[tokio::test]
async fn test_dismiss_conflict() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    seed_conflict(&engine, "/docs/dismiss.txt");

    let auth = root_bearer_token(&jwt_manager);
    let app = rebuild_app(&jwt_manager, &engine);

    let request = Request::builder()
        .method("POST")
        .uri("/sync/dismiss/docs/dismiss.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["dismissed"], true);

    // Verify it's gone
    let app2 = rebuild_app(&jwt_manager, &engine);
    let request2 = Request::builder()
        .method("GET")
        .uri("/sync/conflicts/docs/dismiss.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let response2 = app2.oneshot(request2).await.unwrap();
    assert_eq!(response2.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// POST /admin/conflict-dismiss/{path} — dismiss nonexistent
// ===========================================================================

#[tokio::test]
async fn test_dismiss_conflict_not_found() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/sync/dismiss/nonexistent.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// POST /admin/conflict-resolve/{path} — resolve with invalid pick
// ===========================================================================

#[tokio::test]
async fn test_resolve_conflict_invalid_pick() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    seed_conflict(&engine, "/docs/invalid-pick.txt");

    let auth = root_bearer_token(&jwt_manager);
    let app = rebuild_app(&jwt_manager, &engine);

    let request = Request::builder()
        .method("POST")
        .uri("/sync/resolve/docs/invalid-pick.txt")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"pick": "neither"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// POST /admin/conflict-resolve/{path} — resolve nonexistent
// ===========================================================================

#[tokio::test]
async fn test_resolve_conflict_not_found() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/sync/resolve/nonexistent.txt")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"pick": "winner"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// Auth required — no token
// ===========================================================================

#[tokio::test]
async fn test_conflicts_no_auth_returns_401() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let request = Request::builder()
        .method("GET")
        .uri("/sync/conflicts")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// Auth required — non-root gets 403
// ===========================================================================

#[tokio::test]
async fn test_conflicts_non_root_returns_403() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = non_root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/sync/conflicts")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// Auth required on all conflict endpoints
// ===========================================================================

#[tokio::test]
async fn test_all_conflict_endpoints_require_auth() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    // GET /admin/conflicts
    let r = Request::builder()
        .method("GET")
        .uri("/sync/conflicts")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(r).await.unwrap().status(), StatusCode::UNAUTHORIZED);

    // GET /admin/conflicts/path
    let r = Request::builder()
        .method("GET")
        .uri("/sync/conflicts/some/path")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(r).await.unwrap().status(), StatusCode::UNAUTHORIZED);

    // POST /admin/conflict-resolve/path
    let r = Request::builder()
        .method("POST")
        .uri("/sync/resolve/some/path")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"pick":"winner"}"#))
        .unwrap();
    assert_eq!(app.clone().oneshot(r).await.unwrap().status(), StatusCode::UNAUTHORIZED);

    // POST /admin/conflict-dismiss/path
    let r = Request::builder()
        .method("POST")
        .uri("/sync/dismiss/some/path")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(r).await.unwrap().status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// Non-root forbidden on all conflict endpoints
// ===========================================================================

#[tokio::test]
async fn test_all_conflict_endpoints_forbid_non_root() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = non_root_bearer_token(&jwt_manager);

    // GET /admin/conflicts
    let r = Request::builder()
        .method("GET")
        .uri("/sync/conflicts")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(r).await.unwrap().status(), StatusCode::FORBIDDEN);

    // GET /admin/conflicts/path
    let r = Request::builder()
        .method("GET")
        .uri("/sync/conflicts/some/path")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(r).await.unwrap().status(), StatusCode::FORBIDDEN);

    // POST /admin/conflict-resolve/path
    let r = Request::builder()
        .method("POST")
        .uri("/sync/resolve/some/path")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"pick":"winner"}"#))
        .unwrap();
    assert_eq!(app.clone().oneshot(r).await.unwrap().status(), StatusCode::FORBIDDEN);

    // POST /admin/conflict-dismiss/path
    let r = Request::builder()
        .method("POST")
        .uri("/sync/dismiss/some/path")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.clone().oneshot(r).await.unwrap().status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// Expired token rejected
// ===========================================================================

#[tokio::test]
async fn test_conflicts_expired_token_returns_401() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();

    let now = chrono::Utc::now().timestamp();
    let claims = TokenClaims {
        sub: "00000000-0000-0000-0000-000000000000".to_string(),
        iss: "aeordb".to_string(),
        iat: now - 7200,
        exp: now - 3600,
        scope: None,
        permissions: None,
        key_id: None,
    };
    let token = jwt_manager.create_token(&claims).expect("create token");
    let auth = format!("Bearer {}", token);

    let request = Request::builder()
        .method("GET")
        .uri("/sync/conflicts")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
