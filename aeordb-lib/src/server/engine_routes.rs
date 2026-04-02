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
use crate::engine::query_engine::{QueryEngine, Query, QueryNode, FieldQuery, QueryOp, QueryStrategy};

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

  let file_record = match directory_ops.store_file_with_indexing(&path, &body, content_type) {
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

  match directory_ops.delete_file_with_indexing(&path) {
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
// Query endpoint
// ---------------------------------------------------------------------------

/// Raw query request — accepts `where` as either an array (legacy) or
/// an object (boolean logic). Deserialized as raw JSON so we can detect
/// the format at runtime.
#[derive(Debug, Deserialize)]
pub struct QueryRequest {
  pub path: String,
  pub r#where: serde_json::Value,
  pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct WhereClause {
  pub field: String,
  pub op: String,
  pub value: serde_json::Value,
  pub value2: Option<serde_json::Value>,
}

/// Convert a JSON value to the byte representation used by converters.
/// Numbers -> u64 big-endian bytes.
/// Strings -> UTF-8 bytes.
/// Booleans -> single byte 0 or 1.
fn json_value_to_bytes(value: &serde_json::Value) -> Result<Vec<u8>, String> {
  match value {
    serde_json::Value::Number(number) => {
      if let Some(unsigned) = number.as_u64() {
        Ok(unsigned.to_be_bytes().to_vec())
      } else if let Some(signed) = number.as_i64() {
        Ok((signed as u64).to_be_bytes().to_vec())
      } else if let Some(float) = number.as_f64() {
        Ok((float as u64).to_be_bytes().to_vec())
      } else {
        Err("Unsupported number format".to_string())
      }
    }
    serde_json::Value::String(text) => Ok(text.as_bytes().to_vec()),
    serde_json::Value::Bool(flag) => Ok(vec![if *flag { 1 } else { 0 }]),
    other => Err(format!("Unsupported value type: {}", other)),
  }
}

/// Parse a single field-level where clause JSON object into a QueryNode::Field.
fn parse_single_field_query(value: &serde_json::Value) -> Result<QueryNode, String> {
  let field = value.get("field")
    .and_then(|v| v.as_str())
    .ok_or_else(|| "Missing 'field' in where clause".to_string())?;
  let op = value.get("op")
    .and_then(|v| v.as_str())
    .ok_or_else(|| format!("Missing 'op' in where clause for field '{}'", field))?;
  let raw_value = value.get("value")
    .ok_or_else(|| format!("Missing 'value' in where clause for field '{}'", field))?;

  let operation = match op {
    "eq" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      QueryOp::Eq(bytes)
    }
    "gt" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      QueryOp::Gt(bytes)
    }
    "lt" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      QueryOp::Lt(bytes)
    }
    "between" => {
      let bytes = json_value_to_bytes(raw_value)
        .map_err(|message| format!("Invalid value for field '{}': {}", field, message))?;
      let raw_value2 = value.get("value2")
        .ok_or_else(|| format!("Missing value2 for 'between' operation on field '{}'", field))?;
      let bytes2 = json_value_to_bytes(raw_value2)
        .map_err(|message| format!("Invalid value2 for field '{}': {}", field, message))?;
      QueryOp::Between(bytes, bytes2)
    }
    "in" => {
      let array = raw_value.as_array()
        .ok_or_else(|| format!("'in' operation requires array value for field '{}'", field))?;
      let mut byte_values = Vec::with_capacity(array.len());
      for item in array {
        let bytes = json_value_to_bytes(item)
          .map_err(|message| format!("Invalid value in 'in' array for field '{}': {}", field, message))?;
        byte_values.push(bytes);
      }
      QueryOp::In(byte_values)
    }
    unknown => {
      return Err(format!("Unknown operation: '{}'", unknown));
    }
  };

  Ok(QueryNode::Field(FieldQuery {
    field_name: field.to_string(),
    operation,
  }))
}

/// Recursively parse a where clause JSON value into a QueryNode tree.
/// Supports:
///   - Array: legacy format, sugar for AND of field clauses
///   - Object with "and": AND of child clauses
///   - Object with "or": OR of child clauses
///   - Object with "not": NOT of a single child clause
///   - Object with "field": leaf field query
fn parse_where_clause(value: &serde_json::Value) -> Result<QueryNode, String> {
  if value.is_array() {
    let array = value.as_array().unwrap();
    let children: Result<Vec<QueryNode>, String> = array.iter()
      .map(parse_where_clause)
      .collect();
    return Ok(QueryNode::And(children?));
  }

  if let Some(and_array) = value.get("and") {
    let array = and_array.as_array()
      .ok_or_else(|| "'and' must be an array".to_string())?;
    let children: Result<Vec<QueryNode>, String> = array.iter()
      .map(parse_where_clause)
      .collect();
    return Ok(QueryNode::And(children?));
  }

  if let Some(or_array) = value.get("or") {
    let array = or_array.as_array()
      .ok_or_else(|| "'or' must be an array".to_string())?;
    let children: Result<Vec<QueryNode>, String> = array.iter()
      .map(parse_where_clause)
      .collect();
    return Ok(QueryNode::Or(children?));
  }

  if let Some(not_value) = value.get("not") {
    let child = parse_where_clause(not_value)?;
    return Ok(QueryNode::Not(Box::new(child)));
  }

  if value.get("field").is_some() {
    return parse_single_field_query(value);
  }

  Err(format!("Invalid where clause structure: {}", value))
}

/// POST /query -- execute an index query and return matching file metadata.
/// Supports both legacy array format and nested boolean object format.
pub async fn query_endpoint(
  State(state): State<AppState>,
  Json(body): Json<QueryRequest>,
) -> Response {
  // Parse the where clause into a QueryNode tree.
  let query_node = match parse_where_clause(&body.r#where) {
    Ok(node) => node,
    Err(message) => {
      return ErrorResponse::new(message)
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  };

  // Check for empty where clause (AND with no children).
  let is_empty = matches!(&query_node, QueryNode::And(children) if children.is_empty());

  let query = Query {
    path: body.path.clone(),
    field_queries: Vec::new(),
    node: if is_empty { None } else { Some(query_node) },
    limit: body.limit,
    strategy: QueryStrategy::Full,
  };

  let query_engine = QueryEngine::new(&state.engine);
  match query_engine.execute(&query) {
    Ok(results) => {
      let response_items: Vec<serde_json::Value> = results
        .iter()
        .map(|result| {
          serde_json::json!({
            "path": result.file_record.path,
            "total_size": result.file_record.total_size,
            "content_type": result.file_record.content_type,
            "created_at": result.file_record.created_at,
            "updated_at": result.file_record.updated_at,
          })
        })
        .collect();
      (StatusCode::OK, Json(response_items)).into_response()
    }
    Err(EngineError::NotFound(message)) => {
      ErrorResponse::new(message)
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(EngineError::RangeQueryNotSupported(converter_name)) => {
      ErrorResponse::new(format!(
        "Range query not supported for converter '{}'",
        converter_name,
      ))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Query execution failed: {}", error);
      ErrorResponse::new(format!("Query failed: {}", error))
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
