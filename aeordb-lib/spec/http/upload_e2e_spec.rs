use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::directory_ops::DirectoryOps;
use aeordb::engine::gc::run_gc;
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
    };
    let token = jwt_manager.create_token(&claims).expect("create token");
    format!("Bearer {}", token)
}

/// Collect response body into JSON.
async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

/// Collect response body into raw bytes.
async fn body_bytes(body: Body) -> Vec<u8> {
    body.collect().await.unwrap().to_bytes().to_vec()
}

/// Compute a chunk hash the same way the server expects: blake3("chunk:" + data).
fn compute_chunk_hash(data: &[u8]) -> String {
    let mut input = Vec::with_capacity(6 + data.len());
    input.extend_from_slice(b"chunk:");
    input.extend_from_slice(data);
    hex::encode(blake3::hash(&input).as_bytes())
}

/// Upload a single chunk via the HTTP API, returning its hex hash.
async fn upload_chunk(app: axum::Router, token: &str, data: &[u8]) -> String {
    let hash = compute_chunk_hash(data);
    let resp = app
        .oneshot(
            Request::put(&format!("/upload/chunks/{}", hash))
                .header("Authorization", token)
                .header("Content-Type", "application/octet-stream")
                .body(Body::from(data.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "chunk upload failed with status {}",
        resp.status()
    );
    hash
}

// ===========================================================================
// 1. Full round trip: config -> check -> upload -> commit -> read
// ===========================================================================

#[tokio::test]
async fn test_full_round_trip_config_check_upload_commit_read() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    // Phase 1: GET /upload/config
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::get("/upload/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let config = body_json(resp.into_body()).await;
    assert!(config["chunk_size"].as_u64().unwrap() > 0);
    assert!(config["hash_algorithm"].as_str().is_some());

    // Phase 2: POST /upload/check with chunk hashes
    let file_content = b"This is the complete file content for round-trip test.";
    let chunk_hash = compute_chunk_hash(file_content);

    let check_body = serde_json::json!({ "hashes": [chunk_hash] });
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/upload/check")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&check_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let check_result = body_json(resp.into_body()).await;
    // Chunk should be needed (not yet uploaded)
    assert_eq!(check_result["needed"].as_array().unwrap().len(), 1);
    assert_eq!(check_result["have"].as_array().unwrap().len(), 0);

    // Phase 3: PUT /upload/chunks/{hash}
    let h = upload_chunk(rebuild_app(&jwt, &engine), &token, file_content).await;
    assert_eq!(h, chunk_hash);

    // Phase 4: POST /upload/commit
    let commit_body = serde_json::json!({
        "files": [{
            "path": "/roundtrip/file.txt",
            "chunks": [h],
            "content_type": "text/plain"
        }]
    });
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/upload/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let commit_result = body_json(resp.into_body()).await;
    assert_eq!(commit_result["committed"].as_u64().unwrap(), 1);

    // Phase 5: GET /engine/roundtrip/file.txt to read back
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::get("/engine/roundtrip/file.txt")
                .header("Authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body_bytes(resp.into_body()).await;
    assert_eq!(bytes, file_content);
}

// ===========================================================================
// 2. Incremental upload: shared chunk detected, only new chunk uploaded
// ===========================================================================

#[tokio::test]
async fn test_incremental_upload_only_new_chunks() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    // V1: two chunks -> commit
    let shared_chunk = b"shared data across versions";
    let v1_only_chunk = b"version 1 only data";

    let h_shared = upload_chunk(rebuild_app(&jwt, &engine), &token, shared_chunk).await;
    let h_v1 = upload_chunk(rebuild_app(&jwt, &engine), &token, v1_only_chunk).await;

    let commit_body = serde_json::json!({
        "files": [{
            "path": "/incremental.bin",
            "chunks": [h_shared, h_v1],
            "content_type": "application/octet-stream"
        }]
    });
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/upload/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // V2: shares h_shared but has a new chunk
    let v2_only_chunk = b"version 2 only data";
    let h_v2 = compute_chunk_hash(v2_only_chunk);

    // Check: server should already have h_shared but need h_v2
    let check_body = serde_json::json!({ "hashes": [h_shared, h_v2] });
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/upload/check")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&check_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let check_result = body_json(resp.into_body()).await;
    let have = check_result["have"].as_array().unwrap();
    let needed = check_result["needed"].as_array().unwrap();
    assert_eq!(have.len(), 1, "Should have 1 existing chunk");
    assert_eq!(have[0].as_str().unwrap(), h_shared);
    assert_eq!(needed.len(), 1, "Should need 1 new chunk");
    assert_eq!(needed[0].as_str().unwrap(), h_v2);

    // Upload only the needed chunk
    let h_v2_uploaded = upload_chunk(rebuild_app(&jwt, &engine), &token, v2_only_chunk).await;
    assert_eq!(h_v2_uploaded, h_v2);

    // Commit v2
    let commit_body = serde_json::json!({
        "files": [{
            "path": "/incremental.bin",
            "chunks": [h_shared, h_v2],
            "content_type": "application/octet-stream"
        }]
    });
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/upload/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Read back v2
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::get("/engine/incremental.bin")
                .header("Authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = body_bytes(resp.into_body()).await;
    let expected: Vec<u8> = [shared_chunk.as_slice(), v2_only_chunk.as_slice()].concat();
    assert_eq!(bytes, expected);
}

// ===========================================================================
// 3. GC collects uncommitted chunks
// ===========================================================================

#[tokio::test]
async fn test_gc_collects_uncommitted_chunks() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    // Upload a chunk but never commit it
    let orphan_data = b"orphan chunk that will be garbage collected";
    let _h = upload_chunk(rebuild_app(&jwt, &engine), &token, orphan_data).await;

    // Run GC
    let ctx = RequestContext::system();
    let result = run_gc(&engine, &ctx, false).unwrap();
    assert!(
        result.garbage_entries > 0,
        "GC should have found garbage entries (uncommitted chunk), found: {}",
        result.garbage_entries
    );
}

// ===========================================================================
// 4. Commit file matches regular PUT (byte-identical content)
// ===========================================================================

#[tokio::test]
async fn test_commit_file_matches_regular_put() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    let file_content = b"Content that should be identical whether stored via PUT or chunk upload.";

    // Store via regular DirectoryOps
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file(&ctx, "/regular.txt", file_content, Some("text/plain"))
        .unwrap();

    // Store via upload protocol
    let h = upload_chunk(rebuild_app(&jwt, &engine), &token, file_content).await;
    let commit_body = serde_json::json!({
        "files": [{
            "path": "/chunked.txt",
            "chunks": [h],
            "content_type": "text/plain"
        }]
    });
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/upload/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Read both back via DirectoryOps and compare
    let regular_bytes = ops.read_file("/regular.txt").unwrap();
    let chunked_bytes = ops.read_file("/chunked.txt").unwrap();

    assert_eq!(
        regular_bytes, chunked_bytes,
        "Content stored via regular PUT and via chunk upload should be byte-identical"
    );
    assert_eq!(regular_bytes, file_content.to_vec());
}
