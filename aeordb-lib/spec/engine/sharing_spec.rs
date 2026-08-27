use std::sync::{Arc, Barrier};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::{
  Cache, CrudlifyOp, DirectoryOps, GroupLoader, PathPermissions, PermissionLink, PermissionResolver, PermissionRevokeResult,
  PermissionStore, StorageEngine,
};
use aeordb::engine::system_store;
use aeordb::engine::user::{ROOT_USER_ID, User};
use aeordb::engine::RequestContext;
use aeordb::engine::memory_coordinator::MemoryPolicy;
use aeordb::server::{create_app_with_jwt_and_engine, create_app_with_jwt_engine_and_event_bus, create_temp_engine_for_tests};

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
  let permissions = PathPermissions { links: vec![member_link_with_pattern(&user_group, "cr..l...", "........", "sunset.jpg")] };
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
  let permissions = PathPermissions { links: vec![member_link(&user_group, ".r..l...", "........")] };
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
  assert!(!resolver.check_permission(&user_id, "/project/doc.txt", CrudlifyOp::Update).unwrap(), "doc.txt should NOT be updatable");
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
async fn share_endpoint_classifies_files_without_loading_content() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let target_user_id = create_test_user(&engine, "metadata_only_share_target");
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/archive/large.bin", b"payload", Some("application/octet-stream")).unwrap();
  let record = ops.get_metadata("/archive/large.bin").unwrap().expect("stored file metadata");
  engine.mark_entry_deleted(record.chunk_hashes.first().expect("stored file chunk")).unwrap();

  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(
          serde_json::to_vec(&serde_json::json!({
            "paths": ["/archive/large.bin"],
            "users": [target_user_id.to_string()],
            "permissions": ".r..l..."
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  let permissions = PathPermissions::deserialize(&ops.read_file_buffered("/archive/.aeordb-permissions").unwrap()).unwrap();
  assert!(permissions
    .links
    .iter()
    .any(|link| { link.group == format!("user:{target_user_id}") && link.path_pattern.as_deref() == Some("large.bin") }));
}

#[tokio::test]
async fn share_endpoint_rejects_malformed_permission_authority_without_overwriting_it() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let target_user_id = create_test_user(&engine, "malformed_permission_target");
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  let malformed = b"not permission JSON";
  ops.store_file_buffered(&ctx, "/docs/readme.txt", b"content", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/docs/.aeordb-permissions", malformed, Some("application/json")).unwrap();

  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(
          serde_json::to_vec(&serde_json::json!({
            "paths": ["/docs/readme.txt"],
            "users": [target_user_id.to_string()],
            "permissions": ".r..l..."
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
  assert_eq!(ops.read_file_buffered("/docs/.aeordb-permissions").unwrap(), malformed);
}

#[tokio::test]
async fn multi_path_share_publishes_one_authority_transition() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let target_user_id = create_test_user(&engine, "atomic_multi_share_target");
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/alpha/one.txt", b"one", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/beta/two.txt", b"two", Some("text/plain")).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(
          serde_json::to_vec(&serde_json::json!({
            "paths": ["/alpha/one.txt", "/beta/two.txt"],
            "users": [target_user_id.to_string()],
            "permissions": ".r..l..."
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + 1);
  for path in ["/alpha/.aeordb-permissions", "/beta/.aeordb-permissions"] {
    let permissions = PathPermissions::deserialize(&ops.read_file_buffered(path).unwrap()).unwrap();
    assert!(permissions.links.iter().any(|link| link.group == format!("user:{target_user_id}")));
  }
}

#[tokio::test]
async fn multi_path_share_rejects_later_corrupt_authority_without_partial_publication() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let target_user_id = create_test_user(&engine, "atomic_corrupt_share_target");
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  let malformed = b"not permission JSON";
  ops.store_file_buffered(&ctx, "/alpha/one.txt", b"one", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/beta/two.txt", b"two", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/beta/.aeordb-permissions", malformed, Some("application/json")).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(
          serde_json::to_vec(&serde_json::json!({
            "paths": ["/alpha/one.txt", "/beta/two.txt"],
            "users": [target_user_id.to_string()],
            "permissions": ".r..l..."
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(matches!(ops.read_file_buffered("/alpha/.aeordb-permissions"), Err(aeordb::engine::EngineError::NotFound(_))));
  assert_eq!(ops.read_file_buffered("/beta/.aeordb-permissions").unwrap(), malformed);
}

#[tokio::test]
async fn multi_path_share_rejects_later_missing_path_without_partial_publication() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let target_user_id = create_test_user(&engine, "atomic_missing_share_target");
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&RequestContext::system(), "/alpha/one.txt", b"one", Some("text/plain")).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(
          serde_json::to_vec(&serde_json::json!({
            "paths": ["/alpha/one.txt", "/missing/two.txt"],
            "users": [target_user_id.to_string()],
            "permissions": ".r..l..."
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::NOT_FOUND);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(matches!(ops.read_file_buffered("/alpha/.aeordb-permissions"), Err(aeordb::engine::EngineError::NotFound(_))));
}

#[tokio::test]
async fn identical_share_retry_is_a_zero_sequence_noop() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let target_user_id = create_test_user(&engine, "idempotent_share_target");
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), "/docs/readme.txt", b"readme", Some("text/plain")).unwrap();
  let body = serde_json::to_vec(&serde_json::json!({
    "paths": ["/docs/readme.txt"],
    "users": [target_user_id.to_string()],
    "permissions": ".r..l..."
  }))
  .unwrap();

  let first = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(body.clone()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(first.status(), StatusCode::OK);
  let sequence_after_first = engine.durability_snapshot().unwrap().next_sequence;

  let retry = rebuild_app(&jwt_manager, &engine)
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(body))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(retry.status(), StatusCode::OK);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_after_first);
}

#[test]
fn permission_store_reports_only_paths_changed_by_a_mixed_retry() {
  let (engine, _temp_dir) = test_engine();
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/alpha/one.txt", b"one", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/beta/two.txt", b"two", Some("text/plain")).unwrap();
  PermissionStore::new(&engine)
    .grant_paths(&ctx, vec!["/alpha/one.txt".to_string()], vec!["team".to_string()], ".r..l...".to_string())
    .unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let result = PermissionStore::new(&engine)
    .grant_paths(&ctx, vec!["/alpha/one.txt".to_string(), "/beta/two.txt".to_string()], vec!["team".to_string()], ".r..l...".to_string())
    .unwrap();

  assert_eq!(result.paths, vec!["/alpha/one.txt", "/beta/two.txt"]);
  assert_eq!(result.changed_paths, vec!["/beta/two.txt"]);
  assert_eq!(result.changed_permission_files, 1);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + 1);
}

#[test]
fn permission_store_deduplicates_normalized_paths_and_groups() {
  let (engine, _temp_dir) = test_engine();
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/docs/readme.txt", b"readme", Some("text/plain")).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let result = PermissionStore::new(&engine)
    .grant_paths(
      &ctx,
      vec!["docs/readme.txt".to_string(), "/docs/readme.txt".to_string()],
      vec!["team".to_string(), "team".to_string()],
      ".r..l...".to_string(),
    )
    .unwrap();

  assert_eq!(result.paths, vec!["/docs/readme.txt"]);
  assert_eq!(result.changed_paths, vec!["/docs/readme.txt"]);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + 1);
  let permissions = PathPermissions::deserialize(&ops.read_file_buffered("/docs/.aeordb-permissions").unwrap()).unwrap();
  assert_eq!(permissions.links.len(), 1);
}

#[test]
fn permission_store_rejects_output_above_document_limit_without_publication() {
  const MAX_PERMISSION_BYTES: usize = 1024 * 1024;

  let (engine, _temp_dir) = test_engine();
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/full/file.txt", b"data", Some("text/plain")).unwrap();
  let mut link = member_link_with_pattern("", ".r..l...", "........", "file.txt");
  let fixed_size = PathPermissions { links: vec![link.clone()] }.serialize().len();
  link.group = "x".repeat(MAX_PERMISSION_BYTES - fixed_size);
  let at_limit = PathPermissions { links: vec![link] }.serialize();
  assert_eq!(at_limit.len(), MAX_PERMISSION_BYTES);
  ops.store_file_buffered(&ctx, "/full/.aeordb-permissions", &at_limit, Some("application/json")).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let error = PermissionStore::new(&engine)
    .grant_paths(&ctx, vec!["/full/file.txt".to_string()], vec!["new-team".to_string()], ".r..l...".to_string())
    .expect_err("permission output above the fixed document limit must fail");

  assert!(matches!(error, aeordb::engine::EngineError::InvalidInput(message) if message.contains("would be")));
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert_eq!(ops.read_file_buffered("/full/.aeordb-permissions").unwrap(), at_limit);
}

#[test]
fn permission_store_memory_refusal_publishes_nothing() {
  let (engine, _temp_dir) = test_engine();
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/pressure/file.txt", b"data", Some("text/plain")).unwrap();
  engine.memory_coordinator().reconfigure_policy(MemoryPolicy::new(512 * 1024, 1024 * 1024, 1, 256 * 1024).unwrap()).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let error = PermissionStore::new(&engine)
    .grant_paths(&ctx, vec!["/pressure/file.txt".to_string()], vec!["team".to_string()], ".r..l...".to_string())
    .expect_err("permission batch must honor process memory refusal");

  assert!(matches!(error, aeordb::engine::EngineError::ResourceExhausted(_)));
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(matches!(ops.read_file_buffered("/pressure/.aeordb-permissions"), Err(aeordb::engine::EngineError::NotFound(_))));
}

#[tokio::test]
async fn share_notifications_report_only_acknowledged_changed_paths() {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let auth = root_bearer_token(&jwt_manager);
  let target_user_id = create_test_user(&engine, "event_share_target");
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/alpha/one.txt", b"one", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/beta/two.txt", b"two", Some("text/plain")).unwrap();
  PermissionStore::new(&engine)
    .grant_paths(&ctx, vec!["/alpha/one.txt".to_string()], vec![format!("user:{target_user_id}")], ".r..l...".to_string())
    .unwrap();
  let (app, event_bus) = create_app_with_jwt_engine_and_event_bus(Arc::clone(&jwt_manager), Arc::clone(&engine));
  let mut events = event_bus.subscribe();
  let body = serde_json::to_vec(&serde_json::json!({
    "paths": ["/alpha/one.txt", "/beta/two.txt"],
    "users": [target_user_id.to_string(), target_user_id.to_string()],
    "permissions": ".r..l..."
  }))
  .unwrap();

  let response = app
    .clone()
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(body.clone()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let event = tokio::time::timeout(Duration::from_secs(1), events.recv()).await.expect("missing changed-path notification").unwrap();
  assert_eq!(event.event_type, aeordb::engine::engine_event::EVENT_FILES_SHARED);
  assert_eq!(event.recipient_user_id.as_deref(), Some(target_user_id.to_string().as_str()));
  assert_eq!(event.payload["path"], "/beta/two.txt");
  assert!(tokio::time::timeout(Duration::from_millis(50), events.recv()).await.is_err(), "unchanged path emitted a notification");

  let retry = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(body))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(retry.status(), StatusCode::OK);
  assert!(tokio::time::timeout(Duration::from_millis(50), events.recv()).await.is_err(), "idempotent retry emitted a notification");
}

#[tokio::test]
async fn unshare_emits_one_acknowledged_group_recipient_notification_without_audience_leakage() {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let auth = root_bearer_token(&jwt_manager);
  let target_user_id = create_test_user(&engine, "event_unshare_target");
  let target_group = format!("user:{target_user_id}");
  let path = "/shared/unshared.txt";
  let ctx = RequestContext::system();
  DirectoryOps::new(&engine).store_file_buffered(&ctx, path, b"shared", Some("text/plain")).unwrap();
  PermissionStore::new(&engine).grant_paths(&ctx, vec![path.to_string()], vec![target_group.clone()], ".r..l...".to_string()).unwrap();
  let (app, event_bus) = create_app_with_jwt_engine_and_event_bus(Arc::clone(&jwt_manager), Arc::clone(&engine));
  let mut events = event_bus.subscribe();
  let request_body = serde_json::to_vec(&serde_json::json!({
    "path": path,
    "group": target_group,
    "path_pattern": "unshared.txt"
  }))
  .unwrap();

  let response = app
    .clone()
    .oneshot(
      Request::builder()
        .method("DELETE")
        .uri("/files/shares")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(request_body.clone()))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let event = tokio::time::timeout(Duration::from_secs(1), events.recv()).await.expect("missing unshare notification").unwrap();
  assert_eq!(event.event_type, aeordb::engine::EVENT_FILES_UNSHARED);
  assert_eq!(event.recipient_groups(), Some([format!("user:{target_user_id}")].as_slice()));
  assert_eq!(event.payload, serde_json::json!({"path": path, "action": "refresh"}));
  assert!(serde_json::to_value(&event).unwrap().get("recipient_groups").is_none());

  let retry = app
    .oneshot(
      Request::builder()
        .method("DELETE")
        .uri("/files/shares")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(request_body))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(retry.status(), StatusCode::NOT_FOUND);
  assert!(tokio::time::timeout(Duration::from_millis(100), events.recv()).await.is_err(), "failed unshare retry emitted an event");
}

#[tokio::test]
async fn failed_compound_share_emits_no_recipient_notification() {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let auth = root_bearer_token(&jwt_manager);
  let target_user_id = create_test_user(&engine, "failed_event_share_target");
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/alpha/one.txt", b"one", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/beta/two.txt", b"two", Some("text/plain")).unwrap();
  ops.store_file_buffered(&ctx, "/beta/.aeordb-permissions", b"not permission JSON", Some("application/json")).unwrap();
  let (app, event_bus) = create_app_with_jwt_engine_and_event_bus(jwt_manager, engine);
  let mut events = event_bus.subscribe();

  let response = app
    .oneshot(
      Request::builder()
        .method("POST")
        .uri("/files/share")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(
          serde_json::to_vec(&serde_json::json!({
            "paths": ["/alpha/one.txt", "/beta/two.txt"],
            "users": [target_user_id.to_string()],
            "permissions": ".r..l..."
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
  assert!(tokio::time::timeout(Duration::from_millis(50), events.recv()).await.is_err(), "failed share emitted a notification");
}

#[test]
fn concurrent_permission_grants_preserve_every_group() {
  let (engine, _temp_dir) = test_engine();
  DirectoryOps::new(&engine).store_file_buffered(&RequestContext::system(), "/race/shared.txt", b"shared", Some("text/plain")).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let barrier = Arc::new(Barrier::new(17));
  let mut workers = Vec::new();

  for index in 0..16 {
    let worker_engine = Arc::clone(&engine);
    let worker_barrier = Arc::clone(&barrier);
    workers.push(std::thread::spawn(move || {
      worker_barrier.wait();
      PermissionStore::new(&worker_engine).grant_paths(
        &RequestContext::system(),
        vec!["/race/shared.txt".to_string()],
        vec![format!("team-{index}")],
        ".r..l...".to_string(),
      )
    }));
  }
  barrier.wait();
  for worker in workers {
    worker.join().expect("grant worker panicked").expect("concurrent grant failed");
  }

  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + 16);
  let permissions =
    PathPermissions::deserialize(&DirectoryOps::new(&engine).read_file_buffered("/race/.aeordb-permissions").unwrap()).unwrap();
  assert_eq!(permissions.links.len(), 16);
  for index in 0..16 {
    assert!(permissions.links.iter().any(|link| link.group == format!("team-{index}")));
  }
}

#[test]
fn concurrent_permission_grant_and_revoke_preserve_commuting_updates() {
  let (engine, _temp_dir) = test_engine();
  let ops = DirectoryOps::new(&engine);
  ops.store_file_buffered(&RequestContext::system(), "/race/shared.txt", b"shared", Some("text/plain")).unwrap();
  write_permissions(
    &engine,
    "/race",
    &PathPermissions {
      links: vec![
        member_link_with_pattern("remove-me", ".r..l...", "........", "shared.txt"),
        member_link_with_pattern("preserve-me", ".r..l...", "........", "shared.txt"),
      ],
    },
  );
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let barrier = Arc::new(Barrier::new(3));

  let grant_engine = Arc::clone(&engine);
  let grant_barrier = Arc::clone(&barrier);
  let grant = std::thread::spawn(move || {
    grant_barrier.wait();
    PermissionStore::new(&grant_engine).grant_paths(
      &RequestContext::system(),
      vec!["/race/shared.txt".to_string()],
      vec!["add-me".to_string()],
      ".r..l...".to_string(),
    )
  });
  let revoke_engine = Arc::clone(&engine);
  let revoke_barrier = Arc::clone(&barrier);
  let revoke = std::thread::spawn(move || {
    revoke_barrier.wait();
    PermissionStore::new(&revoke_engine).revoke_path(&RequestContext::system(), "/race/shared.txt", "remove-me", Some("shared.txt"))
  });
  barrier.wait();

  grant.join().expect("grant worker panicked").expect("grant failed");
  assert_eq!(revoke.join().expect("revoke worker panicked").expect("revoke failed"), PermissionRevokeResult::Revoked);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + 2);

  let permissions = PathPermissions::deserialize(&ops.read_file_buffered("/race/.aeordb-permissions").unwrap()).unwrap();
  assert!(permissions.links.iter().any(|link| link.group == "preserve-me"));
  assert!(permissions.links.iter().any(|link| link.group == "add-me"));
  assert!(!permissions.links.iter().any(|link| link.group == "remove-me"));
}

#[test]
fn permission_store_grant_and_revoke_survive_clean_restarts() {
  let temporary = tempfile::tempdir().unwrap();
  let database_path = temporary.path().join("permission-store-restart.aeordb");
  let database_path = database_path.to_str().unwrap();
  let ctx = RequestContext::system();

  {
    let engine = StorageEngine::create(database_path).unwrap();
    let ops = DirectoryOps::new(&engine);
    ops.ensure_root_directory(&ctx).unwrap();
    ops.store_file_buffered(&ctx, "/alpha/one.txt", b"one", Some("text/plain")).unwrap();
    ops.store_file_buffered(&ctx, "/beta/two.txt", b"two", Some("text/plain")).unwrap();
    PermissionStore::new(&engine)
      .grant_paths(
        &ctx,
        vec!["/alpha/one.txt".to_string(), "/beta/two.txt".to_string()],
        vec!["restart-team".to_string()],
        ".r..l...".to_string(),
      )
      .unwrap();
    engine.shutdown().unwrap();
  }

  {
    let engine = StorageEngine::open(database_path).unwrap();
    let ops = DirectoryOps::new(&engine);
    for path in ["/alpha/.aeordb-permissions", "/beta/.aeordb-permissions"] {
      let permissions = PathPermissions::deserialize(&ops.read_file_buffered(path).unwrap()).unwrap();
      assert!(permissions.links.iter().any(|link| link.group == "restart-team"));
    }
    assert_eq!(
      PermissionStore::new(&engine).revoke_path(&ctx, "/alpha/one.txt", "restart-team", Some("one.txt")).unwrap(),
      PermissionRevokeResult::Revoked
    );
    engine.shutdown().unwrap();
  }

  let engine = StorageEngine::open(database_path).unwrap();
  let ops = DirectoryOps::new(&engine);
  let alpha = PathPermissions::deserialize(&ops.read_file_buffered("/alpha/.aeordb-permissions").unwrap()).unwrap();
  let beta = PathPermissions::deserialize(&ops.read_file_buffered("/beta/.aeordb-permissions").unwrap()).unwrap();
  assert!(!alpha.links.iter().any(|link| link.group == "restart-team"));
  assert!(beta.links.iter().any(|link| link.group == "restart-team"));
  engine.shutdown().unwrap();
}

#[tokio::test]
async fn shared_with_me_surfaces_malformed_permission_authority() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let user_id = create_test_user(&engine, "malformed_grants_user");
  DirectoryOps::new(&engine)
    .store_file_buffered(&RequestContext::system(), "/docs/.aeordb-permissions", b"not permission JSON", Some("application/json"))
    .unwrap();

  let response = app
    .oneshot(
      Request::get("/files/shared-with-me").header("authorization", user_bearer_token(&jwt_manager, &user_id)).body(Body::empty()).unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
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
  let matching_links: Vec<_> =
    perms.links.iter().filter(|l| l.group == expected_group && l.path_pattern.as_deref() == Some("report.csv")).collect();

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
  let req =
    Request::builder().method("GET").uri("/files/shares?path=/shared/file.txt").header("authorization", &auth).body(Body::empty()).unwrap();
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
  assert_eq!(user_share.unwrap()["username"], "share_list_user", "Should resolve username for user:UUID groups");

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
  assert!(error_msg == "Permission denied" || error_msg == "Only root can share files", "Expected permission error, got: {}", error_msg);
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

#[tokio::test]
async fn unshare_rejects_corrupt_permission_authority_without_publication() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let ops = DirectoryOps::new(&engine);
  let malformed = b"not permission JSON";
  ops.store_file_buffered(&RequestContext::system(), "/broken/file.txt", b"data", Some("text/plain")).unwrap();
  ops.store_file_buffered(&RequestContext::system(), "/broken/.aeordb-permissions", malformed, Some("application/json")).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let response = app
    .oneshot(
      Request::builder()
        .method("DELETE")
        .uri("/files/shares")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(
          serde_json::to_vec(&serde_json::json!({
            "path": "/broken/file.txt",
            "group": "remove-me",
            "path_pattern": "file.txt"
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert_eq!(ops.read_file_buffered("/broken/.aeordb-permissions").unwrap(), malformed);
}

#[tokio::test]
async fn unshare_file_specific_link_after_file_deletion() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let ops = DirectoryOps::new(&engine);
  let ctx = RequestContext::system();
  ops.store_file_buffered(&ctx, "/deleted/file.txt", b"data", Some("text/plain")).unwrap();
  PermissionStore::new(&engine)
    .grant_paths(&ctx, vec!["/deleted/file.txt".to_string()], vec!["remove-me".to_string()], ".r..l...".to_string())
    .unwrap();
  ops.delete_file(&ctx, "/deleted/file.txt").unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let response = app
    .oneshot(
      Request::builder()
        .method("DELETE")
        .uri("/files/shares")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(
          serde_json::to_vec(&serde_json::json!({
            "path": "/deleted/file.txt",
            "group": "remove-me",
            "path_pattern": "file.txt"
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before + 1);
  let permissions = PathPermissions::deserialize(&ops.read_file_buffered("/deleted/.aeordb-permissions").unwrap()).unwrap();
  assert!(permissions.links.is_empty());
}

#[tokio::test]
async fn unshare_rejects_oversized_permission_authority_without_publication() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let ops = DirectoryOps::new(&engine);
  let oversized = vec![b'x'; 1024 * 1024 + 1];
  ops.store_file_buffered(&RequestContext::system(), "/oversized/file.txt", b"data", Some("text/plain")).unwrap();
  ops.store_file_buffered(&RequestContext::system(), "/oversized/.aeordb-permissions", &oversized, Some("application/json")).unwrap();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;

  let response = app
    .oneshot(
      Request::builder()
        .method("DELETE")
        .uri("/files/shares")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(
          serde_json::to_vec(&serde_json::json!({
            "path": "/oversized/file.txt",
            "group": "remove-me",
            "path_pattern": "file.txt"
          }))
          .unwrap(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();

  assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert_eq!(ops.read_file_buffered("/oversized/.aeordb-permissions").unwrap(), oversized);
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

  let permissions = PathPermissions { links: vec![member_link_with_pattern(&user_group, ".r......", "........", "sunset.jpg")] };
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
async fn share_with_positionally_invalid_permissions_returns_400_without_publication() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let share_body = serde_json::json!({
      "paths": ["/some/path"],
      "users": ["some-user"],
      "permissions": "r......."
  });
  let request = Request::builder()
    .method("POST")
    .uri("/files/share")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
}

#[tokio::test]
async fn share_rejects_over_expanded_request_without_publication() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let paths = (0..257).map(|index| format!("/missing/path-{index}")).collect::<Vec<_>>();
  let groups = (0..256).map(|index| format!("group-{index}")).collect::<Vec<_>>();
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let request = Request::builder()
    .method("POST")
    .uri("/files/share")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(
      serde_json::to_vec(&serde_json::json!({
        "paths": paths,
        "groups": groups,
        "permissions": ".r..l..."
      }))
      .unwrap(),
    ))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
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
async fn share_rejects_nested_registry_concealed_path() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  store_test_file(&engine, "/docs/.aeordb-indexes/private.idx", b"derived index");
  let target_user = create_test_user(&engine, "concealed_share_target");
  let sequence_before = engine.durability_snapshot().unwrap().next_sequence;
  let share_body = serde_json::json!({
      "paths": ["/docs/.aeordb-indexes/private.idx"],
      "users": [target_user.to_string()],
      "permissions": ".r..l..."
  });
  let request = Request::builder()
    .method("POST")
    .uri("/files/share")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
  assert_eq!(engine.durability_snapshot().unwrap().next_sequence, sequence_before);
  assert!(DirectoryOps::new(&engine).read_file_buffered("/docs/.aeordb-permissions").is_err());
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

// ---------------------------------------------------------------------------
// Ancestor-navigation listing tests — users with deep shares can walk down
// from the root via GET /files/{ancestor}/ instead of being forced to use
// /files/shared-with-me.
// ---------------------------------------------------------------------------

/// Share a file (or directory) by attaching a user-group permission link to
/// the directory's `.aeordb-permissions`. Path_pattern is set when sharing
/// a single file.
fn share_directory_with_user(engine: &StorageEngine, dir_path: &str, user_id: &Uuid, allow: &str, pattern: Option<&str>) {
  let group = format!("user:{}", user_id);
  let link = match pattern {
    Some(p) => member_link_with_pattern(&group, allow, "........", p),
    None => member_link(&group, allow, "........"),
  };
  let perms = PathPermissions { links: vec![link] };
  write_permissions(engine, dir_path, &perms);
}

async fn get_items(app: axum::Router, uri: &str, auth: &str) -> serde_json::Value {
  let req = Request::builder().method("GET").uri(uri).header("authorization", auth).body(Body::empty()).unwrap();
  let resp = app.oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK, "GET {} should succeed for user with descendant grants", uri);
  body_json(resp.into_body()).await
}

fn item_names(listing: &serde_json::Value) -> Vec<String> {
  let mut names: Vec<String> = listing["items"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap_or("").to_string()).collect();
  names.sort();
  names
}

#[tokio::test]
async fn user_with_deep_share_can_walk_root_to_target() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);

  // Build /Pictures/Family/Harlo with a file inside, plus a sibling
  // directory /Pictures/Vacation the user must NOT see.
  let upload = |path: &str| {
    Request::builder()
      .method("PUT")
      .uri(path)
      .header("content-type", "text/plain")
      .header("authorization", &auth_root)
      .body(Body::from("payload"))
      .unwrap()
  };
  for path in ["/files/Pictures/Family/Harlo/photo.jpg", "/files/Pictures/Vacation/beach.jpg", "/files/Documents/private.pdf"] {
    let resp = rebuild_app(&jwt_manager, &engine).oneshot(upload(path)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
  }
  drop(app);

  // Share /Pictures/Family/Harlo with a non-root user.
  let user_id = create_test_user(&engine, "harlo_viewer");
  share_directory_with_user(&engine, "/Pictures/Family/Harlo", &user_id, ".r..l...", None);

  let auth = user_bearer_token(&jwt_manager, &user_id);

  // GET / — user should see only Pictures, not Documents.
  let root = get_items(rebuild_app(&jwt_manager, &engine), "/files/", &auth).await;
  assert_eq!(item_names(&root), vec!["Pictures".to_string()], "Root listing should expose only the ancestor of the share");

  // GET /Pictures — only Family, not Vacation.
  let pics = get_items(rebuild_app(&jwt_manager, &engine), "/files/Pictures/", &auth).await;
  assert_eq!(item_names(&pics), vec!["Family".to_string()], "/Pictures listing should expose only Family, not Vacation");

  // GET /Pictures/Family — only Harlo.
  let fam = get_items(rebuild_app(&jwt_manager, &engine), "/files/Pictures/Family/", &auth).await;
  assert_eq!(item_names(&fam), vec!["Harlo".to_string()]);

  // GET /Pictures/Family/Harlo — full direct listing (the shared dir).
  // The .aeordb-permissions metadata file is exposed by the listing
  // endpoint today (not scrubbed); just assert photo.jpg is present.
  let harlo = get_items(rebuild_app(&jwt_manager, &engine), "/files/Pictures/Family/Harlo/", &auth).await;
  let names = item_names(&harlo);
  assert!(names.contains(&"photo.jpg".to_string()), "Shared-directory listing must include the actual content");
}

#[tokio::test]
async fn file_pattern_share_visible_in_parent_listing() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);

  for path in ["/files/Documents/tax-2025.pdf", "/files/Documents/secret.pdf"] {
    let req = Request::builder()
      .method("PUT")
      .uri(path)
      .header("content-type", "application/pdf")
      .header("authorization", &auth_root)
      .body(Body::from("data"))
      .unwrap();
    let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
  }
  drop(app);

  let user_id = create_test_user(&engine, "tax_recipient");
  share_directory_with_user(&engine, "/Documents", &user_id, ".r......", Some("tax-2025.pdf"));

  let auth = user_bearer_token(&jwt_manager, &user_id);

  // Listing of /Documents should show only the shared filename, not
  // sibling secret.pdf.
  let docs = get_items(rebuild_app(&jwt_manager, &engine), "/files/Documents/", &auth).await;
  assert_eq!(item_names(&docs), vec!["tax-2025.pdf".to_string()], "Only the file-pattern-shared filename should be visible");

  // Root listing should still expose /Documents as a navigable ancestor.
  let root = get_items(rebuild_app(&jwt_manager, &engine), "/files/", &auth).await;
  assert_eq!(item_names(&root), vec!["Documents".to_string()]);
}

#[tokio::test]
async fn user_without_any_grants_still_403s_on_root() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);

  let upload = Request::builder()
    .method("PUT")
    .uri("/files/Pictures/photo.jpg")
    .header("content-type", "image/jpeg")
    .header("authorization", &auth_root)
    .body(Body::from("data"))
    .unwrap();
  let resp = app.oneshot(upload).await.unwrap();
  assert_eq!(resp.status(), StatusCode::CREATED);

  let user_id = create_test_user(&engine, "no_shares");
  let auth = user_bearer_token(&jwt_manager, &user_id);

  let req = Request::builder().method("GET").uri("/files/").header("authorization", &auth).body(Body::empty()).unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::FORBIDDEN, "User with no grants anywhere must not be able to list root");
}

#[tokio::test]
async fn recursive_listing_under_ancestor_navigation_only_returns_granted_files() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);

  for path in ["/files/A/B/inside.txt", "/files/A/B/deeper/photo.jpg", "/files/A/C/sibling.txt", "/files/A/secret.txt"] {
    let req = Request::builder()
      .method("PUT")
      .uri(path)
      .header("content-type", "text/plain")
      .header("authorization", &auth_root)
      .body(Body::from("x"))
      .unwrap();
    let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
  }
  drop(app);

  let user_id = create_test_user(&engine, "deep_share_user");
  share_directory_with_user(&engine, "/A/B", &user_id, ".r..l...", None);

  let auth = user_bearer_token(&jwt_manager, &user_id);

  // Recursive listing of /A — should yield only files under /A/B, not
  // /A/C or /A/secret.txt.
  let req = Request::builder().method("GET").uri("/files/A/?depth=-1").header("authorization", &auth).body(Body::empty()).unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  let listing = body_json(resp.into_body()).await;
  let paths: Vec<String> = listing["items"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap_or("").to_string()).collect();
  for path in &paths {
    assert!(path.starts_with("/A/B/"), "Recursive listing leaked path outside grant: {}", path);
  }
  assert!(paths.contains(&"/A/B/inside.txt".to_string()));
  assert!(paths.contains(&"/A/B/deeper/photo.jpg".to_string()));
}

#[tokio::test]
async fn grants_index_invalidates_after_share_change() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);

  let upload = Request::builder()
    .method("PUT")
    .uri("/files/NewShare/file.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth_root)
    .body(Body::from("data"))
    .unwrap();
  let resp = app.oneshot(upload).await.unwrap();
  assert_eq!(resp.status(), StatusCode::CREATED);

  let user_id = create_test_user(&engine, "late_grantee");
  let auth = user_bearer_token(&jwt_manager, &user_id);

  // Before sharing: 403 on /files/.
  let req = Request::builder().method("GET").uri("/files/").header("authorization", &auth).body(Body::empty()).unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::FORBIDDEN);

  // Grant access via the share endpoint (which goes through the proper
  // cache-invalidation path).
  let share_body = serde_json::json!({
      "paths": ["/NewShare"],
      "users": [user_id.to_string()],
      "permissions": ".r..l..."
  });
  let share_req = Request::builder()
    .method("POST")
    .uri("/files/share")
    .header("content-type", "application/json")
    .header("authorization", &auth_root)
    .body(Body::from(serde_json::to_vec(&share_body).unwrap()))
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(share_req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  // After sharing: root listing now exposes /NewShare.
  let root = get_items(rebuild_app(&jwt_manager, &engine), "/files/", &auth).await;
  assert_eq!(item_names(&root), vec!["NewShare".to_string()], "Newly-granted share must be visible without server restart");
}

#[tokio::test]
async fn shared_with_me_returns_user_workspace_grant() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);

  let upload = Request::builder()
    .method("PUT")
    .uri("/files/workspaces/wyatt/readme.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth_root)
    .body(Body::from("workspace"))
    .unwrap();
  let resp = app.oneshot(upload).await.unwrap();
  assert_eq!(resp.status(), StatusCode::CREATED);

  let user_id = create_test_user(&engine, "wyatt");
  share_directory_with_user(&engine, "/workspaces/wyatt", &user_id, "crudlify", None);

  let auth = user_bearer_token(&jwt_manager, &user_id);
  let req = Request::builder().method("GET").uri("/files/shared-with-me").header("authorization", &auth).body(Body::empty()).unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK);

  let json = body_json(resp.into_body()).await;
  let paths = json["paths"].as_array().expect("paths array");
  assert_eq!(paths.len(), 1);
  assert_eq!(paths[0]["path"], "/workspaces/wyatt");
  assert_eq!(paths[0]["permissions"], "crudlify");
  assert!(paths[0]["path_pattern"].is_null());
}

#[tokio::test]
async fn root_user_listing_is_unaffected_by_grants() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);

  for path in ["/files/A/x.txt", "/files/B/y.txt"] {
    let req = Request::builder()
      .method("PUT")
      .uri(path)
      .header("content-type", "text/plain")
      .header("authorization", &auth_root)
      .body(Body::from("x"))
      .unwrap();
    let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
  }
  drop(app);

  // Add a grant on /A — root must still see both A and B.
  let user_id = create_test_user(&engine, "any_user");
  share_directory_with_user(&engine, "/A", &user_id, ".r..l...", None);

  let root = get_items(rebuild_app(&jwt_manager, &engine), "/files/", &auth_root).await;
  let names = item_names(&root);
  assert!(names.contains(&"A".to_string()));
  assert!(names.contains(&"B".to_string()));
}

// ---------------------------------------------------------------------------
// Resolver helper unit tests
// ---------------------------------------------------------------------------

#[test]
fn resolver_check_direct_permission_does_not_grant_ancestor_navigation() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "direct_check_user");
  let group_cache = std::sync::Arc::new(Cache::new(GroupLoader));
  let resolver = PermissionResolver::new(&engine, &group_cache);

  let group = format!("user:{}", user_id);
  let perms = PathPermissions { links: vec![member_link(&group, ".r..l...", "........")] };
  write_permissions(&engine, "/A/B", &perms);
  engine.permissions_cache.evict_all().unwrap();
  engine.grants_index_cache.evict_all().unwrap();

  // Directory paths use trailing slash so path_levels treats them as
  // directory hierarchies (the resolver only walks directory levels).
  // Direct check: ancestors are NOT granted.
  assert!(!resolver.check_direct_permission(&user_id, "/", CrudlifyOp::List).unwrap());
  assert!(!resolver.check_direct_permission(&user_id, "/A/", CrudlifyOp::List).unwrap());
  // But the target itself IS.
  assert!(resolver.check_direct_permission(&user_id, "/A/B/", CrudlifyOp::List).unwrap());

  // Softened check: ancestors get implicit r+l.
  assert!(resolver.check_permission(&user_id, "/", CrudlifyOp::List).unwrap());
  assert!(resolver.check_permission(&user_id, "/A/", CrudlifyOp::List).unwrap());
  assert!(resolver.check_permission(&user_id, "/A/", CrudlifyOp::Read).unwrap());
  // But non-Read/List ops on ancestors stay denied.
  assert!(!resolver.check_permission(&user_id, "/A/", CrudlifyOp::Update).unwrap());
}

#[test]
fn grants_index_surfaces_malformed_permissions_instead_of_caching_empty_grants() {
  let (engine, _temp_dir) = test_engine();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file_buffered(&ctx, "/broken/.aeordb-permissions", b"{not valid json", Some("application/json")).unwrap();
  engine.grants_index_cache.evict_all().unwrap();

  let error = engine.grants_index_cache.get(&(), &engine).expect_err("malformed permission metadata must fail the grant-index load");
  assert!(error.to_string().contains("JSON parse error"), "unexpected malformed-permissions error: {error}");
}

#[test]
fn grants_index_rejects_unknown_protected_traversal() {
  let (engine, _temp_dir) = test_engine();
  let ctx = RequestContext::system();
  DirectoryOps::new(&engine).store_file_buffered(&ctx, "/docs/.aeordb-future/item.json", b"unknown", Some("application/json")).unwrap();
  engine.grants_index_cache.evict_all().unwrap();

  let error = engine.grants_index_cache.get(&(), &engine).expect_err("permission authority must reject unknown protected state");
  assert!(matches!(error, aeordb::engine::EngineError::SystemFamilyPolicy { code: "unknown_protected_system_family", .. }));
}

// ---------------------------------------------------------------------------
// Cross-endpoint leak regressions — handlers that bypass the path-aware
// permission middleware must enforce their own user-level filtering.
// ---------------------------------------------------------------------------

/// Set up a non-root user "wyatt"-style scenario:
///   /Pictures/Family/Harlo/{photo.jpg, photo2.jpg}  — granted r+l
///   /Pictures/Family/Aeolus/secret.jpg              — NOT granted
///   /Documents/private.pdf                          — NOT granted
/// Returns (app-factory closure, jwt_manager, engine, temp_dir, user_id,
/// user_auth string).
async fn setup_user_with_share() -> (Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir, Uuid, String) {
  let (app, jwt_manager, engine, temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);

  let upload = |path: &str| {
    Request::builder()
      .method("PUT")
      .uri(path)
      .header("content-type", "text/plain")
      .header("authorization", &auth_root)
      .body(Body::from("payload"))
      .unwrap()
  };
  for path in [
    "/files/Pictures/Family/Harlo/photo.jpg",
    "/files/Pictures/Family/Harlo/photo2.jpg",
    "/files/Pictures/Family/Aeolus/secret.jpg",
    "/files/Documents/private.pdf",
  ] {
    let resp = rebuild_app(&jwt_manager, &engine).oneshot(upload(path)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
  }
  drop(app);

  let user_id = create_test_user(&engine, "harlo_viewer");
  share_directory_with_user(&engine, "/Pictures/Family/Harlo", &user_id, ".r..l...", None);

  let auth = user_bearer_token(&jwt_manager, &user_id);
  (jwt_manager, engine, temp_dir, user_id, auth)
}

#[tokio::test]
async fn download_zip_rejects_paths_outside_user_grant() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  // Try to download a directory the user has no grant on.
  let body = serde_json::json!({ "paths": ["/Documents"] });
  let req = Request::builder()
    .method("POST")
    .uri("/files/download")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::NOT_FOUND, "download_zip must 404 paths the user has no grant on");
}

#[tokio::test]
async fn download_zip_allows_paths_within_user_grant() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  let body = serde_json::json!({ "paths": ["/Pictures/Family/Harlo"] });
  let req = Request::builder()
    .method("POST")
    .uri("/files/download")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK, "download_zip must succeed for paths within the grant");
}

#[tokio::test]
async fn batch_fetch_rejects_paths_outside_user_grant() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  let body = serde_json::json!({ "paths": ["/Pictures/Family/Harlo/photo.jpg", "/Documents/private.pdf"] });
  let req = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::NOT_FOUND, "batch fetch must 404 when any requested path lacks a direct read grant");
}

#[tokio::test]
async fn batch_fetch_allows_paths_within_user_grant() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  let body = serde_json::json!({ "paths": ["/Pictures/Family/Harlo/photo.jpg", "/Pictures/Family/Harlo/photo2.jpg"] });
  let req = Request::builder()
    .method("POST")
    .uri("/files/fetch")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK, "batch fetch must succeed for paths within the grant");
  let json = body_json(resp.into_body()).await;
  assert_eq!(json["/Pictures/Family/Harlo/photo.jpg"]["content"], "payload");
  assert_eq!(json["/Pictures/Family/Harlo/photo2.jpg"]["content"], "payload");
}

#[tokio::test]
async fn mkdir_rejects_outside_user_grant() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  let body = serde_json::json!({ "path": "/Documents/sneaky" });
  let req = Request::builder()
    .method("POST")
    .uri("/files/mkdir")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::FORBIDDEN, "mkdir must reject paths the user lacks Create perm on");
}

#[tokio::test]
async fn copy_files_rejects_unauthorized_source() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  // User cannot read /Pictures/Family/Aeolus/secret.jpg.
  let body = serde_json::json!({
      "paths": ["/Pictures/Family/Aeolus/secret.jpg"],
      "destination": "/Pictures/Family/Harlo",
  });
  let req = Request::builder()
    .method("POST")
    .uri("/files/copy")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::NOT_FOUND, "copy_files must 404 unauthorized sources");
}

#[tokio::test]
async fn copy_files_rejects_unauthorized_destination() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  // User cannot write into /Documents.
  let body = serde_json::json!({
      "paths": ["/Pictures/Family/Harlo/photo.jpg"],
      "destination": "/Documents",
  });
  let req = Request::builder()
    .method("POST")
    .uri("/files/copy")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::FORBIDDEN, "copy_files must reject unauthorized destinations");
}

#[tokio::test]
async fn copy_files_validates_every_source_before_publishing_any_destination() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  store_test_file(&engine, "/copy-source/valid.txt", b"valid");

  let body = serde_json::json!({
    "paths": ["/copy-source/valid.txt", "/copy-source/missing.txt"],
    "destination": "/copy-destination",
  });
  let request = Request::builder()
    .method("POST")
    .uri("/files/copy")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();
  let response = rebuild_app(&jwt_manager, &engine).oneshot(request).await.unwrap();

  assert_eq!(response.status(), StatusCode::NOT_FOUND);
  assert!(DirectoryOps::new(&engine).get_metadata("/copy-destination/valid.txt").unwrap().is_none());
}

#[tokio::test]
async fn copy_files_rejects_colliding_destination_names_without_partial_publication() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  store_test_file(&engine, "/copy-source/a/same.txt", b"a");
  store_test_file(&engine, "/copy-source/b/same.txt", b"b");

  let body = serde_json::json!({
    "paths": ["/copy-source/a/same.txt", "/copy-source/b/same.txt"],
    "destination": "/copy-destination",
  });
  let request = Request::builder()
    .method("POST")
    .uri("/files/copy")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();
  let response = rebuild_app(&jwt_manager, &engine).oneshot(request).await.unwrap();

  assert_eq!(response.status(), StatusCode::CONFLICT);
  assert!(DirectoryOps::new(&engine).get_metadata("/copy-destination/same.txt").unwrap().is_none());
}

#[tokio::test]
async fn restore_deleted_file_requires_delete_permission() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  // Try to restore a path the user has no Delete perm on (and that
  // likely isn't even deleted — handler must 404 before reaching ops).
  let body = serde_json::json!({ "path": "/Documents/private.pdf" });
  let req = Request::builder()
    .method("POST")
    .uri("/files/restore")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_vec(&body).unwrap()))
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::NOT_FOUND, "restore_deleted_file must 404 paths the user lacks Delete on");
}

#[tokio::test]
async fn file_history_blocks_unauthorized_paths() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  let req = Request::builder()
    .method("GET")
    .uri("/versions/history/Documents/private.pdf")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::NOT_FOUND, "file_history must 404 paths the user has no Read on");
}

#[tokio::test]
async fn file_history_allows_authorized_paths() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  let req = Request::builder()
    .method("GET")
    .uri("/versions/history/Pictures/Family/Harlo/photo.jpg")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK, "file_history must succeed for paths the user can Read");
}

#[tokio::test]
async fn list_shares_blocks_enumeration_of_unauthorized_paths() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  // The user must NOT be able to learn who's been granted access to
  // /Documents/private.pdf (which they have no Read on).
  let req = Request::builder()
    .method("GET")
    .uri("/files/shares?path=/Documents/private.pdf")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::NOT_FOUND, "list_shares must 404 paths the caller can't read");
}

#[tokio::test]
async fn list_shares_allows_own_subtree() {
  let (jwt_manager, engine, _tmp, _user, auth) = setup_user_with_share().await;

  let req = Request::builder()
    .method("GET")
    .uri("/files/shares?path=/Pictures/Family/Harlo")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK, "list_shares must work on paths the caller has access to");
}

#[tokio::test]
async fn snapshot_restore_then_gc_reads_head_without_healing_stale_dir_keys() {
  use aeordb::engine::{gc as gc_mod, VersionManager};

  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Build initial state.
  for path in ["/A/B/file_v1_a.txt", "/A/B/file_v1_b.txt"] {
    let req = Request::builder()
      .method("PUT")
      .uri(&format!("/files{}", path))
      .header("content-type", "text/plain")
      .header("authorization", &auth)
      .body(Body::from("v1"))
      .unwrap();
    let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
  }

  // Snapshot the initial state.
  let ctx = aeordb::engine::RequestContext::system();
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "snap-initial", std::collections::HashMap::new()).unwrap();
  let snap_root_hash = vm.get_head_hash().unwrap();

  // Mutate /A/B so its content_hash diverges from what the snapshot has.
  for path in ["/A/B/file_v2.txt"] {
    let req = Request::builder()
      .method("PUT")
      .uri(&format!("/files{}", path))
      .header("content-type", "text/plain")
      .header("authorization", &auth)
      .body(Body::from("v2"))
      .unwrap();
    let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
  }
  let post_mutation_head = vm.get_head_hash().unwrap();
  assert_ne!(post_mutation_head, snap_root_hash, "HEAD should have advanced after mutation");

  // Restore the snapshot — moves HEAD back but does NOT rewrite dir_keys.
  vm.restore_snapshot(&ctx, "snap-initial").unwrap();
  let post_restore_head = vm.get_head_hash().unwrap();
  assert_eq!(post_restore_head, snap_root_hash, "HEAD should match the restored snapshot");

  drop(app);

  // Run GC. The post-mutation content of /A/B is no longer reachable from
  // HEAD (we restored to before that mutation) and there's only one
  // snapshot — which is the restored state, not the mutated state. The
  // post-mutation content gets swept; the dir_key for /A/B is preserved
  // (path-key marking) but its hard-link target is now dead.
  let gc_engine = engine.clone();
  let gc_ctx = aeordb::engine::RequestContext::system();
  let gc_result = tokio::task::spawn_blocking(move || gc_mod::run_gc(&gc_engine, &gc_ctx, false)).await.unwrap();
  assert!(gc_result.is_ok());

  // Before any list, verify directly via the recovery probe — confirms
  // we did set up the bug pattern (stale dir_key present).
  let report_pre_list = aeordb::engine::verify::verify(&engine, "<test>");
  assert!(
    report_pre_list.stale_dir_path_keys.iter().any(|p| p == "/A/B"),
    "test setup must produce a stale dir_key for /A/B. Report: {:?}",
    report_pre_list
  );

  // Direct GET on /A/B follows HEAD and must not mutate derived locators as a
  // read side effect.
  let req = Request::builder().method("GET").uri("/files/A/B/").header("authorization", &auth).body(Body::empty()).unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::OK, "GET on stale-dir_key directory must succeed via runtime recovery");
  let listing = body_json(resp.into_body()).await;
  let names: Vec<String> = listing["items"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect();
  assert!(names.contains(&"file_v1_a.txt".to_string()));
  assert!(names.contains(&"file_v1_b.txt".to_string()));
  assert!(!names.contains(&"file_v2.txt".to_string()), "Restored snapshot should not show post-mutation file");

  // The stale locator remains repair evidence until an explicit repair owns
  // the write.
  let report_after = aeordb::engine::verify::verify(&engine, "<test>");
  assert!(
    report_after.stale_dir_path_keys.iter().any(|p| p == "/A/B"),
    "ordinary listing must not auto-heal the stale dir_key for /A/B: {:?}",
    report_after.stale_dir_path_keys,
  );

  let repaired = aeordb::engine::verify::verify_and_repair_checked(&engine, "<test>").unwrap();
  assert!(
    !repaired.stale_dir_path_keys.iter().any(|path| path == "/A/B"),
    "explicit repair must reconcile the stale dir_key: {:?}",
    repaired.stale_dir_path_keys,
  );
}

#[tokio::test]
async fn directory_listing_with_space_in_name_repro() {
  use aeordb::engine::gc as gc_mod;

  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Put multiple files at a path with a space in the directory name,
  // matching the live-DB shape (17 files under /Pictures/Family/Aeolus/Coloring pages/).
  for i in 0..5 {
    let put = Request::builder()
      .method("PUT")
      .uri(&format!("/files/A/Coloring%20pages/file{}.txt", i))
      .header("content-type", "text/plain")
      .header("authorization", &auth)
      .body(Body::from(format!("content {}", i)))
      .unwrap();
    let resp = rebuild_app(&jwt_manager, &engine).oneshot(put).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
  }

  // Force GC to run, mimicking the live-DB conditions.
  eprintln!("Running GC...");
  let gc_ctx = aeordb::engine::RequestContext::system();
  let gc_result = tokio::task::spawn_blocking({
    let engine = engine.clone();
    move || gc_mod::run_gc(&engine, &gc_ctx, false)
  })
  .await
  .unwrap();
  eprintln!("GC result: {:?}", gc_result.is_ok());
  assert!(gc_result.is_ok());

  // Now reproduce the user's bug: parent listing works, child file works,
  // direct directory listing 404s.
  let put = Request::builder()
    .method("PUT")
    .uri("/files/A/Coloring%20pages/test.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("hello"))
    .unwrap();
  let resp = app.oneshot(put).await.unwrap();
  eprintln!("PUT status: {}", resp.status());
  assert_eq!(resp.status(), StatusCode::CREATED);

  // Parent listing should show "Coloring pages" (decoded) as a child.
  let req = Request::builder().method("GET").uri("/files/A/").header("authorization", &auth).body(Body::empty()).unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  let parent = body_json(resp.into_body()).await;
  eprintln!("Parent listing items: {}", serde_json::to_string_pretty(&parent["items"]).unwrap());

  // GET the directory itself via %20-encoded URL.
  let req = Request::builder().method("GET").uri("/files/A/Coloring%20pages/").header("authorization", &auth).body(Body::empty()).unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  let status_enc = resp.status();
  let body_enc = body_json(resp.into_body()).await;
  eprintln!("GET /A/Coloring%%20pages/ -> {}: {}", status_enc, body_enc);

  // GET file under the dir via %20.
  let req =
    Request::builder().method("GET").uri("/files/A/Coloring%20pages/test.txt").header("authorization", &auth).body(Body::empty()).unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  let status_file = resp.status();
  eprintln!("GET /A/Coloring%%20pages/test.txt -> {}", status_file);

  assert_eq!(status_file, StatusCode::OK, "file fetch must work");
  assert_eq!(status_enc, StatusCode::OK, "dir listing with %20 must work");
}

#[test]
fn resolver_accessible_child_names_returns_navigable_segments() {
  let (engine, _temp_dir) = test_engine();
  let user_id = create_test_user(&engine, "child_names_user");
  let group_cache = std::sync::Arc::new(Cache::new(GroupLoader));
  let resolver = PermissionResolver::new(&engine, &group_cache);

  let group = format!("user:{}", user_id);
  write_permissions(&engine, "/A/B/C", &PathPermissions { links: vec![member_link(&group, ".r..l...", "........")] });
  write_permissions(
    &engine,
    "/D",
    &PathPermissions { links: vec![member_link_with_pattern(&group, ".r......", "........", "report.pdf")] },
  );
  engine.permissions_cache.evict_all().unwrap();
  engine.grants_index_cache.evict_all().unwrap();

  let mut root_children = resolver.accessible_child_names(&user_id, "/").unwrap();
  root_children.sort();
  assert_eq!(root_children, vec!["A".to_string(), "D".to_string()]);

  assert_eq!(resolver.accessible_child_names(&user_id, "/A").unwrap(), vec!["B".to_string()]);
  assert_eq!(resolver.accessible_child_names(&user_id, "/A/B").unwrap(), vec!["C".to_string()]);
  assert_eq!(resolver.accessible_child_names(&user_id, "/D").unwrap(), vec!["report.pdf".to_string()]);
  assert!(resolver.accessible_child_names(&user_id, "/E").unwrap().is_empty());
}

#[tokio::test]
async fn direct_share_on_directory_grants_descend_without_trailing_slash() {
  // Regression for bot-docs/bug-reports/2026-05-22-listing-vs-descend-
  // permission-inconsistency.md. Setup:
  //   - User has a direct `crudlify` share on /Pictures/Family/Susan
  //     (a directory). No grants on any ancestor.
  //   - Listing the parent /Pictures/Family correctly attaches
  //     `effective_permissions = "crudlify"` to the Susan entry
  //     (engine_routes.rs:677 adds a trailing slash before resolving).
  //   - Pre-fix: descending into /files/Pictures/Family/Susan (NO
  //     trailing slash) returned 403 because the middleware called
  //     `check_direct_permission` which walks ancestor levels only —
  //     the path itself wasn't included in path_levels() without the
  //     trailing slash, so the direct share at Susan was invisible.
  //
  // The fix switches the middleware to `check_path_permission`,
  // which tries both the as-given path AND the directory form. This
  // test asserts both URL shapes (with and without trailing slash)
  // succeed for the same user with the same direct grant.

  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);

  // Seed a directory with a file inside (so Susan really IS a directory).
  let req = Request::builder()
    .method("PUT")
    .uri("/files/Pictures/Family/Susan/photo.jpg")
    .header("content-type", "image/jpeg")
    .header("authorization", &auth_root)
    .body(Body::from("payload"))
    .unwrap();
  let resp = rebuild_app(&jwt_manager, &engine).oneshot(req).await.unwrap();
  assert_eq!(resp.status(), StatusCode::CREATED);
  drop(app);

  // Direct grant on Susan only — no parent grants anywhere.
  let user_id = create_test_user(&engine, "susan_grantee");
  share_directory_with_user(&engine, "/Pictures/Family/Susan", &user_id, "crudlify", None);
  let auth = user_bearer_token(&jwt_manager, &user_id);

  // Descend WITH trailing slash → List op → must succeed.
  let req_slash =
    Request::builder().method("GET").uri("/files/Pictures/Family/Susan/").header("authorization", &auth).body(Body::empty()).unwrap();
  let resp_slash = rebuild_app(&jwt_manager, &engine).oneshot(req_slash).await.unwrap();
  assert_eq!(
    resp_slash.status(),
    StatusCode::OK,
    "GET /files/Pictures/Family/Susan/ with trailing slash must succeed (direct crudlify grant)"
  );

  // Descend WITHOUT trailing slash → Read op → must ALSO succeed.
  // Pre-fix: this returned 403 because check_direct_permission walked
  // only ancestors (which have no grants) and missed Susan's own.
  let req_no_slash =
    Request::builder().method("GET").uri("/files/Pictures/Family/Susan").header("authorization", &auth).body(Body::empty()).unwrap();
  let resp_no_slash = rebuild_app(&jwt_manager, &engine).oneshot(req_no_slash).await.unwrap();
  assert_eq!(
    resp_no_slash.status(),
    StatusCode::OK,
    "GET /files/Pictures/Family/Susan WITHOUT trailing slash must also succeed — \
         the direct grant on Susan applies regardless of the URL's trailing-slash shape. \
         If this returns 403, the middleware regressed back to check_direct_permission."
  );
}
