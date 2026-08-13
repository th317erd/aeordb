use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::Deserialize;

use crate::auth::TokenClaims;
use crate::engine::gc::{execute_gc_run, GcExecutionRequestV1};
use crate::engine::v4::gc_run::GcRunInvocationV1;
use crate::engine::RequestContext;
use crate::server::responses::{engine_error_response, require_root, ErrorResponse};
use crate::server::state::AppState;

#[derive(Deserialize)]
pub struct GcParams {
  pub dry_run: Option<bool>,
}

struct HttpGcCancellation {
  token: tokio_util::sync::CancellationToken,
  completed: bool,
}

impl Drop for HttpGcCancellation {
  fn drop(&mut self) {
    if !self.completed {
      self.token.cancel();
    }
  }
}

/// POST /admin/gc -- run garbage collection.
/// Query params: dry_run=true (default: false).
/// Requires root user.
pub async fn run_gc_endpoint(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Query(params): Query<GcParams>,
) -> Response {
  let _user_id = match require_root(&claims) {
    Ok(id) => id,
    Err(response) => return response,
  };

  let dry_run = params.dry_run.unwrap_or(false);
  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let cancellation = tokio_util::sync::CancellationToken::new();
  let mut request_cancellation = HttpGcCancellation { token: cancellation.clone(), completed: false };

  let engine = state.engine.clone();
  let result = tokio::task::spawn_blocking(move || {
    execute_gc_run(&engine, &ctx, GcExecutionRequestV1::new(GcRunInvocationV1::Http, dry_run, cancellation))
      .map(|execution| execution.result)
  })
  .await;
  request_cancellation.completed = true;

  match result {
    Ok(Ok(gc_result)) => (StatusCode::OK, Json(serde_json::json!(gc_result))).into_response(),
    Ok(Err(error)) => engine_error_response("GC failed", &error),
    Err(e) => ErrorResponse::new(format!("GC task panicked unexpectedly: {}. This is a bug — please report it", e))
      .with_status(StatusCode::INTERNAL_SERVER_ERROR)
      .into_response(),
  }
}
