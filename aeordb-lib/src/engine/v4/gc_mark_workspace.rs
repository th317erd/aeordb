//! Private durable scratch ownership for one v4 mark checkpoint.
//!
//! This module writes external AGWO/AGCW closures only. It does not select a
//! checkpoint, alter database authority, or reclaim storage.

use std::fmt::{self, Write as _};
use std::fs;
use std::io::{Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::gc_mark::{
  MarkResumeContextV1, MarkRunCheckpointV1, MarkWorkspaceDescriptorV1, MarkWorkspaceManifestV1, MarkWorkspaceObjectKindV1,
  WORKSPACE_MANIFEST_MAX, WORKSPACE_OBJECT_HEADER, WORKSPACE_OBJECT_MAX, decode_mark_workspace_manifest, decode_mark_workspace_object,
  validate_mark_checkpoint_resume_context, validate_mark_resume_context, validate_mark_workspace_body, validate_mark_workspace_object,
};
use super::private_workspace::{
  PrivateWorkspaceErrorV1, create_private_directory_synced, ensure_capacity, secure_platform_private_regular_file,
  validate_existing_directory, validate_private_directory, validate_private_directory_readonly, validate_private_regular_file,
  validate_regular_database_path,
};
use crate::engine::HashAlgorithm;
use crate::engine::emergency_spill::{create_new_regular_file_no_follow, open_regular_file_no_follow};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::{
  NativeDurabilityError, durable_install_new_native, preallocate_file, sync_directory_native, sync_file_all_native,
};

const WORKSPACE_SCHEMA: u16 = 1;
const OBJECT_IO_CHUNK_BYTES: usize = 64 * 1024;
const MANIFEST_DESCRIPTOR_BYTES: usize = 68;
const MANIFEST_FIXED_BYTES: usize = 120;
const MANIFEST_CRC_BYTES: usize = 4;

#[derive(Debug, Error)]
pub enum MarkWorkspaceErrorV1 {
  #[error("mark workspace identity is invalid: {0}")]
  Identity(&'static str),
  #[error("mark workspace path is invalid or unavailable: {0}")]
  Path(String),
  #[error("mark workspace state refuses the operation: {0}")]
  State(&'static str),
  #[error("mark workspace operation was canceled")]
  Canceled,
  #[error("mark workspace capacity is unavailable: {0}")]
  Capacity(String),
  #[error("mark workspace format is invalid: {0}")]
  Format(String),
  #[error("mark workspace memory admission failed: {0}")]
  Memory(#[source] Box<MemoryCoordinatorError>),
  #[error("mark workspace memory rollback failed after {primary}: {source}")]
  MemoryRollback {
    primary: String,
    #[source]
    source: Box<MemoryCoordinatorError>,
  },
  #[error("mark workspace allocation failed: {0}")]
  Allocation(String),
  #[error("mark workspace I/O failed during {operation}: {source}")]
  Io {
    operation: &'static str,
    #[source]
    source: std::io::Error,
  },
  #[error("mark workspace durability failed: {0}")]
  Durability(#[source] Box<NativeDurabilityError>),
}

impl MarkWorkspaceErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Identity(_) => "mark_workspace_identity",
      Self::Path(_) => "mark_workspace_path",
      Self::State(_) => "mark_workspace_state",
      Self::Canceled => "mark_workspace_cancelled",
      Self::Capacity(_) => "mark_workspace_capacity",
      Self::Format(_) => "mark_workspace_format",
      Self::Memory(_) => "mark_workspace_memory",
      Self::MemoryRollback { .. } => "mark_workspace_memory",
      Self::Allocation(_) => "mark_workspace_allocation",
      Self::Io { .. } => "mark_workspace_io",
      Self::Durability(_) => "mark_workspace_durability",
    }
  }
}

impl From<PrivateWorkspaceErrorV1> for MarkWorkspaceErrorV1 {
  fn from(error: PrivateWorkspaceErrorV1) -> Self {
    match error {
      PrivateWorkspaceErrorV1::Path(message) => Self::Path(message),
      #[cfg(windows)]
      PrivateWorkspaceErrorV1::State(message) => Self::Path(message),
      PrivateWorkspaceErrorV1::Capacity(message) => Self::Capacity(message),
      #[cfg(windows)]
      PrivateWorkspaceErrorV1::Allocation(message) => Self::Allocation(message),
      PrivateWorkspaceErrorV1::Io { operation, source } => Self::Io { operation, source },
      PrivateWorkspaceErrorV1::Durability(source) => Self::Durability(source),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkWorkspaceIdentityV1 {
  database_id: [u8; 16],
  run_id: [u8; 16],
  generation: u64,
  checkpoint_sequence: u64,
  algorithm: HashAlgorithm,
}

impl MarkWorkspaceIdentityV1 {
  pub fn new(
    database_id: [u8; 16],
    run_id: [u8; 16],
    generation: u64,
    checkpoint_sequence: u64,
    algorithm: HashAlgorithm,
  ) -> Result<Self, MarkWorkspaceErrorV1> {
    if all_zero(&database_id) || all_zero(&run_id) || generation == 0 || checkpoint_sequence == 0 {
      return Err(MarkWorkspaceErrorV1::Identity("database, run, generation, and checkpoint sequence must be nonzero"));
    }
    if !matches!(algorithm.hash_length(), 32 | 64) {
      return Err(MarkWorkspaceErrorV1::Identity("hash width is not supported by the frozen workspace format"));
    }
    Ok(Self { database_id, run_id, generation, checkpoint_sequence, algorithm })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkWorkspaceBasisV1 {
  state: u16,
  created_at_ms: u64,
  updated_at_ms: u64,
  kv_layout_fingerprint: Vec<u8>,
  authority_root_set_digest: Vec<u8>,
  effective_policy_fingerprint: [u8; 32],
}

impl MarkWorkspaceBasisV1 {
  pub fn new(
    state: u16,
    created_at_ms: u64,
    updated_at_ms: u64,
    kv_layout_fingerprint: Vec<u8>,
    authority_root_set_digest: Vec<u8>,
    effective_policy_fingerprint: [u8; 32],
  ) -> Result<Self, MarkWorkspaceErrorV1> {
    if !(1..=5).contains(&state) || created_at_ms == 0 || updated_at_ms < created_at_ms {
      return Err(MarkWorkspaceErrorV1::Identity("checkpoint state or timestamps are invalid"));
    }
    if kv_layout_fingerprint.is_empty()
      || authority_root_set_digest.is_empty()
      || all_zero(&kv_layout_fingerprint)
      || all_zero(&authority_root_set_digest)
      || all_zero(&effective_policy_fingerprint)
    {
      return Err(MarkWorkspaceErrorV1::Identity("resume fingerprints must be nonzero"));
    }
    Ok(Self { state, created_at_ms, updated_at_ms, kv_layout_fingerprint, authority_root_set_digest, effective_policy_fingerprint })
  }

  fn validate_for(&self, identity: MarkWorkspaceIdentityV1) -> Result<(), MarkWorkspaceErrorV1> {
    let width = identity.algorithm.hash_length();
    if self.kv_layout_fingerprint.len() != width || self.authority_root_set_digest.len() != width {
      return Err(MarkWorkspaceErrorV1::Identity("resume fingerprint width does not match the hash profile"));
    }
    Ok(())
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkWorkspaceOptionsV1 {
  scratch_root: Option<PathBuf>,
  maximum_stored_bytes: u64,
  minimum_free_bytes: u64,
}

impl MarkWorkspaceOptionsV1 {
  pub fn new(scratch_root: Option<PathBuf>, maximum_stored_bytes: u64, minimum_free_bytes: u64) -> Result<Self, MarkWorkspaceErrorV1> {
    if maximum_stored_bytes == 0 {
      return Err(MarkWorkspaceErrorV1::Capacity("run cap is zero".to_string()));
    }
    if scratch_root.as_ref().is_some_and(|root| !root.is_absolute()) {
      return Err(MarkWorkspaceErrorV1::Path("configured scratch root must be absolute".to_string()));
    }
    Ok(Self { scratch_root, maximum_stored_bytes, minimum_free_bytes })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkWorkspaceReopenOptionsV1 {
  maximum_stored_bytes: u64,
}

impl MarkWorkspaceReopenOptionsV1 {
  pub fn new(maximum_stored_bytes: u64) -> Result<Self, MarkWorkspaceErrorV1> {
    if maximum_stored_bytes == 0 {
      return Err(MarkWorkspaceErrorV1::Capacity("reopen run cap is zero".to_string()));
    }
    Ok(Self { maximum_stored_bytes })
  }
}

pub struct ValidatedMarkWorkspaceObjectV1 {
  bytes: Vec<u8>,
  _memory: MemoryReservation,
}

impl ValidatedMarkWorkspaceObjectV1 {
  pub fn bytes(&self) -> &[u8] {
    &self.bytes
  }
}

impl fmt::Debug for ValidatedMarkWorkspaceObjectV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("ValidatedMarkWorkspaceObjectV1").field("stored_length", &self.bytes.len()).finish()
  }
}

pub struct ReopenedMarkWorkspaceV1 {
  checkpoint_directory: PathBuf,
  manifest_path: PathBuf,
  manifest_bytes: Vec<u8>,
  manifest_digest: [u8; 32],
  object_count: usize,
  maximum_stored_bytes: u64,
  algorithm: HashAlgorithm,
  cancellation: CancellationToken,
  memory: MemoryCoordinator,
  _manifest_memory: MemoryReservation,
}

impl fmt::Debug for ReopenedMarkWorkspaceV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("ReopenedMarkWorkspaceV1")
      .field("checkpoint_directory", &self.checkpoint_directory)
      .field("manifest_path", &self.manifest_path)
      .field("manifest_digest", &hex::encode(self.manifest_digest))
      .field("object_count", &self.object_count)
      .finish()
  }
}

impl ReopenedMarkWorkspaceV1 {
  pub fn open(
    checkpoint: &MarkRunCheckpointV1<'_>,
    context: &MarkResumeContextV1<'_>,
    options: MarkWorkspaceReopenOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, MarkWorkspaceErrorV1> {
    if cancellation.is_cancelled() {
      return Err(MarkWorkspaceErrorV1::Canceled);
    }
    validate_mark_checkpoint_resume_context(checkpoint, context).map_err(|error| MarkWorkspaceErrorV1::Format(error.to_string()))?;

    let workspace_path = PathBuf::from(checkpoint.workspace_path);
    if !workspace_path.is_absolute() {
      return Err(MarkWorkspaceErrorV1::Path("checkpoint workspace path must be absolute".to_string()));
    }
    validate_private_directory_readonly(&workspace_path, "checkpoint workspace")?;
    let checkpoints_path = workspace_path.join("checkpoints");
    validate_private_directory_readonly(&checkpoints_path, "checkpoint collection")?;
    let checkpoint_directory = checkpoints_path.join(format!("{:016x}", checkpoint.checkpoint_sequence));
    validate_private_directory_readonly(&checkpoint_directory, "selected checkpoint directory")?;
    let manifest_path = checkpoint_directory.join("manifest.agcw");
    let manifest_cap = options.maximum_stored_bytes.min(WORKSPACE_MANIFEST_MAX as u64) as usize;
    let (manifest_bytes, manifest_memory) =
      read_charged_regular_file(&manifest_path, manifest_cap, 2, "workspace manifest", &cancellation, memory)?;
    let manifest = decode_mark_workspace_manifest(&manifest_bytes, context.hash_algorithm)
      .map_err(|error| MarkWorkspaceErrorV1::Format(error.to_string()))?;
    validate_mark_resume_context(checkpoint, &manifest, &manifest_bytes, context)
      .map_err(|error| MarkWorkspaceErrorV1::Format(error.to_string()))?;
    validate_manifest_object_names(&manifest)?;
    enforce_reopen_run_cap(&manifest, manifest_bytes.len(), options.maximum_stored_bytes)?;
    for descriptor in &manifest.descriptors {
      let object = read_workspace_object(&checkpoint_directory, &manifest, descriptor, context.hash_algorithm, &cancellation, memory)?;
      drop(object);
    }

    Ok(Self {
      checkpoint_directory,
      manifest_path,
      manifest_digest: *blake3::hash(&manifest_bytes).as_bytes(),
      object_count: manifest.descriptors.len(),
      maximum_stored_bytes: options.maximum_stored_bytes,
      algorithm: context.hash_algorithm,
      cancellation,
      memory: memory.clone(),
      manifest_bytes,
      _manifest_memory: manifest_memory,
    })
  }

  pub fn checkpoint_directory(&self) -> &Path {
    &self.checkpoint_directory
  }

  pub fn manifest_path(&self) -> &Path {
    &self.manifest_path
  }

  pub const fn manifest_digest(&self) -> [u8; 32] {
    self.manifest_digest
  }

  pub const fn object_count(&self) -> usize {
    self.object_count
  }

  pub fn read_object(&self, kind: MarkWorkspaceObjectKindV1, ordinal: u64) -> Result<ValidatedMarkWorkspaceObjectV1, MarkWorkspaceErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(MarkWorkspaceErrorV1::Canceled);
    }
    let manifest = decode_mark_workspace_manifest(&self.manifest_bytes, self.algorithm)
      .map_err(|error| MarkWorkspaceErrorV1::Format(error.to_string()))?;
    enforce_reopen_run_cap(&manifest, self.manifest_bytes.len(), self.maximum_stored_bytes)?;
    let descriptor = manifest
      .descriptors
      .iter()
      .find(|descriptor| descriptor.kind == kind && descriptor.ordinal == ordinal)
      .ok_or(MarkWorkspaceErrorV1::State("requested object is absent from the selected manifest"))?;
    validate_descriptor_object_name(descriptor)?;
    read_workspace_object(&self.checkpoint_directory, &manifest, descriptor, self.algorithm, &self.cancellation, &self.memory)
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableMarkWorkspaceDescriptorV1 {
  kind: MarkWorkspaceObjectKindV1,
  ordinal: u64,
  stored_length: u64,
  logical_record_count: u64,
  digest: [u8; 32],
  name: String,
}

impl DurableMarkWorkspaceDescriptorV1 {
  pub const fn digest(&self) -> [u8; 32] {
    self.digest
  }

  fn order_key(&self) -> (MarkWorkspaceObjectKindV1, u64) {
    (self.kind, self.ordinal)
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableMarkWorkspaceClosureV1 {
  workspace_path: PathBuf,
  checkpoint_directory: PathBuf,
  manifest_path: PathBuf,
  manifest_digest: [u8; 32],
  object_count: u16,
  object_stored_bytes: u64,
  logical_record_count: u64,
}

impl DurableMarkWorkspaceClosureV1 {
  pub fn workspace_path(&self) -> &Path {
    &self.workspace_path
  }

  pub fn checkpoint_workspace_path(&self) -> Result<String, MarkWorkspaceErrorV1> {
    canonical_checkpoint_workspace_path(&self.workspace_path)
  }

  pub fn checkpoint_directory(&self) -> &Path {
    &self.checkpoint_directory
  }

  pub fn manifest_path(&self) -> &Path {
    &self.manifest_path
  }

  pub const fn manifest_digest(&self) -> [u8; 32] {
    self.manifest_digest
  }

  pub const fn object_count(&self) -> u16 {
    self.object_count
  }

  pub const fn object_stored_bytes(&self) -> u64 {
    self.object_stored_bytes
  }

  pub const fn logical_record_count(&self) -> u64 {
    self.logical_record_count
  }
}

fn canonical_checkpoint_workspace_path(path: &Path) -> Result<String, MarkWorkspaceErrorV1> {
  let native = path.to_str().ok_or_else(|| MarkWorkspaceErrorV1::Path("workspace path is not canonical UTF-8".to_string()))?;
  #[cfg(windows)]
  let canonical = native.replace('\\', "/");
  #[cfg(not(windows))]
  let canonical = native.to_string();
  if !super::gc_mark::canonical_workspace_path(&canonical) {
    return Err(MarkWorkspaceErrorV1::Path("workspace path cannot be represented by the frozen checkpoint format".to_string()));
  }
  Ok(canonical)
}

pub struct DurableMarkWorkspaceV1 {
  identity: MarkWorkspaceIdentityV1,
  basis: MarkWorkspaceBasisV1,
  options: MarkWorkspaceOptionsV1,
  workspace_path: PathBuf,
  checkpoint_directory: PathBuf,
  manifest_path: PathBuf,
  descriptors: Vec<DurableMarkWorkspaceDescriptorV1>,
  object_stored_bytes: u64,
  cancellation: CancellationToken,
  memory: MemoryCoordinator,
  descriptor_memory: MemoryReservation,
  closure: Option<DurableMarkWorkspaceClosureV1>,
  failed: bool,
}

struct PendingMarkWorkspaceObjectV1 {
  kind: MarkWorkspaceObjectKindV1,
  ordinal: u64,
  stored_length: u64,
  logical_record_count: u64,
  name: String,
}

impl fmt::Debug for DurableMarkWorkspaceV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("DurableMarkWorkspaceV1")
      .field("identity", &self.identity)
      .field("workspace_path", &self.workspace_path)
      .field("checkpoint_directory", &self.checkpoint_directory)
      .field("descriptors", &self.descriptors.len())
      .field("object_stored_bytes", &self.object_stored_bytes)
      .field("complete", &self.closure.is_some())
      .field("failed", &self.failed)
      .finish()
  }
}

impl DurableMarkWorkspaceV1 {
  pub fn create(
    database_path: &Path,
    identity: MarkWorkspaceIdentityV1,
    basis: MarkWorkspaceBasisV1,
    options: MarkWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, MarkWorkspaceErrorV1> {
    if cancellation.is_cancelled() {
      return Err(MarkWorkspaceErrorV1::Canceled);
    }
    basis.validate_for(identity)?;
    validate_database_path(database_path)?;
    let database_parent = database_path.parent().ok_or_else(|| MarkWorkspaceErrorV1::Path("database path has no parent".to_string()))?;
    let base = options.scratch_root.as_deref().unwrap_or(database_parent);
    validate_existing_directory(base, "scratch base")?;
    ensure_capacity(base, 0, options.minimum_free_bytes)?;

    let workspace_path = create_workspace_directories(database_path, base, identity, options.scratch_root.is_some())?;
    let checkpoints_path = workspace_path.join("checkpoints");
    create_private_directory_synced(&checkpoints_path, &workspace_path)?;
    let checkpoint_directory = checkpoints_path.join(format!("{:016x}", identity.checkpoint_sequence));
    create_private_directory_synced(&checkpoint_directory, &checkpoints_path)?;
    let manifest_path = checkpoint_directory.join("manifest.agcw");
    let descriptor_memory = memory
      .reserve(MemoryOwner::GarbageCollection, 0, AdmissionClass::Maintenance)
      .map_err(|error| MarkWorkspaceErrorV1::Memory(Box::new(error)))?;

    Ok(Self {
      identity,
      basis,
      options,
      workspace_path,
      checkpoint_directory,
      manifest_path,
      descriptors: Vec::new(),
      object_stored_bytes: 0,
      cancellation,
      memory: memory.clone(),
      descriptor_memory,
      closure: None,
      failed: false,
    })
  }

  pub fn workspace_path(&self) -> &Path {
    &self.workspace_path
  }

  pub fn manifest_path(&self) -> &Path {
    &self.manifest_path
  }

  pub const fn is_failed(&self) -> bool {
    self.failed
  }

  pub fn object_path(&self, kind: MarkWorkspaceObjectKindV1, ordinal: u64) -> PathBuf {
    self.checkpoint_directory.join(object_name(kind, ordinal))
  }

  pub fn write_object(
    &mut self,
    kind: MarkWorkspaceObjectKindV1,
    ordinal: u64,
    body: &[u8],
  ) -> Result<&DurableMarkWorkspaceDescriptorV1, MarkWorkspaceErrorV1> {
    self.preflight_open()?;
    if ordinal == 0 {
      return Err(MarkWorkspaceErrorV1::Identity("object ordinal is zero"));
    }
    let logical_record_count = validate_mark_workspace_body(body, kind, self.identity.generation, self.identity.algorithm)
      .map_err(|error| MarkWorkspaceErrorV1::Format(error.to_string()))?;
    let stored_length = WORKSPACE_OBJECT_HEADER
      .checked_add(body.len())
      .and_then(|length| length.checked_add(4))
      .ok_or_else(|| MarkWorkspaceErrorV1::Capacity("object length overflow".to_string()))?;
    if stored_length > WORKSPACE_OBJECT_MAX {
      return Err(MarkWorkspaceErrorV1::Capacity(format!("object length {stored_length} exceeds {WORKSPACE_OBJECT_MAX}")));
    }
    let stored_length_u64 =
      u64::try_from(stored_length).map_err(|_| MarkWorkspaceErrorV1::Capacity("object length exceeds u64".to_string()))?;
    let insertion = match self.descriptors.binary_search_by_key(&(kind, ordinal), DurableMarkWorkspaceDescriptorV1::order_key) {
      Ok(_) => return Err(MarkWorkspaceErrorV1::State("object identity already exists in this checkpoint")),
      Err(index) => index,
    };
    let name_length = object_name_length(kind);
    let projected_object_bytes = self
      .object_stored_bytes
      .checked_add(stored_length_u64)
      .ok_or_else(|| MarkWorkspaceErrorV1::Capacity("checkpoint object bytes overflow".to_string()))?;
    let projected_manifest = manifest_length(self.identity.algorithm.hash_length(), &self.descriptors, Some(name_length))?;
    enforce_run_cap(projected_object_bytes, projected_manifest, self.options.maximum_stored_bytes)?;
    ensure_capacity(&self.checkpoint_directory, stored_length_u64, self.options.minimum_free_bytes)?;

    let descriptor_bytes = u64::try_from(size_of::<DurableMarkWorkspaceDescriptorV1>() + name_length)
      .map_err(|_| MarkWorkspaceErrorV1::Capacity("descriptor memory exceeds u64".to_string()))?;
    self.descriptor_memory.grow(descriptor_bytes).map_err(|error| MarkWorkspaceErrorV1::Memory(Box::new(error)))?;
    let name = match try_object_name(kind, ordinal, name_length) {
      Ok(name) => name,
      Err(error) => return Err(self.rollback_descriptor_memory(descriptor_bytes, error)),
    };
    if let Err(error) = self.descriptors.try_reserve_exact(1) {
      let error = MarkWorkspaceErrorV1::Allocation(error.to_string());
      return Err(self.rollback_descriptor_memory(descriptor_bytes, error));
    }

    let path = self.object_path(kind, ordinal);
    let pending = PendingMarkWorkspaceObjectV1 { kind, ordinal, stored_length: stored_length_u64, logical_record_count, name };
    let result = self.write_object_file(&path, body, pending);
    match result {
      Ok(descriptor) => {
        self.object_stored_bytes = projected_object_bytes;
        self.descriptors.insert(insertion, descriptor);
        Ok(&self.descriptors[insertion])
      }
      Err(error) => {
        self.failed = true;
        Err(self.rollback_descriptor_memory(descriptor_bytes, error))
      }
    }
  }

  pub fn complete(&mut self) -> Result<DurableMarkWorkspaceClosureV1, MarkWorkspaceErrorV1> {
    if let Some(closure) = &self.closure {
      return Ok(closure.clone());
    }
    self.preflight_open()?;
    let logical_record_count = self.descriptors.iter().try_fold(0u64, |total, descriptor| {
      total
        .checked_add(descriptor.logical_record_count)
        .ok_or_else(|| MarkWorkspaceErrorV1::Capacity("logical record total overflow".to_string()))
    })?;
    let object_count = u16::try_from(self.descriptors.len())
      .map_err(|_| MarkWorkspaceErrorV1::Capacity("object count exceeds frozen manifest width".to_string()))?;
    let manifest_length = manifest_length(self.identity.algorithm.hash_length(), &self.descriptors, None)?;
    let manifest_length_u64 =
      u64::try_from(manifest_length).map_err(|_| MarkWorkspaceErrorV1::Capacity("manifest length exceeds u64".to_string()))?;
    enforce_run_cap(self.object_stored_bytes, manifest_length, self.options.maximum_stored_bytes)?;
    ensure_capacity(&self.checkpoint_directory, manifest_length_u64, self.options.minimum_free_bytes)?;
    for descriptor in &self.descriptors {
      let path = self.checkpoint_directory.join(&descriptor.name);
      let digest = match stream_file_digest(&path, descriptor.stored_length, &self.cancellation) {
        Ok(digest) => digest,
        Err(error) => {
          self.failed = true;
          return Err(error);
        }
      };
      if digest != descriptor.digest {
        self.failed = true;
        return Err(MarkWorkspaceErrorV1::Format(format!("object {} readback digest does not match its descriptor", descriptor.name)));
      }
    }
    let _manifest_memory = self
      .memory
      .reserve(MemoryOwner::GarbageCollection, manifest_length_u64, AdmissionClass::Maintenance)
      .map_err(|error| MarkWorkspaceErrorV1::Memory(Box::new(error)))?;
    let manifest = encode_manifest(self.identity, &self.basis, &self.descriptors, manifest_length)?;
    decode_mark_workspace_manifest(&manifest, self.identity.algorithm).map_err(|error| MarkWorkspaceErrorV1::Format(error.to_string()))?;

    let pending_path = self.checkpoint_directory.join(format!(".manifest-{}.pending", uuid::Uuid::new_v4()));
    if let Err(error) = self.write_manifest_file(&pending_path, &manifest) {
      self.failed = true;
      return Err(error);
    }
    if let Err(error) = durable_install_new_native(&pending_path, &self.manifest_path) {
      self.failed = true;
      return Err(MarkWorkspaceErrorV1::Durability(Box::new(error)));
    }
    let manifest_digest = match stream_file_digest(&self.manifest_path, manifest_length_u64, &self.cancellation) {
      Ok(digest) => digest,
      Err(error) => {
        self.failed = true;
        return Err(error);
      }
    };
    let expected_digest = *blake3::hash(&manifest).as_bytes();
    if manifest_digest != expected_digest {
      self.failed = true;
      return Err(MarkWorkspaceErrorV1::Format("manifest readback digest does not match written bytes".to_string()));
    }
    let closure = DurableMarkWorkspaceClosureV1 {
      workspace_path: self.workspace_path.clone(),
      checkpoint_directory: self.checkpoint_directory.clone(),
      manifest_path: self.manifest_path.clone(),
      manifest_digest,
      object_count,
      object_stored_bytes: self.object_stored_bytes,
      logical_record_count,
    };
    self.closure = Some(closure.clone());
    Ok(closure)
  }

  fn preflight_open(&self) -> Result<(), MarkWorkspaceErrorV1> {
    if self.failed {
      return Err(MarkWorkspaceErrorV1::State("checkpoint writer is latched after a prior file mutation failure"));
    }
    if self.closure.is_some() {
      return Err(MarkWorkspaceErrorV1::State("checkpoint closure is already complete"));
    }
    if self.cancellation.is_cancelled() {
      return Err(MarkWorkspaceErrorV1::Canceled);
    }
    Ok(())
  }

  fn rollback_descriptor_memory(&mut self, bytes: u64, primary: MarkWorkspaceErrorV1) -> MarkWorkspaceErrorV1 {
    match self.descriptor_memory.shrink(bytes) {
      Ok(()) => primary,
      Err(source) => {
        self.failed = true;
        MarkWorkspaceErrorV1::MemoryRollback { primary: primary.to_string(), source: Box::new(source) }
      }
    }
  }

  fn write_object_file(
    &mut self,
    path: &Path,
    body: &[u8],
    pending: PendingMarkWorkspaceObjectV1,
  ) -> Result<DurableMarkWorkspaceDescriptorV1, MarkWorkspaceErrorV1> {
    let object_directory = path.parent().ok_or_else(|| MarkWorkspaceErrorV1::Path("object path has no parent".to_string()))?;
    if !object_directory.exists() {
      create_private_directory_synced(object_directory, &self.checkpoint_directory)?;
    } else {
      validate_private_directory(object_directory, "object directory")?;
    }
    let mut file = create_new_regular_file_no_follow(path).map_err(|error| MarkWorkspaceErrorV1::Path(error.to_string()))?;
    secure_platform_private_regular_file(path)?;
    validate_private_regular_file(path, &file, "new workspace object")?;
    preallocate_file(&file, pending.stored_length).map_err(|error| MarkWorkspaceErrorV1::Durability(Box::new(error)))?;

    let mut header = [0u8; WORKSPACE_OBJECT_HEADER];
    header[..4].copy_from_slice(b"AGWO");
    put_u16(&mut header, 4, WORKSPACE_SCHEMA);
    put_u16(&mut header, 6, pending.kind as u16);
    put_u64(&mut header, 8, pending.stored_length);
    header[16..32].copy_from_slice(&self.identity.database_id);
    header[32..48].copy_from_slice(&self.identity.run_id);
    put_u64(&mut header, 48, self.identity.generation);
    put_u64(&mut header, 56, self.identity.checkpoint_sequence);
    put_u64(&mut header, 64, pending.ordinal);
    put_u64(
      &mut header,
      72,
      u64::try_from(body.len()).map_err(|_| MarkWorkspaceErrorV1::Capacity("object body length exceeds u64".to_string()))?,
    );

    let mut crc = crc32fast::Hasher::new();
    let mut digest = blake3::Hasher::new();
    write_chunks(&mut file, &header, &self.cancellation, &mut crc, &mut digest)?;
    write_chunks(&mut file, body, &self.cancellation, &mut crc, &mut digest)?;
    if self.cancellation.is_cancelled() {
      return Err(MarkWorkspaceErrorV1::Canceled);
    }
    let checksum = crc.finalize().to_le_bytes();
    file.write_all(&checksum).map_err(|source| MarkWorkspaceErrorV1::Io { operation: "object checksum", source })?;
    digest.update(&checksum);
    sync_file_all_native(&file).map_err(|error| MarkWorkspaceErrorV1::Durability(Box::new(error)))?;
    drop(file);
    sync_directory_native(object_directory).map_err(|error| MarkWorkspaceErrorV1::Durability(Box::new(error)))?;
    let expected_digest = *digest.finalize().as_bytes();
    let observed_digest = stream_file_digest(path, pending.stored_length, &self.cancellation)?;
    if observed_digest != expected_digest {
      return Err(MarkWorkspaceErrorV1::Format("object readback digest does not match written bytes".to_string()));
    }
    Ok(DurableMarkWorkspaceDescriptorV1 {
      kind: pending.kind,
      ordinal: pending.ordinal,
      stored_length: pending.stored_length,
      logical_record_count: pending.logical_record_count,
      digest: observed_digest,
      name: pending.name,
    })
  }

  fn write_manifest_file(&self, path: &Path, manifest: &[u8]) -> Result<(), MarkWorkspaceErrorV1> {
    let length = u64::try_from(manifest.len()).map_err(|_| MarkWorkspaceErrorV1::Capacity("manifest length exceeds u64".to_string()))?;
    let mut file = create_new_regular_file_no_follow(path).map_err(|error| MarkWorkspaceErrorV1::Path(error.to_string()))?;
    secure_platform_private_regular_file(path)?;
    validate_private_regular_file(path, &file, "new workspace manifest")?;
    preallocate_file(&file, length).map_err(|error| MarkWorkspaceErrorV1::Durability(Box::new(error)))?;
    for chunk in manifest.chunks(OBJECT_IO_CHUNK_BYTES) {
      if self.cancellation.is_cancelled() {
        return Err(MarkWorkspaceErrorV1::Canceled);
      }
      file.write_all(chunk).map_err(|source| MarkWorkspaceErrorV1::Io { operation: "manifest bytes", source })?;
    }
    sync_file_all_native(&file).map_err(|error| MarkWorkspaceErrorV1::Durability(Box::new(error)))?;
    Ok(())
  }
}

fn validate_manifest_object_names(manifest: &MarkWorkspaceManifestV1<'_>) -> Result<(), MarkWorkspaceErrorV1> {
  for descriptor in &manifest.descriptors {
    validate_descriptor_object_name(descriptor)?;
  }
  Ok(())
}

fn validate_descriptor_object_name(descriptor: &MarkWorkspaceDescriptorV1<'_>) -> Result<(), MarkWorkspaceErrorV1> {
  let expected = object_name(descriptor.kind, descriptor.ordinal);
  if descriptor.name != expected {
    return Err(MarkWorkspaceErrorV1::Format(format!("descriptor {} does not match canonical object name {expected}", descriptor.name)));
  }
  Ok(())
}

fn enforce_reopen_run_cap(
  manifest: &MarkWorkspaceManifestV1<'_>,
  manifest_length: usize,
  maximum_stored_bytes: u64,
) -> Result<(), MarkWorkspaceErrorV1> {
  let manifest_length = match u64::try_from(manifest_length) {
    Ok(length) => length,
    Err(error) => return Err(MarkWorkspaceErrorV1::Capacity(format!("manifest length exceeds u64: {error}"))),
  };
  let total = manifest
    .stored_bytes
    .checked_add(manifest_length)
    .ok_or_else(|| MarkWorkspaceErrorV1::Capacity("reopened workspace byte total overflow".to_string()))?;
  if total > maximum_stored_bytes {
    return Err(MarkWorkspaceErrorV1::Capacity(format!("reopened workspace bytes {total} exceed cap {maximum_stored_bytes}")));
  }
  Ok(())
}

fn read_workspace_object(
  checkpoint_directory: &Path,
  manifest: &MarkWorkspaceManifestV1<'_>,
  descriptor: &MarkWorkspaceDescriptorV1<'_>,
  algorithm: HashAlgorithm,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<ValidatedMarkWorkspaceObjectV1, MarkWorkspaceErrorV1> {
  validate_descriptor_object_name(descriptor)?;
  let expected_length = match usize::try_from(descriptor.stored_length) {
    Ok(length) => length,
    Err(error) => return Err(MarkWorkspaceErrorV1::Capacity(format!("workspace object length exceeds usize: {error}"))),
  };
  if expected_length > WORKSPACE_OBJECT_MAX {
    return Err(MarkWorkspaceErrorV1::Capacity(format!("workspace object length {expected_length} exceeds {WORKSPACE_OBJECT_MAX}")));
  }
  let object_directory = checkpoint_directory.join(descriptor.kind.name());
  validate_private_directory_readonly(&object_directory, "workspace object directory")?;
  let object_path = checkpoint_directory.join(descriptor.name);
  let (bytes, reservation) = read_charged_regular_file(&object_path, WORKSPACE_OBJECT_MAX, 1, "workspace object", cancellation, memory)?;
  if bytes.len() != expected_length {
    return Err(MarkWorkspaceErrorV1::Format(format!(
      "workspace object {} has {} bytes but its descriptor requires {expected_length}",
      descriptor.name,
      bytes.len()
    )));
  }
  let object = decode_mark_workspace_object(&bytes, algorithm).map_err(|error| MarkWorkspaceErrorV1::Format(error.to_string()))?;
  validate_mark_workspace_object(manifest, descriptor, &object, &bytes).map_err(|error| MarkWorkspaceErrorV1::Format(error.to_string()))?;
  Ok(ValidatedMarkWorkspaceObjectV1 { bytes, _memory: reservation })
}

fn read_charged_regular_file(
  path: &Path,
  maximum_length: usize,
  reservation_multiplier: u64,
  role: &'static str,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<(Vec<u8>, MemoryReservation), MarkWorkspaceErrorV1> {
  if cancellation.is_cancelled() {
    return Err(MarkWorkspaceErrorV1::Canceled);
  }
  let mut file = open_regular_file_no_follow(path).map_err(|error| MarkWorkspaceErrorV1::Path(error.to_string()))?;
  let metadata = file.metadata().map_err(|source| MarkWorkspaceErrorV1::Io { operation: role, source })?;
  validate_private_regular_file(path, &file, role)?;
  let length = match usize::try_from(metadata.len()) {
    Ok(length) => length,
    Err(error) => return Err(MarkWorkspaceErrorV1::Capacity(format!("{role} length exceeds usize: {error}"))),
  };
  if length > maximum_length {
    return Err(MarkWorkspaceErrorV1::Capacity(format!("{role} length {length} exceeds cap {maximum_length}")));
  }
  let reservation_length = match u64::try_from(length) {
    Ok(length) => length,
    Err(error) => return Err(MarkWorkspaceErrorV1::Capacity(format!("{role} reservation length exceeds u64: {error}"))),
  };
  let reservation_bytes = reservation_length
    .checked_mul(reservation_multiplier)
    .ok_or_else(|| MarkWorkspaceErrorV1::Capacity(format!("{role} memory reservation overflow")))?;
  let reservation = memory
    .reserve(MemoryOwner::GarbageCollection, reservation_bytes, AdmissionClass::Maintenance)
    .map_err(|error| MarkWorkspaceErrorV1::Memory(Box::new(error)))?;
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(length).map_err(|error| MarkWorkspaceErrorV1::Allocation(error.to_string()))?;
  bytes.resize(length, 0);
  let mut read_total = 0usize;
  while read_total < length {
    if cancellation.is_cancelled() {
      return Err(MarkWorkspaceErrorV1::Canceled);
    }
    let end = (read_total + OBJECT_IO_CHUNK_BYTES).min(length);
    let read = file.read(&mut bytes[read_total..end]).map_err(|source| MarkWorkspaceErrorV1::Io { operation: role, source })?;
    if read == 0 {
      return Err(MarkWorkspaceErrorV1::Format(format!("{role} was truncated while reading")));
    }
    read_total = read_total.checked_add(read).ok_or_else(|| MarkWorkspaceErrorV1::Capacity(format!("{role} read count overflow")))?;
  }
  let mut trailing = [0u8; 1];
  if file.read(&mut trailing).map_err(|source| MarkWorkspaceErrorV1::Io { operation: role, source })? != 0 {
    return Err(MarkWorkspaceErrorV1::Format(format!("{role} grew while reading")));
  }
  let final_length = file.metadata().map_err(|source| MarkWorkspaceErrorV1::Io { operation: role, source })?.len();
  if final_length != metadata.len() {
    return Err(MarkWorkspaceErrorV1::Format(format!("{role} length changed while reading")));
  }
  Ok((bytes, reservation))
}

fn create_workspace_directories(
  database_path: &Path,
  base: &Path,
  identity: MarkWorkspaceIdentityV1,
  overridden: bool,
) -> Result<PathBuf, MarkWorkspaceErrorV1> {
  let database_id = hex::encode(identity.database_id);
  let run_id = hex::encode(identity.run_id);
  if overridden {
    let database_directory = base.join(database_id);
    if database_directory.exists() {
      validate_private_directory(&database_directory, "database workspace directory")?;
    } else {
      create_private_directory_synced(&database_directory, base)?;
    }
    let workspace = database_directory.join(run_id);
    create_private_directory_synced(&workspace, &database_directory)?;
    return Ok(workspace);
  }

  let file_name = database_path
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| MarkWorkspaceErrorV1::Path("database filename is not canonical UTF-8".to_string()))?;
  let workspace = base.join(format!(".{file_name}-gc-{database_id}-{run_id}"));
  create_private_directory_synced(&workspace, base)?;
  Ok(workspace)
}

fn validate_database_path(path: &Path) -> Result<(), MarkWorkspaceErrorV1> {
  validate_regular_database_path(path, "mark workspace database").map_err(Into::into)
}

fn enforce_run_cap(object_bytes: u64, manifest_bytes: usize, maximum_bytes: u64) -> Result<(), MarkWorkspaceErrorV1> {
  let total = object_bytes
    .checked_add(u64::try_from(manifest_bytes).map_err(|_| MarkWorkspaceErrorV1::Capacity("manifest length exceeds u64".to_string()))?)
    .ok_or_else(|| MarkWorkspaceErrorV1::Capacity("run byte total overflow".to_string()))?;
  if total > maximum_bytes {
    return Err(MarkWorkspaceErrorV1::Capacity(format!("projected run bytes {total} exceed cap {maximum_bytes}")));
  }
  Ok(())
}

fn manifest_length(
  hash_width: usize,
  descriptors: &[DurableMarkWorkspaceDescriptorV1],
  additional_name_length: Option<usize>,
) -> Result<usize, MarkWorkspaceErrorV1> {
  let count = descriptors.len() + usize::from(additional_name_length.is_some());
  if count > usize::from(u16::MAX) {
    return Err(MarkWorkspaceErrorV1::Capacity("object count exceeds frozen manifest width".to_string()));
  }
  let names = descriptors.iter().try_fold(0usize, |total, descriptor| {
    total.checked_add(descriptor.name.len()).ok_or_else(|| MarkWorkspaceErrorV1::Capacity("descriptor name total overflow".to_string()))
  })?;
  let names = names
    .checked_add(additional_name_length.unwrap_or(0))
    .ok_or_else(|| MarkWorkspaceErrorV1::Capacity("descriptor name total overflow".to_string()))?;
  let length = MANIFEST_FIXED_BYTES
    .checked_add(hash_width.checked_mul(2).ok_or_else(|| MarkWorkspaceErrorV1::Capacity("manifest hash width overflow".to_string()))?)
    .and_then(|value| value.checked_add(count.checked_mul(MANIFEST_DESCRIPTOR_BYTES)?))
    .and_then(|value| value.checked_add(names))
    .and_then(|value| value.checked_add(MANIFEST_CRC_BYTES))
    .ok_or_else(|| MarkWorkspaceErrorV1::Capacity("manifest length overflow".to_string()))?;
  if length > WORKSPACE_MANIFEST_MAX {
    return Err(MarkWorkspaceErrorV1::Capacity(format!("manifest length {length} exceeds {WORKSPACE_MANIFEST_MAX}")));
  }
  Ok(length)
}

fn encode_manifest(
  identity: MarkWorkspaceIdentityV1,
  basis: &MarkWorkspaceBasisV1,
  descriptors: &[DurableMarkWorkspaceDescriptorV1],
  length: usize,
) -> Result<Vec<u8>, MarkWorkspaceErrorV1> {
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(length).map_err(|error| MarkWorkspaceErrorV1::Allocation(error.to_string()))?;
  bytes.resize(length, 0);
  bytes[..4].copy_from_slice(b"AGCW");
  put_u16(&mut bytes, 4, WORKSPACE_SCHEMA);
  put_u16(&mut bytes, 6, basis.state);
  put_u64(&mut bytes, 8, u64::try_from(length).map_err(|_| MarkWorkspaceErrorV1::Capacity("manifest length exceeds u64".to_string()))?);
  bytes[16..32].copy_from_slice(&identity.database_id);
  bytes[32..48].copy_from_slice(&identity.run_id);
  put_u64(&mut bytes, 48, identity.generation);
  put_u64(&mut bytes, 56, identity.checkpoint_sequence);
  put_u64(&mut bytes, 64, basis.created_at_ms);
  put_u64(&mut bytes, 72, basis.updated_at_ms);
  put_u16(&mut bytes, 80, identity.algorithm.to_u16());
  put_u16(
    &mut bytes,
    82,
    u16::try_from(descriptors.len()).map_err(|_| MarkWorkspaceErrorV1::Capacity("object count exceeds u16".to_string()))?,
  );
  let width = identity.algorithm.hash_length();
  bytes[88..88 + width].copy_from_slice(&basis.kv_layout_fingerprint);
  bytes[88 + width..88 + 2 * width].copy_from_slice(&basis.authority_root_set_digest);
  bytes[88 + 2 * width..120 + 2 * width].copy_from_slice(&basis.effective_policy_fingerprint);
  let mut cursor = 120 + 2 * width;
  for descriptor in descriptors {
    put_u16(&mut bytes, cursor, descriptor.kind as u16);
    put_u64(&mut bytes, cursor + 4, descriptor.ordinal);
    put_u64(&mut bytes, cursor + 12, descriptor.stored_length);
    put_u64(&mut bytes, cursor + 20, descriptor.logical_record_count);
    bytes[cursor + 28..cursor + 60].copy_from_slice(&descriptor.digest);
    put_u32(
      &mut bytes,
      cursor + 60,
      u32::try_from(descriptor.name.len()).map_err(|_| MarkWorkspaceErrorV1::Capacity("descriptor name exceeds u32".to_string()))?,
    );
    bytes[cursor + 68..cursor + 68 + descriptor.name.len()].copy_from_slice(descriptor.name.as_bytes());
    cursor += 68 + descriptor.name.len();
  }
  let checksum = crc32fast::hash(&bytes[..length - 4]);
  put_u32(&mut bytes, length - 4, checksum);
  Ok(bytes)
}

fn write_chunks(
  file: &mut fs::File,
  bytes: &[u8],
  cancellation: &CancellationToken,
  crc: &mut crc32fast::Hasher,
  digest: &mut blake3::Hasher,
) -> Result<(), MarkWorkspaceErrorV1> {
  for chunk in bytes.chunks(OBJECT_IO_CHUNK_BYTES) {
    if cancellation.is_cancelled() {
      return Err(MarkWorkspaceErrorV1::Canceled);
    }
    file.write_all(chunk).map_err(|source| MarkWorkspaceErrorV1::Io { operation: "object bytes", source })?;
    crc.update(chunk);
    digest.update(chunk);
  }
  Ok(())
}

fn stream_file_digest(path: &Path, expected_length: u64, cancellation: &CancellationToken) -> Result<[u8; 32], MarkWorkspaceErrorV1> {
  let mut file = open_regular_file_no_follow(path).map_err(|error| MarkWorkspaceErrorV1::Path(error.to_string()))?;
  let actual_length = file.metadata().map_err(|source| MarkWorkspaceErrorV1::Io { operation: "readback metadata", source })?.len();
  if actual_length != expected_length {
    return Err(MarkWorkspaceErrorV1::Format(format!("readback length {actual_length} disagrees with expected length {expected_length}")));
  }
  let mut hasher = blake3::Hasher::new();
  let mut buffer = [0u8; OBJECT_IO_CHUNK_BYTES];
  let mut read_total = 0u64;
  loop {
    if cancellation.is_cancelled() {
      return Err(MarkWorkspaceErrorV1::Canceled);
    }
    let read = file.read(&mut buffer).map_err(|source| MarkWorkspaceErrorV1::Io { operation: "readback bytes", source })?;
    if read == 0 {
      break;
    }
    read_total = read_total
      .checked_add(u64::try_from(read).map_err(|_| MarkWorkspaceErrorV1::Capacity("readback count exceeds u64".to_string()))?)
      .ok_or_else(|| MarkWorkspaceErrorV1::Capacity("readback count overflow".to_string()))?;
    hasher.update(&buffer[..read]);
  }
  if read_total != expected_length {
    return Err(MarkWorkspaceErrorV1::Format(format!("readback consumed {read_total} bytes but expected {expected_length}")));
  }
  Ok(*hasher.finalize().as_bytes())
}

fn object_name(kind: MarkWorkspaceObjectKindV1, ordinal: u64) -> String {
  format!("{}/{ordinal:016x}.agwo", kind.name())
}

fn object_name_length(kind: MarkWorkspaceObjectKindV1) -> usize {
  kind.name().len() + 1 + 16 + ".agwo".len()
}

fn try_object_name(kind: MarkWorkspaceObjectKindV1, ordinal: u64, length: usize) -> Result<String, MarkWorkspaceErrorV1> {
  let mut name = String::new();
  name.try_reserve_exact(length).map_err(|error| MarkWorkspaceErrorV1::Allocation(error.to_string()))?;
  write!(&mut name, "{}/{ordinal:016x}.agwo", kind.name()).map_err(|error| MarkWorkspaceErrorV1::Allocation(error.to_string()))?;
  if name.len() != length {
    return Err(MarkWorkspaceErrorV1::State("object name length disagrees with the frozen canonical form"));
  }
  Ok(name)
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}
