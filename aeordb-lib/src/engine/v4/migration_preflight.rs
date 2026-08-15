//! Bounded, storage-neutral admission for a v3-to-v4 shadow migration.
//!
//! Existing read-only inspectors produce the observations defined here. This
//! module only closes those observations against one source frontier and
//! issues an opaque permit; it owns no database, namespace, or service I/O.

use std::fmt::{self, Display, Formatter};

use thiserror::Error;

use super::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use super::contract_generated::CONTRACT_REGISTRY_SHA256;
use super::deployment_guard::DeploymentTransitionStateV1;
use super::system_family::embedded_system_family_registry;
use crate::engine::HashAlgorithm;
use crate::engine::file_header::FileHeader;
use crate::engine::memory_coordinator::{MemoryCoordinatorSnapshot, MemoryPressure};
use crate::engine::native_durability::{
  NativeDurabilityMechanism, NativeDurabilityProbeReport, NativeOperationSupport, PlatformFileIdentityDescriptorV1,
};
use crate::engine::verify::VerifyReport;

pub use crate::engine::run_configuration::MigrationRunConfiguration;

const GIB: u64 = 1024 * 1024 * 1024;
const MAX_CAPTURE_BYTES: u64 = 4 * 1024 * GIB;
const MAX_FINDINGS: usize = 32;
const EVIDENCE_DOMAIN_V1: &[u8] = b"aeordb.migration-preflight-evidence.v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationIdentityEvidenceV1 {
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub source_path_digest: [u8; 32],
  pub destination_path_digest: [u8; 32],
  pub source_file_identity: PlatformFileIdentityDescriptorV1,
  pub destination_parent_identity: PlatformFileIdentityDescriptorV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationSourceEvidenceV1 {
  pub hash_algorithm: HashAlgorithm,
  pub file_size: u64,
  pub complete_file_checksum: [u8; 32],
  pub selected_header_slot: u8,
  pub selected_header_sequence: u64,
  pub selected_header_digest: [u8; 32],
  pub head_hash: Vec<u8>,
}

impl MigrationSourceEvidenceV1 {
  pub fn from_v3_header(
    header: &FileHeader,
    selected_header_slot: usize,
    file_size: u64,
    complete_file_checksum: [u8; 32],
    selected_header_digest: [u8; 32],
  ) -> Result<Self, MigrationPreflightObservationErrorV1> {
    if header.header_version != 3 || selected_header_slot > 1 {
      return Err(MigrationPreflightObservationErrorV1::new(
        "source_header_not_v3",
        "migration source evidence requires a selected v3 header slot",
      ));
    }
    Ok(Self {
      hash_algorithm: header.hash_algo,
      file_size,
      complete_file_checksum,
      selected_header_slot: selected_header_slot as u8,
      selected_header_sequence: header.sequence,
      selected_header_digest,
      head_hash: header.head_hash.clone(),
    })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrictVerificationStateV1 {
  CompleteClean,
  CompleteWithIssues,
  Incomplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrictVerificationEvidenceV1 {
  pub state: StrictVerificationStateV1,
  pub source_file_size: u64,
  pub source_header_sequence: u64,
  pub source_complete_file_checksum: [u8; 32],
  pub issue_count: u64,
  pub evidence_digest: [u8; 32],
}

impl StrictVerificationEvidenceV1 {
  /// Convert a successfully completed checked verification report. Operational
  /// failures from `verify_checked` never reach this adapter and must be
  /// represented as `Incomplete` by the owning preflight collector.
  pub fn from_complete_report(report: &VerifyReport, source_header_sequence: u64, source_complete_file_checksum: [u8; 32]) -> Self {
    let issue_count = verification_issue_count(report);
    Self {
      state: if report.has_issues() { StrictVerificationStateV1::CompleteWithIssues } else { StrictVerificationStateV1::CompleteClean },
      source_file_size: report.file_size,
      source_header_sequence,
      source_complete_file_checksum,
      issue_count,
      evidence_digest: verification_evidence_digest(report),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationRecoveryEvidenceV1 {
  pub inspection_complete: bool,
  pub source_header_sequence: u64,
  pub durability_latched: bool,
  pub repair_active: bool,
  pub external_spill_count: u64,
  pub repair_ticket_count: u64,
  pub path_latch_count: u64,
  pub evidence_digest: [u8; 32],
}

impl MigrationRecoveryEvidenceV1 {
  pub fn from_deployment_state(
    state: &DeploymentTransitionStateV1,
    source_header_sequence: u64,
    repair_ticket_count: u64,
    path_latch_count: u64,
  ) -> Result<Self, MigrationPreflightObservationErrorV1> {
    if state.database_header_version != 3 {
      return Err(MigrationPreflightObservationErrorV1::new(
        "recovery_state_not_v3",
        "migration recovery evidence requires a v3 transition inspection",
      ));
    }
    let durability_latched = state.persistent_recovery.as_ref().is_some_and(|recovery| recovery.blocks_writes);
    let repair_active =
      state.persistent_recovery.as_ref().is_some_and(|recovery| recovery.is_repair_verifying() || recovery.is_catalog_replaying());
    let external_spill_count = usize_to_u64(state.external_spill_count);
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aeordb.migration-recovery-observation.v1\0");
    hasher.update(&[state.database_header_version]);
    hash_u64(&mut hasher, source_header_sequence);
    hash_u64(&mut hasher, external_spill_count);
    hash_u64(&mut hasher, repair_ticket_count);
    hash_u64(&mut hasher, path_latch_count);
    hasher.update(&[u8::from(state.requires_transition_capability)]);
    if let Some(recovery) = &state.persistent_recovery {
      hasher.update(&[1]);
      hasher.update(&recovery.database_id);
      hasher.update(&[u8::from(recovery.blocks_writes), u8::from(recovery.redundancy_degraded)]);
      for value in [recovery.latch_state, recovery.catalog_state] {
        match value {
          Some(value) => {
            hasher.update(&[1]);
            hasher.update(&value.to_le_bytes());
          }
          None => {
            hasher.update(&[0]);
          }
        }
      }
      for value in [recovery.latch_sequence, recovery.catalog_sequence] {
        match value {
          Some(value) => {
            hasher.update(&[1]);
            hash_u64(&mut hasher, value);
          }
          None => {
            hasher.update(&[0]);
          }
        }
      }
      hash_len_bytes(&mut hasher, recovery.reason.as_bytes());
    } else {
      hasher.update(&[0]);
    }
    for reason in &state.reasons {
      hash_len_bytes(&mut hasher, reason.as_bytes());
    }
    Ok(Self {
      inspection_complete: true,
      source_header_sequence,
      durability_latched,
      repair_active,
      external_spill_count,
      repair_ticket_count,
      path_latch_count,
      evidence_digest: *hasher.finalize().as_bytes(),
    })
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuthorityInventoryCountsV1 {
  pub protected_families: u64,
  pub modules: u64,
  pub snapshots: u64,
  pub forks: u64,
  pub symlinks: u64,
  pub history_roots: u64,
  pub peers: u64,
  pub sync_states: u64,
  pub tasks: u64,
  pub plugins: u64,
  pub roots: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceAuthorityInventoryV1 {
  pub complete: bool,
  pub source_header_sequence: u64,
  pub unresolved_family_count: u64,
  pub counts: AuthorityInventoryCountsV1,
  pub authority_digest: [u8; 32],
  pub system_family_registry_fingerprint: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum CapacityRoleV1 {
  Destination = 1,
  Workspace = 2,
  Backup = 3,
  Capture = 4,
}

impl CapacityRoleV1 {
  const ALL: [Self; 4] = [Self::Destination, Self::Workspace, Self::Backup, Self::Capture];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationCapacityObservationV1 {
  pub role: CapacityRoleV1,
  pub volume_identity: [u8; 16],
  pub path_identity: PlatformFileIdentityDescriptorV1,
  pub filesystem_capacity_bytes: u64,
  pub available_bytes: u64,
  pub required_bytes: u64,
  pub minimum_remaining_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeCutoverCapabilitiesV1 {
  pub data_barrier: bool,
  pub file_barrier: bool,
  pub parent_directory_sync: bool,
  pub durable_replace: bool,
  pub preallocation: bool,
  pub stable_file_identity: bool,
  pub read_back_verified: bool,
  pub qualification_digest: [u8; 32],
}

impl NativeCutoverCapabilitiesV1 {
  pub fn from_probe_report(report: &NativeDurabilityProbeReport) -> Self {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aeordb.native-cutover-qualification.v1\0");
    hash_len_bytes(&mut hasher, report.filesystem.kind.as_bytes());
    hasher.update(&report.filesystem.flags.to_le_bytes());
    for support in [
      &report.capabilities.data_barrier,
      &report.capabilities.file_barrier,
      &report.capabilities.parent_directory_sync,
      &report.capabilities.durable_replace,
      &report.capabilities.preallocation,
      &report.capabilities.stable_file_identity,
    ] {
      hash_support(&mut hasher, support);
    }
    for mechanism in [
      report.mechanisms.data_barrier,
      report.mechanisms.file_barrier,
      report.mechanisms.parent_directory_sync,
      report.mechanisms.durable_replace,
    ] {
      hasher.update(&[mechanism.map_or(0, mechanism_tag)]);
    }
    hasher.update(&[u8::from(report.read_back_verified)]);
    for identity in
      [report.identity_before_rename, report.identity_after_rename, report.destination_identity_before_replace, report.replaced_identity]
    {
      match identity {
        Some(identity) => {
          hasher.update(&[1]);
          hasher.update(&identity.to_bytes());
        }
        None => {
          hasher.update(&[0]);
        }
      }
    }
    Self {
      data_barrier: is_supported(&report.capabilities.data_barrier),
      file_barrier: is_supported(&report.capabilities.file_barrier),
      parent_directory_sync: is_supported(&report.capabilities.parent_directory_sync),
      durable_replace: is_supported(&report.capabilities.durable_replace),
      preallocation: is_supported(&report.capabilities.preallocation),
      stable_file_identity: is_supported(&report.capabilities.stable_file_identity),
      read_back_verified: report.read_back_verified,
      qualification_digest: *hasher.finalize().as_bytes(),
    }
  }

  fn is_complete(self) -> bool {
    !all_zero(&self.qualification_digest)
  }

  fn is_supported(self) -> bool {
    self.data_barrier
      && self.file_barrier
      && self.parent_directory_sync
      && self.durable_replace
      && self.preallocation
      && self.stable_file_identity
      && self.read_back_verified
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationNativeEvidenceV1 {
  pub source: NativeCutoverCapabilitiesV1,
  pub destination: NativeCutoverCapabilitiesV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationMemoryEvidenceV1 {
  pub source_budget_bytes: u64,
  pub destination_budget_bytes: u64,
  pub coordinator_accounted_bytes: u64,
  pub coordinator_ordinary_limit_bytes: u64,
  pub host_available_bytes: u64,
  pub host_available_floor_bytes: u64,
  pub pressure: MemoryPressure,
  pub evidence_digest: [u8; 32],
}

impl MigrationMemoryEvidenceV1 {
  pub fn from_snapshot(
    snapshot: &MemoryCoordinatorSnapshot,
    source_budget_bytes: u64,
    destination_budget_bytes: u64,
  ) -> Result<Self, MigrationPreflightObservationErrorV1> {
    let policy = snapshot.policy.ok_or_else(|| {
      MigrationPreflightObservationErrorV1::new("memory_policy_unavailable", "migration preflight requires a resolved memory policy")
    })?;
    let host_available_bytes = snapshot.host.host_available_bytes.ok_or_else(|| {
      MigrationPreflightObservationErrorV1::new("host_memory_unavailable", "migration preflight requires a host-available memory sample")
    })?;
    if snapshot.pressure == MemoryPressure::Unconfigured {
      return Err(MigrationPreflightObservationErrorV1::new(
        "memory_pressure_unconfigured",
        "migration preflight cannot use an unconfigured memory snapshot",
      ));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aeordb.migration-memory-observation.v1\0");
    for value in [
      source_budget_bytes,
      destination_budget_bytes,
      snapshot.accounted_bytes,
      policy.ordinary_limit_bytes(),
      host_available_bytes,
      policy.host_available_floor_bytes,
    ] {
      hash_u64(&mut hasher, value);
    }
    hasher.update(&[memory_pressure_tag(snapshot.pressure)]);
    Ok(Self {
      source_budget_bytes,
      destination_budget_bytes,
      coordinator_accounted_bytes: snapshot.accounted_bytes,
      coordinator_ordinary_limit_bytes: policy.ordinary_limit_bytes(),
      host_available_bytes,
      host_available_floor_bytes: policy.host_available_floor_bytes,
      pressure: snapshot.pressure,
      evidence_digest: *hasher.finalize().as_bytes(),
    })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationConfigurationEvidenceV1 {
  pub generation: u64,
  pub capture_max_bytes: u64,
  pub capture_free_reserve_bytes: u64,
  pub checkpoint_after_seconds: u64,
  pub effective_configuration_fingerprint: Vec<u8>,
}

impl MigrationConfigurationEvidenceV1 {
  pub fn from_run_configuration(configuration: MigrationRunConfiguration, effective_configuration_fingerprint: Vec<u8>) -> Self {
    Self {
      generation: configuration.generation,
      capture_max_bytes: configuration.capture_max_bytes,
      capture_free_reserve_bytes: configuration.capture_free_reserve_bytes,
      checkpoint_after_seconds: configuration.checkpoint_after_seconds,
      effective_configuration_fingerprint,
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationBinaryEvidenceV1 {
  pub source_commit: [u8; 20],
  pub executable_sha256: [u8; 32],
  pub contract_registry_sha256: [u8; 32],
  pub capability_profile: BinaryCapabilityProfileV1,
  pub required_reader_capabilities: CapabilitySetV1,
  pub required_writer_capabilities: CapabilitySetV1,
  pub system_family_registry_fingerprint: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationPreflightRequestV1 {
  pub identity: MigrationIdentityEvidenceV1,
  pub source: MigrationSourceEvidenceV1,
  pub verification: StrictVerificationEvidenceV1,
  pub recovery: MigrationRecoveryEvidenceV1,
  pub inventory: SourceAuthorityInventoryV1,
  pub capacity: [MigrationCapacityObservationV1; 4],
  pub native: MigrationNativeEvidenceV1,
  pub memory: MigrationMemoryEvidenceV1,
  pub configuration: MigrationConfigurationEvidenceV1,
  pub binary: MigrationBinaryEvidenceV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u16)]
pub enum MigrationPreflightFindingCodeV1 {
  InvalidIdentity = 1,
  AmbiguousPathIdentity = 2,
  SourceEvidenceInvalid = 3,
  SourceFrontierMismatch = 4,
  StrictVerificationIncomplete = 5,
  StrictVerificationIssues = 6,
  RecoveryInspectionIncomplete = 7,
  RecoveryStateActive = 8,
  InventoryIncomplete = 9,
  ProtectedStateUnresolved = 10,
  NativeQualificationIncomplete = 11,
  NativeCapabilityUnsupported = 12,
  BinaryIdentityInvalid = 13,
  CapabilityFloorInvalid = 14,
  BinaryCapabilityUnsupported = 15,
  RegistryMismatch = 16,
  ConfigurationInvalid = 17,
  CapacityObservationInvalid = 18,
  CapacityRoleMissingOrDuplicate = 19,
  CapacityVolumeInconsistent = 20,
  CapacityOverflow = 21,
  CapacityInsufficient = 22,
  MemoryObservationInvalid = 23,
  MemoryInsufficient = 24,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationPreflightFindingV1 {
  pub code: MigrationPreflightFindingCodeV1,
}

const EMPTY_FINDING: MigrationPreflightFindingV1 = MigrationPreflightFindingV1 { code: MigrationPreflightFindingCodeV1::InvalidIdentity };

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationPreflightReportV1 {
  evidence_fingerprint: [u8; 32],
  findings: [MigrationPreflightFindingV1; MAX_FINDINGS],
  finding_count: usize,
}

impl MigrationPreflightReportV1 {
  fn new(evidence_fingerprint: [u8; 32]) -> Self {
    Self { evidence_fingerprint, findings: [EMPTY_FINDING; MAX_FINDINGS], finding_count: 0 }
  }

  fn push(&mut self, code: MigrationPreflightFindingCodeV1) {
    if self.findings().iter().any(|finding| finding.code == code) {
      return;
    }
    if self.finding_count < MAX_FINDINGS {
      self.findings[self.finding_count] = MigrationPreflightFindingV1 { code };
      self.finding_count += 1;
    }
  }

  fn finish(&mut self) {
    self.findings[..self.finding_count].sort_by_key(|finding| finding.code);
  }

  pub fn findings(&self) -> &[MigrationPreflightFindingV1] {
    &self.findings[..self.finding_count]
  }

  pub const fn evidence_fingerprint(&self) -> [u8; 32] {
    self.evidence_fingerprint
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationPreflightPermitV1 {
  database_id: [u8; 16],
  migration_id: [u8; 16],
  source_physical_instance_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  hash_algorithm: HashAlgorithm,
  source_header_sequence: u64,
  source_capture_head: Vec<u8>,
  configuration_generation: u64,
  effective_configuration_fingerprint: Vec<u8>,
  system_family_registry_fingerprint: Vec<u8>,
  evidence_fingerprint: [u8; 32],
}

impl MigrationPreflightPermitV1 {
  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn migration_id(&self) -> [u8; 16] {
    self.migration_id
  }

  pub const fn source_physical_instance_id(&self) -> [u8; 16] {
    self.source_physical_instance_id
  }

  pub const fn destination_physical_instance_id(&self) -> [u8; 16] {
    self.destination_physical_instance_id
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn source_header_sequence(&self) -> u64 {
    self.source_header_sequence
  }

  pub fn source_capture_head(&self) -> &[u8] {
    &self.source_capture_head
  }

  pub const fn configuration_generation(&self) -> u64 {
    self.configuration_generation
  }

  pub fn effective_configuration_fingerprint(&self) -> &[u8] {
    &self.effective_configuration_fingerprint
  }

  pub fn system_family_registry_fingerprint(&self) -> &[u8] {
    &self.system_family_registry_fingerprint
  }

  pub const fn evidence_fingerprint(&self) -> [u8; 32] {
    self.evidence_fingerprint
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationPreflightRefusalV1 {
  report: MigrationPreflightReportV1,
}

impl MigrationPreflightRefusalV1 {
  pub const fn report(&self) -> &MigrationPreflightReportV1 {
    &self.report
  }
}

impl Display for MigrationPreflightRefusalV1 {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    write!(formatter, "migration preflight refused by")?;
    for finding in self.report.findings() {
      write!(formatter, " {:?}", finding.code)?;
    }
    Ok(())
  }
}

impl std::error::Error for MigrationPreflightRefusalV1 {}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{code}: {message}")]
pub struct MigrationPreflightObservationErrorV1 {
  code: &'static str,
  message: &'static str,
}

impl MigrationPreflightObservationErrorV1 {
  const fn new(code: &'static str, message: &'static str) -> Self {
    Self { code, message }
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }
}

pub fn evaluate_migration_preflight_v1(request: &MigrationPreflightRequestV1) -> MigrationPreflightReportV1 {
  let mut report = MigrationPreflightReportV1::new(evidence_fingerprint(request));
  validate_identity(request, &mut report);
  validate_source_frontier(request, &mut report);
  validate_verification(request, &mut report);
  validate_recovery(request, &mut report);
  validate_inventory_and_registries(request, &mut report);
  validate_native(request, &mut report);
  validate_binary(request, &mut report);
  validate_configuration(request, &mut report);
  validate_capacity(request, &mut report);
  validate_memory(request, &mut report);
  report.finish();
  report
}

pub fn admit_migration_preflight_v1(
  request: &MigrationPreflightRequestV1,
) -> Result<(MigrationPreflightReportV1, MigrationPreflightPermitV1), MigrationPreflightRefusalV1> {
  let report = evaluate_migration_preflight_v1(request);
  if !report.findings().is_empty() {
    return Err(MigrationPreflightRefusalV1 { report });
  }
  let permit = MigrationPreflightPermitV1 {
    database_id: request.identity.database_id,
    migration_id: request.identity.migration_id,
    source_physical_instance_id: request.identity.source_physical_instance_id,
    destination_physical_instance_id: request.identity.destination_physical_instance_id,
    hash_algorithm: request.source.hash_algorithm,
    source_header_sequence: request.source.selected_header_sequence,
    source_capture_head: request.source.head_hash.clone(),
    configuration_generation: request.configuration.generation,
    effective_configuration_fingerprint: request.configuration.effective_configuration_fingerprint.clone(),
    system_family_registry_fingerprint: request.inventory.system_family_registry_fingerprint.clone(),
    evidence_fingerprint: report.evidence_fingerprint,
  };
  Ok((report, permit))
}

fn validate_identity(request: &MigrationPreflightRequestV1, report: &mut MigrationPreflightReportV1) {
  let identity = &request.identity;
  if [identity.database_id, identity.migration_id, identity.source_physical_instance_id, identity.destination_physical_instance_id]
    .iter()
    .any(|value| all_zero(value))
    || identity.source_physical_instance_id == identity.destination_physical_instance_id
    || !valid_platform_identity(identity.source_file_identity)
    || !valid_platform_identity(identity.destination_parent_identity)
  {
    report.push(MigrationPreflightFindingCodeV1::InvalidIdentity);
  }
  if all_zero(&identity.source_path_digest)
    || all_zero(&identity.destination_path_digest)
    || identity.source_path_digest == identity.destination_path_digest
  {
    report.push(MigrationPreflightFindingCodeV1::AmbiguousPathIdentity);
  }
}

fn validate_source_frontier(request: &MigrationPreflightRequestV1, report: &mut MigrationPreflightReportV1) {
  let source = &request.source;
  if source.file_size == 0
    || source.selected_header_slot > 1
    || source.selected_header_sequence == 0
    || all_zero(&source.complete_file_checksum)
    || all_zero(&source.selected_header_digest)
    || source.head_hash.len() != source.hash_algorithm.hash_length()
  {
    report.push(MigrationPreflightFindingCodeV1::SourceEvidenceInvalid);
  }
  if request.verification.source_file_size != source.file_size
    || request.verification.source_header_sequence != source.selected_header_sequence
    || request.verification.source_complete_file_checksum != source.complete_file_checksum
    || request.recovery.source_header_sequence != source.selected_header_sequence
    || request.inventory.source_header_sequence != source.selected_header_sequence
  {
    report.push(MigrationPreflightFindingCodeV1::SourceFrontierMismatch);
  }
}

fn validate_verification(request: &MigrationPreflightRequestV1, report: &mut MigrationPreflightReportV1) {
  let verification = request.verification;
  if all_zero(&verification.evidence_digest) {
    report.push(MigrationPreflightFindingCodeV1::StrictVerificationIncomplete);
  }
  match verification.state {
    StrictVerificationStateV1::CompleteClean if verification.issue_count == 0 => {}
    StrictVerificationStateV1::CompleteClean => report.push(MigrationPreflightFindingCodeV1::StrictVerificationIssues),
    StrictVerificationStateV1::CompleteWithIssues => report.push(MigrationPreflightFindingCodeV1::StrictVerificationIssues),
    StrictVerificationStateV1::Incomplete => report.push(MigrationPreflightFindingCodeV1::StrictVerificationIncomplete),
  }
}

fn validate_recovery(request: &MigrationPreflightRequestV1, report: &mut MigrationPreflightReportV1) {
  let recovery = request.recovery;
  if !recovery.inspection_complete || all_zero(&recovery.evidence_digest) {
    report.push(MigrationPreflightFindingCodeV1::RecoveryInspectionIncomplete);
  }
  if recovery.durability_latched
    || recovery.repair_active
    || recovery.external_spill_count != 0
    || recovery.repair_ticket_count != 0
    || recovery.path_latch_count != 0
  {
    report.push(MigrationPreflightFindingCodeV1::RecoveryStateActive);
  }
}

fn validate_inventory_and_registries(request: &MigrationPreflightRequestV1, report: &mut MigrationPreflightReportV1) {
  if !request.inventory.complete || all_zero(&request.inventory.authority_digest) {
    report.push(MigrationPreflightFindingCodeV1::InventoryIncomplete);
  }
  if request.inventory.unresolved_family_count != 0 {
    report.push(MigrationPreflightFindingCodeV1::ProtectedStateUnresolved);
  }
  match embedded_system_family_registry(request.source.hash_algorithm) {
    Ok(registry)
      if registry.operational_fingerprint == request.inventory.system_family_registry_fingerprint
        && registry.operational_fingerprint == request.binary.system_family_registry_fingerprint => {}
    _ => report.push(MigrationPreflightFindingCodeV1::RegistryMismatch),
  }
}

fn validate_native(request: &MigrationPreflightRequestV1, report: &mut MigrationPreflightReportV1) {
  if !request.native.source.is_complete() || !request.native.destination.is_complete() {
    report.push(MigrationPreflightFindingCodeV1::NativeQualificationIncomplete);
  }
  if !request.native.source.is_supported() || !request.native.destination.is_supported() {
    report.push(MigrationPreflightFindingCodeV1::NativeCapabilityUnsupported);
  }
}

fn validate_binary(request: &MigrationPreflightRequestV1, report: &mut MigrationPreflightReportV1) {
  let binary = &request.binary;
  if all_zero(&binary.source_commit) || all_zero(&binary.executable_sha256) {
    report.push(MigrationPreflightFindingCodeV1::BinaryIdentityInvalid);
  }
  if expected_contract_registry_sha256().is_none_or(|expected| expected != binary.contract_registry_sha256) {
    report.push(MigrationPreflightFindingCodeV1::RegistryMismatch);
  }
  let baseline = CapabilitySetV1::v4_baseline();
  if !baseline.difference(binary.required_reader_capabilities).is_empty()
    || !baseline.difference(binary.required_writer_capabilities).is_empty()
  {
    report.push(MigrationPreflightFindingCodeV1::CapabilityFloorInvalid);
  }
  let missing_reader = binary.required_reader_capabilities.difference(binary.capability_profile.supported_reader_capabilities);
  let writer_floor = binary.required_reader_capabilities.union(binary.required_writer_capabilities);
  let missing_writer = writer_floor.difference(binary.capability_profile.supported_writer_capabilities);
  if !missing_reader.is_empty() || !missing_writer.is_empty() {
    report.push(MigrationPreflightFindingCodeV1::BinaryCapabilityUnsupported);
  }
}

fn validate_configuration(request: &MigrationPreflightRequestV1, report: &mut MigrationPreflightReportV1) {
  let configuration = &request.configuration;
  if configuration.generation == 0
    || !(GIB..=MAX_CAPTURE_BYTES).contains(&configuration.capture_max_bytes)
    || configuration.capture_free_reserve_bytes < GIB
    || !(30..=3_600).contains(&configuration.checkpoint_after_seconds)
    || configuration.effective_configuration_fingerprint.len() != request.source.hash_algorithm.hash_length()
    || all_zero(&configuration.effective_configuration_fingerprint)
  {
    report.push(MigrationPreflightFindingCodeV1::ConfigurationInvalid);
  }
  if let Some(capture) = unique_capacity_role(&request.capacity, CapacityRoleV1::Capture) {
    if capture.required_bytes < configuration.capture_max_bytes
      || capture.minimum_remaining_bytes < configuration.capture_free_reserve_bytes
      || configuration.capture_free_reserve_bytes > capture.filesystem_capacity_bytes / 2
    {
      report.push(MigrationPreflightFindingCodeV1::ConfigurationInvalid);
    }
  }
  for role in [CapacityRoleV1::Destination, CapacityRoleV1::Backup] {
    if unique_capacity_role(&request.capacity, role).is_some_and(|capacity| capacity.required_bytes < request.source.file_size) {
      report.push(MigrationPreflightFindingCodeV1::ConfigurationInvalid);
    }
  }
}

fn validate_capacity(request: &MigrationPreflightRequestV1, report: &mut MigrationPreflightReportV1) {
  for role in CapacityRoleV1::ALL {
    if unique_capacity_role(&request.capacity, role).is_none() {
      report.push(MigrationPreflightFindingCodeV1::CapacityRoleMissingOrDuplicate);
    }
  }
  for observation in request.capacity {
    if all_zero(&observation.volume_identity)
      || !valid_platform_identity(observation.path_identity)
      || observation.path_identity.volume_identity != observation.volume_identity
      || observation.filesystem_capacity_bytes == 0
      || observation.available_bytes > observation.filesystem_capacity_bytes
      || observation.required_bytes == 0
      || observation.minimum_remaining_bytes > observation.filesystem_capacity_bytes / 2
    {
      report.push(MigrationPreflightFindingCodeV1::CapacityObservationInvalid);
    }
  }
  if unique_capacity_role(&request.capacity, CapacityRoleV1::Destination)
    .is_some_and(|capacity| !capacity.path_identity.represents_same_physical_file_as(request.identity.destination_parent_identity))
  {
    report.push(MigrationPreflightFindingCodeV1::AmbiguousPathIdentity);
  }

  let mut sorted = request.capacity;
  sorted.sort_by_key(|observation| (observation.volume_identity, observation.role));
  for index in 0..sorted.len() {
    if index != 0 && sorted[index - 1].volume_identity == sorted[index].volume_identity {
      continue;
    }
    let volume = sorted[index].volume_identity;
    let capacity = sorted[index].filesystem_capacity_bytes;
    let available = sorted[index].available_bytes;
    let mut required = 0u64;
    let mut minimum_remaining = 0u64;
    let mut overflowed = false;
    for observation in sorted.iter().filter(|observation| observation.volume_identity == volume) {
      if observation.filesystem_capacity_bytes != capacity || observation.available_bytes != available {
        report.push(MigrationPreflightFindingCodeV1::CapacityVolumeInconsistent);
      }
      match required.checked_add(observation.required_bytes) {
        Some(value) => required = value,
        None => overflowed = true,
      }
      minimum_remaining = minimum_remaining.max(observation.minimum_remaining_bytes);
    }
    if overflowed {
      report.push(MigrationPreflightFindingCodeV1::CapacityOverflow);
    } else if available.checked_sub(required).is_none_or(|remaining| remaining < minimum_remaining) {
      report.push(MigrationPreflightFindingCodeV1::CapacityInsufficient);
    }
  }
}

fn validate_memory(request: &MigrationPreflightRequestV1, report: &mut MigrationPreflightReportV1) {
  let memory = request.memory;
  if memory.source_budget_bytes == 0
    || memory.destination_budget_bytes == 0
    || memory.coordinator_ordinary_limit_bytes == 0
    || memory.host_available_floor_bytes == 0
    || all_zero(&memory.evidence_digest)
  {
    report.push(MigrationPreflightFindingCodeV1::MemoryObservationInvalid);
  }
  if memory.pressure == MemoryPressure::Unconfigured {
    report.push(MigrationPreflightFindingCodeV1::MemoryObservationInvalid);
  } else if memory.pressure != MemoryPressure::Normal {
    report.push(MigrationPreflightFindingCodeV1::MemoryInsufficient);
  }
  let requested = memory.source_budget_bytes.checked_add(memory.destination_budget_bytes);
  let projected = requested.and_then(|requested| memory.coordinator_accounted_bytes.checked_add(requested));
  let host_required = requested.and_then(|requested| requested.checked_add(memory.host_available_floor_bytes));
  if requested.is_none() || projected.is_none() || host_required.is_none() {
    report.push(MigrationPreflightFindingCodeV1::MemoryObservationInvalid);
  } else if projected.is_some_and(|projected| projected > memory.coordinator_ordinary_limit_bytes)
    || host_required.is_some_and(|required| required > memory.host_available_bytes)
  {
    report.push(MigrationPreflightFindingCodeV1::MemoryInsufficient);
  }
}

fn unique_capacity_role(
  observations: &[MigrationCapacityObservationV1; 4],
  role: CapacityRoleV1,
) -> Option<&MigrationCapacityObservationV1> {
  let mut matching = observations.iter().filter(|observation| observation.role == role);
  let first = matching.next()?;
  if matching.next().is_some() {
    None
  } else {
    Some(first)
  }
}

fn evidence_fingerprint(request: &MigrationPreflightRequestV1) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new();
  hasher.update(EVIDENCE_DOMAIN_V1);
  let identity = request.identity;
  for value in
    [&identity.database_id, &identity.migration_id, &identity.source_physical_instance_id, &identity.destination_physical_instance_id]
  {
    hasher.update(value);
  }
  hasher.update(&identity.source_path_digest);
  hasher.update(&identity.destination_path_digest);
  hasher.update(&identity.source_file_identity.to_bytes());
  hasher.update(&identity.destination_parent_identity.to_bytes());

  hasher.update(&request.source.hash_algorithm.to_u16().to_le_bytes());
  hash_u64(&mut hasher, request.source.file_size);
  hasher.update(&request.source.complete_file_checksum);
  hasher.update(&[request.source.selected_header_slot]);
  hash_u64(&mut hasher, request.source.selected_header_sequence);
  hasher.update(&request.source.selected_header_digest);
  hash_len_bytes(&mut hasher, &request.source.head_hash);

  hasher.update(&[verification_state_tag(request.verification.state)]);
  hash_u64(&mut hasher, request.verification.source_file_size);
  hash_u64(&mut hasher, request.verification.source_header_sequence);
  hasher.update(&request.verification.source_complete_file_checksum);
  hash_u64(&mut hasher, request.verification.issue_count);
  hasher.update(&request.verification.evidence_digest);

  hasher.update(&[u8::from(request.recovery.inspection_complete)]);
  hash_u64(&mut hasher, request.recovery.source_header_sequence);
  hasher.update(&[u8::from(request.recovery.durability_latched), u8::from(request.recovery.repair_active)]);
  for value in [request.recovery.external_spill_count, request.recovery.repair_ticket_count, request.recovery.path_latch_count] {
    hash_u64(&mut hasher, value);
  }
  hasher.update(&request.recovery.evidence_digest);

  hasher.update(&[u8::from(request.inventory.complete)]);
  hash_u64(&mut hasher, request.inventory.source_header_sequence);
  hash_u64(&mut hasher, request.inventory.unresolved_family_count);
  for value in inventory_counts(request.inventory.counts) {
    hash_u64(&mut hasher, value);
  }
  hasher.update(&request.inventory.authority_digest);
  hash_len_bytes(&mut hasher, &request.inventory.system_family_registry_fingerprint);

  let mut capacity = request.capacity;
  capacity.sort_by_key(|observation| {
    (
      observation.role,
      observation.volume_identity,
      observation.path_identity.to_bytes(),
      observation.filesystem_capacity_bytes,
      observation.available_bytes,
      observation.required_bytes,
      observation.minimum_remaining_bytes,
    )
  });
  for observation in capacity {
    hasher.update(&[observation.role as u8]);
    hasher.update(&observation.volume_identity);
    hasher.update(&observation.path_identity.to_bytes());
    for value in
      [observation.filesystem_capacity_bytes, observation.available_bytes, observation.required_bytes, observation.minimum_remaining_bytes]
    {
      hash_u64(&mut hasher, value);
    }
  }

  for native in [request.native.source, request.native.destination] {
    hasher.update(&[
      u8::from(native.data_barrier),
      u8::from(native.file_barrier),
      u8::from(native.parent_directory_sync),
      u8::from(native.durable_replace),
      u8::from(native.preallocation),
      u8::from(native.stable_file_identity),
      u8::from(native.read_back_verified),
    ]);
    hasher.update(&native.qualification_digest);
  }

  for value in [
    request.memory.source_budget_bytes,
    request.memory.destination_budget_bytes,
    request.memory.coordinator_accounted_bytes,
    request.memory.coordinator_ordinary_limit_bytes,
    request.memory.host_available_bytes,
    request.memory.host_available_floor_bytes,
  ] {
    hash_u64(&mut hasher, value);
  }
  hasher.update(&request.memory.evidence_digest);
  hasher.update(&[memory_pressure_tag(request.memory.pressure)]);

  for value in [
    request.configuration.generation,
    request.configuration.capture_max_bytes,
    request.configuration.capture_free_reserve_bytes,
    request.configuration.checkpoint_after_seconds,
  ] {
    hash_u64(&mut hasher, value);
  }
  hash_len_bytes(&mut hasher, &request.configuration.effective_configuration_fingerprint);

  hasher.update(&request.binary.source_commit);
  hasher.update(&request.binary.executable_sha256);
  hasher.update(&request.binary.contract_registry_sha256);
  hasher.update(&request.binary.capability_profile.supported_reader_capabilities.into_bytes());
  hasher.update(&request.binary.capability_profile.supported_writer_capabilities.into_bytes());
  hasher.update(&request.binary.required_reader_capabilities.into_bytes());
  hasher.update(&request.binary.required_writer_capabilities.into_bytes());
  hash_len_bytes(&mut hasher, &request.binary.system_family_registry_fingerprint);
  *hasher.finalize().as_bytes()
}

fn verification_issue_count(report: &VerifyReport) -> u64 {
  let scalar = report
    .corrupt_hash
    .saturating_add(report.corrupt_header)
    .saturating_add(report.stale_kv_entries)
    .saturating_add(report.missing_kv_entries);
  [
    report.missing_children.len(),
    report.unlisted_files.len(),
    report.dangling_file_records.len(),
    report.btree_directory_issues.len(),
    report.invalid_kv_offsets.len(),
    report.invalid_hot_tail_voids.len(),
    report.verification_errors.len(),
    report.broken_snapshots.len(),
    report.stale_dir_path_keys.len(),
  ]
  .into_iter()
  .fold(scalar, |total, count| total.saturating_add(usize_to_u64(count)))
}

fn verification_evidence_digest(report: &VerifyReport) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new();
  hasher.update(b"aeordb.strict-verification-report.v1\0");
  hash_len_bytes(&mut hasher, report.db_path.as_bytes());
  hash_len_bytes(&mut hasher, report.hash_algorithm.as_bytes());
  for value in [
    report.file_size,
    report.total_entries,
    report.chunks,
    report.file_records,
    report.directory_indexes,
    report.symlinks,
    report.snapshots,
    report.deletion_records,
    report.forks,
    report.voids,
    report.void_bytes,
    report.logical_data_size,
    report.retained_logical_data_size,
    report.non_head_retained_logical_data_size,
    report.retained_file_versions,
    report.file_record_payload_size,
    report.chunk_data_size,
    report.dedup_savings,
    report.valid_entries,
    report.corrupt_hash,
    report.corrupt_header,
    report.directories_checked,
    report.kv_entries,
    report.stale_kv_entries,
    report.missing_kv_entries,
    report.snapshots_checked,
    verification_issue_count(report),
  ] {
    hash_u64(&mut hasher, value);
  }
  hash_u64(&mut hasher, usize_to_u64(report.skipped_regions.len()));
  for (offset, length) in &report.skipped_regions {
    hash_u64(&mut hasher, *offset);
    hash_u64(&mut hasher, *length);
  }
  for values in [
    &report.missing_children,
    &report.unlisted_files,
    &report.dangling_file_records,
    &report.btree_directory_issues,
    &report.stale_kv_details,
    &report.missing_kv_details,
    &report.invalid_kv_offsets,
    &report.invalid_hot_tail_voids,
    &report.verification_errors,
    &report.broken_snapshots,
    &report.stale_dir_path_keys,
    &report.repairs,
  ] {
    hash_u64(&mut hasher, usize_to_u64(values.len()));
    for value in values {
      hash_len_bytes(&mut hasher, value.as_bytes());
    }
  }
  hash_u64(&mut hasher, usize_to_u64(report.btree_directory_issue_details.len()));
  for detail in &report.btree_directory_issue_details {
    hash_len_bytes(&mut hasher, detail.path.as_bytes());
    match &detail.node_hash {
      Some(node_hash) => {
        hasher.update(&[1]);
        hash_len_bytes(&mut hasher, node_hash.as_bytes());
      }
      None => {
        hasher.update(&[0]);
      }
    }
    hash_len_bytes(&mut hasher, detail.reason.as_bytes());
  }
  *hasher.finalize().as_bytes()
}

fn inventory_counts(counts: AuthorityInventoryCountsV1) -> [u64; 11] {
  [
    counts.protected_families,
    counts.modules,
    counts.snapshots,
    counts.forks,
    counts.symlinks,
    counts.history_roots,
    counts.peers,
    counts.sync_states,
    counts.tasks,
    counts.plugins,
    counts.roots,
  ]
}

fn valid_platform_identity(identity: PlatformFileIdentityDescriptorV1) -> bool {
  matches!(identity.platform, 1 | 2)
    && identity.schema == 1
    && identity.flags & !(1 << 1) == 0
    && !all_zero(&identity.volume_identity)
    && !all_zero(&identity.file_identity)
    && if identity.flags & (1 << 1) != 0 { !all_zero(&identity.birth_identity) } else { all_zero(&identity.birth_identity) }
}

fn is_supported(support: &NativeOperationSupport) -> bool {
  matches!(support, NativeOperationSupport::Supported)
}

fn hash_support(hasher: &mut blake3::Hasher, support: &NativeOperationSupport) {
  match support {
    NativeOperationSupport::Supported => {
      hasher.update(&[1]);
    }
    NativeOperationSupport::Unsupported { reason } => {
      hasher.update(&[0]);
      hash_len_bytes(hasher, reason.as_bytes());
    }
  }
}

fn mechanism_tag(mechanism: NativeDurabilityMechanism) -> u8 {
  match mechanism {
    NativeDurabilityMechanism::UnixFdatasync => 1,
    NativeDurabilityMechanism::UnixFsync => 2,
    NativeDurabilityMechanism::AppleBarrierFsync => 3,
    NativeDurabilityMechanism::AppleFullFsync => 4,
    NativeDurabilityMechanism::AppleFsyncFallback => 5,
    NativeDurabilityMechanism::WindowsFlushFileBuffers => 6,
    NativeDurabilityMechanism::WindowsDirectoryFlushFileBuffers => 7,
    NativeDurabilityMechanism::UnixRenameAndDirectoryFsync => 8,
    NativeDurabilityMechanism::WindowsReplaceFileAndFlush => 9,
    NativeDurabilityMechanism::WindowsMoveFileExWriteThrough => 10,
  }
}

fn verification_state_tag(state: StrictVerificationStateV1) -> u8 {
  match state {
    StrictVerificationStateV1::CompleteClean => 1,
    StrictVerificationStateV1::CompleteWithIssues => 2,
    StrictVerificationStateV1::Incomplete => 3,
  }
}

fn memory_pressure_tag(pressure: MemoryPressure) -> u8 {
  match pressure {
    MemoryPressure::Unconfigured => 0,
    MemoryPressure::Normal => 1,
    MemoryPressure::Soft => 2,
    MemoryPressure::Hard => 3,
  }
}

fn expected_contract_registry_sha256() -> Option<[u8; 32]> {
  let encoded = CONTRACT_REGISTRY_SHA256.as_bytes();
  if encoded.len() != 64 {
    return None;
  }
  let mut digest = [0u8; 32];
  for index in 0..digest.len() {
    digest[index] = hex_nibble(encoded[index * 2])?.checked_mul(16)?.checked_add(hex_nibble(encoded[index * 2 + 1])?)?;
  }
  Some(digest)
}

fn hex_nibble(value: u8) -> Option<u8> {
  match value {
    b'0'..=b'9' => Some(value - b'0'),
    b'a'..=b'f' => Some(value - b'a' + 10),
    b'A'..=b'F' => Some(value - b'A' + 10),
    _ => None,
  }
}

fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
  hasher.update(&value.to_le_bytes());
}

fn hash_len_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
  hash_u64(hasher, usize_to_u64(value.len()));
  hasher.update(value);
}

fn usize_to_u64(value: usize) -> u64 {
  const { assert!(usize::BITS <= u64::BITS) };
  value as u64
}

fn all_zero(value: &[u8]) -> bool {
  value.iter().all(|byte| *byte == 0)
}
