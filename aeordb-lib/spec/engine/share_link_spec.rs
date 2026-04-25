use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;
use chrono::Utc;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::api_key::NO_EXPIRY_SENTINEL;
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::engine::system_store;
use aeordb::engine::user::{ROOT_USER_ID, User};
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

// ===========================================================================
// Shared test infrastructure
// ===========================================================================

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
    (app, jwt_manager, engine, temp_dir)
}

fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
    create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

fn root_bearer_token(jwt_manager: &JwtManager) -> String {
    let now = Utc::now().timestamp();
    let claims = TokenClaims {
        sub: ROOT_USER_ID.to_string(),
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

fn user_bearer_token(jwt_manager: &JwtManager, user_id: &Uuid) -> String {
    let now = Utc::now().timestamp();
    let claims = TokenClaims {
        sub: user_id.to_string(),
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

fn store_file(engine: &StorageEngine, path: &str, content: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file(&ctx, path, content, None).unwrap();
}

fn create_test_user(engine: &StorageEngine, username: &str) -> Uuid {
    let ctx = RequestContext::system();
    let user = User::new(username, None);
    let user_id = user.user_id;
    system_store::store_user(engine, &ctx, &user).unwrap();
    user_id
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

/// Helper: create a share link via POST /files/share-link and return the parsed JSON response.
async fn create_share_link(
    app: axum::Router,
    auth: &str,
    paths: Vec<&str>,
    permissions: &str,
    expires_in_days: Option<i64>,
) -> (StatusCode, serde_json::Value) {
    let body = serde_json::json!({
        "paths": paths,
        "permissions": permissions,
        "expires_in_days": expires_in_days,
    });
    let request = Request::builder()
        .method("POST")
        .uri("/files/share-link")
        .header("authorization", auth)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let json = body_json(response.into_body()).await;
    (status, json)
}

// ===========================================================================
// Share link creation tests
// ===========================================================================

/// 1. POST /files/share-link with valid path and permissions returns url, token,
///    key_id, permissions, paths fields.
#[tokio::test]
async fn create_share_link_returns_url_and_token() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Upload a file so the path is valid
    store_file(&engine, "photos/hello.jpg", b"image data");

    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "cr..l...",
        Some(7),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "Expected 201, body: {}", json);
    assert!(json["url"].is_string(), "Response must include 'url'");
    assert!(json["token"].is_string(), "Response must include 'token'");
    assert!(json["key_id"].is_string(), "Response must include 'key_id'");
    assert_eq!(json["permissions"].as_str().unwrap(), "cr..l...");
    let paths = json["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].as_str().unwrap(), "/photos/");
}

/// 2. Create with expires_in_days: 7. Verify expires_at is ~7 days from now.
#[tokio::test]
async fn share_link_with_expiry() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    store_file(&engine, "photos/sunset.jpg", b"sunset data");

    let before = Utc::now().timestamp_millis();

    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "cr..l...",
        Some(7),
    )
    .await;

    let after = Utc::now().timestamp_millis();

    assert_eq!(status, StatusCode::CREATED);
    let expires_at = json["expires_at"].as_i64().unwrap();

    let seven_days_ms = 7 * 24 * 60 * 60 * 1000_i64;
    let expected_low = before + seven_days_ms;
    let expected_high = after + seven_days_ms;

    assert!(
        expires_at >= expected_low && expires_at <= expected_high,
        "expires_at {} should be ~7 days from now (range {}..{})",
        expires_at,
        expected_low,
        expected_high,
    );
}

/// 3. Create with expires_in_days: null. Verify expires_at equals NO_EXPIRY_SENTINEL.
#[tokio::test]
async fn share_link_no_expiry() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    store_file(&engine, "photos/beach.jpg", b"beach data");

    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "cr..l...",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let expires_at = json["expires_at"].as_i64().unwrap();
    assert_eq!(
        expires_at, NO_EXPIRY_SENTINEL,
        "Expected NO_EXPIRY_SENTINEL ({}) but got {}",
        NO_EXPIRY_SENTINEL, expires_at,
    );
}

/// 4. Non-root user tries to create a share link -- should get 403.
#[tokio::test]
async fn share_link_requires_root() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    let non_root_id = create_test_user(&engine, "non_root_sharer");
    let non_root_auth = user_bearer_token(&jwt_manager, &non_root_id);

    store_file(&engine, "photos/test.jpg", b"test data");

    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &non_root_auth,
        vec!["/photos/"],
        "cr..l...",
        Some(7),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "Non-root should get 403, body: {}", json);
}

// ===========================================================================
// Share link access tests
// ===========================================================================

/// 5. Create a share link for /photos/. Upload a file at /photos/test.jpg.
///    Use the returned JWT token in Authorization Bearer header to list /photos/.
#[tokio::test]
async fn share_link_token_grants_file_access() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    store_file(&engine, "photos/test.jpg", b"test image data");

    // Create share link
    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "cr..l...",
        Some(30),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let share_token = json["token"].as_str().unwrap();

    // Use the share token to access /files/photos/
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/photos/")
        .header("authorization", format!("Bearer {}", share_token))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Share token should grant access to /photos/"
    );

    let body = body_json(response.into_body()).await;
    // The listing should contain our test file
    let listing_str = serde_json::to_string(&body).unwrap();
    assert!(
        listing_str.contains("test.jpg"),
        "Directory listing should include test.jpg, got: {}",
        listing_str,
    );
}

/// 6. Create a share link for /photos/. Try to access /docs/readme.txt with the
///    share token -- should return 404 (not 403, because scoped key rules return 404).
#[tokio::test]
async fn share_link_token_denied_outside_scope() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    store_file(&engine, "photos/test.jpg", b"photo data");
    store_file(&engine, "docs/readme.txt", b"readme content");

    // Create share link scoped to /photos/
    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "cr..l...",
        Some(30),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let share_token = json["token"].as_str().unwrap();

    // Try to access /docs/readme.txt with the share token
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/docs/readme.txt")
        .header("authorization", format!("Bearer {}", share_token))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Access outside share scope should return 404, not 403"
    );
}

/// 7. Create a share link with read/list only permissions for /photos/.
///    Try to PUT /files/photos/new.jpg -- should fail (404) because write is denied.
///    Note: The permission engine treats '-' as denied and any other char as allowed.
///    So "-r--l---" means: read and list only.
#[tokio::test]
async fn share_link_respects_permission_level() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    store_file(&engine, "photos/existing.jpg", b"existing data");

    // Create share link with read+list only permissions (using '-' for denied slots)
    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "-r--l---",
        Some(30),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let share_token = json["token"].as_str().unwrap();

    // Verify read works
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/photos/existing.jpg")
        .header("authorization", format!("Bearer {}", share_token))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Read should be permitted with '-r--l---' permissions"
    );

    // Try to create a new file (PUT) -- 'c' is denied
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("PUT")
        .uri("/files/photos/new.jpg")
        .header("authorization", format!("Bearer {}", share_token))
        .header("content-type", "application/octet-stream")
        .body(Body::from("new data"))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "PUT (create) should be denied (404) because 'c' is not in '-r--l---' permissions"
    );

    // Try to delete -- 'd' is denied
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("DELETE")
        .uri("/files/photos/existing.jpg")
        .header("authorization", format!("Bearer {}", share_token))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "DELETE should be denied (404) because 'd' is not in '-r--l---' permissions"
    );
}

/// 8. Create a share link. Use ?token= query param instead of Authorization header.
#[tokio::test]
async fn token_in_query_param_works() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    store_file(&engine, "photos/query_test.jpg", b"query param test data");

    // Create share link
    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "cr..l...",
        Some(30),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let share_token = json["token"].as_str().unwrap();

    // Use ?token= query param, NO Authorization header
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri(format!("/files/photos/?token={}", share_token))
        // No authorization header
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Token in query param should grant access"
    );
}

// ===========================================================================
// Share link management tests
// ===========================================================================

/// 9. Create two share links for different paths. GET /files/share-links?path=/photos/
///    should return only the matching link.
#[tokio::test]
async fn list_share_links() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    store_file(&engine, "photos/a.jpg", b"photo a");
    store_file(&engine, "docs/b.txt", b"doc b");

    // Create share link for /photos/
    let app = rebuild_app(&jwt_manager, &engine);
    let (status, _) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "cr..l...",
        Some(30),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Create share link for /docs/
    let app = rebuild_app(&jwt_manager, &engine);
    let (status, _) = create_share_link(
        app,
        &auth,
        vec!["/docs/"],
        ".r..l...",
        Some(30),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // List share links filtered by /photos/
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/share-links?path=/photos/")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let links = json["links"].as_array().unwrap();

    assert_eq!(
        links.len(),
        1,
        "Filtering by /photos/ should return only the photos share link, got: {:?}",
        links
    );

    // Verify the returned link has the correct permissions
    let link = &links[0];
    assert_eq!(link["permissions"].as_str().unwrap(), "cr..l...");

    // Now list ALL share links (no path filter)
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/share-links")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let all_links = json["links"].as_array().unwrap();
    assert!(
        all_links.len() >= 2,
        "Unfiltered list should contain at least 2 share links, got {}",
        all_links.len()
    );
}

/// 10. Create a share link. DELETE /files/share-links/{key_id}.
///     Then use the token to access a file -- should fail (401 or 404).
#[tokio::test]
async fn revoke_share_link() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    store_file(&engine, "photos/revoke_test.jpg", b"revocable data");

    // Create share link
    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "cr..l...",
        Some(30),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let share_token = json["token"].as_str().unwrap().to_string();
    let key_id = json["key_id"].as_str().unwrap().to_string();

    // Verify the token works before revocation
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/photos/")
        .header("authorization", format!("Bearer {}", share_token))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Token should work before revocation"
    );

    // Revoke the share link
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/files/share-links/{}", key_id))
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "Revocation should succeed");

    let revoke_json = body_json(response.into_body()).await;
    assert_eq!(revoke_json["revoked"], true);

    // Now try to use the revoked token
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/photos/")
        .header("authorization", format!("Bearer {}", share_token))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert!(
        response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::NOT_FOUND,
        "Revoked token should return 401 or 404, got {}",
        response.status()
    );
}

/// 11. Create a share link for /photos/. Do NOT create any .permissions files.
///     Use the token to access /photos/ -- should succeed because share keys
///     bypass the permission resolver (key rules are the authority).
#[tokio::test]
async fn share_key_skips_permission_resolver() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Store a file but do NOT create any .permissions files
    store_file(&engine, "photos/no_perms.jpg", b"no permissions set");

    // Create share link
    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "cr..l...",
        Some(30),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let share_token = json["token"].as_str().unwrap();

    // Access using the share token -- should work even without .permissions files
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/photos/no_perms.jpg")
        .header("authorization", format!("Bearer {}", share_token))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Share key should bypass permission resolver and grant access without .permissions files"
    );
}

// ===========================================================================
// Edge cases and error handling
// ===========================================================================

/// Share link creation with empty paths should return 400.
#[tokio::test]
async fn share_link_empty_paths_returns_400() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let app = rebuild_app(&jwt_manager, &engine);
    let (status, _) = create_share_link(
        app,
        &auth,
        vec![],
        "cr..l...",
        Some(7),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Share link creation with invalid permissions length should return 400.
#[tokio::test]
async fn share_link_invalid_permissions_returns_400() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let app = rebuild_app(&jwt_manager, &engine);
    let (status, _) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "rw",
        Some(7),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Revoking a nonexistent share link should return 404.
#[tokio::test]
async fn revoke_nonexistent_share_link_returns_404() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let fake_key_id = Uuid::new_v4();

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/files/share-links/{}", fake_key_id))
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Revoking a share link with an invalid UUID should return 400.
#[tokio::test]
async fn revoke_share_link_invalid_uuid_returns_400() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("DELETE")
        .uri("/files/share-links/not-a-uuid")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Non-root user trying to revoke a share link should get 403.
#[tokio::test]
async fn revoke_share_link_requires_root() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    let non_root_id = create_test_user(&engine, "non_root_revoker");
    let non_root_auth = user_bearer_token(&jwt_manager, &non_root_id);

    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/files/share-links/{}", Uuid::new_v4()))
        .header("authorization", &non_root_auth)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    // The permission middleware will reject non-root before reaching the handler,
    // so this might be 403 from either the middleware or the handler.
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "Non-root should not be able to revoke share links"
    );
}

/// Verify that a share token read of a specific file returns the actual file content.
#[tokio::test]
async fn share_link_token_reads_file_content() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let file_content = b"This is the actual file content for verification.";
    store_file(&engine, "photos/content_check.txt", file_content);

    // Create share link
    let app = rebuild_app(&jwt_manager, &engine);
    let (status, json) = create_share_link(
        app,
        &auth,
        vec!["/photos/"],
        "cr..l...",
        Some(30),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let share_token = json["token"].as_str().unwrap();

    // Read the file using the share token
    let app = rebuild_app(&jwt_manager, &engine);
    let request = Request::builder()
        .method("GET")
        .uri("/files/photos/content_check.txt")
        .header("authorization", format!("Bearer {}", share_token))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    assert_eq!(body_bytes, file_content);
}
