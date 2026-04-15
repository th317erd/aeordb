use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::auth::rate_limiter::RateLimiter;
use aeordb::engine::{EventBus, StorageEngine};
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
// User CRUD tests
// ===========================================================================

#[tokio::test]
async fn test_create_user_returns_201() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"alice","email":"alice@example.com"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_create_user_has_uuid() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"bob"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  let user_id = json["user_id"].as_str().expect("user_id should be a string");
  assert!(
    uuid::Uuid::parse_str(user_id).is_ok(),
    "user_id should be a valid UUID, got: {}",
    user_id,
  );
  assert_eq!(json["username"], "bob");
  assert_eq!(json["is_active"], true);
}

#[tokio::test]
async fn test_list_users() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a user first.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"charlie"}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // List users.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("GET")
    .uri("/admin/users")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let users = json.as_array().expect("response should be an array");
  assert!(!users.is_empty(), "should have at least one user");
  assert!(users.iter().any(|u| u["username"] == "charlie"));
}

#[tokio::test]
async fn test_get_user() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a user.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"diana","email":"diana@test.com"}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
  let created = body_json(response.into_body()).await;
  let user_id = created["user_id"].as_str().unwrap();

  // Get user.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("GET")
    .uri(&format!("/admin/users/{}", user_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["username"], "diana");
  assert_eq!(json["email"], "diana@test.com");
  assert_eq!(json["user_id"], user_id);
}

#[tokio::test]
async fn test_get_user_404() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let nonexistent_id = uuid::Uuid::new_v4();
  let request = Request::builder()
    .method("GET")
    .uri(&format!("/admin/users/{}", nonexistent_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_update_user() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create user.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"eve"}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  let created = body_json(response.into_body()).await;
  let user_id = created["user_id"].as_str().unwrap().to_string();

  // Update user.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("PATCH")
    .uri(&format!("/admin/users/{}", user_id))
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"eve_updated","email":"eve@new.com"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["username"], "eve_updated");
  assert_eq!(json["email"], "eve@new.com");
}

#[tokio::test]
async fn test_update_user_404() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let nonexistent_id = uuid::Uuid::new_v4();
  let request = Request::builder()
    .method("PATCH")
    .uri(&format!("/admin/users/{}", nonexistent_id))
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"nope"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_deactivate_user() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create user.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"frank"}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  let created = body_json(response.into_body()).await;
  let user_id = created["user_id"].as_str().unwrap().to_string();

  // Deactivate.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("DELETE")
    .uri(&format!("/admin/users/{}", user_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["deactivated"], true);
  assert_eq!(json["user_id"], user_id);

  // Verify user is inactive by fetching.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("GET")
    .uri(&format!("/admin/users/{}", user_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let json = body_json(response.into_body()).await;
  assert_eq!(json["is_active"], false);
}

#[tokio::test]
async fn test_deactivate_user_404() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let nonexistent_id = uuid::Uuid::new_v4();
  let request = Request::builder()
    .method("DELETE")
    .uri(&format!("/admin/users/{}", nonexistent_id))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// Group CRUD tests
// ===========================================================================

#[tokio::test]
async fn test_create_group() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/groups")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(
      r#"{"name":"engineers","default_allow":"crudli..","default_deny":"........","query_field":"user_id","query_operator":"in","query_value":"aaa,bbb"}"#,
    ))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["name"], "engineers");
  assert_eq!(json["query_field"], "user_id");
}

#[tokio::test]
async fn test_create_group_rejects_unsafe_field() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/groups")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(
      r#"{"name":"bad_group","default_allow":"........","default_deny":"........","query_field":"email","query_operator":"eq","query_value":"test@example.com"}"#,
    ))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);

  let json = body_json(response.into_body()).await;
  assert!(
    json["error"].as_str().unwrap().contains("Unsafe query field"),
    "Error should mention unsafe query field, got: {}",
    json["error"],
  );
}

#[tokio::test]
async fn test_list_groups() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a group.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/groups")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(
      r#"{"name":"viewers","default_allow":".r..l...","default_deny":"........","query_field":"is_active","query_operator":"eq","query_value":"true"}"#,
    ))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // List groups.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("GET")
    .uri("/admin/groups")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let groups = json.as_array().expect("response should be an array");
  assert!(groups.iter().any(|g| g["name"] == "viewers"));
}

#[tokio::test]
async fn test_get_group() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a group.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/groups")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(
      r#"{"name":"admins","default_allow":"crudlify","default_deny":"........","query_field":"user_id","query_operator":"eq","query_value":"some-uuid"}"#,
    ))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Get group.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("GET")
    .uri("/admin/groups/admins")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["name"], "admins");
  assert_eq!(json["default_allow"], "crudlify");
}

#[tokio::test]
async fn test_get_group_404() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/admin/groups/nonexistent_group")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_update_group() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a group.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/groups")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(
      r#"{"name":"devs","default_allow":"cr......","default_deny":"........","query_field":"user_id","query_operator":"eq","query_value":"old-val"}"#,
    ))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Update group.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("PATCH")
    .uri("/admin/groups/devs")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"default_allow":"crudli..","query_value":"new-val"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["default_allow"], "crudli..");
  assert_eq!(json["query_value"], "new-val");
}

#[tokio::test]
async fn test_update_group_404() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PATCH")
    .uri("/admin/groups/nonexistent_group")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"default_allow":"crudlify"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_update_group_rejects_unsafe_field() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a group.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/groups")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(
      r#"{"name":"safe_group","default_allow":"........","default_deny":"........","query_field":"user_id","query_operator":"eq","query_value":"x"}"#,
    ))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Try to update query_field to an unsafe value.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("PATCH")
    .uri("/admin/groups/safe_group")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"query_field":"username"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_delete_group() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a group.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/groups")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(
      r#"{"name":"deleteme","default_allow":"........","default_deny":"........","query_field":"user_id","query_operator":"eq","query_value":"x"}"#,
    ))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Delete group.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("DELETE")
    .uri("/admin/groups/deleteme")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["deleted"], true);
  assert_eq!(json["name"], "deleteme");

  // Verify it is gone.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("GET")
    .uri("/admin/groups/deleteme")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_group_404() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/admin/groups/nonexistent_group")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ===========================================================================
// Authorization tests
// ===========================================================================

#[tokio::test]
async fn test_admin_requires_root_users_post() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = non_root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"should_fail"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_admin_requires_root_users_get() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = non_root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/admin/users")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_admin_requires_root_groups_post() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = non_root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/groups")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(
      r#"{"name":"nope","default_allow":"........","default_deny":"........","query_field":"user_id","query_operator":"eq","query_value":"x"}"#,
    ))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_admin_requires_root_groups_get() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = non_root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/admin/groups")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_admin_requires_root_user_delete() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = non_root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri(&format!("/admin/users/{}", uuid::Uuid::new_v4()))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_admin_requires_root_group_delete() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = non_root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/admin/groups/some_group")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_admin_requires_auth_no_token() {
  let (app, _, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/admin/users")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ===========================================================================
// Per-user auto-group creation test
// ===========================================================================

#[tokio::test]
async fn test_create_user_returns_auto_group() {
  let (_, jwt_manager, engine, rate_limiter, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  // Create a user.
  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("POST")
    .uri("/admin/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"autogroup_user"}"#))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);
  let created = body_json(response.into_body()).await;
  let user_id = created["user_id"].as_str().unwrap();

  // The auto-group should be named "user:{user_id}".
  let expected_group_name = format!("user:{}", user_id);

  let app = rebuild_app(&jwt_manager, &engine, &rate_limiter);
  let request = Request::builder()
    .method("GET")
    .uri(&format!("/admin/groups/{}", expected_group_name))
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["name"], expected_group_name);
  assert_eq!(json["query_field"], "user_id");
  assert_eq!(json["query_operator"], "eq");
  assert_eq!(json["query_value"], user_id);
}

// ===========================================================================
// Edge cases / invalid input
// ===========================================================================

#[tokio::test]
async fn test_get_user_invalid_uuid() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("GET")
    .uri("/admin/users/not-a-valid-uuid")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_user_malformed_json() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
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

#[tokio::test]
async fn test_create_user_missing_username() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/users")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"email":"no_username@test.com"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let status = response.status();
  assert!(
    status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
    "Expected 400 or 422 for missing username, got {}",
    status,
  );
}

#[tokio::test]
async fn test_create_group_malformed_json() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/groups")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"totally not json"#))
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
async fn test_create_group_missing_required_fields() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/admin/groups")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"name":"incomplete"}"#))
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
async fn test_deactivate_user_invalid_uuid() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("DELETE")
    .uri("/admin/users/bad-uuid")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_update_user_invalid_uuid() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app();
  let auth = root_bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PATCH")
    .uri("/admin/users/bad-uuid")
    .header("content-type", "application/json")
    .header("authorization", &auth)
    .body(Body::from(r#"{"username":"x"}"#))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
