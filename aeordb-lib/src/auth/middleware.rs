use axum::{
  extract::{Request, State},
  http::StatusCode,
  middleware::Next,
  response::{IntoResponse, Response},
  Json,
};

use crate::server::state::AppState;

/// Paths that are exempt from authentication.
const EXEMPT_PATHS: &[&str] = &["/admin/health", "/auth/token"];

/// Axum middleware that validates JWT Bearer tokens.
///
/// Extracts the `Authorization: Bearer <token>` header, verifies the JWT,
/// and injects `TokenClaims` into request extensions. Returns 401 for
/// missing, invalid, or expired tokens. Health and token endpoints are exempt.
pub async fn auth_middleware(
  State(state): State<AppState>,
  mut request: Request,
  next: Next,
) -> Response {
  let path = request.uri().path().to_string();

  // Skip auth for exempt paths
  if EXEMPT_PATHS.iter().any(|exempt| path == *exempt) {
    return next.run(request).await;
  }

  let authorization_header = request
    .headers()
    .get("authorization")
    .and_then(|value| value.to_str().ok())
    .map(|value| value.to_string());

  let token = match authorization_header {
    Some(ref header) if header.starts_with("Bearer ") => &header[7..],
    _ => {
      return (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "Missing or invalid Authorization header" })),
      )
        .into_response();
    }
  };

  let claims = match state.jwt_manager.verify_token(token) {
    Ok(claims) => claims,
    Err(_) => {
      return (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "Invalid or expired token" })),
      )
        .into_response();
    }
  };

  request.extensions_mut().insert(claims);
  next.run(request).await
}
