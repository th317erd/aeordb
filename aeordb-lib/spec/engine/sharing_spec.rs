use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::{
    CrudlifyOp, DirectoryOps, PathPermissions, PermissionLink,
    PermissionResolver, StorageEngine, Cache, GroupLoader,
};
use aeordb::engine::system_store;
use aeordb::engine::user::{ROOT_USER_ID, User};
use aeordb::engine::RequestContext;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_engine() -> (Arc<StorageEngine>, tempfile::TempDir) {
    create_temp_engine_for_tests()
}

fn create_test_user(engine: &StorageEngine, username: &str) -> Uuid {
    let ctx = RequestContext::system();
    let user = User::new(username, None);
    let user_id = user.user_id;
    system_store::store_user(engine, &ctx, &user).unwrap();
    user_id
}

fn write_permissions(engine: &StorageEngine, dir_path: &str, permissions: &PathPermissions) {
    let ctx = RequestContext::system();
    let directory_ops = DirectoryOps::new(engine);
    let perm_path = if dir_path == "/" || dir_path.ends_with('/') {
        format!("{}.aeordb-permissions", dir_path)
    } else {
        format!("{}/.aeordb-permissions", dir_path)
    };
    let data = permissions.serialize();
    directory_ops.store_file_buffered(&ctx, &perm_path, &data, Some("application/json")).unwrap();
}

fn member_link(group: &str, allow: &str, deny: &str) -> PermissionLink {
    PermissionLink {
        group: group.to_string(),
        allow: allow.to_string(),
        deny: deny.to_string(),
        others_allow: None,
        others_deny: None,
        path_pattern: None,
    }
}

fn member_link_with_pattern(group: &str, allow: &str, deny: &str, pattern: &str) -> PermissionLink {
    PermissionLink {
        group: group.to_string(),
        allow: allow.to_string(),
        deny: deny.to_string(),
        others_allow: None,
        others_deny: None,
        path_pattern: Some(pattern.to_string()),
    }
}

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
    let now = chrono::Utc::now().timestamp();
    let claims = TokenClaims {
        sub: ROOT_USER_ID.to_string(),
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

fn user_bearer_token(jwt_manager: &JwtManager, user_id: &Uuid) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = TokenClaims {
        sub: user_id.to_string(),
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
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

/// Helper to store a file via the engine for test setup.
fn store_test_file(engine: &StorageEngine, path: &str, content: &[u8]) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_file_buffered(&ctx, path, content, Some("application/octet-stream")).unwrap();
}

// ---------------------------------------------------------------------------
// Permission resolver tests (unit-level, no HTTP)
// ---------------------------------------------------------------------------

#[test]
fn path_pattern_scopes_to_specific_file() {
    let (engine, _temp_dir) = test_engine();
    let user_id = create_test_user(&engine, "alice");
    let user_group = format!("user:{}", user_id);

    // Store test files so the paths are valid
    store_test_file(&engine, "/photos/sunset.jpg", b"sunset data");
    store_test_file(&engine, "/photos/beach.jpg", b"beach data");

    // Write .permissions at /photos with a path_pattern scoped link
    let permissions = PathPermissions {
        links: vec![
            member_link_with_pattern(&user_group, "cr..l...", "........", "sunset.jpg"),
        ],
    };
    write_permissions(&engine, "/photos", &permissions);

    let group_cache = Cache::new(GroupLoader);
    let resolver = PermissionResolver::new(&engine, &group_cache);

    // User CAN read the matched file
    assert!(
        resolver.check_permission(&user_id, "/photos/sunset.jpg", CrudlifyOp::Read).unwrap(),
        "Should be able to read sunset.jpg with matching path_pattern"
    );
    assert!(
        resolver.check_permission(&user_id, "/photos/sunset.jpg", CrudlifyOp::Create).unwrap(),
        "Should be able to create sunset.jpg with matching path_pattern"
    );

    // User CANNOT read sibling file
    assert!(
        !resolver.check_permission(&user_id, "/photos/beach.jpg", CrudlifyOp::Read).unwrap(),
        "Should NOT be able to read beach.jpg -- pattern only matches sunset.jpg"
    );
}

#[test]
fn path_pattern_none_grants_directory_wide() {
    let (engine, _temp_dir) = test_engine();
    let user_id = create_test_user(&engine, "bob");
    let user_group = format!("user:{}", user_id);

    store_test_file(&engine, "/docs/readme.txt", b"readme");
    store_test_file(&engine, "/docs/notes.txt", b"notes");

    // Write .permissions at /docs with NO path_pattern (directory-wide)
    let permissions = PathPermissions {
        links: vec![
            member_link(&user_group, ".r..l...", "........"),
        ],
    };
    write_permissions(&engine, "/docs", &permissions);

    let group_cache = Cache::new(GroupLoader);
    let resolver = PermissionResolver::new(&engine, &group_cache);

    // User can read any file in /docs/
    assert!(
        resolver.check_permission(&user_id, "/docs/readme.txt", CrudlifyOp::Read).unwrap(),
        "Should be able to read readme.txt with directory-wide permission"
    );
    assert!(
        resolver.check_permission(&user_id, "/docs/notes.txt", CrudlifyOp::Read).unwrap(),
        "Should be able to read notes.txt with directory-wide permission"
    );
    assert!(
        resolver.check_permission(&user_id, "/docs/anything.txt", CrudlifyOp::Read).unwrap(),
        "Should be able to read any file in /docs/ with directory-wide permission"
    );
}

#[test]
fn multiple_patterns_at_same_level() {
    let (engine, _temp_dir) = test_engine();
    let user_id = create_test_user(&engine, "carol");
    let user_group = format!("user:{}", user_id);

    store_test_file(&engine, "/share/alpha.txt", b"alpha");
    store_test_file(&engine, "/share/beta.txt", b"beta");
    store_test_file(&engine, "/share/gamma.txt", b"gamma");

    // Two links with different path_patterns in the same .permissions
    let permissions = PathPermissions {
        links: vec![
            member_link_with_pattern(&user_group, ".r......", "........", "alpha.txt"),
            member_link_with_pattern(&user_group, "c.......", "........", "beta.txt"),
        ],
    };
    write_permissions(&engine, "/share", &permissions);

    let group_cache = Cache::new(GroupLoader);
    let resolver = PermissionResolver::new(&engine, &group_cache);

    // alpha.txt: read only
    assert!(resolver.check_permission(&user_id, "/share/alpha.txt", CrudlifyOp::Read).unwrap());
    assert!(!resolver.check_permission(&user_id, "/share/alpha.txt", CrudlifyOp::Create).unwrap());

    // beta.txt: create only
    assert!(resolver.check_permission(&user_id, "/share/beta.txt", CrudlifyOp::Create).unwrap());
    assert!(!resolver.check_permission(&user_id, "/share/beta.txt", CrudlifyOp::Read).unwrap());

    // gamma.txt: nothing
    assert!(!resolver.check_permission(&user_id, "/share/gamma.txt", CrudlifyOp::Read).unwrap());
    assert!(!resolver.check_permission(&user_id, "/share/gamma.txt", CrudlifyOp::Create).unwrap());
}

#[test]
fn pattern_link_plus_directory_wide_link_merge() {
    let (engine, _temp_dir) = test_engine();
    let user_id = create_test_user(&engine, "dave");
    let user_group = format!("user:{}", user_id);

    store_test_file(&engine, "/project/doc.txt", b"doc");
    store_test_file(&engine, "/project/config.yml", b"config");

    // Directory-wide read + pattern-specific write for config.yml
    let permissions = PathPermissions {
        links: vec![
            member_link(&user_group, ".r......", "........"),
            member_link_with_pattern(&user_group, "c.u.....", "........", "config.yml"),
        ],
    };
    write_permissions(&engine, "/project", &permissions);

    let group_cache = Cache::new(GroupLoader);
    let resolver = PermissionResolver::new(&engine, &group_cache);

    // Both files are readable (directory-wide)
    assert!(
        resolver.check_permission(&user_id, "/project/doc.txt", CrudlifyOp::Read).unwrap(),
        "doc.txt should be readable via directory-wide link"
    );
    assert!(
        resolver.check_permission(&user_id, "/project/config.yml", CrudlifyOp::Read).unwrap(),
        "config.yml should be readable via directory-wide link"
    );

    // Only config.yml is writable (pattern-specific)
    assert!(
        resolver.check_permission(&user_id, "/project/config.yml", CrudlifyOp::Create).unwrap(),
        "config.yml should be writable via pattern-specific link"
    );
    assert!(
        resolver.check_permission(&user_id, "/project/config.yml", CrudlifyOp::Update).unwrap(),
        "config.yml should be updatable via pattern-specific link"
    );
    assert!(
        !resolver.check_permission(&user_id, "/project/doc.txt", CrudlifyOp::Create).unwrap(),
        "doc.txt should NOT be writable -- no pattern-specific write link"
    );
    assert!(
        !resolver.check_permission(&user_id, "/project/doc.txt", CrudlifyOp::Update).unwrap(),
        "doc.txt should NOT be updatable"
    );
}

// ---------------------------------------------------------------------------
// HTTP endpoint tests (axum test client)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn share_endpoint_creates_permissions() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Upload a test file first
    let upload_req = Request::builder()
        .method("PUT")
        .uri("/files/photos/sunset.jpg")
        .header("content-type", "image/jpeg")
        .header("authorization", &auth)
        .body(Body::from("sunset data"))
        .unwrap();

    let resp = app.oneshot(upload_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create a user to share with
    let target_user_id = create_test_user(&engine, "share_target");

    // POST /files/share
    let app2 = rebuild_app(&jwt_manager, &engine);
    let share_body = serde_json::json!({
        "paths": ["/photos/sunset.jpg"],
        "users": [target_user_id.to_string()],
        "permissions": ".r..l..."
    });
    let share_req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
        .unwrap();

    let resp = app2.oneshot(share_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp.into_body()).await;
    assert_eq!(json["shared"], 1);

    // Verify the .permissions file was created with correct link
    let ops = DirectoryOps::new(&engine);
    let perm_data = ops.read_file_buffered("/photos/.aeordb-permissions").unwrap();
    let perms = PathPermissions::deserialize(&perm_data).unwrap();

    let expected_group = format!("user:{}", target_user_id);
    let matching_link = perms.links.iter().find(|l| l.group == expected_group);
    assert!(matching_link.is_some(), "Should have a link for the shared user");

    let link = matching_link.unwrap();
    assert_eq!(link.allow, ".r..l...");
    assert_eq!(link.path_pattern.as_deref(), Some("sunset.jpg"));
}

#[tokio::test]
async fn share_endpoint_updates_existing() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Upload file
    let upload_req = Request::builder()
        .method("PUT")
        .uri("/files/data/report.csv")
        .header("content-type", "text/csv")
        .header("authorization", &auth)
        .body(Body::from("csv data"))
        .unwrap();
    let resp = app.oneshot(upload_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let target_user_id = create_test_user(&engine, "update_target");

    // First share: read-only
    let app2 = rebuild_app(&jwt_manager, &engine);
    let share1 = serde_json::json!({
        "paths": ["/data/report.csv"],
        "users": [target_user_id.to_string()],
        "permissions": ".r..l..."
    });
    let req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&share1).unwrap()))
        .unwrap();
    let resp = app2.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Second share: update to read-write
    let app3 = rebuild_app(&jwt_manager, &engine);
    let share2 = serde_json::json!({
        "paths": ["/data/report.csv"],
        "users": [target_user_id.to_string()],
        "permissions": "cru.l..."
    });
    let req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&share2).unwrap()))
        .unwrap();
    let resp = app3.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify only ONE link exists (updated, not duplicated)
    let ops = DirectoryOps::new(&engine);
    let perm_data = ops.read_file_buffered("/data/.aeordb-permissions").unwrap();
    let perms = PathPermissions::deserialize(&perm_data).unwrap();

    let expected_group = format!("user:{}", target_user_id);
    let matching_links: Vec<_> = perms.links.iter()
        .filter(|l| l.group == expected_group && l.path_pattern.as_deref() == Some("report.csv"))
        .collect();

    assert_eq!(matching_links.len(), 1, "Should have exactly ONE link, not duplicated");
    assert_eq!(matching_links[0].allow, "cru.l...", "Permissions should be updated to read-write");
}

#[tokio::test]
async fn unshare_removes_link() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Upload file
    let upload_req = Request::builder()
        .method("PUT")
        .uri("/files/docs/secret.txt")
        .header("content-type", "text/plain")
        .header("authorization", &auth)
        .body(Body::from("secret"))
        .unwrap();
    let resp = app.oneshot(upload_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let target_user_id = create_test_user(&engine, "unshare_target");
    let expected_group = format!("user:{}", target_user_id);

    // Share first
    let app2 = rebuild_app(&jwt_manager, &engine);
    let share_body = serde_json::json!({
        "paths": ["/docs/secret.txt"],
        "users": [target_user_id.to_string()],
        "permissions": ".r..l..."
    });
    let req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
        .unwrap();
    let resp = app2.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Now unshare via DELETE /files/shares
    let app3 = rebuild_app(&jwt_manager, &engine);
    let unshare_body = serde_json::json!({
        "path": "/docs/secret.txt",
        "group": expected_group,
        "path_pattern": "secret.txt"
    });
    let req = Request::builder()
        .method("DELETE")
        .uri("/files/shares")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&unshare_body).unwrap()))
        .unwrap();
    let resp = app3.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp.into_body()).await;
    assert_eq!(json["revoked"], true);

    // Verify the link was removed from .permissions
    let ops = DirectoryOps::new(&engine);
    let perm_data = ops.read_file_buffered("/docs/.aeordb-permissions").unwrap();
    let perms = PathPermissions::deserialize(&perm_data).unwrap();

    let matching = perms.links.iter().find(|l| l.group == expected_group);
    assert!(matching.is_none(), "Link should have been removed after unshare");
}

#[tokio::test]
async fn list_shares_returns_current_state() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Upload file
    let upload_req = Request::builder()
        .method("PUT")
        .uri("/files/shared/file.txt")
        .header("content-type", "text/plain")
        .header("authorization", &auth)
        .body(Body::from("shared content"))
        .unwrap();
    let resp = app.oneshot(upload_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create user and share with user
    let user_id = create_test_user(&engine, "share_list_user");

    let app2 = rebuild_app(&jwt_manager, &engine);
    let share_body = serde_json::json!({
        "paths": ["/shared/file.txt"],
        "users": [user_id.to_string()],
        "groups": ["viewers"],
        "permissions": ".r..l..."
    });
    let req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
        .unwrap();
    let resp = app2.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET /files/shares?path=/shared/file.txt
    let app3 = rebuild_app(&jwt_manager, &engine);
    let req = Request::builder()
        .method("GET")
        .uri("/files/shares?path=/shared/file.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let resp = app3.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp.into_body()).await;
    let shares = json["shares"].as_array().unwrap();

    // Should have two shares: one for user, one for group
    assert_eq!(shares.len(), 2, "Should have shares for both user and group");

    // Find the user share and verify it has resolved username
    let user_group = format!("user:{}", user_id);
    let user_share = shares.iter().find(|s| s["group"] == user_group);
    assert!(user_share.is_some(), "Should include user share");
    assert_eq!(
        user_share.unwrap()["username"], "share_list_user",
        "Should resolve username for user:UUID groups"
    );

    // Find the group share
    let group_share = shares.iter().find(|s| s["group"] == "viewers");
    assert!(group_share.is_some(), "Should include group share");
    assert_eq!(group_share.unwrap()["allow"], ".r..l...");
}

#[tokio::test]
async fn per_file_share_does_not_grant_sibling_access() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Upload two files
    let upload1 = Request::builder()
        .method("PUT")
        .uri("/files/photos/sunset.jpg")
        .header("content-type", "image/jpeg")
        .header("authorization", &auth)
        .body(Body::from("sunset"))
        .unwrap();
    let resp = app.oneshot(upload1).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let app2 = rebuild_app(&jwt_manager, &engine);
    let upload2 = Request::builder()
        .method("PUT")
        .uri("/files/photos/beach.jpg")
        .header("content-type", "image/jpeg")
        .header("authorization", &auth)
        .body(Body::from("beach"))
        .unwrap();
    let resp = app2.oneshot(upload2).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create user and share only sunset.jpg
    let target_user_id = create_test_user(&engine, "file_scoped_user");

    let app3 = rebuild_app(&jwt_manager, &engine);
    let share_body = serde_json::json!({
        "paths": ["/photos/sunset.jpg"],
        "users": [target_user_id.to_string()],
        "permissions": ".r..l..."
    });
    let req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
        .unwrap();
    let resp = app3.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify with PermissionResolver: user CAN access sunset.jpg, CANNOT access beach.jpg
    let group_cache = Cache::new(GroupLoader);
    let resolver = PermissionResolver::new(&engine, &group_cache);

    assert!(
        resolver.check_permission(&target_user_id, "/photos/sunset.jpg", CrudlifyOp::Read).unwrap(),
        "Should be able to read shared file sunset.jpg"
    );
    assert!(
        !resolver.check_permission(&target_user_id, "/photos/beach.jpg", CrudlifyOp::Read).unwrap(),
        "Should NOT be able to read sibling file beach.jpg"
    );
}

#[tokio::test]
async fn share_requires_root() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Upload a file as root
    let upload_req = Request::builder()
        .method("PUT")
        .uri("/files/restricted/file.txt")
        .header("content-type", "text/plain")
        .header("authorization", &auth)
        .body(Body::from("data"))
        .unwrap();
    let resp = app.oneshot(upload_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create a non-root user
    let non_root_id = create_test_user(&engine, "non_root_sharer");
    let non_root_auth = user_bearer_token(&jwt_manager, &non_root_id);

    // Non-root user tries to share
    let app2 = rebuild_app(&jwt_manager, &engine);
    let share_body = serde_json::json!({
        "paths": ["/restricted/file.txt"],
        "users": [Uuid::new_v4().to_string()],
        "permissions": ".r..l..."
    });
    let req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &non_root_auth)
        .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
        .unwrap();
    let resp = app2.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "Non-root user should get 403");

    // The permission middleware blocks the request before the share handler runs,
    // so the error may be "Permission denied" rather than "Only root can share files".
    let json = body_json(resp.into_body()).await;
    let error_msg = json["error"].as_str().unwrap_or("");
    assert!(
        error_msg == "Permission denied" || error_msg == "Only root can share files",
        "Expected permission error, got: {}",
        error_msg
    );
}

#[tokio::test]
async fn unshare_nonexistent_returns_404() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Upload a file so the path exists
    let upload_req = Request::builder()
        .method("PUT")
        .uri("/files/empty/file.txt")
        .header("content-type", "text/plain")
        .header("authorization", &auth)
        .body(Body::from("data"))
        .unwrap();
    let resp = app.oneshot(upload_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Try to unshare a link that was never created (no .permissions file exists)
    let app2 = rebuild_app(&jwt_manager, &engine);
    let unshare_body = serde_json::json!({
        "path": "/empty/file.txt",
        "group": "user:00000000-0000-0000-0000-000000000099",
        "path_pattern": "file.txt"
    });
    let req = Request::builder()
        .method("DELETE")
        .uri("/files/shares")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&unshare_body).unwrap()))
        .unwrap();
    let resp = app2.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "Unsharing nonexistent link should return 404");
}

// ---------------------------------------------------------------------------
// Additional edge cases
// ---------------------------------------------------------------------------

#[test]
fn path_pattern_does_not_affect_subdirectory() {
    // A pattern at /photos should not apply to /photos/vacation/sunset.jpg
    let (engine, _temp_dir) = test_engine();
    let user_id = create_test_user(&engine, "subdir_test");
    let user_group = format!("user:{}", user_id);

    store_test_file(&engine, "/photos/sunset.jpg", b"sunset");
    store_test_file(&engine, "/photos/vacation/sunset.jpg", b"vacation sunset");

    let permissions = PathPermissions {
        links: vec![
            member_link_with_pattern(&user_group, ".r......", "........", "sunset.jpg"),
        ],
    };
    write_permissions(&engine, "/photos", &permissions);

    let group_cache = Cache::new(GroupLoader);
    let resolver = PermissionResolver::new(&engine, &group_cache);

    // Direct child matches
    assert!(resolver.check_permission(&user_id, "/photos/sunset.jpg", CrudlifyOp::Read).unwrap());
    // Nested subdirectory does NOT match (pattern is on /photos, not /photos/vacation)
    assert!(
        !resolver.check_permission(&user_id, "/photos/vacation/sunset.jpg", CrudlifyOp::Read).unwrap(),
        "Pattern at /photos should NOT grant access to /photos/vacation/sunset.jpg"
    );
}

#[tokio::test]
async fn share_with_empty_paths_returns_400() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let share_body = serde_json::json!({
        "paths": [],
        "users": ["some-user-id"],
        "permissions": ".r..l..."
    });
    let req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn share_with_no_users_or_groups_returns_400() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let share_body = serde_json::json!({
        "paths": ["/some/path"],
        "permissions": ".r..l..."
    });
    let req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn share_with_invalid_permissions_length_returns_400() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let share_body = serde_json::json!({
        "paths": ["/some/path"],
        "users": ["some-user"],
        "permissions": "rw"
    });
    let req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn share_nonexistent_path_returns_404() {
    let (app, jwt_manager, _engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let share_body = serde_json::json!({
        "paths": ["/nonexistent/file.txt"],
        "users": ["some-user"],
        "permissions": ".r..l..."
    });
    let req = Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unshare_requires_root() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    let non_root_id = create_test_user(&engine, "non_root_unsharer");
    let non_root_auth = user_bearer_token(&jwt_manager, &non_root_id);

    let app2 = rebuild_app(&jwt_manager, &engine);
    let unshare_body = serde_json::json!({
        "path": "/some/path",
        "group": "user:00000000-0000-0000-0000-000000000001"
    });
    let req = Request::builder()
        .method("DELETE")
        .uri("/files/shares")
        .header("content-type", "application/json")
        .header("authorization", &non_root_auth)
        .body(Body::from(serde_json::to_vec(&unshare_body).unwrap()))
        .unwrap();
    let resp = app2.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "Non-root should not be able to unshare");
}

#[tokio::test]
async fn list_shares_empty_returns_empty_array() {
    let (app, jwt_manager, engine, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Upload a file but do NOT share it
    let upload_req = Request::builder()
        .method("PUT")
        .uri("/files/unshared/file.txt")
        .header("content-type", "text/plain")
        .header("authorization", &auth)
        .body(Body::from("data"))
        .unwrap();
    let resp = app.oneshot(upload_req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // List shares -- should be empty
    let app2 = rebuild_app(&jwt_manager, &engine);
    let req = Request::builder()
        .method("GET")
        .uri("/files/shares?path=/unshared/file.txt")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();
    let resp = app2.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp.into_body()).await;
    let shares = json["shares"].as_array().unwrap();
    assert_eq!(shares.len(), 0, "Unshared file should have no shares");
}
