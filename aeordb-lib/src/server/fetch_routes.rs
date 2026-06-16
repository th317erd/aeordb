use axum::{
  Extension,
  extract::State,
  http::StatusCode,
  response::{IntoResponse, Response},
  Json,
};
use serde::{Deserialize, Serialize};

use super::responses::ErrorResponse;
use super::route_permissions::RoutePermissionChecker;
use super::state::AppState;
use crate::auth::permission_middleware::ActiveKeyRules;
use crate::auth::TokenClaims;
use crate::engine::api_key_rules::{check_operation_permitted, match_rules};
use crate::engine::directory_ops::{is_system_path, DirectoryOps};
use crate::engine::errors::EngineError;
use crate::engine::path_utils::{file_name, normalize_path};
use crate::engine::permission_resolver::CrudlifyOp;
use crate::engine::range_extract::{extract_range_from_record, RangeExtractionRequest};

const MAX_BATCH_FETCH_FILES: usize = 10_000;
const MAX_BATCH_FETCH_RESPONSE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Deserialize)]
pub struct BatchFetchRequest {
  pub paths: Option<Vec<String>>,
  pub items: Option<Vec<BatchFetchItem>>,
  pub max_bytes: Option<u64>,
  pub continue_on_error: Option<bool>,
}

#[derive(Deserialize)]
pub struct BatchFetchItem {
  pub id: Option<String>,
  pub path: String,
  pub if_content_hash: Option<String>,
  pub if_updated_at: Option<i64>,
  pub range: RangeExtractionRequest,
  pub max_bytes: Option<usize>,
}

#[derive(Serialize)]
struct BatchFetchRangeError {
  #[serde(skip_serializing_if = "Option::is_none")]
  id: Option<String>,
  path: String,
  status: &'static str,
  message: String,
}

/// POST /files/fetch — fetch multiple file bodies as a JSON object keyed by path.
pub async fn batch_fetch(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  active_key_rules: Option<Extension<ActiveKeyRules>>,
  Json(body): Json<BatchFetchRequest>,
) -> Response {
  match (&body.paths, &body.items) {
    (Some(_), Some(_)) => {
      return ErrorResponse::new("Provide either 'paths' for whole-file fetch or 'items' for range fetch, not both")
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
    (Some(paths), None) => return batch_fetch_paths(&state, &claims, active_key_rules.as_ref(), paths, body.max_bytes),
    (None, Some(items)) => {
      return batch_fetch_range_items(
        &state,
        &claims,
        active_key_rules.as_ref(),
        items,
        body.max_bytes,
        body.continue_on_error.unwrap_or(false),
      );
    }
    (None, None) => {
      return ErrorResponse::new("Provide either a non-empty 'paths' array or a non-empty 'items' array")
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    }
  }
}

fn batch_fetch_paths(
  state: &AppState,
  claims: &TokenClaims,
  active_key_rules: Option<&Extension<ActiveKeyRules>>,
  paths: &[String],
  max_bytes: Option<u64>,
) -> Response {
  if paths.is_empty() {
    return ErrorResponse::new("At least one path is required in the 'paths' array").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  if paths.len() > MAX_BATCH_FETCH_FILES {
    return ErrorResponse::new(format!("Too many paths (max {}). Split the request into multiple batches", MAX_BATCH_FETCH_FILES))
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }

  let key_rules = active_key_rules.map(|Extension(rules)| rules.0.as_slice());
  let ops = DirectoryOps::new(&state.engine);
  let mut response = serde_json::Map::new();
  let mut cumulative_size = 0u64;
  let max_response_bytes = max_bytes.unwrap_or(MAX_BATCH_FETCH_RESPONSE_BYTES).min(MAX_BATCH_FETCH_RESPONSE_BYTES);

  for raw_path in paths {
    let normalized = normalize_path(raw_path);

    if is_system_path(&normalized) {
      return not_found(raw_path);
    }

    if !can_fetch_path(&state, &claims, key_rules, &normalized) {
      return not_found(raw_path);
    }

    let file_record = match ops.get_metadata(&normalized) {
      Ok(Some(record)) => record,
      Ok(None) => return not_found(raw_path),
      Err(error) => {
        tracing::error!("Batch fetch: failed to get metadata for '{}': {}", normalized, error);
        return ErrorResponse::new(format!(
          "Failed to read metadata for '{}'. The file may be corrupted — contact your administrator",
          raw_path
        ))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
      }
    };

    cumulative_size = cumulative_size.saturating_add(file_record.total_size);
    if cumulative_size > max_response_bytes {
      return ErrorResponse::new(format!(
        "Batch fetch response would exceed {} bytes. Split the request into smaller batches",
        max_response_bytes
      ))
      .with_status(StatusCode::PAYLOAD_TOO_LARGE)
      .into_response();
    }

    let data = match ops.read_file_buffered(&normalized) {
      Ok(data) => data,
      Err(EngineError::NotFound(_)) => return not_found(raw_path),
      Err(error) => {
        tracing::error!("Batch fetch: failed to read file '{}': {}", normalized, error);
        return ErrorResponse::new(format!(
          "Failed to read file '{}'. The file data may be corrupted — contact your administrator",
          raw_path
        ))
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
        .into_response();
      }
    };

    let content = String::from_utf8_lossy(&data).into_owned();
    let name = file_name(&file_record.path).unwrap_or("").to_string();
    response.insert(
      file_record.path.clone(),
      serde_json::json!({
        "path": file_record.path,
        "name": name,
        "size": file_record.total_size,
        "created_at": file_record.created_at,
        "updated_at": file_record.updated_at,
        "content_type": file_record.content_type,
        "content": content,
      }),
    );
  }

  (StatusCode::OK, Json(serde_json::Value::Object(response))).into_response()
}

fn batch_fetch_range_items(
  state: &AppState,
  claims: &TokenClaims,
  active_key_rules: Option<&Extension<ActiveKeyRules>>,
  items: &[BatchFetchItem],
  max_bytes: Option<u64>,
  continue_on_error: bool,
) -> Response {
  if items.is_empty() {
    return ErrorResponse::new("At least one item is required in the 'items' array").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  if items.len() > MAX_BATCH_FETCH_FILES {
    return ErrorResponse::new(format!("Too many items (max {}). Split the request into multiple batches", MAX_BATCH_FETCH_FILES))
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }

  let key_rules = active_key_rules.map(|Extension(rules)| rules.0.as_slice());
  let ops = DirectoryOps::new(&state.engine);
  let max_response_bytes = max_bytes.unwrap_or(MAX_BATCH_FETCH_RESPONSE_BYTES).min(MAX_BATCH_FETCH_RESPONSE_BYTES);
  let mut cumulative_size = 0u64;
  let mut response_items = Vec::with_capacity(items.len());
  let mut has_errors = false;

  for item in items {
    match fetch_one_range_item(state, claims, key_rules, &ops, item) {
      Ok(mut value) => {
        let content_len = value.get("content").and_then(|v| v.as_str()).map(|s| s.len() as u64).unwrap_or(0);
        if cumulative_size.saturating_add(content_len) > max_response_bytes {
          let error = range_error_value(
            item,
            "too_large",
            format!("Batch fetch response would exceed {} bytes. Split the request into smaller batches", max_response_bytes),
          );
          if !continue_on_error {
            return ErrorResponse::new(error["message"].as_str().unwrap_or("Range fetch response too large"))
              .with_status(StatusCode::PAYLOAD_TOO_LARGE)
              .into_response();
          }
          has_errors = true;
          response_items.push(error);
          continue;
        }
        cumulative_size += content_len;
        value["status"] = serde_json::json!("ok");
        response_items.push(value);
      }
      Err((status, error)) => {
        if !continue_on_error {
          return ErrorResponse::new(error.message).with_status(status).into_response();
        }
        has_errors = true;
        response_items.push(serde_json::to_value(error).unwrap_or_else(|_| serde_json::json!({"status": "error"})));
      }
    }
  }

  (
    StatusCode::OK,
    Json(serde_json::json!({
      "items": response_items,
      "has_errors": has_errors,
    })),
  )
    .into_response()
}

fn fetch_one_range_item(
  state: &AppState,
  claims: &TokenClaims,
  key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>,
  ops: &DirectoryOps<'_>,
  item: &BatchFetchItem,
) -> Result<serde_json::Value, (StatusCode, BatchFetchRangeError)> {
  let normalized = normalize_path(&item.path);

  if is_system_path(&normalized) || !can_fetch_path(state, claims, key_rules, &normalized) {
    return Err((StatusCode::NOT_FOUND, range_error(item, "not_found", format!("Not found: {}", item.path))));
  }

  let file_record = match ops.get_metadata(&normalized) {
    Ok(Some(record)) => record,
    Ok(None) => return Err((StatusCode::NOT_FOUND, range_error(item, "not_found", format!("Not found: {}", item.path)))),
    Err(error) => {
      tracing::error!("Batch range fetch: failed to get metadata for '{}': {}", normalized, error);
      return Err((
        StatusCode::INTERNAL_SERVER_ERROR,
        range_error(
          item,
          "error",
          format!("Failed to read metadata for '{}'. The file may be corrupted — contact your administrator", item.path),
        ),
      ));
    }
  };

  let content_hash = file_record.content_hash_hex();
  if let Some(expected) = item.if_content_hash.as_deref() {
    if expected != content_hash {
      return Err((StatusCode::CONFLICT, range_error(item, "stale", "File content hash changed".to_string())));
    }
  }

  if let Some(expected_updated_at) = item.if_updated_at {
    if expected_updated_at != file_record.updated_at {
      return Err((StatusCode::CONFLICT, range_error(item, "stale", "File updated_at changed".to_string())));
    }
  }

  let mut range_request = item.range.clone();
  if item.max_bytes.is_some() {
    range_request.max_bytes = item.max_bytes;
  }

  let extracted = match extract_range_from_record(&state.engine, &file_record, &range_request) {
    Ok(extracted) => extracted,
    Err(EngineError::NotFound(_)) => {
      return Err((StatusCode::NOT_FOUND, range_error(item, "not_found", format!("Not found: {}", item.path))));
    }
    Err(EngineError::InvalidInput(message)) | Err(EngineError::JsonParseError(message)) => {
      return Err((StatusCode::BAD_REQUEST, range_error(item, "invalid", message)));
    }
    Err(error) => {
      tracing::error!("Batch range fetch: failed to read range for '{}': {}", normalized, error);
      return Err((
        StatusCode::INTERNAL_SERVER_ERROR,
        range_error(
          item,
          "error",
          format!("Failed to read file '{}'. The file data may be corrupted — contact your administrator", item.path),
        ),
      ));
    }
  };

  Ok(serde_json::json!({
    "id": item.id,
    "path": file_record.path,
    "name": file_name(&file_record.path).unwrap_or("").to_string(),
    "size": file_record.total_size,
    "created_at": file_record.created_at,
    "updated_at": file_record.updated_at,
    "content_hash": content_hash,
    "content_type": extracted.content_type,
    "range": {
      "mode": extracted.mode.as_str(),
      "start": extracted.start,
      "end": extracted.end,
      "pointer": extracted.pointer,
    },
    "source_size": extracted.source_size,
    "content": extracted.content,
    "truncated": extracted.truncated,
  }))
}

fn range_error(item: &BatchFetchItem, status: &'static str, message: String) -> BatchFetchRangeError {
  BatchFetchRangeError { id: item.id.clone(), path: normalize_path(&item.path), status, message }
}

fn range_error_value(item: &BatchFetchItem, status: &'static str, message: String) -> serde_json::Value {
  serde_json::to_value(range_error(item, status, message)).unwrap_or_else(|_| serde_json::json!({"status": status}))
}

fn can_fetch_path(state: &AppState, claims: &TokenClaims, key_rules: Option<&[crate::engine::api_key_rules::KeyRule]>, path: &str) -> bool {
  if let Some(rules) = key_rules {
    if !rules.is_empty() {
      return match match_rules(rules, path) {
        Some(rule) => check_operation_permitted(&rule.permitted, 'r'),
        None => false,
      };
    }
  }

  if claims.sub.starts_with("share:") {
    return true;
  }

  let Ok(user_id) = uuid::Uuid::parse_str(&claims.sub) else {
    return false;
  };
  let permissions = RoutePermissionChecker::for_user(state, user_id);
  if permissions.is_root() {
    return true;
  }

  permissions.has_direct_permission(path, CrudlifyOp::Read)
}

fn not_found(path: &str) -> Response {
  ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response()
}
