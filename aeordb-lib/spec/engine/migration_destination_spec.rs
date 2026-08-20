use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use aeordb::engine::memory_coordinator::{MemoryPressure, MemoryOwner};
use aeordb::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity};
use aeordb::engine::request_context::RequestContext;
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
  ImmutableSemanticObjectBatchPublicationRequestV1, PreparedNamespaceTreeV0, SuccessorAuthorityPublicationRequestV1,
};
use aeordb::engine::v4::index_coordinator_recovery::{
  IndexCheckpointRootV1, IndexRecoveryOptionsV1, IndexRecoveryOwnerV1, IndexRecoveryStoreV1,
};
use aeordb::engine::v4::index_coordinator::IndexCoordinatorOptionsV1;
use aeordb::engine::v4::index_operation_control::IndexOperationKindV1;
use aeordb::engine::v4::index_producer_collector::IndexProducerCollectorOptionsV1;
use aeordb::engine::v4::index_producer_coordinator::IndexProducerCoordinatorOptionsV1;
use aeordb::engine::v4::index_producer_source::IndexSemanticScopeLimitsV1;
use aeordb::engine::v4::index_runtime_batch_publisher::{DurableIndexRuntimeBatchPublisherV1, NativeIndexRuntimeBatchPublisherV1};
use aeordb::engine::v4::index_runtime_cadence::IndexRuntimeCadenceErrorV1;
use aeordb::engine::v4::index_recovery_store::{
  IndexScopeOrdinalStoreRegistryOptionsV1, NativeIndexOperationDescriptorV1, NativeIndexRecoveryStoreV1, SharedRetirementJournalOwnerV1,
};
use aeordb::engine::v4::index_runtime_installation::{
  IndexRuntimeNativeRecoveryOptionsV1, IndexRuntimeShadowIdentityV1, NativeIndexRuntimeCadenceInstallationErrorV1,
  NativeIndexRuntimeInstallationErrorV1, NativeIndexRuntimeInstallationRequestV1, install_native_index_runtime_v1,
};
use aeordb::engine::v4::index_runtime_owner::{IndexRuntimeLifecycleV1, IndexRuntimeOwnerOptionsV1};
use aeordb::engine::v4::index_runtime_workspace_store::{
  DurableIndexRuntimeWorkspaceV1, IndexRuntimeWorkspaceIdentityV1, IndexRuntimeWorkspaceOptionsV1,
};
use aeordb::engine::v4::index_task::{
  IndexTaskAttachmentRoleV1, IndexTaskAttachmentWriteV1, IndexTaskCheckpointWriteV1, IndexTaskKindV1, IndexTaskStateV1, JournalOwnerKindV1,
  MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, encode_index_task_checkpoint,
  encode_mutation_journal,
};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, decode_namespace_root, encode_semantic_state_object};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::v4::{database_header::DATABASE_HEADER_V4_DATA_OFFSET, hash::digest_parts};
use aeordb::engine::{DirectoryOps, HashAlgorithm, MockClock, StorageEngine, VirtualClock};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const GIB: u64 = 1024 * 1024 * 1024;

fn id(first: u8) -> [u8; 16] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn digest(first: u8) -> [u8; 32] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
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
      capability_profile: BinaryCapabilityProfileV1::new(baseline, baseline),
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

fn cadence_publisher(fixture: &RuntimeFixture, runtime_id: [u8; 16], workspace_id: [u8; 16]) -> NativeIndexRuntimeBatchPublisherV1 {
  let algorithm = fixture.permit.hash_algorithm();
  let descriptor = descriptor_with_algorithm(algorithm, fixture.permit.database_id(), 0xd1, 0xd2, 0xd3);
  let clock = fixture.clock(7, 1_700_000_000_300);
  let store =
    NativeIndexRecoveryStoreV1::new(descriptor.clone(), fixture.destination.shared_publisher(), fixture.retirement(), Arc::clone(&clock))
      .unwrap();
  let scratch = fixture._directory.path().join(format!("cadence-{}", hex::encode(workspace_id)));
  fs::create_dir(&scratch).unwrap();
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
    IndexRuntimeWorkspaceOptionsV1::new(Some(scratch), 16 * 1024 * 1024, 0, 32).unwrap(),
    fixture.cancellation.clone(),
    &fixture.source.memory_coordinator(),
  )
  .unwrap();
  DurableIndexRuntimeBatchPublisherV1::new_unselected(
    algorithm,
    IndexRecoveryOwnerV1::new(fixture.permit.database_id(), descriptor.index_id().to_vec(), descriptor.operation_id()).unwrap(),
    digest_parts(algorithm, &[b"cadence source root"]),
    1,
    1_700_000_000_300,
    workspace,
    store,
    fixture.cancellation.clone(),
    clock,
  )
  .unwrap()
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
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
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
  let source = source_engine(&source_path, &directory.path().join("spill"));
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
  let clock: Arc<dyn VirtualClock> = Arc::new(MockClock::new(1, 1_700_000_000_200));
  let identity = IndexRuntimeShadowIdentityV1::from_preflight(&permit);

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
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      cancellation: &canceled,
      clock: Arc::clone(&clock),
      now_ms: 1_700_000_000_200,
    },
  )
  .unwrap_err();
  assert!(matches!(error, NativeIndexRuntimeInstallationErrorV1::Canceled));
  assert!(source.index_runtime_snapshot_v1().is_none(), "cancellation must not consume the one-time runtime slot");

  let receipt = install_native_index_runtime_v1(
    &source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x52; 16],
      shadow_identity: &identity,
      publisher: initialized.shared_publisher(),
      retirement_owner: retirement,
      operation_descriptors: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
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
  DirectoryOps::new(&source).store_file_buffered(&RequestContext::system(), "/runtime-routed.txt", b"runtime", Some("text/plain")).unwrap();
  assert_eq!(source.index_runtime_snapshot_v1().unwrap().soft_hub.queued_notices, 1);
  assert_eq!(source.memory_coordinator().snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes > 0, true,);

  let duplicate = install_native_index_runtime_v1(
    &source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: [0x53; 16],
      shadow_identity: &identity,
      publisher: initialized.shared_publisher(),
      retirement_owner: retirement_owner(&source, permit.database_id(), &cancellation),
      operation_descriptors: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      cancellation: &cancellation,
      clock: Arc::new(MockClock::new(2, 1_700_000_000_202)),
      now_ms: 1_700_000_000_202,
    },
  )
  .unwrap_err();
  assert!(matches!(duplicate, NativeIndexRuntimeInstallationErrorV1::AlreadyInstalled));
}

#[test]
fn native_runtime_cadence_installation_rejects_invalid_authority_without_consuming_its_slot() {
  let fixture = RuntimeFixture::new("runtime-cadence-installation");
  let runtime_id = [0x61; 16];

  let missing_runtime = fixture
    .source
    .install_index_runtime_cadence_v1(cadence_publisher(&fixture, runtime_id, id(0xe0)), fixture.clock(8, 1_700_000_000_310))
    .unwrap_err();
  assert_eq!(missing_runtime, NativeIndexRuntimeCadenceInstallationErrorV1::RuntimeNotInstalled);

  let identity = fixture.identity();
  install_native_index_runtime_v1(
    &fixture.source,
    NativeIndexRuntimeInstallationRequestV1 {
      coordinator_id: runtime_id,
      shadow_identity: &identity,
      publisher: fixture.destination.shared_publisher(),
      retirement_owner: fixture.retirement(),
      operation_descriptors: &[],
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      cancellation: &fixture.cancellation,
      clock: fixture.clock(9, 1_700_000_000_320),
      now_ms: 1_700_000_000_320,
    },
  )
  .unwrap();

  let runtime_mismatch = fixture
    .source
    .install_index_runtime_cadence_v1(cadence_publisher(&fixture, [0x62; 16], id(0xe1)), fixture.clock(10, 1_700_000_000_330))
    .unwrap_err();
  assert!(matches!(runtime_mismatch, NativeIndexRuntimeCadenceInstallationErrorV1::Invalid(_)));

  let database_mismatch_fixture =
    RuntimeFixture::new_with_authority("runtime-cadence-database-mismatch", HashAlgorithm::Blake3_256, id(0x11), id(0x40));
  let database_mismatch = fixture
    .source
    .install_index_runtime_cadence_v1(
      cadence_publisher(&database_mismatch_fixture, runtime_id, id(0xe2)),
      fixture.clock(11, 1_700_000_000_331),
    )
    .unwrap_err();
  assert!(matches!(database_mismatch, NativeIndexRuntimeCadenceInstallationErrorV1::Invalid(_)));

  let destination_mismatch_fixture =
    RuntimeFixture::new_with_authority("runtime-cadence-destination-mismatch", HashAlgorithm::Blake3_256, id(0x10), id(0x41));
  let destination_mismatch = fixture
    .source
    .install_index_runtime_cadence_v1(
      cadence_publisher(&destination_mismatch_fixture, runtime_id, id(0xe3)),
      fixture.clock(12, 1_700_000_000_332),
    )
    .unwrap_err();
  assert!(matches!(destination_mismatch, NativeIndexRuntimeCadenceInstallationErrorV1::Invalid(_)));

  let hash_mismatch_fixture =
    RuntimeFixture::new_with_authority("runtime-cadence-hash-mismatch", HashAlgorithm::Sha512, id(0x10), id(0x40));
  let hash_mismatch = fixture
    .source
    .install_index_runtime_cadence_v1(cadence_publisher(&hash_mismatch_fixture, runtime_id, id(0xe4)), fixture.clock(13, 1_700_000_000_333))
    .unwrap_err();
  assert!(matches!(hash_mismatch, NativeIndexRuntimeCadenceInstallationErrorV1::Invalid(_)));

  let invalid_clock = fixture
    .source
    .install_index_runtime_cadence_v1(
      cadence_publisher(&fixture, runtime_id, id(0xe5)),
      Arc::new(MockClock::new(14, 0)) as Arc<dyn VirtualClock>,
    )
    .unwrap_err();
  assert_eq!(invalid_clock, NativeIndexRuntimeCadenceInstallationErrorV1::Cadence(IndexRuntimeCadenceErrorV1::InvalidClock));

  fixture
    .source
    .install_index_runtime_cadence_v1(cadence_publisher(&fixture, runtime_id, id(0xe6)), fixture.clock(15, 1_700_000_000_340))
    .unwrap();
  let duplicate = fixture
    .source
    .install_index_runtime_cadence_v1(cadence_publisher(&fixture, runtime_id, id(0xe7)), fixture.clock(16, 1_700_000_000_350))
    .unwrap_err();
  assert_eq!(duplicate, NativeIndexRuntimeCadenceInstallationErrorV1::AlreadyInstalled);
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
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
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
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
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
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
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
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
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
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
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
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
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
      runtime_options: runtime_options(),
      recovery_options: count_options,
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
      runtime_options: runtime_options(),
      recovery_options: byte_options,
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
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
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
      runtime_options: runtime_options(),
      recovery_options: native_recovery_options(),
      cancellation: &reopened_cancellation,
      clock: Arc::new(MockClock::new(2, 1_700_000_000_300)),
      now_ms: 1_700_000_000_300,
    },
  )
  .unwrap();
  assert_eq!(reopened.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(reopened.recovered_scopes, 1);
  assert_eq!(reopened.highest_checkpoint_sequence, 1);
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
      observe_migration_destination_path_v1(&directory.path().join("link.aeordb")).unwrap_err().code(),
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
