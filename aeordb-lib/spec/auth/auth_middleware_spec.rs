use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::{bootstrap_root_key, generate_api_key, hash_api_key, ApiKeyRecord};
use aeordb::engine::StorageEngine;
use aeordb::engine::system_store;
use aeordb::engine::RequestContext;
use aeordb::server::{create_app_with_jwt, create_temp_engine_for_tests};

/// Create a fresh app with a shared JwtManager for test token creation.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

/// Create a root JWT token (nil UUID = root identity).
fn admin_token(jwt_manager: &JwtManager) -> String {
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
  jwt_manager.create_token(&claims).expect("create admin token")
}

/// Create a non-root JWT token (random UUID).
fn reader_token(jwt_manager: &JwtManager) -> String {
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
  let (app, _, _, _temp_dir) = test_app();
  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/file.txt")
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_valid_bearer_token_passes() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let token = admin_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/file.txt")
    .header("authorization", format!("Bearer {}", token))
    .header("content-type", "text/plain")
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_expired_bearer_token_returns_401() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "expired-user".to_string(),
    iss: "aeordb".to_string(),
    iat: now - 7200,
    exp: now - 3600, // expired 1 hour ago
    scope: None,
    permissions: None,
    key_id: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create expired token");

  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/file.txt")
    .header("authorization", format!("Bearer {}", token))
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_malformed_bearer_token_returns_401() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/file.txt")
    .header("authorization", "Bearer not.a.real.jwt.token")
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_missing_authorization_header_returns_401() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/file.txt")
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_health_endpoint_exempt_from_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .uri("/system/health")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  // Health endpoint now returns a full HealthReport with status "healthy".
  assert_eq!(json["status"], "healthy");
}

#[tokio::test]
async fn test_auth_token_endpoint_exempt_from_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"api_key":"aeor_k_0123456789abcdef_0000000000000000000000000000000000000000000000000000000000000000"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("Invalid API key"), "Expected error to contain 'Invalid API key', got: {}", json["error"]);
}

#[tokio::test]
async fn test_auth_token_with_valid_api_key_returns_jwt() {
  let ctx = RequestContext::system();
  let (app, _, engine, _temp_dir) = test_app();

  // Create an API key via system_store
  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: Some(uuid::Uuid::new_v4()),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: i64::MAX,
    label: None,
    rules: vec![],
  };

  system_store::store_api_key(&engine, &ctx, &record).unwrap();

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
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"api_key":"aeor_k_0123456789abcdef_0000000000000000000000000000000000000000000000000000000000000000"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_create_api_key_requires_admin_role() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let token = reader_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/auth/keys/admin")
    .header("authorization", format!("Bearer {}", token))
    .header("content-type", "application/json")
    .body(Body::from(r#"{}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_create_api_key_returns_new_key() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let token = admin_token(&jwt_manager);
  let target_user_id = uuid::Uuid::new_v4();

  let request = Request::builder()
    .method("POST")
    .uri("/auth/keys/admin")
    .header("authorization", format!("Bearer {}", token))
    .header("content-type", "application/json")
    .body(Body::from(format!(r#"{{"user_id":"{}"}}"#, target_user_id)))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert!(json["key_id"].is_string(), "response should contain key_id");
  let api_key = json["api_key"].as_str().expect("response should contain api_key");
  assert!(api_key.starts_with("aeor_k_"), "API key should have correct prefix");
  assert_eq!(json["user_id"], target_user_id.to_string());
}

#[tokio::test]
async fn test_list_api_keys_returns_metadata() {
  let ctx = RequestContext::system();
  let (app, jwt_manager, engine, _temp_dir) = test_app();

  // Seed a key
  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: Some(uuid::Uuid::new_v4()),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: i64::MAX,
    label: None,
    rules: vec![],
  };

  system_store::store_api_key(&engine, &ctx, &record).unwrap();

  let token = admin_token(&jwt_manager);
  let request = Request::builder()
    .uri("/auth/keys/admin")
    .header("authorization", format!("Bearer {}", token))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let array = json["items"].as_array().expect("response should have items array");
  assert_eq!(array.len(), 1);
  assert!(array[0]["key_id"].is_string());
  assert!(array[0]["user_id"].is_string());
  assert!(array[0]["is_revoked"].is_boolean());
  assert!(array[0].get("key_hash").is_none(), "should not expose key_hash");
  assert!(array[0].get("api_key").is_none(), "should not expose api_key");
}

#[tokio::test]
async fn test_revoke_api_key_succeeds() {
  let ctx = RequestContext::system();
  let (app, jwt_manager, engine, _temp_dir) = test_app();

  // Seed a key
  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: Some(uuid::Uuid::new_v4()),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: i64::MAX,
    label: None,
    rules: vec![],
  };

  system_store::store_api_key(&engine, &ctx, &record).unwrap();

  let token = admin_token(&jwt_manager);
  let request = Request::builder()
    .method("DELETE")
    .uri(format!("/auth/keys/admin/{}", key_id))
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
  let ctx = RequestContext::system();
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, _temp_dir) = create_temp_engine_for_tests();

  // Create and store a key
  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = generate_api_key(key_id);
  let key_hash = hash_api_key(&plaintext_key).unwrap();
  let record = ApiKeyRecord {
    key_id,
    key_hash,
    user_id: Some(uuid::Uuid::new_v4()),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: i64::MAX,
    label: None,
    rules: vec![],
  };

  system_store::store_api_key(&engine, &ctx, &record).unwrap();

  // Revoke it
  system_store::revoke_api_key(&engine, &ctx, key_id).unwrap();

  let app = create_app_with_jwt(jwt_manager.clone(), engine.clone());

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
  let (engine, _temp_dir) = create_temp_engine_for_tests();

  let result = bootstrap_root_key(&engine).expect("bootstrap should succeed");
  assert!(result.is_some(), "should return a plaintext key on first run");

  let plaintext_key = result.unwrap();
  assert!(plaintext_key.starts_with("aeor_k_"), "root key should have correct prefix");

  // Verify it was stored

  let keys = system_store::list_api_keys(&engine).unwrap();
  assert_eq!(keys.len(), 1);
  assert_eq!(keys[0].user_id, Some(uuid::Uuid::nil()));
  assert!(!keys[0].is_revoked);
}

#[tokio::test]
async fn test_bootstrap_returns_none_on_subsequent_runs() {
  let (engine, _temp_dir) = create_temp_engine_for_tests();

  // First run creates the key
  let first_result = bootstrap_root_key(&engine).expect("bootstrap should succeed");
  assert!(first_result.is_some());

  // Second run should return None
  let second_result = bootstrap_root_key(&engine).expect("second bootstrap should succeed");
  assert!(second_result.is_none(), "should return None when keys already exist");

  // Still only one key in storage

  let keys = system_store::list_api_keys(&engine).unwrap();
  assert_eq!(keys.len(), 1);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_authorization_header_without_bearer_prefix_returns_401() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let token = admin_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/file.txt")
    .header("authorization", format!("Basic {}", token))
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_token_from_different_jwt_manager_rejected() {
  let (app, _, _, _temp_dir) = test_app();
  let other_manager = JwtManager::generate();
  let token = admin_token(&other_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/test/file.txt")
    .header("authorization", format!("Bearer {}", token))
    .body(Body::from("hello"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_revoke_nonexistent_key_returns_404() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let token = admin_token(&jwt_manager);
  let fake_id = uuid::Uuid::new_v4();

  let request = Request::builder()
    .method("DELETE")
    .uri(format!("/auth/keys/admin/{}", fake_id))
    .header("authorization", format!("Bearer {}", token))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_list_api_keys_requires_admin_role() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let token = reader_token(&jwt_manager);

  let request = Request::builder()
    .uri("/auth/keys/admin")
    .header("authorization", format!("Bearer {}", token))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_revoke_api_key_requires_admin_role() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let token = reader_token(&jwt_manager);
  let fake_id = uuid::Uuid::new_v4();

  let request = Request::builder()
    .method("DELETE")
    .uri(format!("/auth/keys/admin/{}", fake_id))
    .header("authorization", format!("Bearer {}", token))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Full e2e auth flow test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_full_flow_bootstrap_to_token_to_engine_crud() {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, _temp_dir) = create_temp_engine_for_tests();

  // Step 1: Bootstrap root key
  let plaintext_key = bootstrap_root_key(&engine)
    .expect("bootstrap should succeed")
    .expect("should create root key on first run");

  // Step 2: Exchange API key for JWT
  let app = create_app_with_jwt(jwt_manager.clone(), engine.clone());
  let token_request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(format!(r#"{{"api_key":"{}"}}"#, plaintext_key)))
    .unwrap();

  let token_response = app.oneshot(token_request).await.unwrap();
  assert_eq!(token_response.status(), StatusCode::OK);

  let token_json = body_json(token_response.into_body()).await;
  let jwt_token = token_json["token"].as_str().expect("should have token");
  let bearer = format!("Bearer {}", jwt_token);

  // Step 3: Use JWT to store a file via engine
  let app2 = create_app_with_jwt(jwt_manager.clone(), engine.clone());
  let store_request = Request::builder()
    .method("PUT")
    .uri("/files/e2e/test.txt")
    .header("authorization", &bearer)
    .header("content-type", "text/plain")
    .body(Body::from("e2e test data"))
    .unwrap();

  let store_response = app2.oneshot(store_request).await.unwrap();
  assert_eq!(store_response.status(), StatusCode::CREATED);

  // Step 4: Verify the file exists by fetching it
  let app3 = create_app_with_jwt(jwt_manager.clone(), engine.clone());
  let get_request = Request::builder()
    .uri("/files/e2e/test.txt")
    .header("authorization", &bearer)
    .body(Body::empty())
    .unwrap();

  let get_response = app3.oneshot(get_request).await.unwrap();
  assert_eq!(get_response.status(), StatusCode::OK);

  let body_bytes = get_response
    .into_body()
    .collect()
    .await
    .unwrap()
    .to_bytes()
    .to_vec();
  assert_eq!(String::from_utf8(body_bytes).unwrap(), "e2e test data");
}
