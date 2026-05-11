use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
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
            Request::put(&format!("/blobs/chunks/{}", hash))
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
// 1. Commit a single file: upload chunks, commit, verify readable via GET
// ===========================================================================

#[tokio::test]
async fn test_commit_single_file() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    // Upload one chunk
    let data = b"Hello, commit world!";
    let h1 = upload_chunk(rebuild_app(&jwt, &engine), &token, data).await;

    // Commit
    let commit_body = serde_json::json!({
        "files": [{
            "path": "/test/hello.txt",
            "chunks": [h1],
            "content_type": "text/plain"
        }]
    });

    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/blobs/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp.into_body()).await;
    assert_eq!(json["committed"].as_u64().unwrap(), 1);
    assert_eq!(json["files"].as_array().unwrap().len(), 1);
    assert_eq!(json["files"][0]["path"].as_str().unwrap(), "/test/hello.txt");

    // Read it back via GET /engine/test/hello.txt
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::get("/files/test/hello.txt")
                .header("Authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = body_bytes(resp.into_body()).await;
    assert_eq!(bytes, data);
}

// ===========================================================================
// 2. Commit multiple files in the same directory
// ===========================================================================

#[tokio::test]
async fn test_commit_multiple_files() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    let data_a = b"File A content";
    let data_b = b"File B content";
    let data_c = b"File C content";

    let ha = upload_chunk(rebuild_app(&jwt, &engine), &token, data_a).await;
    let hb = upload_chunk(rebuild_app(&jwt, &engine), &token, data_b).await;
    let hc = upload_chunk(rebuild_app(&jwt, &engine), &token, data_c).await;

    let commit_body = serde_json::json!({
        "files": [
            { "path": "/docs/a.txt", "chunks": [ha], "content_type": "text/plain" },
            { "path": "/docs/b.txt", "chunks": [hb], "content_type": "text/plain" },
            { "path": "/docs/c.txt", "chunks": [hc], "content_type": "text/plain" },
        ]
    });

    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/blobs/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp.into_body()).await;
    assert_eq!(json["committed"].as_u64().unwrap(), 3);

    // Read each file back
    for (path, expected) in [
        ("/docs/a.txt", data_a.as_slice()),
        ("/docs/b.txt", data_b.as_slice()),
        ("/docs/c.txt", data_c.as_slice()),
    ] {
        let resp = rebuild_app(&jwt, &engine)
            .oneshot(
                Request::get(&format!("/files{}", path))
                    .header("Authorization", &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "GET {} failed", path);
        let bytes = body_bytes(resp.into_body()).await;
        assert_eq!(bytes, expected, "Content mismatch for {}", path);
    }
}

// ===========================================================================
// 3. Commit with nonexistent chunk hash returns 400
// ===========================================================================

#[tokio::test]
async fn test_commit_missing_chunks() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    let fake_hash = hex::encode([0xDEu8; 32]);
    let commit_body = serde_json::json!({
        "files": [{
            "path": "/missing.txt",
            "chunks": [fake_hash],
            "content_type": "text/plain"
        }]
    });

    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/blobs/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp.into_body()).await;
    assert!(
        json["error"].as_str().unwrap().contains("Missing"),
        "Expected 'Missing' in error, got: {}",
        json["error"]
    );
}

// ===========================================================================
// 4. Commit with empty files list returns 400
// ===========================================================================

#[tokio::test]
async fn test_commit_empty_files_list() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    let commit_body = serde_json::json!({ "files": [] });

    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/blobs/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp.into_body()).await;
    assert!(
        json["error"].as_str().unwrap().contains("No files"),
        "Expected 'No files' in error, got: {}",
        json["error"]
    );
}

// ===========================================================================
// 5. Commit a file with zero chunks succeeds (empty file)
// ===========================================================================

#[tokio::test]
async fn test_commit_empty_file_zero_chunks() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    let commit_body = serde_json::json!({
        "files": [{
            "path": "/empty.txt",
            "chunks": [],
            "content_type": "text/plain"
        }]
    });

    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/blobs/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp.into_body()).await;
    assert_eq!(json["committed"].as_u64().unwrap(), 1);
    assert_eq!(json["files"][0]["size"].as_u64().unwrap(), 0);
}

// ===========================================================================
// 6. Commit preserves chunk order: upload A then B, read back A+B
// ===========================================================================

#[tokio::test]
async fn test_commit_preserves_chunk_order() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    let chunk_a = b"AAAA_first_chunk_data";
    let chunk_b = b"BBBB_second_chunk_data";

    let ha = upload_chunk(rebuild_app(&jwt, &engine), &token, chunk_a).await;
    let hb = upload_chunk(rebuild_app(&jwt, &engine), &token, chunk_b).await;

    let commit_body = serde_json::json!({
        "files": [{
            "path": "/ordered.bin",
            "chunks": [ha, hb],
            "content_type": "application/octet-stream"
        }]
    });

    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/blobs/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Read back the file
    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::get("/files/ordered.bin")
                .header("Authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = body_bytes(resp.into_body()).await;
    let expected: Vec<u8> = [chunk_a.as_slice(), chunk_b.as_slice()].concat();
    assert_eq!(bytes, expected);
    // Verify ordering: content starts with chunk A's data
    assert!(
        bytes.starts_with(chunk_a),
        "Content should start with chunk A data"
    );
}

// ===========================================================================
// 7. Commit requires auth (no token -> 401)
// ===========================================================================

#[tokio::test]
async fn test_commit_requires_auth() {
    let (app, _jwt, _engine, _tmp) = test_app();

    let commit_body = serde_json::json!({
        "files": [{
            "path": "/unauth.txt",
            "chunks": [],
            "content_type": "text/plain"
        }]
    });

    let resp = app
        .oneshot(
            Request::post("/blobs/commit")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// 8. Commit MUST refuse system paths even for root callers. Without this
//    check, any authenticated user could overwrite /.aeordb-system/api-keys/
//    and mint a root API key — full takeover.
// ===========================================================================

#[tokio::test]
async fn test_commit_rejects_system_path_aeordb_system() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    // Even root cannot write system paths through /blobs/commit. The dedicated
    // system_store APIs are the only legitimate way to mutate /.aeordb-system/.
    let commit_body = serde_json::json!({
        "files": [{
            "path": "/.aeordb-system/api-keys/00000000-0000-0000-0000-000000000000",
            "chunks": [],
            "content_type": "application/json"
        }]
    });

    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/blobs/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let json = body_json(resp.into_body()).await;
    let err = json["error"].as_str().unwrap_or("");
    assert!(
        err.contains("reserved") || err.contains("system"),
        "Expected reserved-path error, got: {}",
        err
    );
}

#[tokio::test]
async fn test_commit_rejects_system_path_aeordb_config() {
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    let commit_body = serde_json::json!({
        "files": [{
            "path": "/.aeordb-config/cron.json",
            "chunks": [],
            "content_type": "application/json"
        }]
    });

    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/blobs/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_commit_rejects_system_path_in_mixed_batch() {
    // Even one system path in a batch must reject the whole commit (atomic).
    let (_app, jwt, engine, _tmp) = test_app();
    let token = root_bearer_token(&jwt);

    let commit_body = serde_json::json!({
        "files": [
            { "path": "/safe.txt", "chunks": [], "content_type": "text/plain" },
            { "path": "/.aeordb-system/api-keys/x", "chunks": [], "content_type": "application/json" }
        ]
    });

    let resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::post("/blobs/commit")
                .header("Authorization", &token)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(&commit_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Verify the safe file was NOT committed (atomic rejection).
    let list_resp = rebuild_app(&jwt, &engine)
        .oneshot(
            Request::get("/files/safe.txt")
                .header("Authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        list_resp.status(),
        StatusCode::NOT_FOUND,
        "safe.txt should not have been committed when the batch failed"
    );
}
