use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::storage::Document;

#[derive(Debug, Serialize)]
pub struct DocumentMetadataResponse {
  pub document_id: Uuid,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  pub is_deleted: bool,
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

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
  pub error: String,
}

impl From<&Document> for DocumentMetadataResponse {
  fn from(document: &Document) -> Self {
    Self {
      document_id: document.document_id,
      created_at: document.created_at,
      updated_at: document.updated_at,
      is_deleted: document.is_deleted,
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
