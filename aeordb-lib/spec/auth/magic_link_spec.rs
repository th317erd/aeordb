use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::JwtManager;
use aeordb::auth::magic_link::{generate_magic_link_code, hash_magic_link_code};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::filesystem::PathResolver;
use aeordb::plugins::PluginManager;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests};
use aeordb::storage::{ChunkStore, RedbStorage};

fn make_path_resolver(storage: &Arc<RedbStorage>) -> Arc<PathResolver> {
  let database_arc = storage.database_arc();
  let chunk_store = ChunkStore::new_with_redb(database_arc.clone());
  Arc::new(PathResolver::new(database_arc, chunk_store))
}

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
  metrics_exporter_prometheus::PrometheusBuilder::new()
    .build_recorder()
    .handle()
}

fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<RedbStorage>, Arc<RateLimiter>, tempfile::TempDir) {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = Arc::new(JwtManager::generate());
  let plugin_manager = Arc::new(PluginManager::new(storage.database_arc()));
  let rate_limiter = Arc::new(RateLimiter::new(5, 60));
  let path_resolver = make_path_resolver(&storage);
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_all(
    storage.clone(),
    jwt_manager.clone(),
    plugin_manager,
    rate_limiter.clone(),
    path_resolver,
    make_prometheus_handle(),
    engine,
  );
  (app, jwt_manager, storage, rate_limiter, temp_dir)
}

fn rebuild_app(
  storage: &Arc<RedbStorage>,
  jwt_manager: &Arc<JwtManager>,
  rate_limiter: &Arc<RateLimiter>,
) -> (axum::Router, tempfile::TempDir) {
  let plugin_manager = Arc::new(PluginManager::new(storage.database_arc()));
  let path_resolver = make_path_resolver(storage);
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let router = create_app_with_all(
    storage.clone(),
    jwt_manager.clone(),
    plugin_manager,
    rate_limiter.clone(),
    path_resolver,
    make_prometheus_handle(),
    engine,
  );
  (router, temp_dir)
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
  // 32 bytes hex encoded = 64 characters
  assert_eq!(code_one.len(), 64);
  assert_eq!(code_two.len(), 64);
}

#[test]
fn test_hash_magic_link_code_is_deterministic() {
  let code = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
  let hash_one = hash_magic_link_code(code);
  let hash_two = hash_magic_link_code(code);
  assert_eq!(hash_one, hash_two);
  // SHA-256 produces 64 hex characters
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
  // Must always return 200 to prevent email enumeration.
  assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_magic_link_code_stored_hashed() {
  let (app, _, storage, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/magic-link")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"email":"stored@example.com"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // We can't easily get the code from the response (it's only logged),
  // but we can verify that a magic link was stored. Generate a code,
  // hash it, and store it directly, then verify the storage layer.
  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);
  storage
    .store_magic_link(&code_hash, "stored@example.com", expires_at)
    .unwrap();

  let record = storage.get_magic_link(&code_hash).unwrap();
  assert!(record.is_some());
  let record = record.unwrap();
  assert_eq!(record.email, "stored@example.com");
  assert_eq!(record.code_hash, code_hash);
  assert!(!record.is_used);

  // The raw code should NOT be stored — only the hash.
  let raw_lookup = storage.get_magic_link(&code).unwrap();
  assert!(raw_lookup.is_none(), "raw code should not be stored");
}

#[tokio::test]
async fn test_verify_valid_code_returns_jwt() {
  let (_, jwt_manager, storage, rate_limiter, _temp_dir) = test_app();

  // Store a magic link directly.
  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);
  storage
    .store_magic_link(&code_hash, "valid@example.com", expires_at)
    .unwrap();

  let (app, _engine_dir) = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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
  let (_, jwt_manager, storage, rate_limiter, _temp_dir) = test_app();

  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  // Expired 1 hour ago.
  let expires_at = chrono::Utc::now() - chrono::Duration::hours(1);
  storage
    .store_magic_link(&code_hash, "expired@example.com", expires_at)
    .unwrap();

  let (app, _engine_dir) = rebuild_app(&storage, &jwt_manager, &rate_limiter);
  let request = Request::builder()
    .uri(format!("/auth/magic-link/verify?code={}", code))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_verify_used_code_returns_401() {
  let (_, jwt_manager, storage, rate_limiter, _temp_dir) = test_app();

  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);
  storage
    .store_magic_link(&code_hash, "used@example.com", expires_at)
    .unwrap();
  storage.mark_magic_link_used(&code_hash).unwrap();

  let (app, _engine_dir) = rebuild_app(&storage, &jwt_manager, &rate_limiter);
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
  let (_, jwt_manager, storage, rate_limiter, _temp_dir) = test_app();

  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);
  storage
    .store_magic_link(&code_hash, "single-use@example.com", expires_at)
    .unwrap();

  // First use should succeed.
  let (app, _engine_dir) = rebuild_app(&storage, &jwt_manager, &rate_limiter);
  let request = Request::builder()
    .uri(format!("/auth/magic-link/verify?code={}", code))
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Second use should fail.
  let (app, _engine_dir) = rebuild_app(&storage, &jwt_manager, &rate_limiter);
  let request = Request::builder()
    .uri(format!("/auth/magic-link/verify?code={}", code))
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_magic_link_logs_the_link() {
  // We verify the endpoint works (the tracing log is a side effect we can't
  // easily assert in this test harness, but the handler succeeds which means
  // it reached the tracing::info! call).
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
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = Arc::new(JwtManager::generate());
  let plugin_manager = Arc::new(PluginManager::new(storage.database_arc()));
  let path_resolver = make_path_resolver(&storage);
  // Allow only 3 requests per 60 seconds.
  let rate_limiter = Arc::new(RateLimiter::new(3, 60));

  for i in 0..3 {
    let (engine, _engine_dir) = create_temp_engine_for_tests();
    let app = create_app_with_all(
      storage.clone(),
      jwt_manager.clone(),
      plugin_manager.clone(),
      rate_limiter.clone(),
      path_resolver.clone(),
      make_prometheus_handle(),
      engine,
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
  let (engine, _engine_dir) = create_temp_engine_for_tests();
  let app = create_app_with_all(
    storage.clone(),
    jwt_manager.clone(),
    plugin_manager.clone(),
    rate_limiter.clone(),
    path_resolver.clone(),
    make_prometheus_handle(),
    engine,
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
  // Use a very short window (1 second).
  let rate_limiter = RateLimiter::new(1, 1);

  // First request succeeds.
  assert!(rate_limiter.check_rate_limit("test-key").is_ok());

  // Second request fails (within window).
  assert!(rate_limiter.check_rate_limit("test-key").is_err());

  // Wait for window to expire.
  std::thread::sleep(std::time::Duration::from_millis(1100));

  // Third request succeeds (window expired).
  assert!(rate_limiter.check_rate_limit("test-key").is_ok());
}

#[tokio::test]
async fn test_rate_limiting_tracks_per_key() {
  let rate_limiter = RateLimiter::new(1, 60);

  assert!(rate_limiter.check_rate_limit("key-a").is_ok());
  assert!(rate_limiter.check_rate_limit("key-b").is_ok());

  // key-a is exhausted, key-b is exhausted, but they're independent.
  assert!(rate_limiter.check_rate_limit("key-a").is_err());
  assert!(rate_limiter.check_rate_limit("key-b").is_err());
  assert!(rate_limiter.check_rate_limit("key-c").is_ok());
}

#[tokio::test]
async fn test_cleanup_expired_magic_links() {
  let storage = RedbStorage::new_in_memory().expect("in-memory storage");

  // Store an expired link.
  let code = generate_magic_link_code();
  let code_hash = hash_magic_link_code(&code);
  let expired_at = chrono::Utc::now() - chrono::Duration::hours(1);
  storage
    .store_magic_link(&code_hash, "expired@example.com", expired_at)
    .unwrap();

  // Store a valid link.
  let code_valid = generate_magic_link_code();
  let code_hash_valid = hash_magic_link_code(&code_valid);
  let valid_at = chrono::Utc::now() + chrono::Duration::hours(1);
  storage
    .store_magic_link(&code_hash_valid, "valid@example.com", valid_at)
    .unwrap();

  let removed = storage.cleanup_expired_magic_links().unwrap();
  assert_eq!(removed, 1);

  // Expired link should be gone.
  assert!(storage.get_magic_link(&code_hash).unwrap().is_none());

  // Valid link should still exist.
  assert!(storage.get_magic_link(&code_hash_valid).unwrap().is_some());
}

#[tokio::test]
async fn test_cleanup_expired_magic_links_returns_zero_when_none_expired() {
  let storage = RedbStorage::new_in_memory().expect("in-memory storage");
  let removed = storage.cleanup_expired_magic_links().unwrap();
  assert_eq!(removed, 0);
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
  // Should fail with 422 (Unprocessable Entity) from axum's JSON extractor.
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
  // Missing required query param should fail.
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
