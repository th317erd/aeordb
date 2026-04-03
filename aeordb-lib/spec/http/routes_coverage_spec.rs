use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::auth::{generate_api_key, hash_api_key, ApiKeyRecord};
use aeordb::engine::{StorageEngine, SystemTables};
use aeordb::plugins::PluginManager;
use aeordb::server::{create_app_with_all, create_temp_engine_for_tests};

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
  let app = create_app_with_all(
    jwt_manager.clone(),
    plugin_manager,
    rate_limiter.clone(),
    make_prometheus_handle(),
    engine.clone(),
  );
  (app, jwt_manager, engine, rate_limiter, temp_dir)
}

fn rebuild_app(
  jwt_manager: &Arc<JwtManager>,
  engine: &Arc<StorageEngine>,
  rate_limiter: &Arc<RateLimiter>,
) -> axum::Router {
  let plugin_manager = Arc::new(PluginManager::new(engine.clone()));
  create_app_with_all(
    jwt_manager.clone(),
    plugin_manager,
    rate_limiter.clone(),
    make_prometheus_handle(),
    engine.clone(),
  )
}

fn admin_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "00000000-0000-0000-0000-000000000000".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

fn non_admin_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: uuid::Uuid::new_v4().to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    scope: None,
    permissions: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

fn expired_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "00000000-0000-0000-0000-000000000000".to_string(),
    iss: "aeordb".to_string(),
    iat: now - 7200,
    exp: now - 3600,
    scope: None,
    permissions: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

fn seed_api_key(engine: &StorageEngine) -> String {
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
  };
  system_tables.store_api_key(&record).unwrap();
  plaintext_key
}

fn seed_revoked_api_key(engine: &StorageEngine) -> String {
  let system_tables = SystemTables::new(engine);
  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: uuid::Uuid::new_v4(),
    created_at: chrono::Utc::now(),
    is_revoked: true,
  };
  system_tables.store_api_key(&record).unwrap();
  plaintext_key
}

// ---------------------------------------------------------------------------
// Auth token exchange error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_auth_token_malformed_json_body() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(r#"this is not json"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  assert!(
    status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
    "Expected 400 or 422, got {}",
    status,
  );
}

#[tokio::test]
async fn test_auth_token_empty_body() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(r#"{}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  assert!(
    status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
    "Expected 400 or 422 for missing api_key field, got {}",
    status,
  );
}

#[tokio::test]
async fn test_auth_token_invalid_api_key_format() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"api_key":"not_a_valid_key_format"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_auth_token_nonexistent_key_returns_401() {
  let (app, _, _, _, _temp_dir) = test_app();

  // Valid format but key does not exist in the system
  let fake_key = format!("aeor_{}_fakesecret1234567890abcdef", uuid::Uuid::new_v4());

  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(format!(r#"{{"api_key":"{}"}}"#, fake_key)))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_auth_token_revoked_key_returns_401() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let revoked_key = seed_revoked_api_key(&engine);

  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(format!(r#"{{"api_key":"{}"}}"#, revoked_key)))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Admin API key routes: auth and error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_api_key_requires_admin_role() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let non_admin_auth = non_admin_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &non_admin_auth)
    .body(Body::from(r#"{}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_list_api_keys_requires_admin_role() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let non_admin_auth = non_admin_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/admin/api-keys")
    .header("authorization", &non_admin_auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_revoke_api_key_requires_admin_role() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let non_admin_auth = non_admin_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/admin/api-keys/00000000-0000-0000-0000-000000000000")
    .header("authorization", &non_admin_auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_revoke_nonexistent_api_key_returns_404() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = admin_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/admin/api-keys/00000000-0000-0000-0000-000000000001")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_revoke_api_key_invalid_uuid_returns_400() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = admin_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/admin/api-keys/not-a-valid-uuid")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_api_key_with_user_id() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = admin_bearer_token(&jwt_manager);
  let target_user_id = uuid::Uuid::new_v4();

  let request = Request::builder()
    .method("POST")
    .uri("/admin/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(format!(r#"{{"user_id":"{}"}}"#, target_user_id)))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert!(json["api_key"].is_string());
  assert!(json["key_id"].is_string());
  assert_eq!(json["user_id"], target_user_id.to_string());
}

#[tokio::test]
async fn test_list_api_keys_returns_stored_keys() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = admin_bearer_token(&jwt_manager);

  // Create an API key first
  let target_user_id = uuid::Uuid::new_v4();
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(format!(r#"{{"user_id":"{}"}}"#, target_user_id)))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // List them
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("GET")
    .uri("/admin/api-keys")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let keys = json.as_array().unwrap();
  assert!(!keys.is_empty(), "should have at least one API key");
}

// ---------------------------------------------------------------------------
// Expired / invalid JWT auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_expired_jwt_returns_401() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let expired_auth = expired_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/admin/api-keys")
    .header("authorization", &expired_auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_malformed_bearer_token_returns_401() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/admin/api-keys")
    .header("authorization", "Bearer not.a.valid.jwt.token")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_missing_authorization_header_returns_401() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/admin/api-keys")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Magic link edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_magic_link_malformed_body_returns_error() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/magic-link")
    .header("content-type", "application/json")
    .body(Body::from(r#"not json"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  assert!(
    status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
    "Expected 400 or 422, got {}",
    status,
  );
}

// ---------------------------------------------------------------------------
// Refresh token edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_refresh_malformed_body_returns_error() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/refresh")
    .header("content-type", "application/json")
    .body(Body::from(r#"not valid json"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  assert!(
    status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
    "Expected 400 or 422, got {}",
    status,
  );
}

// ---------------------------------------------------------------------------
// Plugin routes error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_deploy_plugin_empty_body_returns_400() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = admin_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/testdb/public/mytable/_deploy")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_deploy_plugin_invalid_plugin_type_returns_400() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = admin_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/testdb/public/mytable/_deploy?plugin_type=invalid_type")
    .header("authorization", &auth)
    .body(Body::from(vec![0u8; 10]))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_invoke_nonexistent_plugin_returns_404() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = admin_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/testdb/public/nonexistent/run/_invoke")
    .header("authorization", &auth)
    .body(Body::from("input"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_remove_nonexistent_plugin_returns_404() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = admin_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/testdb/public/nonexistent/run/_remove")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Health check
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_health_check_returns_ok() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/admin/health")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["status"], "ok");
}

// ---------------------------------------------------------------------------
// Metrics endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_metrics_endpoint_requires_auth() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/admin/metrics")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_metrics_endpoint_returns_prometheus_text() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = admin_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/admin/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  assert_eq!(
    response.headers().get("content-type").unwrap().to_str().unwrap(),
    "text/plain; version=0.0.4; charset=utf-8"
  );
}
