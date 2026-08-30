use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use aeordb::engine::index_config::{IndexFieldConfig, PathIndexConfig};
use aeordb::engine::configuration_observability::ConfigurationVisibility;
use aeordb::engine::health::{HealthStatus, check_engine};
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy, MemoryPressure};
use aeordb::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity};
use aeordb::engine::query_engine::QueryBuilder;
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::emergency_spill::scan_for_database_with_dirs;
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::migration_destination::{
  InitializedMigrationDestinationV1, MigrationDestinationInitializationRequestV1, MigrationDestinationPathObservationV1,
  initialize_migration_destination_v1, observe_migration_destination_path_v1,
};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::first_authority::{
  ImmutableSemanticObjectBatchPublicationRequestV1, IndexActivePointerPublicationRequestV1, IndexArtifactBatchPublicationRequestV1,
  PreparedNamespaceTreeV0, SuccessorAuthorityPublicationRequestV1,
};
use aeordb::engine::v4::index_coordinator_recovery::{
  IndexCheckpointRootV1, IndexRecoveryOptionsV1, IndexRecoveryOwnerV1, IndexRecoveryStoreV1,
};
use aeordb::engine::v4::index_artifact::{
  ActivePointerKindV1, ActivePointerWriteV1, CoverageVersionV1, EncodedImmutableIndexArtifactV1, FieldIndexManifestBodyV1,
  FieldNvtManifestBodyV1, IndexManifestBodyV1, IndexManifestWriteV1, ScopeCatalogManifestBodyV1, ValueStoreManifestBodyV1,
  decode_index_manifest, encode_active_pointer, encode_index_manifest,
};
use aeordb::engine::v4::index_coverage_planner::IndexCoverageGenerationHealthV1;
use aeordb::engine::v4::index_coverage_registry::{
  IndexCoverageNvtStatusV1, IndexCoverageRegistryOptionsV1, IndexCoverageRegistryOwnerKindV1, IndexCoverageRegistryOwnerRequestV1,
  IndexCoverageRegistrySelectionV1,
};
use aeordb::engine::v4::index_coordinator::{
  FrozenIndexBatchV1, IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1, IndexMutationRequestV1,
};
use aeordb::engine::v4::index_operation_control::IndexOperationKindV1;
use aeordb::engine::v4::index_page::OrderedIndexRoleV1;
use aeordb::engine::v4::index_producer_collector::IndexProducerCollectorOptionsV1;
use aeordb::engine::v4::index_producer_coordinator::{
  IndexProducerAdmissionV1, IndexProducerCoordinatorOptionsV1, IndexProducerCoordinatorV1, IndexProducerTaskKindV1,
  IndexProducerTaskRequestV1,
};
use aeordb::engine::v4::index_producer_admission::{IndexProducerMaintenanceClassV1, IndexProducerMaintenanceTargetV1};
use aeordb::engine::v4::index_producer_source::IndexSemanticScopeLimitsV1;
use aeordb::engine::v4::index_recovery_store::{
  IndexScopeOrdinalStoreRegistryOptionsV1, NativeIndexOperationDescriptorV1, NativeIndexRecoveryStoreV1, SharedRetirementJournalOwnerV1,
};
use aeordb::engine::v4::index_record::{ScopeReverseRecordV1, encode_scope_reverse_record};
use aeordb::engine::v4::index_runtime_batch_publisher::{DurableIndexRuntimeBatchPublisherV1, NativeIndexRuntimeBatchPublisherV1};
use aeordb::engine::v4::index_runtime_cadence::IndexRuntimeCadenceErrorV1;
use aeordb::engine::v4::index_runtime_installation::{
  IndexRuntimeNativeRecoveryOptionsV1, IndexRuntimeShadowIdentityV1, NativeIndexRuntimeInstallationErrorV1,
  NativeIndexRuntimeInstallationRequestV1, NativeIndexRuntimePublisherOptionsV1, install_native_index_runtime_v1,
};
use aeordb::engine::v4::index_runtime_owner::{IndexRuntimeErrorV1, IndexRuntimeLifecycleV1, IndexRuntimeOwnerOptionsV1};
use aeordb::engine::v4::index_runtime_owner::IndexRuntimeBatchPublisherV1;
use aeordb::engine::v4::index_runtime_workspace_store::{
  DurableIndexRuntimeWorkspaceV1, IndexRuntimeWorkspaceIdentityV1, IndexRuntimeWorkspaceOptionsV1, IndexRuntimeWorkspaceSelectedHeadV1,
};
use aeordb::engine::v4::index_task::{
  IndexTaskAttachmentRoleV1, IndexTaskAttachmentWriteV1, IndexTaskCheckpointWriteV1, IndexTaskKindV1, IndexTaskStateV1, JournalOwnerKindV1,
  MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, encode_index_task_checkpoint,
  encode_mutation_journal,
};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, decode_namespace_root, encode_semantic_state_object};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::v4::{database_header::DATABASE_HEADER_V4_DATA_OFFSET, hash::digest_parts};
use aeordb::engine::{
  DirectoryOps, HashAlgorithm, IndexManager, IndexWriteBuffer, IndexWriteBufferOptions, IndexingPipeline, MockClock, StorageEngine,
  VirtualClock,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const GIB: u64 = 1024 * 1024 * 1024;

fn id(first: u8) -> [u8; 16] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn digest(first: u8) -> [u8; 32] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

struct RuntimeCoverageManifestChain {
  scope: EncodedImmutableIndexArtifactV1,
  value: EncodedImmutableIndexArtifactV1,
  field: EncodedImmutableIndexArtifactV1,
  nvt: EncodedImmutableIndexArtifactV1,
  scope_owner: Vec<u8>,
  field_owner: Vec<u8>,
}

impl RuntimeCoverageManifestChain {
  fn new(algorithm: HashAlgorithm) -> Self {
    let fixture = |suffix: &str| {
      let profile = match algorithm {
        HashAlgorithm::Blake3_256 => "blake3-256",
        HashAlgorithm::Sha512 => "sha512",
        _ => panic!("runtime coverage fixtures require one frozen v4 hash width"),
      };
      fs::read(format!("{}/spec/fixtures/v4/index-artifact-v1/aidx-{profile}-{suffix}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    };
    let scope_fixture_bytes = fixture("scope-catalog-manifest-empty.bin");
    let scope_fixture = decode_index_manifest(&scope_fixture_bytes, algorithm).unwrap();
    let IndexManifestBodyV1::ScopeCatalog(scope_body) = scope_fixture.details else {
      panic!("scope fixture kind");
    };
    let coverage_root = scope_body.coverage.source_namespace_root.to_vec();
    let coverage_epoch = scope_body.coverage.coverage_epoch_id.to_vec();
    let coverage = CoverageVersionV1 {
      source_namespace_root: &coverage_root,
      coverage_epoch_id: &coverage_epoch,
      coverage_publication_sequence: scope_body.coverage.coverage_publication_sequence.max(1),
    };
    let scope = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: scope_fixture.generation,
      owner_id: scope_fixture.owner_id,
      body: IndexManifestBodyV1::ScopeCatalog(ScopeCatalogManifestBodyV1 { coverage: coverage.clone(), ..scope_body }),
    })
    .unwrap();
    let scope_owner = scope_fixture.owner_id.to_vec();

    let value_fixture_bytes = fixture("value-store-manifest-empty.bin");
    let value_fixture = decode_index_manifest(&value_fixture_bytes, algorithm).unwrap();
    let IndexManifestBodyV1::ValueStore(value_body) = value_fixture.details else {
      panic!("value fixture kind");
    };
    let value = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: value_fixture.generation,
      owner_id: value_fixture.owner_id,
      body: IndexManifestBodyV1::ValueStore(ValueStoreManifestBodyV1 {
        coverage: coverage.clone(),
        scope_catalog_manifest: &scope.key,
        ..value_body
      }),
    })
    .unwrap();

    let field_fixture_bytes = fixture("field-index-manifest-empty.bin");
    let field_fixture = decode_index_manifest(&field_fixture_bytes, algorithm).unwrap();
    let IndexManifestBodyV1::FieldIndex(field_body) = field_fixture.details else {
      panic!("field fixture kind");
    };
    let field_owner = field_fixture.owner_id.to_vec();
    let field_generation = field_fixture.generation;
    let field = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: field_generation,
      owner_id: field_fixture.owner_id,
      body: IndexManifestBodyV1::FieldIndex(FieldIndexManifestBodyV1 { coverage, value_store_manifest: &value.key, ..field_body }),
    })
    .unwrap();

    let nvt_fixture_bytes = fixture("field-nvt-manifest-empty.bin");
    let nvt_fixture = decode_index_manifest(&nvt_fixture_bytes, algorithm).unwrap();
    let IndexManifestBodyV1::FieldNvt(nvt_body) = nvt_fixture.details else {
      panic!("NVT fixture kind");
    };
    let nvt = encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: nvt_fixture.generation,
      owner_id: field_fixture.owner_id,
      body: IndexManifestBodyV1::FieldNvt(FieldNvtManifestBodyV1 {
        basis_posting_generation: field_generation,
        basis_source_head_hash: &coverage_root,
        ..nvt_body
      }),
    })
    .unwrap();
    Self { scope, value, field, nvt, scope_owner, field_owner }
  }

  fn scope_successor(&self, algorithm: HashAlgorithm) -> EncodedImmutableIndexArtifactV1 {
    let manifest = decode_index_manifest(&self.scope.value, algorithm).unwrap();
    let IndexManifestBodyV1::ScopeCatalog(body) = manifest.details else {
      panic!("scope manifest kind");
    };
    let source_root = vec![0xa7; algorithm.hash_length()];
    let coverage_epoch = body.coverage.coverage_epoch_id.to_vec();
    encode_index_manifest(&IndexManifestWriteV1 {
      hash_algorithm: algorithm,
      generation: manifest.generation.checked_add(1).unwrap(),
      owner_id: manifest.owner_id,
      body: IndexManifestBodyV1::ScopeCatalog(ScopeCatalogManifestBodyV1 {
        coverage: CoverageVersionV1 {
          source_namespace_root: &source_root,
          coverage_epoch_id: &coverage_epoch,
          coverage_publication_sequence: body.coverage.coverage_publication_sequence.checked_add(1).unwrap(),
        },
        ..body
      }),
    })
    .unwrap()
  }
}

fn publish_runtime_coverage_artifacts(fixture: &RuntimeFixture, artifacts: &[&EncodedImmutableIndexArtifactV1], timestamp_ms: u64) {
  fixture
    .destination
    .publisher()
    .publish_index_artifacts(IndexArtifactBatchPublicationRequestV1 {
      database_id: &fixture.permit.database_id(),
      artifacts,
      publication_timestamp_ms: timestamp_ms,
    })
    .unwrap();
}

fn publish_runtime_coverage_pointer(
  fixture: &RuntimeFixture,
  retirement: &SharedRetirementJournalOwnerV1,
  kind: ActivePointerKindV1,
  manifest: &EncodedImmutableIndexArtifactV1,
  slot: u8,
  sequence: u64,
  timestamp_ms: u64,
) {
  let decoded = decode_index_manifest(&manifest.value, fixture.permit.hash_algorithm()).unwrap();
  let pointer = encode_active_pointer(&ActivePointerWriteV1 {
    kind,
    hash_algorithm: fixture.permit.hash_algorithm(),
    generation: decoded.generation,
    owner_id: decoded.owner_id,
    slot,
    sequence,
    target_manifest_hash: &manifest.key,
  })
  .unwrap();
  let mut retirement = retirement.lock().unwrap();
  fixture
    .destination
    .publisher()
    .publish_index_active_pointer(
      IndexActivePointerPublicationRequestV1 {
        database_id: &fixture.permit.database_id(),
        pointer: &pointer,
        publication_timestamp_ms: timestamp_ms,
        monotonic_now_ms: timestamp_ms,
      },
      &mut retirement,
    )
    .unwrap();
}

fn identity(first: u8, volume: u8) -> PlatformFileIdentityDescriptorV1 {
  PlatformFileIdentityDescriptorV1 {
    platform: 1,
    schema: 1,
    flags: 1 << 1,
    volume_identity: id(volume),
    file_identity: id(first),
    birth_identity: id(first.wrapping_add(0x20)),
  }
}

fn native(first: u8) -> NativeCutoverCapabilitiesV1 {
  NativeCutoverCapabilitiesV1 {
    data_barrier: true,
    file_barrier: true,
    parent_directory_sync: true,
    durable_replace: true,
    preallocation: true,
    stable_file_identity: true,
    read_back_verified: true,
    qualification_digest: digest(first),
  }
}

fn permit(algorithm: HashAlgorithm, destination: &MigrationDestinationPathObservationV1) -> MigrationPreflightPermitV1 {
  permit_with_source_identity(algorithm, destination, identity(0x50, 0x70))
}

fn permit_with_source_identity(
  algorithm: HashAlgorithm,
  destination: &MigrationDestinationPathObservationV1,
  source_file_identity: PlatformFileIdentityDescriptorV1,
) -> MigrationPreflightPermitV1 {
  permit_with_authority(algorithm, destination, source_file_identity, id(0x10), id(0x40))
}

fn permit_with_authority(
  algorithm: HashAlgorithm,
  destination: &MigrationDestinationPathObservationV1,
  source_file_identity: PlatformFileIdentityDescriptorV1,
  database_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
) -> MigrationPreflightPermitV1 {
  let baseline = CapabilitySetV1::v4_baseline();
  let registry = embedded_system_family_registry(algorithm).unwrap();
  let source_checksum = digest(0x71);
  let mut capacities = std::array::from_fn(|index| {
    let role = [CapacityRoleV1::Destination, CapacityRoleV1::Workspace, CapacityRoleV1::Backup, CapacityRoleV1::Capture][index];
    MigrationCapacityObservationV1 {
      role,
      volume_identity: id(0x80 + index as u8),
      path_identity: identity(0x90 + index as u8, 0x80 + index as u8),
      filesystem_capacity_bytes: 256 * GIB,
      available_bytes: 192 * GIB,
      required_bytes: if role == CapacityRoleV1::Capture { 64 * GIB } else { 4 * GIB },
      minimum_remaining_bytes: 16 * GIB,
    }
  });
  capacities[0].volume_identity = destination.parent_identity().volume_identity;
  capacities[0].path_identity = destination.parent_identity();
  let request = MigrationPreflightRequestV1 {
    identity: MigrationIdentityEvidenceV1 {
      database_id,
      migration_id: id(0x20),
      source_physical_instance_id: id(0x30),
      destination_physical_instance_id,
      source_path_digest: digest(0x10),
      destination_path_digest: destination.path_digest(),
      source_file_identity,
      destination_parent_identity: destination.parent_identity(),
    },
    source: MigrationSourceEvidenceV1 {
      hash_algorithm: algorithm,
      file_size: 4 * GIB,
      complete_file_checksum: source_checksum,
      selected_header_slot: 1,
      selected_header_sequence: 41,
      selected_header_digest: digest(0x80),
      head_hash: vec![0x91; algorithm.hash_length()],
    },
    verification: StrictVerificationEvidenceV1 {
      state: StrictVerificationStateV1::CompleteClean,
      source_file_size: 4 * GIB,
      source_header_sequence: 41,
      source_complete_file_checksum: source_checksum,
      issue_count: 0,
      evidence_digest: digest(0xa0),
    },
    recovery: MigrationRecoveryEvidenceV1 {
      inspection_complete: true,
      source_header_sequence: 41,
      durability_latched: false,
      repair_active: false,
      external_spill_count: 0,
      repair_ticket_count: 0,
      path_latch_count: 0,
      evidence_digest: digest(0xb0),
    },
    inventory: SourceAuthorityInventoryV1 {
      complete: true,
      source_header_sequence: 41,
      unresolved_family_count: 0,
      counts: AuthorityInventoryCountsV1 {
        protected_families: 46,
        modules: 2,
        snapshots: 3,
        forks: 1,
        symlinks: 4,
        history_roots: 8,
        peers: 2,
        sync_states: 2,
        tasks: 5,
        plugins: 2,
        roots: 12,
      },
      authority_digest: digest(0xc0),
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
    capacity: capacities,
    native: MigrationNativeEvidenceV1 { source: native(0xd0), destination: native(0xe0) },
    memory: MigrationMemoryEvidenceV1 {
      source_budget_bytes: GIB,
      destination_budget_bytes: 2 * GIB,
      coordinator_accounted_bytes: GIB,
      coordinator_ordinary_limit_bytes: 12 * GIB,
      host_available_bytes: 12 * GIB,
      host_available_floor_bytes: GIB,
      pressure: MemoryPressure::Normal,
      evidence_digest: digest(0xf0),
    },
    configuration: MigrationConfigurationEvidenceV1 {
      generation: 7,
      capture_max_bytes: 64 * GIB,
      capture_free_reserve_bytes: 16 * GIB,
      checkpoint_after_seconds: 300,
      effective_configuration_fingerprint: vec![0x17; algorithm.hash_length()],
    },
    binary: MigrationBinaryEvidenceV1 {
      source_commit: [0x21; 20],
      executable_sha256: digest(0x31),
      contract_registry_sha256: hex::decode(CONTRACT_REGISTRY_SHA256).unwrap().try_into().unwrap(),
      capability_profile: BinaryCapabilityProfileV1::new(BinaryCapabilityProfileV1::current().supported_reader_capabilities, baseline),
      required_reader_capabilities: baseline,
      required_writer_capabilities: baseline,
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
  };
  admit_migration_preflight_v1(&request).unwrap().1
}

fn runtime_options() -> IndexRuntimeOwnerOptionsV1 {
  IndexRuntimeOwnerOptionsV1 {
    soft_hub: aeordb::engine::v4::coverage_runtime::SoftMutationHubOptionsV1::engine_default(),
    producer: IndexProducerCoordinatorOptionsV1::new(32, 2 * 1_024 * 1_024, 3, 10, 1_000, 16, 256, 2 * 1_024 * 1_024).unwrap(),
    mutations: IndexCoordinatorOptionsV1::new(4 * 1_024 * 1_024, 262_144, 30_000, 256 * 1_024).unwrap(),
    collector: IndexProducerCollectorOptionsV1::new(8, 16, 32, 2 * 1_024 * 1_024, 256, 2 * 1_024 * 1_024, 50).unwrap(),
    semantic: IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 * 1_024 * 1_024).unwrap(),
    source_retry_after_ms: 25,
    publication_retry_after_ms: 100,
  }
}

fn native_recovery_options() -> IndexRuntimeNativeRecoveryOptionsV1 {
  IndexRuntimeNativeRecoveryOptionsV1::new(
    128,
    4 * 1_024 * 1_024,
    IndexScopeOrdinalStoreRegistryOptionsV1::new(8, 8 * 1_024 * 1_024).unwrap(),
    IndexCoverageRegistryOptionsV1::new(64, 256 * 1_024).unwrap(),
    IndexRecoveryOptionsV1::new(128, 16 * 1_024 * 1_024, 128, 16 * 1_024 * 1_024).unwrap(),
  )
  .unwrap()
}

fn source_engine(path: &Path, spill: &Path) -> StorageEngine {
  fs::create_dir(spill).unwrap();
  let overrides = aeordb::engine::config_resolver::CommandLineConfigOverrides::from_registered(BTreeMap::from([(
    "--recovery-emergency-spill-dir".to_string(),
    OsString::from(spill.as_os_str()),
  )]))
  .unwrap();
  StorageEngine::create_with_hot_dir_and_configuration_overrides(path.to_str().unwrap(), None, overrides).unwrap()
}

fn retirement_owner(engine: &StorageEngine, database_id: [u8; 16], cancellation: &CancellationToken) -> SharedRetirementJournalOwnerV1 {
  retirement_owner_with_algorithm(engine, engine.hash_algo(), database_id, cancellation)
}

fn retirement_owner_with_algorithm(
  engine: &StorageEngine,
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  cancellation: &CancellationToken,
) -> SharedRetirementJournalOwnerV1 {
  Arc::new(Mutex::new(
    RetirementJournalOwnerV1::new_chain(
      algorithm,
      database_id,
      1,
      901,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      cancellation,
      &engine.memory_coordinator(),
    )
    .unwrap(),
  ))
}

struct RuntimeFixture {
  _directory: TempDir,
  source_path: std::path::PathBuf,
  source: StorageEngine,
  permit: MigrationPreflightPermitV1,
  destination: InitializedMigrationDestinationV1,
  cancellation: CancellationToken,
}

struct CancelingClock {
  cancellation: CancellationToken,
  now_ms: u64,
}

impl VirtualClock for CancelingClock {
  fn now_ms(&self) -> u64 {
    self.cancellation.cancel();
    self.now_ms
  }

  fn node_id(&self) -> u64 {
    9
  }
}

struct AdvancingRuntimeClock {
  calls: AtomicUsize,
  now_ms: u64,
  advance: Mutex<Option<(NativeIndexRuntimeBatchPublisherV1, FrozenIndexBatchV1)>>,
}

struct PressuringRuntimeClock {
  calls: AtomicUsize,
  now_ms: u64,
  memory: Arc<MemoryCoordinator>,
}

impl VirtualClock for PressuringRuntimeClock {
  fn now_ms(&self) -> u64 {
    if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
      self.memory.reconfigure_policy(MemoryPolicy::new(1, 2, 1, 1).unwrap()).unwrap();
    }
    self.now_ms
  }

  fn node_id(&self) -> u64 {
    43
  }
}

impl VirtualClock for AdvancingRuntimeClock {
  fn now_ms(&self) -> u64 {
    if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
      let Some((mut publisher, batch)) = self.advance.lock().unwrap().take() else {
        panic!("runtime selection advance was not available");
      };
      publisher.publish(&batch).unwrap();
    }
    self.now_ms
  }

  fn node_id(&self) -> u64 {
    41
  }
}

impl RuntimeFixture {
  fn new(name: &str) -> Self {
    Self::new_with_authority(name, HashAlgorithm::Blake3_256, id(0x10), id(0x40))
  }

  fn new_with_authority(name: &str, algorithm: HashAlgorithm, database_id: [u8; 16], destination_physical_instance_id: [u8; 16]) -> Self {
    let directory = tempfile::tempdir().unwrap();
    let source_path = directory.path().join(format!("{name}-source.aeordb"));
    let source = source_engine(&source_path, &directory.path().join(format!("{name}-spill")));
    let shadow_path = directory.path().join(format!("{name}-shadow.aeordb"));
    let destination_observation = observe_migration_destination_path_v1(&shadow_path).unwrap();
    let permit = permit_with_authority(
      algorithm,
      &destination_observation,
      platform_file_identity(&source_path).unwrap(),
      database_id,
      destination_physical_instance_id,
    );
    let cancellation = CancellationToken::new();
    let destination = initialize_migration_destination_v1(MigrationDestinationInitializationRequestV1 {
      permit: &permit,
      destination: &destination_observation,
      created_at_ms: 1_700_000_000_000,
      writer_fence_epoch: 7,
      cancellation: &cancellation,
    })
    .unwrap();
    Self { _directory: directory, source_path, source, permit, destination, cancellation }
  }

  fn retirement(&self) -> SharedRetirementJournalOwnerV1 {
    retirement_owner_with_algorithm(&self.source, self.permit.hash_algorithm(), self.permit.database_id(), &self.cancellation)
  }

  fn identity(&self) -> IndexRuntimeShadowIdentityV1 {
    IndexRuntimeShadowIdentityV1::from_preflight(&self.permit)
  }

  fn clock(&self, node_id: u64, now_ms: u64) -> Arc<dyn VirtualClock> {
    Arc::new(MockClock::new(node_id, now_ms))
  }

  fn install_content_only_runtime(&self, coordinator_id: [u8; 16], workspace_id: [u8; 16], now_ms: u64) {
    install_native_index_runtime_v1(
      &self.source,
      NativeIndexRuntimeInstallationRequestV1 {
        coordinator_id,
        shadow_identity: &self.identity(),
        publisher: self.destination.shared_publisher(),
        retirement_owner: self.retirement(),
        operation_descriptors: &[],
        coverage_owner_requests: &[],
        runtime_options: runtime_options(),
        recovery_options: native_recovery_options(),
        runtime_publisher: runtime_publisher_options(self, workspace_id),
        cancellation: &self.cancellation,
        clock: self.clock(79, now_ms),
        now_ms,
      },
    )
    .unwrap();
  }
}

fn publish_complete_semantic_authority(fixture: &RuntimeFixture, definition_count: u64) -> Vec<u8> {
  let publisher = fixture.destination.publisher();
  let header = publisher.observe().unwrap().selected.header;
  let current_root = decode_namespace_root(&fixture.destination.first_authority().namespace_root.value, header.hash_algorithm).unwrap();
  let current_tree =
    publisher.load_immutable_entity_bounded(&current_root.namespace_tree_root, 1 << 20).unwrap().expect("selected namespace tree");
  let semantic_state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: header.required_reader_capabilities,
      availability: SemanticAvailabilityV1::Complete {
        compiler_fingerprint: digest_parts(header.hash_algorithm, &[b"runtime compiler"]),
        semantic_registry_fingerprint: fixture.permit.system_family_registry_fingerprint().to_vec(),
        catalog_root: digest_parts(header.hash_algorithm, &[b"runtime catalog"]),
        catalog_record_count: 1,
        catalog_node_count: 1,
        definition_count,
        dependency_count: 0,
      },
    },
    header.hash_algorithm,
  )
  .unwrap();
  let semantic_root = semantic_state.object_id.clone();
  publisher
    .publish_immutable_semantic_objects(ImmutableSemanticObjectBatchPublicationRequestV1 {
      database_id: &fixture.permit.database_id(),
      objects: std::slice::from_ref(&semantic_state),
      publication_timestamp_ms: 1_700_000_000_090,
    })
    .unwrap();
  publisher
    .publish_successor_authority(&SuccessorAuthorityPublicationRequestV1 {
      database_id: fixture.permit.database_id(),
      transaction_id: id(0x61),
      created_at_ms: 1_700_000_000_100,
      expected_head_hash: header.head_hash,
      namespace_tree: PreparedNamespaceTreeV0 { root_hash: current_root.namespace_tree_root, stored_value: current_tree.stored_value },
      semantic_state,
      required_capabilities: header.required_reader_capabilities,
      typed_closure_digest: digest_parts(header.hash_algorithm, &[b"runtime complete semantic closure"]),
      authority_identity: b"HEAD".to_vec(),
    })
    .unwrap();
  semantic_root
}

fn descriptor(database_id: [u8; 16]) -> NativeIndexOperationDescriptorV1 {
  descriptor_with(database_id, 0xc1, 0xc2, 0xc3)
}

fn descriptor_with(database_id: [u8; 16], index_fill: u8, operation_fill: u8, definition_fill: u8) -> NativeIndexOperationDescriptorV1 {
  descriptor_with_algorithm(HashAlgorithm::Blake3_256, database_id, index_fill, operation_fill, definition_fill)
}

fn descriptor_with_algorithm(
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  index_fill: u8,
  operation_fill: u8,
  definition_fill: u8,
) -> NativeIndexOperationDescriptorV1 {
  let hash_width = algorithm.hash_length();
  NativeIndexOperationDescriptorV1::new(
    algorithm,
    database_id,
    vec![index_fill; hash_width],
    [operation_fill; 16],
    IndexOperationKindV1::Build,
    vec![definition_fill; hash_width],
    None,
    None,
  )
  .unwrap()
}

fn runtime_publisher_options_for(
  base: &Path,
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  workspace_id: [u8; 16],
) -> NativeIndexRuntimePublisherOptionsV1 {
  runtime_publisher_options_for_generation(base, algorithm, database_id, workspace_id, 1)
}

fn runtime_publisher_options_for_generation(
  base: &Path,
  algorithm: HashAlgorithm,
  database_id: [u8; 16],
  workspace_id: [u8; 16],
  generation: u64,
) -> NativeIndexRuntimePublisherOptionsV1 {
  NativeIndexRuntimePublisherOptionsV1::new(
    descriptor_with_algorithm(algorithm, database_id, 0xd1, 0xd2, 0xd3),
    workspace_id,
    generation,
    runtime_workspace_options(base, workspace_id),
  )
  .unwrap()
}

fn runtime_workspace_options(base: &Path, workspace_id: [u8; 16]) -> IndexRuntimeWorkspaceOptionsV1 {
  let scratch = base.join(format!("runtime-{}", hex::encode(workspace_id)));
  fs::create_dir_all(&scratch).unwrap();
  IndexRuntimeWorkspaceOptionsV1::new(Some(scratch), 16 * 1024 * 1024, 0, 32).unwrap()
}

fn runtime_publisher_options(fixture: &RuntimeFixture, workspace_id: [u8; 16]) -> NativeIndexRuntimePublisherOptionsV1 {
  runtime_publisher_options_for(fixture._directory.path(), fixture.permit.hash_algorithm(), fixture.permit.database_id(), workspace_id)
}

fn seed_selected_runtime_dirty_overlay(
  fixture: &RuntimeFixture,
  runtime_id: [u8; 16],
  workspace_id: [u8; 16],
) -> IndexRuntimeWorkspaceSelectedHeadV1 {
  let (selected, _publisher, _successor) = seed_selected_runtime_dirty_overlay_with_successor(fixture, runtime_id, workspace_id);
  selected
}

fn seed_selected_runtime_dirty_overlay_with_successor(
  fixture: &RuntimeFixture,
  runtime_id: [u8; 16],
  workspace_id: [u8; 16],
) -> (IndexRuntimeWorkspaceSelectedHeadV1, NativeIndexRuntimeBatchPublisherV1, FrozenIndexBatchV1) {
  let algorithm = fixture.permit.hash_algorithm();
  let descriptor = descriptor_with_algorithm(algorithm, fixture.permit.database_id(), 0xd1, 0xd2, 0xd3);
  let clock = fixture.clock(31, 1_700_000_000_500);
  let store =
    NativeIndexRecoveryStoreV1::new(descriptor.clone(), fixture.destination.shared_publisher(), fixture.retirement(), Arc::clone(&clock))
      .unwrap();
  let workspace = DurableIndexRuntimeWorkspaceV1::create(
    &fixture.source_path,
    IndexRuntimeWorkspaceIdentityV1::new(
      fixture.permit.database_id(),
      fixture.permit.destination_physical_instance_id(),
      workspace_id,
      runtime_id,
      algorithm,
    )
    .unwrap(),
    runtime_workspace_options(fixture._directory.path(), workspace_id),
    fixture.cancellation.clone(),
    &fixture.source.memory_coordinator(),
  )
  .unwrap();
  let owner = IndexRecoveryOwnerV1::new(fixture.permit.database_id(), descriptor.index_id().to_vec(), descriptor.operation_id()).unwrap();
  let source_root = fixture.destination.publisher().load_selected_semantic_authority().unwrap().root_hash;
  let mut publisher = DurableIndexRuntimeBatchPublisherV1::new_unselected(
    algorithm,
    owner,
    source_root,
    1,
    1_700_000_000_500,
    workspace,
    store,
    fixture.cancellation.clone(),
    Arc::clone(&clock),
  )
  .unwrap();
  let mut coordinator = IndexCoordinatorV1::new(
    runtime_id,
    algorithm,
    (*fixture.source.memory_coordinator()).clone(),
    IndexCoordinatorOptionsV1::new(1024 * 1024, 16, 1_000, 1024 * 1024).unwrap(),
    1_700_000_000_500,
  )
  .unwrap();
  admit_runtime_dirty_record(&mut coordinator, algorithm, 1, 1, 1_700_000_000_501);
  let batch = coordinator.begin_flush(1_700_000_000_502, Some(IndexFlushReasonV1::Explicit), false).unwrap().expect("seeded runtime batch");
  publisher.publish(&batch).unwrap();
  let selected = publisher.workspace_head().unwrap().selected_descriptor();
  coordinator.complete_success(&batch).unwrap();
  admit_runtime_dirty_record(&mut coordinator, algorithm, 2, 2, 1_700_000_000_503);
  let successor =
    coordinator.begin_flush(1_700_000_000_504, Some(IndexFlushReasonV1::Explicit), false).unwrap().expect("successor runtime batch");
  (selected, publisher, successor)
}

fn admit_runtime_dirty_record(
  coordinator: &mut IndexCoordinatorV1,
  algorithm: HashAlgorithm,
  ordinal: u64,
  publication_sequence: u64,
  now_ms: u64,
) {
  let index_id = digest_parts(algorithm, &[b"runtime-dirty-index", &ordinal.to_le_bytes()]);
  let file_key = digest_parts(algorithm, &[b"runtime-dirty-file", &ordinal.to_le_bytes()]);
  let encoded_record =
    encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: ordinal, file_key: &file_key }, algorithm).unwrap();
  coordinator
    .admit(
      IndexMutationRequestV1 {
        index_id: &index_id,
        role: OrderedIndexRoleV1::ScopeReverse,
        publication_sequence,
        operation_id: id(0x73 + ordinal as u8),
        encoded_record: &encoded_record,
      },
      now_ms,
    )
    .unwrap();
}

#[derive(Clone, Copy)]
enum RecoveryFixtureFault {
  None,
  MissingJournal,
  DiscontinuousJournal,
}

fn publish_recoverable_checkpoint(
  fixture: &RuntimeFixture,
  descriptor: &NativeIndexOperationDescriptorV1,
  semantic_state_root: &[u8],
  retirement: SharedRetirementJournalOwnerV1,
  clock: Arc<dyn VirtualClock>,
  fault: RecoveryFixtureFault,
) -> Vec<u8> {
  const JOURNAL_OWNER: [u8; 16] = *b"AEORIDXJOURNALV1";
  let before_root = vec![0xa1; 32];
  let after_root = vec![0xa2; 32];
  let mutation_id = vec![0xa3; 32];
  let revision = vec![0xa4; 32];
  let record = MutationRecordWriteV1 {
    kind: MutationKindV1::Create,
    sequence: 1,
    mutation_id: &mutation_id,
    batch_ordinal: 0,
    batch_count: 1,
    root_before: &before_root,
    root_after: &after_root,
    before: None,
    after: Some(MutationSideWriteV1 { path: "/runtime/recovered.json", revision: &revision }),
    committed_at_ms: 1_700_000_000_110,
  };
  let previous = vec![0; 32];
  let journal = encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    owner_id: JOURNAL_OWNER,
    owner_kind: JournalOwnerKindV1::System,
    generation: 1,
    segment_ordinal: 1,
    chain_reset: true,
    previous_segment: &previous,
    semantic_state_root,
    runtime_boot_id: [0xa5; 16],
    records: &[record],
  })
  .unwrap();
  let attachment_owner = vec![0xa6; 32];
  let attachments = [IndexTaskAttachmentWriteV1 {
    role: IndexTaskAttachmentRoleV1::MutationJournalHead,
    owner_id: &attachment_owner,
    artifact_hash: &journal.key,
    birth_generation: 1,
  }];
  let checkpoint = encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    task_id: descriptor.operation_id(),
    checkpoint_sequence: 1,
    generation: 1,
    task_kind: IndexTaskKindV1::Reconcile,
    state: IndexTaskStateV1::Running,
    phase: 2,
    required_capabilities: &[0; 32],
    started_at_ms: 1_700_000_000_100,
    updated_at_ms: 1_700_000_000_110,
    source_root: &before_root,
    target_root: Some(if matches!(fault, RecoveryFixtureFault::DiscontinuousJournal) { &revision } else { &after_root }),
    primary_id: Some(descriptor.index_id()),
    journal_head: Some(&journal.key),
    journal_floor_sequence: 1,
    journal_audited_through: 1,
    next_document_ordinal: 1,
    completed_work: 1,
    total_work_hint: 1,
    resume_key: b"runtime recovery",
    attachments: &attachments,
    external: None,
  })
  .unwrap();
  let owner = IndexRecoveryOwnerV1::new(fixture.permit.database_id(), descriptor.index_id().to_vec(), descriptor.operation_id()).unwrap();
  let mut store = NativeIndexRecoveryStoreV1::new(descriptor.clone(), fixture.destination.shared_publisher(), retirement, clock).unwrap();
  if matches!(fault, RecoveryFixtureFault::MissingJournal) {
    store.put_immutable_batch(&[&checkpoint]).unwrap();
  } else {
    store.put_immutable_batch(&[&journal, &checkpoint]).unwrap();
  }
  store.sync_immutable().unwrap();
  let checkpoint_key = checkpoint.key.clone();
  store.publish_selected_synced(&owner, None, &IndexCheckpointRootV1::new(1, checkpoint.key).unwrap()).unwrap();
  checkpoint_key
}

fn reopen(path: &Path) -> aeordb::engine::v4::first_authority::V4FirstAuthorityPublisher {
  aeordb::engine::v4::first_authority::V4FirstAuthorityPublisher::open(path).unwrap()
}

fn install_runtime_fixture(
  fixture: &RuntimeFixture,
  selected_descriptor: &NativeIndexOperationDescriptorV1,
  coordinator_byte: u8,
) -> aeordb::engine::v4::index_runtime_installation::NativeIndexRuntimeInstallationReceiptV1 {
  let identity = fixture.identity();
  install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [coordinator_byte; 16],
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: std::slice::from_ref(selected_descriptor),
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(fixture, id(coordinator_byte)),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(u64::from(coordinator_byte), 1_700_000_000_200),
      now_ms: 1_700_000_000_200,
    },
  )
  .unwrap()
}

#[test]
fn initializer_creates_one_private_reopenable_shadow_for_both_hash_widths_without_touching_source() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.aeordb");
    fs::write(&source, b"source-evidence-must-not-change").unwrap();
    let source_before = fs::read(&source).unwrap();
    let path = directory.path().join(format!("shadow-{}.aeordb", algorithm.hash_length()));
    let destination = observe_migration_destination_path_v1(&path).unwrap();
    let permit = permit(algorithm, &destination);
    let cancellation = CancellationToken::new();

    let initialized = initialize_migration_destination_v1(MigrationDestinationInitializationRequestV1 {
      permit: &permit,
      destination: &destination,
      created_at_ms: 1_700_000_000_000,
      writer_fence_epoch: 7,
      cancellation: &cancellation,
    })
    .unwrap();

    assert_eq!(fs::read(&source).unwrap(), source_before);
    assert_eq!(initialized.path(), destination.path());
    assert_eq!(initialized.path_digest(), permit.destination_path_digest());
    assert_eq!(initialized.parent_identity(), permit.destination_parent_identity());
    assert!(initialized
      .file_identity()
      .represents_same_physical_file_as(aeordb::engine::native_durability::platform_file_identity(&path).unwrap()));
    let observed = initialized.publisher().observe().unwrap();
    let header = &observed.selected.header;
    assert!(!observed.selected.redundancy_degraded);
    assert_eq!(header.database_id, permit.database_id());
    assert_eq!(header.physical_instance_id, permit.destination_physical_instance_id());
    assert_eq!(header.writer_fence_epoch, 7);
    assert_eq!(header.required_reader_capabilities, permit.required_reader_capabilities().into_bytes());
    assert_eq!(header.required_writer_capabilities, permit.required_writer_capabilities().into_bytes());
    assert_eq!(header.kv_block_offset, DATABASE_HEADER_V4_DATA_OFFSET);
    assert_eq!(header.head_hash, initialized.first_authority().namespace_root.root_hash);
    let namespace_root = decode_namespace_root(&initialized.first_authority().namespace_root.value, algorithm).unwrap();
    assert_eq!(namespace_root.namespace_tree_root, digest_parts(algorithm, &[b"dirc:"]));
    assert!(initialized.publisher().locator(&header.head_hash).unwrap().is_some());
    let expected_namespace_root = header.head_hash.clone();

    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    drop(initialized);
    let reopened = reopen(&path);
    let reopened_header = reopened.observe().unwrap().selected.header;
    assert_eq!(reopened_header.database_id, permit.database_id());
    assert_eq!(reopened_header.physical_instance_id, permit.destination_physical_instance_id());
    assert_eq!(reopened_header.head_hash, expected_namespace_root);
    assert!(reopened.locator(&reopened_header.head_hash).unwrap().is_some());
  }
}

#[test]
fn content_only_shadow_runtime_installs_once_after_exact_identity_and_cancellation_checks() {
  let directory = tempfile::tempdir().unwrap();
  let source_path = directory.path().join("runtime-source.aeordb");
  let bootstrap_source = source_engine(&source_path, &directory.path().join("spill"));
  DirectoryOps::new(&bootstrap_source)
    .store_file_buffered(&RequestContext::system(), "/bootstrap.txt", b"bootstrap", Some("text/plain"))
    .unwrap();
  bootstrap_source.shutdown().unwrap();
  drop(bootstrap_source);
  let source = StorageEngine::open(source_path.to_str().unwrap()).unwrap();
  let shadow_path = directory.path().join("runtime-shadow.aeordb");
  let destination = observe_migration_destination_path_v1(&shadow_path).unwrap();
  let permit = permit_with_source_identity(HashAlgorithm::Blake3_256, &destination, platform_file_identity(&source_path).unwrap());
  let cancellation = CancellationToken::new();
  let initialized = initialize_migration_destination_v1(MigrationDestinationInitializationRequestV1 {
    permit: &permit,
    destination: &destination,
    created_at_ms: 1_700_000_000_000,
    writer_fence_epoch: 7,
    cancellation: &cancellation,
  })
  .unwrap();
  let retirement = retirement_owner(&source, permit.database_id(), &cancellation);
  let clock: Arc<dyn VirtualClock> = Arc::new(MockClock::new(1, 1_700_000_000_201));
  let identity = IndexRuntimeShadowIdentityV1::from_preflight(&permit);

  assert_eq!(
    source.admit_index_maintenance_task_v1([0x41; 16], IndexProducerMaintenanceClassV1::Reindex, "/runtime").unwrap(),
    None,
    "an engine without an installed runtime must preserve the legacy-only path"
  );
  assert!(source.index_coverage_registry_snapshot_v1().unwrap().is_none(), "ordinary v3 create/open must not install a v4 coverage cache");

  let canceled = CancellationToken::new();
  canceled.cancel();
  let error = install_native_index_runtime_v1(
    &source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x51; 16],
      shadow_identity: &identity,
      publisher: initialized.shared_publisher(),
      retirement_owner: Arc::clone(&retirement),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(directory.path(), permit.hash_algorithm(), permit.database_id(), id(0x51)),
      cancellation: &canceled,
      clock: Arc::clone(&clock),
      now_ms: 1_700_000_000_200,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Canceled));
  assert!(source.index_runtime_snapshot_v1().is_none(), "cancellation must not consume the one-time runtime slot");

  let invalid_coverage_request = IndexCoverageRegistryOwnerRequestV1::new(
    IndexCoverageRegistryOwnerKindV1::ScopeCatalog,
    vec![0x91],
    IndexCoverageGenerationHealthV1::Healthy,
  )
  .unwrap();
  let coverage_memory_before = source.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes;
  let error = install_native_index_runtime_v1(
    &source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x5f; 16],
      shadow_identity: &identity,
      publisher: initialized.shared_publisher(),
      retirement_owner: Arc::clone(&retirement),
      operation_descriptors: &[],
      coverage_owner_requests: std::slice::from_ref(&invalid_coverage_request),
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(directory.path(), permit.hash_algorithm(), permit.database_id(), id(0x5f)),
      cancellation: &cancellation,
      clock: Arc::clone(&clock),
      now_ms: 1_700_000_000_200,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Coverage(ref error) if error.code() == "index_coverage_refresh_invalid"));
  assert!(source.index_runtime_snapshot_v1().is_none(), "invalid coverage ownership must not consume the runtime slot");
  assert_eq!(
    source.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes,
    coverage_memory_before,
    "invalid coverage ownership must release its provisional registry and request reservations"
  );

  let receipt = install_native_index_runtime_v1(
    &source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x52; 16],
      shadow_identity: &identity,
      publisher: initialized.shared_publisher(),
      retirement_owner: retirement,
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(directory.path(), permit.hash_algorithm(), permit.database_id(), id(0x52)),
      cancellation: &cancellation,
      clock,
      now_ms: 1_700_000_000_201,
    },
  )
  .unwrap();
  assert!(receipt.content_only);
  assert_eq!(receipt.recovered_scopes, 0);
  assert_eq!(receipt.highest_checkpoint_sequence, 0);
  assert_eq!(source.index_runtime_snapshot_v1().unwrap().lifecycle, IndexRuntimeLifecycleV1::Running);
  let coverage = source.index_coverage_registry_snapshot_v1().unwrap().expect("migration-qualified runtime coverage registry");
  assert!(coverage.is_empty(), "content-only authority must install an explicit empty registry");
  let coverage_runtime = source.index_runtime_coverage_snapshot_v1().unwrap().expect("installed coverage lifecycle");
  assert_eq!(coverage_runtime.refresh_attempts, 1);
  assert_eq!(coverage_runtime.successful_refreshes, 1);
  assert_eq!(coverage_runtime.failed_refreshes, 0);
  assert!(!coverage_runtime.refresh_pending);
  assert_eq!(coverage_runtime.registry_entries, 0);
  assert_eq!(coverage_runtime.scope_ordinal_cache.entries, 0);
  DirectoryOps::new(&source).store_file_buffered(&RequestContext::system(), "/runtime-routed.txt", b"runtime", Some("text/plain")).unwrap();
  assert_eq!(source.index_runtime_snapshot_v1().unwrap().soft_hub.queued_notices, 1);
  assert_eq!(
    source.admit_index_maintenance_task_v1([0x42; 16], IndexProducerMaintenanceClassV1::Reindex, "/runtime").unwrap(),
    Some(IndexProducerAdmissionV1::Queued)
  );
  assert_eq!(
    source.admit_index_maintenance_task_v1([0x42; 16], IndexProducerMaintenanceClassV1::Reindex, "/runtime").unwrap(),
    Some(IndexProducerAdmissionV1::Duplicate)
  );
  assert_eq!(source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, 1);
  assert!(
    source.admit_index_maintenance_task_v1([0x43; 16], IndexProducerMaintenanceClassV1::Repair, "runtime").is_err(),
    "malformed scope must not enter the root-pinned maintenance doorway"
  );
  assert_eq!(source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, 1);
  let journal_memory_before = source.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes;
  assert!(journal_memory_before > 0);
  let normal_memory_policy = source.memory_coordinator().snapshot().unwrap().policy.unwrap();
  source.memory_coordinator().reconfigure_policy(MemoryPolicy::new(1, 2, 1, 1).unwrap()).unwrap();
  assert!(matches!(source.flush_index_runtime_if_due_v1(), Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Memory(_)))));
  let after_memory_refusal = source.index_runtime_snapshot_v1().unwrap();
  assert_eq!(after_memory_refusal.soft_hub.queued_notices, 1, "memory refusal must restore the leased source notice");
  assert_eq!(after_memory_refusal.producer.pending_tasks, 1, "memory refusal must not admit a journal task");
  assert_eq!(
    source.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes,
    journal_memory_before,
    "memory refusal must release the failed journal reservation and restored lease"
  );
  source.memory_coordinator().reconfigure_policy(normal_memory_policy).unwrap();

  source.flush_index_runtime_if_due_v1().unwrap().expect("installed runtime must service its cadence");
  let after_journal = source.index_runtime_snapshot_v1().unwrap();
  assert_eq!(after_journal.soft_hub.queued_notices, 0, "durable journal and task evidence must retire the leased notice");
  assert_eq!(after_journal.producer.pending_tasks, 0, "one bounded cadence slice must service both small retained tasks");
  assert_eq!(after_journal.producer.completed_tasks, 2);
  assert_eq!(
    source.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes,
    journal_memory_before,
    "journal encoding and persistence must release their temporary working reservation"
  );
  source.flush_index_runtime_if_due_v1().unwrap().expect("an idle follow-up cadence tick must remain valid");
  let after_second_tick = source.index_runtime_snapshot_v1().unwrap();
  assert_eq!(after_second_tick.producer.pending_tasks, 0);
  assert_eq!(after_second_tick.producer.completed_tasks, 2);

  let duplicate = install_native_index_runtime_v1(
    &source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x53; 16],
      shadow_identity: &identity,
      publisher: initialized.shared_publisher(),
      retirement_owner: retirement_owner(&source, permit.database_id(), &cancellation),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(directory.path(), permit.hash_algorithm(), permit.database_id(), id(0x53)),
      cancellation: &cancellation,
      clock: Arc::new(MockClock::new(2, 1_700_000_000_202)),
      now_ms: 1_700_000_000_202,
    },
  )
  .unwrap_err();
  assert!(matches!(duplicate, NativeIndexRuntimeInstallationErrorV1::AlreadyInstalled));
}

#[test]
fn native_runtime_activation_preserves_legacy_query_results_and_cached_observability() {
  let fixture = RuntimeFixture::new("runtime-legacy-query-authority");
  let ops = DirectoryOps::new(&fixture.source);
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![IndexFieldConfig { name: "@hash".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  ops
    .store_file_buffered(&RequestContext::system(), "/legacy/.aeordb-config/indexes.json", &config.serialize(), Some("application/json"))
    .unwrap();
  let contents = br#"{"authority":"legacy-v3"}"#;
  ops.store_file_with_indexing(&RequestContext::system(), "/legacy/document.json", contents, Some("application/json")).unwrap();
  let content_hash = blake3::hash(contents).to_hex().to_string();
  let query = || QueryBuilder::new(&fixture.source, "/legacy").field("@hash").eq(content_hash.as_bytes()).all().unwrap();
  let before = query();
  assert_eq!(before.len(), 1);
  assert_eq!(before[0].file_record.path, "/legacy/document.json");
  drop(before);

  let identity = fixture.identity();
  let receipt = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: id(0x54),
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, id(0x55)),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(3, 1_700_000_000_203),
      now_ms: 1_700_000_000_203,
    },
  )
  .unwrap();
  assert_eq!(receipt.lifecycle, IndexRuntimeLifecycleV1::Running);

  let after = query();
  assert_eq!(after.len(), 1);
  assert_eq!(after[0].file_record.path, "/legacy/document.json");
  let cached = fixture.source.index_runtime_snapshot_v1().expect("installed runtime cached snapshot");
  assert_eq!(cached.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(cached.highest_checkpoint_sequence, 0);
  let observed = fixture.source.runtime_observability_snapshot(ConfigurationVisibility::Root).unwrap();
  assert_eq!(observed.index_runtime.state, "running");
  assert!(observed.index_runtime.installed);
  assert_eq!(observed.index_runtime.producer.pending_tasks, cached.producer.pending_tasks);
  assert_eq!(observed.index_runtime.mutations.active_bytes, cached.mutations.active_bytes);
  assert_eq!(observed.index_runtime.coverage.registry_entries, 0);
  assert_eq!(observed.index_runtime.coverage.successful_refreshes, 1);
  assert!(observed.index_runtime.coverage.last_failure.is_none());
  assert_eq!(observed.index_runtime.scope_ordinal_cache.entries, 0);
}

#[test]
fn migration_runtime_tracks_real_selected_coverage_without_a_second_selector_and_pinned_snapshots_survive_shutdown() {
  let fixture = RuntimeFixture::new("runtime-selected-coverage-lifecycle");
  DirectoryOps::new(&fixture.source)
    .store_file_buffered(&RequestContext::system(), "/coverage-bootstrap.txt", b"coverage bootstrap", Some("text/plain"))
    .unwrap();
  fixture.source.shutdown().unwrap();
  let RuntimeFixture { _directory, source_path, source, permit, destination, cancellation } = fixture;
  drop(source);
  let source = StorageEngine::open(source_path.to_str().unwrap()).unwrap();
  let fixture = RuntimeFixture { _directory, source_path, source, permit, destination, cancellation };
  let algorithm = fixture.permit.hash_algorithm();
  let chain = RuntimeCoverageManifestChain::new(algorithm);
  let retirement = fixture.retirement();
  publish_runtime_coverage_artifacts(&fixture, &[&chain.scope, &chain.value, &chain.field, &chain.nvt], 1_700_000_000_210);
  publish_runtime_coverage_pointer(&fixture, &retirement, ActivePointerKindV1::ScopeCatalog, &chain.scope, 0, 1, 1_700_000_000_211);
  publish_runtime_coverage_pointer(&fixture, &retirement, ActivePointerKindV1::FieldIndex, &chain.field, 0, 1, 1_700_000_000_212);
  publish_runtime_coverage_pointer(&fixture, &retirement, ActivePointerKindV1::FieldNvt, &chain.nvt, 0, 1, 1_700_000_000_213);
  let requests = [
    IndexCoverageRegistryOwnerRequestV1::new(
      IndexCoverageRegistryOwnerKindV1::ScopeCatalog,
      chain.scope_owner.clone(),
      IndexCoverageGenerationHealthV1::Healthy,
    )
    .unwrap(),
    IndexCoverageRegistryOwnerRequestV1::new(
      IndexCoverageRegistryOwnerKindV1::FieldIndex,
      chain.field_owner.clone(),
      IndexCoverageGenerationHealthV1::Healthy,
    )
    .unwrap(),
  ];
  let identity = fixture.identity();
  let runtime_clock = Arc::new(MockClock::new(4, 1_700_000_000_214));
  install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: id(0x56),
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: Arc::clone(&retirement),
      operation_descriptors: &[],
      coverage_owner_requests: &requests,
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, id(0x57)),
      cancellation: &fixture.cancellation,
      clock: runtime_clock.clone(),
      now_ms: 1_700_000_000_214,
    },
  )
  .unwrap();

  let first = fixture.source.index_coverage_registry_snapshot_v1().unwrap().unwrap();
  assert_eq!(first.len(), 2);
  assert!(matches!(
    first.entry(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, &chain.scope_owner).unwrap().selection(),
    IndexCoverageRegistrySelectionV1::Selected(_)
  ));
  assert!(matches!(
    first.entry(IndexCoverageRegistryOwnerKindV1::FieldIndex, &chain.field_owner).unwrap().nvt_status(),
    IndexCoverageNvtStatusV1::Usable(_)
  ));
  let first_scope_generation = match first.entry(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, &chain.scope_owner).unwrap().selection() {
    IndexCoverageRegistrySelectionV1::Selected(generation) => generation.generation(),
    IndexCoverageRegistrySelectionV1::Unavailable(reason) => panic!("initial scope coverage unavailable: {reason:?}"),
  };

  let successor = chain.scope_successor(algorithm);
  publish_runtime_coverage_artifacts(&fixture, &[&successor], 1_700_000_000_220);
  publish_runtime_coverage_pointer(&fixture, &retirement, ActivePointerKindV1::ScopeCatalog, &successor, 1, 2, 1_700_000_000_221);
  DirectoryOps::new(&fixture.source)
    .store_file_buffered(
      &RequestContext::system(),
      "/coverage-refresh-trigger.txt",
      b"refresh selected coverage through the installed cadence",
      Some("text/plain"),
    )
    .unwrap();
  fixture.source.flush_index_runtime_if_due_v1().unwrap().expect("installed cadence must refresh changed selection");
  let second = fixture.source.index_coverage_registry_snapshot_v1().unwrap().unwrap();
  let second_scope_generation = match second.entry(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, &chain.scope_owner).unwrap().selection()
  {
    IndexCoverageRegistrySelectionV1::Selected(generation) => generation.generation(),
    IndexCoverageRegistrySelectionV1::Unavailable(reason) => panic!("replacement scope coverage unavailable: {reason:?}"),
  };
  assert_eq!(second_scope_generation, first_scope_generation + 1);
  assert_eq!(
    match first.entry(IndexCoverageRegistryOwnerKindV1::ScopeCatalog, &chain.scope_owner).unwrap().selection() {
      IndexCoverageRegistrySelectionV1::Selected(generation) => generation.generation(),
      IndexCoverageRegistrySelectionV1::Unavailable(reason) => panic!("pinned scope coverage unavailable: {reason:?}"),
    },
    first_scope_generation,
    "atomic replacement must not mutate an in-flight generation"
  );
  let lifecycle = fixture.source.index_runtime_coverage_snapshot_v1().unwrap().unwrap();
  assert_eq!(lifecycle.refresh_attempts, 2);
  assert_eq!(lifecycle.successful_refreshes, 2);
  assert_eq!(lifecycle.failed_refreshes, 0);
  assert_eq!(lifecycle.registry_entries, 2);
  assert!(lifecycle.owner_requests_retained_bytes > 0);
  assert_eq!(
    lifecycle.total_retained_bytes,
    lifecycle.registry_retained_bytes.checked_add(lifecycle.owner_requests_retained_bytes).unwrap()
  );

  let normal_memory_policy = fixture.source.memory_coordinator().snapshot().unwrap().policy.unwrap();
  assert_eq!(
    fixture.source.admit_index_maintenance_task_v1([0x62; 16], IndexProducerMaintenanceClassV1::Reindex, "/").unwrap(),
    Some(IndexProducerAdmissionV1::Queued)
  );
  fixture.source.memory_coordinator().reconfigure_policy(MemoryPolicy::new(1, 2, 1, 1).unwrap()).unwrap();
  let pressure = fixture.source.flush_index_runtime_if_due_v1().unwrap_err();
  assert!(pressure.to_string().contains("index coverage registry memory admission failed"));
  let retained_after_pressure = fixture.source.index_coverage_registry_snapshot_v1().unwrap().unwrap();
  assert_eq!(retained_after_pressure.entries(), second.entries(), "refresh pressure must retain prior selected coverage");
  let failed_lifecycle = fixture.source.index_runtime_coverage_snapshot_v1().unwrap().unwrap();
  assert_eq!(failed_lifecycle.refresh_attempts, 3);
  assert_eq!(failed_lifecycle.successful_refreshes, 2);
  assert_eq!(failed_lifecycle.failed_refreshes, 1);
  assert!(failed_lifecycle.refresh_pending);
  assert_eq!(failed_lifecycle.last_failure.as_ref().unwrap().code, "index_coverage_refresh_memory");
  let root_observability = fixture.source.runtime_observability_snapshot(ConfigurationVisibility::Root).unwrap();
  assert_ne!(root_observability.index_runtime.coverage.last_failure.as_ref().unwrap().context, "<redacted>");
  let public_observability = fixture.source.runtime_observability_snapshot(ConfigurationVisibility::Redacted).unwrap();
  assert_eq!(public_observability.index_runtime.coverage.last_failure.as_ref().unwrap().code, "index_coverage_refresh_memory");
  assert_eq!(public_observability.index_runtime.coverage.last_failure.as_ref().unwrap().context, "<redacted>");
  fixture.source.memory_coordinator().reconfigure_policy(normal_memory_policy).unwrap();
  runtime_clock.advance(100);
  fixture.source.flush_index_runtime_if_due_v1().unwrap().expect("pending cadence refresh must recover after pressure clears");
  let recovered_lifecycle = fixture.source.index_runtime_coverage_snapshot_v1().unwrap().unwrap();
  assert_eq!(recovered_lifecycle.refresh_attempts, 4);
  assert_eq!(recovered_lifecycle.successful_refreshes, 3);
  assert_eq!(recovered_lifecycle.failed_refreshes, 1);
  assert!(!recovered_lifecycle.refresh_pending);
  assert!(recovered_lifecycle.last_failure.is_none());

  fixture.source.shutdown().unwrap();
  assert_eq!(first.len(), 2, "shutdown must not invalidate an in-flight immutable coverage snapshot");
  assert_eq!(second.len(), 2);

  let RuntimeFixture { _directory, source_path, source, permit, destination, cancellation } = fixture;
  let shadow_path = destination.path().to_path_buf();
  let database_id = permit.database_id();
  drop(source);
  drop(destination);
  drop(cancellation);

  let reopened_source = StorageEngine::open(source_path.to_str().unwrap()).unwrap();
  let reopened_publisher = Arc::new(reopen(&shadow_path));
  let reopened_cancellation = CancellationToken::new();
  let reopened_retirement = retirement_owner(&reopened_source, database_id, &reopened_cancellation);
  install_native_index_runtime_v1(
    &reopened_source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: id(0x56),
      shadow_identity: &identity,
      publisher: reopened_publisher,
      retirement_owner: reopened_retirement,
      operation_descriptors: &[],
      coverage_owner_requests: &requests,
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(_directory.path(), permit.hash_algorithm(), database_id, id(0x57)),
      cancellation: &reopened_cancellation,
      clock: Arc::new(MockClock::new(5, 1_700_000_000_230)),
      now_ms: 1_700_000_000_230,
    },
  )
  .unwrap();
  let reconstructed = reopened_source.index_coverage_registry_snapshot_v1().unwrap().unwrap();
  assert_eq!(reconstructed.entries(), second.entries());
  let reconstructed_lifecycle = reopened_source.index_runtime_coverage_snapshot_v1().unwrap().unwrap();
  assert_eq!(reconstructed_lifecycle.refresh_attempts, 1);
  assert_eq!(reconstructed_lifecycle.successful_refreshes, 1);
  assert_eq!(reconstructed_lifecycle.failed_refreshes, 0);
}

#[test]
fn post_commit_legacy_index_mutations_admit_root_pinned_v4_maintenance_tasks() {
  let fixture = RuntimeFixture::new("runtime-post-commit-maintenance-routing");
  let ops = DirectoryOps::new(&fixture.source);
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![IndexFieldConfig { name: "@hash".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  ops
    .store_file_buffered(&RequestContext::system(), "/legacy/.aeordb-config/indexes.json", &config.serialize(), Some("application/json"))
    .unwrap();
  fixture.install_content_only_runtime(id(0x81), id(0x82), 1_700_000_000_810);
  let baseline = fixture.source.index_runtime_snapshot_v1().unwrap();

  ops.store_file_with_indexing(&RequestContext::system(), "/legacy/routed.json", br#"{"route":"v4"}"#, Some("application/json")).unwrap();
  let after_write = fixture.source.index_runtime_snapshot_v1().unwrap();
  assert!(
    after_write.soft_hub.queued_notices > baseline.soft_hub.queued_notices,
    "the user publication must retain its independent soft notice"
  );
  assert_eq!(
    after_write.producer.pending_tasks,
    baseline.producer.pending_tasks + 1,
    "the direct legacy metadata mutation must enter maintenance admission once"
  );

  ops.delete_file(&RequestContext::system(), "/legacy/routed.json").unwrap();
  let after_delete = fixture.source.index_runtime_snapshot_v1().unwrap();
  assert!(
    after_delete.soft_hub.queued_notices > after_write.soft_hub.queued_notices,
    "the delete publication must retain its independent soft notice"
  );
  assert_eq!(
    after_delete.producer.pending_tasks,
    baseline.producer.pending_tasks + 2,
    "legacy delete cleanup must enter maintenance admission once"
  );
}

#[test]
fn explicit_legacy_index_wrapper_does_not_admit_when_no_configuration_applies() {
  let fixture = RuntimeFixture::new("runtime-no-applicable-legacy-index");
  fixture.install_content_only_runtime(id(0x83), id(0x84), 1_700_000_000_820);
  let baseline = fixture.source.index_runtime_snapshot_v1().unwrap();

  DirectoryOps::new(&fixture.source)
    .store_file_with_indexing(&RequestContext::system(), "/plain/no-index.json", br#"{"index":false}"#, Some("application/json"))
    .unwrap();

  let after = fixture.source.index_runtime_snapshot_v1().unwrap();
  assert_eq!(after.producer.pending_tasks, baseline.producer.pending_tasks);
  assert!(after.soft_hub.queued_notices > baseline.soft_hub.queued_notices);
}

#[test]
fn public_legacy_index_mutation_boundaries_cannot_bypass_v4_maintenance_admission() {
  let fixture = RuntimeFixture::new("runtime-public-legacy-maintenance-routing");
  let ops = DirectoryOps::new(&fixture.source);
  let field = IndexFieldConfig { name: "@hash".to_string(), index_type: "string".to_string(), source: None, min: None, max: None };
  let config = PathIndexConfig { parser: None, parser_memory_limit: None, logging: false, glob: None, indexes: vec![field.clone()] };
  ops
    .store_file_buffered(&RequestContext::system(), "/legacy/.aeordb-config/indexes.json", &config.serialize(), Some("application/json"))
    .unwrap();
  let contents = br#"{"route":"public-pipeline"}"#;
  ops.store_file_buffered(&RequestContext::system(), "/legacy/public.json", contents, Some("application/json")).unwrap();
  ops.store_file_buffered(&RequestContext::system(), "/legacy/buffered.json", contents, Some("application/json")).unwrap();
  fixture.install_content_only_runtime(id(0x91), id(0x92), 1_700_000_000_860);
  let baseline = fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks;

  IndexingPipeline::new(&fixture.source).run(&RequestContext::system(), "/legacy/public.json", contents, Some("application/json")).unwrap();
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, baseline + 1);

  let mut buffer = IndexWriteBuffer::new(&fixture.source, IndexWriteBufferOptions::default());
  IndexingPipeline::new(&fixture.source)
    .run_buffered(&RequestContext::system(), "/legacy/buffered.json", contents, Some("application/json"), &mut buffer)
    .unwrap();
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, baseline + 2);

  buffer.update_index("/legacy", "@hash", &field, &[b"buffer-direct".to_vec()], &[0x93; 32]).unwrap();
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, baseline + 3);

  IndexManager::new(&fixture.source).update_index("/legacy/direct", "@hash", &field, &[b"direct".to_vec()], &[0x94; 32]).unwrap();
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, baseline + 4);

  assert_eq!(IndexManager::new(&fixture.source).delete_indexes_not_in_config("/legacy", &config).unwrap(), 0);
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, baseline + 5);
  assert_eq!(IndexManager::new(&fixture.source).delete_indexes_not_in_config("/legacy", &config).unwrap(), 0);
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, baseline + 5);
  assert!(!ops.migrate_file_record_to_current_version("/legacy/public.json").unwrap());
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, baseline + 6);
  assert!(!ops.migrate_file_record_to_current_version("/legacy/public.json").unwrap());
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, baseline + 6);
  assert!(!ops.repair_stale_dir_key("/legacy").unwrap());
  assert_eq!(
    fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks,
    baseline + 7,
    "successful no-op retries must preserve their original maintenance intent"
  );
  assert!(!ops.repair_stale_dir_key("/legacy").unwrap());
  assert_eq!(
    fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks,
    baseline + 7,
    "successful no-op retries must collapse against their exact retained task"
  );
}

#[test]
fn public_legacy_pipeline_surfaces_v4_admission_failure_after_preserving_legacy_mutation() {
  let fixture = RuntimeFixture::new("runtime-public-legacy-maintenance-failure");
  let ops = DirectoryOps::new(&fixture.source);
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![IndexFieldConfig { name: "@hash".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  ops
    .store_file_buffered(&RequestContext::system(), "/legacy/.aeordb-config/indexes.json", &config.serialize(), Some("application/json"))
    .unwrap();
  let contents = br#"{"route":"failed-admission"}"#;
  ops.store_file_buffered(&RequestContext::system(), "/legacy/failure.json", contents, Some("application/json")).unwrap();
  fixture.install_content_only_runtime(id(0x95), id(0x96), 1_700_000_000_870);
  fixture.cancellation.cancel();

  let error = IndexingPipeline::new(&fixture.source)
    .run(&RequestContext::system(), "/legacy/failure.json", contents, Some("application/json"))
    .unwrap_err();
  assert!(matches!(error, aeordb::engine::EngineError::DurabilityFailure(_)), "unexpected error: {error}");
  assert!(IndexManager::new(&fixture.source).load_index("/legacy", "@hash").unwrap().is_some());
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, 0);
}

#[test]
fn post_commit_maintenance_admission_failure_cannot_reverse_legacy_write_or_delete() {
  let fixture = RuntimeFixture::new("runtime-post-commit-admission-failure");
  let ops = DirectoryOps::new(&fixture.source);
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![IndexFieldConfig { name: "@hash".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  ops
    .store_file_buffered(&RequestContext::system(), "/legacy/.aeordb-config/indexes.json", &config.serialize(), Some("application/json"))
    .unwrap();
  fixture.install_content_only_runtime(id(0x85), id(0x86), 1_700_000_000_830);
  fixture.cancellation.cancel();

  ops
    .store_file_with_indexing(&RequestContext::system(), "/legacy/durable.json", br#"{"durable":true}"#, Some("application/json"))
    .expect("maintenance cancellation is recoverable-soft after the file commit");
  assert_eq!(ops.read_file_buffered("/legacy/durable.json").unwrap(), br#"{"durable":true}"#);
  ops
    .delete_file(&RequestContext::system(), "/legacy/durable.json")
    .expect("maintenance cancellation is recoverable-soft after the delete commit");
  assert!(matches!(ops.read_file_buffered("/legacy/durable.json"), Err(aeordb::engine::EngineError::NotFound(_))));
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, 0);
}

#[test]
fn post_commit_pressure_and_spill_refusal_cannot_reverse_legacy_write_or_delete() {
  let fixture = RuntimeFixture::new("runtime-post-commit-spill-refusal");
  let operations = DirectoryOps::new(&fixture.source);
  let config = PathIndexConfig {
    parser: None,
    parser_memory_limit: None,
    logging: false,
    glob: None,
    indexes: vec![IndexFieldConfig { name: "@hash".to_string(), index_type: "string".to_string(), source: None, min: None, max: None }],
  };
  operations
    .store_file_buffered(&RequestContext::system(), "/legacy/.aeordb-config/indexes.json", &config.serialize(), Some("application/json"))
    .unwrap();

  let mut owner_options = runtime_options();
  owner_options.producer = IndexProducerCoordinatorOptionsV1::new(1, 2 * 1_024 * 1_024, 3, 10, 1_000, 16, 256, 2 * 1_024 * 1_024).unwrap();
  let workspace_id = id(0xa1);
  let workspace_root = fixture._directory.path().join(format!("runtime-{}", hex::encode(workspace_id)));
  fs::create_dir_all(&workspace_root).unwrap();
  let workspace_options = IndexRuntimeWorkspaceOptionsV1::new(Some(workspace_root), 397, 0, 32).unwrap();
  let publisher_options = NativeIndexRuntimePublisherOptionsV1::new(
    descriptor_with_algorithm(fixture.permit.hash_algorithm(), fixture.permit.database_id(), 0xd1, 0xd2, 0xd3),
    workspace_id,
    1,
    workspace_options,
  )
  .unwrap();
  install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: id(0xa2),
      shadow_identity: &fixture.identity(),
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: owner_options,
      recovery_options: native_recovery_options(),
      runtime_publisher: publisher_options,
      cancellation: &fixture.cancellation,
      clock: fixture.clock(80, 1_700_000_000_900),
      now_ms: 1_700_000_000_900,
    },
  )
  .unwrap();
  assert_eq!(
    fixture.source.admit_index_maintenance_task_v1(id(0xa3), IndexProducerMaintenanceClassV1::Repair, "/occupied").unwrap(),
    Some(IndexProducerAdmissionV1::Queued)
  );

  let contents = br#"{"durable":"despite-spill-refusal"}"#;
  operations
    .store_file_with_indexing(&RequestContext::system(), "/legacy/durable.json", contents, Some("application/json"))
    .expect("post-commit spill refusal cannot reverse an acknowledged file write");
  assert_eq!(operations.read_file_buffered("/legacy/durable.json").unwrap(), contents);
  assert!(IndexManager::new(&fixture.source).load_index("/legacy", "@hash").unwrap().is_some());

  operations
    .delete_file(&RequestContext::system(), "/legacy/durable.json")
    .expect("post-commit spill refusal cannot reverse an acknowledged file deletion");
  assert!(matches!(operations.read_file_buffered("/legacy/durable.json"), Err(aeordb::engine::EngineError::NotFound(_))));
  assert_eq!(IndexManager::new(&fixture.source).load_index("/legacy", "@hash").unwrap().unwrap().len(), 0);

  let runtime = fixture.source.index_runtime_snapshot_v1().unwrap();
  assert_eq!(runtime.producer.pending_tasks, 1, "the admitted task must remain retained");
  assert_eq!(runtime.producer.spilled_tasks, 0, "a refused spill cannot be reported as durable");
}

#[test]
fn maintenance_batch_admission_uses_one_stable_operation_and_retries_as_exact_duplicates() {
  let fixture = RuntimeFixture::new("runtime-maintenance-batch-admission");
  DirectoryOps::new(&fixture.source).store_file_buffered(&RequestContext::system(), "/seed.txt", b"seed", Some("text/plain")).unwrap();
  fixture.install_content_only_runtime(id(0x87), id(0x88), 1_700_000_000_840);
  let targets = [
    IndexProducerMaintenanceTargetV1 { class: IndexProducerMaintenanceClassV1::ConfigurationRetirement, scope: "/configured" },
    IndexProducerMaintenanceTargetV1 { class: IndexProducerMaintenanceClassV1::LegacyMigration, scope: "/requested" },
    IndexProducerMaintenanceTargetV1 { class: IndexProducerMaintenanceClassV1::Reindex, scope: "/requested" },
  ];

  assert_eq!(fixture.source.admit_index_maintenance_tasks_v1(id(0x89), &targets).unwrap(), Some(vec![IndexProducerAdmissionV1::Queued; 3]));
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, 3);
  assert_eq!(
    fixture.source.admit_index_maintenance_tasks_v1(id(0x89), &targets).unwrap(),
    Some(vec![IndexProducerAdmissionV1::Duplicate; 3])
  );
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, 3);

  let duplicate_targets = [
    IndexProducerMaintenanceTargetV1 { class: IndexProducerMaintenanceClassV1::Repair, scope: "/duplicate" },
    IndexProducerMaintenanceTargetV1 { class: IndexProducerMaintenanceClassV1::Repair, scope: "/duplicate" },
  ];
  assert!(fixture.source.admit_index_maintenance_tasks_v1(id(0x8a), &duplicate_targets).is_err());
  assert_eq!(
    fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks,
    3,
    "malformed batches must fail before admitting their first target"
  );
  assert!(fixture.source.admit_index_maintenance_tasks_v1(id(0x8b), &[]).is_err());
  let too_many = [IndexProducerMaintenanceTargetV1 { class: IndexProducerMaintenanceClassV1::Repair, scope: "/bounded" }; 9];
  assert!(fixture.source.admit_index_maintenance_tasks_v1(id(0x8c), &too_many).is_err());
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, 3);
}

#[test]
fn online_kv_repair_admits_one_root_pinned_repair_task_after_rebuild() {
  let fixture = RuntimeFixture::new("runtime-online-repair-admission");
  DirectoryOps::new(&fixture.source)
    .store_file_buffered(&RequestContext::system(), "/repair-seed.txt", b"seed", Some("text/plain"))
    .unwrap();
  fixture.install_content_only_runtime(id(0x8a), id(0x8b), 1_700_000_000_850);

  fixture.source.repair_kv_and_admit_index_maintenance_v1(id(0x8c)).unwrap();
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, 1);
}

#[test]
fn public_kv_rebuild_cannot_bypass_root_pinned_repair_admission() {
  let fixture = RuntimeFixture::new("runtime-public-kv-rebuild-admission");
  DirectoryOps::new(&fixture.source)
    .store_file_buffered(&RequestContext::system(), "/repair-public.txt", b"seed", Some("text/plain"))
    .unwrap();
  fixture.install_content_only_runtime(id(0x8d), id(0x8e), 1_700_000_000_855);

  fixture.source.rebuild_kv().unwrap();
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, 1);
}

#[test]
fn public_directory_rebuild_cannot_bypass_root_pinned_repair_admission() {
  let fixture = RuntimeFixture::new("runtime-public-directory-rebuild-admission");
  let operations = DirectoryOps::new(&fixture.source);
  operations.store_file_buffered(&RequestContext::system(), "/repair/tree/file.txt", b"seed", Some("text/plain")).unwrap();
  fixture.install_content_only_runtime(id(0x8f), id(0x90), 1_700_000_000_856);

  assert!(operations.rebuild_directory_tree(&RequestContext::system()).unwrap() > 0);
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().producer.pending_tasks, 1);
}

fn collect_rust_sources(path: &Path, files: &mut Vec<std::path::PathBuf>) {
  for entry in fs::read_dir(path).unwrap() {
    let entry = entry.unwrap();
    let path = entry.path();
    if path.is_dir() {
      collect_rust_sources(&path, files);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      files.push(path);
    }
  }
}

fn source_occurrences_by_file(root: &Path, needle: &str) -> BTreeMap<String, usize> {
  let mut paths = Vec::new();
  collect_rust_sources(root, &mut paths);
  paths
    .into_iter()
    .filter_map(|path| {
      let source = fs::read_to_string(&path).unwrap();
      let count = source.matches(needle).count();
      (count > 0).then(|| (path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"), count))
    })
    .collect()
}

#[test]
fn legacy_index_writer_bypasses_are_closed_to_reviewed_compatibility_adapters() {
  let engine_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine");
  let expected_unrouted = [
    ("index_store.rs", 19usize),
    ("indexing_pipeline.rs", 13),
    ("directory_ops.rs", 9),
    ("task_worker.rs", 3),
    ("index_cleanup.rs", 1),
    ("storage_engine.rs", 7),
    ("verify.rs", 4),
  ];
  let mut sources = Vec::new();
  collect_rust_sources(&engine_root, &mut sources);
  for path in sources {
    let source = fs::read_to_string(&path).unwrap();
    let relative = path.strip_prefix(&engine_root).unwrap();
    let expected = expected_unrouted.iter().find_map(|(file, count)| (relative == Path::new(file)).then_some(*count)).unwrap_or(0);
    assert_eq!(source.matches("_unrouted").count(), expected, "unreviewed raw legacy index writer in {}", relative.display());
  }

  let store = fs::read_to_string(engine_root.join("index_store.rs")).unwrap();
  assert_eq!(store.matches("admit_explicit_legacy_mutation(").count(), 8);
  assert_eq!(store.matches("admit_configuration_retirement(").count(), 2);
  let pipeline = fs::read_to_string(engine_root.join("indexing_pipeline.rs")).unwrap();
  assert_eq!(pipeline.matches("admit_explicit_legacy_mutation_if_applicable(").count(), 7);
  let directory = fs::read_to_string(engine_root.join("directory_ops.rs")).unwrap();
  assert_eq!(directory.matches("admit_implicit_index_maintenance_v1(").count(), 5);
  assert_eq!(directory.matches("IndexProducerMaintenanceClassV1::LegacyMigration").count(), 1);

  let expected_facade_callers: [(&str, &[(&str, usize)]); 4] = [
    ("admit_index_maintenance_task_v1(", &[("directory_ops.rs", 1), ("storage_engine.rs", 3)]),
    ("admit_index_maintenance_tasks_v1(", &[("storage_engine.rs", 2), ("task_worker.rs", 1)]),
    ("admit_implicit_index_maintenance_v1(", &[("directory_ops.rs", 5), ("index_store.rs", 1), ("storage_engine.rs", 5), ("verify.rs", 1)]),
    ("admit_explicit_legacy_index_mutation_v1(", &[("index_store.rs", 1), ("indexing_pipeline.rs", 1), ("storage_engine.rs", 1)]),
  ];
  for (needle, expected) in expected_facade_callers {
    let expected: BTreeMap<_, _> = expected.iter().map(|(file, count)| ((*file).to_string(), *count)).collect();
    assert_eq!(source_occurrences_by_file(&engine_root, needle), expected, "unreviewed maintenance facade caller for {needle}");
  }

  let query_engine = fs::read_to_string(engine_root.join("query_engine.rs")).unwrap();
  assert_eq!(query_engine.matches("IndexManager::new").count(), 2, "query engine no longer solely owns the retained v3 query adapter");
  let search = fs::read_to_string(engine_root.join("search.rs")).unwrap();
  assert!(!search.contains("IndexManager::new"), "search bypassed the shared current-or-selected query reader");
  assert!(search.contains("QueryEngine::with_request_budget"), "search no longer routes current reads through the shared query engine");
  assert!(
    search.contains("QueryEngine::with_read_source_and_budget"),
    "search no longer routes selected-root reads through the shared query engine"
  );

  for (reader, source) in [("query_engine.rs", query_engine), ("search.rs", search)] {
    for mutation in
      ["update_index(", "save_index(", "create_index(", "delete_index(", "delete_indexes_not_in_config(", "remove_file_from_index"]
    {
      assert!(!source.contains(mutation), "{reader} acquired legacy mutation authority through {mutation}");
    }
  }
}

#[test]
fn standalone_unbounded_index_cleanup_worker_cannot_return() {
  let manifest_root = Path::new(env!("CARGO_MANIFEST_DIR"));
  let cleanup = fs::read_to_string(manifest_root.join("src/engine/index_cleanup.rs")).unwrap();
  for forbidden in ["UnboundedSender", "unbounded_channel", "cleanup_loop", "spawn_index_cleanup_worker", "IndexCleanupSender"] {
    assert!(!cleanup.contains(forbidden), "standalone cleanup worker authority returned through {forbidden}");
  }

  let state = fs::read_to_string(manifest_root.join("src/server/state.rs")).unwrap();
  assert!(!state.contains("IndexCleanupSender"));
  assert!(!state.contains("index_cleanup:"));

  let server = fs::read_to_string(manifest_root.join("src/server/mod.rs")).unwrap();
  assert!(!server.contains("spawn_index_cleanup_worker"));
  let routes = fs::read_to_string(manifest_root.join("src/server/engine_routes.rs")).unwrap();
  assert!(!routes.contains(".index_cleanup.queue("));

  let engine = fs::read_to_string(manifest_root.join("src/engine/mod.rs")).unwrap();
  assert!(!engine.contains("pub use index_cleanup::{"));
}

#[test]
fn installed_producer_cadence_and_legacy_buffer_callers_are_closed() {
  let engine_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/engine");
  let expected: [(&str, &[(&str, usize)]); 9] = [
    ("IndexProducerCoordinatorV1::new(", &[("v4/index_runtime_owner.rs", 1)]),
    ("state.producer.admit(", &[("v4/index_runtime_owner.rs", 1)]),
    ("state.producer.admit_or_spill(", &[("v4/index_runtime_owner.rs", 1)]),
    ("state.producer.lease_next(", &[("v4/index_runtime_owner.rs", 1)]),
    ("NativeIndexRuntimeCadenceV1::new(", &[("v4/index_runtime_installation.rs", 1)]),
    ("NativeIndexCompactionExecutorV1::new(", &[("v4/index_runtime_installation.rs", 1)]),
    ("self.cadence.admit_task(", &[("v4/index_runtime_installation.rs", 1)]),
    ("self.cadence.service_bounded_producers(", &[("v4/index_runtime_installation.rs", 1)]),
    ("IndexWriteBuffer::new(", &[("directory_ops.rs", 1), ("task_worker.rs", 1)]),
  ];
  for (needle, callers) in expected {
    let callers: BTreeMap<_, _> = callers.iter().map(|(file, count)| ((*file).to_string(), *count)).collect();
    assert_eq!(source_occurrences_by_file(&engine_root, needle), callers, "unreviewed production caller for {needle}");
  }
}

#[test]
fn native_runtime_atomic_installation_rejects_invalid_publisher_authority_without_exposing_a_partial_runtime() {
  let fixture = RuntimeFixture::new("runtime-atomic-installation");
  let runtime_id = [0x61; 16];
  let identity = fixture.identity();
  let foreign_database = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(fixture._directory.path(), fixture.permit.hash_algorithm(), id(0x11), id(0xe0)),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(9, 1_700_000_000_320),
      now_ms: 1_700_000_000_320,
    },
  )
  .unwrap_err();
  assert!(matches!(
    foreign_database,
    NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_runtime_publisher_authority", .. }
  ));
  assert!(fixture.source.index_runtime_snapshot_v1().is_none());

  let invalid_clock = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, id(0xe1)),
      cancellation: &fixture.cancellation,
      clock: Arc::new(MockClock::new(10, 0)),
      now_ms: 1_700_000_000_330,
    },
  )
  .unwrap_err();
  assert!(matches!(invalid_clock, NativeIndexRuntimeInstallationErrorV1::PublisherStore(_)));
  assert!(fixture.source.index_runtime_snapshot_v1().is_none());

  install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, id(0xe2)),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(11, 1_700_000_000_340),
      now_ms: 1_700_000_000_340,
    },
  )
  .unwrap();
  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().lifecycle, IndexRuntimeLifecycleV1::Running);
}

#[test]
fn selected_runtime_dirty_overlay_resumes_after_source_and_destination_restart_without_claiming_coverage() {
  let fixture = RuntimeFixture::new("runtime-dirty-overlay-restart");
  let runtime_id = id(0x63);
  let workspace_id = id(0xe3);
  let selected = seed_selected_runtime_dirty_overlay(&fixture, runtime_id, workspace_id);
  assert_eq!(selected.durable_sequence(), 1);
  let identity = fixture.identity();
  let RuntimeFixture { _directory, source_path, source, permit, destination, cancellation } = fixture;
  let shadow_path = destination.path().to_path_buf();
  drop(source);
  drop(destination);
  drop(cancellation);

  let reopened_source = StorageEngine::open(source_path.to_str().unwrap()).unwrap();
  assert!(
    reopened_source.index_coverage_registry_snapshot_v1().unwrap().is_none(),
    "plain reopen must remain v3-only until migration recovery explicitly reinstalls the runtime"
  );
  let reopened_publisher = Arc::new(reopen(&shadow_path));
  let reopened_cancellation = CancellationToken::new();
  let receipt = install_native_index_runtime_v1(
    &reopened_source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: Arc::clone(&reopened_publisher),
      retirement_owner: retirement_owner(&reopened_source, permit.database_id(), &reopened_cancellation),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(_directory.path(), permit.hash_algorithm(), permit.database_id(), workspace_id),
      cancellation: &reopened_cancellation,
      clock: Arc::new(MockClock::new(32, 1_700_000_000_600)),
      now_ms: 1_700_000_000_600,
    },
  )
  .unwrap();

  assert_eq!(receipt.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  let snapshot = reopened_source.index_runtime_snapshot_v1().unwrap();
  assert_eq!(snapshot.highest_checkpoint_sequence, 0, "dirty overlay is not query-visible coverage");
  assert_eq!(snapshot.degraded.as_ref().unwrap().code, "native_index_dirty_overlay_requires_reconciliation");
  assert_eq!(reopened_publisher.load_selected_semantic_authority().unwrap().root_hash, receipt.selected_root_hash);
}

#[test]
fn selected_runtime_restart_streams_each_durable_producer_task_into_the_recovering_owner_once() {
  let fixture = RuntimeFixture::new("runtime-selected-producer-restart");
  let runtime_id = id(0x64);
  let workspace_id = id(0xe4);
  let (_selected, mut runtime_publisher, _successor) =
    seed_selected_runtime_dirty_overlay_with_successor(&fixture, runtime_id, workspace_id);
  let authority = fixture.destination.publisher().load_selected_semantic_authority().unwrap();
  let task = IndexProducerTaskRequestV1 {
    operation_id: id(0x91),
    kind: IndexProducerTaskKindV1::Rebuild,
    publication_sequence: authority.root_publication_sequence,
    namespace_root_before: &authority.root_hash,
    namespace_root_after: &authority.root_hash,
    semantic_state_root: &authority.semantic_state.object_id,
    journal_head: None,
    scope: Some("/docs"),
  };
  let mut producer = IndexProducerCoordinatorV1::new(
    fixture.permit.hash_algorithm(),
    (*fixture.source.memory_coordinator()).clone(),
    IndexProducerCoordinatorOptionsV1::new(8, 1024 * 1024, 3, 10, 1_000, 16, 256, 1024 * 1024).unwrap(),
  )
  .unwrap();
  assert_eq!(producer.admit_durable_or_spill(task, 1_700_000_000_510, &mut runtime_publisher).unwrap(), IndexProducerAdmissionV1::Queued);
  let selected_after_task = runtime_publisher.workspace_head().unwrap().selected_descriptor();
  assert_eq!(selected_after_task.durable_sequence(), 2);
  assert_eq!(
    producer.admit_durable_or_spill(task, 1_700_000_000_511, &mut runtime_publisher).unwrap(),
    IndexProducerAdmissionV1::Duplicate
  );
  assert_eq!(runtime_publisher.workspace_head().unwrap().manifest_sequence(), 2, "exact durable retry appended another task object");

  let identity = fixture.identity();
  let RuntimeFixture { _directory, source_path, source, permit, destination, cancellation } = fixture;
  let shadow_path = destination.path().to_path_buf();
  drop(runtime_publisher);
  drop(source);
  drop(destination);
  drop(cancellation);

  let reopened_source = StorageEngine::open(source_path.to_str().unwrap()).unwrap();
  let reopened_publisher = Arc::new(reopen(&shadow_path));
  let reopened_cancellation = CancellationToken::new();
  install_native_index_runtime_v1(
    &reopened_source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: Arc::clone(&reopened_publisher),
      retirement_owner: retirement_owner(&reopened_source, permit.database_id(), &reopened_cancellation),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(_directory.path(), permit.hash_algorithm(), permit.database_id(), workspace_id),
      cancellation: &reopened_cancellation,
      clock: Arc::new(MockClock::new(32, 1_700_000_000_600)),
      now_ms: 1_700_000_000_600,
    },
  )
  .unwrap();

  let snapshot = reopened_source.index_runtime_snapshot_v1().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.producer.pending_tasks, 1);
  assert_eq!(snapshot.producer.leased_tasks, 0);
}

#[test]
fn fresh_runtime_reuses_its_exact_empty_workspace_after_restart() {
  let fixture = RuntimeFixture::new("runtime-empty-workspace-restart");
  let runtime_id = id(0x65);
  let workspace_id = id(0xe5);
  let identity = fixture.identity();
  install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, workspace_id),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(34, 1_700_000_000_620),
      now_ms: 1_700_000_000_620,
    },
  )
  .unwrap();

  let RuntimeFixture { _directory, source_path, source, permit, destination, cancellation } = fixture;
  let shadow_path = destination.path().to_path_buf();
  drop(source);
  drop(destination);
  drop(cancellation);
  let reopened_source = StorageEngine::open(source_path.to_str().unwrap()).unwrap();
  let reopened_publisher = Arc::new(reopen(&shadow_path));
  let reopened_cancellation = CancellationToken::new();
  let receipt = install_native_index_runtime_v1(
    &reopened_source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: reopened_publisher,
      retirement_owner: retirement_owner(&reopened_source, permit.database_id(), &reopened_cancellation),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(_directory.path(), permit.hash_algorithm(), permit.database_id(), workspace_id),
      cancellation: &reopened_cancellation,
      clock: Arc::new(MockClock::new(35, 1_700_000_000_630)),
      now_ms: 1_700_000_000_630,
    },
  )
  .unwrap();
  assert_eq!(receipt.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(receipt.highest_checkpoint_sequence, 0);
}

#[test]
fn malformed_selected_runtime_dirty_overlay_refuses_installation_without_consuming_the_runtime_slot() {
  let fixture = RuntimeFixture::new("runtime-dirty-overlay-malformed");
  let runtime_id = id(0x64);
  let workspace_id = id(0xe4);
  let selected = seed_selected_runtime_dirty_overlay(&fixture, runtime_id, workspace_id);
  let manifest = selected.workspace_path().join("manifests").join(format!("{:016x}.aiwm", selected.durable_sequence()));
  fs::write(&manifest, b"truncated").unwrap();
  let identity = fixture.identity();

  let error = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, workspace_id),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(33, 1_700_000_000_610),
      now_ms: 1_700_000_000_610,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_runtime_publisher_recovery", .. }));
  assert!(fixture.source.index_runtime_snapshot_v1().is_none());
  fixture.source.shutdown().unwrap();
}

#[test]
fn resumed_runtime_refuses_an_unexpected_generation_without_consuming_the_runtime_slot() {
  let fixture = RuntimeFixture::new("runtime-dirty-overlay-generation");
  let runtime_id = id(0x67);
  let workspace_id = id(0xe7);
  seed_selected_runtime_dirty_overlay(&fixture, runtime_id, workspace_id);
  let identity = fixture.identity();

  let error = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for_generation(
        fixture._directory.path(),
        fixture.permit.hash_algorithm(),
        fixture.permit.database_id(),
        workspace_id,
        2,
      ),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(36, 1_700_000_000_650),
      now_ms: 1_700_000_000_650,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_runtime_publisher_resume_generation", .. }));
  assert!(fixture.source.index_runtime_snapshot_v1().is_none());

  let receipt = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, workspace_id),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(37, 1_700_000_000_651),
      now_ms: 1_700_000_000_651,
    },
  )
  .unwrap();
  assert_eq!(receipt.lifecycle, IndexRuntimeLifecycleV1::Degraded);
}

#[test]
fn publisher_recovery_cancellation_and_pressure_leave_no_partial_runtime_or_memory_reservation() {
  let canceled = RuntimeFixture::new("runtime-publisher-recovery-canceled");
  let canceled_runtime_id = id(0x68);
  let canceled_workspace_id = id(0xe8);
  seed_selected_runtime_dirty_overlay(&canceled, canceled_runtime_id, canceled_workspace_id);
  let canceled_identity = canceled.identity();
  let cancellation = CancellationToken::new();
  let error = install_native_index_runtime_v1(
    &canceled.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: canceled_runtime_id,
      shadow_identity: &canceled_identity,
      publisher: canceled.destination.shared_publisher(),
      retirement_owner: canceled.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&canceled, canceled_workspace_id),
      cancellation: &cancellation,
      clock: Arc::new(CancelingClock { cancellation: cancellation.clone(), now_ms: 1_700_000_000_660 }),
      now_ms: 1_700_000_000_660,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Canceled));
  assert!(canceled.source.index_runtime_snapshot_v1().is_none());

  let pressured = RuntimeFixture::new("runtime-publisher-recovery-pressure");
  let pressured_runtime_id = id(0x69);
  let pressured_workspace_id = id(0xe9);
  seed_selected_runtime_dirty_overlay(&pressured, pressured_runtime_id, pressured_workspace_id);
  let pressured_identity = pressured.identity();
  let memory = pressured.source.memory_coordinator();
  let baseline = memory.snapshot().unwrap();
  let baseline_dirty = baseline.owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes;
  let original_policy = baseline.policy.unwrap();
  let error = install_native_index_runtime_v1(
    &pressured.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: pressured_runtime_id,
      shadow_identity: &pressured_identity,
      publisher: pressured.destination.shared_publisher(),
      retirement_owner: pressured.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&pressured, pressured_workspace_id),
      cancellation: &pressured.cancellation,
      clock: Arc::new(PressuringRuntimeClock { calls: AtomicUsize::new(0), now_ms: 1_700_000_000_670, memory: Arc::clone(&memory) }),
      now_ms: 1_700_000_000_670,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::DirtyOverlayRecovery(_)));
  assert!(pressured.source.index_runtime_snapshot_v1().is_none());
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, baseline_dirty);

  memory.reconfigure_policy(original_policy).unwrap();
  let receipt = install_native_index_runtime_v1(
    &pressured.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: pressured_runtime_id,
      shadow_identity: &pressured_identity,
      publisher: pressured.destination.shared_publisher(),
      retirement_owner: pressured.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&pressured, pressured_workspace_id),
      cancellation: &pressured.cancellation,
      clock: pressured.clock(38, 1_700_000_000_671),
      now_ms: 1_700_000_000_671,
    },
  )
  .unwrap();
  assert_eq!(receipt.lifecycle, IndexRuntimeLifecycleV1::Degraded);
}

#[test]
fn runtime_selection_change_during_installation_is_rejected_at_the_final_authority_frontier() {
  let fixture = RuntimeFixture::new("runtime-selection-race");
  let runtime_id = id(0x66);
  let workspace_id = id(0xe6);
  let (_selected, publisher, successor) = seed_selected_runtime_dirty_overlay_with_successor(&fixture, runtime_id, workspace_id);
  let identity = fixture.identity();
  let clock: Arc<dyn VirtualClock> = Arc::new(AdvancingRuntimeClock {
    calls: AtomicUsize::new(0),
    now_ms: 1_700_000_000_640,
    advance: Mutex::new(Some((publisher, successor))),
  });

  let error = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, workspace_id),
      cancellation: &fixture.cancellation,
      clock,
      now_ms: 1_700_000_000_640,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_runtime_publisher_selection_changed", .. }));
  assert!(fixture.source.index_runtime_snapshot_v1().is_none());
}

#[test]
fn storage_shutdown_drains_the_installed_runtime_before_completing() {
  let fixture = RuntimeFixture::new("runtime-shutdown-drain");
  let runtime_id = [0x6a; 16];
  let identity = fixture.identity();
  install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, id(0xea)),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(17, 1_700_000_000_360),
      now_ms: 1_700_000_000_360,
    },
  )
  .unwrap();

  fixture.source.shutdown().unwrap();

  assert_eq!(fixture.source.index_runtime_snapshot_v1().unwrap().lifecycle, IndexRuntimeLifecycleV1::Stopped);
  assert_eq!(fixture.source.active_operations_snapshot().active_operations, 0);
}

#[test]
fn storage_shutdown_cannot_observe_a_runtime_whose_publisher_construction_failed() {
  let fixture = RuntimeFixture::new("runtime-shutdown-atomic-refusal");
  let runtime_id = [0x6b; 16];
  let identity = fixture.identity();
  let error = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, id(0xea)),
      cancellation: &fixture.cancellation,
      clock: Arc::new(MockClock::new(19, 0)),
      now_ms: 1_700_000_000_380,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::PublisherStore(_)));
  assert!(fixture.source.index_runtime_snapshot_v1().is_none());

  fixture.source.shutdown().unwrap();
  assert!(fixture.source.emergency_spill_report().is_none());
}

#[test]
fn storage_shutdown_preserves_the_existing_workspace_when_soft_work_blocks_drain() {
  let fixture = RuntimeFixture::new("runtime-shutdown-queued-spill");
  let runtime_id = [0x6c; 16];
  let workspace_id = id(0xeb);
  let identity = fixture.identity();
  install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, workspace_id),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(20, 1_700_000_000_390),
      now_ms: 1_700_000_000_390,
    },
  )
  .unwrap();
  DirectoryOps::new(&fixture.source)
    .store_file_buffered(&RequestContext::system(), "/queued-before-shutdown.json", br#"{"queued":true}"#, Some("application/json"))
    .unwrap();
  assert!(fixture.source.index_runtime_snapshot_v1().unwrap().soft_hub.queued_notices > 0);

  let error = fixture.source.shutdown().unwrap_err();
  assert!(error.to_string().contains("drain is incomplete"), "{error}");
  let report = fixture.source.emergency_spill_report().expect("failed shutdown must preserve runtime recovery evidence");
  assert!(report.succeeded, "spill failed: {:?}", report.errors);
  let spill_root = fixture._directory.path().join("runtime-shutdown-queued-spill-spill");
  let artifacts = scan_for_database_with_dirs(&fixture.source_path, &[spill_root]).unwrap();
  let runtime = artifacts[0].index_runtime_state.as_ref().expect("typed runtime reconciliation evidence");
  assert!(runtime.soft_queued_notices > 0);
  assert!(runtime.reconciliation_required);
  let workspace = runtime.workspace.as_ref().expect("the installed cadence workspace must be retained");
  assert_eq!(workspace.workspace_id, workspace_id);
  assert!(workspace.path.is_dir());
  assert!(workspace.selected_head.is_none(), "an unselected workspace must not fabricate a durable selected head");
}

#[test]
fn runtime_identity_refusal_and_content_only_descriptor_refusal_do_not_consume_installation() {
  let directory = tempfile::tempdir().unwrap();
  let source_path = directory.path().join("identity-source.aeordb");
  let source = source_engine(&source_path, &directory.path().join("identity-spill"));
  let shadow_path = directory.path().join("identity-shadow.aeordb");
  let destination_observation = observe_migration_destination_path_v1(&shadow_path).unwrap();
  let wrong_permit = permit(HashAlgorithm::Blake3_256, &destination_observation);
  let cancellation = CancellationToken::new();
  let destination = initialize_migration_destination_v1(MigrationDestinationInitializationRequestV1 {
    permit: &wrong_permit,
    destination: &destination_observation,
    created_at_ms: 1_700_000_000_000,
    writer_fence_epoch: 7,
    cancellation: &cancellation,
  })
  .unwrap();
  let wrong_identity = IndexRuntimeShadowIdentityV1::from_preflight(&wrong_permit);
  let clock: Arc<dyn VirtualClock> = Arc::new(MockClock::new(1, 1_700_000_000_200));
  let error = install_native_index_runtime_v1(
    &source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x71; 16],
      shadow_identity: &wrong_identity,
      publisher: destination.shared_publisher(),
      retirement_owner: retirement_owner(&source, wrong_permit.database_id(), &cancellation),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(
        directory.path(),
        wrong_permit.hash_algorithm(),
        wrong_permit.database_id(),
        id(0x71),
      ),
      cancellation: &cancellation,
      clock: Arc::clone(&clock),
      now_ms: 1_700_000_000_200,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_source_identity", .. }));
  assert!(source.index_runtime_snapshot_v1().is_none());

  let correct_permit =
    permit_with_source_identity(HashAlgorithm::Blake3_256, &destination_observation, platform_file_identity(&source_path).unwrap());
  let correct_identity = IndexRuntimeShadowIdentityV1::from_preflight(&correct_permit);
  let selected_descriptor = descriptor(correct_permit.database_id());
  let error = install_native_index_runtime_v1(
    &source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x72; 16],
      shadow_identity: &correct_identity,
      publisher: destination.shared_publisher(),
      retirement_owner: retirement_owner(&source, correct_permit.database_id(), &cancellation),
      operation_descriptors: std::slice::from_ref(&selected_descriptor),
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(
        directory.path(),
        correct_permit.hash_algorithm(),
        correct_permit.database_id(),
        id(0x72),
      ),
      cancellation: &cancellation,
      clock,
      now_ms: 1_700_000_000_201,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_content_only_descriptors", .. }));
  assert!(source.index_runtime_snapshot_v1().is_none());
}

#[test]
fn complete_semantic_authority_without_a_selected_checkpoint_starts_a_clean_build() {
  let empty = RuntimeFixture::new("runtime-complete-empty");
  publish_complete_semantic_authority(&empty, 0);
  let empty_identity = empty.identity();
  let empty_receipt = install_native_index_runtime_v1(
    &empty.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x81; 16],
      shadow_identity: &empty_identity,
      publisher: empty.destination.shared_publisher(),
      retirement_owner: empty.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&empty, id(0x81)),
      cancellation: &empty.cancellation,
      clock: empty.clock(1, 1_700_000_000_200),
      now_ms: 1_700_000_000_200,
    },
  )
  .unwrap();
  assert!(!empty_receipt.content_only);
  assert_eq!(empty_receipt.lifecycle, IndexRuntimeLifecycleV1::Running);

  let unbuilt = RuntimeFixture::new("runtime-complete-unbuilt");
  publish_complete_semantic_authority(&unbuilt, 1);
  let unbuilt_identity = unbuilt.identity();
  let unbuilt_receipt = install_native_index_runtime_v1(
    &unbuilt.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x82; 16],
      shadow_identity: &unbuilt_identity,
      publisher: unbuilt.destination.shared_publisher(),
      retirement_owner: unbuilt.retirement(),
      operation_descriptors: &[],
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&unbuilt, id(0x82)),
      cancellation: &unbuilt.cancellation,
      clock: unbuilt.clock(2, 1_700_000_000_200),
      now_ms: 1_700_000_000_200,
    },
  )
  .unwrap();
  assert_eq!(unbuilt_receipt.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(unbuilt_receipt.recovered_scopes, 0);
  assert_eq!(unbuilt_receipt.highest_checkpoint_sequence, 0);
  assert!(unbuilt.source.index_runtime_snapshot_v1().unwrap().degraded.is_none());
}

#[test]
fn selected_checkpoint_damage_installs_one_typed_degraded_runtime_instead_of_remaining_recovering() {
  let absent = RuntimeFixture::new("runtime-selected-absent");
  publish_complete_semantic_authority(&absent, 1);
  let absent_descriptor = descriptor(absent.permit.database_id());
  let absent_receipt = install_runtime_fixture(&absent, &absent_descriptor, 0xa1);
  assert_eq!(absent_receipt.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(
    absent.source.index_runtime_snapshot_v1().unwrap().degraded.as_ref().unwrap().code,
    "native_index_checkpoint_selection_missing"
  );
  let root_observation = absent.source.runtime_observability_snapshot(ConfigurationVisibility::Root).unwrap();
  let redacted_observation = absent.source.runtime_observability_snapshot(ConfigurationVisibility::Redacted).unwrap();
  assert_eq!(root_observation.index_runtime.state, "degraded");
  assert!(!root_observation.index_runtime.degraded.as_ref().unwrap().context.is_empty());
  assert_eq!(redacted_observation.index_runtime.degraded.as_ref().unwrap().context, "<redacted>");
  assert_eq!(
    redacted_observation.index_runtime.degraded.as_ref().unwrap().code,
    root_observation.index_runtime.degraded.as_ref().unwrap().code
  );
  let health = check_engine(&absent.source, absent.source_path.to_str().unwrap());
  assert_eq!(health.status, HealthStatus::Degraded);
  assert_eq!(health.index_runtime_state, "degraded");

  let missing = RuntimeFixture::new("runtime-selected-missing-journal");
  let missing_semantic = publish_complete_semantic_authority(&missing, 1);
  let missing_descriptor = descriptor(missing.permit.database_id());
  publish_recoverable_checkpoint(
    &missing,
    &missing_descriptor,
    &missing_semantic,
    missing.retirement(),
    missing.clock(2, 1_700_000_000_200),
    RecoveryFixtureFault::MissingJournal,
  );
  let missing_receipt = install_runtime_fixture(&missing, &missing_descriptor, 0xa2);
  assert_eq!(missing_receipt.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(missing.source.index_runtime_snapshot_v1().unwrap().degraded.as_ref().unwrap().code, "native_index_journal_missing");

  let discontinuous = RuntimeFixture::new("runtime-selected-discontinuous");
  let discontinuous_semantic = publish_complete_semantic_authority(&discontinuous, 1);
  let discontinuous_descriptor = descriptor(discontinuous.permit.database_id());
  publish_recoverable_checkpoint(
    &discontinuous,
    &discontinuous_descriptor,
    &discontinuous_semantic,
    discontinuous.retirement(),
    discontinuous.clock(3, 1_700_000_000_200),
    RecoveryFixtureFault::DiscontinuousJournal,
  );
  let discontinuous_receipt = install_runtime_fixture(&discontinuous, &discontinuous_descriptor, 0xa3);
  assert_eq!(discontinuous_receipt.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(
    discontinuous.source.index_runtime_snapshot_v1().unwrap().degraded.as_ref().unwrap().code,
    "native_index_journal_discontinuous"
  );

  let corrupt = RuntimeFixture::new("runtime-selected-corrupt");
  let corrupt_semantic = publish_complete_semantic_authority(&corrupt, 1);
  let corrupt_descriptor = descriptor(corrupt.permit.database_id());
  let checkpoint_key = publish_recoverable_checkpoint(
    &corrupt,
    &corrupt_descriptor,
    &corrupt_semantic,
    corrupt.retirement(),
    corrupt.clock(4, 1_700_000_000_200),
    RecoveryFixtureFault::None,
  );
  let locator = corrupt.destination.publisher().locator(&checkpoint_key).unwrap().unwrap();
  let corrupt_offset = locator.offset + u64::from(locator.total_length) - 1;
  let mut file = fs::OpenOptions::new().read(true).write(true).open(corrupt.destination.path()).unwrap();
  file.seek(SeekFrom::Start(corrupt_offset)).unwrap();
  let mut byte = [0; 1];
  file.read_exact(&mut byte).unwrap();
  byte[0] ^= 0xff;
  file.seek(SeekFrom::Start(corrupt_offset)).unwrap();
  file.write_all(&byte).unwrap();
  file.sync_all().unwrap();

  let corrupt_receipt = install_runtime_fixture(&corrupt, &corrupt_descriptor, 0xa4);
  assert_eq!(corrupt_receipt.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(
    corrupt.source.index_runtime_snapshot_v1().unwrap().degraded.as_ref().unwrap().code,
    "native_index_checkpoint_recovery_failed"
  );
}

#[test]
fn cancellation_during_native_checkpoint_recovery_releases_the_unconsumed_installation_slot() {
  let fixture = RuntimeFixture::new("runtime-selected-cancel-during-recovery");
  let semantic_state_root = publish_complete_semantic_authority(&fixture, 1);
  let selected_descriptor = descriptor(fixture.permit.database_id());
  publish_recoverable_checkpoint(
    &fixture,
    &selected_descriptor,
    &semantic_state_root,
    fixture.retirement(),
    fixture.clock(1, 1_700_000_000_200),
    RecoveryFixtureFault::None,
  );
  let identity = fixture.identity();
  let error = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0xb1; 16],
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: std::slice::from_ref(&selected_descriptor),
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, id(0xb1)),
      cancellation: &fixture.cancellation,
      clock: Arc::new(CancelingClock { cancellation: fixture.cancellation.clone(), now_ms: 1_700_000_000_201 }),
      now_ms: 1_700_000_000_201,
    },
  )
  .unwrap_err();

  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Canceled));
  assert!(fixture.source.index_runtime_snapshot_v1().is_none());
}

#[test]
fn descriptor_duplicates_and_resource_bounds_refuse_before_consuming_the_runtime_slot() {
  let fixture = RuntimeFixture::new("runtime-descriptor-bounds");
  publish_complete_semantic_authority(&fixture, 2);
  let first = descriptor_with(fixture.permit.database_id(), 0xc1, 0xc2, 0xc3);
  let same_scope = descriptor_with(fixture.permit.database_id(), 0xc1, 0xd2, 0xd3);
  let second_scope = descriptor_with(fixture.permit.database_id(), 0xd1, 0xd2, 0xd3);
  let identity = fixture.identity();
  let descriptors = [first.clone(), same_scope];
  let error = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0xc1; 16],
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &descriptors,
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, id(0xc1)),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(1, 1_700_000_000_200),
      now_ms: 1_700_000_000_200,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_descriptor_duplicate", .. }));
  assert!(fixture.source.index_runtime_snapshot_v1().is_none());

  let descriptors = [first.clone(), second_scope];
  let count_options = IndexRuntimeNativeRecoveryOptionsV1::new(
    1,
    4 * 1_024 * 1_024,
    IndexScopeOrdinalStoreRegistryOptionsV1::new(8, 8 * 1_024 * 1_024).unwrap(),
    IndexCoverageRegistryOptionsV1::new(64, 256 * 1_024).unwrap(),
    IndexRecoveryOptionsV1::new(128, 16 * 1_024 * 1_024, 128, 16 * 1_024 * 1_024).unwrap(),
  )
  .unwrap();
  let error = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0xc2; 16],
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &descriptors,
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: count_options,
      runtime_publisher: runtime_publisher_options(&fixture, id(0xc2)),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(2, 1_700_000_000_201),
      now_ms: 1_700_000_000_201,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_descriptor_count", .. }));
  assert!(fixture.source.index_runtime_snapshot_v1().is_none());

  let byte_options = IndexRuntimeNativeRecoveryOptionsV1::new(
    128,
    1,
    IndexScopeOrdinalStoreRegistryOptionsV1::new(8, 8 * 1_024 * 1_024).unwrap(),
    IndexCoverageRegistryOptionsV1::new(64, 256 * 1_024).unwrap(),
    IndexRecoveryOptionsV1::new(128, 16 * 1_024 * 1_024, 128, 16 * 1_024 * 1_024).unwrap(),
  )
  .unwrap();
  let error = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0xc3; 16],
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: std::slice::from_ref(&first),
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: byte_options,
      runtime_publisher: runtime_publisher_options(&fixture, id(0xc3)),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(3, 1_700_000_000_202),
      now_ms: 1_700_000_000_202,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Invalid { code: "native_index_descriptor_bytes", .. }));
  assert!(fixture.source.index_runtime_snapshot_v1().is_none());

  let receipt = install_runtime_fixture(&fixture, &first, 0xc4);
  assert_eq!(receipt.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(
    fixture.source.index_runtime_snapshot_v1().unwrap().degraded.as_ref().unwrap().code,
    "native_index_checkpoint_selection_missing"
  );
}

#[test]
fn selected_native_checkpoint_recovers_again_after_source_and_shadow_reopen() {
  let fixture = RuntimeFixture::new("runtime-selected-reopen");
  let semantic_state_root = publish_complete_semantic_authority(&fixture, 1);
  let selected_descriptor = descriptor(fixture.permit.database_id());
  let retirement = fixture.retirement();
  let clock = fixture.clock(1, 1_700_000_000_200);
  publish_recoverable_checkpoint(
    &fixture,
    &selected_descriptor,
    &semantic_state_root,
    Arc::clone(&retirement),
    Arc::clone(&clock),
    RecoveryFixtureFault::None,
  );
  let identity = fixture.identity();
  let first = install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x91; 16],
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: retirement,
      operation_descriptors: std::slice::from_ref(&selected_descriptor),
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options(&fixture, id(0x91)),
      cancellation: &fixture.cancellation,
      clock,
      now_ms: 1_700_000_000_201,
    },
  )
  .unwrap();
  assert_eq!(first.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(first.recovered_scopes, 1);
  assert_eq!(first.highest_checkpoint_sequence, 1);
  assert_eq!(first.semantic_state_root, semantic_state_root);
  let cache_before_eviction = fixture.source.index_runtime_coverage_snapshot_v1().unwrap().unwrap().scope_ordinal_cache;
  assert_eq!(cache_before_eviction.entries, 1);
  assert_eq!(cache_before_eviction.pinned_entries, 0);
  assert_eq!(fixture.source.evict_clean_index_cache().unwrap(), 1);
  let cache_after_eviction = fixture.source.index_runtime_coverage_snapshot_v1().unwrap().unwrap().scope_ordinal_cache;
  assert_eq!(cache_after_eviction.entries, 0);
  assert_eq!(cache_after_eviction.evictions, cache_before_eviction.evictions + 1);

  let RuntimeFixture { _directory, source_path, source, permit, destination, cancellation } = fixture;
  let shadow_path = destination.path().to_path_buf();
  let database_id = permit.database_id();
  drop(source);
  drop(destination);
  drop(cancellation);

  let reopened_source = StorageEngine::open(source_path.to_str().unwrap()).unwrap();
  assert!(platform_file_identity(&source_path).unwrap().represents_same_physical_file_as(permit.source_file_identity()));
  let reopened_publisher = Arc::new(reopen(&shadow_path));
  let reopened_cancellation = CancellationToken::new();
  let reopened_retirement = retirement_owner(&reopened_source, database_id, &reopened_cancellation);
  let reopened = install_native_index_runtime_v1(
    &reopened_source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x92; 16],
      shadow_identity: &identity,
      publisher: reopened_publisher,
      retirement_owner: reopened_retirement,
      operation_descriptors: std::slice::from_ref(&selected_descriptor),
      coverage_owner_requests: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      runtime_publisher: runtime_publisher_options_for(_directory.path(), permit.hash_algorithm(), permit.database_id(), id(0x92)),
      cancellation: &reopened_cancellation,
      clock: Arc::new(MockClock::new(2, 1_700_000_000_300)),
      now_ms: 1_700_000_000_300,
    },
  )
  .unwrap();
  assert_eq!(reopened.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(reopened.recovered_scopes, 1);
  assert_eq!(reopened.highest_checkpoint_sequence, 1);
  let coverage = reopened_source.index_coverage_registry_snapshot_v1().unwrap().expect("reconstructed coverage registry");
  assert!(coverage.is_empty());
  let coverage_runtime = reopened_source.index_runtime_coverage_snapshot_v1().unwrap().expect("reconstructed coverage lifecycle");
  assert_eq!(coverage_runtime.refresh_attempts, 1);
  assert_eq!(coverage_runtime.successful_refreshes, 1);
  assert_eq!(coverage_runtime.failed_refreshes, 0);
}

#[test]
fn initializer_refuses_invalid_time_fence_cancellation_and_foreign_destination_before_create() {
  let directory = tempfile::tempdir().unwrap();
  let first_path = directory.path().join("first.aeordb");
  let second_path = directory.path().join("second.aeordb");
  let first = observe_migration_destination_path_v1(&first_path).unwrap();
  let second = observe_migration_destination_path_v1(&second_path).unwrap();
  let permit = permit(HashAlgorithm::Blake3_256, &first);
  let cancellation = CancellationToken::new();

  for (created_at_ms, writer_fence_epoch, expected) in
    [(0, 1, "migration_destination_time"), (i64::MAX as u64 + 1, 1, "migration_destination_time"), (1, 0, "migration_destination_fence")]
  {
    let error = initialize_migration_destination_v1(MigrationDestinationInitializationRequestV1 {
      permit: &permit,
      destination: &first,
      created_at_ms,
      writer_fence_epoch,
      cancellation: &cancellation,
    })
    .unwrap_err();
    assert_eq!(error.code(), expected);
    assert!(error.artifact().is_none());
    assert!(!first_path.exists());
  }

  let canceled = CancellationToken::new();
  canceled.cancel();
  let error = initialize_migration_destination_v1(MigrationDestinationInitializationRequestV1 {
    permit: &permit,
    destination: &first,
    created_at_ms: 1,
    writer_fence_epoch: 1,
    cancellation: &canceled,
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_destination_canceled");
  assert!(error.artifact().is_none());
  assert!(!first_path.exists());

  let error = initialize_migration_destination_v1(MigrationDestinationInitializationRequestV1 {
    permit: &permit,
    destination: &second,
    created_at_ms: 1,
    writer_fence_epoch: 1,
    cancellation: &cancellation,
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_destination_identity");
  assert!(error.artifact().is_none());
  assert!(!first_path.exists());
  assert!(!second_path.exists());
}

#[test]
fn destination_observation_refuses_existing_symlink_and_noncanonical_paths() {
  let directory = tempfile::tempdir().unwrap();
  let existing = directory.path().join("existing.aeordb");
  fs::write(&existing, b"keep").unwrap();
  assert_eq!(observe_migration_destination_path_v1(&existing).unwrap_err().code(), "migration_destination_exists");
  assert_eq!(fs::read(&existing).unwrap(), b"keep");

  #[cfg(unix)]
  {
    std::os::unix::fs::symlink(&existing, directory.path().join("link.aeordb")).unwrap();
    assert_eq!(
      observe_migration_destination_path_v1(directory.path().join("link.aeordb")).unwrap_err().code(),
      "migration_destination_symlink",
    );
  }

  fs::create_dir(directory.path().join("child")).unwrap();
  let noncanonical = directory.path().join("child").join("..").join("new.aeordb");
  assert_eq!(observe_migration_destination_path_v1(&noncanonical).unwrap_err().code(), "migration_destination_path_noncanonical");
}

#[test]
fn destination_initializer_is_a_disconnected_composition_of_existing_physical_owners() {
  let package = Path::new(env!("CARGO_MANIFEST_DIR"));
  let source = fs::read_to_string(package.join("src/engine/v4/migration_destination.rs")).unwrap();
  for required in [
    "create_new_regular_file_read_write_no_follow",
    "DiskKVStore::create_with_coordinator",
    "encode_database_header_slot",
    "V4FirstAuthorityPublisher",
    "sync_file_all_native",
    "sync_directory_native",
    "verify_file_bytes_native",
    "platform_file_identity",
  ] {
    assert!(source.contains(required), "destination initializer omitted shared owner {required}");
  }
  for forbidden in ["StorageEngine", "DirectoryOps", "crate::server", "axum", "tokio::spawn", "remove_file(", "rename(", "repair"] {
    assert!(!source.contains(forbidden), "destination initializer gained activation or destructive authority {forbidden}");
  }
}

#[test]
fn native_runtime_has_one_atomic_publisher_and_cadence_installation_boundary() {
  let package = Path::new(env!("CARGO_MANIFEST_DIR"));
  let installation = fs::read_to_string(package.join("src/engine/v4/index_runtime_installation.rs")).unwrap();
  let coverage_runtime = fs::read_to_string(package.join("src/engine/v4/index_coverage_runtime.rs")).unwrap();
  let storage_engine = fs::read_to_string(package.join("src/engine/storage_engine.rs")).unwrap();
  let query_engine = fs::read_to_string(package.join("src/engine/query_engine.rs")).unwrap();
  let server = fs::read_to_string(package.join("src/server/mod.rs")).unwrap();

  assert_eq!(installation.matches("NativeIndexRuntimeCadenceV1::new").count(), 1);
  assert_eq!(installation.matches("NativeIndexCompactionExecutorV1::new").count(), 1);
  assert!(installation.contains("cadence: Arc<NativeIndexRuntimeCadenceV1>"));
  assert!(installation.contains("Arc::clone(&self.retirement_owner)"));
  assert!(installation.contains("compaction_executor: &compaction_executor"));
  assert!(installation.contains("build_runtime_publisher("));
  assert_eq!(installation.matches("coverage: Arc<NativeIndexCoverageRuntimeV1>").count(), 1);
  assert_eq!(installation.matches("NativeIndexCoverageRuntimeV1::new(").count(), 1);
  assert!(coverage_runtime.contains("FirstAuthorityIndexCoverageRegistrySourceV1::new"));
  assert!(!coverage_runtime.contains("publish_index_active_pointer"));
  assert!(!coverage_runtime.contains("tokio::spawn"));
  assert!(!coverage_runtime.contains("std::thread::spawn"));
  assert!(!coverage_runtime.contains("DiskKVStore"));
  assert_eq!(storage_engine.matches("index_runtime_v1: OnceLock<Arc<").count(), 1);
  assert!(
    !storage_engine.contains("pub fn refresh_index_coverage_registry_v1"),
    "coverage refresh must remain owned by the installed shared cadence"
  );
  assert_eq!(server.matches("engine.flush_index_runtime_if_due_v1()").count(), 1);
  let source_frontier = installation
    .find("let source_authority_guard = engine.direct_hard_authority_guard()?;")
    .expect("runtime installation must acquire source hard authority at its final frontier");
  let destination_frontier = installation
    .find("let semantic_authority_guard = request.publisher.selected_semantic_authority_guard()?;")
    .expect("runtime installation must pin destination selection at its final frontier");
  assert!(
    source_frontier < destination_frontier,
    "runtime installation must never hold destination selection while waiting on source authority"
  );
  assert!(installation.contains("installation.install(&source_authority_guard, runtime)"));
  for forbidden in [
    "NativeIndexRuntimeCadenceInstallationErrorV1",
    "install_index_runtime_cadence_v1",
    "OnceLock<Arc<NativeIndexRuntimeCadenceV1>>",
    "PendingNativeIndexCompactionExecutorV1",
  ] {
    assert!(!installation.contains(forbidden), "runtime installation retained split cadence authority {forbidden}");
    assert!(!storage_engine.contains(forbidden), "storage engine retained split cadence authority {forbidden}");
  }
  for shadow_authority in ["NativeIndexRuntimeV1", "V4FirstAuthorityPublisher", "IndexActivePointerPublication"] {
    assert!(!query_engine.contains(shadow_authority), "v3 query authority was bypassed through {shadow_authority}");
  }
}
