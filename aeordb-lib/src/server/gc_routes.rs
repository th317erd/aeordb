use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::Deserialize;

use crate::auth::TokenClaims;
use crate::engine::gc::run_gc;
use crate::engine::RequestContext;
use crate::server::responses::{ErrorResponse, require_root};
use crate::server::state::AppState;

#[derive(Deserialize)]
pub struct GcParams {
    pub dry_run: Option<bool>,
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

    let engine = state.engine.clone();
    let result = tokio::task::spawn_blocking(move || {
        run_gc(&engine, &ctx, dry_run)
    }).await;

    match result {
        Ok(Ok(gc_result)) => {
            (StatusCode::OK, Json(serde_json::json!(gc_result))).into_response()
        }
        Ok(Err(e)) => {
            ErrorResponse::new(format!("GC failed: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
        Err(e) => {
            ErrorResponse::new(format!("GC task panicked: {}", e))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}
