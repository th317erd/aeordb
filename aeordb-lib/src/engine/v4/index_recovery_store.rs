//! Native persistence and bounded adapter caching for v4 index recovery.
//!
//! Immutable artifacts and selected checkpoints remain owned by first
//! authority. The registry caches only scope-local adapter objects; every
//! selected-root observation is reloaded from the A/B control on disk.

use std::collections::BTreeMap;
use std::mem::size_of;
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::{HashAlgorithm, VirtualClock};

use super::control_enums::{RetryClassV1, StableReasonV1};
use super::first_authority::{
  IndexArtifactBatchPublicationRequestV1, IndexOperationControlExpectationV1, IndexOperationControlPublicationRequestV1,
  V4FirstAuthorityPublisher,
};
use super::gc_retirement::RetirementJournalOwnerV1;
use super::index_artifact::EncodedImmutableIndexArtifactV1;
use super::index_coordinator_recovery::{
  IndexCheckpointRootV1, IndexRecoveryOptionsV1, IndexRecoveryOwnerV1, IndexRecoveryStoreErrorV1, IndexRecoveryStoreV1,
};
use super::index_operation_control::{
  IndexOperationControlWriteV1, IndexOperationKindV1, IndexOperationStateV1, decode_index_operation_control, encode_index_operation_control,
};
use super::index_scope_ordinal_checkpoint_store::RecoveryIndexScopeOrdinalStateStoreV1;
use super::index_task::{IndexTaskCheckpointV1, IndexTaskStateV1, decode_index_task_checkpoint};

pub type SharedRetirementJournalOwnerV1 = Arc<Mutex<RetirementJournalOwnerV1>>;
pub type NativeScopeOrdinalStateStoreV1 = RecoveryIndexScopeOrdinalStateStoreV1<NativeIndexRecoveryStoreV1>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeIndexOperationDescriptorV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  index_id: Vec<u8>,
  operation_id: [u8; 16],
  operation_kind: IndexOperationKindV1,
  definition_id: Vec<u8>,
  base_manifest: Option<Vec<u8>>,
  target_manifest: Option<Vec<u8>>,
}

impl NativeIndexOperationDescriptorV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    hash_algorithm: HashAlgorithm,
    database_id: [u8; 16],
    index_id: Vec<u8>,
    operation_id: [u8; 16],
    operation_kind: IndexOperationKindV1,
    definition_id: Vec<u8>,
    base_manifest: Option<Vec<u8>>,
    target_manifest: Option<Vec<u8>>,
  ) -> Result<Self, IndexRecoveryStoreErrorV1> {
    let hash_width = hash_algorithm.hash_length();
    if database_id.iter().all(|byte| *byte == 0) || operation_id.iter().all(|byte| *byte == 0) {
      return Err(store_error("native_index_operation_identity", "database and operation identities must be nonzero"));
    }
    require_hash(&index_id, hash_width, "index identity")?;
    require_hash(&definition_id, hash_width, "definition identity")?;
    require_optional_hash(base_manifest.as_deref(), hash_width, "base manifest")?;
    require_optional_hash(target_manifest.as_deref(), hash_width, "target manifest")?;
    Ok(Self { hash_algorithm, database_id, index_id, operation_id, operation_kind, definition_id, base_manifest, target_manifest })
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub fn index_id(&self) -> &[u8] {
    &self.index_id
  }

  pub const fn operation_id(&self) -> [u8; 16] {
    self.operation_id
  }

  pub fn retained_identity_bytes(&self) -> Result<u64, IndexRecoveryStoreErrorV1> {
    let fixed = u64::try_from(size_of::<Self>())
      .map_err(|error| store_error("native_index_descriptor_size", format!("descriptor fixed size exceeds u64: {error}")))?;
    fixed
      .checked_add(self.variable_bytes()?)
      .ok_or_else(|| store_error("native_index_descriptor_size", "operation descriptor retained size overflowed"))
  }

  fn variable_bytes(&self) -> Result<u64, IndexRecoveryStoreErrorV1> {
    let variable = self
      .index_id
      .len()
      .checked_add(self.definition_id.len())
      .and_then(|bytes| bytes.checked_add(self.base_manifest.as_ref().map_or(0, Vec::len)))
      .and_then(|bytes| bytes.checked_add(self.target_manifest.as_ref().map_or(0, Vec::len)))
      .ok_or_else(|| store_error("index_registry_entry_size", "operation descriptor size overflowed"))?;
    u64::try_from(variable)
      .map_err(|error| store_error("index_registry_entry_size", format!("operation descriptor size exceeds u64: {error}")))
  }
}

pub struct NativeIndexRecoveryStoreV1 {
  descriptor: NativeIndexOperationDescriptorV1,
  destination_physical_instance_id: [u8; 16],
  publisher: Arc<V4FirstAuthorityPublisher>,
  retirement_owner: SharedRetirementJournalOwnerV1,
  clock: Arc<dyn VirtualClock>,
}

impl NativeIndexRecoveryStoreV1 {
  pub fn new(
    descriptor: NativeIndexOperationDescriptorV1,
    publisher: Arc<V4FirstAuthorityPublisher>,
    retirement_owner: SharedRetirementJournalOwnerV1,
    clock: Arc<dyn VirtualClock>,
  ) -> Result<Self, IndexRecoveryStoreErrorV1> {
    let observation = publisher.observe().map_err(authority_error)?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.hash_algorithm != descriptor.hash_algorithm
      || header.database_id != descriptor.database_id
      || header.physical_instance_id.iter().all(|byte| *byte == 0)
    {
      return Err(store_error(
        "native_index_recovery_authority",
        "native recovery store descriptor does not match selected non-degraded first authority",
      ));
    }
    {
      let owner = retirement_owner
        .lock()
        .map_err(|error| store_error("native_index_retirement_poisoned", format!("retirement owner lock was poisoned: {error}")))?;
      if owner.hash_algorithm() != descriptor.hash_algorithm || owner.database_id() != descriptor.database_id {
        return Err(store_error(
          "native_index_retirement_authority",
          "retirement owner does not match the native recovery database authority",
        ));
      }
    }
    if clock.now_ms() == 0 {
      return Err(store_error("native_index_clock", "native recovery clock must return a nonzero timestamp"));
    }
    Ok(Self { descriptor, destination_physical_instance_id: header.physical_instance_id, publisher, retirement_owner, clock })
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.descriptor.hash_algorithm
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.descriptor.database_id
  }

  pub const fn destination_physical_instance_id(&self) -> [u8; 16] {
    self.destination_physical_instance_id
  }

  fn validate_owner(&self, owner: &IndexRecoveryOwnerV1) -> Result<(), IndexRecoveryStoreErrorV1> {
    if owner.database_id() != self.descriptor.database_id
      || owner.index_id() != self.descriptor.index_id
      || owner.operation_id() != self.descriptor.operation_id
    {
      return Err(store_error("native_index_recovery_owner", "recovery owner does not match this native operation store"));
    }
    Ok(())
  }

  fn publish_artifacts(&self, artifacts: &[&EncodedImmutableIndexArtifactV1]) -> Result<(), IndexRecoveryStoreErrorV1> {
    if artifacts.is_empty() {
      return Ok(());
    }
    let timestamp = self.clock.now_ms();
    if timestamp == 0 {
      return Err(store_error("native_index_clock", "native recovery clock returned zero during artifact publication"));
    }
    self
      .publisher
      .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
        database_id: &self.descriptor.database_id,
        artifacts,
        publication_timestamp_ms: timestamp,
      })
      .map(|_| ())
      .map_err(authority_error)
  }

  fn selected_control(&self) -> Result<Option<IndexCheckpointRootV1>, IndexRecoveryStoreErrorV1> {
    let loaded = self
      .publisher
      .load_index_operation_control(&self.descriptor.database_id, &self.descriptor.index_id, &self.descriptor.operation_id)
      .map_err(authority_error)?;
    let Some(loaded) = loaded else {
      return Ok(None);
    };
    let control = decode_index_operation_control(&loaded.bytes, self.descriptor.hash_algorithm).map_err(format_error)?;
    if control.control_sequence != loaded.control_sequence
      || control.database_id != self.descriptor.database_id
      || control.index_id != self.descriptor.index_id
      || control.operation_id != self.descriptor.operation_id
      || control.operation_kind != self.descriptor.operation_kind
      || control.definition_id != self.descriptor.definition_id
      || control.base_manifest != self.descriptor.base_manifest.as_deref()
      || control.target_manifest != self.descriptor.target_manifest.as_deref()
      || control.checkpoint_artifact != Some(loaded.checkpoint_artifact.as_slice())
    {
      return Err(store_error(
        "native_index_control_descriptor",
        "selected index-operation control disagrees with its native recovery descriptor",
      ));
    }
    IndexCheckpointRootV1::new(control.control_sequence, loaded.checkpoint_artifact).map(Some).map_err(recovery_error)
  }

  fn load_checkpoint(&self, next: &IndexCheckpointRootV1) -> Result<Vec<u8>, IndexRecoveryStoreErrorV1> {
    let length = self
      .publisher
      .index_artifact_length(&next.checkpoint_key)
      .map_err(authority_error)?
      .ok_or_else(|| store_error("native_index_checkpoint_missing", "next selected checkpoint artifact is absent"))?;
    self
      .publisher
      .load_index_artifact(&next.checkpoint_key, length)
      .map_err(authority_error)?
      .ok_or_else(|| store_error("native_index_checkpoint_changed", "next selected checkpoint changed after its length probe"))
  }

  fn encode_selected_control(
    &self,
    next: &IndexCheckpointRootV1,
    checkpoint: &IndexTaskCheckpointV1<'_>,
  ) -> Result<Vec<u8>, IndexRecoveryStoreErrorV1> {
    if checkpoint.key != next.checkpoint_key
      || checkpoint.checkpoint_sequence != next.checkpoint_sequence
      || checkpoint.task_id != self.descriptor.operation_id
      || checkpoint.primary_id != self.descriptor.index_id
      || checkpoint.source_root.iter().all(|byte| *byte == 0)
    {
      return Err(store_error(
        "native_index_checkpoint_descriptor",
        "next checkpoint identity, operation, scope, or source root disagrees with the native recovery descriptor",
      ));
    }
    let created_at_ms = i64::try_from(checkpoint.started_at_ms)
      .map_err(|error| store_error("native_index_checkpoint_time", format!("checkpoint start time exceeds i64: {error}")))?;
    let updated_at_ms = i64::try_from(checkpoint.updated_at_ms)
      .map_err(|error| store_error("native_index_checkpoint_time", format!("checkpoint update time exceeds i64: {error}")))?;
    if checkpoint.completed_work > checkpoint.total_work_hint {
      return Err(store_error(
        "native_index_checkpoint_progress",
        "checkpoint completed work exceeds the operation control total-work bound",
      ));
    }
    let state = operation_state(checkpoint.state);
    encode_index_operation_control(
      next.checkpoint_sequence,
      &IndexOperationControlWriteV1 {
        database_id: self.descriptor.database_id,
        index_id: &self.descriptor.index_id,
        operation_id: self.descriptor.operation_id,
        operation_kind: self.descriptor.operation_kind,
        state,
        created_at_ms,
        updated_at_ms,
        requested_namespace_root: checkpoint.source_root,
        definition_id: &self.descriptor.definition_id,
        base_manifest: self.descriptor.base_manifest.as_deref(),
        target_manifest: self.descriptor.target_manifest.as_deref(),
        checkpoint_artifact: Some(&next.checkpoint_key),
        captured_runtime_sequence: checkpoint.journal_floor_sequence,
        reconciled_through_sequence: checkpoint.journal_audited_through,
        completed_work: checkpoint.completed_work,
        total_work_hint: checkpoint.total_work_hint,
        stable_reason: StableReasonV1::NoneOrSuccess,
        retry_class: RetryClassV1::None,
        error_evidence_hash: None,
      },
      self.descriptor.hash_algorithm,
    )
    .map_err(format_error)
  }
}

impl IndexRecoveryStoreV1 for NativeIndexRecoveryStoreV1 {
  fn immutable_length(&mut self, key: &[u8]) -> Result<Option<u64>, IndexRecoveryStoreErrorV1> {
    self.publisher.index_artifact_length(key).map_err(authority_error)
  }

  fn load_immutable(&mut self, key: &[u8], expected_length: u64) -> Result<Option<Vec<u8>>, IndexRecoveryStoreErrorV1> {
    self.publisher.load_index_artifact(key, expected_length).map_err(authority_error)
  }

  fn put_immutable(&mut self, artifact: &EncodedImmutableIndexArtifactV1) -> Result<(), IndexRecoveryStoreErrorV1> {
    self.publish_artifacts(&[artifact])
  }

  fn put_immutable_batch(&mut self, artifacts: &[&EncodedImmutableIndexArtifactV1]) -> Result<(), IndexRecoveryStoreErrorV1> {
    self.publish_artifacts(artifacts)
  }

  fn sync_immutable(&mut self) -> Result<(), IndexRecoveryStoreErrorV1> {
    // Native artifact publication is already a hard first-authority boundary.
    Ok(())
  }

  fn load_selected(&mut self, owner: &IndexRecoveryOwnerV1) -> Result<Option<IndexCheckpointRootV1>, IndexRecoveryStoreErrorV1> {
    self.validate_owner(owner)?;
    self.selected_control()
  }

  fn publish_selected_synced(
    &mut self,
    owner: &IndexRecoveryOwnerV1,
    expected: Option<&IndexCheckpointRootV1>,
    next: &IndexCheckpointRootV1,
  ) -> Result<(), IndexRecoveryStoreErrorV1> {
    self.validate_owner(owner)?;
    let checkpoint_bytes = self.load_checkpoint(next)?;
    let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, self.descriptor.hash_algorithm).map_err(format_error)?;
    let encoded_control = self.encode_selected_control(next, &checkpoint)?;
    let expectation = expected.map(|expected| IndexOperationControlExpectationV1 {
      control_sequence: expected.checkpoint_sequence,
      checkpoint_artifact: &expected.checkpoint_key,
    });
    let timestamp = self.clock.now_ms();
    if timestamp == 0 {
      return Err(store_error("native_index_clock", "native recovery clock returned zero during selector publication"));
    }
    let mut retirement_owner = self
      .retirement_owner
      .lock()
      .map_err(|error| store_error("native_index_retirement_poisoned", format!("retirement owner lock was poisoned: {error}")))?;
    let receipt = self
      .publisher
      .publish_index_operation_control(
        IndexOperationControlPublicationRequestV1 {
          database_id: &self.descriptor.database_id,
          index_id: &self.descriptor.index_id,
          operation_id: &self.descriptor.operation_id,
          expected: expectation,
          encoded_control: &encoded_control,
          publication_timestamp_ms: timestamp,
          monotonic_now_ms: timestamp,
        },
        &mut retirement_owner,
      )
      .map_err(|error| store_error(error.code(), error.to_string()))?;
    if receipt.control_sequence != next.checkpoint_sequence || receipt.checkpoint_artifact != next.checkpoint_key {
      return Err(store_error(
        "native_index_selector_receipt",
        "index-operation publication receipt disagrees with the requested selected checkpoint",
      ));
    }
    Ok(())
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexScopeOrdinalStoreRegistryOptionsV1 {
  pub maximum_entries: usize,
  pub maximum_resident_bytes: u64,
}

impl IndexScopeOrdinalStoreRegistryOptionsV1 {
  pub fn new(maximum_entries: usize, maximum_resident_bytes: u64) -> Result<Self, IndexScopeOrdinalStoreRegistryErrorV1> {
    if maximum_entries == 0 || maximum_resident_bytes == 0 {
      return Err(IndexScopeOrdinalStoreRegistryErrorV1::Invalid("registry count and byte limits must be nonzero"));
    }
    Ok(Self { maximum_entries, maximum_resident_bytes })
  }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexScopeOrdinalStoreRegistrySnapshotV1 {
  pub entries: usize,
  pub resident_bytes: u64,
  pub pinned_entries: usize,
  pub hits: u64,
  pub misses: u64,
  pub evictions: u64,
}

#[derive(Debug, Error)]
pub enum IndexScopeOrdinalStoreRegistryErrorV1 {
  #[error("invalid index scope-store registry options: {0}")]
  Invalid(&'static str),
  #[error("index scope-store registry lock is poisoned: {0}")]
  Poisoned(String),
  #[error("index scope-store registry is canceled and no longer accepts new adapters")]
  Canceled,
  #[error("index scope-store registry is full and every eviction candidate is pinned")]
  AllCandidatesPinned,
  #[error("index scope-store operation descriptor conflicts with an existing identity")]
  DescriptorConflict,
  #[error("index scope-store registry arithmetic overflowed")]
  ArithmeticOverflow,
  #[error("index scope-store registry arithmetic conversion failed: {0}")]
  ArithmeticConversion(String),
  #[error("index scope-store registry memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error(transparent)]
  Store(#[from] IndexRecoveryStoreErrorV1),
  #[error("index scope-store adapter construction failed: {0}")]
  Adapter(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RegistryKeyV1 {
  index_id: Vec<u8>,
  operation_id: [u8; 16],
}

struct RegistryEntryV1 {
  descriptor: NativeIndexOperationDescriptorV1,
  adapter: Arc<NativeScopeOrdinalStateStoreV1>,
  last_access_sequence: u64,
  charged_bytes: u64,
  _reservation: MemoryReservation,
}

#[derive(Default)]
struct RegistryStateV1 {
  entries: BTreeMap<RegistryKeyV1, RegistryEntryV1>,
  resident_bytes: u64,
  access_sequence: u64,
  hits: u64,
  misses: u64,
  evictions: u64,
}

pub struct IndexScopeOrdinalStoreRegistryV1 {
  options: IndexScopeOrdinalStoreRegistryOptionsV1,
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  recovery_options: IndexRecoveryOptionsV1,
  publisher: Arc<V4FirstAuthorityPublisher>,
  retirement_owner: SharedRetirementJournalOwnerV1,
  memory: Arc<MemoryCoordinator>,
  cancellation: CancellationToken,
  clock: Arc<dyn VirtualClock>,
  state: Mutex<RegistryStateV1>,
}

impl IndexScopeOrdinalStoreRegistryV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    options: IndexScopeOrdinalStoreRegistryOptionsV1,
    hash_algorithm: HashAlgorithm,
    database_id: [u8; 16],
    recovery_options: IndexRecoveryOptionsV1,
    publisher: Arc<V4FirstAuthorityPublisher>,
    retirement_owner: SharedRetirementJournalOwnerV1,
    memory: Arc<MemoryCoordinator>,
    cancellation: CancellationToken,
    clock: Arc<dyn VirtualClock>,
  ) -> Result<Self, IndexScopeOrdinalStoreRegistryErrorV1> {
    if database_id.iter().all(|byte| *byte == 0) {
      return Err(IndexScopeOrdinalStoreRegistryErrorV1::Invalid("registry database identity must be nonzero"));
    }
    if cancellation.is_cancelled() {
      return Err(IndexScopeOrdinalStoreRegistryErrorV1::Canceled);
    }
    let observation = publisher.observe().map_err(authority_error)?;
    if observation.selected.redundancy_degraded
      || observation.selected.header.hash_algorithm != hash_algorithm
      || observation.selected.header.database_id != database_id
    {
      return Err(IndexScopeOrdinalStoreRegistryErrorV1::Invalid("registry does not match selected non-degraded first authority"));
    }
    Ok(Self {
      options,
      hash_algorithm,
      database_id,
      recovery_options,
      publisher,
      retirement_owner,
      memory,
      cancellation,
      clock,
      state: Mutex::new(RegistryStateV1::default()),
    })
  }

  pub fn acquire(
    &self,
    descriptor: NativeIndexOperationDescriptorV1,
  ) -> Result<Arc<NativeScopeOrdinalStateStoreV1>, IndexScopeOrdinalStoreRegistryErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(IndexScopeOrdinalStoreRegistryErrorV1::Canceled);
    }
    if descriptor.hash_algorithm != self.hash_algorithm || descriptor.database_id != self.database_id {
      return Err(IndexScopeOrdinalStoreRegistryErrorV1::Invalid("operation descriptor belongs to another registry authority"));
    }
    let key = RegistryKeyV1 { index_id: descriptor.index_id.clone(), operation_id: descriptor.operation_id };
    {
      let mut state = self.lock_state()?;
      if let Some(adapter) = reuse_registry_adapter(&mut state, &key, &descriptor)? {
        return Ok(adapter);
      }
    }
    let owner = IndexRecoveryOwnerV1::new(descriptor.database_id, descriptor.index_id.clone(), descriptor.operation_id)
      .map_err(|error| IndexScopeOrdinalStoreRegistryErrorV1::Adapter(error.to_string()))?;
    let native = NativeIndexRecoveryStoreV1::new(
      descriptor.clone(),
      Arc::clone(&self.publisher),
      Arc::clone(&self.retirement_owner),
      Arc::clone(&self.clock),
    )?;
    let adapter = Arc::new(
      RecoveryIndexScopeOrdinalStateStoreV1::new(
        self.hash_algorithm,
        owner,
        self.recovery_options,
        Arc::clone(&self.memory),
        self.cancellation.clone(),
        Arc::clone(&self.clock),
        native,
      )
      .map_err(|error| IndexScopeOrdinalStoreRegistryErrorV1::Adapter(error.to_string()))?,
    );
    let charged_bytes = registry_entry_charge(&descriptor, &key)?;
    if charged_bytes > self.options.maximum_resident_bytes {
      return Err(IndexScopeOrdinalStoreRegistryErrorV1::Invalid("one registry entry exceeds the configured byte bound"));
    }

    let mut state = self.lock_state()?;
    if self.cancellation.is_cancelled() {
      return Err(IndexScopeOrdinalStoreRegistryErrorV1::Canceled);
    }
    if let Some(adapter) = reuse_registry_adapter(&mut state, &key, &descriptor)? {
      return Ok(adapter);
    }
    state.access_sequence = state.access_sequence.checked_add(1).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
    let access_sequence = state.access_sequence;
    state.misses = state.misses.checked_add(1).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
    self.evict_for(&mut state, charged_bytes)?;
    let reservation = loop {
      match self.memory.reserve(MemoryOwner::IndexCleanCache, charged_bytes, AdmissionClass::Cache) {
        Ok(reservation) => break reservation,
        Err(error) => {
          if !evict_one_unpinned(&mut state)? {
            return Err(error.into());
          }
        }
      }
    };
    state.resident_bytes =
      state.resident_bytes.checked_add(charged_bytes).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
    state.entries.insert(
      key,
      RegistryEntryV1 {
        descriptor,
        adapter: Arc::clone(&adapter),
        last_access_sequence: access_sequence,
        charged_bytes,
        _reservation: reservation,
      },
    );
    Ok(adapter)
  }

  pub fn evict_all_unpinned(&self) -> Result<u64, IndexScopeOrdinalStoreRegistryErrorV1> {
    let mut state = self.lock_state()?;
    let before = state.evictions;
    while evict_one_unpinned(&mut state)? {}
    state.evictions.checked_sub(before).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)
  }

  pub fn snapshot(&self) -> Result<IndexScopeOrdinalStoreRegistrySnapshotV1, IndexScopeOrdinalStoreRegistryErrorV1> {
    let state = self.lock_state()?;
    Ok(IndexScopeOrdinalStoreRegistrySnapshotV1 {
      entries: state.entries.len(),
      resident_bytes: state.resident_bytes,
      pinned_entries: state.entries.values().filter(|entry| Arc::strong_count(&entry.adapter) > 1).count(),
      hits: state.hits,
      misses: state.misses,
      evictions: state.evictions,
    })
  }

  fn lock_state(&self) -> Result<MutexGuard<'_, RegistryStateV1>, IndexScopeOrdinalStoreRegistryErrorV1> {
    match self.state.lock() {
      Ok(state) => Ok(state),
      Err(error) => Err(IndexScopeOrdinalStoreRegistryErrorV1::Poisoned(error.to_string())),
    }
  }

  fn evict_for(&self, state: &mut RegistryStateV1, incoming_bytes: u64) -> Result<(), IndexScopeOrdinalStoreRegistryErrorV1> {
    while state.entries.len() >= self.options.maximum_entries
      || state.resident_bytes.checked_add(incoming_bytes).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?
        > self.options.maximum_resident_bytes
    {
      if !evict_one_unpinned(state)? {
        return Err(IndexScopeOrdinalStoreRegistryErrorV1::AllCandidatesPinned);
      }
    }
    Ok(())
  }
}

fn registry_entry_charge(
  descriptor: &NativeIndexOperationDescriptorV1,
  key: &RegistryKeyV1,
) -> Result<u64, IndexScopeOrdinalStoreRegistryErrorV1> {
  // Conservatively cover std-private BTree node spare capacity and Arc allocation headers.
  const ALLOCATION_OVERHEAD_BYTES: u64 = 4 * 1024;
  let fixed = size_of::<RegistryKeyV1>()
    .checked_add(size_of::<RegistryEntryV1>())
    .and_then(|bytes| bytes.checked_add(size_of::<NativeScopeOrdinalStateStoreV1>()))
    .ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
  let fixed = match u64::try_from(fixed) {
    Ok(fixed) => fixed,
    Err(error) => return Err(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticConversion(error.to_string())),
  };
  let descriptor_variables =
    descriptor.variable_bytes()?.checked_mul(2).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
  let index_bytes = match u64::try_from(key.index_id.len()) {
    Ok(index_bytes) => index_bytes,
    Err(error) => return Err(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticConversion(error.to_string())),
  };
  let owned_key_bytes = index_bytes.checked_mul(2).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
  fixed
    .checked_add(descriptor_variables)
    .and_then(|bytes| bytes.checked_add(owned_key_bytes))
    .and_then(|bytes| bytes.checked_add(ALLOCATION_OVERHEAD_BYTES))
    .ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)
}

fn reuse_registry_adapter(
  state: &mut RegistryStateV1,
  key: &RegistryKeyV1,
  descriptor: &NativeIndexOperationDescriptorV1,
) -> Result<Option<Arc<NativeScopeOrdinalStateStoreV1>>, IndexScopeOrdinalStoreRegistryErrorV1> {
  if !state.entries.contains_key(key) {
    return Ok(None);
  }
  state.access_sequence = state.access_sequence.checked_add(1).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
  let access_sequence = state.access_sequence;
  let adapter = {
    let existing = state.entries.get_mut(key).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
    if &existing.descriptor != descriptor {
      return Err(IndexScopeOrdinalStoreRegistryErrorV1::DescriptorConflict);
    }
    existing.last_access_sequence = access_sequence;
    Arc::clone(&existing.adapter)
  };
  state.hits = state.hits.checked_add(1).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
  Ok(Some(adapter))
}

fn evict_one_unpinned(state: &mut RegistryStateV1) -> Result<bool, IndexScopeOrdinalStoreRegistryErrorV1> {
  let candidate = state
    .entries
    .iter()
    .filter(|(_, entry)| Arc::strong_count(&entry.adapter) == 1)
    .min_by(|(left_key, left), (right_key, right)| {
      left.last_access_sequence.cmp(&right.last_access_sequence).then_with(|| left_key.cmp(right_key))
    })
    .map(|(key, _)| key.clone());
  let Some(candidate) = candidate else {
    return Ok(false);
  };
  let removed = state.entries.remove(&candidate).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
  state.resident_bytes =
    state.resident_bytes.checked_sub(removed.charged_bytes).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
  state.evictions = state.evictions.checked_add(1).ok_or(IndexScopeOrdinalStoreRegistryErrorV1::ArithmeticOverflow)?;
  Ok(true)
}

const fn operation_state(state: IndexTaskStateV1) -> IndexOperationStateV1 {
  match state {
    IndexTaskStateV1::Running => IndexOperationStateV1::Checkpointed,
    IndexTaskStateV1::CancelRequested | IndexTaskStateV1::Canceled => IndexOperationStateV1::Canceled,
    IndexTaskStateV1::FailedRetryable | IndexTaskStateV1::FailedTerminal => IndexOperationStateV1::Failed,
    IndexTaskStateV1::CompleteUnpublished => IndexOperationStateV1::Publishing,
    IndexTaskStateV1::Published => IndexOperationStateV1::Complete,
  }
}

fn require_hash(bytes: &[u8], hash_width: usize, label: &'static str) -> Result<(), IndexRecoveryStoreErrorV1> {
  if bytes.len() != hash_width || bytes.iter().all(|byte| *byte == 0) {
    return Err(store_error("native_index_descriptor_hash", format!("{label} must be one nonzero database-width hash")));
  }
  Ok(())
}

fn require_optional_hash(value: Option<&[u8]>, hash_width: usize, label: &'static str) -> Result<(), IndexRecoveryStoreErrorV1> {
  if let Some(value) = value {
    require_hash(value, hash_width, label)?;
  }
  Ok(())
}

fn authority_error(error: impl std::fmt::Display) -> IndexRecoveryStoreErrorV1 {
  store_error("native_index_authority", error.to_string())
}

fn format_error(error: impl std::fmt::Display) -> IndexRecoveryStoreErrorV1 {
  store_error("native_index_format", error.to_string())
}

fn recovery_error(error: impl std::fmt::Display) -> IndexRecoveryStoreErrorV1 {
  store_error("native_index_recovery", error.to_string())
}

fn store_error(code: &'static str, message: impl Into<String>) -> IndexRecoveryStoreErrorV1 {
  IndexRecoveryStoreErrorV1::new(code, message)
}
