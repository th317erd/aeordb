use axum::{
  extract::{Request, State},
  http::StatusCode,
  middleware::Next,
  response::{IntoResponse, Response},
  Json,
};

use crate::server::responses::ErrorResponse;
use crate::server::state::AppState;

/// Axum middleware that validates JWT Bearer tokens.
///
/// Route-level separation handles public vs protected endpoints.
/// This middleware only runs on protected routes (those behind
/// the `route_layer` in the router). It extracts the
/// `Authorization: Bearer <token>` header, verifies the JWT,
/// and injects `TokenClaims` into request extensions. Returns
/// 401 for missing, invalid, or expired tokens.
pub async fn auth_middleware(
  State(state): State<AppState>,
  mut request: Request,
  next: Next,
) -> Response {
  let authorization_header = request
    .headers()
    .get("authorization")
    .and_then(|value| value.to_str().ok())
    .map(|value| value.to_string());

  let token = match authorization_header {
    Some(ref header) if header.starts_with("Bearer ") => &header[7..],
    _ => {
      metrics::counter!(
        crate::metrics::definitions::AUTH_VALIDATIONS_TOTAL,
        "result" => "missing_header"
      ).increment(1);
      return (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
          error: "Missing or invalid Authorization header".to_string(),
        }),
      )
        .into_response();
    }
  };

  let claims = match state.jwt_manager.verify_token(token) {
    Ok(claims) => claims,
    Err(_) => {
      metrics::counter!(
        crate::metrics::definitions::AUTH_VALIDATIONS_TOTAL,
        "result" => "invalid"
      ).increment(1);
      return (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
          error: "Invalid or expired token".to_string(),
        }),
      )
        .into_response();
    }
  };

  metrics::counter!(
    crate::metrics::definitions::AUTH_VALIDATIONS_TOTAL,
    "result" => "success"
  ).increment(1);
  request.extensions_mut().insert(claims);
  next.run(request).await
}
