use aeordb::engine::EngineError;
use aeordb::server::responses::{engine_error_code, engine_error_status, sanitize_engine_error};
use axum::http::StatusCode;

#[test]
fn resource_exhaustion_is_a_retryable_service_unavailable_response() {
  let error = EngineError::ResourceExhausted("index mutation memory limit reached".to_string());
  assert_eq!(engine_error_status(&error), StatusCode::SERVICE_UNAVAILABLE);
  assert_eq!(engine_error_code(&error), "SERVICE_UNAVAILABLE");
  assert_eq!(sanitize_engine_error("write failed", &error), "write failed: index mutation memory limit reached");
}
