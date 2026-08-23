//! Native source/shadow binding and bounded v4 index-runtime recovery.
//!
//! The v3 source remains query authority. This module only installs one
//! recovered shadow owner after proving that the open source file and selected
//! v4 destination are the exact pair admitted by migration preflight.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::engine::memory_coordinator::{MemoryCoordinatorError, MemoryReservation};
use crate::engine::native_durability::{NativeDurabilityError, PlatformFileIdentityDescriptorV1, platform_file_identity};
use crate::engine::{EngineError, HashAlgorithm, StorageEngine, VirtualClock};

use super::admission::CapabilitySetV1;
use super::first_authority::{FirstAuthorityPublicationErrorV1, SelectedSemanticAuthorityV1, V4FirstAuthorityPublisher};
use super::coverage_journal::{
  CoverageJournalEncodeOptionsV1, CoverageJournalErrorV1, CoverageJournalWindowOptionsV1, CoverageJournalWindowOutcomeV1,
  build_source_coverage_authority, encode_soft_mutation_journal_segment, order_soft_mutation_window, soft_mutation_journal_working_bytes,
};
use super::coverage_runtime::{CoverageBoundaryV1, SoftMutationHubErrorV1, SoftMutationHubOptionsV1};
use super::index_coordinator_recovery::{
  IndexRecoveryErrorV1, IndexRecoveryOptionsV1, IndexRecoveryOutcomeV1, IndexRecoveryOwnerV1, IndexRecoveryReasonV1,
  IndexRecoveryStoreErrorV1, IndexRecoveryStoreV1,
};
use super::index_coverage_registry::{IndexCoverageRegistryOptionsV1, IndexCoverageRegistryOwnerRequestV1};
use super::index_coverage_runtime::{IndexCoverageRuntimeErrorV1, NativeIndexCoverageRuntimeV1};
use super::index_maintenance_scan::IndexMaintenanceScanLimitsV1;
use super::index_native_compaction::{NativeIndexCompactionExecutorV1, NativeIndexCompactionOptionsV1};
use super::index_native_journal_source::FirstAuthorityIndexProducerJournalSourceV1;
use super::index_native_parser::NativeIndexParserExecutorV1;
use super::index_recovery_store::{
  IndexScopeOrdinalStoreRegistryErrorV1, IndexScopeOrdinalStoreRegistryOptionsV1, IndexScopeOrdinalStoreRegistryV1,
  NativeIndexOperationDescriptorV1, NativeIndexRecoveryStoreV1, SharedRetirementJournalOwnerV1,
};
use super::index_native_semantic_source::{
  FirstAuthorityIndexSemanticObjectReadSourceV1, NativeIndexOperationDescriptorCatalogV1, NativeIndexScopeOrdinalAuthorityV1,
  NativeIndexSemanticSourceErrorV1,
};
use super::index_native_source::{
  NativeIndexFileRevisionSourceV1, NativeIndexMaintenanceScanSourceV1, NativeIndexScanTraversalLimitsV1, NativeIndexSourceLimitsV1,
};
use super::index_runtime_batch_publisher::{IndexRuntimeBatchPublisherBuildErrorV1, NativeIndexRuntimeBatchPublisherV1};
use super::index_runtime_cadence::{
  IndexRuntimeCadenceErrorV1, IndexRuntimeProducerServiceLimitsV1, IndexRuntimeProducerServiceSourcesV1, NativeIndexRuntimeCadenceV1,
};
use super::index_producer_admission::{
  IndexProducerMaintenanceAdmissionErrorV1, IndexProducerMaintenanceIntentV1, IndexProducerMaintenanceTargetV1, build_maintenance_task,
};
use super::index_producer_coordinator::IndexProducerAdmissionV1;
use super::index_runtime_dirty_overlay_recovery::{
  IndexRuntimeDirtyOverlayRecoveryErrorV1, IndexRuntimeDirtyOverlayRecoveryOutcomeV1, recover_index_runtime_dirty_overlay_with_task_sink_v1,
};
use super::index_runtime_owner::{
  IndexRuntimeErrorV1, IndexRuntimeLifecycleV1, IndexRuntimeOwnerOptionsV1, IndexRuntimeOwnerV1, IndexRuntimeRecoveryDecisionV1,
};
use super::index_runtime_workspace_store::{
  DurableIndexRuntimeWorkspaceV1, IndexRuntimeRecoveredTaskSinkErrorV1, IndexRuntimeRecoveredTaskSinkV1, IndexRuntimeWorkspaceIdentityV1,
  IndexRuntimeWorkspaceOptionsV1, IndexRuntimeWorkspaceStoreErrorV1,
};
use super::index_scope_ordinal_authority::IndexScopeOrdinalStateOptionsV1;
use super::index_semantic_source::CatalogIndexSemanticScopeSourceV1;
use super::migration_owner::MigrationStateOwnerV1;
use super::migration_preflight::MigrationPreflightPermitV1;
use super::namespace::SemanticAvailabilityV1;
use super::system_family::embedded_system_family_registry;

const INDEX_RUNTIME_SOFT_JOURNAL_MAX_RECORDS: usize = 10_000;

#[derive(Clone, Copy)]
struct NativeIndexRuntimeProducerServiceOptionsV1 {
  source: NativeIndexSourceLimitsV1,
  traversal: NativeIndexScanTraversalLimitsV1,
  maintenance: IndexMaintenanceScanLimitsV1,
  ordinals: IndexScopeOrdinalStateOptionsV1,
  cadence: IndexRuntimeProducerServiceLimitsV1,
  compaction: NativeIndexCompactionOptionsV1,
}

impl NativeIndexRuntimeProducerServiceOptionsV1 {
  fn engine_default(
    semantic_limits: super::index_producer_source::IndexSemanticScopeLimitsV1,
  ) -> Result<Self, NativeIndexRuntimeInstallationErrorV1> {
    Ok(Self {
      source: NativeIndexSourceLimitsV1::new(16 * 1_024 * 1_024, 16 * 1_024 * 1_024, 64)
        .map_err(|error| invalid("native_index_service_source_options", error.to_string()))?,
      traversal: NativeIndexScanTraversalLimitsV1::new(256, 65_536)
        .map_err(|error| invalid("native_index_service_traversal_options", error.to_string()))?,
      maintenance: IndexMaintenanceScanLimitsV1::new(8, 80 * 1_024 * 1_024, 16 * 1_024)
        .map_err(|error| invalid("native_index_service_page_options", error.to_string()))?,
      ordinals: IndexScopeOrdinalStateOptionsV1::new(8, 256)
        .map_err(|error| invalid("native_index_service_ordinal_options", error.to_string()))?,
      cadence: IndexRuntimeProducerServiceLimitsV1::new(256, Duration::from_millis(500))
        .map_err(|error| invalid("native_index_service_cadence_options", error.to_string()))?,
      compaction: NativeIndexCompactionOptionsV1::engine_default(semantic_limits)
        .map_err(|error| invalid("native_index_service_compaction_options", error.to_string()))?,
    })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexRuntimeShadowIdentityV1 {
  database_id: [u8; 16],
  migration_id: [u8; 16],
  source_physical_instance_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  source_file_identity: PlatformFileIdentityDescriptorV1,
  hash_algorithm: HashAlgorithm,
  supported_reader_capabilities: CapabilitySetV1,
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
      supported_reader_capabilities: permit.capability_profile().supported_reader_capabilities,
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
      supported_reader_capabilities: owner.capability_profile().supported_reader_capabilities,
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
  coverage_registry: IndexCoverageRegistryOptionsV1,
  checkpoint: IndexRecoveryOptionsV1,
}

impl IndexRuntimeNativeRecoveryOptionsV1 {
  pub fn new(
    maximum_operation_descriptors: usize,
    maximum_descriptor_bytes: u64,
    registry: IndexScopeOrdinalStoreRegistryOptionsV1,
    coverage_registry: IndexCoverageRegistryOptionsV1,
    checkpoint: IndexRecoveryOptionsV1,
  ) -> Result<Self, NativeIndexRuntimeInstallationErrorV1> {
    if maximum_operation_descriptors == 0 || maximum_descriptor_bytes == 0 {
      return Err(invalid("native_index_recovery_limits", "descriptor count and byte limits must be nonzero"));
    }
    Ok(Self { maximum_operation_descriptors, maximum_descriptor_bytes, registry, coverage_registry, checkpoint })
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
  pub coverage_owner_requests: &'request [IndexCoverageRegistryOwnerRequestV1],
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
  retirement_owner: SharedRetirementJournalOwnerV1,
  registry: Arc<IndexScopeOrdinalStoreRegistryV1>,
  coverage: Arc<NativeIndexCoverageRuntimeV1>,
  descriptor_catalog: Arc<NativeIndexOperationDescriptorCatalogV1>,
  shadow_identity: IndexRuntimeShadowIdentityV1,
  semantic_authority: SelectedSemanticAuthorityV1,
  journal_generation: u64,
  cancellation: CancellationToken,
  cadence: Arc<NativeIndexRuntimeCadenceV1>,
  service_options: NativeIndexRuntimeProducerServiceOptionsV1,
}

pub(crate) struct PreparedNativeSoftMutationJournalV1<'runtime> {
  lease: super::coverage_runtime::SoftMutationLeaseV1<'runtime>,
  artifact: super::index_artifact::EncodedImmutableIndexArtifactV1,
  cadence: Arc<NativeIndexRuntimeCadenceV1>,
  owner: Arc<IndexRuntimeOwnerV1>,
  _memory: MemoryReservation,
}

pub(crate) struct LeasedNativeSoftMutationJournalV1<'runtime> {
  runtime: &'runtime NativeIndexRuntimeV1,
  lease: super::coverage_runtime::SoftMutationLeaseV1<'runtime>,
  soft_options: SoftMutationHubOptionsV1,
}

impl<'runtime> LeasedNativeSoftMutationJournalV1<'runtime> {
  /// Clone and encode after the caller has released source namespace
  /// authority. The lease retains the original queue prefix for exact
  /// rollback until journal and task durability are established.
  pub(crate) fn prepare(self) -> Result<PreparedNativeSoftMutationJournalV1<'runtime>, IndexRuntimeCadenceErrorV1> {
    let Self { runtime, lease, soft_options } = self;
    if runtime.cancellation.is_cancelled() {
      return Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled));
    }
    let working_bytes = soft_mutation_journal_working_bytes(
      runtime.shadow_identity.hash_algorithm,
      soft_options.maximum_notices,
      soft_options.maximum_retained_bytes,
      soft_options.maximum_notice_bytes,
      INDEX_RUNTIME_SOFT_JOURNAL_MAX_RECORDS,
    )
    .map_err(soft_journal_error)?;
    let memory = runtime.owner.reserve_soft_journal_memory(working_bytes)?;
    let semantic_authority = runtime.publisher.load_selected_semantic_authority().map_err(|error| soft_journal(error.to_string()))?;
    validate_selected_semantic_authority_identity(&runtime.shadow_identity, &semantic_authority)
      .map_err(|(code, message)| soft_journal(format!("{code}: {message}")))?;
    let drain = lease.try_clone_drain().map_err(|error| soft_journal(error.to_string()))?;
    let first = drain.notices.first().ok_or_else(|| soft_journal("leased soft-mutation window is empty"))?;
    let last = drain.notices.last().ok_or_else(|| soft_journal("leased soft-mutation window is empty"))?;
    let first_publication_sequence = first.publication_sequence;
    let registry =
      embedded_system_family_registry(runtime.shadow_identity.hash_algorithm).map_err(|error| soft_journal(error.to_string()))?;
    if registry.operational_fingerprint != semantic_authority.system_family_registry_fingerprint {
      return Err(soft_journal("selected semantic authority uses a different SystemFamily registry"));
    }
    let covered_sequence = first_publication_sequence
      .checked_sub(1)
      .ok_or_else(|| soft_journal("soft-mutation window has no valid preceding publication sequence"))?;
    let controls = build_source_coverage_authority(
      runtime.shadow_identity.hash_algorithm,
      &first.previous_namespace_root,
      &semantic_authority.semantic_state,
      registry,
    )
    .map_err(soft_journal_error)?;
    let selected = build_source_coverage_authority(
      runtime.shadow_identity.hash_algorithm,
      &last.namespace_root,
      &semantic_authority.semantic_state,
      registry,
    )
    .map_err(soft_journal_error)?;
    let covered = if covered_sequence == 0 {
      CoverageBoundaryV1::initial(controls)
    } else {
      CoverageBoundaryV1::new(controls, covered_sequence).map_err(|error| soft_journal(error.to_string()))?
    };
    let selected = CoverageBoundaryV1::new(selected, last.publication_sequence).map_err(|error| soft_journal(error.to_string()))?;
    let window = match order_soft_mutation_window(
      runtime.shadow_identity.hash_algorithm,
      drain.notices,
      &covered,
      &selected,
      CoverageJournalWindowOptionsV1::new(soft_options.maximum_notices, soft_options.maximum_retained_bytes).map_err(soft_journal_error)?,
    ) {
      CoverageJournalWindowOutcomeV1::Exact(window) => window,
      CoverageJournalWindowOutcomeV1::BoundedDiffRequired { reason } | CoverageJournalWindowOutcomeV1::RebuildRequired(reason) => {
        return Err(soft_journal(format!("soft-mutation window requires authoritative reconciliation: {reason:?}")));
      }
    };
    let artifact = encode_soft_mutation_journal_segment(
      runtime.shadow_identity.hash_algorithm,
      &window,
      CoverageJournalEncodeOptionsV1 {
        generation: runtime.journal_generation,
        segment_ordinal: first_publication_sequence,
        previous_segment: vec![0; runtime.shadow_identity.hash_algorithm.hash_length()],
        runtime_boot_id: runtime.owner.coordinator_id(),
      },
    )
    .map_err(soft_journal_error)?;
    Ok(PreparedNativeSoftMutationJournalV1 {
      lease,
      artifact,
      cadence: Arc::clone(&runtime.cadence),
      owner: Arc::clone(&runtime.owner),
      _memory: memory,
    })
  }
}

impl PreparedNativeSoftMutationJournalV1<'_> {
  pub(crate) fn persist(self) -> Result<(), IndexRuntimeCadenceErrorV1> {
    let Self { lease, artifact, cadence, owner, _memory } = self;
    let journal = super::index_task::decode_mutation_journal(&artifact.value, owner.hash_algorithm())
      .map_err(|error| soft_journal(error.to_string()))?;
    cadence.persist_and_admit_mutation_journal(&artifact, &journal)?;
    lease.commit().map_err(|error| soft_journal(error.to_string()))?;
    owner.refresh_soft_hub_observation()?;
    Ok(())
  }
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

  pub fn coverage(&self) -> &Arc<NativeIndexCoverageRuntimeV1> {
    &self.coverage
  }

  pub fn descriptor_catalog(&self) -> &Arc<NativeIndexOperationDescriptorCatalogV1> {
    &self.descriptor_catalog
  }

  pub fn semantic_object_source(&self) -> FirstAuthorityIndexSemanticObjectReadSourceV1 {
    FirstAuthorityIndexSemanticObjectReadSourceV1::new(Arc::clone(&self.publisher))
  }

  pub fn scope_ordinal_authority(
    &self,
    options: IndexScopeOrdinalStateOptionsV1,
  ) -> Result<NativeIndexScopeOrdinalAuthorityV1, NativeIndexSemanticSourceErrorV1> {
    NativeIndexScopeOrdinalAuthorityV1::new(
      self.shadow_identity.hash_algorithm,
      Arc::clone(&self.descriptor_catalog),
      Arc::clone(&self.registry),
      options,
    )
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

  pub(crate) fn service_bounded_producers(&self, engine: &StorageEngine) -> Result<u16, IndexRuntimeCadenceErrorV1> {
    if engine.hash_algo() != self.shadow_identity.hash_algorithm {
      return Err(IndexRuntimeCadenceErrorV1::NativeSource("installed runtime and source engine hash profiles disagree".to_string()));
    }
    let memory = engine.memory_coordinator();
    let journal_source = FirstAuthorityIndexProducerJournalSourceV1::new(
      Arc::clone(&self.publisher),
      Arc::clone(&memory),
      self.shadow_identity.hash_algorithm,
    );
    let revision_source = NativeIndexFileRevisionSourceV1::new(engine, self.service_options.source);
    let maintenance_source = NativeIndexMaintenanceScanSourceV1::new(engine, self.service_options.source, self.service_options.traversal);
    let semantic_objects = FirstAuthorityIndexSemanticObjectReadSourceV1::new(Arc::clone(&self.publisher));
    let ordinal_authority = self
      .scope_ordinal_authority(self.service_options.ordinals)
      .map_err(|error| IndexRuntimeCadenceErrorV1::NativeSource(error.to_string()))?;
    let semantic_source = CatalogIndexSemanticScopeSourceV1::new(
      self.shadow_identity.hash_algorithm,
      memory.as_ref().clone(),
      &semantic_objects,
      &ordinal_authority,
    );
    let parser = NativeIndexParserExecutorV1::new(engine);
    let compaction_executor = NativeIndexCompactionExecutorV1::new(
      self.shadow_identity.database_id,
      self.shadow_identity.hash_algorithm,
      Arc::clone(&self.publisher),
      Arc::clone(&self.retirement_owner),
      Arc::clone(&memory),
      &semantic_source,
      self.service_options.compaction,
    )
    .map_err(|error| IndexRuntimeCadenceErrorV1::NativeSource(error.to_string()))?;
    self.cadence.service_bounded_producers(
      IndexRuntimeProducerServiceSourcesV1 {
        journal_source: &journal_source,
        maintenance_source: &maintenance_source,
        compaction_executor: &compaction_executor,
        maintenance_limits: self.service_options.maintenance,
        revision_source: &revision_source,
        semantic_source: &semantic_source,
        parser: &parser,
        mapper: None,
      },
      self.service_options.cadence,
    )
  }

  pub(crate) fn admit_maintenance_tasks(
    &self,
    source_operation_id: [u8; 16],
    publication_sequence: u64,
    namespace_root: &[u8],
    targets: &[IndexProducerMaintenanceTargetV1<'_>],
  ) -> Result<Vec<IndexProducerAdmissionV1>, NativeIndexRuntimeTaskAdmissionErrorV1> {
    if targets.is_empty() || targets.len() > 8 {
      return Err(NativeIndexRuntimeTaskAdmissionErrorV1::Invalid {
        code: "native_index_maintenance_batch",
        message: "maintenance admission batch must contain between one and eight targets".to_string(),
      });
    }
    for (target_index, target) in targets.iter().enumerate() {
      if targets[..target_index].iter().any(|prior| prior.class == target.class && prior.scope == target.scope) {
        return Err(NativeIndexRuntimeTaskAdmissionErrorV1::Invalid {
          code: "native_index_maintenance_batch_duplicate",
          message: format!("maintenance target {} duplicates an earlier class/scope pair", target_index + 1),
        });
      }
    }
    let semantic_authority = self.publisher.load_selected_semantic_authority()?;
    validate_selected_semantic_authority_identity(&self.shadow_identity, &semantic_authority)
      .map_err(|(code, message)| NativeIndexRuntimeTaskAdmissionErrorV1::Invalid { code, message: message.to_string() })?;
    let mut requests = Vec::new();
    requests.try_reserve_exact(targets.len()).map_err(|error| NativeIndexRuntimeTaskAdmissionErrorV1::Invalid {
      code: "native_index_maintenance_batch",
      message: format!("maintenance request allocation failed: {error}"),
    })?;
    for target in targets {
      requests.push(build_maintenance_task(
        self.shadow_identity.hash_algorithm,
        IndexProducerMaintenanceIntentV1 {
          source_operation_id,
          class: target.class,
          publication_sequence,
          namespace_root,
          semantic_state_root: &semantic_authority.semantic_state.object_id,
          scope: target.scope,
        },
      )?);
    }
    let mut outcomes = Vec::new();
    outcomes.try_reserve_exact(requests.len()).map_err(|error| NativeIndexRuntimeTaskAdmissionErrorV1::Invalid {
      code: "native_index_maintenance_batch",
      message: format!("maintenance outcome allocation failed: {error}"),
    })?;
    for request in requests {
      outcomes.push(self.cadence.admit_task(request)?);
    }
    Ok(outcomes)
  }

  /// Lease one exact source window. The caller must hold source namespace
  /// authority so the root and hard-publication frontier cannot move during
  /// this short capture; semantic authority is loaded later during prepare.
  pub(crate) fn lease_soft_mutation_journal(
    &self,
    source_namespace_root: &[u8],
    source_publication_sequence: u64,
  ) -> Result<Option<LeasedNativeSoftMutationJournalV1<'_>>, IndexRuntimeCadenceErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled));
    }
    if !self.owner.has_pending_soft_mutations() {
      return Ok(None);
    }
    let soft = self.owner.soft_mutation_options();
    let lease = match self.owner.lease_soft_mutations(INDEX_RUNTIME_SOFT_JOURNAL_MAX_RECORDS) {
      Ok(Some(lease)) => lease,
      Ok(None) | Err(SoftMutationHubErrorV1::QueueContended) => return Ok(None),
      Err(error) => return Err(soft_journal(error.to_string())),
    };
    if lease.record_count() == 0 {
      return Err(soft_journal("leased soft-mutation window contains no records"));
    }
    let last = lease.drain().notices.last().ok_or_else(|| soft_journal("leased soft-mutation window is empty"))?;
    if last.publication_sequence > source_publication_sequence {
      return Err(soft_journal("leased soft-mutation window is ahead of the source hard-publication frontier"));
    }
    if lease.queue_exhausted() && last.namespace_root != source_namespace_root {
      return Err(soft_journal("complete soft-mutation queue does not close over the current source namespace root"));
    }

    Ok(Some(LeasedNativeSoftMutationJournalV1 { runtime: self, lease, soft_options: soft }))
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
  #[error("native index runtime coverage lifecycle failed: {0}")]
  Coverage(#[from] IndexCoverageRuntimeErrorV1),
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
  let descriptor_catalog = Arc::new(
    NativeIndexOperationDescriptorCatalogV1::new(
      request.shadow_identity.hash_algorithm,
      request.shadow_identity.database_id,
      request.operation_descriptors,
      request.recovery_options.maximum_operation_descriptors,
      request.recovery_options.maximum_descriptor_bytes,
      engine.memory_coordinator(),
      &|| request.cancellation.is_cancelled(),
    )
    .map_err(map_descriptor_catalog_installation_error)?,
  );
  validate_runtime_publisher_descriptor(
    request.shadow_identity,
    descriptor_catalog.descriptors(),
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
  let coverage = Arc::new(NativeIndexCoverageRuntimeV1::new(
    request.shadow_identity.hash_algorithm,
    request.shadow_identity.database_id,
    request.shadow_identity.supported_reader_capabilities,
    request.recovery_options.coverage_registry,
    request.coverage_owner_requests,
    Arc::clone(&request.publisher),
    Arc::clone(&registry),
    Arc::clone(&memory),
    request.cancellation.clone(),
  )?);
  if let Err(error) = coverage.refresh() {
    if error.is_installation_contract_failure() {
      return Err(error.into());
    }
    tracing::warn!(code = error.code(), error = %error, "Initial v4 index coverage refresh failed; exact fallback remains authoritative");
  }

  let content_only = matches!(semantic_authority.semantic_state.availability, SemanticAvailabilityV1::ContentOnly { .. });
  let mut recovery = recover_selected_operations(&registry, descriptor_catalog.descriptors(), &semantic_authority, request.cancellation)?;
  let (runtime_publisher, resumed_dirty_overlay) = build_runtime_publisher(
    engine,
    owner.as_ref(),
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
  let service_options = NativeIndexRuntimeProducerServiceOptionsV1::engine_default(request.runtime_options.semantic)?;
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
    retirement_owner: Arc::clone(&request.retirement_owner),
    registry,
    coverage,
    descriptor_catalog,
    shadow_identity: request.shadow_identity.clone(),
    semantic_authority: semantic_authority.clone(),
    journal_generation: request.runtime_publisher.generation,
    cancellation: request.cancellation.clone(),
    cadence,
    service_options,
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
  runtime_owner: &IndexRuntimeOwnerV1,
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
  let workspace_identity = IndexRuntimeWorkspaceIdentityV1::new(
    identity.database_id,
    identity.destination_physical_instance_id,
    options.workspace_id,
    coordinator_id,
    identity.hash_algorithm,
  )?;
  let mut store = NativeIndexRecoveryStoreV1::new(options.descriptor.clone(), publisher, retirement_owner, Arc::clone(&clock))?;
  check_cancellation(cancellation)?;
  if store.load_selected(&owner)?.is_some() {
    let mut task_sink = NativeIndexRecoveredTaskSinkV1 { owner: runtime_owner, now_ms };
    let recovered = recover_index_runtime_dirty_overlay_with_task_sink_v1(
      &mut store,
      engine.database_path(),
      workspace_identity,
      &owner,
      options.workspace.clone(),
      &engine.memory_coordinator(),
      cancellation,
      &mut task_sink,
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
    let resumed_workspace_identity = publisher.workspace_identity();
    if publisher.runtime_id() != coordinator_id || resumed_workspace_identity.workspace_id() != options.workspace_id {
      return Err(invalid(
        "native_index_runtime_publisher_resume_identity",
        "selected runtime workspace does not match the requested runtime and workspace identities",
      ));
    }
    return Ok((publisher, true));
  }

  let workspace = DurableIndexRuntimeWorkspaceV1::create(
    engine.database_path(),
    workspace_identity,
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

struct NativeIndexRecoveredTaskSinkV1<'owner> {
  owner: &'owner IndexRuntimeOwnerV1,
  now_ms: u64,
}

impl IndexRuntimeRecoveredTaskSinkV1 for NativeIndexRecoveredTaskSinkV1<'_> {
  fn admit_recovered_task(
    &mut self,
    task: super::index_producer_coordinator::IndexProducerTaskRequestV1<'_>,
  ) -> Result<(), IndexRuntimeRecoveredTaskSinkErrorV1> {
    self
      .owner
      .admit_recovered_task(task, self.now_ms)
      .map(|_| ())
      .map_err(|error| IndexRuntimeRecoveredTaskSinkErrorV1::new("native_index_recovered_task_admission", error.to_string()))
  }
}

fn recover_selected_operations(
  registry: &IndexScopeOrdinalStoreRegistryV1,
  descriptors: &[NativeIndexOperationDescriptorV1],
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
  for descriptor in descriptors {
    check_cancellation(cancellation)?;
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

fn map_descriptor_catalog_installation_error(error: NativeIndexSemanticSourceErrorV1) -> NativeIndexRuntimeInstallationErrorV1 {
  match error {
    NativeIndexSemanticSourceErrorV1::Cancelled => NativeIndexRuntimeInstallationErrorV1::Canceled,
    NativeIndexSemanticSourceErrorV1::Memory(error) => NativeIndexRuntimeInstallationErrorV1::Memory(error),
    NativeIndexSemanticSourceErrorV1::Invalid { code, message } => {
      let code = match code {
        "native_scope_descriptor_options" => "native_index_descriptor_options",
        "native_scope_descriptor_count" => "native_index_descriptor_count",
        "native_scope_descriptor_authority" => "native_index_descriptor_authority",
        "native_scope_descriptor_size" => "native_index_descriptor_size",
        "native_scope_descriptor_bytes" => "native_index_descriptor_bytes",
        "native_scope_descriptor_allocation" => "native_index_descriptor_order",
        "native_scope_descriptor_duplicate" => "native_index_descriptor_duplicate",
        _ => "native_index_descriptor_catalog",
      };
      invalid(code, message)
    }
  }
}

fn invalid(code: &'static str, message: impl Into<String>) -> NativeIndexRuntimeInstallationErrorV1 {
  NativeIndexRuntimeInstallationErrorV1::Invalid { code, message: message.into() }
}

fn soft_journal(message: impl Into<String>) -> IndexRuntimeCadenceErrorV1 {
  IndexRuntimeCadenceErrorV1::SoftJournal(message.into())
}

fn soft_journal_error(error: CoverageJournalErrorV1) -> IndexRuntimeCadenceErrorV1 {
  soft_journal(error.to_string())
}

#[cfg(test)]
#[path = "../../../spec/engine/index_runtime_installation_internal_spec.rs"]
mod index_runtime_installation_internal_spec;
