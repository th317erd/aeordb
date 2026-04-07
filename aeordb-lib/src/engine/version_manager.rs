use std::collections::HashMap;

use crate::engine::deletion_record::DeletionRecord;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::kv_store::{KV_TYPE_SNAPSHOT, KV_TYPE_FORK};
use crate::engine::storage_engine::StorageEngine;

/// Information about a named snapshot (a saved point-in-time reference).
#[derive(Debug, Clone)]
pub struct SnapshotInfo {
  pub name: String,
  pub root_hash: Vec<u8>,
  pub created_at: i64,
  pub metadata: HashMap<String, String>,
}

/// Information about a named fork (an isolated branch of writes).
#[derive(Debug, Clone)]
pub struct ForkInfo {
  pub name: String,
  pub root_hash: Vec<u8>,
  pub created_at: i64,
}

/// Serialization helpers for SnapshotInfo.
///
/// Binary format:
///   name_length: u16
///   name: [u8; name_length]
///   root_hash: [u8; hash_length]
///   created_at: i64
///   metadata_json_length: u32
///   metadata_json: [u8; metadata_json_length]
impl SnapshotInfo {
  pub fn serialize(&self, hash_length: usize) -> Vec<u8> {
    let metadata_json = serde_json::to_vec(&self.metadata)
      .unwrap_or_default();
    let name_bytes = self.name.as_bytes();

    let capacity = 2 + name_bytes.len() + hash_length + 8 + 4 + metadata_json.len();
    let mut buffer = Vec::with_capacity(capacity);

    buffer.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(name_bytes);

    // Pad or truncate root_hash to hash_length
    if self.root_hash.len() >= hash_length {
      buffer.extend_from_slice(&self.root_hash[..hash_length]);
    } else {
      buffer.extend_from_slice(&self.root_hash);
      buffer.extend(std::iter::repeat_n(0u8, hash_length - self.root_hash.len()));
    }

    buffer.extend_from_slice(&self.created_at.to_le_bytes());
    buffer.extend_from_slice(&(metadata_json.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&metadata_json);

    buffer
  }

  pub fn deserialize(data: &[u8], hash_length: usize) -> EngineResult<Self> {
    if data.len() < 2 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "SnapshotInfo data too short for name_length".to_string(),
      });
    }

    let name_length = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut cursor = 2;

    if data.len() < cursor + name_length {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "SnapshotInfo data too short for name".to_string(),
      });
    }

    let name = String::from_utf8_lossy(&data[cursor..cursor + name_length]).to_string();
    cursor += name_length;

    if data.len() < cursor + hash_length + 8 + 4 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "SnapshotInfo data too short for hash + timestamp + metadata_length".to_string(),
      });
    }

    let root_hash = data[cursor..cursor + hash_length].to_vec();
    cursor += hash_length;

    let created_at = i64::from_le_bytes([
      data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3],
      data[cursor + 4], data[cursor + 5], data[cursor + 6], data[cursor + 7],
    ]);
    cursor += 8;

    let metadata_json_length = u32::from_le_bytes([
      data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3],
    ]) as usize;
    cursor += 4;

    if data.len() < cursor + metadata_json_length {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "SnapshotInfo data too short for metadata_json".to_string(),
      });
    }

    let metadata: HashMap<String, String> = if metadata_json_length > 0 {
      serde_json::from_slice(&data[cursor..cursor + metadata_json_length])
        .map_err(|error| EngineError::CorruptEntry {
          offset: 0,
          reason: format!("Failed to deserialize snapshot metadata: {}", error),
        })?
    } else {
      HashMap::new()
    };

    Ok(SnapshotInfo {
      name,
      root_hash,
      created_at,
      metadata,
    })
  }
}

/// Serialization helpers for ForkInfo.
///
/// Binary format:
///   name_length: u16
///   name: [u8; name_length]
///   root_hash: [u8; hash_length]
///   created_at: i64
impl ForkInfo {
  pub fn serialize(&self, hash_length: usize) -> Vec<u8> {
    let name_bytes = self.name.as_bytes();
    let capacity = 2 + name_bytes.len() + hash_length + 8;
    let mut buffer = Vec::with_capacity(capacity);

    buffer.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buffer.extend_from_slice(name_bytes);

    if self.root_hash.len() >= hash_length {
      buffer.extend_from_slice(&self.root_hash[..hash_length]);
    } else {
      buffer.extend_from_slice(&self.root_hash);
      buffer.extend(std::iter::repeat_n(0u8, hash_length - self.root_hash.len()));
    }

    buffer.extend_from_slice(&self.created_at.to_le_bytes());

    buffer
  }

  pub fn deserialize(data: &[u8], hash_length: usize) -> EngineResult<Self> {
    if data.len() < 2 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "ForkInfo data too short for name_length".to_string(),
      });
    }

    let name_length = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut cursor = 2;

    if data.len() < cursor + name_length {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "ForkInfo data too short for name".to_string(),
      });
    }

    let name = String::from_utf8_lossy(&data[cursor..cursor + name_length]).to_string();
    cursor += name_length;

    if data.len() < cursor + hash_length + 8 {
      return Err(EngineError::CorruptEntry {
        offset: 0,
        reason: "ForkInfo data too short for hash + timestamp".to_string(),
      });
    }

    let root_hash = data[cursor..cursor + hash_length].to_vec();
    cursor += hash_length;

    let created_at = i64::from_le_bytes([
      data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3],
      data[cursor + 4], data[cursor + 5], data[cursor + 6], data[cursor + 7],
    ]);

    Ok(ForkInfo {
      name,
      root_hash,
      created_at,
    })
  }
}

/// Manages snapshots and forks for versioning.
///
/// Snapshots save the current HEAD hash with a name and timestamp.
/// Forks create separate HEAD pointers for isolated writes.
/// Promoting a fork moves HEAD to the fork's current hash.
pub struct VersionManager<'a> {
  engine: &'a StorageEngine,
}

impl<'a> VersionManager<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    VersionManager { engine }
  }

  /// Compute the KV key hash for a snapshot name.
  fn snapshot_key(&self, name: &str) -> EngineResult<Vec<u8>> {
    self.engine.compute_hash(format!("snap:{}", name).as_bytes())
  }

  /// Compute the KV key hash for a fork name.
  fn fork_key(&self, name: &str) -> EngineResult<Vec<u8>> {
    self.engine.compute_hash(format!("::aeordb:fork:{}", name).as_bytes())
  }

  /// Persist a deletion to disk by writing a DeletionRecord.
  /// The `key_string` is the domain-prefixed key (before hashing) so
  /// that `open_internal` can recompute the hash and replay the deletion.
  fn persist_deletion(engine: &StorageEngine, key_string: &str) -> EngineResult<()> {
    let deletion = DeletionRecord::new(key_string.to_string(), None);
    let deletion_key = engine.compute_hash(
      format!("del:{}:{}", key_string, deletion.deleted_at).as_bytes(),
    )?;
    let deletion_value = deletion.serialize();
    engine.store_entry(
      EntryType::DeletionRecord,
      &deletion_key,
      &deletion_value,
    )?;
    Ok(())
  }

  /// Get the current HEAD hash from the file header.
  pub fn get_head_hash(&self) -> EngineResult<Vec<u8>> {
    self.engine.head_hash()
  }

  /// Look up a fork's current root hash by name.
  pub fn get_fork_hash(&self, name: &str) -> EngineResult<Option<Vec<u8>>> {
    let key = self.fork_key(name)?;
    let entry = self.engine.get_entry(&key)?;

    let Some((_header, _key, value)) = entry else {
      return Ok(None);
    };

    // Check if the KV entry is marked as deleted
    if self.engine.is_entry_deleted(&key)? {
      return Ok(None);
    }

    let hash_length = self.engine.hash_algo().hash_length();
    let fork_info = ForkInfo::deserialize(&value, hash_length)?;
    Ok(Some(fork_info.root_hash))
  }

  /// Look up a snapshot's root hash by name.
  pub fn get_snapshot_hash(&self, name: &str) -> EngineResult<Vec<u8>> {
    let key = self.snapshot_key(name)?;
    let entry = self.engine.get_entry(&key)?;

    let Some((_header, _key, value)) = entry else {
      return Err(EngineError::NotFound(
        format!("Snapshot not found: {}", name),
      ));
    };

    if self.engine.is_entry_deleted(&key)? {
      return Err(EngineError::NotFound(
        format!("Snapshot not found: {}", name),
      ));
    }

    let hash_length = self.engine.hash_algo().hash_length();
    let snapshot_info = SnapshotInfo::deserialize(&value, hash_length)?;
    Ok(snapshot_info.root_hash)
  }

  /// Resolve a version name to a root hash.
  ///
  /// - None or "HEAD" returns the current HEAD hash.
  /// - Otherwise, tries fork first, then snapshot.
  pub fn resolve_root_hash(&self, version: Option<&str>) -> EngineResult<Vec<u8>> {
    match version {
      None => self.get_head_hash(),
      Some("HEAD") => self.get_head_hash(),
      Some(name) => {
        if let Some(hash) = self.get_fork_hash(name)? {
          return Ok(hash);
        }
        self.get_snapshot_hash(name)
      }
    }
  }

  /// Create a named snapshot of the current HEAD state.
  pub fn create_snapshot(
    &self,
    name: &str,
    metadata: HashMap<String, String>,
  ) -> EngineResult<SnapshotInfo> {
    let key = self.snapshot_key(name)?;

    // Check for duplicate name (only if not deleted)
    if self.engine.has_entry(&key)? && !self.engine.is_entry_deleted(&key)? {
      return Err(EngineError::AlreadyExists(
        format!("Snapshot already exists: {}", name),
      ));
    }

    let root_hash = self.get_head_hash()?;
    let created_at = chrono::Utc::now().timestamp_millis();

    let snapshot_info = SnapshotInfo {
      name: name.to_string(),
      root_hash,
      created_at,
      metadata,
    };

    let hash_length = self.engine.hash_algo().hash_length();
    let value = snapshot_info.serialize(hash_length);

    self.engine.store_entry_typed(
      EntryType::Snapshot,
      &key,
      &value,
      KV_TYPE_SNAPSHOT,
    )?;

    Ok(snapshot_info)
  }

  /// Restore a named snapshot by setting HEAD to its root hash.
  pub fn restore_snapshot(&self, name: &str) -> EngineResult<()> {
    let root_hash = self.get_snapshot_hash(name)?;
    self.engine.update_head(&root_hash)
  }

  /// List all snapshots, sorted by created_at ascending.
  pub fn list_snapshots(&self) -> EngineResult<Vec<SnapshotInfo>> {
    let hash_length = self.engine.hash_algo().hash_length();
    let entries = self.engine.entries_by_type(KV_TYPE_SNAPSHOT)?;

    let mut snapshots = Vec::new();
    for (key, value) in entries {
      // Skip deleted entries
      if self.engine.is_entry_deleted(&key)? {
        continue;
      }
      let snapshot = SnapshotInfo::deserialize(&value, hash_length)?;
      snapshots.push(snapshot);
    }

    snapshots.sort_by_key(|snapshot| snapshot.created_at);
    Ok(snapshots)
  }

  /// Delete a named snapshot by marking its KV entry as deleted and
  /// writing a DeletionRecord so the deletion survives restart.
  pub fn delete_snapshot(&self, name: &str) -> EngineResult<()> {
    let key = self.snapshot_key(name)?;

    if !self.engine.has_entry(&key)? || self.engine.is_entry_deleted(&key)? {
      return Err(EngineError::NotFound(
        format!("Snapshot not found: {}", name),
      ));
    }

    self.engine.mark_entry_deleted(&key)?;

    // Persist the deletion to disk so it survives restart.
    let key_string = format!("snap:{}", name);
    Self::persist_deletion(self.engine, &key_string)
  }

  /// Create a named fork.
  ///
  /// - If `base` is None or Some("HEAD"), forks from current HEAD.
  /// - If `base` is a snapshot name, forks from that snapshot's root hash.
  pub fn create_fork(
    &self,
    name: &str,
    base: Option<&str>,
  ) -> EngineResult<ForkInfo> {
    let key = self.fork_key(name)?;

    // Check for duplicate fork name
    if self.engine.has_entry(&key)? && !self.engine.is_entry_deleted(&key)? {
      return Err(EngineError::AlreadyExists(
        format!("Fork already exists: {}", name),
      ));
    }

    let root_hash = match base {
      None | Some("HEAD") => self.get_head_hash()?,
      Some(snapshot_name) => self.get_snapshot_hash(snapshot_name)?,
    };

    let created_at = chrono::Utc::now().timestamp_millis();

    let fork_info = ForkInfo {
      name: name.to_string(),
      root_hash,
      created_at,
    };

    let hash_length = self.engine.hash_algo().hash_length();
    let value = fork_info.serialize(hash_length);

    self.engine.store_entry_typed(
      EntryType::Fork,
      &key,
      &value,
      KV_TYPE_FORK,
    )?;

    Ok(fork_info)
  }

  /// Promote a fork: set HEAD to the fork's root hash, then delete the fork.
  pub fn promote_fork(&self, name: &str) -> EngineResult<()> {
    let fork_hash = self.get_fork_hash(name)?
      .ok_or_else(|| EngineError::NotFound(
        format!("Fork not found: {}", name),
      ))?;

    self.engine.update_head(&fork_hash)?;
    self.abandon_fork(name)
  }

  /// Abandon a fork by marking its KV entry as deleted and
  /// writing a DeletionRecord so the deletion survives restart.
  pub fn abandon_fork(&self, name: &str) -> EngineResult<()> {
    let key = self.fork_key(name)?;

    if !self.engine.has_entry(&key)? || self.engine.is_entry_deleted(&key)? {
      return Err(EngineError::NotFound(
        format!("Fork not found: {}", name),
      ));
    }

    self.engine.mark_entry_deleted(&key)?;

    // Persist the deletion to disk so it survives restart.
    let key_string = format!("::aeordb:fork:{}", name);
    Self::persist_deletion(self.engine, &key_string)
  }

  /// List all active forks.
  pub fn list_forks(&self) -> EngineResult<Vec<ForkInfo>> {
    let hash_length = self.engine.hash_algo().hash_length();
    let entries = self.engine.entries_by_type(KV_TYPE_FORK)?;

    let mut forks = Vec::new();
    for (key, value) in entries {
      if self.engine.is_entry_deleted(&key)? {
        continue;
      }
      let fork = ForkInfo::deserialize(&value, hash_length)?;
      forks.push(fork);
    }

    forks.sort_by_key(|fork| fork.created_at);
    Ok(forks)
  }

  /// Update a fork's root hash (used when writing to a fork).
  pub fn update_fork_hash(
    &self,
    name: &str,
    new_root_hash: &[u8],
  ) -> EngineResult<()> {
    let key = self.fork_key(name)?;

    if !self.engine.has_entry(&key)? || self.engine.is_entry_deleted(&key)? {
      return Err(EngineError::NotFound(
        format!("Fork not found: {}", name),
      ));
    }

    // Read existing fork info to preserve created_at
    let entry = self.engine.get_entry(&key)?
      .ok_or_else(|| EngineError::NotFound(
        format!("Fork not found: {}", name),
      ))?;

    let hash_length = self.engine.hash_algo().hash_length();
    let existing = ForkInfo::deserialize(&entry.2, hash_length)?;

    let updated = ForkInfo {
      name: name.to_string(),
      root_hash: new_root_hash.to_vec(),
      created_at: existing.created_at,
    };

    let value = updated.serialize(hash_length);

    self.engine.store_entry_typed(
      EntryType::Fork,
      &key,
      &value,
      KV_TYPE_FORK,
    )?;

    Ok(())
  }
}
