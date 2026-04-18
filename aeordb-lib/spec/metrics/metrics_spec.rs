use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serial_test::serial;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::engine::StorageEngine;
use aeordb::metrics::initialize_metrics;
use aeordb::server::{create_app_with_jwt_and_metrics, create_temp_engine_for_tests};

/// Create a fresh app with a standalone Prometheus recorder.
fn test_app_standalone() -> (axum::Router, Arc<JwtManager>, metrics_exporter_prometheus::PrometheusHandle, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let prometheus_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
    .build_recorder()
    .handle();
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_metrics(jwt_manager.clone(), prometheus_handle.clone(), engine.clone());
  (app, jwt_manager, prometheus_handle, engine, temp_dir)
}

/// Create a fresh app wired to the global Prometheus recorder.
fn test_app_global() -> (axum::Router, Arc<JwtManager>, metrics_exporter_prometheus::PrometheusHandle, Arc<StorageEngine>, tempfile::TempDir) {
  let jwt_manager = Arc::new(JwtManager::generate());
  let prometheus_handle = initialize_metrics();
  let (engine, temp_dir) = create_temp_engine_for_tests();
  let app = create_app_with_jwt_and_metrics(jwt_manager.clone(), prometheus_handle.clone(), engine.clone());
  (app, jwt_manager, prometheus_handle, engine, temp_dir)
}

fn rebuild_app(
  jwt_manager: &Arc<JwtManager>,
  prometheus_handle: &metrics_exporter_prometheus::PrometheusHandle,
  engine: &Arc<StorageEngine>,
) -> axum::Router {
  create_app_with_jwt_and_metrics(jwt_manager.clone(), prometheus_handle.clone(), engine.clone())
}

/// Create an admin Bearer token value (including "Bearer " prefix).
fn bearer_token(jwt_manager: &JwtManager) -> String {
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

/// Collect response body into bytes.
async fn body_bytes(body: Body) -> Vec<u8> {
  body.collect().await.unwrap().to_bytes().to_vec()
}

/// Collect response body into a string.
async fn body_string(body: Body) -> String {
  String::from_utf8(body_bytes(body).await).expect("valid utf8")
}

// ---------------------------------------------------------------------------
// Metrics endpoint access (standalone -- no global recorder needed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_metrics_endpoint_returns_200() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app_standalone();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/system/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_metrics_endpoint_returns_prometheus_format() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app_standalone();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/system/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let content_type = response
    .headers()
    .get("content-type")
    .expect("content-type header")
    .to_str()
    .unwrap();
  assert!(
    content_type.contains("text/plain"),
    "expected text/plain content type, got: {}",
    content_type
  );
}

#[tokio::test]
async fn test_metrics_endpoint_requires_auth() {
  let (app, _, _, _, _temp_dir) = test_app_standalone();

  let request = Request::builder()
    .uri("/system/metrics")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_metrics_endpoint_rejects_invalid_token() {
  let (app, _, _, _, _temp_dir) = test_app_standalone();

  let request = Request::builder()
    .uri("/system/metrics")
    .header("authorization", "Bearer invalid-token")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_metrics_endpoint_returns_empty_when_no_activity() {
  let (app, jwt_manager, _, _, _temp_dir) = test_app_standalone();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/system/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let output = body_string(response.into_body()).await;
  assert!(
    !output.contains("aeordb_chunks_stored_total"),
    "no chunk metrics should appear without any activity"
  );
}

// ---------------------------------------------------------------------------
// Tests that exercise the global recorder (must be #[serial])
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_http_request_duration_recorded() {
  let (app, _, prometheus_handle, engine, _temp_dir) = test_app_global();

  let request = Request::builder()
    .uri("/system/health")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let jwt_manager = Arc::new(JwtManager::generate());
  let app = create_app_with_jwt_and_metrics(jwt_manager.clone(), prometheus_handle.clone(), engine.clone());
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/system/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let output = body_string(response.into_body()).await;

  assert!(
    output.contains("aeordb_http_requests_total"),
    "metrics should contain HTTP request counter, got:\n{}",
    output
  );
  assert!(
    output.contains("aeordb_http_request_duration_seconds"),
    "metrics should contain HTTP request duration histogram, got:\n{}",
    output
  );
}

#[tokio::test]
#[serial]
async fn test_auth_failure_records_metric() {
  let (app, _, prometheus_handle, engine, _temp_dir) = test_app_global();

  let request = Request::builder()
    .uri("/system/metrics")
    .header("authorization", "Bearer bad-token")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

  let jwt_manager = Arc::new(JwtManager::generate());
  let app = create_app_with_jwt_and_metrics(jwt_manager.clone(), prometheus_handle.clone(), engine.clone());
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/system/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let output = body_string(response.into_body()).await;

  assert!(
    output.contains("aeordb_auth_validations_total"),
    "metrics should contain auth validations counter, got:\n{}",
    output
  );
}

#[tokio::test]
#[serial]
async fn test_missing_auth_header_records_metric() {
  let (app, _, prometheus_handle, engine, _temp_dir) = test_app_global();

  let request = Request::builder()
    .uri("/system/metrics")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

  let jwt_manager = Arc::new(JwtManager::generate());
  let app = create_app_with_jwt_and_metrics(jwt_manager.clone(), prometheus_handle.clone(), engine.clone());
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/system/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let output = body_string(response.into_body()).await;

  assert!(
    output.contains("aeordb_auth_validations_total"),
    "metrics should contain auth validations counter for missing header"
  );
  assert!(
    output.contains("missing_header"),
    "metrics should include 'missing_header' label, got:\n{}",
    output
  );
}

#[tokio::test]
#[serial]
async fn test_engine_file_store_records_metrics() {
  let (app, jwt_manager, prometheus_handle, engine, _temp_dir) = test_app_global();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .method("PUT")
    .uri("/files/metrics-test/file-store.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("hello metrics world"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app = rebuild_app(&jwt_manager, &prometheus_handle, &engine);
  let request = Request::builder()
    .uri("/system/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let output = body_string(response.into_body()).await;
  // The engine routes should have recorded HTTP metrics at minimum.
  assert!(
    output.contains("aeordb_http_requests_total"),
    "metrics should contain HTTP request counter after engine file store, got:\n{}",
    output
  );
}
