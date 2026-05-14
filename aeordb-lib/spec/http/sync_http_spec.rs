use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::auth::FileAuthProvider;
use aeordb::engine::{
    DirectoryOps, EventBus, RequestContext, StorageEngine, VersionManager,
};
use aeordb::plugins::PluginManager;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests, CorsState};

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
    let rate_limiter = Arc::new(RateLimiter::default_config());
    let auth_provider: Arc<dyn aeordb::auth::AuthProvider> =
        Arc::new(FileAuthProvider::new(engine.clone()));
    let app = create_app_with_all(
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
    );
    (app, jwt_manager, engine, temp_dir)
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

fn store_test_file(engine: &StorageEngine, path: &str, data: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file_buffered(&ctx, path, data, Some("application/octet-stream"))
        .expect("store file");
}

/// Create a root-user Bearer token (nil UUID).
fn bearer_token(jwt_manager: &JwtManager) -> String {
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

// ===========================================================================
// POST /sync/diff — full sync (no since_root_hash)
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_full() {
    let (app, jwt, engine, _tmp) = test_app();
    store_test_file(&engine, "/hello.txt", b"hello world");
    store_test_file(&engine, "/subdir/nested.txt", b"nested content");

    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;

    // Should have a root_hash
    assert!(json["root_hash"].is_string());
    assert!(!json["root_hash"].as_str().unwrap().is_empty());

    // All files as "added"
    let added = json["changes"]["files_added"].as_array().unwrap();
    let user_added: Vec<_> = added.iter()
        .filter(|e| !e["path"].as_str().unwrap_or("").starts_with("/.aeordb-system"))
        .collect();
    assert_eq!(user_added.len(), 2);
    // Sorted by path
    assert_eq!(user_added[0]["path"], "/hello.txt");
    assert_eq!(user_added[1]["path"], "/subdir/nested.txt");

    // Each file has hash, size, chunk_hashes
    assert!(added[0]["hash"].is_string());
    assert!(added[0]["size"].is_number());
    assert!(added[0]["chunk_hashes"].is_array());

    // No modified or deleted
    assert!(json["changes"]["files_modified"].as_array().unwrap().is_empty());
    assert!(json["changes"]["files_deleted"].as_array().unwrap().is_empty());

    // chunk_hashes_needed present and non-empty
    let chunk_hashes = json["chunk_hashes_needed"].as_array().unwrap();
    assert!(!chunk_hashes.is_empty());
}

// ===========================================================================
// POST /sync/diff — incremental (since_root_hash provided)
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_incremental() {
    let (app, jwt, engine, _tmp) = test_app();

    // Store initial files
    store_test_file(&engine, "/file_a.txt", b"content A");
    let vm = VersionManager::new(&engine);

    // Capture the head hash as our "since" point
    let since_hash = hex::encode(vm.get_head_hash().unwrap());

    // Store more files (these should appear as added in the diff)
    store_test_file(&engine, "/file_b.txt", b"content B");

    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "since_root_hash": since_hash
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;

    let added = json["changes"]["files_added"].as_array().unwrap();
    // Only file_b should be added
    assert_eq!(added.len(), 1);
    assert_eq!(added[0]["path"], "/file_b.txt");

    // file_a should not be in added
    let added_paths: Vec<&str> = added.iter().map(|v| v["path"].as_str().unwrap()).collect();
    assert!(!added_paths.contains(&"/file_a.txt"));
}

// ===========================================================================
// POST /sync/diff — with path filter
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_with_path_filter() {
    let (app, jwt, engine, _tmp) = test_app();

    store_test_file(&engine, "/docs/readme.txt", b"readme");
    store_test_file(&engine, "/src/main.rs", b"fn main() {}");
    store_test_file(&engine, "/src/lib.rs", b"pub fn lib() {}");

    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "paths": ["/src/*"]
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;

    let added = json["changes"]["files_added"].as_array().unwrap();
    // Only /src/* files
    assert_eq!(added.len(), 2);
    for entry in added {
        assert!(entry["path"].as_str().unwrap().starts_with("/src/"));
    }
}

// ===========================================================================
// POST /sync/diff — missing auth → 401
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_no_auth() {
    let (app, _jwt, _engine, _tmp) = test_app();

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("Authentication required"));
}

// ===========================================================================
// POST /sync/diff — invalid JWT → 401
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_invalid_jwt() {
    let (app, _jwt, _engine, _tmp) = test_app();

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", "Bearer totally.not.valid")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// POST /sync/diff — invalid since_root_hash hex → 400
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_invalid_since_hash() {
    let (app, jwt, _engine, _tmp) = test_app();

    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "since_root_hash": "ZZZZ_not_hex"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// POST /sync/diff — empty database → empty changes
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_empty_database() {
    let (app, jwt, _engine, _tmp) = test_app();

    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;

    // Filter out /.aeordb-system/ entries
    let added: Vec<_> = json["changes"]["files_added"].as_array().unwrap()
        .iter().filter(|e| !e["path"].as_str().unwrap_or("").starts_with("/.aeordb-system")).collect();
    assert!(added.is_empty(), "No user files should be added on empty db");
    assert!(json["changes"]["files_modified"].as_array().unwrap().is_empty());
    assert!(json["changes"]["files_deleted"].as_array().unwrap().is_empty());
}

// ===========================================================================
// POST /sync/chunks — returns chunk data
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_returns_data() {
    let (app, jwt, engine, _tmp) = test_app();

    // Store a file to get real chunk hashes
    store_test_file(&engine, "/data.bin", b"some binary content here");

    let auth = bearer_token(&jwt);

    // Get the chunk hashes from a diff
    let diff_response = app
        .clone()
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(diff_response.status(), StatusCode::OK);
    let diff_json = body_json(diff_response.into_body()).await;
    let chunk_hashes: Vec<String> = diff_json["chunk_hashes_needed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(!chunk_hashes.is_empty(), "should have chunk hashes");

    // Now request the chunks
    let chunks_response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "hashes": chunk_hashes
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(chunks_response.status(), StatusCode::OK);
    let chunks_json = body_json(chunks_response.into_body()).await;
    let chunks = chunks_json["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), chunk_hashes.len());

    // Each chunk should have hash, data (base64), and size
    for chunk in chunks {
        assert!(chunk["hash"].is_string());
        assert!(chunk["data"].is_string());
        assert!(chunk["size"].is_number());
        assert!(chunk["size"].as_u64().unwrap() > 0);
    }
}

// ===========================================================================
// POST /sync/chunks — nonexistent hash → empty result
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_missing_hash() {
    let (app, jwt, _engine, _tmp) = test_app();

    let fake_hash = hex::encode(blake3::hash(b"nonexistent").as_bytes());

    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "hashes": [fake_hash]
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let chunks = json["chunks"].as_array().unwrap();
    assert!(chunks.is_empty(), "nonexistent hash should be skipped");
}

// ===========================================================================
// POST /sync/chunks — missing auth → 401
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_no_auth() {
    let (app, _jwt, _engine, _tmp) = test_app();

    let response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "hashes": []
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// POST /sync/chunks — invalid JWT → 401
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_invalid_jwt() {
    let (app, _jwt, _engine, _tmp) = test_app();

    let response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("authorization", "Bearer bad-token-value")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "hashes": []
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// POST /sync/chunks — invalid hex hash is skipped, valid ones returned
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_invalid_hex_skipped() {
    let (app, jwt, _engine, _tmp) = test_app();

    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "hashes": ["not-valid-hex!!!", "also_bad"]
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    assert!(json["chunks"].as_array().unwrap().is_empty());
}

// ===========================================================================
// POST /sync/chunks — empty hashes list → empty chunks
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_empty_hashes() {
    let (app, jwt, _engine, _tmp) = test_app();

    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "hashes": []
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    assert!(json["chunks"].as_array().unwrap().is_empty());
}

// ===========================================================================
// POST /sync/diff — JWT from wrong signing key → 401
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_wrong_signing_key() {
    let (app, _jwt, _engine, _tmp) = test_app();

    // Create token with a different JwtManager
    let other_jwt = JwtManager::generate();
    let bad_auth = bearer_token(&other_jwt);

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &bad_auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// POST /sync/diff — incremental with file deletion
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_incremental_with_deletion() {
    let (app, jwt, engine, _tmp) = test_app();

    // Store initial files
    store_test_file(&engine, "/keep.txt", b"keep me");
    store_test_file(&engine, "/remove.txt", b"remove me");

    let vm = VersionManager::new(&engine);
    let since_hash = hex::encode(vm.get_head_hash().unwrap());

    // Delete one file
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.delete_file(&ctx, "/remove.txt").unwrap();

    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "since_root_hash": since_hash
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;

    let deleted = json["changes"]["files_deleted"].as_array().unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0]["path"], "/remove.txt");
}

// ===========================================================================
// POST /sync/diff — incremental with file modification
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_incremental_with_modification() {
    let (app, jwt, engine, _tmp) = test_app();

    // Store initial file
    store_test_file(&engine, "/mutable.txt", b"version 1");

    let vm = VersionManager::new(&engine);
    let since_hash = hex::encode(vm.get_head_hash().unwrap());

    // Modify the file
    store_test_file(&engine, "/mutable.txt", b"version 2 with different content");

    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "since_root_hash": since_hash
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;

    let modified = json["changes"]["files_modified"].as_array().unwrap();
    assert_eq!(modified.len(), 1);
    assert_eq!(modified[0]["path"], "/mutable.txt");

    // chunk_hashes_needed should contain the new chunks
    assert!(!json["chunk_hashes_needed"].as_array().unwrap().is_empty());
}

// ===========================================================================
// Sync routes are public routes — JWT is checked inside the handler
// ===========================================================================

#[tokio::test]
async fn test_sync_routes_are_public_with_jwt_check() {
    let (app, jwt, engine, _tmp) = test_app();
    store_test_file(&engine, "/test.txt", b"data");

    // Request with valid JWT (no middleware auth needed — handler verifies JWT)
    let auth = bearer_token(&jwt);
    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}
