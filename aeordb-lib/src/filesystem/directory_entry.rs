use crate::storage::ChunkHash;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EntryType {
  File,
  Directory,
  HardLink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryEntry {
  pub name: String,
  pub entry_type: EntryType,
  pub chunk_hashes: Vec<ChunkHash>,
  pub document_id: Uuid,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  pub content_type: Option<String>,
  pub total_size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum DirectoryEntryError {
  #[error("serialization error: {0}")]
  SerializationError(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, DirectoryEntryError>;

impl DirectoryEntry {
  /// Create a new file entry with auto-generated document_id and timestamps.
  pub fn new_file(
    name: impl Into<String>,
    chunk_hashes: Vec<ChunkHash>,
    content_type: Option<String>,
    total_size: u64,
  ) -> Self {
    let now = Utc::now();
    Self {
      name: name.into(),
      entry_type: EntryType::File,
      chunk_hashes,
      document_id: Uuid::new_v4(),
      created_at: now,
      updated_at: now,
      content_type,
      total_size,
    }
  }

  /// Create a new directory entry with empty chunk_hashes.
  pub fn new_directory(name: impl Into<String>) -> Self {
    let now = Utc::now();
    Self {
      name: name.into(),
      entry_type: EntryType::Directory,
      chunk_hashes: Vec::new(),
      document_id: Uuid::new_v4(),
      created_at: now,
      updated_at: now,
      content_type: None,
      total_size: 0,
    }
  }

  /// Create a new hard link entry, copying chunk_hashes from the target entry.
  pub fn new_hard_link(name: impl Into<String>, target_entry: &DirectoryEntry) -> Self {
    let now = Utc::now();
    Self {
      name: name.into(),
      entry_type: EntryType::HardLink,
      chunk_hashes: target_entry.chunk_hashes.clone(),
      document_id: Uuid::new_v4(),
      created_at: now,
      updated_at: now,
      content_type: target_entry.content_type.clone(),
      total_size: target_entry.total_size,
    }
  }

  /// Serialize this entry to JSON bytes.
  pub fn serialize_to_bytes(&self) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(self)?)
  }

  /// Deserialize an entry from JSON bytes.
  pub fn deserialize_from_bytes(bytes: &[u8]) -> Result<Self> {
    Ok(serde_json::from_slice(bytes)?)
  }
}
