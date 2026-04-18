use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::EntryType;
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

/// Compute a chunk hash the same way the server expects: blake3("chunk:" + data).
fn compute_chunk_hash(data: &[u8]) -> String {
    let mut input = Vec::with_capacity(6 + data.len());
    input.extend_from_slice(b"chunk:");
    input.extend_from_slice(data);
    hex::encode(blake3::hash(&input).as_bytes())
}

// ===========================================================================
// 1. GET /upload/config returns hash algo and chunk size
// ===========================================================================

#[tokio::test]
async fn test_config_returns_hash_algo_and_chunk_size() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/upload/config")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert!(json["hash_algorithm"].as_str().is_some(), "hash_algorithm should be a string");
    assert!(json["chunk_size"].as_u64().is_some(), "chunk_size should be a number");
    assert_eq!(json["chunk_size"].as_u64().unwrap(), 262_144);
    assert_eq!(json["chunk_hash_prefix"].as_str().unwrap(), "chunk:");
    // Default engine uses blake3
    assert!(
        json["hash_algorithm"].as_str().unwrap().contains("blake3"),
        "Expected blake3 in hash_algorithm, got: {}",
        json["hash_algorithm"]
    );
}

// ===========================================================================
// 2. GET /upload/config requires auth (moved behind auth middleware, M5)
// ===========================================================================

#[tokio::test]
async fn test_config_no_auth_required() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/upload/config")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = rebuild_app(&jwt_manager, &engine)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert!(json["hash_algorithm"].as_str().is_some());
    assert!(json["chunk_size"].as_u64().is_some());
}

// ===========================================================================
// 3. POST /upload/check identifies existing chunks
// ===========================================================================

#[tokio::test]
async fn test_check_identifies_existing_chunks() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Store a chunk directly
    let chunk_data = b"hello world chunk data";
    let chunk_hash = compute_chunk_hash(chunk_data);
    let hash_bytes = hex::decode(&chunk_hash).unwrap();
    engine
        .store_entry(EntryType::Chunk, &hash_bytes, chunk_data)
        .unwrap();

    // Rebuild app since engine state changed
    let app = rebuild_app(&jwt_manager, &engine);

    let body = serde_json::json!({ "hashes": [chunk_hash] });
    let request = Request::builder()
        .method("POST")
        .uri("/upload/check")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let have = json["have"].as_array().unwrap();
    let needed = json["needed"].as_array().unwrap();
    assert_eq!(have.len(), 1);
    assert_eq!(have[0].as_str().unwrap(), chunk_hash);
    assert!(needed.is_empty());
}

// ===========================================================================
// 4. POST /upload/check identifies missing chunks
// ===========================================================================

#[tokio::test]
async fn test_check_identifies_missing_chunks() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let fake_hash = hex::encode([0xABu8; 32]);
    let body = serde_json::json!({ "hashes": [fake_hash] });
    let request = Request::builder()
        .method("POST")
        .uri("/upload/check")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let have = json["have"].as_array().unwrap();
    let needed = json["needed"].as_array().unwrap();
    assert!(have.is_empty());
    assert_eq!(needed.len(), 1);
    assert_eq!(needed[0].as_str().unwrap(), fake_hash);
}

// ===========================================================================
// 5. POST /upload/check mixed have and needed
// ===========================================================================

#[tokio::test]
async fn test_check_mixed_have_and_needed() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Store one chunk
    let existing_data = b"existing chunk";
    let existing_hash = compute_chunk_hash(existing_data);
    let existing_bytes = hex::decode(&existing_hash).unwrap();
    engine
        .store_entry(EntryType::Chunk, &existing_bytes, existing_data)
        .unwrap();

    // A hash that does not exist
    let missing_hash = hex::encode([0xCDu8; 32]);

    let app = rebuild_app(&jwt_manager, &engine);

    let body = serde_json::json!({ "hashes": [existing_hash, missing_hash] });
    let request = Request::builder()
        .method("POST")
        .uri("/upload/check")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let have = json["have"].as_array().unwrap();
    let needed = json["needed"].as_array().unwrap();
    assert_eq!(have.len(), 1);
    assert_eq!(have[0].as_str().unwrap(), existing_hash);
    assert_eq!(needed.len(), 1);
    assert_eq!(needed[0].as_str().unwrap(), missing_hash);
}

// ===========================================================================
// 6. POST /upload/check with empty hash list
// ===========================================================================

#[tokio::test]
async fn test_check_empty_hash_list() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let body = serde_json::json!({ "hashes": [] });
    let request = Request::builder()
        .method("POST")
        .uri("/upload/check")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let have = json["have"].as_array().unwrap();
    let needed = json["needed"].as_array().unwrap();
    assert!(have.is_empty());
    assert!(needed.is_empty());
}

// ===========================================================================
// 7. POST /upload/check requires auth (no token -> 401)
// ===========================================================================

#[tokio::test]
async fn test_check_requires_auth() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let body = serde_json::json!({ "hashes": [] });
    let request = Request::builder()
        .method("POST")
        .uri("/upload/check")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// 8. POST /upload/check with invalid hex hash returns 400
// ===========================================================================

#[tokio::test]
async fn test_check_invalid_hex_hash_returns_400() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let body = serde_json::json!({ "hashes": ["not-valid-hex!@#$"] });
    let request = Request::builder()
        .method("POST")
        .uri("/upload/check")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("Invalid hex hash"));
}

// ===========================================================================
// 9. POST /upload/check with invalid JSON body returns 4xx
// ===========================================================================

#[tokio::test]
async fn test_check_invalid_json_body() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/upload/check")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from("not json"))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert!(
        response.status().is_client_error(),
        "Expected 4xx for invalid JSON, got {}",
        response.status()
    );
}

// ===========================================================================
// 10. POST /upload/check with expired token returns 401
// ===========================================================================

#[tokio::test]
async fn test_check_expired_token_returns_401() {
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

    let body = serde_json::json!({ "hashes": [] });
    let request = Request::builder()
        .method("POST")
        .uri("/upload/check")
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// 11. GET /upload/config wrong method (POST) returns 405
// ===========================================================================

#[tokio::test]
async fn test_config_post_method_not_allowed() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/upload/config")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = rebuild_app(&jwt_manager, &engine)
        .oneshot(request)
        .await
        .unwrap();
    assert!(
        response.status() == StatusCode::METHOD_NOT_ALLOWED
            || response.status() == StatusCode::NOT_FOUND,
        "Expected 405 or 404 for POST on GET-only route, got {}",
        response.status()
    );
}

// ===========================================================================
// 12. PUT /upload/chunks/{hash} with valid hash -> 201
// ===========================================================================

#[tokio::test]
async fn test_chunk_upload_valid_hash() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let chunk_data = b"hello world chunk upload test";
    let hash_hex = compute_chunk_hash(chunk_data);

    let request = Request::builder()
        .method("PUT")
        .uri(format!("/upload/chunks/{}", hash_hex))
        .header("authorization", &auth)
        .body(Body::from(chunk_data.to_vec()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["status"].as_str().unwrap(), "created");
    assert_eq!(json["hash"].as_str().unwrap(), hash_hex);
}

// ===========================================================================
// 13. PUT /upload/chunks/{hash} with wrong hash -> 400
// ===========================================================================

#[tokio::test]
async fn test_chunk_upload_hash_mismatch() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let chunk_data = b"some chunk data";
    // Use a valid hex string that doesn't match the actual hash
    let wrong_hash = hex::encode([0xAAu8; 32]);

    let request = Request::builder()
        .method("PUT")
        .uri(format!("/upload/chunks/{}", wrong_hash))
        .header("authorization", &auth)
        .body(Body::from(chunk_data.to_vec()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("Hash mismatch"));
}

// ===========================================================================
// 14. PUT /upload/chunks/{hash} too large -> 400
// ===========================================================================

#[tokio::test]
async fn test_chunk_upload_too_large() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let oversized = vec![0u8; 262_145]; // one byte over limit
    let hash_hex = compute_chunk_hash(&oversized);

    let request = Request::builder()
        .method("PUT")
        .uri(format!("/upload/chunks/{}", hash_hex))
        .header("authorization", &auth)
        .body(Body::from(oversized))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = body_json(response.into_body()).await;
    assert!(json["error"].as_str().unwrap().contains("maximum size"));
}

// ===========================================================================
// 15. PUT /upload/chunks/{hash} dedup: first 201, second 200 "exists"
// ===========================================================================

#[tokio::test]
async fn test_chunk_upload_dedup() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let chunk_data = b"dedup test chunk data";
    let hash_hex = compute_chunk_hash(chunk_data);

    // First upload -> 201 created
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/upload/chunks/{}", hash_hex))
        .header("authorization", &auth)
        .body(Body::from(chunk_data.to_vec()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["status"].as_str().unwrap(), "created");

    // Second upload -> 200 exists
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/upload/chunks/{}", hash_hex))
        .header("authorization", &auth)
        .body(Body::from(chunk_data.to_vec()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["status"].as_str().unwrap(), "exists");
    assert_eq!(json["hash"].as_str().unwrap(), hash_hex);
}

// ===========================================================================
// 16. PUT /upload/chunks/{hash} empty chunk -> 201
// ===========================================================================

#[tokio::test]
async fn test_chunk_upload_empty_chunk() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let chunk_data: &[u8] = b"";
    let hash_hex = compute_chunk_hash(chunk_data);

    let request = Request::builder()
        .method("PUT")
        .uri(format!("/upload/chunks/{}", hash_hex))
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["status"].as_str().unwrap(), "created");
    assert_eq!(json["hash"].as_str().unwrap(), hash_hex);
}

// ===========================================================================
// 17. PUT /upload/chunks/{hash} requires auth (no token -> 401)
// ===========================================================================

#[tokio::test]
async fn test_chunk_upload_requires_auth() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let chunk_data = b"auth test chunk";
    let hash_hex = compute_chunk_hash(chunk_data);

    let request = Request::builder()
        .method("PUT")
        .uri(format!("/upload/chunks/{}", hash_hex))
        .body(Body::from(chunk_data.to_vec()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
