//! Native source/shadow binding and bounded v4 index-runtime recovery.
//!
//! The v3 source remains query authority. This module only installs one
//! recovered shadow owner after proving that the open source file and selected
//! v4 destination are the exact pair admitted by migration preflight.

use std::mem::size_of;
use std::sync::Arc;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinatorError, MemoryOwner, MemoryReservation};
use crate::engine::native_durability::{NativeDurabilityError, PlatformFileIdentityDescriptorV1, platform_file_identity};
use crate::engine::{EngineError, HashAlgorithm, StorageEngine, VirtualClock};

use super::first_authority::{FirstAuthorityPublicationErrorV1, SelectedSemanticAuthorityV1, V4FirstAuthorityPublisher};
use super::index_coordinator_recovery::{
  IndexRecoveryErrorV1, IndexRecoveryOptionsV1, IndexRecoveryOutcomeV1, IndexRecoveryOwnerV1, IndexRecoveryReasonV1,
  IndexRecoveryStoreErrorV1, IndexRecoveryStoreV1,
};
use super::index_recovery_store::{
  IndexScopeOrdinalStoreRegistryErrorV1, IndexScopeOrdinalStoreRegistryOptionsV1, IndexScopeOrdinalStoreRegistryV1,
  NativeIndexOperationDescriptorV1, NativeIndexRecoveryStoreV1, SharedRetirementJournalOwnerV1,
};
use super::index_runtime_batch_publisher::{IndexRuntimeBatchPublisherBuildErrorV1, NativeIndexRuntimeBatchPublisherV1};
use super::index_runtime_cadence::{IndexRuntimeCadenceErrorV1, NativeIndexRuntimeCadenceV1};
use super::index_producer_admission::{
  IndexProducerMaintenanceAdmissionErrorV1, IndexProducerMaintenanceClassV1, IndexProducerMaintenanceIntentV1, build_maintenance_task,
};
use super::index_producer_coordinator::IndexProducerAdmissionV1;
use super::index_runtime_dirty_overlay_recovery::{
  IndexRuntimeDirtyOverlayRecoveryErrorV1, IndexRuntimeDirtyOverlayRecoveryOutcomeV1, recover_index_runtime_dirty_overlay_v1,
};
use super::index_runtime_owner::{
  IndexRuntimeErrorV1, IndexRuntimeLifecycleV1, IndexRuntimeOwnerOptionsV1, IndexRuntimeOwnerV1, IndexRuntimeRecoveryDecisionV1,
};
use super::index_runtime_workspace_store::{
  DurableIndexRuntimeWorkspaceV1, IndexRuntimeWorkspaceIdentityV1, IndexRuntimeWorkspaceOptionsV1, IndexRuntimeWorkspaceStoreErrorV1,
};
use super::migration_owner::MigrationStateOwnerV1;
use super::migration_preflight::MigrationPreflightPermitV1;
use super::namespace::SemanticAvailabilityV1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexRuntimeShadowIdentityV1 {
  database_id: [u8; 16],
  migration_id: [u8; 16],
  source_physical_instance_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  source_file_identity: PlatformFileIdentityDescriptorV1,
  hash_algorithm: HashAlgorithm,
  system_family_registry_fingerprint: Vec<u8>,
}

impl IndexRuntimeShadowIdentityV1 {
  pub fn from_preflight(permit: &MigrationPreflightPermitV1) -> Self {
    Self {
      database_id: permit.database_id(),
      migration_id: permit.migration_id(),
      source_physical_instance_id: permit.source_physical_instance_id(),
      destination_physical_instance_id: permit.destination_physical_instance_id(),
      source_file_identity: permit.source_file_identity(),
      hash_algorithm: permit.hash_algorithm(),
      system_family_registry_fingerprint: permit.system_family_registry_fingerprint().to_vec(),
    }
  }

  pub fn from_migration_owner(owner: &MigrationStateOwnerV1) -> Self {
    Self {
      database_id: owner.database_id(),
      migration_id: owner.migration_id(),
      source_physical_instance_id: owner.source_physical_instance_id(),
      destination_physical_instance_id: owner.destination_physical_instance_id(),
      source_file_identity: owner.source_file_identity(),
      hash_algorithm: owner.hash_algorithm(),
      system_family_registry_fingerprint: owner.system_family_registry_fingerprint().to_vec(),
    }
  }

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
}

#[derive(Clone, Copy, Debug)]
pub struct IndexRuntimeNativeRecoveryOptionsV1 {
  maximum_operation_descriptors: usize,
  maximum_descriptor_bytes: u64,
  registry: IndexScopeOrdinalStoreRegistryOptionsV1,
  checkpoint: IndexRecoveryOptionsV1,
}

impl IndexRuntimeNativeRecoveryOptionsV1 {
  pub fn new(
    maximum_operation_descriptors: usize,
    maximum_descriptor_bytes: u64,
    registry: IndexScopeOrdinalStoreRegistryOptionsV1,
    checkpoint: IndexRecoveryOptionsV1,
  ) -> Result<Self, NativeIndexRuntimeInstallationErrorV1> {
    if maximum_operation_descriptors == 0 || maximum_descriptor_bytes == 0 {
      return Err(invalid("native_index_recovery_limits", "descriptor count and byte limits must be nonzero"));
    }
    Ok(Self { maximum_operation_descriptors, maximum_descriptor_bytes, registry, checkpoint })
  }
}

#[derive(Clone, Debug)]
pub struct NativeIndexRuntimePublisherOptionsV1 {
  descriptor: NativeIndexOperationDescriptorV1,
  workspace_id: [u8; 16],
  generation: u64,
  workspace: IndexRuntimeWorkspaceOptionsV1,
}

impl NativeIndexRuntimePublisherOptionsV1 {
  pub fn new(
    descriptor: NativeIndexOperationDescriptorV1,
    workspace_id: [u8; 16],
    generation: u64,
    workspace: IndexRuntimeWorkspaceOptionsV1,
  ) -> Result<Self, NativeIndexRuntimeInstallationErrorV1> {
    if workspace_id.iter().all(|byte| *byte == 0) || generation == 0 {
      return Err(invalid(
        "native_index_runtime_publisher_identity",
        "runtime publisher workspace identity and generation must be nonzero",
      ));
    }
    Ok(Self { descriptor, workspace_id, generation, workspace })
  }
}

pub struct NativeIndexRuntimeInstallationRequestV1<'request> {
  pub coordinator_id: [u8; 16],
  pub shadow_identity: &'request IndexRuntimeShadowIdentityV1,
  pub publisher: Arc<V4FirstAuthorityPublisher>,
  pub retirement_owner: SharedRetirementJournalOwnerV1,
  pub operation_descriptors: &'request [NativeIndexOperationDescriptorV1],
  pub runtime_options: IndexRuntimeOwnerOptionsV1,
  pub recovery_options: IndexRuntimeNativeRecoveryOptionsV1,
  pub runtime_publisher: NativeIndexRuntimePublisherOptionsV1,
  pub cancellation: &'request CancellationToken,
  pub clock: Arc<dyn VirtualClock>,
  pub now_ms: u64,
}

pub struct NativeIndexRuntimeV1 {
  owner: Arc<IndexRuntimeOwnerV1>,
  publisher: Arc<V4FirstAuthorityPublisher>,
  registry: Arc<IndexScopeOrdinalStoreRegistryV1>,
  shadow_identity: IndexRuntimeShadowIdentityV1,
  semantic_authority: SelectedSemanticAuthorityV1,
  cancellation: CancellationToken,
  cadence: Arc<NativeIndexRuntimeCadenceV1>,
}

impl NativeIndexRuntimeV1 {
  pub fn owner(&self) -> &Arc<IndexRuntimeOwnerV1> {
    &self.owner
  }

  pub fn publisher(&self) -> &Arc<V4FirstAuthorityPublisher> {
    &self.publisher
  }

  pub fn registry(&self) -> &Arc<IndexScopeOrdinalStoreRegistryV1> {
    &self.registry
  }

  pub const fn shadow_identity(&self) -> &IndexRuntimeShadowIdentityV1 {
    &self.shadow_identity
  }

  pub const fn semantic_authority(&self) -> &SelectedSemanticAuthorityV1 {
    &self.semantic_authority
  }

  pub const fn cancellation(&self) -> &CancellationToken {
    &self.cancellation
  }

  pub fn cadence(&self) -> &Arc<NativeIndexRuntimeCadenceV1> {
    &self.cadence
  }

  pub(crate) fn admit_maintenance_task(
    &self,
    source_operation_id: [u8; 16],
    class: IndexProducerMaintenanceClassV1,
    publication_sequence: u64,
    namespace_root: &[u8],
    scope: &str,
  ) -> Result<IndexProducerAdmissionV1, NativeIndexRuntimeTaskAdmissionErrorV1> {
    let semantic_authority = self.publisher.load_selected_semantic_authority()?;
    validate_selected_semantic_authority_identity(&self.shadow_identity, &semantic_authority)
      .map_err(|(code, message)| NativeIndexRuntimeTaskAdmissionErrorV1::Invalid { code, message: message.to_string() })?;
    let request = build_maintenance_task(
      self.shadow_identity.hash_algorithm,
      IndexProducerMaintenanceIntentV1 {
        source_operation_id,
        class,
        publication_sequence,
        namespace_root,
        semantic_state_root: &semantic_authority.semantic_state.object_id,
        scope,
      },
    )?;
    self.cadence.admit_task(request).map_err(Into::into)
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeIndexRuntimeInstallationReceiptV1 {
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub selected_root_hash: Vec<u8>,
  pub semantic_state_root: Vec<u8>,
  pub root_publication_sequence: u64,
  pub content_only: bool,
  pub lifecycle: IndexRuntimeLifecycleV1,
  pub recovered_scopes: u32,
  pub highest_checkpoint_sequence: u64,
}

#[derive(Debug, Error)]
pub enum NativeIndexRuntimeInstallationErrorV1 {
  #[error("a native index runtime is already installed")]
  AlreadyInstalled,
  #[error("native index runtime installation was canceled")]
  Canceled,
  #[error("native index runtime installation rejected {code}: {message}")]
  Invalid { code: &'static str, message: String },
  #[error("native index runtime source-file identity failed: {0}")]
  SourceIdentity(#[from] NativeDurabilityError),
  #[error("native index runtime source engine is not writable: {0}")]
  SourceEngine(#[from] EngineError),
  #[error("native index runtime selected authority failed: {0}")]
  Authority(#[from] FirstAuthorityPublicationErrorV1),
  #[error("native index runtime memory admission failed: {0}")]
  Memory(#[from] MemoryCoordinatorError),
  #[error("native index runtime registry construction failed: {0}")]
  Registry(#[from] IndexScopeOrdinalStoreRegistryErrorV1),
  #[error("native index runtime publisher recovery owner is invalid: {0}")]
  PublisherRecovery(#[from] IndexRecoveryErrorV1),
  #[error("native index runtime publisher recovery store failed: {0}")]
  PublisherStore(#[from] IndexRecoveryStoreErrorV1),
  #[error("native index runtime dirty-overlay recovery failed: {0}")]
  DirtyOverlayRecovery(#[from] IndexRuntimeDirtyOverlayRecoveryErrorV1),
  #[error("native index runtime workspace failed: {0}")]
  Workspace(#[from] IndexRuntimeWorkspaceStoreErrorV1),
  #[error("native index runtime publisher construction failed: {0}")]
  Publisher(#[from] IndexRuntimeBatchPublisherBuildErrorV1),
  #[error("native index runtime cadence construction failed: {0}")]
  Cadence(#[from] IndexRuntimeCadenceErrorV1),
  #[error("native index runtime owner failed: {0}")]
  Runtime(#[from] IndexRuntimeErrorV1),
}

#[derive(Debug, Error)]
pub enum NativeIndexRuntimeTaskAdmissionErrorV1 {
  #[error("native index maintenance source authority failed: {0}")]
  Source(#[from] EngineError),
  #[error("native index maintenance semantic authority failed: {0}")]
  Authority(#[from] FirstAuthorityPublicationErrorV1),
  #[error("native index maintenance intent failed: {0}")]
  Intent(#[from] IndexProducerMaintenanceAdmissionErrorV1),
  #[error("native index maintenance cadence failed: {0}")]
  Cadence(#[from] IndexRuntimeCadenceErrorV1),
  #[error("native index maintenance runtime authority is invalid: {code}: {message}")]
  Invalid { code: &'static str, message: String },
}

pub fn install_native_index_runtime_v1(
  engine: &StorageEngine,
  request: NativeIndexRuntimeInstallationRequestV1<'_>,
) -> Result<NativeIndexRuntimeInstallationReceiptV1, NativeIndexRuntimeInstallationErrorV1> {
  let installation = match engine.begin_index_runtime_installation_v1() {
    Ok(installation) => installation,
    Err(IndexRuntimeErrorV1::AlreadyInstalled) => return Err(NativeIndexRuntimeInstallationErrorV1::AlreadyInstalled),
    Err(error) => return Err(error.into()),
  };
  check_cancellation(request.cancellation)?;
  engine.ensure_writable()?;
  validate_source_and_shadow(engine, request.shadow_identity, &request.publisher)?;
  let semantic_authority = request.publisher.load_selected_semantic_authority()?;
  validate_selected_semantic_authority(request.shadow_identity, &semantic_authority)?;
  let descriptor_order =
    validate_descriptors(engine, request.shadow_identity, request.operation_descriptors, request.recovery_options, request.cancellation)?;
  validate_runtime_publisher_descriptor(
    request.shadow_identity,
    request.operation_descriptors,
    &request.runtime_publisher.descriptor,
    request.recovery_options,
  )?;
  check_cancellation(request.cancellation)?;

  let owner = installation.prepare_owner(request.coordinator_id, request.runtime_options, request.now_ms)?;
  let memory = engine.memory_coordinator();
  let registry = Arc::new(IndexScopeOrdinalStoreRegistryV1::new(
    request.recovery_options.registry,
    request.shadow_identity.hash_algorithm,
    request.shadow_identity.database_id,
    request.recovery_options.checkpoint,
    Arc::clone(&request.publisher),
    Arc::clone(&request.retirement_owner),
    Arc::clone(&memory),
    request.cancellation.clone(),
    Arc::clone(&request.clock),
  )?);

  let content_only = matches!(semantic_authority.semantic_state.availability, SemanticAvailabilityV1::ContentOnly { .. });
  let mut recovery = recover_selected_operations(
    &registry,
    request.operation_descriptors,
    descriptor_order.indices(),
    &semantic_authority,
    request.cancellation,
  )?;
  let (runtime_publisher, resumed_dirty_overlay) = build_runtime_publisher(
    engine,
    request.coordinator_id,
    request.shadow_identity,
    &semantic_authority,
    &request.runtime_publisher,
    Arc::clone(&request.publisher),
    Arc::clone(&request.retirement_owner),
    request.cancellation,
    Arc::clone(&request.clock),
    request.now_ms,
  )?;
  let expected_runtime_checkpoint = runtime_publisher.selected_checkpoint().cloned();
  if resumed_dirty_overlay && matches!(recovery, IndexRuntimeRecoveryDecisionV1::Ready { .. }) {
    recovery = IndexRuntimeRecoveryDecisionV1::ReconciliationRequired {
      code: "native_index_dirty_overlay_requires_reconciliation",
      context: "a selected no-journal dirty overlay was recovered; authoritative reconciliation is required before it can become coverage"
        .to_string(),
    };
  }
  let cadence = Arc::new(NativeIndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    runtime_publisher,
    request.cancellation.clone(),
    Arc::clone(&request.clock),
  )?);
  let source_authority_guard = engine.direct_hard_authority_guard()?;
  let semantic_authority_guard = request.publisher.selected_semantic_authority_guard()?;
  let current_semantic_authority = semantic_authority_guard.load()?;
  validate_selected_semantic_authority(request.shadow_identity, &current_semantic_authority)?;
  let current_runtime_control = semantic_authority_guard.load_index_operation_control(
    &request.shadow_identity.database_id,
    request.runtime_publisher.descriptor.index_id(),
    &request.runtime_publisher.descriptor.operation_id(),
  )?;
  validate_runtime_publisher_selection(expected_runtime_checkpoint.as_ref(), current_runtime_control.as_ref())?;
  validate_final_installation_frontier(
    request.cancellation,
    &semantic_authority.root_hash,
    &semantic_authority.semantic_state.object_id,
    semantic_authority.root_publication_sequence,
    &current_semantic_authority.root_hash,
    &current_semantic_authority.semantic_state.object_id,
    current_semantic_authority.root_publication_sequence,
  )?;
  owner.complete_recovery(recovery)?;
  let runtime = Arc::new(NativeIndexRuntimeV1 {
    owner,
    publisher: Arc::clone(&request.publisher),
    registry,
    shadow_identity: request.shadow_identity.clone(),
    semantic_authority: semantic_authority.clone(),
    cancellation: request.cancellation.clone(),
    cadence,
  });
  check_cancellation(request.cancellation)?;
  let snapshot = installation.install(&source_authority_guard, runtime).map_err(|error| match error {
    IndexRuntimeErrorV1::AlreadyInstalled => NativeIndexRuntimeInstallationErrorV1::AlreadyInstalled,
    error => NativeIndexRuntimeInstallationErrorV1::Runtime(error),
  })?;
  drop(semantic_authority_guard);
  drop(source_authority_guard);

  Ok(NativeIndexRuntimeInstallationReceiptV1 {
    source_physical_instance_id: request.shadow_identity.source_physical_instance_id,
    destination_physical_instance_id: request.shadow_identity.destination_physical_instance_id,
    selected_root_hash: semantic_authority.root_hash,
    semantic_state_root: semantic_authority.semantic_state.object_id,
    root_publication_sequence: semantic_authority.root_publication_sequence,
    content_only,
    lifecycle: snapshot.lifecycle,
    recovered_scopes: snapshot.recovered_scopes,
    highest_checkpoint_sequence: snapshot.highest_checkpoint_sequence,
  })
}

fn validate_source_and_shadow(
  engine: &StorageEngine,
  identity: &IndexRuntimeShadowIdentityV1,
  publisher: &V4FirstAuthorityPublisher,
) -> Result<(), NativeIndexRuntimeInstallationErrorV1> {
  if identity.database_id.iter().all(|byte| *byte == 0)
    || identity.migration_id.iter().all(|byte| *byte == 0)
    || identity.source_physical_instance_id.iter().all(|byte| *byte == 0)
    || identity.destination_physical_instance_id.iter().all(|byte| *byte == 0)
  {
    return Err(invalid("native_index_shadow_identity", "runtime shadow identities must be nonzero"));
  }
  if engine.hash_algo() != identity.hash_algorithm {
    return Err(invalid("native_index_source_hash", "source engine hash profile disagrees with migration preflight"));
  }
  let source_file_identity = platform_file_identity(engine.database_path())?;
  if !source_file_identity.represents_same_physical_file_as(identity.source_file_identity) {
    return Err(invalid("native_index_source_identity", "open source engine is not the physical file admitted by migration preflight"));
  }
  let observation = publisher.observe()?;
  let header = &observation.selected.header;
  if observation.selected.redundancy_degraded
    || header.database_id != identity.database_id
    || header.physical_instance_id != identity.destination_physical_instance_id
    || header.hash_algorithm != identity.hash_algorithm
    || header.system_family_registry_fingerprint != identity.system_family_registry_fingerprint
  {
    return Err(invalid("native_index_destination_identity", "selected v4 shadow authority disagrees with migration preflight"));
  }
  Ok(())
}

fn validate_selected_semantic_authority(
  identity: &IndexRuntimeShadowIdentityV1,
  authority: &SelectedSemanticAuthorityV1,
) -> Result<(), NativeIndexRuntimeInstallationErrorV1> {
  validate_selected_semantic_authority_identity(identity, authority).map_err(|(code, message)| invalid(code, message))
}

fn validate_selected_semantic_authority_identity(
  identity: &IndexRuntimeShadowIdentityV1,
  authority: &SelectedSemanticAuthorityV1,
) -> Result<(), (&'static str, &'static str)> {
  if authority.database_id != identity.database_id
    || authority.physical_instance_id != identity.destination_physical_instance_id
    || authority.system_family_registry_fingerprint != identity.system_family_registry_fingerprint
  {
    return Err(("native_index_semantic_authority_identity", "selected semantic authority belongs to another destination identity"));
  }
  Ok(())
}

fn validate_runtime_publisher_selection(
  expected: Option<&super::index_coordinator_recovery::IndexCheckpointRootV1>,
  current: Option<&super::first_authority::LoadedIndexOperationControlV1>,
) -> Result<(), NativeIndexRuntimeInstallationErrorV1> {
  let agrees = match (expected, current) {
    (None, None) => true,
    (Some(expected), Some(current)) => {
      expected.checkpoint_sequence == current.control_sequence && expected.checkpoint_key == current.checkpoint_artifact
    }
    (None, Some(_)) | (Some(_), None) => false,
  };
  if !agrees {
    return Err(invalid(
      "native_index_runtime_publisher_selection_changed",
      "runtime publisher checkpoint selection changed while native recovery was in progress",
    ));
  }
  Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_final_installation_frontier(
  cancellation: &CancellationToken,
  expected_root: &[u8],
  expected_semantic_root: &[u8],
  expected_publication_sequence: u64,
  current_root: &[u8],
  current_semantic_root: &[u8],
  current_publication_sequence: u64,
) -> Result<(), NativeIndexRuntimeInstallationErrorV1> {
  check_cancellation(cancellation)?;
  if current_root != expected_root
    || current_semantic_root != expected_semantic_root
    || current_publication_sequence != expected_publication_sequence
  {
    return Err(invalid(
      "native_index_semantic_authority_changed",
      "selected shadow semantic authority changed while native index recovery was in progress",
    ));
  }
  Ok(())
}

struct ValidatedDescriptorOrderV1 {
  indices: Vec<usize>,
  _memory: Option<MemoryReservation>,
}

impl ValidatedDescriptorOrderV1 {
  fn indices(&self) -> &[usize] {
    &self.indices
  }
}

fn validate_runtime_publisher_descriptor(
  identity: &IndexRuntimeShadowIdentityV1,
  descriptors: &[NativeIndexOperationDescriptorV1],
  runtime_descriptor: &NativeIndexOperationDescriptorV1,
  options: IndexRuntimeNativeRecoveryOptionsV1,
) -> Result<(), NativeIndexRuntimeInstallationErrorV1> {
  if runtime_descriptor.hash_algorithm() != identity.hash_algorithm || runtime_descriptor.database_id() != identity.database_id {
    return Err(invalid("native_index_runtime_publisher_authority", "runtime publisher descriptor belongs to another shadow authority"));
  }
  if runtime_descriptor
    .retained_identity_bytes()
    .map_err(|error| invalid("native_index_runtime_publisher_descriptor_size", error.to_string()))?
    > options.maximum_descriptor_bytes
  {
    return Err(invalid("native_index_runtime_publisher_descriptor_size", "runtime publisher descriptor exceeds the recovery byte bound"));
  }
  if descriptors.iter().any(|descriptor| {
    descriptor.index_id() == runtime_descriptor.index_id() || descriptor.operation_id() == runtime_descriptor.operation_id()
  }) {
    return Err(invalid(
      "native_index_runtime_publisher_descriptor_collision",
      "runtime publisher descriptor collides with a query-visible index recovery descriptor",
    ));
  }
  Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_runtime_publisher(
  engine: &StorageEngine,
  coordinator_id: [u8; 16],
  identity: &IndexRuntimeShadowIdentityV1,
  semantic_authority: &SelectedSemanticAuthorityV1,
  options: &NativeIndexRuntimePublisherOptionsV1,
  publisher: Arc<V4FirstAuthorityPublisher>,
  retirement_owner: SharedRetirementJournalOwnerV1,
  cancellation: &CancellationToken,
  clock: Arc<dyn VirtualClock>,
  now_ms: u64,
) -> Result<(NativeIndexRuntimeBatchPublisherV1, bool), NativeIndexRuntimeInstallationErrorV1> {
  let owner = IndexRecoveryOwnerV1::new(identity.database_id, options.descriptor.index_id().to_vec(), options.descriptor.operation_id())?;
  let mut store = NativeIndexRecoveryStoreV1::new(options.descriptor.clone(), publisher, retirement_owner, Arc::clone(&clock))?;
  check_cancellation(cancellation)?;
  if store.load_selected(&owner)?.is_some() {
    let recovered = recover_index_runtime_dirty_overlay_v1(
      &mut store,
      identity.hash_algorithm,
      identity.database_id,
      identity.destination_physical_instance_id,
      &owner,
      options.workspace.clone(),
      &engine.memory_coordinator(),
      cancellation,
    )?;
    let recovered = match recovered {
      IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Resumable(recovered) => recovered,
      IndexRuntimeDirtyOverlayRecoveryOutcomeV1::ReconciliationRequired { reason, evidence } => {
        return Err(invalid(
          "native_index_runtime_publisher_recovery",
          format!("selected runtime dirty overlay requires reconciliation ({reason:?}): {evidence:?}"),
        ));
      }
      IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Canceled => return Err(NativeIndexRuntimeInstallationErrorV1::Canceled),
    };
    if recovered.generation() != options.generation {
      return Err(invalid(
        "native_index_runtime_publisher_resume_generation",
        "selected runtime workspace generation does not match the requested runtime generation",
      ));
    }
    let publisher = NativeIndexRuntimeBatchPublisherV1::new_resumed(recovered, store, clock)?;
    let workspace_identity = publisher.workspace_identity();
    if publisher.runtime_id() != coordinator_id || workspace_identity.workspace_id() != options.workspace_id {
      return Err(invalid(
        "native_index_runtime_publisher_resume_identity",
        "selected runtime workspace does not match the requested runtime and workspace identities",
      ));
    }
    return Ok((publisher, true));
  }

  let workspace = DurableIndexRuntimeWorkspaceV1::create(
    engine.database_path(),
    IndexRuntimeWorkspaceIdentityV1::new(
      identity.database_id,
      identity.destination_physical_instance_id,
      options.workspace_id,
      coordinator_id,
      identity.hash_algorithm,
    )?,
    options.workspace.clone(),
    cancellation.clone(),
    &engine.memory_coordinator(),
  )?;
  check_cancellation(cancellation)?;
  let publisher = NativeIndexRuntimeBatchPublisherV1::new_unselected(
    identity.hash_algorithm,
    owner,
    semantic_authority.root_hash.clone(),
    options.generation,
    now_ms,
    workspace,
    store,
    cancellation.clone(),
    clock,
  )?;
  Ok((publisher, false))
}

fn validate_descriptors(
  engine: &StorageEngine,
  identity: &IndexRuntimeShadowIdentityV1,
  descriptors: &[NativeIndexOperationDescriptorV1],
  options: IndexRuntimeNativeRecoveryOptionsV1,
  cancellation: &CancellationToken,
) -> Result<ValidatedDescriptorOrderV1, NativeIndexRuntimeInstallationErrorV1> {
  if descriptors.len() > options.maximum_operation_descriptors || descriptors.len() > u32::MAX as usize {
    return Err(invalid("native_index_descriptor_count", "selected operation descriptor count exceeds the recovery bound"));
  }
  let mut total_bytes = 0u64;
  for descriptor in descriptors {
    check_cancellation(cancellation)?;
    if descriptor.hash_algorithm() != identity.hash_algorithm || descriptor.database_id() != identity.database_id {
      return Err(invalid("native_index_descriptor_authority", "operation descriptor belongs to another shadow authority"));
    }
    total_bytes = total_bytes
      .checked_add(descriptor.retained_identity_bytes().map_err(|error| invalid("native_index_descriptor_size", error.to_string()))?)
      .ok_or_else(|| invalid("native_index_descriptor_size", "operation descriptor bytes overflowed"))?;
    if total_bytes > options.maximum_descriptor_bytes {
      return Err(invalid("native_index_descriptor_bytes", "selected operation descriptors exceed the recovery byte bound"));
    }
  }

  let order_bytes = descriptors
    .len()
    .checked_mul(size_of::<usize>())
    .ok_or_else(|| invalid("native_index_descriptor_order", "descriptor ordering workspace overflowed"))?;
  let order_bytes = u64::try_from(order_bytes)
    .map_err(|error| invalid("native_index_descriptor_order", format!("descriptor ordering workspace exceeds u64: {error}")))?;
  let _order_memory = if order_bytes == 0 {
    None
  } else {
    Some(engine.memory_coordinator().reserve(MemoryOwner::IndexDirtyBuffers, order_bytes, AdmissionClass::Maintenance)?)
  };
  let mut order = Vec::new();
  order
    .try_reserve_exact(descriptors.len())
    .map_err(|error| invalid("native_index_descriptor_order", format!("descriptor ordering allocation failed: {error}")))?;
  order.extend(0..descriptors.len());
  order.sort_unstable_by(|left, right| {
    descriptors[*left]
      .index_id()
      .cmp(descriptors[*right].index_id())
      .then_with(|| descriptors[*left].operation_id().cmp(&descriptors[*right].operation_id()))
  });
  for adjacent in order.windows(2) {
    if descriptors[adjacent[0]].index_id() == descriptors[adjacent[1]].index_id() {
      return Err(invalid(
        "native_index_descriptor_duplicate",
        "selected recovery contains more than one operation descriptor for one index scope",
      ));
    }
  }
  Ok(ValidatedDescriptorOrderV1 { indices: order, _memory: _order_memory })
}

fn recover_selected_operations(
  registry: &IndexScopeOrdinalStoreRegistryV1,
  descriptors: &[NativeIndexOperationDescriptorV1],
  order: &[usize],
  semantic_authority: &SelectedSemanticAuthorityV1,
  cancellation: &CancellationToken,
) -> Result<IndexRuntimeRecoveryDecisionV1, NativeIndexRuntimeInstallationErrorV1> {
  match &semantic_authority.semantic_state.availability {
    SemanticAvailabilityV1::ContentOnly { .. } => {
      if !descriptors.is_empty() {
        return Err(invalid("native_index_content_only_descriptors", "content-only semantic authority cannot select index checkpoints"));
      }
      return Ok(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 0, highest_checkpoint_sequence: 0 });
    }
    SemanticAvailabilityV1::Complete { .. } if descriptors.is_empty() => {
      return Ok(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 0, highest_checkpoint_sequence: 0 });
    }
    SemanticAvailabilityV1::Complete { .. } => {}
  }

  let mut recovered_scopes = 0u32;
  let mut highest_checkpoint_sequence = 0u64;
  for descriptor_index in order {
    check_cancellation(cancellation)?;
    let descriptor = &descriptors[*descriptor_index];
    let adapter = match registry.acquire(descriptor.clone()) {
      Ok(adapter) => adapter,
      Err(IndexScopeOrdinalStoreRegistryErrorV1::Canceled) => return Err(NativeIndexRuntimeInstallationErrorV1::Canceled),
      Err(error) => {
        return Ok(IndexRuntimeRecoveryDecisionV1::ReconciliationRequired {
          code: "native_index_registry_recovery_failed",
          context: error.to_string(),
        });
      }
    };
    let recovered = match adapter.recover_selected_checkpoint() {
      Ok(recovered) => recovered,
      Err(IndexRecoveryErrorV1::Canceled) => return Err(NativeIndexRuntimeInstallationErrorV1::Canceled),
      Err(error) => {
        return Ok(IndexRuntimeRecoveryDecisionV1::ReconciliationRequired {
          code: "native_index_checkpoint_recovery_failed",
          context: error.to_string(),
        });
      }
    };
    match recovered {
      IndexRecoveryOutcomeV1::Resumable(checkpoint) => {
        if checkpoint.journal.semantic_state_root != semantic_authority.semantic_state.object_id {
          return Ok(IndexRuntimeRecoveryDecisionV1::ReconciliationRequired {
            code: "native_index_checkpoint_semantic_root",
            context: format!(
              "scope {} selected checkpoint {} references another semantic state",
              hex::encode(descriptor.index_id()),
              checkpoint.checkpoint_sequence,
            ),
          });
        }
        recovered_scopes = recovered_scopes
          .checked_add(1)
          .ok_or_else(|| invalid("native_index_recovered_scope_count", "recovered scope count overflowed"))?;
        highest_checkpoint_sequence = highest_checkpoint_sequence.max(checkpoint.checkpoint_sequence);
      }
      IndexRecoveryOutcomeV1::ReconciliationRequired { reason, evidence } => {
        return Ok(IndexRuntimeRecoveryDecisionV1::ReconciliationRequired {
          code: recovery_reason_code(reason),
          context: format!("scope {} selected checkpoint requires reconciliation: {evidence:?}", hex::encode(descriptor.index_id()),),
        });
      }
      IndexRecoveryOutcomeV1::Canceled => return Err(NativeIndexRuntimeInstallationErrorV1::Canceled),
    }
  }
  Ok(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes, highest_checkpoint_sequence })
}

fn recovery_reason_code(reason: IndexRecoveryReasonV1) -> &'static str {
  match reason {
    IndexRecoveryReasonV1::CheckpointSelectionMissing => "native_index_checkpoint_selection_missing",
    IndexRecoveryReasonV1::CheckpointMissing => "native_index_checkpoint_missing",
    IndexRecoveryReasonV1::CheckpointCorrupt => "native_index_checkpoint_corrupt",
    IndexRecoveryReasonV1::CheckpointDiscontinuous => "native_index_checkpoint_discontinuous",
    IndexRecoveryReasonV1::AttachmentMissing => "native_index_attachment_missing",
    IndexRecoveryReasonV1::AttachmentCorrupt => "native_index_attachment_corrupt",
    IndexRecoveryReasonV1::JournalMissing => "native_index_journal_missing",
    IndexRecoveryReasonV1::JournalCorrupt => "native_index_journal_corrupt",
    IndexRecoveryReasonV1::JournalChainDiscontinuous => "native_index_journal_discontinuous",
    IndexRecoveryReasonV1::RecoveryLimitExceeded => "native_index_recovery_limit_exceeded",
  }
}

fn check_cancellation(cancellation: &CancellationToken) -> Result<(), NativeIndexRuntimeInstallationErrorV1> {
  if cancellation.is_cancelled() {
    Err(NativeIndexRuntimeInstallationErrorV1::Canceled)
  } else {
    Ok(())
  }
}

fn invalid(code: &'static str, message: impl Into<String>) -> NativeIndexRuntimeInstallationErrorV1 {
  NativeIndexRuntimeInstallationErrorV1::Invalid { code, message: message.into() }
}

#[cfg(test)]
#[path = "../../../spec/engine/index_runtime_installation_internal_spec.rs"]
mod index_runtime_installation_internal_spec;
