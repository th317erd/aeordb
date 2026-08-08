use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use crate::engine::deletion_record::DeletionRecord;
use crate::engine::engine_event::{
  VersionEventData, EVENT_VERSIONS_CREATED, EVENT_VERSIONS_DELETED, EVENT_VERSIONS_PROMOTED, EVENT_VERSIONS_RESTORED,
};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::kv_store::{KV_TYPE_SNAPSHOT, KV_TYPE_FORK};
use crate::engine::namespace_mutation::{
  NamespaceMutationAcknowledgement, NamespaceMutationBatch, NamespaceMutationCoordinator, NamespaceMutationFanout, NamespaceMutationKind,
  NamespaceMutationSourceIdentity, validate_existing_namespace_root,
};
use crate::engine::request_context::RequestContext;
use crate::engine::rss_sampler::PhaseSampler;
use crate::engine::storage_engine::StorageEngine;

#[derive(Debug)]
enum VersionMutationCounterEffect {
  None,
  SnapshotCreated,
  SnapshotDeleted,
  ForkCreated,
  ForkDeleted,
}

#[derive(Debug)]
struct VersionMutationEffects {
  throughput_bytes: u64,
  counter: VersionMutationCounterEffect,
  events: Vec<(&'static str, serde_json::Value)>,
}

impl VersionMutationEffects {
  fn new(throughput_bytes: u64, counter: VersionMutationCounterEffect) -> Self {
    Self { throughput_bytes, counter, events: Vec::new() }
  }

  fn with_event(mut self, event_type: &'static str, payload: serde_json::Value) -> Self {
    self.events.push((event_type, payload));
    self
  }
}

struct VersionMutationFanout<'a> {
  engine: &'a StorageEngine,
  context: Option<&'a RequestContext>,
  effects: Arc<OnceLock<VersionMutationEffects>>,
}

impl NamespaceMutationFanout for VersionMutationFanout<'_> {
  fn publish(&self, acknowledgement: &NamespaceMutationAcknowledgement) {
    let Some(effects) = self.effects.get() else {
      tracing::error!(operation_id = %acknowledgement.operation_id, "Version mutation committed without post-commit effects");
      return;
    };

    self.engine.counters().record_write(effects.throughput_bytes);
    match effects.counter {
      VersionMutationCounterEffect::None => {}
      VersionMutationCounterEffect::SnapshotCreated => self.engine.counters().increment_snapshots(),
      VersionMutationCounterEffect::SnapshotDeleted => self.engine.counters().decrement_snapshots(),
      VersionMutationCounterEffect::ForkCreated => self.engine.counters().increment_forks(),
      VersionMutationCounterEffect::ForkDeleted => self.engine.counters().decrement_forks(),
    }

    let Some(context) = self.context else {
      return;
    };
    for (event_type, event_payload) in &effects.events {
      let mut payload = event_payload.clone();
      if let Err(error) = acknowledgement.annotate_event_payload(&mut payload) {
        tracing::error!(operation_id = %acknowledgement.operation_id, error = %error, "Version mutation event payload is invalid");
        continue;
      }
      context.emit(event_type, payload);
    }
  }
}

/// Information about a named snapshot (a saved point-in-time reference).
#[derive(Debug, Clone)]
pub struct SnapshotInfo {
  /// Human-readable snapshot name (display only).
  pub name: String,
  /// Content-addressed root hash at the time of the snapshot.
  pub root_hash: Vec<u8>,
  /// When the snapshot was created (ms since epoch).
  pub created_at: i64,
  /// Arbitrary key-value metadata attached to the snapshot.
  pub metadata: HashMap<String, String>,
}

impl SnapshotInfo {
  /// Unique identifier for this snapshot (hex-encoded root hash).
  pub fn id(&self) -> String {
    hex::encode(&self.root_hash)
  }
}

/// Information about a named fork (an isolated branch of writes).
#[derive(Debug, Clone)]
pub struct ForkInfo {
  /// Human-readable fork name.
  pub name: String,
  /// Current root hash of the fork.
  pub root_hash: Vec<u8>,
  /// When the fork was created (ms since epoch).
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
  pub fn serialize(&self, hash_length: usize) -> EngineResult<Vec<u8>> {
    let metadata_json = serde_json::to_vec(&self.metadata).unwrap_or_default();
    let name_bytes = self.name.as_bytes();

    if name_bytes.len() > u16::MAX as usize {
      return Err(EngineError::InvalidInput(format!("Snapshot name too long: {} bytes exceeds u16 max (65535)", name_bytes.len())));
    }
    if metadata_json.len() > u32::MAX as usize {
      return Err(EngineError::InvalidInput(format!("Snapshot metadata too large: {} bytes exceeds u32 max", metadata_json.len())));
    }

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

    Ok(buffer)
  }

  pub fn deserialize(data: &[u8], hash_length: usize, version: u8) -> EngineResult<Self> {
    match version {
      0 => Self::deserialize_v0(data, hash_length),
      _ => Err(crate::engine::errors::EngineError::InvalidEntryVersion(version)),
    }
  }

  fn deserialize_v0(data: &[u8], hash_length: usize) -> EngineResult<Self> {
    if data.len() < 2 {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "SnapshotInfo data too short for name_length".to_string() });
    }

    let name_length = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut cursor = 2;

    if data.len() < cursor + name_length {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "SnapshotInfo data too short for name".to_string() });
    }

    let name = String::from_utf8(data[cursor..cursor + name_length].to_vec())
      .map_err(|_| EngineError::CorruptEntry { offset: 0, reason: "Invalid UTF-8 in snapshot name".to_string() })?;
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
      data[cursor],
      data[cursor + 1],
      data[cursor + 2],
      data[cursor + 3],
      data[cursor + 4],
      data[cursor + 5],
      data[cursor + 6],
      data[cursor + 7],
    ]);
    cursor += 8;

    let metadata_json_length = u32::from_le_bytes([data[cursor], data[cursor + 1], data[cursor + 2], data[cursor + 3]]) as usize;
    cursor += 4;

    if data.len() < cursor + metadata_json_length {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "SnapshotInfo data too short for metadata_json".to_string() });
    }

    let metadata: HashMap<String, String> = if metadata_json_length > 0 {
      serde_json::from_slice(&data[cursor..cursor + metadata_json_length])
        .map_err(|error| EngineError::CorruptEntry { offset: 0, reason: format!("Failed to deserialize snapshot metadata: {}", error) })?
    } else {
      HashMap::new()
    };

    Ok(SnapshotInfo { name, root_hash, created_at, metadata })
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
  pub fn serialize(&self, hash_length: usize) -> EngineResult<Vec<u8>> {
    let name_bytes = self.name.as_bytes();

    if name_bytes.len() > u16::MAX as usize {
      return Err(EngineError::InvalidInput(format!("Fork name too long: {} bytes exceeds u16 max (65535)", name_bytes.len())));
    }

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

    Ok(buffer)
  }

  pub fn deserialize(data: &[u8], hash_length: usize, version: u8) -> EngineResult<Self> {
    match version {
      0 => Self::deserialize_v0(data, hash_length),
      _ => Err(crate::engine::errors::EngineError::InvalidEntryVersion(version)),
    }
  }

  fn deserialize_v0(data: &[u8], hash_length: usize) -> EngineResult<Self> {
    if data.len() < 2 {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "ForkInfo data too short for name_length".to_string() });
    }

    let name_length = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut cursor = 2;

    if data.len() < cursor + name_length {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "ForkInfo data too short for name".to_string() });
    }

    let name = String::from_utf8(data[cursor..cursor + name_length].to_vec())
      .map_err(|_| EngineError::CorruptEntry { offset: 0, reason: "Invalid UTF-8 in fork name".to_string() })?;
    cursor += name_length;

    if data.len() < cursor + hash_length + 8 {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "ForkInfo data too short for hash + timestamp".to_string() });
    }

    let root_hash = data[cursor..cursor + hash_length].to_vec();
    cursor += hash_length;

    let created_at = i64::from_le_bytes([
      data[cursor],
      data[cursor + 1],
      data[cursor + 2],
      data[cursor + 3],
      data[cursor + 4],
      data[cursor + 5],
      data[cursor + 6],
      data[cursor + 7],
    ]);

    Ok(ForkInfo { name, root_hash, created_at })
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

  fn locator_source_path(family: &str, key: &[u8]) -> String {
    format!("/.aeordb-system/version-locators/{family}/{}", hex::encode(key))
  }

  fn read_snapshot_locator(engine: &StorageEngine, key: &[u8], name: &str) -> EngineResult<Option<SnapshotInfo>> {
    let Some((header, stored_key, value)) = engine.get_entry(key)? else {
      return Ok(None);
    };
    if header.entry_type != EntryType::Snapshot || stored_key != key {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("snapshot locator '{}' does not select a Snapshot entry", name) });
    }
    SnapshotInfo::deserialize(&value, engine.hash_algo().hash_length(), header.entry_version).map(Some)
  }

  fn read_fork_locator(engine: &StorageEngine, key: &[u8], name: &str) -> EngineResult<Option<ForkInfo>> {
    let Some((header, stored_key, value)) = engine.get_entry(key)? else {
      return Ok(None);
    };
    if header.entry_type != EntryType::Fork || stored_key != key {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("fork locator '{}' does not select a Fork entry", name) });
    }
    ForkInfo::deserialize(&value, engine.hash_algo().hash_length(), header.entry_version).map(Some)
  }

  /// Prepare the durable tombstone needed for deletion replay without writing
  /// it before the owning namespace transaction is admitted.
  fn prepare_deletion(engine: &StorageEngine, key_string: &str) -> EngineResult<(Vec<u8>, Vec<u8>)> {
    let deletion = DeletionRecord::new(key_string.to_string(), None);
    let deletion_key = engine.compute_hash(format!("del:{}:{}", key_string, deletion.deleted_at).as_bytes())?;
    let deletion_value = deletion.serialize();
    Ok((deletion_key, deletion_value))
  }

  fn execute_version_mutation<'operation, T, F>(
    &'operation self,
    context: Option<&'operation RequestContext>,
    prepare: F,
  ) -> EngineResult<T>
  where
    F: FnOnce(&StorageEngine) -> EngineResult<(Option<(NamespaceMutationBatch, VersionMutationEffects)>, T)>,
  {
    let effects = Arc::new(OnceLock::new());
    let fanout = Arc::new(VersionMutationFanout { engine: self.engine, context, effects: effects.clone() });
    let coordinator = NamespaceMutationCoordinator::with_fanout(self.engine, fanout);
    let (_acknowledgement, output) = coordinator.prepare_and_maybe_execute(|planning_engine| {
      let (prepared, output) = prepare(planning_engine)?;
      let Some((batch, planned_effects)) = prepared else {
        return Ok((None, output));
      };
      effects
        .set(planned_effects)
        .map_err(|_| EngineError::InvalidInput("version mutation effects were prepared more than once".to_string()))?;
      Ok((Some(batch), output))
    })?;
    Ok(output)
  }

  /// Get the current HEAD hash from the file header.
  pub fn get_head_hash(&self) -> EngineResult<Vec<u8>> {
    self.engine.head_hash()
  }

  /// Look up a fork's current root hash by name.
  pub fn get_fork_hash(&self, name: &str) -> EngineResult<Option<Vec<u8>>> {
    let key = self.fork_key(name)?;
    Ok(Self::read_fork_locator(self.engine, &key, name)?.map(|fork| fork.root_hash))
  }

  /// Look up a snapshot's root hash by name.
  pub fn get_snapshot_hash(&self, name: &str) -> EngineResult<Vec<u8>> {
    let key = self.snapshot_key(name)?;
    let Some(snapshot) = Self::read_snapshot_locator(self.engine, &key, name)? else {
      return Err(EngineError::NotFound(format!("Snapshot not found: {}", name)));
    };
    Ok(snapshot.root_hash)
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

  /// Create a named snapshot of the current HEAD state with optional metadata.
  ///
  /// Returns an error if a snapshot with the given name already exists.
  pub fn create_snapshot(&self, ctx: &RequestContext, name: &str, metadata: HashMap<String, String>) -> EngineResult<SnapshotInfo> {
    let _mem = PhaseSampler::start("create_snapshot", std::time::Duration::from_millis(50));
    crate::engine::lifecycle_config::ensure_snapshot_writes_enabled(self.engine)?;
    let key = self.snapshot_key(name)?;
    let created_at = chrono::Utc::now().timestamp_millis();
    let name = name.to_string();
    self.execute_version_mutation(Some(ctx), move |planning_engine| {
      if Self::read_snapshot_locator(planning_engine, &key, &name)?.is_some() {
        return Err(EngineError::AlreadyExists(format!("Snapshot already exists: {}", name)));
      }

      let root_hash = planning_engine.head_hash()?;
      let existing_snapshots = self.list_snapshots()?;
      if let Some(existing) = existing_snapshots.into_iter().find(|existing| existing.root_hash == root_hash) {
        return Ok((None, existing));
      }

      let mut metadata = metadata;
      metadata
        .entry(crate::engine::lifecycle_config::SNAPSHOT_TYPE_KEY.to_string())
        .or_insert_with(|| crate::engine::lifecycle_config::SNAPSHOT_TYPE_MANUAL.to_string());
      let snapshot_info = SnapshotInfo { name: name.clone(), root_hash: root_hash.clone(), created_at, metadata };
      let value = snapshot_info.serialize(planning_engine.hash_algo().hash_length())?;

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      batch.replace_locator(EntryType::Snapshot, key.clone(), value.clone(), 0)?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: Self::locator_source_path("snapshots", &key),
        entry_type: Some(EntryType::Snapshot.to_u8()),
        previous_identity: None,
        new_identity: Some(root_hash),
      })?;
      let version_data = VersionEventData {
        name: name.clone(),
        version_type: Some("snapshot".to_string()),
        root_hash: hex::encode(&snapshot_info.root_hash),
        created_at: Some(snapshot_info.created_at),
      };
      let effects = VersionMutationEffects::new(value.len() as u64, VersionMutationCounterEffect::SnapshotCreated)
        .with_event(EVENT_VERSIONS_CREATED, serde_json::json!({"versions": [version_data]}));
      Ok((Some((batch, effects)), snapshot_info))
    })
  }

  /// Restore a named snapshot by rewinding HEAD to its root hash.
  pub fn restore_snapshot(&self, ctx: &RequestContext, name: &str) -> EngineResult<()> {
    let name = name.to_string();
    self.execute_version_mutation(Some(ctx), move |planning_engine| {
      let root_hash = self.get_snapshot_hash(&name)?;
      let previous_root_hash = planning_engine.head_hash()?;
      if previous_root_hash == root_hash {
        return Ok((None, ()));
      }

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::Restore);
      batch.set_head_hash(root_hash.clone());
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: "/".to_string(),
        entry_type: Some(EntryType::DirectoryIndex.to_u8()),
        previous_identity: Some(previous_root_hash),
        new_identity: Some(root_hash.clone()),
      })?;
      let effects = VersionMutationEffects::new(0, VersionMutationCounterEffect::None).with_event(
        EVENT_VERSIONS_RESTORED,
        serde_json::json!({"versions": [VersionEventData {
          name: name.clone(),
          version_type: Some("snapshot".to_string()),
          root_hash: hex::encode(&root_hash),
          created_at: None,
        }]}),
      );
      Ok((Some((batch, effects)), ()))
    })
  }

  /// List all snapshots, sorted by created_at ascending.
  pub fn list_snapshots(&self) -> EngineResult<Vec<SnapshotInfo>> {
    let hash_length = self.engine.hash_algo().hash_length();
    let entries = self.engine.entries_by_type_strict(KV_TYPE_SNAPSHOT)?;

    let mut snapshots = Vec::new();
    for (header, _key, value) in entries {
      let snapshot = SnapshotInfo::deserialize(&value, hash_length, header.entry_version)?;
      snapshots.push(snapshot);
    }

    snapshots.sort_by_key(|snapshot| snapshot.created_at);
    Ok(snapshots)
  }

  /// Find a snapshot by its ID (hex-encoded root hash).
  pub fn get_snapshot_by_id(&self, id: &str) -> EngineResult<Option<SnapshotInfo>> {
    let target_hash = hex::decode(id).map_err(|_| EngineError::InvalidInput(format!("Invalid snapshot ID: {}", id)))?;
    let snapshots = self.list_snapshots()?;
    Ok(snapshots.into_iter().find(|s| s.root_hash == target_hash))
  }

  /// Resolve a snapshot identifier — tries ID (hex hash) first, then name.
  pub fn resolve_snapshot(&self, identifier: &str) -> EngineResult<SnapshotInfo> {
    // Try as ID first (64-char hex string)
    if identifier.len() == 64 && identifier.chars().all(|c| c.is_ascii_hexdigit()) {
      if let Some(snap) = self.get_snapshot_by_id(identifier)? {
        return Ok(snap);
      }
    }
    let key = self.snapshot_key(identifier)?;
    Self::read_snapshot_locator(self.engine, &key, identifier)?
      .ok_or_else(|| EngineError::NotFound(format!("Snapshot not found: {}", identifier)))
  }

  /// Delete a named snapshot by marking its KV entry as deleted and
  /// writing a DeletionRecord so the deletion survives restart.
  pub fn delete_snapshot(&self, ctx: &RequestContext, name: &str) -> EngineResult<()> {
    let _mem = PhaseSampler::start("delete_snapshot", std::time::Duration::from_millis(50));
    let key = self.snapshot_key(name)?;
    let name = name.to_string();
    self.execute_version_mutation(Some(ctx), move |planning_engine| {
      let Some(snapshot) = Self::read_snapshot_locator(planning_engine, &key, &name)? else {
        return Err(EngineError::NotFound(format!("Snapshot not found: {}", name)));
      };
      let key_string = format!("snap:{}", name);
      let (deletion_key, deletion_value) = Self::prepare_deletion(planning_engine, &key_string)?;

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion_value, 0)?;
      batch.retire_locator(key.clone())?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: Self::locator_source_path("snapshots", &key),
        entry_type: Some(EntryType::Snapshot.to_u8()),
        previous_identity: Some(snapshot.root_hash),
        new_identity: None,
      })?;
      let effects = VersionMutationEffects::new(0, VersionMutationCounterEffect::SnapshotDeleted).with_event(
        EVENT_VERSIONS_DELETED,
        serde_json::json!({"versions": [VersionEventData {
          name: name.clone(),
          version_type: Some("snapshot".to_string()),
          root_hash: hex::encode(&key),
          created_at: None,
        }]}),
      );
      Ok((Some((batch, effects)), ()))
    })
  }

  /// Rename a snapshot. Creates a new snapshot entry with the new name
  /// and the same root hash/metadata, then deletes the old one.
  pub fn rename_snapshot(&self, ctx: &RequestContext, old_name: &str, new_name: &str) -> EngineResult<SnapshotInfo> {
    crate::engine::lifecycle_config::ensure_snapshot_writes_enabled(self.engine)?;
    let old_key = self.snapshot_key(old_name)?;
    let new_key = self.snapshot_key(new_name)?;
    let old_name = old_name.to_string();
    let new_name = new_name.to_string();
    self.execute_version_mutation(Some(ctx), move |planning_engine| {
      let Some(old_snapshot) = Self::read_snapshot_locator(planning_engine, &old_key, &old_name)? else {
        return Err(EngineError::NotFound(format!("Snapshot not found: {}", old_name)));
      };
      if Self::read_snapshot_locator(planning_engine, &new_key, &new_name)?.is_some() {
        return Err(EngineError::AlreadyExists(format!("Snapshot already exists: {}", new_name)));
      }

      let hash_length = planning_engine.hash_algo().hash_length();
      let root_hash = old_snapshot.root_hash.clone();
      let mut metadata = old_snapshot.metadata;
      metadata.insert(
        crate::engine::lifecycle_config::SNAPSHOT_TYPE_KEY.to_string(),
        crate::engine::lifecycle_config::SNAPSHOT_TYPE_MANUAL.to_string(),
      );
      let new_snapshot =
        SnapshotInfo { name: new_name.clone(), root_hash: root_hash.clone(), created_at: old_snapshot.created_at, metadata };
      let new_value = new_snapshot.serialize(hash_length)?;
      let (deletion_key, deletion_value) = Self::prepare_deletion(planning_engine, &format!("snap:{}", old_name))?;

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion_value, 0)?;
      batch.replace_locator(EntryType::Snapshot, new_key.clone(), new_value.clone(), 0)?;
      batch.retire_locator(old_key.clone())?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: Self::locator_source_path("snapshots", &old_key),
        entry_type: Some(EntryType::Snapshot.to_u8()),
        previous_identity: Some(root_hash.clone()),
        new_identity: None,
      })?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: Self::locator_source_path("snapshots", &new_key),
        entry_type: Some(EntryType::Snapshot.to_u8()),
        previous_identity: None,
        new_identity: Some(root_hash),
      })?;
      let effects = VersionMutationEffects::new(new_value.len() as u64, VersionMutationCounterEffect::None);
      Ok((Some((batch, effects)), new_snapshot))
    })
  }

  /// Create a named fork for isolated writes.
  ///
  /// - If `base` is `None` or `Some("HEAD")`, forks from the current HEAD.
  /// - If `base` is a snapshot name, forks from that snapshot's root hash.
  ///
  /// Returns an error if a fork with the given name already exists.
  pub fn create_fork(&self, ctx: &RequestContext, name: &str, base: Option<&str>) -> EngineResult<ForkInfo> {
    let key = self.fork_key(name)?;
    let created_at = chrono::Utc::now().timestamp_millis();
    let name = name.to_string();
    let base = base.map(str::to_string);
    self.execute_version_mutation(Some(ctx), move |planning_engine| {
      if Self::read_fork_locator(planning_engine, &key, &name)?.is_some() {
        return Err(EngineError::AlreadyExists(format!("Fork already exists: {}", name)));
      }
      let root_hash = match base.as_deref() {
        None | Some("HEAD") => planning_engine.head_hash()?,
        Some(snapshot_name) => self.get_snapshot_hash(snapshot_name)?,
      };
      let fork_info = ForkInfo { name: name.clone(), root_hash: root_hash.clone(), created_at };
      let value = fork_info.serialize(planning_engine.hash_algo().hash_length())?;

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      batch.replace_locator(EntryType::Fork, key.clone(), value.clone(), 0)?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: Self::locator_source_path("forks", &key),
        entry_type: Some(EntryType::Fork.to_u8()),
        previous_identity: None,
        new_identity: Some(root_hash),
      })?;
      let version_data = VersionEventData {
        name: name.clone(),
        version_type: Some("fork".to_string()),
        root_hash: hex::encode(&fork_info.root_hash),
        created_at: Some(fork_info.created_at),
      };
      let effects = VersionMutationEffects::new(value.len() as u64, VersionMutationCounterEffect::ForkCreated)
        .with_event(EVENT_VERSIONS_CREATED, serde_json::json!({"versions": [version_data]}));
      Ok((Some((batch, effects)), fork_info))
    })
  }

  /// Promote a fork: advance HEAD to the fork's root hash, then delete the fork.
  ///
  /// After promotion, the fork's state becomes the new main-line version.
  pub fn promote_fork(&self, ctx: &RequestContext, name: &str) -> EngineResult<()> {
    let key = self.fork_key(name)?;
    let name = name.to_string();
    self.execute_version_mutation(Some(ctx), move |planning_engine| {
      let Some(fork) = Self::read_fork_locator(planning_engine, &key, &name)? else {
        return Err(EngineError::NotFound(format!("Fork not found: {}", name)));
      };
      let previous_root_hash = planning_engine.head_hash()?;
      let (deletion_key, deletion_value) = Self::prepare_deletion(planning_engine, &format!("::aeordb:fork:{}", name))?;

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::Promote);
      batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion_value, 0)?;
      batch.retire_locator(key.clone())?;
      batch.set_head_hash(fork.root_hash.clone());
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: "/".to_string(),
        entry_type: Some(EntryType::DirectoryIndex.to_u8()),
        previous_identity: Some(previous_root_hash),
        new_identity: Some(fork.root_hash.clone()),
      })?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: Self::locator_source_path("forks", &key),
        entry_type: Some(EntryType::Fork.to_u8()),
        previous_identity: Some(fork.root_hash.clone()),
        new_identity: None,
      })?;

      let promoted = VersionEventData {
        name: name.clone(),
        version_type: Some("fork".to_string()),
        root_hash: hex::encode(&fork.root_hash),
        created_at: None,
      };
      let deleted =
        VersionEventData { name: name.clone(), version_type: Some("fork".to_string()), root_hash: hex::encode(&key), created_at: None };
      let effects = VersionMutationEffects::new(0, VersionMutationCounterEffect::ForkDeleted)
        .with_event(EVENT_VERSIONS_PROMOTED, serde_json::json!({"versions": [promoted]}))
        .with_event(EVENT_VERSIONS_DELETED, serde_json::json!({"versions": [deleted]}));
      Ok((Some((batch, effects)), ()))
    })
  }

  /// Abandon a fork by marking its KV entry as deleted and
  /// writing a DeletionRecord so the deletion survives restart.
  pub fn abandon_fork(&self, ctx: &RequestContext, name: &str) -> EngineResult<()> {
    let key = self.fork_key(name)?;
    let name = name.to_string();
    self.execute_version_mutation(Some(ctx), move |planning_engine| {
      let Some(fork) = Self::read_fork_locator(planning_engine, &key, &name)? else {
        return Err(EngineError::NotFound(format!("Fork not found: {}", name)));
      };
      let (deletion_key, deletion_value) = Self::prepare_deletion(planning_engine, &format!("::aeordb:fork:{}", name))?;

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      batch.store_dependency(EntryType::DeletionRecord, deletion_key, deletion_value, 0)?;
      batch.retire_locator(key.clone())?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: Self::locator_source_path("forks", &key),
        entry_type: Some(EntryType::Fork.to_u8()),
        previous_identity: Some(fork.root_hash),
        new_identity: None,
      })?;
      let effects = VersionMutationEffects::new(0, VersionMutationCounterEffect::ForkDeleted).with_event(
        EVENT_VERSIONS_DELETED,
        serde_json::json!({"versions": [VersionEventData {
          name: name.clone(),
          version_type: Some("fork".to_string()),
          root_hash: hex::encode(&key),
          created_at: None,
        }]}),
      );
      Ok((Some((batch, effects)), ()))
    })
  }

  /// List all active forks.
  pub fn list_forks(&self) -> EngineResult<Vec<ForkInfo>> {
    let hash_length = self.engine.hash_algo().hash_length();
    let entries = self.engine.entries_by_type_strict(KV_TYPE_FORK)?;

    let mut forks = Vec::new();
    for (header, _key, value) in entries {
      let fork = ForkInfo::deserialize(&value, hash_length, header.entry_version)?;
      forks.push(fork);
    }

    forks.sort_by_key(|fork| fork.created_at);
    Ok(forks)
  }

  /// Update a fork's root hash (used when writing to a fork).
  pub fn update_fork_hash(&self, name: &str, new_root_hash: &[u8]) -> EngineResult<()> {
    let key = self.fork_key(name)?;
    let name = name.to_string();
    let new_root_hash = new_root_hash.to_vec();
    self.execute_version_mutation(None, move |planning_engine| {
      let Some(existing) = Self::read_fork_locator(planning_engine, &key, &name)? else {
        return Err(EngineError::NotFound(format!("Fork not found: {}", name)));
      };
      let hash_length = planning_engine.hash_algo().hash_length();
      if existing.root_hash == new_root_hash {
        return Ok((None, ()));
      }
      validate_existing_namespace_root(planning_engine, &new_root_hash, "fork update")?;
      let previous_root_hash = existing.root_hash;
      let updated = ForkInfo { name: name.clone(), root_hash: new_root_hash.clone(), created_at: existing.created_at };
      let value = updated.serialize(hash_length)?;

      let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
      batch.replace_locator(EntryType::Fork, key.clone(), value.clone(), 0)?;
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: Self::locator_source_path("forks", &key),
        entry_type: Some(EntryType::Fork.to_u8()),
        previous_identity: Some(previous_root_hash),
        new_identity: Some(new_root_hash),
      })?;
      let effects = VersionMutationEffects::new(value.len() as u64, VersionMutationCounterEffect::None);
      Ok((Some((batch, effects)), ()))
    })
  }
}
