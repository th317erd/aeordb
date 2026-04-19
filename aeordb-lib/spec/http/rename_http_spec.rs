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
// 1. Rename file — basic rename within same directory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_file_basic() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/docs/readme.txt", b"hello world");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/docs/readme.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/docs/readme2.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["from"], "/docs/readme.txt");
    assert_eq!(json["to"], "/docs/readme2.txt");
    assert_eq!(json["entry_type"], "file");
}

// ---------------------------------------------------------------------------
// 2. Move file — cross-directory move
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_move_file_cross_directory() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/src/main.rs", b"fn main() {}");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/src/main.rs")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/archive/old_main.rs"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["from"], "/src/main.rs");
    assert_eq!(json["to"], "/archive/old_main.rs");
    assert_eq!(json["entry_type"], "file");
}

// ---------------------------------------------------------------------------
// 3. Rename preserves content — read from new path returns same data
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_file_preserves_content() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    let content = b"important data that must survive rename";
    store_file(&engine, "/data/original.bin", content);

    // Rename via HTTP
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/data/original.bin")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/data/renamed.bin"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Read from the new path and verify content
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/data/renamed.bin")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, content);
}

// ---------------------------------------------------------------------------
// 4. Rename old path returns 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_file_old_path_returns_404() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/file.txt", b"content");

    // Rename
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/file.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/moved.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Old path should 404
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/file.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// 5. Rename to existing destination returns 409
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_file_to_existing_returns_409() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/a.txt", b"file a");
    store_file(&engine, "/b.txt", b"file b");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/a.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/b.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// 6. Rename non-existent source returns 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_nonexistent_source_returns_404() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("PATCH")
        .uri("/files/does-not-exist.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/new.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// 7. Rename symlink — basic rename
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_symlink_basic() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/target.txt", b"target content");
    store_symlink(&engine, "/link", "/target.txt");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/link")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/renamed-link"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["from"], "/link");
    assert_eq!(json["to"], "/renamed-link");
    assert_eq!(json["entry_type"], "symlink");
}

// ---------------------------------------------------------------------------
// 8. Rename symlink preserves target
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_symlink_preserves_target() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/real-file.txt", b"the real data");
    store_symlink(&engine, "/old-link", "/real-file.txt");

    // Rename the symlink
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/old-link")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/new-link"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Read through the renamed symlink — should still resolve to the target
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/new-link")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"the real data");

    // Verify the symlink target header still points to the original target
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/new-link?nofollow=true")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["target"], "/real-file.txt");
}

// ---------------------------------------------------------------------------
// 9. Rename to same path returns 400
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_to_same_path_returns_400() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/same.txt", b"data");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/same.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/same.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = body_json(response.into_body()).await;
    let error_msg = json["error"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("same"),
        "Expected error mentioning 'same', got: {}",
        error_msg
    );
}

// ---------------------------------------------------------------------------
// 10. Rename across system boundary returns 403
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_across_system_boundary_returns_404() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    // All /.system/ paths are invisible via the API — renaming to a
    // .system/ destination returns 404, never revealing .system/ exists.
    store_file(&engine, "/user-file.txt", b"user data");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/user-file.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/.system/stolen.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    // System paths are invisible — returns 404 (not 400 or 403)
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// 11. Rename without auth returns 401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_without_auth_returns_401() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "/file.txt", b"data");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/file.txt")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"to":"/new.txt"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// 12. Rename with missing 'to' field returns 400
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_missing_to_field_returns_400() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("PATCH")
        .uri("/files/file.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// 13. Rename with empty 'to' field returns 400
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_empty_to_field_returns_400() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("PATCH")
        .uri("/files/file.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":""}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// 14. Rename symlink old path returns 404
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_symlink_old_path_returns_404() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_symlink(&engine, "/old-symlink", "/some-target");

    // Rename the symlink
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/old-symlink")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/new-symlink"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Old symlink path should 404 (with nofollow to check symlink metadata)
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/old-symlink?nofollow=true")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// 15. Rename file to existing symlink path returns 409
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_file_to_existing_symlink_returns_409() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/source.txt", b"source");
    store_symlink(&engine, "/occupied", "/some-target");

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/source.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/occupied"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

// ---------------------------------------------------------------------------
// 16. Rename preserves created_at timestamp
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_rename_file_preserves_created_at() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = bearer_token(&jwt_manager);

    store_file(&engine, "/ts-test.txt", b"timestamp test");

    // Get the original created_at from metadata (HEAD)
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("HEAD")
        .uri("/files/ts-test.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let original_created = response.headers().get("X-AeorDB-Created")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());

    // Rename
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PATCH")
        .uri("/files/ts-test.txt")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"/ts-renamed.txt"}"#))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Check renamed file's created_at matches original
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("HEAD")
        .uri("/files/ts-renamed.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let renamed_created = response.headers().get("X-AeorDB-Created")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());

    assert!(original_created.is_some(), "Expected X-AeorDB-Created header on original file");
    assert_eq!(original_created, renamed_created, "created_at should be preserved across rename");
}
