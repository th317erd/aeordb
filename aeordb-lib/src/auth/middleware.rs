use axum::{
  extract::{Request, State},
  http::StatusCode,
  middleware::Next,
  response::{IntoResponse, Response},
  Json,
};

use crate::auth::jwt::TokenClaims;
use crate::engine::ROOT_USER_ID;
use crate::server::responses::ErrorResponse;
use crate::server::state::AppState;

/// Axum middleware that validates JWT Bearer tokens.
///
/// When the auth provider is disabled (NoAuth mode), this middleware
/// skips JWT validation entirely and injects root claims (nil UUID)
/// so every request is treated as root.
pub async fn auth_middleware(
  State(state): State<AppState>,
  mut request: Request,
  next: Next,
) -> Response {
  // If auth is disabled, inject root claims and skip validation.
  if !state.auth_provider.is_enabled() {
    let now = chrono::Utc::now().timestamp();
    let root_claims = TokenClaims {
      sub: ROOT_USER_ID.to_string(),
      iss: "aeordb".to_string(),
      iat: now,
      exp: now + crate::auth::jwt::DEFAULT_EXPIRY_SECONDS,
      scope: None,
      permissions: None,
      key_id: None,
    };
    request.extensions_mut().insert(root_claims);
    return next.run(request).await;
  }

  // Extract token from Authorization header or ?token= query param
  let authorization_header = request
    .headers()
    .get("authorization")
    .and_then(|value| value.to_str().ok())
    .map(|value| value.to_string());

  let token_from_header = authorization_header
    .as_ref()
    .filter(|h| h.starts_with("Bearer "))
    .map(|h| h[7..].to_string());

  let token_from_query = if token_from_header.is_none() {
    request.uri().query()
      .and_then(|q| {
        // Use form_urlencoded for proper percent-decoding of the token value.
        // This handles tokens that contain URL-encoded characters (e.g. %3D for =).
        form_urlencoded::parse(q.as_bytes())
          .find(|(key, _)| key == "token")
          .map(|(_, value)| value.into_owned())
      })
  } else {
    None
  };

  let token = match token_from_header.or(token_from_query) {
    Some(t) => t,
    None => {
      tracing::warn!("Auth failed: missing or invalid Authorization header");
      metrics::counter!(
        crate::metrics::definitions::AUTH_VALIDATIONS_TOTAL,
        "result" => "missing_header"
      ).increment(1);
      return (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
          error: "Missing or invalid Authorization header".to_string(),
          code: None,
        }),
      )
        .into_response();
    }
  };

  let claims = match state.jwt_manager.verify_token(&token) {
    Ok(claims) => claims,
    Err(error) => {
      tracing::warn!(reason = %error, "JWT validation failed");
      metrics::counter!(
        crate::metrics::definitions::AUTH_VALIDATIONS_TOTAL,
        "result" => "invalid"
      ).increment(1);
      return (
        StatusCode::UNAUTHORIZED,
        Json(ErrorResponse {
          error: "Invalid or expired token".to_string(),
          code: None,
        }),
      )
        .into_response();
    }
  };

  tracing::debug!(user_id = %claims.sub, "JWT validation succeeded");
  metrics::counter!(
    crate::metrics::definitions::AUTH_VALIDATIONS_TOTAL,
    "result" => "success"
  ).increment(1);
  request.extensions_mut().insert(claims);
  next.run(request).await
}
