use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::{bootstrap_root_key, generate_api_key, hash_api_key, ApiKeyRecord};
use aeordb::server::create_app_with_jwt;
use aeordb::storage::RedbStorage;

/// Create a fresh in-memory app with a shared JwtManager for test token creation.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<RedbStorage>) {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = Arc::new(JwtManager::generate());
  let app = create_app_with_jwt(storage.clone(), jwt_manager.clone());
  (app, jwt_manager, storage)
}

/// Create an admin JWT token.
fn admin_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "admin-user".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    roles: vec!["admin".to_string()],
  };
  jwt_manager.create_token(&claims).expect("create admin token")
}

/// Create a non-admin JWT token.
fn reader_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "reader-user".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    roles: vec!["reader".to_string()],
  };
  jwt_manager.create_token(&claims).expect("create reader token")
}

async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ---------------------------------------------------------------------------
// Auth middleware tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_unauthenticated_request_returns_401() {
  let (app, _, _) = test_app();
  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_valid_bearer_token_passes() {
  let (app, jwt_manager, _) = test_app();
  let token = admin_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("authorization", format!("Bearer {}", token))
    .header("content-type", "text/plain")
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_expired_bearer_token_returns_401() {
  let (app, jwt_manager, _) = test_app();
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "expired-user".to_string(),
    iss: "aeordb".to_string(),
    iat: now - 7200,
    exp: now - 3600, // expired 1 hour ago
    roles: vec!["admin".to_string()],
  };
  let token = jwt_manager.create_token(&claims).expect("create expired token");

  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("authorization", format!("Bearer {}", token))
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_malformed_bearer_token_returns_401() {
  let (app, _, _) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("authorization", "Bearer not.a.real.jwt.token")
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_missing_authorization_header_returns_401() {
  let (app, _, _) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_health_endpoint_exempt_from_auth() {
  let (app, _, _) = test_app();

  let request = Request::builder()
    .uri("/admin/health")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn test_auth_token_endpoint_exempt_from_auth() {
  let (app, _, _) = test_app();

  // Even without a valid API key, the endpoint itself should be reachable (not 401 from middleware).
  // It will return 401 from the handler because the API key is invalid, but that's different.
  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"api_key":"aeor_k_invalid"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  // Should be 401 from the handler (invalid key), NOT from the middleware
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["error"], "Invalid API key");
}

#[tokio::test]
async fn test_auth_token_with_valid_api_key_returns_jwt() {
  let (app, _, storage) = test_app();

  // Create an API key in storage
  let plaintext_key = generate_api_key();
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let record = ApiKeyRecord {
    key_id: uuid::Uuid::new_v4(),
    key_hash,
    roles: vec!["admin".to_string()],
    created_at: chrono::Utc::now(),
    is_revoked: false,
  };
  storage.store_api_key(&record).unwrap();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(format!(r#"{{"api_key":"{}"}}"#, plaintext_key)))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert!(json["token"].is_string(), "response should contain a token");
  assert_eq!(json["expires_in"], DEFAULT_EXPIRY_SECONDS);
}

#[tokio::test]
async fn test_auth_token_with_invalid_api_key_returns_401() {
  let (app, _, _) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"api_key":"aeor_k_0000000000000000000000000000000000000000000000000000000000000000"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_create_api_key_requires_admin_role() {
  let (app, jwt_manager, _) = test_app();
  let token = reader_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/api-keys")
    .header("authorization", format!("Bearer {}", token))
    .header("content-type", "application/json")
    .body(Body::from(r#"{"roles":["reader"]}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_create_api_key_returns_new_key() {
  let (app, jwt_manager, _) = test_app();
  let token = admin_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/api-keys")
    .header("authorization", format!("Bearer {}", token))
    .header("content-type", "application/json")
    .body(Body::from(r#"{"roles":["reader"]}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert!(json["key_id"].is_string(), "response should contain key_id");
  let api_key = json["api_key"].as_str().expect("response should contain api_key");
  assert!(api_key.starts_with("aeor_k_"), "API key should have correct prefix");
  assert_eq!(json["roles"], serde_json::json!(["reader"]));
}

#[tokio::test]
async fn test_list_api_keys_returns_metadata() {
  let (app, jwt_manager, storage) = test_app();

  // Seed a key
  let plaintext_key = generate_api_key();
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let record = ApiKeyRecord {
    key_id: uuid::Uuid::new_v4(),
    key_hash,
    roles: vec!["admin".to_string()],
    created_at: chrono::Utc::now(),
    is_revoked: false,
  };
  storage.store_api_key(&record).unwrap();

  let token = admin_token(&jwt_manager);
  let request = Request::builder()
    .uri("/admin/api-keys")
    .header("authorization", format!("Bearer {}", token))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let array = json.as_array().expect("response should be an array");
  assert_eq!(array.len(), 1);
  assert!(array[0]["key_id"].is_string());
  assert!(array[0]["roles"].is_array());
  assert!(array[0]["is_revoked"].is_boolean());
  // Should NOT contain key_hash or the plaintext key
  assert!(array[0].get("key_hash").is_none(), "should not expose key_hash");
  assert!(array[0].get("api_key").is_none(), "should not expose api_key");
}

#[tokio::test]
async fn test_revoke_api_key_succeeds() {
  let (app, jwt_manager, storage) = test_app();

  // Seed a key
  let plaintext_key = generate_api_key();
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let key_id = uuid::Uuid::new_v4();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    roles: vec!["reader".to_string()],
    created_at: chrono::Utc::now(),
    is_revoked: false,
  };
  storage.store_api_key(&record).unwrap();

  let token = admin_token(&jwt_manager);
  let request = Request::builder()
    .method("DELETE")
    .uri(format!("/admin/api-keys/{}", key_id))
    .header("authorization", format!("Bearer {}", token))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["revoked"], true);
  assert_eq!(json["key_id"], key_id.to_string());
}

#[tokio::test]
async fn test_revoked_api_key_cannot_get_token() {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = Arc::new(JwtManager::generate());

  // Create and store a key
  let plaintext_key = generate_api_key();
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let key_id = uuid::Uuid::new_v4();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    roles: vec!["admin".to_string()],
    created_at: chrono::Utc::now(),
    is_revoked: false,
  };
  storage.store_api_key(&record).unwrap();

  // Revoke it
  storage.revoke_api_key(key_id).unwrap();

  let app = create_app_with_jwt(storage.clone(), jwt_manager.clone());

  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(format!(r#"{{"api_key":"{}"}}"#, plaintext_key)))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_bootstrap_creates_root_key_on_first_run() {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = JwtManager::generate();

  let result = bootstrap_root_key(&storage, &jwt_manager);
  assert!(result.is_some(), "should return a plaintext key on first run");

  let plaintext_key = result.unwrap();
  assert!(plaintext_key.starts_with("aeor_k_"), "root key should have correct prefix");

  // Verify it was stored
  let keys = storage.list_system_api_keys().unwrap();
  assert_eq!(keys.len(), 1);
  assert_eq!(keys[0].roles, vec!["admin".to_string()]);
  assert!(!keys[0].is_revoked);
}

#[tokio::test]
async fn test_bootstrap_returns_none_on_subsequent_runs() {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = JwtManager::generate();

  // First run creates the key
  let first_result = bootstrap_root_key(&storage, &jwt_manager);
  assert!(first_result.is_some());

  // Second run should return None
  let second_result = bootstrap_root_key(&storage, &jwt_manager);
  assert!(second_result.is_none(), "should return None when keys already exist");

  // Still only one key in storage
  let keys = storage.list_system_api_keys().unwrap();
  assert_eq!(keys.len(), 1);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_authorization_header_without_bearer_prefix_returns_401() {
  let (app, jwt_manager, _) = test_app();
  let token = admin_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("authorization", format!("Basic {}", token))
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_token_from_different_jwt_manager_rejected() {
  let (app, _, _) = test_app();
  let other_manager = JwtManager::generate();
  let token = admin_token(&other_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/mydb/users")
    .header("authorization", format!("Bearer {}", token))
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_revoke_nonexistent_key_returns_404() {
  let (app, jwt_manager, _) = test_app();
  let token = admin_token(&jwt_manager);
  let fake_id = uuid::Uuid::new_v4();

  let request = Request::builder()
    .method("DELETE")
    .uri(format!("/admin/api-keys/{}", fake_id))
    .header("authorization", format!("Bearer {}", token))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_list_api_keys_requires_admin_role() {
  let (app, jwt_manager, _) = test_app();
  let token = reader_token(&jwt_manager);

  let request = Request::builder()
    .uri("/admin/api-keys")
    .header("authorization", format!("Bearer {}", token))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_revoke_api_key_requires_admin_role() {
  let (app, jwt_manager, _) = test_app();
  let token = reader_token(&jwt_manager);
  let fake_id = uuid::Uuid::new_v4();

  let request = Request::builder()
    .method("DELETE")
    .uri(format!("/admin/api-keys/{}", fake_id))
    .header("authorization", format!("Bearer {}", token))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
