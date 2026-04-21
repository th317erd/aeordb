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
use crate::engine::directory_ops::{DirectoryOps, is_system_path};
use crate::engine::path_utils::normalize_path;
use crate::engine::request_context::RequestContext;

#[derive(Deserialize)]
pub struct CreateSymlinkRequest {
    pub target: Option<String>,
}

/// PUT /links/{*path} — create or update a symlink.
pub async fn create_symlink(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(path): Path<String>,
    Json(payload): Json<CreateSymlinkRequest>,
) -> Response {
    let target = match payload.target {
        Some(ref t) if !t.is_empty() => t.as_str(),
        _ => {
            return ErrorResponse::new("Missing required field 'target' in request body. Symlink creation requires {\"target\": \"/path/to/target\"}")
                .with_status(StatusCode::BAD_REQUEST)
                .into_response();
        }
    };

    // Block ALL users from creating symlinks that point to /.system/ paths —
    // system data is invisible through the API for all users, including root.
    let normalized_target = normalize_path(target);
    if is_system_path(&normalized_target) {
        return ErrorResponse::new(format!("Not found: {}", path))
            .with_status(StatusCode::NOT_FOUND)
            .into_response();
    }

    // Block ALL users from creating symlinks under /.system/ paths
    let normalized_path = normalize_path(&path);
    if is_system_path(&normalized_path) {
        return ErrorResponse::new(format!("Not found: {}", path))
            .with_status(StatusCode::NOT_FOUND)
            .into_response();
    }

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
        Err(crate::engine::errors::EngineError::InvalidInput(msg)) => {
            ErrorResponse::new(msg)
                .with_status(StatusCode::BAD_REQUEST)
                .into_response()
        }
        Err(error) => {
            tracing::error!("Failed to create symlink at '{}': {}", path, error);
            ErrorResponse::new(format!("Failed to create symlink: {}", error))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

/// GET /links/{*path} — read symlink metadata without following it.
pub async fn get_symlink(
    State(state): State<AppState>,
    Extension(_claims): Extension<TokenClaims>,
    Path(path): Path<String>,
) -> Response {
    // Block ALL access to /.system/ via API — system data is only accessible
    // through the internal system_store module, never through HTTP endpoints.
    let normalized_path = normalize_path(&path);
    if is_system_path(&normalized_path) {
        return ErrorResponse::new(format!("Not found: {}", path))
            .with_status(StatusCode::NOT_FOUND)
            .into_response();
    }

    let ops = DirectoryOps::new(&state.engine);

    match ops.get_symlink(&path) {
        Ok(Some(symlink_record)) => {
            (StatusCode::OK, Json(serde_json::json!({
                "path": symlink_record.path,
                "target": symlink_record.target,
                "entry_type": 8,
                "created_at": symlink_record.created_at,
                "updated_at": symlink_record.updated_at,
            }))).into_response()
        }
        Ok(None) => {
            ErrorResponse::new(format!("Symlink not found at '{}'. Verify the path or use GET /files/ to browse", path))
                .with_status(StatusCode::NOT_FOUND)
                .into_response()
        }
        Err(error) => {
            tracing::error!("Failed to get symlink at '{}': {}", path, error);
            ErrorResponse::new(format!("Failed to get symlink: {}", error))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}

/// DELETE /links/{*path} — delete a symlink.
pub async fn delete_symlink(
    State(state): State<AppState>,
    Extension(_claims): Extension<TokenClaims>,
    Path(path): Path<String>,
) -> Response {
    // Block ALL access to /.system/ via API — system data is only accessible
    // through the internal system_store module, never through HTTP endpoints.
    let normalized_path = normalize_path(&path);
    if is_system_path(&normalized_path) {
        return ErrorResponse::new(format!("Not found: {}", path))
            .with_status(StatusCode::NOT_FOUND)
            .into_response();
    }

    let ctx = RequestContext::from_claims(&_claims.sub, state.event_bus.clone());
    let ops = DirectoryOps::new(&state.engine);

    match ops.delete_symlink(&ctx, &path) {
        Ok(()) => {
            (StatusCode::OK, Json(serde_json::json!({
                "deleted": true,
                "path": path,
                "entry_type": "symlink",
            }))).into_response()
        }
        Err(error) => {
            tracing::error!("Failed to delete symlink at '{}': {}", path, error);
            ErrorResponse::new(format!("Failed to delete symlink: {}", error))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR)
                .into_response()
        }
    }
}
