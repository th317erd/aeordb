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
