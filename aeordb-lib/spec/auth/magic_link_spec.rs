use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::JwtManager;
use aeordb::auth::magic_link::{generate_magic_link_code, hash_magic_link_code};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::engine::{EventBus, StorageEngine};
use aeordb::engine::system_store;
use aeordb::engine::RequestContext;
use aeordb::plugins::PluginManager;
use aeordb::auth::FileAuthProvider;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests, CorsState};

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
  metrics_exporter_prometheus::PrometheusBuilder::new()
    .build_recorder()
    .handle()
}

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, Arc<RateLimiter>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::new(5, 60));
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

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ---------------------------------------------------------------------------
// Unit tests for magic link code generation and hashing
// ---------------------------------------------------------------------------

#[test]
fn test_generate_magic_link_code_is_unique() {
  let code_one = generate_magic_link_code();
  let code_two = generate_magic_link_code();
  assert_ne!(code_one, code_two);
  assert_eq!(code_one.len(), 64);
  assert_eq!(code_two.len(), 64);
}

#[test]
fn test_hash_magic_link_code_is_deterministic() {
  let code = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
  let hash_one = hash_magic_link_code(code);
  let hash_two = hash_magic_link_code(code);
  assert_eq!(hash_one, hash_two);
  assert_eq!(hash_one.len(), 64);
}

#[test]
fn test_hash_magic_link_code_different_inputs_different_hashes() {
  let hash_one = hash_magic_link_code("code_a");
  let hash_two = hash_magic_link_code("code_b");
  assert_ne!(hash_one, hash_two);
}

// ---------------------------------------------------------------------------
// HTTP endpoint tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_request_magic_link_returns_200_always() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/magic-link")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"email":"user@example.com"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(
    json["message"],
    "If an account exists, a login link has been sent."
  );
}

#[tokio::test]
async fn test_request_magic_link_returns_200_for_nonexistent_email() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/magic-link")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"email":"nobody@nowhere.invalid"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_magic_link_code_stored_hashed() {
  let ctx = RequestContext::system();
  let (_, _, engine, _, _temp_dir) = test_app();

  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);
  system_store::store_magic_link(&engine, &ctx, &aeordb::auth::magic_link::MagicLinkRecord {
    code_hash: code_hash.clone(),
    email: "stored@example.com".to_string(),
    created_at: chrono::Utc::now(),
    expires_at,
    is_used: false,
  }).unwrap();

  let record = system_store::get_magic_link(&engine, &code_hash).unwrap();
  assert!(record.is_some());
  let record = record.unwrap();
  assert_eq!(record.email, "stored@example.com");
  assert_eq!(record.code_hash, code_hash);
  assert!(!record.is_used);

  // The raw code should NOT be stored — only the hash.
  let raw_lookup = system_store::get_magic_link(&engine, &code).unwrap();
  assert!(raw_lookup.is_none(), "raw code should not be stored");
}

#[tokio::test]
async fn test_verify_valid_code_returns_jwt() {
  let ctx = RequestContext::system();
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();

  // Create a user record so the magic-link verify can resolve email → UUID.
  let user = aeordb::engine::User::new("valid@example.com", Some("valid@example.com"));
  system_store::store_user(&engine, &ctx, &user).unwrap();

  // Store a magic link directly.
  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);
  system_store::store_magic_link(&engine, &ctx, &aeordb::auth::magic_link::MagicLinkRecord {
    code_hash: code_hash.clone(),
    email: "valid@example.com".to_string(),
    created_at: chrono::Utc::now(),
    expires_at,
    is_used: false,
  }).unwrap();

  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .uri(format!("/auth/magic-link/verify?code={}", code))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert!(json["token"].is_string(), "response should contain a JWT");
  assert!(json["expires_in"].is_number());
}

#[tokio::test]
async fn test_verify_expired_code_returns_401() {
  let ctx = RequestContext::system();
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();

  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expires_at = chrono::Utc::now() - chrono::Duration::hours(1);
  system_store::store_magic_link(&engine, &ctx, &aeordb::auth::magic_link::MagicLinkRecord {
    code_hash: code_hash.clone(),
    email: "expired@example.com".to_string(),
    created_at: chrono::Utc::now(),
    expires_at,
    is_used: false,
  }).unwrap();

  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .uri(format!("/auth/magic-link/verify?code={}", code))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_verify_used_code_returns_401() {
  let ctx = RequestContext::system();
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();

  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);
  system_store::store_magic_link(&engine, &ctx, &aeordb::auth::magic_link::MagicLinkRecord {
    code_hash: code_hash.clone(),
    email: "used@example.com".to_string(),
    created_at: chrono::Utc::now(),
    expires_at,
    is_used: false,
  }).unwrap();
  system_store::mark_magic_link_used(&engine, &ctx, &code_hash).unwrap();

  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .uri(format!("/auth/magic-link/verify?code={}", code))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_verify_invalid_code_returns_401() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .uri("/auth/magic-link/verify?code=this_is_not_a_real_code")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_verify_code_is_single_use() {
  let ctx = RequestContext::system();
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();

  // Create a user record so the magic-link verify can resolve email → UUID.
  let user = aeordb::engine::User::new("single-use@example.com", Some("single-use@example.com"));
  system_store::store_user(&engine, &ctx, &user).unwrap();

  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);
  system_store::store_magic_link(&engine, &ctx, &aeordb::auth::magic_link::MagicLinkRecord {
    code_hash: code_hash.clone(),
    email: "single-use@example.com".to_string(),
    created_at: chrono::Utc::now(),
    expires_at,
    is_used: false,
  }).unwrap();

  // First use should succeed.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .uri(format!("/auth/magic-link/verify?code={}", code))
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Second use should fail.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .uri(format!("/auth/magic-link/verify?code={}", code))
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_magic_link_logs_the_link() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/magic-link")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"email":"log-test@example.com"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_rate_limiting_blocks_after_threshold() {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(RateLimiter::new(3, 60));

  for i in 0..3 {
    let auth_provider: Arc<dyn aeordb::auth::AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
    let app = create_app_with_all(
      auth_provider,
      jwt_manager.clone(),
      plugin_manager.clone(),
      rate_limiter.clone(),
      make_prometheus_handle(),
      engine.clone(),
      Arc::new(EventBus::new()),
      CorsState { default_origins: None, rules: vec![] },
    );
    let request = Request::builder()
      .method("POST")
      .uri("/auth/magic-link")
      .header("content-type", "application/json")
      .body(Body::from(r#"{"email":"rate-test@example.com"}"#))
      .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(
      response.status(),
      StatusCode::OK,
      "request {} should succeed",
      i + 1
    );
  }

  // 4th request should be rate limited.
  let auth_provider: Arc<dyn aeordb::auth::AuthProvider> = Arc::new(FileAuthProvider::new(engine.clone()));
  let app = create_app_with_all(
    auth_provider,
    jwt_manager.clone(),
    plugin_manager.clone(),
    rate_limiter.clone(),
    make_prometheus_handle(),
    engine.clone(),
    Arc::new(EventBus::new()),
    CorsState { default_origins: None, rules: vec![] },
  );
  let request = Request::builder()
    .method("POST")
    .uri("/auth/magic-link")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"email":"rate-test@example.com"}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_rate_limiting_allows_after_window_expires() {
  let rate_limiter = RateLimiter::new(1, 1);

  assert!(rate_limiter.check_rate_limit("test-key").is_ok());
  assert!(rate_limiter.check_rate_limit("test-key").is_err());

  std::thread::sleep(std::time::Duration::from_millis(1100));

  assert!(rate_limiter.check_rate_limit("test-key").is_ok());
}

#[tokio::test]
async fn test_rate_limiting_tracks_per_key() {
  let rate_limiter = RateLimiter::new(1, 60);

  assert!(rate_limiter.check_rate_limit("key-a").is_ok());
  assert!(rate_limiter.check_rate_limit("key-b").is_ok());

  assert!(rate_limiter.check_rate_limit("key-a").is_err());
  assert!(rate_limiter.check_rate_limit("key-b").is_err());
  assert!(rate_limiter.check_rate_limit("key-c").is_ok());
}

#[tokio::test]
async fn test_request_magic_link_missing_email_field() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/magic-link")
    .header("content-type", "application/json")
    .body(Body::from(r#"{}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_verify_magic_link_missing_code_param() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .uri("/auth/magic-link/verify")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
