use axum::{
  Extension,
  extract::State,
  http::StatusCode,
  response::{IntoResponse, Response},
  Json,
};
use serde::Deserialize;
use uuid::Uuid;

use super::responses::ErrorResponse;
use super::state::AppState;
use crate::auth::permission_middleware::ActiveKeyRules;
use crate::auth::TokenClaims;
use crate::engine::api_key_rules::{check_operation_permitted, match_rules};
use crate::engine::directory_ops::{is_system_path, DirectoryOps};
use crate::engine::errors::EngineError;
use crate::engine::path_utils::{file_name, normalize_path};
use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};
use crate::engine::user::is_root;

const MAX_BATCH_FETCH_FILES: usize = 10_000;
const MAX_BATCH_FETCH_RESPONSE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Deserialize)]
pub struct BatchFetchRequest {
  pub paths: Vec<String>,
  pub max_bytes: Option<u64>,
}

/// POST /files/fetch — fetch multiple file bodies as a JSON object keyed by path.
pub async fn batch_fetch(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  active_key_rules: Option<Extension<ActiveKeyRules>>,
  Json(body): Json<BatchFetchRequest>,
) -> Response {
  if body.paths.is_empty() {
    return ErrorResponse::new("At least one path is required in the 'paths' array").with_status(StatusCode::BAD_REQUEST).into_response();
  }

  if body.paths.len() > MAX_BATCH_FETCH_FILES {
    return ErrorResponse::new(format!("Too many paths (max {}). Split the request into multiple batches", MAX_BATCH_FETCH_FILES))
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
  }

  let key_rules = active_key_rules.as_ref().map(|Extension(rules)| rules.0.as_slice());
  let ops = DirectoryOps::new(&state.engine);
  let mut response = serde_json::Map::new();
  let mut cumulative_size = 0u64;
  let max_response_bytes = body.max_bytes.unwrap_or(MAX_BATCH_FETCH_RESPONSE_BYTES).min(MAX_BATCH_FETCH_RESPONSE_BYTES);

  for raw_path in &body.paths {
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

  let Ok(user_id) = Uuid::parse_str(&claims.sub) else {
    return false;
  };
  if is_root(&user_id) {
    return true;
  }

  let resolver = PermissionResolver::new(&state.engine, &state.group_cache);
  resolver.check_direct_permission(&user_id, path, CrudlifyOp::Read).unwrap_or(false)
}

fn not_found(path: &str) -> Response {
  ErrorResponse::new(format!("Not found: {}", path)).with_status(StatusCode::NOT_FOUND).into_response()
}
