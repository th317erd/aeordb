use axum::{
  http::StatusCode,
  response::{IntoResponse, Response},
  Json,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::filesystem::DirectoryEntry;
use crate::storage::Document;

#[derive(Debug, Serialize)]
pub struct DocumentMetadataResponse {
  pub document_id: Uuid,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  pub content_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateDocumentResponse {
  pub document_id: Uuid,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct DeleteDocumentResponse {
  pub deleted: bool,
  pub document_id: Uuid,
}

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

impl From<&Document> for DocumentMetadataResponse {
  fn from(document: &Document) -> Self {
    Self {
      document_id: document.document_id,
      created_at: document.created_at,
      updated_at: document.updated_at,
      content_type: document.content_type.clone(),
    }
  }
}

impl From<&Document> for CreateDocumentResponse {
  fn from(document: &Document) -> Self {
    Self {
      document_id: document.document_id,
      created_at: document.created_at,
      updated_at: document.updated_at,
    }
  }
}

#[derive(Debug, Serialize)]
pub struct FileEntryResponse {
  pub name: String,
  pub entry_type: String,
  pub document_id: Uuid,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  pub content_type: Option<String>,
  pub total_size: u64,
}

impl From<&DirectoryEntry> for FileEntryResponse {
  fn from(entry: &DirectoryEntry) -> Self {
    let entry_type = match entry.entry_type {
      crate::filesystem::EntryType::File => "file",
      crate::filesystem::EntryType::Directory => "directory",
      crate::filesystem::EntryType::HardLink => "hard_link",
    };
    Self {
      name: entry.name.clone(),
      entry_type: entry_type.to_string(),
      document_id: entry.document_id,
      created_at: entry.created_at,
      updated_at: entry.updated_at,
      content_type: entry.content_type.clone(),
      total_size: entry.total_size,
    }
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
