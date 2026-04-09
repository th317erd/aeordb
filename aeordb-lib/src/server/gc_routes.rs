use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::TokenClaims;
use crate::engine::gc::run_gc;
use crate::engine::{RequestContext, is_root};
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
    let user_id = match Uuid::parse_str(&claims.sub) {
        Ok(id) => id,
        Err(_) => {
            return (StatusCode::FORBIDDEN, Json(serde_json::json!({
                "error": "Invalid user ID"
            }))).into_response();
        }
    };

    if !is_root(&user_id) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({
            "error": "Only root user can run garbage collection"
        }))).into_response();
    }

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
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("GC failed: {}", e)
            }))).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
                "error": format!("GC task panicked: {}", e)
            }))).into_response()
        }
    }
}
