use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use super::chunk::ChunkHash;
use super::chunk_storage::{ChunkStorage, ChunkStoreError};
use super::hash_map_store::{ContentHashMap, HashMapStore};
use super::chunk_config::ChunkConfig;
use super::version_storage::{Version, VersionStorage};

/// Difference between two versions.
#[derive(Debug, Clone)]
pub struct VersionDiff {
  pub version_a_id: Uuid,
  pub version_b_id: Uuid,
  pub chunks_added: Vec<ChunkHash>,
  pub chunks_removed: Vec<ChunkHash>,
  pub chunks_unchanged: Vec<ChunkHash>,
  pub data_added_bytes: u64,
  pub data_removed_bytes: u64,
}

/// Manages versions as named pointers to root ContentHashMaps.
pub struct VersionStore {
  hash_map_store: HashMapStore,
  chunk_storage: Arc<dyn ChunkStorage>,
  version_storage: Arc<dyn VersionStorage>,
}

impl VersionStore {
  pub fn new(
    chunk_storage: Arc<dyn ChunkStorage>,
    version_storage: Arc<dyn VersionStorage>,
  ) -> Self {
    let config = ChunkConfig::default();
    let hash_map_store = HashMapStore::new(chunk_storage.clone(), config);
    Self {
      hash_map_store,
      chunk_storage,
      version_storage,
    }
  }

  /// Create a new version pointing to the given root hash map.
  /// Automatically sets parent to the current latest version.
  pub fn create_version(
    &self,
    root_hash_map: &ContentHashMap,
    name: Option<String>,
    metadata: HashMap<String, String>,
  ) -> Result<Version, ChunkStoreError> {
    // Store the hash map as a chunk so it can be retrieved later.
    let root_hash = self.hash_map_store.store_hash_map_as_chunk(root_hash_map)?;

    let parent_version = self.version_storage.get_latest_version()?;
    let parent_version_id = parent_version.map(|version| version.version_id);

    let version = Version {
      version_id: Uuid::new_v4(),
      name,
      root_hash,
      parent_version_id,
      created_at: Utc::now(),
      metadata,
    };

    self.version_storage.store_version(&version)?;
    Ok(version)
  }

  /// Get a specific version by ID.
  pub fn get_version(&self, version_id: &Uuid) -> Result<Option<Version>, ChunkStoreError> {
    self.version_storage.get_version(version_id)
  }

  /// Get a version by its name/tag.
  pub fn get_version_by_name(&self, name: &str) -> Result<Option<Version>, ChunkStoreError> {
    self.version_storage.get_version_by_name(name)
  }

  /// Get the most recent version.
  pub fn get_latest_version(&self) -> Result<Option<Version>, ChunkStoreError> {
    self.version_storage.get_latest_version()
  }

  /// List all versions, ordered by created_at descending.
  pub fn list_versions(&self) -> Result<Vec<Version>, ChunkStoreError> {
    self.version_storage.list_versions()
  }

  /// Load the root hash map from a version, returning the full ContentHashMap.
  pub fn restore_version(&self, version_id: &Uuid) -> Result<ContentHashMap, ChunkStoreError> {
    let version = self.version_storage.get_version(version_id)?
      .ok_or_else(|| ChunkStoreError::IoError(
        format!("version not found: {version_id}")
      ))?;

    self.hash_map_store.load_hash_map(&version.root_hash)
  }

  /// Compute the difference between two versions.
  pub fn diff_versions(
    &self,
    version_a_id: &Uuid,
    version_b_id: &Uuid,
  ) -> Result<VersionDiff, ChunkStoreError> {
    let map_a = self.restore_version(version_a_id)?;
    let map_b = self.restore_version(version_b_id)?;

    let set_a: HashSet<ChunkHash> = map_a.chunk_hashes.iter().copied().collect();
    let set_b: HashSet<ChunkHash> = map_b.chunk_hashes.iter().copied().collect();

    let chunks_added: Vec<ChunkHash> = map_b.chunk_hashes.iter()
      .filter(|hash| !set_a.contains(*hash))
      .copied()
      .collect();

    let chunks_removed: Vec<ChunkHash> = map_a.chunk_hashes.iter()
      .filter(|hash| !set_b.contains(*hash))
      .copied()
      .collect();

    let chunks_unchanged: Vec<ChunkHash> = map_a.chunk_hashes.iter()
      .filter(|hash| set_b.contains(*hash))
      .copied()
      .collect();

    let mut data_added_bytes = 0u64;
    for hash in &chunks_added {
      if let Some(chunk) = self.chunk_storage.get_chunk(hash)? {
        data_added_bytes += chunk.data.len() as u64;
      }
    }

    let mut data_removed_bytes = 0u64;
    for hash in &chunks_removed {
      if let Some(chunk) = self.chunk_storage.get_chunk(hash)? {
        data_removed_bytes += chunk.data.len() as u64;
      }
    }

    Ok(VersionDiff {
      version_a_id: *version_a_id,
      version_b_id: *version_b_id,
      chunks_added,
      chunks_removed,
      chunks_unchanged,
      data_added_bytes,
      data_removed_bytes,
    })
  }

  /// Delete a version (but NOT its chunks).
  pub fn delete_version(&self, version_id: &Uuid) -> Result<(), ChunkStoreError> {
    let deleted = self.version_storage.delete_version(version_id)?;
    if !deleted {
      return Err(ChunkStoreError::IoError(
        format!("version not found: {version_id}")
      ));
    }
    Ok(())
  }

  /// Add or update a name/tag on a version.
  pub fn tag_version(&self, version_id: &Uuid, name: &str) -> Result<(), ChunkStoreError> {
    self.version_storage.update_version_name(version_id, name)
  }
}
