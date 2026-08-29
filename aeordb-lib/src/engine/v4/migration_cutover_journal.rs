//! Byte-exact writer for the frozen external `cutover.acut` A/B journal.
//!
//! The SideBySideCutover body remains owned and validated by `system_control`.
//! This module deliberately treats that body as opaque until P8 freezes the
//! complete typed transition semantics. It only proves that an already-valid
//! ACUT control body is mirrored byte-for-byte into the node-local journal.

use std::fmt;
use std::fs;
use std::mem::size_of;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::emergency_spill::open_regular_file_read_write_no_follow;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::{
  NativeDurabilityError, NativeDurabilityResult, PlatformFileIdentityDescriptorV1, platform_file_identity,
  platform_file_identity_from_file, preallocate_file, sync_directory_native, sync_file_all_native, write_file_at_native,
};
#[cfg(unix)]
use crate::engine::native_durability::read_file_at_native;
#[cfg(windows)]
use crate::engine::native_durability::NativeDurabilityOperation;
use crate::engine::HashAlgorithm;

use super::private_workspace::{
  PrivateWorkspaceErrorV1, create_private_directory_synced, create_private_regular_file, ensure_capacity, validate_existing_directory,
  validate_private_directory_readonly, validate_private_regular_file,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::system_control::{
  JOURNAL_LENGTH, JOURNAL_SLOT_CRC_OFFSET, JOURNAL_SLOT_LENGTH, SystemControlKindV1, SystemControlSlotV1, decode_system_control,
  select_cutover_journal,
};

const JOURNAL_SLOT_BODY_OFFSET: usize = 32;
const CUTOVER_CONTROL_IDENTITY_VALIDATION_BYTES: u64 = 16;
const JOURNAL_PUBLICATION_BUFFER_BYTES: u64 = (2 * JOURNAL_LENGTH + JOURNAL_SLOT_LENGTH) as u64 + CUTOVER_CONTROL_IDENTITY_VALIDATION_BYTES;
pub const CUTOVER_JOURNAL_FILE_NAME_V1: &str = "cutover.acut";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutoverJournalWorkspaceOptionsV1 {
  minimum_free_bytes: u64,
}

impl CutoverJournalWorkspaceOptionsV1 {
  pub const fn new(minimum_free_bytes: u64) -> Self {
    Self { minimum_free_bytes }
  }

  pub const fn minimum_free_bytes(self) -> u64 {
    self.minimum_free_bytes
  }
}

/// Stable observation points for synthetic crash rehearsal.
///
/// Cancellation is checked before `BeforeSlotWrite`. Once that boundary has
/// been crossed, the operation either returns a receipt or a failure whose
/// disposition requires recovery from the durable file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutoverJournalPublicationBoundaryV1 {
  BeforeSlotWrite,
  AfterSlotWrite,
  AfterFileSync,
  AfterReadBack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutoverJournalFailureDispositionV1 {
  PriorAuthorityRetained,
  SelectionMustBeReopened,
  SyncedSelectionMustBeReopened,
  SuccessorDurable,
}

impl CutoverJournalPublicationBoundaryV1 {
  pub const fn failure_disposition(self) -> CutoverJournalFailureDispositionV1 {
    match self {
      Self::BeforeSlotWrite => CutoverJournalFailureDispositionV1::PriorAuthorityRetained,
      Self::AfterSlotWrite => CutoverJournalFailureDispositionV1::SelectionMustBeReopened,
      Self::AfterFileSync => CutoverJournalFailureDispositionV1::SyncedSelectionMustBeReopened,
      Self::AfterReadBack => CutoverJournalFailureDispositionV1::SuccessorDurable,
    }
  }
}

/// Rehearsal-only hook used to stop at exact file-publication boundaries.
/// Returning `true` injects a failure at the supplied boundary.
pub trait CutoverJournalFaultInjectorV1 {
  fn inject(&mut self, boundary: CutoverJournalPublicationBoundaryV1) -> bool;
}

#[derive(Debug, Error)]
pub enum CutoverJournalWorkspaceErrorV1 {
  #[error("cutover journal workspace identity is invalid: {0}")]
  Identity(String),
  #[error("cutover journal workspace path is invalid or unavailable: {0}")]
  Path(String),
  #[error("cutover journal workspace state refuses the operation: {0}")]
  State(&'static str),
  #[error("cutover journal workspace is already owned: {0}")]
  Locked(String),
  #[error("cutover journal workspace operation was canceled")]
  Canceled,
  #[error("cutover journal workspace capacity is unavailable: {0}")]
  Capacity(String),
  #[error("cutover journal workspace format is invalid: {0}")]
  Format(#[source] Box<FormatError>),
  #[error("cutover journal workspace memory admission failed: {0}")]
  Memory(#[source] Box<MemoryCoordinatorError>),
  #[error("cutover journal workspace I/O failed during {operation}: {source}")]
  Io {
    operation: &'static str,
    #[source]
    source: std::io::Error,
  },
  #[error("cutover journal workspace durability failed after {boundary:?}: {source}")]
  PublicationDurability {
    boundary: CutoverJournalPublicationBoundaryV1,
    #[source]
    source: Box<NativeDurabilityError>,
  },
  #[error("cutover journal workspace publication validation failed after {boundary:?}: {message}")]
  PublicationFormat { boundary: CutoverJournalPublicationBoundaryV1, message: String },
  #[error("cutover journal workspace read-back failed after {boundary:?}: {source}")]
  PublicationReadBack {
    boundary: CutoverJournalPublicationBoundaryV1,
    #[source]
    source: Box<CutoverJournalWorkspaceErrorV1>,
  },
  #[error("cutover journal workspace injected fault after {boundary:?}")]
  InjectedFault { boundary: CutoverJournalPublicationBoundaryV1 },
  #[error("cutover journal workspace durability failed: {0}")]
  Durability(#[source] Box<NativeDurabilityError>),
}

impl CutoverJournalWorkspaceErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Identity(_) => "cutover_journal_workspace_identity",
      Self::Path(_) => "cutover_journal_workspace_path",
      Self::State(_) => "cutover_journal_workspace_state",
      Self::Locked(_) => "cutover_journal_workspace_locked",
      Self::Canceled => "cutover_journal_workspace_cancelled",
      Self::Capacity(_) => "cutover_journal_workspace_capacity",
      Self::Format(_) | Self::PublicationFormat { .. } => "cutover_journal_workspace_format",
      Self::PublicationReadBack { source, .. } => source.code(),
      Self::Memory(_) => "cutover_journal_workspace_memory",
      Self::Io { .. } => "cutover_journal_workspace_io",
      Self::PublicationDurability { .. } | Self::Durability(_) => "cutover_journal_workspace_durability",
      Self::InjectedFault { .. } => "cutover_journal_workspace_injected_fault",
    }
  }

  pub const fn publication_boundary(&self) -> Option<CutoverJournalPublicationBoundaryV1> {
    match self {
      Self::PublicationDurability { boundary, .. }
      | Self::PublicationFormat { boundary, .. }
      | Self::PublicationReadBack { boundary, .. }
      | Self::InjectedFault { boundary } => Some(*boundary),
      _ => None,
    }
  }

  pub const fn failure_disposition(&self) -> CutoverJournalFailureDispositionV1 {
    match self.publication_boundary() {
      Some(boundary) => boundary.failure_disposition(),
      None => CutoverJournalFailureDispositionV1::PriorAuthorityRetained,
    }
  }
}

impl From<PrivateWorkspaceErrorV1> for CutoverJournalWorkspaceErrorV1 {
  fn from(error: PrivateWorkspaceErrorV1) -> Self {
    match error {
      PrivateWorkspaceErrorV1::Path(message) => Self::Path(message),
      #[cfg(windows)]
      PrivateWorkspaceErrorV1::State(message) => Self::Path(message),
      PrivateWorkspaceErrorV1::Capacity(message) => Self::Capacity(message),
      #[cfg(windows)]
      PrivateWorkspaceErrorV1::Allocation(message) => Self::Capacity(message),
      PrivateWorkspaceErrorV1::Io { operation, source } => Self::Io { operation, source },
      PrivateWorkspaceErrorV1::Durability(source) => Self::Durability(source),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutoverJournalPublicationReceiptV1 {
  selected_slot: SystemControlSlotV1,
  sequence: u64,
  redundancy_degraded: bool,
  changed: bool,
}

impl CutoverJournalPublicationReceiptV1 {
  pub const fn selected_slot(self) -> SystemControlSlotV1 {
    self.selected_slot
  }

  pub const fn sequence(self) -> u64 {
    self.sequence
  }

  pub const fn redundancy_degraded(self) -> bool {
    self.redundancy_degraded
  }

  pub const fn changed(self) -> bool {
    self.changed
  }
}

pub struct DurableCutoverJournalWorkspaceV1 {
  workspace_path: PathBuf,
  journal_path: PathBuf,
  journal_file: fs::File,
  journal_identity: PlatformFileIdentityDescriptorV1,
  journal_bytes: Vec<u8>,
  algorithm: HashAlgorithm,
  selected_slot: SystemControlSlotV1,
  sequence: u64,
  redundancy_degraded: bool,
  options: CutoverJournalWorkspaceOptionsV1,
  cancellation: CancellationToken,
  state_memory: MemoryReservation,
  failed: bool,
}

impl fmt::Debug for DurableCutoverJournalWorkspaceV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("DurableCutoverJournalWorkspaceV1")
      .field("workspace_path", &self.workspace_path)
      .field("journal_path", &self.journal_path)
      .field("selected_slot", &self.selected_slot)
      .field("sequence", &self.sequence)
      .field("redundancy_degraded", &self.redundancy_degraded)
      .field("failed", &self.failed)
      .finish()
  }
}

impl DurableCutoverJournalWorkspaceV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn create(
    workspace_path: &Path,
    sequence_a: u64,
    sequence_b: u64,
    encoded_control: &[u8],
    algorithm: HashAlgorithm,
    options: CutoverJournalWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, CutoverJournalWorkspaceErrorV1> {
    check_cancellation(&cancellation)?;
    validate_workspace_path(workspace_path)?;
    let reserved_memory_bytes = state_accounting_bytes(workspace_path)?;
    let state_memory = memory
      .reserve(MemoryOwner::Migration, reserved_memory_bytes, AdmissionClass::Maintenance)
      .map_err(|source| CutoverJournalWorkspaceErrorV1::Memory(Box::new(source)))?;
    let journal_path = workspace_path.join(CUTOVER_JOURNAL_FILE_NAME_V1);
    let journal_bytes = encode_cutover_journal_pair_v1(sequence_a, sequence_b, encoded_control, algorithm)
      .map_err(|source| CutoverJournalWorkspaceErrorV1::Format(Box::new(source)))?;
    let expected_body =
      validated_cutover_body(encoded_control, algorithm).map_err(|source| CutoverJournalWorkspaceErrorV1::Format(Box::new(source)))?;
    let parent = workspace_path.parent().ok_or_else(|| CutoverJournalWorkspaceErrorV1::Path("workspace path has no parent".to_string()))?;
    validate_existing_directory(parent, "cutover journal workspace parent")?;
    ensure_capacity(parent, JOURNAL_LENGTH as u64, options.minimum_free_bytes)?;
    check_cancellation(&cancellation)?;

    create_private_directory_synced(workspace_path, parent)?;
    let journal_file = create_private_regular_file(&journal_path, "cutover journal")?;
    acquire_exclusive_journal_lock(&journal_file, &journal_path)?;
    let journal_identity = capture_journal_identity(&journal_file, &journal_path)?;
    preallocate_file(&journal_file, JOURNAL_LENGTH as u64)
      .map_err(|source| CutoverJournalWorkspaceErrorV1::Durability(Box::new(source)))?;
    journal_file
      .set_len(JOURNAL_LENGTH as u64)
      .map_err(|source| CutoverJournalWorkspaceErrorV1::Io { operation: "cutover journal exact length", source })?;
    write_file_at_native(&journal_file, 0, &journal_bytes)
      .map_err(|source| CutoverJournalWorkspaceErrorV1::Durability(Box::new(source)))?;
    sync_file_all_native(&journal_file).map_err(|source| CutoverJournalWorkspaceErrorV1::Durability(Box::new(source)))?;
    sync_directory_native(workspace_path).map_err(|source| CutoverJournalWorkspaceErrorV1::Durability(Box::new(source)))?;
    let read_back = read_exact_journal(&journal_file)?;
    if read_back != journal_bytes {
      return Err(CutoverJournalWorkspaceErrorV1::PublicationFormat {
        boundary: CutoverJournalPublicationBoundaryV1::AfterFileSync,
        message: "created journal read-back differs from encoded bytes".to_string(),
      });
    }
    let (selected_slot, sequence, redundancy_degraded) = validate_selected_journal(&read_back, expected_body, algorithm)?;
    Ok(Self {
      workspace_path: workspace_path.to_path_buf(),
      journal_path,
      journal_file,
      journal_identity,
      journal_bytes: read_back,
      algorithm,
      selected_slot,
      sequence,
      redundancy_degraded,
      options,
      cancellation,
      state_memory,
      failed: false,
    })
  }

  pub fn open(
    workspace_path: &Path,
    expected_encoded_control: &[u8],
    algorithm: HashAlgorithm,
    options: CutoverJournalWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, CutoverJournalWorkspaceErrorV1> {
    let expected_body = validated_cutover_body(expected_encoded_control, algorithm)
      .map_err(|source| CutoverJournalWorkspaceErrorV1::Format(Box::new(source)))?;
    Self::open_selected_internal(workspace_path, Some(expected_body), algorithm, options, cancellation, memory)
  }

  /// Reopens and locks whichever valid ACUT slot is durably selected.
  ///
  /// This is intentionally untyped: the cutover transition owner must decode
  /// and bind the selected opaque body against database authority and admitted
  /// evidence before it may publish or mutate the namespace.
  pub fn open_selected(
    workspace_path: &Path,
    algorithm: HashAlgorithm,
    options: CutoverJournalWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, CutoverJournalWorkspaceErrorV1> {
    Self::open_selected_internal(workspace_path, None, algorithm, options, cancellation, memory)
  }

  fn open_selected_internal(
    workspace_path: &Path,
    expected_body: Option<&[u8]>,
    algorithm: HashAlgorithm,
    options: CutoverJournalWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
  ) -> Result<Self, CutoverJournalWorkspaceErrorV1> {
    check_cancellation(&cancellation)?;
    validate_workspace_path(workspace_path)?;
    let reserved_memory_bytes = state_accounting_bytes(workspace_path)?;
    let state_memory = memory
      .reserve(MemoryOwner::Migration, reserved_memory_bytes, AdmissionClass::Maintenance)
      .map_err(|source| CutoverJournalWorkspaceErrorV1::Memory(Box::new(source)))?;
    let journal_path = workspace_path.join(CUTOVER_JOURNAL_FILE_NAME_V1);
    validate_private_directory_readonly(workspace_path, "cutover journal workspace")?;
    ensure_capacity(workspace_path, 0, options.minimum_free_bytes)?;
    let journal_file =
      open_regular_file_read_write_no_follow(&journal_path).map_err(|source| CutoverJournalWorkspaceErrorV1::Path(source.to_string()))?;
    acquire_exclusive_journal_lock(&journal_file, &journal_path)?;
    validate_private_regular_file(&journal_path, &journal_file, "cutover journal")?;
    let journal_identity = capture_journal_identity(&journal_file, &journal_path)?;
    let journal_bytes = read_exact_journal(&journal_file)?;
    let selection =
      select_cutover_journal(&journal_bytes, algorithm).map_err(|source| CutoverJournalWorkspaceErrorV1::Format(Box::new(source)))?;
    if expected_body.is_some_and(|expected| selection.body != expected) {
      return Err(CutoverJournalWorkspaceErrorV1::Identity(
        "selected external journal body differs from the expected database cutover control".to_string(),
      ));
    }
    let selected_slot = selection.selected_slot;
    let sequence = selection.sequence;
    let redundancy_degraded = selection.redundancy_degraded;
    check_cancellation(&cancellation)?;
    Ok(Self {
      workspace_path: workspace_path.to_path_buf(),
      journal_path,
      journal_file,
      journal_identity,
      journal_bytes,
      algorithm,
      selected_slot,
      sequence,
      redundancy_degraded,
      options,
      cancellation,
      state_memory,
      failed: false,
    })
  }

  pub fn workspace_path(&self) -> &Path {
    &self.workspace_path
  }

  pub fn journal_path(&self) -> &Path {
    &self.journal_path
  }

  pub const fn selected_slot(&self) -> SystemControlSlotV1 {
    self.selected_slot
  }

  pub const fn sequence(&self) -> u64 {
    self.sequence
  }

  pub const fn redundancy_degraded(&self) -> bool {
    self.redundancy_degraded
  }

  pub const fn reserved_memory_bytes(&self) -> u64 {
    self.state_memory.bytes()
  }

  pub fn selected_body(&self) -> Result<&[u8], CutoverJournalWorkspaceErrorV1> {
    select_cutover_journal(&self.journal_bytes, self.algorithm)
      .map(|selection| selection.body)
      .map_err(|source| CutoverJournalWorkspaceErrorV1::Format(Box::new(source)))
  }

  pub fn publish(&mut self, encoded_control: &[u8]) -> Result<CutoverJournalPublicationReceiptV1, CutoverJournalWorkspaceErrorV1> {
    self.publish_with_fault_injector(encoded_control, &mut NoCutoverJournalFaultV1)
  }

  pub fn publish_with_fault_injector(
    &mut self,
    encoded_control: &[u8],
    fault_injector: &mut dyn CutoverJournalFaultInjectorV1,
  ) -> Result<CutoverJournalPublicationReceiptV1, CutoverJournalWorkspaceErrorV1> {
    self.preflight_publication()?;
    let target_body =
      validated_cutover_body(encoded_control, self.algorithm).map_err(|source| CutoverJournalWorkspaceErrorV1::Format(Box::new(source)))?;
    let current_body_matches = self.selected_body()? == target_body;
    if current_body_matches && !self.redundancy_degraded {
      return Ok(self.receipt(false));
    }
    let next_sequence = if current_body_matches {
      self.sequence
    } else {
      self
        .sequence
        .checked_add(1)
        .ok_or_else(|| CutoverJournalWorkspaceErrorV1::Identity("cutover journal sequence is exhausted".to_string()))?
    };
    let (write_offset, slot_offset) = match self.selected_slot {
      SystemControlSlotV1::A => (JOURNAL_SLOT_LENGTH as u64, JOURNAL_SLOT_LENGTH),
      SystemControlSlotV1::B => (0, 0),
      SystemControlSlotV1::Immutable => {
        return Err(CutoverJournalWorkspaceErrorV1::State("external cutover journal selected an immutable slot"));
      }
    };
    let slot = encode_cutover_journal_slot_v1(next_sequence, encoded_control, self.algorithm)
      .map_err(|source| CutoverJournalWorkspaceErrorV1::Format(Box::new(source)))?;
    inject_fault(fault_injector, CutoverJournalPublicationBoundaryV1::BeforeSlotWrite)?;
    if let Err(source) = write_file_at_native(&self.journal_file, write_offset, &slot) {
      self.failed = true;
      return Err(CutoverJournalWorkspaceErrorV1::PublicationDurability {
        boundary: CutoverJournalPublicationBoundaryV1::AfterSlotWrite,
        source: Box::new(source),
      });
    }
    inject_fault(fault_injector, CutoverJournalPublicationBoundaryV1::AfterSlotWrite).inspect_err(|_| self.failed = true)?;
    if let Err(source) = sync_file_all_native(&self.journal_file) {
      self.failed = true;
      return Err(CutoverJournalWorkspaceErrorV1::PublicationDurability {
        boundary: CutoverJournalPublicationBoundaryV1::AfterSlotWrite,
        source: Box::new(source),
      });
    }
    inject_fault(fault_injector, CutoverJournalPublicationBoundaryV1::AfterFileSync).inspect_err(|_| self.failed = true)?;
    let read_back = match read_exact_journal(&self.journal_file) {
      Ok(read_back) => read_back,
      Err(CutoverJournalWorkspaceErrorV1::Durability(source)) => {
        self.failed = true;
        return Err(CutoverJournalWorkspaceErrorV1::PublicationDurability {
          boundary: CutoverJournalPublicationBoundaryV1::AfterFileSync,
          source,
        });
      }
      Err(error) => {
        self.failed = true;
        return Err(CutoverJournalWorkspaceErrorV1::PublicationReadBack {
          boundary: CutoverJournalPublicationBoundaryV1::AfterFileSync,
          source: Box::new(error),
        });
      }
    };
    if read_back[slot_offset..slot_offset + JOURNAL_SLOT_LENGTH] != slot {
      self.failed = true;
      return Err(CutoverJournalWorkspaceErrorV1::PublicationFormat {
        boundary: CutoverJournalPublicationBoundaryV1::AfterFileSync,
        message: "published inactive slot does not match exact read-back".to_string(),
      });
    }
    let (selected_slot, sequence, redundancy_degraded) = match validate_selected_journal(&read_back, target_body, self.algorithm) {
      Ok(selection) => selection,
      Err(error) => {
        self.failed = true;
        return Err(CutoverJournalWorkspaceErrorV1::PublicationFormat {
          boundary: CutoverJournalPublicationBoundaryV1::AfterFileSync,
          message: error.to_string(),
        });
      }
    };
    if sequence != next_sequence || redundancy_degraded {
      self.failed = true;
      return Err(CutoverJournalWorkspaceErrorV1::PublicationFormat {
        boundary: CutoverJournalPublicationBoundaryV1::AfterFileSync,
        message: "published journal did not select one healthy expected successor".to_string(),
      });
    }
    if let Err(error) = self.validate_current_journal_identity() {
      self.failed = true;
      return Err(CutoverJournalWorkspaceErrorV1::PublicationFormat {
        boundary: CutoverJournalPublicationBoundaryV1::AfterFileSync,
        message: error.to_string(),
      });
    }
    inject_fault(fault_injector, CutoverJournalPublicationBoundaryV1::AfterReadBack).inspect_err(|_| self.failed = true)?;
    self.journal_bytes = read_back;
    self.selected_slot = selected_slot;
    self.sequence = sequence;
    self.redundancy_degraded = redundancy_degraded;
    Ok(self.receipt(true))
  }

  fn preflight_publication(&self) -> Result<(), CutoverJournalWorkspaceErrorV1> {
    if self.failed {
      return Err(CutoverJournalWorkspaceErrorV1::State("writer is latched after an uncertain journal publication"));
    }
    check_cancellation(&self.cancellation)?;
    self.state_memory.check_admission().map_err(|source| CutoverJournalWorkspaceErrorV1::Memory(Box::new(source)))?;
    ensure_capacity(&self.workspace_path, 0, self.options.minimum_free_bytes)?;
    self.validate_current_journal_identity()?;
    Ok(())
  }

  fn validate_current_journal_identity(&self) -> Result<(), CutoverJournalWorkspaceErrorV1> {
    let handle_identity = platform_file_identity_from_file(&self.journal_file)
      .map_err(|source| CutoverJournalWorkspaceErrorV1::Durability(Box::new(source)))?;
    if !self.journal_identity.represents_same_physical_file_as(handle_identity) {
      return Err(CutoverJournalWorkspaceErrorV1::Identity("locked cutover journal handle changed physical identity".to_string()));
    }
    let path_identity =
      platform_file_identity(&self.journal_path).map_err(|source| CutoverJournalWorkspaceErrorV1::Durability(Box::new(source)))?;
    if !self.journal_identity.represents_same_physical_file_as(path_identity) {
      return Err(CutoverJournalWorkspaceErrorV1::Identity("cutover.acut path no longer resolves to the locked journal file".to_string()));
    }
    Ok(())
  }

  const fn receipt(&self, changed: bool) -> CutoverJournalPublicationReceiptV1 {
    CutoverJournalPublicationReceiptV1 {
      selected_slot: self.selected_slot,
      sequence: self.sequence,
      redundancy_degraded: self.redundancy_degraded,
      changed,
    }
  }
}

struct NoCutoverJournalFaultV1;

impl CutoverJournalFaultInjectorV1 for NoCutoverJournalFaultV1 {
  fn inject(&mut self, _boundary: CutoverJournalPublicationBoundaryV1) -> bool {
    false
  }
}

fn inject_fault(
  fault_injector: &mut dyn CutoverJournalFaultInjectorV1,
  boundary: CutoverJournalPublicationBoundaryV1,
) -> Result<(), CutoverJournalWorkspaceErrorV1> {
  if fault_injector.inject(boundary) {
    return Err(CutoverJournalWorkspaceErrorV1::InjectedFault { boundary });
  }
  Ok(())
}

fn validate_workspace_path(workspace_path: &Path) -> Result<(), CutoverJournalWorkspaceErrorV1> {
  if !workspace_path.is_absolute() {
    return Err(CutoverJournalWorkspaceErrorV1::Path("cutover journal workspace path must be absolute".to_string()));
  }
  if workspace_path.file_name().is_none() {
    return Err(CutoverJournalWorkspaceErrorV1::Path("cutover journal workspace path has no final component".to_string()));
  }
  Ok(())
}

fn acquire_exclusive_journal_lock(file: &fs::File, journal_path: &Path) -> Result<(), CutoverJournalWorkspaceErrorV1> {
  match FileExt::try_lock_exclusive(file) {
    Ok(()) => Ok(()),
    Err(source) if is_journal_lock_contention(&source) => {
      Err(CutoverJournalWorkspaceErrorV1::Locked(format!("another cutover journal owner holds {}", journal_path.display())))
    }
    Err(source) => Err(CutoverJournalWorkspaceErrorV1::Io { operation: "cutover journal exclusive lock", source }),
  }
}

fn is_journal_lock_contention(error: &std::io::Error) -> bool {
  if error.kind() == std::io::ErrorKind::WouldBlock {
    return true;
  }
  #[cfg(windows)]
  {
    // Windows reports byte-range lock contention as ERROR_SHARING_VIOLATION
    // or ERROR_LOCK_VIOLATION without mapping either value to WouldBlock.
    return matches!(error.raw_os_error(), Some(32 | 33));
  }
  #[cfg(not(windows))]
  false
}

fn capture_journal_identity(
  file: &fs::File,
  journal_path: &Path,
) -> Result<PlatformFileIdentityDescriptorV1, CutoverJournalWorkspaceErrorV1> {
  let handle_identity =
    platform_file_identity_from_file(file).map_err(|source| CutoverJournalWorkspaceErrorV1::Durability(Box::new(source)))?;
  let path_identity =
    platform_file_identity(journal_path).map_err(|source| CutoverJournalWorkspaceErrorV1::Durability(Box::new(source)))?;
  if !handle_identity.represents_same_physical_file_as(path_identity) {
    return Err(CutoverJournalWorkspaceErrorV1::Identity("cutover.acut path does not resolve to its locked file handle".to_string()));
  }
  Ok(handle_identity)
}

fn read_exact_journal(file: &fs::File) -> Result<Vec<u8>, CutoverJournalWorkspaceErrorV1> {
  let length =
    file.metadata().map_err(|source| CutoverJournalWorkspaceErrorV1::Io { operation: "cutover journal metadata", source })?.len();
  if length != JOURNAL_LENGTH as u64 {
    return Err(CutoverJournalWorkspaceErrorV1::Format(Box::new(FormatError::new(
      MalformedInputClass::TruncationOrTrailingBytes,
      "cutover_journal_workspace_length",
      format!("external cutover journal is {length} bytes instead of {JOURNAL_LENGTH}"),
    ))));
  }
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(JOURNAL_LENGTH).map_err(|source| {
    CutoverJournalWorkspaceErrorV1::Format(Box::new(FormatError::new(
      MalformedInputClass::AllocationAmplification,
      "cutover_journal_workspace_allocation",
      source.to_string(),
    )))
  })?;
  bytes.resize(JOURNAL_LENGTH, 0);
  read_locked_file_at_native(file, 0, &mut bytes).map_err(|source| CutoverJournalWorkspaceErrorV1::Durability(Box::new(source)))?;
  Ok(bytes)
}

#[cfg(unix)]
fn read_locked_file_at_native(file: &fs::File, offset: u64, bytes: &mut [u8]) -> NativeDurabilityResult<()> {
  read_file_at_native(file, offset, bytes)
}

#[cfg(windows)]
fn read_locked_file_at_native(file: &fs::File, offset: u64, bytes: &mut [u8]) -> NativeDurabilityResult<()> {
  use std::os::windows::fs::FileExt;

  // Windows byte-range locks reject reads from the separate handle created by
  // the shared positional reader. This workspace owns the handle exclusively,
  // and every journal access carries an explicit offset, so read through the
  // same locked handle without releasing the lock or opening a race window.
  let mut read = 0usize;
  while read < bytes.len() {
    let read_offset = offset.checked_add(read as u64).ok_or_else(|| {
      NativeDurabilityError::operation_io(
        NativeDurabilityOperation::ReadBack,
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "locked journal read offset overflow"),
      )
    })?;
    let count = file
      .seek_read(&mut bytes[read..], read_offset)
      .map_err(|source| NativeDurabilityError::operation_io(NativeDurabilityOperation::ReadBack, source))?;
    if count == 0 {
      return Err(NativeDurabilityError::operation_io(
        NativeDurabilityOperation::ReadBack,
        std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
      ));
    }
    read += count;
  }
  Ok(())
}

fn validate_selected_journal(
  journal_bytes: &[u8],
  expected_body: &[u8],
  algorithm: HashAlgorithm,
) -> Result<(SystemControlSlotV1, u64, bool), CutoverJournalWorkspaceErrorV1> {
  let selected =
    select_cutover_journal(journal_bytes, algorithm).map_err(|source| CutoverJournalWorkspaceErrorV1::Format(Box::new(source)))?;
  if selected.body != expected_body {
    return Err(CutoverJournalWorkspaceErrorV1::Identity(
      "selected external journal body differs from the expected database cutover control".to_string(),
    ));
  }
  Ok((selected.selected_slot, selected.sequence, selected.redundancy_degraded))
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), CutoverJournalWorkspaceErrorV1> {
  if cancellation.is_cancelled() {
    return Err(CutoverJournalWorkspaceErrorV1::Canceled);
  }
  Ok(())
}

fn state_accounting_bytes(workspace_path: &Path) -> Result<u64, CutoverJournalWorkspaceErrorV1> {
  let workspace_bytes = workspace_path.as_os_str().len() as u64;
  let journal_name_bytes = CUTOVER_JOURNAL_FILE_NAME_V1.len() as u64;
  let journal_bytes = workspace_bytes
    .checked_add(1)
    .and_then(|bytes| bytes.checked_add(journal_name_bytes))
    .ok_or_else(|| CutoverJournalWorkspaceErrorV1::Capacity("journal path accounting overflow".to_string()))?;
  let state_bytes = size_of::<DurableCutoverJournalWorkspaceV1>() as u64;
  JOURNAL_PUBLICATION_BUFFER_BYTES
    .checked_add(workspace_bytes)
    .and_then(|bytes| bytes.checked_add(journal_bytes))
    .and_then(|bytes| bytes.checked_add(state_bytes))
    .ok_or_else(|| CutoverJournalWorkspaceErrorV1::Capacity("workspace memory accounting overflow".to_string()))
}

pub fn encode_cutover_journal_slot_v1(sequence: u64, encoded_control: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_sequence(sequence)?;
  let body = validated_cutover_body(encoded_control, algorithm)?;
  let slot = encode_slot(sequence, body)?;

  let mut round_trip = allocate_zeroed(JOURNAL_LENGTH, "cutover_journal_roundtrip_allocation")?;
  round_trip[..JOURNAL_SLOT_LENGTH].copy_from_slice(&slot);
  round_trip[JOURNAL_SLOT_LENGTH..].copy_from_slice(&slot);
  let selected = select_cutover_journal(&round_trip, algorithm)?;
  if selected.sequence != sequence || selected.body != body {
    return Err(identity_error("cutover_journal_encode_roundtrip", "encoded cutover journal slot did not round-trip exactly"));
  }
  Ok(slot)
}

pub fn encode_cutover_journal_pair_v1(
  sequence_a: u64,
  sequence_b: u64,
  encoded_control: &[u8],
  algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  validate_sequence(sequence_a)?;
  validate_sequence(sequence_b)?;
  let body = validated_cutover_body(encoded_control, algorithm)?;
  let slot_a = encode_slot(sequence_a, body)?;
  let slot_b = encode_slot(sequence_b, body)?;
  let mut journal = allocate_zeroed(JOURNAL_LENGTH, "cutover_journal_pair_allocation")?;
  journal[..JOURNAL_SLOT_LENGTH].copy_from_slice(&slot_a);
  journal[JOURNAL_SLOT_LENGTH..].copy_from_slice(&slot_b);

  let selected = select_cutover_journal(&journal, algorithm)?;
  if selected.sequence != sequence_a.max(sequence_b) || selected.body != body {
    return Err(identity_error("cutover_journal_encode_roundtrip", "encoded cutover journal pair did not select the expected body"));
  }
  Ok(journal)
}

fn validated_cutover_body(encoded_control: &[u8], algorithm: HashAlgorithm) -> FormatResult<&[u8]> {
  let control = decode_system_control(encoded_control, algorithm)?;
  if control.kind != SystemControlKindV1::SideBySideCutover {
    return Err(kind_error("cutover_journal_control_kind", "external cutover journal input is not a SideBySideCutover control"));
  }
  Ok(control.body)
}

fn encode_slot(sequence: u64, body: &[u8]) -> FormatResult<Vec<u8>> {
  let body_end = JOURNAL_SLOT_BODY_OFFSET.checked_add(body.len()).ok_or_else(|| length_error("cutover journal body end overflow"))?;
  if body_end > JOURNAL_SLOT_CRC_OFFSET {
    return Err(length_error("cutover journal body exceeds its fixed slot"));
  }
  let body_length = body.len() as u32;

  let mut slot = allocate_zeroed(JOURNAL_SLOT_LENGTH, "cutover_journal_slot_allocation")?;
  slot[..4].copy_from_slice(b"ACUT");
  slot[4..6].copy_from_slice(&1u16.to_le_bytes());
  slot[6..8].copy_from_slice(&(JOURNAL_SLOT_LENGTH as u16).to_le_bytes());
  slot[8..16].copy_from_slice(&sequence.to_le_bytes());
  slot[16..20].copy_from_slice(&body_length.to_le_bytes());
  slot[JOURNAL_SLOT_BODY_OFFSET..body_end].copy_from_slice(body);
  let crc = crc32fast::hash(&slot[..JOURNAL_SLOT_CRC_OFFSET]);
  slot[JOURNAL_SLOT_CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
  Ok(slot)
}

fn validate_sequence(sequence: u64) -> FormatResult<()> {
  if sequence == 0 {
    return Err(identity_error("cutover_journal_sequence", "cutover journal slot sequence must be nonzero"));
  }
  Ok(())
}

fn allocate_zeroed(length: usize, code: &'static str) -> FormatResult<Vec<u8>> {
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(length)
    .map_err(|error| FormatError::new(MalformedInputClass::AllocationAmplification, code, error.to_string()))?;
  bytes.resize(length, 0);
  Ok(bytes)
}

fn length_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, "cutover_journal_length", context)
}

fn identity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, context)
}

fn kind_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::UnknownTypeKindOrEnum, code, context)
}
