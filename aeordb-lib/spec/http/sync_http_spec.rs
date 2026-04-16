use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::JwtManager;
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::auth::FileAuthProvider;
use aeordb::engine::{
    DirectoryOps, EventBus, RequestContext, StorageEngine, VersionManager,
};
use aeordb::engine::system_store;
use aeordb::plugins::PluginManager;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests, CorsState};

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

const TEST_SECRET: &str = "my-super-secret-cluster-key";

fn setup_cluster_secret(engine: &StorageEngine, secret: &str) {
    let hash = blake3::hash(secret.as_bytes());
    let ctx = RequestContext::system();
    system_store::store_cluster_secret_hash(engine, &ctx, hash.as_bytes())
        .unwrap();
}

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    setup_cluster_secret(&engine, TEST_SECRET);
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
    ops.store_file(&ctx, path, data, Some("application/octet-stream"))
        .expect("store file");
}

// ===========================================================================
// POST /sync/diff — full sync (no since_root_hash)
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_full() {
    let (app, _jwt, engine, _tmp) = test_app();
    store_test_file(&engine, "/hello.txt", b"hello world");
    store_test_file(&engine, "/subdir/nested.txt", b"nested content");

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
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
    assert_eq!(added.len(), 2);
    // Sorted by path
    assert_eq!(added[0]["path"], "/hello.txt");
    assert_eq!(added[1]["path"], "/subdir/nested.txt");

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
    let (app, _jwt, engine, _tmp) = test_app();

    // Store initial files
    store_test_file(&engine, "/file_a.txt", b"content A");
    let vm = VersionManager::new(&engine);

    // Capture the head hash as our "since" point
    let since_hash = hex::encode(vm.get_head_hash().unwrap());

    // Store more files (these should appear as added in the diff)
    store_test_file(&engine, "/file_b.txt", b"content B");

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
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
    let (app, _jwt, engine, _tmp) = test_app();

    store_test_file(&engine, "/docs/readme.txt", b"readme");
    store_test_file(&engine, "/src/main.rs", b"fn main() {}");
    store_test_file(&engine, "/src/lib.rs", b"pub fn lib() {}");

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
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
// POST /sync/diff — missing secret → 401
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_no_secret() {
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
// POST /sync/diff — wrong secret → 401
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_wrong_secret() {
    let (app, _jwt, _engine, _tmp) = test_app();

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", "wrong-secret-value")
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
    let (app, _jwt, _engine, _tmp) = test_app();

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
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
    let (app, _jwt, _engine, _tmp) = test_app();

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;

    assert!(json["changes"]["files_added"].as_array().unwrap().is_empty());
    assert!(json["changes"]["files_modified"].as_array().unwrap().is_empty());
    assert!(json["changes"]["files_deleted"].as_array().unwrap().is_empty());
    assert!(json["chunk_hashes_needed"].as_array().unwrap().is_empty());
}

// ===========================================================================
// POST /sync/chunks — returns chunk data
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_returns_data() {
    let (app, _jwt, engine, _tmp) = test_app();

    // Store a file to get real chunk hashes
    store_test_file(&engine, "/data.bin", b"some binary content here");

    // Get the chunk hashes from a diff
    let diff_response = app
        .clone()
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
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
                .header("X-Cluster-Secret", TEST_SECRET)
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
    let (app, _jwt, _engine, _tmp) = test_app();

    let fake_hash = hex::encode(blake3::hash(b"nonexistent").as_bytes());

    let response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
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
// POST /sync/chunks — missing secret → 401
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_no_secret() {
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
// POST /sync/chunks — wrong secret → 401
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_wrong_secret() {
    let (app, _jwt, _engine, _tmp) = test_app();

    let response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", "bad-secret")
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
    let (app, _jwt, _engine, _tmp) = test_app();

    let response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
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
    let (app, _jwt, _engine, _tmp) = test_app();

    let response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
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
// POST /sync/diff — no cluster secret configured → 401 for any request
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_no_secret_configured() {
    // Build an app WITHOUT setting up a cluster secret
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, _tmp) = create_temp_engine_for_tests();
    // deliberately NOT calling setup_cluster_secret
    let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
    let rate_limiter = Arc::new(RateLimiter::default_config());
    let auth_provider: Arc<dyn aeordb::auth::AuthProvider> =
        Arc::new(FileAuthProvider::new(engine.clone()));
    let app = create_app_with_all(
        auth_provider,
        jwt_manager,
        plugin_manager,
        rate_limiter,
        make_prometheus_handle(),
        engine,
        Arc::new(EventBus::new()),
        CorsState {
            default_origins: None,
            rules: vec![],
        },
    );

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", "any-secret")
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
    let (app, _jwt, engine, _tmp) = test_app();

    // Store initial files
    store_test_file(&engine, "/keep.txt", b"keep me");
    store_test_file(&engine, "/remove.txt", b"remove me");

    let vm = VersionManager::new(&engine);
    let since_hash = hex::encode(vm.get_head_hash().unwrap());

    // Delete one file
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    ops.delete_file(&ctx, "/remove.txt").unwrap();

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
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
    let (app, _jwt, engine, _tmp) = test_app();

    // Store initial file
    store_test_file(&engine, "/mutable.txt", b"version 1");

    let vm = VersionManager::new(&engine);
    let since_hash = hex::encode(vm.get_head_hash().unwrap());

    // Modify the file
    store_test_file(&engine, "/mutable.txt", b"version 2 with different content");

    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
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
// Sync routes bypass JWT auth (no Authorization header needed)
// ===========================================================================

#[tokio::test]
async fn test_sync_routes_bypass_jwt() {
    let (app, _jwt, engine, _tmp) = test_app();
    store_test_file(&engine, "/test.txt", b"data");

    // Request with cluster secret but NO Authorization header
    let response = app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("X-Cluster-Secret", TEST_SECRET)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    // Should succeed — JWT is not required for sync routes
    assert_eq!(response.status(), StatusCode::OK);
}
