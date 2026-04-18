use std::sync::Arc;
use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::{StorageEngine, DirectoryOps, RequestContext};
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::version_access::read_file_at_version;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
    (app, jwt_manager, engine, temp_dir)
}

fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
    create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

fn bearer_token(jwt_manager: &JwtManager) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = TokenClaims {
        sub: uuid::Uuid::nil().to_string(),
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
    serde_json::from_slice(&bytes).expect("valid JSON")
}

// ---------------------------------------------------------------------------
// Engine helper functions (direct access, no HTTP)
// ---------------------------------------------------------------------------

fn store_file(engine: &StorageEngine, path: &str, content: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, path, content, None).unwrap();
}

fn store_file_with_type(engine: &StorageEngine, path: &str, content: &[u8], content_type: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, path, content, Some(content_type)).unwrap();
}

fn create_snapshot(engine: &StorageEngine, name: &str) {
    let ctx = RequestContext::system();
    let vm = VersionManager::new(engine);
    vm.create_snapshot(&ctx, name, HashMap::new()).unwrap();
}

fn read_file(engine: &StorageEngine, path: &str) -> Vec<u8> {
    let ops = DirectoryOps::new(engine);
    ops.read_file(path).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Basic restore: store "original", snapshot, modify, restore from snapshot -> file is "original"
#[tokio::test]
async fn test_restore_basic() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/readme.txt", b"original");
    create_snapshot(&engine, "snap1");
    store_file(&engine, "/readme.txt", b"modified");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/readme.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"snapshot":"snap1"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["restored"], true);

    // Verify file content is restored to "original"
    let content = read_file(&engine, "/readme.txt");
    assert_eq!(content, b"original");
}

/// After restore, verify a "pre-restore-*" snapshot exists
#[tokio::test]
async fn test_restore_creates_auto_snapshot() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/data.txt", b"original");
    create_snapshot(&engine, "snap1");
    store_file(&engine, "/data.txt", b"modified");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/data.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"snapshot":"snap1"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let auto_snap_name = json["auto_snapshot"].as_str().unwrap();
    assert!(auto_snap_name.starts_with("pre-restore-"), "auto snapshot name should start with 'pre-restore-', got: {}", auto_snap_name);

    // Verify the auto snapshot exists in list
    let vm = VersionManager::new(&engine);
    let snapshots = vm.list_snapshots().unwrap();
    let found = snapshots.iter().any(|s| s.name == auto_snap_name);
    assert!(found, "Auto-snapshot '{}' should exist in snapshot list", auto_snap_name);
}

/// The auto-snapshot should capture the state BEFORE restore
#[tokio::test]
async fn test_restore_auto_snapshot_preserves_pre_restore_state() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/file.txt", b"original");
    create_snapshot(&engine, "snap1");
    store_file(&engine, "/file.txt", b"modified");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/file.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"snapshot":"snap1"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let auto_snap_name = json["auto_snapshot"].as_str().unwrap();

    // Current HEAD should have the restored "original" content
    let current_content = read_file(&engine, "/file.txt");
    assert_eq!(current_content, b"original");

    // Auto-snapshot should preserve the pre-restore "modified" content
    let vm = VersionManager::new(&engine);
    let auto_root_hash = vm.resolve_root_hash(Some(auto_snap_name)).unwrap();
    let pre_restore_content = read_file_at_version(&engine, &auto_root_hash, "/file.txt").unwrap();
    assert_eq!(pre_restore_content, b"modified");
}

/// Restore using a version hex hash instead of snapshot name
#[tokio::test]
async fn test_restore_from_version_hash() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/hashfile.txt", b"original");
    create_snapshot(&engine, "snap1");

    // Get the root hash of snap1
    let vm = VersionManager::new(&engine);
    let root_hash = vm.resolve_root_hash(Some("snap1")).unwrap();
    let hex_hash = hex::encode(&root_hash);

    store_file(&engine, "/hashfile.txt", b"modified");

    let app = rebuild_app(&jwt_manager, &engine);
    let body = serde_json::json!({"version": hex_hash}).to_string();
    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/hashfile.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(body))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["restored"], true);
    assert_eq!(json["from_version"], hex_hash);

    let content = read_file(&engine, "/hashfile.txt");
    assert_eq!(content, b"original");
}

/// File didn't exist at snapshot -> 404
#[tokio::test]
async fn test_restore_file_not_at_version() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    // Create a snapshot WITHOUT the target file
    store_file(&engine, "/other.txt", b"other");
    create_snapshot(&engine, "snap1");

    // Now store the file we'll try to restore (it doesn't exist at snap1)
    store_file(&engine, "/newfile.txt", b"new content");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/newfile.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"snapshot":"snap1"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Nonexistent snapshot name -> 404
#[tokio::test]
async fn test_restore_snapshot_not_found() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/file.txt", b"content");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/file.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"snapshot":"nonexistent"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Non-root token -> 403
#[tokio::test]
async fn test_restore_no_permission() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = non_root_bearer_token(&jwt_manager);

    store_file(&engine, "/file.txt", b"content");
    create_snapshot(&engine, "snap1");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/file.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"snapshot":"snap1"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// Empty JSON body {} -> 400
#[tokio::test]
async fn test_restore_missing_both_params() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/file.txt", b"content");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/file.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Verify response has restored, path, auto_snapshot, size, from_snapshot fields
#[tokio::test]
async fn test_restore_response_shape() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/shape.txt", b"hello world");
    create_snapshot(&engine, "snap1");
    store_file(&engine, "/shape.txt", b"changed");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/shape.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"snapshot":"snap1"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;

    // Must have these fields
    assert_eq!(json["restored"], true);
    assert_eq!(json["path"], "shape.txt");
    assert!(json["auto_snapshot"].is_string(), "auto_snapshot should be a string");
    assert!(json["auto_snapshot"].as_str().unwrap().starts_with("pre-restore-"));
    assert_eq!(json["size"], 11); // "hello world" is 11 bytes
    assert_eq!(json["from_snapshot"], "snap1");

    // from_version should not be present when using snapshot
    assert!(json.get("from_version").is_none() || json["from_version"].is_null());
}

/// Store file with explicit content-type, snapshot, modify, restore -> content-type preserved
#[tokio::test]
async fn test_restore_preserves_content_type() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file_with_type(&engine, "/config.json", b"{\"a\":1}", "application/json");
    create_snapshot(&engine, "snap1");
    store_file_with_type(&engine, "/config.json", b"{\"a\":2}", "application/json");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/config.json")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"snapshot":"snap1"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Verify the file content is restored
    let content = read_file(&engine, "/config.json");
    assert_eq!(content, b"{\"a\":1}");

    // Verify content-type is preserved by reading via HTTP GET /engine
    let app2 = rebuild_app(&jwt_manager, &engine);
    let get_request = Request::builder()
        .method("GET")
        .uri("/files/config.json")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let get_response = app2.oneshot(get_request).await.unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);

    let ct = get_response.headers().get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(ct, "application/json", "Content-type should be preserved after restore");
}
