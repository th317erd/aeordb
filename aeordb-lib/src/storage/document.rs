use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub struct Document {
  pub document_id: Uuid,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  pub is_deleted: bool,
  pub data: Vec<u8>,
  pub content_type: Option<String>,
}

/// Sparse updates to document metadata fields.
/// Only fields set to Some(...) will be applied.
#[derive(Debug, Clone, Default)]
pub struct MetadataUpdates {
  pub is_deleted: Option<bool>,
  pub created_at: Option<DateTime<Utc>>,
  pub updated_at: Option<DateTime<Utc>>,
  // None = don't update, Some(None) = clear content_type, Some(Some(value)) = set content_type
  pub content_type: Option<Option<String>>,
}
