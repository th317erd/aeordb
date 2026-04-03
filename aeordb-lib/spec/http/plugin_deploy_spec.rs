use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::StorageEngine;
use aeordb::server::{create_app_with_jwt, create_temp_engine_for_tests};

/// Create a fresh app with a shared JwtManager.
fn test_app() -> (axum::Router, Arc<JwtManager>, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt(jwt_manager.clone(), engine.clone());
  (app, jwt_manager, engine, temp_dir)
}

/// Rebuild the app from shared state.
fn rebuild_app(jwt_manager: &Arc<JwtManager>, engine: &Arc<StorageEngine>) -> axum::Router {
  create_app_with_jwt(jwt_manager.clone(), engine.clone())
}

/// Create an admin Bearer token value (including "Bearer " prefix).
fn bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "test-admin".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,

    scope: None,
    permissions: None,
  };
  let token = jwt_manager.create_token(&claims).expect("create token");
  format!("Bearer {}", token)
}

/// Helper to collect the response body into bytes.
async fn body_bytes(body: Body) -> Vec<u8> {
  body.collect().await.unwrap().to_bytes().to_vec()
}

/// Helper to collect the response body into a JSON value.
async fn body_json(body: Body) -> serde_json::Value {
  let bytes = body_bytes(body).await;
  serde_json::from_slice(&bytes).expect("valid JSON response body")
}

/// Compile a minimal valid WASM module.
fn minimal_wasm_bytes() -> Vec<u8> {
  let wat = r#"
  (module
    (memory (export "memory") 1)
    (func (export "handle") (param $request_ptr i32) (param $request_len i32) (result i64)
      (i64.or
        (i64.shl
          (i64.extend_i32_u (local.get $request_ptr))
          (i64.const 32)
        )
        (i64.extend_i32_u (local.get $request_len))
      )
    )
  )
  "#;
  wat::parse_str(wat).expect("WAT should be valid")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_deploy_wasm_plugin_returns_200() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let wasm_bytes = minimal_wasm_bytes();

  let request = Request::builder()
    .method("PUT")
    .uri("/testdb/public/myfunc/_deploy?name=myfunc")
    .header("authorization", &auth)
    .body(Body::from(wasm_bytes))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["name"], "myfunc");
  assert_eq!(json["path"], "testdb/public/myfunc");
  assert_eq!(json["plugin_type"], "wasm");
  assert!(json["plugin_id"].is_string());
}

#[tokio::test]
async fn test_deploy_invalid_wasm_returns_400() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let garbage = vec![0x00, 0x61, 0x73, 0x6d, 0xFF, 0xFF, 0xFF, 0xFF];
  let request = Request::builder()
    .method("PUT")
    .uri("/testdb/public/badfunc/_deploy?name=badfunc")
    .header("authorization", &auth)
    .body(Body::from(garbage))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);

  let json = body_json(response.into_body()).await;
  assert!(json["error"].as_str().unwrap().contains("Invalid plugin"));
}

#[tokio::test]
async fn test_deploy_empty_body_returns_400() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/testdb/public/empty/_deploy?name=empty")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_invoke_deployed_plugin_returns_result() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let wasm_bytes = minimal_wasm_bytes();

  // Deploy
  let app = rebuild_app(&jwt_manager, &engine);
  let deploy_request = Request::builder()
    .method("PUT")
    .uri("/testdb/public/echo/_deploy?name=echo")
    .header("authorization", &auth)
    .body(Body::from(wasm_bytes))
    .unwrap();
  let deploy_response = app.oneshot(deploy_request).await.unwrap();
  assert_eq!(deploy_response.status(), StatusCode::OK);

  // Invoke
  let app = rebuild_app(&jwt_manager, &engine);
  let invoke_request = Request::builder()
    .method("POST")
    .uri("/testdb/public/echo/handle/_invoke")
    .header("authorization", &auth)
    .body(Body::from("hello plugin"))
    .unwrap();
  let invoke_response = app.oneshot(invoke_request).await.unwrap();
  assert_eq!(invoke_response.status(), StatusCode::OK);

  let response_bytes = body_bytes(invoke_response.into_body()).await;
  assert_eq!(response_bytes, b"hello plugin");
}

#[tokio::test]
async fn test_invoke_nonexistent_plugin_returns_404() {
  let (app, jwt_manager, _, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("POST")
    .uri("/testdb/public/missing/handle/_invoke")
    .header("authorization", &auth)
    .body(Body::from("data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_list_deployed_plugins() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let wasm_bytes = minimal_wasm_bytes();

  // Deploy two plugins
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/testdb/public/func_a/_deploy?name=func_a")
    .header("authorization", &auth)
    .body(Body::from(wasm_bytes.clone()))
    .unwrap();
  app.oneshot(request).await.unwrap();

  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/testdb/public/func_b/_deploy?name=func_b")
    .header("authorization", &auth)
    .body(Body::from(wasm_bytes))
    .unwrap();
  app.oneshot(request).await.unwrap();

  // List
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("GET")
    .uri("/testdb/_plugins")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  let plugins = json.as_array().expect("should be an array");
  assert_eq!(plugins.len(), 2);
}

#[tokio::test]
async fn test_remove_deployed_plugin() {
  let (_, jwt_manager, engine, _temp_dir) = test_app();
  let auth = bearer_token(&jwt_manager);
  let wasm_bytes = minimal_wasm_bytes();

  // Deploy
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("PUT")
    .uri("/testdb/public/removeme/_deploy?name=removeme")
    .header("authorization", &auth)
    .body(Body::from(wasm_bytes))
    .unwrap();
  app.oneshot(request).await.unwrap();

  // Remove
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("DELETE")
    .uri("/testdb/public/removeme/handle/_remove")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let json = body_json(response.into_body()).await;
  assert_eq!(json["removed"], true);

  // Verify it is gone
  let app = rebuild_app(&jwt_manager, &engine);
  let request = Request::builder()
    .method("POST")
    .uri("/testdb/public/removeme/handle/_invoke")
    .header("authorization", &auth)
    .body(Body::from("data"))
    .unwrap();
  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_deploy_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let wasm_bytes = minimal_wasm_bytes();
  let request = Request::builder()
    .method("PUT")
    .uri("/testdb/public/func/_deploy?name=func")
    // No authorization header
    .body(Body::from(wasm_bytes))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_list_plugins_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("GET")
    .uri("/testdb/_plugins")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_invoke_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("POST")
    .uri("/testdb/public/func/handle/_invoke")
    .body(Body::from("data"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_remove_requires_auth() {
  let (app, _, _, _temp_dir) = test_app();

  let request = Request::builder()
    .method("DELETE")
    .uri("/testdb/public/func/handle/_remove")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
