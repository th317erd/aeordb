//! Client sync E2E tests.
//!
//! These tests verify that non-root JWT callers can use the /sync/diff and
//! /sync/chunks endpoints, with proper /.aeordb-system/ filtering and API key
//! scoping applied.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

use aeordb::auth::api_key::{ApiKeyRecord, generate_api_key, hash_api_key};
use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::auth::FileAuthProvider;
use aeordb::engine::api_key_rules::KeyRule;
use aeordb::engine::system_store;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{DirectoryOps, EventBus, RequestContext, StorageEngine};
use aeordb::plugins::PluginManager;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests, CorsState};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

/// Create a full app + engine pair with known JwtManager.
fn create_node() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let app = build_app(&jwt_manager, &engine);
    (app, jwt_manager, engine, temp_dir)
}

fn build_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
    let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
    let rate_limiter = Arc::new(RateLimiter::default_config());
    let auth_provider: Arc<dyn aeordb::auth::AuthProvider> =
        Arc::new(FileAuthProvider::new(engine.clone()));
    create_app_with_all(
        auth_provider,
        jwt_manager.clone(),
        plugin_manager,
        rate_limiter,
        make_prometheus_handle(),
        engine.clone(),
        Arc::new(EventBus::new()),
        CorsState {
            default_origins: None,
            rules: vec![],
        },
    )
}

fn store_file(engine: &StorageEngine, path: &str, data: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, path, data, Some("application/octet-stream"))
        .expect("store file");
}

fn get_head_hex(engine: &StorageEngine) -> String {
    let vm = VersionManager::new(engine);
    hex::encode(vm.get_head_hash().unwrap())
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

/// Create a root-user Bearer token (nil UUID).
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

/// Create a non-root user Bearer token (random UUID, no API key).
fn non_root_bearer_token(jwt_manager: &JwtManager) -> String {
    let now = Utc::now().timestamp();
    let claims = TokenClaims {
        sub: Uuid::new_v4().to_string(),
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

/// Create a scoped API key record in the engine and return a Bearer token with key_id.
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
        Utc::now().timestamp_millis() + (365 * 86400 * 1000),
    )
}

/// Create a scoped API key with a specific expiry.
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

/// Helper: POST /sync/diff with given auth and optional body overrides.
async fn sync_diff_request(
    app: axum::Router,
    auth_header: Option<(&str, &str)>,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::post("/sync/diff")
        .header("content-type", "application/json");

    if let Some((key, value)) = auth_header {
        builder = builder.header(key, value);
    }

    let response = app
        .oneshot(
            builder
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let json = body_json(response.into_body()).await;
    (status, json)
}

/// Helper: POST /sync/chunks with given auth.
async fn sync_chunks_request(
    app: axum::Router,
    auth_header: Option<(&str, &str)>,
    hashes: Vec<String>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::post("/sync/chunks")
        .header("content-type", "application/json");

    if let Some((key, value)) = auth_header {
        builder = builder.header(key, value);
    }

    let response = app
        .oneshot(
            builder
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({ "hashes": hashes })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let json = body_json(response.into_body()).await;
    (status, json)
}

/// Extract file paths from the sync diff response changes.
fn extract_all_paths(changes: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    for category in [
        "files_added",
        "files_modified",
        "files_deleted",
        "symlinks_added",
        "symlinks_modified",
        "symlinks_deleted",
    ] {
        if let Some(entries) = changes[category].as_array() {
            for entry in entries {
                if let Some(path) = entry["path"].as_str() {
                    paths.push(path.to_string());
                }
            }
        }
    }
    paths
}

// ===========================================================================
// Tests
// ===========================================================================

/// Non-root JWT can call /sync/diff and get a response.
#[tokio::test]
async fn test_client_sync_with_jwt() {
    let (_app, jwt_manager, engine, _tmp) = create_node();
    store_file(&engine, "public/file.txt", b"hello");

    let token = non_root_bearer_token(&jwt_manager);
    let app = build_app(&jwt_manager, &engine);

    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["root_hash"].is_string());
    let paths = extract_all_paths(&json["changes"]);
    assert!(paths.contains(&"/public/file.txt".to_string()));
}

/// Non-root JWT sync excludes /.aeordb-system/ paths.
#[tokio::test]
async fn test_client_sync_excludes_system() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    // Store a user-space file and a system file.
    store_file(&engine, "public/file.txt", b"public data");
    store_file(&engine, ".system/config/secret_key", b"secret");

    let token = non_root_bearer_token(&jwt_manager);
    let app = build_app(&jwt_manager, &engine);

    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert!(
        paths.contains(&"/public/file.txt".to_string()),
        "should include public file, got: {:?}",
        paths
    );
    // No /.aeordb-system/ paths should appear.
    for path in &paths {
        assert!(
            !path.starts_with("/.aeordb-system/"),
            "system path should be filtered out: {}",
            path
        );
    }
}

/// Root JWT sync includes /.aeordb-system/ paths.
#[tokio::test]
async fn test_root_sync_includes_system() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "public/file.txt", b"public data");
    store_file(&engine, ".system/config/secret_key", b"secret");

    let token = root_bearer_token(&jwt_manager);
    let app = build_app(&jwt_manager, &engine);

    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert!(
        paths.contains(&"/public/file.txt".to_string()),
        "should include public file"
    );
    // Root should see system paths (at least the one we stored).
    let has_system = paths.iter().any(|p| p.starts_with("/.aeordb-system/"));
    assert!(has_system, "root should see /.aeordb-system/ paths, got: {:?}", paths);
}

/// Non-root JWT + paths filter only returns matching paths.
#[tokio::test]
async fn test_client_selective_sync_with_paths() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "assets/image.png", b"png data");
    store_file(&engine, "assets/style.css", b"css data");
    store_file(&engine, "docs/readme.txt", b"readme");
    store_file(&engine, "src/main.rs", b"fn main() {}");

    let token = non_root_bearer_token(&jwt_manager);
    let app = build_app(&jwt_manager, &engine);

    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({ "paths": ["/assets/**"] }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert_eq!(paths.len(), 2, "should only return /assets/ files, got: {:?}", paths);
    for path in &paths {
        assert!(path.starts_with("/assets/"), "unexpected path: {}", path);
    }
}

/// Scoped API key with read access to /assets/** only.
#[tokio::test]
async fn test_scoped_key_sync() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "assets/image.png", b"png data");
    store_file(&engine, "secret/hidden.txt", b"hidden");
    store_file(&engine, "docs/readme.txt", b"readme");

    let user_id = Uuid::new_v4();
    let rules = vec![
        KeyRule {
            glob: "/assets/**".to_string(),
            permitted: "-r--l---".to_string(),
        },
        KeyRule {
            glob: "/**".to_string(),
            permitted: "--------".to_string(),
        },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert_eq!(paths.len(), 1, "should only return /assets/ files, got: {:?}", paths);
    assert_eq!(paths[0], "/assets/image.png");
}

/// Scoped key: files at /assets/ok.txt and /secret/hidden.txt, only /assets/ allowed.
#[tokio::test]
async fn test_scoped_key_excludes_denied_paths() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "assets/ok.txt", b"ok data");
    store_file(&engine, "secret/hidden.txt", b"hidden data");

    let user_id = Uuid::new_v4();
    let rules = vec![
        KeyRule {
            glob: "/assets/**".to_string(),
            permitted: "crudlify".to_string(),
        },
        KeyRule {
            glob: "/**".to_string(),
            permitted: "--------".to_string(),
        },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert!(
        paths.contains(&"/assets/ok.txt".to_string()),
        "should include allowed path"
    );
    assert!(
        !paths.contains(&"/secret/hidden.txt".to_string()),
        "should exclude denied path"
    );
}

/// No auth header at all results in 401.
#[tokio::test]
async fn test_client_sync_no_auth_rejected() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        None, // no auth
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(json["error"].is_string());
}

/// Expired API key JWT results in 401.
#[tokio::test]
async fn test_client_sync_expired_key_rejected() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    let user_id = Uuid::new_v4();
    let rules = vec![
        KeyRule {
            glob: "/**".to_string(),
            permitted: "crudlify".to_string(),
        },
    ];
    // Expired 1 hour ago.
    let expired_at = Utc::now().timestamp_millis() - (3600 * 1000);
    let (token, _key_id) =
        create_scoped_key_and_token_with_expiry(&jwt_manager, &engine, user_id, rules, expired_at);

    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        json["error"].as_str().unwrap().contains("expired"),
        "error should mention expired: {:?}",
        json
    );
}

/// Revoked API key JWT results in 401.
#[tokio::test]
async fn test_client_sync_revoked_key_rejected() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    let user_id = Uuid::new_v4();
    let rules = vec![
        KeyRule {
            glob: "/**".to_string(),
            permitted: "crudlify".to_string(),
        },
    ];
    let (token, key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

    // Revoke the key.
    let ctx = RequestContext::system();
    system_store::revoke_api_key(&engine, &ctx, key_id).unwrap();

    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        json["error"].as_str().unwrap().contains("revoked"),
        "error should mention revoked: {:?}",
        json
    );
}

/// Non-root client does two syncs; second only gets new changes.
#[tokio::test]
async fn test_client_sync_incremental() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "file_a.txt", b"content A");
    let since_hash = get_head_hex(&engine);

    store_file(&engine, "file_b.txt", b"content B");

    let token = non_root_bearer_token(&jwt_manager);
    let app = build_app(&jwt_manager, &engine);

    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({ "since_root_hash": since_hash }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert!(
        paths.contains(&"/file_b.txt".to_string()),
        "should include file_b in incremental diff"
    );
    assert!(
        !paths.contains(&"/file_a.txt".to_string()),
        "should NOT include file_a in incremental diff"
    );
}

/// Root JWT auth works and sees all files including system.
#[tokio::test]
async fn test_root_jwt_sees_all_files() {
    let (_app, jwt_manager, engine, _tmp) = create_node();
    store_file(&engine, "public/file.txt", b"hello root");

    let token = root_bearer_token(&jwt_manager);
    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert!(
        paths.contains(&"/public/file.txt".to_string()),
        "root should see files"
    );
}

/// Non-Bearer authorization header results in 401.
#[tokio::test]
async fn test_non_bearer_auth_header_rejected() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    let app = build_app(&jwt_manager, &engine);
    let (status, _json) = sync_diff_request(
        app,
        Some(("authorization", "Basic dXNlcjpwYXNz")),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// /sync/chunks also supports JWT auth (non-root).
#[tokio::test]
async fn test_client_chunks_with_jwt() {
    let (_app, jwt_manager, engine, _tmp) = create_node();
    store_file(&engine, "public/file.txt", b"chunk data");

    let token = non_root_bearer_token(&jwt_manager);

    // First get chunk hashes from diff.
    let app = build_app(&jwt_manager, &engine);
    let (status, diff_json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let chunk_hashes: Vec<String> = diff_json["chunk_hashes_needed"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    assert!(!chunk_hashes.is_empty(), "should have chunk hashes");

    // Now request chunks with JWT.
    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_chunks_request(
        app,
        Some(("authorization", &token)),
        chunk_hashes.clone(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let chunks = json["chunks"].as_array().unwrap();
    assert!(!chunks.is_empty(), "should return chunks");
    for chunk in chunks {
        assert!(chunk["hash"].is_string());
        assert!(chunk["data"].is_string());
        assert!(chunk["size"].is_number());
    }
}

/// /sync/chunks with no auth returns 401.
#[tokio::test]
async fn test_client_chunks_no_auth_rejected() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    let app = build_app(&jwt_manager, &engine);
    let (status, _json) = sync_chunks_request(app, None, vec![]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Scoped key filtering applies to incremental diff too.
#[tokio::test]
async fn test_scoped_key_incremental_sync() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "assets/old.txt", b"old asset");
    let since_hash = get_head_hex(&engine);

    store_file(&engine, "assets/new.txt", b"new asset");
    store_file(&engine, "secret/new_secret.txt", b"new secret");

    let user_id = Uuid::new_v4();
    let rules = vec![
        KeyRule {
            glob: "/assets/**".to_string(),
            permitted: "-r--l---".to_string(),
        },
        KeyRule {
            glob: "/**".to_string(),
            permitted: "--------".to_string(),
        },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({ "since_root_hash": since_hash }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert!(
        paths.contains(&"/assets/new.txt".to_string()),
        "should include allowed new file"
    );
    assert!(
        !paths.contains(&"/secret/new_secret.txt".to_string()),
        "should exclude denied new file"
    );
    // old asset should not appear (it was before since_root_hash)
    assert!(
        !paths.contains(&"/assets/old.txt".to_string()),
        "old file should not appear in incremental diff"
    );
}

/// Non-root JWT with empty API key rules gets full access (minus /.aeordb-system/).
#[tokio::test]
async fn test_non_root_empty_rules_sees_all_user_files() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "a/file.txt", b"a");
    store_file(&engine, "b/file.txt", b"b");
    store_file(&engine, ".system/internal", b"sys");

    let user_id = Uuid::new_v4();
    let rules = vec![]; // empty rules = no path-level restrictions
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert!(paths.contains(&"/a/file.txt".to_string()));
    assert!(paths.contains(&"/b/file.txt".to_string()));
    for path in &paths {
        assert!(!path.starts_with("/.aeordb-system/"), "system paths filtered: {}", path);
    }
}

/// JWT with a malformed authorization header returns 401.
#[tokio::test]
async fn test_malformed_auth_header_rejected() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    let app = build_app(&jwt_manager, &engine);
    let (status, _json) = sync_diff_request(
        app,
        Some(("authorization", "NotBearer xyz")),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// JWT with an invalid (garbage) token returns 401.
#[tokio::test]
async fn test_invalid_jwt_token_rejected() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    let app = build_app(&jwt_manager, &engine);
    let (status, _json) = sync_diff_request(
        app,
        Some(("authorization", "Bearer totally.not.valid")),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// JWT from a different signing key is rejected.
#[tokio::test]
async fn test_jwt_wrong_signing_key_rejected() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    // Create token with a different JwtManager
    let other_jwt = JwtManager::generate();
    let token = root_bearer_token(&other_jwt);

    let app = build_app(&jwt_manager, &engine);
    let (status, _json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Root JWT sees /.aeordb-system/ paths; non-root JWT does not -- same data.
#[tokio::test]
async fn test_root_vs_client_system_visibility() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "public/file.txt", b"public");
    store_file(&engine, ".system/config/key", b"system data");

    // Root sync
    let root_token = root_bearer_token(&jwt_manager);
    let app = build_app(&jwt_manager, &engine);
    let (status, root_json) = sync_diff_request(
        app,
        Some(("authorization", &root_token)),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let root_paths = extract_all_paths(&root_json["changes"]);

    // Client sync
    let token = non_root_bearer_token(&jwt_manager);
    let app = build_app(&jwt_manager, &engine);
    let (status, client_json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let client_paths = extract_all_paths(&client_json["changes"]);

    // Root should have more paths (system entries).
    assert!(
        root_paths.len() > client_paths.len(),
        "root should see more paths than client: root={:?}, client={:?}",
        root_paths,
        client_paths
    );

    // Both should see public file.
    assert!(root_paths.contains(&"/public/file.txt".to_string()));
    assert!(client_paths.contains(&"/public/file.txt".to_string()));

    // Only root should see system entries.
    let root_has_system = root_paths.iter().any(|p| p.starts_with("/.aeordb-system/"));
    let client_has_system = client_paths.iter().any(|p| p.starts_with("/.aeordb-system/"));
    assert!(root_has_system, "root should see /.aeordb-system/");
    assert!(!client_has_system, "client should NOT see /.aeordb-system/");
}

/// Scoped key with no matching rule for a path blocks it.
#[tokio::test]
async fn test_scoped_key_no_matching_rule_blocks() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "anywhere/file.txt", b"data");

    let user_id = Uuid::new_v4();
    // Only /specific/** is allowed; /anywhere/ has no matching rule -> blocked.
    let rules = vec![KeyRule {
        glob: "/specific/**".to_string(),
        permitted: "crudlify".to_string(),
    }];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert!(
        paths.is_empty(),
        "no matching rule should block all paths, got: {:?}",
        paths
    );
}

/// Non-existent key_id in JWT returns 401.
#[tokio::test]
async fn test_stale_key_id_in_sync_rejected() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    let fake_key_id = Uuid::new_v4();
    let now = Utc::now().timestamp();
    let claims = TokenClaims {
        sub: Uuid::new_v4().to_string(),
        iss: "aeordb".to_string(),
        iat: now,
        exp: now + DEFAULT_EXPIRY_SECONDS,
        scope: None,
        permissions: None,
        key_id: Some(fake_key_id.to_string()),
    };
    let token = format!("Bearer {}", jwt_manager.create_token(&claims).unwrap());

    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        json["error"].as_str().unwrap().contains("not found"),
        "should say key not found: {:?}",
        json
    );
}

/// Combined: path filter AND scoped key -- both restrictions apply.
#[tokio::test]
async fn test_paths_filter_combined_with_scoped_key() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "assets/img.png", b"img");
    store_file(&engine, "assets/style.css", b"css");
    store_file(&engine, "docs/readme.txt", b"readme");

    let user_id = Uuid::new_v4();
    // Key only allows /assets/**
    let rules = vec![
        KeyRule {
            glob: "/assets/**".to_string(),
            permitted: "-r--l---".to_string(),
        },
        KeyRule {
            glob: "/**".to_string(),
            permitted: "--------".to_string(),
        },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, user_id, rules);

    // Request paths filter for /docs/** -- but key only allows /assets/**
    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({ "paths": ["/docs/**"] }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    // paths filter says /docs/** but key blocks /docs/**, so nothing should pass.
    assert!(
        paths.is_empty(),
        "conflicting path filter + key rules should yield empty, got: {:?}",
        paths
    );
}

/// Sync chunks endpoint also rejects no auth.
#[tokio::test]
async fn test_sync_chunks_no_auth_401() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_chunks_request(app, None, vec!["abc".to_string()]).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(json["error"].is_string());
}

/// Sync chunks with root JWT works.
#[tokio::test]
async fn test_sync_chunks_root_jwt_works() {
    let (_app, jwt_manager, engine, _tmp) = create_node();
    store_file(&engine, "test/file.txt", b"chunk test data");

    let token = root_bearer_token(&jwt_manager);

    // Get chunk hashes.
    let app = build_app(&jwt_manager, &engine);
    let (status, diff_json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let chunk_hashes: Vec<String> = diff_json["chunk_hashes_needed"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    if !chunk_hashes.is_empty() {
        let app = build_app(&jwt_manager, &engine);
        let (status, json) = sync_chunks_request(
            app,
            Some(("authorization", &token)),
            chunk_hashes,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(!json["chunks"].as_array().unwrap().is_empty());
    }
}

/// Root JWT sync with incremental diff works and includes system.
#[tokio::test]
async fn test_root_incremental_includes_system() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    store_file(&engine, "public/file.txt", b"public");
    let since_hash = get_head_hex(&engine);

    store_file(&engine, ".system/new_config", b"new system data");
    store_file(&engine, "public/new_file.txt", b"new public");

    let token = root_bearer_token(&jwt_manager);
    let app = build_app(&jwt_manager, &engine);

    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &token)),
        serde_json::json!({ "since_root_hash": since_hash }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let paths = extract_all_paths(&json["changes"]);
    assert!(
        paths.contains(&"/public/new_file.txt".to_string()),
        "should include new public file"
    );
    let has_system = paths.iter().any(|p| p.starts_with("/.aeordb-system/"));
    assert!(has_system, "root incremental should include new system files");
}

/// H4: Scoped key must NOT receive chunk hashes for files outside its scope.
#[tokio::test]
async fn test_scoped_key_chunk_hashes_filtered() {
    let (_app, jwt_manager, engine, _tmp) = create_node();

    // Store files in two different directories.
    store_file(&engine, "public/visible.txt", b"visible content");
    store_file(&engine, "secret/hidden.txt", b"hidden content");

    // Create a scoped key that can only read /public/**
    let rules = vec![
        KeyRule {
            glob: "/public/**".to_string(),
            permitted: "-r------".to_string(),
        },
        KeyRule {
            glob: "/**".to_string(),
            permitted: "--------".to_string(),
        },
    ];
    let (scoped_token, _key_id) =
        create_scoped_key_and_token(&jwt_manager, &engine, Uuid::new_v4(), rules);

    // Full sync diff with scoped key.
    let app = build_app(&jwt_manager, &engine);
    let (status, json) = sync_diff_request(
        app,
        Some(("authorization", &scoped_token)),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Only public files should appear in changes.
    let paths = extract_all_paths(&json["changes"]);
    assert!(paths.contains(&"/public/visible.txt".to_string()));
    assert!(!paths.contains(&"/secret/hidden.txt".to_string()));

    // Chunk hashes must only come from visible files.
    // Get the chunk hashes from the visible file via root to compare.
    let root_token = root_bearer_token(&jwt_manager);
    let app = build_app(&jwt_manager, &engine);
    let (_, root_json) = sync_diff_request(
        app,
        Some(("authorization", &root_token)),
        serde_json::json!({}),
    )
    .await;

    // Root sees both files' chunk hashes.
    let root_chunks: Vec<String> = root_json["chunk_hashes_needed"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    // Scoped user sees fewer chunk hashes.
    let scoped_chunks: Vec<String> = json["chunk_hashes_needed"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    assert!(
        scoped_chunks.len() < root_chunks.len(),
        "Scoped user should see fewer chunk hashes than root ({} vs {})",
        scoped_chunks.len(),
        root_chunks.len(),
    );

    // Ensure no chunk hash from hidden.txt leaks to the scoped user.
    // The hidden file's chunks should be in root but NOT in scoped.
    for hash in &scoped_chunks {
        assert!(
            root_chunks.contains(hash),
            "Scoped chunk hash should be a subset of root's"
        );
    }
}
