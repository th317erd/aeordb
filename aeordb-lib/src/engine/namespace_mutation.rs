use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde::Serialize;
use uuid::Uuid;

use crate::engine::entry_header::EntryHeader;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::kv_store::KVEntry;
use crate::engine::storage_engine::{StorageEngine, TransactionGuard};

/// Logical mutation families accepted by the shared namespace authority.
///
/// P2d freezes this service contract without changing existing producer
/// behavior. P2e migrates each producer wave onto these operation families.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceMutationKind {
  FileWrite,
  FileDelete,
  DirectoryCreate,
  DirectoryDelete,
  SymlinkWrite,
  SymlinkDelete,
  Copy,
  Rename,
  BatchWrite,
  Merge,
  Restore,
  Promote,
  Import,
  SyncApply,
  SystemWrite,
  PluginWrite,
  MaintenanceRepair,
}

impl NamespaceMutationKind {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::FileWrite => "file_write",
      Self::FileDelete => "file_delete",
      Self::DirectoryCreate => "directory_create",
      Self::DirectoryDelete => "directory_delete",
      Self::SymlinkWrite => "symlink_write",
      Self::SymlinkDelete => "symlink_delete",
      Self::Copy => "copy",
      Self::Rename => "rename",
      Self::BatchWrite => "batch_write",
      Self::Merge => "merge",
      Self::Restore => "restore",
      Self::Promote => "promote",
      Self::Import => "import",
      Self::SyncApply => "sync_apply",
      Self::SystemWrite => "system_write",
      Self::PluginWrite => "plugin_write",
      Self::MaintenanceRepair => "maintenance_repair",
    }
  }
}

/// Exact physical KV incarnation selected for a stable locator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LocatorPhysicalIncarnation {
  pub type_flags: u8,
  pub offset: u64,
  pub total_length: u32,
}

impl From<&KVEntry> for LocatorPhysicalIncarnation {
  fn from(entry: &KVEntry) -> Self {
    Self { type_flags: entry.type_flags, offset: entry.offset, total_length: entry.total_length }
  }
}

/// One stable-key transition in dependency order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LocatorReplacement {
  pub ordinal: u32,
  pub stable_key: Vec<u8>,
  pub old_incarnation: Option<LocatorPhysicalIncarnation>,
  pub new_incarnation: Option<LocatorPhysicalIncarnation>,
}

/// Canonical source identity attached to one logical mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NamespaceMutationSourceIdentity {
  pub path: String,
  pub entry_type: Option<u8>,
  pub previous_identity: Option<Vec<u8>>,
  pub new_identity: Option<Vec<u8>>,
}

/// One hard-authority acknowledgement produced after the exact durability
/// waiter has crossed the global frontier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NamespaceMutationAcknowledgement {
  pub operation_id: Uuid,
  pub kind: NamespaceMutationKind,
  pub publication_sequence: u64,
  pub previous_root_hash: Vec<u8>,
  pub root_hash: Vec<u8>,
  pub source_identities: Vec<NamespaceMutationSourceIdentity>,
  pub locator_replacements: Vec<LocatorReplacement>,
}

impl NamespaceMutationAcknowledgement {
  pub fn annotate_event_payload(&self, payload: &mut serde_json::Value) -> EngineResult<()> {
    let object =
      payload.as_object_mut().ok_or_else(|| EngineError::InvalidInput("namespace mutation event payload must be an object".to_string()))?;
    object.insert("operation_id".to_string(), serde_json::Value::String(self.operation_id.to_string()));
    object.insert("publication_sequence".to_string(), serde_json::Value::from(self.publication_sequence));
    object.insert("mutation_kind".to_string(), serde_json::Value::String(self.kind.as_str().to_string()));
    Ok(())
  }
}

/// Recoverable-soft fanout invoked exactly once after hard namespace authority.
/// Implementations must reconstruct gaps from authority and therefore cannot
/// make a committed user mutation fail.
pub trait NamespaceMutationFanout: Send + Sync {
  fn publish(&self, acknowledgement: &NamespaceMutationAcknowledgement);
}

#[derive(Debug)]
struct NoopNamespaceMutationFanout;

impl NamespaceMutationFanout for NoopNamespaceMutationFanout {
  fn publish(&self, _acknowledgement: &NamespaceMutationAcknowledgement) {}
}

struct RootPublicationFanout<'a> {
  engine: &'a StorageEngine,
}

impl NamespaceMutationFanout for RootPublicationFanout<'_> {
  fn publish(&self, acknowledgement: &NamespaceMutationAcknowledgement) {
    self.engine.counters().record_write(0);
    if let Err(error) = self.engine.counters().reconcile_live_namespace_from_head(self.engine) {
      tracing::error!(operation_id = %acknowledgement.operation_id, %error, "Whole-root publication could not reconcile live namespace counters");
    }
  }
}

#[derive(Debug)]
struct PlannedEntryWrite {
  entry_type: EntryType,
  key: Vec<u8>,
  value: Vec<u8>,
  flags: u8,
  entry_version: u8,
}

#[derive(Debug)]
enum PlannedLocatorMutation {
  Replace(PlannedEntryWrite),
  Retire { key: Vec<u8> },
}

impl PlannedLocatorMutation {
  fn key(&self) -> &[u8] {
    match self {
      Self::Replace(write) => &write.key,
      Self::Retire { key } => key,
    }
  }
}

/// Fully prepared v3 namespace publication.
///
/// Construction performs no engine mutation. Duplicate stable locators are
/// rejected while the batch is still only caller-owned memory.
#[derive(Debug)]
pub struct NamespaceMutationBatch {
  kind: NamespaceMutationKind,
  dependencies: Vec<PlannedEntryWrite>,
  locator_mutations: Vec<PlannedLocatorMutation>,
  write_keys: HashSet<Vec<u8>>,
  source_identities: Vec<NamespaceMutationSourceIdentity>,
  head_hash: Option<Vec<u8>>,
}

impl NamespaceMutationBatch {
  pub fn new(kind: NamespaceMutationKind) -> Self {
    Self {
      kind,
      dependencies: Vec::new(),
      locator_mutations: Vec::new(),
      write_keys: HashSet::new(),
      source_identities: Vec::new(),
      head_hash: None,
    }
  }

  pub fn store_dependency(&mut self, entry_type: EntryType, key: Vec<u8>, value: Vec<u8>, flags: u8) -> EngineResult<()> {
    self.store_dependency_with_version(entry_type, key, value, flags, crate::engine::entry_header::CURRENT_ENTRY_VERSION)
  }

  pub fn store_dependency_with_version(
    &mut self,
    entry_type: EntryType,
    key: Vec<u8>,
    value: Vec<u8>,
    flags: u8,
    entry_version: u8,
  ) -> EngineResult<()> {
    self.reserve_write_key(&key, "dependency")?;
    self.dependencies.push(PlannedEntryWrite { entry_type, key, value, flags, entry_version });
    Ok(())
  }

  pub fn replace_locator(&mut self, entry_type: EntryType, key: Vec<u8>, value: Vec<u8>, flags: u8) -> EngineResult<()> {
    self.replace_locator_with_version(entry_type, key, value, flags, crate::engine::entry_header::CURRENT_ENTRY_VERSION)
  }

  pub fn replace_locator_with_version(
    &mut self,
    entry_type: EntryType,
    key: Vec<u8>,
    value: Vec<u8>,
    flags: u8,
    entry_version: u8,
  ) -> EngineResult<()> {
    validate_locator_entry_type(entry_type)?;
    self.reserve_write_key(&key, "stable locator")?;
    self.locator_mutations.push(PlannedLocatorMutation::Replace(PlannedEntryWrite { entry_type, key, value, flags, entry_version }));
    Ok(())
  }

  pub fn retire_locator(&mut self, key: Vec<u8>) -> EngineResult<()> {
    self.reserve_write_key(&key, "stable locator")?;
    self.locator_mutations.push(PlannedLocatorMutation::Retire { key });
    Ok(())
  }

  pub fn add_source_identity(&mut self, source: NamespaceMutationSourceIdentity) -> EngineResult<()> {
    if source.path.is_empty() {
      return Err(EngineError::InvalidInput("namespace mutation source path cannot be empty".to_string()));
    }
    if source.path.bytes().any(|byte| byte < 0x20 || byte == 0x7F) {
      return Err(EngineError::InvalidInput("namespace mutation source path contains control characters".to_string()));
    }
    if crate::engine::path_utils::normalize_path(&source.path) != source.path {
      return Err(EngineError::InvalidInput("namespace mutation source path must be canonical".to_string()));
    }
    if source.previous_identity.is_none() && source.new_identity.is_none() {
      return Err(EngineError::InvalidInput("namespace mutation source must include a previous or new identity".to_string()));
    }
    self.source_identities.push(source);
    Ok(())
  }

  pub fn set_head_hash(&mut self, head_hash: Vec<u8>) {
    self.head_hash = Some(head_hash);
  }

  fn reserve_write_key(&mut self, key: &[u8], role: &str) -> EngineResult<()> {
    if key.is_empty() {
      return Err(EngineError::InvalidInput(format!("namespace mutation {role} key cannot be empty")));
    }
    if !self.write_keys.insert(key.to_vec()) {
      return Err(EngineError::InvalidInput(format!("namespace mutation {role} key aliases another dependency or stable locator")));
    }
    Ok(())
  }

  fn validate(&self, engine: &StorageEngine) -> EngineResult<u64> {
    if self.locator_mutations.is_empty() && self.head_hash.is_none() {
      return Err(EngineError::InvalidInput("namespace mutation batch contains no namespace authority change".to_string()));
    }
    if self.source_identities.is_empty() {
      return Err(EngineError::InvalidInput("namespace mutation batch contains no source identities".to_string()));
    }
    let hash_length = engine.hash_algo().hash_length();
    let retired_locator_bytes = u64::try_from(hash_length)
      .ok()
      .and_then(|length| length.checked_add(14))
      .ok_or_else(|| EngineError::ResourceExhausted("namespace mutation durability estimate overflow".to_string()))?;
    let mut estimated_dependency_bytes = 0u64;
    for dependency in &self.dependencies {
      validate_hash_width(&dependency.key, hash_length, "dependency key")?;
      estimated_dependency_bytes = add_entry_estimate(estimated_dependency_bytes, engine, dependency)?;
    }
    for locator in &self.locator_mutations {
      validate_hash_width(locator.key(), hash_length, "stable locator key")?;
      estimated_dependency_bytes = match locator {
        PlannedLocatorMutation::Replace(write) => add_entry_estimate(estimated_dependency_bytes, engine, write)?,
        PlannedLocatorMutation::Retire { .. } => estimated_dependency_bytes
          .checked_add(retired_locator_bytes)
          .ok_or_else(|| EngineError::ResourceExhausted("namespace mutation durability estimate overflow".to_string()))?,
      };
    }
    if let Some(head_hash) = self.head_hash.as_ref() {
      validate_hash_width(head_hash, hash_length, "HEAD hash")?;
      self.validate_head_target(engine, head_hash)?;
    }
    for source in &self.source_identities {
      if let Some(entry_type) = source.entry_type {
        EntryType::from_u8(entry_type)?;
      }
      if let Some(identity) = source.previous_identity.as_ref() {
        validate_hash_width(identity, hash_length, "previous source identity")?;
      }
      if let Some(identity) = source.new_identity.as_ref() {
        validate_hash_width(identity, hash_length, "new source identity")?;
      }
    }
    Ok(estimated_dependency_bytes)
  }

  fn validate_head_target(&self, engine: &StorageEngine, head_hash: &[u8]) -> EngineResult<()> {
    if let Some(dependency) = self.dependencies.iter().find(|dependency| dependency.key == head_hash) {
      if dependency.entry_type != EntryType::DirectoryIndex {
        return Err(EngineError::InvalidInput("namespace mutation HEAD dependency must be a DirectoryIndex entry".to_string()));
      }
      return validate_namespace_root_value(
        engine,
        head_hash,
        "namespace mutation HEAD dependency",
        &dependency.value,
        dependency.entry_version,
      );
    }
    validate_existing_namespace_root(engine, head_hash, "namespace mutation HEAD")
  }
}

pub(crate) fn validate_existing_namespace_root(engine: &StorageEngine, root_hash: &[u8], role: &str) -> EngineResult<()> {
  validate_namespace_root_with(engine, root_hash, role, |reason| EngineError::CorruptEntry { offset: 0, reason })
}

fn validate_requested_namespace_root(engine: &StorageEngine, root_hash: &[u8], role: &str) -> EngineResult<()> {
  validate_namespace_root_with(engine, root_hash, role, EngineError::InvalidInput)
}

fn validate_namespace_root_with<F>(engine: &StorageEngine, root_hash: &[u8], role: &str, wrong_type: F) -> EngineResult<()>
where
  F: FnOnce(String) -> EngineError,
{
  validate_hash_width(root_hash, engine.hash_algo().hash_length(), role)?;
  let Some((header, stored_key, value)) = engine.get_entry_verified(root_hash)? else {
    return Err(EngineError::NotFound(format!("{role} root {}", hex::encode(root_hash))));
  };
  if header.entry_type != EntryType::DirectoryIndex || stored_key != root_hash {
    return Err(wrong_type(format!("{role} root {} is not a DirectoryIndex entry", hex::encode(root_hash))));
  }
  validate_namespace_root_value(engine, root_hash, role, &value, header.entry_version)
}

fn validate_namespace_root_value(
  engine: &StorageEngine,
  root_hash: &[u8],
  role: &str,
  value: &[u8],
  entry_version: u8,
) -> EngineResult<()> {
  let algorithm = engine.hash_algo();
  let hash_length = algorithm.hash_length();
  let canonical_hash = if !value.is_empty() && crate::engine::btree::is_btree_format(&value) {
    crate::engine::btree::BTreeNode::deserialize(value, hash_length, entry_version)
      .and_then(|node| node.content_hash(hash_length, &algorithm))
      .map_err(|error| EngineError::CorruptEntry {
        offset: 0,
        reason: format!("{role} root {} has malformed B-tree content: {error}", hex::encode(root_hash)),
      })?
  } else {
    if !value.is_empty() {
      crate::engine::directory_entry::deserialize_child_entries(value, hash_length, entry_version).map_err(|error| {
        EngineError::CorruptEntry {
          offset: 0,
          reason: format!("{role} root {} has malformed flat-directory content: {error}", hex::encode(root_hash)),
        }
      })?;
    }
    crate::engine::directory_ops::directory_content_hash(value, &algorithm)?
  };
  if canonical_hash != root_hash {
    return Err(EngineError::CorruptEntry {
      offset: 0,
      reason: format!(
        "{role} root {} is stored under a noncanonical key; expected {}",
        hex::encode(root_hash),
        hex::encode(canonical_hash)
      ),
    });
  }
  Ok(())
}

fn validate_locator_entry_type(entry_type: EntryType) -> EngineResult<()> {
  if matches!(entry_type, EntryType::Chunk | EntryType::Void) {
    return Err(EngineError::InvalidInput(format!(
      "{} entries are immutable payload or physical-GC state, not stable namespace locators",
      entry_type.to_u8()
    )));
  }
  Ok(())
}

fn add_entry_estimate(current: u64, engine: &StorageEngine, write: &PlannedEntryWrite) -> EngineResult<u64> {
  let entry_bytes = EntryHeader::compute_total_length(engine.hash_algo(), write.key.len(), write.value.len())?;
  current
    .checked_add(u64::from(entry_bytes))
    .ok_or_else(|| EngineError::ResourceExhausted("namespace mutation durability estimate overflow".to_string()))
}

fn validate_hash_width(value: &[u8], expected: usize, field: &str) -> EngineResult<()> {
  if value.len() != expected {
    return Err(EngineError::InvalidInput(format!("namespace mutation {field} must be exactly {expected} bytes")));
  }
  Ok(())
}

/// Applies stable-key replacements while namespace authority and its outer
/// transaction are held by [`NamespaceMutationCoordinator`].
pub struct LocatorReplacementCoordinator<'a> {
  engine: &'a StorageEngine,
  old_incarnations: Vec<(u32, Option<LocatorPhysicalIncarnation>)>,
}

impl<'a> LocatorReplacementCoordinator<'a> {
  fn preflight(engine: &'a StorageEngine, mutations: &[PlannedLocatorMutation]) -> EngineResult<Self> {
    let mut old_incarnations = Vec::with_capacity(mutations.len());
    for (index, mutation) in mutations.iter().enumerate() {
      let ordinal =
        u32::try_from(index).map_err(|_| EngineError::ResourceExhausted("namespace mutation locator count exceeds u32".to_string()))?;
      let old = engine.get_kv_entry(mutation.key())?.as_ref().map(LocatorPhysicalIncarnation::from);
      if old.as_ref().is_some_and(|incarnation| {
        matches!(incarnation.type_flags & 0x0F, crate::engine::kv_store::KV_TYPE_CHUNK | crate::engine::kv_store::KV_TYPE_VOID)
      }) {
        return Err(EngineError::InvalidInput(
          "immutable payload and physical-GC entries cannot be replaced or retired as namespace locators".to_string(),
        ));
      }
      if matches!(mutation, PlannedLocatorMutation::Retire { .. }) && old.is_none() {
        return Err(EngineError::NotFound(format!("stable locator {}", hex::encode(mutation.key()))));
      }
      old_incarnations.push((ordinal, old));
    }
    Ok(Self { engine, old_incarnations })
  }

  fn apply(self, mutations: &[PlannedLocatorMutation]) -> EngineResult<Vec<LocatorReplacement>> {
    let mut replacements = Vec::with_capacity(mutations.len());
    for (index, mutation) in mutations.iter().enumerate() {
      let (ordinal, old_incarnation) = self.old_incarnations[index].clone();
      let new_incarnation = match mutation {
        PlannedLocatorMutation::Replace(write) => {
          let offset = store_planned_entry(self.engine, write)?;
          let current = self.engine.get_kv_entry(&write.key)?.ok_or_else(|| EngineError::CorruptEntry {
            offset,
            reason: "stable locator write did not publish a KV incarnation".to_string(),
          })?;
          if current.offset != offset || current.entry_type() != write.entry_type.to_kv_type() {
            return Err(EngineError::CorruptEntry {
              offset,
              reason: "stable locator KV incarnation disagrees with the appended entry".to_string(),
            });
          }
          Some(LocatorPhysicalIncarnation::from(&current))
        }
        PlannedLocatorMutation::Retire { key } => {
          self.engine.mark_entry_deleted(key)?;
          if self.engine.get_kv_entry(key)?.is_some() {
            return Err(EngineError::CorruptEntry {
              offset: old_incarnation.as_ref().map(|incarnation| incarnation.offset).unwrap_or_default(),
              reason: "retired stable locator remains live in the KV authority".to_string(),
            });
          }
          None
        }
      };
      replacements.push(LocatorReplacement { ordinal, stable_key: mutation.key().to_vec(), old_incarnation, new_incarnation });
    }
    Ok(replacements)
  }
}

fn store_planned_entry(engine: &StorageEngine, write: &PlannedEntryWrite) -> EngineResult<u64> {
  if write.flags == 0 {
    engine.store_entry_with_version(write.entry_type, &write.key, &write.value, write.entry_version)
  } else {
    engine.store_entry_with_flags_and_version(write.entry_type, &write.key, &write.value, write.flags, write.entry_version)
  }
}

/// Shared hard-authority owner for prepared namespace mutations.
pub struct NamespaceMutationCoordinator<'a> {
  engine: &'a StorageEngine,
  fanout: Arc<dyn NamespaceMutationFanout + 'a>,
  #[cfg(test)]
  test_faults: NamespaceMutationTestFaults,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
struct NamespaceMutationTestFaults {
  fail_after_dependency_writes: Option<usize>,
  fail_hard_before_commit: bool,
}

impl<'a> NamespaceMutationCoordinator<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    Self {
      engine,
      fanout: Arc::new(NoopNamespaceMutationFanout),
      #[cfg(test)]
      test_faults: NamespaceMutationTestFaults::default(),
    }
  }

  pub fn with_fanout<F>(engine: &'a StorageEngine, fanout: Arc<F>) -> Self
  where
    F: NamespaceMutationFanout + 'a,
  {
    Self {
      engine,
      fanout,
      #[cfg(test)]
      test_faults: NamespaceMutationTestFaults::default(),
    }
  }

  #[cfg(test)]
  fn with_test_faults<F>(engine: &'a StorageEngine, fanout: Arc<F>, test_faults: NamespaceMutationTestFaults) -> Self
  where
    F: NamespaceMutationFanout + 'a,
  {
    Self { engine, fanout, test_faults }
  }

  pub fn execute(&self, batch: NamespaceMutationBatch) -> EngineResult<NamespaceMutationAcknowledgement> {
    let namespace = self.engine.namespace_write_guard()?;
    let estimated_dependency_bytes = batch.validate(self.engine)?;
    self.execute_with_namespace(batch, estimated_dependency_bytes, namespace)
  }

  /// Build a mutation plan while holding the same namespace authority that
  /// will publish it. The preparation closure may read current authority but
  /// must not mutate storage; all writes belong in the returned batch.
  pub fn prepare_and_execute<T, F>(&self, prepare: F) -> EngineResult<(NamespaceMutationAcknowledgement, T)>
  where
    F: FnOnce(&StorageEngine) -> EngineResult<(NamespaceMutationBatch, T)>,
  {
    let (acknowledgement, output) = self.prepare_and_maybe_execute(|planning_engine| {
      let (batch, output) = prepare(planning_engine)?;
      Ok((Some(batch), output))
    })?;
    let acknowledgement =
      acknowledgement.ok_or_else(|| EngineError::InvalidInput("required namespace mutation preparation returned no batch".to_string()))?;
    Ok((acknowledgement, output))
  }

  /// Prepare a conditional mutation under namespace authority. A `None` batch
  /// is a proven no-op: it consumes no durability ticket and emits no fanout.
  pub fn prepare_and_maybe_execute<T, F>(&self, prepare: F) -> EngineResult<(Option<NamespaceMutationAcknowledgement>, T)>
  where
    F: FnOnce(&StorageEngine) -> EngineResult<(Option<NamespaceMutationBatch>, T)>,
  {
    let namespace = self.engine.namespace_write_guard()?;
    let (batch, output) = prepare(self.engine)?;
    let Some(batch) = batch else {
      return Ok((None, output));
    };
    let estimated_dependency_bytes = batch.validate(self.engine)?;
    let acknowledgement = self.execute_with_namespace(batch, estimated_dependency_bytes, namespace)?;
    Ok((Some(acknowledgement), output))
  }

  fn execute_with_namespace(
    &self,
    batch: NamespaceMutationBatch,
    estimated_dependency_bytes: u64,
    namespace: crate::engine::storage_engine::NamespaceWriteGuard<'a>,
  ) -> EngineResult<NamespaceMutationAcknowledgement> {
    let previous_root_hash = self.engine.head_hash()?;
    if batch.dependencies.is_empty()
      && batch.locator_mutations.is_empty()
      && batch.head_hash.as_deref() == Some(previous_root_hash.as_slice())
    {
      return Err(EngineError::InvalidInput("namespace mutation HEAD already selects the requested root".to_string()));
    }
    let locator_coordinator = LocatorReplacementCoordinator::preflight(self.engine, &batch.locator_mutations)?;
    let transaction = TransactionGuard::new_top_level(self.engine, estimated_dependency_bytes)?;

    let mut mutation_may_have_started = false;
    let prepared = (|| -> EngineResult<NamespaceMutationAcknowledgement> {
      #[cfg(test)]
      let mut dependency_writes = 0usize;
      for dependency in &batch.dependencies {
        store_planned_entry(self.engine, dependency)?;
        mutation_may_have_started = true;
        #[cfg(test)]
        {
          dependency_writes += 1;
          if self.test_faults.fail_after_dependency_writes == Some(dependency_writes) {
            return Err(EngineError::InvalidInput("injected failure after namespace dependency append".to_string()));
          }
        }
      }
      if !batch.locator_mutations.is_empty() {
        mutation_may_have_started = true;
      }
      let locator_replacements = locator_coordinator.apply(&batch.locator_mutations)?;
      let root_hash = match batch.head_hash.as_ref() {
        Some(head_hash) if head_hash != &previous_root_hash => {
          mutation_may_have_started = true;
          self.engine.update_head(head_hash)?;
          head_hash.clone()
        }
        Some(_) => previous_root_hash.clone(),
        None => previous_root_hash.clone(),
      };
      Ok(NamespaceMutationAcknowledgement {
        operation_id: Uuid::new_v4(),
        kind: batch.kind,
        publication_sequence: 0,
        previous_root_hash,
        root_hash,
        source_identities: batch.source_identities,
        locator_replacements,
      })
    })();

    let mut acknowledgement = match prepared {
      Ok(acknowledgement) => acknowledgement,
      Err(error) => {
        let error = if mutation_may_have_started && !matches!(error, EngineError::PostMutationDurabilityFailure(_)) {
          EngineError::PostMutationDurabilityFailure(format!("namespace mutation failed after storage mutation may have begun: {error}"))
        } else {
          error
        };
        let completion = transaction.finish_after::<()>(Err(error), namespace);
        return match completion {
          Err(error) => Err(error),
          Ok(()) => Err(EngineError::DurabilityFailure("failed namespace mutation unexpectedly completed without an error".to_string())),
        };
      }
    };

    #[cfg(test)]
    if self.test_faults.fail_hard_before_commit {
      if let Err(error) = transaction.fail_admitted_hard_for_test() {
        let completion = transaction.finish_after::<NamespaceMutationAcknowledgement>(Err(error), namespace);
        return match completion {
          Err(error) => Err(error),
          Ok(_) => {
            Err(EngineError::DurabilityFailure("failed hard-publication injection unexpectedly completed without an error".to_string()))
          }
        };
      }
    }
    let receipt = transaction.commit_top_level_after(namespace)?;
    acknowledgement.publication_sequence = receipt.sequence;

    if catch_unwind(AssertUnwindSafe(|| {
      metrics::counter!(
        "aeordb_namespace_mutation_acknowledgements_total",
        "mutation_kind" => acknowledgement.kind.as_str()
      )
      .increment(1);
      self.fanout.publish(&acknowledgement);
    }))
    .is_err()
    {
      tracing::error!(operation_id = %acknowledgement.operation_id, "Post-commit namespace mutation fanout panicked");
    }

    Ok(acknowledgement)
  }
}

/// Publish a validated whole-namespace root through the shared hard authority.
///
/// The transition is constant-memory: the selected root is the reconciliation
/// boundary, so callers must stage its immutable closure before invoking this
/// function rather than expanding every descendant into one transaction.
pub fn publish_namespace_root(
  engine: &StorageEngine,
  root_hash: &[u8],
  kind: NamespaceMutationKind,
) -> EngineResult<Option<NamespaceMutationAcknowledgement>> {
  publish_namespace_root_checked(engine, None, root_hash, kind, Arc::new(RootPublicationFanout { engine }))
}

/// Publish a namespace root only if HEAD still matches the caller's captured
/// root. Long-running import/sync producers use this to avoid overwriting an
/// acknowledged mutation that completed while immutable dependencies staged.
pub fn publish_namespace_root_from(
  engine: &StorageEngine,
  expected_root_hash: &[u8],
  root_hash: &[u8],
  kind: NamespaceMutationKind,
) -> EngineResult<Option<NamespaceMutationAcknowledgement>> {
  publish_namespace_root_checked(engine, Some(expected_root_hash), root_hash, kind, Arc::new(RootPublicationFanout { engine }))
}

pub fn publish_namespace_root_with_fanout<'a, F>(
  engine: &'a StorageEngine,
  root_hash: &[u8],
  kind: NamespaceMutationKind,
  fanout: Arc<F>,
) -> EngineResult<Option<NamespaceMutationAcknowledgement>>
where
  F: NamespaceMutationFanout + 'a,
{
  publish_namespace_root_checked(engine, None, root_hash, kind, fanout)
}

pub fn publish_namespace_root_from_with_fanout<'a, F>(
  engine: &'a StorageEngine,
  expected_root_hash: &[u8],
  root_hash: &[u8],
  kind: NamespaceMutationKind,
  fanout: Arc<F>,
) -> EngineResult<Option<NamespaceMutationAcknowledgement>>
where
  F: NamespaceMutationFanout + 'a,
{
  publish_namespace_root_checked(engine, Some(expected_root_hash), root_hash, kind, fanout)
}

fn publish_namespace_root_checked<'a, F>(
  engine: &'a StorageEngine,
  expected_root_hash: Option<&[u8]>,
  root_hash: &[u8],
  kind: NamespaceMutationKind,
  fanout: Arc<F>,
) -> EngineResult<Option<NamespaceMutationAcknowledgement>>
where
  F: NamespaceMutationFanout + 'a,
{
  let requested_root = root_hash.to_vec();
  let expected_root = expected_root_hash.map(<[u8]>::to_vec);
  NamespaceMutationCoordinator::with_fanout(engine, fanout)
    .prepare_and_maybe_execute(|planning_engine| {
      validate_requested_namespace_root(planning_engine, &requested_root, "requested namespace")?;
      let previous_root = planning_engine.head_hash()?;
      if expected_root.as_ref().is_some_and(|expected| expected != &previous_root) {
        return Err(EngineError::AlreadyExists(format!(
          "namespace HEAD changed from {} to {} while the transition was staging",
          hex::encode(expected_root.as_ref().expect("checked above")),
          hex::encode(&previous_root)
        )));
      }
      if previous_root == requested_root {
        return Ok((None, ()));
      }

      let mut batch = NamespaceMutationBatch::new(kind);
      batch.set_head_hash(requested_root.clone());
      batch.add_source_identity(NamespaceMutationSourceIdentity {
        path: "/".to_string(),
        entry_type: Some(EntryType::DirectoryIndex.to_u8()),
        previous_identity: Some(previous_root),
        new_identity: Some(requested_root.clone()),
      })?;
      Ok((Some(batch), ()))
    })
    .map(|(acknowledgement, ())| acknowledgement)
}

#[cfg(test)]
#[path = "../../spec/engine/namespace_mutation_internal_spec.rs"]
mod namespace_mutation_internal_spec;
