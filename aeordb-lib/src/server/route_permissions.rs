use axum::{
  http::StatusCode,
  response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::auth::TokenClaims;
use crate::engine::permission_resolver::{CrudlifyOp, PermissionResolver};
use crate::engine::user::is_root;
use crate::server::responses::ErrorResponse;
use crate::server::state::AppState;

pub fn reject_share_key(claims: &TokenClaims, message: &'static str) -> Result<(), Response> {
  if claims.sub.starts_with("share:") {
    Err(ErrorResponse::new(message).with_status(StatusCode::FORBIDDEN).into_response())
  } else {
    Ok(())
  }
}

pub fn parse_user_id(claims: &TokenClaims, invalid_message: &'static str) -> Result<Uuid, Response> {
  Uuid::parse_str(&claims.sub).map_err(|_| ErrorResponse::new(invalid_message).with_status(StatusCode::FORBIDDEN).into_response())
}

/// Route-local permission checker that centralizes claim parsing and
/// `PermissionResolver` construction while leaving each handler in control of
/// its denial semantics.
pub struct RoutePermissionChecker<'a> {
  user_id: Uuid,
  resolver: PermissionResolver<'a>,
}

impl<'a> RoutePermissionChecker<'a> {
  pub fn from_claims(state: &'a AppState, claims: &TokenClaims, invalid_message: &'static str) -> Result<Self, Response> {
    let user_id = parse_user_id(claims, invalid_message)?;
    Ok(Self::for_user(state, user_id))
  }

  pub fn for_user(state: &'a AppState, user_id: Uuid) -> Self {
    Self { user_id, resolver: PermissionResolver::new(&state.engine, &state.group_cache) }
  }

  pub fn is_root(&self) -> bool {
    is_root(&self.user_id)
  }

  pub fn has_permission(&self, path: &str, operation: CrudlifyOp) -> bool {
    self.resolver.check_permission(&self.user_id, path, operation).unwrap_or(false)
  }

  pub fn has_direct_permission(&self, path: &str, operation: CrudlifyOp) -> bool {
    self.resolver.check_direct_permission(&self.user_id, path, operation).unwrap_or(false)
  }

  pub fn has_path_permission(&self, path: &str, operation: CrudlifyOp) -> bool {
    self.resolver.check_path_permission(&self.user_id, path, operation).unwrap_or(false)
  }

  pub fn has_any_path_permission(&self, path: &str, operations: &[CrudlifyOp]) -> bool {
    operations.iter().any(|operation| self.has_path_permission(path, *operation))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn claims(sub: &str) -> TokenClaims {
    TokenClaims { sub: sub.to_string(), iss: "aeordb-test".to_string(), iat: 0, exp: 1, scope: None, permissions: None, key_id: None }
  }

  #[test]
  fn rejects_share_keys_with_route_message() {
    let response = reject_share_key(&claims("share:test"), "no share keys").expect_err("share key should be rejected");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
  }

  #[test]
  fn parses_uuid_subjects_and_rejects_invalid_subjects() {
    let valid = Uuid::new_v4();
    assert_eq!(parse_user_id(&claims(&valid.to_string()), "bad user").unwrap(), valid);

    let response = parse_user_id(&claims("not-a-uuid"), "bad user").expect_err("invalid user should be rejected");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
  }
}
