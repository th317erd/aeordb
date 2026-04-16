use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::TokenClaims;
use crate::engine::conflict_store;
use crate::engine::{RequestContext, is_root};
use crate::server::state::AppState;

// ---------------------------------------------------------------------------
// Authorization helper (mirrors admin_routes pattern)
// ---------------------------------------------------------------------------

fn require_root(claims: &TokenClaims) -> Result<(), Response> {
    if let Ok(user_id) = Uuid::parse_str(&claims.sub) {
        if is_root(&user_id) {
            return Ok(());
        }
    }
    Err((
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({"error": "Admin access required"})),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// GET /admin/conflicts — list all unresolved conflicts
// ---------------------------------------------------------------------------

pub async fn list_conflicts(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
) -> Response {
    if let Err(response) = require_root(&claims) {
        return response;
    }

    let engine = state.engine.clone();
    let result =
        tokio::task::spawn_blocking(move || conflict_store::list_conflicts(&engine)).await;

    match result {
        Ok(Ok(conflicts)) => (StatusCode::OK, Json(serde_json::json!(conflicts))).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to list conflicts: {}", e)})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Task panicked: {}", e)})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// GET /admin/conflicts/{*path} — get conflict details for a specific path
// ---------------------------------------------------------------------------

pub async fn get_conflict(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(path): Path<String>,
) -> Response {
    if let Err(response) = require_root(&claims) {
        return response;
    }

    let full_path = format!("/{}", path);
    let engine = state.engine.clone();
    let result = tokio::task::spawn_blocking(move || {
        conflict_store::get_conflict(&engine, &full_path)
    })
    .await;

    match result {
        Ok(Ok(Some(meta))) => (StatusCode::OK, Json(meta)).into_response(),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("No conflict found for path: /{}", path)})),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to get conflict: {}", e)})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Task panicked: {}", e)})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /admin/conflict-resolve/{*path} — resolve a conflict by picking a version
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ResolveRequest {
    pub pick: String,
}

pub async fn resolve_conflict(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(path): Path<String>,
    Json(payload): Json<ResolveRequest>,
) -> Response {
    if let Err(response) = require_root(&claims) {
        return response;
    }

    let full_path = format!("/{}", path);
    let pick = payload.pick.clone();
    let engine = state.engine.clone();
    let event_bus = state.event_bus.clone();
    let sub = claims.sub.clone();

    let result = tokio::task::spawn_blocking(move || {
        let ctx = RequestContext::from_claims(&sub, event_bus);
        conflict_store::resolve_conflict(&engine, &ctx, &full_path, &payload.pick)
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"resolved": true, "path": format!("/{}", path), "pick": pick})),
        )
            .into_response(),
        Ok(Err(crate::engine::errors::EngineError::NotFound(msg))) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
        Ok(Err(crate::engine::errors::EngineError::InvalidInput(msg))) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to resolve conflict: {}", e)})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Task panicked: {}", e)})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /admin/conflict-dismiss/{*path} — dismiss a conflict (accept auto-winner)
// ---------------------------------------------------------------------------

pub async fn dismiss_conflict(
    State(state): State<AppState>,
    Extension(claims): Extension<TokenClaims>,
    Path(path): Path<String>,
) -> Response {
    if let Err(response) = require_root(&claims) {
        return response;
    }

    let full_path = format!("/{}", path);
    let engine = state.engine.clone();
    let event_bus = state.event_bus.clone();
    let sub = claims.sub.clone();

    let result = tokio::task::spawn_blocking(move || {
        let ctx = RequestContext::from_claims(&sub, event_bus);
        conflict_store::dismiss_conflict(&engine, &ctx, &full_path)
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({"dismissed": true, "path": format!("/{}", path)})),
        )
            .into_response(),
        Ok(Err(crate::engine::errors::EngineError::NotFound(msg))) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to dismiss conflict: {}", e)})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Task panicked: {}", e)})),
        )
            .into_response(),
    }
}
