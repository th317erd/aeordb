use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::api_key::{DEFAULT_EXPIRY_DAYS, MAX_EXPIRY_DAYS};
use aeordb::engine::StorageEngine;
use aeordb::server::{create_app_with_jwt_and_engine, create_temp_engine_for_tests};

/// Create a fresh in-memory app with engine support.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

/// Rebuild app from shared state (for multi-request tests).
fn rebuild_app(
  jwt_manager: &Arc<JwtManager>,
  engine: &Arc<StorageEngine>,
) -> axum::Router {
  create_app_with_jwt_and_engine(jwt_manager.clone(), engine.clone())
}

/// Create a root-user Bearer token (nil UUID = ROOT_USER_ID).
fn root_bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: uuid::Uuid::nil().to_string(),
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

/// Create a non-root Bearer token for a specific user UUID.
fn user_bearer_token(jwt_manager: &JwtManager, user_id: uuid::Uuid) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: user_id.to_string(),
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

/// Collect response body into JSON.
async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body.collect().await.unwrap().to_bytes().to_vec();
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

// ---------------------------------------------------------------------------
// POST /api-keys
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_own_key() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"label": "my-key"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert!(json["key_id"].is_string());
  assert!(json["key"].as_str().unwrap().starts_with("aeor_k_"));
  assert_eq!(json["label"], "my-key");
  assert!(json["expires_at"].is_number());
  assert_eq!(json["user_id"], uuid::Uuid::nil().to_string());
}

#[tokio::test]
async fn test_create_key_with_rules() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "label": "restricted",
    "rules": [
      { "/docs/**": "cr------" },
      { "/public/**": "-r--l---" }
    ]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let rules = json["rules"].as_array().unwrap();
  assert_eq!(rules.len(), 2);
  assert_eq!(rules[0]["glob"], "/docs/**");
  assert_eq!(rules[0]["permitted"], "cr------");
}

#[tokio::test]
async fn test_create_key_default_expiry() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let expires_at = json["expires_at"].as_i64().unwrap();
  let now_millis = chrono::Utc::now().timestamp_millis();
  let expected_millis = DEFAULT_EXPIRY_DAYS * 24 * 60 * 60 * 1000;

  // Should be approximately DEFAULT_EXPIRY_DAYS from now (within 10 seconds tolerance).
  let diff = (expires_at - now_millis - expected_millis).abs();
  assert!(diff < 10_000, "Expiry should be ~{} days from now, diff was {} ms", DEFAULT_EXPIRY_DAYS, diff);
}

#[tokio::test]
async fn test_create_key_max_expiry_clamped() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let body = serde_json::json!({ "expires_in_days": 9999 });

  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let expires_at = json["expires_at"].as_i64().unwrap();
  let now_millis = chrono::Utc::now().timestamp_millis();
  let max_millis = MAX_EXPIRY_DAYS * 24 * 60 * 60 * 1000;

  // Should be clamped to MAX_EXPIRY_DAYS (within 10 seconds tolerance).
  let diff = (expires_at - now_millis - max_millis).abs();
  assert!(diff < 10_000, "Expiry should be clamped to ~{} days, diff was {} ms", MAX_EXPIRY_DAYS, diff);
}

#[tokio::test]
async fn test_create_key_invalid_rules() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "rules": [{ "/docs/**": "badflags" }]
  });

  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);

  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("Invalid rules"));
}

#[tokio::test]
async fn test_create_key_rules_not_array() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let body = serde_json::json!({
    "rules": "not-an-array"
  });

  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// GET /api-keys
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_own_keys() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create two keys.
  for label in &["key-a", "key-b"] {
    let body = serde_json::json!({ "label": label });
    let request = Request::builder()
      .method("POST")
      .uri("/api-keys")
      .header("content-type", "application/json")
      .header("authorization", &auth)
      .body(Body::from(serde_json::to_string(&body).unwrap()))
      .unwrap();

    let app = rebuild_app(&jwt_manager, &engine);
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
  }

  // List them.
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/api-keys")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let keys = json.as_array().unwrap();
  // Root will also have a bootstrap key from engine initialization, plus our 2.
  assert!(keys.len() >= 2, "Expected at least 2 keys, got {}", keys.len());

  // Verify no key_hash is exposed.
  for key in keys {
    assert!(key.get("key_hash").is_none(), "key_hash should not be exposed");
  }
}

#[tokio::test]
async fn test_list_keys_filters_by_user() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let user_a = uuid::Uuid::new_v4();
  let user_b = uuid::Uuid::new_v4();

  // Root creates a key for user_a.
  let auth_root = root_bearer_token(&jwt_manager);
  let body = serde_json::json!({ "label": "a-key", "user_id": user_a.to_string() });
  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth_root)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // User B lists keys -- should see zero.
  let auth_b = user_bearer_token(&jwt_manager, user_b);
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/api-keys")
    .header("authorization", &auth_b)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert_eq!(json.as_array().unwrap().len(), 0);

  // User A lists keys -- should see one.
  let auth_a = user_bearer_token(&jwt_manager, user_a);
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/api-keys")
    .header("authorization", &auth_a)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert_eq!(json.as_array().unwrap().len(), 1);
  assert_eq!(json[0]["label"], "a-key");
}

// ---------------------------------------------------------------------------
// DELETE /api-keys/{key_id}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_revoke_own_key() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a key.
  let body = serde_json::json!({ "label": "to-revoke" });
  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
  let json = body_json(response.into_body()).await;
  let key_id = json["key_id"].as_str().unwrap().to_string();

  // Revoke it.
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("DELETE")
    .uri(&format!("/api-keys/{}", key_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert_eq!(json["revoked"], true);
}

#[tokio::test]
async fn test_revoke_nonexistent_key() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let fake_id = uuid::Uuid::new_v4();
  let request = Request::builder()
    .method("DELETE")
    .uri(&format!("/api-keys/{}", fake_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_revoke_invalid_key_id() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/api-keys/not-a-uuid")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Root-only: create for another user
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_key_for_other_user_as_root() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);
  let other_user = uuid::Uuid::new_v4();

  let body = serde_json::json!({
    "label": "delegated",
    "user_id": other_user.to_string(),
  });

  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["user_id"], other_user.to_string());
}

#[tokio::test]
async fn test_create_key_for_other_forbidden() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let non_root_user = uuid::Uuid::new_v4();
  let auth = user_bearer_token(&jwt_manager, non_root_user);
  let other_user = uuid::Uuid::new_v4();

  let body = serde_json::json!({
    "label": "sneaky",
    "user_id": other_user.to_string(),
  });

  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Cross-user revoke forbidden for non-root
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_revoke_other_users_key_forbidden() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);
  let other_user = uuid::Uuid::new_v4();

  // Root creates a key for other_user.
  let body = serde_json::json!({ "label": "theirs", "user_id": other_user.to_string() });
  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth_root)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
  let json = body_json(response.into_body()).await;
  let key_id = json["key_id"].as_str().unwrap().to_string();

  // A different non-root user tries to revoke it.
  let attacker = uuid::Uuid::new_v4();
  let auth_attacker = user_bearer_token(&jwt_manager, attacker);
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("DELETE")
    .uri(&format!("/api-keys/{}", key_id))
    .header("authorization", &auth_attacker)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Root can revoke anyone's key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_root_can_revoke_other_users_key() {
  let (app, jwt_manager, engine, _temp_dir) = test_app();
  let auth_root = root_bearer_token(&jwt_manager);
  let other_user = uuid::Uuid::new_v4();

  // Root creates a key for other_user.
  let body = serde_json::json!({ "label": "theirs", "user_id": other_user.to_string() });
  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth_root)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
  let json = body_json(response.into_body()).await;
  let key_id = json["key_id"].as_str().unwrap().to_string();

  // Root revokes it.
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("DELETE")
    .uri(&format!("/api-keys/{}", key_id))
    .header("authorization", &auth_root)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Token exchange: expired key rejection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_token_exchange_rejects_expired_key() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();

  // Manually store a key that's already expired.
  let key_id = uuid::Uuid::new_v4();
  let plaintext_key = aeordb::auth::generate_api_key(key_id);
  let key_hash = aeordb::auth::hash_api_key(&plaintext_key).unwrap();
  let record = aeordb::auth::ApiKeyRecord {
    key_id,
    key_hash,
    user_id: uuid::Uuid::nil(),
    created_at: chrono::Utc::now(),
    is_revoked: false,
    expires_at: 1000, // epoch + 1 second — way in the past
    label: Some("expired-key".to_string()),
    rules: vec![],
  };

  let ctx = aeordb::engine::RequestContext::system();
  let system_tables = aeordb::engine::SystemTables::new(&engine);
  system_tables.store_api_key_for_bootstrap(&ctx, &record).unwrap();

  let app = rebuild_app(&jwt_manager, &engine);
  let body = serde_json::json!({ "api_key": plaintext_key });
  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("expired"));
}

// ---------------------------------------------------------------------------
// Token exchange: key_id embedded in JWT
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_token_exchange_embeds_key_id() {
  let (_app, jwt_manager, engine, _temp_dir) = test_app();

  // Create a fresh key via the self-service endpoint.
  let auth_root = root_bearer_token(&jwt_manager);
  let app = rebuild_app(&jwt_manager, &engine);
  let body = serde_json::json!({ "label": "embed-test" });
  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth_root)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
  let create_json = body_json(response.into_body()).await;
  let plaintext_key = create_json["key"].as_str().unwrap().to_string();
  let expected_key_id = create_json["key_id"].as_str().unwrap().to_string();

  // Exchange the key for a JWT.
  let app = rebuild_app(&jwt_manager, &engine);
  let exchange_body = serde_json::json!({ "api_key": plaintext_key });
  let request = Request::builder()
    .method("POST")
    .uri("/auth/token")
    .header("content-type", "application/json")
    .body(Body::from(serde_json::to_string(&exchange_body).unwrap()))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let token_json = body_json(response.into_body()).await;
  let token = token_json["token"].as_str().unwrap();

  // Decode the JWT and verify key_id is embedded.
  let claims = jwt_manager.verify_token(token).expect("valid token");
  assert_eq!(claims.key_id, Some(expected_key_id));
}

// ---------------------------------------------------------------------------
// Non-root can create keys for themselves
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_non_root_creates_own_key() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let user = uuid::Uuid::new_v4();
  let auth = user_bearer_token(&jwt_manager, user);

  let body = serde_json::json!({ "label": "my-personal-key" });
  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["user_id"], user.to_string());
  assert_eq!(json["label"], "my-personal-key");
}

// ---------------------------------------------------------------------------
// Unauthenticated requests rejected
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_unauthenticated_create_rejected() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .body(Body::from(r#"{"label": "no-auth"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_unauthenticated_list_rejected() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/api-keys")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_unauthenticated_revoke_rejected() {
  let (app, _, _, _temp_dir) = test_app();
  let fake_id = uuid::Uuid::new_v4();

  let request = Request::builder()
    .method("DELETE")
    .uri(&format!("/api-keys/{}", fake_id))
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Expires_in_days minimum clamped to 1
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_key_zero_days_clamped_to_one() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let body = serde_json::json!({ "expires_in_days": 0 });
  let request = Request::builder()
    .method("POST")
    .uri("/api-keys")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(serde_json::to_string(&body).unwrap()))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let expires_at = json["expires_at"].as_i64().unwrap();
  let now_millis = chrono::Utc::now().timestamp_millis();
  let one_day_millis = 1 * 24 * 60 * 60 * 1000;

  // Should be approximately 1 day from now.
  let diff = (expires_at - now_millis - one_day_millis).abs();
  assert!(diff < 10_000, "Expected ~1 day expiry, diff was {} ms", diff);
}
