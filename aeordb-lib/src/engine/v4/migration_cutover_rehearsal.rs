//! Disconnected same-filesystem rehearsal owner for pre-acceptance cutover.
//!
//! This owner consumes already-complete destination-verification progress. It
//! mirrors one typed ACUT body into the external journal and physical v4
//! control before touching the namespace, then installs the frozen v3 source
//! as an identified backup and the v4 shadow at the service path. It has no
//! route, CLI, startup, service-write, acceptance, or rollback-boundary hook.

use std::ffi::OsString;
use std::fmt::{self, Formatter};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use thiserror::Error as ThisError;
use tokio_util::sync::CancellationToken;

use crate::engine::emergency_spill::open_regular_file_no_follow;
use crate::engine::file_header::{FILE_HEADER_SIZE, read_active_header};
use crate::engine::memory_coordinator::{MemoryCoordinator, MemoryCoordinatorError};
use crate::engine::native_durability::{
  NativeDurabilityError, durable_install_new_native, platform_file_identity, sync_directory_native, sync_file_all_native,
};

use super::database_header::DATABASE_HEADER_V4_SLOT_LENGTH;
use super::first_authority::{FirstAuthorityPublicationErrorV1, V4FirstAuthorityPublisher};
use super::gc_retirement::RetirementJournalOwnerV1;
use super::migration_control::{
  MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED, MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
  MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD, MigrationPhaseV1, MigrationProgressBodyV1, MigrationProgressStateV1,
};
use super::migration_cutover_control::{
  CutoverArtifactRoleV1, CutoverStableFileIdentityEvidenceV1, SideBySideCutoverBodyV1, cutover_path_identity_hash_v1,
  cutover_stable_file_identity_hash_v1, decode_side_by_side_cutover_body_v1, decode_side_by_side_cutover_control_v1,
  encode_side_by_side_cutover_control_v1,
};
use super::migration_cutover_journal::{
  CutoverJournalFaultInjectorV1, CutoverJournalPublicationBoundaryV1, CutoverJournalWorkspaceErrorV1, CutoverJournalWorkspaceOptionsV1,
  DurableCutoverJournalWorkspaceV1,
};
use super::migration_owner::{
  MigrationCutoverFailureRequestV1, MigrationCutoverProgressRequestV1, MigrationStateOwnerErrorV1, MigrationStateOwnerV1,
};
use super::system_control::{SystemControlKindV1, decode_system_control};

const EXECUTION_CLOCK_STEPS: u64 = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBySideCutoverPathsV1 {
  service_path: PathBuf,
  destination_path: PathBuf,
  backup_path: PathBuf,
  journal_workspace_path: PathBuf,
}

impl SideBySideCutoverPathsV1 {
  pub fn new(
    service_path: impl Into<PathBuf>,
    destination_path: impl Into<PathBuf>,
    journal_workspace_path: impl Into<PathBuf>,
    migration_id: [u8; 16],
  ) -> Result<Self, SideBySideCutoverRehearsalErrorV1> {
    let service_path = service_path.into();
    let destination_path = destination_path.into();
    let journal_workspace_path = journal_workspace_path.into();
    if !service_path.is_absolute() || !destination_path.is_absolute() || !journal_workspace_path.is_absolute() {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_path_absolute",
        "service, destination, and journal workspace paths must be absolute",
      ));
    }
    if service_path == destination_path
      || service_path == journal_workspace_path
      || destination_path == journal_workspace_path
      || migration_id.iter().all(|byte| *byte == 0)
    {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_path_distinct",
        "cutover paths and migration identity must be distinct and nonzero",
      ));
    }
    let service_parent = service_path
      .parent()
      .ok_or_else(|| SideBySideCutoverRehearsalErrorV1::invalid("cutover_path_parent", "service path has no parent directory"))?;
    if destination_path.parent() != Some(service_parent) || journal_workspace_path.parent() != Some(service_parent) {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_path_parent",
        "service, destination, and journal workspace must share one same-filesystem parent",
      ));
    }
    let service_name = service_path
      .file_name()
      .ok_or_else(|| SideBySideCutoverRehearsalErrorV1::invalid("cutover_path_name", "service path has no final component"))?;
    let mut backup_name = OsString::from(service_name);
    backup_name.push(format!(".v3-{}.backup", hex::encode(migration_id)));
    let backup_path = service_parent.join(backup_name);
    if backup_path == destination_path || backup_path == journal_workspace_path {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_backup_path",
        "derived v3 backup path collides with another cutover artifact",
      ));
    }
    Ok(Self { service_path, destination_path, backup_path, journal_workspace_path })
  }

  pub fn service_path(&self) -> &Path {
    &self.service_path
  }

  pub fn destination_path(&self) -> &Path {
    &self.destination_path
  }

  pub fn backup_path(&self) -> &Path {
    &self.backup_path
  }

  pub fn journal_workspace_path(&self) -> &Path {
    &self.journal_workspace_path
  }

  fn parent(&self) -> Result<&Path, SideBySideCutoverRehearsalErrorV1> {
    self.service_path.parent().ok_or_else(|| {
      SideBySideCutoverRehearsalErrorV1::invalid("cutover_path_parent", "validated cutover service path no longer has a parent")
    })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBySideCutoverEvidenceV1 {
  pub source_path_digest: [u8; 32],
  pub destination_path_digest: [u8; 32],
  pub source_file: CutoverStableFileIdentityEvidenceV1,
  pub source_complete_file_checksum: [u8; 32],
  pub destination_file: CutoverStableFileIdentityEvidenceV1,
  pub destination_full_verification_evidence: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SideBySideCutoverClockV1 {
  pub updated_at_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideBySideCutoverBoundaryV1 {
  Journal { phase: MigrationPhaseV1, boundary: CutoverJournalPublicationBoundaryV1 },
  AfterInitialJournal,
  BeforeDatabaseControl { phase: MigrationPhaseV1 },
  AfterDatabaseControl { phase: MigrationPhaseV1 },
  BeforeSourceFileSync,
  AfterSourceFileSync,
  BeforeDestinationFileSync,
  AfterDestinationFileSync,
  BeforeParentDirectorySync,
  AfterParentDirectorySync,
  BeforeSourceBackupInstall,
  AfterSourceBackupInstall,
  BeforeDestinationServiceInstall,
  AfterDestinationServiceInstall,
  BeforeReopen,
  AfterReopen,
  BeforeRollbackDestinationRestore,
  AfterRollbackDestinationRestore,
  BeforeRollbackSourceRestore,
  AfterRollbackSourceRestore,
}

pub trait SideBySideCutoverFaultInjectorV1 {
  fn inject(&mut self, boundary: SideBySideCutoverBoundaryV1) -> bool;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideBySideCutoverFailureDispositionV1 {
  NoNamespaceMutation,
  InspectDurableEvidenceAndPaths,
  V4InstalledReadOnly,
}

#[derive(Debug, ThisError)]
pub enum SideBySideCutoverRehearsalErrorV1 {
  #[error("{code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("cutover operation canceled before the next durable boundary")]
  Canceled,
  #[error("cutover injected fault at {boundary:?}")]
  InjectedFault { boundary: SideBySideCutoverBoundaryV1 },
  #[error("migration owner refused cutover: {0}")]
  MigrationOwner(#[source] Box<MigrationStateOwnerErrorV1>),
  #[error("external cutover journal refused cutover: {0}")]
  Journal(#[source] Box<CutoverJournalWorkspaceErrorV1>),
  #[error("cutover native durability failed: {0}")]
  Durability(#[source] Box<NativeDurabilityError>),
  #[error("cutover destination authority failed: {0}")]
  Authority(#[source] Box<FirstAuthorityPublicationErrorV1>),
  #[error("cutover memory observation failed: {0}")]
  Memory(#[source] Box<MemoryCoordinatorError>),
  #[error("cutover file I/O failed during {operation}: {source}")]
  Io {
    operation: &'static str,
    #[source]
    source: std::io::Error,
  },
}

impl SideBySideCutoverRehearsalErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Canceled => "cutover_rehearsal_cancelled",
      Self::InjectedFault { .. } => "cutover_rehearsal_injected_fault",
      Self::MigrationOwner(source) => source.code(),
      Self::Journal(source) => source.code(),
      Self::Durability(_) => "cutover_rehearsal_durability",
      Self::Authority(source) => source.code(),
      Self::Memory(_) => "cutover_rehearsal_memory",
      Self::Io { .. } => "cutover_rehearsal_io",
    }
  }

  pub const fn failure_disposition(&self) -> SideBySideCutoverFailureDispositionV1 {
    match self {
      Self::InjectedFault {
        boundary:
          SideBySideCutoverBoundaryV1::AfterDestinationServiceInstall
          | SideBySideCutoverBoundaryV1::BeforeReopen
          | SideBySideCutoverBoundaryV1::AfterReopen,
      } => SideBySideCutoverFailureDispositionV1::V4InstalledReadOnly,
      Self::InjectedFault {
        boundary:
          SideBySideCutoverBoundaryV1::BeforeSourceBackupInstall
          | SideBySideCutoverBoundaryV1::AfterSourceBackupInstall
          | SideBySideCutoverBoundaryV1::BeforeDestinationServiceInstall
          | SideBySideCutoverBoundaryV1::BeforeRollbackDestinationRestore
          | SideBySideCutoverBoundaryV1::AfterRollbackDestinationRestore
          | SideBySideCutoverBoundaryV1::BeforeRollbackSourceRestore
          | SideBySideCutoverBoundaryV1::AfterRollbackSourceRestore,
      } => SideBySideCutoverFailureDispositionV1::InspectDurableEvidenceAndPaths,
      Self::Journal(source) => match source.failure_disposition() {
        super::migration_cutover_journal::CutoverJournalFailureDispositionV1::PriorAuthorityRetained => {
          SideBySideCutoverFailureDispositionV1::NoNamespaceMutation
        }
        _ => SideBySideCutoverFailureDispositionV1::InspectDurableEvidenceAndPaths,
      },
      Self::Durability(_) => SideBySideCutoverFailureDispositionV1::InspectDurableEvidenceAndPaths,
      _ => SideBySideCutoverFailureDispositionV1::NoNamespaceMutation,
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl From<MigrationStateOwnerErrorV1> for SideBySideCutoverRehearsalErrorV1 {
  fn from(source: MigrationStateOwnerErrorV1) -> Self {
    Self::MigrationOwner(Box::new(source))
  }
}

impl From<CutoverJournalWorkspaceErrorV1> for SideBySideCutoverRehearsalErrorV1 {
  fn from(source: CutoverJournalWorkspaceErrorV1) -> Self {
    Self::Journal(Box::new(source))
  }
}

impl From<NativeDurabilityError> for SideBySideCutoverRehearsalErrorV1 {
  fn from(source: NativeDurabilityError) -> Self {
    Self::Durability(Box::new(source))
  }
}

impl From<FirstAuthorityPublicationErrorV1> for SideBySideCutoverRehearsalErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(Box::new(source))
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBySideCutoverPreparationReceiptV1 {
  pub database_control_sequence: u64,
  pub journal_sequence: u64,
  pub destination_header_sequence: u64,
  pub idempotent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBySideCutoverExecutionReceiptV1 {
  pub database_control_sequence: u64,
  pub journal_sequence: u64,
  pub phase: MigrationPhaseV1,
  pub backup_path: PathBuf,
  pub service_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBySideCutoverRollbackReceiptV1 {
  pub database_control_sequence: u64,
  pub journal_sequence: u64,
  pub phase: MigrationPhaseV1,
  pub rollback_evidence: Vec<u8>,
  pub destination_path: PathBuf,
  pub service_path: PathBuf,
}

pub struct SideBySideCutoverRehearsalOwnerV1 {
  migration_owner: MigrationStateOwnerV1,
  paths: SideBySideCutoverPathsV1,
  evidence: SideBySideCutoverEvidenceV1,
  journal: DurableCutoverJournalWorkspaceV1,
  selected_body: SideBySideCutoverBodyV1,
  cancellation: CancellationToken,
}

impl fmt::Debug for SideBySideCutoverRehearsalOwnerV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("SideBySideCutoverRehearsalOwnerV1")
      .field("paths", &self.paths)
      .field("phase", &self.selected_body.phase)
      .field("journal_sequence", &self.selected_body.journal_sequence)
      .finish_non_exhaustive()
  }
}

impl SideBySideCutoverRehearsalOwnerV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn prepare(
    migration_owner: MigrationStateOwnerV1,
    paths: SideBySideCutoverPathsV1,
    evidence: SideBySideCutoverEvidenceV1,
    clock: SideBySideCutoverClockV1,
    journal_options: CutoverJournalWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<(Self, SideBySideCutoverPreparationReceiptV1), SideBySideCutoverRehearsalErrorV1> {
    Self::prepare_with_fault_injector(
      migration_owner,
      paths,
      evidence,
      clock,
      journal_options,
      cancellation,
      memory,
      retirement_owner,
      &mut NoSideBySideCutoverFaultV1,
    )
  }

  #[allow(clippy::too_many_arguments)]
  pub fn prepare_with_fault_injector(
    migration_owner: MigrationStateOwnerV1,
    paths: SideBySideCutoverPathsV1,
    evidence: SideBySideCutoverEvidenceV1,
    clock: SideBySideCutoverClockV1,
    journal_options: CutoverJournalWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
    retirement_owner: &mut RetirementJournalOwnerV1,
    fault_injector: &mut dyn SideBySideCutoverFaultInjectorV1,
  ) -> Result<(Self, SideBySideCutoverPreparationReceiptV1), SideBySideCutoverRehearsalErrorV1> {
    validate_clock(clock, 0)?;
    check_cancellation(&cancellation)?;
    validate_initial_namespace(&paths)?;
    validate_evidence(&migration_owner, &paths, &evidence)?;
    let progress = migration_owner.observe_cutover_progress(clock.updated_at_ms, clock.publication_timestamp_ms, clock.monotonic_now_ms)?;
    validate_destination_verification_binding(&progress, &evidence)?;
    if progress.phase != MigrationPhaseV1::DestinationVerify || progress.state != MigrationProgressStateV1::Complete {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_destination_verification_progress",
        "initial ACUT preparation requires selected complete destination-verification progress",
      ));
    }
    validate_source_file(&paths.service_path, &evidence.source_file, evidence.source_complete_file_checksum)?;
    validate_destination_file(&migration_owner, &paths.destination_path, &evidence.destination_file)?;

    let algorithm = migration_owner.hash_algorithm();
    let body = cutover_body(&migration_owner, &evidence, MigrationPhaseV1::DestinationVerify, 1, clock.updated_at_ms)?;
    validate_selected_stable_file_identity(algorithm, &paths.service_path, &evidence.source_file, &body.source_stable_file_identity_hash)?;
    validate_selected_stable_file_identity(
      algorithm,
      &paths.destination_path,
      &evidence.destination_file,
      &body.destination_stable_file_identity_hash,
    )?;
    let encoded = encode_side_by_side_cutover_control_v1(1, &body, algorithm).map_err(MigrationStateOwnerErrorV1::from)?;
    let journal = if paths.journal_workspace_path.exists() {
      DurableCutoverJournalWorkspaceV1::open(
        &paths.journal_workspace_path,
        &encoded,
        algorithm,
        journal_options,
        cancellation.clone(),
        memory,
      )?
    } else {
      DurableCutoverJournalWorkspaceV1::create(
        &paths.journal_workspace_path,
        1,
        1,
        &encoded,
        algorithm,
        journal_options,
        cancellation.clone(),
        memory,
      )?
    };
    inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterInitialJournal)?;
    inject_fault(fault_injector, SideBySideCutoverBoundaryV1::BeforeDatabaseControl { phase: MigrationPhaseV1::DestinationVerify })?;
    let database =
      migration_owner.publish_cutover_control(&body, clock.publication_timestamp_ms, clock.monotonic_now_ms, retirement_owner)?;
    inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterDatabaseControl { phase: MigrationPhaseV1::DestinationVerify })?;
    let owner = Self { migration_owner, paths, evidence, journal, selected_body: body, cancellation };
    owner.validate_mirrored_evidence()?;
    let receipt = SideBySideCutoverPreparationReceiptV1 {
      database_control_sequence: database.control_sequence,
      journal_sequence: database.journal_sequence,
      destination_header_sequence: owner.selected_body.destination_header_sequence,
      idempotent: database.idempotent,
    };
    Ok((owner, receipt))
  }

  #[allow(clippy::too_many_arguments)]
  pub fn recover_pre_acceptance(
    migration_owner: MigrationStateOwnerV1,
    paths: SideBySideCutoverPathsV1,
    evidence: SideBySideCutoverEvidenceV1,
    clock: SideBySideCutoverClockV1,
    journal_options: CutoverJournalWorkspaceOptionsV1,
    cancellation: CancellationToken,
    memory: &MemoryCoordinator,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<Self, SideBySideCutoverRehearsalErrorV1> {
    validate_clock(clock, 2)?;
    check_cancellation(&cancellation)?;
    validate_evidence(&migration_owner, &paths, &evidence)?;
    let progress = migration_owner.observe_cutover_progress(clock.updated_at_ms, clock.publication_timestamp_ms, clock.monotonic_now_ms)?;
    validate_destination_verification_binding(&progress, &evidence)?;
    let algorithm = migration_owner.hash_algorithm();
    let mut journal = DurableCutoverJournalWorkspaceV1::open_selected(
      &paths.journal_workspace_path,
      algorithm,
      journal_options,
      cancellation.clone(),
      memory,
    )?;
    let selected_body =
      decode_side_by_side_cutover_body_v1(journal.selected_body()?, algorithm).map_err(MigrationStateOwnerErrorV1::from)?;
    validate_recovery_body(&migration_owner, &evidence, &selected_body, journal.sequence())?;

    let database = migration_owner.publisher().load_mutable_system_control(
      SystemControlKindV1::SideBySideCutover,
      &migration_owner.database_id(),
      &migration_owner.migration_id(),
    )?;
    let database_control_sequence = match database {
      None => {
        if selected_body.phase != MigrationPhaseV1::DestinationVerify
          || selected_body.journal_sequence != 1
          || selected_body.last_error_evidence.iter().any(|byte| *byte != 0)
        {
          return Err(SideBySideCutoverRehearsalErrorV1::invalid(
            "cutover_recovery_database_control_missing",
            "only the initial destination-verification journal may recover a missing database ACUT control",
          ));
        }
        migration_owner
          .publish_cutover_control(&selected_body, clock.publication_timestamp_ms, clock.monotonic_now_ms, retirement_owner)?
          .control_sequence
      }
      Some(selected) => {
        let database_control =
          decode_side_by_side_cutover_control_v1(&selected.bytes, algorithm).map_err(MigrationStateOwnerErrorV1::from)?;
        validate_recovery_body(&migration_owner, &evidence, &database_control.body, database_control.body.journal_sequence)?;
        if database_control.body == selected_body {
          database_control.sequence
        } else if is_exact_recovery_successor(&database_control.body, &selected_body) {
          migration_owner
            .publish_cutover_control(&selected_body, clock.publication_timestamp_ms, clock.monotonic_now_ms, retirement_owner)?
            .control_sequence
        } else {
          return Err(SideBySideCutoverRehearsalErrorV1::invalid(
            "cutover_evidence_disagreement",
            "database and external ACUT evidence are neither equal nor one exact journal-first successor",
          ));
        }
      }
    };
    if journal.redundancy_degraded() {
      let encoded = encode_side_by_side_cutover_control_v1(database_control_sequence, &selected_body, algorithm)
        .map_err(MigrationStateOwnerErrorV1::from)?;
      journal.publish(&encoded)?;
    }
    let owner = Self { migration_owner, paths, evidence, journal, selected_body, cancellation };
    owner.validate_mirrored_evidence()?;
    let namespace = classify_namespace(&owner.paths, &owner.evidence)?;
    validate_selected_stable_file_identity(
      algorithm,
      namespace.source_path(&owner.paths),
      &owner.evidence.source_file,
      &owner.selected_body.source_stable_file_identity_hash,
    )?;
    validate_selected_stable_file_identity(
      algorithm,
      namespace.destination_path(&owner.paths),
      &owner.evidence.destination_file,
      &owner.selected_body.destination_stable_file_identity_hash,
    )?;
    if owner.selected_body.last_error_evidence.iter().any(|byte| *byte != 0) {
      let failure_clock = clock_at(clock, 1)?;
      owner.migration_owner.fail_cutover_progress(
        MigrationCutoverFailureRequestV1 {
          phase: owner.selected_body.phase,
          last_error_evidence: owner.selected_body.last_error_evidence.clone(),
          updated_at_ms: failure_clock.updated_at_ms,
          publication_timestamp_ms: failure_clock.publication_timestamp_ms,
          monotonic_now_ms: failure_clock.monotonic_now_ms,
        },
        retirement_owner,
      )?;
    }
    Ok(owner)
  }

  pub fn execute(
    &mut self,
    clock: SideBySideCutoverClockV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<SideBySideCutoverExecutionReceiptV1, SideBySideCutoverRehearsalErrorV1> {
    self.execute_with_fault_injector(clock, retirement_owner, &mut NoSideBySideCutoverFaultV1)
  }

  pub fn execute_with_fault_injector(
    &mut self,
    clock: SideBySideCutoverClockV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
    fault_injector: &mut dyn SideBySideCutoverFaultInjectorV1,
  ) -> Result<SideBySideCutoverExecutionReceiptV1, SideBySideCutoverRehearsalErrorV1> {
    validate_clock(clock, EXECUTION_CLOCK_STEPS)?;
    check_cancellation(&self.cancellation)?;
    self.validate_mirrored_evidence()?;
    if self.selected_body.last_error_evidence.iter().any(|byte| *byte != 0) {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_rollback_already_requested",
        "cutover execution cannot resume after durable rollback evidence was selected",
      ));
    }
    let mut namespace = classify_namespace(&self.paths, &self.evidence)?;
    let progress =
      self.migration_owner.observe_cutover_progress(clock.updated_at_ms, clock.publication_timestamp_ms, clock.monotonic_now_ms)?;
    validate_destination_verification_binding(&progress, &self.evidence)?;
    validate_selected_stable_file_identity(
      self.migration_owner.hash_algorithm(),
      namespace.source_path(&self.paths),
      &self.evidence.source_file,
      &self.selected_body.source_stable_file_identity_hash,
    )?;
    validate_selected_stable_file_identity(
      self.migration_owner.hash_algorithm(),
      namespace.destination_path(&self.paths),
      &self.evidence.destination_file,
      &self.selected_body.destination_stable_file_identity_hash,
    )?;
    if matches!(progress.state, MigrationProgressStateV1::Failed | MigrationProgressStateV1::Canceled) {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_progress_terminal",
        "failed or canceled migration progress cannot execute cutover",
      ));
    }
    let mut phase = progress.phase;
    let mut state = progress.state;
    if phase == MigrationPhaseV1::DestinationVerify && state == MigrationProgressStateV1::Complete {
      self.migration_owner.advance_cutover_progress(
        cutover_progress_request(MigrationPhaseV1::Cutover, MigrationProgressStateV1::Pending, clock, 0)?,
        retirement_owner,
      )?;
      phase = MigrationPhaseV1::Cutover;
      state = MigrationProgressStateV1::Pending;
    }
    if phase == MigrationPhaseV1::Cutover && state == MigrationProgressStateV1::Pending {
      if self.selected_body.phase == MigrationPhaseV1::DestinationVerify {
        let cutover_phase_body = cutover_body(
          &self.migration_owner,
          &self.evidence,
          MigrationPhaseV1::Cutover,
          self.next_journal_sequence()?,
          checked_updated_at(clock.updated_at_ms, 1)?,
        )?;
        self.publish_mirrored_control(cutover_phase_body, clock_at(clock, 1)?, retirement_owner, fault_injector)?;
      } else {
        require_selected_phase(&self.selected_body, MigrationPhaseV1::Cutover)?;
      }
      self.migration_owner.advance_cutover_progress(
        cutover_progress_request(MigrationPhaseV1::Cutover, MigrationProgressStateV1::Running, clock, 2)?,
        retirement_owner,
      )?;
      state = MigrationProgressStateV1::Running;
    }
    if phase == MigrationPhaseV1::Cutover && state == MigrationProgressStateV1::Running {
      require_selected_phase(&self.selected_body, MigrationPhaseV1::Cutover)?;
      let source_path = namespace.source_path(&self.paths);
      let destination_path = namespace.destination_path(&self.paths);
      validate_source_file(source_path, &self.evidence.source_file, self.evidence.source_complete_file_checksum)?;
      validate_destination_physical_file(&self.migration_owner, destination_path, &self.evidence.destination_file)?;

      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::BeforeSourceFileSync)?;
      let source_file = open_regular_file_no_follow(source_path).map_err(|source| SideBySideCutoverRehearsalErrorV1::Io {
        operation: "open frozen source for sync",
        source: std::io::Error::other(source.to_string()),
      })?;
      sync_file_all_native(&source_file)?;
      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterSourceFileSync)?;
      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::BeforeDestinationFileSync)?;
      let destination_file = open_regular_file_no_follow(destination_path).map_err(|source| SideBySideCutoverRehearsalErrorV1::Io {
        operation: "open destination for sync",
        source: std::io::Error::other(source.to_string()),
      })?;
      sync_file_all_native(&destination_file)?;
      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterDestinationFileSync)?;
      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::BeforeParentDirectorySync)?;
      sync_directory_native(self.paths.parent()?)?;
      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterParentDirectorySync)?;

      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::BeforeSourceBackupInstall)?;
      match namespace {
        CutoverNamespaceStateV1::Pristine => durable_install_new_native(&self.paths.service_path, &self.paths.backup_path)?,
        CutoverNamespaceStateV1::SourceLinked => remove_verified_duplicate(
          &self.paths.service_path,
          &self.paths.backup_path,
          &self.evidence.source_file,
          "remove linked source service path",
        )?,
        _ => {}
      }
      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterSourceBackupInstall)?;
      namespace = classify_namespace(&self.paths, &self.evidence)?;
      validate_source_file(&self.paths.backup_path, &self.evidence.source_file, self.evidence.source_complete_file_checksum)?;
      validate_selected_stable_file_identity(
        self.migration_owner.hash_algorithm(),
        &self.paths.backup_path,
        &self.evidence.source_file,
        &self.selected_body.source_stable_file_identity_hash,
      )?;

      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::BeforeDestinationServiceInstall)?;
      match namespace {
        CutoverNamespaceStateV1::SourceBackedUp => durable_install_new_native(&self.paths.destination_path, &self.paths.service_path)?,
        CutoverNamespaceStateV1::DestinationLinked => remove_verified_duplicate(
          &self.paths.destination_path,
          &self.paths.service_path,
          &self.evidence.destination_file,
          "remove linked destination shadow path",
        )?,
        CutoverNamespaceStateV1::DestinationInstalled => {}
        _ => {
          return Err(SideBySideCutoverRehearsalErrorV1::invalid(
            "cutover_namespace_transition",
            "source backup did not select a recoverable namespace state",
          ));
        }
      }
      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterDestinationServiceInstall)?;
      namespace = classify_namespace(&self.paths, &self.evidence)?;
      if namespace != CutoverNamespaceStateV1::DestinationInstalled {
        return Err(SideBySideCutoverRehearsalErrorV1::invalid(
          "cutover_namespace_transition",
          "destination installation did not select the v4 service namespace",
        ));
      }
      validate_destination_physical_file(&self.migration_owner, &self.paths.service_path, &self.evidence.destination_file)?;
      validate_selected_stable_file_identity(
        self.migration_owner.hash_algorithm(),
        &self.paths.service_path,
        &self.evidence.destination_file,
        &self.selected_body.destination_stable_file_identity_hash,
      )?;

      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::BeforeReopen)?;
      validate_reopened_destination(&self.migration_owner, &self.paths.service_path, &self.selected_body)?;
      inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterReopen)?;
      self.migration_owner.advance_cutover_progress(
        cutover_progress_request(MigrationPhaseV1::Cutover, MigrationProgressStateV1::Complete, clock, 3)?,
        retirement_owner,
      )?;
      state = MigrationProgressStateV1::Complete;
    }
    if phase == MigrationPhaseV1::Cutover && state == MigrationProgressStateV1::Complete {
      if namespace != CutoverNamespaceStateV1::DestinationInstalled {
        return Err(SideBySideCutoverRehearsalErrorV1::invalid(
          "cutover_progress_namespace_disagreement",
          "complete cutover progress requires the exact v4-installed namespace",
        ));
      }
      self.migration_owner.advance_cutover_progress(
        cutover_progress_request(MigrationPhaseV1::ReadOnlyValidation, MigrationProgressStateV1::Pending, clock, 4)?,
        retirement_owner,
      )?;
      phase = MigrationPhaseV1::ReadOnlyValidation;
      state = MigrationProgressStateV1::Pending;
    }
    if phase != MigrationPhaseV1::ReadOnlyValidation || state != MigrationProgressStateV1::Pending {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_progress_recovery_state",
        "cutover execution selected an unsupported pre-acceptance progress state",
      ));
    }
    if namespace != CutoverNamespaceStateV1::DestinationInstalled {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_progress_namespace_disagreement",
        "read-only validation requires the exact v4-installed namespace",
      ));
    }
    let database = if self.selected_body.phase == MigrationPhaseV1::Cutover {
      let validation_body = cutover_body(
        &self.migration_owner,
        &self.evidence,
        MigrationPhaseV1::ReadOnlyValidation,
        self.next_journal_sequence()?,
        checked_updated_at(clock.updated_at_ms, 5)?,
      )?;
      self.publish_mirrored_control(validation_body, clock_at(clock, 5)?, retirement_owner, fault_injector)?
    } else {
      require_selected_phase(&self.selected_body, MigrationPhaseV1::ReadOnlyValidation)?;
      selected_database_receipt(&self.migration_owner, &self.selected_body)?
    };
    validate_reopened_destination(&self.migration_owner, &self.paths.service_path, &self.selected_body)?;
    validate_source_file(&self.paths.backup_path, &self.evidence.source_file, self.evidence.source_complete_file_checksum)?;
    Ok(SideBySideCutoverExecutionReceiptV1 {
      database_control_sequence: database.control_sequence,
      journal_sequence: database.journal_sequence,
      phase: database.phase,
      backup_path: self.paths.backup_path.clone(),
      service_path: self.paths.service_path.clone(),
    })
  }

  pub fn rollback_pre_acceptance(
    &mut self,
    rollback_evidence: Vec<u8>,
    clock: SideBySideCutoverClockV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<SideBySideCutoverRollbackReceiptV1, SideBySideCutoverRehearsalErrorV1> {
    self.rollback_pre_acceptance_with_fault_injector(rollback_evidence, clock, retirement_owner, &mut NoSideBySideCutoverFaultV1)
  }

  pub fn rollback_pre_acceptance_with_fault_injector(
    &mut self,
    rollback_evidence: Vec<u8>,
    clock: SideBySideCutoverClockV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
    fault_injector: &mut dyn SideBySideCutoverFaultInjectorV1,
  ) -> Result<SideBySideCutoverRollbackReceiptV1, SideBySideCutoverRehearsalErrorV1> {
    validate_clock(clock, 4)?;
    check_cancellation(&self.cancellation)?;
    let hash_width = self.migration_owner.hash_algorithm().hash_length();
    if rollback_evidence.len() != hash_width || rollback_evidence.iter().all(|byte| *byte == 0) {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_rollback_evidence",
        "pre-acceptance rollback requires nonzero database-profile evidence",
      ));
    }
    self.validate_mirrored_evidence()?;
    let database = if self.selected_body.last_error_evidence.iter().all(|byte| *byte == 0) {
      let mut rollback_body = self.selected_body.clone();
      rollback_body.journal_sequence = self.next_journal_sequence()?;
      rollback_body.updated_at_ms = clock.updated_at_ms;
      rollback_body.last_error_evidence = rollback_evidence.clone();
      self.publish_mirrored_control(rollback_body, clock, retirement_owner, fault_injector)?
    } else if self.selected_body.last_error_evidence == rollback_evidence {
      selected_database_receipt(&self.migration_owner, &self.selected_body)?
    } else {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_rollback_evidence_conflict",
        "selected ACUT control records different rollback evidence",
      ));
    };
    let failure_clock = clock_at(clock, 1)?;
    self.migration_owner.fail_cutover_progress(
      MigrationCutoverFailureRequestV1 {
        phase: self.selected_body.phase,
        last_error_evidence: rollback_evidence.clone(),
        updated_at_ms: failure_clock.updated_at_ms,
        publication_timestamp_ms: failure_clock.publication_timestamp_ms,
        monotonic_now_ms: failure_clock.monotonic_now_ms,
      },
      retirement_owner,
    )?;

    let mut namespace = classify_namespace(&self.paths, &self.evidence)?;
    inject_fault(fault_injector, SideBySideCutoverBoundaryV1::BeforeRollbackDestinationRestore)?;
    match namespace {
      CutoverNamespaceStateV1::DestinationInstalled => durable_install_new_native(&self.paths.service_path, &self.paths.destination_path)?,
      CutoverNamespaceStateV1::DestinationLinked => remove_verified_duplicate(
        &self.paths.service_path,
        &self.paths.destination_path,
        &self.evidence.destination_file,
        "remove linked destination service path",
      )?,
      _ => {}
    }
    inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterRollbackDestinationRestore)?;
    namespace = classify_namespace(&self.paths, &self.evidence)?;

    inject_fault(fault_injector, SideBySideCutoverBoundaryV1::BeforeRollbackSourceRestore)?;
    match namespace {
      CutoverNamespaceStateV1::SourceBackedUp => durable_install_new_native(&self.paths.backup_path, &self.paths.service_path)?,
      CutoverNamespaceStateV1::SourceLinked => remove_verified_duplicate(
        &self.paths.backup_path,
        &self.paths.service_path,
        &self.evidence.source_file,
        "remove linked source backup path",
      )?,
      CutoverNamespaceStateV1::Pristine => {}
      _ => {
        return Err(SideBySideCutoverRehearsalErrorV1::invalid(
          "cutover_rollback_namespace",
          "destination restoration did not select a recoverable v3 namespace state",
        ));
      }
    }
    inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterRollbackSourceRestore)?;
    if classify_namespace(&self.paths, &self.evidence)? != CutoverNamespaceStateV1::Pristine {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_rollback_namespace",
        "pre-acceptance rollback did not restore the exact v3 service namespace",
      ));
    }
    validate_source_file(&self.paths.service_path, &self.evidence.source_file, self.evidence.source_complete_file_checksum)?;
    validate_reopened_destination(&self.migration_owner, &self.paths.destination_path, &self.selected_body)?;
    validate_selected_stable_file_identity(
      self.migration_owner.hash_algorithm(),
      &self.paths.service_path,
      &self.evidence.source_file,
      &self.selected_body.source_stable_file_identity_hash,
    )?;
    validate_selected_stable_file_identity(
      self.migration_owner.hash_algorithm(),
      &self.paths.destination_path,
      &self.evidence.destination_file,
      &self.selected_body.destination_stable_file_identity_hash,
    )?;
    Ok(SideBySideCutoverRollbackReceiptV1 {
      database_control_sequence: database.control_sequence,
      journal_sequence: database.journal_sequence,
      phase: database.phase,
      rollback_evidence,
      destination_path: self.paths.destination_path.clone(),
      service_path: self.paths.service_path.clone(),
    })
  }

  pub fn selected_body(&self) -> &SideBySideCutoverBodyV1 {
    &self.selected_body
  }

  pub fn paths(&self) -> &SideBySideCutoverPathsV1 {
    &self.paths
  }

  fn next_journal_sequence(&self) -> Result<u64, SideBySideCutoverRehearsalErrorV1> {
    self
      .journal
      .sequence()
      .checked_add(1)
      .ok_or_else(|| SideBySideCutoverRehearsalErrorV1::invalid("cutover_journal_sequence_exhausted", "journal sequence is exhausted"))
  }

  fn publish_mirrored_control(
    &mut self,
    body: SideBySideCutoverBodyV1,
    clock: SideBySideCutoverClockV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
    fault_injector: &mut dyn SideBySideCutoverFaultInjectorV1,
  ) -> Result<super::migration_owner::MigrationCutoverControlReceiptV1, SideBySideCutoverRehearsalErrorV1> {
    let current = self
      .migration_owner
      .publisher()
      .load_mutable_system_control(
        SystemControlKindV1::SideBySideCutover,
        &self.migration_owner.database_id(),
        &self.migration_owner.migration_id(),
      )?
      .ok_or_else(|| {
        SideBySideCutoverRehearsalErrorV1::invalid("cutover_database_control_missing", "prepared database ACUT control is missing")
      })?;
    let next_control_sequence = current.control_sequence.checked_add(1).ok_or_else(|| {
      SideBySideCutoverRehearsalErrorV1::invalid("cutover_database_sequence_exhausted", "database ACUT sequence is exhausted")
    })?;
    let encoded = encode_side_by_side_cutover_control_v1(next_control_sequence, &body, self.migration_owner.hash_algorithm())
      .map_err(MigrationStateOwnerErrorV1::from)?;
    let mut journal_faults = JournalFaultAdapterV1 { phase: body.phase, owner: fault_injector };
    let journal_receipt = self.journal.publish_with_fault_injector(&encoded, &mut journal_faults)?;
    if journal_receipt.sequence() != body.journal_sequence {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_journal_sequence_binding",
        "external selected slot does not match the typed ACUT journal sequence",
      ));
    }
    inject_fault(fault_injector, SideBySideCutoverBoundaryV1::BeforeDatabaseControl { phase: body.phase })?;
    let database =
      self.migration_owner.publish_cutover_control(&body, clock.publication_timestamp_ms, clock.monotonic_now_ms, retirement_owner)?;
    inject_fault(fault_injector, SideBySideCutoverBoundaryV1::AfterDatabaseControl { phase: body.phase })?;
    self.selected_body = body;
    self.validate_mirrored_evidence()?;
    Ok(database)
  }

  fn validate_mirrored_evidence(&self) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
    let selected = self
      .migration_owner
      .publisher()
      .load_mutable_system_control(
        SystemControlKindV1::SideBySideCutover,
        &self.migration_owner.database_id(),
        &self.migration_owner.migration_id(),
      )?
      .ok_or_else(|| SideBySideCutoverRehearsalErrorV1::invalid("cutover_database_control_missing", "database ACUT control is absent"))?;
    let decoded = decode_side_by_side_cutover_control_v1(&selected.bytes, self.migration_owner.hash_algorithm())
      .map_err(MigrationStateOwnerErrorV1::from)?;
    let envelope =
      decode_system_control(&selected.bytes, self.migration_owner.hash_algorithm()).map_err(MigrationStateOwnerErrorV1::from)?;
    if decoded.body != self.selected_body
      || self.journal.selected_body()? != envelope.body
      || self.journal.sequence() != decoded.body.journal_sequence
    {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid(
        "cutover_evidence_disagreement",
        "database and external ACUT evidence disagree; neither authority may win by precedence",
      ));
    }
    Ok(())
  }
}

struct NoSideBySideCutoverFaultV1;

impl SideBySideCutoverFaultInjectorV1 for NoSideBySideCutoverFaultV1 {
  fn inject(&mut self, _boundary: SideBySideCutoverBoundaryV1) -> bool {
    false
  }
}

struct JournalFaultAdapterV1<'a> {
  phase: MigrationPhaseV1,
  owner: &'a mut dyn SideBySideCutoverFaultInjectorV1,
}

impl CutoverJournalFaultInjectorV1 for JournalFaultAdapterV1<'_> {
  fn inject(&mut self, boundary: CutoverJournalPublicationBoundaryV1) -> bool {
    self.owner.inject(SideBySideCutoverBoundaryV1::Journal { phase: self.phase, boundary })
  }
}

fn inject_fault(
  fault_injector: &mut dyn SideBySideCutoverFaultInjectorV1,
  boundary: SideBySideCutoverBoundaryV1,
) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  if fault_injector.inject(boundary) {
    return Err(SideBySideCutoverRehearsalErrorV1::InjectedFault { boundary });
  }
  Ok(())
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  if cancellation.is_cancelled() {
    return Err(SideBySideCutoverRehearsalErrorV1::Canceled);
  }
  Ok(())
}

fn validate_clock(clock: SideBySideCutoverClockV1, additional_steps: u64) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  if clock.updated_at_ms < 0 || clock.publication_timestamp_ms == 0 || clock.monotonic_now_ms == 0 {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_clock",
      "cutover semantic, publication, and monotonic clocks must be nonnegative/nonzero",
    ));
  }
  let additional_steps_i64 = i64::try_from(additional_steps).map_err(|error| {
    SideBySideCutoverRehearsalErrorV1::invalid("cutover_clock_overflow", format!("cutover clock step conversion failed: {error}"))
  })?;
  clock
    .updated_at_ms
    .checked_add(additional_steps_i64)
    .ok_or_else(|| SideBySideCutoverRehearsalErrorV1::invalid("cutover_clock_overflow", "cutover semantic clock would overflow"))?;
  clock
    .publication_timestamp_ms
    .checked_add(additional_steps)
    .ok_or_else(|| SideBySideCutoverRehearsalErrorV1::invalid("cutover_clock_overflow", "cutover publication clock would overflow"))?;
  clock
    .monotonic_now_ms
    .checked_add(additional_steps)
    .ok_or_else(|| SideBySideCutoverRehearsalErrorV1::invalid("cutover_clock_overflow", "cutover monotonic clock would overflow"))?;
  Ok(())
}

fn clock_at(clock: SideBySideCutoverClockV1, offset: u64) -> Result<SideBySideCutoverClockV1, SideBySideCutoverRehearsalErrorV1> {
  validate_clock(clock, offset)?;
  Ok(SideBySideCutoverClockV1 {
    updated_at_ms: checked_updated_at(clock.updated_at_ms, offset)?,
    publication_timestamp_ms: clock.publication_timestamp_ms + offset,
    monotonic_now_ms: clock.monotonic_now_ms + offset,
  })
}

fn checked_updated_at(value: i64, offset: u64) -> Result<i64, SideBySideCutoverRehearsalErrorV1> {
  value
    .checked_add(i64::try_from(offset).map_err(|error| {
      SideBySideCutoverRehearsalErrorV1::invalid("cutover_clock_overflow", format!("cutover clock step conversion failed: {error}"))
    })?)
    .ok_or_else(|| SideBySideCutoverRehearsalErrorV1::invalid("cutover_clock_overflow", "cutover semantic clock would overflow"))
}

fn cutover_progress_request(
  phase: MigrationPhaseV1,
  state: MigrationProgressStateV1,
  clock: SideBySideCutoverClockV1,
  offset: u64,
) -> Result<MigrationCutoverProgressRequestV1, SideBySideCutoverRehearsalErrorV1> {
  let clock = clock_at(clock, offset)?;
  Ok(MigrationCutoverProgressRequestV1 {
    phase,
    state,
    updated_at_ms: clock.updated_at_ms,
    publication_timestamp_ms: clock.publication_timestamp_ms,
    monotonic_now_ms: clock.monotonic_now_ms,
  })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CutoverPathArtifactV1 {
  Absent,
  Source,
  Destination,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CutoverNamespaceStateV1 {
  Pristine,
  SourceLinked,
  SourceBackedUp,
  DestinationLinked,
  DestinationInstalled,
}

impl CutoverNamespaceStateV1 {
  fn source_path(self, paths: &SideBySideCutoverPathsV1) -> &Path {
    match self {
      Self::Pristine | Self::SourceLinked => &paths.service_path,
      Self::SourceBackedUp | Self::DestinationLinked | Self::DestinationInstalled => &paths.backup_path,
    }
  }

  fn destination_path(self, paths: &SideBySideCutoverPathsV1) -> &Path {
    match self {
      Self::Pristine | Self::SourceLinked | Self::SourceBackedUp => &paths.destination_path,
      Self::DestinationLinked | Self::DestinationInstalled => &paths.service_path,
    }
  }
}

fn classify_namespace(
  paths: &SideBySideCutoverPathsV1,
  evidence: &SideBySideCutoverEvidenceV1,
) -> Result<CutoverNamespaceStateV1, SideBySideCutoverRehearsalErrorV1> {
  let service = classify_path(&paths.service_path, evidence)?;
  let destination = classify_path(&paths.destination_path, evidence)?;
  let backup = classify_path(&paths.backup_path, evidence)?;
  match (service, destination, backup) {
    (CutoverPathArtifactV1::Source, CutoverPathArtifactV1::Destination, CutoverPathArtifactV1::Absent) => {
      Ok(CutoverNamespaceStateV1::Pristine)
    }
    (CutoverPathArtifactV1::Source, CutoverPathArtifactV1::Destination, CutoverPathArtifactV1::Source) => {
      Ok(CutoverNamespaceStateV1::SourceLinked)
    }
    (CutoverPathArtifactV1::Absent, CutoverPathArtifactV1::Destination, CutoverPathArtifactV1::Source) => {
      Ok(CutoverNamespaceStateV1::SourceBackedUp)
    }
    (CutoverPathArtifactV1::Destination, CutoverPathArtifactV1::Destination, CutoverPathArtifactV1::Source) => {
      Ok(CutoverNamespaceStateV1::DestinationLinked)
    }
    (CutoverPathArtifactV1::Destination, CutoverPathArtifactV1::Absent, CutoverPathArtifactV1::Source) => {
      Ok(CutoverNamespaceStateV1::DestinationInstalled)
    }
    _ => Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_namespace_ambiguous",
      format!("cutover paths select an unknown or incomplete state: service={service:?}, destination={destination:?}, backup={backup:?}"),
    )),
  }
}

fn classify_path(path: &Path, evidence: &SideBySideCutoverEvidenceV1) -> Result<CutoverPathArtifactV1, SideBySideCutoverRehearsalErrorV1> {
  let metadata = match fs::symlink_metadata(path) {
    Ok(metadata) => metadata,
    Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(CutoverPathArtifactV1::Absent),
    Err(source) => return Err(SideBySideCutoverRehearsalErrorV1::Io { operation: "inspect cutover namespace path", source }),
  };
  if !metadata.file_type().is_file() {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_namespace_artifact",
      format!("cutover path is not a no-follow regular file: {}", path.display()),
    ));
  }
  let identity = platform_file_identity(path)?;
  let source = evidence.source_file.platform_file_identity.represents_same_physical_file_as(identity);
  let destination = evidence.destination_file.platform_file_identity.represents_same_physical_file_as(identity);
  match (source, destination) {
    (true, false) => Ok(CutoverPathArtifactV1::Source),
    (false, true) => Ok(CutoverPathArtifactV1::Destination),
    _ => Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_namespace_identity",
      format!("cutover path does not identify exactly one admitted source or destination: {}", path.display()),
    )),
  }
}

fn remove_verified_duplicate(
  remove_path: &Path,
  retained_path: &Path,
  evidence: &CutoverStableFileIdentityEvidenceV1,
  operation: &'static str,
) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  let remove_identity = platform_file_identity(remove_path)?;
  let retained_identity = platform_file_identity(retained_path)?;
  if !evidence.platform_file_identity.represents_same_physical_file_as(remove_identity)
    || !evidence.platform_file_identity.represents_same_physical_file_as(retained_identity)
    || !remove_identity.represents_same_physical_file_as(retained_identity)
  {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_duplicate_identity",
      "cutover duplicate removal requires two names for the exact admitted physical file",
    ));
  }
  fs::remove_file(remove_path).map_err(|source| SideBySideCutoverRehearsalErrorV1::Io { operation, source })?;
  let parent = remove_path.parent().ok_or_else(|| {
    SideBySideCutoverRehearsalErrorV1::invalid("cutover_duplicate_parent", "cutover duplicate path has no parent directory")
  })?;
  sync_directory_native(parent)?;
  let retained_after = platform_file_identity(retained_path)?;
  if remove_path.exists() || !evidence.platform_file_identity.represents_same_physical_file_as(retained_after) {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_duplicate_readback",
      "cutover duplicate removal did not retain exactly one admitted physical file",
    ));
  }
  Ok(())
}

fn validate_recovery_body(
  owner: &MigrationStateOwnerV1,
  evidence: &SideBySideCutoverEvidenceV1,
  selected: &SideBySideCutoverBodyV1,
  journal_sequence: u64,
) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  if !matches!(selected.phase, MigrationPhaseV1::DestinationVerify | MigrationPhaseV1::Cutover | MigrationPhaseV1::ReadOnlyValidation)
    || selected.journal_sequence != journal_sequence
  {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_recovery_phase",
      "selected ACUT body is outside the pre-acceptance rehearsal or is not bound to its journal sequence",
    ));
  }
  let mut expected = cutover_body(owner, evidence, selected.phase, selected.journal_sequence, selected.updated_at_ms)?;
  expected.last_error_evidence = selected.last_error_evidence.clone();
  if expected != *selected {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_recovery_evidence",
      "selected ACUT body differs from the admitted cutover evidence",
    ));
  }
  Ok(())
}

fn is_exact_recovery_successor(current: &SideBySideCutoverBodyV1, successor: &SideBySideCutoverBodyV1) -> bool {
  if current.journal_sequence.checked_add(1) != Some(successor.journal_sequence) || successor.updated_at_ms < current.updated_at_ms {
    return false;
  }
  let phase_advanced = matches!(
    (current.phase, successor.phase),
    (MigrationPhaseV1::DestinationVerify, MigrationPhaseV1::Cutover) | (MigrationPhaseV1::Cutover, MigrationPhaseV1::ReadOnlyValidation)
  ) && current.last_error_evidence.iter().all(|byte| *byte == 0)
    && successor.last_error_evidence.iter().all(|byte| *byte == 0);
  let rollback_started = current.phase == successor.phase
    && current.last_error_evidence.iter().all(|byte| *byte == 0)
    && successor.last_error_evidence.iter().any(|byte| *byte != 0);
  if !phase_advanced && !rollback_started {
    return false;
  }
  let mut expected = current.clone();
  expected.phase = successor.phase;
  expected.journal_sequence = successor.journal_sequence;
  expected.updated_at_ms = successor.updated_at_ms;
  expected.last_error_evidence = successor.last_error_evidence.clone();
  expected == *successor
}

fn require_selected_phase(selected: &SideBySideCutoverBodyV1, expected: MigrationPhaseV1) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  if selected.phase != expected {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_selected_phase",
      format!("selected ACUT phase {:?} does not match required phase {expected:?}", selected.phase),
    ));
  }
  Ok(())
}

fn selected_database_receipt(
  owner: &MigrationStateOwnerV1,
  expected_body: &SideBySideCutoverBodyV1,
) -> Result<super::migration_owner::MigrationCutoverControlReceiptV1, SideBySideCutoverRehearsalErrorV1> {
  let selected = owner
    .publisher()
    .load_mutable_system_control(SystemControlKindV1::SideBySideCutover, &owner.database_id(), &owner.migration_id())?
    .ok_or_else(|| SideBySideCutoverRehearsalErrorV1::invalid("cutover_database_control_missing", "database ACUT control is absent"))?;
  let control =
    decode_side_by_side_cutover_control_v1(&selected.bytes, owner.hash_algorithm()).map_err(MigrationStateOwnerErrorV1::from)?;
  if control.body != *expected_body {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_evidence_disagreement",
      "database ACUT body differs from the selected external journal body",
    ));
  }
  Ok(super::migration_owner::MigrationCutoverControlReceiptV1 {
    control_sequence: control.sequence,
    journal_sequence: control.body.journal_sequence,
    phase: control.body.phase,
    idempotent: true,
  })
}

fn validate_initial_namespace(paths: &SideBySideCutoverPathsV1) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  let service_is_regular = fs::symlink_metadata(&paths.service_path).is_ok_and(|metadata| metadata.file_type().is_file());
  let destination_is_regular = fs::symlink_metadata(&paths.destination_path).is_ok_and(|metadata| metadata.file_type().is_file());
  if !service_is_regular || !destination_is_regular {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_initial_namespace",
      "initial cutover requires no-follow regular source and destination files",
    ));
  }
  match fs::symlink_metadata(&paths.backup_path) {
    Ok(_) => {
      return Err(SideBySideCutoverRehearsalErrorV1::invalid("cutover_backup_collision", "derived v3 backup path already exists"));
    }
    Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
    Err(source) => {
      return Err(SideBySideCutoverRehearsalErrorV1::Io { operation: "inspect derived v3 backup path", source });
    }
  }
  Ok(())
}

fn validate_evidence(
  owner: &MigrationStateOwnerV1,
  paths: &SideBySideCutoverPathsV1,
  evidence: &SideBySideCutoverEvidenceV1,
) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  let permit = owner.permit();
  let hash_width = owner.hash_algorithm().hash_length();
  if evidence.source_path_digest != permit.source_path_digest()
    || evidence.destination_path_digest != permit.destination_path_digest()
    || evidence.source_file.role != CutoverArtifactRoleV1::Source
    || evidence.destination_file.role != CutoverArtifactRoleV1::Destination
    || evidence.source_file.database_id != owner.database_id()
    || evidence.destination_file.database_id != owner.database_id()
    || evidence.source_file.physical_instance_id != owner.source_physical_instance_id()
    || evidence.destination_file.physical_instance_id != owner.destination_physical_instance_id()
    || evidence.source_complete_file_checksum.iter().all(|byte| *byte == 0)
    || evidence.destination_full_verification_evidence.len() != hash_width
    || evidence.destination_full_verification_evidence.iter().all(|byte| *byte == 0)
  {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_evidence_binding",
      "cutover evidence differs from the permit, physical incarnations, roles, or full-verification proof",
    ));
  }
  let parent_identity = platform_file_identity(paths.parent()?)?;
  if !permit.destination_parent_identity().represents_same_physical_file_as(parent_identity)
    || evidence.source_file.platform_file_identity.volume_identity != evidence.destination_file.platform_file_identity.volume_identity
    || evidence.source_file.platform_file_identity.volume_identity != parent_identity.volume_identity
    || evidence.source_file.platform_file_identity.represents_same_physical_file_as(evidence.destination_file.platform_file_identity)
  {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_same_filesystem",
      "source, destination, admitted parent, and cutover paths must remain on the selected filesystem",
    ));
  }
  Ok(())
}

fn validate_destination_verification_binding(
  progress: &MigrationProgressBodyV1,
  evidence: &SideBySideCutoverEvidenceV1,
) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  let required_flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED
    | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD
    | MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED;
  if progress.flags & required_flags != required_flags
    || progress.destination_header_sequence == 0
    || progress.destination_header_sequence > evidence.destination_file.selected_header_sequence
    || progress.legacy_root_map_control_payload_hash != evidence.destination_full_verification_evidence
  {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_destination_verification_binding",
      "cutover evidence is not bound to the selected fully verified destination progress and root map",
    ));
  }
  Ok(())
}

fn validate_selected_stable_file_identity(
  algorithm: crate::engine::HashAlgorithm,
  path: &Path,
  evidence: &CutoverStableFileIdentityEvidenceV1,
  expected_hash: &[u8],
) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  let current_identity = platform_file_identity(path)?;
  if !evidence.platform_file_identity.represents_same_physical_file_as(current_identity) {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_stable_file_identity",
      "cutover path no longer resolves to the admitted physical file",
    ));
  }
  let mut current = *evidence;
  current.platform_file_identity = current_identity;
  let current_hash = cutover_stable_file_identity_hash_v1(algorithm, &current).map_err(MigrationStateOwnerErrorV1::from)?;
  if current_hash != expected_hash {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_stable_file_identity",
      "recomputed complete platform descriptor differs from selected ACUT stable-file evidence",
    ));
  }
  Ok(())
}

fn validate_source_file(
  path: &Path,
  evidence: &CutoverStableFileIdentityEvidenceV1,
  expected_complete_checksum: [u8; 32],
) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  let mut file = open_regular_file_no_follow(path).map_err(|source| SideBySideCutoverRehearsalErrorV1::Io {
    operation: "open frozen v3 source",
    source: std::io::Error::other(source.to_string()),
  })?;
  let identity = platform_file_identity(path)?;
  if !evidence.platform_file_identity.represents_same_physical_file_as(identity) {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_source_file_identity",
      "v3 source path resolves to another physical file",
    ));
  }
  let length = file.metadata().map_err(|source| SideBySideCutoverRehearsalErrorV1::Io { operation: "read source metadata", source })?.len();
  let (header, selected_slot) = read_active_header(&mut file).map_err(|source| {
    SideBySideCutoverRehearsalErrorV1::invalid("cutover_source_header", format!("v3 source header is invalid: {source}"))
  })?;
  file
    .seek(SeekFrom::Start((selected_slot * FILE_HEADER_SIZE) as u64))
    .map_err(|source| SideBySideCutoverRehearsalErrorV1::Io { operation: "seek selected source header", source })?;
  let mut selected_header = [0u8; FILE_HEADER_SIZE];
  file
    .read_exact(&mut selected_header)
    .map_err(|source| SideBySideCutoverRehearsalErrorV1::Io { operation: "read selected source header", source })?;
  let selected_header_blake3 = *blake3::hash(&selected_header).as_bytes();
  let complete_checksum = complete_file_blake3(&mut file)?;
  if evidence.format != 3
    || header.hash_algo.hash_length() == 0
    || header.sequence != evidence.selected_header_sequence
    || selected_header_blake3 != evidence.selected_header_blake3
    || length != evidence.file_size
    || complete_checksum != expected_complete_checksum
  {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_source_file_evidence",
      "v3 source bytes, selected header, size, or checksum differ from frozen evidence",
    ));
  }
  Ok(())
}

fn complete_file_blake3(file: &mut File) -> Result<[u8; 32], SideBySideCutoverRehearsalErrorV1> {
  file.seek(SeekFrom::Start(0)).map_err(|source| SideBySideCutoverRehearsalErrorV1::Io { operation: "seek source checksum", source })?;
  let mut hasher = blake3::Hasher::new();
  let mut buffer = [0u8; 64 * 1024];
  loop {
    let count =
      file.read(&mut buffer).map_err(|source| SideBySideCutoverRehearsalErrorV1::Io { operation: "read source checksum", source })?;
    if count == 0 {
      break;
    }
    hasher.update(&buffer[..count]);
  }
  Ok(*hasher.finalize().as_bytes())
}

fn validate_destination_file(
  owner: &MigrationStateOwnerV1,
  path: &Path,
  evidence: &CutoverStableFileIdentityEvidenceV1,
) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  validate_destination_physical_file(owner, path, evidence)?;
  let observation = owner.destination_observation()?;
  let selected_start = observation.selected.selected_slot * DATABASE_HEADER_V4_SLOT_LENGTH;
  let selected_header_blake3 =
    *blake3::hash(&observation.region[selected_start..selected_start + DATABASE_HEADER_V4_SLOT_LENGTH]).as_bytes();
  let length =
    path.metadata().map_err(|source| SideBySideCutoverRehearsalErrorV1::Io { operation: "read destination metadata", source })?.len();
  if evidence.format != 4
    || observation.selected.redundancy_degraded
    || observation.selected.header.slot_sequence != evidence.selected_header_sequence
    || selected_header_blake3 != evidence.selected_header_blake3
    || length != evidence.file_size
  {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_destination_file_evidence",
      "v4 destination differs from the fully verified pre-ACUT header and size evidence",
    ));
  }
  Ok(())
}

fn validate_destination_physical_file(
  owner: &MigrationStateOwnerV1,
  path: &Path,
  evidence: &CutoverStableFileIdentityEvidenceV1,
) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  let identity = platform_file_identity(path)?;
  if !evidence.platform_file_identity.represents_same_physical_file_as(identity) {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_destination_file_identity",
      "v4 destination path resolves to another physical file",
    ));
  }
  let observation = owner.destination_observation()?;
  if observation.selected.redundancy_degraded
    || observation.selected.header.database_id != owner.database_id()
    || observation.selected.header.physical_instance_id != owner.destination_physical_instance_id()
    || observation.selected.header.hash_algorithm != owner.hash_algorithm()
  {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_destination_authority",
      "selected v4 destination header differs from the cutover owner",
    ));
  }
  Ok(())
}

fn validate_reopened_destination(
  owner: &MigrationStateOwnerV1,
  path: &Path,
  expected_body: &SideBySideCutoverBodyV1,
) -> Result<(), SideBySideCutoverRehearsalErrorV1> {
  let reopened = V4FirstAuthorityPublisher::open(path)?;
  let observation = reopened.observe()?;
  if observation.selected.redundancy_degraded
    || observation.selected.header.database_id != owner.database_id()
    || observation.selected.header.physical_instance_id != owner.destination_physical_instance_id()
    || observation.selected.header.hash_algorithm != owner.hash_algorithm()
  {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_reopen_authority",
      "reopened service path is not the exact non-degraded v4 destination",
    ));
  }
  let selected = reopened
    .load_mutable_system_control(SystemControlKindV1::SideBySideCutover, &owner.database_id(), &owner.migration_id())?
    .ok_or_else(|| SideBySideCutoverRehearsalErrorV1::invalid("cutover_reopen_control", "reopened destination has no ACUT control"))?;
  let control =
    decode_side_by_side_cutover_control_v1(&selected.bytes, owner.hash_algorithm()).map_err(MigrationStateOwnerErrorV1::from)?;
  if control.body != *expected_body {
    return Err(SideBySideCutoverRehearsalErrorV1::invalid(
      "cutover_reopen_control",
      "reopened destination ACUT body differs from selected cutover evidence",
    ));
  }
  Ok(())
}

fn cutover_body(
  owner: &MigrationStateOwnerV1,
  evidence: &SideBySideCutoverEvidenceV1,
  phase: MigrationPhaseV1,
  journal_sequence: u64,
  updated_at_ms: i64,
) -> Result<SideBySideCutoverBodyV1, SideBySideCutoverRehearsalErrorV1> {
  let algorithm = owner.hash_algorithm();
  let source_path_identity_hash = cutover_path_identity_hash_v1(algorithm, CutoverArtifactRoleV1::Source, evidence.source_path_digest)
    .map_err(MigrationStateOwnerErrorV1::from)?;
  let destination_path_identity_hash =
    cutover_path_identity_hash_v1(algorithm, CutoverArtifactRoleV1::Destination, evidence.destination_path_digest)
      .map_err(MigrationStateOwnerErrorV1::from)?;
  let source_stable_file_identity_hash =
    cutover_stable_file_identity_hash_v1(algorithm, &evidence.source_file).map_err(MigrationStateOwnerErrorV1::from)?;
  let destination_stable_file_identity_hash =
    cutover_stable_file_identity_hash_v1(algorithm, &evidence.destination_file).map_err(MigrationStateOwnerErrorV1::from)?;
  Ok(SideBySideCutoverBodyV1 {
    database_id: owner.database_id(),
    migration_id: owner.migration_id(),
    source_physical_instance_id: owner.source_physical_instance_id(),
    destination_physical_instance_id: owner.destination_physical_instance_id(),
    holder_boot_id: owner.holder_boot_id(),
    fencing_token: owner.fencing_token(),
    phase,
    journal_sequence,
    destination_header_sequence: evidence.destination_file.selected_header_sequence,
    source_file_size: evidence.source_file.file_size,
    destination_file_size: evidence.destination_file.file_size,
    updated_at_ms,
    source_path_identity_hash,
    destination_path_identity_hash,
    source_stable_file_identity_hash,
    destination_stable_file_identity_hash,
    last_error_evidence: vec![0; algorithm.hash_length()],
  })
}
