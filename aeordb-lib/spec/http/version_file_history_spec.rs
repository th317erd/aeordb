use std::sync::Arc;
use std::collections::HashMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::{StorageEngine, DirectoryOps, RequestContext};
use aeordb::engine::version_manager::VersionManager;
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

async fn body_bytes(body: Body) -> Vec<u8> {
    body.collect().await.unwrap().to_bytes().to_vec()
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body_bytes(body).await;
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

fn delete_file(engine: &StorageEngine, path: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.delete_file(&ctx, path).unwrap();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_history_file_added() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/readme.txt", b"hello");
    create_snapshot(&engine, "snap1");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/readme.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["path"], "readme.txt");

    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["snapshot"], "snap1");
    assert_eq!(history[0]["change_type"], "added");
    assert!(history[0]["size"].is_number());
    assert!(history[0]["content_hash"].is_string());
}

#[tokio::test]
async fn test_history_file_modified() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/data.txt", b"version1");
    create_snapshot(&engine, "snap1");

    store_file(&engine, "/data.txt", b"version2");
    create_snapshot(&engine, "snap2");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/data.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);

    // Newest first
    assert_eq!(history[0]["snapshot"], "snap2");
    assert_eq!(history[0]["change_type"], "modified");
    assert_eq!(history[1]["snapshot"], "snap1");
    assert_eq!(history[1]["change_type"], "added");

    // Hashes must differ between added and modified
    let hash1 = history[1]["content_hash"].as_str().unwrap();
    let hash2 = history[0]["content_hash"].as_str().unwrap();
    assert_ne!(hash1, hash2);
}

#[tokio::test]
async fn test_history_file_unchanged() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/stable.txt", b"same content");
    create_snapshot(&engine, "snap1");
    create_snapshot(&engine, "snap2");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/stable.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);

    // Newest first
    assert_eq!(history[0]["snapshot"], "snap2");
    assert_eq!(history[0]["change_type"], "unchanged");
    assert_eq!(history[1]["snapshot"], "snap1");
    assert_eq!(history[1]["change_type"], "added");

    // Hashes must be the same for unchanged
    let hash1 = history[1]["content_hash"].as_str().unwrap();
    let hash2 = history[0]["content_hash"].as_str().unwrap();
    assert_eq!(hash1, hash2);
}

#[tokio::test]
async fn test_history_file_deleted() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/temp.txt", b"temporary");
    create_snapshot(&engine, "snap1");

    delete_file(&engine, "/temp.txt");
    create_snapshot(&engine, "snap2");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/temp.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);

    // Newest first
    assert_eq!(history[0]["snapshot"], "snap2");
    assert_eq!(history[0]["change_type"], "deleted");
    // Deleted entries should NOT have size or content_hash
    assert!(history[0].get("size").is_none() || history[0]["size"].is_null());

    assert_eq!(history[1]["snapshot"], "snap1");
    assert_eq!(history[1]["change_type"], "added");
}

#[tokio::test]
async fn test_history_full_lifecycle() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    // snap1: file added
    store_file(&engine, "/lifecycle.txt", b"original");
    create_snapshot(&engine, "snap1");

    // snap2: file modified
    store_file(&engine, "/lifecycle.txt", b"updated content");
    create_snapshot(&engine, "snap2");

    // snap3: file unchanged
    create_snapshot(&engine, "snap3");

    // snap4: file deleted
    delete_file(&engine, "/lifecycle.txt");
    create_snapshot(&engine, "snap4");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/lifecycle.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 4);

    // Newest first
    assert_eq!(history[0]["snapshot"], "snap4");
    assert_eq!(history[0]["change_type"], "deleted");

    assert_eq!(history[1]["snapshot"], "snap3");
    assert_eq!(history[1]["change_type"], "unchanged");

    assert_eq!(history[2]["snapshot"], "snap2");
    assert_eq!(history[2]["change_type"], "modified");

    assert_eq!(history[3]["snapshot"], "snap1");
    assert_eq!(history[3]["change_type"], "added");
}

#[tokio::test]
async fn test_history_ordering_newest_first() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/ordered.txt", b"v1");
    create_snapshot(&engine, "alpha");

    store_file(&engine, "/ordered.txt", b"v2");
    create_snapshot(&engine, "beta");

    store_file(&engine, "/ordered.txt", b"v3");
    create_snapshot(&engine, "gamma");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/ordered.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 3);

    // First element is the newest snapshot
    assert_eq!(history[0]["snapshot"], "gamma");
    assert_eq!(history[1]["snapshot"], "beta");
    assert_eq!(history[2]["snapshot"], "alpha");

    // Timestamps should be in descending order
    let ts0 = history[0]["timestamp"].as_i64().unwrap();
    let ts1 = history[1]["timestamp"].as_i64().unwrap();
    let ts2 = history[2]["timestamp"].as_i64().unwrap();
    assert!(ts0 >= ts1);
    assert!(ts1 >= ts2);
}

#[tokio::test]
async fn test_history_never_existed() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    // Create a snapshot with some other file, not the one we query
    store_file(&engine, "/other.txt", b"other");
    create_snapshot(&engine, "snap1");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/nonexistent.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["path"], "nonexistent.txt");

    let history = json["history"].as_array().unwrap();
    assert!(history.is_empty());
}

#[tokio::test]
async fn test_history_includes_metadata() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file_with_type(&engine, "/meta.json", b"{\"key\":\"value\"}", "application/json");
    create_snapshot(&engine, "snap1");

    store_file_with_type(&engine, "/meta.json", b"{\"key\":\"updated\"}", "application/json");
    create_snapshot(&engine, "snap2");

    delete_file(&engine, "/meta.json");
    create_snapshot(&engine, "snap3");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/meta.json")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 3);

    // Check "added" entry (snap1, which is history[2] since newest-first)
    let added = &history[2];
    assert_eq!(added["change_type"], "added");
    assert!(added["snapshot"].is_string());
    assert!(added["timestamp"].is_number());
    assert!(added["size"].is_number());
    assert!(added["content_hash"].is_string());
    assert_eq!(added["content_type"], "application/json");

    // Check "modified" entry (snap2 -> history[1])
    let modified = &history[1];
    assert_eq!(modified["change_type"], "modified");
    assert!(modified["size"].is_number());
    assert!(modified["content_hash"].is_string());
    assert_eq!(modified["content_type"], "application/json");

    // Check "deleted" entry (snap3 -> history[0])
    let deleted = &history[0];
    assert_eq!(deleted["change_type"], "deleted");
    assert!(deleted["snapshot"].is_string());
    assert!(deleted["timestamp"].is_number());
    // Deleted entries should NOT have size or content_hash
    assert!(deleted.get("size").is_none() || deleted["size"].is_null());
    assert!(deleted.get("content_hash").is_none() || deleted["content_hash"].is_null());
}

#[tokio::test]
async fn test_history_no_snapshots() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    // Store a file but do NOT create any snapshots
    store_file(&engine, "/lonely.txt", b"no snapshots");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/lonely.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["path"], "lonely.txt");

    let history = json["history"].as_array().unwrap();
    assert!(history.is_empty());
}

#[tokio::test]
async fn test_history_unauthenticated_returns_401() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "/secret.txt", b"classified");
    create_snapshot(&engine, "snap1");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/secret.txt")
        // No authorization header
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_history_invalid_token_returns_401() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "/secret.txt", b"classified");
    create_snapshot(&engine, "snap1");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/secret.txt")
        .header("authorization", "Bearer invalid-garbage-token")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_history_nested_path() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/deep/nested/dir/file.txt", b"nested content");
    create_snapshot(&engine, "snap1");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/deep/nested/dir/file.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    // The path in the wildcard match includes "deep/nested/dir/file.txt/history"
    // but the route pattern is /version/file-history/{*path}/history so axum should
    // capture "deep/nested/dir/file.txt" as the path.
    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["change_type"], "added");
}

#[tokio::test]
async fn test_history_re_added_after_delete() {
    let (_, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    // Add, snapshot, delete, snapshot, re-add, snapshot
    store_file(&engine, "/phoenix.txt", b"born");
    create_snapshot(&engine, "snap1");

    delete_file(&engine, "/phoenix.txt");
    create_snapshot(&engine, "snap2");

    store_file(&engine, "/phoenix.txt", b"reborn");
    create_snapshot(&engine, "snap3");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/phoenix.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 3);

    // Newest first
    assert_eq!(history[0]["snapshot"], "snap3");
    assert_eq!(history[0]["change_type"], "added");

    assert_eq!(history[1]["snapshot"], "snap2");
    assert_eq!(history[1]["change_type"], "deleted");

    assert_eq!(history[2]["snapshot"], "snap1");
    assert_eq!(history[2]["change_type"], "added");
}
