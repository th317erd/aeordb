//! Bounded conversion of optional migration notices into immutable AINX.
//!
//! Persistence and AMPR selection remain owned by the migration capture
//! workspace and migration state owner. This module never acknowledges source
//! writes and cannot make their success depend on capture.

use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::coverage_journal::{
  CoverageJournalEncodeOptionsV1, CoverageJournalErrorV1, CoverageJournalWindowOptionsV1, CoverageJournalWindowOutcomeV1,
  CoverageRebuildReasonV1, encode_owned_soft_mutation_journal_segment, order_soft_mutation_window,
};
use super::coverage_runtime::{
  CoverageAuthorityV1, CoverageBoundaryV1, SoftMutationDrainV1, SoftMutationHubErrorV1, SoftMutationHubOptionsV1, SoftMutationHubV1,
  SoftMutationNoticeV1,
};
use super::gc_retirement::RetirementJournalOwnerV1;
use super::hash::digest_parts;
use super::index_task::JournalOwnerKindV1;
use super::migration_capture_subscription::{
  MigrationCaptureSubscriptionErrorV1, MigrationCaptureSubscriptionIdentityV1, MigrationCaptureSubscriptionOwnerV1,
  MigrationCaptureSubscriptionV1,
};
use super::migration_capture_workspace::{
  DurableMigrationCaptureWorkspaceV1, MIGRATION_CAPTURE_SEGMENT_MAX_BYTES_V1, MigrationCaptureWorkspaceBasisV1,
  MigrationCaptureWorkspaceErrorV1, MigrationCaptureWorkspaceIdentityV1, MigrationCaptureWorkspaceOptionsV1,
  MigrationCaptureWorkspaceReopenOptionsV1, ReopenedMigrationCaptureWorkspaceV1,
};
use super::migration_owner::{
  MigrationCaptureCheckpointPublicationRequestV1, MigrationFullReconciliationLatchRequestV1, MigrationStateOwnerErrorV1,
  MigrationStateOwnerV1,
};
use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::storage_engine::StorageEngine;

const MAXIMUM_CHECKPOINT_INTERVAL_MS: u64 = 300_000;
const FAILURE_EVIDENCE_DOMAIN: &[u8] = b"aeordb.migration-capture-runtime-failure.v1\0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureDrainPlanV1 {
  hash_algorithm: HashAlgorithm,
  migration_id: [u8; 16],
  capture_generation: u64,
  segment_ordinal: u64,
  runtime_boot_id: [u8; 16],
  covered_publication_sequence: u64,
  covered_source_root: Vec<u8>,
  previous_segment: Vec<u8>,
  maximum_notices: usize,
  maximum_retained_bytes: usize,
}

impl MigrationCaptureDrainPlanV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    hash_algorithm: HashAlgorithm,
    migration_id: [u8; 16],
    capture_generation: u64,
    segment_ordinal: u64,
    runtime_boot_id: [u8; 16],
    covered_publication_sequence: u64,
    covered_source_root: Vec<u8>,
    previous_segment: Vec<u8>,
    maximum_notices: usize,
    maximum_retained_bytes: usize,
  ) -> Result<Self, MigrationCaptureRuntimeErrorV1> {
    let hash_width = hash_algorithm.hash_length();
    if migration_id == [0; 16]
      || runtime_boot_id == [0; 16]
      || capture_generation == 0
      || segment_ordinal == 0
      || maximum_notices == 0
      || maximum_retained_bytes == 0
      || covered_source_root.len() != hash_width
      || covered_source_root.iter().all(|byte| *byte == 0)
      || previous_segment.len() != hash_width
    {
      return Err(MigrationCaptureRuntimeErrorV1::InvalidPlan);
    }
    Ok(Self {
      hash_algorithm,
      migration_id,
      capture_generation,
      segment_ordinal,
      runtime_boot_id,
      covered_publication_sequence,
      covered_source_root,
      previous_segment,
      maximum_notices,
      maximum_retained_bytes,
    })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationCaptureInexactReasonV1 {
  InvalidNotice,
  ConflictingOperation,
  AuthorityDiscontinuity,
  PublicationGap,
  WindowLimitExceeded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedMigrationCaptureSegmentV1 {
  bytes: Vec<u8>,
  identity: Vec<u8>,
  captured_through_publication_sequence: u64,
  source_root_after: Vec<u8>,
}

impl PreparedMigrationCaptureSegmentV1 {
  pub fn bytes(&self) -> &[u8] {
    &self.bytes
  }

  pub fn identity(&self) -> &[u8] {
    &self.identity
  }

  pub const fn captured_through_publication_sequence(&self) -> u64 {
    self.captured_through_publication_sequence
  }

  pub fn source_root_after(&self) -> &[u8] {
    &self.source_root_after
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationCaptureDrainOutcomeV1 {
  Empty,
  Exact(PreparedMigrationCaptureSegmentV1),
  FullReconciliationRequired(MigrationCaptureInexactReasonV1),
}

pub fn prepare_migration_capture_drain(
  drain: SoftMutationDrainV1,
  plan: &MigrationCaptureDrainPlanV1,
) -> Result<MigrationCaptureDrainOutcomeV1, MigrationCaptureRuntimeErrorV1> {
  if drain.notices.len() > plan.maximum_notices || drain.retained_bytes > plan.maximum_retained_bytes {
    return Ok(MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::WindowLimitExceeded));
  }
  let retained_bytes = drain
    .notices
    .iter()
    .try_fold(0usize, |total, notice| total.checked_add(notice.retained_bytes()))
    .ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  if retained_bytes != drain.retained_bytes {
    return Err(MigrationCaptureRuntimeErrorV1::DrainAccounting);
  }
  if drain.notices.is_empty() {
    return Ok(MigrationCaptureDrainOutcomeV1::Empty);
  }

  let selected = drain
    .notices
    .iter()
    .max_by(|left, right| {
      left.publication_sequence.cmp(&right.publication_sequence).then_with(|| left.operation_id.cmp(&right.operation_id))
    })
    .ok_or(MigrationCaptureRuntimeErrorV1::DrainAccounting)?;
  if selected.namespace_root.len() != plan.hash_algorithm.hash_length() || selected.namespace_root.iter().all(|byte| *byte == 0) {
    return Ok(MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::InvalidNotice));
  }
  let covered_authority =
    CoverageAuthorityV1::new(plan.hash_algorithm, plan.covered_source_root.clone(), Vec::new()).map_err(CoverageJournalErrorV1::Runtime)?;
  let selected_authority =
    CoverageAuthorityV1::new(plan.hash_algorithm, selected.namespace_root.clone(), Vec::new()).map_err(CoverageJournalErrorV1::Runtime)?;
  let covered = CoverageBoundaryV1 { authority: covered_authority, publication_sequence: plan.covered_publication_sequence };
  let selected = CoverageBoundaryV1 { authority: selected_authority, publication_sequence: selected.publication_sequence };
  let window = match order_soft_mutation_window(
    plan.hash_algorithm,
    drain.notices,
    &covered,
    &selected,
    CoverageJournalWindowOptionsV1::new(plan.maximum_notices, plan.maximum_retained_bytes)?,
  ) {
    CoverageJournalWindowOutcomeV1::Exact(window) => window,
    CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason } | CoverageJournalWindowOutcomeV1::RebuildRequired(reason) => {
      return Ok(MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(map_inexact_reason(reason)));
    }
  };

  let mut expected_sequence = plan.covered_publication_sequence.checked_add(1).ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  for notice in window.notices() {
    if notice.publication_sequence != expected_sequence {
      return Ok(MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(MigrationCaptureInexactReasonV1::PublicationGap));
    }
    expected_sequence = expected_sequence.checked_add(1).ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  }

  let encoded = encode_owned_soft_mutation_journal_segment(
    plan.hash_algorithm,
    &window,
    plan.migration_id,
    JournalOwnerKindV1::Task,
    window.root_after(),
    CoverageJournalEncodeOptionsV1 {
      generation: plan.capture_generation,
      segment_ordinal: plan.segment_ordinal,
      previous_segment: plan.previous_segment.clone(),
      runtime_boot_id: plan.runtime_boot_id,
    },
  )?;
  Ok(MigrationCaptureDrainOutcomeV1::Exact(PreparedMigrationCaptureSegmentV1 {
    bytes: encoded.value,
    identity: encoded.key,
    captured_through_publication_sequence: selected.publication_sequence,
    source_root_after: selected.authority.source_namespace_root,
  }))
}

fn map_inexact_reason(reason: CoverageRebuildReasonV1) -> MigrationCaptureInexactReasonV1 {
  match reason {
    CoverageRebuildReasonV1::InvalidNotice => MigrationCaptureInexactReasonV1::InvalidNotice,
    CoverageRebuildReasonV1::ConflictingMutation => MigrationCaptureInexactReasonV1::ConflictingOperation,
    CoverageRebuildReasonV1::WindowLimitExceeded | CoverageRebuildReasonV1::JournalLimitExceeded => {
      MigrationCaptureInexactReasonV1::WindowLimitExceeded
    }
    _ => MigrationCaptureInexactReasonV1::AuthorityDiscontinuity,
  }
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationCaptureRuntimeErrorV1 {
  #[error("migration capture drain plan is invalid")]
  InvalidPlan,
  #[error("migration capture drain accounting overflowed")]
  AccountingOverflow,
  #[error("migration capture drain retained-byte accounting is inconsistent")]
  DrainAccounting,
  #[error("migration capture journal preparation failed: {0}")]
  Journal(#[from] CoverageJournalErrorV1),
  #[error("migration capture runtime options are invalid: {0}")]
  InvalidOptions(&'static str),
  #[error("migration capture runtime clock is invalid")]
  InvalidClock,
  #[error("migration capture runtime memory admission failed: {0}")]
  Memory(#[source] Box<MemoryCoordinatorError>),
  #[error("migration capture subscription failed: {0}")]
  Subscription(#[from] MigrationCaptureSubscriptionErrorV1),
  #[error("migration capture workspace failed: {0}")]
  Workspace(#[from] MigrationCaptureWorkspaceErrorV1),
  #[error("migration capture progress authority failed: {0}")]
  StateOwner(#[source] Box<MigrationStateOwnerErrorV1>),
  #[error("migration capture runtime state refuses the operation: {0}")]
  State(&'static str),
}

impl MigrationCaptureRuntimeErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::InvalidPlan => "migration_capture_runtime_plan",
      Self::AccountingOverflow => "migration_capture_runtime_overflow",
      Self::DrainAccounting => "migration_capture_runtime_drain_accounting",
      Self::Journal(_) => "migration_capture_runtime_journal",
      Self::InvalidOptions(_) => "migration_capture_runtime_options",
      Self::InvalidClock => "migration_capture_runtime_clock",
      Self::Memory(_) => "migration_capture_runtime_memory",
      Self::Subscription(error) => error.code(),
      Self::Workspace(error) => error.code(),
      Self::StateOwner(error) => error.code(),
      Self::State(_) => "migration_capture_runtime_state",
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MigrationCaptureRuntimeClockV1 {
  pub updated_at_ms: i64,
  pub publication_timestamp_ms: u64,
  pub monotonic_now_ms: u64,
}

impl MigrationCaptureRuntimeClockV1 {
  pub fn new(updated_at_ms: i64, publication_timestamp_ms: u64, monotonic_now_ms: u64) -> Result<Self, MigrationCaptureRuntimeErrorV1> {
    if updated_at_ms < 0
      || publication_timestamp_ms == 0
      || publication_timestamp_ms == u64::MAX
      || monotonic_now_ms == 0
      || monotonic_now_ms == u64::MAX
    {
      return Err(MigrationCaptureRuntimeErrorV1::InvalidClock);
    }
    Ok(Self { updated_at_ms, publication_timestamp_ms, monotonic_now_ms })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureRuntimeOptionsV1 {
  capture_generation: u64,
  registration_id: [u8; 16],
  hub: SoftMutationHubOptionsV1,
  maximum_drain_notices: usize,
  maximum_drain_bytes: usize,
  checkpoint_interval_ms: u64,
  workspace: MigrationCaptureWorkspaceOptionsV1,
}

impl MigrationCaptureRuntimeOptionsV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    capture_generation: u64,
    registration_id: [u8; 16],
    hub: SoftMutationHubOptionsV1,
    maximum_drain_notices: usize,
    maximum_drain_bytes: usize,
    checkpoint_interval_ms: u64,
    workspace: MigrationCaptureWorkspaceOptionsV1,
  ) -> Result<Self, MigrationCaptureRuntimeErrorV1> {
    SoftMutationHubOptionsV1::new(hub.maximum_notices, hub.maximum_retained_bytes, hub.maximum_notice_bytes)
      .map_err(|_| MigrationCaptureRuntimeErrorV1::InvalidOptions("soft mutation hub limits are invalid"))?;
    if capture_generation == 0 || registration_id == [0; 16] {
      return Err(MigrationCaptureRuntimeErrorV1::InvalidOptions("capture generation and registration identity must be nonzero"));
    }
    if maximum_drain_notices == 0
      || maximum_drain_notices > hub.maximum_notices
      || maximum_drain_bytes == 0
      || maximum_drain_bytes > hub.maximum_retained_bytes
    {
      return Err(MigrationCaptureRuntimeErrorV1::InvalidOptions("drain limits must be nonzero and no wider than the reserved hub"));
    }
    if checkpoint_interval_ms == 0 || checkpoint_interval_ms > MAXIMUM_CHECKPOINT_INTERVAL_MS {
      return Err(MigrationCaptureRuntimeErrorV1::InvalidOptions("checkpoint interval must be within 1..=300000 milliseconds"));
    }
    Ok(Self { capture_generation, registration_id, hub, maximum_drain_notices, maximum_drain_bytes, checkpoint_interval_ms, workspace })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationCaptureRuntimeStateV1 {
  Capturing,
  NeedsFullReconcile,
  Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureRuntimeStatusV1 {
  pub state: MigrationCaptureRuntimeStateV1,
  pub starting_publication_sequence: u64,
  pub captured_through_publication_sequence: u64,
  pub checkpoint_sequence: u64,
  pub selected_checkpoint_artifact: Vec<u8>,
  pub queue_reservation_bytes: u64,
  pub failure_code: Option<&'static str>,
  pub failure_evidence: Vec<u8>,
  pub durable_reconciliation_latched: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureRecoveryRequestV1 {
  workspace_path: PathBuf,
  identity: MigrationCaptureWorkspaceIdentityV1,
  basis: MigrationCaptureWorkspaceBasisV1,
  maximum_stored_bytes: u64,
}

impl MigrationCaptureRecoveryRequestV1 {
  pub fn new(
    workspace_path: PathBuf,
    identity: MigrationCaptureWorkspaceIdentityV1,
    basis: MigrationCaptureWorkspaceBasisV1,
    maximum_stored_bytes: u64,
  ) -> Result<Self, MigrationCaptureRuntimeErrorV1> {
    if !workspace_path.is_absolute() || maximum_stored_bytes == 0 {
      return Err(MigrationCaptureRuntimeErrorV1::InvalidOptions("capture recovery path must be absolute and its stored-byte cap nonzero"));
    }
    Ok(Self { workspace_path, identity, basis, maximum_stored_bytes })
  }

  pub fn workspace_path(&self) -> &Path {
    &self.workspace_path
  }
}

#[derive(Debug)]
pub struct RecoveredMigrationCaptureV1 {
  workspace: Option<ReopenedMigrationCaptureWorkspaceV1>,
  needs_full_reconciliation: bool,
  durable_reconciliation_latched: bool,
  failure_code: Option<&'static str>,
}

impl RecoveredMigrationCaptureV1 {
  pub const fn workspace(&self) -> Option<&ReopenedMigrationCaptureWorkspaceV1> {
    self.workspace.as_ref()
  }

  pub const fn needs_full_reconciliation(&self) -> bool {
    self.needs_full_reconciliation
  }

  pub const fn durable_reconciliation_latched(&self) -> bool {
    self.durable_reconciliation_latched
  }

  pub const fn failure_code(&self) -> Option<&'static str> {
    self.failure_code
  }
}

pub struct MigrationCaptureRuntimeV1 {
  source: Arc<StorageEngine>,
  owner: Arc<MigrationStateOwnerV1>,
  hash_algorithm: HashAlgorithm,
  migration_id: [u8; 16],
  holder_boot_id: [u8; 16],
  fencing_token: u64,
  capture_generation: u64,
  maximum_drain_notices: usize,
  maximum_drain_bytes: usize,
  checkpoint_interval_ms: u64,
  next_checkpoint_monotonic_ms: u64,
  last_clock: MigrationCaptureRuntimeClockV1,
  subscription_owner: Option<MigrationCaptureSubscriptionOwnerV1>,
  subscription: Option<Arc<MigrationCaptureSubscriptionV1>>,
  workspace: Option<DurableMigrationCaptureWorkspaceV1>,
  queue_memory: Option<MemoryReservation>,
  recovery_request: Option<MigrationCaptureRecoveryRequestV1>,
  status: MigrationCaptureRuntimeStatusV1,
}

impl MigrationCaptureRuntimeV1 {
  pub fn start(
    source: Arc<StorageEngine>,
    owner: Arc<MigrationStateOwnerV1>,
    options: MigrationCaptureRuntimeOptionsV1,
    cancellation: CancellationToken,
    clock: MigrationCaptureRuntimeClockV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<Self, MigrationCaptureRuntimeErrorV1> {
    let next_checkpoint_monotonic_ms =
      clock.monotonic_now_ms.checked_add(options.checkpoint_interval_ms).ok_or(MigrationCaptureRuntimeErrorV1::InvalidClock)?;
    let hash_algorithm = owner.hash_algorithm();
    let migration_id = owner.migration_id();
    let holder_boot_id = owner.holder_boot_id();
    let fencing_token = owner.fencing_token();
    let zero = vec![0; hash_algorithm.hash_length()];
    let mut runtime = Self {
      source,
      owner,
      hash_algorithm,
      migration_id,
      holder_boot_id,
      fencing_token,
      capture_generation: options.capture_generation,
      maximum_drain_notices: options.maximum_drain_notices,
      maximum_drain_bytes: options.maximum_drain_bytes,
      checkpoint_interval_ms: options.checkpoint_interval_ms,
      next_checkpoint_monotonic_ms,
      last_clock: clock,
      subscription_owner: None,
      subscription: None,
      workspace: None,
      queue_memory: None,
      recovery_request: None,
      status: MigrationCaptureRuntimeStatusV1 {
        state: MigrationCaptureRuntimeStateV1::Capturing,
        starting_publication_sequence: 0,
        captured_through_publication_sequence: 0,
        checkpoint_sequence: 0,
        selected_checkpoint_artifact: zero.clone(),
        queue_reservation_bytes: 0,
        failure_code: None,
        failure_evidence: zero,
        durable_reconciliation_latched: false,
      },
    };

    let queue_bytes =
      modeled_runtime_memory_bytes(&options.hub, options.maximum_drain_notices, options.maximum_drain_bytes, hash_algorithm);
    let queue_bytes = match queue_bytes {
      Ok(bytes) => bytes,
      Err(error) => {
        runtime.fail_capture(clock, error.code(), retirement_owner);
        return Ok(runtime);
      }
    };
    let reservation = match runtime.source.memory_coordinator().reserve(MemoryOwner::Migration, queue_bytes, AdmissionClass::Maintenance) {
      Ok(reservation) => reservation,
      Err(error) => {
        tracing::warn!(%error, "Optional migration capture memory admission failed; exact reconciliation is required");
        runtime.fail_capture(clock, "migration_capture_runtime_memory", retirement_owner);
        return Ok(runtime);
      }
    };
    runtime.status.queue_reservation_bytes = reservation.bytes();
    runtime.queue_memory = Some(reservation);

    let hub = match SoftMutationHubV1::new(options.hub) {
      Ok(hub) => Arc::new(hub),
      Err(error) => {
        tracing::warn!(%error, "Optional migration capture queue allocation failed; exact reconciliation is required");
        runtime.fail_capture(clock, hub_error_code(&error), retirement_owner);
        return Ok(runtime);
      }
    };
    let subscription_identity = match MigrationCaptureSubscriptionIdentityV1::new(
      runtime.migration_id,
      runtime.holder_boot_id,
      runtime.fencing_token,
      options.registration_id,
    ) {
      Ok(identity) => identity,
      Err(error) => {
        runtime.fail_capture(clock, error.code(), retirement_owner);
        return Ok(runtime);
      }
    };
    let (subscription_owner, subscription) =
      match MigrationCaptureSubscriptionOwnerV1::register(&runtime.source, subscription_identity, hub) {
        Ok(registered) => registered,
        Err(error) => {
          runtime.fail_capture(clock, error.code(), retirement_owner);
          return Ok(runtime);
        }
      };
    runtime.status.starting_publication_sequence = subscription.boundary().publication_sequence;
    runtime.status.captured_through_publication_sequence = subscription.boundary().publication_sequence;
    runtime.subscription_owner = Some(subscription_owner);
    runtime.subscription = Some(Arc::clone(&subscription));

    let identity = match MigrationCaptureWorkspaceIdentityV1::new(
      runtime.owner.database_id(),
      runtime.owner.migration_id(),
      runtime.owner.source_physical_instance_id(),
      runtime.owner.destination_physical_instance_id(),
      runtime.owner.holder_boot_id(),
      runtime.owner.fencing_token(),
      options.capture_generation,
      hash_algorithm,
    ) {
      Ok(identity) => identity,
      Err(error) => {
        tracing::error!(%error, "Optional migration capture workspace identity was rejected");
        runtime.fail_capture(clock, error.code(), retirement_owner);
        return Ok(runtime);
      }
    };
    let basis = match MigrationCaptureWorkspaceBasisV1::new(
      clock.updated_at_ms,
      subscription.boundary().publication_sequence,
      subscription.boundary().source_namespace_root.clone(),
      runtime.owner.effective_configuration_fingerprint().to_vec(),
      runtime.owner.system_family_registry_fingerprint().to_vec(),
      runtime.owner.source_authority_digest(),
    ) {
      Ok(basis) => basis,
      Err(error) => {
        tracing::error!(%error, "Optional migration capture workspace basis was rejected");
        runtime.fail_capture(clock, error.code(), retirement_owner);
        return Ok(runtime);
      }
    };
    let maximum_stored_bytes = options.workspace.maximum_stored_bytes();
    match DurableMigrationCaptureWorkspaceV1::create(
      runtime.source.database_path(),
      identity,
      basis.clone(),
      options.workspace,
      cancellation,
      &runtime.source.memory_coordinator(),
    ) {
      Ok(workspace) => {
        let workspace_path = workspace.workspace_path().to_path_buf();
        runtime.workspace = Some(workspace);
        match MigrationCaptureRecoveryRequestV1::new(workspace_path, identity, basis, maximum_stored_bytes) {
          Ok(request) => runtime.recovery_request = Some(request),
          Err(error) => {
            runtime.fail_capture(clock, error.code(), retirement_owner);
            return Ok(runtime);
          }
        }
      }
      Err(error) => {
        tracing::warn!(%error, "Optional migration capture workspace creation failed; exact reconciliation is required");
        runtime.fail_capture(clock, error.code(), retirement_owner);
        return Ok(runtime);
      }
    }
    if let Err(error) = runtime.publish_checkpoint(clock, retirement_owner) {
      let code = error.code();
      runtime.fail_capture(clock, code, retirement_owner);
    }
    Ok(runtime)
  }

  pub const fn status(&self) -> &MigrationCaptureRuntimeStatusV1 {
    &self.status
  }

  pub fn recovery_request(&self) -> Option<MigrationCaptureRecoveryRequestV1> {
    self.recovery_request.clone()
  }

  pub fn recover_selected(
    owner: Arc<MigrationStateOwnerV1>,
    request: MigrationCaptureRecoveryRequestV1,
    cancellation: CancellationToken,
    clock: MigrationCaptureRuntimeClockV1,
    memory: &MemoryCoordinator,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<RecoveredMigrationCaptureV1, MigrationCaptureRuntimeErrorV1> {
    validate_recovery_binding(&owner, &request)?;
    let selected = owner
      .observe_capture_state(clock.updated_at_ms, clock.publication_timestamp_ms, clock.monotonic_now_ms)
      .map_err(|error| MigrationCaptureRuntimeErrorV1::StateOwner(Box::new(error)))?;
    if selected.checkpoint_artifact.iter().all(|byte| *byte == 0) {
      let code = "migration_capture_recovery_checkpoint_missing";
      let durable =
        if selected.needs_full_reconciliation { true } else { latch_recovery_failure(&owner, &request, clock, code, retirement_owner) };
      return Ok(RecoveredMigrationCaptureV1 {
        workspace: None,
        needs_full_reconciliation: true,
        durable_reconciliation_latched: durable,
        failure_code: Some(code),
      });
    }

    let reopened = match ReopenedMigrationCaptureWorkspaceV1::open_selected(
      &request.workspace_path,
      &selected.checkpoint_artifact,
      request.identity,
      request.basis.clone(),
      MigrationCaptureWorkspaceReopenOptionsV1::new(request.maximum_stored_bytes)?,
      cancellation,
      memory,
    ) {
      Ok(reopened) => reopened,
      Err(error) => {
        tracing::error!(%error, "AMPR-selected migration capture checkpoint could not be reopened");
        let code = error.code();
        let durable =
          if selected.needs_full_reconciliation { true } else { latch_recovery_failure(&owner, &request, clock, code, retirement_owner) };
        return Ok(RecoveredMigrationCaptureV1 {
          workspace: None,
          needs_full_reconciliation: true,
          durable_reconciliation_latched: durable,
          failure_code: Some(code),
        });
      }
    };
    let failure_code =
      if request.identity.runtime_boot_id() != owner.holder_boot_id() || request.identity.fencing_token() != owner.fencing_token() {
        Some("migration_capture_recovery_boot_changed")
      } else if reopened.has_unselected_tail() {
        Some("migration_capture_recovery_unselected_tail")
      } else if reopened.captured_through_publication_sequence() != selected.captured_through_publication_sequence {
        Some("migration_capture_recovery_watermark_mismatch")
      } else {
        None
      };
    let needs_full_reconciliation = selected.needs_full_reconciliation || failure_code.is_some();
    let durable_reconciliation_latched = if selected.needs_full_reconciliation {
      true
    } else if let Some(code) = failure_code {
      latch_recovery_failure(&owner, &request, clock, code, retirement_owner)
    } else {
      false
    };
    Ok(RecoveredMigrationCaptureV1 { workspace: Some(reopened), needs_full_reconciliation, durable_reconciliation_latched, failure_code })
  }

  pub fn poll(
    &mut self,
    clock: MigrationCaptureRuntimeClockV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<(), MigrationCaptureRuntimeErrorV1> {
    if self.status.state != MigrationCaptureRuntimeStateV1::Capturing {
      return Ok(());
    }
    if !self.accept_clock(clock) {
      self.fail_capture(clock, "migration_capture_runtime_clock_regression", retirement_owner);
      return Ok(());
    }
    if let Err(error) = self.drain_once() {
      let code = error.code();
      self.fail_capture(clock, code, retirement_owner);
      return Ok(());
    }
    if let Some(subscription) = &self.subscription {
      match subscription.snapshot() {
        Ok(snapshot) if !snapshot.reconciliation_required && !snapshot.admission_closed => {}
        Ok(_) => {
          self.fail_capture(clock, "migration_capture_runtime_subscription_inexact", retirement_owner);
          return Ok(());
        }
        Err(error) => {
          let code = hub_error_code(&error);
          self.fail_capture(clock, code, retirement_owner);
          return Ok(());
        }
      }
    }
    if clock.monotonic_now_ms >= self.next_checkpoint_monotonic_ms {
      if let Err(error) = self.publish_checkpoint(clock, retirement_owner) {
        let code = error.code();
        self.fail_capture(clock, code, retirement_owner);
      }
    }
    Ok(())
  }

  pub fn stop(
    &mut self,
    clock: MigrationCaptureRuntimeClockV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<(), MigrationCaptureRuntimeErrorV1> {
    if self.status.state != MigrationCaptureRuntimeStateV1::Capturing {
      return Ok(());
    }
    if !self.accept_clock(clock) {
      self.fail_capture(clock, "migration_capture_runtime_clock_regression", retirement_owner);
      return Ok(());
    }
    let retired = match self.unregister() {
      Ok(retired) => retired,
      Err(error) => {
        let code = error.code();
        self.fail_capture(clock, code, retirement_owner);
        return Ok(());
      }
    };
    if retired.close_error().is_some() {
      drop(retired);
      self.release_queue_memory();
      self.fail_capture(clock, "migration_capture_runtime_close", retirement_owner);
      return Ok(());
    }
    self.subscription = Some(Arc::clone(retired.subscription()));
    loop {
      match self.drain_once() {
        Ok(true) => continue,
        Ok(false) => break,
        Err(error) => {
          drop(retired);
          self.subscription.take();
          self.release_queue_memory();
          let code = error.code();
          self.fail_capture(clock, code, retirement_owner);
          return Ok(());
        }
      }
    }
    drop(retired);
    self.subscription.take();
    if let Err(error) = self.publish_checkpoint(clock, retirement_owner) {
      self.release_queue_memory();
      let code = error.code();
      self.fail_capture(clock, code, retirement_owner);
      return Ok(());
    }
    self.release_queue_memory();
    self.status.state = MigrationCaptureRuntimeStateV1::Stopped;
    Ok(())
  }

  fn drain_once(&mut self) -> Result<bool, MigrationCaptureRuntimeErrorV1> {
    let subscription = self.subscription.as_ref().ok_or(MigrationCaptureRuntimeErrorV1::State("capture subscription is unavailable"))?;
    let snapshot = subscription
      .snapshot()
      .map_err(|error| MigrationCaptureRuntimeErrorV1::Subscription(MigrationCaptureSubscriptionErrorV1::Hub(error)))?;
    if snapshot.reconciliation_required {
      return Err(MigrationCaptureRuntimeErrorV1::State("capture subscription reports lost source publications"));
    }
    let drain = subscription
      .try_drain(self.maximum_drain_notices, self.maximum_drain_bytes)
      .map_err(|error| MigrationCaptureRuntimeErrorV1::Subscription(MigrationCaptureSubscriptionErrorV1::Hub(error)))?;
    let drained = !drain.notices.is_empty();
    let workspace = self.workspace.as_mut().ok_or(MigrationCaptureRuntimeErrorV1::State("capture workspace is unavailable"))?;
    let summary = workspace.summary();
    let plan = MigrationCaptureDrainPlanV1::new(
      self.hash_algorithm,
      self.migration_id,
      self.capture_generation,
      summary.last_segment_ordinal().checked_add(1).ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?,
      self.holder_boot_id,
      summary.captured_through_publication_sequence(),
      summary.source_root_after().to_vec(),
      summary.segment_head().to_vec(),
      self.maximum_drain_notices,
      self.maximum_drain_bytes,
    )?;
    match prepare_migration_capture_drain(drain, &plan)? {
      MigrationCaptureDrainOutcomeV1::Empty => Ok(false),
      MigrationCaptureDrainOutcomeV1::Exact(segment) => {
        workspace.append_segment(segment.bytes())?;
        self.status.captured_through_publication_sequence = segment.captured_through_publication_sequence();
        Ok(drained)
      }
      MigrationCaptureDrainOutcomeV1::FullReconciliationRequired(_) => {
        Err(MigrationCaptureRuntimeErrorV1::State("capture drain is not an exact publication/root chain"))
      }
    }
  }

  fn publish_checkpoint(
    &mut self,
    clock: MigrationCaptureRuntimeClockV1,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) -> Result<(), MigrationCaptureRuntimeErrorV1> {
    let workspace = self.workspace.as_mut().ok_or(MigrationCaptureRuntimeErrorV1::State("capture workspace is unavailable"))?;
    let manifest = workspace.prepare_capturing_checkpoint(clock.updated_at_ms)?;
    let durable = workspace.publish_checkpoint(&manifest)?;
    self
      .owner
      .publish_capture_checkpoint(
        MigrationCaptureCheckpointPublicationRequestV1 {
          captured_through_publication_sequence: manifest.captured_through_publication_sequence,
          checkpoint_artifact: durable.manifest_identity().to_vec(),
          updated_at_ms: clock.updated_at_ms,
          publication_timestamp_ms: clock.publication_timestamp_ms,
          monotonic_now_ms: clock.monotonic_now_ms,
        },
        retirement_owner,
      )
      .map_err(|error| MigrationCaptureRuntimeErrorV1::StateOwner(Box::new(error)))?;
    self.status.captured_through_publication_sequence = manifest.captured_through_publication_sequence;
    self.status.checkpoint_sequence = durable.checkpoint_sequence();
    self.status.selected_checkpoint_artifact = durable.manifest_identity().to_vec();
    self.next_checkpoint_monotonic_ms =
      clock.monotonic_now_ms.checked_add(self.checkpoint_interval_ms).ok_or(MigrationCaptureRuntimeErrorV1::InvalidClock)?;
    Ok(())
  }

  fn fail_capture(
    &mut self,
    clock: MigrationCaptureRuntimeClockV1,
    failure_code: &'static str,
    retirement_owner: &mut RetirementJournalOwnerV1,
  ) {
    self.status.state = MigrationCaptureRuntimeStateV1::NeedsFullReconcile;
    self.status.failure_code = Some(failure_code);
    self.status.failure_evidence =
      failure_evidence(self.hash_algorithm, self.migration_id, self.status.captured_through_publication_sequence, failure_code);
    self.deactivate_after_failure();
    let request = MigrationFullReconciliationLatchRequestV1 {
      last_error_evidence: self.status.failure_evidence.clone(),
      updated_at_ms: clock.updated_at_ms,
      publication_timestamp_ms: clock.publication_timestamp_ms,
      monotonic_now_ms: clock.monotonic_now_ms,
    };
    self.status.durable_reconciliation_latched = match self.owner.latch_needs_full_reconciliation(request, retirement_owner) {
      Ok(_) => true,
      Err(error) => {
        tracing::error!(code = error.code(), %error, failure_code, "Failed to persist the optional migration capture reconciliation latch");
        match self.owner.observe_capture_state(clock.updated_at_ms, clock.publication_timestamp_ms, clock.monotonic_now_ms) {
          Ok(observed) => observed.needs_full_reconciliation,
          Err(observe_error) => {
            tracing::error!(
              code = observe_error.code(),
              %observe_error,
              failure_code,
              "Failed to observe migration progress after reconciliation latch failure"
            );
            false
          }
        }
      }
    };
  }

  fn unregister(
    &mut self,
  ) -> Result<super::migration_capture_subscription::RetiredMigrationCaptureSubscriptionV1, MigrationCaptureSubscriptionErrorV1> {
    let owner = self.subscription_owner.as_ref().ok_or(MigrationCaptureSubscriptionErrorV1::NotRegistered)?;
    let retired = owner.unregister(&self.source)?;
    self.subscription_owner.take();
    self.subscription.take();
    Ok(retired)
  }

  fn deactivate_after_failure(&mut self) {
    if self.subscription_owner.is_some() {
      match self.unregister() {
        Ok(retired) => {
          drop(retired);
          self.release_queue_memory();
          return;
        }
        Err(error) => {
          tracing::error!(code = error.code(), %error, "Failed to unregister stopped optional migration capture");
          if let Some(subscription) = &self.subscription {
            subscription.force_reconciliation_required(self.status.captured_through_publication_sequence);
            if let Err(close_error) = subscription.close_admission() {
              tracing::error!(%close_error, "Failed to close stopped optional migration capture admission");
            }
          }
          return;
        }
      }
    }
    self.subscription.take();
    self.release_queue_memory();
  }

  fn release_queue_memory(&mut self) {
    self.status.queue_reservation_bytes = 0;
    if let Some(reservation) = self.queue_memory.take() {
      if let Err(error) = reservation.release() {
        tracing::error!(%error, "Failed to release migration capture queue reservation");
      }
    }
  }

  fn accept_clock(&mut self, clock: MigrationCaptureRuntimeClockV1) -> bool {
    if clock.updated_at_ms < self.last_clock.updated_at_ms
      || clock.publication_timestamp_ms < self.last_clock.publication_timestamp_ms
      || clock.monotonic_now_ms < self.last_clock.monotonic_now_ms
    {
      return false;
    }
    self.last_clock = clock;
    true
  }
}

impl Drop for MigrationCaptureRuntimeV1 {
  fn drop(&mut self) {
    if self.subscription_owner.is_some() {
      match self.unregister() {
        Ok(retired) => drop(retired),
        Err(error) => {
          tracing::error!(code = error.code(), %error, "Migration capture drop could not unregister its source subscription");
          if let Some(subscription) = &self.subscription {
            subscription.force_reconciliation_required(self.status.captured_through_publication_sequence);
            if let Err(close_error) = subscription.close_admission() {
              tracing::error!(%close_error, "Migration capture drop could not close source admission");
            }
          }
          if let Some(reservation) = self.queue_memory.take() {
            std::mem::forget(reservation);
          }
          return;
        }
      }
    }
    self.release_queue_memory();
  }
}

fn modeled_runtime_memory_bytes(
  hub: &SoftMutationHubOptionsV1,
  maximum_drain_notices: usize,
  maximum_drain_bytes: usize,
  hash_algorithm: HashAlgorithm,
) -> Result<u64, MigrationCaptureRuntimeErrorV1> {
  let queue_slots = hub
    .maximum_notices
    .checked_add(maximum_drain_notices)
    .and_then(|slots| slots.checked_mul(size_of::<SoftMutationNoticeV1>()))
    .ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  let working_bytes = maximum_drain_bytes.checked_mul(2).ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  let encoded_bytes = MIGRATION_CAPTURE_SEGMENT_MAX_BYTES_V1.checked_mul(2).ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  let mutation_hashes =
    maximum_drain_notices.checked_mul(hash_algorithm.hash_length()).ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  let total = hub
    .maximum_retained_bytes
    .checked_add(queue_slots)
    .and_then(|value| value.checked_add(working_bytes))
    .and_then(|value| value.checked_add(encoded_bytes))
    .and_then(|value| value.checked_add(mutation_hashes))
    .ok_or(MigrationCaptureRuntimeErrorV1::AccountingOverflow)?;
  u64::try_from(total).map_err(|_| MigrationCaptureRuntimeErrorV1::AccountingOverflow)
}

fn validate_recovery_binding(
  owner: &MigrationStateOwnerV1,
  request: &MigrationCaptureRecoveryRequestV1,
) -> Result<(), MigrationCaptureRuntimeErrorV1> {
  if request.identity.database_id() != owner.database_id()
    || request.identity.migration_id() != owner.migration_id()
    || request.identity.source_physical_instance_id() != owner.source_physical_instance_id()
    || request.identity.destination_physical_instance_id() != owner.destination_physical_instance_id()
    || request.identity.hash_algorithm() != owner.hash_algorithm()
    || request.basis.effective_config_fingerprint() != owner.effective_configuration_fingerprint()
    || request.basis.system_family_registry_fingerprint() != owner.system_family_registry_fingerprint()
    || request.basis.source_authority_digest() != owner.source_authority_digest()
  {
    return Err(MigrationCaptureRuntimeErrorV1::State("capture recovery request does not match the selected migration authority"));
  }
  Ok(())
}

fn latch_recovery_failure(
  owner: &MigrationStateOwnerV1,
  request: &MigrationCaptureRecoveryRequestV1,
  clock: MigrationCaptureRuntimeClockV1,
  failure_code: &'static str,
  retirement_owner: &mut RetirementJournalOwnerV1,
) -> bool {
  let evidence =
    failure_evidence(owner.hash_algorithm(), owner.migration_id(), request.basis.starting_publication_sequence(), failure_code);
  match owner.latch_needs_full_reconciliation(
    MigrationFullReconciliationLatchRequestV1 {
      last_error_evidence: evidence,
      updated_at_ms: clock.updated_at_ms,
      publication_timestamp_ms: clock.publication_timestamp_ms,
      monotonic_now_ms: clock.monotonic_now_ms,
    },
    retirement_owner,
  ) {
    Ok(_) => true,
    Err(error) => {
      tracing::error!(code = error.code(), %error, failure_code, "Failed to persist the recovered migration capture reconciliation latch");
      match owner.observe_capture_state(clock.updated_at_ms, clock.publication_timestamp_ms, clock.monotonic_now_ms) {
        Ok(observed) => observed.needs_full_reconciliation,
        Err(observe_error) => {
          tracing::error!(
            code = observe_error.code(),
            %observe_error,
            failure_code,
            "Failed to observe migration progress after recovered reconciliation latch failure"
          );
          false
        }
      }
    }
  }
}

fn failure_evidence(
  hash_algorithm: HashAlgorithm,
  migration_id: [u8; 16],
  captured_through_publication_sequence: u64,
  failure_code: &'static str,
) -> Vec<u8> {
  digest_parts(
    hash_algorithm,
    &[FAILURE_EVIDENCE_DOMAIN, &migration_id, &captured_through_publication_sequence.to_le_bytes(), failure_code.as_bytes()],
  )
}

fn hub_error_code(error: &SoftMutationHubErrorV1) -> &'static str {
  match error {
    SoftMutationHubErrorV1::InvalidOptions(_) => "migration_capture_runtime_hub_options",
    SoftMutationHubErrorV1::Allocation(_) => "migration_capture_runtime_hub_allocation",
    SoftMutationHubErrorV1::QueueUnavailable => "migration_capture_runtime_hub_unavailable",
    SoftMutationHubErrorV1::QueueContended => "migration_capture_runtime_hub_contended",
    SoftMutationHubErrorV1::ArithmeticOverflow => "migration_capture_runtime_hub_overflow",
    SoftMutationHubErrorV1::DrainLimitTooSmall { .. } => "migration_capture_runtime_hub_drain_limit",
  }
}
