use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::auth::FileAuthProvider;
use aeordb::engine::{DirectoryOps, EventBus, RequestContext, StorageEngine};
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
        // POST /system/users requires both username and email (CreateUserRequest)
        let body = format!(
            r#"{{"username":"{}","email":"{}@test.local"}}"#,
            username, username
        );
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
        // Engine reads `.aeordb-permissions` (per cache_loaders.rs); use that
        // filename at every depth.
        let permissions_path = if path == "/" || path.ends_with('/') {
            format!("{}.aeordb-permissions", path)
        } else {
            format!("{}/.aeordb-permissions", path)
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

    /// Store a file via the API (for non-.system/ paths).
    async fn store_file_via_api(&self, path: &str, content: &[u8]) {
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

    /// Store a file directly via the engine (bypassing HTTP layer).
    /// This is the ONLY way to seed .system/ data for tests, since
    /// the HTTP API blocks all .system/ writes.
    fn store_file_via_engine(&self, path: &str, content: &[u8]) {
        let ctx = RequestContext::from_claims(
            "00000000-0000-0000-0000-000000000000",
            Arc::new(EventBus::new()),
        );
        let ops = DirectoryOps::new(&self.engine);
        ops.store_file_buffered(&ctx, path, content, Some("application/octet-stream"))
            .unwrap_or_else(|e| panic!("Failed to store file at '{}': {}", path, e));
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
// 1. GET /files/.aeordb-system/* returns 404 for ALL users including root
// ===========================================================================

#[tokio::test]
async fn test_system_get_returns_404_for_root() {
    let harness = TestHarness::new();
    // Seed .system/ data directly via engine to verify the HTTP layer blocks
    // it. NOTE: must NOT write to /.aeordb-system/config/jwt_signing_key
    // because the auth provider validates that key on every construction —
    // any non-Ed25519 bytes would crash the next FileAuthProvider::new (the
    // app() helper builds a fresh provider). Pick a path that's just data.
    harness.store_file_via_engine("/.aeordb-system/users/00000000-0000-0000-0000-000000000001", b"\"opaque\"");

    let request = Request::builder()
        .method("GET")
        .uri("/files/.aeordb-system/users/00000000-0000-0000-0000-000000000001")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Root GET of /.aeordb-system/ path should return 404"
    );
}

#[tokio::test]
async fn test_system_get_returns_404_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness.store_file_via_engine("/.aeordb-system/config/test.json", b"secret");

    let non_root_jwt = harness.make_non_root_user("alice").await;

    let request = Request::builder()
        .method("GET")
        .uri("/files/.aeordb-system/config/test.json")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_system_get_returns_404_not_403() {
    let harness = TestHarness::new();
    harness.store_file_via_engine("/.aeordb-system/secret.bin", b"classified");

    // Root user should get 404, not 403
    let request = Request::builder()
        .method("GET")
        .uri("/files/.aeordb-system/secret.bin")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_ne!(response.status(), StatusCode::FORBIDDEN);
}

// ===========================================================================
// 2. GET /files/.aeordb-system/ (directory listing) returns 404 for ALL users
// ===========================================================================

#[tokio::test]
async fn test_system_directory_listing_returns_404_for_root() {
    let harness = TestHarness::new();
    harness.store_file_via_engine("/.aeordb-system/config/test.json", b"root-only");

    let request = Request::builder()
        .method("GET")
        .uri("/files/.aeordb-system/")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Root listing of /.aeordb-system/ should return 404"
    );
}

#[tokio::test]
async fn test_system_directory_listing_returns_404_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness.store_file_via_engine("/.aeordb-system/config/test.json", b"hidden");

    let non_root_jwt = harness.make_non_root_user("bob").await;

    let request = Request::builder()
        .method("GET")
        .uri("/files/.aeordb-system/")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Non-root should not be able to list /.aeordb-system/"
    );
}

// ===========================================================================
// 3. Root listing of / does NOT show .system/
// ===========================================================================

#[tokio::test]
async fn test_root_listing_hides_system_directory() {
    let harness = TestHarness::new();
    harness.store_file_via_engine("/.aeordb-system/config/test.json", b"hidden");
    harness.store_file_via_api("data/readme.txt", b"visible").await;

    // Recursive listing from data/ with depth=-1 should NOT show .system
    // (We use data/ rather than root "/" because axum's {*path} wildcard
    // requires at least one character.)
    let request = Request::builder()
        .method("GET")
        .uri("/files/data/?depth=-1")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let listing = json["items"].as_array().expect("listing should have items array");
    for entry in listing {
        let path = entry["path"].as_str().unwrap_or("");
        assert!(
            !path.starts_with("/.aeordb-system"),
            "Root listing should not contain /.system paths, found: {}",
            path
        );
    }
}

// ===========================================================================
// 4. PUT /files/.aeordb-system/* blocked for ALL users
// ===========================================================================

#[tokio::test]
async fn test_system_put_blocked_for_root() {
    let harness = TestHarness::new();

    let request = Request::builder()
        .method("PUT")
        .uri("/files/.aeordb-system/config/evil.json")
        .header("content-type", "application/json")
        .header("authorization", &harness.root_jwt)
        .body(Body::from(b"should-not-store".to_vec()))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Root PUT to /.aeordb-system/ should return 404"
    );
}

#[tokio::test]
async fn test_system_put_blocked_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    let non_root_jwt = harness.make_non_root_user("charlie").await;

    let request = Request::builder()
        .method("PUT")
        .uri("/files/.aeordb-system/config/evil.json")
        .header("content-type", "application/json")
        .header("authorization", &non_root_jwt)
        .body(Body::from(b"should-not-store".to_vec()))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Non-root PUT to /.aeordb-system/ should return 404"
    );
}

// ===========================================================================
// 5. DELETE /files/.aeordb-system/* blocked for ALL users
// ===========================================================================

#[tokio::test]
async fn test_system_delete_blocked_for_root() {
    let harness = TestHarness::new();
    harness.store_file_via_engine("/.aeordb-system/config/victim.json", b"data");

    let request = Request::builder()
        .method("DELETE")
        .uri("/files/.aeordb-system/config/victim.json")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Root DELETE of /.aeordb-system/ path should return 404"
    );
}

#[tokio::test]
async fn test_system_delete_blocked_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness.store_file_via_engine("/.aeordb-system/config/victim.json", b"data");

    let non_root_jwt = harness.make_non_root_user("dave").await;

    let request = Request::builder()
        .method("DELETE")
        .uri("/files/.aeordb-system/config/victim.json")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Non-root DELETE of /.aeordb-system/ path should return 404"
    );
}

// ===========================================================================
// 6. HEAD /files/.aeordb-system/* blocked for ALL users
// ===========================================================================

#[tokio::test]
async fn test_system_head_blocked_for_root() {
    let harness = TestHarness::new();
    harness.store_file_via_engine("/.aeordb-system/config/metadata.json", b"meta");

    let request = Request::builder()
        .method("HEAD")
        .uri("/files/.aeordb-system/config/metadata.json")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Root HEAD of /.aeordb-system/ path should return 404"
    );
}

#[tokio::test]
async fn test_system_head_blocked_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness.store_file_via_engine("/.aeordb-system/config/metadata.json", b"meta");

    let non_root_jwt = harness.make_non_root_user("eve").await;

    let request = Request::builder()
        .method("HEAD")
        .uri("/files/.aeordb-system/config/metadata.json")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Non-root HEAD of /.aeordb-system/ path should return 404"
    );
}

// ===========================================================================
// 7. Symlinks TO .system/ cannot be created (even by root)
// ===========================================================================

#[tokio::test]
async fn test_symlink_to_system_blocked_for_root() {
    let harness = TestHarness::new();

    let request = Request::builder()
        .method("PUT")
        .uri("/links/sneaky-link")
        .header("content-type", "application/json")
        .header("authorization", &harness.root_jwt)
        .body(Body::from(r#"{"target":"/.aeordb-system/config/jwt_signing_key"}"#))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Root creating symlink TO /.aeordb-system/ should return 404"
    );
}

#[tokio::test]
async fn test_symlink_to_system_blocked_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    let non_root_jwt = harness.make_non_root_user("frank").await;

    let request = Request::builder()
        .method("PUT")
        .uri("/links/sneaky-link")
        .header("content-type", "application/json")
        .header("authorization", &non_root_jwt)
        .body(Body::from(r#"{"target":"/.aeordb-system/config/jwt_signing_key"}"#))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Non-root creating symlink TO /.aeordb-system/ should return 404"
    );
}

// ===========================================================================
// 8. Symlinks AT .system/ paths cannot be created (even by root)
// ===========================================================================

#[tokio::test]
async fn test_symlink_at_system_path_blocked_for_root() {
    let harness = TestHarness::new();

    let request = Request::builder()
        .method("PUT")
        .uri("/links/.aeordb-system/config/my-link")
        .header("content-type", "application/json")
        .header("authorization", &harness.root_jwt)
        .body(Body::from(r#"{"target":"/data/public-file"}"#))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Root creating symlink AT /.aeordb-system/ should return 404"
    );
}

// ===========================================================================
// 9. Rename to/from .system/ paths blocked for ALL users
// ===========================================================================

#[tokio::test]
async fn test_rename_to_system_blocked_for_root() {
    let harness = TestHarness::new();
    harness.store_file_via_api("public/test.txt", b"data").await;

    let request = Request::builder()
        .method("POST")
        .uri("/rename/public/test.txt")
        .header("content-type", "application/json")
        .header("authorization", &harness.root_jwt)
        .body(Body::from(r#"{"to":"/.aeordb-system/stolen-data.txt"}"#))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Root renaming TO /.aeordb-system/ should return 404"
    );
}

#[tokio::test]
async fn test_rename_from_system_blocked_for_root() {
    let harness = TestHarness::new();
    harness.store_file_via_engine("/.aeordb-system/config/secret.json", b"secret");

    let request = Request::builder()
        .method("POST")
        .uri("/rename/.aeordb-system/config/secret.json")
        .header("content-type", "application/json")
        .header("authorization", &harness.root_jwt)
        .body(Body::from(r#"{"to":"/data/exfiltrated.json"}"#))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Root renaming FROM /.aeordb-system/ should return 404"
    );
}

// ===========================================================================
// 10. Query results don't include .system/ paths
// ===========================================================================

#[tokio::test]
async fn test_query_results_exclude_system_paths() {
    let harness = TestHarness::new();
    harness.store_file_via_engine("/.aeordb-system/config/key.json", b"secret-key");
    harness.store_file_via_api("data/public.txt", b"public-data").await;

    // Query with @size gt 0 -- should match both files but only return public one
    let query_body = serde_json::json!({
        "path": "/",
        "where": [{"field": "@size", "op": "gt", "value": "0"}]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/files/query")
        .header("content-type", "application/json")
        .header("authorization", &harness.root_jwt)
        .body(Body::from(serde_json::to_vec(&query_body).unwrap()))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let items = json["items"].as_array().expect("should have items array");
    for item in items {
        let path = item["path"].as_str().unwrap_or("");
        assert!(
            !path.starts_with("/.aeordb-system"),
            "Query results should not contain /.system paths, found: {}",
            path
        );
    }
}

// ===========================================================================
// 11. Recursive listing never shows .system/
// ===========================================================================

#[tokio::test]
async fn test_recursive_listing_excludes_system_for_root() {
    let harness = TestHarness::new();
    harness.store_file_via_engine("/.aeordb-system/deep/nested.json", b"hidden");
    harness.store_file_via_api("data/visible.txt", b"visible").await;

    // Recursive listing from data/ with depth=-1 as root
    // (We use data/ rather than root "/" because axum's {*path} wildcard
    // requires at least one character.)
    let request = Request::builder()
        .method("GET")
        .uri("/files/data/?depth=-1")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let listing = json["items"].as_array().expect("listing should have items array");

    for entry in listing {
        let path = entry["path"].as_str().unwrap_or("");
        assert!(
            !path.starts_with("/.aeordb-system"),
            "Root recursive listing should not contain /.system paths, found: {}",
            path
        );
    }
}

#[tokio::test]
async fn test_recursive_listing_excludes_system_for_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;

    harness.store_file_via_engine("/.aeordb-system/deep/nested.json", b"hidden");
    harness.store_file_via_api("data/visible.txt", b"visible").await;

    let non_root_jwt = harness.make_non_root_user("grace").await;

    let request = Request::builder()
        .method("GET")
        .uri("/files/data/?depth=-1")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    let listing = json["items"].as_array().expect("listing should have items array");

    for entry in listing {
        let path = entry["path"].as_str().unwrap_or("");
        assert!(
            !path.starts_with("/.aeordb-system"),
            "Non-root recursive listing should not contain /.system paths, found: {}",
            path
        );
    }
}

// ===========================================================================
// 12. Non-.system/ paths still work normally
// ===========================================================================

#[tokio::test]
async fn test_non_system_path_accessible_to_non_root() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;
    harness.store_file_via_api("public/hello.txt", b"world").await;

    let non_root_jwt = harness.make_non_root_user("hank").await;

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
async fn test_non_system_path_accessible_to_root() {
    let harness = TestHarness::new();
    harness.store_file_via_api("public/root-file.txt", b"root-data").await;

    let request = Request::builder()
        .method("GET")
        .uri("/files/public/root-file.txt")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "Root should be able to access regular paths"
    );

    let bytes = body_bytes(response.into_body()).await;
    assert_eq!(bytes, b"root-data");
}

// ===========================================================================
// 13. Nonexistent .system/ paths return 404 consistently
// ===========================================================================

#[tokio::test]
async fn test_system_nonexistent_file_returns_404_for_both() {
    let harness = TestHarness::new();
    harness.setup_open_permissions().await;

    let non_root_jwt = harness.make_non_root_user("ivan").await;

    // Non-root accessing nonexistent /.aeordb-system/ file -> 404
    let request = Request::builder()
        .method("GET")
        .uri("/files/.aeordb-system/does-not-exist.json")
        .header("authorization", &non_root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Root accessing nonexistent /.aeordb-system/ file -> also 404
    let request = Request::builder()
        .method("GET")
        .uri("/files/.aeordb-system/does-not-exist.json")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// 14. Internal system_store still works (engine-level, not HTTP)
// ===========================================================================

#[tokio::test]
async fn test_internal_engine_access_still_works() {
    let harness = TestHarness::new();

    // Store via engine (bypassing HTTP)
    harness.store_file_via_engine("/.aeordb-system/config/internal.json", b"engine-data");

    // Read via engine (bypassing HTTP) -- should succeed
    let ops = DirectoryOps::new(&harness.engine);
    let result = ops.read_file_buffered("/.aeordb-system/config/internal.json");
    assert!(result.is_ok(), "Internal engine read of .system/ should succeed");
    assert_eq!(result.unwrap(), b"engine-data");
}

// ===========================================================================
// 15. Version/file-history blocked for .system/ paths
// ===========================================================================

#[tokio::test]
async fn test_file_history_blocked_for_system_path() {
    let harness = TestHarness::new();

    let request = Request::builder()
        .method("GET")
        .uri("/versions/history/.aeordb-system/config/jwt_signing_key")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "File history for .system/ path should return 404"
    );
}

// ===========================================================================
// 16. Version/file-restore blocked for .system/ paths
// ===========================================================================

#[tokio::test]
async fn test_file_restore_blocked_for_system_path() {
    let harness = TestHarness::new();

    let request = Request::builder()
        .method("POST")
        .uri("/versions/restore/.aeordb-system/config/jwt_signing_key")
        .header("content-type", "application/json")
        .header("authorization", &harness.root_jwt)
        .body(Body::from(r#"{"snapshot":"v1"}"#))
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "File restore for .system/ path should return 404"
    );
}

// ===========================================================================
// 17. Sub-directory listing of /.aeordb-system/config/ blocked for root
// ===========================================================================

#[tokio::test]
async fn test_system_config_listing_blocked_for_root() {
    let harness = TestHarness::new();
    harness.store_file_via_engine("/.aeordb-system/config/a.json", b"aaa");
    harness.store_file_via_engine("/.aeordb-system/config/b.json", b"bbb");

    let request = Request::builder()
        .method("GET")
        .uri("/files/.aeordb-system/config/")
        .header("authorization", &harness.root_jwt)
        .body(Body::empty())
        .unwrap();

    let response = harness.app().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "Root listing of /.aeordb-system/config/ should return 404"
    );
}
