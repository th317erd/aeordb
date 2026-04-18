use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::auth::FileAuthProvider;
use aeordb::engine::{EventBus, StorageEngine};
use aeordb::plugins::PluginManager;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests, CorsState};

// ===========================================================================
// Shared test infrastructure
// ===========================================================================

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

struct TestHarness {
    jwt_manager: Arc<JwtManager>,
    engine: Arc<StorageEngine>,
    rate_limiter: Arc<RateLimiter>,
    root_jwt: String,
    _temp_dir: tempfile::TempDir,
}

impl TestHarness {
    fn new() -> Self {
        let jwt_manager = Arc::new(JwtManager::generate());
        let (engine, temp_dir) = create_temp_engine_for_tests();
        let rate_limiter = Arc::new(RateLimiter::default_config());
        let root_jwt = root_bearer_token(&jwt_manager);
        TestHarness {
            jwt_manager,
            engine,
            rate_limiter,
            root_jwt,
            _temp_dir: temp_dir,
        }
    }

    fn app(&self) -> axum::Router {
        let plugin_manager = Arc::new(PluginManager::new(self.engine.clone()));
        let auth_provider: Arc<dyn aeordb::auth::AuthProvider> =
            Arc::new(FileAuthProvider::new(self.engine.clone()));
        create_app_with_all(
            auth_provider,
            self.jwt_manager.clone(),
            plugin_manager,
            self.rate_limiter.clone(),
            make_prometheus_handle(),
            self.engine.clone(),
            Arc::new(EventBus::new()),
            CorsState {
                default_origins: None,
                rules: vec![],
            },
        )
    }

    async fn create_user(&self, username: &str) -> String {
        let body = format!(r#"{{"username":"{}"}}"#, username);
        let request = Request::builder()
            .method("POST")
            .uri("/system/users")
            .header("content-type", "application/json")
            .header("authorization", &self.root_jwt)
            .body(Body::from(body))
            .unwrap();
        let response = self.app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let json = body_json(response.into_body()).await;
        json["user_id"].as_str().unwrap().to_string()
    }

    async fn create_api_key(&self, user_id: &str) -> String {
        let body = format!(r#"{{"user_id":"{}"}}"#, user_id);
        let request = Request::builder()
            .method("POST")
            .uri("/auth/keys/admin")
            .header("content-type", "application/json")
            .header("authorization", &self.root_jwt)
            .body(Body::from(body))
            .unwrap();
        let response = self.app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let json = body_json(response.into_body()).await;
        json["api_key"].as_str().unwrap().to_string()
    }

    async fn get_jwt(&self, api_key: &str) -> String {
        let body = format!(r#"{{"api_key":"{}"}}"#, api_key);
        let request = Request::builder()
            .method("POST")
            .uri("/auth/token")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = self.app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json = body_json(response.into_body()).await;
        format!("Bearer {}", json["token"].as_str().unwrap())
    }

    async fn make_non_root_user(&self, username: &str) -> String {
        let user_id = self.create_user(username).await;
        let api_key = self.create_api_key(&user_id).await;
        self.get_jwt(&api_key).await
    }

    async fn create_everyone_group(&self, allow_flags: &str) {
        let body = serde_json::json!({
            "name": "everyone",
            "default_allow": allow_flags,
            "default_deny": "........",
            "query_field": "is_active",
            "query_operator": "eq",
            "query_value": "true",
        });
        let request = Request::builder()
            .method("POST")
            .uri("/system/groups")
            .header("content-type", "application/json")
            .header("authorization", &self.root_jwt)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let response = self.app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    async fn set_permissions(&self, path: &str, links: serde_json::Value) {
        let permissions_body = serde_json::json!({ "links": links });
        let permissions_path = if path == "/" || path.ends_with('/') {
            format!("{}.permissions", path)
        } else {
            format!("{}/.permissions", path)
        };
        let uri = format!("/files/{}", permissions_path.trim_start_matches('/'));
        let request = Request::builder()
            .method("PUT")
            .uri(&uri)
            .header("content-type", "application/json")
            .header("authorization", &self.root_jwt)
            .body(Body::from(serde_json::to_vec(&permissions_body).unwrap()))
            .unwrap();
        let response = self.app().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "Failed to set permissions at '{}'",
            uri,
        );
    }

    async fn store_file_as_root(&self, path: &str, content: &[u8]) {
        let uri = format!("/files/{}", path.trim_start_matches('/'));
        let request = Request::builder()
            .method("PUT")
            .uri(&uri)
            .header("content-type", "application/octet-stream")
            .header("authorization", &self.root_jwt)
            .body(Body::from(content.to_vec()))
            .unwrap();
        let response = self.app().oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CREATED,
            "Failed to store file at '{}'",
            path,
        );
    }

    /// Set up "everyone" group with full crudl permissions at root.
    async fn setup_open_permissions(&self) {
        self.create_everyone_group("crudl...").await;
        self.set_permissions(
            "/",
            serde_json::json!([{"group": "everyone", "allow": "crudl...", "deny": "........"}]),
        )
        .await;
    }
}

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

async fn body_bytes(body: Body) -> Vec<u8> {
    body.collect().await.unwrap().to_bytes().to_vec()
}

async fn body_json(body: Body) -> serde_json::Value {
    let bytes = body_bytes(body).await;
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ===========================================================================
// Phase 4 tests: /.system/ access enforcement
// ===========================================================================

#[tokio::test]
async fn test_system_path_hidden_from_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness
        .store_file_as_root(".system/config/test.json", b"secret")
        .await;

    let non_root_jwt = harness.make_non_root_user("alice").await;

    let request = Request::builder()
        .method("GET")
        .uri("/files/.system/config/test.json")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_system_path_visible_to_root() {
    let harness = TestHarness::new();
    harness
        .store_file_as_root(".system/config/test.json", b"secret-data")
        .await;

    let request = Request::builder()
        .method("GET")
        .uri("/files/.system/config/test.json")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"secret-data");
}

#[tokio::test]
async fn test_system_directory_hidden_in_listing() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;

    // Store files in /.system/ and in /data/ — both are children of root.
    // We'll store a sibling file under /data/ so that /data/ directory listing works.
    // Then we use depth=-1 recursive listing from /data/ to verify no /.system/ leaks.
    // More importantly, we list the parent directory that contains both .system and data.
    harness
        .store_file_as_root(".system/config/test.json", b"hidden")
        .await;
    harness
        .store_file_as_root("data/readme.txt", b"visible")
        .await;

    let non_root_jwt = harness.make_non_root_user("bob").await;

    // List /data/ directory -- should work and not leak /.system/
    let request = Request::builder()
        .method("GET")
        .uri("/files/data/")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let listing = json.as_array().expect("listing should be an array");
    for entry in listing {
        let path = entry["path"].as_str().unwrap_or("");
        assert!(
            !path.starts_with("/.system"),
            "Non-root listing should not contain /.system paths, found: {}",
            path
        );
    }

    // Also verify that the /.system/ directory itself is blocked for GET
    let request = Request::builder()
        .method("GET")
        .uri("/files/.system/")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Non-root should not be able to list /.system/"
    );
}

#[tokio::test]
async fn test_system_directory_visible_to_root() {
    let harness = TestHarness::new();
    harness
        .store_file_as_root(".system/config/test.json", b"root-only")
        .await;

    // Root should be able to list /.system/ directory
    let request = Request::builder()
        .method("GET")
        .uri("/files/.system/")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let listing = json.as_array().expect("listing should be an array");
    assert!(
        !listing.is_empty(),
        "Root listing of /.system/ should have entries"
    );
}

#[tokio::test]
async fn test_system_put_blocked_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    let non_root_jwt = harness.make_non_root_user("charlie").await;

    let request = Request::builder()
        .method("PUT")
        .uri("/files/.system/config/evil.json")
        .header("content-type", "application/json")
        .header("authorization", &non_root_jwt)
        .body(Body::from(b"should-not-store".to_vec()))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Non-root PUT to /.system/ should return 404"
    );
}

#[tokio::test]
async fn test_system_delete_blocked_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness
        .store_file_as_root(".system/config/victim.json", b"data")
        .await;

    let non_root_jwt = harness.make_non_root_user("dave").await;

    let request = Request::builder()
        .method("DELETE")
        .uri("/files/.system/config/victim.json")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Non-root DELETE of /.system/ path should return 404"
    );

    // Verify the file is still there as root
    let request = Request::builder()
        .method("GET")
        .uri("/files/.system/config/victim.json")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "File should still exist after non-root DELETE attempt"
    );
}

#[tokio::test]
async fn test_system_head_blocked_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness
        .store_file_as_root(".system/config/metadata.json", b"meta")
        .await;

    let non_root_jwt = harness.make_non_root_user("eve").await;

    let request = Request::builder()
        .method("HEAD")
        .uri("/files/.system/config/metadata.json")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Non-root HEAD of /.system/ path should return 404"
    );
}

// ===========================================================================
// Root operations succeed
// ===========================================================================

#[tokio::test]
async fn test_system_root_put_allowed() {
    let harness = TestHarness::new();

    let request = Request::builder()
        .method("PUT")
        .uri("/files/.system/config/root-write.json")
        .header("content-type", "application/json")
        .header("authorization", &harness.root_jwt)
        .body(Body::from(b"root-data".to_vec()))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "Root PUT to /.system/ should succeed"
    );
}

#[tokio::test]
async fn test_system_root_delete_allowed() {
    let harness = TestHarness::new();
    harness
        .store_file_as_root(".system/config/to-delete.json", b"bye")
        .await;

    let request = Request::builder()
        .method("DELETE")
        .uri("/files/.system/config/to-delete.json")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Root DELETE of /.system/ path should succeed"
    );
}

#[tokio::test]
async fn test_system_head_allowed_for_root() {
    let harness = TestHarness::new();
    harness
        .store_file_as_root(".system/config/head-test.json", b"head")
        .await;

    let request = Request::builder()
        .method("HEAD")
        .uri("/files/.system/config/head-test.json")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Root HEAD of /.system/ path should succeed"
    );
}

// ===========================================================================
// Edge cases
// ===========================================================================

#[tokio::test]
async fn test_system_recursive_listing_filtered_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;

    harness
        .store_file_as_root(".system/deep/nested.json", b"hidden")
        .await;
    harness
        .store_file_as_root("data/visible.txt", b"visible")
        .await;

    let non_root_jwt = harness.make_non_root_user("frank").await;

    // Recursive listing from /data/ with depth=-1 as non-root
    let request = Request::builder()
        .method("GET")
        .uri("/files/data/?depth=-1")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let listing = json.as_array().expect("listing should be an array");

    for entry in listing {
        let path = entry["path"].as_str().unwrap_or("");
        assert!(
            !path.starts_with("/.system"),
            "Non-root recursive listing should not contain /.system paths, found: {}",
            path
        );
    }
}

#[tokio::test]
async fn test_non_system_path_accessible_to_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness
        .store_file_as_root("public/hello.txt", b"world")
        .await;

    let non_root_jwt = harness.make_non_root_user("grace").await;

    let request = Request::builder()
        .method("GET")
        .uri("/files/public/hello.txt")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Non-root should be able to access regular paths"
    );

    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"world");
}

#[tokio::test]
async fn test_system_get_returns_404_not_403() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness
        .store_file_as_root(".system/secret.bin", b"classified")
        .await;

    let non_root_jwt = harness.make_non_root_user("hank").await;

    let request = Request::builder()
        .method("GET")
        .uri("/files/.system/secret.bin")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    // Must be 404, NOT 403 -- no information leakage
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_ne!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_system_nonexistent_file_returns_404_for_both() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;

    let non_root_jwt = harness.make_non_root_user("ivan").await;

    // Non-root accessing nonexistent /.system/ file -> 404
    let request = Request::builder()
        .method("GET")
        .uri("/files/.system/does-not-exist.json")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Root accessing nonexistent /.system/ file -> also 404
    let request = Request::builder()
        .method("GET")
        .uri("/files/.system/does-not-exist.json")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_system_listing_blocked_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness
        .store_file_as_root(".system/config/a.json", b"aaa")
        .await;
    harness
        .store_file_as_root(".system/config/b.json", b"bbb")
        .await;

    let non_root_jwt = harness.make_non_root_user("julia").await;

    // Try to list /.system/config/ as non-root -> 404
    let request = Request::builder()
        .method("GET")
        .uri("/files/.system/config/")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Non-root listing of /.system/config/ should return 404"
    );
}

#[tokio::test]
async fn test_system_listing_visible_for_root() {
    let harness = TestHarness::new();
    harness
        .store_file_as_root(".system/config/a.json", b"aaa")
        .await;
    harness
        .store_file_as_root(".system/config/b.json", b"bbb")
        .await;

    // List /.system/config/ as root -> 200
    let request = Request::builder()
        .method("GET")
        .uri("/files/.system/config/")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let listing = json.as_array().expect("listing should be an array");
    assert!(
        listing.len() >= 2,
        "Root should see at least 2 files in /.system/config/"
    );
}
