use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serial_test::serial;
use tower::ServiceExt;

use aeordb::auth::jwt::{JwtManager, TokenClaims, DEFAULT_EXPIRY_SECONDS};
use aeordb::metrics::initialize_metrics;
use aeordb::server::create_app_with_jwt_and_metrics;
use aeordb::storage::RedbStorage;

/// Create a fresh in-memory app with an isolated Prometheus recorder.
fn test_app_with_metrics() -> (axum::Router, Arc<JwtManager>, Arc<RedbStorage>, metrics_exporter_prometheus::PrometheusHandle) {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = Arc::new(JwtManager::generate());
  let prometheus_handle = metrics_exporter_prometheus::PrometheusBuilder::new()
    .build_recorder()
    .handle();
  let app = create_app_with_jwt_and_metrics(storage.clone(), jwt_manager.clone(), prometheus_handle.clone());
  (app, jwt_manager, storage, prometheus_handle)
}

/// Create an app that uses a globally-installed Prometheus recorder.
fn test_app_with_global_metrics() -> (axum::Router, Arc<JwtManager>, Arc<RedbStorage>, metrics_exporter_prometheus::PrometheusHandle) {
  let storage = Arc::new(RedbStorage::new_in_memory().expect("in-memory storage"));
  let jwt_manager = Arc::new(JwtManager::generate());
  let prometheus_handle = initialize_metrics();
  let app = create_app_with_jwt_and_metrics(storage.clone(), jwt_manager.clone(), prometheus_handle.clone());
  (app, jwt_manager, storage, prometheus_handle)
}

fn rebuild_app(
  storage: &Arc<RedbStorage>,
  jwt_manager: &Arc<JwtManager>,
  prometheus_handle: &metrics_exporter_prometheus::PrometheusHandle,
) -> axum::Router {
  create_app_with_jwt_and_metrics(storage.clone(), jwt_manager.clone(), prometheus_handle.clone())
}

/// Create an admin Bearer token value (including "Bearer " prefix).
fn bearer_token(jwt_manager: &JwtManager) -> String {
  let now = chrono::Utc::now().timestamp();
  let claims = TokenClaims {
    sub: "test-admin".to_string(),
    iss: "aeordb".to_string(),
    iat: now,
    exp: now + DEFAULT_EXPIRY_SECONDS,
    roles: vec!["admin".to_string()],
    scope: None,
    permissions: None,
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
// Metrics endpoint access
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_metrics_endpoint_returns_200() {
  let (app, jwt_manager, _, _) = test_app_with_metrics();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/admin/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_metrics_endpoint_returns_prometheus_format() {
  let (app, jwt_manager, _, _) = test_app_with_metrics();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/admin/metrics")
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
  let (app, _, _, _) = test_app_with_metrics();

  // No auth header at all.
  let request = Request::builder()
    .uri("/admin/metrics")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_metrics_endpoint_rejects_invalid_token() {
  let (app, _, _, _) = test_app_with_metrics();

  let request = Request::builder()
    .uri("/admin/metrics")
    .header("authorization", "Bearer invalid-token")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Chunk write increments counter (uses global recorder)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_chunk_write_increments_counter() {
  let (app, jwt_manager, storage, prometheus_handle) = test_app_with_global_metrics();
  let auth = bearer_token(&jwt_manager);

  // Store a file (which internally stores chunks).
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/metrics-test/chunk-write.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("hello metrics world"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Now check the metrics output.
  let app = rebuild_app(&storage, &jwt_manager, &prometheus_handle);
  let request = Request::builder()
    .uri("/admin/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  let output = body_string(response.into_body()).await;
  assert!(
    output.contains("aeordb_chunks_stored_total"),
    "metrics output should contain aeordb_chunks_stored_total, got:\n{}",
    output
  );
  assert!(
    output.contains("aeordb_files_stored_total"),
    "metrics output should contain aeordb_files_stored_total, got:\n{}",
    output
  );
}

// ---------------------------------------------------------------------------
// File store records metrics
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_file_store_records_metrics() {
  let (app, jwt_manager, storage, prometheus_handle) = test_app_with_global_metrics();
  let auth = bearer_token(&jwt_manager);

  let data = "some file data for metrics test";
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/metrics-test/file-store.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from(data))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  let app = rebuild_app(&storage, &jwt_manager, &prometheus_handle);
  let request = Request::builder()
    .uri("/admin/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let output = body_string(response.into_body()).await;

  assert!(
    output.contains("aeordb_file_store_duration_seconds"),
    "metrics should contain file store duration histogram"
  );
  assert!(
    output.contains("aeordb_file_bytes_stored_total"),
    "metrics should contain file bytes stored counter"
  );
}

// ---------------------------------------------------------------------------
// HTTP request duration recorded
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_http_request_duration_recorded() {
  let (app, _, storage, prometheus_handle) = test_app_with_global_metrics();

  // Make a simple health check request (public, no auth needed).
  let request = Request::builder()
    .uri("/admin/health")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Rebuild to check metrics. Use a dummy JWT manager to read metrics.
  let jwt_manager = Arc::new(JwtManager::generate());
  let app = create_app_with_jwt_and_metrics(storage.clone(), jwt_manager.clone(), prometheus_handle.clone());
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/admin/metrics")
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

// ---------------------------------------------------------------------------
// Auth failure records metric
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_auth_failure_records_metric() {
  let (app, _, storage, prometheus_handle) = test_app_with_global_metrics();

  // Send a request to a protected route with a bad token.
  let request = Request::builder()
    .uri("/admin/metrics")
    .header("authorization", "Bearer bad-token")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

  // Check metrics for auth validation failure.
  let jwt_manager = Arc::new(JwtManager::generate());
  let app = create_app_with_jwt_and_metrics(storage.clone(), jwt_manager.clone(), prometheus_handle.clone());
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/admin/metrics")
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

// ---------------------------------------------------------------------------
// Missing auth header records metric
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_missing_auth_header_records_metric() {
  let (app, _, storage, prometheus_handle) = test_app_with_global_metrics();

  // No auth header at all on protected route.
  let request = Request::builder()
    .uri("/admin/metrics")
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

  // Check metrics.
  let jwt_manager = Arc::new(JwtManager::generate());
  let app = create_app_with_jwt_and_metrics(storage.clone(), jwt_manager.clone(), prometheus_handle.clone());
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/admin/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let output = body_string(response.into_body()).await;

  assert!(
    output.contains("aeordb_auth_validations_total"),
    "metrics should contain auth validations counter for missing header"
  );
  // Check the specific label
  assert!(
    output.contains("missing_header"),
    "metrics should include 'missing_header' label, got:\n{}",
    output
  );
}

// ---------------------------------------------------------------------------
// Metrics endpoint returns empty output when no metrics recorded
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_metrics_endpoint_returns_empty_when_no_activity() {
  let (app, jwt_manager, _, _) = test_app_with_metrics();
  let auth = bearer_token(&jwt_manager);

  let request = Request::builder()
    .uri("/admin/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // With no activity, the prometheus output should be empty or minimal
  // (no aeordb-specific metrics should appear).
  let output = body_string(response.into_body()).await;
  assert!(
    !output.contains("aeordb_chunks_stored_total"),
    "no chunk metrics should appear without any activity"
  );
}

// ---------------------------------------------------------------------------
// File delete records metrics
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_file_delete_records_metric() {
  let (app, jwt_manager, storage, prometheus_handle) = test_app_with_global_metrics();
  let auth = bearer_token(&jwt_manager);

  // Store a file first.
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/metrics-test/to-delete.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("delete me"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Delete the file.
  let app = rebuild_app(&storage, &jwt_manager, &prometheus_handle);
  let request = Request::builder()
    .method("DELETE")
    .uri("/fs/metrics-test/to-delete.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Check metrics.
  let app = rebuild_app(&storage, &jwt_manager, &prometheus_handle);
  let request = Request::builder()
    .uri("/admin/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let output = body_string(response.into_body()).await;

  assert!(
    output.contains("aeordb_files_deleted_total"),
    "metrics should contain files deleted counter, got:\n{}",
    output
  );
  assert!(
    output.contains("aeordb_file_delete_duration_seconds"),
    "metrics should contain file delete duration histogram"
  );
}

// ---------------------------------------------------------------------------
// File read records metrics
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_file_read_records_metric() {
  let (app, jwt_manager, storage, prometheus_handle) = test_app_with_global_metrics();
  let auth = bearer_token(&jwt_manager);

  // Store a file first.
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/metrics-test/to-read.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("read me"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // Read the file.
  let app = rebuild_app(&storage, &jwt_manager, &prometheus_handle);
  let request = Request::builder()
    .uri("/fs/metrics-test/to-read.txt")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Check metrics.
  let app = rebuild_app(&storage, &jwt_manager, &prometheus_handle);
  let request = Request::builder()
    .uri("/admin/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let output = body_string(response.into_body()).await;

  assert!(
    output.contains("aeordb_files_read_total"),
    "metrics should contain files read counter, got:\n{}",
    output
  );
}

// ---------------------------------------------------------------------------
// Directory listing records metric
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_directory_list_records_metric() {
  let (app, jwt_manager, storage, prometheus_handle) = test_app_with_global_metrics();
  let auth = bearer_token(&jwt_manager);

  // Store a file to create the directory.
  let request = Request::builder()
    .method("PUT")
    .uri("/fs/metrics-dir/sample.txt")
    .header("content-type", "text/plain")
    .header("authorization", &auth)
    .body(Body::from("sample"))
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::CREATED);

  // List the directory.
  let app = rebuild_app(&storage, &jwt_manager, &prometheus_handle);
  let request = Request::builder()
    .uri("/fs/metrics-dir")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);

  // Check metrics.
  let app = rebuild_app(&storage, &jwt_manager, &prometheus_handle);
  let request = Request::builder()
    .uri("/admin/metrics")
    .header("authorization", &auth)
    .body(Body::empty())
    .unwrap();

  let response = app.oneshot(request).await.unwrap();
  let output = body_string(response.into_body()).await;

  assert!(
    output.contains("aeordb_directory_list_duration_seconds"),
    "metrics should contain directory list duration histogram, got:\n{}",
    output
  );
}
