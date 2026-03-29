use crate::storage::ChunkHash;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::ChunkStoreError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntryType {
  File,
  Directory,
  HardLink,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ChunkList {
  Inline(Vec<ChunkHash>),
  Overflow(ChunkHash),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
  pub name: String,
  pub entry_type: EntryType,
  pub chunk_list: ChunkList,
  pub document_id: Uuid,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  pub content_type: Option<String>,
  pub total_size: u64,
}

impl IndexEntry {
  pub fn serialize(&self) -> Result<Vec<u8>, ChunkStoreError> {
    serde_json::to_vec(self).map_err(|error| {
      ChunkStoreError::SerializationError(format!("failed to serialize IndexEntry: {error}"))
    })
  }

  pub fn deserialize(bytes: &[u8]) -> Result<IndexEntry, ChunkStoreError> {
    serde_json::from_slice(bytes).map_err(|error| {
      ChunkStoreError::SerializationError(format!("failed to deserialize IndexEntry: {error}"))
    })
  }
}

impl PartialEq for IndexEntry {
  fn eq(&self, other: &Self) -> bool {
    self.name == other.name
      && self.entry_type == other.entry_type
      && self.chunk_list == other.chunk_list
      && self.document_id == other.document_id
      && self.created_at == other.created_at
      && self.updated_at == other.updated_at
      && self.content_type == other.content_type
      && self.total_size == other.total_size
  }
}
