use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header::CONTENT_TYPE, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;

use crate::auth::TokenClaims;
use crate::engine::config_resolver::ConfigurationFamily;
use crate::engine::configuration_observability::{configuration_envelope, ConfigurationVisibility};
use crate::server::responses::{engine_error_response, require_root, ErrorResponse, RouteResponseError};
use crate::server::state::AppState;

pub async fn get_runtime(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>) -> Response {
  get_configuration(state, claims, ConfigurationFamily::Runtime)
}

pub async fn put_runtime(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  if let Err(response) = require_root(&claims) {
    return response.into_response();
  }
  if let Err(response) = require_json_content_type(&headers, false) {
    return response.into_response();
  }
  put_configuration(state, ConfigurationFamily::Runtime, body)
}

pub async fn patch_runtime(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  if let Err(response) = require_root(&claims) {
    return response.into_response();
  }
  if let Err(response) = require_json_content_type(&headers, true) {
    return response.into_response();
  }
  patch_configuration(state, ConfigurationFamily::Runtime, body)
}

pub async fn get_lifecycle(State(state): State<AppState>, Extension(claims): Extension<TokenClaims>) -> Response {
  get_configuration(state, claims, ConfigurationFamily::Lifecycle)
}

pub async fn put_lifecycle(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  if let Err(response) = require_root(&claims) {
    return response.into_response();
  }
  if let Err(response) = require_json_content_type(&headers, false) {
    return response.into_response();
  }
  put_configuration(state, ConfigurationFamily::Lifecycle, body)
}

pub async fn patch_lifecycle(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  if let Err(response) = require_root(&claims) {
    return response.into_response();
  }
  if let Err(response) = require_json_content_type(&headers, true) {
    return response.into_response();
  }
  patch_configuration(state, ConfigurationFamily::Lifecycle, body)
}

fn require_json_content_type(headers: &HeaderMap, merge_patch: bool) -> Result<(), RouteResponseError> {
  let media_type = headers.get(CONTENT_TYPE).and_then(|value| value.to_str().ok()).and_then(|value| value.split(';').next()).map(str::trim);
  let accepted = media_type.is_some_and(|media_type| {
    media_type.eq_ignore_ascii_case("application/json") || (merge_patch && media_type.eq_ignore_ascii_case("application/merge-patch+json"))
  });
  if accepted {
    return Ok(());
  }
  let expected = if merge_patch { "application/json or application/merge-patch+json" } else { "application/json" };
  Err(
    ErrorResponse::new(format!("configuration request Content-Type must be {expected}"))
      .with_status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
      .into_response()
      .into(),
  )
}

fn get_configuration(state: AppState, claims: TokenClaims, family: ConfigurationFamily) -> Response {
  if let Err(response) = require_root(&claims) {
    return response.into_response();
  }
  Json(configuration_envelope(&state.engine.configuration_snapshot(), family, ConfigurationVisibility::Root)).into_response()
}

fn put_configuration(state: AppState, family: ConfigurationFamily, body: Bytes) -> Response {
  match state.engine.replace_configuration_document(family, &body) {
    Ok(snapshot) => Json(configuration_envelope(&snapshot, family, ConfigurationVisibility::Root)).into_response(),
    Err(error) => engine_error_response(&format!("failed to replace {} configuration", family.name()), &error),
  }
}

fn patch_configuration(state: AppState, family: ConfigurationFamily, body: Bytes) -> Response {
  match state.engine.patch_configuration_document(family, &body) {
    Ok(snapshot) => Json(configuration_envelope(&snapshot, family, ConfigurationVisibility::Root)).into_response(),
    Err(error) => engine_error_response(&format!("failed to patch {} configuration", family.name()), &error),
  }
}
