use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::chunk::ChunkHash;
use super::chunk_storage::ChunkStoreError;

/// A version snapshot pointing to a root ContentHashMap at a point in time.
#[derive(Debug, Clone)]
pub struct Version {
  pub version_id: Uuid,
  pub name: Option<String>,
  pub root_hash: ChunkHash,
  pub parent_version_id: Option<Uuid>,
  pub created_at: DateTime<Utc>,
  pub metadata: HashMap<String, String>,
}

/// Trait for persisting Version records.
pub trait VersionStorage: Send + Sync {
  fn store_version(&self, version: &Version) -> Result<(), ChunkStoreError>;
  fn get_version(&self, version_id: &Uuid) -> Result<Option<Version>, ChunkStoreError>;
  fn get_version_by_name(&self, name: &str) -> Result<Option<Version>, ChunkStoreError>;
  fn get_latest_version(&self) -> Result<Option<Version>, ChunkStoreError>;
  fn list_versions(&self) -> Result<Vec<Version>, ChunkStoreError>;
  fn delete_version(&self, version_id: &Uuid) -> Result<bool, ChunkStoreError>;
  fn update_version_name(&self, version_id: &Uuid, name: &str) -> Result<(), ChunkStoreError>;
}

/// In-memory implementation of VersionStorage for testing.
pub struct InMemoryVersionStorage {
  versions: RwLock<Vec<Version>>,
}

impl InMemoryVersionStorage {
  pub fn new() -> Self {
    Self {
      versions: RwLock::new(Vec::new()),
    }
  }
}

impl Default for InMemoryVersionStorage {
  fn default() -> Self {
    Self::new()
  }
}

impl VersionStorage for InMemoryVersionStorage {
  fn store_version(&self, version: &Version) -> Result<(), ChunkStoreError> {
    let mut versions = self.versions.write().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    versions.push(version.clone());
    Ok(())
  }

  fn get_version(&self, version_id: &Uuid) -> Result<Option<Version>, ChunkStoreError> {
    let versions = self.versions.read().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    Ok(versions.iter().find(|version| version.version_id == *version_id).cloned())
  }

  fn get_version_by_name(&self, name: &str) -> Result<Option<Version>, ChunkStoreError> {
    let versions = self.versions.read().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    Ok(
      versions
        .iter()
        .find(|version| version.name.as_deref() == Some(name))
        .cloned(),
    )
  }

  fn get_latest_version(&self) -> Result<Option<Version>, ChunkStoreError> {
    let versions = self.versions.read().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    Ok(
      versions
        .iter()
        .max_by_key(|version| version.created_at)
        .cloned(),
    )
  }

  fn list_versions(&self) -> Result<Vec<Version>, ChunkStoreError> {
    let versions = self.versions.read().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    let mut sorted = versions.clone();
    sorted.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(sorted)
  }

  fn delete_version(&self, version_id: &Uuid) -> Result<bool, ChunkStoreError> {
    let mut versions = self.versions.write().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;
    let original_length = versions.len();
    versions.retain(|version| version.version_id != *version_id);
    Ok(versions.len() < original_length)
  }

  fn update_version_name(&self, version_id: &Uuid, name: &str) -> Result<(), ChunkStoreError> {
    let mut versions = self.versions.write().map_err(|error| {
      ChunkStoreError::IoError(format!("lock poisoned: {error}"))
    })?;

    // Remove the name from any other version that has it (tags are unique).
    let name_string = name.to_string();
    for version in versions.iter_mut() {
      if version.name.as_deref() == Some(name) {
        version.name = None;
      }
    }

    let target = versions
      .iter_mut()
      .find(|version| version.version_id == *version_id);

    match target {
      Some(version) => {
        version.name = Some(name_string);
        Ok(())
      }
      None => Err(ChunkStoreError::ChunkNotFound([0u8; 32])),
    }
  }
}
