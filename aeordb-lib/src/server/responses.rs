use axum::{
  http::StatusCode,
  response::{IntoResponse, Response},
  Json,
};
use serde::Serialize;

use crate::auth::TokenClaims;

#[derive(Debug, Serialize, Clone)]
pub struct ErrorResponse {
  pub error: String,
  /// Machine-readable error code from [`error_codes`].
  #[serde(skip_serializing_if = "Option::is_none")]
  pub code: Option<String>,
}

impl ErrorResponse {
  pub fn new(error: impl Into<String>) -> Self {
    Self {
      error: error.into(),
      code: None,
    }
  }

  /// Attach a machine-readable error code (from [`error_codes`]).
  pub fn with_code(mut self, code: &str) -> Self {
    self.code = Some(code.to_owned());
    self
  }

  pub fn with_status(self, status: StatusCode) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(self))
  }
}

// ---------------------------------------------------------------------------
// Machine-readable error codes
// ---------------------------------------------------------------------------

pub mod error_codes {
  pub const NOT_FOUND: &str = "NOT_FOUND";
  pub const ALREADY_EXISTS: &str = "ALREADY_EXISTS";
  pub const CONFLICT: &str = "CONFLICT";
  pub const INVALID_INPUT: &str = "INVALID_INPUT";
  pub const INVALID_PATH: &str = "INVALID_PATH";
  pub const AUTH_REQUIRED: &str = "AUTH_REQUIRED";
  pub const FORBIDDEN: &str = "FORBIDDEN";
  pub const RATE_LIMITED: &str = "RATE_LIMITED";
  pub const PAYLOAD_TOO_LARGE: &str = "PAYLOAD_TOO_LARGE";
  pub const METHOD_NOT_ALLOWED: &str = "METHOD_NOT_ALLOWED";
  pub const SERVICE_UNAVAILABLE: &str = "SERVICE_UNAVAILABLE";
  pub const INTERNAL_ERROR: &str = "INTERNAL_ERROR";
}

impl IntoResponse for ErrorResponse {
  fn into_response(self) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
  }
}

/// Check that the caller is root. Returns the parsed UUID on success,
/// or a 403 Forbidden Response on failure.
pub fn require_root(claims: &TokenClaims) -> Result<uuid::Uuid, Response> {
  let user_id = uuid::Uuid::parse_str(&claims.sub).map_err(|_| {
    ErrorResponse::new("Invalid user identity: token 'sub' claim is not a valid UUID")
      .with_status(StatusCode::FORBIDDEN)
      .into_response()
  })?;
  if !crate::engine::user::is_root(&user_id) {
    return Err(
      ErrorResponse::new("root access required. This endpoint is restricted to the root user")
        .with_status(StatusCode::FORBIDDEN)
        .into_response(),
    );
  }
  Ok(user_id)
}

// ---------------------------------------------------------------------------
// Engine response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct EngineFileResponse {
  pub path: String,
  pub content_type: Option<String>,
  pub size: u64,
  pub created_at: i64,
  pub updated_at: i64,
  /// Content-addressed hash (hex-encoded) for fetch-by-hash lookups.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub hash: Option<String>,
}

impl From<&crate::engine::FileRecord> for EngineFileResponse {
  fn from(record: &crate::engine::FileRecord) -> Self {
    Self {
      path: record.path.clone(),
      content_type: record.content_type.clone(),
      size: record.total_size,
      created_at: record.created_at,
      updated_at: record.updated_at,
      hash: None,
    }
  }
}

#[derive(Debug, Serialize)]
pub struct SnapshotResponse {
  pub id: String,
  pub name: String,
  pub root_hash: String,
  pub created_at: i64,
  pub metadata: std::collections::HashMap<String, String>,
}

impl From<&crate::engine::SnapshotInfo> for SnapshotResponse {
  fn from(info: &crate::engine::SnapshotInfo) -> Self {
    Self {
      id: info.id(),
      name: info.name.clone(),
      root_hash: hex::encode(&info.root_hash),
      created_at: info.created_at,
      metadata: info.metadata.clone(),
    }
  }
}

#[derive(Debug, Serialize)]
pub struct ForkResponse {
  pub name: String,
  pub root_hash: String,
  pub created_at: i64,
}

impl From<&crate::engine::ForkInfo> for ForkResponse {
  fn from(info: &crate::engine::ForkInfo) -> Self {
    Self {
      name: info.name.clone(),
      root_hash: hex::encode(&info.root_hash),
      created_at: info.created_at,
    }
  }
}

// ---------------------------------------------------------------------------
// User / Group response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct UserResponse {
  pub user_id: String,
  pub username: String,
  pub email: Option<String>,
  pub is_active: bool,
  pub created_at: i64,
  pub updated_at: i64,
  pub tags: Vec<String>,
}

impl From<&crate::engine::User> for UserResponse {
  fn from(user: &crate::engine::User) -> Self {
    Self {
      user_id: user.user_id.to_string(),
      username: user.username.clone(),
      email: user.email.clone(),
      is_active: user.is_active,
      created_at: user.created_at,
      updated_at: user.updated_at,
      tags: user.tags.clone(),
    }
  }
}

#[derive(Debug, Serialize)]
pub struct GroupResponse {
  pub name: String,
  pub default_allow: String,
  pub default_deny: String,
  pub query_field: String,
  pub query_operator: String,
  pub query_value: String,
  pub created_at: i64,
  pub updated_at: i64,
}

impl From<&crate::engine::Group> for GroupResponse {
  fn from(group: &crate::engine::Group) -> Self {
    Self {
      name: group.name.clone(),
      default_allow: group.default_allow.clone(),
      default_deny: group.default_deny.clone(),
      query_field: group.query_field.clone(),
      query_operator: group.query_operator.clone(),
      query_value: group.query_value.clone(),
      created_at: group.created_at,
      updated_at: group.updated_at,
    }
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;

  // ---- error_codes constant values ----------------------------------------

  #[test]
  fn error_code_not_found() {
    assert_eq!(error_codes::NOT_FOUND, "NOT_FOUND");
  }

  #[test]
  fn error_code_forbidden() {
    assert_eq!(error_codes::FORBIDDEN, "FORBIDDEN");
  }

  #[test]
  fn error_code_auth_required() {
    assert_eq!(error_codes::AUTH_REQUIRED, "AUTH_REQUIRED");
  }

  #[test]
  fn error_code_invalid_input() {
    assert_eq!(error_codes::INVALID_INPUT, "INVALID_INPUT");
  }

  #[test]
  fn error_code_already_exists() {
    assert_eq!(error_codes::ALREADY_EXISTS, "ALREADY_EXISTS");
  }

  #[test]
  fn error_code_invalid_path() {
    assert_eq!(error_codes::INVALID_PATH, "INVALID_PATH");
  }

  #[test]
  fn error_code_rate_limited() {
    assert_eq!(error_codes::RATE_LIMITED, "RATE_LIMITED");
  }

  #[test]
  fn error_code_conflict() {
    assert_eq!(error_codes::CONFLICT, "CONFLICT");
  }

  #[test]
  fn error_code_internal_error() {
    assert_eq!(error_codes::INTERNAL_ERROR, "INTERNAL_ERROR");
  }

  #[test]
  fn error_code_payload_too_large() {
    assert_eq!(error_codes::PAYLOAD_TOO_LARGE, "PAYLOAD_TOO_LARGE");
  }

  #[test]
  fn error_code_method_not_allowed() {
    assert_eq!(error_codes::METHOD_NOT_ALLOWED, "METHOD_NOT_ALLOWED");
  }

  #[test]
  fn error_code_service_unavailable() {
    assert_eq!(error_codes::SERVICE_UNAVAILABLE, "SERVICE_UNAVAILABLE");
  }

  // ---- ErrorResponse behaviour -------------------------------------------

  #[test]
  fn error_response_new_has_no_code() {
    let resp = ErrorResponse::new("something broke");
    assert_eq!(resp.error, "something broke");
    assert!(resp.code.is_none());
  }

  #[test]
  fn error_response_with_code_attaches_code() {
    let resp = ErrorResponse::new("forbidden")
      .with_code(error_codes::FORBIDDEN);
    assert_eq!(resp.code.as_deref(), Some("FORBIDDEN"));
  }

  #[test]
  fn error_response_with_status_preserves_code() {
    let (status, Json(body)) = ErrorResponse::new("gone")
      .with_code(error_codes::NOT_FOUND)
      .with_status(StatusCode::NOT_FOUND);
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.code.as_deref(), Some("NOT_FOUND"));
    assert_eq!(body.error, "gone");
  }

  #[test]
  fn error_response_serializes_without_code_when_none() {
    let resp = ErrorResponse::new("oops");
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json.get("code").is_none(), "code field should be absent when None");
    assert_eq!(json["error"], "oops");
  }

  #[test]
  fn error_response_serializes_with_code_when_set() {
    let resp = ErrorResponse::new("too big")
      .with_code(error_codes::PAYLOAD_TOO_LARGE);
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["code"], "PAYLOAD_TOO_LARGE");
    assert_eq!(json["error"], "too big");
  }

  #[test]
  fn error_response_clone_preserves_code() {
    let resp = ErrorResponse::new("err")
      .with_code(error_codes::CONFLICT);
    let cloned = resp.clone();
    assert_eq!(cloned.error, "err");
    assert_eq!(cloned.code.as_deref(), Some("CONFLICT"));
  }

  #[test]
  fn with_code_is_chainable_last_wins() {
    let resp = ErrorResponse::new("x")
      .with_code(error_codes::INVALID_INPUT)
      .with_code(error_codes::INTERNAL_ERROR);
    assert_eq!(resp.code.as_deref(), Some("INTERNAL_ERROR"));
  }
}
