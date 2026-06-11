use axum::{
  http::StatusCode,
  response::{IntoResponse, Response},
};

use crate::engine::EngineResult;
use crate::server::responses::{engine_error_response, ErrorResponse};

/// Run disk/engine-bound route work on a blocking worker and map the common
/// engine/join failure cases into HTTP responses.
pub async fn run_engine_blocking<T, F>(operation: &'static str, failure_prefix: &'static str, work: F) -> Result<T, Response>
where
  T: Send + 'static,
  F: FnOnce() -> EngineResult<T> + Send + 'static,
{
  match tokio::task::spawn_blocking(work).await {
    Ok(Ok(value)) => Ok(value),
    Ok(Err(error)) => {
      tracing::error!("Engine: {} failed: {}", operation, error);
      Err(engine_error_response(failure_prefix, &error))
    }
    Err(join_error) => {
      tracing::error!("{} task panicked: {}", operation, join_error);
      Err(
        ErrorResponse::new(format!("{}: internal task error", failure_prefix))
          .with_status(StatusCode::INTERNAL_SERVER_ERROR)
          .into_response(),
      )
    }
  }
}
