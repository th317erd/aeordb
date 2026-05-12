//! Snapshot and fork HTTP handlers.
//!
//! Split out of `engine_routes.rs` to keep that file under control.
//! Handlers here cover the `/version/...` and `/versions/...` endpoints —
//! anything touching the named-version forest (snapshots + forks).

use std::collections::HashMap;

use axum::{
  extract::{Path, State},
  http::StatusCode,
  response::{IntoResponse, Response},
  Extension, Json,
};
use serde::Deserialize;

use crate::auth::TokenClaims;
use crate::engine::errors::EngineError;
use crate::engine::request_context::RequestContext;
use crate::engine::user::is_root;
use crate::engine::VersionManager;

use super::responses::{ErrorResponse, ForkResponse, SnapshotResponse};
use super::state::AppState;

// ---------------------------------------------------------------------------
// Snapshot routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSnapshotRequest {
  pub name: String,
  #[serde(default)]
  pub metadata: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct RestoreSnapshotRequest {
  /// Snapshot ID (hex root hash) — authoritative identifier.
  pub id: Option<String>,
  /// Snapshot name — fallback for backward compatibility.
  pub name: Option<String>,
}

/// POST /version/snapshot -- create a named snapshot of the current HEAD.
pub async fn snapshot_create(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CreateSnapshotRequest>,
) -> Response {
  if std::env::var("AEORDB_DISABLE_SNAPSHOT_RATE_LIMIT").is_err() {
    use std::sync::atomic::Ordering;
    let now = chrono::Utc::now().timestamp_millis();
    let last = state.engine.last_manual_snapshot.load(Ordering::Relaxed);
    let elapsed = now - last;
    if elapsed < 60_000 && last > 0 {
      let remaining = (60_000 - elapsed) / 1000;
      return ErrorResponse::new(format!(
        "Snapshot rate limited. Try again in {} seconds.", remaining
      ))
        .with_status(StatusCode::TOO_MANY_REQUESTS)
        .into_response();
    }
    let _ = state.engine.last_manual_snapshot
      .compare_exchange(last, now, Ordering::SeqCst, Ordering::Relaxed);
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.create_snapshot(&ctx, &payload.name, payload.metadata) {
    Ok(snapshot_info) => {
      let is_duplicate = snapshot_info.name != payload.name;
      let status = if is_duplicate { StatusCode::OK } else { StatusCode::CREATED };
      let mut response_body = serde_json::to_value(SnapshotResponse::from(&snapshot_info))
        .unwrap_or_default();
      if is_duplicate {
        response_body["duplicate"] = serde_json::json!(true);
        state.engine.last_manual_snapshot.store(0, std::sync::atomic::Ordering::Relaxed);
      }
      (status, Json(response_body)).into_response()
    }
    Err(EngineError::AlreadyExists(message)) => {
      ErrorResponse::new(message)
        .with_status(StatusCode::CONFLICT)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to create snapshot: {}", error);
      ErrorResponse::new(format!("Failed to create snapshot: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// GET /version/snapshots -- list all snapshots.
pub async fn snapshot_list(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.list_snapshots() {
    Ok(snapshots) => {
      let listing: Vec<SnapshotResponse> = snapshots
        .iter()
        .map(SnapshotResponse::from)
        .collect();
      (StatusCode::OK, Json(serde_json::json!({"items": listing}))).into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to list snapshots: {}", error);
      ErrorResponse::new(format!("Failed to list snapshots: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// POST /version/restore -- restore a named snapshot (requires root).
pub async fn snapshot_restore(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<RestoreSnapshotRequest>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can restore snapshots"
    }))).into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  if payload.id.is_none() && payload.name.is_none() {
    return ErrorResponse::new("Either 'id' or 'name' must be provided to identify the snapshot to restore.".to_string())
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }

  let identifier = payload.id.as_deref()
    .or(payload.name.as_deref())
    .unwrap_or("");

  let snapshot = match version_manager.resolve_snapshot(identifier) {
    Ok(s) => s,
    Err(_) => {
      return ErrorResponse::new(format!("Snapshot not found: '{}'. Use GET /versions/snapshots to list available snapshots", identifier))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
  };

  match version_manager.restore_snapshot(&ctx, &snapshot.name) {
    Ok(()) => {
      state.engine.permissions_cache.evict_all();
      state.engine.index_config_cache.evict_all();
      state.group_cache.evict_all();
      state.api_key_cache.evict_all();
      state.engine.clear_dir_content_cache();
      (
        StatusCode::OK,
        Json(serde_json::json!({ "restored": true, "id": snapshot.id(), "name": snapshot.name })),
      )
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to restore snapshot '{}': {}", snapshot.name, error);
      ErrorResponse::new(format!("Failed to restore snapshot: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// DELETE /version/snapshot/:id_or_name -- delete a snapshot (requires root).
pub async fn snapshot_delete(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(id_or_name): Path<String>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can delete snapshots"
    }))).into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  let snapshot = match version_manager.resolve_snapshot(&id_or_name) {
    Ok(s) => s,
    Err(_) => {
      return ErrorResponse::new(format!("Snapshot not found: '{}'", id_or_name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
  };

  let snap_id = snapshot.id();
  let snap_name = snapshot.name.clone();
  match version_manager.delete_snapshot(&ctx, &snap_name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "deleted": true, "id": snap_id, "name": snap_name })),
      )
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to delete snapshot '{}': {}", snap_name, error);
      ErrorResponse::new(format!("Failed to delete snapshot: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// PATCH /versions/snapshots/{name} -- rename a snapshot (requires root).
pub async fn snapshot_rename(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(id_or_name): Path<String>,
  Json(payload): Json<serde_json::Value>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can rename snapshots"
    }))).into_response();
  }

  let new_name = match payload.get("name").and_then(|v| v.as_str()) {
    Some(name) if !name.is_empty() => name,
    _ => {
      return ErrorResponse::new("Missing or empty 'name' field")
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  let snapshot = match version_manager.resolve_snapshot(&id_or_name) {
    Ok(s) => s,
    Err(_) => {
      return ErrorResponse::new(format!("Snapshot not found: '{}'", id_or_name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response();
    }
  };

  match version_manager.rename_snapshot(&ctx, &snapshot.name, new_name) {
    Ok(_) => {
      (StatusCode::OK, Json(serde_json::json!({
        "renamed": true,
        "from": snapshot.name,
        "to": new_name,
      }))).into_response()
    }
    Err(EngineError::AlreadyExists(msg)) => {
      ErrorResponse::new(msg)
        .with_status(StatusCode::CONFLICT)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Failed to rename snapshot '{}': {}", snapshot.name, error);
      ErrorResponse::new(format!("Failed to rename snapshot: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

// ---------------------------------------------------------------------------
// Fork routes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateForkRequest {
  pub name: String,
  pub base: Option<String>,
}

/// POST /version/fork -- create a named fork.
pub async fn fork_create(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Json(payload): Json<CreateForkRequest>,
) -> Response {
  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.create_fork(&ctx, &payload.name, payload.base.as_deref()) {
    Ok(fork_info) => {
      let response_body = ForkResponse::from(&fork_info);
      (StatusCode::CREATED, Json(response_body)).into_response()
    }
    Err(EngineError::AlreadyExists(message)) => {
      ErrorResponse::new(message)
        .with_status(StatusCode::CONFLICT)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to create fork: {}", error);
      ErrorResponse::new(format!("Failed to create fork: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// GET /version/forks -- list all active forks.
pub async fn fork_list(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.list_forks() {
    Ok(forks) => {
      let listing: Vec<ForkResponse> = forks
        .iter()
        .map(ForkResponse::from)
        .collect();
      (StatusCode::OK, Json(serde_json::json!({"items": listing}))).into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to list forks: {}", error);
      ErrorResponse::new(format!("Failed to list forks: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// POST /version/fork/:name/promote -- promote a fork to HEAD (requires root).
pub async fn fork_promote(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(name): Path<String>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can promote forks"
    }))).into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.promote_fork(&ctx, &name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "promoted": true, "name": name })),
      )
        .into_response()
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Fork not found: '{}'. Use GET /versions/forks to list active forks", name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to promote fork '{}': {}", name, error);
      ErrorResponse::new(format!("Failed to promote fork: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// DELETE /version/fork/:name -- abandon a fork (requires root).
pub async fn fork_abandon(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Path(name): Path<String>,
) -> Response {
  let user_id = match uuid::Uuid::parse_str(&claims.sub) {
    Ok(id) => id,
    Err(_) => {
      return (StatusCode::FORBIDDEN, Json(serde_json::json!({
        "error": "Invalid user ID"
      }))).into_response();
    }
  };
  if !is_root(&user_id) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can abandon forks"
    }))).into_response();
  }

  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.abandon_fork(&ctx, &name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "abandoned": true, "name": name })),
      )
        .into_response()
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Fork not found: '{}'. Use GET /versions/forks to list active forks", name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to abandon fork '{}': {}", name, error);
      ErrorResponse::new(format!("Failed to abandon fork: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}
