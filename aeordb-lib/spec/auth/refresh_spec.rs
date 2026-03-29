use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::refresh::{generate_refresh_token, hash_refresh_token, DEFAULT_REFRESH_EXPIRY_SECONDS};
use aeordb::auth::{generate_api_key, hash_api_key, ApiKeyRecord};
use aeordb::plugins::PluginManager;
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::filesystem::PathResolver;
use aeordb::server::create_app_with_all;
use aeordb::storage::{ChunkStore, RedbStorage};

fn make_path_resolver(storage: &Arc<RedbStorage>) -> Arc<PathResolver> {
  let database_arc = storage.database_arc();
  let chunk_store = ChunkStore::new_with_redb(database_arc.clone());
  Arc::new(PathResolver::new(database_arc, chunk_store))
}

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<RedbStorage>, Arc<RateLimiter>) {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = Arc::new(JwtManager::generate());
  let plugin_manager = Arc::new(PluginManager::new(storage.database_arc()));
  let rate_limiter = Arc::new(RateLimiter::default_config());
  let path_resolver = make_path_resolver(&storage);
  let app = create_app_with_all(
    storage.clone(),
    jwt_manager.clone(),
    plugin_manager,
    rate_limiter.clone(),
    path_resolver,
  );
  (app, jwt_manager, storage, rate_limiter)
}

fn rebuild_app(
  storage: &Arc<RedbStorage>,
  jwt_manager: &Arc<JwtManager>,
  rate_limiter: &Arc<RateLimiter>,
) -> axum::Router {
  let plugin_manager = Arc::new(PluginManager::new(storage.database_arc()));
  let path_resolver = make_path_resolver(storage);
  create_app_with_all(
    storage.clone(),
    jwt_manager.clone(),
    plugin_manager,
    rate_limiter.clone(),
    path_resolver,
  )
}

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

/// Helper: create an API key in storage and return the plaintext key.
fn seed_api_key(storage: &RedbStorage) -> String {
  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    roles: vec!["admin".to_string()],
    created_at: chrono::Utc::now(),
    is_revoked: false,
  };
  storage.store_api_key(&record).unwrap();
  plaintext_key
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_token_endpoint_returns_refresh_token() {
  let (_, jwt_manager, storage, rate_limiter) = test_app();
  let plaintext_key = seed_api_key(&storage);

  let app = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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
  let (_, jwt_manager, storage, rate_limiter) = test_app();

  // Seed a refresh token directly.
  let refresh_token = generate_refresh_token();
  let token_hash = hash_refresh_token(&refresh_token);
  let expires_at = chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);
  storage
    .store_refresh_token(&token_hash, "test-user", expires_at)
    .unwrap();

  let app = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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
  let (_, jwt_manager, storage, rate_limiter) = test_app();

  let refresh_token = generate_refresh_token();
  let token_hash = hash_refresh_token(&refresh_token);
  let expires_at = chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);
  storage
    .store_refresh_token(&token_hash, "rotate-user", expires_at)
    .unwrap();

  let app = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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

  // New refresh token should be different from the old one.
  assert_ne!(new_refresh_token, refresh_token);
  assert!(new_refresh_token.starts_with("aeor_r_"));
}

#[tokio::test]
async fn test_old_refresh_token_rejected_after_rotation() {
  let (_, jwt_manager, storage, rate_limiter) = test_app();

  let refresh_token = generate_refresh_token();
  let token_hash = hash_refresh_token(&refresh_token);
  let expires_at = chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);
  storage
    .store_refresh_token(&token_hash, "rotation-user", expires_at)
    .unwrap();

  // First refresh succeeds and rotates.
  let app = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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
  let app = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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
  let (_, jwt_manager, storage, rate_limiter) = test_app();

  let refresh_token = generate_refresh_token();
  let token_hash = hash_refresh_token(&refresh_token);
  // Expired 1 hour ago.
  let expires_at = chrono::Utc::now() - chrono::Duration::hours(1);
  storage
    .store_refresh_token(&token_hash, "expired-user", expires_at)
    .unwrap();

  let app = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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
  let (app, _, _, _) = test_app();

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
  let (app, _, _, _) = test_app();

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
  let (_, jwt_manager, storage, rate_limiter) = test_app();
  let plaintext_key = seed_api_key(&storage);

  // Step 1: Get initial token + refresh token.
  let app = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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
  let _initial_jwt = json["token"].as_str().unwrap().to_string();

  // Step 2: Use refresh token to get new JWT.
  let app = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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

  // New refresh token should be different from the old one.
  assert_ne!(new_refresh_token, initial_refresh_token);

  // New JWT should be valid and verifiable.
  let claims = jwt_manager.verify_token(&new_jwt);
  assert!(claims.is_ok(), "new JWT should be valid");
}

#[tokio::test]
async fn test_revoked_refresh_token_rejected() {
  let (_, jwt_manager, storage, rate_limiter) = test_app();

  let refresh_token = generate_refresh_token();
  let token_hash = hash_refresh_token(&refresh_token);
  let expires_at = chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_REFRESH_EXPIRY_SECONDS);
  storage
    .store_refresh_token(&token_hash, "revoke-test-user", expires_at)
    .unwrap();

  // Manually revoke it.
  storage.revoke_refresh_token(&token_hash).unwrap();

  let app = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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
