//! Atomic first-authority publication for a disconnected v4 database.

use std::collections::HashSet;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::engine::durability_coordinator::DurabilityCoordinator;
use crate::engine::errors::EngineError;
use crate::engine::file_record::FileRecord;
use crate::engine::kv_store::{KV_TYPE_CHUNK, KV_TYPE_DIRECTORY, KV_TYPE_FILE_RECORD, KVEntry};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::{
  NativeDurabilityError, NativeDurabilityOperation, read_file_at_native, verify_file_bytes_native, write_file_at_native,
};
use crate::engine::{CompressionAlgorithm, DiskKVStore, HashAlgorithm};
use tokio_util::sync::CancellationToken;

use super::control_store::SYSTEM_CONTROL_CONTENT_TYPE;
use super::contract_generated::kv_tag;
use super::database_header::DatabaseHeaderV4;
use super::entity::{EntryTypeV4, WHOLE_ENTITY_V1_FLAG_SYSTEM, WholeEntityWriteV1, decode_whole_entity, encode_whole_entity};
use super::header_publication::{
  DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4, DatabaseHeaderPublisherV4, HeaderPublicationDependencyV4,
  observe_database_header_v4,
};
use super::hash::digest_parts;
use super::gc::{
  EncodedGcActiveControlV1, EncodedImmutableGcArtifactV1, GcActiveControlV1, GcArtifactKindV1, decode_gc_active_control,
  decode_gc_artifact_envelope, gc_active_control_key, immutable_gc_artifact_key, select_gc_active_control,
};
use super::gc_audit::{
  AuditArtifactV1, CorruptGcEvidenceDurabilityReceiptV1, CorruptGcEvidenceDurableSinkV1, CorruptGcEvidenceSinkErrorV1,
  decode_audit_artifact,
};
use super::gc_retirement::{
  PreparedRetirementJournalSegmentV1, RetirementJournalDurabilityReceiptV1, RetirementJournalDurableSinkV1, RetirementJournalOwnerErrorV1,
  RetirementJournalOwnerV1, RetirementJournalReplacementAdmissionErrorV1, RetirementJournalReplacementBatchV1,
  RetirementJournalReplacementCoordinatorV1, RetirementJournalReplacementV1, RetirementJournalSinkErrorV1,
};
use super::gc_state::{RetirementReasonV1, decode_retirement_journal_segment_v1, retirement_journal_records_v1};
use super::gc_mark::{
  GcMarkArtifactV1, MARK_CHECKPOINT_VALUE_MAX, MarkResumeContextV1, decode_gc_mark_artifact, validate_mark_checkpoint_resume_context,
};
use super::gc_mark_convergence::{
  MarkMutationJournalDurabilityReceiptV1, MarkMutationJournalDurableSinkV1, MarkMutationJournalSinkErrorV1,
  PreparedMarkMutationJournalSegmentV1,
};
use super::gc_mark_workspace::{DurableMarkWorkspaceClosureV1, MarkWorkspaceErrorV1, MarkWorkspaceReopenOptionsV1, ReopenedMarkWorkspaceV1};
use super::namespace::{
  EncodedNamespaceRootV1, EncodedSemanticObjectV1, NamespaceRootWriteV1, SemanticObjectKind, decode_namespace_tree_root_v0,
  decode_semantic_object, encode_namespace_root,
};
use super::reader::FormatError;
use super::root_authority::{
  RootAdmissionCommitV1, RootAuthorityKindV1, RootPublicationPrepareV1, decode_root_admission_commit, encode_root_admission_commit_control,
  encode_root_publication_prepare_control,
};
use super::semantic_store::{SEMANTIC_OBJECT_CONTENT_TYPE, semantic_object_path};
use super::system_control::{SystemControlKindV1, SystemControlSlotV1, system_control_path};

const FIRST_AUTHORITY_ENTITY_COUNT: usize = 8;
const FIRST_AUTHORITY_NAMESPACE_TREE_CAP: usize = 48 * 1024 * 1024;
const FIRST_AUTHORITY_CONTROL_BODY_CAP: usize = 16 * 1024;
const FIRST_AUTHORITY_CONTROL_ENTITY_CAP: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedNamespaceTreeV0 {
  pub root_hash: Vec<u8>,
  pub stored_value: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstAuthorityPublicationRequestV1 {
  pub database_id: [u8; 16],
  pub transaction_id: [u8; 16],
  pub created_at_ms: u64,
  pub namespace_tree: PreparedNamespaceTreeV0,
  pub semantic_state: EncodedSemanticObjectV1,
  pub required_capabilities: [u8; 32],
  pub typed_closure_digest: Vec<u8>,
  pub authority_identity: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstAuthorityPublicationReceiptV1 {
  pub namespace_root: EncodedNamespaceRootV1,
  pub prepare_control: Vec<u8>,
  pub admission_control: Vec<u8>,
  pub publication_sequence: u64,
  pub observation: DatabaseHeaderObservationV4,
  pub idempotent: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct MarkRunCheckpointPublicationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub checkpoint: &'a EncodedImmutableGcArtifactV1,
  pub control: &'a EncodedGcActiveControlV1,
  pub workspace: &'a DurableMarkWorkspaceClosureV1,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Debug)]
pub enum MarkRunCheckpointLineageStateV1 {
  NotRequired,
  HardPublished { hard_publication_sequence: u64 },
  BufferedAfterFlushFailure { code: &'static str, message: String },
  MissingAfterCommit { code: &'static str, message: String },
}

impl MarkRunCheckpointLineageStateV1 {
  pub const fn code(&self) -> &'static str {
    match self {
      Self::NotRequired => "not_required",
      Self::HardPublished { .. } => "hard_published",
      Self::BufferedAfterFlushFailure { .. } => "buffered_after_flush_failure",
      Self::MissingAfterCommit { .. } => "missing_after_commit",
    }
  }
}

#[must_use = "checkpoint publication may retain buffered retirement lineage that requires a later flush"]
#[derive(Debug)]
pub struct MarkRunCheckpointPublicationReceiptV1 {
  pub checkpoint_key: Vec<u8>,
  pub checkpoint_write_sequence: u64,
  pub control_key: Vec<u8>,
  pub control_write_sequence: u64,
  pub control_slot: u8,
  pub replaced_control: bool,
  pub lineage_state: MarkRunCheckpointLineageStateV1,
  pub observation: DatabaseHeaderObservationV4,
  pub idempotent: bool,
}

#[derive(Debug)]
pub enum MarkRunCheckpointPublicationErrorV1 {
  Invalid { code: &'static str, message: String },
  Committed { code: &'static str, message: String, receipt: Box<MarkRunCheckpointPublicationReceiptV1> },
  Format(FormatError),
  Authority(FirstAuthorityPublicationErrorV1),
  RetirementAdmission(RetirementJournalReplacementAdmissionErrorV1),
  RetirementOwner(RetirementJournalOwnerErrorV1),
}

impl MarkRunCheckpointPublicationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Committed { code, .. } => code,
      Self::Format(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::RetirementAdmission(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  fn committed(code: &'static str, message: impl Into<String>, receipt: MarkRunCheckpointPublicationReceiptV1) -> Self {
    Self::Committed { code, message: message.into(), receipt: Box::new(receipt) }
  }

  pub fn committed_receipt(&self) -> Option<&MarkRunCheckpointPublicationReceiptV1> {
    match self {
      Self::Committed { receipt, .. } => Some(receipt),
      Self::Invalid { .. } | Self::Format(_) | Self::Authority(_) | Self::RetirementAdmission(_) | Self::RetirementOwner(_) => None,
    }
  }
}

impl Display for MarkRunCheckpointPublicationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Committed { code, message, receipt } => write!(
        formatter,
        "{code}: mark checkpoint control {} committed, but post-commit handling failed: {message}",
        receipt.control_write_sequence
      ),
      Self::Format(source) => write!(formatter, "mark checkpoint format error: {source}"),
      Self::Authority(source) => write!(formatter, "mark checkpoint authority error: {source}"),
      Self::RetirementAdmission(source) => write!(formatter, "mark checkpoint retirement admission error: {source}"),
      Self::RetirementOwner(source) => write!(formatter, "mark checkpoint retirement owner error: {source}"),
    }
  }
}

impl Error for MarkRunCheckpointPublicationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Authority(source) => Some(source),
      Self::Format(source) => Some(source),
      Self::RetirementAdmission(source) => Some(source),
      Self::RetirementOwner(source) => Some(source),
      Self::Invalid { .. } | Self::Committed { .. } => None,
    }
  }
}

impl From<FirstAuthorityPublicationErrorV1> for MarkRunCheckpointPublicationErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<EngineError> for MarkRunCheckpointPublicationErrorV1 {
  fn from(source: EngineError) -> Self {
    Self::Authority(FirstAuthorityPublicationErrorV1::Engine(source))
  }
}

impl From<DatabaseHeaderPublicationErrorV4> for MarkRunCheckpointPublicationErrorV1 {
  fn from(source: DatabaseHeaderPublicationErrorV4) -> Self {
    Self::Authority(FirstAuthorityPublicationErrorV1::Header(source))
  }
}

impl From<FormatError> for MarkRunCheckpointPublicationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<RetirementJournalReplacementAdmissionErrorV1> for MarkRunCheckpointPublicationErrorV1 {
  fn from(source: RetirementJournalReplacementAdmissionErrorV1) -> Self {
    Self::RetirementAdmission(source)
  }
}

impl From<RetirementJournalOwnerErrorV1> for MarkRunCheckpointPublicationErrorV1 {
  fn from(source: RetirementJournalOwnerErrorV1) -> Self {
    Self::RetirementOwner(source)
  }
}

#[derive(Clone)]
pub struct MarkRunCheckpointSelectionRequestV1<'a> {
  pub resume_contexts: &'a [MarkResumeContextV1<'a>],
  pub workspace_options: MarkWorkspaceReopenOptionsV1,
  pub cancellation: &'a CancellationToken,
  pub memory: &'a MemoryCoordinator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkRunCheckpointSlotDiagnosticV1 {
  pub slot: u8,
  pub code: &'static str,
  pub message: String,
}

#[derive(Debug)]
pub enum MarkRunCheckpointSelectionV1 {
  Absent,
  Selected(Box<SelectedMarkRunCheckpointV1>),
}

pub struct SelectedMarkRunCheckpointV1 {
  algorithm: HashAlgorithm,
  write_sequence_high_water: u64,
  checkpoint_entity_bytes: Vec<u8>,
  _checkpoint_memory: MemoryReservation,
  workspace: ReopenedMarkWorkspaceV1,
  pub control_slot: u8,
  pub control_sequence: u64,
  pub control_write_sequence: u64,
  pub checkpoint_key: Vec<u8>,
  pub checkpoint_write_sequence: u64,
  pub degraded_slots: Vec<MarkRunCheckpointSlotDiagnosticV1>,
}

impl fmt::Debug for SelectedMarkRunCheckpointV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("SelectedMarkRunCheckpointV1")
      .field("control_slot", &self.control_slot)
      .field("control_sequence", &self.control_sequence)
      .field("control_write_sequence", &self.control_write_sequence)
      .field("checkpoint_key", &hex::encode(&self.checkpoint_key))
      .field("checkpoint_write_sequence", &self.checkpoint_write_sequence)
      .field("degraded_slots", &self.degraded_slots)
      .finish()
  }
}

impl SelectedMarkRunCheckpointV1 {
  pub fn checkpoint(&self) -> Result<Box<super::gc_mark::MarkRunCheckpointV1<'_>>, FormatError> {
    let entity = decode_whole_entity(&self.checkpoint_entity_bytes, self.algorithm, self.write_sequence_high_water)?;
    let GcMarkArtifactV1::Checkpoint(checkpoint) = decode_gc_mark_artifact(entity.stored_value, self.algorithm)? else {
      return Err(super::reader::FormatError::new(
        super::reader::MalformedInputClass::UnknownTypeKindOrEnum,
        "mark_checkpoint_selected_kind",
        "selected mark checkpoint entity contains another mark artifact kind",
      ));
    };
    Ok(checkpoint)
  }

  pub fn workspace(&self) -> &ReopenedMarkWorkspaceV1 {
    &self.workspace
  }
}

#[derive(Debug)]
pub enum MarkRunCheckpointSelectionErrorV1 {
  Invalid { code: &'static str, message: String },
  Authority(FirstAuthorityPublicationErrorV1),
  Workspace(MarkWorkspaceErrorV1),
  Memory(MemoryCoordinatorError),
}

impl MarkRunCheckpointSelectionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Authority(source) => source.code(),
      Self::Workspace(source) => source.code(),
      Self::Memory(_) => "mark_checkpoint_selection_memory",
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for MarkRunCheckpointSelectionErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Authority(source) => write!(formatter, "mark checkpoint selection authority error: {source}"),
      Self::Workspace(source) => write!(formatter, "mark checkpoint selection workspace error: {source}"),
      Self::Memory(source) => write!(formatter, "mark checkpoint selection memory error: {source}"),
    }
  }
}

impl Error for MarkRunCheckpointSelectionErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Authority(source) => Some(source),
      Self::Workspace(source) => Some(source),
      Self::Memory(source) => Some(source),
      Self::Invalid { .. } => None,
    }
  }
}

impl From<FirstAuthorityPublicationErrorV1> for MarkRunCheckpointSelectionErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<MemoryCoordinatorError> for MarkRunCheckpointSelectionErrorV1 {
  fn from(source: MemoryCoordinatorError) -> Self {
    Self::Memory(source)
  }
}

#[derive(Debug)]
pub enum FirstAuthorityPublicationErrorV1 {
  Invalid { code: &'static str, message: String },
  Committed { code: &'static str, message: String, receipt: Box<FirstAuthorityPublicationReceiptV1> },
  Format(FormatError),
  Engine(EngineError),
  Header(DatabaseHeaderPublicationErrorV4),
  StateLockPoisoned,
}

impl FirstAuthorityPublicationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::Committed { code, .. } => code,
      Self::Format(error) => error.code(),
      Self::Engine(_) => "engine_failure",
      Self::Header(error) => error.code(),
      Self::StateLockPoisoned => "first_authority_lock_poisoned",
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  fn committed(code: &'static str, message: impl Into<String>, receipt: FirstAuthorityPublicationReceiptV1) -> Self {
    Self::Committed { code, message: message.into(), receipt: Box::new(receipt) }
  }

  pub fn committed_receipt(&self) -> Option<&FirstAuthorityPublicationReceiptV1> {
    match self {
      Self::Committed { receipt, .. } => Some(receipt),
      Self::Invalid { .. } | Self::Format(_) | Self::Engine(_) | Self::Header(_) | Self::StateLockPoisoned => None,
    }
  }
}

impl Display for FirstAuthorityPublicationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Committed { code, message, receipt } => write!(
        formatter,
        "{code}: authority publication {} committed, but post-commit handling failed: {message}",
        receipt.publication_sequence
      ),
      Self::Format(error) => write!(formatter, "first-authority format error: {error}"),
      Self::Engine(error) => write!(formatter, "first-authority storage error: {error}"),
      Self::Header(error) => write!(formatter, "first-authority header error: {error}"),
      Self::StateLockPoisoned => formatter.write_str("first-authority state lock is poisoned"),
    }
  }
}

impl Error for FirstAuthorityPublicationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Format(error) => Some(error),
      Self::Engine(error) => Some(error),
      Self::Header(error) => Some(error),
      Self::Invalid { .. } | Self::Committed { .. } | Self::StateLockPoisoned => None,
    }
  }
}

impl From<FormatError> for FirstAuthorityPublicationErrorV1 {
  fn from(error: FormatError) -> Self {
    Self::Format(error)
  }
}

impl From<EngineError> for FirstAuthorityPublicationErrorV1 {
  fn from(error: EngineError) -> Self {
    Self::Engine(error)
  }
}

impl From<DatabaseHeaderPublicationErrorV4> for FirstAuthorityPublicationErrorV1 {
  fn from(error: DatabaseHeaderPublicationErrorV4) -> Self {
    Self::Header(error)
  }
}

#[derive(Clone)]
struct PreparedWholeEntityV1 {
  key: Vec<u8>,
  kv_type: u8,
  bytes: Vec<u8>,
}

struct FirstAuthorityPackageV1 {
  namespace_root: EncodedNamespaceRootV1,
  prepare_control: Vec<u8>,
  admission_control: Vec<u8>,
  entities: Vec<PreparedWholeEntityV1>,
  hot_tail_offset: u64,
  write_sequence_high_water: u64,
}

trait FirstAuthorityDependencyObserverV1 {
  fn before_entity(&mut self, _index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn entity_written(&mut self, _index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn entity_staged(&mut self, _index: usize, _entity: &PreparedWholeEntityV1) -> Result<(), NativeDurabilityError> {
    Ok(())
  }

  fn staged(&mut self, kv: &DiskKVStore, entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError>;

  fn authority_committed(
    &mut self,
    _kv: &DiskKVStore,
    _entities: &[PreparedWholeEntityV1],
  ) -> Result<(), FirstAuthorityPublicationErrorV1> {
    Ok(())
  }
}

struct NoopFirstAuthorityDependencyObserverV1;

impl FirstAuthorityDependencyObserverV1 for NoopFirstAuthorityDependencyObserverV1 {
  fn staged(&mut self, _kv: &DiskKVStore, _entities: &[PreparedWholeEntityV1]) -> Result<(), NativeDurabilityError> {
    Ok(())
  }
}

struct ImmutableGcArtifactPublicationV1<'a> {
  kind: GcArtifactKindV1,
  database_id: &'a [u8; 16],
  artifact_key: &'a [u8],
  value: &'a [u8],
  minimum_timestamp_ms: u64,
  committed_postcondition_code: &'static str,
}

struct StoredGcControlEntityV1 {
  locator: KVEntry,
  entity_bytes: Vec<u8>,
  stored_value: Vec<u8>,
  control_sequence: u64,
  generation: u64,
  target_manifest_hash: Vec<u8>,
  write_sequence: u64,
  integrity_hash: Vec<u8>,
}

struct ChargedSelectionEntityV1 {
  bytes: Vec<u8>,
  _memory: MemoryReservation,
}

struct LoadedMarkRunControlV1 {
  entity: ChargedSelectionEntityV1,
  write_sequence: u64,
}

enum LoadedMarkRunControlSlotV1 {
  Absent,
  Invalid(MarkRunCheckpointSlotDiagnosticV1),
  Valid(LoadedMarkRunControlV1),
}

struct LoadedMarkRunCheckpointV1 {
  entity: ChargedSelectionEntityV1,
  key: Vec<u8>,
  write_sequence: u64,
  resume_context_index: usize,
}

struct ValidatedMarkRunClosureV1 {
  checkpoint_entity: ChargedSelectionEntityV1,
  checkpoint_key: Vec<u8>,
  checkpoint_write_sequence: u64,
  workspace: ReopenedMarkWorkspaceV1,
}

#[derive(Clone, Copy, Debug)]
enum GcControlPublicationFailureV1 {
  MissingAuthority,
  ControlDatabase,
  BaselineNotFlushed,
  ControlKind,
  ControlKey,
  ControlSlot,
  ControlSequence,
  WriteSequenceExhausted,
  FileTruncated,
  ControlWrite,
  ControlReadback,
  WalOverflow,
  EntryCountOverflow,
  WalPrefix,
  DependencySize,
  RetirementActivation,
  PhysicalIncarnation,
  ControlCollision,
  ControlMissing,
  ControlRepresentation,
  ControlAmbiguous,
  CommittedDependencyMissing,
  CommittedVisibilityFailure,
  CommittedPostconditionFailure,
  CommittedReadbackFailure,
}

impl GcControlPublicationFailureV1 {
  const fn code(self) -> &'static str {
    match self {
      Self::MissingAuthority => "gc_control_missing_authority",
      Self::ControlDatabase => "gc_control_database",
      Self::BaselineNotFlushed => "gc_control_baseline_not_flushed",
      Self::ControlKind => "gc_control_kind",
      Self::ControlKey => "gc_control_key",
      Self::ControlSlot => "gc_control_slot",
      Self::ControlSequence => "gc_control_sequence",
      Self::WriteSequenceExhausted => "gc_control_write_sequence_exhausted",
      Self::FileTruncated => "gc_control_file_truncated",
      Self::ControlWrite => "gc_control_write",
      Self::ControlReadback => "gc_control_readback",
      Self::WalOverflow => "gc_control_wal_overflow",
      Self::EntryCountOverflow => "gc_control_entry_count_overflow",
      Self::WalPrefix => "gc_control_wal_prefix",
      Self::DependencySize => "gc_control_dependency_size",
      Self::RetirementActivation => "gc_control_retirement_activation",
      Self::PhysicalIncarnation => "gc_control_physical_incarnation",
      Self::ControlCollision => "gc_control_collision",
      Self::ControlMissing => "gc_control_missing",
      Self::ControlRepresentation => "gc_control_representation",
      Self::ControlAmbiguous => "gc_control_ambiguous",
      Self::CommittedDependencyMissing => "gc_control_committed_dependency_missing",
      Self::CommittedVisibilityFailure => "gc_control_committed_visibility_failure",
      Self::CommittedPostconditionFailure => "gc_control_committed_postcondition_failure",
      Self::CommittedReadbackFailure => "gc_control_committed_readback_failure",
    }
  }

  const fn mark_run_code(self) -> &'static str {
    match self {
      Self::MissingAuthority => "mark_checkpoint_missing_authority",
      Self::ControlDatabase => "mark_checkpoint_control_database",
      Self::BaselineNotFlushed => "mark_checkpoint_baseline_not_flushed",
      Self::ControlKind => "mark_checkpoint_control_kind",
      Self::ControlKey => "mark_checkpoint_control_key",
      Self::ControlSlot => "mark_checkpoint_control_slot",
      Self::ControlSequence => "mark_checkpoint_control_sequence",
      Self::WriteSequenceExhausted => "mark_checkpoint_write_sequence_exhausted",
      Self::FileTruncated => "mark_checkpoint_file_truncated",
      Self::ControlWrite => "mark_checkpoint_control_write",
      Self::ControlReadback => "mark_checkpoint_control_readback",
      Self::WalOverflow => "mark_checkpoint_wal_overflow",
      Self::EntryCountOverflow => "mark_checkpoint_entry_count_overflow",
      Self::WalPrefix => "mark_checkpoint_wal_prefix",
      Self::DependencySize => "mark_checkpoint_dependency_size",
      Self::RetirementActivation => "mark_checkpoint_retirement_activation",
      Self::PhysicalIncarnation => "mark_checkpoint_physical_incarnation",
      Self::ControlCollision => "mark_checkpoint_control_collision",
      Self::ControlMissing => "mark_checkpoint_control_missing",
      Self::ControlRepresentation => "mark_checkpoint_control_representation",
      Self::ControlAmbiguous => "mark_checkpoint_control_ambiguous",
      Self::CommittedDependencyMissing => "mark_checkpoint_control_committed_dependency_missing",
      Self::CommittedVisibilityFailure => "mark_checkpoint_control_committed_visibility_failure",
      Self::CommittedPostconditionFailure => "mark_checkpoint_control_committed_postcondition_failure",
      Self::CommittedReadbackFailure => "mark_checkpoint_control_committed_readback_failure",
    }
  }
}

#[derive(Debug)]
enum GcControlPublicationErrorV1 {
  Invalid { failure: GcControlPublicationFailureV1, message: String },
  Format(FormatError),
  Authority(FirstAuthorityPublicationErrorV1),
  RetirementAdmission(RetirementJournalReplacementAdmissionErrorV1),
  RetirementOwner(RetirementJournalOwnerErrorV1),
}

impl GcControlPublicationErrorV1 {
  fn code(&self) -> &'static str {
    match self {
      Self::Invalid { failure, .. } => failure.code(),
      Self::Format(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::RetirementAdmission(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
    }
  }

  fn invalid(failure: GcControlPublicationFailureV1, message: impl Into<String>) -> Self {
    Self::Invalid { failure, message: message.into() }
  }
}

impl Display for GcControlPublicationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    let code = self.code();
    match self {
      Self::Invalid { message, .. } => write!(formatter, "{code}: {message}"),
      Self::Format(source) => write!(formatter, "{code}: GC control format error: {source}"),
      Self::Authority(source) => write!(formatter, "{code}: GC control authority error: {source}"),
      Self::RetirementAdmission(source) => write!(formatter, "{code}: GC control retirement admission error: {source}"),
      Self::RetirementOwner(source) => write!(formatter, "{code}: GC control retirement owner error: {source}"),
    }
  }
}

impl Error for GcControlPublicationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Format(source) => Some(source),
      Self::Authority(source) => Some(source),
      Self::RetirementAdmission(source) => Some(source),
      Self::RetirementOwner(source) => Some(source),
      Self::Invalid { .. } => None,
    }
  }
}

impl From<FormatError> for GcControlPublicationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for GcControlPublicationErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<EngineError> for GcControlPublicationErrorV1 {
  fn from(source: EngineError) -> Self {
    Self::Authority(FirstAuthorityPublicationErrorV1::Engine(source))
  }
}

impl From<DatabaseHeaderPublicationErrorV4> for GcControlPublicationErrorV1 {
  fn from(source: DatabaseHeaderPublicationErrorV4) -> Self {
    Self::Authority(FirstAuthorityPublicationErrorV1::Header(source))
  }
}

impl From<RetirementJournalReplacementAdmissionErrorV1> for GcControlPublicationErrorV1 {
  fn from(source: RetirementJournalReplacementAdmissionErrorV1) -> Self {
    Self::RetirementAdmission(source)
  }
}

impl From<RetirementJournalOwnerErrorV1> for GcControlPublicationErrorV1 {
  fn from(source: RetirementJournalOwnerErrorV1) -> Self {
    Self::RetirementOwner(source)
  }
}

impl From<GcControlPublicationErrorV1> for MarkRunCheckpointPublicationErrorV1 {
  fn from(source: GcControlPublicationErrorV1) -> Self {
    match source {
      GcControlPublicationErrorV1::Invalid { failure, message } => Self::Invalid { code: failure.mark_run_code(), message },
      GcControlPublicationErrorV1::Format(source) => Self::Format(source),
      GcControlPublicationErrorV1::Authority(source) => Self::Authority(source),
      GcControlPublicationErrorV1::RetirementAdmission(source) => Self::RetirementAdmission(source),
      GcControlPublicationErrorV1::RetirementOwner(source) => Self::RetirementOwner(source),
    }
  }
}

#[derive(Debug)]
struct GcControlPublicationV1 {
  control_write_sequence: u64,
  control_slot: u8,
  replaced_control: bool,
  observation: DatabaseHeaderObservationV4,
  idempotent: bool,
}

#[derive(Clone, Copy)]
struct GcControlPublicationRequestV1<'a> {
  expected_control_kind: GcArtifactKindV1,
  encoded_control: &'a EncodedGcActiveControlV1,
  publication_timestamp_ms: u64,
  monotonic_now_ms: u64,
}

#[derive(Debug)]
enum GcControlPublicationOutcomeV1 {
  Complete(GcControlPublicationV1),
  CommittedFailure { publication: GcControlPublicationV1, failure: GcControlPublicationFailureV1, message: String },
}

enum GcControlDependencyOutcomeV1 {
  Complete(super::header_publication::DatabaseHeaderPublicationReceiptV4),
  CommittedFailure {
    publication: super::header_publication::DatabaseHeaderPublicationReceiptV4,
    failure: GcControlPublicationFailureV1,
    message: String,
  },
}

struct BufferedOnlyRetirementSinkV1;

impl RetirementJournalDurableSinkV1 for BufferedOnlyRetirementSinkV1 {
  fn publish_synced(
    &mut self,
    _segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    Err(RetirementJournalSinkErrorV1::new(
      "buffered_retirement_sink_invoked",
      std::io::Error::other("buffered retirement admission unexpectedly invoked its durable sink"),
    ))
  }
}

pub struct V4FirstAuthorityPublisher {
  file: File,
  kv: Mutex<DiskKVStore>,
  header_publisher: DatabaseHeaderPublisherV4,
  root_state: Mutex<()>,
}

impl V4FirstAuthorityPublisher {
  pub fn new(kv: DiskKVStore, coordinator: Arc<DurabilityCoordinator>) -> Result<Self, FirstAuthorityPublicationErrorV1> {
    if !kv.shares_durability_coordinator(&coordinator) {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_coordinator_mismatch",
        "the KV store and header publisher must share one durability coordinator",
      ));
    }
    let file = kv.clone_database_file()?;
    let observation = observe_database_header_v4(&file)?;
    validate_kv_header_alignment(&kv, &observation.selected.header)?;
    Ok(Self { file, kv: Mutex::new(kv), header_publisher: DatabaseHeaderPublisherV4::new(coordinator), root_state: Mutex::new(()) })
  }

  pub fn observe(&self) -> Result<DatabaseHeaderObservationV4, FirstAuthorityPublicationErrorV1> {
    observe_database_header_v4(&self.file).map_err(Into::into)
  }

  pub fn locator(&self, key: &[u8]) -> Result<Option<KVEntry>, FirstAuthorityPublicationErrorV1> {
    let kv = self.lock_kv()?;
    kv.get(key).map_err(Into::into)
  }

  pub fn admission_locator(&self, root_hash: &[u8]) -> Result<Option<KVEntry>, FirstAuthorityPublicationErrorV1> {
    let observation = self.observe()?;
    let path = system_control_path(SystemControlKindV1::RootAdmissionCommit, root_hash, SystemControlSlotV1::Immutable)?;
    let key = first_authority_file_path_hash(&path, observation.selected.header.hash_algorithm);
    self.locator(&key)
  }

  pub fn select_mark_run_checkpoint(
    &self,
    request: MarkRunCheckpointSelectionRequestV1<'_>,
  ) -> Result<MarkRunCheckpointSelectionV1, MarkRunCheckpointSelectionErrorV1> {
    if request.cancellation.is_cancelled() {
      return Err(MarkRunCheckpointSelectionErrorV1::invalid(
        "mark_checkpoint_selection_cancelled",
        "mark checkpoint selection was canceled before database access",
      ));
    }
    if request.resume_contexts.len() > 2 {
      return Err(MarkRunCheckpointSelectionErrorV1::invalid(
        "mark_checkpoint_selection_contexts",
        "mark checkpoint selection accepts at most one exact resume context per A/B slot",
      ));
    }

    let (observation, controls, checkpoint_loads, control_keys, control_locators) = {
      let _authority = self.root_state.lock().map_err(|poisoned| {
        drop(poisoned);
        MarkRunCheckpointSelectionErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
      })?;
      let observation = self.observe()?;
      let header = &observation.selected.header;
      if observation.selected.redundancy_degraded || header.head_hash.iter().all(|byte| *byte == 0) {
        return Err(MarkRunCheckpointSelectionErrorV1::invalid(
          "mark_checkpoint_selection_authority",
          "mark checkpoint selection requires selected non-degraded first authority",
        ));
      }
      for context in request.resume_contexts {
        if context.hash_algorithm != header.hash_algorithm || context.database_id != header.database_id {
          return Err(MarkRunCheckpointSelectionErrorV1::invalid(
            "mark_checkpoint_selection_contexts",
            "resume context hash profile or database identity differs from selected authority",
          ));
        }
      }
      let control_keys = [
        gc_active_control_key(header.hash_algorithm, GcArtifactKindV1::MarkRunActiveControl, &header.database_id, 0)
          .map_err(|source| MarkRunCheckpointSelectionErrorV1::invalid(source.code(), source.to_string()))?,
        gc_active_control_key(header.hash_algorithm, GcArtifactKindV1::MarkRunActiveControl, &header.database_id, 1)
          .map_err(|source| MarkRunCheckpointSelectionErrorV1::invalid(source.code(), source.to_string()))?,
      ];
      let kv = self.lock_kv()?;
      validate_kv_header_alignment(&kv, header)?;
      let controls = [
        load_mark_run_control_for_selection(&self.file, &kv, &control_keys[0], 0, header, request.cancellation, request.memory)?,
        load_mark_run_control_for_selection(&self.file, &kv, &control_keys[1], 1, header, request.cancellation, request.memory)?,
      ];
      let control_locators = [
        kv.get(&control_keys[0]).map_err(FirstAuthorityPublicationErrorV1::from)?,
        kv.get(&control_keys[1]).map_err(FirstAuthorityPublicationErrorV1::from)?,
      ];
      if matches!(controls[0], LoadedMarkRunControlSlotV1::Absent) && matches!(controls[1], LoadedMarkRunControlSlotV1::Absent) {
        return Ok(MarkRunCheckpointSelectionV1::Absent);
      }
      let mut checkpoint_loads = Vec::with_capacity(2);
      for slot in 0u8..=1 {
        let loaded = &controls[usize::from(slot)];
        let checkpoint = match loaded {
          LoadedMarkRunControlSlotV1::Valid(control) => Some(load_mark_run_checkpoint_for_selection(
            &self.file,
            &kv,
            control,
            slot,
            header,
            request.resume_contexts,
            request.cancellation,
            request.memory,
          )?),
          LoadedMarkRunControlSlotV1::Absent | LoadedMarkRunControlSlotV1::Invalid(_) => None,
        };
        checkpoint_loads.push(checkpoint);
      }
      (observation, controls, checkpoint_loads, control_keys, control_locators)
    };

    let header = &observation.selected.header;
    let mut diagnostics = Vec::new();
    for loaded in &controls {
      if let LoadedMarkRunControlSlotV1::Invalid(diagnostic) = loaded {
        diagnostics.push(diagnostic.clone());
      }
    }
    let mut closures: [Option<ValidatedMarkRunClosureV1>; 2] = std::array::from_fn(|_| None);
    for (slot, checkpoint_load) in (0u8..=1).zip(checkpoint_loads) {
      let Some(checkpoint_load) = checkpoint_load else {
        continue;
      };
      let checkpoint_load = match checkpoint_load {
        Ok(checkpoint) => checkpoint,
        Err(diagnostic) => {
          diagnostics.push(diagnostic);
          continue;
        }
      };
      let checkpoint = decode_loaded_mark_checkpoint(&checkpoint_load, header)
        .map_err(|source| MarkRunCheckpointSelectionErrorV1::invalid(source.code(), source.to_string()))?;
      let context = &request.resume_contexts[checkpoint_load.resume_context_index];
      match ReopenedMarkWorkspaceV1::open(
        &checkpoint,
        context,
        request.workspace_options.clone(),
        request.cancellation.clone(),
        request.memory,
      ) {
        Ok(workspace) => {
          closures[usize::from(slot)] = Some(ValidatedMarkRunClosureV1 {
            checkpoint_entity: checkpoint_load.entity,
            checkpoint_key: checkpoint_load.key,
            checkpoint_write_sequence: checkpoint_load.write_sequence,
            workspace,
          });
        }
        Err(source) => diagnostics.push(classify_workspace_selection_error(slot, source)?),
      }
    }

    if request.cancellation.is_cancelled() {
      return Err(MarkRunCheckpointSelectionErrorV1::invalid(
        "mark_checkpoint_selection_cancelled",
        "mark checkpoint selection was canceled before authority revalidation",
      ));
    }
    {
      let _authority = self.root_state.lock().map_err(|poisoned| {
        drop(poisoned);
        MarkRunCheckpointSelectionErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
      })?;
      let current = self.observe()?;
      if current.selected.header.database_id != header.database_id || current.selected.header.hash_algorithm != header.hash_algorithm {
        return Err(MarkRunCheckpointSelectionErrorV1::invalid(
          "mark_checkpoint_selection_changed",
          "selected database identity or hash profile changed during workspace validation",
        ));
      }
      let kv = self.lock_kv()?;
      validate_kv_header_alignment(&kv, &current.selected.header)?;
      for slot in 0..2 {
        if kv.get(&control_keys[slot]).map_err(FirstAuthorityPublicationErrorV1::from)? != control_locators[slot] {
          return Err(MarkRunCheckpointSelectionErrorV1::invalid(
            "mark_checkpoint_selection_changed",
            "mark-run A/B authority changed during workspace validation; selection must be retried",
          ));
        }
      }
    }

    let selected_slot = match (&controls[0], &controls[1]) {
      (LoadedMarkRunControlSlotV1::Valid(a), LoadedMarkRunControlSlotV1::Valid(b)) => {
        let a = decode_loaded_mark_control(a, header)
          .map_err(|source| MarkRunCheckpointSelectionErrorV1::invalid(source.code(), source.to_string()))?;
        let b = decode_loaded_mark_control(b, header)
          .map_err(|source| MarkRunCheckpointSelectionErrorV1::invalid(source.code(), source.to_string()))?;
        select_gc_active_control(&a, closures[0].is_some(), &b, closures[1].is_some())
          .map_err(|source| MarkRunCheckpointSelectionErrorV1::invalid(source.code(), source.to_string()))?
          .map(|selected| selected.slot)
      }
      (LoadedMarkRunControlSlotV1::Valid(_), _) if closures[0].is_some() => Some(0),
      (_, LoadedMarkRunControlSlotV1::Valid(_)) if closures[1].is_some() => Some(1),
      _ => None,
    };
    let Some(selected_slot) = selected_slot else {
      diagnostics.sort_by_key(|diagnostic| diagnostic.slot);
      return Err(MarkRunCheckpointSelectionErrorV1::invalid(
        "mark_checkpoint_selection_no_valid_closure",
        format!("no complete mark checkpoint closure is selectable: {diagnostics:?}"),
      ));
    };
    let selected_index = usize::from(selected_slot);
    let selected_control = match &controls[selected_index] {
      LoadedMarkRunControlSlotV1::Valid(control) => control,
      LoadedMarkRunControlSlotV1::Absent | LoadedMarkRunControlSlotV1::Invalid(_) => {
        return Err(MarkRunCheckpointSelectionErrorV1::invalid(
          "mark_checkpoint_selection_internal",
          "frozen selector returned a slot without a valid control",
        ));
      }
    };
    let decoded_control = decode_loaded_mark_control(selected_control, header)
      .map_err(|source| MarkRunCheckpointSelectionErrorV1::invalid(source.code(), source.to_string()))?;
    let selected = closures[selected_index].take().ok_or_else(|| {
      MarkRunCheckpointSelectionErrorV1::invalid("mark_checkpoint_selection_internal", "frozen selector returned an incomplete closure")
    })?;
    diagnostics.sort_by_key(|diagnostic| diagnostic.slot);
    Ok(MarkRunCheckpointSelectionV1::Selected(Box::new(SelectedMarkRunCheckpointV1 {
      algorithm: header.hash_algorithm,
      write_sequence_high_water: header.write_sequence_high_water,
      checkpoint_entity_bytes: selected.checkpoint_entity.bytes,
      _checkpoint_memory: selected.checkpoint_entity._memory,
      workspace: selected.workspace,
      control_slot: selected_slot,
      control_sequence: decoded_control.sequence,
      control_write_sequence: selected_control.write_sequence,
      checkpoint_key: selected.checkpoint_key,
      checkpoint_write_sequence: selected.checkpoint_write_sequence,
      degraded_slots: diagnostics,
    })))
  }

  pub fn publish_mark_run_checkpoint(
    &mut self,
    request: MarkRunCheckpointPublicationRequestV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  ) -> Result<MarkRunCheckpointPublicationReceiptV1, MarkRunCheckpointPublicationErrorV1> {
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    self.publish_mark_run_checkpoint_with_control_observer(request, retirement_owner, &mut observer)
  }

  fn publish_mark_run_checkpoint_with_control_observer(
    &mut self,
    request: MarkRunCheckpointPublicationRequestV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<MarkRunCheckpointPublicationReceiptV1, MarkRunCheckpointPublicationErrorV1> {
    let (checkpoint, _) = validate_mark_run_checkpoint_publication(&request)?;
    if retirement_owner.hash_algorithm() != request.hash_algorithm || retirement_owner.database_id() != checkpoint.database_id {
      return Err(MarkRunCheckpointPublicationErrorV1::invalid(
        "mark_checkpoint_retirement_owner",
        "retirement owner database or hash profile differs from the mark checkpoint",
      ));
    }

    let Ok(checkpoint_database_id): Result<[u8; 16], _> = checkpoint.database_id.try_into() else {
      return Err(MarkRunCheckpointPublicationErrorV1::invalid(
        "mark_checkpoint_database",
        "mark checkpoint database identity has the wrong width",
      ));
    };
    retirement_owner.flush(self)?;
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    let checkpoint_write_sequence = self.publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::MarkRunCheckpoint,
        database_id: &checkpoint_database_id,
        artifact_key: &request.checkpoint.key,
        value: &request.checkpoint.value,
        minimum_timestamp_ms: checkpoint.updated_at_ms.max(request.publication_timestamp_ms),
        committed_postcondition_code: "mark_checkpoint_committed_postcondition",
      },
      &mut observer,
    )?;

    let control_publication = self
      .publish_gc_active_control(
        GcControlPublicationRequestV1 {
          expected_control_kind: GcArtifactKindV1::MarkRunActiveControl,
          encoded_control: request.control,
          publication_timestamp_ms: request.publication_timestamp_ms,
          monotonic_now_ms: request.monotonic_now_ms,
        },
        retirement_owner,
        control_observer,
      )
      .map_err(MarkRunCheckpointPublicationErrorV1::from)?;
    let (control_publication, mut committed_failure) = match control_publication {
      GcControlPublicationOutcomeV1::Complete(publication) => (publication, None),
      GcControlPublicationOutcomeV1::CommittedFailure { publication, failure, message } => {
        (publication, Some((failure.mark_run_code(), message)))
      }
    };
    let lineage_state = if control_publication.replaced_control {
      match retirement_owner.flush(self) {
        Ok(true) => MarkRunCheckpointLineageStateV1::HardPublished {
          hard_publication_sequence: retirement_owner.status().last_hard_publication_sequence,
        },
        Ok(false) => MarkRunCheckpointLineageStateV1::MissingAfterCommit {
          code: "mark_checkpoint_retirement_missing",
          message: "control replacement committed without retained retirement lineage".to_string(),
        },
        Err(source) => MarkRunCheckpointLineageStateV1::BufferedAfterFlushFailure { code: source.code(), message: source.to_string() },
      }
    } else {
      MarkRunCheckpointLineageStateV1::NotRequired
    };
    let missing_lineage = matches!(lineage_state, MarkRunCheckpointLineageStateV1::MissingAfterCommit { .. });
    let observation = if matches!(lineage_state, MarkRunCheckpointLineageStateV1::HardPublished { .. }) {
      match self.observe() {
        Ok(observation) => observation,
        Err(source) => {
          if committed_failure.is_none() {
            committed_failure = Some(("mark_checkpoint_committed_lineage_readback_failure", source.to_string()));
          }
          control_publication.observation
        }
      }
    } else {
      control_publication.observation
    };
    let receipt = MarkRunCheckpointPublicationReceiptV1 {
      checkpoint_key: request.checkpoint.key.clone(),
      checkpoint_write_sequence,
      control_key: request.control.key.clone(),
      control_write_sequence: control_publication.control_write_sequence,
      control_slot: control_publication.control_slot,
      replaced_control: control_publication.replaced_control,
      lineage_state,
      observation,
      idempotent: control_publication.idempotent,
    };
    if let Some((code, message)) = committed_failure {
      return Err(MarkRunCheckpointPublicationErrorV1::committed(code, message, receipt));
    }
    if missing_lineage {
      return Err(MarkRunCheckpointPublicationErrorV1::committed(
        "mark_checkpoint_retirement_missing",
        "control replacement committed without retained retirement lineage",
        receipt,
      ));
    }
    Ok(receipt)
  }

  fn publish_gc_active_control(
    &mut self,
    request: GcControlPublicationRequestV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<GcControlPublicationOutcomeV1, GcControlPublicationErrorV1> {
    let expected_control_kind = request.expected_control_kind;
    let encoded_control = request.encoded_control;
    let publication_timestamp_ms = request.publication_timestamp_ms;
    let monotonic_now_ms = request.monotonic_now_ms;
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      GcControlPublicationErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded || header.head_hash.iter().all(|byte| *byte == 0) {
      return Err(GcControlPublicationErrorV1::invalid(
        GcControlPublicationFailureV1::MissingAuthority,
        "GC control publication requires selected non-degraded first authority",
      ));
    }
    let control = decode_gc_active_control(&encoded_control.value, header.hash_algorithm)?;
    if !expected_control_kind.is_control() || control.kind != expected_control_kind || control.key != encoded_control.key {
      return Err(GcControlPublicationErrorV1::invalid(
        GcControlPublicationFailureV1::ControlKind,
        "prepared and expected GC control identities disagree",
      ));
    }
    if header.hash_algorithm != retirement_owner.hash_algorithm()
      || retirement_owner.database_id() != header.database_id
      || control.database_id != header.database_id
    {
      return Err(GcControlPublicationErrorV1::invalid(
        GcControlPublicationFailureV1::ControlDatabase,
        "GC control, retirement owner, and selected database authority disagree",
      ));
    }

    let mut kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    kv.flush()?;
    validate_kv_header_alignment(&kv, header)?;
    if kv.write_buffer_len() != 0 || kv.hot_buffer_len() != 0 {
      return Err(GcControlPublicationErrorV1::invalid(
        GcControlPublicationFailureV1::BaselineNotFlushed,
        "GC control publication requires an empty KV write and hot-buffer baseline",
      ));
    }

    let control_keys = gc_control_keys(header.hash_algorithm, expected_control_kind, &header.database_id)?;
    if encoded_control.key != control_keys[usize::from(control.slot)] {
      return Err(GcControlPublicationErrorV1::invalid(
        GcControlPublicationFailureV1::ControlKey,
        "prepared GC control key does not match its kind, database, and slot",
      ));
    }
    let stored_a = load_gc_control_entity(&self.file, &kv, expected_control_kind, &control_keys[0], header)?;
    let stored_b = load_gc_control_entity(&self.file, &kv, expected_control_kind, &control_keys[1], header)?;
    let stored_controls = [stored_a, stored_b];
    if let Some(existing) = &stored_controls[usize::from(control.slot)] {
      if existing.stored_value == encoded_control.value {
        return Ok(GcControlPublicationOutcomeV1::Complete(GcControlPublicationV1 {
          control_write_sequence: existing.write_sequence,
          control_slot: control.slot,
          replaced_control: false,
          observation,
          idempotent: true,
        }));
      }
    }

    let required_slot = required_gc_control_slot(&stored_controls)?;
    if control.slot != required_slot {
      return Err(GcControlPublicationErrorV1::invalid(
        GcControlPublicationFailureV1::ControlSlot,
        format!("GC control must publish slot {required_slot}, got {}", control.slot),
      ));
    }
    let maximum_control_sequence = stored_controls.iter().flatten().map(|stored| stored.control_sequence).fold(0, u64::max);
    if control.sequence <= maximum_control_sequence {
      return Err(GcControlPublicationErrorV1::invalid(
        GcControlPublicationFailureV1::ControlSequence,
        "GC control sequence does not advance the selected A/B pair",
      ));
    }

    let write_sequence = header.write_sequence_high_water.checked_add(1).ok_or_else(|| {
      GcControlPublicationErrorV1::invalid(GcControlPublicationFailureV1::WriteSequenceExhausted, "v4 write sequence is exhausted")
    })?;
    let mut reservation_candidate = header.clone();
    reservation_candidate.updated_at_ms = header.updated_at_ms.max(publication_timestamp_ms);
    reservation_candidate.write_sequence_high_water = write_sequence;
    let reservation = self.header_publisher.publish_inactive_slot(&self.file, &observation, reservation_candidate)?;
    let reserved_observation = reservation.observation;
    let header = &reserved_observation.selected.header;
    validate_kv_header_alignment(&kv, header)?;
    let entity_bytes = encode_entity(
      EntryTypeV4::GcArtifact,
      WHOLE_ENTITY_V1_FLAG_SYSTEM,
      header.hash_algorithm,
      header.updated_at_ms.max(publication_timestamp_ms),
      write_sequence,
      &encoded_control.key,
      &encoded_control.value,
    )?;
    let replacement_entity = decode_whole_entity(&entity_bytes, header.hash_algorithm, write_sequence)?;
    let append_start = self.file.metadata().map_err(EngineError::IoError)?.len();
    if append_start < header.hot_tail_offset {
      return Err(GcControlPublicationErrorV1::invalid(
        GcControlPublicationFailureV1::FileTruncated,
        "database length precedes the selected v4 hot-tail offset",
      ));
    }
    write_file_at_native(&self.file, append_start, &entity_bytes)
      .map_err(|source| GcControlPublicationErrorV1::invalid(GcControlPublicationFailureV1::ControlWrite, source.to_string()))?;
    verify_file_bytes_native(&self.file, append_start, &entity_bytes)
      .map_err(|source| GcControlPublicationErrorV1::invalid(GcControlPublicationFailureV1::ControlReadback, source.to_string()))?;
    let expected_hot_tail_offset = append_start.checked_add(entity_bytes.len() as u64).ok_or_else(|| {
      GcControlPublicationErrorV1::invalid(GcControlPublicationFailureV1::WalOverflow, "GC control WAL offset overflowed")
    })?;
    let expected_existing = stored_controls[usize::from(control.slot)].as_ref().map(|stored| &stored.locator);
    let replacement_incarnation = encode_v4_physical_incarnation(
      header.hash_algorithm,
      &encoded_control.key,
      replacement_entity.integrity_hash,
      append_start,
      write_sequence,
      entity_bytes.len(),
      EntryTypeV4::GcArtifact,
    )?;
    let prepared_retirement = if let Some(stored) = &stored_controls[usize::from(control.slot)] {
      let old_incarnation = encode_v4_physical_incarnation(
        header.hash_algorithm,
        &encoded_control.key,
        &stored.integrity_hash,
        stored.locator.offset,
        stored.write_sequence,
        stored.entity_bytes.len(),
        EntryTypeV4::GcArtifact,
      )?;
      let replacements = [RetirementJournalReplacementV1 {
        reason: RetirementReasonV1::PointerOrControlReplace,
        old_incarnation: &old_incarnation,
        replacement_incarnation: &replacement_incarnation,
      }];
      let mut sink = BufferedOnlyRetirementSinkV1;
      Some(RetirementJournalReplacementCoordinatorV1::new(retirement_owner, &mut sink).prepare_buffered_single(
        RetirementJournalReplacementBatchV1 {
          replacement_publication_sequence: write_sequence,
          retired_at_ms: publication_timestamp_ms,
          replacements: &replacements,
        },
        monotonic_now_ms,
      )?)
    } else {
      None
    };

    let entry_count = header.entry_count.checked_add(u64::from(expected_existing.is_none())).ok_or_else(|| {
      GcControlPublicationErrorV1::invalid(GcControlPublicationFailureV1::EntryCountOverflow, "v4 entry count overflowed")
    })?;
    let entities = [PreparedWholeEntityV1 { key: encoded_control.key.clone(), kv_type: kv_tag::GC_ARTIFACT, bytes: entity_bytes }];
    let orphan_prefix_bytes = append_start.checked_sub(header.hot_tail_offset).ok_or_else(|| {
      GcControlPublicationErrorV1::invalid(GcControlPublicationFailureV1::WalPrefix, "GC control append precedes the selected hot tail")
    })?;
    let dependency_bytes = entity_dependency_bytes(&entities, header.hash_algorithm.hash_length())?
      .checked_add(orphan_prefix_bytes)
      .ok_or_else(|| GcControlPublicationErrorV1::invalid(GcControlPublicationFailureV1::DependencySize, "dependency bytes overflowed"))?;
    let mut candidate = header.clone();
    candidate.updated_at_ms = header.updated_at_ms.max(publication_timestamp_ms);
    candidate.write_sequence_high_water = write_sequence;
    candidate.hot_tail_offset = expected_hot_tail_offset;
    candidate.entry_count = entry_count;
    let admitted =
      self.header_publisher.admit_inactive_slot_with_dependency_bytes(&self.file, &reserved_observation, candidate, dependency_bytes)?;
    let authority_sequence = admitted.sequence();
    let batch = kv.begin_atomic_visibility_batch(1, authority_sequence)?;
    let publication = if let Some(prepared) = prepared_retirement {
      match prepared.activate(|_| {
        commit_gc_control_dependency(
          &self.file,
          &mut kv,
          admitted,
          batch,
          authority_sequence,
          &entities,
          append_start,
          expected_hot_tail_offset,
          expected_existing,
          observer,
        )
      }) {
        Ok(outcome) => outcome.output,
        Err(source) => {
          let Some((source, prepared)) = source.into_activation_failure() else {
            return Err(GcControlPublicationErrorV1::invalid(
              GcControlPublicationFailureV1::RetirementActivation,
              "prepared retirement activation returned an admission error",
            ));
          };
          if let Err(discard_error) = prepared.discard_buffered(retirement_owner) {
            let (discard_source, _prepared) = discard_error.into_parts();
            return Err(discard_source.into());
          }
          return Err(source.into());
        }
      }
    } else {
      commit_gc_control_dependency(
        &self.file,
        &mut kv,
        admitted,
        batch,
        authority_sequence,
        &entities,
        append_start,
        expected_hot_tail_offset,
        expected_existing,
        observer,
      )?
    };
    let (publication, committed_failure) = match publication {
      GcControlDependencyOutcomeV1::Complete(publication) => (publication, None),
      GcControlDependencyOutcomeV1::CommittedFailure { publication, failure, message } => (publication, Some((failure, message))),
    };
    let control_publication = GcControlPublicationV1 {
      control_write_sequence: write_sequence,
      control_slot: control.slot,
      replaced_control: expected_existing.is_some(),
      observation: publication.observation,
      idempotent: false,
    };
    if let Some((failure, message)) = committed_failure {
      return Ok(GcControlPublicationOutcomeV1::CommittedFailure { publication: control_publication, failure, message });
    }
    let stored = match read_entity_bounded(
      &self.file,
      &kv,
      &encoded_control.key,
      entities[0].bytes.len(),
      control_publication.observation.selected.header.write_sequence_high_water,
    ) {
      Ok(Some(stored)) => stored,
      Ok(None) => {
        return Ok(GcControlPublicationOutcomeV1::CommittedFailure {
          publication: control_publication,
          failure: GcControlPublicationFailureV1::CommittedReadbackFailure,
          message: "published GC control is absent".to_string(),
        });
      }
      Err(source) => {
        return Ok(GcControlPublicationOutcomeV1::CommittedFailure {
          publication: control_publication,
          failure: GcControlPublicationFailureV1::CommittedReadbackFailure,
          message: source.to_string(),
        });
      }
    };
    if stored != entities[0].bytes {
      return Ok(GcControlPublicationOutcomeV1::CommittedFailure {
        publication: control_publication,
        failure: GcControlPublicationFailureV1::CommittedReadbackFailure,
        message: "published GC control differs from its exact prepared bytes".to_string(),
      });
    }
    Ok(GcControlPublicationOutcomeV1::Complete(control_publication))
  }

  fn publish_retirement_journal_segment(
    &self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    let observation = self.observe().map_err(retirement_sink_first_authority_error)?;
    let header = &observation.selected.header;
    let decoded = decode_retirement_journal_segment_v1(segment.value, header.hash_algorithm).map_err(retirement_sink_format_error)?;
    if decoded.key != segment.artifact_key
      || decoded.segment_ordinal != segment.segment_ordinal
      || decoded.generation != segment.generation
      || decoded.first_replacement_sequence != segment.first_replacement_sequence
      || decoded.last_replacement_sequence != segment.last_replacement_sequence
      || decoded.record_count != segment.record_count
    {
      return Err(retirement_sink_invalid(
        "retirement_journal_prepared_mismatch",
        "prepared retirement-journal fields do not match the exact immutable artifact",
      ));
    }
    if decoded.database_id != header.database_id {
      return Err(retirement_sink_invalid(
        "retirement_journal_database_mismatch",
        "retirement-journal segment belongs to another logical database",
      ));
    }
    let mut segment_timestamp_ms = 0;
    for record in retirement_journal_records_v1(&decoded, header.hash_algorithm).map_err(retirement_sink_format_error)? {
      let record = record.map_err(retirement_sink_format_error)?;
      segment_timestamp_ms = segment_timestamp_ms.max(record.retired_at_ms);
    }
    let database_id: [u8; 16] = decoded.database_id.try_into().map_err(|_| {
      retirement_sink_invalid("retirement_journal_database_mismatch", "retirement-journal database identity has the wrong width")
    })?;
    let write_sequence = self
      .publish_immutable_gc_artifact(
        ImmutableGcArtifactPublicationV1 {
          kind: GcArtifactKindV1::RetirementJournalSegment,
          database_id: &database_id,
          artifact_key: segment.artifact_key,
          value: segment.value,
          minimum_timestamp_ms: segment_timestamp_ms,
          committed_postcondition_code: "immutable_gc_committed_postcondition",
        },
        observer,
      )
      .map_err(retirement_sink_first_authority_error)?;
    retirement_journal_receipt(segment, write_sequence)
  }

  fn publish_mark_mutation_journal_segment(
    &self,
    segment: &PreparedMarkMutationJournalSegmentV1<'_>,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<MarkMutationJournalDurabilityReceiptV1, MarkMutationJournalSinkErrorV1> {
    let observation = self.observe().map_err(mark_mutation_sink_first_authority_error)?;
    let header = &observation.selected.header;
    let GcMarkArtifactV1::MutationJournal(decoded) =
      decode_gc_mark_artifact(segment.value, header.hash_algorithm).map_err(mark_mutation_sink_format_error)?
    else {
      return Err(mark_mutation_sink_invalid("mark_mutation_artifact_kind", "prepared mark-mutation segment is another GC artifact kind"));
    };
    if decoded.key != segment.artifact_key
      || decoded.segment_sequence != segment.segment_ordinal
      || decoded.generation != segment.generation
      || decoded.first_sequence != segment.first_publication_sequence
      || decoded.last_sequence != segment.last_publication_sequence
      || decoded.record_count != segment.record_count
    {
      return Err(mark_mutation_sink_invalid(
        "mark_mutation_prepared_mismatch",
        "prepared mark-mutation fields do not match the exact immutable artifact",
      ));
    }
    if decoded.database_id != header.database_id {
      return Err(mark_mutation_sink_invalid(
        "mark_mutation_database_mismatch",
        "mark-mutation segment belongs to another logical database",
      ));
    }
    let database_id: [u8; 16] =
      decoded.database_id.try_into().map_err(|error| MarkMutationJournalSinkErrorV1::new("mark_mutation_database_mismatch", error))?;
    let hard_publication_sequence = self
      .publish_immutable_gc_artifact(
        ImmutableGcArtifactPublicationV1 {
          kind: GcArtifactKindV1::MarkMutationJournalSegment,
          database_id: &database_id,
          artifact_key: segment.artifact_key,
          value: segment.value,
          minimum_timestamp_ms: 0,
          committed_postcondition_code: "immutable_gc_committed_postcondition",
        },
        observer,
      )
      .map_err(mark_mutation_sink_first_authority_error)?;
    mark_mutation_journal_receipt(segment, hard_publication_sequence)
  }

  fn publish_immutable_gc_artifact(
    &self,
    request: ImmutableGcArtifactPublicationV1<'_>,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<u64, FirstAuthorityPublicationErrorV1> {
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      FirstAuthorityPublicationErrorV1::StateLockPoisoned
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_gc_degraded_header",
        "immutable GC publication requires two valid v4 header slots",
      ));
    }
    if header.head_hash.iter().all(|byte| *byte == 0) {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_gc_missing_authority",
        "immutable GC publication requires selected first authority",
      ));
    }
    if &header.database_id != request.database_id {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_gc_database_mismatch",
        "immutable GC artifact belongs to another logical database",
      ));
    }
    let envelope = decode_gc_artifact_envelope(request.value)?;
    if envelope.kind != request.kind
      || envelope.identity.len() < 16
      || &envelope.identity[..16] != request.database_id
      || immutable_gc_artifact_key(header.hash_algorithm, request.kind, request.value) != request.artifact_key
    {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_gc_prepared_mismatch",
        "immutable GC key, kind, database identity, or value disagrees",
      ));
    }
    let publication_timestamp_ms = header.updated_at_ms.max(request.minimum_timestamp_ms);
    let mut kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    if let Some(locator) = kv.get(request.artifact_key)? {
      if locator.type_flags != kv_tag::GC_ARTIFACT {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "immutable_gc_identity_collision",
          "immutable GC artifact key resolves to another KV role",
        ));
      }
      let maximum_length =
        super::entity::checked_whole_entity_encoded_length(header.hash_algorithm, request.artifact_key.len(), request.value.len())?;
      let bytes = read_entity_bounded(&self.file, &kv, request.artifact_key, maximum_length, header.write_sequence_high_water)?
        .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("immutable_gc_readback_missing", "immutable GC locator disappeared"))?;
      let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water)?;
      if entity.entry_type != EntryTypeV4::GcArtifact
        || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
        || entity.compression_algorithm != CompressionAlgorithm::None
        || entity.key != request.artifact_key
        || entity.stored_value != request.value
        || entity.timestamp_ms < request.minimum_timestamp_ms
      {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "immutable_gc_identity_collision",
          "existing immutable GC entity differs from its exact artifact representation",
        ));
      }
      return Ok(entity.write_sequence);
    }

    let write_sequence = header.write_sequence_high_water.checked_add(1).ok_or_else(|| {
      FirstAuthorityPublicationErrorV1::invalid("immutable_gc_write_sequence_exhausted", "v4 write sequence is exhausted")
    })?;
    let entry_count = header
      .entry_count
      .checked_add(1)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("immutable_gc_entry_count_overflow", "v4 entry count overflowed"))?;
    let entity_bytes = encode_entity(
      EntryTypeV4::GcArtifact,
      WHOLE_ENTITY_V1_FLAG_SYSTEM,
      header.hash_algorithm,
      publication_timestamp_ms,
      write_sequence,
      request.artifact_key,
      request.value,
    )?;
    let entities = [PreparedWholeEntityV1 { key: request.artifact_key.to_vec(), kv_type: kv_tag::GC_ARTIFACT, bytes: entity_bytes }];
    let dependency_bytes = entity_dependency_bytes(&entities, header.hash_algorithm.hash_length())?;

    kv.flush()?;
    validate_kv_header_alignment(&kv, header)?;
    if kv.write_buffer_len() != 0 || kv.hot_buffer_len() != 0 {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_gc_baseline_not_flushed",
        "immutable GC publication requires an empty KV write and hot-buffer baseline",
      ));
    }
    let append_start = self.file.metadata().map_err(EngineError::IoError)?.len();
    if append_start < header.hot_tail_offset {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_gc_file_truncated",
        "database length precedes the selected v4 hot-tail offset",
      ));
    }
    let expected_hot_tail_offset = append_start
      .checked_add(entities[0].bytes.len() as u64)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("immutable_gc_wal_overflow", "immutable GC WAL offset overflowed"))?;
    let mut candidate = header.clone();
    candidate.updated_at_ms = publication_timestamp_ms;
    candidate.write_sequence_high_water = write_sequence;
    candidate.hot_tail_offset = expected_hot_tail_offset;
    candidate.entry_count = entry_count;
    let admitted =
      self.header_publisher.admit_inactive_slot_with_dependency_bytes(&self.file, &observation, candidate, dependency_bytes)?;
    let authority_sequence = admitted.sequence();
    let batch = kv.begin_atomic_visibility_batch(1, authority_sequence)?;
    let (publication_result, append_completed) = {
      let mut dependency = FirstAuthorityDependencyV1 {
        file: &self.file,
        kv: &mut kv,
        batch,
        expected_publication_sequence: authority_sequence,
        entities: &entities,
        start_offset: append_start,
        expected_hot_tail_offset,
        expected_existing: None,
        prewritten: false,
        append_completed: false,
        observer,
      };
      let publication_result = admitted.commit_with_dependency(&mut dependency);
      (publication_result, dependency.append_completed)
    };
    let publication = match publication_result {
      Ok(publication) => publication,
      Err(error) => {
        kv.abort_atomic_visibility_batch(batch)?;
        return Err(error.into());
      }
    };
    if !append_completed {
      kv.abort_atomic_visibility_batch(batch)?;
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_gc_dependency_missing",
        "header publication completed without the exact immutable GC dependency append",
      ));
    }
    kv.complete_hot_tail_dependency();
    kv.publish_atomic_visibility_after_authority(batch, &publication.durability)?;
    observer
      .authority_committed(&kv, &entities)
      .map_err(|error| FirstAuthorityPublicationErrorV1::invalid(request.committed_postcondition_code, error.to_string()))?;

    let stored = read_entity_bounded(
      &self.file,
      &kv,
      request.artifact_key,
      entities[0].bytes.len(),
      publication.observation.selected.header.write_sequence_high_water,
    )?
    .ok_or_else(|| {
      FirstAuthorityPublicationErrorV1::invalid("immutable_gc_readback_missing", "published immutable GC locator is absent")
    })?;
    if stored != entities[0].bytes {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_gc_readback_mismatch",
        "published immutable GC entity differs from its exact prepared bytes",
      ));
    }
    Ok(write_sequence)
  }

  pub fn publish(
    &self,
    request: &FirstAuthorityPublicationRequestV1,
  ) -> Result<FirstAuthorityPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    self.publish_with_observer(request, &mut observer)
  }

  fn publish_with_observer(
    &self,
    request: &FirstAuthorityPublicationRequestV1,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<FirstAuthorityPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    let _root_state = match self.root_state.lock() {
      Ok(root_state) => root_state,
      Err(poisoned) => {
        drop(poisoned);
        return Err(FirstAuthorityPublicationErrorV1::StateLockPoisoned);
      }
    };
    let observation = observe_database_header_v4(&self.file)?;
    if observation.selected.redundancy_degraded {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_degraded_header",
        "first authority requires two valid v4 header slots",
      ));
    }
    let header = &observation.selected.header;
    if header.database_id != request.database_id {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_database_mismatch",
        "the request belongs to a different logical database",
      ));
    }
    let namespace_root = prepare_namespace_root(request, header.hash_algorithm, header.write_sequence_high_water)?;
    if header.head_hash.iter().any(|byte| *byte != 0) {
      return self.load_idempotent(request, namespace_root, observation);
    }

    let mut kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let selected_header_slot_sequence = header
      .slot_sequence
      .checked_add(1)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_header_sequence_exhausted", "header sequence exhausted"))?;
    let sizing_package = build_package(request, namespace_root.clone(), header, selected_header_slot_sequence, 1)?;
    refuse_existing_entities(&kv, &sizing_package.entities)?;
    let expected_hot_tail_offset = sizing_package.hot_tail_offset;
    let expected_write_sequence_high_water = sizing_package.write_sequence_high_water;
    let dependency_bytes = package_dependency_bytes(&sizing_package, header.hash_algorithm.hash_length())?;
    drop(sizing_package);
    let mut candidate = header.clone();
    candidate.updated_at_ms = candidate.updated_at_ms.max(request.created_at_ms);
    candidate.write_sequence_high_water = expected_write_sequence_high_water;
    candidate.hot_tail_offset = expected_hot_tail_offset;
    candidate.entry_count = candidate
      .entry_count
      .checked_add(FIRST_AUTHORITY_ENTITY_COUNT as u64)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_entry_count_overflow", "header entry count overflow"))?;
    candidate.head_hash = namespace_root.root_hash.clone();

    let admitted =
      self.header_publisher.admit_inactive_slot_with_dependency_bytes(&self.file, &observation, candidate, dependency_bytes)?;
    let publication_sequence = admitted.sequence();
    let package = build_package(request, namespace_root, header, selected_header_slot_sequence, publication_sequence)?;
    if package.hot_tail_offset != expected_hot_tail_offset || package.write_sequence_high_water != expected_write_sequence_high_water {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_sizing_changed",
        "publication sequence changed the pre-admitted physical layout",
      ));
    }
    let batch = kv.begin_atomic_visibility_batch(FIRST_AUTHORITY_ENTITY_COUNT, publication_sequence)?;
    let (publication_result, append_completed) = {
      let mut dependency = FirstAuthorityDependencyV1 {
        file: &self.file,
        kv: &mut kv,
        batch,
        expected_publication_sequence: publication_sequence,
        entities: &package.entities,
        start_offset: header.hot_tail_offset,
        expected_hot_tail_offset: package.hot_tail_offset,
        expected_existing: None,
        prewritten: false,
        append_completed: false,
        observer,
      };
      let publication_result = admitted.commit_with_dependency(&mut dependency);
      (publication_result, dependency.append_completed)
    };
    let publication = match publication_result {
      Ok(publication) => publication,
      Err(error) => {
        kv.abort_atomic_visibility_batch(batch)?;
        return Err(error.into());
      }
    };
    if !append_completed {
      kv.abort_atomic_visibility_batch(batch)?;
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_dependency_missing",
        "header publication completed without the exact root dependency append",
      ));
    }
    kv.complete_hot_tail_dependency();
    let receipt = FirstAuthorityPublicationReceiptV1 {
      namespace_root: package.namespace_root.clone(),
      prepare_control: package.prepare_control.clone(),
      admission_control: package.admission_control.clone(),
      publication_sequence,
      observation: publication.observation,
      idempotent: false,
    };
    match kv.publish_atomic_visibility_after_authority(batch, &publication.durability) {
      Ok(()) => {}
      Err(error) => {
        return Err(FirstAuthorityPublicationErrorV1::committed(
          "first_authority_committed_visibility_failure",
          error.to_string(),
          receipt,
        ));
      }
    }
    match observer.authority_committed(&kv, &package.entities) {
      Ok(()) => {}
      Err(error) => {
        return Err(FirstAuthorityPublicationErrorV1::committed(
          "first_authority_committed_postcondition_failure",
          error.to_string(),
          receipt,
        ));
      }
    }
    match verify_package_locators(&self.file, &kv, &package, &receipt.observation.selected.header) {
      Ok(()) => Ok(receipt),
      Err(error) => {
        Err(FirstAuthorityPublicationErrorV1::committed("first_authority_committed_readback_failure", error.to_string(), receipt))
      }
    }
  }

  fn load_idempotent(
    &self,
    request: &FirstAuthorityPublicationRequestV1,
    namespace_root: EncodedNamespaceRootV1,
    observation: DatabaseHeaderObservationV4,
  ) -> Result<FirstAuthorityPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    if observation.selected.header.head_hash != namespace_root.root_hash {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_already_selected",
        "the database already selects a different first authority",
      ));
    }
    let kv = self.lock_kv()?;
    let admission_control =
      load_system_file(&self.file, &kv, &observation.selected.header, SystemControlKindV1::RootAdmissionCommit, &namespace_root.root_hash)?;
    let admission = decode_root_admission_commit(&admission_control, observation.selected.header.hash_algorithm)?;
    if admission.selected_header_slot_sequence != observation.selected.header.slot_sequence
      || admission.namespace_root != namespace_root.root_hash
      || admission.database_id != request.database_id
      || admission.transaction_id != request.transaction_id
      || admission.authority_kind != RootAuthorityKindV1::Head
    {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_witness_mismatch",
        "selected HEAD and first-admission witness do not describe the requested transaction",
      ));
    }
    let base_write_sequence =
      observation.selected.header.write_sequence_high_water.checked_sub(FIRST_AUTHORITY_ENTITY_COUNT as u64).ok_or_else(|| {
        FirstAuthorityPublicationErrorV1::invalid("first_authority_sequence_underflow", "selected write sequence is too small")
      })?;
    let mut source_header = observation.selected.header.clone();
    source_header.write_sequence_high_water = base_write_sequence;
    source_header.hot_tail_offset = package_start_offset(&self.file, &kv, &observation.selected.header, &request.namespace_tree.root_hash)?;
    let package =
      build_package(request, namespace_root, &source_header, observation.selected.header.slot_sequence, admission.publication_sequence)?;
    if package.admission_control != admission_control {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_retry_collision",
        "selected admission bytes differ from the exact retry request",
      ));
    }
    verify_package_locators(&self.file, &kv, &package, &observation.selected.header)?;
    Ok(FirstAuthorityPublicationReceiptV1 {
      namespace_root: package.namespace_root,
      prepare_control: package.prepare_control,
      admission_control: package.admission_control,
      publication_sequence: admission.publication_sequence,
      observation,
      idempotent: true,
    })
  }

  fn lock_kv(&self) -> Result<MutexGuard<'_, DiskKVStore>, FirstAuthorityPublicationErrorV1> {
    match self.kv.lock() {
      Ok(kv) => Ok(kv),
      Err(poisoned) => {
        drop(poisoned);
        Err(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
      }
    }
  }
}

impl RetirementJournalDurableSinkV1 for V4FirstAuthorityPublisher {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    self.publish_retirement_journal_segment(segment, &mut observer)
  }
}

impl MarkMutationJournalDurableSinkV1 for V4FirstAuthorityPublisher {
  fn publish_mark_mutation_segment_synced(
    &mut self,
    segment: &PreparedMarkMutationJournalSegmentV1<'_>,
  ) -> Result<MarkMutationJournalDurabilityReceiptV1, MarkMutationJournalSinkErrorV1> {
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    self.publish_mark_mutation_journal_segment(segment, &mut observer)
  }
}

impl CorruptGcEvidenceDurableSinkV1 for V4FirstAuthorityPublisher {
  fn publish_corrupt_evidence_synced(
    &mut self,
    artifact_key: &[u8],
    value: &[u8],
  ) -> Result<CorruptGcEvidenceDurabilityReceiptV1, CorruptGcEvidenceSinkErrorV1> {
    let observation = self.observe().map_err(corrupt_evidence_sink_first_authority_error)?;
    let algorithm = observation.selected.header.hash_algorithm;
    let AuditArtifactV1::CorruptEvidence(evidence) = decode_audit_artifact(value, algorithm).map_err(corrupt_evidence_sink_format_error)?
    else {
      return Err(corrupt_evidence_sink_invalid(
        "corrupt_gc_evidence_kind",
        "corrupt-evidence sink received another immutable GC artifact kind",
      ));
    };
    if evidence.key != artifact_key {
      return Err(corrupt_evidence_sink_invalid("corrupt_gc_evidence_key", "corrupt-evidence key does not bind the exact immutable value"));
    }
    let database_id: [u8; 16] = evidence.database_id.try_into().map_err(|_| {
      corrupt_evidence_sink_invalid("corrupt_gc_evidence_database", "corrupt-evidence database identity has the wrong width")
    })?;
    let minimum_timestamp_ms = u64::try_from(evidence.detected_at_ms).map_err(|_| {
      corrupt_evidence_sink_invalid("corrupt_gc_evidence_timestamp", "corrupt-evidence detection time is outside the storage range")
    })?;
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    let hard_publication_sequence = self
      .publish_immutable_gc_artifact(
        ImmutableGcArtifactPublicationV1 {
          kind: GcArtifactKindV1::CorruptGcEvidence,
          database_id: &database_id,
          artifact_key,
          value,
          minimum_timestamp_ms,
          committed_postcondition_code: "immutable_gc_committed_postcondition",
        },
        &mut observer,
      )
      .map_err(corrupt_evidence_sink_first_authority_error)?;
    let stored_value_length =
      u32::try_from(value.len()).map_err(|error| CorruptGcEvidenceSinkErrorV1::new("corrupt_gc_evidence_value_length", error))?;
    Ok(CorruptGcEvidenceDurabilityReceiptV1 { artifact_key: artifact_key.to_vec(), stored_value_length, hard_publication_sequence })
  }
}

fn prepare_namespace_root(
  request: &FirstAuthorityPublicationRequestV1,
  algorithm: HashAlgorithm,
  write_sequence_high_water: u64,
) -> Result<EncodedNamespaceRootV1, FirstAuthorityPublicationErrorV1> {
  if request.namespace_tree.stored_value.len() > FIRST_AUTHORITY_NAMESPACE_TREE_CAP {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_namespace_tree_exceeds_cap",
      format!(
        "namespace tree is {} bytes, exceeding the {FIRST_AUTHORITY_NAMESPACE_TREE_CAP}-byte first-authority cap",
        request.namespace_tree.stored_value.len()
      ),
    ));
  }
  let semantic = decode_semantic_object(&request.semantic_state.value, algorithm)?;
  let semantic_cap = super::semantic_store::semantic_object_cap(semantic.kind_id)?;
  if request.semantic_state.value.len() > semantic_cap {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_semantic_state_exceeds_cap",
      format!("semantic state is {} bytes, exceeding its {semantic_cap}-byte kind cap", request.semantic_state.value.len()),
    ));
  }
  if semantic.object_id != request.semantic_state.object_id || !matches!(semantic.kind, SemanticObjectKind::State { .. }) {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_semantic_state_mismatch",
      "semantic-state bytes do not match their state identity",
    ));
  }
  let tree_sequence = write_sequence_high_water
    .checked_add(1)
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_write_sequence_exhausted", "write sequence exhausted"))?;
  let tree_entity = encode_entity(
    EntryTypeV4::DirectoryIndex,
    0,
    algorithm,
    request.created_at_ms,
    tree_sequence,
    &request.namespace_tree.root_hash,
    &request.namespace_tree.stored_value,
  )?;
  decode_namespace_tree_root_v0(&tree_entity, &request.namespace_tree.root_hash, algorithm, tree_sequence)?;
  encode_namespace_root(
    &NamespaceRootWriteV1 {
      required_capabilities: request.required_capabilities,
      namespace_tree_root: request.namespace_tree.root_hash.clone(),
      semantic_state_root: request.semantic_state.object_id.clone(),
    },
    algorithm,
  )
  .map_err(Into::into)
}

fn refuse_existing_entities(kv: &DiskKVStore, entities: &[PreparedWholeEntityV1]) -> Result<(), FirstAuthorityPublicationErrorV1> {
  for entity in entities {
    if kv.get(&entity.key)?.is_some() {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_identity_collision",
        format!("first-authority identity {} already exists before root admission", hex::encode(&entity.key)),
      ));
    }
  }
  Ok(())
}

fn package_dependency_bytes(package: &FirstAuthorityPackageV1, hash_length: usize) -> Result<u64, FirstAuthorityPublicationErrorV1> {
  entity_dependency_bytes(&package.entities, hash_length)
}

fn entity_dependency_bytes(entities: &[PreparedWholeEntityV1], hash_length: usize) -> Result<u64, FirstAuthorityPublicationErrorV1> {
  let entity_bytes = entities.iter().try_fold(0u64, |total, entity| {
    let length = match u64::try_from(entity.bytes.len()) {
      Ok(length) => length,
      Err(error) => {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "first_authority_package_size",
          format!("entity length exceeds u64: {error}"),
        ));
      }
    };
    total
      .checked_add(length)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_package_size", "entity byte total overflowed"))
  })?;
  let hot_tail_bytes = crate::engine::hot_tail::serialized_size(entities.len(), 0, hash_length)?;
  let hot_tail_bytes = match u64::try_from(hot_tail_bytes) {
    Ok(hot_tail_bytes) => hot_tail_bytes,
    Err(error) => {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_package_size",
        format!("hot-tail length exceeds u64: {error}"),
      ));
    }
  };
  entity_bytes
    .checked_add(hot_tail_bytes)
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_package_size", "dependency byte total overflowed"))
}

fn retirement_journal_receipt(
  segment: &PreparedRetirementJournalSegmentV1<'_>,
  hard_publication_sequence: u64,
) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
  let stored_value_length =
    u32::try_from(segment.value.len()).map_err(|error| RetirementJournalSinkErrorV1::new("retirement_journal_value_length", error))?;
  if hard_publication_sequence == 0 {
    return Err(retirement_sink_invalid(
      "retirement_journal_publication_sequence",
      "retirement-journal entity has no durable v4 write sequence",
    ));
  }
  Ok(RetirementJournalDurabilityReceiptV1 { artifact_key: segment.artifact_key.to_vec(), stored_value_length, hard_publication_sequence })
}

fn mark_mutation_journal_receipt(
  segment: &PreparedMarkMutationJournalSegmentV1<'_>,
  hard_publication_sequence: u64,
) -> Result<MarkMutationJournalDurabilityReceiptV1, MarkMutationJournalSinkErrorV1> {
  let stored_value_length =
    u32::try_from(segment.value.len()).map_err(|error| MarkMutationJournalSinkErrorV1::new("mark_mutation_value_length", error))?;
  if hard_publication_sequence == 0 {
    return Err(mark_mutation_sink_invalid("mark_mutation_publication_sequence", "mark-mutation entity has no durable v4 write sequence"));
  }
  Ok(MarkMutationJournalDurabilityReceiptV1 { artifact_key: segment.artifact_key.to_vec(), stored_value_length, hard_publication_sequence })
}

fn mark_mutation_sink_invalid(code: &'static str, message: &'static str) -> MarkMutationJournalSinkErrorV1 {
  MarkMutationJournalSinkErrorV1::new(code, FirstAuthorityPublicationErrorV1::invalid(code, message))
}

fn mark_mutation_sink_format_error(error: FormatError) -> MarkMutationJournalSinkErrorV1 {
  MarkMutationJournalSinkErrorV1::new(error.code(), error)
}

fn mark_mutation_sink_first_authority_error(error: FirstAuthorityPublicationErrorV1) -> MarkMutationJournalSinkErrorV1 {
  let code = match error.code() {
    "immutable_gc_degraded_header" => "mark_mutation_degraded_header",
    "immutable_gc_missing_authority" => "mark_mutation_missing_authority",
    "immutable_gc_database_mismatch" => "mark_mutation_database_mismatch",
    "immutable_gc_prepared_mismatch" => "mark_mutation_prepared_mismatch",
    "immutable_gc_identity_collision" => "mark_mutation_identity_collision",
    "immutable_gc_readback_missing" => "mark_mutation_readback_missing",
    "immutable_gc_write_sequence_exhausted" => "mark_mutation_write_sequence_exhausted",
    "immutable_gc_entry_count_overflow" => "mark_mutation_entry_count_overflow",
    "immutable_gc_baseline_not_flushed" => "mark_mutation_baseline_not_flushed",
    "immutable_gc_file_truncated" => "mark_mutation_file_truncated",
    "immutable_gc_wal_overflow" => "mark_mutation_wal_overflow",
    "immutable_gc_dependency_missing" => "mark_mutation_dependency_missing",
    "immutable_gc_readback_mismatch" => "mark_mutation_readback_mismatch",
    "immutable_gc_committed_postcondition" => "mark_mutation_committed_postcondition",
    "first_authority_lock_poisoned" => "mark_mutation_authority_lock",
    "engine_failure" => "mark_mutation_storage",
    code => code,
  };
  MarkMutationJournalSinkErrorV1::new(code, error)
}

fn retirement_sink_invalid(code: &'static str, message: &'static str) -> RetirementJournalSinkErrorV1 {
  RetirementJournalSinkErrorV1::new(code, FirstAuthorityPublicationErrorV1::invalid(code, message))
}

fn retirement_sink_format_error(error: FormatError) -> RetirementJournalSinkErrorV1 {
  RetirementJournalSinkErrorV1::new(error.code(), error)
}

fn retirement_sink_first_authority_error(error: FirstAuthorityPublicationErrorV1) -> RetirementJournalSinkErrorV1 {
  let code = match error.code() {
    "immutable_gc_degraded_header" => "retirement_journal_degraded_header",
    "immutable_gc_missing_authority" => "retirement_journal_missing_authority",
    "immutable_gc_database_mismatch" => "retirement_journal_database_mismatch",
    "immutable_gc_prepared_mismatch" => "retirement_journal_prepared_mismatch",
    "immutable_gc_identity_collision" => "retirement_journal_identity_collision",
    "immutable_gc_readback_missing" => "retirement_journal_readback_missing",
    "immutable_gc_write_sequence_exhausted" => "retirement_journal_write_sequence_exhausted",
    "immutable_gc_entry_count_overflow" => "retirement_journal_entry_count_overflow",
    "immutable_gc_baseline_not_flushed" => "retirement_journal_baseline_not_flushed",
    "immutable_gc_file_truncated" => "retirement_journal_file_truncated",
    "immutable_gc_wal_overflow" => "retirement_journal_wal_overflow",
    "immutable_gc_dependency_missing" => "retirement_journal_dependency_missing",
    "immutable_gc_readback_mismatch" => "retirement_journal_readback_mismatch",
    "immutable_gc_committed_postcondition" => "retirement_journal_committed_postcondition",
    "first_authority_lock_poisoned" => "retirement_journal_authority_lock",
    "engine_failure" => "retirement_journal_storage",
    code => code,
  };
  RetirementJournalSinkErrorV1::new(code, error)
}

fn corrupt_evidence_sink_invalid(code: &'static str, message: &'static str) -> CorruptGcEvidenceSinkErrorV1 {
  CorruptGcEvidenceSinkErrorV1::new(code, FirstAuthorityPublicationErrorV1::invalid(code, message))
}

fn corrupt_evidence_sink_format_error(error: FormatError) -> CorruptGcEvidenceSinkErrorV1 {
  CorruptGcEvidenceSinkErrorV1::new(error.code(), error)
}

fn corrupt_evidence_sink_first_authority_error(error: FirstAuthorityPublicationErrorV1) -> CorruptGcEvidenceSinkErrorV1 {
  let code = error.code();
  CorruptGcEvidenceSinkErrorV1::new(code, error)
}

fn build_package(
  request: &FirstAuthorityPublicationRequestV1,
  namespace_root: EncodedNamespaceRootV1,
  source_header: &DatabaseHeaderV4,
  selected_header_slot_sequence: u64,
  publication_sequence: u64,
) -> Result<FirstAuthorityPackageV1, FirstAuthorityPublicationErrorV1> {
  let algorithm = source_header.hash_algorithm;
  let timestamp_i64 = match i64::try_from(request.created_at_ms) {
    Ok(timestamp) => timestamp,
    Err(error) => {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_timestamp_range",
        format!("timestamp exceeds signed v1 control range: {error}"),
      ));
    }
  };
  let authority_identity_digest = digest_parts(algorithm, &[&request.authority_identity]);
  let zero_hash = vec![0; algorithm.hash_length()];
  let prepare = RootPublicationPrepareV1 {
    database_id: request.database_id,
    transaction_id: request.transaction_id,
    created_at_ms: timestamp_i64,
    target_namespace_root: namespace_root.root_hash.clone(),
    target_semantic_state: request.semantic_state.object_id.clone(),
    typed_closure_digest: request.typed_closure_digest.clone(),
    authority_kind: RootAuthorityKindV1::Head,
    authority_identity: request.authority_identity.clone(),
    expected_authority_before: zero_hash,
    expected_authority_after: namespace_root.root_hash.clone(),
    intended_header_slot_sequence: selected_header_slot_sequence,
    intended_publication_sequence: publication_sequence,
  };
  let prepare_control = encode_root_publication_prepare_control(&prepare, algorithm)?;
  let admission = RootAdmissionCommitV1 {
    database_id: request.database_id,
    namespace_root: namespace_root.root_hash.clone(),
    transaction_id: request.transaction_id,
    publication_started_at_ms: timestamp_i64,
    authority_kind: RootAuthorityKindV1::Head,
    recovered_from_selected_authority: false,
    authority_identity_digest,
    authority_after: namespace_root.root_hash.clone(),
    selected_header_slot_sequence,
    publication_sequence,
    prepare_payload_hash: digest_parts(algorithm, &[&prepare_control]),
  };
  let admission_control = encode_root_admission_commit_control(&admission, algorithm)?;

  let mut next_sequence = source_header.write_sequence_high_water;
  let mut entities = Vec::with_capacity(FIRST_AUTHORITY_ENTITY_COUNT);
  next_sequence = append_entity(
    &mut entities,
    EntryTypeV4::DirectoryIndex,
    0,
    KV_TYPE_DIRECTORY,
    algorithm,
    request.created_at_ms,
    next_sequence,
    &request.namespace_tree.root_hash,
    &request.namespace_tree.stored_value,
  )?;
  next_sequence = append_system_file(
    &mut entities,
    semantic_object_path(algorithm, 1, &request.semantic_state.object_id)?,
    SEMANTIC_OBJECT_CONTENT_TYPE,
    &request.semantic_state.value,
    algorithm,
    request.created_at_ms,
    next_sequence,
  )?;
  next_sequence = append_entity(
    &mut entities,
    EntryTypeV4::DirectoryIndex,
    WHOLE_ENTITY_V1_FLAG_SYSTEM,
    KV_TYPE_DIRECTORY,
    algorithm,
    request.created_at_ms,
    next_sequence,
    &namespace_root.root_hash,
    &namespace_root.value,
  )?;
  let prepare_path =
    system_control_path(SystemControlKindV1::RootPublicationPrepare, &request.transaction_id, SystemControlSlotV1::Immutable)?;
  next_sequence = append_system_file(
    &mut entities,
    prepare_path,
    SYSTEM_CONTROL_CONTENT_TYPE,
    &prepare_control,
    algorithm,
    request.created_at_ms,
    next_sequence,
  )?;
  let admission_path =
    system_control_path(SystemControlKindV1::RootAdmissionCommit, &namespace_root.root_hash, SystemControlSlotV1::Immutable)?;
  next_sequence = append_system_file(
    &mut entities,
    admission_path,
    SYSTEM_CONTROL_CONTENT_TYPE,
    &admission_control,
    algorithm,
    request.created_at_ms,
    next_sequence,
  )?;
  if entities.len() != FIRST_AUTHORITY_ENTITY_COUNT {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_entity_count",
      format!("constructed {} entities, expected {FIRST_AUTHORITY_ENTITY_COUNT}", entities.len()),
    ));
  }
  let mut identities = HashSet::with_capacity(entities.len());
  if entities.iter().any(|entity| !identities.insert(entity.key.clone())) {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_duplicate_identity",
      "first-authority entities contain a duplicate KV identity",
    ));
  }
  let hot_tail_offset = entities.iter().try_fold(source_header.hot_tail_offset, |offset, entity| {
    offset
      .checked_add(entity.bytes.len() as u64)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_wal_overflow", "WAL offset overflow"))
  })?;
  Ok(FirstAuthorityPackageV1 {
    namespace_root,
    prepare_control,
    admission_control,
    entities,
    hot_tail_offset,
    write_sequence_high_water: next_sequence,
  })
}

#[allow(clippy::too_many_arguments)]
fn append_entity(
  entities: &mut Vec<PreparedWholeEntityV1>,
  entry_type: EntryTypeV4,
  flags: u8,
  kv_type: u8,
  algorithm: HashAlgorithm,
  timestamp_ms: u64,
  previous_sequence: u64,
  key: &[u8],
  stored_value: &[u8],
) -> Result<u64, FirstAuthorityPublicationErrorV1> {
  let write_sequence = previous_sequence
    .checked_add(1)
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_write_sequence_exhausted", "write sequence exhausted"))?;
  let bytes = encode_entity(entry_type, flags, algorithm, timestamp_ms, write_sequence, key, stored_value)?;
  entities.push(PreparedWholeEntityV1 { key: key.to_vec(), kv_type, bytes });
  Ok(write_sequence)
}

#[allow(clippy::too_many_arguments)]
fn append_system_file(
  entities: &mut Vec<PreparedWholeEntityV1>,
  path: String,
  content_type: &str,
  body: &[u8],
  algorithm: HashAlgorithm,
  timestamp_ms: u64,
  previous_sequence: u64,
) -> Result<u64, FirstAuthorityPublicationErrorV1> {
  let chunk_key = first_authority_system_chunk_hash(body, algorithm);
  let sequence = append_entity(
    entities,
    EntryTypeV4::Chunk,
    WHOLE_ENTITY_V1_FLAG_SYSTEM,
    KV_TYPE_CHUNK,
    algorithm,
    timestamp_ms,
    previous_sequence,
    &chunk_key,
    body,
  )?;
  let timestamp_i64 = match i64::try_from(timestamp_ms) {
    Ok(timestamp) => timestamp,
    Err(error) => {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_timestamp_range",
        format!("timestamp exceeds FileRecord range: {error}"),
      ));
    }
  };
  let record = FileRecord {
    path: path.clone(),
    content_type: Some(content_type.to_string()),
    total_size: body.len() as u64,
    created_at: timestamp_i64,
    updated_at: timestamp_i64,
    metadata: Vec::new(),
    content_hash: first_authority_content_hash(body, algorithm),
    chunk_hashes: vec![chunk_key],
  };
  let value = record.serialize(algorithm.hash_length())?;
  let path_key = first_authority_file_path_hash(&path, algorithm);
  append_entity(
    entities,
    EntryTypeV4::FileRecord,
    WHOLE_ENTITY_V1_FLAG_SYSTEM,
    KV_TYPE_FILE_RECORD,
    algorithm,
    timestamp_ms,
    sequence,
    &path_key,
    &value,
  )
}

fn encode_entity(
  entry_type: EntryTypeV4,
  flags: u8,
  algorithm: HashAlgorithm,
  timestamp_ms: u64,
  write_sequence: u64,
  key: &[u8],
  stored_value: &[u8],
) -> Result<Vec<u8>, FirstAuthorityPublicationErrorV1> {
  encode_whole_entity(&WholeEntityWriteV1 {
    entry_type,
    flags,
    hash_algorithm: algorithm,
    compression_algorithm: CompressionAlgorithm::None,
    timestamp_ms,
    write_sequence,
    key,
    stored_value,
  })
  .map_err(Into::into)
}

fn first_authority_file_path_hash(path: &str, algorithm: HashAlgorithm) -> Vec<u8> {
  digest_parts(algorithm, &[b"file:", path.as_bytes()])
}

fn first_authority_system_chunk_hash(body: &[u8], algorithm: HashAlgorithm) -> Vec<u8> {
  digest_parts(algorithm, &[b"system::", body])
}

fn first_authority_content_hash(body: &[u8], algorithm: HashAlgorithm) -> Vec<u8> {
  digest_parts(algorithm, &[body])
}

struct FirstAuthorityDependencyV1<'a> {
  file: &'a File,
  kv: &'a mut DiskKVStore,
  batch: crate::engine::disk_kv_store::AtomicKvVisibilityBatch,
  expected_publication_sequence: u64,
  entities: &'a [PreparedWholeEntityV1],
  start_offset: u64,
  expected_hot_tail_offset: u64,
  expected_existing: Option<&'a KVEntry>,
  prewritten: bool,
  append_completed: bool,
  observer: &'a mut dyn FirstAuthorityDependencyObserverV1,
}

impl HeaderPublicationDependencyV4 for FirstAuthorityDependencyV1<'_> {
  fn append_dependency(&mut self, publication_sequence: u64) -> Result<(), NativeDurabilityError> {
    if publication_sequence != self.expected_publication_sequence {
      return Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        "first-authority dependency received another publication sequence",
      ));
    }
    for entity in self.entities {
      let observed = self.kv.get(&entity.key).map_err(native_engine_error)?;
      if observed.as_ref() != self.expected_existing {
        return Err(NativeDurabilityError::invalid(
          NativeDurabilityOperation::WriteAt,
          format!("first-authority identity {} changed before dependency activation", hex::encode(&entity.key)),
        ));
      }
    }

    let mut offset = self.start_offset;
    for (index, entity) in self.entities.iter().enumerate() {
      self.observer.before_entity(index, entity)?;
      if !self.prewritten {
        write_file_at_native(self.file, offset, &entity.bytes)?;
      }
      verify_file_bytes_native(self.file, offset, &entity.bytes)?;
      self.observer.entity_written(index, entity)?;
      let total_length = match u32::try_from(entity.bytes.len()) {
        Ok(total_length) => total_length,
        Err(error) => {
          return Err(NativeDurabilityError::invalid(
            NativeDurabilityOperation::WriteAt,
            format!("first-authority entity length exceeds u32: {error}"),
          ));
        }
      };
      self
        .kv
        .stage_atomic_visibility_entry(self.batch, KVEntry { type_flags: entity.kv_type, hash: entity.key.clone(), offset, total_length })
        .map_err(native_engine_error)?;
      self.observer.entity_staged(index, entity)?;
      offset = offset
        .checked_add(entity.bytes.len() as u64)
        .ok_or_else(|| NativeDurabilityError::invalid(NativeDurabilityOperation::WriteAt, "first-authority WAL offset overflow"))?;
    }
    if offset != self.expected_hot_tail_offset {
      return Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        "first-authority append ended at an unexpected hot-tail offset",
      ));
    }
    self.kv.set_hot_tail_offset(offset);
    let wrote_hot_tail = self
      .kv
      .prepare_hot_tail_dependency(true)
      .map_err(|error| NativeDurabilityError::from_io(NativeDurabilityOperation::WriteAt, error))?;
    if !wrote_hot_tail {
      return Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        "first-authority KV dependency did not write its hot tail",
      ));
    }
    self.observer.staged(self.kv, self.entities)?;
    self.append_completed = true;
    Ok(())
  }
}

fn native_engine_error(error: EngineError) -> NativeDurabilityError {
  NativeDurabilityError::invalid(NativeDurabilityOperation::WriteAt, error.to_string())
}

fn validate_mark_run_checkpoint_publication<'a>(
  request: &'a MarkRunCheckpointPublicationRequestV1<'a>,
) -> Result<(Box<super::gc_mark::MarkRunCheckpointV1<'a>>, GcActiveControlV1<'a>), MarkRunCheckpointPublicationErrorV1> {
  if request.publication_timestamp_ms == 0 || request.monotonic_now_ms == 0 {
    return Err(MarkRunCheckpointPublicationErrorV1::invalid(
      "mark_checkpoint_publication_time",
      "mark checkpoint publication and monotonic times must be nonzero",
    ));
  }
  let GcMarkArtifactV1::Checkpoint(checkpoint) = decode_gc_mark_artifact(&request.checkpoint.value, request.hash_algorithm)? else {
    return Err(MarkRunCheckpointPublicationErrorV1::invalid(
      "mark_checkpoint_artifact_kind",
      "prepared mark checkpoint is not a checkpoint artifact",
    ));
  };
  if request.publication_timestamp_ms < checkpoint.updated_at_ms {
    return Err(MarkRunCheckpointPublicationErrorV1::invalid(
      "mark_checkpoint_publication_time",
      "mark checkpoint publication time predates the completed checkpoint",
    ));
  }
  if checkpoint.key != request.checkpoint.key || !checkpoint.resumable || checkpoint.canceled || checkpoint.state != 1 {
    return Err(MarkRunCheckpointPublicationErrorV1::invalid(
      "mark_checkpoint_artifact_state",
      "mark checkpoint key or complete resumable state is invalid",
    ));
  }
  let workspace_path = request.workspace.checkpoint_workspace_path().map_err(|error| {
    MarkRunCheckpointPublicationErrorV1::invalid("mark_checkpoint_workspace_path", format!("durable workspace path is invalid: {error}"))
  })?;
  if checkpoint.workspace_path != workspace_path || checkpoint.workspace_manifest_digest != request.workspace.manifest_digest() {
    return Err(MarkRunCheckpointPublicationErrorV1::invalid(
      "mark_checkpoint_workspace_closure",
      "mark checkpoint path or manifest digest differs from the durable workspace closure",
    ));
  }
  let control = decode_gc_active_control(&request.control.value, request.hash_algorithm)?;
  if control.key != request.control.key
    || control.kind != GcArtifactKindV1::MarkRunActiveControl
    || control.database_id != checkpoint.database_id
    || control.generation != checkpoint.generation
    || control.target_manifest_hash != request.checkpoint.key
  {
    return Err(MarkRunCheckpointPublicationErrorV1::invalid(
      "mark_checkpoint_control_closure",
      "mark control key, kind, database, generation, or checkpoint target disagrees",
    ));
  }
  Ok((checkpoint, control))
}

fn mark_selection_diagnostic(slot: u8, code: &'static str, message: impl Into<String>) -> MarkRunCheckpointSlotDiagnosticV1 {
  MarkRunCheckpointSlotDiagnosticV1 { slot, code, message: message.into() }
}

fn load_mark_run_control_for_selection(
  file: &File,
  kv: &DiskKVStore,
  key: &[u8],
  slot: u8,
  header: &DatabaseHeaderV4,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<LoadedMarkRunControlSlotV1, MarkRunCheckpointSelectionErrorV1> {
  let Some(locator) = kv.get(key).map_err(FirstAuthorityPublicationErrorV1::from)? else {
    return Ok(LoadedMarkRunControlSlotV1::Absent);
  };
  if locator.type_flags != kv_tag::GC_ARTIFACT {
    return Ok(LoadedMarkRunControlSlotV1::Invalid(mark_selection_diagnostic(
      slot,
      "mark_checkpoint_control_collision",
      "mark control key resolves to another KV role",
    )));
  }
  let length = match usize::try_from(locator.total_length) {
    Ok(length) => length,
    Err(source) => {
      return Ok(LoadedMarkRunControlSlotV1::Invalid(mark_selection_diagnostic(
        slot,
        "mark_checkpoint_control_length",
        format!("mark control locator length exceeds usize: {source}"),
      )));
    }
  };
  if length > FIRST_AUTHORITY_CONTROL_ENTITY_CAP {
    return Ok(LoadedMarkRunControlSlotV1::Invalid(mark_selection_diagnostic(
      slot,
      "mark_checkpoint_control_length",
      format!("mark control entity length {length} exceeds {FIRST_AUTHORITY_CONTROL_ENTITY_CAP}"),
    )));
  }
  let entity = read_selection_entity(file, &locator, length, "mark control", cancellation, memory)?;
  let write_sequence = {
    let decoded = match decode_whole_entity(&entity.bytes, header.hash_algorithm, header.write_sequence_high_water) {
      Ok(decoded) => decoded,
      Err(source) => {
        return Ok(LoadedMarkRunControlSlotV1::Invalid(mark_selection_diagnostic(slot, source.code(), source.to_string())));
      }
    };
    if decoded.entry_type != EntryTypeV4::GcArtifact
      || decoded.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
      || decoded.compression_algorithm != CompressionAlgorithm::None
      || decoded.key != key
    {
      return Ok(LoadedMarkRunControlSlotV1::Invalid(mark_selection_diagnostic(
        slot,
        "mark_checkpoint_control_representation",
        "stored mark control is not one canonical system GC WholeEntity",
      )));
    }
    let control = match decode_gc_active_control(decoded.stored_value, header.hash_algorithm) {
      Ok(control) => control,
      Err(source) => {
        return Ok(LoadedMarkRunControlSlotV1::Invalid(mark_selection_diagnostic(slot, source.code(), source.to_string())));
      }
    };
    if control.kind != GcArtifactKindV1::MarkRunActiveControl
      || control.database_id != header.database_id
      || control.slot != slot
      || control.key != key
    {
      return Ok(LoadedMarkRunControlSlotV1::Invalid(mark_selection_diagnostic(
        slot,
        "mark_checkpoint_control_representation",
        "stored GC entity is not the canonical mark-run control for its database and slot",
      )));
    }
    decoded.write_sequence
  };
  Ok(LoadedMarkRunControlSlotV1::Valid(LoadedMarkRunControlV1 { entity, write_sequence }))
}

#[allow(clippy::too_many_arguments)]
fn load_mark_run_checkpoint_for_selection(
  file: &File,
  kv: &DiskKVStore,
  control: &LoadedMarkRunControlV1,
  slot: u8,
  header: &DatabaseHeaderV4,
  resume_contexts: &[MarkResumeContextV1<'_>],
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<Result<LoadedMarkRunCheckpointV1, MarkRunCheckpointSlotDiagnosticV1>, MarkRunCheckpointSelectionErrorV1> {
  let control = decode_loaded_mark_control(control, header)
    .map_err(|source| MarkRunCheckpointSelectionErrorV1::invalid(source.code(), source.to_string()))?;
  let Some(locator) = kv.get(control.target_manifest_hash).map_err(FirstAuthorityPublicationErrorV1::from)? else {
    return Ok(Err(mark_selection_diagnostic(slot, "mark_checkpoint_entity_missing", "mark control target checkpoint is absent")));
  };
  if locator.type_flags != kv_tag::GC_ARTIFACT {
    return Ok(Err(mark_selection_diagnostic(slot, "mark_checkpoint_entity_collision", "mark checkpoint key resolves to another KV role")));
  }
  let maximum_entity_length = super::entity::checked_whole_entity_encoded_length(
    header.hash_algorithm,
    control.target_manifest_hash.len(),
    MARK_CHECKPOINT_VALUE_MAX,
  )
  .map_err(|source| MarkRunCheckpointSelectionErrorV1::invalid(source.code(), source.to_string()))?;
  let length = match usize::try_from(locator.total_length) {
    Ok(length) => length,
    Err(source) => {
      return Ok(Err(mark_selection_diagnostic(
        slot,
        "mark_checkpoint_entity_length",
        format!("mark checkpoint locator length exceeds usize: {source}"),
      )));
    }
  };
  if length > maximum_entity_length {
    return Ok(Err(mark_selection_diagnostic(
      slot,
      "mark_checkpoint_entity_length",
      format!("mark checkpoint entity length {length} exceeds {maximum_entity_length}"),
    )));
  }
  let entity = read_selection_entity(file, &locator, length, "mark checkpoint", cancellation, memory)?;
  let (key, write_sequence, resume_context_index) = {
    let decoded = match decode_whole_entity(&entity.bytes, header.hash_algorithm, header.write_sequence_high_water) {
      Ok(decoded) => decoded,
      Err(source) => return Ok(Err(mark_selection_diagnostic(slot, source.code(), source.to_string()))),
    };
    if decoded.entry_type != EntryTypeV4::GcArtifact
      || decoded.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
      || decoded.compression_algorithm != CompressionAlgorithm::None
      || decoded.key != control.target_manifest_hash
    {
      return Ok(Err(mark_selection_diagnostic(
        slot,
        "mark_checkpoint_entity_representation",
        "stored mark checkpoint is not one canonical system GC WholeEntity",
      )));
    }
    let checkpoint = match decode_gc_mark_artifact(decoded.stored_value, header.hash_algorithm) {
      Ok(GcMarkArtifactV1::Checkpoint(checkpoint)) => checkpoint,
      Ok(GcMarkArtifactV1::MutationJournal(_)) => {
        return Ok(Err(mark_selection_diagnostic(
          slot,
          "mark_checkpoint_entity_kind",
          "mark control target is a mutation journal rather than a checkpoint",
        )));
      }
      Err(source) => return Ok(Err(mark_selection_diagnostic(slot, source.code(), source.to_string()))),
    };
    if checkpoint.key != control.target_manifest_hash
      || checkpoint.database_id != control.database_id
      || checkpoint.generation != control.generation
    {
      return Ok(Err(mark_selection_diagnostic(
        slot,
        "mark_checkpoint_control_closure",
        "mark checkpoint key, database, or generation does not close against its control",
      )));
    }
    let mut resume_context_index = None;
    let mut mismatch_details = Vec::new();
    for (index, context) in resume_contexts.iter().enumerate() {
      if context.hash_algorithm != header.hash_algorithm {
        mismatch_details.push(format!("context {index}: hash profile mismatch"));
        continue;
      }
      match validate_mark_checkpoint_resume_context(&checkpoint, context) {
        Ok(()) => {
          resume_context_index = Some(index);
          break;
        }
        Err(source) => mismatch_details.push(format!("context {index}: {}: {}", source.code(), source)),
      }
    }
    let Some(resume_context_index) = resume_context_index else {
      return Ok(Err(mark_selection_diagnostic(
        slot,
        "mark_checkpoint_resume_context",
        format!("mark checkpoint does not match any exact expected resume context: {}", mismatch_details.join("; ")),
      )));
    };
    (checkpoint.key.clone(), decoded.write_sequence, resume_context_index)
  };
  Ok(Ok(LoadedMarkRunCheckpointV1 { entity, key, write_sequence, resume_context_index }))
}

fn read_selection_entity(
  file: &File,
  locator: &KVEntry,
  length: usize,
  role: &'static str,
  cancellation: &CancellationToken,
  memory: &MemoryCoordinator,
) -> Result<ChargedSelectionEntityV1, MarkRunCheckpointSelectionErrorV1> {
  if cancellation.is_cancelled() {
    return Err(MarkRunCheckpointSelectionErrorV1::invalid(
      "mark_checkpoint_selection_cancelled",
      format!("mark checkpoint selection was canceled before reading {role}"),
    ));
  }
  let reservation_bytes = u64::try_from(length).map_err(|source| {
    MarkRunCheckpointSelectionErrorV1::invalid(
      "mark_checkpoint_selection_allocation",
      format!("{role} length exceeds u64 memory accounting: {source}"),
    )
  })?;
  let reservation = memory.reserve(MemoryOwner::GarbageCollection, reservation_bytes, AdmissionClass::Maintenance)?;
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(length).map_err(|source| {
    MarkRunCheckpointSelectionErrorV1::invalid("mark_checkpoint_selection_allocation", format!("failed to allocate {role}: {source}"))
  })?;
  bytes.resize(length, 0);
  read_file_at_native(file, locator.offset, &mut bytes).map_err(|source| {
    MarkRunCheckpointSelectionErrorV1::Authority(FirstAuthorityPublicationErrorV1::invalid(
      "mark_checkpoint_selection_read",
      format!("failed to read {role}: {source}"),
    ))
  })?;
  if cancellation.is_cancelled() {
    return Err(MarkRunCheckpointSelectionErrorV1::invalid(
      "mark_checkpoint_selection_cancelled",
      format!("mark checkpoint selection was canceled after reading {role}"),
    ));
  }
  Ok(ChargedSelectionEntityV1 { bytes, _memory: reservation })
}

fn decode_loaded_mark_control<'a>(
  loaded: &'a LoadedMarkRunControlV1,
  header: &DatabaseHeaderV4,
) -> Result<GcActiveControlV1<'a>, FormatError> {
  let entity = decode_whole_entity(&loaded.entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  decode_gc_active_control(entity.stored_value, header.hash_algorithm)
}

fn decode_loaded_mark_checkpoint<'a>(
  loaded: &'a LoadedMarkRunCheckpointV1,
  header: &DatabaseHeaderV4,
) -> Result<Box<super::gc_mark::MarkRunCheckpointV1<'a>>, FormatError> {
  let entity = decode_whole_entity(&loaded.entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  let GcMarkArtifactV1::Checkpoint(checkpoint) = decode_gc_mark_artifact(entity.stored_value, header.hash_algorithm)? else {
    return Err(FormatError::new(
      super::reader::MalformedInputClass::UnknownTypeKindOrEnum,
      "mark_checkpoint_selected_kind",
      "selected mark checkpoint entity contains another mark artifact kind",
    ));
  };
  Ok(checkpoint)
}

fn classify_workspace_selection_error(
  slot: u8,
  source: MarkWorkspaceErrorV1,
) -> Result<MarkRunCheckpointSlotDiagnosticV1, MarkRunCheckpointSelectionErrorV1> {
  let code = source.code();
  match source {
    MarkWorkspaceErrorV1::Canceled => Err(MarkRunCheckpointSelectionErrorV1::invalid(
      "mark_checkpoint_selection_cancelled",
      "mark checkpoint workspace validation was canceled",
    )),
    MarkWorkspaceErrorV1::Memory(source) => Err(MarkRunCheckpointSelectionErrorV1::Memory(*source)),
    source @ (MarkWorkspaceErrorV1::MemoryRollback { .. }
    | MarkWorkspaceErrorV1::Allocation(_)
    | MarkWorkspaceErrorV1::Io { .. }
    | MarkWorkspaceErrorV1::Durability(_)) => Err(MarkRunCheckpointSelectionErrorV1::Workspace(source)),
    source @ (MarkWorkspaceErrorV1::Identity(_)
    | MarkWorkspaceErrorV1::Path(_)
    | MarkWorkspaceErrorV1::State(_)
    | MarkWorkspaceErrorV1::Capacity(_)
    | MarkWorkspaceErrorV1::Format(_)) => Ok(mark_selection_diagnostic(slot, code, source.to_string())),
  }
}

fn gc_control_keys(
  algorithm: HashAlgorithm,
  kind: GcArtifactKindV1,
  database_id: &[u8; 16],
) -> Result<[Vec<u8>; 2], GcControlPublicationErrorV1> {
  Ok([gc_active_control_key(algorithm, kind, database_id, 0)?, gc_active_control_key(algorithm, kind, database_id, 1)?])
}

fn load_gc_control_entity(
  file: &File,
  kv: &DiskKVStore,
  expected_kind: GcArtifactKindV1,
  key: &[u8],
  header: &DatabaseHeaderV4,
) -> Result<Option<StoredGcControlEntityV1>, GcControlPublicationErrorV1> {
  let Some(locator) = kv.get(key)? else {
    return Ok(None);
  };
  if locator.type_flags != kv_tag::GC_ARTIFACT {
    return Err(GcControlPublicationErrorV1::invalid(
      GcControlPublicationFailureV1::ControlCollision,
      "GC control key resolves to another KV role",
    ));
  }
  let entity_bytes = read_entity_bounded(file, kv, key, FIRST_AUTHORITY_CONTROL_ENTITY_CAP, header.write_sequence_high_water)?
    .ok_or_else(|| GcControlPublicationErrorV1::invalid(GcControlPublicationFailureV1::ControlMissing, "GC control locator disappeared"))?;
  let entity = decode_whole_entity(&entity_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  if entity.entry_type != EntryTypeV4::GcArtifact
    || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
    || entity.compression_algorithm != CompressionAlgorithm::None
    || entity.key != key
  {
    return Err(GcControlPublicationErrorV1::invalid(
      GcControlPublicationFailureV1::ControlRepresentation,
      "stored GC control is not one canonical system GC WholeEntity",
    ));
  }
  let control = decode_gc_active_control(entity.stored_value, header.hash_algorithm)?;
  if control.kind != expected_kind || control.key != key {
    return Err(GcControlPublicationErrorV1::invalid(
      GcControlPublicationFailureV1::ControlRepresentation,
      "stored GC entity is not the expected canonical control for its key",
    ));
  }
  let stored_value = entity.stored_value.to_vec();
  let write_sequence = entity.write_sequence;
  let integrity_hash = entity.integrity_hash.to_vec();
  let control_sequence = control.sequence;
  let generation = control.generation;
  let target_manifest_hash = control.target_manifest_hash.to_vec();
  Ok(Some(StoredGcControlEntityV1 {
    locator,
    entity_bytes,
    stored_value,
    control_sequence,
    generation,
    target_manifest_hash,
    write_sequence,
    integrity_hash,
  }))
}

fn required_gc_control_slot(controls: &[Option<StoredGcControlEntityV1>; 2]) -> Result<u8, GcControlPublicationErrorV1> {
  match (&controls[0], &controls[1]) {
    (None, None) => Ok(0),
    (Some(_), None) => Ok(1),
    (None, Some(_)) => Ok(0),
    (Some(a), Some(b)) => {
      if a.control_sequence == b.control_sequence && (a.generation != b.generation || a.target_manifest_hash != b.target_manifest_hash) {
        return Err(GcControlPublicationErrorV1::invalid(
          GcControlPublicationFailureV1::ControlAmbiguous,
          "equal A/B control sequences disagree on generation or target manifest",
        ));
      }
      if a.control_sequence < b.control_sequence {
        Ok(0)
      } else {
        Ok(1)
      }
    }
  }
}

fn encode_v4_physical_incarnation(
  algorithm: HashAlgorithm,
  logical_key: &[u8],
  integrity_hash: &[u8],
  wal_offset: u64,
  write_sequence: u64,
  entity_length: usize,
  entry_type: EntryTypeV4,
) -> Result<Vec<u8>, GcControlPublicationErrorV1> {
  if logical_key.len() != algorithm.hash_length()
    || integrity_hash.len() != algorithm.hash_length()
    || wal_offset == 0
    || write_sequence == 0
  {
    return Err(GcControlPublicationErrorV1::invalid(
      GcControlPublicationFailureV1::PhysicalIncarnation,
      "GC control incarnation identity, offset, or sequence is invalid",
    ));
  }
  let entity_length = u32::try_from(entity_length).map_err(|error| {
    GcControlPublicationErrorV1::invalid(GcControlPublicationFailureV1::PhysicalIncarnation, format!("entity length exceeds u32: {error}"))
  })?;
  let mut incarnation = Vec::with_capacity(24 + 2 * logical_key.len());
  incarnation.extend_from_slice(logical_key);
  incarnation.extend_from_slice(integrity_hash);
  incarnation.extend_from_slice(&wal_offset.to_le_bytes());
  incarnation.extend_from_slice(&write_sequence.to_le_bytes());
  incarnation.extend_from_slice(&entity_length.to_le_bytes());
  incarnation.push(entry_type.to_u8());
  incarnation.push(super::entity::WHOLE_ENTITY_V1_VERSION);
  incarnation.extend_from_slice(&[0, 0]);
  super::gc::decode_physical_incarnation(&incarnation, algorithm)?;
  Ok(incarnation)
}

#[allow(clippy::too_many_arguments)]
fn commit_gc_control_dependency(
  file: &File,
  kv: &mut DiskKVStore,
  admitted: super::header_publication::AdmittedDatabaseHeaderPublicationV4<'_>,
  batch: crate::engine::disk_kv_store::AtomicKvVisibilityBatch,
  authority_sequence: u64,
  entities: &[PreparedWholeEntityV1],
  append_start: u64,
  expected_hot_tail_offset: u64,
  expected_existing: Option<&KVEntry>,
  observer: &mut dyn FirstAuthorityDependencyObserverV1,
) -> Result<GcControlDependencyOutcomeV1, FirstAuthorityPublicationErrorV1> {
  let (publication_result, append_completed) = {
    let mut dependency = FirstAuthorityDependencyV1 {
      file,
      kv,
      batch,
      expected_publication_sequence: authority_sequence,
      entities,
      start_offset: append_start,
      expected_hot_tail_offset,
      expected_existing,
      prewritten: true,
      append_completed: false,
      observer,
    };
    let publication_result = admitted.commit_with_dependency(&mut dependency);
    (publication_result, dependency.append_completed)
  };
  let publication = match publication_result {
    Ok(publication) => publication,
    Err(error) => {
      kv.abort_atomic_visibility_batch(batch)?;
      return Err(error.into());
    }
  };
  if !append_completed {
    let rollback_failure_suffix = match kv.abort_atomic_visibility_batch(batch) {
      Ok(()) => String::new(),
      Err(error) => format!("; visibility rollback also failed: {error}"),
    };
    return Ok(GcControlDependencyOutcomeV1::CommittedFailure {
      publication,
      failure: GcControlPublicationFailureV1::CommittedDependencyMissing,
      message: format!("header publication completed without the exact GC control dependency{rollback_failure_suffix}"),
    });
  }
  kv.complete_hot_tail_dependency();
  if let Err(error) = kv.publish_atomic_visibility_after_authority(batch, &publication.durability) {
    return Ok(GcControlDependencyOutcomeV1::CommittedFailure {
      publication,
      failure: GcControlPublicationFailureV1::CommittedVisibilityFailure,
      message: error.to_string(),
    });
  }
  if let Err(error) = observer.authority_committed(kv, entities) {
    return Ok(GcControlDependencyOutcomeV1::CommittedFailure {
      publication,
      failure: GcControlPublicationFailureV1::CommittedPostconditionFailure,
      message: error.to_string(),
    });
  }
  Ok(GcControlDependencyOutcomeV1::Complete(publication))
}

fn validate_kv_header_alignment(kv: &DiskKVStore, header: &DatabaseHeaderV4) -> Result<(), FirstAuthorityPublicationErrorV1> {
  if kv.hash_algo() != header.hash_algorithm
    || kv.kv_block_offset() != header.kv_block_offset
    || kv.kv_block_length() != header.kv_block_length
    || kv.stage() != header.kv_block_stage as usize
    || kv.hot_tail_offset() != header.hot_tail_offset
    || kv.len() as u64 != header.entry_count
  {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_kv_header_mismatch",
      "KV state does not match the selected v4 header",
    ));
  }
  Ok(())
}

fn verify_package_locators(
  file: &File,
  kv: &DiskKVStore,
  package: &FirstAuthorityPackageV1,
  header: &DatabaseHeaderV4,
) -> Result<(), FirstAuthorityPublicationErrorV1> {
  for entity in &package.entities {
    let stored = read_entity_bounded(file, kv, &entity.key, entity.bytes.len(), header.write_sequence_high_water)?
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_readback_missing", "published locator is absent"))?;
    if stored != entity.bytes {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_readback_mismatch",
        format!("published entity {} differs from its exact bytes", hex::encode(&entity.key)),
      ));
    }
  }
  Ok(())
}

fn read_entity_bounded(
  file: &File,
  kv: &DiskKVStore,
  key: &[u8],
  maximum_total_length: usize,
  write_sequence_high_water: u64,
) -> Result<Option<Vec<u8>>, FirstAuthorityPublicationErrorV1> {
  let Some(locator) = kv.get(key)? else {
    return Ok(None);
  };
  let length = match usize::try_from(locator.total_length) {
    Ok(length) => length,
    Err(error) => {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_locator_length",
        format!("locator length exceeds usize: {error}"),
      ));
    }
  };
  if length > maximum_total_length {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_locator_exceeds_cap",
      format!("locator length {length} exceeds its {maximum_total_length}-byte role cap"),
    ));
  }
  let mut bytes = vec![0; length];
  read_file_at_native(file, locator.offset, &mut bytes)
    .map_err(|error| FirstAuthorityPublicationErrorV1::invalid("first_authority_readback_io", error.to_string()))?;
  let entity = decode_whole_entity(&bytes, kv.hash_algo(), write_sequence_high_water)?;
  if entity.key != key {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_locator_identity",
      "KV locator resolves to another WholeEntity key",
    ));
  }
  Ok(Some(bytes))
}

fn load_system_file(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  kind: SystemControlKindV1,
  identity: &[u8],
) -> Result<Vec<u8>, FirstAuthorityPublicationErrorV1> {
  let path = system_control_path(kind, identity, SystemControlSlotV1::Immutable)?;
  let path_key = first_authority_file_path_hash(&path, header.hash_algorithm);
  let record_bytes = read_entity_bounded(file, kv, &path_key, FIRST_AUTHORITY_CONTROL_ENTITY_CAP, header.write_sequence_high_water)?
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_control_missing", format!("missing {path}")))?;
  let entity = decode_whole_entity(&record_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  if entity.entry_type != EntryTypeV4::FileRecord || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_representation",
      format!("{path} is not a system FileRecord"),
    ));
  }
  let record = FileRecord::deserialize(entity.stored_value, header.hash_algorithm.hash_length(), 1)?;
  if record.path != path || record.content_type.as_deref() != Some(SYSTEM_CONTROL_CONTENT_TYPE) || !record.metadata.is_empty() {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_file_record",
      format!("{path} FileRecord metadata is not canonical"),
    ));
  }
  if record.chunk_hashes.len() != 1 || record.total_size > FIRST_AUTHORITY_CONTROL_BODY_CAP as u64 {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_file_record",
      format!("{path} must contain one bounded canonical control chunk"),
    ));
  }
  let mut body = Vec::with_capacity(record.total_size as usize);
  for chunk_key in &record.chunk_hashes {
    let chunk_bytes = read_entity_bounded(file, kv, chunk_key, FIRST_AUTHORITY_CONTROL_ENTITY_CAP, header.write_sequence_high_water)?
      .ok_or_else(|| {
        FirstAuthorityPublicationErrorV1::invalid("first_authority_control_chunk_missing", format!("missing chunk for {path}"))
      })?;
    let chunk = decode_whole_entity(&chunk_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    if chunk.entry_type != EntryTypeV4::Chunk || chunk.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "first_authority_control_chunk_representation",
        format!("{path} references a non-system chunk"),
      ));
    }
    body.extend_from_slice(chunk.stored_value);
  }
  if body.len() as u64 != record.total_size || first_authority_content_hash(&body, header.hash_algorithm) != record.content_hash {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_content",
      format!("{path} content does not match its FileRecord"),
    ));
  }
  Ok(body)
}

fn package_start_offset(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  namespace_tree_root: &[u8],
) -> Result<u64, FirstAuthorityPublicationErrorV1> {
  let tree = kv.get(namespace_tree_root)?.ok_or_else(|| {
    FirstAuthorityPublicationErrorV1::invalid("first_authority_tree_missing", "selected namespace tree locator is absent")
  })?;
  let maximum_tree_entity_length = FIRST_AUTHORITY_NAMESPACE_TREE_CAP
    .checked_add(super::entity::WHOLE_ENTITY_V1_MAX_HEADER_LENGTH)
    .and_then(|length| length.checked_add(header.hash_algorithm.hash_length()))
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_tree_cap", "namespace-tree entity cap overflowed"))?;
  let tree_bytes = read_entity_bounded(file, kv, namespace_tree_root, maximum_tree_entity_length, header.write_sequence_high_water)?
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_tree_missing", "selected namespace tree entity is absent"))?;
  let tree_entity = decode_whole_entity(&tree_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  let package_high_water = tree_entity
    .write_sequence
    .checked_add(FIRST_AUTHORITY_ENTITY_COUNT as u64 - 1)
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_tree_sequence", "first package sequence overflows"))?;
  if package_high_water != header.write_sequence_high_water {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_tree_sequence",
      "selected namespace tree sequence does not begin the first-authority package",
    ));
  }
  Ok(tree.offset)
}

#[cfg(test)]
#[path = "../../../spec/engine/v4_first_authority_internal_spec.rs"]
mod v4_first_authority_internal_spec;
