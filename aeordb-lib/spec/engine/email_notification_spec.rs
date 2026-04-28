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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, Arc<RateLimiter>, tempfile::TempDir) {
    let jwt_manager = Arc::new(JwtManager::generate());
    let (engine, temp_dir) = create_temp_engine_for_tests();
    let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
    let rate_limiter = Arc::new(RateLimiter::default_config());
    let auth_provider: Arc<dyn aeordb::auth::AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
    let app = create_app_with_all(
        auth_provider,
        jwt_manager.clone(),
        plugin_manager,
        rate_limiter.clone(),
        make_prometheus_handle(),
        engine.clone(),
        Arc::new(EventBus::new()),
        CorsState { default_origins: None, rules: vec![] },
    );
    (app, jwt_manager, engine, rate_limiter, temp_dir)
}

fn rebuild_app(
    jwt_manager: &Arc<JwtManager>,
    engine: &Arc<StorageEngine>,
    rate_limiter: &Arc<RateLimiter>,
) -> axum::Router {
    let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
    let auth_provider: Arc<dyn aeordb::auth::AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
    create_app_with_all(
        auth_provider,
        jwt_manager.clone(),
        plugin_manager,
        rate_limiter.clone(),
        make_prometheus_handle(),
        engine.clone(),
        Arc::new(EventBus::new()),
        CorsState { default_origins: None, rules: vec![] },
    )
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
    serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ===========================================================================
// User model tests (email is now a required field)
// ===========================================================================

#[tokio::test]
async fn create_user_without_email_fails() {
    let (app, jwt_manager, _, _, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Send only username, omit email -- should fail because email is required
    let request = Request::builder()
        .method("POST")
        .uri("/system/users")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"username":"bob"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "Expected 400 or 422 for missing email, got {}",
        status,
    );
}

#[tokio::test]
async fn create_user_with_email_succeeds() {
    let (app, jwt_manager, _, _, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("POST")
        .uri("/system/users")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"username":"bob","email":"bob@test.com"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["username"], "bob");
    assert_eq!(json["email"], "bob@test.com");
}

#[tokio::test]
async fn two_users_same_email_succeeds() {
    let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Create first user with shared email
    let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
    let request = Request::builder()
        .method("POST")
        .uri("/system/users")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"username":"user_a","email":"same@test.com"}"#))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED, "First user with same@test.com should succeed");

    // Create second user with the same email
    let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
    let request = Request::builder()
        .method("POST")
        .uri("/system/users")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"username":"user_b","email":"same@test.com"}"#))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED, "Second user with same@test.com should also succeed");
}

// ===========================================================================
// Email config tests (via HTTP)
// ===========================================================================

#[tokio::test]
async fn save_smtp_config() {
    let (app, jwt_manager, engine, _rate_limiter, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let smtp_config = serde_json::json!({
        "provider": "smtp",
        "host": "smtp.example.com",
        "port": 587,
        "username": "user",
        "password": "secret",
        "from_address": "test@example.com",
        "from_name": "Test",
        "tls": "starttls"
    });

    let request = Request::builder()
        .method("PUT")
        .uri("/system/email-config")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&smtp_config).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["saved"], true);
    assert_eq!(json["provider"], "smtp");
    assert_eq!(json["from_address"], "test@example.com");

    // Verify storage: read the config back from /.aeordb-system/email-config.json
    let ops = aeordb::engine::DirectoryOps::new(&engine);
    let data = ops.read_file("/.aeordb-system/email-config.json").expect("email config should be stored");
    let stored: serde_json::Value = serde_json::from_slice(&data).expect("stored config should be valid JSON");
    assert_eq!(stored["provider"], "smtp");
    assert_eq!(stored["host"], "smtp.example.com");
    assert_eq!(stored["password"], "secret", "Raw password should be stored (not masked) in the file");
}

#[tokio::test]
async fn save_oauth_config() {
    let (app, jwt_manager, _, _, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let oauth_config = serde_json::json!({
        "provider": "oauth",
        "oauth_provider": "google",
        "client_id": "my-client-id",
        "client_secret": "my-client-secret",
        "refresh_token": "my-refresh-token",
        "from_address": "noreply@example.com",
        "from_name": "AeorDB"
    });

    let request = Request::builder()
        .method("PUT")
        .uri("/system/email-config")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&oauth_config).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["saved"], true);
    assert_eq!(json["provider"], "oauth");
    assert_eq!(json["from_address"], "noreply@example.com");
}

#[tokio::test]
async fn get_config_masks_secrets() {
    let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // First, save an SMTP config
    let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
    let smtp_config = serde_json::json!({
        "provider": "smtp",
        "host": "smtp.example.com",
        "port": 587,
        "username": "user",
        "password": "supersecret",
        "from_address": "test@example.com",
        "from_name": "Test",
        "tls": "starttls"
    });

    let request = Request::builder()
        .method("PUT")
        .uri("/system/email-config")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&smtp_config).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Now GET the config -- password should be masked
    let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
    let request = Request::builder()
        .method("GET")
        .uri("/system/email-config")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["configured"], true);
    assert_eq!(json["password"], "--------", "Password should be masked with dashes");
    assert_eq!(json["host"], "smtp.example.com", "Non-secret fields should be preserved");
    assert_eq!(json["username"], "user", "Username should not be masked");
    // Verify the actual secret is NOT present
    let body_str = serde_json::to_string(&json).unwrap();
    assert!(!body_str.contains("supersecret"), "Raw password should not appear in GET response");
}

#[tokio::test]
async fn get_config_not_configured() {
    let (app, jwt_manager, _, _, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // GET without saving any config first
    let request = Request::builder()
        .method("GET")
        .uri("/system/email-config")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["configured"], false);
}

#[tokio::test]
async fn config_requires_root_get() {
    let (app, jwt_manager, _, _, _temp_dir) = test_app();
    let auth = non_root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("GET")
        .uri("/system/email-config")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn config_requires_root_put() {
    let (app, jwt_manager, _, _, _temp_dir) = test_app();
    let auth = non_root_bearer_token(&jwt_manager);

    let smtp_config = serde_json::json!({
        "provider": "smtp",
        "host": "smtp.example.com",
        "port": 587,
        "username": "user",
        "password": "secret",
        "from_address": "test@example.com",
        "from_name": "Test",
        "tls": "starttls"
    });

    let request = Request::builder()
        .method("PUT")
        .uri("/system/email-config")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&smtp_config).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_email_requires_config() {
    let (app, jwt_manager, _, _, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // POST /system/email-test without any config saved
    let request = Request::builder()
        .method("POST")
        .uri("/system/email-test")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"to":"someone@example.com"}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let json = body_json(response.into_body()).await;
    let error_msg = json["error"].as_str().unwrap_or("");
    assert!(
        error_msg.contains("not configured"),
        "Expected error about email not configured, got: {}",
        error_msg,
    );
}

// ===========================================================================
// Email template unit tests
// ===========================================================================

#[test]
fn template_produces_valid_content() {
    let (subject, html, text) = aeordb::engine::email_template::build_share_notification(
        "Alice",
        &["/photos/".to_string()],
        "crudl...",
        "http://example.com",
    );

    // Subject contains sharer name
    assert!(
        subject.contains("Alice"),
        "Subject should contain 'Alice', got: {}",
        subject,
    );

    // HTML body checks
    assert!(
        html.contains("Alice"),
        "HTML should contain 'Alice'",
    );
    assert!(
        html.contains("/photos/"),
        "HTML should contain '/photos/'",
    );
    assert!(
        html.contains("Can edit"),
        "HTML should contain 'Can edit' for crudl... permissions",
    );
    assert!(
        html.contains("View Files"),
        "HTML should contain 'View Files' CTA link",
    );
    assert!(
        html.contains("http://example.com"),
        "HTML should contain the portal URL",
    );

    // Text fallback checks
    assert!(
        text.contains("Alice"),
        "Text fallback should contain 'Alice'",
    );
    assert!(
        text.contains("/photos/"),
        "Text fallback should contain '/photos/'",
    );
    assert!(
        text.contains("Can edit"),
        "Text fallback should contain 'Can edit'",
    );
}

// ===========================================================================
// Additional edge case tests
// ===========================================================================

#[test]
fn template_view_only_permissions() {
    let (_, html, text) = aeordb::engine::email_template::build_share_notification(
        "Bob",
        &["/docs/readme.txt".to_string()],
        "cr..l...",
        "http://example.com",
    );
    assert!(html.contains("View only"), "cr..l... should map to 'View only'");
    assert!(text.contains("View only"));
}

#[test]
fn template_full_access_permissions() {
    let (_, html, text) = aeordb::engine::email_template::build_share_notification(
        "Admin",
        &["/everything/".to_string()],
        "crudlify",
        "http://example.com",
    );
    assert!(html.contains("Full access"), "crudlify should map to 'Full access'");
    assert!(text.contains("Full access"));
}

#[test]
fn template_unknown_permissions_shows_raw() {
    let (_, html, _) = aeordb::engine::email_template::build_share_notification(
        "X",
        &["/test".to_string()],
        "cr..l.f.",
        "http://example.com",
    );
    assert!(html.contains("cr..l.f."), "Unknown permission string should be shown as-is");
}

#[test]
fn template_multiple_paths() {
    let paths = vec![
        "/photos/vacation/".to_string(),
        "/docs/report.pdf".to_string(),
        "/music/song.mp3".to_string(),
    ];
    let (_, html, text) = aeordb::engine::email_template::build_share_notification(
        "Sharer",
        &paths,
        "crudl...",
        "http://example.com",
    );
    for path in &paths {
        assert!(html.contains(path.as_str()), "HTML should contain path: {}", path);
        assert!(text.contains(path.as_str()), "Text should contain path: {}", path);
    }
}

#[test]
fn template_html_escapes_malicious_input() {
    let (_, html, _) = aeordb::engine::email_template::build_share_notification(
        "<script>alert('xss')</script>",
        &["<img onerror=hack>".to_string()],
        "crudlify",
        "http://evil.com?a=1&b=2",
    );
    assert!(!html.contains("<script>"), "HTML should escape script tags in sharer name");
    assert!(!html.contains("<img"), "HTML should escape img tags in paths");
    assert!(html.contains("&amp;"), "HTML should escape ampersands in URLs");
}

#[tokio::test]
async fn put_email_config_invalid_json_fails() {
    let (app, jwt_manager, _, _, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("PUT")
        .uri("/system/email-config")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"not valid json"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "Expected 400 or 422 for invalid JSON, got {}",
        status,
    );
}

#[tokio::test]
async fn put_email_config_missing_provider_fails() {
    let (app, jwt_manager, _, _, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    let request = Request::builder()
        .method("PUT")
        .uri("/system/email-config")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(r#"{"host":"smtp.example.com","port":587}"#))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "Expected 400 or 422 for missing provider field, got {}",
        status,
    );
}

#[tokio::test]
async fn get_config_no_auth_returns_401() {
    let (app, _, _, _, _temp_dir) = test_app();

    let request = Request::builder()
        .method("GET")
        .uri("/system/email-config")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn get_oauth_config_masks_all_secrets() {
    let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
    let auth = root_bearer_token(&jwt_manager);

    // Save an OAuth config
    let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
    let oauth_config = serde_json::json!({
        "provider": "oauth",
        "oauth_provider": "google",
        "client_id": "my-client-id",
        "client_secret": "top-secret-client",
        "refresh_token": "top-secret-refresh",
        "from_address": "noreply@example.com",
        "from_name": "AeorDB"
    });

    let request = Request::builder()
        .method("PUT")
        .uri("/system/email-config")
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body(Body::from(serde_json::to_vec(&oauth_config).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // GET and verify both client_secret and refresh_token are masked
    let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
    let request = Request::builder()
        .method("GET")
        .uri("/system/email-config")
        .header("authorization", &auth)
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response.into_body()).await;
    assert_eq!(json["configured"], true);
    assert_eq!(json["client_secret"], "--------", "client_secret should be masked");
    assert_eq!(json["refresh_token"], "--------", "refresh_token should be masked");
    assert_eq!(json["client_id"], "my-client-id", "client_id should not be masked");

    let body_str = serde_json::to_string(&json).unwrap();
    assert!(!body_str.contains("top-secret"), "No raw secrets should appear in GET response");
}
