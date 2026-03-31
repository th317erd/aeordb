use std::collections::HashMap;

use axum::{
  body::Body,
  extract::{Path, State},
  http::{HeaderMap, StatusCode},
  response::{IntoResponse, Response},
  Json,
};
use futures_util::stream;
use serde::Deserialize;

use super::responses::{EngineFileResponse, ErrorResponse, ForkResponse, SnapshotResponse};
use super::state::AppState;
use crate::engine::{DirectoryOps, VersionManager};
use crate::engine::errors::EngineError;

// ---------------------------------------------------------------------------
// Engine file routes
// ---------------------------------------------------------------------------

/// PUT /engine/*path -- store a file via the custom storage engine.
pub async fn engine_store_file(
  State(state): State<AppState>,
  Path(path): Path<String>,
  headers: HeaderMap,
  body: axum::body::Bytes,
) -> Response {
  let content_type = headers
    .get("content-type")
    .and_then(|value| value.to_str().ok());

  let directory_ops = DirectoryOps::new(&state.engine);

  let file_record = match directory_ops.store_file(&path, &body, content_type) {
    Ok(record) => record,
    Err(error) => {
      tracing::error!("Engine: failed to store file at '{}': {}", path, error);
      let status = engine_error_status(&error);
      return ErrorResponse::new(format!("Failed to store file: {}", error))
        .with_status(status)
        .into_response();
    }
  };

  let response_body = EngineFileResponse::from(&file_record);
  (StatusCode::CREATED, Json(response_body)).into_response()
}

/// GET /engine/*path -- read a file (streaming) or list a directory.
pub async fn engine_get(
  State(state): State<AppState>,
  Path(path): Path<String>,
) -> Response {
  let directory_ops = DirectoryOps::new(&state.engine);

  // Try as file first
  match directory_ops.get_metadata(&path) {
    Ok(Some(file_record)) => {
      // It is a file -- stream the chunks.
      let file_stream = match directory_ops.read_file_streaming(&path) {
        Ok(file_stream) => file_stream,
        Err(error) => {
          tracing::error!("Engine: failed to read file '{}': {}", path, error);
          return ErrorResponse::new(format!("Failed to read file: {}", error))
            .with_status(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
        }
      };

      let chunk_stream = stream::iter(file_stream.map(|chunk_result| {
        chunk_result
          .map(axum::body::Bytes::from)
          .map_err(|error| std::io::Error::other(error.to_string()))
      }));

      let body = Body::from_stream(chunk_stream);

      let mut response_builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("X-Path", &file_record.path)
        .header("X-Total-Size", file_record.total_size.to_string())
        .header("X-Created-At", file_record.created_at.to_string())
        .header("X-Updated-At", file_record.updated_at.to_string());

      if let Some(ref content_type) = file_record.content_type {
        response_builder = response_builder.header("content-type", content_type.as_str());
      }

      return response_builder
        .body(body)
        .unwrap_or_else(|_| {
          (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build response").into_response()
        });
    }
    Ok(None) => {
      // Not a file -- try as directory
    }
    Err(error) => {
      tracing::error!("Engine: failed to get metadata for '{}': {}", path, error);
      return ErrorResponse::new(format!("Failed to read path: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
    }
  }

  // Try as directory
  match directory_ops.list_directory(&path) {
    Ok(entries) => {
      let listing: Vec<serde_json::Value> = entries
        .iter()
        .map(|child| {
          serde_json::json!({
            "name": child.name,
            "entry_type": child.entry_type,
            "total_size": child.total_size,
            "created_at": child.created_at,
            "updated_at": child.updated_at,
            "content_type": child.content_type,
          })
        })
        .collect();
      (StatusCode::OK, Json(listing)).into_response()
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Not found: {}", path))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to list directory '{}': {}", path, error);
      ErrorResponse::new(format!("Failed to list directory: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// DELETE /engine/*path -- delete a file via the custom storage engine.
pub async fn engine_delete_file(
  State(state): State<AppState>,
  Path(path): Path<String>,
) -> Response {
  let directory_ops = DirectoryOps::new(&state.engine);

  match directory_ops.delete_file(&path) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "deleted": true, "path": path })),
      )
        .into_response()
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Not found: {}", path))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to delete file '{}': {}", path, error);
      ErrorResponse::new(format!("Failed to delete file: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// HEAD /engine/*path -- return metadata as headers.
pub async fn engine_head(
  State(state): State<AppState>,
  Path(path): Path<String>,
) -> Response {
  let directory_ops = DirectoryOps::new(&state.engine);

  match directory_ops.get_metadata(&path) {
    Ok(Some(file_record)) => {
      let mut response_builder = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("X-Entry-Type", "file")
        .header("X-Path", &file_record.path)
        .header("X-Total-Size", file_record.total_size.to_string())
        .header("X-Created-At", file_record.created_at.to_string())
        .header("X-Updated-At", file_record.updated_at.to_string());

      if let Some(ref content_type) = file_record.content_type {
        response_builder = response_builder.header("content-type", content_type.as_str());
      }

      response_builder
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
    Ok(None) => {
      // Check if it is a directory
      match directory_ops.list_directory(&path) {
        Ok(_) => {
          axum::http::Response::builder()
            .status(StatusCode::OK)
            .header("X-Entry-Type", "directory")
            .header("X-Path", &path)
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
      }
    }
    Err(error) => {
      tracing::error!("Engine: failed to get metadata for '{}': {}", path, error);
      StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
  }
}

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
  pub name: String,
}

/// POST /version/snapshot -- create a named snapshot of the current HEAD.
pub async fn snapshot_create(
  State(state): State<AppState>,
  Json(payload): Json<CreateSnapshotRequest>,
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.create_snapshot(&payload.name, payload.metadata) {
    Ok(snapshot_info) => {
      let response_body = SnapshotResponse::from(&snapshot_info);
      (StatusCode::CREATED, Json(response_body)).into_response()
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
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.list_snapshots() {
    Ok(snapshots) => {
      let listing: Vec<SnapshotResponse> = snapshots
        .iter()
        .map(SnapshotResponse::from)
        .collect();
      (StatusCode::OK, Json(listing)).into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to list snapshots: {}", error);
      ErrorResponse::new(format!("Failed to list snapshots: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// POST /version/restore -- restore a named snapshot.
pub async fn snapshot_restore(
  State(state): State<AppState>,
  Json(payload): Json<RestoreSnapshotRequest>,
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.restore_snapshot(&payload.name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "restored": true, "name": payload.name })),
      )
        .into_response()
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Snapshot not found: {}", payload.name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to restore snapshot '{}': {}", payload.name, error);
      ErrorResponse::new(format!("Failed to restore snapshot: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// DELETE /version/snapshot/:name -- delete a named snapshot.
pub async fn snapshot_delete(
  State(state): State<AppState>,
  Path(name): Path<String>,
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.delete_snapshot(&name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "deleted": true, "name": name })),
      )
        .into_response()
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Snapshot not found: {}", name))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to delete snapshot '{}': {}", name, error);
      ErrorResponse::new(format!("Failed to delete snapshot: {}", error))
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
  Json(payload): Json<CreateForkRequest>,
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.create_fork(&payload.name, payload.base.as_deref()) {
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
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.list_forks() {
    Ok(forks) => {
      let listing: Vec<ForkResponse> = forks
        .iter()
        .map(ForkResponse::from)
        .collect();
      (StatusCode::OK, Json(listing)).into_response()
    }
    Err(error) => {
      tracing::error!("Engine: failed to list forks: {}", error);
      ErrorResponse::new(format!("Failed to list forks: {}", error))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response()
    }
  }
}

/// POST /version/fork/:name/promote -- promote a fork to HEAD.
pub async fn fork_promote(
  State(state): State<AppState>,
  Path(name): Path<String>,
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.promote_fork(&name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "promoted": true, "name": name })),
      )
        .into_response()
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Fork not found: {}", name))
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

/// DELETE /version/fork/:name -- abandon a fork.
pub async fn fork_abandon(
  State(state): State<AppState>,
  Path(name): Path<String>,
) -> Response {
  let version_manager = VersionManager::new(&state.engine);

  match version_manager.abandon_fork(&name) {
    Ok(()) => {
      (
        StatusCode::OK,
        Json(serde_json::json!({ "abandoned": true, "name": name })),
      )
        .into_response()
    }
    Err(EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Fork not found: {}", name))
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn engine_error_status(error: &EngineError) -> StatusCode {
  match error {
    EngineError::NotFound(_) => StatusCode::NOT_FOUND,
    EngineError::AlreadyExists(_) => StatusCode::CONFLICT,
    _ => StatusCode::INTERNAL_SERVER_ERROR,
  }
}
