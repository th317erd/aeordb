//! Private, bounded, restart-verifiable storage for optional migration capture.
//!
//! This module owns external immutable journal segments and AMCM checkpoints.
//! It does not select AMPR, observe source writes, mutate either database, or
//! claim that a capture is sufficient for final reconciliation.

use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::index_task::{JournalOwnerKindV1, MutationJournalV1, decode_mutation_journal};
use super::migration_capture::{
  MigrationCaptureManifestWriteV1, decode_migration_capture_manifest, encode_migration_capture_manifest,
  migration_capture_manifest_identity,
};
use super::private_workspace::{
  PrivateWorkspaceErrorV1, create_private_directory_synced, ensure_capacity, secure_platform_private_regular_file,
  validate_existing_directory, validate_private_directory, validate_private_directory_readonly, validate_private_regular_file,
  validate_regular_database_path,
};
use crate::engine::HashAlgorithm;
use crate::engine::emergency_spill::{create_new_regular_file_no_follow, open_regular_file_no_follow};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::{NativeDurabilityError, durable_install_new_native, preallocate_file, sync_file_all_native};

const SEGMENT_MAX_BYTES: usize = 16 * 1_024 * 1_024;
const MANIFEST_MAX_BYTES: usize = 1_024;
const IO_CHUNK_BYTES: usize = 64 * 1_024;
const STATE_ACCOUNTING_BASE_BYTES: u64 = 1_024;

#[derive(Debug, Error)]
pub enum MigrationCaptureWorkspaceErrorV1 {
  #[error("migration capture workspace identity is invalid: {0}")]
  Identity(&'static str),
  #[error("migration capture workspace path is invalid or unavailable: {0}")]
  Path(String),
  #[error("migration capture workspace state refuses the operation: {0}")]
  State(&'static str),
  #[error("migration capture workspace chain is invalid: {0}")]
  Chain(&'static str),
  #[error("migration capture workspace checkpoint is invalid: {0}")]
  Checkpoint(String),
  #[error("migration capture workspace operation was canceled")]
  Canceled,
  #[error("migration capture workspace capacity is unavailable: {0}")]
  Capacity(String),
  #[error("migration capture workspace format is invalid: {0}")]
  Format(String),
  #[error("migration capture workspace memory admission failed: {0}")]
  Memory(#[source] Box<MemoryCoordinatorError>),
  #[error("migration capture workspace allocation failed: {0}")]
  Allocation(String),
  #[error("migration capture workspace I/O failed during {operation}: {source}")]
  Io {
    operation: &'static str,
    #[source]
    source: std::io::Error,
  },
  #[error("migration capture workspace durability failed: {0}")]
  Durability(#[source] Box<NativeDurabilityError>),
}

impl MigrationCaptureWorkspaceErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Identity(_) => "migration_capture_workspace_identity",
      Self::Path(_) => "migration_capture_workspace_path",
      Self::State(_) => "migration_capture_workspace_state",
      Self::Chain(_) => "migration_capture_workspace_chain",
      Self::Checkpoint(_) => "migration_capture_workspace_checkpoint",
      Self::Canceled => "migration_capture_workspace_cancelled",
      Self::Capacity(_) => "migration_capture_workspace_capacity",
      Self::Format(_) => "migration_capture_workspace_format",
      Self::Memory(_) => "migration_capture_workspace_memory",
      Self::Allocation(_) => "migration_capture_workspace_allocation",
      Self::Io { .. } => "migration_capture_workspace_io",
      Self::Durability(_) => "migration_capture_workspace_durability",
    }
  }
}

impl From<PrivateWorkspaceErrorV1> for MigrationCaptureWorkspaceErrorV1 {
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
pub struct MigrationCaptureWorkspaceIdentityV1 {
  database_id: [u8; 16],
  migration_id: [u8; 16],
  source_physical_instance_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  runtime_boot_id: [u8; 16],
  fencing_token: u64,
  capture_generation: u64,
  algorithm: HashAlgorithm,
}

impl MigrationCaptureWorkspaceIdentityV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    database_id: [u8; 16],
    migration_id: [u8; 16],
    source_physical_instance_id: [u8; 16],
    destination_physical_instance_id: [u8; 16],
    runtime_boot_id: [u8; 16],
    fencing_token: u64,
    capture_generation: u64,
    algorithm: HashAlgorithm,
  ) -> Result<Self, MigrationCaptureWorkspaceErrorV1> {
    if [&database_id, &migration_id, &source_physical_instance_id, &destination_physical_instance_id, &runtime_boot_id]
      .into_iter()
      .any(|value| all_zero(value))
      || source_physical_instance_id == destination_physical_instance_id
      || fencing_token == 0
      || capture_generation == 0
      || !matches!(algorithm.hash_length(), 32 | 64)
    {
      return Err(MigrationCaptureWorkspaceErrorV1::Identity(
        "IDs, fence, generation, source/destination separation, or hash profile are invalid",
      ));
    }
    Ok(Self {
      database_id,
      migration_id,
      source_physical_instance_id,
      destination_physical_instance_id,
      runtime_boot_id,
      fencing_token,
      capture_generation,
      algorithm,
    })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureWorkspaceBasisV1 {
  created_at_ms: i64,
  starting_publication_sequence: u64,
  starting_source_root: Vec<u8>,
  effective_config_fingerprint: Vec<u8>,
  system_family_registry_fingerprint: Vec<u8>,
  source_authority_digest: [u8; 32],
}

impl MigrationCaptureWorkspaceBasisV1 {
  pub fn new(
    created_at_ms: i64,
    starting_publication_sequence: u64,
    starting_source_root: Vec<u8>,
    effective_config_fingerprint: Vec<u8>,
    system_family_registry_fingerprint: Vec<u8>,
    source_authority_digest: [u8; 32],
  ) -> Result<Self, MigrationCaptureWorkspaceErrorV1> {
    if created_at_ms < 0 || starting_publication_sequence == 0 || all_zero(&source_authority_digest) {
      return Err(MigrationCaptureWorkspaceErrorV1::Identity("capture basis time, sequence, or source authority is invalid"));
    }
    Ok(Self {
      created_at_ms,
      starting_publication_sequence,
      starting_source_root,
      effective_config_fingerprint,
      system_family_registry_fingerprint,
      source_authority_digest,
    })
  }

  fn validate_for(&self, identity: MigrationCaptureWorkspaceIdentityV1) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
    let width = identity.algorithm.hash_length();
    for value in [&self.starting_source_root, &self.effective_config_fingerprint, &self.system_family_registry_fingerprint] {
      if value.len() != width || all_zero(value) {
        return Err(MigrationCaptureWorkspaceErrorV1::Identity("capture basis hash width or identity is invalid"));
      }
    }
    Ok(())
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureWorkspaceOptionsV1 {
  scratch_root: Option<PathBuf>,
  maximum_stored_bytes: u64,
  minimum_free_bytes: u64,
}

impl MigrationCaptureWorkspaceOptionsV1 {
  pub fn new(
    scratch_root: Option<PathBuf>,
    maximum_stored_bytes: u64,
    minimum_free_bytes: u64,
  ) -> Result<Self, MigrationCaptureWorkspaceErrorV1> {
    if maximum_stored_bytes == 0 {
      return Err(MigrationCaptureWorkspaceErrorV1::Capacity("capture cap is zero".to_string()));
    }
    if scratch_root.as_ref().is_some_and(|path| !path.is_absolute()) {
      return Err(MigrationCaptureWorkspaceErrorV1::Path("configured capture root must be absolute".to_string()));
    }
    Ok(Self { scratch_root, maximum_stored_bytes, minimum_free_bytes })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationCaptureWorkspaceReopenOptionsV1 {
  maximum_stored_bytes: u64,
}

impl MigrationCaptureWorkspaceReopenOptionsV1 {
  pub fn new(maximum_stored_bytes: u64) -> Result<Self, MigrationCaptureWorkspaceErrorV1> {
    if maximum_stored_bytes == 0 {
      return Err(MigrationCaptureWorkspaceErrorV1::Capacity("capture reopen cap is zero".to_string()));
    }
    Ok(Self { maximum_stored_bytes })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureWorkspaceSummaryV1 {
  first_segment_ordinal: u64,
  last_segment_ordinal: u64,
  segment_count: u64,
  segment_stored_bytes: u64,
  captured_through_publication_sequence: u64,
  source_root_before: Vec<u8>,
  source_root_after: Vec<u8>,
  segment_head: Vec<u8>,
}

impl MigrationCaptureWorkspaceSummaryV1 {
  pub const fn first_segment_ordinal(&self) -> u64 {
    self.first_segment_ordinal
  }

  pub const fn last_segment_ordinal(&self) -> u64 {
    self.last_segment_ordinal
  }

  pub const fn segment_count(&self) -> u64 {
    self.segment_count
  }

  pub const fn segment_stored_bytes(&self) -> u64 {
    self.segment_stored_bytes
  }

  pub const fn captured_through_publication_sequence(&self) -> u64 {
    self.captured_through_publication_sequence
  }

  pub fn source_root_before(&self) -> &[u8] {
    &self.source_root_before
  }

  pub fn source_root_after(&self) -> &[u8] {
    &self.source_root_after
  }

  pub fn segment_head(&self) -> &[u8] {
    &self.segment_head
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableMigrationCaptureCheckpointV1 {
  workspace_path: PathBuf,
  manifest_path: PathBuf,
  manifest_identity: Vec<u8>,
  checkpoint_sequence: u64,
  segment_count: u64,
  stored_bytes: u64,
}

impl DurableMigrationCaptureCheckpointV1 {
  pub fn workspace_path(&self) -> &Path {
    &self.workspace_path
  }

  pub fn manifest_path(&self) -> &Path {
    &self.manifest_path
  }

  pub fn manifest_identity(&self) -> &[u8] {
    &self.manifest_identity
  }

  pub const fn checkpoint_sequence(&self) -> u64 {
    self.checkpoint_sequence
  }

  pub const fn segment_count(&self) -> u64 {
    self.segment_count
  }

  pub const fn stored_bytes(&self) -> u64 {
    self.stored_bytes
  }
}

pub struct DurableMigrationCaptureWorkspaceV1 {
  identity: MigrationCaptureWorkspaceIdentityV1,
  basis: MigrationCaptureWorkspaceBasisV1,
  options: MigrationCaptureWorkspaceOptionsV1,
  workspace_path: PathBuf,
  segments_path: PathBuf,
  checkpoints_path: PathBuf,
  summary: MigrationCaptureWorkspaceSummaryV1,
  checkpoint_stored_bytes: u64,
  last_checkpoint_sequence: u64,
  last_manifest_identity: Vec<u8>,
  cancellation: CancellationToken,
  _state_memory: MemoryReservation,
  failed: bool,
}

impl fmt::Debug for DurableMigrationCaptureWorkspaceV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("DurableMigrationCaptureWorkspaceV1")
      .field("workspace_path", &self.workspace_path)
      .field("segment_count", &self.summary.segment_count)
      .field("last_checkpoint_sequence", &self.last_checkpoint_sequence)
      .field("failed", &self.failed)
      .finish()
  }
}

impl DurableMigrationCaptureWorkspaceV1 {
  pub fn create(
    database_path: &Path,
    identity: MigrationCaptureWorkspaceIdentityV1,
    basis: MigrationCaptureWorkspaceBasisV1,
    options: MigrationCaptureWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, MigrationCaptureWorkspaceErrorV1> {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    basis.validate_for(identity)?;
    validate_regular_database_path(database_path, "migration capture source")?;
    let database_parent =
      database_path.parent().ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Path("source database path has no parent".to_string()))?;
    let base = match options.scratch_root.as_deref() {
      Some(scratch_root) => scratch_root,
      None => database_parent,
    };
    validate_existing_directory(base, "capture workspace base")?;
    ensure_capacity(base, 0, options.minimum_free_bytes)?;
    let workspace_path = create_workspace_directories(database_path, base, identity, options.scratch_root.is_some())?;
    let segments_path = workspace_path.join("segments");
    create_private_directory_synced(&segments_path, &workspace_path)?;
    let checkpoints_path = workspace_path.join("checkpoints");
    create_private_directory_synced(&checkpoints_path, &workspace_path)?;
    let state_bytes = state_accounting_bytes(&workspace_path, identity.algorithm.hash_length())?;
    let state_memory = memory
      .reserve(MemoryOwner::Migration, state_bytes, AdmissionClass::Maintenance)
      .map_err(|error| MigrationCaptureWorkspaceErrorV1::Memory(Box::new(error)))?;
    let zero = vec![0; identity.algorithm.hash_length()];
    Ok(Self {
      identity,
      summary: MigrationCaptureWorkspaceSummaryV1 {
        first_segment_ordinal: 0,
        last_segment_ordinal: 0,
        segment_count: 0,
        segment_stored_bytes: 0,
        captured_through_publication_sequence: basis.starting_publication_sequence,
        source_root_before: basis.starting_source_root.clone(),
        source_root_after: basis.starting_source_root.clone(),
        segment_head: zero.clone(),
      },
      basis,
      options,
      workspace_path,
      segments_path,
      checkpoints_path,
      checkpoint_stored_bytes: 0,
      last_checkpoint_sequence: 0,
      last_manifest_identity: zero,
      cancellation,
      _state_memory: state_memory,
      failed: false,
    })
  }

  pub fn workspace_path(&self) -> &Path {
    &self.workspace_path
  }

  pub fn summary(&self) -> &MigrationCaptureWorkspaceSummaryV1 {
    &self.summary
  }

  pub fn segment_path(&self, ordinal: u64) -> PathBuf {
    self.segments_path.join(segment_name(ordinal))
  }

  pub fn append_segment(&mut self, bytes: &[u8]) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
    self.preflight_open()?;
    if bytes.len() > SEGMENT_MAX_BYTES {
      return Err(MigrationCaptureWorkspaceErrorV1::Capacity(format!(
        "capture segment has {} bytes, exceeding {SEGMENT_MAX_BYTES}",
        bytes.len()
      )));
    }
    let journal = decode_mutation_journal(bytes, self.identity.algorithm)
      .map_err(|error| MigrationCaptureWorkspaceErrorV1::Format(error.to_string()))?;
    validate_journal_for_summary(&journal, self.identity, &self.summary)?;
    let stored_length = u64::try_from(bytes.len())
      .map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("capture segment length exceeds u64".to_string()))?;
    let projected_segment_bytes = self
      .summary
      .segment_stored_bytes
      .checked_add(stored_length)
      .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("capture segment byte total overflow".to_string()))?;
    let next_segment_count = self
      .summary
      .segment_count
      .checked_add(1)
      .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("capture segment count overflow".to_string()))?;
    enforce_capture_cap(projected_segment_bytes, self.checkpoint_stored_bytes, self.options.maximum_stored_bytes)?;
    ensure_capacity(&self.segments_path, stored_length, self.options.minimum_free_bytes)?;
    let path = self.segment_path(journal.segment_ordinal);
    if let Err(error) = write_immutable_file(&path, bytes, &self.cancellation) {
      self.failed = true;
      return Err(error);
    }
    self.summary.first_segment_ordinal =
      if self.summary.segment_count == 0 { journal.segment_ordinal } else { self.summary.first_segment_ordinal };
    self.summary.last_segment_ordinal = journal.segment_ordinal;
    self.summary.segment_count = next_segment_count;
    self.summary.segment_stored_bytes = projected_segment_bytes;
    self.summary.captured_through_publication_sequence = journal.last_sequence;
    self.summary.source_root_after.clear();
    self.summary.source_root_after.extend_from_slice(journal.source_root_after);
    self.summary.segment_head.clear();
    self.summary.segment_head.extend_from_slice(&journal.key);
    Ok(())
  }

  pub fn publish_checkpoint(
    &mut self,
    request: &MigrationCaptureManifestWriteV1,
  ) -> Result<DurableMigrationCaptureCheckpointV1, MigrationCaptureWorkspaceErrorV1> {
    self.preflight_open()?;
    validate_checkpoint_request(
      request,
      self.identity,
      &self.basis,
      &self.summary,
      self.last_checkpoint_sequence,
      &self.last_manifest_identity,
    )?;
    let bytes = encode_migration_capture_manifest(request, self.identity.algorithm)
      .map_err(|error| MigrationCaptureWorkspaceErrorV1::Checkpoint(error.to_string()))?;
    let manifest_identity = migration_capture_manifest_identity(&bytes, self.identity.algorithm);
    let stored_length = u64::try_from(bytes.len())
      .map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("capture manifest length exceeds u64".to_string()))?;
    let projected_checkpoint_bytes = self
      .checkpoint_stored_bytes
      .checked_add(stored_length)
      .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("capture checkpoint byte total overflow".to_string()))?;
    enforce_capture_cap(self.summary.segment_stored_bytes, projected_checkpoint_bytes, self.options.maximum_stored_bytes)?;
    ensure_capacity(&self.checkpoints_path, stored_length, self.options.minimum_free_bytes)?;
    let checkpoint_path = self.checkpoints_path.join(format!("{:016x}", request.checkpoint_sequence));
    let manifest_path = checkpoint_path.join("manifest.amcm");
    let stored_bytes = self
      .summary
      .segment_stored_bytes
      .checked_add(projected_checkpoint_bytes)
      .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("capture stored byte total overflow".to_string()))?;
    let receipt = DurableMigrationCaptureCheckpointV1 {
      workspace_path: self.workspace_path.clone(),
      manifest_path: manifest_path.clone(),
      manifest_identity,
      checkpoint_sequence: request.checkpoint_sequence,
      segment_count: self.summary.segment_count,
      stored_bytes,
    };
    if let Err(error) = create_private_directory_synced(&checkpoint_path, &self.checkpoints_path) {
      self.failed = true;
      return Err(error.into());
    }
    if let Err(error) = write_immutable_file(&manifest_path, &bytes, &self.cancellation) {
      self.failed = true;
      return Err(error);
    }
    self.checkpoint_stored_bytes = projected_checkpoint_bytes;
    self.last_checkpoint_sequence = request.checkpoint_sequence;
    self.last_manifest_identity.copy_from_slice(&receipt.manifest_identity);
    Ok(receipt)
  }

  fn preflight_open(&self) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
    if self.failed {
      return Err(MigrationCaptureWorkspaceErrorV1::State("writer is latched after a prior workspace mutation failure"));
    }
    if self.cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    Ok(())
  }
}

pub struct ReopenedMigrationCaptureWorkspaceV1 {
  workspace_path: PathBuf,
  segments_path: PathBuf,
  identity: MigrationCaptureWorkspaceIdentityV1,
  basis: MigrationCaptureWorkspaceBasisV1,
  manifest: MigrationCaptureManifestWriteV1,
  maximum_stored_bytes: u64,
  cancellation: CancellationToken,
  memory: MemoryCoordinator,
  _manifest_memory: MemoryReservation,
}

impl fmt::Debug for ReopenedMigrationCaptureWorkspaceV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("ReopenedMigrationCaptureWorkspaceV1")
      .field("workspace_path", &self.workspace_path)
      .field("checkpoint_sequence", &self.manifest.checkpoint_sequence)
      .field("segment_count", &self.manifest.segment_count)
      .finish()
  }
}

impl ReopenedMigrationCaptureWorkspaceV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn open(
    workspace_path: &Path,
    checkpoint_sequence: u64,
    expected_manifest_identity: &[u8],
    identity: MigrationCaptureWorkspaceIdentityV1,
    basis: MigrationCaptureWorkspaceBasisV1,
    options: MigrationCaptureWorkspaceReopenOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, MigrationCaptureWorkspaceErrorV1> {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    basis.validate_for(identity)?;
    if !workspace_path.is_absolute()
      || checkpoint_sequence == 0
      || expected_manifest_identity.len() != identity.algorithm.hash_length()
      || all_zero(expected_manifest_identity)
    {
      return Err(MigrationCaptureWorkspaceErrorV1::Identity("reopen path, checkpoint, or manifest identity is invalid"));
    }
    validate_private_directory_readonly(workspace_path, "migration capture workspace")?;
    let segments_path = workspace_path.join("segments");
    validate_private_directory_readonly(&segments_path, "migration capture segment collection")?;
    let checkpoints_path = workspace_path.join("checkpoints");
    validate_private_directory_readonly(&checkpoints_path, "migration capture checkpoint collection")?;
    let checkpoint_path = checkpoints_path.join(format!("{checkpoint_sequence:016x}"));
    validate_private_directory_readonly(&checkpoint_path, "selected migration capture checkpoint")?;
    let manifest_path = checkpoint_path.join("manifest.amcm");
    let (manifest_bytes, manifest_memory) =
      read_charged_file(&manifest_path, MANIFEST_MAX_BYTES, 2, "migration capture manifest", &cancellation, memory)?;
    let observed_identity = migration_capture_manifest_identity(&manifest_bytes, identity.algorithm);
    if observed_identity != expected_manifest_identity {
      return Err(MigrationCaptureWorkspaceErrorV1::Checkpoint("selected manifest identity does not match AMPR".to_string()));
    }
    let manifest = decode_migration_capture_manifest(&manifest_bytes, identity.algorithm)
      .map_err(|error| MigrationCaptureWorkspaceErrorV1::Format(error.to_string()))?;
    validate_manifest_binding(&manifest, identity, &basis, checkpoint_sequence)?;
    let checkpoint_stored_bytes =
      validate_checkpoint_predecessor_chain(&checkpoints_path, &manifest, manifest_bytes.len(), identity, &basis, &cancellation, memory)?;
    enforce_capture_cap(manifest.segment_stored_bytes, checkpoint_stored_bytes, options.maximum_stored_bytes)?;
    enforce_physical_workspace_cap(workspace_path, &cancellation, options.maximum_stored_bytes)?;
    let reopened = Self {
      workspace_path: workspace_path.to_path_buf(),
      segments_path,
      identity,
      basis,
      manifest,
      maximum_stored_bytes: options.maximum_stored_bytes,
      cancellation,
      memory: memory.clone(),
      _manifest_memory: manifest_memory,
    };
    reopened.validate_segment_closure(|_| Ok(()))?;
    Ok(reopened)
  }

  pub const fn segment_count(&self) -> u64 {
    self.manifest.segment_count
  }

  pub const fn captured_through_publication_sequence(&self) -> u64 {
    self.manifest.captured_through_publication_sequence
  }

  pub fn for_each_segment<F>(&self, visit: F) -> Result<(), MigrationCaptureWorkspaceErrorV1>
  where
    F: FnMut(&MutationJournalV1<'_>) -> Result<(), MigrationCaptureWorkspaceErrorV1>,
  {
    self.validate_segment_closure(visit)
  }

  fn validate_segment_closure<F>(&self, mut visit: F) -> Result<(), MigrationCaptureWorkspaceErrorV1>
  where
    F: FnMut(&MutationJournalV1<'_>) -> Result<(), MigrationCaptureWorkspaceErrorV1>,
  {
    if self.cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    let mut summary = MigrationCaptureWorkspaceSummaryV1 {
      first_segment_ordinal: 0,
      last_segment_ordinal: 0,
      segment_count: 0,
      segment_stored_bytes: 0,
      captured_through_publication_sequence: self.basis.starting_publication_sequence,
      source_root_before: self.basis.starting_source_root.clone(),
      source_root_after: self.basis.starting_source_root.clone(),
      segment_head: vec![0; self.identity.algorithm.hash_length()],
    };
    if self.manifest.segment_count == 0 {
      validate_summary_matches_manifest(&summary, &self.manifest)?;
      return Ok(());
    }
    for ordinal in self.manifest.first_segment_ordinal..=self.manifest.last_segment_ordinal {
      let path = self.segments_path.join(segment_name(ordinal));
      let (bytes, _reservation) =
        read_charged_file(&path, SEGMENT_MAX_BYTES, 1, "migration capture segment", &self.cancellation, &self.memory)?;
      let journal = decode_mutation_journal(&bytes, self.identity.algorithm)
        .map_err(|error| MigrationCaptureWorkspaceErrorV1::Format(error.to_string()))?;
      validate_journal_for_summary(&journal, self.identity, &summary)?;
      summary.first_segment_ordinal = if summary.segment_count == 0 { journal.segment_ordinal } else { summary.first_segment_ordinal };
      summary.last_segment_ordinal = journal.segment_ordinal;
      summary.segment_count = summary
        .segment_count
        .checked_add(1)
        .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("reopened segment count overflow".to_string()))?;
      summary.segment_stored_bytes = summary
        .segment_stored_bytes
        .checked_add(
          u64::try_from(bytes.len())
            .map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("reopened capture segment length exceeds u64".to_string()))?,
        )
        .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("reopened segment byte total overflow".to_string()))?;
      if summary.segment_stored_bytes > self.maximum_stored_bytes {
        return Err(MigrationCaptureWorkspaceErrorV1::Capacity("reopened segment bytes exceed capture cap".to_string()));
      }
      summary.captured_through_publication_sequence = journal.last_sequence;
      summary.source_root_after.clear();
      summary.source_root_after.extend_from_slice(journal.source_root_after);
      summary.segment_head.clear();
      summary.segment_head.extend_from_slice(&journal.key);
      visit(&journal)?;
    }
    validate_summary_matches_manifest(&summary, &self.manifest)
  }
}

fn validate_journal_for_summary(
  journal: &MutationJournalV1<'_>,
  identity: MigrationCaptureWorkspaceIdentityV1,
  summary: &MigrationCaptureWorkspaceSummaryV1,
) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
  let expected_ordinal = summary
    .last_segment_ordinal
    .checked_add(1)
    .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("capture segment ordinal overflow".to_string()))?;
  let expected_sequence = summary
    .captured_through_publication_sequence
    .checked_add(1)
    .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("capture publication sequence overflow".to_string()))?;
  if journal.owner_kind != JournalOwnerKindV1::Task
    || journal.owner_id != identity.migration_id
    || journal.generation != identity.capture_generation
    || journal.runtime_boot_id != identity.runtime_boot_id
    || journal.segment_ordinal != expected_ordinal
    || journal.first_sequence != expected_sequence
    || journal.source_root_before != summary.source_root_after
    || journal.semantic_state_root != journal.source_root_after
    || (summary.segment_count == 0 && (!journal.chain_reset || !all_zero(journal.previous_segment)))
    || (summary.segment_count != 0 && (journal.chain_reset || journal.previous_segment != summary.segment_head))
  {
    return Err(MigrationCaptureWorkspaceErrorV1::Chain("journal identity, ordinal, sequence, root, or predecessor is discontinuous"));
  }
  let mut prior_sequence = summary.captured_through_publication_sequence;
  let mut prior_root = summary.source_root_after.as_slice();
  let mut active_sequence = 0u64;
  let mut active_root_before: &[u8] = &[];
  let mut active_root_after: &[u8] = &[];
  for record in journal.records.iter() {
    let record = record.map_err(|error| MigrationCaptureWorkspaceErrorV1::Format(error.to_string()))?;
    if record.sequence != prior_sequence && prior_sequence.checked_add(1) != Some(record.sequence) {
      return Err(MigrationCaptureWorkspaceErrorV1::Chain("journal contains a publication-sequence gap"));
    }
    if record.sequence == active_sequence {
      if record.root_before != active_root_before || record.root_after != active_root_after {
        return Err(MigrationCaptureWorkspaceErrorV1::Chain("same-sequence records disagree on source roots"));
      }
    } else {
      if record.root_before != prior_root {
        return Err(MigrationCaptureWorkspaceErrorV1::Chain("journal contains a source-root discontinuity"));
      }
      active_sequence = record.sequence;
      active_root_before = record.root_before;
      active_root_after = record.root_after;
      prior_sequence = record.sequence;
      prior_root = record.root_after;
    }
  }
  Ok(())
}

fn validate_checkpoint_request(
  request: &MigrationCaptureManifestWriteV1,
  identity: MigrationCaptureWorkspaceIdentityV1,
  basis: &MigrationCaptureWorkspaceBasisV1,
  summary: &MigrationCaptureWorkspaceSummaryV1,
  last_checkpoint_sequence: u64,
  last_manifest_identity: &[u8],
) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
  let expected_checkpoint = last_checkpoint_sequence
    .checked_add(1)
    .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("capture checkpoint sequence overflow".to_string()))?;
  validate_manifest_binding(request, identity, basis, expected_checkpoint)?;
  if request.previous_manifest != last_manifest_identity {
    return Err(MigrationCaptureWorkspaceErrorV1::Checkpoint(
      "manifest predecessor does not match the prior durable checkpoint".to_string(),
    ));
  }
  validate_summary_matches_manifest(summary, request)
}

fn validate_manifest_binding(
  request: &MigrationCaptureManifestWriteV1,
  identity: MigrationCaptureWorkspaceIdentityV1,
  basis: &MigrationCaptureWorkspaceBasisV1,
  expected_checkpoint_sequence: u64,
) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
  if request.database_id != identity.database_id
    || request.migration_id != identity.migration_id
    || request.source_physical_instance_id != identity.source_physical_instance_id
    || request.destination_physical_instance_id != identity.destination_physical_instance_id
    || request.fencing_token != identity.fencing_token
    || request.capture_generation != identity.capture_generation
    || request.checkpoint_sequence != expected_checkpoint_sequence
    || request.created_at_ms != basis.created_at_ms
    || request.effective_config_fingerprint != basis.effective_config_fingerprint
    || request.system_family_registry_fingerprint != basis.system_family_registry_fingerprint
    || request.source_authority_digest != basis.source_authority_digest
  {
    return Err(MigrationCaptureWorkspaceErrorV1::Checkpoint(
      "manifest identity, fence, generation, policy closure, or sequence does not match the workspace".to_string(),
    ));
  }
  Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_checkpoint_predecessor_chain(
  checkpoints_path: &Path,
  selected: &MigrationCaptureManifestWriteV1,
  selected_stored_bytes: usize,
  identity: MigrationCaptureWorkspaceIdentityV1,
  basis: &MigrationCaptureWorkspaceBasisV1,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<u64, MigrationCaptureWorkspaceErrorV1> {
  let mut checkpoint_stored_bytes = u64::try_from(selected_stored_bytes)
    .map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("selected manifest length exceeds u64".to_string()))?;
  let mut later = selected.clone();
  while later.checkpoint_sequence > 1 {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    let predecessor_sequence = later.checkpoint_sequence - 1;
    let predecessor_path = checkpoints_path.join(format!("{predecessor_sequence:016x}"));
    validate_private_directory_readonly(&predecessor_path, "migration capture predecessor checkpoint")?;
    let manifest_path = predecessor_path.join("manifest.amcm");
    let (bytes, _reservation) =
      read_charged_file(&manifest_path, MANIFEST_MAX_BYTES, 2, "migration capture predecessor", cancellation, memory)?;
    let observed_identity = migration_capture_manifest_identity(&bytes, identity.algorithm);
    if observed_identity != later.previous_manifest {
      return Err(MigrationCaptureWorkspaceErrorV1::Checkpoint(
        "capture checkpoint predecessor identity does not match its successor".to_string(),
      ));
    }
    let predecessor = decode_migration_capture_manifest(&bytes, identity.algorithm)
      .map_err(|error| MigrationCaptureWorkspaceErrorV1::Format(error.to_string()))?;
    validate_manifest_binding(&predecessor, identity, basis, predecessor_sequence)?;
    validate_checkpoint_progression(&predecessor, &later)?;
    checkpoint_stored_bytes = checkpoint_stored_bytes
      .checked_add(
        u64::try_from(bytes.len())
          .map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("predecessor manifest length exceeds u64".to_string()))?,
      )
      .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("checkpoint predecessor byte total overflow".to_string()))?;
    later = predecessor;
  }
  Ok(checkpoint_stored_bytes)
}

fn validate_checkpoint_progression(
  predecessor: &MigrationCaptureManifestWriteV1,
  successor: &MigrationCaptureManifestWriteV1,
) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
  let shared_summary_changed = predecessor.segment_count == successor.segment_count
    && (predecessor.first_segment_ordinal != successor.first_segment_ordinal
      || predecessor.last_segment_ordinal != successor.last_segment_ordinal
      || predecessor.segment_stored_bytes != successor.segment_stored_bytes
      || predecessor.captured_through_publication_sequence != successor.captured_through_publication_sequence
      || predecessor.source_root_before != successor.source_root_before
      || predecessor.source_root_after != successor.source_root_after
      || predecessor.segment_head != successor.segment_head);
  if predecessor.updated_at_ms > successor.updated_at_ms
    || predecessor.segment_count > successor.segment_count
    || predecessor.segment_stored_bytes > successor.segment_stored_bytes
    || predecessor.captured_through_publication_sequence > successor.captured_through_publication_sequence
    || predecessor.observed_through_publication_sequence > successor.observed_through_publication_sequence
    || predecessor.source_root_before != successor.source_root_before
    || shared_summary_changed
  {
    return Err(MigrationCaptureWorkspaceErrorV1::Checkpoint(
      "capture checkpoint predecessor regresses or rewrites durable progress".to_string(),
    ));
  }
  Ok(())
}

fn validate_summary_matches_manifest(
  summary: &MigrationCaptureWorkspaceSummaryV1,
  request: &MigrationCaptureManifestWriteV1,
) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
  if request.captured_through_publication_sequence != summary.captured_through_publication_sequence
    || request.first_segment_ordinal != summary.first_segment_ordinal
    || request.last_segment_ordinal != summary.last_segment_ordinal
    || request.segment_count != summary.segment_count
    || request.segment_stored_bytes != summary.segment_stored_bytes
    || request.source_root_before != summary.source_root_before
    || request.source_root_after != summary.source_root_after
    || request.segment_head != summary.segment_head
  {
    return Err(MigrationCaptureWorkspaceErrorV1::Checkpoint(
      "manifest segment summary does not match the durable capture chain".to_string(),
    ));
  }
  Ok(())
}

fn create_workspace_directories(
  database_path: &Path,
  base: &Path,
  identity: MigrationCaptureWorkspaceIdentityV1,
  overridden: bool,
) -> Result<PathBuf, MigrationCaptureWorkspaceErrorV1> {
  let database_id = hex::encode(identity.database_id);
  let migration_id = hex::encode(identity.migration_id);
  let generation = format!("{:016x}", identity.capture_generation);
  if overridden {
    let database_directory = base.join(database_id);
    if database_directory.exists() {
      validate_private_directory(&database_directory, "migration database workspace directory")?;
    } else {
      create_private_directory_synced(&database_directory, base)?;
    }
    let migration_directory = database_directory.join(migration_id);
    if migration_directory.exists() {
      validate_private_directory(&migration_directory, "migration workspace directory")?;
    } else {
      create_private_directory_synced(&migration_directory, &database_directory)?;
    }
    let workspace = migration_directory.join(generation);
    create_private_directory_synced(&workspace, &migration_directory)?;
    return Ok(workspace);
  }
  let file_name = database_path
    .file_name()
    .and_then(|name| name.to_str())
    .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Path("source database filename is not canonical UTF-8".to_string()))?;
  let workspace = base.join(format!(".{file_name}-migration-{database_id}-{migration_id}-{generation}"));
  create_private_directory_synced(&workspace, base)?;
  Ok(workspace)
}

fn write_immutable_file(path: &Path, bytes: &[u8], cancellation: &CancellationToken) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
  if cancellation.is_cancelled() {
    return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
  }
  let parent = path.parent().ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Path("immutable capture path has no parent".to_string()))?;
  validate_private_directory(parent, "immutable capture parent")?;
  let pending = parent.join(format!(".capture-{}.pending", uuid::Uuid::new_v4()));
  let mut file = create_new_regular_file_no_follow(&pending).map_err(|error| MigrationCaptureWorkspaceErrorV1::Path(error.to_string()))?;
  secure_platform_private_regular_file(&pending)?;
  validate_private_regular_file(&pending, &file, "new migration capture artifact")?;
  let length = u64::try_from(bytes.len())
    .map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("capture artifact length exceeds u64".to_string()))?;
  preallocate_file(&file, length).map_err(|error| MigrationCaptureWorkspaceErrorV1::Durability(Box::new(error)))?;
  for chunk in bytes.chunks(IO_CHUNK_BYTES) {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    file.write_all(chunk).map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture artifact write", source })?;
  }
  sync_file_all_native(&file).map_err(|error| MigrationCaptureWorkspaceErrorV1::Durability(Box::new(error)))?;
  drop(file);
  durable_install_new_native(&pending, path).map_err(|error| MigrationCaptureWorkspaceErrorV1::Durability(Box::new(error)))?;
  verify_file_exact(path, bytes, cancellation)
}

fn verify_file_exact(path: &Path, expected: &[u8], cancellation: &CancellationToken) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
  let mut file = open_regular_file_no_follow(path).map_err(|error| MigrationCaptureWorkspaceErrorV1::Path(error.to_string()))?;
  validate_private_regular_file(path, &file, "migration capture readback")?;
  let metadata =
    file.metadata().map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture readback metadata", source })?;
  let expected_length = u64::try_from(expected.len())
    .map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("capture readback length exceeds u64".to_string()))?;
  if metadata.len() != expected_length {
    return Err(MigrationCaptureWorkspaceErrorV1::Format("capture readback length does not match written bytes".to_string()));
  }
  let mut buffer = [0u8; IO_CHUNK_BYTES];
  let mut offset = 0usize;
  while offset < expected.len() {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    let length = (expected.len() - offset).min(buffer.len());
    file
      .read_exact(&mut buffer[..length])
      .map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture artifact readback", source })?;
    if buffer[..length] != expected[offset..offset + length] {
      return Err(MigrationCaptureWorkspaceErrorV1::Format("capture readback bytes do not match written bytes".to_string()));
    }
    offset += length;
  }
  Ok(())
}

fn read_charged_file(
  path: &Path,
  maximum_length: usize,
  reservation_multiplier: u64,
  role: &'static str,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<(Vec<u8>, MemoryReservation), MigrationCaptureWorkspaceErrorV1> {
  if cancellation.is_cancelled() {
    return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
  }
  let mut file = open_regular_file_no_follow(path).map_err(|error| MigrationCaptureWorkspaceErrorV1::Path(error.to_string()))?;
  let metadata = file.metadata().map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: role, source })?;
  validate_private_regular_file(path, &file, role)?;
  let length =
    usize::try_from(metadata.len()).map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity(format!("{role} length exceeds usize")))?;
  if length == 0 || length > maximum_length {
    return Err(MigrationCaptureWorkspaceErrorV1::Capacity(format!("{role} length {length} is outside 1..={maximum_length}")));
  }
  let reservation_length =
    u64::try_from(length).map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity(format!("{role} reservation length exceeds u64")))?;
  let reservation_bytes = reservation_length
    .checked_mul(reservation_multiplier)
    .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity(format!("{role} reservation length overflow")))?;
  let reservation = memory
    .reserve(MemoryOwner::Migration, reservation_bytes, AdmissionClass::Maintenance)
    .map_err(|error| MigrationCaptureWorkspaceErrorV1::Memory(Box::new(error)))?;
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(length).map_err(|error| MigrationCaptureWorkspaceErrorV1::Allocation(error.to_string()))?;
  bytes.resize(length, 0);
  let mut offset = 0usize;
  while offset < length {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    let end = (offset + IO_CHUNK_BYTES).min(length);
    let read = file.read(&mut bytes[offset..end]).map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: role, source })?;
    if read == 0 {
      return Err(MigrationCaptureWorkspaceErrorV1::Format(format!("{role} was truncated while reading")));
    }
    offset = offset.checked_add(read).ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity(format!("{role} read count overflow")))?;
  }
  let mut trailing = [0u8; 1];
  if file.read(&mut trailing).map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: role, source })? != 0 {
    return Err(MigrationCaptureWorkspaceErrorV1::Format(format!("{role} grew while reading")));
  }
  if file.metadata().map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: role, source })?.len() != metadata.len() {
    return Err(MigrationCaptureWorkspaceErrorV1::Format(format!("{role} length changed while reading")));
  }
  Ok((bytes, reservation))
}

fn enforce_capture_cap(segment_bytes: u64, checkpoint_bytes: u64, maximum_bytes: u64) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
  let total = segment_bytes
    .checked_add(checkpoint_bytes)
    .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("capture byte total overflow".to_string()))?;
  if total > maximum_bytes {
    return Err(MigrationCaptureWorkspaceErrorV1::Capacity(format!("projected capture bytes {total} exceed cap {maximum_bytes}")));
  }
  Ok(())
}

fn enforce_physical_workspace_cap(
  workspace_path: &Path,
  cancellation: &CancellationToken,
  maximum_bytes: u64,
) -> Result<(), MigrationCaptureWorkspaceErrorV1> {
  let segments_path = workspace_path.join("segments");
  let checkpoints_path = workspace_path.join("checkpoints");
  let mut total = 0u64;
  for entry in fs::read_dir(workspace_path)
    .map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture workspace inventory", source })?
  {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    let entry = entry.map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture workspace inventory entry", source })?;
    let path = entry.path();
    if path != segments_path && path != checkpoints_path {
      return Err(MigrationCaptureWorkspaceErrorV1::Path(format!(
        "capture workspace contains an unknown top-level entry: {}",
        path.display()
      )));
    }
    validate_private_directory_readonly(&path, "capture workspace collection")?;
  }
  total = inventory_segment_files(&segments_path, cancellation, maximum_bytes, total)?;
  total = inventory_checkpoint_files(&checkpoints_path, cancellation, maximum_bytes, total)?;
  if total > maximum_bytes {
    return Err(MigrationCaptureWorkspaceErrorV1::Capacity(format!(
      "physical capture workspace uses {total} bytes, exceeding cap {maximum_bytes}"
    )));
  }
  Ok(())
}

fn inventory_segment_files(
  segments_path: &Path,
  cancellation: &CancellationToken,
  maximum_bytes: u64,
  mut total: u64,
) -> Result<u64, MigrationCaptureWorkspaceErrorV1> {
  for entry in
    fs::read_dir(segments_path).map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture segment inventory", source })?
  {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    let entry = entry.map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture segment inventory entry", source })?;
    let path = entry.path();
    let name = entry
      .file_name()
      .into_string()
      .map_err(|_| MigrationCaptureWorkspaceErrorV1::Path("capture segment filename is not UTF-8".to_string()))?;
    if !canonical_segment_name(&name) && !canonical_pending_name(&name) {
      return Err(MigrationCaptureWorkspaceErrorV1::Path(format!("capture segment collection contains unknown entry {name}")));
    }
    total = account_private_file(&path, "capture segment inventory file", SEGMENT_MAX_BYTES, total, maximum_bytes)?;
  }
  Ok(total)
}

fn inventory_checkpoint_files(
  checkpoints_path: &Path,
  cancellation: &CancellationToken,
  maximum_bytes: u64,
  mut total: u64,
) -> Result<u64, MigrationCaptureWorkspaceErrorV1> {
  for entry in fs::read_dir(checkpoints_path)
    .map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture checkpoint inventory", source })?
  {
    if cancellation.is_cancelled() {
      return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
    }
    let entry = entry.map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture checkpoint inventory entry", source })?;
    let path = entry.path();
    let name = entry
      .file_name()
      .into_string()
      .map_err(|_| MigrationCaptureWorkspaceErrorV1::Path("capture checkpoint directory name is not UTF-8".to_string()))?;
    if !canonical_checkpoint_name(&name) {
      return Err(MigrationCaptureWorkspaceErrorV1::Path(format!("capture checkpoint collection contains unknown entry {name}")));
    }
    validate_private_directory_readonly(&path, "capture checkpoint inventory directory")?;
    for child in fs::read_dir(&path)
      .map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture checkpoint file inventory", source })?
    {
      if cancellation.is_cancelled() {
        return Err(MigrationCaptureWorkspaceErrorV1::Canceled);
      }
      let child =
        child.map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture checkpoint file inventory entry", source })?;
      let child_path = child.path();
      let child_name = child
        .file_name()
        .into_string()
        .map_err(|_| MigrationCaptureWorkspaceErrorV1::Path("capture checkpoint filename is not UTF-8".to_string()))?;
      if child_name != "manifest.amcm" && !canonical_pending_name(&child_name) {
        return Err(MigrationCaptureWorkspaceErrorV1::Path(format!("capture checkpoint contains unknown entry {child_name}")));
      }
      total = account_private_file(&child_path, "capture checkpoint inventory file", MANIFEST_MAX_BYTES, total, maximum_bytes)?;
    }
  }
  Ok(total)
}

fn account_private_file(
  path: &Path,
  role: &str,
  per_file_maximum: usize,
  total: u64,
  workspace_maximum: u64,
) -> Result<u64, MigrationCaptureWorkspaceErrorV1> {
  let file = open_regular_file_no_follow(path).map_err(|error| MigrationCaptureWorkspaceErrorV1::Path(error.to_string()))?;
  validate_private_regular_file(path, &file, role)?;
  let length =
    file.metadata().map_err(|source| MigrationCaptureWorkspaceErrorV1::Io { operation: "capture inventory file metadata", source })?.len();
  let per_file_maximum_u64 = u64::try_from(per_file_maximum)
    .map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("per-file workspace cap exceeds u64".to_string()))?;
  if length > per_file_maximum_u64 {
    return Err(MigrationCaptureWorkspaceErrorV1::Capacity(format!(
      "{role} has {length} bytes, exceeding per-file cap {per_file_maximum}"
    )));
  }
  let projected = total
    .checked_add(length)
    .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("physical workspace byte total overflow".to_string()))?;
  if projected > workspace_maximum {
    return Err(MigrationCaptureWorkspaceErrorV1::Capacity(format!(
      "physical capture workspace uses at least {projected} bytes, exceeding cap {workspace_maximum}"
    )));
  }
  Ok(projected)
}

fn canonical_segment_name(name: &str) -> bool {
  let Some(ordinal) = name.strip_prefix("segment-").and_then(|value| value.strip_suffix(".ainx")) else {
    return false;
  };
  ordinal.len() == 16 && ordinal.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) && ordinal != "0000000000000000"
}

fn canonical_checkpoint_name(name: &str) -> bool {
  name.len() == 16 && name.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) && name != "0000000000000000"
}

fn canonical_pending_name(name: &str) -> bool {
  let Some(value) = name.strip_prefix(".capture-").and_then(|value| value.strip_suffix(".pending")) else {
    return false;
  };
  uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

fn state_accounting_bytes(path: &Path, hash_width: usize) -> Result<u64, MigrationCaptureWorkspaceErrorV1> {
  let path_bytes = u64::try_from(path.as_os_str().len())
    .map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("workspace path length exceeds u64".to_string()))?;
  let hash_width_u64 =
    u64::try_from(hash_width).map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("workspace hash width exceeds u64".to_string()))?;
  let hash_bytes = hash_width_u64
    .checked_mul(8)
    .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("workspace state hash accounting overflow".to_string()))?;
  let summary_bytes = u64::try_from(size_of::<MigrationCaptureWorkspaceSummaryV1>())
    .map_err(|_| MigrationCaptureWorkspaceErrorV1::Capacity("workspace summary accounting exceeds u64".to_string()))?;
  STATE_ACCOUNTING_BASE_BYTES
    .checked_add(
      path_bytes
        .checked_mul(4)
        .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("workspace path accounting overflow".to_string()))?,
    )
    .and_then(|bytes| bytes.checked_add(hash_bytes))
    .and_then(|bytes| bytes.checked_add(summary_bytes))
    .ok_or_else(|| MigrationCaptureWorkspaceErrorV1::Capacity("workspace state accounting overflow".to_string()))
}

fn segment_name(ordinal: u64) -> String {
  format!("segment-{ordinal:016x}.ainx")
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}
