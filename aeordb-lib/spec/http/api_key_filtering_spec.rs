use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;
use chrono::Utc;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::api_key::{ApiKeyRecord, hash_api_key, generate_api_key};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::engine::api_key_rules::KeyRule;
use aeordb::engine::system_store;
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

fn create_scoped_key_and_token(
    jwt_manager: &JwtManager,
    engine: &StorageEngine,
    user_id: Uuid,
    rules: Vec<KeyRule>,
) -> (String, Uuid) {
    let key_id = Uuid::new_v4();
    let plaintext = generate_api_key(key_id);
    let key_hash = hash_api_key(&plaintext).unwrap();
    let now = Utc::now();

    let record = ApiKeyRecord {
        key_id,
        key_hash,
        user_id: Some(user_id),
        created_at: now,
        is_revoked: false,
        expires_at: now.timestamp_millis() + (365 * 86400 * 1000),
        label: Some("test-filtering-key".to_string()),
        rules,
    };

    let ctx = RequestContext::system();

    system_store::store_api_key_for_bootstrap(engine, &ctx, &record).unwrap();

    let now_ts = now.timestamp();
    let claims = TokenClaims {
        sub: user_id.to_string(),
        iss: "aeordb".to_string(),
        iat: now_ts,
        exp: now_ts + DEFAULT_EXPIRY_SECONDS,
        scope: None,
        permissions: None,
        key_id: Some(key_id.to_string()),
    };
    let token = jwt_manager.create_token(&claims).unwrap();
    (format!("Bearer {}", token), key_id)
}

fn root_bearer_token(jwt_manager: &JwtManager) -> String {
    let now = Utc::now().timestamp();
    let claims = TokenClaims {
        sub: Uuid::nil().to_string(),
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

fn store_symlink(engine: &StorageEngine, path: &str, target: &str) {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(engine);
    ops.store_symlink(&ctx, path, target).unwrap();
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body.collect().await.unwrap().to_bytes().to_vec();
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

async fn body_bytes(body: Body) -> Vec<u8> {
    body.collect().await.unwrap().to_bytes().to_vec()
}

// ===========================================================================
// Directory listing filtering tests
// ===========================================================================

/// Recursive listing from a parent directory filters entries based on key rules.
/// Files under /stuff/allowed/ should appear, /stuff/denied/ should not.
#[tokio::test]
async fn test_listing_filters_denied_entries() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "stuff/allowed/a.txt", b"allowed content");
    store_file(&engine, "stuff/denied/b.txt", b"denied content");

    let rules = vec![
        KeyRule { glob: "/stuff/allowed/**".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/stuff/denied/**".to_string(), permitted: "--------".to_string() },
        // Allow list on the parent so middleware lets the listing request through
        KeyRule { glob: "/stuff/**".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/stuff/".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/stuff/?depth=-1")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let entries = json["items"].as_array().expect("listing should have items array");

    let paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();

    // /stuff/allowed/a.txt should be present (crudlify includes 'l')
    assert!(paths.contains(&"/stuff/allowed/a.txt"), "Should contain /stuff/allowed/a.txt, got: {:?}", paths);

    // /stuff/denied/b.txt should NOT be present (explicitly denied)
    assert!(
        !paths.iter().any(|p| p.contains("/stuff/denied")),
        "No /stuff/denied entries should appear, got: {:?}",
        paths
    );
}

/// Recursive listing: entries under denied paths are filtered out.
/// Uses explicit deny rules for denied paths.
#[tokio::test]
async fn test_recursive_listing_filters_denied() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "dir/allowed/a.txt", b"allowed");
    store_file(&engine, "dir/denied/b.txt", b"denied");

    let rules = vec![
        KeyRule { glob: "/dir/allowed/**".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/dir/denied/**".to_string(), permitted: "--------".to_string() },
        // Allow list on /dir/ so middleware lets the request through
        KeyRule { glob: "/dir/".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/dir/**".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/dir/?depth=-1")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let entries = json["items"].as_array().expect("listing should have items array");

    let paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();

    assert!(
        paths.iter().any(|p| p.contains("/dir/allowed/a.txt")),
        "Should contain /dir/allowed/a.txt, got: {:?}",
        paths
    );
    assert!(
        !paths.iter().any(|p| p.contains("/dir/denied")),
        "Should not contain any /dir/denied entries, got: {:?}",
        paths
    );
}

/// Default listing (no depth/glob) should filter out denied child directories.
#[tokio::test]
async fn test_default_listing_filters() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "root/visible/file.txt", b"visible");
    store_file(&engine, "root/hidden/file.txt", b"hidden");

    let rules = vec![
        KeyRule { glob: "/root/visible".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/root/visible/**".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/root/hidden".to_string(), permitted: "--------".to_string() },
        KeyRule { glob: "/root/hidden/**".to_string(), permitted: "--------".to_string() },
        // Allow listing /root/ itself
        KeyRule { glob: "/root/".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "----l---".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/root/")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let entries = json["items"].as_array().expect("listing should have items array");

    let paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();

    assert!(paths.contains(&"/root/visible"), "Should contain /root/visible, got: {:?}", paths);
    assert!(!paths.contains(&"/root/hidden"), "Should not contain /root/hidden, got: {:?}", paths);
}

/// Recursive listing should filter deep trees correctly.
#[tokio::test]
async fn test_recursive_listing_prunes() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "tree/ok/a.txt", b"ok-a");
    store_file(&engine, "tree/ok/sub/b.txt", b"ok-sub-b");
    store_file(&engine, "tree/no/c.txt", b"no-c");

    let rules = vec![
        KeyRule { glob: "/tree/ok/**".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/tree/no/**".to_string(), permitted: "--------".to_string() },
        KeyRule { glob: "/tree/**".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/tree/".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/tree/?depth=-1")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let entries = json["items"].as_array().expect("listing should have items array");

    let paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();

    assert!(paths.contains(&"/tree/ok/a.txt"), "Should contain /tree/ok/a.txt, got: {:?}", paths);
    assert!(paths.contains(&"/tree/ok/sub/b.txt"), "Should contain /tree/ok/sub/b.txt, got: {:?}", paths);
    assert!(!paths.iter().any(|p| p.contains("/tree/no")), "No /tree/no entries should appear, got: {:?}", paths);
}

/// Unscoped token (no key_id) should return all entries without filtering.
#[tokio::test]
async fn test_unscoped_token_no_filtering() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "dir/a/file.txt", b"a-file");
    store_file(&engine, "dir/b/file.txt", b"b-file");

    let token = root_bearer_token(&jwt_manager);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/dir/?depth=-1")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let entries = json["items"].as_array().expect("listing should have items array");

    let paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert!(paths.iter().any(|p| p.contains("/dir/a/")), "Should contain /dir/a/ path, got: {:?}", paths);
    assert!(paths.iter().any(|p| p.contains("/dir/b/")), "Should contain /dir/b/ path, got: {:?}", paths);
}

/// Key with empty rules should not filter anything (full pass-through).
#[tokio::test]
async fn test_empty_rules_no_filtering() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "ns/x/file.txt", b"x");
    store_file(&engine, "ns/y/file.txt", b"y");

    let rules = vec![];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/ns/?depth=-1")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let entries = json["items"].as_array().expect("listing should have items array");

    let paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert!(paths.iter().any(|p| p.contains("/ns/x/")), "Should contain /ns/x/ path, got: {:?}", paths);
    assert!(paths.iter().any(|p| p.contains("/ns/y/")), "Should contain /ns/y/ path, got: {:?}", paths);
}

/// Listing checks the 'l' flag for entry visibility, not the 'r' flag.
/// An entry with only 'r' but no 'l' should be hidden from listings.
#[tokio::test]
async fn test_listing_checks_l_flag_not_r() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/public.txt", b"public");
    store_file(&engine, "data/private.txt", b"private");

    let rules = vec![
        KeyRule { glob: "/data/public.txt".to_string(), permitted: "-r--l---".to_string() },
        KeyRule { glob: "/data/private.txt".to_string(), permitted: "-r------".to_string() },
        KeyRule { glob: "/data/".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "----l---".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let entries = json["items"].as_array().expect("listing should have items array");
    let paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();

    assert!(paths.contains(&"/data/public.txt"), "Should contain /data/public.txt, got: {:?}", paths);
    assert!(!paths.contains(&"/data/private.txt"), "Should not contain /data/private.txt, got: {:?}", paths);
}

/// Listing where ALL child entries are denied returns an empty array, not an error.
#[tokio::test]
async fn test_listing_all_denied_returns_empty_array() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "secret/a.txt", b"a");
    store_file(&engine, "secret/b.txt", b"b");

    let rules = vec![
        KeyRule { glob: "/secret/".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/secret/**".to_string(), permitted: "--------".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/secret/")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let entries = json["items"].as_array().expect("listing should have items array");
    assert!(entries.is_empty(), "All entries denied should return empty array, got: {:?}", entries);
}

/// Mixed permissions in the same directory: some entries allowed, some denied.
#[tokio::test]
async fn test_listing_mixed_permissions_same_directory() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "mixed/public.txt", b"public");
    store_file(&engine, "mixed/private.txt", b"private");
    store_file(&engine, "mixed/also_public.txt", b"also public");

    let rules = vec![
        KeyRule { glob: "/mixed/public.txt".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/mixed/also_public.txt".to_string(), permitted: "crudlify".to_string() },
        // Allow listing the /mixed/ directory itself (before the deny-all wildcard)
        KeyRule { glob: "/mixed/".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/mixed/**".to_string(), permitted: "--------".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "----l---".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/mixed/")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let entries = json["items"].as_array().expect("listing should have items array");
    let paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();

    assert!(paths.contains(&"/mixed/public.txt"), "Should contain public.txt, got: {:?}", paths);
    assert!(paths.contains(&"/mixed/also_public.txt"), "Should contain also_public.txt, got: {:?}", paths);
    assert!(!paths.contains(&"/mixed/private.txt"), "Should not contain private.txt, got: {:?}", paths);
}

// ===========================================================================
// Symlink filtering tests
// ===========================================================================

/// Symlink at allowed path pointing to a denied target.
/// Following the symlink should return 404 because the resolved target is denied.
#[tokio::test]
async fn test_symlink_allowed_to_denied_target() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "secret/data.txt", b"secret content");
    store_symlink(&engine, "/link", "/secret/data.txt");

    let rules = vec![
        KeyRule { glob: "/link".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/link/**".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/secret/**".to_string(), permitted: "--------".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/link")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Symlink where both symlink path and target are allowed.
/// Should successfully return the file content.
#[tokio::test]
async fn test_symlink_both_allowed() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/file.txt", b"target content");
    store_symlink(&engine, "/link", "/data/file.txt");

    let rules = vec![
        KeyRule { glob: "/**".to_string(), permitted: "crudlify".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/link")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"target content");
}

/// Symlink at a denied path. The middleware should catch this before
/// symlink resolution even happens, returning 404.
#[tokio::test]
async fn test_symlink_denied_path() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/file.txt", b"file content");
    store_symlink(&engine, "/denied/link", "/data/file.txt");

    let rules = vec![
        KeyRule { glob: "/denied/**".to_string(), permitted: "--------".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "crudlify".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/denied/link")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// nofollow on a symlink to a denied target should still return symlink metadata.
/// nofollow doesn't resolve the target, so target permissions don't apply.
#[tokio::test]
async fn test_nofollow_allowed_symlink_to_denied() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "secret/file.txt", b"secret");
    store_symlink(&engine, "/link", "/secret/file.txt");

    let rules = vec![
        KeyRule { glob: "/link".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/link/**".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/secret/**".to_string(), permitted: "--------".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/link?nofollow=true")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    assert_eq!(json["target"].as_str().unwrap(), "/secret/file.txt");
    assert_eq!(json["entry_type"].as_u64().unwrap(), 8);
}

/// Symlink target that has no matching rule at all should be denied.
#[tokio::test]
async fn test_symlink_target_no_matching_rule() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "other/file.txt", b"other content");
    store_symlink(&engine, "/link", "/other/file.txt");

    let rules = vec![
        KeyRule { glob: "/link".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/link/**".to_string(), permitted: "crudlify".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/link")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Symlink appearing in a filtered listing: only the allowed symlink should show.
#[tokio::test]
async fn test_symlink_in_listing_filtered() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    store_file(&engine, "data/target.txt", b"target");
    store_symlink(&engine, "/data/allowed_link", "/data/target.txt");
    store_symlink(&engine, "/data/denied_link", "/data/target.txt");

    let rules = vec![
        KeyRule { glob: "/data/allowed_link".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/data/target.txt".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/data/denied_link".to_string(), permitted: "--------".to_string() },
        KeyRule { glob: "/data/".to_string(), permitted: "----l---".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "----l---".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/files/data/")
                .header("authorization", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let entries = json["items"].as_array().expect("listing should have items array");
    let paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();

    assert!(paths.contains(&"/data/allowed_link"), "Should contain allowed_link, got: {:?}", paths);
    assert!(paths.contains(&"/data/target.txt"), "Should contain target.txt, got: {:?}", paths);
    assert!(!paths.contains(&"/data/denied_link"), "Should not contain denied_link, got: {:?}", paths);
}

// ===========================================================================
// Query result filtering tests
// ===========================================================================

/// Query results should filter out entries the key cannot read.
#[tokio::test]
async fn test_query_filters_denied_results() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
    let config = PathIndexConfig {
        parser: None,
        parser_memory_limit: None,
        logging: false,
        glob: None,

        indexes: vec![
            IndexFieldConfig {
                name: "name".to_string(),
                index_type: "string".to_string(),
                source: None,
                min: None,
                max: None,
            },
        ],
    };

    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    let config_data = config.serialize();

    // Set up index configs first
    ops.store_file(&ctx, "allowed/.aeordb-config/indexes.json", &config_data, Some("application/json")).unwrap();
    ops.store_file(&ctx, "denied/.aeordb-config/indexes.json", &config_data, Some("application/json")).unwrap();

    // Store indexed files using full pipeline to trigger indexing
    ops.store_file_with_full_pipeline(
        &ctx, "allowed/doc1.json", br#"{"name": "allowed-doc", "value": 42}"#,
        Some("application/json"), None,
    ).unwrap();
    ops.store_file_with_full_pipeline(
        &ctx, "denied/doc2.json", br#"{"name": "denied-doc", "value": 99}"#,
        Some("application/json"), None,
    ).unwrap();

    let rules = vec![
        KeyRule { glob: "/allowed/**".to_string(), permitted: "crudlify".to_string() },
        KeyRule { glob: "/**".to_string(), permitted: "--------".to_string() },
    ];
    let (token, _key_id) = create_scoped_key_and_token(&jwt_manager, &engine, Uuid::nil(), rules);

    let query_body = serde_json::json!({
        "path": "/allowed",
        "where": [
            { "field": "name", "op": "eq", "value": "allowed-doc" }
        ]
    });

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/files/query")
                .header("authorization", &token)
                .header("content-type", "application/json")
                .body(Body::from(query_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let results = json["items"].as_array().expect("results should be array");

    // All results should be under /allowed/
    let denied_paths: Vec<&str> = results
        .iter()
        .filter_map(|r| r["path"].as_str())
        .filter(|p| p.starts_with("/denied"))
        .collect();
    assert!(denied_paths.is_empty(), "No denied paths should appear in query results, got: {:?}", denied_paths);

    // Should have at least the allowed doc
    let allowed_paths: Vec<&str> = results
        .iter()
        .filter_map(|r| r["path"].as_str())
        .filter(|p| p.contains("/allowed/"))
        .collect();
    assert!(!allowed_paths.is_empty(), "Should have allowed results, got: {:?}", allowed_paths);
}

/// Query with no key rules (unscoped) should return all results.
#[tokio::test]
async fn test_query_unscoped_no_filtering() {
    let (_app, jwt_manager, engine, _temp_dir) = test_app();

    use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
    let config = PathIndexConfig {
        parser: None,
        parser_memory_limit: None,
        logging: false,
        glob: None,

        indexes: vec![
            IndexFieldConfig {
                name: "name".to_string(),
                index_type: "string".to_string(),
                source: None,
                min: None,
                max: None,
            },
        ],
    };

    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&engine);
    let config_data = config.serialize();
    ops.store_file(&ctx, "a/.aeordb-config/indexes.json", &config_data, Some("application/json")).unwrap();
    ops.store_file(&ctx, "b/.aeordb-config/indexes.json", &config_data, Some("application/json")).unwrap();

    ops.store_file_with_full_pipeline(
        &ctx, "a/doc.json", br#"{"name": "a-doc"}"#,
        Some("application/json"), None,
    ).unwrap();
    ops.store_file_with_full_pipeline(
        &ctx, "b/doc.json", br#"{"name": "b-doc"}"#,
        Some("application/json"), None,
    ).unwrap();

    let token = root_bearer_token(&jwt_manager);

    let query_body = serde_json::json!({
        "path": "/a",
        "where": [
            { "field": "name", "op": "eq", "value": "a-doc" }
        ]
    });

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/files/query")
                .header("authorization", &token)
                .header("content-type", "application/json")
                .body(Body::from(query_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response.into_body()).await;
    let results = json["items"].as_array().expect("results should be array");

    let paths: Vec<&str> = results.iter().filter_map(|r| r["path"].as_str()).collect();
    assert!(paths.iter().any(|p| p.contains("/a/")), "Should contain /a/ path, got: {:?}", paths);
}
