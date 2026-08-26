use axum::{
  Extension,
  extract::{Path, Query as AxumQuery, State},
  http::{Method, StatusCode},
  response::{IntoResponse, Response},
  Json,
};
use serde::Deserialize;

use super::responses::ErrorResponse;
use super::engine_routes::{
  EngineGetQuery, attach_root_headers, reject_historical_share_selector, require_legacy_root_plan, resolve_legacy_root,
  root_api_error_response, selected_path_filter,
};
use super::root_api::{RootRequestAdapterV1, RouteRootRequestPlanV1};
use super::route_permissions::require_generic_data_path;
use super::state::AppState;
use crate::auth::TokenClaims;
use crate::auth::permission_middleware::{ActiveKeyRules, FilteredListing};
use crate::engine::directory_ops::DirectoryOps;
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
      return ErrorResponse::new(
        "Missing required field 'target' in request body. Symlink creation requires {\"target\": \"/path/to/target\"}",
      )
      .with_status(StatusCode::BAD_REQUEST)
      .into_response();
    }
  };

  let normalized_target = normalize_path(target);
  if let Err(response) = require_generic_data_path(&state, &normalized_target) {
    return response;
  }

  let normalized_path = normalize_path(&path);
  if let Err(response) = require_generic_data_path(&state, &normalized_path) {
    return response;
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
      ErrorResponse::new(msg).with_status(StatusCode::BAD_REQUEST).into_response()
    }
    Err(error) => {
      tracing::error!("Failed to create symlink at '{}': {}", path, error);
      ErrorResponse::new(format!("Failed to create symlink: {}", error)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}

/// GET /links/{*path} — read symlink metadata without following it.
pub async fn get_symlink(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Extension(root_plan): Extension<RouteRootRequestPlanV1>,
  active_key_rules: Option<Extension<ActiveKeyRules>>,
  filtered_listing: Option<Extension<FilteredListing>>,
  (method, Path(path)): (Method, Path<String>),
  AxumQuery(query): AxumQuery<EngineGetQuery>,
) -> Response {
  if let Err(error) = require_legacy_root_plan(root_plan, RootRequestAdapterV1::ResolveSingleRoot) {
    return error.into_response();
  }
  let normalized_path = normalize_path(&path);
  if let Err(response) = require_generic_data_path(&state, &normalized_path) {
    return response;
  }
  let selector = match query.selector(&state.engine) {
    Ok(selector) => selector,
    Err(error) => return root_api_error_response(error, false),
  };
  if let Err(error) = reject_historical_share_selector(&claims, &selector) {
    return error.into_response();
  }
  let selected = match resolve_legacy_root(&state.engine, &selector) {
    Ok(selected) => selected,
    Err(error) => return error.into_response(),
  };
  let current_filter = filtered_listing.as_ref().map(|Extension(filter)| filter);
  if let Err(error) = selected_path_filter(
    &state,
    &claims,
    &selected,
    &normalized_path,
    crate::engine::permission_resolver::CrudlifyOp::Read,
    current_filter,
    active_key_rules.is_some(),
  ) {
    return error.into_response();
  }

  match selected.symlink(&normalized_path) {
    Ok(symlink_record) => {
      let response = (
        StatusCode::OK,
        Json(serde_json::json!({
          "root": selected.root(),
          "path": symlink_record.record.path,
          "target": symlink_record.record.target,
          "entry_type": 8,
          "created_at": symlink_record.record.created_at,
          "updated_at": symlink_record.record.updated_at,
        })),
      )
        .into_response();
      if method == Method::HEAD {
        attach_root_headers(response, &selected)
      } else {
        response
      }
    }
    Err(crate::engine::errors::EngineError::NotFound(_)) => {
      ErrorResponse::new(format!("Symlink not found at '{}'. Verify the path or use GET /files/ to browse", path))
        .with_status(StatusCode::NOT_FOUND)
        .into_response()
    }
    Err(error) => {
      tracing::error!("Failed to get symlink at '{}': {}", path, error);
      ErrorResponse::new(format!("Failed to get symlink: {}", error)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}

/// DELETE /links/{*path} — delete a symlink.
pub async fn delete_symlink(
  State(state): State<AppState>,
  Extension(_claims): Extension<TokenClaims>,
  Path(path): Path<String>,
) -> Response {
  let normalized_path = normalize_path(&path);
  if let Err(response) = require_generic_data_path(&state, &normalized_path) {
    return response;
  }

  let ctx = RequestContext::from_claims(&_claims.sub, state.event_bus.clone());
  let ops = DirectoryOps::new(&state.engine);

  match ops.delete_symlink(&ctx, &path) {
    Ok(()) => (
      StatusCode::OK,
      Json(serde_json::json!({
          "deleted": true,
          "path": path,
          "entry_type": "symlink",
      })),
    )
      .into_response(),
    Err(error) => {
      tracing::error!("Failed to delete symlink at '{}': {}", path, error);
      ErrorResponse::new(format!("Failed to delete symlink: {}", error)).with_status(StatusCode::INTERNAL_SERVER_ERROR).into_response()
    }
  }
}
