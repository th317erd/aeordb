use axum::{
  http::StatusCode,
  response::{IntoResponse, Response},
  Json,
};
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct ErrorResponse {
  pub error: String,
}

impl ErrorResponse {
  pub fn new(error: impl Into<String>) -> Self {
    Self {
      error: error.into(),
    }
  }

  pub fn with_status(self, status: StatusCode) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(self))
  }
}

impl IntoResponse for ErrorResponse {
  fn into_response(self) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(self)).into_response()
  }
}

// ---------------------------------------------------------------------------
// Engine response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct EngineFileResponse {
  pub path: String,
  pub content_type: Option<String>,
  pub total_size: u64,
  pub created_at: i64,
  pub updated_at: i64,
}

impl From<&crate::engine::FileRecord> for EngineFileResponse {
  fn from(record: &crate::engine::FileRecord) -> Self {
    Self {
      path: record.path.clone(),
      content_type: record.content_type.clone(),
      total_size: record.total_size,
      created_at: record.created_at,
      updated_at: record.updated_at,
    }
  }
}

#[derive(Debug, Serialize)]
pub struct SnapshotResponse {
  pub name: String,
  pub root_hash: String,
  pub created_at: i64,
  pub metadata: std::collections::HashMap<String, String>,
}

impl From<&crate::engine::SnapshotInfo> for SnapshotResponse {
  fn from(info: &crate::engine::SnapshotInfo) -> Self {
    Self {
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
