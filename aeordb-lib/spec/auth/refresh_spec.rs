use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::refresh::{generate_refresh_token, hash_refresh_token, DEFAULT_REFRESH_EXPIRY_SECONDS};
use aeordb::auth::{generate_api_key, hash_api_key, ApiKeyRecord};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::engine::{EventBus, StorageEngine, SystemTables};
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

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

/// Helper: create an API key in engine and return the plaintext key.
fn seed_api_key(engine: &StorageEngine) -> String {
  let ctx = RequestContext::system();
  let system_tables = SystemTables::new(engine);
  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: uuid::Uuid::new_v4(),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: i64::MAX,
    label: None,
    rules: vec![],
  };
  system_tables.store_api_key(&ctx, &record).unwrap();
  plaintext_key
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_token_endpoint_returns_refresh_token() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let plaintext_key = seed_api_key(&engine);

  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(format!(r#"{{"api_key":"{}"}}"#, plaintext_key)))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert!(json["token"].is_string(), "should contain a JWT");
  assert!(json["refresh_token"].is_string(), "should contain a refresh token");
  let refresh_token = json["refresh_token"].as_str().unwrap();
  assert!(
    refresh_token.starts_with("aeor_r_"),
    "refresh token should have correct prefix"
  );
}

#[tokio::test]
async fn test_refresh_returns_new_jwt() {
  let ctx = RequestContext::system();
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();

  let system_tables = SystemTables::new(&engine);
  let refresh_token = generate_refresh_token();
  let token_hash = hash_refresh_token(&refresh_token);
  let expires_at = chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);
  system_tables
    .store_refresh_token(&ctx, &token_hash, "test-user", expires_at)
    .unwrap();

  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/auth/refresh")
    .header("content-type", "application/json")
    .body(Body::from(format!(
      r#"{{"refresh_token":"{}"}}"#,
      refresh_token
    )))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert!(json["token"].is_string(), "should contain a new JWT");
  assert_eq!(json["expires_in"], DEFAULT_EXPIRY_SECONDS);
}

#[tokio::test]
async fn test_refresh_rotates_refresh_token() {
  let ctx = RequestContext::system();
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();

  let system_tables = SystemTables::new(&engine);
  let refresh_token = generate_refresh_token();
  let token_hash = hash_refresh_token(&refresh_token);
  let expires_at = chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);
  system_tables
    .store_refresh_token(&ctx, &token_hash, "rotate-user", expires_at)
    .unwrap();

  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/auth/refresh")
    .header("content-type", "application/json")
    .body(Body::from(format!(
      r#"{{"refresh_token":"{}"}}"#,
      refresh_token
    )))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let new_refresh_token = json["refresh_token"].as_str().unwrap();
  assert_ne!(new_refresh_token, refresh_token);
  assert!(new_refresh_token.starts_with("aeor_r_"));
}

#[tokio::test]
async fn test_old_refresh_token_rejected_after_rotation() {
  let ctx = RequestContext::system();
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();

  let system_tables = SystemTables::new(&engine);
  let refresh_token = generate_refresh_token();
  let token_hash = hash_refresh_token(&refresh_token);
  let expires_at = chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);
  system_tables
    .store_refresh_token(&ctx, &token_hash, "rotation-user", expires_at)
    .unwrap();

  // First refresh succeeds and rotates.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/auth/refresh")
    .header("content-type", "application/json")
    .body(Body::from(format!(
      r#"{{"refresh_token":"{}"}}"#,
      refresh_token
    )))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Using the old token again should fail.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/auth/refresh")
    .header("content-type", "application/json")
    .body(Body::from(format!(
      r#"{{"refresh_token":"{}"}}"#,
      refresh_token
    )))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_expired_refresh_token_rejected() {
  let ctx = RequestContext::system();
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();

  let system_tables = SystemTables::new(&engine);
  let refresh_token = generate_refresh_token();
  let token_hash = hash_refresh_token(&refresh_token);
  let expires_at = chrono::Utc::now() - chrono::Duration::hours(1);
  system_tables
    .store_refresh_token(&ctx, &token_hash, "expired-user", expires_at)
    .unwrap();

  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/auth/refresh")
    .header("content-type", "application/json")
    .body(Body::from(format!(
      r#"{{"refresh_token":"{}"}}"#,
      refresh_token
    )))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_invalid_refresh_token_rejected() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/refresh")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"refresh_token":"aeor_r_this_is_not_a_real_token"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_refresh_missing_body_field() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/refresh")
    .header("content-type", "application/json")
    .body(Body::from(r#"{}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_full_refresh_flow_from_api_key() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let plaintext_key = seed_api_key(&engine);

  // Step 1: Get initial token + refresh token.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(format!(r#"{{"api_key":"{}"}}"#, plaintext_key)))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let initial_refresh_token = json["refresh_token"].as_str().unwrap().to_string();

  // Step 2: Use refresh token to get new JWT.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/auth/refresh")
    .header("content-type", "application/json")
    .body(Body::from(format!(
      r#"{{"refresh_token":"{}"}}"#,
      initial_refresh_token
    )))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let new_jwt = json["token"].as_str().unwrap().to_string();
  let new_refresh_token = json["refresh_token"].as_str().unwrap().to_string();

  assert_ne!(new_refresh_token, initial_refresh_token);

  let claims = jwt_manager.verify_token(&new_jwt);
  assert!(claims.is_ok(), "new JWT should be valid");
}

#[tokio::test]
async fn test_revoked_refresh_token_rejected() {
  let ctx = RequestContext::system();
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();

  let system_tables = SystemTables::new(&engine);
  let refresh_token = generate_refresh_token();
  let token_hash = hash_refresh_token(&refresh_token);
  let expires_at = chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);
  system_tables
    .store_refresh_token(&ctx, &token_hash, "revoke-test-user", expires_at)
    .unwrap();

  system_tables.revoke_refresh_token(&ctx, &token_hash).unwrap();

  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/auth/refresh")
    .header("content-type", "application/json")
    .body(Body::from(format!(
      r#"{{"refresh_token":"{}"}}"#,
      refresh_token
    )))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
