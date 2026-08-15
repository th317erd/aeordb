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

#[test]
fn migration_gc_suspension_is_retryable_and_exposes_only_the_fencing_token() {
  let error = EngineError::MigrationGcSuspended { migration_id: [0x71; 16], fencing_token: 9 };
  assert_eq!(engine_error_status(&error), StatusCode::SERVICE_UNAVAILABLE);
  assert_eq!(engine_error_code(&error), "SERVICE_UNAVAILABLE");
  assert_eq!(
    sanitize_engine_error("GC unavailable", &error),
    "GC unavailable: mutating garbage collection is suspended by migration fencing token 9"
  );
}
