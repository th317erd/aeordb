use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::auth_uri::{AuthMode, expand_tilde, parse_auth_uri, resolve_auth_mode};
use aeordb::engine::system_store;
use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::provider::{AuthProvider, FileAuthProvider, NoAuthProvider};
use aeordb::auth::{bootstrap_root_key, generate_api_key, hash_api_key, ApiKeyRecord};
use aeordb::engine::{StorageEngine, ROOT_USER_ID};
use aeordb::engine::RequestContext;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests, CorsState};

// ===========================================================================
// parse_auth_uri tests
// ===========================================================================

#[test]
fn test_parse_auth_uri_false() {
  assert_eq!(parse_auth_uri("false").unwrap(), AuthMode::Disabled);
}

#[test]
fn test_parse_auth_uri_null() {
  assert_eq!(parse_auth_uri("null").unwrap(), AuthMode::Disabled);
}

#[test]
fn test_parse_auth_uri_no() {
  assert_eq!(parse_auth_uri("no").unwrap(), AuthMode::Disabled);
}

#[test]
fn test_parse_auth_uri_zero() {
  assert_eq!(parse_auth_uri("0").unwrap(), AuthMode::Disabled);
}

#[test]
fn test_parse_auth_uri_false_case_insensitive() {
  assert_eq!(parse_auth_uri("FALSE").unwrap(), AuthMode::Disabled);
  assert_eq!(parse_auth_uri("False").unwrap(), AuthMode::Disabled);
  assert_eq!(parse_auth_uri("NO").unwrap(), AuthMode::Disabled);
  assert_eq!(parse_auth_uri("NULL").unwrap(), AuthMode::Disabled);
}

#[test]
fn test_parse_auth_uri_self() {
  assert_eq!(parse_auth_uri("self").unwrap(), AuthMode::SelfContained);
}

#[test]
fn test_parse_auth_uri_self_case_insensitive() {
  assert_eq!(parse_auth_uri("SELF").unwrap(), AuthMode::SelfContained);
  assert_eq!(parse_auth_uri("Self").unwrap(), AuthMode::SelfContained);
}

#[test]
fn test_parse_auth_uri_dot_slash() {
  assert_eq!(parse_auth_uri("./").unwrap(), AuthMode::SelfContained);
}

#[test]
fn test_parse_auth_uri_file() {
  let result = parse_auth_uri("file:///etc/aeordb/identity").unwrap();
  assert_eq!(result, AuthMode::File("/etc/aeordb/identity".to_string()));
}

#[test]
fn test_parse_auth_uri_file_with_tilde() {
  let result = parse_auth_uri("file://~/.config/aeordb/identity").unwrap();
  // The tilde should be expanded to the home directory.
  let home = std::env::var("HOME").unwrap();
  let expected = format!("{}/.config/aeordb/identity", home);
  assert_eq!(result, AuthMode::File(expected));
}

#[test]
fn test_parse_auth_uri_file_empty_path() {
  let result = parse_auth_uri("file://");
  assert!(result.is_err());
  assert!(result.unwrap_err().contains("requires a path"));
}

#[test]
fn test_parse_auth_uri_unknown() {
  let result = parse_auth_uri("https://auth.example.com");
  assert!(result.is_err());
  assert!(result.unwrap_err().contains("Unknown auth URI"));
}

#[test]
fn test_parse_auth_uri_unknown_gibberish() {
  let result = parse_auth_uri("not-a-valid-uri");
  assert!(result.is_err());
}

// ===========================================================================
// expand_tilde tests
// ===========================================================================

#[test]
fn test_expand_tilde_home_prefix() {
  let result = expand_tilde("~/Documents/test");
  let home = std::env::var("HOME").unwrap();
  assert_eq!(result, format!("{}/Documents/test", home));
}

#[test]
fn test_expand_tilde_bare_tilde() {
  let result = expand_tilde("~");
  let home = std::env::var("HOME").unwrap();
  assert_eq!(result, home);
}

#[test]
fn test_expand_tilde_no_tilde() {
  let result = expand_tilde("/etc/aeordb/identity");
  assert_eq!(result, "/etc/aeordb/identity");
}

#[test]
fn test_expand_tilde_tilde_not_at_start() {
  let result = expand_tilde("/path/to/~file");
  assert_eq!(result, "/path/to/~file");
}

// ===========================================================================
// resolve_auth_mode tests
// ===========================================================================

#[test]
fn test_resolve_auth_mode_cli_flag_wins() {
  // CLI flag should always win, even if env var is set.
  std::env::set_var("AEORDB_AUTH", "false");
  let result = resolve_auth_mode(Some("self"));
  assert_eq!(result, AuthMode::SelfContained);
  std::env::remove_var("AEORDB_AUTH");
}

#[test]
fn test_resolve_auth_mode_cli_false() {
  let result = resolve_auth_mode(Some("false"));
  assert_eq!(result, AuthMode::Disabled);
}

#[test]
fn test_resolve_auth_mode_env_var() {
  std::env::set_var("AEORDB_AUTH", "false");
  let result = resolve_auth_mode(None);
  assert_eq!(result, AuthMode::Disabled);
  std::env::remove_var("AEORDB_AUTH");
}

#[test]
fn test_resolve_auth_mode_env_var_file() {
  std::env::set_var("AEORDB_AUTH", "file:///tmp/test-identity");
  let result = resolve_auth_mode(None);
  assert_eq!(result, AuthMode::File("/tmp/test-identity".to_string()));
  std::env::remove_var("AEORDB_AUTH");
}

#[test]
fn test_resolve_auth_mode_default_self() {
  // Remove env var, ensure no default identity file exists.
  std::env::remove_var("AEORDB_AUTH");
  let result = resolve_auth_mode(None);
  // Should be SelfContained (unless ~/.config/aeordb/identity exists).
  // We don't create that file in tests, so this should be SelfContained.
  assert!(
    result == AuthMode::SelfContained || matches!(result, AuthMode::File(_)),
    "Expected SelfContained or File, got {:?}",
    result
  );
}

#[test]
fn test_resolve_auth_mode_invalid_cli_flag_falls_back() {
  // Invalid URI should fall back to SelfContained.
  let result = resolve_auth_mode(Some("not-valid"));
  assert_eq!(result, AuthMode::SelfContained);
}

// ===========================================================================
// NoAuthProvider tests
// ===========================================================================

#[test]
fn test_no_auth_provider_is_not_enabled() {
  let provider = NoAuthProvider::new();
  assert!(!provider.is_enabled());
}

#[test]
fn test_no_auth_provider_allows_everything() {
  let provider = NoAuthProvider::new();

  // Any key lookup returns a fake root record.
  let result = provider.get_api_key_by_prefix("anything").unwrap();
  assert!(result.is_some());
  let record = result.unwrap();
  assert_eq!(record.user_id, ROOT_USER_ID);
  assert!(!record.is_revoked);
}

#[test]
fn test_no_auth_provider_store_is_noop() {
  let ctx = RequestContext::system();
  let provider = NoAuthProvider::new();
  let record = ApiKeyRecord {
    key_id: uuid::Uuid::new_v4(),
    key_hash: "hash".to_string(),
    user_id: uuid::Uuid::new_v4(),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: i64::MAX,
    label: None,
    rules: vec![],
  };
  assert!(provider.store_api_key(&record).is_ok());
  assert!(provider.store_api_key_for_bootstrap(&record).is_ok());
}

#[test]
fn test_no_auth_provider_list_returns_empty() {
  let provider = NoAuthProvider::new();
  let keys = provider.list_api_keys().unwrap();
  assert!(keys.is_empty());
}

#[test]
fn test_no_auth_provider_revoke_returns_false() {
  let provider = NoAuthProvider::new();
  let result = provider.revoke_api_key(uuid::Uuid::new_v4()).unwrap();
  assert!(!result);
}

#[test]
fn test_no_auth_provider_has_jwt_manager() {
  let provider = NoAuthProvider::new();
  // Should have a working JWT manager (for consistency).
  let claims = TokenClaims {
    sub: "test".to_string(),
    iss: "aeordb".to_string(),
    iat: chrono::Utc::now().timestamp(),
    exp: chrono::Utc::now().timestamp() + 3600,
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = provider.jwt_manager().create_token(&claims);
  assert!(token.is_ok());
}

// ===========================================================================
// FileAuthProvider tests
// ===========================================================================

#[test]
fn test_file_auth_provider_is_enabled() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let provider = FileAuthProvider::new(engine);
  assert!(provider.is_enabled());
}

#[test]
fn test_file_auth_provider_validates_key() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let provider = FileAuthProvider::new(engine.clone());

  // Store a key via system_store (like bootstrap does).
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
  provider.store_api_key(&record).unwrap();

  // Look up by key_id prefix.
  let key_id_prefix = &key_id.simple().to_string()[..16];
  let found = provider.get_api_key_by_prefix(key_id_prefix).unwrap();
  assert!(found.is_some());
  assert_eq!(found.unwrap().key_id, key_id);
}

#[test]
fn test_file_auth_provider_rejects_invalid_key() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let provider = FileAuthProvider::new(engine);

  let found = provider.get_api_key_by_prefix("0000000000000000").unwrap();
  assert!(found.is_none());
}

#[test]
fn test_file_auth_provider_list_and_revoke() {
  let ctx = RequestContext::system();
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let provider = FileAuthProvider::new(engine.clone());

  // Start with no keys.
  let keys = provider.list_api_keys().unwrap();
  assert!(keys.is_empty());

  // Store a key.
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
  provider.store_api_key(&record).unwrap();

  // Should have 1 key.
  let keys = provider.list_api_keys().unwrap();
  assert_eq!(keys.len(), 1);

  // Revoke it.
  let revoked = provider.revoke_api_key(key_id).unwrap();
  assert!(revoked);

  // Key should be revoked.
  let key_id_prefix = &key_id.simple().to_string()[..16];
  let found = provider.get_api_key_by_prefix(key_id_prefix).unwrap().unwrap();
  assert!(found.is_revoked);
}

#[test]
fn test_file_auth_provider_jwt_manager_persists() {
  let temp_dir = tempfile::tempdir().unwrap();
  let engine_file = temp_dir.path().join("jwt-test.aeordb");
  let engine_path = engine_file.to_str().unwrap();

  // Create engine, create provider, get JWT bytes.
  let engine = Arc::new(StorageEngine::create(engine_path).unwrap());
  let provider1 = FileAuthProvider::new(engine.clone());
  let jwt_bytes_1 = provider1.jwt_manager().to_bytes();
  drop(provider1);
  drop(engine);

  // Re-open the same engine, verify same JWT key is loaded.
  let engine2 = Arc::new(StorageEngine::open(engine_path).unwrap());
  let provider2 = FileAuthProvider::new(engine2);
  let jwt_bytes_2 = provider2.jwt_manager().to_bytes();

  assert_eq!(jwt_bytes_1, jwt_bytes_2, "JWT signing key should persist across restarts");
}

#[test]
fn test_file_auth_provider_from_identity_file() {
  let temp_dir = tempfile::tempdir().unwrap();
  let identity_path = temp_dir.path().join("test-identity.aeordb");
  let identity_str = identity_path.to_str().unwrap();

  // Should create a new identity file and bootstrap a root key.
  let (provider, bootstrap_key) = FileAuthProvider::from_identity_file(identity_str).unwrap();
  assert!(bootstrap_key.is_some(), "Should bootstrap a root key for new identity file");
  assert!(provider.is_enabled());

  // Verify the key is stored.
  let keys = provider.list_api_keys().unwrap();
  assert_eq!(keys.len(), 1);
  assert_eq!(keys[0].user_id, ROOT_USER_ID);
}

#[test]
fn test_file_auth_provider_from_identity_file_no_double_bootstrap() {
  let temp_dir = tempfile::tempdir().unwrap();
  let identity_path = temp_dir.path().join("test-identity2.aeordb");
  let identity_str = identity_path.to_str().unwrap();

  // First creation bootstraps.
  let (_provider1, key1) = FileAuthProvider::from_identity_file(identity_str).unwrap();
  assert!(key1.is_some());

  // Second creation should NOT bootstrap again.
  let (_provider2, key2) = FileAuthProvider::from_identity_file(identity_str).unwrap();
  assert!(key2.is_none(), "Should not bootstrap again on second open");
}

#[test]
fn test_file_auth_provider_from_identity_file_creates_parent_dirs() {
  let temp_dir = tempfile::tempdir().unwrap();
  let nested_path = temp_dir.path().join("nested/deep/dir/identity.aeordb");
  let nested_str = nested_path.to_str().unwrap();

  let result = FileAuthProvider::from_identity_file(nested_str);
  assert!(result.is_ok(), "Should create nested parent directories");
}

// ===========================================================================
// NoAuth mode integration test (middleware bypass)
// ===========================================================================

fn make_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
  metrics_exporter_prometheus::PrometheusBuilder::new()
    .build_recorder()
    .handle()
}

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

#[tokio::test]
async fn test_no_auth_mode_allows_engine_writes_without_token() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let no_auth: Arc<dyn AuthProvider> = Arc::new(NoAuthProvider::new());
  let jwt_manager = Arc::new(JwtManager::generate());
  let plugin_manager = Arc::new(aeordb::plugins::PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(aeordb::auth::RateLimiter::default_config());

  let app = create_app_with_all(
    no_auth,
    jwt_manager,
    plugin_manager,
    rate_limiter,
    make_prometheus_handle(),
    engine,
    Arc::new(aeordb::engine::EventBus::new()),
    CorsState { default_origins: None, rules: vec![] },
  );

  // No Authorization header at all -- should still work.
  let request = Request::builder()
    .method("PUT")
    .uri("/engine/test/hello.txt")
    .header("content-type", "text/plain")
    .body(Body::from("hello world"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_no_auth_mode_allows_admin_without_token() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let no_auth: Arc<dyn AuthProvider> = Arc::new(NoAuthProvider::new());
  let jwt_manager = Arc::new(JwtManager::generate());
  let plugin_manager = Arc::new(aeordb::plugins::PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(aeordb::auth::RateLimiter::default_config());

  let app = create_app_with_all(
    no_auth,
    jwt_manager,
    plugin_manager,
    rate_limiter,
    make_prometheus_handle(),
    engine,
    Arc::new(aeordb::engine::EventBus::new()),
    CorsState { default_origins: None, rules: vec![] },
  );

  // GET /admin/api-keys without auth should work (root claims injected).
  let request = Request::builder()
    .uri("/admin/api-keys")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  // Should succeed because NoAuth injects root claims.
  assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_no_auth_mode_engine_read_after_write() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let no_auth: Arc<dyn AuthProvider> = Arc::new(NoAuthProvider::new());
  let jwt_manager = Arc::new(JwtManager::generate());
  let plugin_manager = Arc::new(aeordb::plugins::PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(aeordb::auth::RateLimiter::default_config());

  // Write a file.
  let app = create_app_with_all(
    no_auth.clone(),
    jwt_manager.clone(),
    plugin_manager.clone(),
    rate_limiter.clone(),
    make_prometheus_handle(),
    engine.clone(),
    Arc::new(aeordb::engine::EventBus::new()),
    CorsState { default_origins: None, rules: vec![] },
  );

  let write_req = Request::builder()
    .method("PUT")
    .uri("/engine/noauth/data.json")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"test": true}"#))
    .unwrap();

  let write_resp = app.oneshot(write_req).await.unwrap();
  assert_eq!(write_resp.status(), StatusCode::CREATED);

  // Read it back.
  let app2 = create_app_with_all(
    no_auth,
    jwt_manager,
    Arc::new(aeordb::plugins::PluginManager::new(engine.clone())),
    rate_limiter,
    make_prometheus_handle(),
    engine,
    Arc::new(aeordb::engine::EventBus::new()),
    CorsState { default_origins: None, rules: vec![] },
  );

  let read_req = Request::builder()
    .uri("/engine/noauth/data.json")
    .body(Body::empty())
    .unwrap();

  let read_resp = app2.oneshot(read_req).await.unwrap();
  assert_eq!(read_resp.status(), StatusCode::OK);

  let body_bytes = read_resp.into_body().collect().await.unwrap().to_bytes().to_vec();
  let body_str = String::from_utf8(body_bytes).unwrap();
  assert!(body_str.contains("\"test\""));
}

#[tokio::test]
async fn test_file_auth_provider_token_exchange_works() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();
  let provider = Arc::new(FileAuthProvider::new(engine.clone()));
  let jwt_manager = Arc::new(
    JwtManager::from_bytes(&provider.jwt_manager().to_bytes()).unwrap()
  );

  // Bootstrap a root key.
  let root_key = bootstrap_root_key(&engine).expect("should bootstrap");

  let plugin_manager = Arc::new(aeordb::plugins::PluginManager::new(engine.clone()));
  let rate_limiter = Arc::new(aeordb::auth::RateLimiter::default_config());

  let app = create_app_with_all(
    provider as Arc<dyn AuthProvider>,
    jwt_manager,
    plugin_manager,
    rate_limiter,
    make_prometheus_handle(),
    engine,
    Arc::new(aeordb::engine::EventBus::new()),
    CorsState { default_origins: None, rules: vec![] },
  );

  // Exchange the root key for a JWT.
  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(format!(r#"{{"api_key":"{}"}}"#, root_key)))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert!(json["token"].is_string(), "response should contain a token");
  assert_eq!(json["expires_in"], DEFAULT_EXPIRY_SECONDS);
}
