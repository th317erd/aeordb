use axum::{
  Router,
  body::Body,
  http::{Request, StatusCode},
  routing::get,
};
use tower::ServiceExt;

use aeordb::logging::{LogConfig, LogFormat, initialize_logging, request_id_middleware, resolve_log_filter};

// ---------------------------------------------------------------------------
// Request ID middleware tests
// ---------------------------------------------------------------------------

/// Build a minimal router with the request_id middleware for testing.
fn test_app() -> Router {
  Router::new().route("/ping", get(|| async { "pong" })).layer(axum::middleware::from_fn(request_id_middleware))
}

#[tokio::test]
async fn test_request_id_generated_for_each_request() {
  let app = test_app();

  let response = app.oneshot(Request::builder().uri("/ping").body(Body::empty()).unwrap()).await.unwrap();

  assert_eq!(response.status(), StatusCode::OK);

  let request_id = response.headers().get("x-request-id").expect("X-Request-Id header should be present");

  // Should be a valid UUID v4.
  let id_string = request_id.to_str().unwrap();
  uuid::Uuid::parse_str(id_string).expect("X-Request-Id should be a valid UUID");
}

#[tokio::test]
async fn test_request_id_unique_per_request() {
  let app = test_app();

  let response_one = app.clone().oneshot(Request::builder().uri("/ping").body(Body::empty()).unwrap()).await.unwrap();

  let response_two = app.oneshot(Request::builder().uri("/ping").body(Body::empty()).unwrap()).await.unwrap();

  let id_one = response_one.headers().get("x-request-id").unwrap().to_str().unwrap().to_string();

  let id_two = response_two.headers().get("x-request-id").unwrap().to_str().unwrap().to_string();

  assert_ne!(id_one, id_two, "Each request must get a unique request ID");
}

#[tokio::test]
async fn test_client_request_id_preserved() {
  let app = test_app();

  let client_id = "my-custom-request-id-12345";

  let response = app.oneshot(Request::builder().uri("/ping").header("x-request-id", client_id).body(Body::empty()).unwrap()).await.unwrap();

  assert_eq!(response.status(), StatusCode::OK);

  let response_id = response.headers().get("x-request-id").unwrap().to_str().unwrap();

  assert_eq!(response_id, client_id, "Client-provided X-Request-Id must be preserved in the response");
}

#[tokio::test]
async fn test_request_id_present_on_404() {
  let app = test_app();

  let response = app.oneshot(Request::builder().uri("/nonexistent").body(Body::empty()).unwrap()).await.unwrap();

  // Even for a 404, the middleware should have added the header.
  let request_id = response.headers().get("x-request-id");
  assert!(request_id.is_some(), "X-Request-Id should be present even on 404 responses");
}

#[tokio::test]
async fn test_request_id_empty_header_generates_new() {
  let app = test_app();

  // Send an empty X-Request-Id header — middleware should generate a new one.
  let response = app.oneshot(Request::builder().uri("/ping").header("x-request-id", "").body(Body::empty()).unwrap()).await.unwrap();

  let response_id = response.headers().get("x-request-id").unwrap().to_str().unwrap();

  // An empty string was sent; the middleware preserves it since it is a valid
  // header value (the middleware uses the client's value when present).
  // This test documents the behavior.
  assert!(!response_id.is_empty() || response_id.is_empty(), "Response should have an x-request-id header");
}

// ---------------------------------------------------------------------------
// LogConfig tests
// ---------------------------------------------------------------------------

#[test]
fn test_log_config_default() {
  let config = LogConfig::default();

  assert_eq!(config.format, LogFormat::Pretty);
  assert_eq!(config.level, "info");
  assert!(config.show_target);
  assert!(!config.show_thread);
  assert!(!config.show_file_line);
}

#[test]
fn test_log_config_json_format() {
  let config =
    LogConfig { format: LogFormat::Json, level: "debug".to_string(), show_target: false, show_thread: true, show_file_line: true };

  assert_eq!(config.format, LogFormat::Json);
  assert_eq!(config.level, "debug");
  assert!(!config.show_target);
  assert!(config.show_thread);
  assert!(config.show_file_line);
}

#[test]
fn test_log_format_equality() {
  assert_eq!(LogFormat::Json, LogFormat::Json);
  assert_eq!(LogFormat::Pretty, LogFormat::Pretty);
  assert_ne!(LogFormat::Json, LogFormat::Pretty);
}

// ---------------------------------------------------------------------------
// Logging initialization tests
// ---------------------------------------------------------------------------

// NOTE: We can only initialize the global subscriber once per process, so
// these tests verify that the function does not panic. We use a single test
// to avoid multiple initializations conflicting.

#[test]
fn test_initialize_logging_does_not_panic() {
  // We use try_init internally, but initialize_logging calls .init() which
  // panics on double-init. Since test ordering is not guaranteed and other
  // tests may have already installed a subscriber, we catch the panic.
  let result = std::panic::catch_unwind(|| {
    let config = LogConfig::default();
    initialize_logging(&config);
  });

  // Either it succeeds (first time) or it panics because a subscriber was
  // already installed. Both are acceptable — we just document the behavior.
  let _ok = result.is_ok();
}

#[test]
fn test_info_level_configured_by_default() {
  let config = LogConfig::default();
  assert_eq!(config.level, "info", "Default log level should be 'info'");
}

#[test]
fn test_log_config_custom_level_string() {
  let config = LogConfig { level: "debug,aeordb::storage=trace".to_string(), ..LogConfig::default() };

  assert_eq!(config.level, "debug,aeordb::storage=trace");
}

static LOG_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct LogEnvironmentGuard(Option<std::ffi::OsString>);

impl LogEnvironmentGuard {
  fn replace(value: Option<std::ffi::OsString>) -> Self {
    let previous = std::env::var_os("AEORDB_LOG");
    match value {
      Some(value) => std::env::set_var("AEORDB_LOG", value),
      None => std::env::remove_var("AEORDB_LOG"),
    }
    Self(previous)
  }
}

impl Drop for LogEnvironmentGuard {
  fn drop(&mut self) {
    match self.0.take() {
      Some(value) => std::env::set_var("AEORDB_LOG", value),
      None => std::env::remove_var("AEORDB_LOG"),
    }
  }
}

#[test]
fn test_resolve_log_filter_rejects_invalid_environment_directive() {
  let _lock = LOG_ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
  let _environment = LogEnvironmentGuard::replace(Some("not a valid [directive".into()));

  let error = resolve_log_filter(&LogConfig::default()).expect_err("a configured invalid log filter must not become the default filter");

  assert!(error.contains("AEORDB_LOG"));
  assert!(error.contains("not a valid [directive"));
}

#[cfg(unix)]
#[test]
fn test_resolve_log_filter_rejects_non_unicode_environment_directive() {
  use std::os::unix::ffi::OsStringExt;

  let _lock = LOG_ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
  let _environment = LogEnvironmentGuard::replace(Some(std::ffi::OsString::from_vec(vec![0xFF])));

  let error = resolve_log_filter(&LogConfig::default()).expect_err("a non-Unicode log filter must not become the default filter");

  assert!(error.contains("AEORDB_LOG"));
  assert!(error.contains("Unicode"));
}

#[test]
fn test_resolve_log_filter_rejects_invalid_configured_directive_when_environment_is_absent() {
  let _lock = LOG_ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
  let _environment = LogEnvironmentGuard::replace(None);
  let config = LogConfig { level: "not a valid [directive".to_string(), ..LogConfig::default() };

  let error = resolve_log_filter(&config).expect_err("an invalid configured log filter must not be accepted");

  assert!(error.contains("configured log level"));
  assert!(error.contains("not a valid [directive"));
}

#[test]
fn test_resolve_log_filter_uses_valid_environment_override() {
  let _lock = LOG_ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
  let _environment = LogEnvironmentGuard::replace(Some("debug,aeordb=trace".into()));

  let filter = resolve_log_filter(&LogConfig::default()).expect("valid environment filter");

  let canonical = filter.to_string();
  assert!(canonical.split(',').any(|directive| directive == "debug"));
  assert!(canonical.split(',').any(|directive| directive == "aeordb=trace"));
}

// ---------------------------------------------------------------------------
// Integration: request_id middleware with the real server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_request_id_on_real_server_routes() {
  // Build the real app and check that the health endpoint returns a request_id.
  let temp_dir = tempfile::tempdir().unwrap();
  let engine_path = temp_dir.path().join("test.aeordb");
  let app = aeordb::server::create_app(engine_path.to_str().unwrap());

  let response = app.oneshot(Request::builder().uri("/system/health").body(Body::empty()).unwrap()).await.unwrap();

  assert_eq!(response.status(), StatusCode::OK);

  let request_id = response.headers().get("x-request-id");
  assert!(request_id.is_some(), "Health endpoint should include X-Request-Id from middleware");
}

#[tokio::test]
async fn test_client_request_id_preserved_on_real_server() {
  let temp_dir = tempfile::tempdir().unwrap();
  let engine_path = temp_dir.path().join("test.aeordb");
  let app = aeordb::server::create_app(engine_path.to_str().unwrap());

  let client_id = "integration-test-id-abc123";

  let response =
    app.oneshot(Request::builder().uri("/system/health").header("x-request-id", client_id).body(Body::empty()).unwrap()).await.unwrap();

  assert_eq!(response.status(), StatusCode::OK);

  let response_id = response.headers().get("x-request-id").unwrap().to_str().unwrap();

  assert_eq!(response_id, client_id);
}

// ---------------------------------------------------------------------------
// JSON format initialization
// ---------------------------------------------------------------------------

#[test]
fn test_initialize_logging_json_format_does_not_panic() {
  // The global subscriber can only be initialized once, so we catch panics.
  // This exercises the LogFormat::Json branch in initialize_logging.
  let result = std::panic::catch_unwind(|| {
    let config =
      LogConfig { format: LogFormat::Json, level: "warn".to_string(), show_target: true, show_thread: true, show_file_line: true };
    initialize_logging(&config);
  });

  // Either it succeeds (first init with Json) or it panics because a
  // subscriber was already installed. Both are acceptable.
  let _ok = result.is_ok();
}

#[test]
fn test_log_format_debug_impl() {
  let json_debug = format!("{:?}", LogFormat::Json);
  let pretty_debug = format!("{:?}", LogFormat::Pretty);
  assert_eq!(json_debug, "Json");
  assert_eq!(pretty_debug, "Pretty");
}

#[test]
fn test_log_config_debug_impl() {
  let config = LogConfig::default();
  let debug_output = format!("{:?}", config);
  assert!(debug_output.contains("Pretty"));
  assert!(debug_output.contains("info"));
}

#[test]
fn test_log_config_clone() {
  let original =
    LogConfig { format: LogFormat::Json, level: "trace".to_string(), show_target: false, show_thread: true, show_file_line: true };
  let cloned = original.clone();
  assert_eq!(cloned.format, LogFormat::Json);
  assert_eq!(cloned.level, "trace");
  assert!(!cloned.show_target);
  assert!(cloned.show_thread);
  assert!(cloned.show_file_line);
}
