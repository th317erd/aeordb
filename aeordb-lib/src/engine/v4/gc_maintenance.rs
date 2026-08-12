use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::entity::EntryTypeV4;
use super::gc::{
  GcArtifactKindV1, decode_gc_active_control, decode_gc_artifact_envelope, gc_active_control_key, immutable_gc_artifact_key,
  select_gc_active_control, u16_at,
};
use super::gc_audit::decode_audit_artifact;
use super::gc_lifecycle::{
  decode_root_expiry_manifest_v1, decode_root_lifecycle_manifest_v1, decode_root_object_reclaim_proof_v1, decode_root_retirement_commit_v1,
};
use super::gc_mark::{decode_gc_mark_artifact, decode_mark_workspace_manifest, decode_mark_workspace_object};
use super::gc_quarantine::{decode_candidate_delta_v1, decode_quarantine_manifest_v1};
use super::gc_state::{decode_gc_state_artifact, decode_physical_inventory_manifest_v1};
use super::gc_void::decode_sweep_void_artifact;
use super::gc_run::{
  GcRunBasisV1, GcRunErrorV1, GcRunOperationV1, GcRunPhaseOutcomeV1, GcRunPhaseReporterV1, GcRunPhaseV1, GcRunProgressUpdateV1,
};
use super::hash::digest_parts;
use super::reader::FormatError;
use super::system_family::{
  MigrationPolicyV1, SystemFamilyPolicyDecisionV1, SystemFamilyPolicyResolverV1, SystemFamilySubjectV1, SystemFamilyTransferOperationV1,
  TransferPolicyV1,
};
use crate::engine::HashAlgorithm;

const CONTROL_FAMILIES: [GcArtifactKindV1; 6] = [
  GcArtifactKindV1::QuarantineActiveControl,
  GcArtifactKindV1::MarkRunActiveControl,
  GcArtifactKindV1::PhysicalInventoryActiveControl,
  GcArtifactKindV1::AuditCatalogActiveControl,
  GcArtifactKindV1::VoidCatalogActiveControl,
  GcArtifactKindV1::RootLifecycleActiveControl,
];
const MAXIMUM_ARTIFACT_LIMIT: u64 = 1_000_000_000;
const MAXIMUM_WORKSPACE_LIMIT: u64 = 1_000_000;
const MAXIMUM_FINDING_LIMIT: usize = 4_096;
const MAXIMUM_REPAIR_TICKET_LIMIT: usize = 64;
const MAXIMUM_EVIDENCE_KEYS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcArtifactInspectionClassV1 {
  ActiveControl,
  ImmutableArtifact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcArtifactInspectionV1 {
  pub class: GcArtifactInspectionClassV1,
  pub kind: GcArtifactKindV1,
  pub generation: u64,
  pub canonical_key: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcWorkspaceInspectionClassV1 {
  Manifest,
  Object,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcWorkspaceInspectionV1 {
  pub class: GcWorkspaceInspectionClassV1,
  pub database_id: [u8; 16],
  pub run_id: [u8; 16],
  pub generation: u64,
  pub checkpoint_sequence: u64,
}

#[derive(Debug, Error)]
pub enum GcMaintenanceInspectionErrorV1 {
  #[error("active GC controls cannot be dispatched as immutable artifacts")]
  ActiveControlDispatch,
  #[error("GC inspection key does not match the artifact's canonical identity")]
  KeyMismatch,
  #[error("GC workspace value is neither an AGCW manifest nor an AGWO object")]
  WorkspaceEnvelope,
  #[error("GC workspace identity width is invalid: {0}")]
  WorkspaceIdentityWidth(#[source] std::array::TryFromSliceError),
  #[error(transparent)]
  Format(#[from] FormatError),
}

impl GcMaintenanceInspectionErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::ActiveControlDispatch => "gc_inspection_active_control_dispatch",
      Self::KeyMismatch => "gc_inspection_key_mismatch",
      Self::WorkspaceEnvelope => "gc_inspection_workspace_envelope",
      Self::WorkspaceIdentityWidth(_) => "gc_inspection_workspace_identity_width",
      Self::Format(error) => error.code(),
    }
  }
}

/// Deeply decode one GC artifact and prove its locator key is canonical.
///
/// This dispatcher is the maintenance choke point for verify, repair, backup,
/// and migration inspection. It validates the complete family payload rather
/// than accepting a common AGCA envelope as sufficient evidence.
pub fn inspect_gc_artifact_v1(
  algorithm: HashAlgorithm,
  expected_key: &[u8],
  bytes: &[u8],
) -> Result<GcArtifactInspectionV1, GcMaintenanceInspectionErrorV1> {
  let envelope = decode_gc_artifact_envelope(bytes)?;
  let (class, canonical_key) = if envelope.kind.is_control() {
    let control = decode_gc_active_control(bytes, algorithm)?;
    (GcArtifactInspectionClassV1::ActiveControl, control.key)
  } else {
    inspect_immutable_gc_artifact_v1(algorithm, envelope.kind, bytes)?;
    (GcArtifactInspectionClassV1::ImmutableArtifact, immutable_gc_artifact_key(algorithm, envelope.kind, bytes))
  };
  if canonical_key != expected_key {
    return Err(GcMaintenanceInspectionErrorV1::KeyMismatch);
  }
  Ok(GcArtifactInspectionV1 { class, kind: envelope.kind, generation: envelope.generation, canonical_key })
}

pub fn inspect_gc_workspace_v1(algorithm: HashAlgorithm, bytes: &[u8]) -> Result<GcWorkspaceInspectionV1, GcMaintenanceInspectionErrorV1> {
  match bytes.get(..4) {
    Some(b"AGCW") => {
      let manifest = decode_mark_workspace_manifest(bytes, algorithm)?;
      Ok(GcWorkspaceInspectionV1 {
        class: GcWorkspaceInspectionClassV1::Manifest,
        database_id: copy_identity(manifest.database_id)?,
        run_id: copy_identity(manifest.run_id)?,
        generation: manifest.generation,
        checkpoint_sequence: manifest.checkpoint_sequence,
      })
    }
    Some(b"AGWO") => {
      let object = decode_mark_workspace_object(bytes, algorithm)?;
      Ok(GcWorkspaceInspectionV1 {
        class: GcWorkspaceInspectionClassV1::Object,
        database_id: copy_identity(object.database_id)?,
        run_id: copy_identity(object.run_id)?,
        generation: object.generation,
        checkpoint_sequence: object.checkpoint_sequence,
      })
    }
    _ => Err(GcMaintenanceInspectionErrorV1::WorkspaceEnvelope),
  }
}

fn inspect_immutable_gc_artifact_v1(
  algorithm: HashAlgorithm,
  kind: GcArtifactKindV1,
  bytes: &[u8],
) -> Result<(), GcMaintenanceInspectionErrorV1> {
  match kind {
    GcArtifactKindV1::QuarantineActiveControl
    | GcArtifactKindV1::MarkRunActiveControl
    | GcArtifactKindV1::PhysicalInventoryActiveControl
    | GcArtifactKindV1::AuditCatalogActiveControl
    | GcArtifactKindV1::VoidCatalogActiveControl
    | GcArtifactKindV1::RootLifecycleActiveControl => return Err(GcMaintenanceInspectionErrorV1::ActiveControlDispatch),
    GcArtifactKindV1::QuarantineManifest => {
      let _artifact = decode_quarantine_manifest_v1(bytes, algorithm)?;
    }
    GcArtifactKindV1::RootExpiryCatalogManifest => {
      let _artifact = decode_root_expiry_manifest_v1(bytes, algorithm)?;
    }
    GcArtifactKindV1::PhysicalInventoryManifest => {
      let _artifact = decode_physical_inventory_manifest_v1(bytes, algorithm)?;
    }
    GcArtifactKindV1::MarkRunCheckpoint | GcArtifactKindV1::MarkMutationJournalSegment => {
      let _artifact = decode_gc_mark_artifact(bytes, algorithm)?;
    }
    GcArtifactKindV1::AuditCatalogManifest
    | GcArtifactKindV1::GcRunSummary
    | GcArtifactKindV1::CorruptGcEvidence
    | GcArtifactKindV1::AuditDetailPage
    | GcArtifactKindV1::AuditSummaryPage
    | GcArtifactKindV1::AuditPin => {
      let _artifact = decode_audit_artifact(bytes, algorithm)?;
    }
    GcArtifactKindV1::VoidCatalogManifest
    | GcArtifactKindV1::VoidExtentPage
    | GcArtifactKindV1::VoidClaim
    | GcArtifactKindV1::SweepProposal
    | GcArtifactKindV1::SweepCommitReceipt
    | GcArtifactKindV1::RecoveredSweepReceipt
    | GcArtifactKindV1::VoidClaimSettlementReceipt => {
      let _artifact = decode_sweep_void_artifact(bytes, algorithm)?;
    }
    GcArtifactKindV1::RootLifecycleManifest => {
      let _artifact = decode_root_lifecycle_manifest_v1(bytes, algorithm)?;
    }
    GcArtifactKindV1::GcArtifactDirectoryNode => inspect_gc_directory_v1(algorithm, bytes)?,
    GcArtifactKindV1::CandidatePage
    | GcArtifactKindV1::RootExpiryPage
    | GcArtifactKindV1::RetirementJournalSegment
    | GcArtifactKindV1::PhysicalInventoryPage
    | GcArtifactKindV1::RootCandidatePage => {
      let _artifact = decode_gc_state_artifact(bytes, algorithm)?;
    }
    GcArtifactKindV1::CandidateDelta => {
      let _artifact = decode_candidate_delta_v1(bytes, algorithm)?;
    }
    GcArtifactKindV1::RootRetirementCommit => {
      let _artifact = decode_root_retirement_commit_v1(bytes, algorithm)?;
    }
    GcArtifactKindV1::RootObjectReclaimProof => {
      let _artifact = decode_root_object_reclaim_proof_v1(bytes, algorithm)?;
    }
  }
  Ok(())
}

fn inspect_gc_directory_v1(algorithm: HashAlgorithm, bytes: &[u8]) -> Result<(), GcMaintenanceInspectionErrorV1> {
  let envelope = decode_gc_artifact_envelope(bytes)?;
  let role = u16_at(envelope.identity, 32)?;
  match role {
    1..=5 | 8 => {
      let _artifact = decode_gc_state_artifact(bytes, algorithm)?;
    }
    6 | 7 => {
      let _artifact = decode_audit_artifact(bytes, algorithm)?;
    }
    _ => {
      let _artifact = decode_gc_state_artifact(bytes, algorithm)?;
    }
  }
  Ok(())
}

fn copy_identity(bytes: &[u8]) -> Result<[u8; 16], GcMaintenanceInspectionErrorV1> {
  bytes.try_into().map_err(GcMaintenanceInspectionErrorV1::WorkspaceIdentityWidth)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcMaintenanceInspectionLimitsV1 {
  maximum_artifacts: u64,
  maximum_workspaces: u64,
  maximum_findings: usize,
  maximum_repair_tickets: usize,
}

impl GcMaintenanceInspectionLimitsV1 {
  pub fn new(
    maximum_artifacts: u64,
    maximum_workspaces: u64,
    maximum_findings: usize,
    maximum_repair_tickets: usize,
  ) -> Result<Self, GcRunErrorV1> {
    if maximum_artifacts == 0
      || maximum_artifacts > MAXIMUM_ARTIFACT_LIMIT
      || maximum_workspaces == 0
      || maximum_workspaces > MAXIMUM_WORKSPACE_LIMIT
      || maximum_findings == 0
      || maximum_findings > MAXIMUM_FINDING_LIMIT
      || maximum_repair_tickets == 0
      || maximum_repair_tickets > MAXIMUM_REPAIR_TICKET_LIMIT
    {
      return Err(GcRunErrorV1::operation(
        "gc_maintenance_limits",
        "maintenance inspection limits must be nonzero and within the frozen hard caps",
      ));
    }
    Ok(Self { maximum_artifacts, maximum_workspaces, maximum_findings, maximum_repair_tickets })
  }

  pub fn for_tests() -> Self {
    Self { maximum_artifacts: 4_096, maximum_workspaces: 128, maximum_findings: 256, maximum_repair_tickets: 64 }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcCorruptionScopeV1 {
  GcAuthority,
  AuthoritativeNamespace,
}

#[derive(Debug, Clone, Copy)]
pub struct GcActiveControlObservationV1<'a> {
  pub expected_kind: GcArtifactKindV1,
  pub expected_slot: u8,
  pub expected_key: &'a [u8],
  pub bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
pub struct GcArtifactObservationV1<'a> {
  pub expected_key: &'a [u8],
  pub bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
pub struct GcWorkspaceObservationV1<'a> {
  pub bytes: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
pub struct GcAuthoritativeCorruptionObservationV1<'a> {
  pub scope: GcCorruptionScopeV1,
  pub code: &'a str,
  pub root_hash: Option<&'a [u8]>,
  pub path_digest: Option<&'a [u8]>,
  pub evidence_keys: &'a [&'a [u8]],
}

pub trait GcMaintenanceObservationVisitorV1 {
  fn observe_active_control(
    &mut self,
    observation: GcActiveControlObservationV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1>;

  fn observe_immutable_artifact(
    &mut self,
    observation: GcArtifactObservationV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1>;

  fn observe_workspace(&mut self, observation: GcWorkspaceObservationV1<'_>, cancellation: &CancellationToken) -> Result<(), GcRunErrorV1>;

  fn observe_authoritative_corruption(
    &mut self,
    observation: GcAuthoritativeCorruptionObservationV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1>;
}

pub trait GcMaintenanceInspectionSourceV1 {
  fn capture_basis(&mut self, cancellation: &CancellationToken) -> Result<GcRunBasisV1, GcRunErrorV1>;

  fn visit_active_controls(
    &mut self,
    visitor: &mut dyn GcMaintenanceObservationVisitorV1,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1>;

  fn visit_immutable_artifacts(
    &mut self,
    visitor: &mut dyn GcMaintenanceObservationVisitorV1,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1>;

  fn visit_workspaces(
    &mut self,
    visitor: &mut dyn GcMaintenanceObservationVisitorV1,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1>;

  fn visit_authoritative_corruption(
    &mut self,
    visitor: &mut dyn GcMaintenanceObservationVisitorV1,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcMaintenanceFindingV1 {
  pub code: String,
  pub blocking: bool,
  pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcRepairScopeV1 {
  GcAuthority,
  AuthoritativeNamespace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcRepairTicketProposalV1 {
  pub ticket_id: [u8; 16],
  pub scope: GcRepairScopeV1,
  pub code: String,
  pub root_hash: Option<Vec<u8>>,
  pub path_digest: Option<Vec<u8>>,
  pub evidence_keys: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcPathLatchProposalV1 {
  pub path_digest: Vec<u8>,
  pub ticket_ids: Vec<[u8; 16]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcTransferDispositionV1 {
  IncludeValidated,
  OmitDeclared,
  NodeLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcTransferPolicyInspectionV1 {
  pub physical_copy: GcTransferDispositionV1,
  pub logical_backup: GcTransferDispositionV1,
  pub import: GcTransferDispositionV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcBackupInspectionV1 {
  pub gc_artifacts: GcTransferPolicyInspectionV1,
  pub gc_workspaces: GcTransferPolicyInspectionV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcDestinationStateV1 {
  NeverMarked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcMigrationResetPlanV1 {
  pub destination_state: GcDestinationStateV1,
  pub copy_gc_artifacts: bool,
  pub copy_gc_audit_or_corrupt_evidence: bool,
  pub copy_gc_workspaces: bool,
  pub copy_gc_repair_state: bool,
  pub required_fresh_complete_marks: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct GcMaintenanceInspectionSummaryV1<'a> {
  pub artifact_count: u64,
  pub workspace_count: u64,
  pub selected_control_families: usize,
  pub findings: &'a [GcMaintenanceFindingV1],
  pub repair_tickets: &'a [GcRepairTicketProposalV1],
  pub path_latches: &'a [GcPathLatchProposalV1],
  pub destructive_gc_eligible: bool,
  pub backup: GcBackupInspectionV1,
  pub migration: GcMigrationResetPlanV1,
}

#[derive(Debug, Clone)]
struct ObservedControlV1 {
  bytes: Vec<u8>,
  target_key: Vec<u8>,
  target_kind: GcArtifactKindV1,
  generation: u64,
  target_found: bool,
}

#[derive(Debug, Clone)]
enum ControlSlotObservationV1 {
  Absent,
  Invalid,
  Valid(ObservedControlV1),
}

struct GcMaintenanceInspectionStateV1 {
  basis: GcRunBasisV1,
  limits: GcMaintenanceInspectionLimitsV1,
  controls: [ControlSlotObservationV1; 12],
  artifact_count: u64,
  workspace_count: u64,
  selected_control_families: usize,
  findings: Vec<GcMaintenanceFindingV1>,
  repair_tickets: Vec<GcRepairTicketProposalV1>,
  path_latches: Vec<GcPathLatchProposalV1>,
  destructive_gc_eligible: bool,
  backup: Option<GcBackupInspectionV1>,
  migration: Option<GcMigrationResetPlanV1>,
  finalized: bool,
}

impl GcMaintenanceInspectionStateV1 {
  fn new(basis: GcRunBasisV1, limits: GcMaintenanceInspectionLimitsV1) -> Self {
    Self {
      basis,
      limits,
      controls: std::array::from_fn(|_| ControlSlotObservationV1::Absent),
      artifact_count: 0,
      workspace_count: 0,
      selected_control_families: 0,
      findings: Vec::new(),
      repair_tickets: Vec::new(),
      path_latches: Vec::new(),
      destructive_gc_eligible: false,
      backup: None,
      migration: None,
      finalized: false,
    }
  }

  fn increment_artifacts(&mut self) -> Result<(), GcRunErrorV1> {
    self.artifact_count = self
      .artifact_count
      .checked_add(1)
      .ok_or_else(|| GcRunErrorV1::operation("gc_maintenance_artifact_limit", "maintenance artifact count overflowed"))?;
    if self.artifact_count > self.limits.maximum_artifacts {
      return Err(GcRunErrorV1::operation(
        "gc_maintenance_artifact_limit",
        "maintenance artifact inspection exceeded its configured bound",
      ));
    }
    Ok(())
  }

  fn add_finding(&mut self, code: impl Into<String>, blocking: bool, message: impl Into<String>) -> Result<(), GcRunErrorV1> {
    if self.findings.len() >= self.limits.maximum_findings {
      return Err(GcRunErrorV1::operation("gc_maintenance_finding_limit", "maintenance findings exceeded their configured bound"));
    }
    self.findings.try_reserve(1).map_err(|error| {
      GcRunErrorV1::operation("gc_maintenance_memory", format!("maintenance findings could not reserve bounded memory: {error}"))
    })?;
    self.findings.push(GcMaintenanceFindingV1 { code: code.into(), blocking, message: message.into() });
    Ok(())
  }

  fn reconcile_controls(&mut self) -> Result<(), GcRunErrorV1> {
    self.selected_control_families = 0;
    for (family_index, kind) in CONTROL_FAMILIES.iter().copied().enumerate() {
      let slot_a_index = family_index * 2;
      let slot_b_index = slot_a_index + 1;
      let slot_a = self.controls[slot_a_index].clone();
      let slot_b = self.controls[slot_b_index].clone();
      self.reconcile_control_pair(kind, slot_a, slot_b)?;
    }
    Ok(())
  }

  fn reconcile_control_pair(
    &mut self,
    kind: GcArtifactKindV1,
    slot_a: ControlSlotObservationV1,
    slot_b: ControlSlotObservationV1,
  ) -> Result<(), GcRunErrorV1> {
    let invalid_slot = matches!(slot_a, ControlSlotObservationV1::Invalid) || matches!(slot_b, ControlSlotObservationV1::Invalid);
    let valid_a = match &slot_a {
      ControlSlotObservationV1::Valid(control) => Some(control),
      ControlSlotObservationV1::Absent | ControlSlotObservationV1::Invalid => None,
    };
    let valid_b = match &slot_b {
      ControlSlotObservationV1::Valid(control) => Some(control),
      ControlSlotObservationV1::Absent | ControlSlotObservationV1::Invalid => None,
    };

    if valid_a.is_none() && valid_b.is_none() {
      if invalid_slot {
        let evidence = self.control_pair_keys(kind)?;
        self.add_gc_authority_failure("gc_control_unavailable", format!("{} has no valid A/B control", kind.name()), &evidence)?;
      }
      return Ok(());
    }

    if valid_a.is_some_and(|control| !control.target_found) || valid_b.is_some_and(|control| !control.target_found) {
      let mut evidence = Vec::new();
      evidence.try_reserve(2).map_err(|error| {
        GcRunErrorV1::operation("gc_maintenance_memory", format!("control-target evidence could not reserve bounded memory: {error}"))
      })?;
      if let Some(control) = valid_a.filter(|control| !control.target_found) {
        evidence.push(control.target_key.clone());
      }
      if let Some(control) = valid_b.filter(|control| !control.target_found) {
        evidence.push(control.target_key.clone());
      }
      self.add_gc_authority_failure(
        "gc_control_target_unavailable",
        format!("{} references a missing, corrupt, or mismatched target", kind.name()),
        &evidence,
      )?;
    }

    let selected = match (valid_a, valid_b) {
      (Some(control_a), Some(control_b)) => {
        let decoded_a = decode_gc_active_control(&control_a.bytes, self.basis.hash_algorithm).map_err(format_run_error)?;
        let decoded_b = decode_gc_active_control(&control_b.bytes, self.basis.hash_algorithm).map_err(format_run_error)?;
        match select_gc_active_control(&decoded_a, control_a.target_found, &decoded_b, control_b.target_found) {
          Ok(selected) => selected.is_some(),
          Err(error) => {
            let evidence = [control_a.target_key.clone(), control_b.target_key.clone()];
            self.add_gc_authority_failure("gc_control_pair_ambiguous", error.to_string(), &evidence)?;
            false
          }
        }
      }
      (Some(control), None) | (None, Some(control)) => control.target_found,
      (None, None) => false,
    };
    if selected {
      self.selected_control_families += 1;
    } else {
      let evidence = self.control_pair_keys(kind)?;
      self.add_gc_authority_failure(
        "gc_control_closure_unavailable",
        format!("{} has no closure-valid selected control", kind.name()),
        &evidence,
      )?;
    }
    Ok(())
  }

  fn add_gc_authority_failure(&mut self, code: &'static str, message: String, evidence_keys: &[Vec<u8>]) -> Result<(), GcRunErrorV1> {
    self.add_finding(code, true, message)?;
    let evidence: Vec<_> = evidence_keys.iter().map(Vec::as_slice).collect();
    self.add_repair_proposal(GcAuthoritativeCorruptionObservationV1 {
      scope: GcCorruptionScopeV1::GcAuthority,
      code,
      root_hash: None,
      path_digest: None,
      evidence_keys: &evidence,
    })
  }

  fn control_pair_keys(&self, kind: GcArtifactKindV1) -> Result<[Vec<u8>; 2], GcRunErrorV1> {
    Ok([
      gc_active_control_key(self.basis.hash_algorithm, kind, &self.basis.database_id, 0).map_err(format_run_error)?,
      gc_active_control_key(self.basis.hash_algorithm, kind, &self.basis.database_id, 1).map_err(format_run_error)?,
    ])
  }

  fn add_repair_proposal(&mut self, observation: GcAuthoritativeCorruptionObservationV1<'_>) -> Result<(), GcRunErrorV1> {
    validate_corruption_observation(self.basis.hash_algorithm, observation)?;
    let scope = match observation.scope {
      GcCorruptionScopeV1::GcAuthority => GcRepairScopeV1::GcAuthority,
      GcCorruptionScopeV1::AuthoritativeNamespace => GcRepairScopeV1::AuthoritativeNamespace,
    };
    let mut evidence_keys = Vec::new();
    evidence_keys.try_reserve(observation.evidence_keys.len()).map_err(|error| {
      GcRunErrorV1::operation("gc_maintenance_memory", format!("maintenance repair evidence could not reserve bounded memory: {error}"))
    })?;
    for key in observation.evidence_keys {
      evidence_keys.push(key.to_vec());
    }
    evidence_keys.sort();
    evidence_keys.dedup();
    let ticket_id = repair_ticket_id(
      self.basis.hash_algorithm,
      self.basis.database_id,
      scope,
      observation.code,
      observation.root_hash,
      observation.path_digest,
      &evidence_keys,
    )?;
    if self.repair_tickets.iter().any(|ticket| ticket.ticket_id == ticket_id) {
      return Ok(());
    }
    if self.repair_tickets.len() >= self.limits.maximum_repair_tickets {
      return Err(GcRunErrorV1::operation(
        "gc_maintenance_repair_ticket_limit",
        "maintenance repair-ticket proposals exceeded their configured bound",
      ));
    }
    self.repair_tickets.try_reserve(1).map_err(|error| {
      GcRunErrorV1::operation(
        "gc_maintenance_memory",
        format!("maintenance repair-ticket proposals could not reserve bounded memory: {error}"),
      )
    })?;
    self.repair_tickets.push(GcRepairTicketProposalV1 {
      ticket_id,
      scope,
      code: observation.code.to_string(),
      root_hash: observation.root_hash.map(ToOwned::to_owned),
      path_digest: observation.path_digest.map(ToOwned::to_owned),
      evidence_keys,
    });

    if scope == GcRepairScopeV1::AuthoritativeNamespace {
      let path_digest = observation.path_digest.ok_or_else(|| {
        GcRunErrorV1::operation("gc_maintenance_corruption_observation", "validated namespace corruption lost its path digest")
      })?;
      if let Some(latch) = self.path_latches.iter_mut().find(|latch| latch.path_digest == path_digest) {
        if !latch.ticket_ids.contains(&ticket_id) {
          latch.ticket_ids.try_reserve(1).map_err(|error| {
            GcRunErrorV1::operation(
              "gc_maintenance_memory",
              format!("maintenance path-latch ticket IDs could not reserve bounded memory: {error}"),
            )
          })?;
          latch.ticket_ids.push(ticket_id);
          latch.ticket_ids.sort();
        }
      } else {
        self.path_latches.try_reserve(1).map_err(|error| {
          GcRunErrorV1::operation(
            "gc_maintenance_memory",
            format!("maintenance path-latch proposals could not reserve bounded memory: {error}"),
          )
        })?;
        self.path_latches.push(GcPathLatchProposalV1 { path_digest: path_digest.to_vec(), ticket_ids: vec![ticket_id] });
      }
    }
    Ok(())
  }

  fn finalize(&mut self) -> Result<(), GcRunErrorV1> {
    let (backup, migration) = project_transfer_and_migration_policy(self.basis.hash_algorithm)?;
    self.destructive_gc_eligible =
      self.selected_control_families == CONTROL_FAMILIES.len() && !self.findings.iter().any(|finding| finding.blocking);
    self.backup = Some(backup);
    self.migration = Some(migration);
    self.finalized = true;
    Ok(())
  }
}

impl GcMaintenanceObservationVisitorV1 for GcMaintenanceInspectionStateV1 {
  fn observe_active_control(
    &mut self,
    observation: GcActiveControlObservationV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1> {
    check_cancellation(cancellation)?;
    self.increment_artifacts()?;
    let family_index = control_family_index(observation.expected_kind)
      .ok_or_else(|| GcRunErrorV1::operation("gc_maintenance_source_contract", "active-control source supplied a non-control kind"))?;
    if observation.expected_slot > 1 {
      return Err(GcRunErrorV1::operation("gc_maintenance_source_contract", "active-control source supplied a slot outside A/B"));
    }
    let slot_index = family_index * 2 + usize::from(observation.expected_slot);
    let canonical_control_key =
      gc_active_control_key(self.basis.hash_algorithm, observation.expected_kind, &self.basis.database_id, observation.expected_slot)
        .map_err(format_run_error)?;
    if !matches!(self.controls[slot_index], ControlSlotObservationV1::Absent) {
      self.controls[slot_index] = ControlSlotObservationV1::Invalid;
      self.add_gc_authority_failure(
        "gc_control_duplicate_slot",
        "active-control source returned a duplicate A/B slot".to_string(),
        &[canonical_control_key],
      )?;
      return Ok(());
    }

    let inspected = inspect_gc_artifact_v1(self.basis.hash_algorithm, observation.expected_key, observation.bytes);
    let decoded = decode_gc_active_control(observation.bytes, self.basis.hash_algorithm);
    match (inspected, decoded) {
      (Ok(inspected), Ok(control))
        if inspected.class == GcArtifactInspectionClassV1::ActiveControl
          && control.kind == observation.expected_kind
          && control.slot == observation.expected_slot
          && control.database_id == self.basis.database_id =>
      {
        self.controls[slot_index] = ControlSlotObservationV1::Valid(ObservedControlV1 {
          bytes: observation.bytes.to_vec(),
          target_key: control.target_manifest_hash.to_vec(),
          target_kind: control
            .kind
            .control_target()
            .ok_or_else(|| GcRunErrorV1::operation("gc_maintenance_control_target", "validated active control has no target kind"))?,
          generation: control.generation,
          target_found: false,
        });
      }
      (inspection, decode) => {
        self.controls[slot_index] = ControlSlotObservationV1::Invalid;
        let detail = match (inspection, decode) {
          (Err(error), _) => error.code(),
          (Ok(_), Err(error)) => error.code(),
          (Ok(_), Ok(_)) => "gc_control_identity_mismatch",
        };
        self.add_gc_authority_failure(
          "gc_control_invalid",
          format!("active GC control failed deep inspection: {detail}"),
          &[canonical_control_key],
        )?;
      }
    }
    Ok(())
  }

  fn observe_immutable_artifact(
    &mut self,
    observation: GcArtifactObservationV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1> {
    check_cancellation(cancellation)?;
    self.increment_artifacts()?;
    match inspect_gc_artifact_v1(self.basis.hash_algorithm, observation.expected_key, observation.bytes) {
      Ok(inspected) if inspected.class == GcArtifactInspectionClassV1::ImmutableArtifact => {
        for slot in &mut self.controls {
          let ControlSlotObservationV1::Valid(control) = slot else {
            continue;
          };
          if control.target_key == observation.expected_key {
            control.target_found = inspected.kind == control.target_kind && inspected.generation == control.generation;
          }
        }
      }
      Ok(_) => {
        self.add_finding("gc_artifact_source_mismatch", true, "immutable-artifact source returned an active control")?;
      }
      Err(error) => {
        self.add_finding("gc_artifact_invalid", false, format!("GC artifact failed deep inspection: {}", error.code()))?;
      }
    }
    Ok(())
  }

  fn observe_workspace(&mut self, observation: GcWorkspaceObservationV1<'_>, cancellation: &CancellationToken) -> Result<(), GcRunErrorV1> {
    check_cancellation(cancellation)?;
    self.workspace_count = self
      .workspace_count
      .checked_add(1)
      .ok_or_else(|| GcRunErrorV1::operation("gc_maintenance_workspace_limit", "maintenance workspace count overflowed"))?;
    if self.workspace_count > self.limits.maximum_workspaces {
      return Err(GcRunErrorV1::operation(
        "gc_maintenance_workspace_limit",
        "maintenance workspace inspection exceeded its configured bound",
      ));
    }
    match inspect_gc_workspace_v1(self.basis.hash_algorithm, observation.bytes) {
      Ok(workspace) if workspace.database_id == self.basis.database_id => Ok(()),
      Ok(_) => self.add_finding("gc_workspace_identity_mismatch", false, "GC workspace belongs to a different database"),
      Err(error) => self.add_finding("gc_workspace_invalid", false, format!("GC workspace failed deep inspection: {}", error.code())),
    }
  }

  fn observe_authoritative_corruption(
    &mut self,
    observation: GcAuthoritativeCorruptionObservationV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<(), GcRunErrorV1> {
    check_cancellation(cancellation)?;
    validate_corruption_observation(self.basis.hash_algorithm, observation)?;
    self.add_finding(observation.code, true, "authoritative traversal reported structural corruption")?;
    self.add_repair_proposal(observation)
  }
}

pub struct GcMaintenanceInspectionOperationV1<S> {
  source: S,
  limits: GcMaintenanceInspectionLimitsV1,
  state: Option<GcMaintenanceInspectionStateV1>,
}

impl<S> GcMaintenanceInspectionOperationV1<S>
where
  S: GcMaintenanceInspectionSourceV1,
{
  pub fn new(source: S, limits: GcMaintenanceInspectionLimitsV1) -> Result<Self, GcRunErrorV1> {
    Ok(Self { source, limits, state: None })
  }

  pub fn summary(&self) -> Option<GcMaintenanceInspectionSummaryV1<'_>> {
    let state = self.state.as_ref().filter(|state| state.finalized)?;
    Some(GcMaintenanceInspectionSummaryV1 {
      artifact_count: state.artifact_count,
      workspace_count: state.workspace_count,
      selected_control_families: state.selected_control_families,
      findings: &state.findings,
      repair_tickets: &state.repair_tickets,
      path_latches: &state.path_latches,
      destructive_gc_eligible: state.destructive_gc_eligible,
      backup: state.backup?,
      migration: state.migration?,
    })
  }

  fn state_mut(&mut self) -> Result<&mut GcMaintenanceInspectionStateV1, GcRunErrorV1> {
    self
      .state
      .as_mut()
      .ok_or_else(|| GcRunErrorV1::operation("gc_maintenance_phase_order", "maintenance inspection phase ran before basis capture"))
  }
}

impl<S> GcRunOperationV1 for GcMaintenanceInspectionOperationV1<S>
where
  S: GcMaintenanceInspectionSourceV1,
{
  fn execute_phase(&mut self, phase: GcRunPhaseV1, reporter: &mut GcRunPhaseReporterV1<'_>) -> Result<GcRunPhaseOutcomeV1, GcRunErrorV1> {
    match phase {
      GcRunPhaseV1::Prepare => {
        let basis = self.source.capture_basis(reporter.cancellation())?;
        reporter.capture_basis(basis.clone())?;
        self.state = Some(GcMaintenanceInspectionStateV1::new(basis, self.limits));
      }
      GcRunPhaseV1::Inventory => {
        let (source, state) = (&mut self.source, self.state.as_mut().ok_or_else(phase_order_error)?);
        source.visit_active_controls(state, reporter.cancellation())?;
        source.visit_immutable_artifacts(state, reporter.cancellation())?;
      }
      GcRunPhaseV1::Mark => {
        let (source, state) = (&mut self.source, self.state.as_mut().ok_or_else(phase_order_error)?);
        source.visit_workspaces(state, reporter.cancellation())?;
        source.visit_authoritative_corruption(state, reporter.cancellation())?;
      }
      GcRunPhaseV1::MutationConvergence => self.state_mut()?.reconcile_controls()?,
      GcRunPhaseV1::Finalize => self.state_mut()?.finalize()?,
    }
    let completed_units = self.state.as_ref().map_or(0, |state| state.artifact_count + state.workspace_count);
    reporter.report(GcRunProgressUpdateV1 {
      phase_progress: 1.0,
      completed_units,
      message: Some(format!("completed bounded {} inspection", phase.name())),
      ..GcRunProgressUpdateV1::default()
    })?;
    if phase == GcRunPhaseV1::Finalize && self.state.as_ref().is_some_and(|state| !state.findings.is_empty()) {
      return Ok(GcRunPhaseOutcomeV1::Incomplete {
        code: "gc_maintenance_incomplete",
        message: "maintenance inspection found corruption, degraded state, or unavailable authority".to_string(),
      });
    }
    Ok(GcRunPhaseOutcomeV1::Continue)
  }
}

fn control_family_index(kind: GcArtifactKindV1) -> Option<usize> {
  CONTROL_FAMILIES.iter().position(|candidate| *candidate == kind)
}

fn validate_corruption_observation(
  algorithm: HashAlgorithm,
  observation: GcAuthoritativeCorruptionObservationV1<'_>,
) -> Result<(), GcRunErrorV1> {
  if !valid_code(observation.code) || observation.evidence_keys.is_empty() || observation.evidence_keys.len() > MAXIMUM_EVIDENCE_KEYS {
    return Err(GcRunErrorV1::operation(
      "gc_maintenance_corruption_observation",
      "corruption observations require a bounded code and one to 64 evidence keys",
    ));
  }
  let hash_width = algorithm.hash_length();
  let valid_digest = |digest: &[u8]| digest.len() == hash_width && digest.iter().any(|byte| *byte != 0);
  if observation.evidence_keys.iter().any(|key| !valid_digest(key))
    || observation.root_hash.is_some_and(|root| !valid_digest(root))
    || observation.path_digest.is_some_and(|path| !valid_digest(path))
  {
    return Err(GcRunErrorV1::operation(
      "gc_maintenance_corruption_observation",
      "corruption observation digests must be nonzero and use the selected hash width",
    ));
  }
  match observation.scope {
    GcCorruptionScopeV1::GcAuthority if observation.path_digest.is_some() => {
      Err(GcRunErrorV1::operation("gc_maintenance_corruption_observation", "GC-authority corruption cannot propose a namespace path latch"))
    }
    GcCorruptionScopeV1::AuthoritativeNamespace if observation.root_hash.is_none() || observation.path_digest.is_none() => Err(
      GcRunErrorV1::operation("gc_maintenance_corruption_observation", "authoritative namespace corruption requires root and path digests"),
    ),
    GcCorruptionScopeV1::GcAuthority | GcCorruptionScopeV1::AuthoritativeNamespace => Ok(()),
  }
}

fn repair_ticket_id(
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  scope: GcRepairScopeV1,
  code: &str,
  root_hash: Option<&[u8]>,
  path_digest: Option<&[u8]>,
  evidence_keys: &[Vec<u8>],
) -> Result<[u8; 16], GcRunErrorV1> {
  let scope_byte = [match scope {
    GcRepairScopeV1::GcAuthority => 1,
    GcRepairScopeV1::AuthoritativeNamespace => 2,
  }];
  let code_length = u16::try_from(code.len())
    .map_err(|error| {
      GcRunErrorV1::operation("gc_maintenance_corruption_observation", format!("corruption code length exceeds u16: {error}"))
    })?
    .to_le_bytes();
  let root_presence = [u8::from(root_hash.is_some())];
  let path_presence = [u8::from(path_digest.is_some())];
  let evidence_count = u16::try_from(evidence_keys.len())
    .map_err(|error| GcRunErrorV1::operation("gc_maintenance_corruption_observation", format!("evidence count exceeds u16: {error}")))?
    .to_le_bytes();
  let mut parts: Vec<&[u8]> = Vec::new();
  parts.try_reserve(10 + evidence_keys.len()).map_err(|error| {
    GcRunErrorV1::operation("gc_maintenance_memory", format!("repair-ticket digest parts could not reserve bounded memory: {error}"))
  })?;
  parts.extend_from_slice(&[b"aeordb:v4:gc-repair-ticket:v1", &database_id, &scope_byte, &code_length, code.as_bytes(), &root_presence]);
  if let Some(root_hash) = root_hash {
    parts.push(root_hash);
  }
  parts.push(&path_presence);
  if let Some(path_digest) = path_digest {
    parts.push(path_digest);
  }
  parts.push(&evidence_count);
  for evidence_key in evidence_keys {
    parts.push(evidence_key);
  }
  let digest = digest_parts(algorithm, &parts);
  let mut ticket_id = [0u8; 16];
  ticket_id.copy_from_slice(&digest[..16]);
  Ok(ticket_id)
}

fn project_transfer_and_migration_policy(algorithm: HashAlgorithm) -> Result<(GcBackupInspectionV1, GcMigrationResetPlanV1), GcRunErrorV1> {
  let resolver = SystemFamilyPolicyResolverV1::embedded(algorithm).map_err(format_run_error)?;
  let gc_subject = SystemFamilySubjectV1::EntryType(u16::from(EntryTypeV4::GcArtifact.to_u8()));
  let gc_artifact_physical_copy = require_transfer_policy(
    resolver,
    gc_subject,
    SystemFamilyTransferOperationV1::PhysicalCopy,
    TransferPolicyV1::RequiredInclude,
    GcTransferDispositionV1::IncludeValidated,
    0x0051,
  )?;
  let gc_artifact_logical_backup = require_transfer_policy(
    resolver,
    gc_subject,
    SystemFamilyTransferOperationV1::LogicalBackup,
    TransferPolicyV1::OmitDeclared,
    GcTransferDispositionV1::OmitDeclared,
    0x0051,
  )?;
  let gc_artifact_import = require_transfer_policy(
    resolver,
    gc_subject,
    SystemFamilyTransferOperationV1::Import,
    TransferPolicyV1::NodeLocal,
    GcTransferDispositionV1::NodeLocal,
    0x0051,
  )?;
  require_destination_local_migration(resolver, gc_subject, 0x0051)?;

  let workspace_subject = SystemFamilySubjectV1::ExternalWorkspaceKind(3);
  let gc_workspace_physical_copy = require_transfer_policy(
    resolver,
    workspace_subject,
    SystemFamilyTransferOperationV1::PhysicalCopy,
    TransferPolicyV1::NodeLocal,
    GcTransferDispositionV1::NodeLocal,
    0x0071,
  )?;
  let gc_workspace_logical_backup = require_transfer_policy(
    resolver,
    workspace_subject,
    SystemFamilyTransferOperationV1::LogicalBackup,
    TransferPolicyV1::OmitDeclared,
    GcTransferDispositionV1::OmitDeclared,
    0x0071,
  )?;
  let gc_workspace_import = require_transfer_policy(
    resolver,
    workspace_subject,
    SystemFamilyTransferOperationV1::Import,
    TransferPolicyV1::NodeLocal,
    GcTransferDispositionV1::NodeLocal,
    0x0071,
  )?;
  require_destination_local_migration(resolver, workspace_subject, 0x0071)?;

  Ok((
    GcBackupInspectionV1 {
      gc_artifacts: GcTransferPolicyInspectionV1 {
        physical_copy: gc_artifact_physical_copy,
        logical_backup: gc_artifact_logical_backup,
        import: gc_artifact_import,
      },
      gc_workspaces: GcTransferPolicyInspectionV1 {
        physical_copy: gc_workspace_physical_copy,
        logical_backup: gc_workspace_logical_backup,
        import: gc_workspace_import,
      },
    },
    GcMigrationResetPlanV1 {
      destination_state: GcDestinationStateV1::NeverMarked,
      copy_gc_artifacts: false,
      copy_gc_audit_or_corrupt_evidence: false,
      copy_gc_workspaces: false,
      copy_gc_repair_state: false,
      required_fresh_complete_marks: 2,
    },
  ))
}

fn require_transfer_policy(
  resolver: SystemFamilyPolicyResolverV1,
  subject: SystemFamilySubjectV1<'_>,
  operation: SystemFamilyTransferOperationV1,
  expected: TransferPolicyV1,
  disposition: GcTransferDispositionV1,
  expected_family_id: u16,
) -> Result<GcTransferDispositionV1, GcRunErrorV1> {
  match resolver.transfer_policy(subject, operation).map_err(format_run_error)? {
    SystemFamilyPolicyDecisionV1::Known { family_id, policy } if family_id == expected_family_id && policy == expected => Ok(disposition),
    _ => Err(GcRunErrorV1::operation(
      "gc_maintenance_system_family_policy",
      "selected SystemFamily registry disagrees with the frozen GC transfer policy",
    )),
  }
}

fn require_destination_local_migration(
  resolver: SystemFamilyPolicyResolverV1,
  subject: SystemFamilySubjectV1<'_>,
  expected_family_id: u16,
) -> Result<(), GcRunErrorV1> {
  match resolver.policy(subject, "GC maintenance migration reset").map_err(format_run_error)? {
    SystemFamilyPolicyDecisionV1::Known { family_id, policy }
      if family_id == expected_family_id && policy.migration_policy == MigrationPolicyV1::DestinationLocal =>
    {
      Ok(())
    }
    _ => Err(GcRunErrorV1::operation(
      "gc_maintenance_system_family_policy",
      "selected SystemFamily registry disagrees with the frozen GC destination-local migration policy",
    )),
  }
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), GcRunErrorV1> {
  if cancellation.is_cancelled() {
    return Err(GcRunErrorV1::operation("gc_run_cancelled", "maintenance inspection was cancelled"));
  }
  Ok(())
}

fn phase_order_error() -> GcRunErrorV1 {
  GcRunErrorV1::operation("gc_maintenance_phase_order", "maintenance inspection phase ran before basis capture")
}

fn format_run_error(error: FormatError) -> GcRunErrorV1 {
  GcRunErrorV1::operation("gc_maintenance_format", error.to_string())
}

fn valid_code(code: &str) -> bool {
  !code.is_empty() && code.len() <= 128 && code.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}
