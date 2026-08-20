//! Private immutable storage for recoverable v4 index-runtime state.
//!
//! The workspace is node-local recovery evidence. Objects become durable before
//! cumulative manifests, and neither artifact is query or namespace authority.

use std::cmp::Ordering;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::emergency_spill::{create_new_regular_file_no_follow, open_regular_file_no_follow};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::{
  NativeDurabilityError, durable_install_new_native, preallocate_file, sync_directory_native, sync_file_all_native,
};

use super::index_coordinator::FrozenIndexBatchV1;
use super::index_producer_coordinator::IndexProducerTaskRequestV1;
use super::index_runtime_workspace::{
  INDEX_WORKSPACE_MANIFEST_LENGTH_V1, INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1, INDEX_WORKSPACE_OBJECT_MAX_LENGTH_V1,
  IndexWorkspaceManifestV1, IndexWorkspaceManifestWriteV1, IndexWorkspaceObjectHeaderV1, IndexWorkspaceObjectHeaderWriteV1,
  IndexWorkspaceObjectKindV1, decode_index_workspace_manifest_v1, decode_index_workspace_object_header_v1,
  encode_index_workspace_manifest_v1, encode_index_workspace_object_header_v1, index_workspace_manifest_digest_v1,
};
use super::index_runtime_workspace_payload::{
  RUNTIME_BATCH_HEADER_LENGTH, RUNTIME_MUTATION_FRAME_LENGTH, decode_index_workspace_producer_task_payload_v1,
  decode_index_workspace_runtime_batch_stream_header_v1, decode_index_workspace_runtime_mutation_frame_v1,
  encode_index_workspace_producer_task_payload_v1, index_workspace_producer_task_payload_bounds_v1,
  plan_index_workspace_producer_task_payload_v1, plan_index_workspace_runtime_batch_payload_v1,
  stream_index_workspace_runtime_batch_payload_v1, validate_index_workspace_runtime_mutation_frame_header_v1,
};
use super::private_workspace::{
  PrivateWorkspaceErrorV1, create_private_directory_synced, ensure_capacity, secure_platform_private_regular_file,
  validate_existing_directory, validate_private_directory, validate_private_directory_readonly, validate_private_regular_file,
  validate_regular_database_path,
};
use super::reader::FormatError;

const IO_CHUNK_BYTES: usize = 64 * 1024;
const MAX_WORKSPACE_OBJECTS_V1: u64 = 1_048_576;
const RUNTIME_OBJECT_DIRECTORY: &str = "runtime";
const PRODUCER_OBJECT_DIRECTORY: &str = "tasks";
const MANIFEST_DIRECTORY: &str = "manifests";

#[derive(Debug, Error)]
pub enum IndexRuntimeWorkspaceStoreErrorV1 {
  #[error("index runtime workspace options or identity are invalid: {0}")]
  Invalid(String),
  #[error("index runtime workspace operation was canceled")]
  Canceled,
  #[error("index runtime workspace path failed: {0}")]
  Path(String),
  #[error("index runtime workspace state failed: {0}")]
  State(String),
  #[error("index runtime workspace capacity failed: {0}")]
  Capacity(String),
  #[error("index runtime workspace resource pressure: {0}")]
  Resource(String),
  #[error("index runtime workspace allocation failed: {0}")]
  Allocation(String),
  #[error("index runtime workspace format failed: {0}")]
  Format(String),
  #[error("index runtime workspace I/O failed during {operation}: {source}")]
  Io {
    operation: &'static str,
    #[source]
    source: std::io::Error,
  },
  #[error("index runtime workspace private-path validation failed: {0}")]
  Workspace(String),
  #[error("index runtime workspace durability failed: {0}")]
  Durability(#[source] Box<NativeDurabilityError>),
  #[error("index runtime workspace memory admission failed: {0}")]
  Memory(#[source] Box<MemoryCoordinatorError>),
  #[error("index runtime workspace cleanup failed after {primary}: {source}")]
  Cleanup {
    primary: String,
    #[source]
    source: std::io::Error,
  },
  #[error("index runtime workspace completion is uncertain after {primary}; exact reopen also failed: {reopen}")]
  Uncertain { primary: String, reopen: String },
}

impl From<FormatError> for IndexRuntimeWorkspaceStoreErrorV1 {
  fn from(error: FormatError) -> Self {
    Self::Format(error.to_string())
  }
}

impl From<PrivateWorkspaceErrorV1> for IndexRuntimeWorkspaceStoreErrorV1 {
  fn from(error: PrivateWorkspaceErrorV1) -> Self {
    match error {
      PrivateWorkspaceErrorV1::Path(context) => Self::Workspace(context),
      #[cfg(windows)]
      PrivateWorkspaceErrorV1::State(context) => Self::Workspace(context),
      PrivateWorkspaceErrorV1::Capacity(context) => Self::Resource(context),
      #[cfg(windows)]
      PrivateWorkspaceErrorV1::Allocation(context) => Self::Allocation(context),
      PrivateWorkspaceErrorV1::Io { operation, source } => Self::Io { operation, source },
      PrivateWorkspaceErrorV1::Durability(source) => Self::Durability(source),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexRuntimeWorkspaceIdentityV1 {
  database_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  workspace_id: [u8; 16],
  runtime_id: [u8; 16],
  hash_algorithm: HashAlgorithm,
}

impl IndexRuntimeWorkspaceIdentityV1 {
  pub fn new(
    database_id: [u8; 16],
    destination_physical_instance_id: [u8; 16],
    workspace_id: [u8; 16],
    runtime_id: [u8; 16],
    hash_algorithm: HashAlgorithm,
  ) -> Result<Self, IndexRuntimeWorkspaceStoreErrorV1> {
    if [database_id, destination_physical_instance_id, workspace_id, runtime_id]
      .iter()
      .any(|identity| identity.iter().all(|byte| *byte == 0))
    {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Invalid("workspace identities must be nonzero".to_string()));
    }
    Ok(Self { database_id, destination_physical_instance_id, workspace_id, runtime_id, hash_algorithm })
  }

  pub const fn database_id(self) -> [u8; 16] {
    self.database_id
  }

  pub const fn destination_physical_instance_id(self) -> [u8; 16] {
    self.destination_physical_instance_id
  }

  pub const fn workspace_id(self) -> [u8; 16] {
    self.workspace_id
  }

  pub const fn runtime_id(self) -> [u8; 16] {
    self.runtime_id
  }

  pub const fn hash_algorithm(self) -> HashAlgorithm {
    self.hash_algorithm
  }
}

#[derive(Debug, Clone)]
pub struct IndexRuntimeWorkspaceOptionsV1 {
  scratch_root: Option<PathBuf>,
  maximum_stored_bytes: u64,
  minimum_free_bytes: u64,
  maximum_object_count: u64,
}

impl IndexRuntimeWorkspaceOptionsV1 {
  pub fn new(
    scratch_root: Option<PathBuf>,
    maximum_stored_bytes: u64,
    minimum_free_bytes: u64,
    maximum_object_count: u64,
  ) -> Result<Self, IndexRuntimeWorkspaceStoreErrorV1> {
    validate_limits(maximum_stored_bytes, maximum_object_count)?;
    if let Some(path) = &scratch_root {
      validate_canonical_native_path(path, "scratch root")?;
    }
    Ok(Self { scratch_root, maximum_stored_bytes, minimum_free_bytes, maximum_object_count })
  }
}

#[derive(Debug, Clone, Copy)]
pub struct IndexRuntimeWorkspaceReopenOptionsV1 {
  maximum_stored_bytes: u64,
  maximum_object_count: u64,
}

impl IndexRuntimeWorkspaceReopenOptionsV1 {
  pub fn new(maximum_stored_bytes: u64, maximum_object_count: u64) -> Result<Self, IndexRuntimeWorkspaceStoreErrorV1> {
    validate_limits(maximum_stored_bytes, maximum_object_count)?;
    Ok(Self { maximum_stored_bytes, maximum_object_count })
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRuntimeWorkspaceSelectedHeadV1 {
  workspace_path: PathBuf,
  workspace_id: [u8; 16],
  manifest_digest: [u8; 32],
  durable_sequence: u64,
  durable_bytes: u64,
}

impl IndexRuntimeWorkspaceSelectedHeadV1 {
  pub fn new(
    workspace_path: PathBuf,
    workspace_id: [u8; 16],
    manifest_digest: [u8; 32],
    durable_sequence: u64,
    durable_bytes: u64,
  ) -> Result<Self, IndexRuntimeWorkspaceStoreErrorV1> {
    validate_canonical_native_path(&workspace_path, "selected workspace path")?;
    if workspace_id.iter().all(|byte| *byte == 0)
      || manifest_digest.iter().all(|byte| *byte == 0)
      || durable_sequence == 0
      || durable_bytes == 0
    {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Invalid("selected workspace head is not canonical".to_string()));
    }
    Ok(Self { workspace_path, workspace_id, manifest_digest, durable_sequence, durable_bytes })
  }

  pub fn workspace_path(&self) -> &Path {
    &self.workspace_path
  }

  pub const fn workspace_id(&self) -> [u8; 16] {
    self.workspace_id
  }

  pub const fn manifest_digest(&self) -> [u8; 32] {
    self.manifest_digest
  }

  pub const fn durable_sequence(&self) -> u64 {
    self.durable_sequence
  }

  pub const fn durable_bytes(&self) -> u64 {
    self.durable_bytes
  }
}

#[derive(Debug, Clone)]
pub struct IndexRuntimeWorkspaceHeadV1 {
  selected: IndexRuntimeWorkspaceSelectedHeadV1,
  identity: IndexRuntimeWorkspaceIdentityV1,
  cumulative_object_count: u64,
  runtime_batch_count: u64,
  producer_task_count: u64,
  latest_object_kind: IndexWorkspaceObjectKindV1,
  latest_object_id: [u8; 16],
  latest_object_digest: [u8; 32],
  latest_object_stored_bytes: u64,
  created_at_ms: u64,
}

impl IndexRuntimeWorkspaceHeadV1 {
  pub fn workspace_path(&self) -> &Path {
    self.selected.workspace_path()
  }

  pub const fn manifest_sequence(&self) -> u64 {
    self.selected.durable_sequence
  }

  pub const fn durable_bytes(&self) -> u64 {
    self.selected.durable_bytes
  }

  pub const fn cumulative_object_count(&self) -> u64 {
    self.cumulative_object_count
  }

  pub const fn runtime_batch_count(&self) -> u64 {
    self.runtime_batch_count
  }

  pub const fn producer_task_count(&self) -> u64 {
    self.producer_task_count
  }

  pub const fn runtime_id(&self) -> [u8; 16] {
    self.identity.runtime_id
  }

  pub fn selected_descriptor(&self) -> IndexRuntimeWorkspaceSelectedHeadV1 {
    self.selected.clone()
  }

  pub(super) const fn latest_object_created_at_ms(&self) -> u64 {
    self.created_at_ms
  }
}

pub struct DurableIndexRuntimeWorkspaceV1 {
  identity: IndexRuntimeWorkspaceIdentityV1,
  options: IndexRuntimeWorkspaceOptionsV1,
  cancellation: CancellationToken,
  memory: MemoryCoordinator,
  workspace_path: PathBuf,
  manifests_path: PathBuf,
  runtime_objects_path: PathBuf,
  producer_objects_path: PathBuf,
  head: Option<IndexRuntimeWorkspaceHeadV1>,
  append_state: WorkspaceAppendStateV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceAppendStateV1 {
  Clean,
  Unselected { kind: IndexWorkspaceObjectKindV1, object_id: [u8; 16] },
  ReconcileInventory,
}

#[derive(Clone, Copy)]
struct WorkspaceObjectPlanV1 {
  kind: IndexWorkspaceObjectKindV1,
  payload_length: usize,
  logical_record_count: u64,
  minimum_publication_sequence: u64,
  maximum_publication_sequence: u64,
  payload_digest: [u8; 32],
}

impl DurableIndexRuntimeWorkspaceV1 {
  pub const fn identity(&self) -> IndexRuntimeWorkspaceIdentityV1 {
    self.identity
  }

  pub fn head(&self) -> Option<&IndexRuntimeWorkspaceHeadV1> {
    self.head.as_ref()
  }

  pub(super) fn retained_heap_capacity(&self) -> Option<usize> {
    let capacities = [
      self.options.scratch_root.as_ref().map_or(0, PathBuf::capacity),
      self.workspace_path.capacity(),
      self.manifests_path.capacity(),
      self.runtime_objects_path.capacity(),
      self.producer_objects_path.capacity(),
      self.head.as_ref().map_or(0, |head| head.selected.workspace_path.capacity()),
    ];
    capacities.into_iter().try_fold(0usize, usize::checked_add)
  }

  pub fn create(
    database_path: &Path,
    identity: IndexRuntimeWorkspaceIdentityV1,
    options: IndexRuntimeWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, IndexRuntimeWorkspaceStoreErrorV1> {
    check_cancellation(&cancellation)?;
    validate_canonical_native_path(database_path, "database path")?;
    validate_regular_database_path(database_path, "index runtime workspace database")?;
    let workspace_path = create_workspace_root(database_path, identity, &options)?;
    let manifests_path = workspace_path.join(MANIFEST_DIRECTORY);
    let objects_path = workspace_path.join("objects");
    let runtime_objects_path = objects_path.join(RUNTIME_OBJECT_DIRECTORY);
    let producer_objects_path = objects_path.join(PRODUCER_OBJECT_DIRECTORY);
    create_private_directory_synced(&manifests_path, &workspace_path)?;
    create_private_directory_synced(&objects_path, &workspace_path)?;
    create_private_directory_synced(&runtime_objects_path, &objects_path)?;
    create_private_directory_synced(&producer_objects_path, &objects_path)?;
    Ok(Self {
      identity,
      options,
      cancellation,
      memory: memory.clone(),
      workspace_path,
      manifests_path,
      runtime_objects_path,
      producer_objects_path,
      head: None,
      append_state: WorkspaceAppendStateV1::Clean,
    })
  }

  pub fn resume(
    database_id: [u8; 16],
    destination_physical_instance_id: [u8; 16],
    hash_algorithm: HashAlgorithm,
    selected: IndexRuntimeWorkspaceSelectedHeadV1,
    options: IndexRuntimeWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, IndexRuntimeWorkspaceStoreErrorV1> {
    check_cancellation(&cancellation)?;
    let workspace_path = selected.workspace_path.clone();
    let reopened = ReopenedIndexRuntimeWorkspaceV1::open(
      &workspace_path,
      database_id,
      destination_physical_instance_id,
      hash_algorithm,
      selected,
      IndexRuntimeWorkspaceReopenOptionsV1::new(options.maximum_stored_bytes, options.maximum_object_count)?,
      cancellation.clone(),
      memory,
    )?;
    ensure_capacity(&workspace_path, 0, options.minimum_free_bytes)?;
    let objects_path = workspace_path.join("objects");
    Ok(Self {
      identity: reopened.head.identity,
      options,
      cancellation,
      memory: memory.clone(),
      manifests_path: workspace_path.join(MANIFEST_DIRECTORY),
      runtime_objects_path: objects_path.join(RUNTIME_OBJECT_DIRECTORY),
      producer_objects_path: objects_path.join(PRODUCER_OBJECT_DIRECTORY),
      workspace_path,
      head: Some(reopened.head),
      append_state: WorkspaceAppendStateV1::ReconcileInventory,
    })
  }

  pub fn append_runtime_batch(
    &mut self,
    object_id: [u8; 16],
    created_at_ms: u64,
    batch: &FrozenIndexBatchV1,
  ) -> Result<IndexRuntimeWorkspaceHeadV1, IndexRuntimeWorkspaceStoreErrorV1> {
    let payload_plan = plan_index_workspace_runtime_batch_payload_v1(batch, self.identity.hash_algorithm)?;
    let plan = WorkspaceObjectPlanV1 {
      kind: IndexWorkspaceObjectKindV1::RuntimeBatch,
      payload_length: payload_plan.payload_length(),
      logical_record_count: payload_plan.logical_record_count(),
      minimum_publication_sequence: payload_plan.minimum_publication_sequence(),
      maximum_publication_sequence: payload_plan.maximum_publication_sequence(),
      payload_digest: payload_plan.payload_digest(),
    };
    self.append_object(object_id, created_at_ms, plan, |path, expected, cancellation, memory| {
      write_runtime_object(path, expected, batch, payload_plan, cancellation, memory)
    })
  }

  pub fn append_producer_task(
    &mut self,
    object_id: [u8; 16],
    created_at_ms: u64,
    task: &IndexProducerTaskRequestV1<'_>,
  ) -> Result<IndexRuntimeWorkspaceHeadV1, IndexRuntimeWorkspaceStoreErrorV1> {
    let payload_plan = plan_index_workspace_producer_task_payload_v1(task, self.identity.hash_algorithm)?;
    let payload_bytes = u64::try_from(payload_plan.payload_length())
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("producer task payload length exceeds u64: {error}")))?;
    let _payload_reservation = self
      .memory
      .reserve(MemoryOwner::IndexDirtyBuffers, payload_bytes, AdmissionClass::Maintenance)
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Memory(Box::new(error)))?;
    let payload = encode_index_workspace_producer_task_payload_v1(task, self.identity.hash_algorithm)?;
    let plan = WorkspaceObjectPlanV1 {
      kind: IndexWorkspaceObjectKindV1::ProducerTask,
      payload_length: payload.len(),
      logical_record_count: 1,
      minimum_publication_sequence: task.publication_sequence,
      maximum_publication_sequence: task.publication_sequence,
      payload_digest: *blake3::hash(&payload).as_bytes(),
    };
    self.append_object(object_id, created_at_ms, plan, |path, expected, cancellation, memory| {
      write_buffered_object(path, expected, &payload, cancellation, memory)
    })
  }

  pub(super) fn selected_contains_producer_task(
    &self,
    selected: &IndexRuntimeWorkspaceSelectedHeadV1,
    object_id: [u8; 16],
    task: &IndexProducerTaskRequestV1<'_>,
  ) -> Result<bool, IndexRuntimeWorkspaceStoreErrorV1> {
    check_cancellation(&self.cancellation)?;
    if selected.workspace_path != self.workspace_path || selected.workspace_id != self.identity.workspace_id {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::State("selected producer lookup is bound to another workspace".to_string()));
    }
    if selected.durable_sequence > self.options.maximum_object_count {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Capacity(
        "selected producer lookup exceeds the workspace object-count cap".to_string(),
      ));
    }
    enforce_selected_bytes(selected.durable_bytes, selected.durable_sequence, self.options.maximum_stored_bytes)?;

    let object_path = object_path(&self.producer_objects_path, object_id);
    if !path_present(&object_path)? {
      return Ok(false);
    }
    let payload_plan = plan_index_workspace_producer_task_payload_v1(task, self.identity.hash_algorithm)?;
    let payload_bytes = u64::try_from(payload_plan.payload_length())
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("producer task payload length exceeds u64: {error}")))?;
    let payload_reservation = self
      .memory
      .reserve(MemoryOwner::IndexDirtyBuffers, payload_bytes, AdmissionClass::Maintenance)
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Memory(Box::new(error)))?;
    let payload = encode_index_workspace_producer_task_payload_v1(task, self.identity.hash_algorithm)?;
    let plan = WorkspaceObjectPlanV1 {
      kind: IndexWorkspaceObjectKindV1::ProducerTask,
      payload_length: payload.len(),
      logical_record_count: 1,
      minimum_publication_sequence: task.publication_sequence,
      maximum_publication_sequence: task.publication_sequence,
      payload_digest: *blake3::hash(&payload).as_bytes(),
    };
    let expected = ExpectedObjectV1::payload_bound(self.identity, object_id, plan);
    drop(payload);
    drop(payload_reservation);
    let object = validate_object_file(&object_path, &expected, &self.cancellation, &self.memory)?;
    if object.object_sequence > selected.durable_sequence {
      return Ok(false);
    }

    let mut sequence = selected.durable_sequence;
    let mut expected_digest = selected.manifest_digest;
    let mut expected_cumulative_count = selected.durable_sequence;
    let mut expected_cumulative_bytes = selected.durable_bytes;
    loop {
      check_cancellation(&self.cancellation)?;
      let observed = read_manifest(&manifest_path(&self.manifests_path, sequence), &self.cancellation)?;
      if observed.digest != expected_digest {
        return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(format!(
          "manifest {sequence} digest disagrees with the selected producer lookup chain"
        )));
      }
      let manifest = observed.manifest;
      validate_manifest_identity(
        &manifest,
        self.identity.database_id,
        self.identity.destination_physical_instance_id,
        self.identity.workspace_id,
        self.identity.runtime_id,
        sequence,
      )?;
      if manifest.cumulative_object_count != expected_cumulative_count
        || manifest.cumulative_stored_bytes != expected_cumulative_bytes
        || manifest.cumulative_object_count != sequence
      {
        return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(format!(
          "manifest {sequence} cumulative count or bytes are discontinuous during selected producer lookup"
        )));
      }
      if sequence == object.object_sequence {
        if manifest.object_kind != IndexWorkspaceObjectKindV1::ProducerTask
          || manifest.object_id != object_id
          || manifest.object_digest != object.digest
          || manifest.object_stored_bytes != object.stored_bytes
          || manifest.created_at_ms != object.created_at_ms
        {
          return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(
            "selected producer manifest does not close over the exact producer object".to_string(),
          ));
        }
        return Ok(true);
      }
      expected_cumulative_count = expected_cumulative_count
        .checked_sub(1)
        .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Format("selected producer manifest count underflow".to_string()))?;
      expected_cumulative_bytes = expected_cumulative_bytes
        .checked_sub(manifest.object_stored_bytes)
        .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Format("selected producer manifest byte total underflow".to_string()))?;
      expected_digest = manifest.previous_manifest_digest;
      sequence = sequence
        .checked_sub(1)
        .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Format("selected producer manifest chain ended before its object".to_string()))?;
    }
  }

  fn append_object(
    &mut self,
    object_id: [u8; 16],
    created_at_ms: u64,
    plan: WorkspaceObjectPlanV1,
    write_object: impl FnOnce(
      &Path,
      &ExpectedObjectV1,
      &CancellationToken,
      &MemoryCoordinator,
    ) -> Result<ValidatedObjectV1, IndexRuntimeWorkspaceStoreErrorV1>,
  ) -> Result<IndexRuntimeWorkspaceHeadV1, IndexRuntimeWorkspaceStoreErrorV1> {
    self.preflight_append(plan.kind, object_id, created_at_ms)?;
    if let Some(head) = &self.head {
      if head.latest_object_id == object_id {
        if head.latest_object_kind != plan.kind {
          return Err(IndexRuntimeWorkspaceStoreErrorV1::State(
            "object identity was already used by another workspace object kind".to_string(),
          ));
        }
        if created_at_ms != head.created_at_ms {
          return Err(IndexRuntimeWorkspaceStoreErrorV1::State(
            "exact selected workspace retry changed its creation timestamp".to_string(),
          ));
        }
        let expected = ExpectedObjectV1::exact(self.identity, object_id, head.manifest_sequence(), head.created_at_ms, plan);
        let observed =
          validate_object_file(&object_path(self.object_directory(plan.kind), object_id), &expected, &self.cancellation, &self.memory)?;
        if observed.digest != head.latest_object_digest || observed.stored_bytes != head.latest_object_stored_bytes {
          return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("retry object disagrees with the selected workspace head".to_string()));
        }
        return Ok(head.clone());
      }
    }

    let sequence = self.head.as_ref().map_or(Ok(1), |head| {
      head
        .manifest_sequence()
        .checked_add(1)
        .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace manifest sequence overflow".to_string()))
    })?;
    let object_already_installed = matches!(
      self.append_state,
      WorkspaceAppendStateV1::Unselected { kind, object_id: unselected_id } if kind == plan.kind && unselected_id == object_id
    );
    let expected = if object_already_installed {
      ExpectedObjectV1::durable_retry(self.identity, object_id, sequence, plan)
    } else {
      ExpectedObjectV1::exact(self.identity, object_id, sequence, created_at_ms, plan)
    };
    let object_bytes = object_stored_bytes(plan.payload_length)?;
    self.enforce_append_capacity(object_bytes, object_already_installed)?;
    let object_path = object_path(self.object_directory(plan.kind), object_id);
    let object = match write_object(&object_path, &expected, &self.cancellation, &self.memory) {
      Ok(object) => {
        self.append_state = WorkspaceAppendStateV1::Unselected { kind: expected.kind, object_id };
        object
      }
      Err(error) => {
        self.append_state = WorkspaceAppendStateV1::ReconcileInventory;
        return Err(error);
      }
    };
    let previous_digest = self.head.as_ref().map_or([0; 32], |head| head.selected.manifest_digest);
    let previous_count = self.head.as_ref().map_or(0, |head| head.cumulative_object_count);
    let previous_bytes = self.head.as_ref().map_or(0, IndexRuntimeWorkspaceHeadV1::durable_bytes);
    let cumulative_object_count = previous_count
      .checked_add(1)
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace object count overflow".to_string()))?;
    let cumulative_stored_bytes = previous_bytes
      .checked_add(object.stored_bytes)
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace durable byte total overflow".to_string()))?;
    let manifest = encode_index_workspace_manifest_v1(&IndexWorkspaceManifestWriteV1 {
      database_id: self.identity.database_id,
      destination_physical_instance_id: self.identity.destination_physical_instance_id,
      workspace_id: self.identity.workspace_id,
      runtime_id: self.identity.runtime_id,
      manifest_sequence: sequence,
      previous_manifest_digest: previous_digest,
      object_kind: plan.kind,
      object_id,
      object_digest: object.digest,
      object_stored_bytes: object.stored_bytes,
      cumulative_object_count,
      cumulative_stored_bytes,
      created_at_ms: object.created_at_ms,
    })?;
    let installed_manifest_path = manifest_path(&self.manifests_path, sequence);
    write_immutable_bytes(&installed_manifest_path, &manifest, &self.cancellation)?;
    let manifest_digest = index_workspace_manifest_digest_v1(&manifest)?;
    let observed_manifest = read_manifest(&installed_manifest_path, &self.cancellation)?;
    if observed_manifest.digest != manifest_digest || observed_manifest.manifest != decode_index_workspace_manifest_v1(&manifest)? {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(
        "installed workspace manifest readback disagrees with requested bytes".to_string(),
      ));
    }
    if let Some(previous) = &self.head {
      let predecessor = read_manifest(&manifest_path(&self.manifests_path, previous.manifest_sequence()), &self.cancellation)?;
      if predecessor.digest != previous.selected.manifest_digest {
        return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(
          "installed predecessor manifest changed before head advancement".to_string(),
        ));
      }
    }
    let selected = IndexRuntimeWorkspaceSelectedHeadV1::new(
      self.workspace_path.clone(),
      self.identity.workspace_id,
      manifest_digest,
      sequence,
      cumulative_stored_bytes,
    )?;
    let head = IndexRuntimeWorkspaceHeadV1 {
      selected,
      identity: self.identity,
      cumulative_object_count,
      runtime_batch_count: increment_kind_count(
        self.head.as_ref().map_or(0, |head| head.runtime_batch_count),
        plan.kind == IndexWorkspaceObjectKindV1::RuntimeBatch,
        "runtime batch count overflow",
      )?,
      producer_task_count: increment_kind_count(
        self.head.as_ref().map_or(0, |head| head.producer_task_count),
        plan.kind == IndexWorkspaceObjectKindV1::ProducerTask,
        "producer task count overflow",
      )?,
      latest_object_kind: plan.kind,
      latest_object_id: object_id,
      latest_object_digest: object.digest,
      latest_object_stored_bytes: object.stored_bytes,
      created_at_ms: object.created_at_ms,
    };
    self.append_state = WorkspaceAppendStateV1::Clean;
    self.head = Some(head.clone());
    Ok(head)
  }

  fn object_directory(&self, kind: IndexWorkspaceObjectKindV1) -> &Path {
    match kind {
      IndexWorkspaceObjectKindV1::RuntimeBatch => &self.runtime_objects_path,
      IndexWorkspaceObjectKindV1::ProducerTask => &self.producer_objects_path,
    }
  }

  fn preflight_append(
    &mut self,
    requested_kind: IndexWorkspaceObjectKindV1,
    object_id: [u8; 16],
    created_at_ms: u64,
  ) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
    check_cancellation(&self.cancellation)?;
    if object_id.iter().all(|byte| *byte == 0) || created_at_ms == 0 {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Invalid("workspace object identity and creation time must be nonzero".to_string()));
    }
    if self.append_state == WorkspaceAppendStateV1::ReconcileInventory {
      self.append_state = self.reconcile_object_inventory(object_id, requested_kind)?;
    }
    match self.append_state {
      WorkspaceAppendStateV1::Clean => Ok(()),
      WorkspaceAppendStateV1::Unselected { kind, object_id: unselected_id } if kind == requested_kind && unselected_id == object_id => {
        Ok(())
      }
      WorkspaceAppendStateV1::Unselected { .. } | WorkspaceAppendStateV1::ReconcileInventory => {
        Err(IndexRuntimeWorkspaceStoreErrorV1::State(
          "workspace contains an unselected object; only its exact object-identity retry is admissible".to_string(),
        ))
      }
    }
  }

  fn reconcile_object_inventory(
    &self,
    requested_object_id: [u8; 16],
    requested_kind: IndexWorkspaceObjectKindV1,
  ) -> Result<WorkspaceAppendStateV1, IndexRuntimeWorkspaceStoreErrorV1> {
    let mut count = 0u64;
    let mut requested_present = false;
    for (directory, kind) in [
      (&self.runtime_objects_path, IndexWorkspaceObjectKindV1::RuntimeBatch),
      (&self.producer_objects_path, IndexWorkspaceObjectKindV1::ProducerTask),
    ] {
      validate_private_directory(directory, "index runtime object inventory directory")?;
      let entries = fs::read_dir(directory)
        .map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation: "workspace object inventory", source })?;
      for entry in entries {
        let entry =
          entry.map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation: "workspace object inventory entry", source })?;
        let file_type = entry
          .file_type()
          .map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation: "workspace object inventory type", source })?;
        if !file_type.is_file() || file_type.is_symlink() {
          return Err(IndexRuntimeWorkspaceStoreErrorV1::Path(format!(
            "workspace object inventory contains a non-regular entry: {}",
            entry.path().display()
          )));
        }
        let name = entry
          .file_name()
          .to_str()
          .map(str::to_owned)
          .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Path("workspace object name is not UTF-8".to_string()))?;
        let encoded_id = name
          .strip_suffix(".aiwo")
          .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Path(format!("workspace object name is not canonical: {name}")))?;
        if encoded_id.len() != 32 || !encoded_id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
          return Err(IndexRuntimeWorkspaceStoreErrorV1::Path(format!("workspace object name is not canonical lowercase hex: {name}")));
        }
        let decoded_id = hex::decode(encoded_id)
          .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Path(format!("workspace object name is not hexadecimal: {error}")))?;
        if decoded_id.len() != 16 {
          return Err(IndexRuntimeWorkspaceStoreErrorV1::Path("workspace object name does not encode a 16-byte ID".to_string()));
        }
        let mut decoded_id_array = [0u8; 16];
        decoded_id_array.copy_from_slice(&decoded_id);
        let decoded_id = decoded_id_array;
        if kind == requested_kind && decoded_id == requested_object_id {
          requested_present = true;
        }
        count = count
          .checked_add(1)
          .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace object inventory count overflow".to_string()))?;
        let inventory_cap = self
          .options
          .maximum_object_count
          .checked_add(1)
          .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace inventory cap overflow".to_string()))?;
        if count > inventory_cap {
          return Err(IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace object inventory exceeds its bounded scan cap".to_string()));
        }
      }
    }
    let selected_count = self.head.as_ref().map_or(0, |head| head.cumulative_object_count);
    let exact_selected_retry = self.head.as_ref().is_some_and(|head| {
      head.latest_object_kind == requested_kind && head.latest_object_id == requested_object_id && count == selected_count
    });
    let unselected_count = selected_count
      .checked_add(1)
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace selected object count overflow".to_string()))?;
    let requested_is_selected_head =
      self.head.as_ref().is_some_and(|head| head.latest_object_kind == requested_kind && head.latest_object_id == requested_object_id);
    let exact_unselected_retry = requested_present && !requested_is_selected_head && count == unselected_count;
    let fresh_append = !requested_present && count == selected_count;
    if exact_selected_retry || fresh_append {
      return Ok(WorkspaceAppendStateV1::Clean);
    }
    if exact_unselected_retry {
      return Ok(WorkspaceAppendStateV1::Unselected { kind: requested_kind, object_id: requested_object_id });
    }
    Err(IndexRuntimeWorkspaceStoreErrorV1::State(
      "workspace contains an unselected or conflicting object; only its exact retry is admissible".to_string(),
    ))
  }

  fn enforce_append_capacity(&self, object_bytes: u64, object_already_installed: bool) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
    let prior_objects = self.head.as_ref().map_or(0, |head| head.cumulative_object_count);
    if prior_objects >= self.options.maximum_object_count {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace object-count cap is exhausted".to_string()));
    }
    let prior_bytes = self.head.as_ref().map_or(0, IndexRuntimeWorkspaceHeadV1::durable_bytes);
    let manifest_count = prior_objects
      .checked_add(1)
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace manifest count overflow".to_string()))?;
    let manifest_bytes = manifest_count
      .checked_mul(INDEX_WORKSPACE_MANIFEST_LENGTH_V1 as u64)
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace manifest byte total overflow".to_string()))?;
    let projected = prior_bytes
      .checked_add(object_bytes)
      .and_then(|bytes| bytes.checked_add(manifest_bytes))
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace projected byte total overflow".to_string()))?;
    if projected > self.options.maximum_stored_bytes {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!(
        "workspace projected bytes {projected} exceed cap {}",
        self.options.maximum_stored_bytes
      )));
    }
    let additional_bytes = incremental_append_allocation_bytes(object_bytes, object_already_installed)?;
    ensure_capacity(&self.workspace_path, additional_bytes, self.options.minimum_free_bytes)?;
    Ok(())
  }
}

fn increment_kind_count(prior: u64, increment: bool, overflow_context: &'static str) -> Result<u64, IndexRuntimeWorkspaceStoreErrorV1> {
  if increment {
    prior.checked_add(1).ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity(overflow_context.to_string()))
  } else {
    Ok(prior)
  }
}

fn incremental_append_allocation_bytes(
  object_bytes: u64,
  object_already_installed: bool,
) -> Result<u64, IndexRuntimeWorkspaceStoreErrorV1> {
  if object_already_installed {
    Ok(INDEX_WORKSPACE_MANIFEST_LENGTH_V1 as u64)
  } else {
    object_bytes
      .checked_add(INDEX_WORKSPACE_MANIFEST_LENGTH_V1 as u64)
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace capacity request overflow".to_string()))
  }
}

pub struct ReopenedIndexRuntimeWorkspaceV1 {
  head: IndexRuntimeWorkspaceHeadV1,
}

impl ReopenedIndexRuntimeWorkspaceV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn open(
    workspace_path: &Path,
    database_id: [u8; 16],
    destination_physical_instance_id: [u8; 16],
    hash_algorithm: HashAlgorithm,
    selected: IndexRuntimeWorkspaceSelectedHeadV1,
    options: IndexRuntimeWorkspaceReopenOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, IndexRuntimeWorkspaceStoreErrorV1> {
    check_cancellation(&cancellation)?;
    if database_id.iter().all(|byte| *byte == 0) || destination_physical_instance_id.iter().all(|byte| *byte == 0) {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Invalid(
        "reopen database and destination physical-instance identities must be nonzero".to_string(),
      ));
    }
    if workspace_path != selected.workspace_path {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Invalid("selected workspace path is not the requested canonical path".to_string()));
    }
    validate_canonical_native_path(workspace_path, "selected workspace path")?;
    validate_private_directory_readonly(workspace_path, "index runtime workspace")?;
    let manifests_path = workspace_path.join(MANIFEST_DIRECTORY);
    let objects_path = workspace_path.join("objects");
    let runtime_objects_path = objects_path.join(RUNTIME_OBJECT_DIRECTORY);
    let producer_objects_path = objects_path.join(PRODUCER_OBJECT_DIRECTORY);
    for (path, role) in [
      (&manifests_path, "index runtime manifest directory"),
      (&objects_path, "index runtime object directory"),
      (&runtime_objects_path, "index runtime batch directory"),
      (&producer_objects_path, "index runtime producer-task directory"),
    ] {
      validate_private_directory_readonly(path, role)?;
    }
    if selected.durable_sequence > options.maximum_object_count {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Capacity("selected sequence exceeds the reopen object-count cap".to_string()));
    }
    enforce_selected_bytes(selected.durable_bytes, selected.durable_sequence, options.maximum_stored_bytes)?;

    let mut sequence = selected.durable_sequence;
    let mut expected_digest = selected.manifest_digest;
    let mut expected_cumulative_count = selected.durable_sequence;
    let mut expected_cumulative_bytes = selected.durable_bytes;
    let mut runtime_batch_count = 0u64;
    let mut producer_task_count = 0u64;
    let mut head_manifest = None;
    let mut latest_object_digest = [0; 32];
    let mut latest_object_stored_bytes = 0u64;
    while sequence != 0 {
      check_cancellation(&cancellation)?;
      let observed = read_manifest(&manifest_path(&manifests_path, sequence), &cancellation)?;
      if observed.digest != expected_digest {
        return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(format!(
          "manifest {sequence} digest disagrees with its successor or selected head"
        )));
      }
      let manifest = observed.manifest;
      validate_manifest_identity(
        &manifest,
        database_id,
        destination_physical_instance_id,
        selected.workspace_id,
        head_manifest.as_ref().map_or(manifest.runtime_id, |head: &IndexWorkspaceManifestV1| head.runtime_id),
        sequence,
      )?;
      if manifest.cumulative_object_count != expected_cumulative_count
        || manifest.cumulative_stored_bytes != expected_cumulative_bytes
        || manifest.cumulative_object_count != sequence
      {
        return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(format!("manifest {sequence} cumulative count or bytes are discontinuous")));
      }
      let object_directory = match manifest.object_kind {
        IndexWorkspaceObjectKindV1::RuntimeBatch => {
          runtime_batch_count = runtime_batch_count
            .checked_add(1)
            .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("runtime batch count overflow".to_string()))?;
          &runtime_objects_path
        }
        IndexWorkspaceObjectKindV1::ProducerTask => {
          producer_task_count = producer_task_count
            .checked_add(1)
            .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("producer task count overflow".to_string()))?;
          &producer_objects_path
        }
      };
      let expected_object = ExpectedObjectV1::manifest_bound(
        IndexRuntimeWorkspaceIdentityV1::new(
          database_id,
          destination_physical_instance_id,
          selected.workspace_id,
          manifest.runtime_id,
          hash_algorithm,
        )?,
        manifest.object_kind,
        manifest.object_id,
        sequence,
        manifest.created_at_ms,
      );
      let object = validate_object_file(&object_path(object_directory, manifest.object_id), &expected_object, &cancellation, memory)?;
      if object.digest != manifest.object_digest || object.stored_bytes != manifest.object_stored_bytes {
        return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(format!("manifest {sequence} object digest or bytes do not close")));
      }
      if sequence == selected.durable_sequence {
        latest_object_digest = object.digest;
        latest_object_stored_bytes = object.stored_bytes;
        head_manifest = Some(manifest);
      }
      expected_cumulative_count = expected_cumulative_count
        .checked_sub(1)
        .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Format("workspace cumulative object count underflow".to_string()))?;
      expected_cumulative_bytes = expected_cumulative_bytes
        .checked_sub(object.stored_bytes)
        .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Format("workspace cumulative byte total underflow".to_string()))?;
      expected_digest = manifest.previous_manifest_digest;
      sequence -= 1;
    }
    if expected_cumulative_count != 0 || expected_cumulative_bytes != 0 || expected_digest != [0; 32] {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("workspace manifest chain does not terminate exactly".to_string()));
    }
    let head_manifest =
      head_manifest.ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Format("selected workspace has no head manifest".to_string()))?;
    let identity = IndexRuntimeWorkspaceIdentityV1::new(
      database_id,
      destination_physical_instance_id,
      selected.workspace_id,
      head_manifest.runtime_id,
      hash_algorithm,
    )?;
    Ok(Self {
      head: IndexRuntimeWorkspaceHeadV1 {
        selected,
        identity,
        cumulative_object_count: head_manifest.cumulative_object_count,
        runtime_batch_count,
        producer_task_count,
        latest_object_kind: head_manifest.object_kind,
        latest_object_id: head_manifest.object_id,
        latest_object_digest,
        latest_object_stored_bytes,
        created_at_ms: head_manifest.created_at_ms,
      },
    })
  }

  pub const fn runtime_id(&self) -> [u8; 16] {
    self.head.identity.runtime_id
  }

  pub const fn manifest_sequence(&self) -> u64 {
    self.head.selected.durable_sequence
  }

  pub const fn runtime_batch_count(&self) -> u64 {
    self.head.runtime_batch_count
  }

  pub const fn producer_task_count(&self) -> u64 {
    self.head.producer_task_count
  }
}

#[derive(Debug, Clone, Copy)]
struct ExpectedObjectV1 {
  identity: IndexRuntimeWorkspaceIdentityV1,
  kind: IndexWorkspaceObjectKindV1,
  object_id: [u8; 16],
  object_sequence: Option<u64>,
  created_at_ms: Option<u64>,
  payload: Option<ExpectedObjectPayloadV1>,
}

#[derive(Debug, Clone, Copy)]
struct ExpectedObjectPayloadV1 {
  payload_length: usize,
  logical_record_count: u64,
  minimum_publication_sequence: u64,
  maximum_publication_sequence: u64,
  payload_digest: [u8; 32],
}

impl ExpectedObjectV1 {
  fn exact(
    identity: IndexRuntimeWorkspaceIdentityV1,
    object_id: [u8; 16],
    object_sequence: u64,
    created_at_ms: u64,
    plan: WorkspaceObjectPlanV1,
  ) -> Self {
    Self {
      identity,
      kind: plan.kind,
      object_id,
      object_sequence: Some(object_sequence),
      created_at_ms: Some(created_at_ms),
      payload: Some(plan.into()),
    }
  }

  fn durable_retry(
    identity: IndexRuntimeWorkspaceIdentityV1,
    object_id: [u8; 16],
    object_sequence: u64,
    plan: WorkspaceObjectPlanV1,
  ) -> Self {
    Self { identity, kind: plan.kind, object_id, object_sequence: Some(object_sequence), created_at_ms: None, payload: Some(plan.into()) }
  }

  fn manifest_bound(
    identity: IndexRuntimeWorkspaceIdentityV1,
    kind: IndexWorkspaceObjectKindV1,
    object_id: [u8; 16],
    object_sequence: u64,
    created_at_ms: u64,
  ) -> Self {
    Self { identity, kind, object_id, object_sequence: Some(object_sequence), created_at_ms: Some(created_at_ms), payload: None }
  }

  fn payload_bound(identity: IndexRuntimeWorkspaceIdentityV1, object_id: [u8; 16], plan: WorkspaceObjectPlanV1) -> Self {
    Self { identity, kind: plan.kind, object_id, object_sequence: None, created_at_ms: None, payload: Some(plan.into()) }
  }

  fn exact_write(self) -> Result<(IndexWorkspaceObjectHeaderWriteV1, ExpectedObjectPayloadV1), IndexRuntimeWorkspaceStoreErrorV1> {
    let object_sequence = self
      .object_sequence
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::State("workspace writer requires an exact object sequence".to_string()))?;
    let created_at_ms = self
      .created_at_ms
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::State("workspace writer requires an exact creation timestamp".to_string()))?;
    let payload = self
      .payload
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::State("workspace writer requires an exact payload contract".to_string()))?;
    Ok((
      IndexWorkspaceObjectHeaderWriteV1 {
        kind: self.kind,
        hash_algorithm: self.identity.hash_algorithm,
        database_id: self.identity.database_id,
        destination_physical_instance_id: self.identity.destination_physical_instance_id,
        workspace_id: self.identity.workspace_id,
        runtime_id: self.identity.runtime_id,
        object_id: self.object_id,
        object_sequence,
        created_at_ms,
        payload_length: payload.payload_length,
        logical_record_count: payload.logical_record_count,
        minimum_publication_sequence: payload.minimum_publication_sequence,
        maximum_publication_sequence: payload.maximum_publication_sequence,
        payload_digest: payload.payload_digest,
      },
      payload,
    ))
  }
}

impl From<WorkspaceObjectPlanV1> for ExpectedObjectPayloadV1 {
  fn from(plan: WorkspaceObjectPlanV1) -> Self {
    Self {
      payload_length: plan.payload_length,
      logical_record_count: plan.logical_record_count,
      minimum_publication_sequence: plan.minimum_publication_sequence,
      maximum_publication_sequence: plan.maximum_publication_sequence,
      payload_digest: plan.payload_digest,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ValidatedObjectV1 {
  digest: [u8; 32],
  stored_bytes: u64,
  object_sequence: u64,
  created_at_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct ObservedManifestV1 {
  manifest: IndexWorkspaceManifestV1,
  digest: [u8; 32],
}

fn validate_limits(maximum_stored_bytes: u64, maximum_object_count: u64) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  if maximum_stored_bytes < (INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1 + 5 + INDEX_WORKSPACE_MANIFEST_LENGTH_V1) as u64
    || maximum_object_count == 0
    || maximum_object_count > MAX_WORKSPACE_OBJECTS_V1
  {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Invalid(
      "workspace byte cap is too small or object-count cap is outside 1..=1048576".to_string(),
    ));
  }
  Ok(())
}

fn create_workspace_root(
  database_path: &Path,
  identity: IndexRuntimeWorkspaceIdentityV1,
  options: &IndexRuntimeWorkspaceOptionsV1,
) -> Result<PathBuf, IndexRuntimeWorkspaceStoreErrorV1> {
  if let Some(base) = &options.scratch_root {
    validate_existing_directory(base, "index runtime scratch root")?;
    ensure_capacity(base, 0, options.minimum_free_bytes)?;
    let database_directory = base.join(hex::encode(identity.database_id));
    if path_present(&database_directory)? {
      validate_private_directory(&database_directory, "index runtime database workspace directory")?;
    } else {
      create_private_directory_synced(&database_directory, base)?;
    }
    let workspace = database_directory.join(hex::encode(identity.workspace_id));
    create_private_directory_synced(&workspace, &database_directory)?;
    return Ok(workspace);
  }
  let parent = database_path.parent().ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Path("database path has no parent".to_string()))?;
  ensure_capacity(parent, 0, options.minimum_free_bytes)?;
  let file_name = database_path
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Path("database filename is not canonical UTF-8".to_string()))?;
  let workspace =
    parent.join(format!(".{file_name}-index-runtime-{}-{}", hex::encode(identity.database_id), hex::encode(identity.workspace_id)));
  create_private_directory_synced(&workspace, parent)?;
  Ok(workspace)
}

fn object_stored_bytes(payload_length: usize) -> Result<u64, IndexRuntimeWorkspaceStoreErrorV1> {
  let total = INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1
    .checked_add(payload_length)
    .and_then(|bytes| bytes.checked_add(4))
    .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace object length overflow".to_string()))?;
  if total > INDEX_WORKSPACE_OBJECT_MAX_LENGTH_V1 {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace object exceeds the frozen maximum".to_string()));
  }
  u64::try_from(total).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("workspace object length exceeds u64: {error}")))
}

fn object_path(directory: &Path, object_id: [u8; 16]) -> PathBuf {
  directory.join(format!("{}.aiwo", hex::encode(object_id)))
}

fn manifest_path(directory: &Path, sequence: u64) -> PathBuf {
  directory.join(format!("{sequence:016x}.aiwm"))
}

fn write_runtime_object(
  path: &Path,
  expected: &ExpectedObjectV1,
  batch: &FrozenIndexBatchV1,
  plan: super::index_runtime_workspace_payload::IndexWorkspaceRuntimeBatchPlanV1,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<ValidatedObjectV1, IndexRuntimeWorkspaceStoreErrorV1> {
  write_object_file(path, expected, cancellation, memory, |file, crc, payload_digest, object_digest| {
    stream_index_workspace_runtime_batch_payload_v1::<IndexRuntimeWorkspaceStoreErrorV1>(batch, plan, |chunk| {
      write_hashed(file, chunk, cancellation, crc, object_digest, Some(payload_digest))
    })
  })
}

fn write_buffered_object(
  path: &Path,
  expected: &ExpectedObjectV1,
  payload: &[u8],
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<ValidatedObjectV1, IndexRuntimeWorkspaceStoreErrorV1> {
  write_object_file(path, expected, cancellation, memory, |file, crc, payload_digest, object_digest| {
    write_hashed(file, payload, cancellation, crc, object_digest, Some(payload_digest))
  })
}

fn write_object_file(
  path: &Path,
  expected: &ExpectedObjectV1,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
  write_payload: impl FnOnce(
    &mut fs::File,
    &mut crc32fast::Hasher,
    &mut blake3::Hasher,
    &mut blake3::Hasher,
  ) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1>,
) -> Result<ValidatedObjectV1, IndexRuntimeWorkspaceStoreErrorV1> {
  if path_present(path)? {
    return validate_object_file(path, expected, cancellation, memory);
  }
  let (header_write, expected_payload) = expected.exact_write()?;
  let header = encode_index_workspace_object_header_v1(&header_write)?;
  let stored_bytes = object_stored_bytes(expected_payload.payload_length)?;
  let parent = path.parent().ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Path("workspace object has no parent".to_string()))?;
  validate_private_directory(parent, "index runtime object parent")?;
  let pending = parent.join(format!(".{}.pending", uuid::Uuid::new_v4()));
  let result: Result<ValidatedObjectV1, IndexRuntimeWorkspaceStoreErrorV1> = (|| {
    let mut file =
      create_new_regular_file_no_follow(&pending).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Path(error.to_string()))?;
    secure_platform_private_regular_file(&pending)?;
    validate_private_regular_file(&pending, &file, "pending index runtime object")?;
    preallocate_file(&file, stored_bytes).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Durability(Box::new(error)))?;
    let mut crc = crc32fast::Hasher::new();
    let mut object_digest = blake3::Hasher::new();
    write_hashed(&mut file, &header, cancellation, &mut crc, &mut object_digest, None)?;
    let mut payload_digest = blake3::Hasher::new();
    write_payload(&mut file, &mut crc, &mut payload_digest, &mut object_digest)?;
    if *payload_digest.finalize().as_bytes() != expected_payload.payload_digest {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("streamed payload digest disagrees with its plan".to_string()));
    }
    let checksum = crc.finalize().to_le_bytes();
    write_plain(&mut file, &checksum, cancellation, "workspace object checksum")?;
    object_digest.update(&checksum);
    sync_file_all_native(&file).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Durability(Box::new(error)))?;
    drop(file);
    durable_install_new_native(&pending, path).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Durability(Box::new(error)))?;
    Ok(ValidatedObjectV1 {
      digest: *object_digest.finalize().as_bytes(),
      stored_bytes,
      object_sequence: header_write.object_sequence,
      created_at_ms: header_write.created_at_ms,
    })
  })();
  match result {
    Ok(written) => {
      let observed = validate_object_file(path, expected, cancellation, memory)?;
      if observed != written {
        return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("workspace object readback disagrees with streamed bytes".to_string()));
      }
      Ok(observed)
    }
    Err(primary) => {
      if path_present(path)? {
        match validate_object_file(path, expected, cancellation, memory) {
          Ok(observed) => {
            cleanup_pending_after_exact_install(&pending, &primary)?;
            return Ok(observed);
          }
          Err(reopen) => {
            let uncertain = IndexRuntimeWorkspaceStoreErrorV1::Uncertain { primary: primary.to_string(), reopen: reopen.to_string() };
            return cleanup_pending(&pending, uncertain);
          }
        }
      }
      cleanup_pending(&pending, primary)
    }
  }
}

fn validate_object_file(
  path: &Path,
  expected: &ExpectedObjectV1,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<ValidatedObjectV1, IndexRuntimeWorkspaceStoreErrorV1> {
  check_cancellation(cancellation)?;
  let mut file = open_regular_file_no_follow(path).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Path(error.to_string()))?;
  validate_private_regular_file(path, &file, "index runtime workspace object")?;
  let metadata =
    file.metadata().map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation: "workspace object metadata", source })?;
  let actual_length = usize::try_from(metadata.len())
    .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("workspace object length exceeds usize: {error}")))?;
  if actual_length > INDEX_WORKSPACE_OBJECT_MAX_LENGTH_V1 {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Capacity("workspace object exceeds the frozen maximum".to_string()));
  }
  let mut header_bytes = [0u8; INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1];
  read_exact_cancellable(&mut file, &mut header_bytes, cancellation, "workspace object header")?;
  let header = decode_index_workspace_object_header_v1(&header_bytes, actual_length)?;
  validate_object_header(&header, expected)?;
  let mut crc = crc32fast::Hasher::new();
  crc.update(&header_bytes);
  let mut payload_digest = blake3::Hasher::new();
  let mut object_digest = blake3::Hasher::new();
  object_digest.update(&header_bytes);
  match header.kind {
    IndexWorkspaceObjectKindV1::RuntimeBatch => {
      validate_runtime_payload_stream(&mut file, &header, cancellation, memory, &mut crc, &mut payload_digest, &mut object_digest)?
    }
    IndexWorkspaceObjectKindV1::ProducerTask => {
      validate_producer_payload_stream(&mut file, &header, cancellation, memory, &mut crc, &mut payload_digest, &mut object_digest)?
    }
  }
  if *payload_digest.finalize().as_bytes() != header.payload_digest {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("workspace object payload digest does not match".to_string()));
  }
  let mut checksum = [0u8; 4];
  read_exact_cancellable(&mut file, &mut checksum, cancellation, "workspace object checksum")?;
  if u32::from_le_bytes(checksum) != crc.finalize() {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("workspace object CRC32 does not match".to_string()));
  }
  object_digest.update(&checksum);
  let mut trailing = [0u8; 1];
  if file
    .read(&mut trailing)
    .map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation: "workspace object trailing probe", source })?
    != 0
  {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("workspace object contains trailing bytes".to_string()));
  }
  if file.metadata().map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation: "workspace object final metadata", source })?.len()
    != metadata.len()
  {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("workspace object length changed while reading".to_string()));
  }
  Ok(ValidatedObjectV1 {
    digest: *object_digest.finalize().as_bytes(),
    stored_bytes: metadata.len(),
    object_sequence: header.object_sequence,
    created_at_ms: header.created_at_ms,
  })
}

#[allow(clippy::too_many_arguments)]
fn validate_runtime_payload_stream(
  file: &mut fs::File,
  object: &IndexWorkspaceObjectHeaderV1,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
  crc: &mut crc32fast::Hasher,
  payload_digest: &mut blake3::Hasher,
  object_digest: &mut blake3::Hasher,
) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  let mut batch_header = [0u8; RUNTIME_BATCH_HEADER_LENGTH];
  read_payload_bytes(file, &mut batch_header, cancellation, crc, payload_digest, object_digest)?;
  let decoded_header = decode_index_workspace_runtime_batch_stream_header_v1(&batch_header, object.hash_algorithm, object.payload_length)?;
  let mut consumed = RUNTIME_BATCH_HEADER_LENGTH;
  let mut prior_key: Option<(Vec<u8>, MemoryReservation)> = None;
  let mut minimum_publication_sequence = u64::MAX;
  let mut maximum_publication_sequence = 0u64;
  for _ in 0..decoded_header.record_count {
    check_cancellation(cancellation)?;
    let mut fixed = [0u8; RUNTIME_MUTATION_FRAME_LENGTH];
    read_payload_bytes(file, &mut fixed, cancellation, crc, payload_digest, object_digest)?;
    let (frame_length, order_length) = validate_index_workspace_runtime_mutation_frame_header_v1(&fixed, object.hash_algorithm)?;
    if frame_length < RUNTIME_MUTATION_FRAME_LENGTH || consumed.checked_add(frame_length).is_none_or(|end| end > object.payload_length) {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("runtime mutation frame is outside the declared payload".to_string()));
    }
    let frame_bytes = u64::try_from(frame_length)
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("runtime frame length exceeds u64: {error}")))?;
    let order_bytes = u64::try_from(order_length)
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("runtime order-key length exceeds u64: {error}")))?;
    let reservation_bytes = frame_bytes
      .checked_add(order_bytes)
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("runtime frame reservation overflow".to_string()))?;
    let frame_reservation = memory
      .reserve(MemoryOwner::IndexDirtyBuffers, reservation_bytes, AdmissionClass::Maintenance)
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Memory(Box::new(error)))?;
    let mut frame = Vec::new();
    frame
      .try_reserve_exact(frame_length)
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Allocation(format!("runtime frame allocation failed: {error}")))?;
    frame.resize(frame_length, 0);
    frame[..RUNTIME_MUTATION_FRAME_LENGTH].copy_from_slice(&fixed);
    read_payload_bytes(file, &mut frame[RUNTIME_MUTATION_FRAME_LENGTH..], cancellation, crc, payload_digest, object_digest)?;
    let record = decode_index_workspace_runtime_mutation_frame_v1(&frame, object.hash_algorithm)?;
    if let Some((prior, _reservation)) = &prior_key {
      if compare_previous_runtime_sort_key(prior, object.hash_algorithm.hash_length(), &record)? != Ordering::Less {
        return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("runtime mutations are not in strict canonical order".to_string()));
      }
    }
    let key_length = record
      .index_id
      .len()
      .checked_add(1)
      .and_then(|length| length.checked_add(record.order_key.len()))
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("runtime prior-key length overflow".to_string()))?;
    let key_reservation = memory
      .reserve(
        MemoryOwner::IndexDirtyBuffers,
        u64::try_from(key_length)
          .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("runtime prior-key length exceeds u64: {error}")))?,
        AdmissionClass::Maintenance,
      )
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Memory(Box::new(error)))?;
    let mut key = Vec::new();
    key
      .try_reserve_exact(key_length)
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Allocation(format!("runtime prior-key allocation failed: {error}")))?;
    key.extend_from_slice(record.index_id);
    key.push(record.role.id());
    key.extend_from_slice(record.order_key);
    minimum_publication_sequence = minimum_publication_sequence.min(record.publication_sequence);
    maximum_publication_sequence = maximum_publication_sequence.max(record.publication_sequence);
    prior_key = Some((key, key_reservation));
    drop(frame_reservation);
    consumed = consumed
      .checked_add(frame_length)
      .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("runtime payload consumed-byte total overflow".to_string()))?;
  }
  if consumed != object.payload_length
    || decoded_header.record_count as u64 != object.logical_record_count
    || minimum_publication_sequence != object.minimum_publication_sequence
    || maximum_publication_sequence != object.maximum_publication_sequence
  {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(
      "runtime payload count, bytes, or publication bounds do not close over its object".to_string(),
    ));
  }
  Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_producer_payload_stream(
  file: &mut fs::File,
  object: &IndexWorkspaceObjectHeaderV1,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
  crc: &mut crc32fast::Hasher,
  payload_digest: &mut blake3::Hasher,
  object_digest: &mut blake3::Hasher,
) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  let (minimum_length, maximum_length) = index_workspace_producer_task_payload_bounds_v1(object.hash_algorithm)?;
  if object.payload_length < minimum_length || object.payload_length > maximum_length {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("producer-task object payload is outside its frozen bounds".to_string()));
  }
  let payload_bytes = u64::try_from(object.payload_length)
    .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("producer payload length exceeds u64: {error}")))?;
  let _reservation = memory
    .reserve(MemoryOwner::IndexDirtyBuffers, payload_bytes, AdmissionClass::Maintenance)
    .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Memory(Box::new(error)))?;
  let mut payload = Vec::new();
  payload
    .try_reserve_exact(object.payload_length)
    .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Allocation(format!("producer payload allocation failed: {error}")))?;
  payload.resize(object.payload_length, 0);
  read_payload_bytes(file, &mut payload, cancellation, crc, payload_digest, object_digest)?;
  let decoded = decode_index_workspace_producer_task_payload_v1(&payload, object.hash_algorithm)?;
  if object.logical_record_count != 1
    || decoded.publication_sequence != object.minimum_publication_sequence
    || decoded.publication_sequence != object.maximum_publication_sequence
  {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("producer-task payload counters do not close over its object".to_string()));
  }
  Ok(())
}

fn compare_previous_runtime_sort_key(
  bytes: &[u8],
  hash_width: usize,
  current: &super::index_runtime_workspace::IndexWorkspaceRuntimeMutationV1<'_>,
) -> Result<Ordering, IndexRuntimeWorkspaceStoreErrorV1> {
  if bytes.len() <= hash_width {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("prior runtime sort key is truncated".to_string()));
  }
  let role = super::index_page::OrderedIndexRoleV1::from_id(bytes[hash_width])
    .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Format("prior runtime sort-key role is unknown".to_string()))?;
  Ok(bytes[..hash_width].cmp(current.index_id).then(role.id().cmp(&current.role.id())).then(bytes[hash_width + 1..].cmp(current.order_key)))
}

fn validate_object_header(
  header: &IndexWorkspaceObjectHeaderV1,
  expected: &ExpectedObjectV1,
) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  if header.kind != expected.kind
    || header.hash_algorithm != expected.identity.hash_algorithm
    || header.database_id != expected.identity.database_id
    || header.destination_physical_instance_id != expected.identity.destination_physical_instance_id
    || header.workspace_id != expected.identity.workspace_id
    || header.runtime_id != expected.identity.runtime_id
    || header.object_id != expected.object_id
    || expected.object_sequence.is_some_and(|object_sequence| header.object_sequence != object_sequence)
    || expected.created_at_ms.is_some_and(|created_at_ms| header.created_at_ms != created_at_ms)
    || expected.payload.is_some_and(|payload| {
      header.payload_length != payload.payload_length
        || header.logical_record_count != payload.logical_record_count
        || header.minimum_publication_sequence != payload.minimum_publication_sequence
        || header.maximum_publication_sequence != payload.maximum_publication_sequence
        || header.payload_digest != payload.payload_digest
    })
  {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(
      "workspace object header disagrees with its manifest or append request".to_string(),
    ));
  }
  Ok(())
}

fn validate_manifest_identity(
  manifest: &IndexWorkspaceManifestV1,
  database_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  workspace_id: [u8; 16],
  runtime_id: [u8; 16],
  sequence: u64,
) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  if manifest.database_id != database_id
    || manifest.destination_physical_instance_id != destination_physical_instance_id
    || manifest.workspace_id != workspace_id
    || manifest.runtime_id != runtime_id
    || manifest.manifest_sequence != sequence
  {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(format!("manifest {sequence} has foreign or discontinuous identity")));
  }
  Ok(())
}

fn read_manifest(path: &Path, cancellation: &CancellationToken) -> Result<ObservedManifestV1, IndexRuntimeWorkspaceStoreErrorV1> {
  check_cancellation(cancellation)?;
  let mut file = open_regular_file_no_follow(path).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Path(error.to_string()))?;
  validate_private_regular_file(path, &file, "index runtime workspace manifest")?;
  let metadata =
    file.metadata().map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation: "workspace manifest metadata", source })?;
  if metadata.len() != INDEX_WORKSPACE_MANIFEST_LENGTH_V1 as u64 {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("workspace manifest length is not 208 bytes".to_string()));
  }
  let mut bytes = [0u8; INDEX_WORKSPACE_MANIFEST_LENGTH_V1];
  read_exact_cancellable(&mut file, &mut bytes, cancellation, "workspace manifest bytes")?;
  let manifest = decode_index_workspace_manifest_v1(&bytes)?;
  let digest = index_workspace_manifest_digest_v1(&bytes)?;
  Ok(ObservedManifestV1 { manifest, digest })
}

fn write_immutable_bytes(path: &Path, bytes: &[u8], cancellation: &CancellationToken) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  if path_present(path)? {
    let observed = read_exact_private_file(path, bytes.len(), cancellation)?;
    if observed == bytes {
      return Ok(());
    }
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(format!("existing immutable artifact conflicts: {}", path.display())));
  }
  let parent = path.parent().ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Path("immutable artifact has no parent".to_string()))?;
  validate_private_directory(parent, "immutable index runtime artifact parent")?;
  let pending = parent.join(format!(".{}.pending", uuid::Uuid::new_v4()));
  let result: Result<(), IndexRuntimeWorkspaceStoreErrorV1> = (|| {
    let mut file =
      create_new_regular_file_no_follow(&pending).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Path(error.to_string()))?;
    secure_platform_private_regular_file(&pending)?;
    validate_private_regular_file(&pending, &file, "pending immutable index runtime artifact")?;
    let length = u64::try_from(bytes.len())
      .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("immutable artifact length exceeds u64: {error}")))?;
    preallocate_file(&file, length).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Durability(Box::new(error)))?;
    write_plain(&mut file, bytes, cancellation, "immutable artifact bytes")?;
    sync_file_all_native(&file).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Durability(Box::new(error)))?;
    drop(file);
    durable_install_new_native(&pending, path).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Durability(Box::new(error)))?;
    Ok(())
  })();
  match result {
    Ok(()) => {
      if read_exact_private_file(path, bytes.len(), cancellation)? != bytes {
        return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("immutable artifact readback disagrees with requested bytes".to_string()));
      }
      Ok(())
    }
    Err(primary) => {
      if path_present(path)? {
        match read_exact_private_file(path, bytes.len(), cancellation) {
          Ok(observed) if observed == bytes => {
            cleanup_pending_after_exact_install(&pending, &primary)?;
            return Ok(());
          }
          Ok(_) => {
            let uncertain = IndexRuntimeWorkspaceStoreErrorV1::Uncertain {
              primary: primary.to_string(),
              reopen: "installed immutable bytes conflict with the requested artifact".to_string(),
            };
            return cleanup_pending(&pending, uncertain);
          }
          Err(reopen) => {
            let uncertain = IndexRuntimeWorkspaceStoreErrorV1::Uncertain { primary: primary.to_string(), reopen: reopen.to_string() };
            return cleanup_pending(&pending, uncertain);
          }
        }
      }
      cleanup_pending(&pending, primary)
    }
  }
}

fn read_exact_private_file(
  path: &Path,
  length: usize,
  cancellation: &CancellationToken,
) -> Result<Vec<u8>, IndexRuntimeWorkspaceStoreErrorV1> {
  let mut file = open_regular_file_no_follow(path).map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Path(error.to_string()))?;
  validate_private_regular_file(path, &file, "immutable index runtime readback")?;
  if file.metadata().map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation: "immutable readback metadata", source })?.len()
    != length as u64
  {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Format("immutable readback length disagrees".to_string()));
  }
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(length)
    .map_err(|error| IndexRuntimeWorkspaceStoreErrorV1::Allocation(format!("immutable readback allocation failed: {error}")))?;
  bytes.resize(length, 0);
  read_exact_cancellable(&mut file, &mut bytes, cancellation, "immutable readback bytes")?;
  Ok(bytes)
}

fn enforce_selected_bytes(durable_bytes: u64, sequence: u64, maximum: u64) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  let manifests = sequence
    .checked_mul(INDEX_WORKSPACE_MANIFEST_LENGTH_V1 as u64)
    .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("selected manifest byte total overflow".to_string()))?;
  let total = durable_bytes
    .checked_add(manifests)
    .ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity("selected workspace byte total overflow".to_string()))?;
  if total > maximum {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("selected workspace bytes {total} exceed cap {maximum}")));
  }
  Ok(())
}

fn write_hashed(
  file: &mut fs::File,
  bytes: &[u8],
  cancellation: &CancellationToken,
  crc: &mut crc32fast::Hasher,
  object_digest: &mut blake3::Hasher,
  mut payload_digest: Option<&mut blake3::Hasher>,
) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  for chunk in bytes.chunks(IO_CHUNK_BYTES) {
    check_cancellation(cancellation)?;
    file.write_all(chunk).map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation: "workspace object bytes", source })?;
    crc.update(chunk);
    object_digest.update(chunk);
    if let Some(payload_digest) = &mut payload_digest {
      payload_digest.update(chunk);
    }
  }
  Ok(())
}

fn write_plain(
  file: &mut fs::File,
  bytes: &[u8],
  cancellation: &CancellationToken,
  operation: &'static str,
) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  for chunk in bytes.chunks(IO_CHUNK_BYTES) {
    check_cancellation(cancellation)?;
    file.write_all(chunk).map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation, source })?;
  }
  Ok(())
}

fn read_payload_bytes(
  file: &mut fs::File,
  bytes: &mut [u8],
  cancellation: &CancellationToken,
  crc: &mut crc32fast::Hasher,
  payload_digest: &mut blake3::Hasher,
  object_digest: &mut blake3::Hasher,
) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  read_exact_cancellable(file, bytes, cancellation, "workspace object payload")?;
  crc.update(bytes);
  payload_digest.update(bytes);
  object_digest.update(bytes);
  Ok(())
}

fn read_exact_cancellable(
  file: &mut fs::File,
  bytes: &mut [u8],
  cancellation: &CancellationToken,
  operation: &'static str,
) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  let mut offset = 0usize;
  while offset < bytes.len() {
    check_cancellation(cancellation)?;
    let end = (offset + IO_CHUNK_BYTES).min(bytes.len());
    let read = file.read(&mut bytes[offset..end]).map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Io { operation, source })?;
    if read == 0 {
      return Err(IndexRuntimeWorkspaceStoreErrorV1::Format(format!("{operation} was truncated")));
    }
    offset =
      offset.checked_add(read).ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Capacity(format!("{operation} read count overflow")))?;
  }
  Ok(())
}

fn cleanup_pending<T>(path: &Path, primary: IndexRuntimeWorkspaceStoreErrorV1) -> Result<T, IndexRuntimeWorkspaceStoreErrorV1> {
  match fs::remove_file(path) {
    Ok(()) => Err(primary),
    Err(source) if source.kind() == std::io::ErrorKind::NotFound => Err(primary),
    Err(source) => Err(IndexRuntimeWorkspaceStoreErrorV1::Cleanup { primary: primary.to_string(), source }),
  }
}

fn cleanup_pending_after_exact_install(
  path: &Path,
  primary: &IndexRuntimeWorkspaceStoreErrorV1,
) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  match fs::remove_file(path) {
    Ok(()) => {}
    Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
    Err(source) => Err(IndexRuntimeWorkspaceStoreErrorV1::Cleanup {
      primary: format!("{primary}; exact installed artifact validated but its pending name remains"),
      source,
    })?,
  };
  let parent =
    path.parent().ok_or_else(|| IndexRuntimeWorkspaceStoreErrorV1::Path("pending immutable artifact has no parent".to_string()))?;
  sync_directory_native(parent).map_err(|source| IndexRuntimeWorkspaceStoreErrorV1::Uncertain {
    primary: format!("{primary}; exact installed artifact validated and its pending name was removed"),
    reopen: format!("pending-name parent durability failed: {source}"),
  })
}

fn path_present(path: &Path) -> Result<bool, IndexRuntimeWorkspaceStoreErrorV1> {
  match fs::symlink_metadata(path) {
    Ok(_) => Ok(true),
    Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
    Err(source) => Err(IndexRuntimeWorkspaceStoreErrorV1::Io { operation: "workspace path presence", source }),
  }
}

fn validate_canonical_native_path(path: &Path, role: &str) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  if !path.is_absolute()
    || path.to_str().is_none()
    || path.components().any(|component| matches!(component, Component::CurDir | Component::ParentDir))
  {
    return Err(IndexRuntimeWorkspaceStoreErrorV1::Invalid(format!("{role} is not a canonical absolute UTF-8 path")));
  }
  Ok(())
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), IndexRuntimeWorkspaceStoreErrorV1> {
  if cancellation.is_cancelled() {
    Err(IndexRuntimeWorkspaceStoreErrorV1::Canceled)
  } else {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::{INDEX_WORKSPACE_MANIFEST_LENGTH_V1, incremental_append_allocation_bytes};

  #[test]
  fn exact_orphan_retry_admits_only_new_manifest_allocation() {
    let object_bytes = 8 * 1024 * 1024;
    assert_eq!(incremental_append_allocation_bytes(object_bytes, true).unwrap(), INDEX_WORKSPACE_MANIFEST_LENGTH_V1 as u64);
    assert_eq!(incremental_append_allocation_bytes(object_bytes, false).unwrap(), object_bytes + INDEX_WORKSPACE_MANIFEST_LENGTH_V1 as u64);
  }
}
