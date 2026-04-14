use axum::{
    Extension,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::engine::directory_ops::DirectoryOps;
use crate::engine::request_context::RequestContext;

#[derive(Deserialize)]
pub struct CreateSymlinkRequest {
    pub target: Option<String>,
}

/// POST /engine-symlink/{*path} — create or update a symlink.
pub async fn create_symlink(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(path): Path<String>,
    Json(payload): Json<CreateSymlinkRequest>,
) -> Response {
    let target = match payload.target {
        Some(ref t) if !t.is_empty() => t.as_str(),
        _ => {
            return ErrorResponse::new("Request must include non-empty 'target' field")
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
        }
    };

    let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
    let ops = DirectoryOps::new(&state.engine);

    match ops.store_symlink(&ctx, &path, target) {
        Ok(record) => {
            let response = serde_json::json!({
                "path": record.path,
                "target": record.target,
                "entry_type": 8,
                "created_at": record.created_at,
                "updated_at": record.updated_at,
            });
            (StatusCode::CREATED, Json(response)).into_response()
        }
        Err(error) => {
            tracing::error!("Failed to create symlink at '{}': {}", path, error);
            ErrorResponse::new(format!("Failed to create symlink: {}", error))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}
