//! Atomic first-authority publication for a disconnected v4 database.

use std::collections::HashSet;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::num::TryFromIntError;
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

use super::control_store::{SYSTEM_CONTROL_CONTENT_TYPE, discover_mutable_control};
use super::contract_generated::kv_tag;
use super::database_header::DatabaseHeaderV4;
use super::entity::{EntryTypeV4, WHOLE_ENTITY_V1_FLAG_SYSTEM, WholeEntityWriteV1, decode_whole_entity, encode_whole_entity};
use super::header_publication::{
  DatabaseHeaderObservationV4, DatabaseHeaderPublicationErrorV4, DatabaseHeaderPublisherV4, HeaderPublicationDependencyV4,
  observe_database_header_v4,
};
use super::hash::digest_parts;
use super::index_artifact::{EncodedImmutableIndexArtifactV1, ImmutableIndexArtifactKindV1, decode_immutable_index_artifact};
use super::index_operation_control::{IndexOperationControlV1, decode_index_operation_control};
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
use super::gc_state::{
  GcDirectoryRoleV1, GcStateArtifactV1, RetirementReasonV1, decode_gc_state_artifact, decode_retirement_journal_segment_v1,
  retirement_journal_records_v1,
};
use super::gc_mark::{
  GcMarkArtifactV1, MARK_CHECKPOINT_VALUE_MAX, MarkResumeContextV1, decode_gc_mark_artifact, validate_mark_checkpoint_resume_context,
};
use super::gc_mark_convergence::{
  MarkMutationJournalDurabilityReceiptV1, MarkMutationJournalDurableSinkV1, MarkMutationJournalSinkErrorV1,
  PreparedMarkMutationJournalSegmentV1,
};
use super::gc_mark_workspace::{DurableMarkWorkspaceClosureV1, MarkWorkspaceErrorV1, MarkWorkspaceReopenOptionsV1, ReopenedMarkWorkspaceV1};
use super::gc_quarantine::decode_candidate_delta_v1;
use super::gc_quarantine_publication::PhysicalQuarantinePublicationPermitV1;
use super::gc_sweep::SweepProposalPublicationPermitV1;
use super::gc_sweep_removal::{
  SweepLocatorRemovalAuthorityRequestV1, SweepLocatorRemovalAuthorityV1, SweepLocatorRemovalCompletionPermitV1, SweepLocatorRemovalErrorV1,
  complete_sweep_locator_removal_v1, reserve_sweep_locator_removal_results_v1, validate_sweep_locator_removal_snapshot_v1,
};
use super::gc_sweep_reconciliation::{
  SweepReceiptReconciliationErrorV1, SweepReceiptReconciliationSourceV1, SweepReceiptVoidAuthorityRequestV1, SweepReceiptVoidAuthorityV1,
  prepare_sweep_receipt_reconciliation_v1, reserve_sweep_receipt_reconciliation_v1, validate_existing_sweep_receipt_v1,
  validate_sweep_receipt_void_authority_v1,
};
use super::gc_void::{SweepVoidArtifactV1, VoidClaimSettlementOutcomeV1, decode_sweep_void_artifact};
use super::gc_void_claim::{
  VoidClaimAdmissionAuthorityRequestV1, VoidClaimAdmissionAuthorityV1, VoidClaimAdmissionErrorV1, VoidClaimAdmittedExtentV1,
  VoidClaimTransitionLimitsV1, VoidClaimTransitionSummaryV1, VoidClaimTransitionValidatorV1, validate_void_claim_admission_authority_v1,
};
use super::gc_void_publication::{
  VoidCatalogClosureLimitsV1, VoidCatalogClosureSummaryV1, VoidCatalogClosureValidatorV1, VoidCatalogPublicationAuthorityRequestV1,
  VoidCatalogPublicationAuthorityV1, VoidCatalogPublicationErrorV1, validate_void_catalog_publication_authority_v1,
};
pub use super::gc_void_runtime::VoidReusableStateReconstructionRequestV1;
use super::gc_void_runtime::{
  VoidReclaimReceiptAuthorityV1, VoidReusableSpaceStateV1, VoidReusableStateErrorV1, VoidReusableStateIdentityV1,
  VoidReusableStateValidatorV1,
};
use super::gc_void_settlement::{
  VoidClaimConsumptionOutcomeV1, VoidClaimConsumptionPermitV1, VoidClaimSettlementAuthorityRequestV1, VoidClaimSettlementAuthorityV1,
  VoidClaimSettlementHardPublicationReceiptV1, VoidClaimSettlementPublicationErrorV1, VoidClaimSettlementPublicationRequestV1,
  VoidClaimSettlementTransitionSummaryV1, VoidClaimSettlementTransitionValidatorV1, validate_void_claim_settlement_authority_v1,
};
use super::gc_lifecycle::{
  RootLifecycleSupportClosureV1, decode_root_expiry_manifest_v1, decode_root_lifecycle_manifest_v1, decode_root_object_reclaim_proof_v1,
  decode_root_retirement_commit_v1, validate_root_lifecycle_expiry_manifest,
};
use super::gc_root_reclaim::RootExpiryRetentionPermitV1;
use super::gc_root_transition::RootRetirementIntentV1;
use super::namespace::{
  EncodedNamespaceRootV1, EncodedSemanticObjectV1, NamespaceRootWriteV1, SemanticObjectKind, decode_namespace_tree_root_v0,
  decode_semantic_object, encode_namespace_root,
};
use super::reader::FormatError;
use super::read_view::{RootPinCoordinatorErrorV1, RootReadPinCoordinatorV1};
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
const INDEX_ARTIFACT_BATCH_COUNT_CAP: usize = 4_097;
const INDEX_ARTIFACT_BATCH_BYTES_CAP: usize = 64 * 1024 * 1024;

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
pub struct IndexArtifactBatchPublicationRequestV1<'a> {
  pub database_id: &'a [u8; 16],
  pub artifacts: &'a [&'a EncodedImmutableIndexArtifactV1],
  pub publication_timestamp_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexArtifactPublicationReceiptV1 {
  pub artifact_key: Vec<u8>,
  pub write_sequence: u64,
  pub idempotent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexArtifactBatchPublicationReceiptV1 {
  pub artifacts: Vec<IndexArtifactPublicationReceiptV1>,
  pub observation: DatabaseHeaderObservationV4,
  pub idempotent: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct IndexOperationControlExpectationV1<'a> {
  pub control_sequence: u64,
  pub checkpoint_artifact: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct IndexOperationControlPublicationRequestV1<'a> {
  pub database_id: &'a [u8; 16],
  pub index_id: &'a [u8],
  pub operation_id: &'a [u8; 16],
  pub expected: Option<IndexOperationControlExpectationV1<'a>>,
  pub encoded_control: &'a [u8],
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedIndexOperationControlV1 {
  pub selected_slot: SystemControlSlotV1,
  pub control_sequence: u64,
  pub checkpoint_artifact: Vec<u8>,
  pub redundancy_degraded: bool,
  pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexOperationControlPublicationReceiptV1 {
  pub selected_slot: SystemControlSlotV1,
  pub control_sequence: u64,
  pub checkpoint_artifact: Vec<u8>,
  pub replaced_slot: bool,
  pub retirement_hard_publication_sequence: Option<u64>,
  pub observation: DatabaseHeaderObservationV4,
  pub idempotent: bool,
}

#[derive(Debug)]
pub enum IndexOperationControlPublicationErrorV1 {
  Invalid { code: &'static str, message: String },
  Committed { code: &'static str, message: String, receipt: Box<IndexOperationControlPublicationReceiptV1> },
  Format(FormatError),
  Authority(FirstAuthorityPublicationErrorV1),
  RetirementAdmission(RetirementJournalReplacementAdmissionErrorV1),
  RetirementOwner(RetirementJournalOwnerErrorV1),
}

impl IndexOperationControlPublicationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Invalid { code, .. } | Self::Committed { code, .. } => code,
      Self::Format(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::RetirementAdmission(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
    }
  }

  pub fn committed_receipt(&self) -> Option<&IndexOperationControlPublicationReceiptV1> {
    match self {
      Self::Committed { receipt, .. } => Some(receipt),
      Self::Invalid { .. } | Self::Format(_) | Self::Authority(_) | Self::RetirementAdmission(_) | Self::RetirementOwner(_) => None,
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  fn committed(code: &'static str, message: impl Into<String>, receipt: IndexOperationControlPublicationReceiptV1) -> Self {
    Self::Committed { code, message: message.into(), receipt: Box::new(receipt) }
  }
}

impl Display for IndexOperationControlPublicationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    let code = self.code();
    match self {
      Self::Invalid { message, .. } => write!(formatter, "{code}: {message}"),
      Self::Committed { message, receipt, .. } => {
        write!(
          formatter,
          "{code}: index-operation control {} committed, but post-commit handling failed: {message}",
          receipt.control_sequence
        )
      }
      Self::Format(source) => write!(formatter, "{code}: index-operation control format error: {source}"),
      Self::Authority(source) => write!(formatter, "{code}: index-operation control authority error: {source}"),
      Self::RetirementAdmission(source) => write!(formatter, "{code}: index-operation control retirement admission error: {source}"),
      Self::RetirementOwner(source) => write!(formatter, "{code}: index-operation control retirement owner error: {source}"),
    }
  }
}

impl Error for IndexOperationControlPublicationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Format(source) => Some(source),
      Self::Authority(source) => Some(source),
      Self::RetirementAdmission(source) => Some(source),
      Self::RetirementOwner(source) => Some(source),
      Self::Invalid { .. } | Self::Committed { .. } => None,
    }
  }
}

impl From<FormatError> for IndexOperationControlPublicationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for IndexOperationControlPublicationErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<EngineError> for IndexOperationControlPublicationErrorV1 {
  fn from(source: EngineError) -> Self {
    Self::Authority(FirstAuthorityPublicationErrorV1::Engine(source))
  }
}

impl From<DatabaseHeaderPublicationErrorV4> for IndexOperationControlPublicationErrorV1 {
  fn from(source: DatabaseHeaderPublicationErrorV4) -> Self {
    Self::Authority(FirstAuthorityPublicationErrorV1::Header(source))
  }
}

impl From<RetirementJournalReplacementAdmissionErrorV1> for IndexOperationControlPublicationErrorV1 {
  fn from(source: RetirementJournalReplacementAdmissionErrorV1) -> Self {
    Self::RetirementAdmission(source)
  }
}

impl From<RetirementJournalOwnerErrorV1> for IndexOperationControlPublicationErrorV1 {
  fn from(source: RetirementJournalOwnerErrorV1) -> Self {
    Self::RetirementOwner(source)
  }
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

#[derive(Clone, Copy, Debug)]
pub struct RootLifecycleSupportPublicationRequestV1<'a> {
  pub database_id: &'a [u8; 16],
  pub artifact: &'a EncodedImmutableGcArtifactV1,
  pub publication_timestamp_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootLifecycleSupportPublicationReceiptV1 {
  pub artifact_key: Vec<u8>,
  pub artifact_kind: GcArtifactKindV1,
  pub hard_publication_sequence: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct PhysicalQuarantineSupportPublicationRequestV1<'a> {
  pub database_id: &'a [u8; 16],
  pub artifact: &'a EncodedImmutableGcArtifactV1,
  pub publication_timestamp_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalQuarantineSupportPublicationReceiptV1 {
  pub artifact_key: Vec<u8>,
  pub artifact_kind: GcArtifactKindV1,
  pub hard_publication_sequence: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct VoidCatalogSupportPublicationRequestV1<'a> {
  pub database_id: &'a [u8; 16],
  pub artifact: &'a EncodedImmutableGcArtifactV1,
  pub publication_timestamp_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoidCatalogSupportPublicationReceiptV1 {
  pub artifact_key: Vec<u8>,
  pub artifact_kind: GcArtifactKindV1,
  pub hard_publication_sequence: u64,
}

#[derive(Clone, Copy)]
pub struct VoidCatalogPublicationRequestV1<'a> {
  pub completion: &'a SweepLocatorRemovalCompletionPermitV1,
  pub manifest: &'a EncodedImmutableGcArtifactV1,
  pub control: &'a EncodedGcActiveControlV1,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
  pub cancellation: &'a CancellationToken,
  pub memory: &'a MemoryCoordinator,
  pub closure_limits: VoidCatalogClosureLimitsV1,
}

#[must_use = "a selected Void catalog remains blocked until this exact catalog is reconciled into a sweep receipt"]
#[derive(Debug)]
pub struct VoidCatalogPublicationReceiptV1 {
  pub manifest_key: Vec<u8>,
  pub manifest_write_sequence: u64,
  pub control_key: Vec<u8>,
  pub control_write_sequence: u64,
  pub control_slot: u8,
  pub lineage_state: RootRetirementLineageStateV1,
  pub observation: DatabaseHeaderObservationV4,
  pub receipt_reconciliation_required: bool,
  pub reuse_blocked: bool,
  pub idempotent: bool,
}

#[derive(Clone, Copy)]
pub struct VoidClaimAdmissionRequestV1<'a> {
  pub claim: &'a EncodedImmutableGcArtifactV1,
  pub result_manifest: &'a EncodedImmutableGcArtifactV1,
  pub result_control: &'a EncodedGcActiveControlV1,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
  pub cancellation: &'a CancellationToken,
  pub memory: &'a MemoryCoordinator,
  pub transition_limits: VoidClaimTransitionLimitsV1,
}

#[must_use = "a Void claim permit is the only authority to consume its exact selected, durably reserved extents"]
pub struct VoidClaimAdmissionPermitV1 {
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  claim_id: [u8; 16],
  claim_key: Vec<u8>,
  claim_write_sequence: u64,
  source_manifest_key: Vec<u8>,
  result_manifest_key: Vec<u8>,
  result_manifest_write_sequence: u64,
  result_control_key: Vec<u8>,
  result_control_sequence: u64,
  result_control_write_sequence: u64,
  result_control_slot: u8,
  generation: u64,
  claimed_bytes: u64,
  claimed_extents: Box<[VoidClaimAdmittedExtentV1]>,
  lineage_state: RootRetirementLineageStateV1,
  observation: DatabaseHeaderObservationV4,
  idempotent: bool,
  _memory: MemoryReservation,
}

impl std::fmt::Debug for VoidClaimAdmissionPermitV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("VoidClaimAdmissionPermitV1")
      .field("claim_key", &hex::encode(&self.claim_key))
      .field("result_manifest_key", &hex::encode(&self.result_manifest_key))
      .field("result_control_sequence", &self.result_control_sequence)
      .field("result_control_write_sequence", &self.result_control_write_sequence)
      .field("claimed_extent_count", &self.claimed_extents.len())
      .field("claimed_bytes", &self.claimed_bytes)
      .field("idempotent", &self.idempotent)
      .finish_non_exhaustive()
  }
}

impl VoidClaimAdmissionPermitV1 {
  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn claim_id(&self) -> [u8; 16] {
    self.claim_id
  }

  pub fn claim_key(&self) -> &[u8] {
    &self.claim_key
  }

  pub const fn claim_write_sequence(&self) -> u64 {
    self.claim_write_sequence
  }

  pub fn source_manifest_key(&self) -> &[u8] {
    &self.source_manifest_key
  }

  pub fn result_manifest_key(&self) -> &[u8] {
    &self.result_manifest_key
  }

  pub const fn result_manifest_write_sequence(&self) -> u64 {
    self.result_manifest_write_sequence
  }

  pub fn result_control_key(&self) -> &[u8] {
    &self.result_control_key
  }

  pub const fn result_control_write_sequence(&self) -> u64 {
    self.result_control_write_sequence
  }

  pub const fn result_control_sequence(&self) -> u64 {
    self.result_control_sequence
  }

  pub const fn result_control_slot(&self) -> u8 {
    self.result_control_slot
  }

  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub const fn claimed_bytes(&self) -> u64 {
    self.claimed_bytes
  }

  pub fn claimed_extents(&self) -> &[VoidClaimAdmittedExtentV1] {
    &self.claimed_extents
  }

  pub const fn lineage_state(&self) -> &RootRetirementLineageStateV1 {
    &self.lineage_state
  }

  pub const fn observation(&self) -> &DatabaseHeaderObservationV4 {
    &self.observation
  }

  pub const fn idempotent(&self) -> bool {
    self.idempotent
  }
}

#[derive(Clone, Copy, Debug)]
pub struct PhysicalQuarantineAuthorityRecheckRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub prior_manifest_hash: &'a [u8],
  pub next_manifest_hash: &'a [u8],
  pub mark_generation: u64,
  pub expected_authority_root_set_digest: &'a [u8],
  pub expected_semantic_state_digest: &'a [u8],
  pub expected_kv_layout_fingerprint: &'a [u8],
  pub expected_mark_result_digest: &'a [u8],
  pub expected_root_lifecycle_manifest: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalQuarantineAuthoritySnapshotV1 {
  pub selected_complete_mark_generation: u64,
  pub authority_root_set_digest: Vec<u8>,
  pub semantic_state_digest: Vec<u8>,
  pub kv_layout_fingerprint: Vec<u8>,
  pub mark_result_digest: Vec<u8>,
  pub selected_root_lifecycle_manifest: Vec<u8>,
  pub physical_inventory_and_lineage_complete: bool,
  pub all_candidate_incarnations_exact_and_unreachable: bool,
  pub task_and_audit_pins_absent: bool,
}

#[derive(Debug)]
pub struct PhysicalQuarantineAuthorityRecheckErrorV1 {
  code: String,
  message: String,
}

impl PhysicalQuarantineAuthorityRecheckErrorV1 {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into() }
  }

  pub fn code(&self) -> &str {
    &self.code
  }
}

impl Display for PhysicalQuarantineAuthorityRecheckErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.message)
  }
}

impl Error for PhysicalQuarantineAuthorityRecheckErrorV1 {}

pub trait PhysicalQuarantineAuthorityVerifierV1 {
  /// Recheck complete-mark, physical-inventory, retirement-lineage,
  /// lifecycle, locator, task, and audit authority while all request-pin
  /// admission and first-authority publication are excluded. Implementations
  /// must not reenter either coordinator from this callback.
  fn recheck_physical_quarantine_authority(
    &mut self,
    request: PhysicalQuarantineAuthorityRecheckRequestV1<'_>,
  ) -> Result<PhysicalQuarantineAuthoritySnapshotV1, PhysicalQuarantineAuthorityRecheckErrorV1>;
}

#[derive(Clone, Copy)]
pub struct PhysicalQuarantinePublicationRequestV1<'a> {
  pub permit: &'a PhysicalQuarantinePublicationPermitV1,
  pub quarantine_manifest: &'a EncodedImmutableGcArtifactV1,
  pub quarantine_control: &'a EncodedGcActiveControlV1,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
  pub cancellation: &'a CancellationToken,
  pub pin_coordinator: &'a RootReadPinCoordinatorV1,
}

pub type PhysicalQuarantineLineageStateV1 = RootRetirementLineageStateV1;

#[must_use = "a physical-quarantine receipt classifies the selector commit point and retained replacement lineage"]
#[derive(Debug)]
pub struct PhysicalQuarantinePublicationReceiptV1 {
  pub quarantine_manifest_key: Vec<u8>,
  pub quarantine_manifest_write_sequence: u64,
  pub quarantine_control_key: Vec<u8>,
  pub quarantine_control_write_sequence: u64,
  pub quarantine_control_slot: u8,
  pub lineage_state: PhysicalQuarantineLineageStateV1,
  pub observation: DatabaseHeaderObservationV4,
  pub idempotent: bool,
}

#[derive(Clone, Copy)]
pub struct SweepProposalHardPublicationRequestV1<'a> {
  pub permit: &'a SweepProposalPublicationPermitV1,
  pub publication_timestamp_ms: u64,
  pub cancellation: &'a CancellationToken,
}

#[must_use = "a hard-published sweep proposal remains non-authoritative until guarded locator removal records outcomes"]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepProposalHardPublicationReceiptV1 {
  pub proposal_key: Vec<u8>,
  pub hard_publication_sequence: u64,
}

#[derive(Clone, Copy)]
pub struct SweepLocatorRemovalRequestV1<'a> {
  pub permit: &'a SweepProposalPublicationPermitV1,
  pub hard_publication: &'a SweepProposalHardPublicationReceiptV1,
  pub cancellation: &'a CancellationToken,
  pub pin_coordinator: &'a RootReadPinCoordinatorV1,
}

#[derive(Clone, Copy)]
pub struct SweepReceiptReconciliationRequestV1<'a> {
  pub source: SweepReceiptReconciliationSourceV1<'a>,
  pub cancellation: &'a CancellationToken,
  pub memory: &'a MemoryCoordinator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SweepReceiptHardPublicationReceiptV1 {
  pub receipt_key: Vec<u8>,
  pub hard_publication_sequence: u64,
  pub recovered: bool,
  pub void_catalog_hash: Vec<u8>,
  pub void_catalog_generation: u64,
  pub reclaim_committed_at_ms: i64,
}

#[derive(Debug)]
pub enum SweepProposalHardPublicationErrorV1 {
  Invalid { code: &'static str, message: String },
  CreationTime(TryFromIntError),
  Format(FormatError),
  Authority(FirstAuthorityPublicationErrorV1),
  Quarantine(PhysicalQuarantinePublicationErrorV1),
}

impl SweepProposalHardPublicationErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Invalid { code, .. } => code,
      Self::CreationTime(_) => "sweep_proposal_publication_time",
      Self::Format(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::Quarantine(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }
}

impl Display for SweepProposalHardPublicationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::CreationTime(source) => write!(formatter, "qualified sweep proposal creation time is outside its persisted range: {source}"),
      Self::Format(source) => write!(formatter, "sweep proposal format error: {source}"),
      Self::Authority(source) => write!(formatter, "sweep proposal authority error: {source}"),
      Self::Quarantine(source) => write!(formatter, "sweep proposal quarantine error: {source}"),
    }
  }
}

impl Error for SweepProposalHardPublicationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Format(source) => Some(source),
      Self::Authority(source) => Some(source),
      Self::Quarantine(source) => Some(source),
      Self::CreationTime(source) => Some(source),
      Self::Invalid { .. } => None,
    }
  }
}

impl From<FormatError> for SweepProposalHardPublicationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for SweepProposalHardPublicationErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<PhysicalQuarantinePublicationErrorV1> for SweepProposalHardPublicationErrorV1 {
  fn from(source: PhysicalQuarantinePublicationErrorV1) -> Self {
    Self::Quarantine(source)
  }
}

#[derive(Debug)]
pub enum PhysicalQuarantinePublicationErrorV1 {
  Invalid { code: &'static str, message: String },
  Committed { code: &'static str, message: String, receipt: Box<PhysicalQuarantinePublicationReceiptV1> },
  Format(FormatError),
  Authority(FirstAuthorityPublicationErrorV1),
  Pin(RootPinCoordinatorErrorV1),
  AuthorityRecheck(PhysicalQuarantineAuthorityRecheckErrorV1),
  RetirementAdmission(RetirementJournalReplacementAdmissionErrorV1),
  RetirementOwner(RetirementJournalOwnerErrorV1),
}

impl PhysicalQuarantinePublicationErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Invalid { code, .. } | Self::Committed { code, .. } => code,
      Self::Format(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::Pin(source) => source.code(),
      Self::AuthorityRecheck(source) => source.code(),
      Self::RetirementAdmission(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  fn committed(code: &'static str, message: impl Into<String>, receipt: PhysicalQuarantinePublicationReceiptV1) -> Self {
    Self::Committed { code, message: message.into(), receipt: Box::new(receipt) }
  }

  pub fn committed_receipt(&self) -> Option<&PhysicalQuarantinePublicationReceiptV1> {
    match self {
      Self::Committed { receipt, .. } => Some(receipt),
      Self::Invalid { .. }
      | Self::Format(_)
      | Self::Authority(_)
      | Self::Pin(_)
      | Self::AuthorityRecheck(_)
      | Self::RetirementAdmission(_)
      | Self::RetirementOwner(_) => None,
    }
  }
}

impl Display for PhysicalQuarantinePublicationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Committed { code, message, receipt } => write!(
        formatter,
        "{code}: physical quarantine committed at control sequence {}, but post-commit handling failed: {message}",
        receipt.quarantine_control_write_sequence,
      ),
      Self::Format(source) => write!(formatter, "physical-quarantine format error: {source}"),
      Self::Authority(source) => write!(formatter, "physical-quarantine authority error: {source}"),
      Self::Pin(source) => write!(formatter, "physical-quarantine pin error: {source}"),
      Self::AuthorityRecheck(source) => write!(formatter, "physical-quarantine external authority error: {source}"),
      Self::RetirementAdmission(source) => write!(formatter, "physical-quarantine lineage admission error: {source}"),
      Self::RetirementOwner(source) => write!(formatter, "physical-quarantine lineage owner error: {source}"),
    }
  }
}

impl Error for PhysicalQuarantinePublicationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Format(source) => Some(source),
      Self::Authority(source) => Some(source),
      Self::Pin(source) => Some(source),
      Self::AuthorityRecheck(source) => Some(source),
      Self::RetirementAdmission(source) => Some(source),
      Self::RetirementOwner(source) => Some(source),
      Self::Invalid { .. } | Self::Committed { .. } => None,
    }
  }
}

impl From<FormatError> for PhysicalQuarantinePublicationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for PhysicalQuarantinePublicationErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<RootPinCoordinatorErrorV1> for PhysicalQuarantinePublicationErrorV1 {
  fn from(source: RootPinCoordinatorErrorV1) -> Self {
    Self::Pin(source)
  }
}

impl From<PhysicalQuarantineAuthorityRecheckErrorV1> for PhysicalQuarantinePublicationErrorV1 {
  fn from(source: PhysicalQuarantineAuthorityRecheckErrorV1) -> Self {
    Self::AuthorityRecheck(source)
  }
}

impl From<RetirementJournalReplacementAdmissionErrorV1> for PhysicalQuarantinePublicationErrorV1 {
  fn from(source: RetirementJournalReplacementAdmissionErrorV1) -> Self {
    Self::RetirementAdmission(source)
  }
}

impl From<RetirementJournalOwnerErrorV1> for PhysicalQuarantinePublicationErrorV1 {
  fn from(source: RetirementJournalOwnerErrorV1) -> Self {
    Self::RetirementOwner(source)
  }
}

#[derive(Clone, Copy, Debug)]
pub struct RootRetirementAuthorityRecheckRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub namespace_root_hash: &'a [u8],
  pub expected_authority_root_set_digest: &'a [u8],
  pub final_mark_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootRetirementAuthoritySnapshotV1 {
  pub target_is_authoritative: bool,
  pub authority_root_set_digest: Vec<u8>,
}

#[derive(Debug)]
pub struct RootRetirementAuthorityRecheckErrorV1 {
  code: String,
  message: String,
}

impl RootRetirementAuthorityRecheckErrorV1 {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self { code: code.into(), message: message.into() }
  }

  pub fn code(&self) -> &str {
    &self.code
  }
}

impl Display for RootRetirementAuthorityRecheckErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.message)
  }
}

impl Error for RootRetirementAuthorityRecheckErrorV1 {}

pub trait RootRetirementAuthorityVerifierV1 {
  /// Recheck every caller-owned authority root while first-authority and the
  /// target root's read-pin gate are excluded. Implementations must not
  /// reenter either coordinator from this callback.
  fn recheck_authority_roots(
    &mut self,
    request: RootRetirementAuthorityRecheckRequestV1<'_>,
  ) -> Result<RootRetirementAuthoritySnapshotV1, RootRetirementAuthorityRecheckErrorV1>;
}

#[derive(Clone, Copy)]
pub struct RootRetirementPublicationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub intent: &'a RootRetirementIntentV1,
  pub support_closure: &'a RootLifecycleSupportClosureV1,
  pub retirement_commit: &'a EncodedImmutableGcArtifactV1,
  pub expiry_manifest: &'a EncodedImmutableGcArtifactV1,
  pub lifecycle_manifest: &'a EncodedImmutableGcArtifactV1,
  pub lifecycle_control: &'a EncodedGcActiveControlV1,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
  pub cancellation: &'a CancellationToken,
  pub pin_coordinator: &'a RootReadPinCoordinatorV1,
}

#[derive(Clone, Copy)]
pub struct RootReclaimPublicationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub retention_permit: &'a RootExpiryRetentionPermitV1,
  pub support_closure: &'a RootLifecycleSupportClosureV1,
  pub root_object_reclaim_proof: &'a EncodedImmutableGcArtifactV1,
  pub expiry_manifest: &'a EncodedImmutableGcArtifactV1,
  pub lifecycle_manifest: &'a EncodedImmutableGcArtifactV1,
  pub lifecycle_control: &'a EncodedGcActiveControlV1,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
  pub cancellation: &'a CancellationToken,
  pub pin_coordinator: &'a RootReadPinCoordinatorV1,
}

#[derive(Debug)]
pub enum RootRetirementLineageStateV1 {
  NotRequired,
  HardPublished { hard_publication_sequence: u64 },
  BufferedAfterFlushFailure { code: &'static str, message: String },
  MissingAfterCommit { code: &'static str, message: String },
}

impl RootRetirementLineageStateV1 {
  pub const fn code(&self) -> &'static str {
    match self {
      Self::NotRequired => "not_required",
      Self::HardPublished { .. } => "hard_published",
      Self::BufferedAfterFlushFailure { .. } => "buffered_after_flush_failure",
      Self::MissingAfterCommit { .. } => "missing_after_commit",
    }
  }
}

#[must_use = "a root-retirement receipt classifies the logical commit point and retained replacement lineage"]
#[derive(Debug)]
pub struct RootRetirementPublicationReceiptV1 {
  pub namespace_root_hash: Vec<u8>,
  pub retirement_commit_key: Vec<u8>,
  pub retirement_commit_write_sequence: u64,
  pub expiry_manifest_key: Vec<u8>,
  pub expiry_manifest_write_sequence: u64,
  pub lifecycle_manifest_key: Vec<u8>,
  pub lifecycle_manifest_write_sequence: u64,
  pub lifecycle_control_key: Vec<u8>,
  pub lifecycle_control_write_sequence: u64,
  pub lifecycle_control_slot: u8,
  pub lineage_state: RootRetirementLineageStateV1,
  pub observation: DatabaseHeaderObservationV4,
  pub idempotent: bool,
}

pub type RootReclaimLineageStateV1 = RootRetirementLineageStateV1;

#[must_use = "a root-reclaim receipt classifies the selector commit point and retained replacement lineage"]
#[derive(Debug)]
pub struct RootReclaimPublicationReceiptV1 {
  pub namespace_root_hash: Vec<u8>,
  pub root_object_reclaim_proof_key: Vec<u8>,
  pub root_object_reclaim_proof_write_sequence: u64,
  pub expiry_manifest_key: Vec<u8>,
  pub expiry_manifest_write_sequence: u64,
  pub lifecycle_manifest_key: Vec<u8>,
  pub lifecycle_manifest_write_sequence: u64,
  pub lifecycle_control_key: Vec<u8>,
  pub lifecycle_control_write_sequence: u64,
  pub lifecycle_control_slot: u8,
  pub lineage_state: RootReclaimLineageStateV1,
  pub observation: DatabaseHeaderObservationV4,
  pub idempotent: bool,
}

#[derive(Debug)]
pub enum RootRetirementPublicationErrorV1 {
  Invalid { code: &'static str, message: String },
  Committed { code: &'static str, message: String, receipt: Box<RootRetirementPublicationReceiptV1> },
  Format(FormatError),
  Authority(FirstAuthorityPublicationErrorV1),
  Pin(RootPinCoordinatorErrorV1),
  AuthorityRecheck(RootRetirementAuthorityRecheckErrorV1),
  RetirementAdmission(RetirementJournalReplacementAdmissionErrorV1),
  RetirementOwner(RetirementJournalOwnerErrorV1),
}

impl RootRetirementPublicationErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Invalid { code, .. } | Self::Committed { code, .. } => code,
      Self::Format(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::Pin(source) => source.code(),
      Self::AuthorityRecheck(source) => source.code(),
      Self::RetirementAdmission(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  fn committed(code: &'static str, message: impl Into<String>, receipt: RootRetirementPublicationReceiptV1) -> Self {
    Self::Committed { code, message: message.into(), receipt: Box::new(receipt) }
  }

  pub fn committed_receipt(&self) -> Option<&RootRetirementPublicationReceiptV1> {
    match self {
      Self::Committed { receipt, .. } => Some(receipt),
      Self::Invalid { .. }
      | Self::Format(_)
      | Self::Authority(_)
      | Self::Pin(_)
      | Self::AuthorityRecheck(_)
      | Self::RetirementAdmission(_)
      | Self::RetirementOwner(_) => None,
    }
  }
}

impl Display for RootRetirementPublicationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Committed { code, message, receipt } => write!(
        formatter,
        "{code}: root {} retired at lifecycle control sequence {}, but post-commit handling failed: {message}",
        hex::encode(&receipt.namespace_root_hash),
        receipt.lifecycle_control_write_sequence,
      ),
      Self::Format(source) => write!(formatter, "root-retirement format error: {source}"),
      Self::Authority(source) => write!(formatter, "root-retirement authority error: {source}"),
      Self::Pin(source) => write!(formatter, "root-retirement pin error: {source}"),
      Self::AuthorityRecheck(source) => write!(formatter, "root-retirement external authority error: {source}"),
      Self::RetirementAdmission(source) => write!(formatter, "root-retirement lineage admission error: {source}"),
      Self::RetirementOwner(source) => write!(formatter, "root-retirement lineage owner error: {source}"),
    }
  }
}

impl Error for RootRetirementPublicationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Format(source) => Some(source),
      Self::Authority(source) => Some(source),
      Self::Pin(source) => Some(source),
      Self::AuthorityRecheck(source) => Some(source),
      Self::RetirementAdmission(source) => Some(source),
      Self::RetirementOwner(source) => Some(source),
      Self::Invalid { .. } | Self::Committed { .. } => None,
    }
  }
}

#[derive(Debug)]
pub enum RootReclaimPublicationErrorV1 {
  Invalid { code: &'static str, message: String },
  Committed { code: &'static str, message: String, receipt: Box<RootReclaimPublicationReceiptV1> },
  Format(FormatError),
  Authority(FirstAuthorityPublicationErrorV1),
  Pin(RootPinCoordinatorErrorV1),
  RetirementAdmission(RetirementJournalReplacementAdmissionErrorV1),
  RetirementOwner(RetirementJournalOwnerErrorV1),
}

impl RootReclaimPublicationErrorV1 {
  pub fn code(&self) -> &str {
    match self {
      Self::Invalid { code, .. } | Self::Committed { code, .. } => code,
      Self::Format(source) => source.code(),
      Self::Authority(source) => source.code(),
      Self::Pin(source) => source.code(),
      Self::RetirementAdmission(source) => source.code(),
      Self::RetirementOwner(source) => source.code(),
    }
  }

  fn invalid(code: &'static str, message: impl Into<String>) -> Self {
    Self::Invalid { code, message: message.into() }
  }

  fn committed(code: &'static str, message: impl Into<String>, receipt: RootReclaimPublicationReceiptV1) -> Self {
    Self::Committed { code, message: message.into(), receipt: Box::new(receipt) }
  }

  pub fn committed_receipt(&self) -> Option<&RootReclaimPublicationReceiptV1> {
    match self {
      Self::Committed { receipt, .. } => Some(receipt),
      Self::Invalid { .. }
      | Self::Format(_)
      | Self::Authority(_)
      | Self::Pin(_)
      | Self::RetirementAdmission(_)
      | Self::RetirementOwner(_) => None,
    }
  }
}

impl Display for RootReclaimPublicationErrorV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid { code, message } => write!(formatter, "{code}: {message}"),
      Self::Committed { code, message, receipt } => write!(
        formatter,
        "{code}: root {} reclaimed at lifecycle control sequence {}, but post-commit handling failed: {message}",
        hex::encode(&receipt.namespace_root_hash),
        receipt.lifecycle_control_write_sequence,
      ),
      Self::Format(source) => write!(formatter, "root-reclaim format error: {source}"),
      Self::Authority(source) => write!(formatter, "root-reclaim authority error: {source}"),
      Self::Pin(source) => write!(formatter, "root-reclaim pin error: {source}"),
      Self::RetirementAdmission(source) => write!(formatter, "root-reclaim lineage admission error: {source}"),
      Self::RetirementOwner(source) => write!(formatter, "root-reclaim lineage owner error: {source}"),
    }
  }
}

impl Error for RootReclaimPublicationErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Format(source) => Some(source),
      Self::Authority(source) => Some(source),
      Self::Pin(source) => Some(source),
      Self::RetirementAdmission(source) => Some(source),
      Self::RetirementOwner(source) => Some(source),
      Self::Invalid { .. } | Self::Committed { .. } => None,
    }
  }
}

impl From<FormatError> for RootReclaimPublicationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for RootReclaimPublicationErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<EngineError> for RootReclaimPublicationErrorV1 {
  fn from(source: EngineError) -> Self {
    Self::Authority(FirstAuthorityPublicationErrorV1::Engine(source))
  }
}

impl From<RootPinCoordinatorErrorV1> for RootReclaimPublicationErrorV1 {
  fn from(source: RootPinCoordinatorErrorV1) -> Self {
    Self::Pin(source)
  }
}

impl From<RetirementJournalReplacementAdmissionErrorV1> for RootReclaimPublicationErrorV1 {
  fn from(source: RetirementJournalReplacementAdmissionErrorV1) -> Self {
    Self::RetirementAdmission(source)
  }
}

impl From<RetirementJournalOwnerErrorV1> for RootReclaimPublicationErrorV1 {
  fn from(source: RetirementJournalOwnerErrorV1) -> Self {
    Self::RetirementOwner(source)
  }
}

impl From<FormatError> for RootRetirementPublicationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Format(source)
  }
}

impl From<FirstAuthorityPublicationErrorV1> for RootRetirementPublicationErrorV1 {
  fn from(source: FirstAuthorityPublicationErrorV1) -> Self {
    Self::Authority(source)
  }
}

impl From<EngineError> for RootRetirementPublicationErrorV1 {
  fn from(source: EngineError) -> Self {
    Self::Authority(FirstAuthorityPublicationErrorV1::Engine(source))
  }
}

impl From<RootPinCoordinatorErrorV1> for RootRetirementPublicationErrorV1 {
  fn from(source: RootPinCoordinatorErrorV1) -> Self {
    Self::Pin(source)
  }
}

impl From<RootRetirementAuthorityRecheckErrorV1> for RootRetirementPublicationErrorV1 {
  fn from(source: RootRetirementAuthorityRecheckErrorV1) -> Self {
    Self::AuthorityRecheck(source)
  }
}

impl From<RetirementJournalReplacementAdmissionErrorV1> for RootRetirementPublicationErrorV1 {
  fn from(source: RetirementJournalReplacementAdmissionErrorV1) -> Self {
    Self::RetirementAdmission(source)
  }
}

impl From<RetirementJournalOwnerErrorV1> for RootRetirementPublicationErrorV1 {
  fn from(source: RetirementJournalOwnerErrorV1) -> Self {
    Self::RetirementOwner(source)
  }
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

struct LoadedSystemFileV1 {
  locator: KVEntry,
  entity_bytes: Vec<u8>,
  record: FileRecord,
  body: Vec<u8>,
  write_sequence: u64,
  integrity_hash: Vec<u8>,
}

struct LoadedIndexOperationControlPairV1 {
  slots: [Option<LoadedSystemFileV1>; 2],
  selected: Option<LoadedIndexOperationControlV1>,
}

struct ValidatedRootRetirementPublicationV1<'a> {
  lifecycle_control: GcActiveControlV1<'a>,
}

struct SelectedRootLifecycleControlV1 {
  stored_value: Vec<u8>,
  target_manifest_hash: Vec<u8>,
}

struct RootRetirementLockedPublicationV1 {
  retirement_commit_write_sequence: u64,
  expiry_manifest_write_sequence: u64,
  lifecycle_manifest_write_sequence: u64,
  control: GcControlPublicationV1,
  committed_failure: Option<(GcControlPublicationFailureV1, String)>,
}

struct ValidatedRootReclaimPublicationV1<'a> {
  lifecycle_control: GcActiveControlV1<'a>,
}

struct RootReclaimLockedPublicationV1 {
  root_object_reclaim_proof_write_sequence: u64,
  expiry_manifest_write_sequence: u64,
  lifecycle_manifest_write_sequence: u64,
  control: GcControlPublicationV1,
  committed_failure: Option<(GcControlPublicationFailureV1, String)>,
}

struct ValidatedPhysicalQuarantinePublicationV1<'a> {
  manifest: super::gc_quarantine::QuarantineManifestV1<'a>,
  control: GcActiveControlV1<'a>,
}

struct SelectedPhysicalQuarantineControlV1 {
  stored_value: Vec<u8>,
  target_manifest_hash: Vec<u8>,
}

struct PhysicalQuarantineLockedPublicationV1 {
  quarantine_manifest_write_sequence: u64,
  control: GcControlPublicationV1,
  committed_failure: Option<(GcControlPublicationFailureV1, String)>,
}

struct SelectedVoidCatalogControlV1 {
  stored_value: Vec<u8>,
  control_key: Vec<u8>,
  target_manifest_hash: Vec<u8>,
  control_sequence: u64,
  write_sequence: u64,
  slot: u8,
}

struct VoidCatalogLockedPublicationV1 {
  manifest_write_sequence: u64,
  control: GcControlPublicationV1,
  committed_failure: Option<(GcControlPublicationFailureV1, String)>,
}

struct VoidClaimLockedAdmissionV1 {
  claim_write_sequence: u64,
  claim_generation: u64,
  transition: VoidClaimTransitionSummaryV1,
  result_manifest_write_sequence: u64,
  result_control_sequence: u64,
  control: GcControlPublicationV1,
  committed_failure: Option<(GcControlPublicationFailureV1, String)>,
}

struct VoidClaimLockedSettlementV1 {
  result_manifest_write_sequence: u64,
  control: GcControlPublicationV1,
  settlement_preexisting: bool,
  committed_failure: Option<(GcControlPublicationFailureV1, String)>,
}

struct VerifiedVoidClaimSettlementTransitionV1 {
  source_entity: ChargedVoidCatalogSupportEntityV1,
  claim_entity: ChargedVoidCatalogSupportEntityV1,
  transition: VoidClaimSettlementTransitionSummaryV1,
}

struct VerifiedVoidClaimTransitionV1 {
  source_entity: ChargedVoidCatalogSupportEntityV1,
  claim_write_sequence: u64,
  transition: VoidClaimTransitionSummaryV1,
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
  CommittedAuthorityUncertain,
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
      Self::CommittedAuthorityUncertain => "gc_control_committed_authority_uncertain",
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
      Self::CommittedAuthorityUncertain => "mark_checkpoint_control_committed_authority_uncertain",
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

impl From<GcControlPublicationErrorV1> for RootRetirementPublicationErrorV1 {
  fn from(source: GcControlPublicationErrorV1) -> Self {
    match source {
      GcControlPublicationErrorV1::Invalid { failure, message } => Self::Invalid { code: failure.code(), message },
      GcControlPublicationErrorV1::Format(source) => Self::Format(source),
      GcControlPublicationErrorV1::Authority(source) => Self::Authority(source),
      GcControlPublicationErrorV1::RetirementAdmission(source) => Self::RetirementAdmission(source),
      GcControlPublicationErrorV1::RetirementOwner(source) => Self::RetirementOwner(source),
    }
  }
}

impl From<GcControlPublicationErrorV1> for RootReclaimPublicationErrorV1 {
  fn from(source: GcControlPublicationErrorV1) -> Self {
    match source {
      GcControlPublicationErrorV1::Invalid { failure, message } => Self::Invalid { code: failure.code(), message },
      GcControlPublicationErrorV1::Format(source) => Self::Format(source),
      GcControlPublicationErrorV1::Authority(source) => Self::Authority(source),
      GcControlPublicationErrorV1::RetirementAdmission(source) => Self::RetirementAdmission(source),
      GcControlPublicationErrorV1::RetirementOwner(source) => Self::RetirementOwner(source),
    }
  }
}

impl From<GcControlPublicationErrorV1> for VoidCatalogPublicationErrorV1 {
  fn from(source: GcControlPublicationErrorV1) -> Self {
    match source {
      GcControlPublicationErrorV1::Invalid { failure, message } => Self::Invalid { code: failure.code(), message },
      GcControlPublicationErrorV1::Format(source) => Self::Format(source),
      GcControlPublicationErrorV1::Authority(source) => Self::Authority(source),
      GcControlPublicationErrorV1::RetirementAdmission(source) => Self::RetirementAdmission(source),
      GcControlPublicationErrorV1::RetirementOwner(source) => Self::RetirementOwner(source),
    }
  }
}

impl From<GcControlPublicationErrorV1> for VoidClaimAdmissionErrorV1 {
  fn from(source: GcControlPublicationErrorV1) -> Self {
    match source {
      GcControlPublicationErrorV1::Invalid { failure, message } => Self::Invalid { code: failure.code(), message },
      GcControlPublicationErrorV1::Format(source) => Self::Format(source),
      GcControlPublicationErrorV1::Authority(source) => Self::Authority(source),
      GcControlPublicationErrorV1::RetirementAdmission(source) => Self::RetirementAdmission(source),
      GcControlPublicationErrorV1::RetirementOwner(source) => Self::RetirementOwner(source),
    }
  }
}

impl From<GcControlPublicationErrorV1> for VoidClaimSettlementPublicationErrorV1 {
  fn from(source: GcControlPublicationErrorV1) -> Self {
    match source {
      GcControlPublicationErrorV1::Invalid { failure, message } => Self::Invalid { code: failure.code(), message },
      GcControlPublicationErrorV1::Format(source) => Self::Format(source),
      GcControlPublicationErrorV1::Authority(source) => Self::Authority(source),
      GcControlPublicationErrorV1::RetirementAdmission(source) => Self::RetirementAdmission(source),
      GcControlPublicationErrorV1::RetirementOwner(source) => Self::RetirementOwner(source),
    }
  }
}

impl From<GcControlPublicationErrorV1> for PhysicalQuarantinePublicationErrorV1 {
  fn from(source: GcControlPublicationErrorV1) -> Self {
    match source {
      GcControlPublicationErrorV1::Invalid { failure, message } => Self::Invalid { code: failure.code(), message },
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

enum StableEntityDependencyOutcomeV1 {
  Complete(StableEntityDependencyPublicationV1),
  CommittedFailure { publication: StableEntityDependencyPublicationV1, failure: StableEntityDependencyFailureV1, message: String },
}

struct StableEntityDependencyPublicationV1 {
  observation: DatabaseHeaderObservationV4,
}

#[derive(Clone, Copy, Debug)]
enum StableEntityDependencyFailureV1 {
  DependencyMissing,
  AuthorityUncertain,
  VisibilityFailure,
  PostconditionFailure,
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

struct SharedFirstAuthorityRetirementSinkV1<'a> {
  publisher: &'a V4FirstAuthorityPublisher,
}

impl RetirementJournalDurableSinkV1 for SharedFirstAuthorityRetirementSinkV1<'_> {
  fn publish_synced(
    &mut self,
    segment: &PreparedRetirementJournalSegmentV1<'_>,
  ) -> Result<RetirementJournalDurabilityReceiptV1, RetirementJournalSinkErrorV1> {
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    self.publisher.publish_retirement_journal_segment(segment, &mut observer)
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

  pub fn load_index_artifact(&self, key: &[u8], expected_value_length: u64) -> Result<Option<Vec<u8>>, FirstAuthorityPublicationErrorV1> {
    let observation = self.observe()?;
    let header = &observation.selected.header;
    validate_index_artifact_key(header.hash_algorithm, key)?;
    let expected_value_length = usize::try_from(expected_value_length).map_err(|error| {
      FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_expected_length",
        format!("expected IndexArtifact length exceeds usize: {error}"),
      )
    })?;
    if expected_value_length == 0 || expected_value_length > ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length() {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_expected_length",
        format!("expected IndexArtifact length {expected_value_length} is outside the frozen bounds"),
      ));
    }
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    load_index_artifact_entity(&self.file, &kv, header, key)
      .map(|loaded| loaded.and_then(|loaded| (loaded.value.len() == expected_value_length).then_some(loaded.value)))
  }

  pub fn index_artifact_length(&self, key: &[u8]) -> Result<Option<u64>, FirstAuthorityPublicationErrorV1> {
    let observation = self.observe()?;
    let header = &observation.selected.header;
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    validated_index_artifact_locator(&self.file, &kv, header, key)?
      .map(|located| {
        u64::try_from(located.value_length).map_err(|error| {
          FirstAuthorityPublicationErrorV1::invalid(
            "immutable_index_locator_length",
            format!("IndexArtifact value length exceeds u64: {error}"),
          )
        })
      })
      .transpose()
  }

  pub fn load_index_operation_control(
    &self,
    database_id: &[u8; 16],
    index_id: &[u8],
    operation_id: &[u8; 16],
  ) -> Result<Option<LoadedIndexOperationControlV1>, FirstAuthorityPublicationErrorV1> {
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      FirstAuthorityPublicationErrorV1::StateLockPoisoned
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    validate_index_operation_identity(header, database_id, index_id, operation_id)?;
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    Ok(load_index_operation_control_pair(&self.file, &kv, header, index_id, operation_id)?.selected)
  }

  pub fn publish_index_operation_control(
    &self,
    request: IndexOperationControlPublicationRequestV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  ) -> Result<IndexOperationControlPublicationReceiptV1, IndexOperationControlPublicationErrorV1> {
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    self.publish_index_operation_control_with_observer(request, retirement_owner, &mut observer)
  }

  fn publish_index_operation_control_with_observer(
    &self,
    request: IndexOperationControlPublicationRequestV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<IndexOperationControlPublicationReceiptV1, IndexOperationControlPublicationErrorV1> {
    // No prior buffered replacement may be hidden behind the publication this
    // call is about to make. The sink reacquires first authority, so flush
    // before taking the root-state lock.
    retirement_owner.flush(&mut SharedFirstAuthorityRetirementSinkV1 { publisher: self })?;
    let prior_hard_publication_sequence = retirement_owner.status().last_hard_publication_sequence;
    let authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      FirstAuthorityPublicationErrorV1::StateLockPoisoned
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded || header.head_hash.iter().all(|byte| *byte == 0) {
      return Err(IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_missing_authority",
        "index-operation control publication requires selected non-degraded first authority",
      ));
    }
    validate_index_operation_identity(header, request.database_id, request.index_id, request.operation_id)?;
    if retirement_owner.hash_algorithm() != header.hash_algorithm || retirement_owner.database_id() != header.database_id {
      return Err(IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_retirement_authority",
        "index-operation control and retirement owner belong to different database authority",
      ));
    }
    if request.publication_timestamp_ms == 0 || request.monotonic_now_ms == 0 {
      return Err(IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_publication_time",
        "publication and monotonic timestamps must be nonzero",
      ));
    }
    let incoming = decode_index_operation_control(request.encoded_control, header.hash_algorithm)?;
    validate_index_operation_control_request(&incoming, &request)?;
    let incoming_checkpoint = incoming.checkpoint_artifact.ok_or_else(|| {
      IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_checkpoint_missing",
        "selected recovery control must identify an immutable checkpoint artifact",
      )
    })?;

    let mut kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    kv.flush()?;
    validate_kv_header_alignment(&kv, header)?;
    if kv.write_buffer_len() != 0 || kv.hot_buffer_len() != 0 {
      return Err(IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_baseline_not_flushed",
        "index-operation control publication requires an empty KV write and hot-buffer baseline",
      ));
    }
    let pair = load_index_operation_control_pair(&self.file, &kv, header, request.index_id, request.operation_id)?;
    if let Some(current) = pair.selected.as_ref() {
      if current.bytes == request.encoded_control {
        return Ok(IndexOperationControlPublicationReceiptV1 {
          selected_slot: current.selected_slot,
          control_sequence: current.control_sequence,
          checkpoint_artifact: current.checkpoint_artifact.clone(),
          replaced_slot: false,
          retirement_hard_publication_sequence: None,
          observation,
          idempotent: true,
        });
      }
    }
    validate_index_operation_expectation(pair.selected.as_ref(), request.expected, header.hash_algorithm)?;
    let expected_sequence = match pair.selected.as_ref() {
      Some(current) => current.control_sequence.checked_add(1).ok_or_else(|| {
        IndexOperationControlPublicationErrorV1::invalid("index_operation_sequence_exhausted", "control sequence is exhausted")
      })?,
      None => 1,
    };
    if incoming.control_sequence != expected_sequence {
      return Err(IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_sequence",
        format!("expected control sequence {expected_sequence}, received {}", incoming.control_sequence),
      ));
    }
    let target_slot = match pair.selected.as_ref().map(|selected| selected.selected_slot) {
      Some(SystemControlSlotV1::A) => SystemControlSlotV1::B,
      Some(SystemControlSlotV1::B) | None => SystemControlSlotV1::A,
      Some(SystemControlSlotV1::Immutable) => {
        return Err(IndexOperationControlPublicationErrorV1::invalid(
          "index_operation_selected_slot",
          "mutable index-operation control selected the immutable slot",
        ));
      }
    };
    let target_index = usize::from(target_slot == SystemControlSlotV1::B);
    let target = pair.slots[target_index].as_ref();
    let identity = index_operation_control_identity(header.hash_algorithm, request.index_id, request.operation_id)?;
    let path = system_control_path(SystemControlKindV1::IndexOperation, &identity, target_slot)?;
    let path_key = first_authority_file_path_hash(&path, header.hash_algorithm);
    let chunk_key = first_authority_system_chunk_hash(request.encoded_control, header.hash_algorithm);
    let stored_chunk = load_system_chunk(&self.file, &kv, header, &chunk_key, request.encoded_control)?;

    let mut next_write_sequence = header.write_sequence_high_water;
    if stored_chunk.is_none() {
      next_write_sequence = next_write_sequence.checked_add(1).ok_or_else(|| {
        IndexOperationControlPublicationErrorV1::invalid("index_operation_write_sequence_exhausted", "v4 write sequence is exhausted")
      })?;
    }
    next_write_sequence = next_write_sequence.checked_add(1).ok_or_else(|| {
      IndexOperationControlPublicationErrorV1::invalid("index_operation_write_sequence_exhausted", "v4 write sequence is exhausted")
    })?;
    let file_record_write_sequence = next_write_sequence;
    let mut reservation_candidate = header.clone();
    reservation_candidate.updated_at_ms = header.updated_at_ms.max(request.publication_timestamp_ms);
    reservation_candidate.write_sequence_high_water = next_write_sequence;
    let reservation = self.header_publisher.publish_inactive_slot(&self.file, &observation, reservation_candidate)?;
    let reserved_observation = reservation.observation;
    let header = &reserved_observation.selected.header;
    validate_kv_header_alignment(&kv, header)?;

    let timestamp = i64::try_from(request.publication_timestamp_ms).map_err(|error| {
      IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_timestamp_range",
        format!("publication timestamp exceeds FileRecord range: {error}"),
      )
    })?;
    let mut entities = Vec::new();
    entities.try_reserve_exact(2).map_err(|error| {
      IndexOperationControlPublicationErrorV1::invalid("index_operation_entity_allocation", format!("entity allocation failed: {error}"))
    })?;
    let mut expected_existing = Vec::new();
    expected_existing.try_reserve_exact(2).map_err(|error| {
      IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_expectation_allocation",
        format!("dependency expectation allocation failed: {error}"),
      )
    })?;
    if stored_chunk.is_none() {
      let chunk_sequence = file_record_write_sequence - 1;
      let chunk = encode_entity(
        EntryTypeV4::Chunk,
        WHOLE_ENTITY_V1_FLAG_SYSTEM,
        header.hash_algorithm,
        request.publication_timestamp_ms,
        chunk_sequence,
        &chunk_key,
        request.encoded_control,
      )?;
      entities.push(PreparedWholeEntityV1 { key: chunk_key.clone(), kv_type: KV_TYPE_CHUNK, bytes: chunk });
      expected_existing.push(None);
    }
    let mut content_type = String::new();
    content_type.try_reserve_exact(SYSTEM_CONTROL_CONTENT_TYPE.len()).map_err(|error| {
      IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_file_record_allocation",
        format!("content-type allocation failed: {error}"),
      )
    })?;
    content_type.push_str(SYSTEM_CONTROL_CONTENT_TYPE);
    let mut chunk_hashes = Vec::new();
    chunk_hashes.try_reserve_exact(1).map_err(|error| {
      IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_file_record_allocation",
        format!("chunk identity allocation failed: {error}"),
      )
    })?;
    chunk_hashes.push(chunk_key);
    let record = FileRecord {
      path,
      content_type: Some(content_type),
      total_size: request.encoded_control.len() as u64,
      created_at: target.map_or(timestamp, |loaded| loaded.record.created_at),
      updated_at: timestamp,
      metadata: Vec::new(),
      content_hash: first_authority_content_hash(request.encoded_control, header.hash_algorithm),
      chunk_hashes,
    };
    let record_value = record.serialize(header.hash_algorithm.hash_length())?;
    let record_entity = encode_entity(
      EntryTypeV4::FileRecord,
      WHOLE_ENTITY_V1_FLAG_SYSTEM,
      header.hash_algorithm,
      request.publication_timestamp_ms,
      file_record_write_sequence,
      &path_key,
      &record_value,
    )?;
    let replacement_entity = decode_whole_entity(&record_entity, header.hash_algorithm, file_record_write_sequence)?;
    let replacement_integrity_hash = replacement_entity.integrity_hash.to_vec();
    let record_entity_length = record_entity.len();
    entities.push(PreparedWholeEntityV1 { key: path_key.clone(), kv_type: KV_TYPE_FILE_RECORD, bytes: record_entity });
    expected_existing.push(target.map(|loaded| loaded.locator.clone()));

    let append_start = self.file.metadata().map_err(EngineError::IoError)?.len();
    if append_start < header.hot_tail_offset {
      return Err(IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_file_truncated",
        "database length precedes the selected v4 hot-tail offset",
      ));
    }
    let mut write_offset = append_start;
    for entity in &entities {
      write_file_at_native(&self.file, write_offset, &entity.bytes)
        .map_err(|source| IndexOperationControlPublicationErrorV1::invalid("index_operation_control_write", source.to_string()))?;
      verify_file_bytes_native(&self.file, write_offset, &entity.bytes)
        .map_err(|source| IndexOperationControlPublicationErrorV1::invalid("index_operation_control_readback", source.to_string()))?;
      write_offset = write_offset
        .checked_add(entity.bytes.len() as u64)
        .ok_or_else(|| IndexOperationControlPublicationErrorV1::invalid("index_operation_wal_overflow", "control WAL offset overflowed"))?;
    }
    let expected_hot_tail_offset = write_offset;
    let record_entity_offset = expected_hot_tail_offset - record_entity_length as u64;
    let replacement_incarnation = encode_v4_physical_incarnation(
      header.hash_algorithm,
      &path_key,
      &replacement_integrity_hash,
      record_entity_offset,
      file_record_write_sequence,
      record_entity_length,
      EntryTypeV4::FileRecord,
    )
    .map_err(index_operation_from_gc_control_error)?;
    let old_incarnation = target
      .map(|target| {
        encode_v4_physical_incarnation(
          header.hash_algorithm,
          &path_key,
          &target.integrity_hash,
          target.locator.offset,
          target.write_sequence,
          target.entity_bytes.len(),
          EntryTypeV4::FileRecord,
        )
        .map_err(index_operation_from_gc_control_error)
      })
      .transpose()?;

    let orphan_prefix_bytes = append_start.checked_sub(header.hot_tail_offset).ok_or_else(|| {
      IndexOperationControlPublicationErrorV1::invalid("index_operation_wal_prefix", "control append precedes the selected hot tail")
    })?;
    let dependency_bytes = entity_dependency_bytes(&entities, header.hash_algorithm.hash_length())?
      .checked_add(orphan_prefix_bytes)
      .ok_or_else(|| IndexOperationControlPublicationErrorV1::invalid("index_operation_dependency_size", "dependency bytes overflowed"))?;
    let new_locators = u64::from(stored_chunk.is_none()) + u64::from(target.is_none());
    let mut candidate = header.clone();
    candidate.updated_at_ms = header.updated_at_ms.max(request.publication_timestamp_ms);
    candidate.write_sequence_high_water = file_record_write_sequence;
    candidate.hot_tail_offset = expected_hot_tail_offset;
    candidate.entry_count = header
      .entry_count
      .checked_add(new_locators)
      .ok_or_else(|| IndexOperationControlPublicationErrorV1::invalid("index_operation_entry_count", "v4 entry count overflowed"))?;
    let admitted =
      self.header_publisher.admit_inactive_slot_with_dependency_bytes(&self.file, &reserved_observation, candidate, dependency_bytes)?;
    let authority_sequence = admitted.sequence();
    let batch = kv.begin_atomic_visibility_batch(entities.len(), authority_sequence)?;
    let prepared_retirement = if let Some(old_incarnation) = old_incarnation.as_ref() {
      let replacements = [RetirementJournalReplacementV1 {
        reason: RetirementReasonV1::PointerOrControlReplace,
        old_incarnation,
        replacement_incarnation: &replacement_incarnation,
      }];
      let mut sink = BufferedOnlyRetirementSinkV1;
      match RetirementJournalReplacementCoordinatorV1::new(retirement_owner, &mut sink).prepare_buffered_single(
        RetirementJournalReplacementBatchV1 {
          replacement_publication_sequence: file_record_write_sequence,
          retired_at_ms: request.publication_timestamp_ms,
          replacements: &replacements,
        },
        request.monotonic_now_ms,
      ) {
        Ok(prepared) => Some(prepared),
        Err(source) => {
          kv.abort_atomic_visibility_batch(batch)?;
          return Err(source.into());
        }
      }
    } else {
      None
    };
    let publication = if let Some(prepared) = prepared_retirement {
      match prepared.activate(|_| {
        commit_stable_entity_dependency(
          &self.file,
          &mut kv,
          admitted,
          batch,
          authority_sequence,
          &entities,
          append_start,
          expected_hot_tail_offset,
          &expected_existing,
          observer,
        )
      }) {
        Ok(outcome) => outcome.output,
        Err(source) => {
          let Some((source, prepared)) = source.into_activation_failure() else {
            return Err(IndexOperationControlPublicationErrorV1::invalid(
              "index_operation_retirement_activation",
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
      commit_stable_entity_dependency(
        &self.file,
        &mut kv,
        admitted,
        batch,
        authority_sequence,
        &entities,
        append_start,
        expected_hot_tail_offset,
        &expected_existing,
        observer,
      )?
    };
    let (publication_observation, committed_failure) = match publication {
      StableEntityDependencyOutcomeV1::Complete(publication) => (publication.observation, None),
      StableEntityDependencyOutcomeV1::CommittedFailure { publication, failure, message } => {
        (publication.observation, Some((failure, message)))
      }
    };
    let selected =
      load_index_operation_control_pair(&self.file, &kv, &publication_observation.selected.header, request.index_id, request.operation_id)?
        .selected;
    let readback_failure = match selected {
      Some(ref selected)
        if selected.selected_slot == target_slot
          && selected.control_sequence == incoming.control_sequence
          && selected.bytes == request.encoded_control =>
      {
        None
      }
      Some(_) => Some("published index-operation control was not selected exactly".to_string()),
      None => Some("published index-operation control is absent".to_string()),
    };
    drop(kv);
    drop(authority);

    let replaced_slot = target.is_some();
    let mut receipt = IndexOperationControlPublicationReceiptV1 {
      selected_slot: target_slot,
      control_sequence: incoming.control_sequence,
      checkpoint_artifact: incoming_checkpoint.to_vec(),
      replaced_slot,
      retirement_hard_publication_sequence: None,
      observation: publication_observation,
      idempotent: false,
    };
    if replaced_slot {
      if let Err(source) = retirement_owner.flush(&mut SharedFirstAuthorityRetirementSinkV1 { publisher: self }) {
        return Err(IndexOperationControlPublicationErrorV1::committed("index_operation_retirement_flush", source.to_string(), receipt));
      }
      let hard_sequence = retirement_owner.status().last_hard_publication_sequence;
      if hard_sequence <= prior_hard_publication_sequence {
        return Err(IndexOperationControlPublicationErrorV1::committed(
          "index_operation_retirement_missing",
          "control replacement committed without a new hard retirement publication",
          receipt,
        ));
      }
      receipt.retirement_hard_publication_sequence = Some(hard_sequence);
      receipt.observation = self.observe()?;
    }
    if let Some((failure, message)) = committed_failure {
      return Err(IndexOperationControlPublicationErrorV1::committed(index_operation_committed_failure_code(failure), message, receipt));
    }
    if let Some(message) = readback_failure {
      return Err(IndexOperationControlPublicationErrorV1::committed("index_operation_committed_readback", message, receipt));
    }
    Ok(receipt)
  }

  pub fn publish_index_artifacts(
    &self,
    request: IndexArtifactBatchPublicationRequestV1<'_>,
  ) -> Result<IndexArtifactBatchPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    self.publish_index_artifacts_with_observer(request, &mut observer)
  }

  fn publish_index_artifacts_with_observer(
    &self,
    request: IndexArtifactBatchPublicationRequestV1<'_>,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<IndexArtifactBatchPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      FirstAuthorityPublicationErrorV1::StateLockPoisoned
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded || header.head_hash.iter().all(|byte| *byte == 0) {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_missing_authority",
        "immutable IndexArtifact publication requires selected non-degraded first authority",
      ));
    }
    if &header.database_id != request.database_id {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_database_mismatch",
        "immutable IndexArtifact batch belongs to another logical database",
      ));
    }
    if request.artifacts.is_empty() || request.artifacts.len() > INDEX_ARTIFACT_BATCH_COUNT_CAP {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_batch_count",
        format!("immutable IndexArtifact batch count {} is outside 1..={INDEX_ARTIFACT_BATCH_COUNT_CAP}", request.artifacts.len()),
      ));
    }

    let mut unique = HashSet::new();
    unique.try_reserve(request.artifacts.len()).map_err(|error| {
      FirstAuthorityPublicationErrorV1::invalid("immutable_index_batch_allocation", format!("artifact identity allocation failed: {error}"))
    })?;
    let mut total_value_bytes = 0usize;
    for artifact in request.artifacts {
      let decoded = decode_immutable_index_artifact(
        &artifact.value,
        header.hash_algorithm,
        ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length(),
      )?;
      if decoded.key != artifact.key {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "immutable_index_prepared_mismatch",
          "immutable IndexArtifact key disagrees with its encoded value",
        ));
      }
      if !unique.insert(artifact.key.as_slice()) {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "immutable_index_duplicate",
          "immutable IndexArtifact batch contains a duplicate key",
        ));
      }
      total_value_bytes = total_value_bytes.checked_add(artifact.value.len()).ok_or_else(|| {
        FirstAuthorityPublicationErrorV1::invalid("immutable_index_batch_bytes", "immutable IndexArtifact batch bytes overflowed")
      })?;
      if total_value_bytes > INDEX_ARTIFACT_BATCH_BYTES_CAP {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "immutable_index_batch_bytes",
          format!("immutable IndexArtifact batch exceeds {INDEX_ARTIFACT_BATCH_BYTES_CAP} bytes"),
        ));
      }
    }

    let mut kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    kv.flush()?;
    validate_kv_header_alignment(&kv, header)?;
    if kv.write_buffer_len() != 0 || kv.hot_buffer_len() != 0 {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_baseline_not_flushed",
        "immutable IndexArtifact publication requires an empty KV write and hot-buffer baseline",
      ));
    }

    let mut receipts = Vec::new();
    receipts.try_reserve_exact(request.artifacts.len()).map_err(|error| {
      FirstAuthorityPublicationErrorV1::invalid("immutable_index_receipt_allocation", format!("receipt allocation failed: {error}"))
    })?;
    let mut entities = Vec::new();
    entities.try_reserve_exact(request.artifacts.len()).map_err(|error| {
      FirstAuthorityPublicationErrorV1::invalid("immutable_index_entity_allocation", format!("entity allocation failed: {error}"))
    })?;
    let mut next_write_sequence = header.write_sequence_high_water;
    for artifact in request.artifacts {
      if let Some(existing) = load_index_artifact_entity(&self.file, &kv, header, &artifact.key)? {
        if existing.value != artifact.value {
          return Err(FirstAuthorityPublicationErrorV1::invalid(
            "immutable_index_identity_collision",
            "existing immutable IndexArtifact differs from its exact canonical bytes",
          ));
        }
        receipts.push(IndexArtifactPublicationReceiptV1 {
          artifact_key: artifact.key.clone(),
          write_sequence: existing.write_sequence,
          idempotent: true,
        });
        continue;
      }
      next_write_sequence = next_write_sequence.checked_add(1).ok_or_else(|| {
        FirstAuthorityPublicationErrorV1::invalid("immutable_index_write_sequence_exhausted", "v4 write sequence is exhausted")
      })?;
      let entity_bytes = encode_entity(
        EntryTypeV4::IndexArtifact,
        WHOLE_ENTITY_V1_FLAG_SYSTEM,
        header.hash_algorithm,
        header.updated_at_ms.max(request.publication_timestamp_ms),
        next_write_sequence,
        &artifact.key,
        &artifact.value,
      )?;
      entities.push(PreparedWholeEntityV1 { key: artifact.key.clone(), kv_type: kv_tag::INDEX_ARTIFACT, bytes: entity_bytes });
      receipts.push(IndexArtifactPublicationReceiptV1 {
        artifact_key: artifact.key.clone(),
        write_sequence: next_write_sequence,
        idempotent: false,
      });
    }
    if entities.is_empty() {
      return Ok(IndexArtifactBatchPublicationReceiptV1 { artifacts: receipts, observation, idempotent: true });
    }

    let append_start = self.file.metadata().map_err(EngineError::IoError)?.len();
    if append_start < header.hot_tail_offset {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_file_truncated",
        "database length precedes the selected v4 hot-tail offset",
      ));
    }
    let expected_hot_tail_offset = entities.iter().try_fold(append_start, |offset, entity| {
      offset.checked_add(entity.bytes.len() as u64).ok_or_else(|| {
        FirstAuthorityPublicationErrorV1::invalid("immutable_index_wal_overflow", "immutable IndexArtifact WAL offset overflowed")
      })
    })?;
    let orphan_prefix_bytes = append_start.checked_sub(header.hot_tail_offset).ok_or_else(|| {
      FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_wal_prefix",
        "immutable IndexArtifact append precedes the selected hot tail",
      )
    })?;
    let dependency_bytes = entity_dependency_bytes(&entities, header.hash_algorithm.hash_length())?
      .checked_add(orphan_prefix_bytes)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("immutable_index_dependency_bytes", "dependency byte count overflowed"))?;
    let entry_increment = u64::try_from(entities.len()).map_err(|error| {
      FirstAuthorityPublicationErrorV1::invalid("immutable_index_entry_count", format!("entity count exceeds u64: {error}"))
    })?;
    let mut candidate = header.clone();
    candidate.updated_at_ms = header.updated_at_ms.max(request.publication_timestamp_ms);
    candidate.write_sequence_high_water = next_write_sequence;
    candidate.hot_tail_offset = expected_hot_tail_offset;
    candidate.entry_count = header
      .entry_count
      .checked_add(entry_increment)
      .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("immutable_index_entry_count", "v4 entry count overflowed"))?;
    let admitted =
      self.header_publisher.admit_inactive_slot_with_dependency_bytes(&self.file, &observation, candidate, dependency_bytes)?;
    let authority_sequence = admitted.sequence();
    let batch = kv.begin_atomic_visibility_batch(entities.len(), authority_sequence)?;
    let mut expected_existing = Vec::new();
    expected_existing.try_reserve_exact(entities.len()).map_err(|error| {
      FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_expectation_allocation",
        format!("dependency expectation allocation failed: {error}"),
      )
    })?;
    expected_existing.resize(entities.len(), None);
    let (publication_result, append_completed) = {
      let mut dependency = FirstAuthorityDependencyV1 {
        file: &self.file,
        kv: &mut kv,
        batch,
        expected_publication_sequence: authority_sequence,
        entities: &entities,
        start_offset: append_start,
        expected_hot_tail_offset,
        expected_existing: &expected_existing,
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
        "immutable_index_dependency_missing",
        "header publication completed without the exact immutable IndexArtifact dependency append",
      ));
    }
    kv.complete_hot_tail_dependency();
    kv.publish_atomic_visibility_after_authority(batch, &publication.durability)?;
    observer.authority_committed(&kv, &entities)?;

    for artifact in request.artifacts {
      let stored =
        load_index_artifact_entity(&self.file, &kv, &publication.observation.selected.header, &artifact.key)?.ok_or_else(|| {
          FirstAuthorityPublicationErrorV1::invalid(
            "immutable_index_readback_missing",
            "published immutable IndexArtifact locator is absent",
          )
        })?;
      if stored.value != artifact.value {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "immutable_index_readback_mismatch",
          "published immutable IndexArtifact differs from its exact prepared bytes",
        ));
      }
    }
    Ok(IndexArtifactBatchPublicationReceiptV1 { artifacts: receipts, observation: publication.observation, idempotent: false })
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
    &self,
    request: GcControlPublicationRequestV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<GcControlPublicationOutcomeV1, GcControlPublicationErrorV1> {
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      GcControlPublicationErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    self.publish_gc_active_control_locked(request, retirement_owner, observer)
  }

  fn publish_gc_active_control_locked(
    &self,
    request: GcControlPublicationRequestV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<GcControlPublicationOutcomeV1, GcControlPublicationErrorV1> {
    let expected_control_kind = request.expected_control_kind;
    let encoded_control = request.encoded_control;
    let publication_timestamp_ms = request.publication_timestamp_ms;
    let monotonic_now_ms = request.monotonic_now_ms;
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
        commit_stable_entity_dependency(
          &self.file,
          &mut kv,
          admitted,
          batch,
          authority_sequence,
          &entities,
          append_start,
          expected_hot_tail_offset,
          &[expected_existing.cloned()],
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
      commit_stable_entity_dependency(
        &self.file,
        &mut kv,
        admitted,
        batch,
        authority_sequence,
        &entities,
        append_start,
        expected_hot_tail_offset,
        &[expected_existing.cloned()],
        observer,
      )?
    };
    let (publication, committed_failure) = match publication {
      StableEntityDependencyOutcomeV1::Complete(publication) => (publication, None),
      StableEntityDependencyOutcomeV1::CommittedFailure { publication, failure, message } => {
        (publication, Some((stable_dependency_to_gc_failure(failure), message)))
      }
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

  pub fn publish_physical_quarantine(
    &mut self,
    request: PhysicalQuarantinePublicationRequestV1<'_>,
    authority_verifier: &mut dyn PhysicalQuarantineAuthorityVerifierV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  ) -> Result<PhysicalQuarantinePublicationReceiptV1, PhysicalQuarantinePublicationErrorV1> {
    self.publish_physical_quarantine_with_control_observer(
      request,
      authority_verifier,
      retirement_owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
  }

  fn publish_physical_quarantine_with_control_observer(
    &mut self,
    request: PhysicalQuarantinePublicationRequestV1<'_>,
    authority_verifier: &mut dyn PhysicalQuarantineAuthorityVerifierV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<PhysicalQuarantinePublicationReceiptV1, PhysicalQuarantinePublicationErrorV1> {
    let validated = validate_physical_quarantine_publication(&request)?;
    if request.cancellation.is_cancelled() {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_publication_canceled",
        "physical-quarantine publication was canceled before authority exclusion",
      ));
    }
    if request.pin_coordinator.hash_algorithm() != request.permit.hash_algorithm() {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_publication_pin_identity",
        "request-pin coordinator hash profile differs from the quarantine publication permit",
      ));
    }
    if retirement_owner.hash_algorithm() != request.permit.hash_algorithm()
      || retirement_owner.database_id() != *request.permit.database_id()
    {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_publication_lineage_identity",
        "retirement lineage owner differs from the quarantine database or hash profile",
      ));
    }
    retirement_owner.flush(self)?;

    let mut locked_result = None;
    let exclusion_result = request.pin_coordinator.with_global_exclusion(request.cancellation, || {
      locked_result =
        Some(self.publish_physical_quarantine_excluded(&request, &validated, authority_verifier, retirement_owner, control_observer));
      Ok(())
    });
    let exclusion_error = exclusion_result.err();
    let (locked, exclusion_error) = match (locked_result, exclusion_error) {
      (Some(Ok(locked)), exclusion_error) => (locked, exclusion_error),
      (Some(Err(operation_error)), Some(exclusion_error)) => {
        return Err(PhysicalQuarantinePublicationErrorV1::invalid(
          "quarantine_publication_exclusion_cleanup",
          format!("quarantine publication failed with {operation_error}; releasing global exclusion also failed with {exclusion_error}"),
        ));
      }
      (None, Some(error)) => return Err(PhysicalQuarantinePublicationErrorV1::Pin(error)),
      (Some(Err(error)), None) => return Err(error),
      (None, None) => {
        return Err(PhysicalQuarantinePublicationErrorV1::invalid(
          "quarantine_publication_internal",
          "global request-pin exclusion returned no publication result",
        ));
      }
    };

    let mut committed_failure = locked.committed_failure.map(|(failure, message)| (failure.code(), message));
    if let Some(error) = exclusion_error {
      if let Some((_, message)) = &mut committed_failure {
        message.push_str(&format!("; releasing global request-pin exclusion also failed: {error}"));
      } else {
        committed_failure = Some(("quarantine_publication_committed_pin_cleanup", error.to_string()));
      }
    }
    let lineage_state = if locked.control.replaced_control {
      match retirement_owner.flush(self) {
        Ok(true) => PhysicalQuarantineLineageStateV1::HardPublished {
          hard_publication_sequence: retirement_owner.status().last_hard_publication_sequence,
        },
        Ok(false) => PhysicalQuarantineLineageStateV1::MissingAfterCommit {
          code: "quarantine_publication_lineage_missing",
          message: "quarantine control replacement committed without retained retirement lineage".to_string(),
        },
        Err(source) => PhysicalQuarantineLineageStateV1::BufferedAfterFlushFailure { code: source.code(), message: source.to_string() },
      }
    } else {
      PhysicalQuarantineLineageStateV1::NotRequired
    };
    if !matches!(lineage_state, PhysicalQuarantineLineageStateV1::NotRequired | PhysicalQuarantineLineageStateV1::HardPublished { .. })
      && committed_failure.is_none()
    {
      committed_failure =
        Some(("quarantine_publication_committed_lineage", "quarantine replacement lineage did not hard-publish".to_string()));
    }
    let observation = if matches!(lineage_state, PhysicalQuarantineLineageStateV1::HardPublished { .. }) {
      match self.observe() {
        Ok(observation) => observation,
        Err(source) => {
          if committed_failure.is_none() {
            committed_failure = Some(("quarantine_publication_committed_lineage_readback", source.to_string()));
          }
          locked.control.observation.clone()
        }
      }
    } else {
      locked.control.observation.clone()
    };
    let receipt = PhysicalQuarantinePublicationReceiptV1 {
      quarantine_manifest_key: request.quarantine_manifest.key.clone(),
      quarantine_manifest_write_sequence: locked.quarantine_manifest_write_sequence,
      quarantine_control_key: request.quarantine_control.key.clone(),
      quarantine_control_write_sequence: locked.control.control_write_sequence,
      quarantine_control_slot: locked.control.control_slot,
      lineage_state,
      observation,
      idempotent: locked.control.idempotent,
    };
    if let Some((code, message)) = committed_failure {
      return Err(PhysicalQuarantinePublicationErrorV1::committed(code, message, receipt));
    }
    Ok(receipt)
  }

  fn verify_physical_quarantine_support_is_durable(
    &self,
    request: &PhysicalQuarantinePublicationRequestV1<'_>,
    validated: &ValidatedPhysicalQuarantinePublicationV1<'_>,
  ) -> Result<(), PhysicalQuarantinePublicationErrorV1> {
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.hash_algorithm != request.permit.hash_algorithm()
      || header.database_id != *request.permit.database_id()
    {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_publication_database_authority",
        "selected first authority is degraded or differs from the quarantine publication permit",
      ));
    }
    let memory = request.pin_coordinator.memory_coordinator();
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let reader = PhysicalQuarantineSupportReadContextV1 { file: &self.file, kv: &kv, header, memory: &memory };
    let lifecycle_entity =
      reader.load_entity(validated.manifest.captured_root_lifecycle_manifest, GcArtifactKindV1::RootLifecycleManifest)?;
    let lifecycle_whole = decode_whole_entity(&lifecycle_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    let GcStateArtifactV1::Manifest(lifecycle) = decode_gc_state_artifact(lifecycle_whole.stored_value, header.hash_algorithm)? else {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_lifecycle_kind",
        "captured root-lifecycle key resolves to another GC artifact kind",
      ));
    };
    if lifecycle.key != validated.manifest.captured_root_lifecycle_manifest || lifecycle.database_id != validated.manifest.database_id {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_lifecycle_changed",
        "captured root-lifecycle manifest identity differs from the quarantine basis",
      ));
    }

    let root_entity = match validated.manifest.candidate_directory_root {
      Some(root) => Some(reader.load_entity(root, GcArtifactKindV1::GcArtifactDirectoryNode)?),
      None => None,
    };
    let root_directory = match &root_entity {
      Some(entity) => {
        let whole = decode_whole_entity(&entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
        let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(whole.stored_value, header.hash_algorithm)? else {
          return Err(PhysicalQuarantinePublicationErrorV1::invalid(
            "quarantine_support_kind",
            "candidate-directory root resolves to another GC artifact kind",
          ));
        };
        Some(directory)
      }
      None => None,
    };
    let maximum_support_artifacts = request.permit.support_closure().support_artifact_count.max(1);
    let mut validator = super::gc_quarantine::QuarantineClosureValidatorV1::new(
      &validated.manifest,
      root_directory.as_ref(),
      &lifecycle,
      header.hash_algorithm,
      request.cancellation.clone(),
      super::gc_quarantine::QuarantineClosureLimitsV1 { maximum_support_artifacts },
      &memory,
    )
    .map_err(physical_quarantine_support_error)?;
    if let Some(root) = root_directory.as_ref() {
      reader.revalidate_subtree(root, 0, &mut validator)?;
    }
    for delta_hash in validated.manifest.delta_hashes.chunks_exact(header.hash_algorithm.hash_length()) {
      let delta_entity = reader.load_entity(delta_hash, GcArtifactKindV1::CandidateDelta)?;
      let whole = decode_whole_entity(&delta_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
      validator.observe_delta(whole.stored_value).map_err(physical_quarantine_support_error)?;
    }
    let observed = validator.finish().map_err(physical_quarantine_support_error)?;
    if observed != *request.permit.support_closure() {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_changed",
        "durable physical-quarantine support closure differs from the qualified publication permit",
      ));
    }
    Ok(())
  }

  fn publish_physical_quarantine_excluded(
    &self,
    request: &PhysicalQuarantinePublicationRequestV1<'_>,
    validated: &ValidatedPhysicalQuarantinePublicationV1<'_>,
    authority_verifier: &mut dyn PhysicalQuarantineAuthorityVerifierV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<PhysicalQuarantineLockedPublicationV1, PhysicalQuarantinePublicationErrorV1> {
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      PhysicalQuarantinePublicationErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.hash_algorithm != request.permit.hash_algorithm()
      || header.database_id != *request.permit.database_id()
    {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_publication_database_authority",
        "selected first authority is degraded or differs from the quarantine publication permit",
      ));
    }
    if request.cancellation.is_cancelled() {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_publication_canceled",
        "physical-quarantine publication was canceled during final authority recheck",
      ));
    }
    self.verify_physical_quarantine_support_is_durable(request, validated)?;
    let selected_control = {
      let kv = self.lock_kv()?;
      validate_kv_header_alignment(&kv, header)?;
      select_physical_quarantine_control(&self.file, &kv, header)?
    };
    let exact_retry = selected_control.stored_value == request.quarantine_control.value
      && selected_control.target_manifest_hash == request.quarantine_manifest.key;
    if !exact_retry {
      if selected_control.target_manifest_hash != request.permit.prior_manifest_hash() {
        return Err(PhysicalQuarantinePublicationErrorV1::invalid(
          "quarantine_publication_prior_authority_changed",
          "selected quarantine authority no longer matches the qualified prior manifest",
        ));
      }
      let authority = authority_verifier.recheck_physical_quarantine_authority(PhysicalQuarantineAuthorityRecheckRequestV1 {
        hash_algorithm: header.hash_algorithm,
        database_id: header.database_id,
        prior_manifest_hash: request.permit.prior_manifest_hash(),
        next_manifest_hash: request.permit.next_manifest_hash(),
        mark_generation: request.permit.mark_generation(),
        expected_authority_root_set_digest: validated.manifest.authority_root_set_digest,
        expected_semantic_state_digest: validated.manifest.semantic_state_digest,
        expected_kv_layout_fingerprint: validated.manifest.kv_layout_fingerprint,
        expected_mark_result_digest: validated.manifest.mark_result_digest,
        expected_root_lifecycle_manifest: validated.manifest.captured_root_lifecycle_manifest,
      })?;
      if authority.selected_complete_mark_generation != request.permit.mark_generation()
        || authority.authority_root_set_digest != validated.manifest.authority_root_set_digest
        || authority.semantic_state_digest != validated.manifest.semantic_state_digest
        || authority.kv_layout_fingerprint != validated.manifest.kv_layout_fingerprint
        || authority.mark_result_digest != validated.manifest.mark_result_digest
        || authority.selected_root_lifecycle_manifest != validated.manifest.captured_root_lifecycle_manifest
        || !authority.physical_inventory_and_lineage_complete
        || !authority.all_candidate_incarnations_exact_and_unreachable
        || !authority.task_and_audit_pins_absent
      {
        return Err(PhysicalQuarantinePublicationErrorV1::invalid(
          "quarantine_publication_authority_changed",
          "complete-mark, inventory, lineage, lifecycle, locator, task, or audit authority changed before publication",
        ));
      }
    }

    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    let quarantine_manifest_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::QuarantineManifest,
        database_id: request.permit.database_id(),
        artifact_key: &request.quarantine_manifest.key,
        value: &request.quarantine_manifest.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "quarantine_manifest_committed_postcondition",
      },
      &mut observer,
    )?;
    let control = self.publish_gc_active_control_locked(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::QuarantineActiveControl,
        encoded_control: request.quarantine_control,
        publication_timestamp_ms: request.publication_timestamp_ms,
        monotonic_now_ms: request.monotonic_now_ms,
      },
      retirement_owner,
      control_observer,
    )?;
    let (control, committed_failure) = match control {
      GcControlPublicationOutcomeV1::Complete(control) => (control, None),
      GcControlPublicationOutcomeV1::CommittedFailure { publication, failure, message } => (publication, Some((failure, message))),
    };
    debug_assert_eq!(control.control_slot, validated.control.slot);
    Ok(PhysicalQuarantineLockedPublicationV1 { quarantine_manifest_write_sequence, control, committed_failure })
  }

  pub fn publish_root_retirement(
    &mut self,
    request: RootRetirementPublicationRequestV1<'_>,
    authority_verifier: &mut dyn RootRetirementAuthorityVerifierV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  ) -> Result<RootRetirementPublicationReceiptV1, RootRetirementPublicationErrorV1> {
    self.publish_root_retirement_with_control_observer(
      request,
      authority_verifier,
      retirement_owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
  }

  fn publish_root_retirement_with_control_observer(
    &mut self,
    request: RootRetirementPublicationRequestV1<'_>,
    authority_verifier: &mut dyn RootRetirementAuthorityVerifierV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<RootRetirementPublicationReceiptV1, RootRetirementPublicationErrorV1> {
    let validated = validate_root_retirement_publication(&request)?;
    if request.cancellation.is_cancelled() {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_canceled",
        "root retirement was canceled before authority exclusion",
      ));
    }
    if request.pin_coordinator.hash_algorithm() != request.hash_algorithm {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_pin_identity",
        "root-pin coordinator hash profile differs from the retirement closure",
      ));
    }
    if retirement_owner.hash_algorithm() != request.hash_algorithm
      || retirement_owner.database_id() != request.support_closure.database_id()
    {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_lineage_identity",
        "retirement lineage owner differs from the retirement closure database or hash profile",
      ));
    }
    self.verify_root_retirement_support_is_durable(&request)?;

    retirement_owner.flush(self)?;
    let mut locked_result = None;
    let exclusion_result =
      request.pin_coordinator.with_retirement_exclusion(&request.intent.namespace_root_hash, request.cancellation, || {
        locked_result =
          Some(self.publish_root_retirement_excluded(&request, &validated, authority_verifier, retirement_owner, control_observer));
        Ok(())
      });
    let exclusion_error = exclusion_result.err();
    let (locked, exclusion_error) = match (locked_result, exclusion_error) {
      (Some(Ok(locked)), exclusion_error) => (locked, exclusion_error),
      (Some(Err(operation_error)), Some(exclusion_error)) => {
        return Err(RootRetirementPublicationErrorV1::invalid(
          "root_retirement_exclusion_cleanup",
          format!("retirement failed with {operation_error}; releasing the exclusion also failed with {exclusion_error}"),
        ));
      }
      (None, Some(exclusion_error)) => {
        return Err(RootRetirementPublicationErrorV1::Pin(exclusion_error));
      }
      (Some(Err(error)), None) => return Err(error),
      (None, None) => {
        return Err(RootRetirementPublicationErrorV1::invalid("root_retirement_internal", "retirement exclusion returned no result"));
      }
    };

    let mut committed_failure = locked.committed_failure.map(|(failure, message)| (failure.code(), message));
    if let Some(error) = exclusion_error {
      if let Some((_, message)) = &mut committed_failure {
        message.push_str(&format!("; releasing the root retirement exclusion also failed: {error}"));
      } else {
        committed_failure = Some(("root_retirement_committed_pin_cleanup", error.to_string()));
      }
    }
    let lineage_state = if locked.control.replaced_control {
      match retirement_owner.flush(self) {
        Ok(true) => RootRetirementLineageStateV1::HardPublished {
          hard_publication_sequence: retirement_owner.status().last_hard_publication_sequence,
        },
        Ok(false) => RootRetirementLineageStateV1::MissingAfterCommit {
          code: "root_retirement_lineage_missing",
          message: "lifecycle control replacement committed without retained retirement lineage".to_string(),
        },
        Err(source) => RootRetirementLineageStateV1::BufferedAfterFlushFailure { code: source.code(), message: source.to_string() },
      }
    } else {
      RootRetirementLineageStateV1::NotRequired
    };
    if !matches!(lineage_state, RootRetirementLineageStateV1::NotRequired | RootRetirementLineageStateV1::HardPublished { .. })
      && committed_failure.is_none()
    {
      committed_failure = Some(("root_retirement_committed_lineage", "retirement lineage did not hard-publish".to_string()));
    }
    let observation = if matches!(lineage_state, RootRetirementLineageStateV1::HardPublished { .. }) {
      match self.observe() {
        Ok(observation) => observation,
        Err(source) => {
          if committed_failure.is_none() {
            committed_failure = Some(("root_retirement_committed_lineage_readback", source.to_string()));
          }
          locked.control.observation
        }
      }
    } else {
      locked.control.observation
    };
    let receipt = RootRetirementPublicationReceiptV1 {
      namespace_root_hash: request.intent.namespace_root_hash.clone(),
      retirement_commit_key: request.retirement_commit.key.clone(),
      retirement_commit_write_sequence: locked.retirement_commit_write_sequence,
      expiry_manifest_key: request.expiry_manifest.key.clone(),
      expiry_manifest_write_sequence: locked.expiry_manifest_write_sequence,
      lifecycle_manifest_key: request.lifecycle_manifest.key.clone(),
      lifecycle_manifest_write_sequence: locked.lifecycle_manifest_write_sequence,
      lifecycle_control_key: request.lifecycle_control.key.clone(),
      lifecycle_control_write_sequence: locked.control.control_write_sequence,
      lifecycle_control_slot: locked.control.control_slot,
      lineage_state,
      observation,
      idempotent: locked.control.idempotent,
    };
    if let Some((code, message)) = committed_failure {
      return Err(RootRetirementPublicationErrorV1::committed(code, message, receipt));
    }
    Ok(receipt)
  }

  fn verify_root_retirement_support_is_durable(
    &self,
    request: &RootRetirementPublicationRequestV1<'_>,
  ) -> Result<(), RootRetirementPublicationErrorV1> {
    let lifecycle = decode_root_lifecycle_manifest_v1(&request.lifecycle_manifest.value, request.hash_algorithm)?;
    let expiry = decode_root_expiry_manifest_v1(&request.expiry_manifest.value, request.hash_algorithm)?;
    let retirement = decode_root_retirement_commit_v1(&request.retirement_commit.value, request.hash_algorithm)?;
    let memory = request.pin_coordinator.memory_coordinator();
    let mut builder = super::gc_lifecycle::RootLifecycleSupportClosureBuilderV1::new_for_retirement(
      &lifecycle,
      &expiry,
      &retirement,
      request.hash_algorithm,
      request.cancellation,
      super::gc_lifecycle::RootLifecycleSupportLimitsV1 {
        maximum_candidate_records: lifecycle.candidate_count,
        maximum_expiry_records: expiry.record_count,
        maximum_support_artifacts: request.support_closure.support_artifact_count(),
      },
      &memory,
    )
    .map_err(root_retirement_support_error)?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.hash_algorithm != request.hash_algorithm
      || header.database_id != request.support_closure.database_id()
    {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_database_authority",
        "selected first authority is degraded or differs from the retirement closure",
      ));
    }
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let support_reader = RootLifecycleSupportReadContextV1 { file: &self.file, kv: &kv, header, memory: &memory };
    if let Some(root) = request.support_closure.candidate_directory_hash() {
      support_reader.revalidate_subtree(root, GcDirectoryRoleV1::RootCandidates, None, 0, &mut builder)?;
    }
    if let Some(root) = request.support_closure.expiry_directory_hash() {
      support_reader.revalidate_subtree(root, GcDirectoryRoleV1::RootExpiry, None, 0, &mut builder)?;
    }
    let observed = builder.finish().map_err(root_retirement_support_error)?;
    if observed != *request.support_closure {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_changed",
        "durable root-lifecycle support closure differs from the validated request closure",
      ));
    }
    Ok(())
  }

  fn publish_root_retirement_excluded(
    &self,
    request: &RootRetirementPublicationRequestV1<'_>,
    validated: &ValidatedRootRetirementPublicationV1<'_>,
    authority_verifier: &mut dyn RootRetirementAuthorityVerifierV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<RootRetirementLockedPublicationV1, RootRetirementPublicationErrorV1> {
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      RootRetirementPublicationErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    let database_id = request.support_closure.database_id();
    if observation.selected.redundancy_degraded || header.hash_algorithm != request.hash_algorithm || header.database_id != database_id {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_database_authority",
        "selected first authority is degraded or differs from the retirement closure",
      ));
    }
    if header.head_hash == request.intent.namespace_root_hash {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_current_head",
        "the selected HEAD cannot be logically retired",
      ));
    }
    if request.cancellation.is_cancelled() {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_canceled",
        "root retirement was canceled during final authority recheck",
      ));
    }

    let selected_control = {
      let kv = self.lock_kv()?;
      validate_kv_header_alignment(&kv, header)?;
      select_root_lifecycle_control(&self.file, &kv, header)?
    };
    let exact_retry = selected_control.stored_value == request.lifecycle_control.value
      && selected_control.target_manifest_hash == request.lifecycle_manifest.key;
    if !exact_retry {
      if selected_control.target_manifest_hash != request.intent.prior_lifecycle_manifest_hash {
        return Err(RootRetirementPublicationErrorV1::invalid(
          "root_retirement_prior_lifecycle_changed",
          "selected lifecycle authority no longer matches the retirement intent",
        ));
      }
      let admission_payload = {
        let kv = self.lock_kv()?;
        validate_kv_header_alignment(&kv, header)?;
        load_system_file(&self.file, &kv, header, SystemControlKindV1::RootAdmissionCommit, &request.intent.namespace_root_hash)?
      };
      let admission = decode_root_admission_commit(&admission_payload, header.hash_algorithm)?;
      if admission.database_id != header.database_id
        || admission.namespace_root != request.intent.namespace_root_hash
        || digest_parts(header.hash_algorithm, &[&admission_payload]) != request.intent.admission_commit_payload_hash
      {
        return Err(RootRetirementPublicationErrorV1::invalid(
          "root_retirement_admission_changed",
          "root admission evidence no longer matches the frozen retirement intent",
        ));
      }
      let authority = authority_verifier.recheck_authority_roots(RootRetirementAuthorityRecheckRequestV1 {
        hash_algorithm: header.hash_algorithm,
        database_id: header.database_id,
        namespace_root_hash: &request.intent.namespace_root_hash,
        expected_authority_root_set_digest: &request.intent.authority_root_set_digest,
        final_mark_generation: request.intent.final_mark_generation,
      })?;
      if authority.target_is_authoritative || authority.authority_root_set_digest != request.intent.authority_root_set_digest {
        return Err(RootRetirementPublicationErrorV1::invalid(
          "root_retirement_authority_changed",
          "caller-owned authority roots changed or now retain the retirement target",
        ));
      }
    }

    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    let retirement_commit_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::RootRetirementCommit,
        database_id: &database_id,
        artifact_key: &request.retirement_commit.key,
        value: &request.retirement_commit.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "root_retirement_commit_committed_postcondition",
      },
      &mut observer,
    )?;
    let expiry_manifest_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::RootExpiryCatalogManifest,
        database_id: &database_id,
        artifact_key: &request.expiry_manifest.key,
        value: &request.expiry_manifest.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "root_expiry_manifest_committed_postcondition",
      },
      &mut observer,
    )?;
    let lifecycle_manifest_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::RootLifecycleManifest,
        database_id: &database_id,
        artifact_key: &request.lifecycle_manifest.key,
        value: &request.lifecycle_manifest.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "root_lifecycle_manifest_committed_postcondition",
      },
      &mut observer,
    )?;
    let control = self.publish_gc_active_control_locked(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::RootLifecycleActiveControl,
        encoded_control: request.lifecycle_control,
        publication_timestamp_ms: request.publication_timestamp_ms,
        monotonic_now_ms: request.monotonic_now_ms,
      },
      retirement_owner,
      control_observer,
    )?;
    let (control, committed_failure) = match control {
      GcControlPublicationOutcomeV1::Complete(control) => (control, None),
      GcControlPublicationOutcomeV1::CommittedFailure { publication, failure, message } => (publication, Some((failure, message))),
    };
    debug_assert_eq!(control.control_slot, validated.lifecycle_control.slot);
    Ok(RootRetirementLockedPublicationV1 {
      retirement_commit_write_sequence,
      expiry_manifest_write_sequence,
      lifecycle_manifest_write_sequence,
      control,
      committed_failure,
    })
  }

  pub fn publish_root_reclaim(
    &mut self,
    request: RootReclaimPublicationRequestV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  ) -> Result<RootReclaimPublicationReceiptV1, RootReclaimPublicationErrorV1> {
    self.publish_root_reclaim_with_control_observer(request, retirement_owner, &mut NoopFirstAuthorityDependencyObserverV1)
  }

  fn publish_root_reclaim_with_control_observer(
    &mut self,
    request: RootReclaimPublicationRequestV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<RootReclaimPublicationReceiptV1, RootReclaimPublicationErrorV1> {
    let validated = validate_root_reclaim_publication(&request)?;
    if request.cancellation.is_cancelled() {
      return Err(RootReclaimPublicationErrorV1::invalid("root_reclaim_canceled", "root reclaim was canceled before authority exclusion"));
    }
    if request.pin_coordinator.hash_algorithm() != request.hash_algorithm {
      return Err(RootReclaimPublicationErrorV1::invalid(
        "root_reclaim_pin_identity",
        "root-pin coordinator hash profile differs from the reclaim closure",
      ));
    }
    if retirement_owner.hash_algorithm() != request.hash_algorithm
      || retirement_owner.database_id() != request.support_closure.database_id()
    {
      return Err(RootReclaimPublicationErrorV1::invalid(
        "root_reclaim_lineage_identity",
        "retirement lineage owner differs from the reclaim closure database or hash profile",
      ));
    }
    self.verify_root_reclaim_support_is_durable(&request)?;

    retirement_owner.flush(self)?;
    let mut locked_result = None;
    let exclusion_result =
      request.pin_coordinator.with_retirement_exclusion(request.retention_permit.namespace_root_hash(), request.cancellation, || {
        locked_result = Some(self.publish_root_reclaim_excluded(&request, &validated, retirement_owner, control_observer));
        Ok(())
      });
    let exclusion_error = exclusion_result.err();
    let (locked, exclusion_error) = match (locked_result, exclusion_error) {
      (Some(Ok(locked)), exclusion_error) => (locked, exclusion_error),
      (Some(Err(operation_error)), Some(exclusion_error)) => {
        return Err(RootReclaimPublicationErrorV1::invalid(
          "root_reclaim_exclusion_cleanup",
          format!("reclaim failed with {operation_error}; releasing the exclusion also failed with {exclusion_error}"),
        ));
      }
      (None, Some(exclusion_error)) => return Err(RootReclaimPublicationErrorV1::Pin(exclusion_error)),
      (Some(Err(error)), None) => return Err(error),
      (None, None) => {
        return Err(RootReclaimPublicationErrorV1::invalid("root_reclaim_internal", "reclaim exclusion returned no result"));
      }
    };

    let mut committed_failure = locked.committed_failure.map(|(failure, message)| (failure.code(), message));
    if let Some(error) = exclusion_error {
      if let Some((_, message)) = &mut committed_failure {
        message.push_str(&format!("; releasing the root reclaim exclusion also failed: {error}"));
      } else {
        committed_failure = Some(("root_reclaim_committed_pin_cleanup", error.to_string()));
      }
    }
    let lineage_state = if locked.control.replaced_control {
      match retirement_owner.flush(self) {
        Ok(true) => {
          RootReclaimLineageStateV1::HardPublished { hard_publication_sequence: retirement_owner.status().last_hard_publication_sequence }
        }
        Ok(false) => RootReclaimLineageStateV1::MissingAfterCommit {
          code: "root_reclaim_lineage_missing",
          message: "lifecycle control replacement committed without retained retirement lineage".to_string(),
        },
        Err(source) => RootReclaimLineageStateV1::BufferedAfterFlushFailure { code: source.code(), message: source.to_string() },
      }
    } else {
      RootReclaimLineageStateV1::NotRequired
    };
    if !matches!(lineage_state, RootReclaimLineageStateV1::NotRequired | RootReclaimLineageStateV1::HardPublished { .. })
      && committed_failure.is_none()
    {
      committed_failure = Some(("root_reclaim_committed_lineage", "reclaim lineage did not hard-publish".to_string()));
    }
    let observation = if matches!(lineage_state, RootReclaimLineageStateV1::HardPublished { .. }) {
      match self.observe() {
        Ok(observation) => observation,
        Err(source) => {
          if committed_failure.is_none() {
            committed_failure = Some(("root_reclaim_committed_lineage_readback", source.to_string()));
          }
          locked.control.observation
        }
      }
    } else {
      locked.control.observation
    };
    let receipt = RootReclaimPublicationReceiptV1 {
      namespace_root_hash: request.retention_permit.namespace_root_hash().to_vec(),
      root_object_reclaim_proof_key: request.root_object_reclaim_proof.key.clone(),
      root_object_reclaim_proof_write_sequence: locked.root_object_reclaim_proof_write_sequence,
      expiry_manifest_key: request.expiry_manifest.key.clone(),
      expiry_manifest_write_sequence: locked.expiry_manifest_write_sequence,
      lifecycle_manifest_key: request.lifecycle_manifest.key.clone(),
      lifecycle_manifest_write_sequence: locked.lifecycle_manifest_write_sequence,
      lifecycle_control_key: request.lifecycle_control.key.clone(),
      lifecycle_control_write_sequence: locked.control.control_write_sequence,
      lifecycle_control_slot: locked.control.control_slot,
      lineage_state,
      observation,
      idempotent: locked.control.idempotent,
    };
    if let Some((code, message)) = committed_failure {
      return Err(RootReclaimPublicationErrorV1::committed(code, message, receipt));
    }
    Ok(receipt)
  }

  fn verify_root_reclaim_support_is_durable(
    &self,
    request: &RootReclaimPublicationRequestV1<'_>,
  ) -> Result<(), RootReclaimPublicationErrorV1> {
    let lifecycle = decode_root_lifecycle_manifest_v1(&request.lifecycle_manifest.value, request.hash_algorithm)?;
    let expiry = decode_root_expiry_manifest_v1(&request.expiry_manifest.value, request.hash_algorithm)?;
    let proof = decode_root_object_reclaim_proof_v1(&request.root_object_reclaim_proof.value, request.hash_algorithm)?;
    let memory = request.pin_coordinator.memory_coordinator();
    let mut builder = super::gc_lifecycle::RootLifecycleSupportClosureBuilderV1::new_for_reclaim(
      &lifecycle,
      &expiry,
      &proof,
      request.hash_algorithm,
      request.cancellation,
      super::gc_lifecycle::RootLifecycleSupportLimitsV1 {
        maximum_candidate_records: lifecycle.candidate_count,
        maximum_expiry_records: expiry.record_count,
        maximum_support_artifacts: request.support_closure.support_artifact_count(),
      },
      &memory,
    )
    .map_err(root_reclaim_support_error)?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.hash_algorithm != request.hash_algorithm
      || header.database_id != request.support_closure.database_id()
    {
      return Err(RootReclaimPublicationErrorV1::invalid(
        "root_reclaim_database_authority",
        "selected first authority is degraded or differs from the reclaim closure",
      ));
    }
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let support_reader = RootLifecycleSupportReadContextV1 { file: &self.file, kv: &kv, header, memory: &memory };
    if let Some(root) = request.support_closure.candidate_directory_hash() {
      support_reader
        .revalidate_subtree(root, GcDirectoryRoleV1::RootCandidates, None, 0, &mut builder)
        .map_err(root_reclaim_from_retirement_error)?;
    }
    if let Some(root) = request.support_closure.expiry_directory_hash() {
      support_reader
        .revalidate_subtree(root, GcDirectoryRoleV1::RootExpiry, None, 0, &mut builder)
        .map_err(root_reclaim_from_retirement_error)?;
    }
    let observed = builder.finish().map_err(root_reclaim_support_error)?;
    if observed != *request.support_closure {
      return Err(RootReclaimPublicationErrorV1::invalid(
        "root_reclaim_support_changed",
        "durable root-lifecycle support closure differs from the validated request closure",
      ));
    }
    Ok(())
  }

  fn publish_root_reclaim_excluded(
    &self,
    request: &RootReclaimPublicationRequestV1<'_>,
    validated: &ValidatedRootReclaimPublicationV1<'_>,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<RootReclaimLockedPublicationV1, RootReclaimPublicationErrorV1> {
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      RootReclaimPublicationErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    let database_id = request.support_closure.database_id();
    if observation.selected.redundancy_degraded || header.hash_algorithm != request.hash_algorithm || header.database_id != database_id {
      return Err(RootReclaimPublicationErrorV1::invalid(
        "root_reclaim_database_authority",
        "selected first authority is degraded or differs from the reclaim closure",
      ));
    }
    if header.head_hash == request.retention_permit.namespace_root_hash() {
      return Err(RootReclaimPublicationErrorV1::invalid("root_reclaim_current_head", "the selected HEAD cannot be physically reclaimed"));
    }
    if request.cancellation.is_cancelled() {
      return Err(RootReclaimPublicationErrorV1::invalid(
        "root_reclaim_canceled",
        "root reclaim was canceled during final authority recheck",
      ));
    }

    let selected_control = {
      let kv = self.lock_kv()?;
      validate_kv_header_alignment(&kv, header)?;
      select_root_lifecycle_control(&self.file, &kv, header).map_err(root_reclaim_from_retirement_error)?
    };
    let exact_retry = selected_control.stored_value == request.lifecycle_control.value
      && selected_control.target_manifest_hash == request.lifecycle_manifest.key;
    if !exact_retry {
      if selected_control.target_manifest_hash != request.retention_permit.prior_lifecycle_manifest_hash() {
        return Err(RootReclaimPublicationErrorV1::invalid(
          "root_reclaim_prior_lifecycle_changed",
          "selected lifecycle authority no longer matches the retention permit",
        ));
      }
      self.validate_root_reclaim_prior_authority(request, header)?;
    }

    let mut observer = NoopFirstAuthorityDependencyObserverV1;
    let root_object_reclaim_proof_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::RootObjectReclaimProof,
        database_id: &database_id,
        artifact_key: &request.root_object_reclaim_proof.key,
        value: &request.root_object_reclaim_proof.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "root_reclaim_proof_committed_postcondition",
      },
      &mut observer,
    )?;
    let expiry_manifest_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::RootExpiryCatalogManifest,
        database_id: &database_id,
        artifact_key: &request.expiry_manifest.key,
        value: &request.expiry_manifest.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "root_reclaim_expiry_manifest_committed_postcondition",
      },
      &mut observer,
    )?;
    let lifecycle_manifest_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::RootLifecycleManifest,
        database_id: &database_id,
        artifact_key: &request.lifecycle_manifest.key,
        value: &request.lifecycle_manifest.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "root_reclaim_lifecycle_manifest_committed_postcondition",
      },
      &mut observer,
    )?;
    let control = self.publish_gc_active_control_locked(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::RootLifecycleActiveControl,
        encoded_control: request.lifecycle_control,
        publication_timestamp_ms: request.publication_timestamp_ms,
        monotonic_now_ms: request.monotonic_now_ms,
      },
      retirement_owner,
      control_observer,
    )?;
    let (control, committed_failure) = match control {
      GcControlPublicationOutcomeV1::Complete(control) => (control, None),
      GcControlPublicationOutcomeV1::CommittedFailure { publication, failure, message } => (publication, Some((failure, message))),
    };
    debug_assert_eq!(control.control_slot, validated.lifecycle_control.slot);
    Ok(RootReclaimLockedPublicationV1 {
      root_object_reclaim_proof_write_sequence,
      expiry_manifest_write_sequence,
      lifecycle_manifest_write_sequence,
      control,
      committed_failure,
    })
  }

  fn validate_root_reclaim_prior_authority(
    &self,
    request: &RootReclaimPublicationRequestV1<'_>,
    header: &DatabaseHeaderV4,
  ) -> Result<(), RootReclaimPublicationErrorV1> {
    let memory = request.pin_coordinator.memory_coordinator();
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let support_reader = RootLifecycleSupportReadContextV1 { file: &self.file, kv: &kv, header, memory: &memory };
    let prior_lifecycle_entity = support_reader
      .load_entity(request.retention_permit.prior_lifecycle_manifest_hash(), GcArtifactKindV1::RootLifecycleManifest)
      .map_err(root_reclaim_from_retirement_error)?;
    let prior_lifecycle_whole =
      decode_whole_entity(&prior_lifecycle_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    let prior_lifecycle = decode_root_lifecycle_manifest_v1(prior_lifecycle_whole.stored_value, header.hash_algorithm)?;
    if prior_lifecycle.root_expiry_manifest_hash != Some(request.retention_permit.prior_expiry_manifest_hash()) {
      return Err(RootReclaimPublicationErrorV1::invalid(
        "root_reclaim_prior_expiry_changed",
        "selected prior lifecycle does not reference the permit's exact expiry manifest",
      ));
    }
    let prior_expiry_entity = support_reader
      .load_entity(request.retention_permit.prior_expiry_manifest_hash(), GcArtifactKindV1::RootExpiryCatalogManifest)
      .map_err(root_reclaim_from_retirement_error)?;
    let prior_expiry_whole = decode_whole_entity(&prior_expiry_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    let prior_expiry = decode_root_expiry_manifest_v1(prior_expiry_whole.stored_value, header.hash_algorithm)?;
    validate_root_lifecycle_expiry_manifest(&prior_lifecycle, &prior_expiry)?;

    let next_lifecycle = decode_root_lifecycle_manifest_v1(&request.lifecycle_manifest.value, request.hash_algorithm)?;
    if prior_lifecycle.key != request.retention_permit.prior_lifecycle_manifest_hash()
      || prior_expiry.key != request.retention_permit.prior_expiry_manifest_hash()
      || prior_lifecycle.generation >= next_lifecycle.generation
      || prior_lifecycle.source_complete_mark_generation != next_lifecycle.source_complete_mark_generation
      || prior_lifecycle.authority_root_set_digest != next_lifecycle.authority_root_set_digest
      || prior_lifecycle.candidate_directory_hash != next_lifecycle.candidate_directory_hash
      || prior_lifecycle.next_page_id != next_lifecycle.next_page_id
      || prior_lifecycle.candidate_count != next_lifecycle.candidate_count
      || prior_lifecycle.pending_count != next_lifecycle.pending_count
      || prior_lifecycle.candidate_bytes != next_lifecycle.candidate_bytes
    {
      return Err(RootReclaimPublicationErrorV1::invalid(
        "root_reclaim_prior_lifecycle_changed",
        "reclaim attempted to change non-expiry lifecycle authority or did not advance its generation",
      ));
    }
    Ok(())
  }

  /// Hard-publish one immutable page, directory, or delta used by a future
  /// physical-quarantine manifest. This method never selects quarantine
  /// authority.
  pub fn publish_physical_quarantine_support_artifact(
    &self,
    request: PhysicalQuarantineSupportPublicationRequestV1<'_>,
  ) -> Result<PhysicalQuarantineSupportPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    let observation = self.observe()?;
    let algorithm = observation.selected.header.hash_algorithm;
    let decoded = decode_gc_state_artifact(&request.artifact.value, algorithm)?;
    let (kind, database_id) = match &decoded {
      GcStateArtifactV1::Page(page) if page.role == GcDirectoryRoleV1::Candidates => (GcArtifactKindV1::CandidatePage, page.database_id),
      GcStateArtifactV1::Directory(directory) if directory.role == GcDirectoryRoleV1::Candidates => {
        (GcArtifactKindV1::GcArtifactDirectoryNode, directory.database_id)
      }
      GcStateArtifactV1::CandidateDelta { .. } => {
        let delta = decode_candidate_delta_v1(&request.artifact.value, algorithm)?;
        (GcArtifactKindV1::CandidateDelta, delta.database_id)
      }
      GcStateArtifactV1::Page(_)
      | GcStateArtifactV1::Directory(_)
      | GcStateArtifactV1::Manifest(_)
      | GcStateArtifactV1::RootRetirementCommit { .. }
      | GcStateArtifactV1::RootObjectReclaimProof { .. }
      | GcStateArtifactV1::RetirementJournal { .. } => {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "quarantine_support_kind",
          "physical-quarantine support publication accepts only candidate pages, candidate directories, and candidate deltas",
        ));
      }
    };
    if database_id != request.database_id || decoded.key() != request.artifact.key {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "quarantine_support_identity",
        "physical-quarantine support artifact database or canonical key disagrees with its request",
      ));
    }

    let hard_publication_sequence = self.publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind,
        database_id: request.database_id,
        artifact_key: &request.artifact.key,
        value: &request.artifact.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "quarantine_support_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )?;
    Ok(PhysicalQuarantineSupportPublicationReceiptV1 {
      artifact_key: request.artifact.key.clone(),
      artifact_kind: kind,
      hard_publication_sequence,
    })
  }

  /// Execute one guarded, plural locator-removal boundary against an exact
  /// hard-published proposal. The caller-owned authority performs the actual
  /// v4 mutation and returns complete proposal-ordered outcomes. This method
  /// publishes no sweep receipt and grants no reusable Void authority.
  pub fn execute_sweep_locator_removals(
    &self,
    request: SweepLocatorRemovalRequestV1<'_>,
    removal_authority: &mut dyn SweepLocatorRemovalAuthorityV1,
  ) -> Result<SweepLocatorRemovalCompletionPermitV1, SweepLocatorRemovalErrorV1> {
    if request.cancellation.is_cancelled() {
      return Err(SweepLocatorRemovalErrorV1::invalid(
        "sweep_removal_canceled",
        "sweep locator removal was canceled before memory admission",
      ));
    }
    if request.pin_coordinator.hash_algorithm() != request.permit.hash_algorithm()
      || request.hard_publication.proposal_key != request.permit.proposal().key
      || request.hard_publication.hard_publication_sequence == 0
    {
      return Err(SweepLocatorRemovalErrorV1::invalid(
        "sweep_removal_identity",
        "request-pin identity or hard-publication receipt differs from the qualified proposal",
      ));
    }
    let SweepVoidArtifactV1::SweepProposal(qualified_proposal) =
      decode_sweep_void_artifact(&request.permit.proposal().value, request.permit.hash_algorithm())?
    else {
      return Err(SweepLocatorRemovalErrorV1::invalid(
        "sweep_removal_proposal_kind",
        "qualified sweep proposal bytes decode as another artifact kind",
      ));
    };
    if qualified_proposal.key != request.permit.proposal().key
      || qualified_proposal.database_id != request.permit.database_id()
      || qualified_proposal.batch_id != request.permit.batch_id()
      || qualified_proposal.generation != request.permit.generation()
      || qualified_proposal.quarantine_manifest_hash != request.permit.quarantine_manifest_hash()
      || qualified_proposal.candidate_count != request.permit.candidate_count()
    {
      return Err(SweepLocatorRemovalErrorV1::invalid(
        "sweep_removal_identity",
        "qualified proposal identity differs from its non-constructible permit",
      ));
    }
    let memory = request.pin_coordinator.memory_coordinator();
    let maximum_proposal_value_length = GcArtifactKindV1::SweepProposal
      .immutable_maximum_encoded_length()
      .ok_or_else(|| SweepLocatorRemovalErrorV1::invalid("sweep_removal_proposal_kind", "sweep proposal has no immutable role cap"))?;
    let proposal_entity_scratch = super::entity::checked_whole_entity_encoded_length(
      request.permit.hash_algorithm(),
      request.permit.proposal().key.len(),
      maximum_proposal_value_length,
    )?;
    let quarantine_manifest_entity_scratch = super::entity::checked_whole_entity_encoded_length(
      request.permit.hash_algorithm(),
      request.permit.quarantine_manifest_hash().len(),
      GcArtifactKindV1::QuarantineManifest.immutable_maximum_encoded_length().ok_or_else(|| {
        SweepLocatorRemovalErrorV1::invalid("sweep_removal_quarantine_kind", "quarantine manifest has no immutable role cap")
      })?,
    )?;
    let control_entity_scratch = FIRST_AUTHORITY_CONTROL_ENTITY_CAP
      .checked_mul(2)
      .ok_or_else(|| SweepLocatorRemovalErrorV1::invalid("sweep_removal_read_size", "control read estimate overflowed"))?;
    let read_scratch_bytes = proposal_entity_scratch
      .checked_add(quarantine_manifest_entity_scratch)
      .and_then(|bytes| bytes.checked_add(control_entity_scratch))
      .ok_or_else(|| SweepLocatorRemovalErrorV1::invalid("sweep_removal_read_size", "guarded read estimate overflowed"))?;
    let _read_memory = memory.reserve(MemoryOwner::GarbageCollection, u64::try_from(read_scratch_bytes)?, AdmissionClass::Maintenance)?;
    let mut result_memory = Some(reserve_sweep_locator_removal_results_v1(&memory, qualified_proposal.candidate_count)?);
    let operation_result = request.pin_coordinator.with_global_exclusion(request.cancellation, || {
      let operation_result = (|| {
        let _authority = self.root_state.lock().map_err(|poisoned| {
          drop(poisoned);
          SweepLocatorRemovalErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
        })?;
        if request.cancellation.is_cancelled() {
          return Err(SweepLocatorRemovalErrorV1::invalid(
            "sweep_removal_canceled",
            "sweep locator removal was canceled after authority exclusion",
          ));
        }

        let observation = self.observe()?;
        let header = &observation.selected.header;
        if observation.selected.redundancy_degraded
          || header.head_hash.iter().all(|byte| *byte == 0)
          || header.hash_algorithm != request.permit.hash_algorithm()
          || header.database_id != *request.permit.database_id()
        {
          return Err(SweepLocatorRemovalErrorV1::invalid(
            "sweep_removal_database_authority",
            "selected first authority is absent, degraded, or differs from the sweep proposal",
          ));
        }

        let kv = self.lock_kv()?;
        validate_kv_header_alignment(&kv, header)?;
        let selected_control = select_physical_quarantine_control(&self.file, &kv, header)?;
        let selected = decode_gc_active_control(&selected_control.stored_value, header.hash_algorithm)?;
        if selected.kind != GcArtifactKindV1::QuarantineActiveControl
          || selected.target_manifest_hash != request.permit.quarantine_manifest_hash()
          || selected.generation != request.permit.generation()
        {
          return Err(SweepLocatorRemovalErrorV1::invalid(
            "sweep_removal_quarantine_changed",
            "physically selected quarantine authority differs from the durable proposal",
          ));
        }

        let quarantine_manifest_key = request.permit.quarantine_manifest_hash();
        let quarantine_manifest_locator =
          kv.get(quarantine_manifest_key).map_err(FirstAuthorityPublicationErrorV1::from)?.ok_or_else(|| {
            SweepLocatorRemovalErrorV1::invalid(
              "sweep_removal_quarantine_missing",
              "selected physical-quarantine manifest locator is absent",
            )
          })?;
        if quarantine_manifest_locator.type_flags != kv_tag::GC_ARTIFACT {
          return Err(SweepLocatorRemovalErrorV1::invalid(
            "sweep_removal_quarantine_collision",
            "selected physical-quarantine manifest key resolves to another KV role",
          ));
        }
        let quarantine_manifest_bytes = read_entity_bounded(
          &self.file,
          &kv,
          quarantine_manifest_key,
          quarantine_manifest_entity_scratch,
          header.write_sequence_high_water,
        )?
        .ok_or_else(|| {
          SweepLocatorRemovalErrorV1::invalid("sweep_removal_quarantine_missing", "selected physical-quarantine manifest disappeared")
        })?;
        let quarantine_manifest_entity =
          decode_whole_entity(&quarantine_manifest_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
        if quarantine_manifest_entity.entry_type != EntryTypeV4::GcArtifact
          || quarantine_manifest_entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
          || quarantine_manifest_entity.compression_algorithm != CompressionAlgorithm::None
          || quarantine_manifest_entity.key != quarantine_manifest_key
        {
          return Err(SweepLocatorRemovalErrorV1::invalid(
            "sweep_removal_quarantine_changed",
            "selected physical-quarantine manifest is not one canonical system GC WholeEntity",
          ));
        }
        let quarantine_manifest =
          super::gc_quarantine::decode_quarantine_manifest_v1(quarantine_manifest_entity.stored_value, header.hash_algorithm)?;
        if quarantine_manifest.key != quarantine_manifest_key
          || quarantine_manifest.database_id != header.database_id
          || quarantine_manifest.mark_generation != request.permit.generation()
          || quarantine_manifest.eligible_count_hint != u64::from(request.permit.candidate_count())
        {
          return Err(SweepLocatorRemovalErrorV1::invalid(
            "sweep_removal_quarantine_changed",
            "selected physical-quarantine manifest identity, generation, or eligible count differs from the proposal",
          ));
        }

        let proposal_key = request.permit.proposal().key.as_slice();
        let proposal_locator = kv.get(proposal_key).map_err(FirstAuthorityPublicationErrorV1::from)?.ok_or_else(|| {
          SweepLocatorRemovalErrorV1::invalid("sweep_removal_proposal_missing", "hard-published sweep proposal locator is absent")
        })?;
        if proposal_locator.type_flags != kv_tag::GC_ARTIFACT {
          return Err(SweepLocatorRemovalErrorV1::invalid(
            "sweep_removal_proposal_collision",
            "hard-published sweep proposal key resolves to another KV role",
          ));
        }
        let proposal_bytes = read_entity_bounded(&self.file, &kv, proposal_key, proposal_entity_scratch, header.write_sequence_high_water)?
          .ok_or_else(|| {
            SweepLocatorRemovalErrorV1::invalid("sweep_removal_proposal_missing", "hard-published sweep proposal disappeared")
          })?;
        let proposal_entity = decode_whole_entity(&proposal_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
        if proposal_entity.entry_type != EntryTypeV4::GcArtifact
          || proposal_entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
          || proposal_entity.compression_algorithm != CompressionAlgorithm::None
          || proposal_entity.key != proposal_key
          || proposal_entity.stored_value != request.permit.proposal().value
          || proposal_entity.write_sequence != request.hard_publication.hard_publication_sequence
        {
          return Err(SweepLocatorRemovalErrorV1::invalid(
            "sweep_removal_proposal_changed",
            "persisted sweep proposal representation or write sequence differs from its hard-publication receipt",
          ));
        }
        let SweepVoidArtifactV1::SweepProposal(persisted_proposal) =
          decode_sweep_void_artifact(proposal_entity.stored_value, header.hash_algorithm)?
        else {
          return Err(SweepLocatorRemovalErrorV1::invalid(
            "sweep_removal_proposal_kind",
            "persisted sweep proposal decoded as another GC artifact kind",
          ));
        };
        drop(kv);

        let authority_request = SweepLocatorRemovalAuthorityRequestV1 {
          hash_algorithm: header.hash_algorithm,
          database_id: request.permit.database_id(),
          batch_id: request.permit.batch_id(),
          generation: request.permit.generation(),
          proposal_hash: proposal_key,
          proposal_write_sequence: request.hard_publication.hard_publication_sequence,
          quarantine_manifest_hash: request.permit.quarantine_manifest_hash(),
          proposal: &persisted_proposal,
          cancellation: request.cancellation,
        };
        let snapshot = removal_authority.recheck_sweep_locator_removal_authority(authority_request)?;
        validate_sweep_locator_removal_snapshot_v1(authority_request, &snapshot)?;
        if request.cancellation.is_cancelled() {
          return Err(SweepLocatorRemovalErrorV1::invalid(
            "sweep_removal_canceled",
            "sweep locator removal was canceled after the final authority recheck",
          ));
        }
        let batch_outcome = removal_authority.remove_sweep_locators(authority_request);
        let admitted_result_memory = result_memory.take().ok_or_else(|| {
          SweepLocatorRemovalErrorV1::invalid("sweep_removal_internal", "admitted sweep result memory was already consumed")
        })?;
        complete_sweep_locator_removal_v1(authority_request, batch_outcome, admitted_result_memory)
      })();
      Ok(operation_result)
    })?;
    operation_result
  }

  /// Reconcile one live or restart-discovered sweep into exactly one semantic
  /// receipt after a caller-owned authority proves the receipt-backed Void
  /// catalog is selected. Until this returns, allocator admission must remain
  /// blocked. The receipt itself never grants reusable-space authority.
  pub fn reconcile_sweep_receipt(
    &self,
    request: SweepReceiptReconciliationRequestV1<'_>,
    void_authority: &mut dyn SweepReceiptVoidAuthorityV1,
  ) -> Result<SweepReceiptHardPublicationReceiptV1, SweepReceiptReconciliationErrorV1> {
    if request.cancellation.is_cancelled() {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_canceled",
        "sweep receipt reconciliation was canceled before memory admission",
      ));
    }
    let completion = match request.source {
      SweepReceiptReconciliationSourceV1::Completion(completion) => Some(completion),
      SweepReceiptReconciliationSourceV1::Recovery(_) => None,
    };
    let (hash_algorithm, database_id, proposal_hash, proposal_write_sequence, recovery) = match request.source {
      SweepReceiptReconciliationSourceV1::Completion(completion) => {
        (completion.hash_algorithm(), completion.database_id(), completion.proposal_hash(), completion.proposal_write_sequence(), false)
      }
      SweepReceiptReconciliationSourceV1::Recovery(identity) => {
        (identity.hash_algorithm, *identity.database_id, identity.proposal_hash, identity.proposal_write_sequence, true)
      }
    };
    if proposal_hash.len() != hash_algorithm.hash_length() || proposal_hash.iter().all(|byte| *byte == 0) || proposal_write_sequence == 0 {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_proposal_identity",
        "sweep receipt proposal hash or hard-publication sequence is invalid",
      ));
    }

    let proposal_entity_scratch = super::entity::checked_whole_entity_encoded_length(
      hash_algorithm,
      proposal_hash.len(),
      GcArtifactKindV1::SweepProposal
        .immutable_maximum_encoded_length()
        .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_proposal_kind", "proposal has no immutable role cap"))?,
    )?;
    let receipt_entity_scratch = super::entity::checked_whole_entity_encoded_length(
      hash_algorithm,
      hash_algorithm.hash_length(),
      GcArtifactKindV1::RecoveredSweepReceipt
        .immutable_maximum_encoded_length()
        .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_kind", "receipt has no immutable role cap"))?,
    )?;
    let read_scratch_bytes = proposal_entity_scratch
      .checked_add(receipt_entity_scratch)
      .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_read_size", "receipt read estimate overflowed"))?;
    let _read_memory =
      request.memory.reserve(MemoryOwner::GarbageCollection, u64::try_from(read_scratch_bytes)?, AdmissionClass::Maintenance)?;

    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      SweepReceiptReconciliationErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    if request.cancellation.is_cancelled() {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_canceled",
        "sweep receipt reconciliation was canceled after first-authority exclusion",
      ));
    }

    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.head_hash.iter().all(|byte| *byte == 0)
      || header.hash_algorithm != hash_algorithm
      || header.database_id != database_id
    {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_database_authority",
        "selected first authority is absent, degraded, or differs from the receipt source",
      ));
    }

    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let proposal_locator = kv.get(proposal_hash).map_err(FirstAuthorityPublicationErrorV1::from)?.ok_or_else(|| {
      SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_proposal_missing", "durable sweep proposal locator is absent")
    })?;
    if proposal_locator.type_flags != kv_tag::GC_ARTIFACT {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_proposal_collision",
        "durable sweep proposal key resolves to another KV role",
      ));
    }
    let proposal_bytes = read_entity_bounded(&self.file, &kv, proposal_hash, proposal_entity_scratch, header.write_sequence_high_water)?
      .ok_or_else(|| SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_proposal_missing", "durable sweep proposal disappeared"))?;
    let proposal_entity = decode_whole_entity(&proposal_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    if proposal_entity.entry_type != EntryTypeV4::GcArtifact
      || proposal_entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
      || proposal_entity.compression_algorithm != CompressionAlgorithm::None
      || proposal_entity.key != proposal_hash
      || proposal_entity.write_sequence != proposal_write_sequence
    {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_proposal_changed",
        "durable sweep proposal representation or write sequence differs from its receipt source",
      ));
    }
    let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(proposal_entity.stored_value, header.hash_algorithm)?
    else {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_proposal_kind",
        "durable sweep proposal decoded as another GC artifact kind",
      ));
    };
    if proposal.key != proposal_hash || proposal.database_id != database_id {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_proposal_identity",
        "durable sweep proposal belongs to another database or canonical key",
      ));
    }
    if let Some(completion) = completion {
      if proposal.batch_id != completion.batch_id()
        || proposal.generation != completion.generation()
        || proposal.quarantine_manifest_hash != completion.quarantine_manifest_hash()
        || proposal.candidate_count != u32::try_from(completion.outcomes().len())?
      {
        return Err(SweepReceiptReconciliationErrorV1::invalid(
          "sweep_receipt_completion_changed",
          "durable proposal identity differs from the live locator-removal completion",
        ));
      }
    }
    drop(kv);

    let authority_request = SweepReceiptVoidAuthorityRequestV1 {
      hash_algorithm: header.hash_algorithm,
      database_id: &database_id,
      batch_id: proposal.batch_id.try_into()?,
      generation: proposal.generation,
      proposal_hash,
      proposal_write_sequence,
      proposal: &proposal,
      recovery,
      cancellation: request.cancellation,
    };
    let snapshot = void_authority.recheck_sweep_receipt_void_authority(authority_request)?;
    validate_sweep_receipt_void_authority_v1(authority_request, &snapshot)?;
    if request.cancellation.is_cancelled() {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_canceled",
        "sweep receipt reconciliation was canceled after selected-Void recheck",
      ));
    }

    if let Some(existing) = snapshot.existing_receipt.as_ref() {
      let kv = self.lock_kv()?;
      let existing_locator = kv.get(&existing.receipt_hash).map_err(FirstAuthorityPublicationErrorV1::from)?.ok_or_else(|| {
        SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_existing_missing", "claimed existing receipt locator is absent")
      })?;
      if existing_locator.type_flags != kv_tag::GC_ARTIFACT {
        return Err(SweepReceiptReconciliationErrorV1::invalid(
          "sweep_receipt_existing_collision",
          "claimed existing receipt key resolves to another KV role",
        ));
      }
      let existing_bytes =
        read_entity_bounded(&self.file, &kv, &existing.receipt_hash, receipt_entity_scratch, header.write_sequence_high_water)?
          .ok_or_else(|| {
            SweepReceiptReconciliationErrorV1::invalid("sweep_receipt_existing_missing", "claimed existing receipt disappeared")
          })?;
      let existing_entity = decode_whole_entity(&existing_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
      if existing_entity.entry_type != EntryTypeV4::GcArtifact
        || existing_entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
        || existing_entity.compression_algorithm != CompressionAlgorithm::None
        || existing_entity.key != existing.receipt_hash
        || existing_entity.write_sequence != existing.receipt_write_sequence
      {
        return Err(SweepReceiptReconciliationErrorV1::invalid(
          "sweep_receipt_existing_changed",
          "claimed existing receipt representation or write sequence changed",
        ));
      }
      let SweepVoidArtifactV1::SweepReceipt(existing_receipt) =
        decode_sweep_void_artifact(existing_entity.stored_value, header.hash_algorithm)?
      else {
        return Err(SweepReceiptReconciliationErrorV1::invalid(
          "sweep_receipt_existing_kind",
          "claimed existing receipt decoded as another GC artifact kind",
        ));
      };
      if existing_receipt.key != existing.receipt_hash {
        return Err(SweepReceiptReconciliationErrorV1::invalid(
          "sweep_receipt_existing_conflict",
          "claimed existing receipt canonical key differs from its selected locator",
        ));
      }
      validate_existing_sweep_receipt_v1(
        authority_request,
        &snapshot,
        &existing_receipt,
        completion.map(SweepLocatorRemovalCompletionPermitV1::outcomes),
      )?;
      return Ok(SweepReceiptHardPublicationReceiptV1 {
        receipt_key: existing.receipt_hash.clone(),
        hard_publication_sequence: existing.receipt_write_sequence,
        recovered: existing_receipt.recovered,
        void_catalog_hash: snapshot.selected_void_catalog_hash,
        void_catalog_generation: snapshot.selected_void_catalog_generation,
        reclaim_committed_at_ms: snapshot.reclaim_committed_at_ms,
      });
    }

    let receipt_memory = reserve_sweep_receipt_reconciliation_v1(header.hash_algorithm, proposal.candidate_count, request.memory)?;
    let recovered_outcomes = if recovery { Some(void_authority.recover_sweep_receipt_outcomes(authority_request)?) } else { None };
    if request.cancellation.is_cancelled() {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_canceled",
        "sweep receipt reconciliation was canceled after outcome recovery",
      ));
    }
    let outcomes = match (recovered_outcomes.as_deref(), completion) {
      (Some(outcomes), None) => outcomes,
      (None, Some(completion)) => completion.outcomes(),
      _ => {
        return Err(SweepReceiptReconciliationErrorV1::invalid(
          "sweep_receipt_outcomes",
          "receipt source supplied an ambiguous or incomplete outcome set",
        ));
      }
    };
    let prepared = prepare_sweep_receipt_reconciliation_v1(authority_request, &snapshot, outcomes, receipt_memory)?;
    if request.cancellation.is_cancelled() {
      return Err(SweepReceiptReconciliationErrorV1::invalid(
        "sweep_receipt_canceled",
        "sweep receipt reconciliation was canceled before hard publication",
      ));
    }
    let receipt_kind = if prepared.recovered { GcArtifactKindV1::RecoveredSweepReceipt } else { GcArtifactKindV1::SweepCommitReceipt };
    let hard_publication_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: receipt_kind,
        database_id: &database_id,
        artifact_key: &prepared.artifact.key,
        value: &prepared.artifact.value,
        minimum_timestamp_ms: u64::try_from(prepared.reclaim_committed_at_ms)?,
        committed_postcondition_code: "sweep_receipt_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )?;
    Ok(SweepReceiptHardPublicationReceiptV1 {
      receipt_key: prepared.artifact.key,
      hard_publication_sequence,
      recovered: prepared.recovered,
      void_catalog_hash: prepared.void_catalog_hash,
      void_catalog_generation: prepared.void_catalog_generation,
      reclaim_committed_at_ms: prepared.reclaim_committed_at_ms,
    })
  }

  /// Hard-publish one already-qualified sweep proposal after independently
  /// reselecting its exact quarantine authority. This method appends immutable
  /// evidence only; it exposes no locator-removal or Void authority.
  pub fn publish_sweep_proposal(
    &self,
    request: SweepProposalHardPublicationRequestV1<'_>,
  ) -> Result<SweepProposalHardPublicationReceiptV1, SweepProposalHardPublicationErrorV1> {
    if request.cancellation.is_cancelled() {
      return Err(SweepProposalHardPublicationErrorV1::invalid(
        "sweep_proposal_publication_canceled",
        "sweep proposal publication was canceled before authority selection",
      ));
    }
    let SweepVoidArtifactV1::SweepProposal(proposal) =
      decode_sweep_void_artifact(&request.permit.proposal().value, request.permit.hash_algorithm())?
    else {
      return Err(SweepProposalHardPublicationErrorV1::invalid(
        "sweep_proposal_publication_kind",
        "qualified sweep proposal bytes decode as another artifact kind",
      ));
    };
    let created_at_ms = match u64::try_from(proposal.created_at_ms) {
      Ok(created_at_ms) => created_at_ms,
      Err(source) => return Err(SweepProposalHardPublicationErrorV1::CreationTime(source)),
    };
    if proposal.key != request.permit.proposal().key
      || proposal.database_id != request.permit.database_id()
      || proposal.batch_id != request.permit.batch_id()
      || proposal.generation != request.permit.generation()
      || proposal.quarantine_manifest_hash != request.permit.quarantine_manifest_hash()
      || proposal.candidate_count != request.permit.candidate_count()
      || request.publication_timestamp_ms < created_at_ms
    {
      return Err(SweepProposalHardPublicationErrorV1::invalid(
        "sweep_proposal_publication_identity",
        "qualified sweep proposal identity or publication time differs from its permit",
      ));
    }

    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      SweepProposalHardPublicationErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.hash_algorithm != request.permit.hash_algorithm()
      || header.database_id != *request.permit.database_id()
    {
      return Err(SweepProposalHardPublicationErrorV1::invalid(
        "sweep_proposal_publication_database_authority",
        "selected first authority is degraded or differs from the sweep proposal permit",
      ));
    }
    let selected_control = {
      let kv = self.lock_kv()?;
      validate_kv_header_alignment(&kv, header)?;
      select_physical_quarantine_control(&self.file, &kv, header)?
    };
    let selected = decode_gc_active_control(&selected_control.stored_value, header.hash_algorithm)?;
    if selected.kind != GcArtifactKindV1::QuarantineActiveControl
      || selected.target_manifest_hash != request.permit.quarantine_manifest_hash()
      || selected.generation != request.permit.generation()
    {
      return Err(SweepProposalHardPublicationErrorV1::invalid(
        "sweep_proposal_publication_quarantine_changed",
        "physically selected quarantine authority differs from the qualified proposal",
      ));
    }
    if request.cancellation.is_cancelled() {
      return Err(SweepProposalHardPublicationErrorV1::invalid(
        "sweep_proposal_publication_canceled",
        "sweep proposal publication was canceled after authority selection",
      ));
    }
    let hard_publication_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::SweepProposal,
        database_id: request.permit.database_id(),
        artifact_key: &request.permit.proposal().key,
        value: &request.permit.proposal().value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "sweep_proposal_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )?;
    Ok(SweepProposalHardPublicationReceiptV1 { proposal_key: request.permit.proposal().key.clone(), hard_publication_sequence })
  }

  /// Reconstruct one bounded observation of the selected, receipt-backed Void
  /// catalog. This does not expose overwrite authority; durable claim
  /// admission remains the only allocator entry point.
  pub fn reconstruct_void_reusable_state(
    &self,
    request: VoidReusableStateReconstructionRequestV1<'_>,
    authority: &mut dyn VoidReclaimReceiptAuthorityV1,
  ) -> Result<Option<VoidReusableSpaceStateV1>, VoidReusableStateErrorV1> {
    if request.cancellation.is_cancelled() {
      return Err(VoidReusableStateErrorV1::Canceled);
    }
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      VoidReusableStateErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded || header.head_hash.iter().all(|byte| *byte == 0) {
      return Err(VoidReusableStateErrorV1::invalid(
        "void_runtime_database_authority",
        "Void reusable-state reconstruction requires selected non-degraded first authority",
      ));
    }
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let Some(selected_control) = select_void_catalog_control(&self.file, &kv, header)? else {
      return Ok(None);
    };
    let control = decode_gc_active_control(&selected_control.stored_value, header.hash_algorithm)?;
    if control.kind != GcArtifactKindV1::VoidCatalogActiveControl
      || control.database_id != header.database_id
      || control.target_manifest_hash != selected_control.target_manifest_hash
      || control.sequence != selected_control.control_sequence
      || control.slot != selected_control.slot
    {
      return Err(VoidReusableStateErrorV1::invalid(
        "void_runtime_control_identity",
        "selected Void control identity changed or differs from its durable representation",
      ));
    }
    let reader =
      VoidCatalogSupportReadContextV1 { file: &self.file, kv: &kv, header, memory: request.memory, cancellation: request.cancellation };
    let manifest_entity = reader.load_entity(&selected_control.target_manifest_hash, GcArtifactKindV1::VoidCatalogManifest)?;
    let manifest_whole_entity = decode_whole_entity(&manifest_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(manifest_whole_entity.stored_value, header.hash_algorithm)?
    else {
      return Err(VoidReusableStateErrorV1::invalid(
        "void_runtime_manifest_kind",
        "selected Void manifest key resolves to another GC artifact kind",
      ));
    };
    if manifest.key != selected_control.target_manifest_hash
      || manifest.database_id != header.database_id
      || manifest.generation != control.generation
    {
      return Err(VoidReusableStateErrorV1::invalid(
        "void_runtime_manifest_identity",
        "selected Void manifest identity or generation differs from first authority",
      ));
    }
    let mut validator = VoidReusableStateValidatorV1::new(
      &manifest,
      header.hash_algorithm,
      VoidReusableStateIdentityV1 {
        selected_manifest_key: &selected_control.target_manifest_hash,
        selected_control_key: &selected_control.control_key,
        selected_control_sequence: selected_control.control_sequence,
        selected_control_write_sequence: selected_control.write_sequence,
        selected_control_slot: selected_control.slot,
      },
      request.cancellation.clone(),
      request.limits,
      request.memory,
    )?;
    if !manifest.claim_root.iter().all(|byte| *byte == 0) {
      let mut observer = VoidReusableClaimsObserverV1 { validator: &mut validator };
      reader.revalidate_subtree(manifest.claim_root, GcDirectoryRoleV1::Claims, None, 0, &mut observer)?;
    }
    validator.finish_claims()?;
    if !manifest.free_root.iter().all(|byte| *byte == 0) {
      let mut observer = VoidReusableFreeObserverV1 { validator: &mut validator, authority };
      reader.revalidate_subtree(manifest.free_root, GcDirectoryRoleV1::FreeExtents, None, 0, &mut observer)?;
    }
    Ok(Some(validator.finish()?))
  }

  /// Select one complete Void catalog after all immutable support is durable.
  /// The returned receipt deliberately keeps reuse blocked until the separate
  /// sweep-receipt reconciler proves the exact selected catalog.
  pub fn publish_void_catalog(
    &mut self,
    request: VoidCatalogPublicationRequestV1<'_>,
    authority: &mut dyn VoidCatalogPublicationAuthorityV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  ) -> Result<VoidCatalogPublicationReceiptV1, VoidCatalogPublicationErrorV1> {
    self.publish_void_catalog_with_control_observer(request, authority, retirement_owner, &mut NoopFirstAuthorityDependencyObserverV1)
  }

  fn publish_void_catalog_with_control_observer(
    &mut self,
    request: VoidCatalogPublicationRequestV1<'_>,
    authority: &mut dyn VoidCatalogPublicationAuthorityV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<VoidCatalogPublicationReceiptV1, VoidCatalogPublicationErrorV1> {
    if request.cancellation.is_cancelled() {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_canceled",
        "Void catalog publication was canceled before authority selection",
      ));
    }
    if retirement_owner.hash_algorithm() != request.completion.hash_algorithm()
      || retirement_owner.database_id() != request.completion.database_id()
    {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_lineage_identity",
        "retirement lineage owner differs from the completed sweep database or hash profile",
      ));
    }
    validate_void_catalog_publication_request(&request)?;
    retirement_owner.flush(self)?;

    let locked = self.publish_void_catalog_locked(&request, authority, retirement_owner, control_observer)?;
    let mut committed_failure = locked.committed_failure.map(|(failure, message)| (failure.code(), message));
    let lineage_state = if locked.control.replaced_control {
      match retirement_owner.flush(self) {
        Ok(true) => RootRetirementLineageStateV1::HardPublished {
          hard_publication_sequence: retirement_owner.status().last_hard_publication_sequence,
        },
        Ok(false) => RootRetirementLineageStateV1::MissingAfterCommit {
          code: "void_publication_lineage_missing",
          message: "Void control replacement committed without retained retirement lineage".to_string(),
        },
        Err(source) => RootRetirementLineageStateV1::BufferedAfterFlushFailure { code: source.code(), message: source.to_string() },
      }
    } else {
      RootRetirementLineageStateV1::NotRequired
    };
    if !matches!(lineage_state, RootRetirementLineageStateV1::NotRequired | RootRetirementLineageStateV1::HardPublished { .. })
      && committed_failure.is_none()
    {
      committed_failure = Some(("void_publication_committed_lineage", "Void replacement lineage did not hard-publish".to_string()));
    }
    let observation = if matches!(lineage_state, RootRetirementLineageStateV1::HardPublished { .. }) {
      match self.observe() {
        Ok(observation) => observation,
        Err(source) => {
          if committed_failure.is_none() {
            committed_failure = Some(("void_publication_committed_lineage_readback", source.to_string()));
          }
          locked.control.observation
        }
      }
    } else {
      locked.control.observation
    };
    let receipt = VoidCatalogPublicationReceiptV1 {
      manifest_key: request.manifest.key.clone(),
      manifest_write_sequence: locked.manifest_write_sequence,
      control_key: request.control.key.clone(),
      control_write_sequence: locked.control.control_write_sequence,
      control_slot: locked.control.control_slot,
      lineage_state,
      observation,
      receipt_reconciliation_required: true,
      reuse_blocked: true,
      idempotent: locked.control.idempotent,
    };
    if let Some((code, message)) = committed_failure {
      return Err(VoidCatalogPublicationErrorV1::committed(code, message, receipt));
    }
    Ok(receipt)
  }

  fn publish_void_catalog_locked(
    &self,
    request: &VoidCatalogPublicationRequestV1<'_>,
    authority: &mut dyn VoidCatalogPublicationAuthorityV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<VoidCatalogLockedPublicationV1, VoidCatalogPublicationErrorV1> {
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      VoidCatalogPublicationErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.head_hash.iter().all(|byte| *byte == 0)
      || header.hash_algorithm != request.completion.hash_algorithm()
      || header.database_id != request.completion.database_id()
    {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_database_authority",
        "selected first authority is absent, degraded, or differs from the completed sweep",
      ));
    }
    if request.cancellation.is_cancelled() {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_canceled",
        "Void catalog publication was canceled during final authority recheck",
      ));
    }
    let closure = self.verify_void_catalog_support_is_durable(request)?;
    let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(&request.manifest.value, header.hash_algorithm)? else {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_manifest_kind",
        "proposed Void manifest bytes decode as another GC artifact kind",
      ));
    };
    let selected_control = {
      let kv = self.lock_kv()?;
      validate_kv_header_alignment(&kv, header)?;
      select_void_catalog_control(&self.file, &kv, header)?
    };
    let exact_retry = selected_control
      .as_ref()
      .is_some_and(|selected| selected.stored_value == request.control.value && selected.target_manifest_hash == request.manifest.key);
    if !exact_retry {
      let selected_prior_manifest_hash = selected_control.as_ref().map(|selected| selected.target_manifest_hash.as_slice());
      let selected_prior_control_sequence = selected_control.as_ref().map_or(0, |selected| selected.control_sequence);
      if selected_prior_control_sequence != manifest.previous_control_sequence
        || manifest.previous_control_sequence == 0 && selected_prior_manifest_hash.is_some()
        || manifest.previous_control_sequence != 0 && selected_prior_manifest_hash.is_none()
      {
        return Err(VoidCatalogPublicationErrorV1::invalid(
          "void_publication_prior_authority",
          "physically selected prior Void authority differs from the proposed manifest predecessor",
        ));
      }
      let authority_snapshot = authority.recheck_void_catalog_publication_authority(VoidCatalogPublicationAuthorityRequestV1 {
        completion: request.completion,
        manifest: &manifest,
        closure: &closure,
        selected_prior_manifest_hash,
        selected_prior_control_sequence,
        cancellation: request.cancellation,
      })?;
      validate_void_catalog_publication_authority_v1(
        VoidCatalogPublicationAuthorityRequestV1 {
          completion: request.completion,
          manifest: &manifest,
          closure: &closure,
          selected_prior_manifest_hash,
          selected_prior_control_sequence,
          cancellation: request.cancellation,
        },
        &authority_snapshot,
      )?;
    }

    let database_id = request.completion.database_id();
    let manifest_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::VoidCatalogManifest,
        database_id: &database_id,
        artifact_key: &request.manifest.key,
        value: &request.manifest.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "void_manifest_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )?;
    let control = self.publish_gc_active_control_locked(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::VoidCatalogActiveControl,
        encoded_control: request.control,
        publication_timestamp_ms: request.publication_timestamp_ms,
        monotonic_now_ms: request.monotonic_now_ms,
      },
      retirement_owner,
      control_observer,
    )?;
    let (control, committed_failure) = match control {
      GcControlPublicationOutcomeV1::Complete(control) => (control, None),
      GcControlPublicationOutcomeV1::CommittedFailure { publication, failure, message } => (publication, Some((failure, message))),
    };
    Ok(VoidCatalogLockedPublicationV1 { manifest_write_sequence, control, committed_failure })
  }

  fn verify_void_catalog_support_is_durable(
    &self,
    request: &VoidCatalogPublicationRequestV1<'_>,
  ) -> Result<VoidCatalogClosureSummaryV1, VoidCatalogPublicationErrorV1> {
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.hash_algorithm != request.completion.hash_algorithm()
      || header.database_id != request.completion.database_id()
    {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_database_authority",
        "selected first authority is degraded or differs from the proposed Void catalog",
      ));
    }
    let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(&request.manifest.value, header.hash_algorithm)? else {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_manifest_kind",
        "proposed Void manifest bytes decode as another GC artifact kind",
      ));
    };
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let reader =
      VoidCatalogSupportReadContextV1 { file: &self.file, kv: &kv, header, memory: request.memory, cancellation: request.cancellation };
    let proposal_entity = reader.load_entity(request.completion.proposal_hash(), GcArtifactKindV1::SweepProposal)?;
    let proposal_whole = decode_whole_entity(&proposal_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    if proposal_whole.write_sequence != request.completion.proposal_write_sequence() {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_proposal_sequence",
        "durable sweep proposal write sequence differs from its locator-removal completion",
      ));
    }
    let SweepVoidArtifactV1::SweepProposal(proposal) = decode_sweep_void_artifact(proposal_whole.stored_value, header.hash_algorithm)?
    else {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_proposal_kind",
        "durable sweep proposal key resolves to another GC artifact kind",
      ));
    };
    let candidate_count = usize::try_from(proposal.candidate_count).map_err(|source| {
      VoidCatalogPublicationErrorV1::invalid(
        "void_publication_proposal_identity",
        format!("durable sweep proposal candidate count exceeds usize: {source}"),
      )
    })?;
    if proposal.key != request.completion.proposal_hash()
      || proposal.database_id != request.completion.database_id()
      || proposal.batch_id != request.completion.batch_id()
      || proposal.generation != request.completion.generation()
      || proposal.quarantine_manifest_hash != request.completion.quarantine_manifest_hash()
      || candidate_count != request.completion.outcomes().len()
    {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_proposal_identity",
        "durable sweep proposal identity or candidate count differs from its locator-removal completion",
      ));
    }

    let digest_memory_bytes =
      request.completion.outcomes().len().checked_mul(std::mem::size_of::<Vec<u8>>() + header.hash_algorithm.hash_length()).ok_or_else(
        || VoidCatalogPublicationErrorV1::invalid("void_publication_digest_memory", "incarnation digest memory size overflowed"),
      )?;
    let digest_memory_bytes = u64::try_from(digest_memory_bytes).map_err(|source| {
      VoidCatalogPublicationErrorV1::invalid(
        "void_publication_digest_memory",
        format!("incarnation digest memory size exceeds u64: {source}"),
      )
    })?;
    let _digest_memory = request
      .memory
      .reserve(MemoryOwner::GarbageCollection, digest_memory_bytes, AdmissionClass::Maintenance)
      .map_err(|source| VoidCatalogPublicationErrorV1::invalid("void_publication_digest_memory", source.to_string()))?;
    let mut reclaimed_incarnation_digests = Vec::new();
    reclaimed_incarnation_digests.try_reserve_exact(request.completion.outcomes().len())?;
    for (index, candidate) in proposal.candidate_records(header.hash_algorithm)?.enumerate() {
      let candidate = candidate?;
      let outcome =
        request.completion.outcomes().get(index).ok_or_else(|| {
          VoidCatalogPublicationErrorV1::invalid("void_publication_outcomes", "sweep completion omits a proposal outcome")
        })?;
      if outcome.ordinal as usize != index {
        return Err(VoidCatalogPublicationErrorV1::invalid(
          "void_publication_outcomes",
          "sweep completion outcomes are not in exact proposal order",
        ));
      }
      if outcome.outcome == super::gc_void::SweepOutcomeClassV1::Reclaimed {
        let mut encoded = vec![0u8; 24 + 2 * header.hash_algorithm.hash_length()];
        super::gc::encode_physical_incarnation_into(&mut encoded, &candidate, header.hash_algorithm)?;
        reclaimed_incarnation_digests.push(digest_parts(header.hash_algorithm, &[&encoded]));
      }
    }

    let mut validator = VoidCatalogClosureValidatorV1::new(
      &manifest,
      header.hash_algorithm,
      request.cancellation.clone(),
      request.closure_limits,
      request.memory,
    )?;
    validator.bind_sweep_completion_with_incarnation_digests(request.completion, &reclaimed_incarnation_digests)?;
    if manifest.free_root.iter().any(|byte| *byte != 0) {
      reader.revalidate_subtree(manifest.free_root, GcDirectoryRoleV1::FreeExtents, None, 0, &mut validator)?;
    }
    if manifest.claim_root.iter().any(|byte| *byte != 0) {
      reader.revalidate_subtree(manifest.claim_root, GcDirectoryRoleV1::Claims, None, 0, &mut validator)?;
    }
    validator.finish().map_err(Into::into)
  }

  /// Hard-publish one immutable page or directory used by a future Void
  /// catalog. Claims are restricted to the selector-owning admission path.
  pub fn publish_void_catalog_support_artifact(
    &self,
    request: VoidCatalogSupportPublicationRequestV1<'_>,
  ) -> Result<VoidCatalogSupportPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    let observation = self.observe()?;
    let algorithm = observation.selected.header.hash_algorithm;
    let decoded = decode_sweep_void_artifact(&request.artifact.value, algorithm)?;
    let (kind, database_id) = match &decoded {
      SweepVoidArtifactV1::VoidExtentPage(page) => (GcArtifactKindV1::VoidExtentPage, page.database_id),
      SweepVoidArtifactV1::VoidClaim(claim) => {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "void_support_claim_owner",
          format!("generic Void support rejects claim publication outside claim admission for database {}", hex::encode(claim.database_id)),
        ));
      }
      SweepVoidArtifactV1::VoidDirectory(directory)
        if matches!(directory.role, GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims) =>
      {
        (GcArtifactKindV1::GcArtifactDirectoryNode, directory.database_id)
      }
      SweepVoidArtifactV1::VoidDirectory(_) => {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "void_support_role",
          "Void support publication rejects non-Void directory roles",
        ));
      }
      SweepVoidArtifactV1::SweepProposal(_)
      | SweepVoidArtifactV1::SweepReceipt(_)
      | SweepVoidArtifactV1::VoidCatalog(_)
      | SweepVoidArtifactV1::VoidClaimSettlement(_) => {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "void_support_kind",
          "Void support publication accepts only extent pages and Void directories",
        ));
      }
    };
    if database_id != request.database_id || decoded.key() != request.artifact.key {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "void_support_identity",
        "Void support artifact database or canonical key disagrees with its request",
      ));
    }
    let hard_publication_sequence = self.publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind,
        database_id: request.database_id,
        artifact_key: &request.artifact.key,
        value: &request.artifact.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "void_support_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )?;
    Ok(VoidCatalogSupportPublicationReceiptV1 {
      artifact_key: request.artifact.key.clone(),
      artifact_kind: kind,
      hard_publication_sequence,
    })
  }

  /// Reserve exact selected free extents by selecting a replacement catalog
  /// that removes them and contains their immutable outstanding claim.
  pub fn admit_void_claim(
    &mut self,
    request: VoidClaimAdmissionRequestV1<'_>,
    authority: &mut dyn VoidClaimAdmissionAuthorityV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  ) -> Result<VoidClaimAdmissionPermitV1, VoidClaimAdmissionErrorV1> {
    self.admit_void_claim_with_control_observer(request, authority, retirement_owner, &mut NoopFirstAuthorityDependencyObserverV1)
  }

  fn admit_void_claim_with_control_observer(
    &mut self,
    request: VoidClaimAdmissionRequestV1<'_>,
    authority: &mut dyn VoidClaimAdmissionAuthorityV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<VoidClaimAdmissionPermitV1, VoidClaimAdmissionErrorV1> {
    if request.cancellation.is_cancelled() {
      return Err(VoidClaimAdmissionErrorV1::invalid(
        "void_claim_admission_canceled",
        "Void claim admission was canceled before authority selection",
      ));
    }
    let observation = self.observe()?;
    if retirement_owner.hash_algorithm() != observation.selected.header.hash_algorithm
      || retirement_owner.database_id() != observation.selected.header.database_id
    {
      return Err(VoidClaimAdmissionErrorV1::invalid(
        "void_claim_admission_lineage_identity",
        "retirement lineage owner differs from selected first authority",
      ));
    }
    retirement_owner.flush(self)?;
    let locked = self.admit_void_claim_locked(&request, authority, retirement_owner, control_observer)?;
    let mut committed_failure = locked.committed_failure.map(|(failure, message)| (failure.code(), message));
    let lineage_state = if locked.control.replaced_control {
      match retirement_owner.flush(self) {
        Ok(true) => RootRetirementLineageStateV1::HardPublished {
          hard_publication_sequence: retirement_owner.status().last_hard_publication_sequence,
        },
        Ok(false) => RootRetirementLineageStateV1::MissingAfterCommit {
          code: "void_claim_admission_lineage_missing",
          message: "Void claim control replacement committed without retained retirement lineage".to_string(),
        },
        Err(source) => RootRetirementLineageStateV1::BufferedAfterFlushFailure { code: source.code(), message: source.to_string() },
      }
    } else {
      RootRetirementLineageStateV1::NotRequired
    };
    if !matches!(lineage_state, RootRetirementLineageStateV1::NotRequired | RootRetirementLineageStateV1::HardPublished { .. })
      && committed_failure.is_none()
    {
      committed_failure =
        Some(("void_claim_admission_committed_lineage", "Void claim replacement lineage did not hard-publish".to_string()));
    }
    let observation = if matches!(lineage_state, RootRetirementLineageStateV1::HardPublished { .. }) {
      match self.observe() {
        Ok(observation) => observation,
        Err(source) => {
          if committed_failure.is_none() {
            committed_failure = Some(("void_claim_admission_committed_lineage_readback", source.to_string()));
          }
          locked.control.observation
        }
      }
    } else {
      locked.control.observation
    };
    let algorithm = observation.selected.header.hash_algorithm;
    let database_id = observation.selected.header.database_id;
    let claim_key = locked.transition.claim_key.clone();
    let source_manifest_key = locked.transition.source_manifest_key.clone();
    let result_manifest_key = locked.transition.result_manifest_key.clone();
    let claim_id = locked.transition.claim_id;
    let claimed_bytes = locked.transition.claimed_bytes;
    let (claimed_extents, memory) = locked.transition.into_claimed_extents_with_memory();
    let permit = VoidClaimAdmissionPermitV1 {
      hash_algorithm: algorithm,
      database_id,
      claim_id,
      claim_key,
      claim_write_sequence: locked.claim_write_sequence,
      source_manifest_key,
      result_manifest_key,
      result_manifest_write_sequence: locked.result_manifest_write_sequence,
      result_control_key: request.result_control.key.clone(),
      result_control_sequence: locked.result_control_sequence,
      result_control_write_sequence: locked.control.control_write_sequence,
      result_control_slot: locked.control.control_slot,
      generation: locked.claim_generation,
      claimed_bytes,
      claimed_extents,
      lineage_state,
      observation,
      idempotent: locked.control.idempotent,
      _memory: memory,
    };
    if let Some((code, message)) = committed_failure {
      return Err(VoidClaimAdmissionErrorV1::committed(code, message, permit));
    }
    Ok(permit)
  }

  fn admit_void_claim_locked(
    &self,
    request: &VoidClaimAdmissionRequestV1<'_>,
    authority: &mut dyn VoidClaimAdmissionAuthorityV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<VoidClaimLockedAdmissionV1, VoidClaimAdmissionErrorV1> {
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      VoidClaimAdmissionErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded || header.head_hash.iter().all(|byte| *byte == 0) {
      return Err(VoidClaimAdmissionErrorV1::invalid(
        "void_claim_admission_database_authority",
        "selected first authority is absent or degraded",
      ));
    }
    if retirement_owner.hash_algorithm() != header.hash_algorithm || retirement_owner.database_id() != header.database_id {
      return Err(VoidClaimAdmissionErrorV1::invalid(
        "void_claim_admission_lineage_identity",
        "retirement lineage owner differs from selected first authority",
      ));
    }
    let (claim, result_manifest, result_control) = validate_void_claim_admission_request(request, header)?;
    let selected_control = {
      let kv = self.lock_kv()?;
      validate_kv_header_alignment(&kv, header)?;
      select_void_catalog_control(&self.file, &kv, header).map_err(|source| VoidClaimAdmissionErrorV1::support(&source))?
    }
    .ok_or_else(|| {
      VoidClaimAdmissionErrorV1::invalid(
        "void_claim_admission_source_missing",
        "claim admission requires one selected receipt-backed source Void catalog",
      )
    })?;
    let exact_retry =
      selected_control.stored_value == request.result_control.value && selected_control.target_manifest_hash == request.result_manifest.key;
    let selected_source_control_sequence = if exact_retry {
      result_manifest.previous_control_sequence
    } else {
      if selected_control.target_manifest_hash != claim.source_manifest_hash
        || selected_control.control_sequence != result_manifest.previous_control_sequence
      {
        return Err(VoidClaimAdmissionErrorV1::invalid(
          "void_claim_admission_source_changed",
          "physically selected Void source differs from the claim and replacement predecessor",
        ));
      }
      selected_control.control_sequence
    };

    let claim_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::VoidClaim,
        database_id: &header.database_id,
        artifact_key: &request.claim.key,
        value: &request.claim.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "void_claim_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )?;
    let post_claim_observation = self.observe()?;
    let post_claim_header = &post_claim_observation.selected.header;
    if post_claim_observation.selected.redundancy_degraded
      || post_claim_header.hash_algorithm != header.hash_algorithm
      || post_claim_header.database_id != header.database_id
    {
      return Err(VoidClaimAdmissionErrorV1::invalid(
        "void_claim_admission_database_changed",
        "first authority changed or degraded after immutable claim publication",
      ));
    }
    let verified = self.verify_void_claim_transition_is_durable(request, post_claim_header, claim_write_sequence)?;
    let source_entity =
      decode_whole_entity(&verified.source_entity.bytes, post_claim_header.hash_algorithm, post_claim_header.write_sequence_high_water)?;
    let SweepVoidArtifactV1::VoidCatalog(source_manifest) =
      decode_sweep_void_artifact(source_entity.stored_value, post_claim_header.hash_algorithm)?
    else {
      return Err(VoidClaimAdmissionErrorV1::invalid(
        "void_claim_admission_source_kind",
        "selected source key resolves to another GC artifact kind",
      ));
    };
    if !exact_retry {
      if request.cancellation.is_cancelled() {
        return Err(VoidClaimAdmissionErrorV1::invalid(
          "void_claim_admission_canceled",
          "Void claim admission was canceled during final authority recheck",
        ));
      }
      let snapshot = authority.recheck_void_claim_admission_authority(VoidClaimAdmissionAuthorityRequestV1 {
        source_manifest: &source_manifest,
        result_manifest: &result_manifest,
        claim: &claim,
        transition: &verified.transition,
        selected_source_control_sequence,
        cancellation: request.cancellation,
      })?;
      validate_void_claim_admission_authority_v1(
        VoidClaimAdmissionAuthorityRequestV1 {
          source_manifest: &source_manifest,
          result_manifest: &result_manifest,
          claim: &claim,
          transition: &verified.transition,
          selected_source_control_sequence,
          cancellation: request.cancellation,
        },
        &snapshot,
      )?;
    }

    let result_manifest_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::VoidCatalogManifest,
        database_id: &header.database_id,
        artifact_key: &request.result_manifest.key,
        value: &request.result_manifest.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "void_claim_manifest_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )?;
    let control = self.publish_gc_active_control_locked(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::VoidCatalogActiveControl,
        encoded_control: request.result_control,
        publication_timestamp_ms: request.publication_timestamp_ms,
        monotonic_now_ms: request.monotonic_now_ms,
      },
      retirement_owner,
      control_observer,
    )?;
    let (control, committed_failure) = match control {
      GcControlPublicationOutcomeV1::Complete(control) => (control, None),
      GcControlPublicationOutcomeV1::CommittedFailure { publication, failure, message } => (publication, Some((failure, message))),
    };
    Ok(VoidClaimLockedAdmissionV1 {
      claim_write_sequence: verified.claim_write_sequence,
      claim_generation: claim.generation,
      transition: verified.transition,
      result_manifest_write_sequence,
      result_control_sequence: result_control.sequence,
      control,
      committed_failure,
    })
  }

  fn verify_void_claim_transition_is_durable(
    &self,
    request: &VoidClaimAdmissionRequestV1<'_>,
    header: &DatabaseHeaderV4,
    claim_write_sequence: u64,
  ) -> Result<VerifiedVoidClaimTransitionV1, VoidClaimAdmissionErrorV1> {
    let claim_artifact = decode_sweep_void_artifact(&request.claim.value, header.hash_algorithm)?;
    let result_artifact = decode_sweep_void_artifact(&request.result_manifest.value, header.hash_algorithm)?;
    let SweepVoidArtifactV1::VoidClaim(claim) = &claim_artifact else {
      return Err(VoidClaimAdmissionErrorV1::invalid("void_claim_admission_claim_kind", "claim bytes decode as another GC artifact kind"));
    };
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let reader =
      VoidCatalogSupportReadContextV1 { file: &self.file, kv: &kv, header, memory: request.memory, cancellation: request.cancellation };
    let source_entity = reader
      .load_entity(claim.source_manifest_hash, GcArtifactKindV1::VoidCatalogManifest)
      .map_err(|source| VoidClaimAdmissionErrorV1::support(&source))?;
    let source_whole = decode_whole_entity(&source_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    let source_artifact = decode_sweep_void_artifact(source_whole.stored_value, header.hash_algorithm)?;
    let durable_claim_entity =
      reader.load_entity(&request.claim.key, GcArtifactKindV1::VoidClaim).map_err(|source| VoidClaimAdmissionErrorV1::support(&source))?;
    let durable_claim = decode_whole_entity(&durable_claim_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    if durable_claim.write_sequence != claim_write_sequence || durable_claim.stored_value != request.claim.value {
      return Err(VoidClaimAdmissionErrorV1::invalid(
        "void_claim_admission_claim_changed",
        "durable claim sequence or bytes differ from the requested immutable claim",
      ));
    }
    let mut validator = VoidClaimTransitionValidatorV1::new(
      &source_artifact,
      &result_artifact,
      &claim_artifact,
      request.cancellation.clone(),
      request.transition_limits,
      request.memory,
    )?;
    let SweepVoidArtifactV1::VoidCatalog(source_manifest) = &source_artifact else {
      return Err(VoidClaimAdmissionErrorV1::invalid(
        "void_claim_admission_source_kind",
        "source manifest key resolves to another GC artifact kind",
      ));
    };
    if source_manifest.free_root.iter().any(|byte| *byte != 0) {
      let mut observer = VoidClaimSourceTransitionObserverV1 { validator: &mut validator };
      reader
        .revalidate_subtree(source_manifest.free_root, GcDirectoryRoleV1::FreeExtents, None, 0, &mut observer)
        .map_err(|source| VoidClaimAdmissionErrorV1::support(&source))?;
    }
    if source_manifest.claim_root.iter().any(|byte| *byte != 0) {
      let mut observer = VoidClaimSourceTransitionObserverV1 { validator: &mut validator };
      reader
        .revalidate_subtree(source_manifest.claim_root, GcDirectoryRoleV1::Claims, None, 0, &mut observer)
        .map_err(|source| VoidClaimAdmissionErrorV1::support(&source))?;
    }
    validator.finish_source()?;
    let SweepVoidArtifactV1::VoidCatalog(result_manifest) = &result_artifact else {
      return Err(VoidClaimAdmissionErrorV1::invalid(
        "void_claim_admission_result_kind",
        "result manifest bytes decode as another GC artifact kind",
      ));
    };
    if result_manifest.free_root.iter().any(|byte| *byte != 0) {
      let mut observer = VoidClaimResultTransitionObserverV1 { validator: &mut validator };
      reader
        .revalidate_subtree(result_manifest.free_root, GcDirectoryRoleV1::FreeExtents, None, 0, &mut observer)
        .map_err(|source| VoidClaimAdmissionErrorV1::support(&source))?;
    }
    if result_manifest.claim_root.iter().any(|byte| *byte != 0) {
      let mut observer = VoidClaimResultTransitionObserverV1 { validator: &mut validator };
      reader
        .revalidate_subtree(result_manifest.claim_root, GcDirectoryRoleV1::Claims, None, 0, &mut observer)
        .map_err(|source| VoidClaimAdmissionErrorV1::support(&source))?;
    }
    let transition = validator.finish()?;
    Ok(VerifiedVoidClaimTransitionV1 { source_entity, claim_write_sequence, transition })
  }

  /// Select a claim-free Void catalog only after every used locator and every
  /// uncertain range has independent durable evidence. The immutable
  /// settlement receipt is published after selector and lineage durability.
  pub fn settle_void_claim(
    &mut self,
    consumption: &VoidClaimConsumptionPermitV1,
    request: VoidClaimSettlementPublicationRequestV1<'_>,
    authority: &mut dyn VoidClaimSettlementAuthorityV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
  ) -> Result<VoidClaimSettlementHardPublicationReceiptV1, VoidClaimSettlementPublicationErrorV1> {
    self.settle_void_claim_with_control_observer(
      consumption,
      request,
      authority,
      retirement_owner,
      &mut NoopFirstAuthorityDependencyObserverV1,
    )
  }

  fn settle_void_claim_with_control_observer(
    &mut self,
    consumption: &VoidClaimConsumptionPermitV1,
    request: VoidClaimSettlementPublicationRequestV1<'_>,
    authority: &mut dyn VoidClaimSettlementAuthorityV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<VoidClaimSettlementHardPublicationReceiptV1, VoidClaimSettlementPublicationErrorV1> {
    if request.cancellation.is_cancelled() {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_canceled",
        "Void claim settlement was canceled before authority selection",
      ));
    }
    let observation = self.observe()?;
    if retirement_owner.hash_algorithm() != observation.selected.header.hash_algorithm
      || retirement_owner.database_id() != observation.selected.header.database_id
      || consumption.hash_algorithm() != observation.selected.header.hash_algorithm
      || consumption.database_id() != observation.selected.header.database_id
    {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_lineage_identity",
        "settlement permit or retirement lineage owner differs from selected first authority",
      ));
    }
    retirement_owner.flush(self)?;
    let locked = self.settle_void_claim_locked(consumption, &request, authority, retirement_owner, control_observer)?;
    let mut committed_failure = locked.committed_failure.map(|(failure, message)| (failure.code(), message));
    let lineage_state = if locked.control.replaced_control {
      match retirement_owner.flush(self) {
        Ok(true) => RootRetirementLineageStateV1::HardPublished {
          hard_publication_sequence: retirement_owner.status().last_hard_publication_sequence,
        },
        Ok(false) => RootRetirementLineageStateV1::MissingAfterCommit {
          code: "void_claim_settlement_lineage_missing",
          message: "Void claim settlement control committed without retained replacement lineage".to_string(),
        },
        Err(source) => RootRetirementLineageStateV1::BufferedAfterFlushFailure { code: source.code(), message: source.to_string() },
      }
    } else {
      RootRetirementLineageStateV1::NotRequired
    };
    if !matches!(lineage_state, RootRetirementLineageStateV1::NotRequired | RootRetirementLineageStateV1::HardPublished { .. })
      && committed_failure.is_none()
    {
      committed_failure =
        Some(("void_claim_settlement_committed_lineage", "Void claim settlement replacement lineage did not hard-publish".to_string()));
    }
    let observation = match self.observe() {
      Ok(observation) => observation,
      Err(source) => {
        let receipt = VoidClaimSettlementHardPublicationReceiptV1 {
          result_manifest_key: request.result_manifest.key.clone(),
          result_manifest_write_sequence: locked.result_manifest_write_sequence,
          result_control_key: request.result_control.key.clone(),
          result_control_write_sequence: locked.control.control_write_sequence,
          result_control_slot: locked.control.control_slot,
          settlement_key: request.settlement.key.clone(),
          settlement_write_sequence: 0,
          outcome: consumption.outcome(),
          lineage_state,
          observation: locked.control.observation,
          idempotent: false,
        };
        return Err(VoidClaimSettlementPublicationErrorV1::committed(
          "void_claim_settlement_committed_readback",
          source.to_string(),
          receipt,
        ));
      }
    };
    let mut receipt = VoidClaimSettlementHardPublicationReceiptV1 {
      result_manifest_key: request.result_manifest.key.clone(),
      result_manifest_write_sequence: locked.result_manifest_write_sequence,
      result_control_key: request.result_control.key.clone(),
      result_control_write_sequence: locked.control.control_write_sequence,
      result_control_slot: locked.control.control_slot,
      settlement_key: request.settlement.key.clone(),
      settlement_write_sequence: 0,
      outcome: consumption.outcome(),
      lineage_state,
      observation,
      idempotent: false,
    };
    if let Some((code, message)) = committed_failure {
      return Err(VoidClaimSettlementPublicationErrorV1::committed(code, message, receipt));
    }
    let settlement_artifact = decode_sweep_void_artifact(&request.settlement.value, consumption.hash_algorithm())?;
    let SweepVoidArtifactV1::VoidClaimSettlement(settlement) = settlement_artifact else {
      return Err(VoidClaimSettlementPublicationErrorV1::committed(
        "void_claim_settlement_receipt_kind",
        "selected claim-free result has a settlement artifact of another kind",
        receipt,
      ));
    };
    let settlement_timestamp_ms = u64::try_from(settlement.settled_at_ms)?;
    let settlement_write_sequence = match self.publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::VoidClaimSettlementReceipt,
        database_id: &consumption.database_id(),
        artifact_key: &request.settlement.key,
        value: &request.settlement.value,
        minimum_timestamp_ms: settlement_timestamp_ms.max(request.publication_timestamp_ms),
        committed_postcondition_code: "void_claim_settlement_receipt_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    ) {
      Ok(sequence) => sequence,
      Err(source) => {
        return Err(VoidClaimSettlementPublicationErrorV1::committed(
          "void_claim_settlement_committed_receipt",
          source.to_string(),
          receipt,
        ));
      }
    };
    receipt.settlement_write_sequence = settlement_write_sequence;
    receipt.idempotent = locked.control.idempotent && locked.settlement_preexisting;
    receipt.observation = match self.observe() {
      Ok(observation) => observation,
      Err(source) => {
        return Err(VoidClaimSettlementPublicationErrorV1::committed(
          "void_claim_settlement_committed_receipt_readback",
          source.to_string(),
          receipt,
        ));
      }
    };
    Ok(receipt)
  }

  fn settle_void_claim_locked(
    &self,
    consumption: &VoidClaimConsumptionPermitV1,
    request: &VoidClaimSettlementPublicationRequestV1<'_>,
    authority: &mut dyn VoidClaimSettlementAuthorityV1,
    retirement_owner: &mut RetirementJournalOwnerV1<'_>,
    control_observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<VoidClaimLockedSettlementV1, VoidClaimSettlementPublicationErrorV1> {
    let _authority = self.root_state.lock().map_err(|poisoned| {
      drop(poisoned);
      VoidClaimSettlementPublicationErrorV1::Authority(FirstAuthorityPublicationErrorV1::StateLockPoisoned)
    })?;
    let observation = self.observe()?;
    let header = &observation.selected.header;
    if observation.selected.redundancy_degraded
      || header.head_hash.iter().all(|byte| *byte == 0)
      || header.hash_algorithm != consumption.hash_algorithm()
      || header.database_id != consumption.database_id()
    {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_database_authority",
        "selected first authority is absent, degraded, or differs from the settlement permit",
      ));
    }
    if retirement_owner.hash_algorithm() != header.hash_algorithm || retirement_owner.database_id() != header.database_id {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_lineage_identity",
        "retirement lineage owner differs from selected first authority",
      ));
    }
    let result_control = decode_gc_active_control(&request.result_control.value, header.hash_algorithm)?;
    let result_artifact = decode_sweep_void_artifact(&request.result_manifest.value, header.hash_algorithm)?;
    let SweepVoidArtifactV1::VoidCatalog(result_manifest) = &result_artifact else {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_result_kind",
        "result manifest bytes decode as another GC artifact kind",
      ));
    };
    if request.result_manifest.key != result_manifest.key
      || result_control.kind != GcArtifactKindV1::VoidCatalogActiveControl
      || result_control.database_id != consumption.database_id()
      || result_control.target_manifest_hash != request.result_manifest.key
      || result_control.generation != result_manifest.generation
      || result_control.sequence
        != consumption.source_control_sequence().checked_add(1).ok_or_else(|| {
          VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_control_sequence", "control sequence overflowed")
        })?
      || result_control.slot == consumption.source_control_slot()
      || result_control.key != request.result_control.key
    {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_result_control",
        "result manifest and inactive control do not form the next Void authority",
      ));
    }
    let selected_control = {
      let kv = self.lock_kv()?;
      validate_kv_header_alignment(&kv, header)?;
      select_void_catalog_control(&self.file, &kv, header)
        .map_err(|source| VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_source_control", source.to_string()))?
    }
    .ok_or_else(|| {
      VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_source_missing",
        "settlement requires one selected outstanding-claim Void catalog",
      )
    })?;
    let selected_decoded = decode_gc_active_control(&selected_control.stored_value, header.hash_algorithm)?;
    let exact_retry =
      selected_control.stored_value == request.result_control.value && selected_control.target_manifest_hash == request.result_manifest.key;
    if !exact_retry
      && (selected_control.target_manifest_hash != consumption.source_manifest_key()
        || selected_control.control_sequence != consumption.source_control_sequence()
        || selected_control.write_sequence != consumption.source_control_write_sequence()
        || selected_decoded.key != consumption.source_control_key()
        || selected_decoded.slot != consumption.source_control_slot())
    {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_source_changed",
        "physically selected Void source differs from the consumed claim permit",
      ));
    }

    let verified = self.verify_void_claim_settlement_transition_is_durable(consumption, request, header)?;
    let source_whole = decode_whole_entity(&verified.source_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    let source_artifact = decode_sweep_void_artifact(source_whole.stored_value, header.hash_algorithm)?;
    let SweepVoidArtifactV1::VoidCatalog(source_manifest) = &source_artifact else {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_source_kind",
        "durable source manifest decoded as another GC artifact kind",
      ));
    };
    let claim_whole = decode_whole_entity(&verified.claim_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    let claim_artifact = decode_sweep_void_artifact(claim_whole.stored_value, header.hash_algorithm)?;
    let SweepVoidArtifactV1::VoidClaim(claim) = &claim_artifact else {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_claim_kind",
        "durable claim decoded as another GC artifact kind",
      ));
    };
    validate_requested_void_claim_settlement(consumption, request, source_manifest, result_manifest, claim)?;
    let authority_request = VoidClaimSettlementAuthorityRequestV1 {
      source_manifest,
      result_manifest,
      claim,
      transition: &verified.transition,
      consumption,
      cancellation: request.cancellation,
    };
    let snapshot = authority.recheck_void_claim_settlement_authority(authority_request)?;
    validate_void_claim_settlement_authority_v1(authority_request, &snapshot)?;
    let settlement_preexisting = if let Some(existing) = snapshot.existing_receipt.as_ref() {
      if !exact_retry || existing.receipt_hash != request.settlement.key {
        return Err(VoidClaimSettlementPublicationErrorV1::invalid(
          "void_claim_settlement_existing_conflict",
          "existing settlement receipt differs from the exact selected result",
        ));
      }
      let kv = self.lock_kv()?;
      let reader =
        VoidCatalogSupportReadContextV1 { file: &self.file, kv: &kv, header, memory: request.memory, cancellation: request.cancellation };
      let entity = reader
        .load_entity(&existing.receipt_hash, GcArtifactKindV1::VoidClaimSettlementReceipt)
        .map_err(|source| VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_existing", source.to_string()))?;
      let whole = decode_whole_entity(&entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
      if whole.write_sequence != existing.receipt_write_sequence || whole.stored_value != request.settlement.value {
        return Err(VoidClaimSettlementPublicationErrorV1::invalid(
          "void_claim_settlement_existing_conflict",
          "existing settlement receipt sequence or bytes differ from the exact request",
        ));
      }
      true
    } else {
      false
    };
    if request.cancellation.is_cancelled() {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_canceled",
        "Void claim settlement was canceled after final authority recheck",
      ));
    }
    let result_manifest_write_sequence = self.publish_immutable_gc_artifact_locked(
      ImmutableGcArtifactPublicationV1 {
        kind: GcArtifactKindV1::VoidCatalogManifest,
        database_id: &header.database_id,
        artifact_key: &request.result_manifest.key,
        value: &request.result_manifest.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "void_claim_settlement_manifest_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )?;
    let control = self.publish_gc_active_control_locked(
      GcControlPublicationRequestV1 {
        expected_control_kind: GcArtifactKindV1::VoidCatalogActiveControl,
        encoded_control: request.result_control,
        publication_timestamp_ms: request.publication_timestamp_ms,
        monotonic_now_ms: request.monotonic_now_ms,
      },
      retirement_owner,
      control_observer,
    )?;
    let (control, committed_failure) = match control {
      GcControlPublicationOutcomeV1::Complete(control) => (control, None),
      GcControlPublicationOutcomeV1::CommittedFailure { publication, failure, message } => (publication, Some((failure, message))),
    };
    Ok(VoidClaimLockedSettlementV1 { result_manifest_write_sequence, control, settlement_preexisting, committed_failure })
  }

  fn verify_void_claim_settlement_transition_is_durable(
    &self,
    consumption: &VoidClaimConsumptionPermitV1,
    request: &VoidClaimSettlementPublicationRequestV1<'_>,
    header: &DatabaseHeaderV4,
  ) -> Result<VerifiedVoidClaimSettlementTransitionV1, VoidClaimSettlementPublicationErrorV1> {
    let result_artifact = decode_sweep_void_artifact(&request.result_manifest.value, header.hash_algorithm)?;
    let kv = self.lock_kv()?;
    validate_kv_header_alignment(&kv, header)?;
    let reader =
      VoidCatalogSupportReadContextV1 { file: &self.file, kv: &kv, header, memory: request.memory, cancellation: request.cancellation };
    let source_entity = reader
      .load_entity(consumption.source_manifest_key(), GcArtifactKindV1::VoidCatalogManifest)
      .map_err(|source| VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_source_support", source.to_string()))?;
    let source_whole = decode_whole_entity(&source_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    if source_whole.write_sequence != consumption.source_manifest_write_sequence() {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_source_sequence",
        "durable source manifest sequence differs from the consumption permit",
      ));
    }
    let source_artifact = decode_sweep_void_artifact(source_whole.stored_value, header.hash_algorithm)?;
    let claim_entity = reader
      .load_entity(consumption.claim_key(), GcArtifactKindV1::VoidClaim)
      .map_err(|source| VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_claim_support", source.to_string()))?;
    let claim_whole = decode_whole_entity(&claim_entity.bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    if claim_whole.write_sequence != consumption.claim_write_sequence() {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_claim_sequence",
        "durable claim sequence differs from the consumption permit",
      ));
    }
    let claim_artifact = decode_sweep_void_artifact(claim_whole.stored_value, header.hash_algorithm)?;
    let mut validator = VoidClaimSettlementTransitionValidatorV1::new(
      &source_artifact,
      &result_artifact,
      &claim_artifact,
      consumption,
      request.cancellation.clone(),
      request.transition_limits,
      request.memory,
    )?;
    let SweepVoidArtifactV1::VoidCatalog(source_manifest) = &source_artifact else {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_source_kind",
        "source manifest key resolves to another GC artifact kind",
      ));
    };
    if source_manifest.free_root.iter().any(|byte| *byte != 0) {
      let mut observer = VoidClaimSettlementSourceTransitionObserverV1 { validator: &mut validator };
      reader
        .revalidate_subtree(source_manifest.free_root, GcDirectoryRoleV1::FreeExtents, None, 0, &mut observer)
        .map_err(|source| VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_source_support", source.to_string()))?;
    }
    if source_manifest.claim_root.iter().any(|byte| *byte != 0) {
      let mut observer = VoidClaimSettlementSourceTransitionObserverV1 { validator: &mut validator };
      reader
        .revalidate_subtree(source_manifest.claim_root, GcDirectoryRoleV1::Claims, None, 0, &mut observer)
        .map_err(|source| VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_source_support", source.to_string()))?;
    }
    validator.finish_source()?;
    let SweepVoidArtifactV1::VoidCatalog(result_manifest) = &result_artifact else {
      return Err(VoidClaimSettlementPublicationErrorV1::invalid(
        "void_claim_settlement_result_kind",
        "result manifest bytes decode as another GC artifact kind",
      ));
    };
    if result_manifest.free_root.iter().any(|byte| *byte != 0) {
      let mut observer = VoidClaimSettlementResultTransitionObserverV1 { validator: &mut validator };
      reader
        .revalidate_subtree(result_manifest.free_root, GcDirectoryRoleV1::FreeExtents, None, 0, &mut observer)
        .map_err(|source| VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_result_support", source.to_string()))?;
    }
    if result_manifest.claim_root.iter().any(|byte| *byte != 0) {
      let mut observer = VoidClaimSettlementResultTransitionObserverV1 { validator: &mut validator };
      reader
        .revalidate_subtree(result_manifest.claim_root, GcDirectoryRoleV1::Claims, None, 0, &mut observer)
        .map_err(|source| VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_result_support", source.to_string()))?;
    }
    let transition = validator.finish()?;
    Ok(VerifiedVoidClaimSettlementTransitionV1 { source_entity, claim_entity, transition })
  }

  /// Hard-publish one immutable page or directory used by a future root-
  /// lifecycle manifest. This method never selects lifecycle authority.
  pub fn publish_root_lifecycle_support_artifact(
    &self,
    request: RootLifecycleSupportPublicationRequestV1<'_>,
  ) -> Result<RootLifecycleSupportPublicationReceiptV1, FirstAuthorityPublicationErrorV1> {
    let observation = self.observe()?;
    let algorithm = observation.selected.header.hash_algorithm;
    let decoded = decode_gc_state_artifact(&request.artifact.value, algorithm)?;
    let (kind, database_id, role) = match &decoded {
      GcStateArtifactV1::Page(page) => {
        let kind = match page.role {
          GcDirectoryRoleV1::RootCandidates => GcArtifactKindV1::RootCandidatePage,
          GcDirectoryRoleV1::RootExpiry => GcArtifactKindV1::RootExpiryPage,
          GcDirectoryRoleV1::Candidates
          | GcDirectoryRoleV1::PhysicalInventory
          | GcDirectoryRoleV1::FreeExtents
          | GcDirectoryRoleV1::Claims => {
            return Err(FirstAuthorityPublicationErrorV1::invalid(
              "root_lifecycle_support_role",
              "root-lifecycle support publication rejects non-lifecycle page roles",
            ));
          }
        };
        (kind, page.database_id, page.role)
      }
      GcStateArtifactV1::Directory(directory) => {
        if !matches!(directory.role, GcDirectoryRoleV1::RootCandidates | GcDirectoryRoleV1::RootExpiry) {
          return Err(FirstAuthorityPublicationErrorV1::invalid(
            "root_lifecycle_support_role",
            "root-lifecycle support publication rejects non-lifecycle directory roles",
          ));
        }
        (GcArtifactKindV1::GcArtifactDirectoryNode, directory.database_id, directory.role)
      }
      GcStateArtifactV1::Manifest(_)
      | GcStateArtifactV1::CandidateDelta { .. }
      | GcStateArtifactV1::RootRetirementCommit { .. }
      | GcStateArtifactV1::RootObjectReclaimProof { .. }
      | GcStateArtifactV1::RetirementJournal { .. } => {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "root_lifecycle_support_kind",
          "root-lifecycle support publication accepts only candidate/expiry pages and directories",
        ));
      }
    };
    if database_id != request.database_id || decoded.key() != request.artifact.key {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "root_lifecycle_support_identity",
        "root-lifecycle support artifact database or canonical key disagrees with its request",
      ));
    }
    debug_assert!(matches!(role, GcDirectoryRoleV1::RootCandidates | GcDirectoryRoleV1::RootExpiry));

    let hard_publication_sequence = self.publish_immutable_gc_artifact(
      ImmutableGcArtifactPublicationV1 {
        kind,
        database_id: request.database_id,
        artifact_key: &request.artifact.key,
        value: &request.artifact.value,
        minimum_timestamp_ms: request.publication_timestamp_ms,
        committed_postcondition_code: "root_lifecycle_support_committed_postcondition",
      },
      &mut NoopFirstAuthorityDependencyObserverV1,
    )?;
    Ok(RootLifecycleSupportPublicationReceiptV1 {
      artifact_key: request.artifact.key.clone(),
      artifact_kind: kind,
      hard_publication_sequence,
    })
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
    self.publish_immutable_gc_artifact_locked(request, observer)
  }

  fn publish_immutable_gc_artifact_locked(
    &self,
    request: ImmutableGcArtifactPublicationV1<'_>,
    observer: &mut dyn FirstAuthorityDependencyObserverV1,
  ) -> Result<u64, FirstAuthorityPublicationErrorV1> {
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
    let expected_existing = [None];
    let (publication_result, append_completed) = {
      let mut dependency = FirstAuthorityDependencyV1 {
        file: &self.file,
        kv: &mut kv,
        batch,
        expected_publication_sequence: authority_sequence,
        entities: &entities,
        start_offset: append_start,
        expected_hot_tail_offset,
        expected_existing: &expected_existing,
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
    let expected_existing: [Option<KVEntry>; FIRST_AUTHORITY_ENTITY_COUNT] = std::array::from_fn(|_| None);
    let (publication_result, append_completed) = {
      let mut dependency = FirstAuthorityDependencyV1 {
        file: &self.file,
        kv: &mut kv,
        batch,
        expected_publication_sequence: publication_sequence,
        entities: &package.entities,
        start_offset: header.hot_tail_offset,
        expected_hot_tail_offset: package.hot_tail_offset,
        expected_existing: &expected_existing,
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
  expected_existing: &'a [Option<KVEntry>],
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
    if self.entities.len() != self.expected_existing.len() {
      return Err(NativeDurabilityError::invalid(
        NativeDurabilityOperation::WriteAt,
        "first-authority dependency expectations do not match its entity count",
      ));
    }
    for (entity, expected_existing) in self.entities.iter().zip(self.expected_existing) {
      let observed = self.kv.get(&entity.key).map_err(native_engine_error)?;
      if observed.as_ref() != expected_existing.as_ref() {
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

struct ChargedRootLifecycleSupportEntityV1 {
  bytes: Vec<u8>,
  _memory: MemoryReservation,
}

struct ChargedVoidCatalogSupportEntityV1 {
  bytes: Vec<u8>,
  _memory: MemoryReservation,
}

struct VoidCatalogSupportReadContextV1<'a> {
  file: &'a File,
  kv: &'a DiskKVStore,
  header: &'a DatabaseHeaderV4,
  memory: &'a MemoryCoordinator,
  cancellation: &'a CancellationToken,
}

trait VoidCatalogSupportObserverV1 {
  fn observe_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidCatalogPublicationErrorV1>;
}

impl VoidCatalogSupportObserverV1 for VoidCatalogClosureValidatorV1<'_> {
  fn observe_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidCatalogPublicationErrorV1> {
    VoidCatalogClosureValidatorV1::observe_encoded(self, bytes).map_err(Into::into)
  }
}

struct VoidClaimSourceTransitionObserverV1<'a, 'b> {
  validator: &'a mut VoidClaimTransitionValidatorV1<'b>,
}

impl VoidCatalogSupportObserverV1 for VoidClaimSourceTransitionObserverV1<'_, '_> {
  fn observe_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidCatalogPublicationErrorV1> {
    self
      .validator
      .observe_source_encoded(bytes)
      .map_err(|source| VoidCatalogPublicationErrorV1::invalid("void_claim_transition_support", format!("{}: {source}", source.code())))
  }
}

struct VoidClaimResultTransitionObserverV1<'a, 'b> {
  validator: &'a mut VoidClaimTransitionValidatorV1<'b>,
}

impl VoidCatalogSupportObserverV1 for VoidClaimResultTransitionObserverV1<'_, '_> {
  fn observe_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidCatalogPublicationErrorV1> {
    self
      .validator
      .observe_result_encoded(bytes)
      .map_err(|source| VoidCatalogPublicationErrorV1::invalid("void_claim_transition_support", format!("{}: {source}", source.code())))
  }
}

struct VoidClaimSettlementSourceTransitionObserverV1<'a, 'b> {
  validator: &'a mut VoidClaimSettlementTransitionValidatorV1<'b>,
}

impl VoidCatalogSupportObserverV1 for VoidClaimSettlementSourceTransitionObserverV1<'_, '_> {
  fn observe_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidCatalogPublicationErrorV1> {
    self.validator.observe_source_encoded(bytes).map_err(|source| {
      VoidCatalogPublicationErrorV1::invalid("void_claim_settlement_transition_support", format!("{}: {source}", source.code()))
    })
  }
}

struct VoidClaimSettlementResultTransitionObserverV1<'a, 'b> {
  validator: &'a mut VoidClaimSettlementTransitionValidatorV1<'b>,
}

impl VoidCatalogSupportObserverV1 for VoidClaimSettlementResultTransitionObserverV1<'_, '_> {
  fn observe_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidCatalogPublicationErrorV1> {
    self.validator.observe_result_encoded(bytes).map_err(|source| {
      VoidCatalogPublicationErrorV1::invalid("void_claim_settlement_transition_support", format!("{}: {source}", source.code()))
    })
  }
}

struct VoidReusableClaimsObserverV1<'a, 'b> {
  validator: &'a mut VoidReusableStateValidatorV1<'b>,
}

impl VoidCatalogSupportObserverV1 for VoidReusableClaimsObserverV1<'_, '_> {
  fn observe_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidCatalogPublicationErrorV1> {
    self
      .validator
      .observe_claim_encoded(bytes)
      .map_err(|source| VoidCatalogPublicationErrorV1::invalid("void_runtime_claim_support", format!("{}: {source}", source.code())))
  }
}

struct VoidReusableFreeObserverV1<'a, 'b> {
  validator: &'a mut VoidReusableStateValidatorV1<'b>,
  authority: &'a mut dyn VoidReclaimReceiptAuthorityV1,
}

impl VoidCatalogSupportObserverV1 for VoidReusableFreeObserverV1<'_, '_> {
  fn observe_encoded(&mut self, bytes: &[u8]) -> Result<(), VoidCatalogPublicationErrorV1> {
    self
      .validator
      .observe_free_encoded(bytes, self.authority)
      .map_err(|source| VoidCatalogPublicationErrorV1::invalid("void_runtime_free_support", format!("{}: {source}", source.code())))
  }
}

impl VoidCatalogSupportReadContextV1<'_> {
  fn revalidate_subtree(
    &self,
    key: &[u8],
    expected_role: GcDirectoryRoleV1,
    expected_level: Option<u16>,
    depth: u16,
    validator: &mut dyn VoidCatalogSupportObserverV1,
  ) -> Result<(), VoidCatalogPublicationErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(VoidCatalogPublicationErrorV1::invalid("void_publication_canceled", "Void support validation was canceled"));
    }
    if depth > 16 {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_support_depth",
        "durable Void support directory exceeds depth 16",
      ));
    }
    let directory_entity = self.load_entity(key, GcArtifactKindV1::GcArtifactDirectoryNode)?;
    let entity = decode_whole_entity(&directory_entity.bytes, self.header.hash_algorithm, self.header.write_sequence_high_water)?;
    let SweepVoidArtifactV1::VoidDirectory(directory) = decode_sweep_void_artifact(entity.stored_value, self.header.hash_algorithm)? else {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_support_kind",
        "Void directory key resolves to another GC artifact kind",
      ));
    };
    if directory.key != key || directory.role != expected_role || expected_level.is_some_and(|level| level != directory.level) {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_support_changed",
        "durable Void directory identity, role, or level differs from its parent descriptor",
      ));
    }
    for descriptor in &directory.entries {
      if directory.level == 0 {
        self.revalidate_leaf(descriptor.child_hash, expected_role, validator)?;
      } else {
        self.revalidate_subtree(descriptor.child_hash, expected_role, Some(directory.level - 1), depth + 1, validator)?;
      }
    }
    validator.observe_encoded(entity.stored_value)?;
    Ok(())
  }

  fn revalidate_leaf(
    &self,
    key: &[u8],
    expected_role: GcDirectoryRoleV1,
    validator: &mut dyn VoidCatalogSupportObserverV1,
  ) -> Result<(), VoidCatalogPublicationErrorV1> {
    let expected_kind = match expected_role {
      GcDirectoryRoleV1::FreeExtents => GcArtifactKindV1::VoidExtentPage,
      GcDirectoryRoleV1::Claims => GcArtifactKindV1::VoidClaim,
      GcDirectoryRoleV1::Candidates
      | GcDirectoryRoleV1::PhysicalInventory
      | GcDirectoryRoleV1::RootCandidates
      | GcDirectoryRoleV1::RootExpiry => {
        return Err(VoidCatalogPublicationErrorV1::invalid(
          "void_publication_support_role",
          "Void closure cannot contain a non-Void leaf role",
        ));
      }
    };
    let leaf_entity = self.load_entity(key, expected_kind)?;
    let entity = decode_whole_entity(&leaf_entity.bytes, self.header.hash_algorithm, self.header.write_sequence_high_water)?;
    let leaf = decode_sweep_void_artifact(entity.stored_value, self.header.hash_algorithm)?;
    let leaf_matches = match (&leaf, expected_role) {
      (SweepVoidArtifactV1::VoidExtentPage(page), GcDirectoryRoleV1::FreeExtents) => page.key == key,
      (SweepVoidArtifactV1::VoidClaim(claim), GcDirectoryRoleV1::Claims) => claim.key == key,
      _ => false,
    };
    if !leaf_matches {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_support_changed",
        "durable Void leaf identity or role differs from its parent descriptor",
      ));
    }
    validator.observe_encoded(entity.stored_value)?;
    Ok(())
  }

  fn load_entity(
    &self,
    key: &[u8],
    expected_kind: GcArtifactKindV1,
  ) -> Result<ChargedVoidCatalogSupportEntityV1, VoidCatalogPublicationErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_canceled",
        "Void support validation was canceled before a durable read",
      ));
    }
    let Some(locator) = self.kv.get(key).map_err(FirstAuthorityPublicationErrorV1::from)? else {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_support_missing",
        format!("Void support artifact {} is absent", hex::encode(key)),
      ));
    };
    if locator.type_flags != kv_tag::GC_ARTIFACT {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_support_collision",
        "Void support key resolves to another KV role",
      ));
    }
    let maximum_value_length = expected_kind
      .immutable_maximum_encoded_length()
      .ok_or_else(|| VoidCatalogPublicationErrorV1::invalid("void_publication_support_kind", "Void support role has no immutable cap"))?;
    let maximum_entity_length =
      super::entity::checked_whole_entity_encoded_length(self.header.hash_algorithm, key.len(), maximum_value_length)?;
    let locator_length = usize::try_from(locator.total_length).map_err(|source| {
      VoidCatalogPublicationErrorV1::invalid(
        "void_publication_support_length",
        format!("Void support locator length exceeds usize: {source}"),
      )
    })?;
    if locator_length > maximum_entity_length {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_support_length",
        format!("Void support entity length {locator_length} exceeds its {maximum_entity_length}-byte role cap"),
      ));
    }
    let reservation = self
      .memory
      .reserve(
        MemoryOwner::GarbageCollection,
        u64::try_from(locator_length).map_err(|source| {
          VoidCatalogPublicationErrorV1::invalid(
            "void_publication_support_length",
            format!("Void support length cannot be accounted: {source}"),
          )
        })?,
        AdmissionClass::Maintenance,
      )
      .map_err(|source| VoidCatalogPublicationErrorV1::invalid("void_publication_support_memory", source.to_string()))?;
    let bytes =
      read_entity_bounded(self.file, self.kv, key, maximum_entity_length, self.header.write_sequence_high_water)?.ok_or_else(|| {
        VoidCatalogPublicationErrorV1::invalid(
          "void_publication_support_missing",
          format!("Void support artifact {} disappeared", hex::encode(key)),
        )
      })?;
    let entity = decode_whole_entity(&bytes, self.header.hash_algorithm, self.header.write_sequence_high_water)?;
    if entity.entry_type != EntryTypeV4::GcArtifact
      || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
      || entity.compression_algorithm != CompressionAlgorithm::None
      || entity.key != key
    {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_support_representation",
        "Void support artifact is not one canonical system GC WholeEntity",
      ));
    }
    let envelope = decode_gc_artifact_envelope(entity.stored_value)?;
    if envelope.kind != expected_kind {
      return Err(VoidCatalogPublicationErrorV1::invalid(
        "void_publication_support_kind",
        "Void support artifact kind differs from its requested role",
      ));
    }
    Ok(ChargedVoidCatalogSupportEntityV1 { bytes, _memory: reservation })
  }
}

struct ChargedPhysicalQuarantineSupportEntityV1 {
  bytes: Vec<u8>,
  _memory: MemoryReservation,
}

struct PhysicalQuarantineSupportReadContextV1<'a> {
  file: &'a File,
  kv: &'a DiskKVStore,
  header: &'a DatabaseHeaderV4,
  memory: &'a MemoryCoordinator,
}

impl PhysicalQuarantineSupportReadContextV1<'_> {
  fn revalidate_subtree(
    &self,
    directory: &super::gc_state::GcStateDirectoryV1<'_>,
    depth: u16,
    validator: &mut super::gc_quarantine::QuarantineClosureValidatorV1<'_>,
  ) -> Result<(), PhysicalQuarantinePublicationErrorV1> {
    if depth > 16 {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_depth",
        "durable physical-quarantine candidate directory exceeds depth 16",
      ));
    }
    if directory.role != GcDirectoryRoleV1::Candidates {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_role",
        "physical-quarantine base graph contains a non-candidate directory",
      ));
    }
    for descriptor in &directory.entries {
      if directory.level == 0 {
        self.revalidate_page(descriptor.child_hash, validator)?;
      } else {
        let child_entity = self.load_entity(descriptor.child_hash, GcArtifactKindV1::GcArtifactDirectoryNode)?;
        let child_whole = decode_whole_entity(&child_entity.bytes, self.header.hash_algorithm, self.header.write_sequence_high_water)?;
        let GcStateArtifactV1::Directory(child) = decode_gc_state_artifact(child_whole.stored_value, self.header.hash_algorithm)? else {
          return Err(PhysicalQuarantinePublicationErrorV1::invalid(
            "quarantine_support_kind",
            "candidate directory descriptor resolves to another GC artifact kind",
          ));
        };
        if child.key != descriptor.child_hash || child.level + 1 != directory.level {
          return Err(PhysicalQuarantinePublicationErrorV1::invalid(
            "quarantine_support_changed",
            "durable candidate directory identity or level differs from its parent descriptor",
          ));
        }
        self.revalidate_subtree(&child, depth + 1, validator)?;
        validator.observe_base_directory(&child).map_err(physical_quarantine_support_error)?;
      }
    }
    Ok(())
  }

  fn revalidate_page(
    &self,
    key: &[u8],
    validator: &mut super::gc_quarantine::QuarantineClosureValidatorV1<'_>,
  ) -> Result<(), PhysicalQuarantinePublicationErrorV1> {
    let page_entity = self.load_entity(key, GcArtifactKindV1::CandidatePage)?;
    let page_whole = decode_whole_entity(&page_entity.bytes, self.header.hash_algorithm, self.header.write_sequence_high_water)?;
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(page_whole.stored_value, self.header.hash_algorithm)? else {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_kind",
        "candidate page key resolves to another GC artifact kind",
      ));
    };
    if page.key != key || page.role != GcDirectoryRoleV1::Candidates {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_changed",
        "durable candidate page identity or role differs from its parent descriptor",
      ));
    }
    validator.observe_base_page(&page).map_err(physical_quarantine_support_error)
  }

  fn load_entity(
    &self,
    key: &[u8],
    expected_kind: GcArtifactKindV1,
  ) -> Result<ChargedPhysicalQuarantineSupportEntityV1, PhysicalQuarantinePublicationErrorV1> {
    let Some(locator) = self.kv.get(key).map_err(FirstAuthorityPublicationErrorV1::from)? else {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_missing",
        format!("physical-quarantine support artifact {} is absent", hex::encode(key)),
      ));
    };
    if locator.type_flags != kv_tag::GC_ARTIFACT {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_collision",
        "physical-quarantine support key resolves to another KV role",
      ));
    }
    let maximum_value_length = expected_kind.immutable_maximum_encoded_length().ok_or_else(|| {
      PhysicalQuarantinePublicationErrorV1::invalid("quarantine_support_kind", "physical-quarantine support role has no immutable cap")
    })?;
    let maximum_entity_length =
      super::entity::checked_whole_entity_encoded_length(self.header.hash_algorithm, key.len(), maximum_value_length)?;
    let locator_length = usize::try_from(locator.total_length).map_err(|error| {
      PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_length",
        format!("physical-quarantine support locator length exceeds usize: {error}"),
      )
    })?;
    if locator_length > maximum_entity_length {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_length",
        format!("physical-quarantine support entity length {locator_length} exceeds its {maximum_entity_length}-byte role cap"),
      ));
    }
    let reservation_bytes = u64::try_from(locator_length).map_err(|error| {
      PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_length",
        format!("physical-quarantine support length cannot be accounted: {error}"),
      )
    })?;
    let reservation = self
      .memory
      .reserve(MemoryOwner::GarbageCollection, reservation_bytes, AdmissionClass::Maintenance)
      .map_err(|error| PhysicalQuarantinePublicationErrorV1::invalid("quarantine_support_memory", error.to_string()))?;
    let bytes =
      read_entity_bounded(self.file, self.kv, key, maximum_entity_length, self.header.write_sequence_high_water)?.ok_or_else(|| {
        PhysicalQuarantinePublicationErrorV1::invalid(
          "quarantine_support_missing",
          format!("physical-quarantine support artifact {} disappeared", hex::encode(key)),
        )
      })?;
    let entity = decode_whole_entity(&bytes, self.header.hash_algorithm, self.header.write_sequence_high_water)?;
    if entity.entry_type != EntryTypeV4::GcArtifact
      || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
      || entity.compression_algorithm != CompressionAlgorithm::None
      || entity.key != key
    {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_representation",
        "physical-quarantine support artifact is not one canonical system GC WholeEntity",
      ));
    }
    let envelope = decode_gc_artifact_envelope(entity.stored_value)?;
    if envelope.kind != expected_kind {
      return Err(PhysicalQuarantinePublicationErrorV1::invalid(
        "quarantine_support_kind",
        "physical-quarantine support artifact kind differs from its requested role",
      ));
    }
    Ok(ChargedPhysicalQuarantineSupportEntityV1 { bytes, _memory: reservation })
  }
}

struct RootLifecycleSupportReadContextV1<'a> {
  file: &'a File,
  kv: &'a DiskKVStore,
  header: &'a DatabaseHeaderV4,
  memory: &'a MemoryCoordinator,
}

impl RootLifecycleSupportReadContextV1<'_> {
  fn revalidate_subtree(
    &self,
    key: &[u8],
    expected_role: GcDirectoryRoleV1,
    expected_level: Option<u16>,
    depth: u16,
    builder: &mut super::gc_lifecycle::RootLifecycleSupportClosureBuilderV1<'_>,
  ) -> Result<(), RootRetirementPublicationErrorV1> {
    if depth > 16 {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_depth",
        "durable root-lifecycle support directory exceeds depth 16",
      ));
    }
    let directory_entity = self.load_entity(key, GcArtifactKindV1::GcArtifactDirectoryNode)?;
    let entity = decode_whole_entity(&directory_entity.bytes, self.header.hash_algorithm, self.header.write_sequence_high_water)?;
    let GcStateArtifactV1::Directory(directory) = decode_gc_state_artifact(entity.stored_value, self.header.hash_algorithm)? else {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_kind",
        "root-lifecycle support directory key resolves to another GC artifact kind",
      ));
    };
    if directory.key != key || directory.role != expected_role || expected_level.is_some_and(|level| level != directory.level) {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_changed",
        "durable root-lifecycle directory identity, role, or level differs from its parent descriptor",
      ));
    }
    for descriptor in &directory.entries {
      if directory.level == 0 {
        self.revalidate_page(descriptor.child_hash, expected_role, builder)?;
      } else {
        self.revalidate_subtree(descriptor.child_hash, expected_role, Some(directory.level - 1), depth + 1, builder)?;
      }
    }
    builder.observe_encoded(entity.stored_value).map_err(root_retirement_support_error)
  }

  fn revalidate_page(
    &self,
    key: &[u8],
    expected_role: GcDirectoryRoleV1,
    builder: &mut super::gc_lifecycle::RootLifecycleSupportClosureBuilderV1<'_>,
  ) -> Result<(), RootRetirementPublicationErrorV1> {
    let kind = match expected_role {
      GcDirectoryRoleV1::RootCandidates => GcArtifactKindV1::RootCandidatePage,
      GcDirectoryRoleV1::RootExpiry => GcArtifactKindV1::RootExpiryPage,
      GcDirectoryRoleV1::Candidates | GcDirectoryRoleV1::PhysicalInventory | GcDirectoryRoleV1::FreeExtents | GcDirectoryRoleV1::Claims => {
        return Err(RootRetirementPublicationErrorV1::invalid(
          "root_retirement_support_role",
          "root-retirement closure cannot contain non-lifecycle page roles",
        ));
      }
    };
    let page_entity = self.load_entity(key, kind)?;
    let entity = decode_whole_entity(&page_entity.bytes, self.header.hash_algorithm, self.header.write_sequence_high_water)?;
    let GcStateArtifactV1::Page(page) = decode_gc_state_artifact(entity.stored_value, self.header.hash_algorithm)? else {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_kind",
        "root-lifecycle support page key resolves to another GC artifact kind",
      ));
    };
    if page.key != key || page.role != expected_role {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_changed",
        "durable root-lifecycle page identity or role differs from its parent descriptor",
      ));
    }
    builder.observe_encoded(entity.stored_value).map_err(root_retirement_support_error)
  }

  fn load_entity(
    &self,
    key: &[u8],
    expected_kind: GcArtifactKindV1,
  ) -> Result<ChargedRootLifecycleSupportEntityV1, RootRetirementPublicationErrorV1> {
    let Some(locator) = self.kv.get(key)? else {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_missing",
        format!("root-lifecycle support artifact {} is absent", hex::encode(key)),
      ));
    };
    if locator.type_flags != kv_tag::GC_ARTIFACT {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_collision",
        "root-lifecycle support key resolves to another KV role",
      ));
    }
    let maximum_value_length = expected_kind.immutable_maximum_encoded_length().ok_or_else(|| {
      RootRetirementPublicationErrorV1::invalid("root_retirement_support_kind", "root-lifecycle support role has no immutable cap")
    })?;
    let maximum_entity_length =
      super::entity::checked_whole_entity_encoded_length(self.header.hash_algorithm, key.len(), maximum_value_length)?;
    let locator_length = usize::try_from(locator.total_length).map_err(|error| {
      RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_length",
        format!("root-lifecycle support locator length exceeds usize: {error}"),
      )
    })?;
    if locator_length > maximum_entity_length {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_length",
        format!("root-lifecycle support entity length {locator_length} exceeds its {maximum_entity_length}-byte role cap"),
      ));
    }
    let reservation = self
      .memory
      .reserve(
        MemoryOwner::GarbageCollection,
        u64::try_from(locator_length).map_err(|error| {
          RootRetirementPublicationErrorV1::invalid(
            "root_retirement_support_length",
            format!("root-lifecycle support length cannot be accounted: {error}"),
          )
        })?,
        AdmissionClass::Maintenance,
      )
      .map_err(|error| RootRetirementPublicationErrorV1::invalid("root_retirement_support_memory", error.to_string()))?;
    let bytes =
      read_entity_bounded(self.file, self.kv, key, maximum_entity_length, self.header.write_sequence_high_water)?.ok_or_else(|| {
        RootRetirementPublicationErrorV1::invalid(
          "root_retirement_support_missing",
          format!("root-lifecycle support artifact {} disappeared", hex::encode(key)),
        )
      })?;
    let entity = decode_whole_entity(&bytes, self.header.hash_algorithm, self.header.write_sequence_high_water)?;
    if entity.entry_type != EntryTypeV4::GcArtifact
      || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
      || entity.compression_algorithm != CompressionAlgorithm::None
      || entity.key != key
    {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_representation",
        "root-lifecycle support artifact is not one canonical system GC WholeEntity",
      ));
    }
    let envelope = decode_gc_artifact_envelope(entity.stored_value)?;
    if envelope.kind != expected_kind {
      return Err(RootRetirementPublicationErrorV1::invalid(
        "root_retirement_support_kind",
        "root-lifecycle support artifact kind differs from its directory role",
      ));
    }
    Ok(ChargedRootLifecycleSupportEntityV1 { bytes, _memory: reservation })
  }
}

fn root_retirement_support_error(source: super::gc_lifecycle::RootLifecycleSupportClosureErrorV1) -> RootRetirementPublicationErrorV1 {
  RootRetirementPublicationErrorV1::Invalid { code: source.code(), message: source.to_string() }
}

fn root_reclaim_support_error(source: super::gc_lifecycle::RootLifecycleSupportClosureErrorV1) -> RootReclaimPublicationErrorV1 {
  RootReclaimPublicationErrorV1::Invalid { code: source.code(), message: source.to_string() }
}

fn root_reclaim_from_retirement_error(source: RootRetirementPublicationErrorV1) -> RootReclaimPublicationErrorV1 {
  match source {
    RootRetirementPublicationErrorV1::Invalid { code, message } => RootReclaimPublicationErrorV1::Invalid { code, message },
    RootRetirementPublicationErrorV1::Format(source) => RootReclaimPublicationErrorV1::Format(source),
    RootRetirementPublicationErrorV1::Authority(source) => RootReclaimPublicationErrorV1::Authority(source),
    RootRetirementPublicationErrorV1::Pin(source) => RootReclaimPublicationErrorV1::Pin(source),
    RootRetirementPublicationErrorV1::RetirementAdmission(source) => RootReclaimPublicationErrorV1::RetirementAdmission(source),
    RootRetirementPublicationErrorV1::RetirementOwner(source) => RootReclaimPublicationErrorV1::RetirementOwner(source),
    RootRetirementPublicationErrorV1::AuthorityRecheck(source) => RootReclaimPublicationErrorV1::invalid(
      "root_reclaim_support_authority_recheck",
      format!("unexpected retirement authority recheck error while reading reclaim support: {source}"),
    ),
    RootRetirementPublicationErrorV1::Committed { code, message, .. } => RootReclaimPublicationErrorV1::invalid(
      "root_reclaim_support_committed_error",
      format!("unexpected committed retirement error {code} while reading reclaim support: {message}"),
    ),
  }
}

fn physical_quarantine_support_error(source: super::gc_quarantine::QuarantineClosureErrorV1) -> PhysicalQuarantinePublicationErrorV1 {
  PhysicalQuarantinePublicationErrorV1::Invalid { code: source.code(), message: source.to_string() }
}

fn validate_physical_quarantine_publication<'a>(
  request: &'a PhysicalQuarantinePublicationRequestV1<'_>,
) -> Result<ValidatedPhysicalQuarantinePublicationV1<'a>, PhysicalQuarantinePublicationErrorV1> {
  let manifest = super::gc_quarantine::decode_quarantine_manifest_v1(&request.quarantine_manifest.value, request.permit.hash_algorithm())?;
  let control = decode_gc_active_control(&request.quarantine_control.value, request.permit.hash_algorithm())?;
  if manifest.key != request.quarantine_manifest.key || control.key != request.quarantine_control.key {
    return Err(PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_prepared_identity",
      "prepared quarantine manifest or control key differs from its canonical bytes",
    ));
  }
  if manifest.hash_algorithm != request.permit.hash_algorithm()
    || manifest.database_id != request.permit.database_id()
    || manifest.key != request.permit.next_manifest_hash()
    || manifest.mark_generation != request.permit.mark_generation()
    || request.permit.support_closure().manifest_key() != manifest.key
  {
    return Err(PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_permit",
      "quarantine manifest differs from the exact qualified publication permit",
    ));
  }
  if manifest.candidate_count != request.permit.resulting_candidate_count()
    || manifest.candidate_bytes != request.permit.resulting_candidate_bytes()
    || manifest.eligible_count_hint != request.permit.eligible_count()
  {
    return Err(PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_aggregates",
      "quarantine manifest aggregates differ from the completed transition",
    ));
  }
  if control.kind != GcArtifactKindV1::QuarantineActiveControl
    || control.database_id != manifest.database_id
    || control.generation != manifest.mark_generation
    || control.target_manifest_hash != manifest.key
  {
    return Err(PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_control",
      "quarantine selector does not target the exact qualified manifest",
    ));
  }
  if request.publication_timestamp_ms < manifest.completed_at_ms {
    return Err(PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_timestamp",
      "hard-publication timestamp precedes quarantine transition completion",
    ));
  }
  Ok(ValidatedPhysicalQuarantinePublicationV1 { manifest, control })
}

fn validate_root_reclaim_publication<'a>(
  request: &'a RootReclaimPublicationRequestV1<'_>,
) -> Result<ValidatedRootReclaimPublicationV1<'a>, RootReclaimPublicationErrorV1> {
  let proof = decode_root_object_reclaim_proof_v1(&request.root_object_reclaim_proof.value, request.hash_algorithm)?;
  let expiry_manifest = decode_root_expiry_manifest_v1(&request.expiry_manifest.value, request.hash_algorithm)?;
  let lifecycle_manifest = decode_root_lifecycle_manifest_v1(&request.lifecycle_manifest.value, request.hash_algorithm)?;
  let lifecycle_control = decode_gc_active_control(&request.lifecycle_control.value, request.hash_algorithm)?;
  let database_id = request.support_closure.database_id();
  if request.root_object_reclaim_proof.key != proof.key
    || request.expiry_manifest.key != expiry_manifest.key
    || request.lifecycle_manifest.key != lifecycle_manifest.key
    || request.lifecycle_control.key != lifecycle_control.key
  {
    return Err(RootReclaimPublicationErrorV1::invalid(
      "root_reclaim_prepared_identity",
      "prepared reclaim artifact keys differ from their canonical bytes",
    ));
  }
  if request.retention_permit.hash_algorithm() != request.hash_algorithm
    || request.support_closure.hash_algorithm() != request.hash_algorithm
    || request.retention_permit.database_id() != database_id
    || proof.database_id != database_id
    || expiry_manifest.database_id != database_id
    || lifecycle_manifest.database_id != database_id
    || lifecycle_control.database_id != database_id
  {
    return Err(RootReclaimPublicationErrorV1::invalid(
      "root_reclaim_prepared_database",
      "reclaim permit, closure, artifacts, and hash profile disagree on database identity",
    ));
  }
  if request.support_closure.lifecycle_manifest_hash() != request.lifecycle_manifest.key
    || request.support_closure.expiry_manifest_hash() != Some(request.expiry_manifest.key.as_slice())
    || request.support_closure.root_object_reclaim_proof_hash() != Some(request.root_object_reclaim_proof.key.as_slice())
    || request.support_closure.lifecycle_generation() != lifecycle_manifest.generation
    || request.retention_permit.root_object_reclaim_proof_hash() != request.root_object_reclaim_proof.key
    || request.support_closure.root_expiry_result_digest() != Some(request.retention_permit.resulting_expiry_records_digest())
    || request.retention_permit.namespace_root_hash() != proof.namespace_root_hash
  {
    return Err(RootReclaimPublicationErrorV1::invalid(
      "root_reclaim_support_closure",
      "bounded support closure and retention permit do not bind the exact reclaim manifests and proof",
    ));
  }
  validate_root_lifecycle_expiry_manifest(&lifecycle_manifest, &expiry_manifest)?;
  let summary = request.retention_permit.summary();
  if lifecycle_manifest.generation != request.retention_permit.lifecycle_generation()
    || expiry_manifest.generation != lifecycle_manifest.generation
    || lifecycle_manifest.published_at_ms != request.retention_permit.completed_at_ms()
    || expiry_manifest.retention_ms != request.retention_permit.retention_ms()
    || expiry_manifest.optional_byte_budget != request.retention_permit.optional_byte_budget()
    || expiry_manifest.record_count != summary.resulting_count
    || expiry_manifest.logical_bytes != summary.resulting_bytes
    || expiry_manifest.mandatory_count != summary.resulting_mandatory_count
    || expiry_manifest.mandatory_bytes != summary.resulting_mandatory_bytes
    || expiry_manifest.optional_count != summary.resulting_optional_count
    || expiry_manifest.optional_bytes != summary.resulting_optional_bytes
    || expiry_manifest.oldest_retired_at_ms != summary.oldest_retired_at_ms
    || expiry_manifest.newest_retired_at_ms != summary.newest_retired_at_ms
    || lifecycle_manifest.retired_evidence_count != summary.resulting_count
    || lifecycle_manifest.expiry_bytes != summary.resulting_bytes
    || lifecycle_control.kind != GcArtifactKindV1::RootLifecycleActiveControl
    || lifecycle_control.generation != lifecycle_manifest.generation
    || lifecycle_control.target_manifest_hash != request.lifecycle_manifest.key
  {
    return Err(RootReclaimPublicationErrorV1::invalid(
      "root_reclaim_manifest_closure",
      "reclaim permit aggregates, manifests, and lifecycle selector do not close exactly",
    ));
  }
  if proof.reclaimed_at_ms > request.retention_permit.completed_at_ms() {
    return Err(RootReclaimPublicationErrorV1::invalid("root_reclaim_timestamp", "reclaim proof time follows retention completion"));
  }
  let completed_at_ms = u64::try_from(request.retention_permit.completed_at_ms()).map_err(|error| {
    RootReclaimPublicationErrorV1::invalid("root_reclaim_timestamp", format!("retention completion time is negative: {error}"))
  })?;
  let published_at_ms = u64::try_from(lifecycle_manifest.published_at_ms).map_err(|error| {
    RootReclaimPublicationErrorV1::invalid("root_reclaim_timestamp", format!("lifecycle publication time is negative: {error}"))
  })?;
  if request.publication_timestamp_ms < completed_at_ms || request.publication_timestamp_ms < published_at_ms {
    return Err(RootReclaimPublicationErrorV1::invalid(
      "root_reclaim_timestamp",
      "hard-publication timestamp precedes retention completion or the lifecycle manifest",
    ));
  }
  Ok(ValidatedRootReclaimPublicationV1 { lifecycle_control })
}

fn validate_root_retirement_publication<'a>(
  request: &'a RootRetirementPublicationRequestV1<'_>,
) -> Result<ValidatedRootRetirementPublicationV1<'a>, RootRetirementPublicationErrorV1> {
  let retirement = decode_root_retirement_commit_v1(&request.retirement_commit.value, request.hash_algorithm)?;
  let expiry_manifest = decode_root_expiry_manifest_v1(&request.expiry_manifest.value, request.hash_algorithm)?;
  let lifecycle_manifest = decode_root_lifecycle_manifest_v1(&request.lifecycle_manifest.value, request.hash_algorithm)?;
  let lifecycle_control = decode_gc_active_control(&request.lifecycle_control.value, request.hash_algorithm)?;
  let database_id = request.support_closure.database_id();
  if request.retirement_commit.key != retirement.key
    || request.expiry_manifest.key != expiry_manifest.key
    || request.lifecycle_manifest.key != lifecycle_manifest.key
    || request.lifecycle_control.key != lifecycle_control.key
  {
    return Err(RootRetirementPublicationErrorV1::invalid(
      "root_retirement_prepared_identity",
      "prepared retirement artifact keys differ from their canonical bytes",
    ));
  }
  if request.support_closure.hash_algorithm() != request.hash_algorithm
    || retirement.database_id != database_id
    || expiry_manifest.database_id != database_id
    || lifecycle_manifest.database_id != database_id
    || lifecycle_control.database_id != database_id
  {
    return Err(RootRetirementPublicationErrorV1::invalid(
      "root_retirement_prepared_database",
      "retirement artifacts disagree on database identity or hash profile",
    ));
  }
  if request.support_closure.lifecycle_manifest_hash() != request.lifecycle_manifest.key
    || request.support_closure.expiry_manifest_hash() != Some(request.expiry_manifest.key.as_slice())
    || request.support_closure.retirement_commit_hash() != Some(request.retirement_commit.key.as_slice())
    || request.support_closure.lifecycle_generation() != lifecycle_manifest.generation
    || request.support_closure.source_complete_mark_generation() != lifecycle_manifest.source_complete_mark_generation
  {
    return Err(RootRetirementPublicationErrorV1::invalid(
      "root_retirement_support_closure",
      "bounded support closure does not bind the exact retirement manifests and evidence",
    ));
  }
  validate_root_lifecycle_expiry_manifest(&lifecycle_manifest, &expiry_manifest)?;
  if expiry_manifest.generation != lifecycle_manifest.generation
    || lifecycle_manifest.source_complete_mark_generation != request.intent.final_mark_generation
    || lifecycle_manifest.authority_root_set_digest != request.intent.authority_root_set_digest
    || lifecycle_control.kind != GcArtifactKindV1::RootLifecycleActiveControl
    || lifecycle_control.generation != lifecycle_manifest.generation
    || lifecycle_control.target_manifest_hash != request.lifecycle_manifest.key
  {
    return Err(RootRetirementPublicationErrorV1::invalid(
      "root_retirement_manifest_closure",
      "retirement manifests, complete mark, authority digest, and lifecycle selector do not close exactly",
    ));
  }
  if retirement.namespace_root_hash != request.intent.namespace_root_hash
    || retirement.committed_at_ms != request.intent.committed_at_ms
    || retirement.pending_since_ms != request.intent.pending_since_ms
    || retirement.grace_at_pending_ms != request.intent.grace_at_pending_ms
    || retirement.final_mark_generation != request.intent.final_mark_generation
    || retirement.reason != request.intent.reason
    || retirement.prior_lifecycle_manifest_hash != request.intent.prior_lifecycle_manifest_hash
    || retirement.authority_root_set_digest != request.intent.authority_root_set_digest
    || retirement.admission_commit_payload_hash != request.intent.admission_commit_payload_hash
  {
    return Err(RootRetirementPublicationErrorV1::invalid(
      "root_retirement_intent_changed",
      "retirement commit differs from the exact frozen transition intent",
    ));
  }
  let committed_at_ms = u64::try_from(retirement.committed_at_ms).map_err(|error| {
    RootRetirementPublicationErrorV1::invalid("root_retirement_timestamp", format!("retirement commit time is negative: {error}"))
  })?;
  let published_at_ms = u64::try_from(lifecycle_manifest.published_at_ms).map_err(|error| {
    RootRetirementPublicationErrorV1::invalid("root_retirement_timestamp", format!("lifecycle publication time is negative: {error}"))
  })?;
  if request.publication_timestamp_ms < committed_at_ms || request.publication_timestamp_ms < published_at_ms {
    return Err(RootRetirementPublicationErrorV1::invalid(
      "root_retirement_timestamp",
      "hard-publication timestamp precedes the retirement commit or lifecycle manifest",
    ));
  }
  Ok(ValidatedRootRetirementPublicationV1 { lifecycle_control })
}

fn validate_void_claim_admission_request<'a>(
  request: &'a VoidClaimAdmissionRequestV1<'a>,
  header: &DatabaseHeaderV4,
) -> Result<(super::gc_void::VoidClaimV1<'a>, super::gc_void::VoidCatalogManifestV1<'a>, GcActiveControlV1<'a>), VoidClaimAdmissionErrorV1>
{
  let SweepVoidArtifactV1::VoidClaim(claim) = decode_sweep_void_artifact(&request.claim.value, header.hash_algorithm)? else {
    return Err(VoidClaimAdmissionErrorV1::invalid("void_claim_admission_claim_kind", "claim bytes decode as another GC artifact kind"));
  };
  let SweepVoidArtifactV1::VoidCatalog(result_manifest) =
    decode_sweep_void_artifact(&request.result_manifest.value, header.hash_algorithm)?
  else {
    return Err(VoidClaimAdmissionErrorV1::invalid(
      "void_claim_admission_result_kind",
      "result manifest bytes decode as another GC artifact kind",
    ));
  };
  let result_control = decode_gc_active_control(&request.result_control.value, header.hash_algorithm)?;
  let claim_created_at_ms = u64::try_from(claim.created_at_ms).map_err(|source| {
    VoidClaimAdmissionErrorV1::invalid("void_claim_admission_timestamp", format!("claim creation time is negative: {source}"))
  })?;
  let result_published_at_ms = u64::try_from(result_manifest.published_at_ms).map_err(|source| {
    VoidClaimAdmissionErrorV1::invalid("void_claim_admission_timestamp", format!("result catalog publication time is negative: {source}"))
  })?;
  if claim.key != request.claim.key
    || result_manifest.key != request.result_manifest.key
    || claim.database_id != header.database_id
    || result_manifest.database_id != header.database_id
    || result_manifest.previous_control_sequence == 0
    || result_control.kind != GcArtifactKindV1::VoidCatalogActiveControl
    || result_control.key != request.result_control.key
    || result_control.database_id != header.database_id
    || result_control.generation != result_manifest.generation
    || result_control.target_manifest_hash != result_manifest.key
    || result_control.sequence <= result_manifest.previous_control_sequence
    || request.publication_timestamp_ms < claim_created_at_ms
    || request.publication_timestamp_ms < result_published_at_ms
  {
    return Err(VoidClaimAdmissionErrorV1::invalid(
      "void_claim_admission_identity",
      "claim, result manifest, control, sequence, or publication time do not close exactly",
    ));
  }
  Ok((claim, result_manifest, result_control))
}

fn validate_requested_void_claim_settlement(
  consumption: &VoidClaimConsumptionPermitV1,
  request: &VoidClaimSettlementPublicationRequestV1<'_>,
  source_manifest: &super::gc_void::VoidCatalogManifestV1<'_>,
  result_manifest: &super::gc_void::VoidCatalogManifestV1<'_>,
  claim: &super::gc_void::VoidClaimV1<'_>,
) -> Result<(), VoidClaimSettlementPublicationErrorV1> {
  let SweepVoidArtifactV1::VoidClaimSettlement(settlement) =
    decode_sweep_void_artifact(&request.settlement.value, consumption.hash_algorithm())?
  else {
    return Err(VoidClaimSettlementPublicationErrorV1::invalid(
      "void_claim_settlement_receipt_kind",
      "settlement bytes decode as another GC artifact kind",
    ));
  };
  let expected_outcome = match consumption.outcome() {
    VoidClaimConsumptionOutcomeV1::Settled => VoidClaimSettlementOutcomeV1::Settled,
    VoidClaimConsumptionOutcomeV1::AbandonedToQuarantine => VoidClaimSettlementOutcomeV1::AbandonedToQuarantine,
  };
  let settled_at_ms = u64::try_from(settlement.settled_at_ms).map_err(|source| {
    VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_timestamp", format!("settlement time is negative: {source}"))
  })?;
  let result_published_at_ms = u64::try_from(result_manifest.published_at_ms).map_err(|source| {
    VoidClaimSettlementPublicationErrorV1::invalid(
      "void_claim_settlement_timestamp",
      format!("result catalog publication time is negative: {source}"),
    )
  })?;
  if request.settlement.key != settlement.key
    || settlement.database_id != consumption.database_id()
    || settlement.claim_id != consumption.claim_id()
    || settlement.generation != result_manifest.generation
    || settlement.outcome != expected_outcome
    || settlement.recovered
    || settlement.source_manifest_hash != source_manifest.key
    || settlement.result_manifest_hash != result_manifest.key
    || settlement.used_count != u32::try_from(consumption.durable_uses().len())?
    || settlement.unused_count != u32::try_from(consumption.returned_extents().len())?
    || settlement.used_bytes != consumption.used_bytes()
    || settlement.returned_bytes != consumption.returned_bytes()
    || settlement.evidence_digest != consumption.evidence_digest()
    || source_manifest.key != consumption.source_manifest_key()
    || source_manifest.generation != consumption.generation()
    || claim.key != consumption.claim_key()
    || claim.claim_id != consumption.claim_id()
    || claim.generation != consumption.generation()
    || result_manifest.generation
      != source_manifest
        .generation
        .checked_add(1)
        .ok_or_else(|| VoidClaimSettlementPublicationErrorV1::invalid("void_claim_settlement_generation", "result generation overflowed"))?
    || settled_at_ms < result_published_at_ms
    || request.publication_timestamp_ms < settled_at_ms
  {
    return Err(VoidClaimSettlementPublicationErrorV1::invalid(
      "void_claim_settlement_receipt_identity",
      "settlement receipt, claim, catalogs, outcome, evidence, aggregates, generation, or time do not close exactly",
    ));
  }
  Ok(())
}

fn validate_void_catalog_publication_request(request: &VoidCatalogPublicationRequestV1<'_>) -> Result<(), VoidCatalogPublicationErrorV1> {
  let algorithm = request.completion.hash_algorithm();
  let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(&request.manifest.value, algorithm)? else {
    return Err(VoidCatalogPublicationErrorV1::invalid(
      "void_publication_manifest_kind",
      "proposed Void manifest bytes decode as another GC artifact kind",
    ));
  };
  let control = decode_gc_active_control(&request.control.value, algorithm)?;
  let published_at_ms = u64::try_from(manifest.published_at_ms).map_err(|source| {
    VoidCatalogPublicationErrorV1::invalid("void_publication_timestamp", format!("Void manifest publication time is negative: {source}"))
  })?;
  if manifest.key != request.manifest.key
    || manifest.database_id != request.completion.database_id()
    || control.kind != GcArtifactKindV1::VoidCatalogActiveControl
    || control.key != request.control.key
    || control.database_id != manifest.database_id
    || control.generation != manifest.generation
    || control.target_manifest_hash != manifest.key
    || control.sequence <= manifest.previous_control_sequence
    || request.publication_timestamp_ms < published_at_ms
    || !request.completion.outcomes().iter().any(|outcome| outcome.outcome == super::gc_void::SweepOutcomeClassV1::Reclaimed)
  {
    return Err(VoidCatalogPublicationErrorV1::invalid(
      "void_publication_identity",
      "Void manifest, control, sweep completion, sequence, or publication time do not close exactly",
    ));
  }
  Ok(())
}

fn select_physical_quarantine_control(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
) -> Result<SelectedPhysicalQuarantineControlV1, PhysicalQuarantinePublicationErrorV1> {
  let keys = gc_control_keys(header.hash_algorithm, GcArtifactKindV1::QuarantineActiveControl, &header.database_id)?;
  let controls = [
    load_gc_control_entity(file, kv, GcArtifactKindV1::QuarantineActiveControl, &keys[0], header)?,
    load_gc_control_entity(file, kv, GcArtifactKindV1::QuarantineActiveControl, &keys[1], header)?,
  ];
  let closure_valid = [
    match &controls[0] {
      Some(control) => physical_quarantine_manifest_is_present(file, kv, header, &control.target_manifest_hash)?,
      None => false,
    },
    match &controls[1] {
      Some(control) => physical_quarantine_manifest_is_present(file, kv, header, &control.target_manifest_hash)?,
      None => false,
    },
  ];
  let selected_slot = match (&controls[0], &controls[1]) {
    (Some(a), Some(b)) => {
      let a_control = decode_gc_active_control(&a.stored_value, header.hash_algorithm)?;
      let b_control = decode_gc_active_control(&b.stored_value, header.hash_algorithm)?;
      select_gc_active_control(&a_control, closure_valid[0], &b_control, closure_valid[1])?.map(|control| control.slot)
    }
    (Some(_), None) if closure_valid[0] => Some(0),
    (None, Some(_)) if closure_valid[1] => Some(1),
    (Some(_), None) | (None, Some(_)) | (None, None) => None,
  }
  .ok_or_else(|| {
    PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_authority_unavailable",
      "no complete physical-quarantine A/B control is selectable",
    )
  })?;
  let stored = controls[usize::from(selected_slot)].as_ref().ok_or_else(|| {
    PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_authority_internal",
      "selected physical-quarantine control slot is absent",
    )
  })?;
  Ok(SelectedPhysicalQuarantineControlV1 {
    stored_value: stored.stored_value.clone(),
    target_manifest_hash: stored.target_manifest_hash.clone(),
  })
}

fn select_void_catalog_control(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
) -> Result<Option<SelectedVoidCatalogControlV1>, VoidCatalogPublicationErrorV1> {
  let keys = gc_control_keys(header.hash_algorithm, GcArtifactKindV1::VoidCatalogActiveControl, &header.database_id)?;
  let controls = [
    load_gc_control_entity(file, kv, GcArtifactKindV1::VoidCatalogActiveControl, &keys[0], header)?,
    load_gc_control_entity(file, kv, GcArtifactKindV1::VoidCatalogActiveControl, &keys[1], header)?,
  ];
  let closure_valid = [
    match &controls[0] {
      Some(control) => void_catalog_manifest_is_present(file, kv, header, &control.target_manifest_hash)?,
      None => false,
    },
    match &controls[1] {
      Some(control) => void_catalog_manifest_is_present(file, kv, header, &control.target_manifest_hash)?,
      None => false,
    },
  ];
  let selected_slot = match (&controls[0], &controls[1]) {
    (Some(a), Some(b)) => {
      let a_control = decode_gc_active_control(&a.stored_value, header.hash_algorithm)?;
      let b_control = decode_gc_active_control(&b.stored_value, header.hash_algorithm)?;
      select_gc_active_control(&a_control, closure_valid[0], &b_control, closure_valid[1])?.map(|control| control.slot)
    }
    (Some(_), None) if closure_valid[0] => Some(0),
    (None, Some(_)) if closure_valid[1] => Some(1),
    (Some(_), None) | (None, Some(_)) | (None, None) => None,
  };
  let Some(selected_slot) = selected_slot else {
    if controls.iter().all(Option::is_none) {
      return Ok(None);
    }
    return Err(VoidCatalogPublicationErrorV1::invalid(
      "void_publication_authority_unavailable",
      "no complete Void-catalog A/B control is selectable",
    ));
  };
  let stored = controls[usize::from(selected_slot)]
    .as_ref()
    .ok_or_else(|| VoidCatalogPublicationErrorV1::invalid("void_publication_authority_internal", "selected Void control slot is absent"))?;
  Ok(Some(SelectedVoidCatalogControlV1 {
    stored_value: stored.stored_value.clone(),
    control_key: keys[usize::from(selected_slot)].clone(),
    target_manifest_hash: stored.target_manifest_hash.clone(),
    control_sequence: stored.control_sequence,
    write_sequence: stored.write_sequence,
    slot: selected_slot,
  }))
}

fn void_catalog_manifest_is_present(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  key: &[u8],
) -> Result<bool, VoidCatalogPublicationErrorV1> {
  let Some(locator) = kv.get(key).map_err(FirstAuthorityPublicationErrorV1::from)? else {
    return Ok(false);
  };
  if locator.type_flags != kv_tag::GC_ARTIFACT {
    return Err(VoidCatalogPublicationErrorV1::invalid(
      "void_publication_manifest_collision",
      "Void catalog manifest key resolves to another KV role",
    ));
  }
  let maximum_value_length = GcArtifactKindV1::VoidCatalogManifest.immutable_maximum_encoded_length().ok_or_else(|| {
    VoidCatalogPublicationErrorV1::invalid("void_publication_manifest_kind", "Void catalog manifest has no immutable cap")
  })?;
  let maximum_entity_length = super::entity::checked_whole_entity_encoded_length(header.hash_algorithm, key.len(), maximum_value_length)?;
  let bytes = read_entity_bounded(file, kv, key, maximum_entity_length, header.write_sequence_high_water)?.ok_or_else(|| {
    VoidCatalogPublicationErrorV1::invalid("void_publication_manifest_missing", "Void catalog manifest locator disappeared")
  })?;
  let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  if entity.entry_type != EntryTypeV4::GcArtifact
    || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
    || entity.compression_algorithm != CompressionAlgorithm::None
    || entity.key != key
  {
    return Err(VoidCatalogPublicationErrorV1::invalid(
      "void_publication_manifest_representation",
      "selected Void catalog manifest is not one canonical system GC WholeEntity",
    ));
  }
  let SweepVoidArtifactV1::VoidCatalog(manifest) = decode_sweep_void_artifact(entity.stored_value, header.hash_algorithm)? else {
    return Err(VoidCatalogPublicationErrorV1::invalid(
      "void_publication_manifest_kind",
      "selected Void catalog key resolves to another GC artifact kind",
    ));
  };
  if manifest.database_id != header.database_id || manifest.key != key {
    return Err(VoidCatalogPublicationErrorV1::invalid(
      "void_publication_manifest_representation",
      "selected Void catalog manifest identity differs from its control target",
    ));
  }
  Ok(true)
}

fn physical_quarantine_manifest_is_present(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  key: &[u8],
) -> Result<bool, PhysicalQuarantinePublicationErrorV1> {
  let Some(locator) = kv.get(key).map_err(FirstAuthorityPublicationErrorV1::from)? else {
    return Ok(false);
  };
  if locator.type_flags != kv_tag::GC_ARTIFACT {
    return Err(PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_manifest_collision",
      "physical-quarantine manifest key resolves to another KV role",
    ));
  }
  let maximum_value_length = GcArtifactKindV1::QuarantineManifest.immutable_maximum_encoded_length().ok_or_else(|| {
    PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_manifest_kind",
      "physical-quarantine manifest has no immutable cap",
    )
  })?;
  let maximum_entity_length = super::entity::checked_whole_entity_encoded_length(header.hash_algorithm, key.len(), maximum_value_length)?;
  let bytes = read_entity_bounded(file, kv, key, maximum_entity_length, header.write_sequence_high_water)?.ok_or_else(|| {
    PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_manifest_missing",
      "physical-quarantine manifest locator disappeared",
    )
  })?;
  let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  if entity.entry_type != EntryTypeV4::GcArtifact
    || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
    || entity.compression_algorithm != CompressionAlgorithm::None
    || entity.key != key
  {
    return Err(PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_manifest_representation",
      "selected physical-quarantine manifest is not one canonical system GC WholeEntity",
    ));
  }
  let manifest = super::gc_quarantine::decode_quarantine_manifest_v1(entity.stored_value, header.hash_algorithm)?;
  if manifest.database_id != header.database_id || manifest.key != key {
    return Err(PhysicalQuarantinePublicationErrorV1::invalid(
      "quarantine_publication_manifest_representation",
      "selected physical-quarantine manifest identity differs from its control target",
    ));
  }
  Ok(true)
}

fn select_root_lifecycle_control(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
) -> Result<SelectedRootLifecycleControlV1, RootRetirementPublicationErrorV1> {
  let keys = gc_control_keys(header.hash_algorithm, GcArtifactKindV1::RootLifecycleActiveControl, &header.database_id)?;
  let controls = [
    load_gc_control_entity(file, kv, GcArtifactKindV1::RootLifecycleActiveControl, &keys[0], header)?,
    load_gc_control_entity(file, kv, GcArtifactKindV1::RootLifecycleActiveControl, &keys[1], header)?,
  ];
  let closure_valid = [
    match &controls[0] {
      Some(control) => root_lifecycle_manifest_is_present(file, kv, header, &control.target_manifest_hash)?,
      None => false,
    },
    match &controls[1] {
      Some(control) => root_lifecycle_manifest_is_present(file, kv, header, &control.target_manifest_hash)?,
      None => false,
    },
  ];
  let selected_slot = match (&controls[0], &controls[1]) {
    (Some(a), Some(b)) => {
      let a_control = decode_gc_active_control(&a.stored_value, header.hash_algorithm)?;
      let b_control = decode_gc_active_control(&b.stored_value, header.hash_algorithm)?;
      select_gc_active_control(&a_control, closure_valid[0], &b_control, closure_valid[1])?.map(|control| control.slot)
    }
    (Some(_), None) if closure_valid[0] => Some(0),
    (None, Some(_)) if closure_valid[1] => Some(1),
    (Some(_), None) | (None, Some(_)) | (None, None) => None,
  }
  .ok_or_else(|| {
    RootRetirementPublicationErrorV1::invalid(
      "root_retirement_lifecycle_unavailable",
      "no complete root-lifecycle A/B control is selectable",
    )
  })?;
  let stored = controls[usize::from(selected_slot)].as_ref().ok_or_else(|| {
    RootRetirementPublicationErrorV1::invalid("root_retirement_lifecycle_internal", "selected lifecycle control slot is absent")
  })?;
  Ok(SelectedRootLifecycleControlV1 {
    stored_value: stored.stored_value.clone(),
    target_manifest_hash: stored.target_manifest_hash.clone(),
  })
}

fn root_lifecycle_manifest_is_present(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  key: &[u8],
) -> Result<bool, RootRetirementPublicationErrorV1> {
  let Some(locator) = kv.get(key)? else {
    return Ok(false);
  };
  if locator.type_flags != kv_tag::GC_ARTIFACT {
    return Err(RootRetirementPublicationErrorV1::invalid(
      "root_retirement_lifecycle_collision",
      "root-lifecycle manifest key resolves to another KV role",
    ));
  }
  let maximum_value_length = GcArtifactKindV1::RootLifecycleManifest.immutable_maximum_encoded_length().ok_or_else(|| {
    RootRetirementPublicationErrorV1::invalid("root_retirement_lifecycle_kind", "lifecycle manifest has no immutable cap")
  })?;
  let maximum_entity_length = super::entity::checked_whole_entity_encoded_length(header.hash_algorithm, key.len(), maximum_value_length)?;
  let bytes = read_entity_bounded(file, kv, key, maximum_entity_length, header.write_sequence_high_water)?.ok_or_else(|| {
    RootRetirementPublicationErrorV1::invalid("root_retirement_lifecycle_missing", "lifecycle manifest locator disappeared")
  })?;
  let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  if entity.entry_type != EntryTypeV4::GcArtifact
    || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
    || entity.compression_algorithm != CompressionAlgorithm::None
    || entity.key != key
  {
    return Err(RootRetirementPublicationErrorV1::invalid(
      "root_retirement_lifecycle_representation",
      "selected lifecycle manifest is not one canonical system GC WholeEntity",
    ));
  }
  let manifest = decode_root_lifecycle_manifest_v1(entity.stored_value, header.hash_algorithm)?;
  if manifest.database_id != header.database_id || manifest.key != key {
    return Err(RootRetirementPublicationErrorV1::invalid(
      "root_retirement_lifecycle_representation",
      "selected lifecycle manifest identity differs from its control target",
    ));
  }
  Ok(true)
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
fn commit_stable_entity_dependency(
  file: &File,
  kv: &mut DiskKVStore,
  admitted: super::header_publication::AdmittedDatabaseHeaderPublicationV4<'_>,
  batch: crate::engine::disk_kv_store::AtomicKvVisibilityBatch,
  authority_sequence: u64,
  entities: &[PreparedWholeEntityV1],
  append_start: u64,
  expected_hot_tail_offset: u64,
  expected_existing: &[Option<KVEntry>],
  observer: &mut dyn FirstAuthorityDependencyObserverV1,
) -> Result<StableEntityDependencyOutcomeV1, FirstAuthorityPublicationErrorV1> {
  let expected_observation = admitted.expected_observation();
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
      let abort_result = kv.abort_atomic_visibility_batch(batch);
      let observed = match observe_database_header_v4(file) {
        Ok(observed) => observed,
        Err(observed_error) => {
          return Err(FirstAuthorityPublicationErrorV1::invalid(
            "stable_entity_failure_observation",
            format!("stable-entity publication failed as {error}; selected-header reconciliation also failed: {observed_error}"),
          ));
        }
      };
      if append_completed && observed == expected_observation {
        let abort_message = match &abort_result {
          Ok(()) => String::new(),
          Err(abort_error) => format!("; volatile KV rollback also failed: {abort_error}"),
        };
        return Ok(StableEntityDependencyOutcomeV1::CommittedFailure {
          publication: StableEntityDependencyPublicationV1 { observation: observed },
          failure: StableEntityDependencyFailureV1::AuthorityUncertain,
          message: format!("the exact stable-entity header is selected after an uncertain durability failure: {error}{abort_message}"),
        });
      }
      if let Err(abort_error) = abort_result {
        return Err(FirstAuthorityPublicationErrorV1::invalid(
          "stable_entity_failure_rollback",
          format!("stable-entity publication failed as {error}; volatile KV rollback also failed: {abort_error}"),
        ));
      }
      return Err(error.into());
    }
  };
  if !append_completed {
    let rollback_failure_suffix = match kv.abort_atomic_visibility_batch(batch) {
      Ok(()) => String::new(),
      Err(error) => format!("; visibility rollback also failed: {error}"),
    };
    return Ok(StableEntityDependencyOutcomeV1::CommittedFailure {
      publication: StableEntityDependencyPublicationV1 { observation: publication.observation },
      failure: StableEntityDependencyFailureV1::DependencyMissing,
      message: format!("header publication completed without the exact stable-entity dependency{rollback_failure_suffix}"),
    });
  }
  kv.complete_hot_tail_dependency();
  if let Err(error) = kv.publish_atomic_visibility_after_authority(batch, &publication.durability) {
    return Ok(StableEntityDependencyOutcomeV1::CommittedFailure {
      publication: StableEntityDependencyPublicationV1 { observation: publication.observation },
      failure: StableEntityDependencyFailureV1::VisibilityFailure,
      message: error.to_string(),
    });
  }
  if let Err(error) = observer.authority_committed(kv, entities) {
    return Ok(StableEntityDependencyOutcomeV1::CommittedFailure {
      publication: StableEntityDependencyPublicationV1 { observation: publication.observation },
      failure: StableEntityDependencyFailureV1::PostconditionFailure,
      message: error.to_string(),
    });
  }
  Ok(StableEntityDependencyOutcomeV1::Complete(StableEntityDependencyPublicationV1 { observation: publication.observation }))
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

struct LoadedIndexArtifactEntityV1 {
  value: Vec<u8>,
  write_sequence: u64,
}

struct ValidatedIndexArtifactLocatorV1 {
  locator: KVEntry,
  total_length: usize,
  value_length: usize,
}

fn validate_index_artifact_key(hash_algorithm: HashAlgorithm, key: &[u8]) -> Result<(), FirstAuthorityPublicationErrorV1> {
  if key.len() != hash_algorithm.hash_length() {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "immutable_index_key",
      format!("immutable IndexArtifact key length {} does not match the database hash width {}", key.len(), hash_algorithm.hash_length()),
    ));
  }
  Ok(())
}

fn validated_index_artifact_locator(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  key: &[u8],
) -> Result<Option<ValidatedIndexArtifactLocatorV1>, FirstAuthorityPublicationErrorV1> {
  validate_index_artifact_key(header.hash_algorithm, key)?;
  let Some(locator) = kv.get(key)? else {
    return Ok(None);
  };
  if locator.type_flags != kv_tag::INDEX_ARTIFACT {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "immutable_index_identity_collision",
      "immutable IndexArtifact key resolves to another KV role",
    ));
  }
  let total_length = usize::try_from(locator.total_length).map_err(|error| {
    FirstAuthorityPublicationErrorV1::invalid(
      "immutable_index_locator_length",
      format!("IndexArtifact locator length exceeds usize: {error}"),
    )
  })?;
  let base_length = super::entity::checked_whole_entity_encoded_length(header.hash_algorithm, key.len(), 0)?;
  let maximum_value_length = ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length();
  let maximum_entity_length = super::entity::checked_whole_entity_encoded_length(header.hash_algorithm, key.len(), maximum_value_length)?;
  if total_length <= base_length || total_length > maximum_entity_length {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "immutable_index_locator_length",
      format!("IndexArtifact locator length {total_length} is outside the frozen entity bounds"),
    ));
  }
  let entity_end = locator
    .offset
    .checked_add(u64::from(locator.total_length))
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("immutable_index_locator_range", "IndexArtifact locator range overflowed"))?;
  let file_length = file.metadata().map_err(EngineError::IoError)?.len();
  if entity_end > header.hot_tail_offset || entity_end > file_length {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "immutable_index_locator_range",
      format!(
        "IndexArtifact locator end {entity_end} exceeds the durable hot tail {} or file length {file_length}",
        header.hot_tail_offset
      ),
    ));
  }
  Ok(Some(ValidatedIndexArtifactLocatorV1 { locator, total_length, value_length: total_length - base_length }))
}

fn load_index_artifact_entity(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  key: &[u8],
) -> Result<Option<LoadedIndexArtifactEntityV1>, FirstAuthorityPublicationErrorV1> {
  let Some(located) = validated_index_artifact_locator(file, kv, header, key)? else {
    return Ok(None);
  };
  let maximum_value_length = ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length();
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(located.total_length).map_err(|error| {
    FirstAuthorityPublicationErrorV1::invalid(
      "immutable_index_read_allocation",
      format!("IndexArtifact read allocation failed for {} bytes: {error}", located.total_length),
    )
  })?;
  bytes.resize(located.total_length, 0);
  read_file_at_native(file, located.locator.offset, &mut bytes)
    .map_err(|error| FirstAuthorityPublicationErrorV1::invalid("first_authority_readback_io", error.to_string()))?;
  let write_sequence = {
    let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water)?;
    if entity.entry_type != EntryTypeV4::IndexArtifact
      || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
      || entity.compression_algorithm != CompressionAlgorithm::None
      || entity.key != key
      || entity.stored_value.len() != located.value_length
    {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_representation",
        "immutable IndexArtifact WholeEntity representation is noncanonical",
      ));
    }
    let artifact = decode_immutable_index_artifact(entity.stored_value, header.hash_algorithm, maximum_value_length)?;
    if artifact.key != key {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "immutable_index_prepared_mismatch",
        "immutable IndexArtifact envelope key disagrees with its WholeEntity identity",
      ));
    }
    entity.write_sequence
  };
  let value_start = bytes.len() - located.value_length;
  bytes.copy_within(value_start.., 0);
  bytes.truncate(located.value_length);
  Ok(Some(LoadedIndexArtifactEntityV1 { value: bytes, write_sequence }))
}

fn load_system_file(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  kind: SystemControlKindV1,
  identity: &[u8],
) -> Result<Vec<u8>, FirstAuthorityPublicationErrorV1> {
  let path = system_control_path(kind, identity, SystemControlSlotV1::Immutable)?;
  load_system_file_slot(file, kv, header, kind, identity, SystemControlSlotV1::Immutable)?
    .map(|loaded| loaded.body)
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_control_missing", format!("missing {path}")))
}

fn load_system_file_slot(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  kind: SystemControlKindV1,
  identity: &[u8],
  slot: SystemControlSlotV1,
) -> Result<Option<LoadedSystemFileV1>, FirstAuthorityPublicationErrorV1> {
  let path = system_control_path(kind, identity, slot)?;
  let path_key = first_authority_file_path_hash(&path, header.hash_algorithm);
  let Some(locator) = kv.get(&path_key)? else {
    return Ok(None);
  };
  if locator.type_flags != KV_TYPE_FILE_RECORD {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_identity_collision",
      format!("{path} path identity resolves to another KV role"),
    ));
  }
  let record_bytes = read_entity_bounded(file, kv, &path_key, FIRST_AUTHORITY_CONTROL_ENTITY_CAP, header.write_sequence_high_water)?
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("first_authority_control_missing", format!("missing {path}")))?;
  let entity = decode_whole_entity(&record_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  if entity.entry_type != EntryTypeV4::FileRecord
    || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
    || entity.compression_algorithm != CompressionAlgorithm::None
    || entity.key != path_key
  {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_representation",
      format!("{path} is not a canonical system FileRecord"),
    ));
  }
  let write_sequence = entity.write_sequence;
  let integrity_hash = entity.integrity_hash.to_vec();
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
  let body_length = usize::try_from(record.total_size).map_err(|error| {
    FirstAuthorityPublicationErrorV1::invalid("first_authority_control_size", format!("{path} body length exceeds usize: {error}"))
  })?;
  let chunk_key = &record.chunk_hashes[0];
  let Some(chunk_locator) = kv.get(chunk_key)? else {
    return Err(FirstAuthorityPublicationErrorV1::invalid("first_authority_control_chunk_missing", format!("missing chunk for {path}")));
  };
  if chunk_locator.type_flags != KV_TYPE_CHUNK {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_chunk_identity_collision",
      format!("{path} chunk identity resolves to another KV role"),
    ));
  }
  let chunk_bytes = read_entity_bounded(file, kv, chunk_key, FIRST_AUTHORITY_CONTROL_ENTITY_CAP, header.write_sequence_high_water)?
    .ok_or_else(|| {
      FirstAuthorityPublicationErrorV1::invalid("first_authority_control_chunk_missing", format!("missing chunk for {path}"))
    })?;
  let chunk = decode_whole_entity(&chunk_bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  if chunk.entry_type != EntryTypeV4::Chunk
    || chunk.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
    || chunk.compression_algorithm != CompressionAlgorithm::None
    || chunk.key != chunk_key.as_slice()
  {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_chunk_representation",
      format!("{path} references a noncanonical system chunk"),
    ));
  }
  let mut body = Vec::new();
  body.try_reserve_exact(body_length).map_err(|error| {
    FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_allocation",
      format!("{path} body allocation failed for {body_length} bytes: {error}"),
    )
  })?;
  body.extend_from_slice(chunk.stored_value);
  if body.len() as u64 != record.total_size || first_authority_content_hash(&body, header.hash_algorithm) != record.content_hash {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_content",
      format!("{path} content does not match its FileRecord"),
    ));
  }
  if first_authority_system_chunk_hash(&body, header.hash_algorithm) != *chunk_key {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "first_authority_control_chunk_identity",
      format!("{path} chunk key does not match its canonical system payload identity"),
    ));
  }
  Ok(Some(LoadedSystemFileV1 { locator, entity_bytes: record_bytes, record, body, write_sequence, integrity_hash }))
}

fn validate_index_operation_identity(
  header: &DatabaseHeaderV4,
  database_id: &[u8; 16],
  index_id: &[u8],
  operation_id: &[u8; 16],
) -> Result<(), FirstAuthorityPublicationErrorV1> {
  if database_id != &header.database_id {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "index_operation_database_mismatch",
      "index-operation control belongs to another logical database",
    ));
  }
  if index_id.len() != header.hash_algorithm.hash_length()
    || index_id.iter().all(|byte| *byte == 0)
    || operation_id.iter().all(|byte| *byte == 0)
  {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "index_operation_identity",
      "index and operation identities must have canonical nonzero widths",
    ));
  }
  Ok(())
}

fn index_operation_control_identity(
  algorithm: HashAlgorithm,
  index_id: &[u8],
  operation_id: &[u8; 16],
) -> Result<Vec<u8>, FirstAuthorityPublicationErrorV1> {
  if index_id.len() != algorithm.hash_length() {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "index_operation_identity",
      "index identity width does not match the database hash profile",
    ));
  }
  let length = index_id
    .len()
    .checked_add(operation_id.len())
    .ok_or_else(|| FirstAuthorityPublicationErrorV1::invalid("index_operation_identity", "index-operation identity length overflowed"))?;
  let mut identity = Vec::new();
  identity.try_reserve_exact(length).map_err(|error| {
    FirstAuthorityPublicationErrorV1::invalid("index_operation_identity_allocation", format!("identity allocation failed: {error}"))
  })?;
  identity.extend_from_slice(index_id);
  identity.extend_from_slice(operation_id);
  Ok(identity)
}

fn load_index_operation_control_pair(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  index_id: &[u8],
  operation_id: &[u8; 16],
) -> Result<LoadedIndexOperationControlPairV1, FirstAuthorityPublicationErrorV1> {
  let identity = index_operation_control_identity(header.hash_algorithm, index_id, operation_id)?;
  let a = load_system_file_slot(file, kv, header, SystemControlKindV1::IndexOperation, &identity, SystemControlSlotV1::A)?;
  let b = load_system_file_slot(file, kv, header, SystemControlKindV1::IndexOperation, &identity, SystemControlSlotV1::B)?;
  let selected = discover_mutable_control(
    header.hash_algorithm,
    SystemControlKindV1::IndexOperation,
    &identity,
    a.as_ref().map(|loaded| loaded.body.clone()),
    b.as_ref().map(|loaded| loaded.body.clone()),
  )?
  .map(|selected| {
    if selected.database_id != header.database_id {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "index_operation_database_mismatch",
        "selected index-operation control belongs to another logical database",
      ));
    }
    let control = decode_index_operation_control(&selected.bytes, header.hash_algorithm)?;
    if control.index_id != index_id || control.operation_id != *operation_id {
      return Err(FirstAuthorityPublicationErrorV1::invalid(
        "index_operation_identity",
        "selected typed control identity differs from its system-control path",
      ));
    }
    let checkpoint_artifact = control.checkpoint_artifact.ok_or_else(|| {
      FirstAuthorityPublicationErrorV1::invalid(
        "index_operation_checkpoint_missing",
        "selected recovery control does not identify a checkpoint artifact",
      )
    })?;
    Ok(LoadedIndexOperationControlV1 {
      selected_slot: selected.selected_slot,
      control_sequence: selected.sequence,
      checkpoint_artifact: checkpoint_artifact.to_vec(),
      redundancy_degraded: selected.redundancy_degraded,
      bytes: selected.bytes,
    })
  })
  .transpose()?;
  Ok(LoadedIndexOperationControlPairV1 { slots: [a, b], selected })
}

fn validate_index_operation_control_request(
  control: &IndexOperationControlV1<'_>,
  request: &IndexOperationControlPublicationRequestV1<'_>,
) -> Result<(), IndexOperationControlPublicationErrorV1> {
  if control.database_id != *request.database_id || control.index_id != request.index_id || control.operation_id != *request.operation_id {
    return Err(IndexOperationControlPublicationErrorV1::invalid(
      "index_operation_prepared_mismatch",
      "encoded index-operation control does not match the requested database, index, and operation",
    ));
  }
  Ok(())
}

fn validate_index_operation_expectation(
  current: Option<&LoadedIndexOperationControlV1>,
  expected: Option<IndexOperationControlExpectationV1<'_>>,
  algorithm: HashAlgorithm,
) -> Result<(), IndexOperationControlPublicationErrorV1> {
  if let Some(expected) = expected {
    if expected.control_sequence == 0
      || expected.checkpoint_artifact.len() != algorithm.hash_length()
      || expected.checkpoint_artifact.iter().all(|byte| *byte == 0)
    {
      return Err(IndexOperationControlPublicationErrorV1::invalid(
        "index_operation_expectation",
        "expected control sequence and checkpoint identity are noncanonical",
      ));
    }
  }
  match (current, expected) {
    (None, None) => Ok(()),
    (Some(current), Some(expected))
      if current.control_sequence == expected.control_sequence && current.checkpoint_artifact == expected.checkpoint_artifact =>
    {
      Ok(())
    }
    _ => Err(IndexOperationControlPublicationErrorV1::invalid(
      "index_operation_selector_conflict",
      "selected index-operation checkpoint differs from the caller's compare-and-swap expectation",
    )),
  }
}

fn load_system_chunk(
  file: &File,
  kv: &DiskKVStore,
  header: &DatabaseHeaderV4,
  key: &[u8],
  expected_body: &[u8],
) -> Result<Option<KVEntry>, FirstAuthorityPublicationErrorV1> {
  let Some(locator) = kv.get(key)? else {
    return Ok(None);
  };
  if locator.type_flags != KV_TYPE_CHUNK {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "index_operation_chunk_identity_collision",
      "system chunk identity resolves to another KV role",
    ));
  }
  let maximum_length = super::entity::checked_whole_entity_encoded_length(header.hash_algorithm, key.len(), expected_body.len())?;
  let bytes = read_entity_bounded(file, kv, key, maximum_length, header.write_sequence_high_water)?.ok_or_else(|| {
    FirstAuthorityPublicationErrorV1::invalid("index_operation_chunk_missing", "system chunk locator disappeared during validation")
  })?;
  let entity = decode_whole_entity(&bytes, header.hash_algorithm, header.write_sequence_high_water)?;
  if entity.entry_type != EntryTypeV4::Chunk
    || entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM
    || entity.compression_algorithm != CompressionAlgorithm::None
    || entity.key != key
    || entity.stored_value != expected_body
    || first_authority_system_chunk_hash(entity.stored_value, header.hash_algorithm) != key
  {
    return Err(FirstAuthorityPublicationErrorV1::invalid(
      "index_operation_chunk_identity_collision",
      "existing system chunk differs from its canonical immutable representation",
    ));
  }
  Ok(Some(locator))
}

fn index_operation_from_gc_control_error(source: GcControlPublicationErrorV1) -> IndexOperationControlPublicationErrorV1 {
  IndexOperationControlPublicationErrorV1::invalid("index_operation_physical_incarnation", source.to_string())
}

const fn index_operation_committed_failure_code(failure: StableEntityDependencyFailureV1) -> &'static str {
  match failure {
    StableEntityDependencyFailureV1::DependencyMissing => "index_operation_committed_dependency_missing",
    StableEntityDependencyFailureV1::AuthorityUncertain => "index_operation_committed_authority_uncertain",
    StableEntityDependencyFailureV1::VisibilityFailure => "index_operation_committed_visibility_failure",
    StableEntityDependencyFailureV1::PostconditionFailure => "index_operation_committed_postcondition_failure",
  }
}

const fn stable_dependency_to_gc_failure(failure: StableEntityDependencyFailureV1) -> GcControlPublicationFailureV1 {
  match failure {
    StableEntityDependencyFailureV1::DependencyMissing => GcControlPublicationFailureV1::CommittedDependencyMissing,
    StableEntityDependencyFailureV1::AuthorityUncertain => GcControlPublicationFailureV1::CommittedAuthorityUncertain,
    StableEntityDependencyFailureV1::VisibilityFailure => GcControlPublicationFailureV1::CommittedVisibilityFailure,
    StableEntityDependencyFailureV1::PostconditionFailure => GcControlPublicationFailureV1::CommittedPostconditionFailure,
  }
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
