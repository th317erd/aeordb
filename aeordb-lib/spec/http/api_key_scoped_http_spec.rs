use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;
use chrono::Utc;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::api_key::{ApiKeyRecord, hash_api_key, generate_api_key};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::engine::api_key_rules::KeyRule;
use aeordb::engine::system_store;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

// ===========================================================================
// Shared test infrastructure
// ===========================================================================

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
    (app, jwt_manager, engine, temp_dir)
}

fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
    create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

/// Create a scoped API key record directly in the engine and return a JWT with key_id.
fn create_scoped_key_and_token(
    jwt_manager: &JwtManager,
    engine: &StorageEngine,
    user_id: Uuid,
    rules: Vec<KeyRule>,
) -> (String, Uuid) {
    create_scoped_key_and_token_with_expiry(
        jwt_manager,
        engine,
        user_id,
        rules,
        Utc::now().timestamp_millis() + (365 * 86400 * 1000), // 1 year
    )
}

/// Create a scoped API key record with a specific expiry.
fn create_scoped_key_and_token_with_expiry(
    jwt_manager: &JwtManager,
    engine: &StorageEngine,
    user_id: Uuid,
    rules: Vec<KeyRule>,
    expires_at: i64,
) -> (String, Uuid) {
    let key_id = Uuid::new_v4();
    let plaintext = generate_api_key(key_id);
    let key_hash = hash_api_key(&plaintext).unwrap();
    let now = Utc::now();

    let record = ApiKeyRecord {
        key_id,
        key_hash,
        user_id: Some(user_id),
        created_at: now,
        is_revoked: false,
        expires_at,
        label: Some("test-scoped-key".to_string()),
        rules,
    };

    let ctx = RequestContext::system();

    system_store::store_api_key_for_bootstrap(engine, &ctx, &record).unwrap();

    // Create JWT with key_id embedded.
    let now_ts = now.timestamp();
    let claims = TokenClaims {
        sub: user_id.to_string(),
        iss: "aeordb".to_string(),
        iat: now_ts,
        exp: now_ts + DEFAULT_EXPIRY_SECONDS,
        scope: None,
        permissions: None,
        key_id: Some(key_id.to_string()),
    };
    let token = jwt_manager.create_token(&claims).unwrap();
    (format!("Bearer {}", token), key_id)
}

/// Create a root-user Bearer token (nil UUID) WITHOUT key_id.
fn root_bearer_token(jwt_manager: &JwtManager) -> String {
    let now = Utc::now().timestamp();
    let claims = TokenClaims {
        sub: Uuid::nil().to_string(),
        iss: "aeordb".to_string(),
        iat: now,
        exp: now + DEFAULT_EXPIRY_SECONDS,
        scope: None,
        permissions: None,
        key_id: None,
    };
    let token = jwt_manager.create_token(&claims).unwrap();
    format!("Bearer {}", token)
}

fn store_file(engine: &StorageEngine, path: &str, content: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file_buffered(&ctx, path, content, None).unwrap();
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

async fn body_bytes(body: Body) -> Vec<u8> {
    body.collect().await.unwrap().to_bytes().to_vec()
}

// ===========================================================================
// Tests
// ===========================================================================

/// Key with rules allowing /allowed/** and denying everything else.
/// Access to /allowed/file.txt should return 200.
#[tokio::test]
async fn test_scoped_key_allowed_path() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "allowed/file.txt", b"hello allowed");

    let rules = vec![
        KeyRule { glob: "/allowed/**".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/allowed/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"hello allowed");
}

/// Key with rules allowing /allowed/** and denying everything else.
/// Access to /denied/file.txt should return 404 (NOT 403).
#[tokio::test]
async fn test_scoped_key_denied_path_returns_404() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "denied/file.txt", b"secret data");

    let rules = vec![
        KeyRule { glob: "/allowed/**".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/denied/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("Not found"));
}

/// Key with rules only for /specific/**. Access to /other/file.txt has no
/// matching rule, so it should return 404.
#[tokio::test]
async fn test_scoped_key_no_matching_rule() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "other/file.txt", b"other data");

    let rules = vec![
        KeyRule { glob: "/specific/**".to_string(), permitted: "crudlify".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/other/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Key with read+list only. GET should succeed, PUT should return 404.
#[tokio::test]
async fn test_scoped_key_read_only() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/file.txt", b"readonly content");

    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "-r--l---".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    // GET should work (read permitted).
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // PUT should be denied (no create/update) — returns 404.
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/files/data/new_file.txt")
                .header("authorization", &token)
                .header("content-type", "application/octet-stream")
                .body(Body::from(b"new data".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// First-match-wins: Rules [("/a/**", "-r------"), ("/a/special/**", "crudlify"), ("/**", "--------")].
/// /a/special/file.txt matches the FIRST rule ("/a/**" with read-only), not the second.
/// So a PUT should be denied.
#[tokio::test]
async fn test_scoped_key_first_match_wins() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "a/special/file.txt", b"special data");

    let rules = vec![
        KeyRule { glob: "/a/**".to_string(), permitted: "-r------".to_string() },
        KeyRule { glob: "/a/special/**".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    // GET should work (read allowed by first rule).
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/a/special/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // DELETE should fail (first rule only allows read).
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/files/a/special/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Expired key should return 401.
#[tokio::test]
async fn test_scoped_key_expired_returns_401() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/file.txt", b"some data");

    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "crudlify".to_string() },
    ];
    // Expired 1 hour ago.
    let expired_at = Utc::now().timestamp_millis() - (3600 * 1000);
    let (token, _key_id) = create_scoped_key_and_token_with_expiry(
        &jwt_manager, &engine, Uuid::nil(), rules, expired_at,
    );

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("expired"));
}

/// Revoked key should return 401.
#[tokio::test]
async fn test_scoped_key_revoked_returns_401() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/file.txt", b"some data");

    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "crudlify".to_string() },
    ];
    let (token, key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    // Revoke the key.
    let ctx = RequestContext::system();

    system_store::revoke_api_key(&engine, &ctx, key_id).unwrap();

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("revoked"));
}

/// JWT without key_id should not trigger key enforcement.
/// Normal permission checks apply.
#[tokio::test]
async fn test_no_key_id_normal_permissions() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/file.txt", b"normal access data");

    let token = root_bearer_token(&jwt_manager);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"normal access data");
}

/// Key with empty rules vec should have no path-level restrictions.
#[tokio::test]
async fn test_empty_rules_full_passthrough() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "anywhere/file.txt", b"passthrough data");

    let rules = vec![];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/anywhere/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"passthrough data");
}

/// Key with full crudlify on all paths. PUT, GET, DELETE should all work.
#[tokio::test]
async fn test_full_crudlify_all_operations() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "crudlify".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    // PUT (create).
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/files/data/crud_file.txt")
                .header("authorization", &token)
                .header("content-type", "application/octet-stream")
                .body(Body::from(b"crud data".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // GET (read).
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/crud_file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"crud data");

    // DELETE.
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/files/data/crud_file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// Key with deny-all on all paths. GET and PUT should both return 404.
#[tokio::test]
async fn test_deny_all_operations() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/file.txt", b"invisible data");

    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    // GET should be denied.
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // PUT should be denied.
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/files/data/file.txt")
                .header("authorization", &token)
                .header("content-type", "application/octet-stream")
                .body(Body::from(b"try write".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Key with read-only (no 'l' flag). Listing a directory should return 404.
#[tokio::test]
async fn test_list_operation_denied() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "dir/file.txt", b"file in dir");

    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "-r------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    // Listing directory (path ends with /) triggers List operation.
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/dir/")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Key with read+list flags. Listing a directory should succeed.
#[tokio::test]
async fn test_list_operation_allowed() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "dir/file.txt", b"file in dir");

    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "-r--l---".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/dir/")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// Key referencing a non-existent key_id in the JWT should return 401.
#[tokio::test]
async fn test_stale_key_id_returns_401() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/file.txt", b"some data");

    // Create a JWT with a key_id that doesn't exist in the engine.
    let fake_key_id = Uuid::new_v4();
    let now = Utc::now().timestamp();
    let claims = TokenClaims {
        sub: Uuid::nil().to_string(),
        iss: "aeordb".to_string(),
        iat: now,
        exp: now + DEFAULT_EXPIRY_SECONDS,
        scope: None,
        permissions: None,
        key_id: Some(fake_key_id.to_string()),
    };
    let token_str = jwt_manager.create_token(&claims).unwrap();
    let token = format!("Bearer {}", token_str);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("not found"));
}

/// Key with malformed (non-UUID) key_id in JWT should return 401.
#[tokio::test]
async fn test_malformed_key_id_returns_401() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/file.txt", b"some data");

    let now = Utc::now().timestamp();
    let claims = TokenClaims {
        sub: Uuid::nil().to_string(),
        iss: "aeordb".to_string(),
        iat: now,
        exp: now + DEFAULT_EXPIRY_SECONDS,
        scope: None,
        permissions: None,
        key_id: Some("not-a-uuid".to_string()),
    };
    let token_str = jwt_manager.create_token(&claims).unwrap();
    let token = format!("Bearer {}", token_str);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Malformed UUID -> cache returns None -> "API key not found"
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Key with create-only permission. PUT new file succeeds, GET fails.
#[tokio::test]
async fn test_create_only_key() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "c-------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    // PUT (create) should work.
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/files/data/new_file.txt")
                .header("authorization", &token)
                .header("content-type", "application/octet-stream")
                .body(Body::from(b"created".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // GET (read) should fail.
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/new_file.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Key with delete-only permission. DELETE works, GET fails.
#[tokio::test]
async fn test_delete_only_key() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/doomed.txt", b"doomed data");

    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "---d----".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    // GET should fail.
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/doomed.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // DELETE should work.
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/files/data/doomed.txt")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// Non-engine routes (like /api-keys) are NOT affected by key rules.
/// Key rules only apply to /engine/ paths.
#[tokio::test]
async fn test_non_engine_routes_bypass_key_rules() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    // Key that denies everything.
    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    // /api-keys is NOT an /engine/ route — should not be blocked by key rules.
    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/auth/keys")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Should succeed (200) because key rules only enforce /engine/ paths.
    assert_eq!(response.status(), StatusCode::OK);
}
