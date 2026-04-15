use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
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

fn store_file(engine: &StorageEngine, path: &str, content: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, path, content, None).unwrap();
}

fn store_symlink(engine: &StorageEngine, path: &str, target: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_symlink(&ctx, path, target).unwrap();
}

// ---------------------------------------------------------------------------
// POST /engine-symlink/{*path}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_symlink() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/engine-symlink/link.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"target":"/data.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["path"], "/link.txt");
    assert_eq!(json["target"], "/data.txt");
    assert_eq!(json["entry_type"], 8);
    assert!(json["created_at"].is_number());
    assert!(json["updated_at"].is_number());
}

#[tokio::test]
async fn test_create_symlink_update() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    // First create
    let request = Request::builder()
        .method("POST")
        .uri("/engine-symlink/link.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"target":"/old.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let first_json = body_json(response.into_body()).await;
    assert_eq!(first_json["target"], "/old.txt");

    // Update with a new target
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("POST")
        .uri("/engine-symlink/link.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"target":"/new.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let second_json = body_json(response.into_body()).await;
    assert_eq!(second_json["path"], "/link.txt");
    assert_eq!(second_json["target"], "/new.txt");
}

#[tokio::test]
async fn test_create_symlink_missing_target() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/engine-symlink/link.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = body_json(response.into_body()).await;
    let error_msg = json["error"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("target"),
        "Expected error about missing target, got: {}",
        error_msg
    );
}

#[tokio::test]
async fn test_create_symlink_empty_target() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/engine-symlink/link.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"target":""}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = body_json(response.into_body()).await;
    let error_msg = json["error"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("target"),
        "Expected error about empty target, got: {}",
        error_msg
    );
}

// ---------------------------------------------------------------------------
// GET /engine/{*path} — transparent symlink resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_symlink_transparent() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/file.txt", b"hello");
    store_symlink(&engine, "/link", "/file.txt");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/engine/link")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Should have symlink target header
    let symlink_target = response.headers().get("X-Symlink-Target")
        .expect("X-Symlink-Target header missing")
        .to_str().unwrap();
    assert_eq!(symlink_target, "/file.txt");

    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"hello");
}

// ---------------------------------------------------------------------------
// GET /engine/{*path}?nofollow=true — symlink metadata
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_symlink_nofollow() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_symlink(&engine, "/link", "/file.txt");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/engine/link?nofollow=true")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["path"], "/link");
    assert_eq!(json["target"], "/file.txt");
    assert_eq!(json["entry_type"], 8);
}

// ---------------------------------------------------------------------------
// HEAD /engine/{*path} — symlink headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_head_symlink() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_symlink(&engine, "/link", "/somewhere.txt");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("HEAD")
        .uri("/engine/link")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let entry_type = response.headers().get("X-Entry-Type")
        .expect("X-Entry-Type header missing")
        .to_str().unwrap();
    assert_eq!(entry_type, "symlink");

    let symlink_target = response.headers().get("X-Symlink-Target")
        .expect("X-Symlink-Target header missing")
        .to_str().unwrap();
    assert_eq!(symlink_target, "/somewhere.txt");
}

// ---------------------------------------------------------------------------
// DELETE /engine/{*path} — symlink deletion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_symlink() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/file.txt", b"data");
    store_symlink(&engine, "/link", "/file.txt");

    // Delete the symlink
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("DELETE")
        .uri("/engine/link")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["deleted"], true);
    assert_eq!(json["type"], "symlink");

    // Original file should still be accessible
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/engine/file.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"data");
}

// ---------------------------------------------------------------------------
// Dangling symlink
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_dangling_symlink() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_symlink(&engine, "/link", "/nonexistent.txt");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/engine/link")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let json = body_json(response.into_body()).await;
    let error_msg = json["error"].as_str().unwrap_or("");
    assert!(
        error_msg.to_lowercase().contains("dangling"),
        "Expected 'Dangling' in error, got: {}",
        error_msg
    );
}

// ---------------------------------------------------------------------------
// Symlink to directory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_symlink_to_directory() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/stuff/a.txt", b"aaa");
    store_file(&engine, "/stuff/b.txt", b"bbb");
    store_symlink(&engine, "/shortcut", "/stuff");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/engine/shortcut")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let arr = json.as_array().expect("expected array listing");
    assert!(arr.len() >= 2, "Expected at least 2 entries in listing, got {}", arr.len());

    let names: Vec<&str> = arr.iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(names.contains(&"a.txt"), "Missing a.txt in listing: {:?}", names);
    assert!(names.contains(&"b.txt"), "Missing b.txt in listing: {:?}", names);
}

// ---------------------------------------------------------------------------
// Symlink chain: link1 -> link2 -> file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_symlink_chain() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/file.txt", b"chain-content");
    store_symlink(&engine, "/link2", "/file.txt");
    store_symlink(&engine, "/link1", "/link2");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/engine/link1")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"chain-content");
}

// ---------------------------------------------------------------------------
// Cyclic symlink
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_cyclic_symlink() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_symlink(&engine, "/a", "/b");
    store_symlink(&engine, "/b", "/a");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/engine/a")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = body_json(response.into_body()).await;
    let error_msg = json["error"].as_str().unwrap_or("");
    assert!(
        error_msg.to_lowercase().contains("cycle"),
        "Expected 'cycle' in error, got: {}",
        error_msg
    );
}

// ---------------------------------------------------------------------------
// Symlink in directory listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_symlink_in_listing() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/dir/real.txt", b"real");
    store_symlink(&engine, "/dir/link", "/dir/real.txt");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/engine/dir/")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let arr = json.as_array().expect("expected array listing");

    // Find the symlink entry
    let symlink_entry = arr.iter()
        .find(|e| e["name"].as_str() == Some("link"))
        .expect("symlink entry not found in listing");

    assert_eq!(symlink_entry["entry_type"], 8, "Expected entry_type 8 (Symlink)");
    assert_eq!(
        symlink_entry["target"].as_str().unwrap_or(""),
        "/dir/real.txt",
        "Expected target field in symlink listing entry"
    );
}

// ---------------------------------------------------------------------------
// Auth required
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_symlink_requires_auth() {
    let (app, _jwt_manager, _engine, _temp_dir) = test_app();

    let request = Request::builder()
        .method("POST")
        .uri("/engine-symlink/link.txt")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"target":"/data.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// DELETE symlink leaves other symlinks intact
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_symlink_does_not_affect_other_symlinks() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/target.txt", b"data");
    store_symlink(&engine, "/link1", "/target.txt");
    store_symlink(&engine, "/link2", "/target.txt");

    // Delete link1
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("DELETE")
        .uri("/engine/link1")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // link2 should still work
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/engine/link2")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"data");
}

// ---------------------------------------------------------------------------
// HEAD on non-symlink still works (regression check)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_head_file_still_works() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/normal.txt", b"hello");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("HEAD")
        .uri("/engine/normal.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let entry_type = response.headers().get("X-Entry-Type")
        .expect("X-Entry-Type header missing")
        .to_str().unwrap();
    assert_eq!(entry_type, "file");
}

// ---------------------------------------------------------------------------
// nofollow=false should follow (same as default)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_symlink_nofollow_false() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/file.txt", b"content");
    store_symlink(&engine, "/link", "/file.txt");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/engine/link?nofollow=false")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Should follow and return file content
    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"content");
}
