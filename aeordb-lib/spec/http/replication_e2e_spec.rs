//! End-to-end replication tests.
//!
//! These tests simulate the HTTP sync protocol between two in-process nodes
//! using the tower `oneshot` pattern (no real TCP sockets). Node B calls
//! Node A's `/sync/diff` and `/sync/chunks` endpoints, then applies the
//! returned changes through its own engine -- exactly the same flow as
//! `do_sync_cycle_remote` but exercised through the full HTTP layer.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use chrono::Utc;
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::auth::FileAuthProvider;
use aeordb::engine::compression::{decompress, CompressionAlgorithm};
use aeordb::engine::tree_walker::walk_version_tree;
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

/// Create a full app + engine pair (a "node") with a JwtManager for auth.
fn create_node() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
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

/// Rebuild the router from an existing engine and JwtManager (needed for
/// multi-request tests because `oneshot` consumes the router).
fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
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

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

fn store_file(engine: &StorageEngine, path: &str, data: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file_buffered(&ctx, path, data, Some("application/octet-stream"))
        .expect("store file");
}

fn store_file_typed(engine: &StorageEngine, path: &str, data: &[u8], content_type: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file_buffered(&ctx, path, data, Some(content_type))
        .expect("store file");
}

fn read_file(engine: &StorageEngine, path: &str) -> Vec<u8> {
    let ops = DirectoryOps::new(engine);
    ops.read_file_buffered(path).unwrap()
}

fn file_exists(engine: &StorageEngine, path: &str) -> bool {
    let ops = DirectoryOps::new(engine);
    ops.read_file_buffered(path).is_ok()
}

fn store_symlink(engine: &StorageEngine, path: &str, target: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_symlink(&ctx, path, target).expect("store symlink");
}

fn delete_file(engine: &StorageEngine, path: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.delete_file(&ctx, path).expect("delete file");
}

fn get_head_hex(engine: &StorageEngine) -> String {
    let vm = VersionManager::new(engine);
    hex::encode(vm.get_head_hash().unwrap())
}

/// Simulate the sync protocol: Node B pulls from Node A's HTTP endpoints
/// and applies changes locally. Returns the number of operations applied.
///
/// This mirrors the logic in `SyncEngine::do_sync_cycle_remote` but drives
/// it through the actual HTTP layer.
async fn sync_pull(
    source_jwt: &Arc<JwtManager>,
    source_engine: &Arc<StorageEngine>,
    target_engine: &Arc<StorageEngine>,
    since_root_hash: Option<&str>,
) -> (usize, serde_json::Value) {
    // Step 1: POST /sync/diff on the source
    let source_app = rebuild_app(source_jwt, source_engine);
    let auth = bearer_token(source_jwt);

    let mut diff_body = serde_json::json!({});
    if let Some(since) = since_root_hash {
        diff_body["since_root_hash"] = serde_json::json!(since);
    }

    let diff_response = source_app
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(serde_json::to_string(&diff_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        diff_response.status(),
        StatusCode::OK,
        "sync/diff should succeed"
    );
    let diff_json = body_json(diff_response.into_body()).await;

    // Step 2: Fetch chunks
    let chunk_hashes: Vec<String> = diff_json["chunk_hashes_needed"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if !chunk_hashes.is_empty() {
        let source_app = rebuild_app(source_jwt, source_engine);
        let chunks_response = source_app
            .oneshot(
                Request::post("/sync/chunks")
                    .header("content-type", "application/json")
                    .header("authorization", &auth)
                    .body(Body::from(
                        serde_json::to_string(&serde_json::json!({ "hashes": chunk_hashes }))
                            .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            chunks_response.status(),
            StatusCode::OK,
            "sync/chunks should succeed"
        );
        let chunks_json = body_json(chunks_response.into_body()).await;

        // Store chunks in target engine
        if let Some(chunks) = chunks_json["chunks"].as_array() {
            for chunk in chunks {
                let hash_hex = chunk["hash"].as_str().unwrap_or("");
                let data_b64 = chunk["data"].as_str().unwrap_or("");
                if let (Ok(hash), Ok(data)) = (
                    hex::decode(hash_hex),
                    base64::engine::general_purpose::STANDARD.decode(data_b64),
                ) {
                    if !target_engine.has_entry(&hash).unwrap_or(false) {
                        let _ = target_engine.store_entry(
                            aeordb::engine::EntryType::Chunk,
                            &hash,
                            &data,
                        );
                    }
                }
            }
        }
    }

    // Step 3: Apply changes to target engine
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(target_engine);
    let changes = &diff_json["changes"];
    let mut ops_count: usize = 0;

    // File additions and modifications
    for category in ["files_added", "files_modified"] {
        if let Some(entries) = changes[category].as_array() {
            for entry in entries {
                let path = entry["path"].as_str().unwrap_or("");
                if path.is_empty() {
                    continue;
                }

                let entry_chunk_hashes: Vec<Vec<u8>> = entry["chunk_hashes"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|h| h.as_str().and_then(|s| hex::decode(s).ok()))
                            .collect()
                    })
                    .unwrap_or_default();

                let mut file_data = Vec::new();
                let mut ok = true;
                for ch in &entry_chunk_hashes {
                    match target_engine.get_entry(ch) {
                        Ok(Some((header, _key, value))) => {
                            let data = if header.compression_algo != CompressionAlgorithm::None {
                                decompress(&value, header.compression_algo).unwrap_or(value)
                            } else {
                                value
                            };
                            file_data.extend_from_slice(&data);
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }

                if ok {
                    let ct = entry["content_type"].as_str();
                    ops.store_file_buffered(&ctx, path, &file_data, ct).unwrap();
                    ops_count += 1;
                }
            }
        }
    }

    // File deletions
    if let Some(deleted) = changes["files_deleted"].as_array() {
        for entry in deleted {
            let path = entry["path"].as_str().unwrap_or("");
            if !path.is_empty() {
                let _ = ops.delete_file(&ctx, path);
                ops_count += 1;
            }
        }
    }

    // Symlink additions and modifications
    for category in ["symlinks_added", "symlinks_modified"] {
        if let Some(entries) = changes[category].as_array() {
            for entry in entries {
                let path = entry["path"].as_str().unwrap_or("");
                let target_path = entry["target"].as_str().unwrap_or("");
                if !path.is_empty() && !target_path.is_empty() {
                    ops.store_symlink(&ctx, path, target_path).unwrap();
                    ops_count += 1;
                }
            }
        }
    }

    // Symlink deletions
    if let Some(deleted) = changes["symlinks_deleted"].as_array() {
        for entry in deleted {
            let path = entry["path"].as_str().unwrap_or("");
            if !path.is_empty() {
                let _ = ops.delete_symlink(&ctx, path);
                ops_count += 1;
            }
        }
    }

    (ops_count, diff_json)
}

// ===========================================================================
// Test 1: Full file transfer — store files on A, sync to B, verify
// ===========================================================================

#[tokio::test]
async fn test_full_sync_file_transfer() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    // Store files on Node A
    store_file(&engine_a, "/hello.txt", b"hello world");
    store_file(&engine_a, "/data.bin", b"\x00\x01\x02\x03\x04\x05");
    store_file_typed(&engine_a, "/page.html", b"<h1>Test</h1>", "text/html");

    // Sync: B pulls from A (full sync, no since_root_hash)
    let (ops, _) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    assert!(ops >= 3, "should apply at least 3 file operations, got {}", ops);

    // Verify Node B has all the files
    assert_eq!(read_file(&engine_b, "/hello.txt"), b"hello world");
    assert_eq!(read_file(&engine_b, "/data.bin"), b"\x00\x01\x02\x03\x04\x05");
    assert_eq!(read_file(&engine_b, "/page.html"), b"<h1>Test</h1>");
}

// ===========================================================================
// Test 2: Sync with deletions
// ===========================================================================

#[tokio::test]
async fn test_sync_with_deletes() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    // Store files on A, sync to B
    store_file(&engine_a, "/keep.txt", b"keep me");
    store_file(&engine_a, "/remove.txt", b"remove me");

    let (_, diff_json) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    let since_hash = diff_json["root_hash"].as_str().unwrap().to_string();

    assert!(file_exists(&engine_b, "/keep.txt"));
    assert!(file_exists(&engine_b, "/remove.txt"));

    // Delete a file on A
    delete_file(&engine_a, "/remove.txt");

    // Incremental sync: B pulls from A with since_root_hash
    let (ops, _) = sync_pull(&jwt_a, &engine_a, &engine_b, Some(&since_hash)).await;
    assert!(ops >= 1, "should apply at least 1 deletion");

    // Verify
    assert!(file_exists(&engine_b, "/keep.txt"));
    assert!(!file_exists(&engine_b, "/remove.txt"), "deleted file should be gone on B");
}

// ===========================================================================
// Test 3: Sync symlinks
// ===========================================================================

#[tokio::test]
async fn test_sync_symlinks() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    // Store a file and a symlink on A
    store_file(&engine_a, "/original.txt", b"original content");
    store_symlink(&engine_a, "/link.txt", "/original.txt");

    // Sync to B
    let (ops, _) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    assert!(ops >= 2, "should sync file + symlink");

    // Verify the file synced
    assert_eq!(read_file(&engine_b, "/original.txt"), b"original content");

    // Verify the symlink synced (check via tree walker)
    let head = engine_b.head_hash().unwrap();
    let tree = walk_version_tree(&engine_b, &head).unwrap();
    assert!(
        tree.symlinks.contains_key("/link.txt"),
        "symlink should exist on B"
    );
    assert_eq!(
        tree.symlinks["/link.txt"].1.target,
        "/original.txt",
        "symlink target should match"
    );
}

// ===========================================================================
// Test 4: Nested directory tree syncs correctly
// ===========================================================================

#[tokio::test]
async fn test_sync_nested_directories() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    // Create a deep directory tree on A
    store_file(&engine_a, "/a/b/c/deep.txt", b"deep content");
    store_file(&engine_a, "/a/b/sibling.txt", b"sibling");
    store_file(&engine_a, "/a/top.txt", b"top level");
    store_file(&engine_a, "/root.txt", b"root file");
    store_file(&engine_a, "/x/y/z/w/very-deep.bin", b"\xDE\xAD\xBE\xEF");

    // Sync to B
    let (ops, _) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    assert!(ops >= 5, "should sync all 5 files, got {}", ops);

    // Verify all files on B
    assert_eq!(read_file(&engine_b, "/a/b/c/deep.txt"), b"deep content");
    assert_eq!(read_file(&engine_b, "/a/b/sibling.txt"), b"sibling");
    assert_eq!(read_file(&engine_b, "/a/top.txt"), b"top level");
    assert_eq!(read_file(&engine_b, "/root.txt"), b"root file");
    assert_eq!(
        read_file(&engine_b, "/x/y/z/w/very-deep.bin"),
        b"\xDE\xAD\xBE\xEF"
    );
}

// ===========================================================================
// Test 5: Incremental sync — first sync transfers everything, second only changes
// ===========================================================================

#[tokio::test]
async fn test_sync_incremental() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    // Store initial files on A
    store_file(&engine_a, "/file1.txt", b"content 1");
    store_file(&engine_a, "/file2.txt", b"content 2");

    // Full sync
    let (ops_full, diff_json) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    let since_hash = diff_json["root_hash"].as_str().unwrap().to_string();
    assert!(ops_full >= 2, "full sync should transfer at least 2 files");

    // Add a new file on A
    store_file(&engine_a, "/file3.txt", b"content 3");

    // Incremental sync
    let (ops_incr, _) = sync_pull(&jwt_a, &engine_a, &engine_b, Some(&since_hash)).await;
    assert_eq!(ops_incr, 1, "incremental sync should only transfer 1 new file");

    // Verify all files exist on B
    assert_eq!(read_file(&engine_b, "/file1.txt"), b"content 1");
    assert_eq!(read_file(&engine_b, "/file2.txt"), b"content 2");
    assert_eq!(read_file(&engine_b, "/file3.txt"), b"content 3");
}

// ===========================================================================
// Test 6: Bidirectional sync — changes on both sides
// ===========================================================================

#[tokio::test]
async fn test_sync_bidirectional() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, jwt_b, engine_b, _tmp_b) = create_node();

    // Store a file on A and sync to B
    store_file(&engine_a, "/shared.txt", b"initial");
    let (_, diff_json) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    let since_a = diff_json["root_hash"].as_str().unwrap().to_string();

    // Capture B's HEAD before creating new files (this is the common ancestor)
    let since_b = get_head_hex(&engine_b);

    // Now store different files on each side
    store_file(&engine_a, "/from_a.txt", b"created on A");
    store_file(&engine_b, "/from_b.txt", b"created on B");

    // B pulls from A (gets /from_a.txt)
    let (ops, _) = sync_pull(&jwt_a, &engine_a, &engine_b, Some(&since_a)).await;
    assert!(ops >= 1, "B should get at least /from_a.txt");
    assert_eq!(read_file(&engine_b, "/from_a.txt"), b"created on A");

    // A pulls from B (gets /from_b.txt)
    let (ops, _) = sync_pull(&jwt_b, &engine_b, &engine_a, Some(&since_b)).await;
    assert!(ops >= 1, "A should get at least /from_b.txt");
    assert_eq!(read_file(&engine_a, "/from_b.txt"), b"created on B");

    // Both should now have all files
    assert!(file_exists(&engine_a, "/shared.txt"));
    assert!(file_exists(&engine_a, "/from_a.txt"));
    assert!(file_exists(&engine_a, "/from_b.txt"));
    assert!(file_exists(&engine_b, "/shared.txt"));
    assert!(file_exists(&engine_b, "/from_a.txt"));
    assert!(file_exists(&engine_b, "/from_b.txt"));
}

// ===========================================================================
// Test 7: Empty source — sync from empty database
// ===========================================================================

#[tokio::test]
async fn test_sync_from_empty_source() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    // B already has files
    store_file(&engine_b, "/local.txt", b"local content");

    // Sync from empty A — should produce 0 operations
    let (ops, _) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;

    // The diff from empty A will report 0 user files (only system paths
    // which we handle, but user-visible ops should be 0 or only system).
    // The local file on B should remain intact.
    assert_eq!(read_file(&engine_b, "/local.txt"), b"local content");
    // ops may include system files; the point is no crash and local data intact.
    let _ = ops;
}

// ===========================================================================
// Test 8: Large file sync (multi-chunk)
// ===========================================================================

#[tokio::test]
async fn test_sync_large_file() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    // Create a file larger than the default chunk size (256KB)
    let large_data: Vec<u8> = (0..300_000).map(|i| (i % 256) as u8).collect();
    store_file(&engine_a, "/large.bin", &large_data);

    // Sync to B
    let (ops, _) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    assert!(ops >= 1, "should sync the large file");

    // Verify content integrity
    let synced = read_file(&engine_b, "/large.bin");
    assert_eq!(synced.len(), large_data.len(), "synced file size should match");
    assert_eq!(synced, large_data, "synced file content should match exactly");
}

// ===========================================================================
// Test 9: File modification syncs correctly
// ===========================================================================

#[tokio::test]
async fn test_sync_file_modification() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    // Store v1 on A and sync
    store_file(&engine_a, "/mutable.txt", b"version 1");
    let (_, diff_json) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    let since = diff_json["root_hash"].as_str().unwrap().to_string();
    assert_eq!(read_file(&engine_b, "/mutable.txt"), b"version 1");

    // Modify file on A
    store_file(&engine_a, "/mutable.txt", b"version 2 with more content");

    // Incremental sync
    let (ops, _) = sync_pull(&jwt_a, &engine_a, &engine_b, Some(&since)).await;
    assert!(ops >= 1, "should sync the modification");
    assert_eq!(
        read_file(&engine_b, "/mutable.txt"),
        b"version 2 with more content"
    );
}

// ===========================================================================
// Test 10: Sync diff endpoint rejects invalid JWT
// ===========================================================================

#[tokio::test]
async fn test_sync_rejects_invalid_jwt() {
    let (app_a, _jwt_a, engine_a, _tmp_a) = create_node();
    store_file(&engine_a, "/secret-data.txt", b"classified");

    let response = app_a
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", "Bearer invalid-token-value")
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
// Test 11: Sync diff endpoint rejects missing auth
// ===========================================================================

#[tokio::test]
async fn test_sync_rejects_missing_auth() {
    let (app_a, _jwt_a, _engine_a, _tmp_a) = create_node();

    let response = app_a
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
}

// ===========================================================================
// Test 12: Chunks endpoint returns correct base64 data
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_data_integrity() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();

    let test_data = b"chunk integrity test data 1234567890";
    store_file(&engine_a, "/integrity.txt", test_data);

    let auth = bearer_token(&jwt_a);

    // Get diff to discover chunk hashes
    let app = rebuild_app(&jwt_a, &engine_a);
    let diff_response = app
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

    let diff_json = body_json(diff_response.into_body()).await;

    // Find the specific file entry and get its chunk hashes (not the global
    // chunk_hashes_needed which may include system file chunks)
    let added = diff_json["changes"]["files_added"].as_array().unwrap();
    let integrity_file = added
        .iter()
        .find(|e| e["path"].as_str() == Some("/integrity.txt"))
        .expect("should find /integrity.txt in added files");

    let file_chunk_hashes: Vec<String> = integrity_file["chunk_hashes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    assert!(!file_chunk_hashes.is_empty(), "file should have chunk hashes");

    // Fetch chunks
    let app = rebuild_app(&jwt_a, &engine_a);
    let chunks_response = app
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({ "hashes": file_chunk_hashes }))
                        .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let chunks_json = body_json(chunks_response.into_body()).await;
    let chunks = chunks_json["chunks"].as_array().unwrap();
    assert_eq!(
        chunks.len(),
        file_chunk_hashes.len(),
        "should get all requested chunks"
    );

    // Reassemble file from chunks in the order specified by the file record
    let mut reassembled = Vec::new();
    for hash_hex in &file_chunk_hashes {
        let chunk = chunks
            .iter()
            .find(|c| c["hash"].as_str() == Some(hash_hex))
            .expect("chunk should be in response");
        let data_b64 = chunk["data"].as_str().unwrap();
        let data = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .unwrap();
        reassembled.extend_from_slice(&data);
    }

    assert_eq!(
        reassembled, test_data,
        "reassembled data should match original"
    );
}

// ===========================================================================
// Test 13: Sync with invalid since_root_hash returns error
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_invalid_since_hash() {
    let (app_a, jwt_a, _engine_a, _tmp_a) = create_node();

    let auth = bearer_token(&jwt_a);
    let response = app_a
        .oneshot(
            Request::post("/sync/diff")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "since_root_hash": "NOT_VALID_HEX!!!"
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
// Test 14: Sync preserves content types
// ===========================================================================

#[tokio::test]
async fn test_sync_preserves_content_type() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    store_file_typed(&engine_a, "/doc.json", b"{\"key\":\"value\"}", "application/json");
    store_file_typed(&engine_a, "/image.png", b"\x89PNG\r\n\x1a\n", "image/png");

    let (ops, _) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    assert!(ops >= 2);

    // Verify files transferred
    assert_eq!(read_file(&engine_b, "/doc.json"), b"{\"key\":\"value\"}");
    assert_eq!(read_file(&engine_b, "/image.png"), b"\x89PNG\r\n\x1a\n");

    // Verify metadata preserved
    let ops_b = DirectoryOps::new(&engine_b);
    let meta = ops_b.get_metadata("/doc.json").unwrap().unwrap();
    assert_eq!(meta.content_type, Some("application/json".to_string()));
}

// ===========================================================================
// Test 15: Multiple incremental syncs work correctly
// ===========================================================================

#[tokio::test]
async fn test_multiple_incremental_syncs() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    // Round 1: initial file
    store_file(&engine_a, "/r1.txt", b"round 1");
    let (_, d1) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    let since1 = d1["root_hash"].as_str().unwrap().to_string();
    assert_eq!(read_file(&engine_b, "/r1.txt"), b"round 1");

    // Round 2: add another file
    store_file(&engine_a, "/r2.txt", b"round 2");
    let (ops2, d2) = sync_pull(&jwt_a, &engine_a, &engine_b, Some(&since1)).await;
    let since2 = d2["root_hash"].as_str().unwrap().to_string();
    assert_eq!(ops2, 1, "round 2 should only sync 1 file");
    assert_eq!(read_file(&engine_b, "/r2.txt"), b"round 2");

    // Round 3: modify r1 and add r3
    store_file(&engine_a, "/r1.txt", b"round 1 modified");
    store_file(&engine_a, "/r3.txt", b"round 3");
    let (ops3, _) = sync_pull(&jwt_a, &engine_a, &engine_b, Some(&since2)).await;
    assert_eq!(ops3, 2, "round 3 should sync 2 changes (1 modified + 1 added)");
    assert_eq!(read_file(&engine_b, "/r1.txt"), b"round 1 modified");
    assert_eq!(read_file(&engine_b, "/r3.txt"), b"round 3");
}

// ===========================================================================
// Test 16: Symlink deletion syncs correctly
// ===========================================================================

#[tokio::test]
async fn test_sync_symlink_deletion() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();
    let (_app_b, _jwt_b, engine_b, _tmp_b) = create_node();

    // Create file and symlink on A
    store_file(&engine_a, "/target.txt", b"target data");
    store_symlink(&engine_a, "/symlink.txt", "/target.txt");

    // Sync to B
    let (_, d) = sync_pull(&jwt_a, &engine_a, &engine_b, None).await;
    let since = d["root_hash"].as_str().unwrap().to_string();

    // Verify symlink exists on B
    let head = engine_b.head_hash().unwrap();
    let tree = walk_version_tree(&engine_b, &head).unwrap();
    assert!(tree.symlinks.contains_key("/symlink.txt"));

    // Delete symlink on A
    let ctx = RequestContext::system();
    let ops_a = DirectoryOps::new(&engine_a);
    ops_a.delete_symlink(&ctx, "/symlink.txt").unwrap();

    // Incremental sync
    let (ops, _) = sync_pull(&jwt_a, &engine_a, &engine_b, Some(&since)).await;
    assert!(ops >= 1, "should sync symlink deletion");

    // Verify symlink is gone on B
    let head = engine_b.head_hash().unwrap();
    let tree = walk_version_tree(&engine_b, &head).unwrap();
    assert!(
        !tree.symlinks.contains_key("/symlink.txt"),
        "symlink should be deleted on B"
    );
    // Target file should still exist
    assert!(file_exists(&engine_b, "/target.txt"));
}

// ===========================================================================
// Test 17: Sync with empty chunk hashes — edge case
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_empty_request() {
    let (app_a, jwt_a, _engine_a, _tmp_a) = create_node();

    let auth = bearer_token(&jwt_a);
    let response = app_a
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({ "hashes": [] })).unwrap(),
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
// Test 18: Sync nonexistent chunk hashes — returns empty chunks
// ===========================================================================

#[tokio::test]
async fn test_sync_chunks_nonexistent_hashes() {
    let (app_a, jwt_a, _engine_a, _tmp_a) = create_node();

    let fake = hex::encode(blake3::hash(b"nonexistent").as_bytes());

    let auth = bearer_token(&jwt_a);
    let response = app_a
        .oneshot(
            Request::post("/sync/chunks")
                .header("content-type", "application/json")
                .header("authorization", &auth)
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({ "hashes": [fake] })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    assert!(
        json["chunks"].as_array().unwrap().is_empty(),
        "nonexistent hash should not return chunks"
    );
}

// ===========================================================================
// Test 19: Diff response structure is well-formed
// ===========================================================================

#[tokio::test]
async fn test_sync_diff_response_structure() {
    let (_app_a, jwt_a, engine_a, _tmp_a) = create_node();

    store_file(&engine_a, "/a.txt", b"aaa");
    store_file(&engine_a, "/b.txt", b"bbb");

    let auth = bearer_token(&jwt_a);
    let app = rebuild_app(&jwt_a, &engine_a);
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

    // Verify top-level structure
    assert!(json["root_hash"].is_string());
    assert!(json["changes"].is_object());
    assert!(json["chunk_hashes_needed"].is_array());

    // Verify changes structure
    let changes = &json["changes"];
    assert!(changes["files_added"].is_array());
    assert!(changes["files_modified"].is_array());
    assert!(changes["files_deleted"].is_array());
    assert!(changes["symlinks_added"].is_array());
    assert!(changes["symlinks_modified"].is_array());
    assert!(changes["symlinks_deleted"].is_array());

    // Verify file entries have required fields
    let added = changes["files_added"].as_array().unwrap();
    for entry in added {
        assert!(entry["path"].is_string(), "entry should have path");
        assert!(entry["hash"].is_string(), "entry should have hash");
        assert!(entry["size"].is_number(), "entry should have size");
        assert!(entry["chunk_hashes"].is_array(), "entry should have chunk_hashes");
    }
}
