use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
use std::time::Duration;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy, MemoryPressure};
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::coverage_runtime::SoftMutationHubOptionsV1;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, MutableSystemControlExpectationV1, MutableSystemControlPublicationRequestV1, PreparedNamespaceTreeV0,
  V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::migration_control::{
  MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED, MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE,
  MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED, MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD, MigrationLeaseBodyV1,
  MigrationLeaseStateV1, MigrationPhaseV1, MigrationProgressBodyV1, MigrationProgressStateV1, decode_migration_lease_control,
  decode_migration_progress_control, encode_migration_lease_control, encode_migration_progress_control,
};
use aeordb::engine::v4::migration_capture_runtime::{
  MigrationCaptureRuntimeClockV1, MigrationCaptureRuntimeOptionsV1, MigrationCaptureRuntimeStateV1, MigrationCaptureRuntimeV1,
};
use aeordb::engine::v4::migration_capture_workspace::MigrationCaptureWorkspaceOptionsV1;
use aeordb::engine::v4::migration_owner::{
  MigrationAcquisitionRequestV1, MigrationCaptureCheckpointPublicationRequestV1, MigrationFullReconciliationLatchRequestV1,
  MigrationLeaseReleaseRequestV1, MigrationLeaseRenewalRequestV1, MigrationProgressTransitionRequestV1,
  MigrationReplayCheckpointPublicationRequestV1, MigrationStateOwnerV1, MigrationTakeoverRequestV1,
};
use aeordb::engine::v4::migration_source_gc::{MigrationSourceGcSuspensionOwnerV1, MigrationSourceGcSuspensionRequestV1};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::v4::system_control::{SystemControlKindV1, SystemControlSlotV1};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::gc::{GcExecutionRequestV1, execute_gc_run, gc_mark, gc_sweep, run_gc, run_gc_with_post_start_hook};
use aeordb::engine::lifecycle_config::{prune_expired_snapshots, prune_expired_snapshots_with_post_capture_hook};
use aeordb::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity};
use aeordb::engine::request_context::RequestContext;
use aeordb::engine::v4::gc_run::GcRunInvocationV1;
use aeordb::engine::version_manager::VersionManager;
use aeordb::engine::{DirectoryOps, DiskKVStore, EngineError, HashAlgorithm};
use aeordb::server::create_temp_engine_for_tests;
use tokio_util::sync::CancellationToken;

const GIB: u64 = 1024 * 1024 * 1024;
const DATABASE_ID: [u8; 16] = [0x31; 16];
const MIGRATION_ID: [u8; 16] = [0x71; 16];
const SOURCE_PHYSICAL_ID: [u8; 16] = [0x41; 16];
const DESTINATION_PHYSICAL_ID: [u8; 16] = [0x51; 16];
const HOLDER_BOOT_ID: [u8; 16] = [0x61; 16];
const ACQUIRED_AT_MS: i64 = 1_700_000_000_200;
const LEASE_DURATION_MS: i64 = 60_000;

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

fn capacity(role: CapacityRoleV1, volume: u8, required_bytes: u64, minimum_remaining_bytes: u64) -> MigrationCapacityObservationV1 {
  MigrationCapacityObservationV1 {
    role,
    volume_identity: id(volume),
    path_identity: identity(0x80 + role as u8, volume),
    filesystem_capacity_bytes: 256 * GIB,
    available_bytes: 192 * GIB,
    required_bytes,
    minimum_remaining_bytes,
  }
}

fn preflight_request(algorithm: HashAlgorithm, destination_physical_instance_id: [u8; 16]) -> MigrationPreflightRequestV1 {
  preflight_request_with_source_identity(algorithm, destination_physical_instance_id, identity(0x50, 0x10))
}

fn preflight_request_with_source_identity(
  algorithm: HashAlgorithm,
  destination_physical_instance_id: [u8; 16],
  source_file_identity: PlatformFileIdentityDescriptorV1,
) -> MigrationPreflightRequestV1 {
  let baseline = CapabilitySetV1::v4_baseline();
  let registry = embedded_system_family_registry(algorithm).unwrap();
  let source_checksum = digest(0x70);
  MigrationPreflightRequestV1 {
    identity: MigrationIdentityEvidenceV1 {
      database_id: DATABASE_ID,
      migration_id: MIGRATION_ID,
      source_physical_instance_id: SOURCE_PHYSICAL_ID,
      destination_physical_instance_id,
      source_path_digest: digest(0x10),
      destination_path_digest: digest(0x30),
      source_file_identity,
      destination_parent_identity: identity(0x81, 0x31),
    },
    source: MigrationSourceEvidenceV1 {
      hash_algorithm: algorithm,
      file_size: 4 * GIB,
      complete_file_checksum: source_checksum,
      selected_header_slot: 1,
      selected_header_sequence: 41,
      selected_header_digest: digest(0x80),
      head_hash: digest_parts(algorithm, &[b"source head"]),
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
    capacity: [
      capacity(CapacityRoleV1::Destination, 0x31, 8 * GIB, 16 * GIB),
      capacity(CapacityRoleV1::Workspace, 0x32, 4 * GIB, 16 * GIB),
      capacity(CapacityRoleV1::Backup, 0x33, 4 * GIB, 16 * GIB),
      capacity(CapacityRoleV1::Capture, 0x34, 64 * GIB, 16 * GIB),
    ],
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
      effective_configuration_fingerprint: digest_parts(algorithm, &[b"effective migration configuration"]),
    },
    binary: MigrationBinaryEvidenceV1 {
      source_commit: std::array::from_fn(|offset| 0x21 + offset as u8),
      executable_sha256: digest(0x31),
      contract_registry_sha256: hex::decode(CONTRACT_REGISTRY_SHA256).unwrap().try_into().unwrap(),
      capability_profile: BinaryCapabilityProfileV1::new(baseline, baseline),
      required_reader_capabilities: baseline,
      required_writer_capabilities: baseline,
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
  }
}

fn initial_header_for(algorithm: HashAlgorithm, kv_block_length: u64) -> DatabaseHeaderV4 {
  DatabaseHeaderV4 {
    hash_algorithm: algorithm,
    slot_sequence: 1,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    database_id: DATABASE_ID,
    write_sequence_high_water: 1,
    required_reader_capabilities: [0; 32],
    kv_block_offset: DATABASE_HEADER_V4_DATA_OFFSET,
    kv_block_length,
    kv_block_version: DiskKVStore::CURRENT_KV_BLOCK_VERSION,
    kv_block_stage: 0,
    resize_in_progress: false,
    resize_target_stage: 0,
    nvt_offset: DATABASE_HEADER_V4_DATA_OFFSET + kv_block_length,
    nvt_length: 0,
    nvt_version: 1,
    backup_type: 0,
    hot_tail_offset: DATABASE_HEADER_V4_DATA_OFFSET + kv_block_length,
    buffer_kvs_offset: 0,
    buffer_nvt_offset: 0,
    entry_count: 0,
    head_hash: vec![0; algorithm.hash_length()],
    base_hash: vec![0; algorithm.hash_length()],
    target_hash: vec![0; algorithm.hash_length()],
    required_writer_capabilities: [0; 32],
    system_family_registry_version: 1,
    system_family_registry_fingerprint: embedded_system_family_registry(algorithm).unwrap().operational_fingerprint.clone(),
    writer_fence_epoch: 1,
    physical_instance_id: DESTINATION_PHYSICAL_ID,
  }
}

fn create_publisher(algorithm: HashAlgorithm) -> (tempfile::TempDir, PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("migration-owner.aeordb");
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let header = initial_header_for(algorithm, initial_block_size());
  let slot = encode_database_header_slot(&header).unwrap();
  file.seek(SeekFrom::Start(0)).unwrap();
  file.write_all(&slot).unwrap();
  file.write_all(&slot).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::create_with_coordinator(
    file.try_clone().unwrap(),
    algorithm,
    header.kv_block_offset,
    header.hot_tail_offset,
    0,
    coordinator.clone(),
  )
  .unwrap();
  file.sync_all().unwrap();
  let publisher = V4FirstAuthorityPublisher::new(kv, coordinator).unwrap();
  publisher
    .publish(&FirstAuthorityPublicationRequestV1 {
      database_id: DATABASE_ID,
      transaction_id: [0x91; 16],
      created_at_ms: 1_700_000_000_100,
      namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(algorithm, &[b"dirc:"]), stored_value: Vec::new() },
      semantic_state: encode_semantic_state_object(
        &SemanticStateWriteV1 {
          required_capabilities: [0; 32],
          availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
        },
        algorithm,
      )
      .unwrap(),
      required_capabilities: [0; 32],
      typed_closure_digest: digest_parts(algorithm, &[b"typed migration-owner closure"]),
      authority_identity: b"HEAD".to_vec(),
    })
    .unwrap();
  (directory, path, publisher)
}

fn reopen(path: &Path) -> V4FirstAuthorityPublisher {
  let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
  let observation = aeordb::engine::v4::header_publication::observe_database_header_v4(&file).unwrap();
  let header = &observation.selected.header;
  let hot_tail = read_hot_tail_checked(&mut file, header.hot_tail_offset, header.hash_algorithm.hash_length()).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::open_with_coordinator(
    file.try_clone().unwrap(),
    header.hash_algorithm,
    header.kv_block_offset,
    header.hot_tail_offset,
    header.kv_block_stage as usize,
    hot_tail.writes,
    hot_tail.voids,
    header.kv_block_version,
    coordinator.clone(),
  )
  .unwrap();
  V4FirstAuthorityPublisher::new(kv, coordinator).unwrap()
}

fn retirement_owner(algorithm: HashAlgorithm, cancellation: &CancellationToken, memory: &MemoryCoordinator) -> RetirementJournalOwnerV1 {
  RetirementJournalOwnerV1::new_chain(
    algorithm,
    DATABASE_ID,
    1,
    901,
    RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
    cancellation,
    memory,
  )
  .unwrap()
}

fn capture_runtime_options(scratch_root: &Path, maximum_notices: usize, maximum_stored_bytes: u64) -> MigrationCaptureRuntimeOptionsV1 {
  MigrationCaptureRuntimeOptionsV1::new(
    1,
    [0x81; 16],
    SoftMutationHubOptionsV1::new(maximum_notices, 64 * 1024, 32 * 1024).unwrap(),
    maximum_notices,
    64 * 1024,
    10,
    MigrationCaptureWorkspaceOptionsV1::new(Some(scratch_root.to_path_buf()), maximum_stored_bytes, 0).unwrap(),
  )
  .unwrap()
}

fn capture_clock(elapsed_ms: u64) -> MigrationCaptureRuntimeClockV1 {
  MigrationCaptureRuntimeClockV1::new(
    ACQUIRED_AT_MS + i64::try_from(elapsed_ms).unwrap(),
    u64::try_from(ACQUIRED_AT_MS).unwrap() + elapsed_ms + 100,
    20_000 + elapsed_ms,
  )
  .unwrap()
}

fn acquisition_request(holder_boot_id: [u8; 16]) -> MigrationAcquisitionRequestV1 {
  MigrationAcquisitionRequestV1 {
    holder_boot_id,
    acquired_at_ms: ACQUIRED_AT_MS,
    lease_duration_ms: LEASE_DURATION_MS,
    publication_timestamp_ms: 1_700_000_000_300,
    monotonic_now_ms: 10_000,
  }
}

fn initial_progress_body(permit: &MigrationPreflightPermitV1, fencing_token: u64) -> MigrationProgressBodyV1 {
  let hash_width = permit.hash_algorithm().hash_length();
  MigrationProgressBodyV1 {
    database_id: permit.database_id(),
    migration_id: permit.migration_id(),
    source_physical_instance_id: permit.source_physical_instance_id(),
    destination_physical_instance_id: permit.destination_physical_instance_id(),
    fencing_token,
    phase: MigrationPhaseV1::Preflight,
    state: MigrationProgressStateV1::Pending,
    flags: 0,
    source_header_sequence: permit.source_header_sequence(),
    destination_header_sequence: 0,
    copied_through_write_sequence: 0,
    captured_through_publication_sequence: 0,
    reconciled_through_publication_sequence: 0,
    namespace_count: 0,
    entity_count: 0,
    copied_bytes: 0,
    updated_at_ms: ACQUIRED_AT_MS,
    source_capture_head: permit.source_capture_head().to_vec(),
    checkpoint_artifact: vec![0; hash_width],
    legacy_root_map_control_payload_hash: vec![0; hash_width],
    effective_config_fingerprint: permit.effective_configuration_fingerprint().to_vec(),
    system_family_registry_fingerprint: permit.system_family_registry_fingerprint().to_vec(),
    last_error_evidence: vec![0; hash_width],
  }
}

fn renewal_request(renewed_at_ms: i64) -> MigrationLeaseRenewalRequestV1 {
  MigrationLeaseRenewalRequestV1 {
    renewed_at_ms,
    lease_duration_ms: LEASE_DURATION_MS,
    publication_timestamp_ms: renewed_at_ms as u64 + 100,
    monotonic_now_ms: renewed_at_ms as u64 - ACQUIRED_AT_MS as u64 + 20_000,
  }
}

fn release_request(publication_timestamp_ms: u64) -> MigrationLeaseReleaseRequestV1 {
  MigrationLeaseReleaseRequestV1 { publication_timestamp_ms, monotonic_now_ms: publication_timestamp_ms - ACQUIRED_AT_MS as u64 + 40_000 }
}

fn takeover_request(new_holder_boot_id: [u8; 16], expected_fencing_token: u64, takeover_at_ms: i64) -> MigrationTakeoverRequestV1 {
  MigrationTakeoverRequestV1 {
    new_holder_boot_id,
    expected_fencing_token,
    takeover_at_ms,
    lease_duration_ms: LEASE_DURATION_MS,
    publication_timestamp_ms: takeover_at_ms as u64 + 100,
    monotonic_now_ms: takeover_at_ms as u64 - ACQUIRED_AT_MS as u64 + 50_000,
  }
}

fn progress_transition(
  algorithm: HashAlgorithm,
  phase: MigrationPhaseV1,
  state: MigrationProgressStateV1,
  flags: u32,
  updated_at_ms: i64,
) -> MigrationProgressTransitionRequestV1 {
  MigrationProgressTransitionRequestV1 {
    phase,
    state,
    flags,
    destination_header_sequence: 7,
    copied_through_write_sequence: 11,
    reconciled_through_publication_sequence: 0,
    namespace_count: 17,
    entity_count: 19,
    copied_bytes: 23,
    updated_at_ms,
    legacy_root_map_control_payload_hash: vec![0; algorithm.hash_length()],
    last_error_evidence: vec![0; algorithm.hash_length()],
    publication_timestamp_ms: updated_at_ms as u64 + 100,
    monotonic_now_ms: updated_at_ms as u64 - ACQUIRED_AT_MS as u64 + 30_000,
  }
}

fn replace_progress(
  publisher: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  retirement: &mut RetirementJournalOwnerV1,
  mutate: impl FnOnce(&mut MigrationProgressBodyV1),
) {
  let current =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let mut progress = decode_migration_progress_control(&current.bytes, algorithm).unwrap();
  mutate(&mut progress.body);
  let encoded = encode_migration_progress_control(progress.sequence + 1, &progress.body, algorithm).unwrap();
  let publication_timestamp_ms = progress.body.updated_at_ms as u64 + 100;
  let monotonic_now_ms = progress.body.updated_at_ms as u64 - ACQUIRED_AT_MS as u64 + 30_000;
  publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationProgress,
        identity: &MIGRATION_ID,
        expected: Some(MutableSystemControlExpectationV1 {
          selected_slot: current.selected_slot,
          control_sequence: current.control_sequence,
          control_digest: current.control_digest,
        }),
        guards: &[],
        encoded_control: &encoded,
        publication_timestamp_ms,
        monotonic_now_ms,
      },
      retirement,
    )
    .unwrap();
}

fn replace_lease(
  publisher: &V4FirstAuthorityPublisher,
  algorithm: HashAlgorithm,
  retirement: &mut RetirementJournalOwnerV1,
  renewed_at_ms: i64,
  mutate: impl FnOnce(&mut MigrationLeaseBodyV1),
) {
  let current = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let mut lease = decode_migration_lease_control(&current.bytes, algorithm).unwrap();
  mutate(&mut lease.body);
  lease.body.renewed_at_ms = renewed_at_ms;
  lease.body.expires_at_ms = renewed_at_ms + LEASE_DURATION_MS;
  let encoded = encode_migration_lease_control(lease.sequence + 1, &lease.body, algorithm).unwrap();
  publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationLease,
        identity: &MIGRATION_ID,
        expected: Some(MutableSystemControlExpectationV1 {
          selected_slot: current.selected_slot,
          control_sequence: current.control_sequence,
          control_digest: current.control_digest,
        }),
        guards: &[],
        encoded_control: &encoded,
        publication_timestamp_ms: renewed_at_ms as u64 + 100,
        monotonic_now_ms: renewed_at_ms as u64 - ACQUIRED_AT_MS as u64 + 20_000,
      },
      retirement,
    )
    .unwrap();
}

#[test]
fn acquisition_publishes_fenced_lease_then_progress_and_reopens() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, publisher) = create_publisher(algorithm);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, receipt) =
    MigrationStateOwnerV1::acquire(Arc::new(publisher), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();

  assert_eq!(owner.fencing_token(), 1);
  assert_eq!(receipt.lease_control_sequence, 1);
  assert_eq!(receipt.progress_control_sequence, 1);
  assert!(!receipt.resumed_partial);
  assert!(!receipt.idempotent);
  drop(owner);

  let reopened = reopen(&path);
  let lease = reopened.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let progress =
    reopened.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let lease = decode_migration_lease_control(&lease.bytes, algorithm).unwrap();
  let progress = decode_migration_progress_control(&progress.bytes, algorithm).unwrap();
  assert_eq!(lease.body.state, MigrationLeaseStateV1::Held);
  assert_eq!(lease.body.holder_boot_id, HOLDER_BOOT_ID);
  assert_eq!(lease.body.fencing_token, 1);
  assert_eq!(progress.body.phase, MigrationPhaseV1::Preflight);
  assert_eq!(progress.body.state, MigrationProgressStateV1::Pending);
  assert_eq!(progress.body.flags, 0);
  assert_eq!(progress.body.fencing_token, 1);
  assert_eq!(progress.body.destination_header_sequence, 0);
}

#[test]
fn completed_observation_requires_bound_held_lease_and_exact_verified_progress() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (_owner, _) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  assert_eq!(
    MigrationStateOwnerV1::observe_completed_destination_verification_if_present(&publisher, &permit).unwrap(),
    None,
    "valid Pending progress is resumable, not completed",
  );
  let required_flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED
    | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD
    | MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED;
  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.phase = MigrationPhaseV1::DestinationVerify;
    progress.state = MigrationProgressStateV1::Complete;
    progress.flags = required_flags;
    progress.destination_header_sequence = 17;
    progress.namespace_count = 5;
    progress.entity_count = 19;
    progress.copied_bytes = 23;
    progress.legacy_root_map_control_payload_hash = digest_parts(algorithm, &[b"selected root map"]);
    progress.updated_at_ms = ACQUIRED_AT_MS + 5_000;
  });

  let observed = MigrationStateOwnerV1::observe_completed_destination_verification(&publisher, &permit).unwrap();
  assert_eq!(MigrationStateOwnerV1::observe_completed_destination_verification_if_present(&publisher, &permit).unwrap(), Some(observed),);
  assert_eq!(observed.fencing_token, 1);
  assert_eq!(observed.phase, MigrationPhaseV1::DestinationVerify);
  assert_eq!(observed.state, MigrationProgressStateV1::Complete);
  assert_eq!(observed.destination_header_sequence, 17);
  assert_eq!(observed.namespace_count, 5);
  assert_eq!(observed.entity_count, 19);
  assert_eq!(observed.copied_bytes, 23);

  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.flags |= MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE;
    progress.last_error_evidence = digest_parts(algorithm, &[b"reconciliation required"]);
    progress.updated_at_ms = ACQUIRED_AT_MS + 6_000;
  });
  assert_eq!(
    MigrationStateOwnerV1::observe_completed_destination_verification(&publisher, &permit).unwrap_err().code(),
    "migration_completed_progress"
  );

  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.flags = required_flags;
    progress.last_error_evidence.fill(0);
    progress.updated_at_ms = ACQUIRED_AT_MS + 7_000;
  });
  replace_lease(&publisher, algorithm, &mut retirement, ACQUIRED_AT_MS + 8_000, |lease| lease.fencing_token += 1);
  assert_eq!(
    MigrationStateOwnerV1::observe_completed_destination_verification(&publisher, &permit).unwrap_err().code(),
    "migration_progress_rebind_required"
  );
}

#[test]
fn acquisition_resumes_a_durable_lease_only_partial_and_retries_exactly() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let lease = encode_migration_lease_control(
    1,
    &MigrationLeaseBodyV1 {
      database_id: DATABASE_ID,
      migration_id: MIGRATION_ID,
      source_physical_instance_id: SOURCE_PHYSICAL_ID,
      destination_physical_instance_id: DESTINATION_PHYSICAL_ID,
      holder_boot_id: HOLDER_BOOT_ID,
      fencing_token: 1,
      acquired_at_ms: ACQUIRED_AT_MS,
      renewed_at_ms: ACQUIRED_AT_MS,
      expires_at_ms: ACQUIRED_AT_MS + LEASE_DURATION_MS,
      source_header_sequence: 41,
      state: MigrationLeaseStateV1::Held,
    },
    algorithm,
  )
  .unwrap();
  publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationLease,
        identity: &MIGRATION_ID,
        expected: None,
        guards: &[],
        encoded_control: &lease,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 10_000,
      },
      &mut retirement,
    )
    .unwrap();

  let (owner, resumed) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  assert!(resumed.resumed_partial);
  assert!(!resumed.idempotent);
  drop(owner);

  let (_, retry) = MigrationStateOwnerV1::acquire(publisher, permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  assert!(!retry.resumed_partial);
  assert!(retry.idempotent);
}

#[test]
fn expired_takeover_reconstructs_missing_initial_progress_and_retries_exactly() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let lease = encode_migration_lease_control(
    1,
    &MigrationLeaseBodyV1 {
      database_id: DATABASE_ID,
      migration_id: MIGRATION_ID,
      source_physical_instance_id: SOURCE_PHYSICAL_ID,
      destination_physical_instance_id: DESTINATION_PHYSICAL_ID,
      holder_boot_id: HOLDER_BOOT_ID,
      fencing_token: 1,
      acquired_at_ms: ACQUIRED_AT_MS,
      renewed_at_ms: ACQUIRED_AT_MS,
      expires_at_ms: ACQUIRED_AT_MS + LEASE_DURATION_MS,
      source_header_sequence: 41,
      state: MigrationLeaseStateV1::Held,
    },
    algorithm,
  )
  .unwrap();
  publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationLease,
        identity: &MIGRATION_ID,
        expected: None,
        guards: &[],
        encoded_control: &lease,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 10_000,
      },
      &mut retirement,
    )
    .unwrap();
  let request = takeover_request([0x72; 16], 1, ACQUIRED_AT_MS + LEASE_DURATION_MS);

  let (owner, receipt) = MigrationStateOwnerV1::takeover(publisher.clone(), permit.clone(), request, &mut retirement).unwrap();
  assert_eq!(owner.fencing_token(), 2);
  assert_eq!(receipt.lease_control_sequence, 2);
  assert_eq!(receipt.progress_control_sequence, 1);
  assert_eq!(receipt.fencing_token, 2);
  assert!(!receipt.resumed_rebind);
  assert!(!receipt.idempotent);
  let progress =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let progress = decode_migration_progress_control(&progress.bytes, algorithm).unwrap();
  assert_eq!(progress.sequence, 1);
  assert_eq!(progress.body.fencing_token, 2);
  assert_eq!(progress.body.phase, MigrationPhaseV1::Preflight);
  assert_eq!(progress.body.state, MigrationProgressStateV1::Pending);

  let (_, retry) = MigrationStateOwnerV1::takeover(publisher, permit, request, &mut retirement).unwrap();
  assert!(retry.idempotent);
}

#[test]
fn acquisition_rejects_a_foreign_destination_and_an_active_foreign_holder() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, foreign_destination) = admit_migration_preflight_v1(&preflight_request(algorithm, [0x52; 16])).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let error = MigrationStateOwnerV1::acquire(publisher.clone(), foreign_destination, acquisition_request(HOLDER_BOOT_ID), &mut retirement)
    .unwrap_err();
  assert_eq!(error.code(), "migration_destination_identity");

  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let (owner, _) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request([0x62; 16]), &mut retirement).unwrap();
  drop(owner);
  let error = MigrationStateOwnerV1::acquire(publisher, permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap_err();
  assert_eq!(error.code(), "migration_lease_held_by_other_boot");
}

#[test]
fn acquisition_rejects_invalid_request_values_before_publishing_a_lease() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let invalid = [
    MigrationAcquisitionRequestV1 { holder_boot_id: [0; 16], ..acquisition_request(HOLDER_BOOT_ID) },
    MigrationAcquisitionRequestV1 { acquired_at_ms: -1, ..acquisition_request(HOLDER_BOOT_ID) },
    MigrationAcquisitionRequestV1 { lease_duration_ms: 0, ..acquisition_request(HOLDER_BOOT_ID) },
    MigrationAcquisitionRequestV1 { acquired_at_ms: i64::MAX, lease_duration_ms: 1, ..acquisition_request(HOLDER_BOOT_ID) },
    MigrationAcquisitionRequestV1 { publication_timestamp_ms: 0, ..acquisition_request(HOLDER_BOOT_ID) },
    MigrationAcquisitionRequestV1 { monotonic_now_ms: 0, ..acquisition_request(HOLDER_BOOT_ID) },
    MigrationAcquisitionRequestV1 { publication_timestamp_ms: u64::MAX, ..acquisition_request(HOLDER_BOOT_ID) },
    MigrationAcquisitionRequestV1 { monotonic_now_ms: u64::MAX, ..acquisition_request(HOLDER_BOOT_ID) },
    MigrationAcquisitionRequestV1 { publication_timestamp_ms: ACQUIRED_AT_MS as u64 - 1, ..acquisition_request(HOLDER_BOOT_ID) },
  ];
  let expected = [
    "migration_holder_boot_identity",
    "migration_lease_times",
    "migration_lease_times",
    "migration_lease_time_overflow",
    "migration_publication_times",
    "migration_publication_times",
    "migration_publication_time_range",
    "migration_progress_time_overflow",
    "migration_publication_before_transition",
  ];
  for (request, expected) in invalid.into_iter().zip(expected) {
    let error = MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), request, &mut retirement).unwrap_err();
    assert_eq!(error.code(), expected);
  }
  assert!(publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().is_none());
}

#[test]
fn acquisition_rejects_orphan_progress_and_an_expired_same_boot_lease() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let progress = encode_migration_progress_control(1, &initial_progress_body(&permit, 1), algorithm).unwrap();
  publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationProgress,
        identity: &MIGRATION_ID,
        expected: None,
        guards: &[],
        encoded_control: &progress,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 10_000,
      },
      &mut retirement,
    )
    .unwrap();
  let error = MigrationStateOwnerV1::acquire(publisher, permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap_err();
  assert_eq!(error.code(), "migration_progress_without_lease");

  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let lease = encode_migration_lease_control(
    1,
    &MigrationLeaseBodyV1 {
      database_id: DATABASE_ID,
      migration_id: MIGRATION_ID,
      source_physical_instance_id: SOURCE_PHYSICAL_ID,
      destination_physical_instance_id: DESTINATION_PHYSICAL_ID,
      holder_boot_id: HOLDER_BOOT_ID,
      fencing_token: 1,
      acquired_at_ms: ACQUIRED_AT_MS,
      renewed_at_ms: ACQUIRED_AT_MS,
      expires_at_ms: ACQUIRED_AT_MS + LEASE_DURATION_MS,
      source_header_sequence: 41,
      state: MigrationLeaseStateV1::Held,
    },
    algorithm,
  )
  .unwrap();
  publisher
    .publish_mutable_system_control(
      MutableSystemControlPublicationRequestV1 {
        database_id: &DATABASE_ID,
        kind: SystemControlKindV1::MigrationLease,
        identity: &MIGRATION_ID,
        expected: None,
        guards: &[],
        encoded_control: &lease,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 10_000,
      },
      &mut retirement,
    )
    .unwrap();
  let request = MigrationAcquisitionRequestV1 {
    acquired_at_ms: ACQUIRED_AT_MS + LEASE_DURATION_MS,
    publication_timestamp_ms: (ACQUIRED_AT_MS + LEASE_DURATION_MS + 100) as u64,
    ..acquisition_request(HOLDER_BOOT_ID)
  };
  let error = MigrationStateOwnerV1::acquire(publisher, permit, request, &mut retirement).unwrap_err();
  assert_eq!(error.code(), "migration_lease_expired");
}

#[test]
fn concurrent_foreign_holders_select_exactly_one_fenced_owner() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let barrier = Arc::new(Barrier::new(3));
  let mut workers = Vec::new();
  for holder_boot_id in [HOLDER_BOOT_ID, [0x62; 16]] {
    let publisher = publisher.clone();
    let permit = permit.clone();
    let barrier = barrier.clone();
    workers.push(thread::spawn(move || {
      let cancellation = CancellationToken::new();
      let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
      let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
      barrier.wait();
      let result = MigrationStateOwnerV1::acquire(publisher, permit, acquisition_request(holder_boot_id), &mut retirement)
        .map(|(owner, _)| owner.holder_boot_id())
        .map_err(|error| error.code());
      (holder_boot_id, result)
    }));
  }
  barrier.wait();
  let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
  assert_eq!(results.iter().filter(|(_, result)| result.is_ok()).count(), 1);
  assert_eq!(results.iter().filter(|(_, result)| result.is_err()).count(), 1);
  let selected = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let selected = decode_migration_lease_control(&selected.bytes, algorithm).unwrap();
  assert_eq!(Some(selected.body.holder_boot_id), results.iter().find_map(|(_, result)| result.as_ref().ok().copied()));
}

#[test]
fn acquisition_preserves_the_widest_database_hash_profile() {
  let algorithm = HashAlgorithm::Sha512;
  let (_directory, path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, receipt) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  assert_eq!(receipt.fencing_token, 1);
  owner.renew(renewal_request(ACQUIRED_AT_MS + 1_000), &mut retirement).unwrap();
  owner
    .transition_progress(
      progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Running, 0, ACQUIRED_AT_MS + 2_000),
      &mut retirement,
    )
    .unwrap();
  drop(owner);
  let (owner, takeover) = MigrationStateOwnerV1::takeover(
    publisher,
    permit,
    takeover_request([0x72; 16], 1, ACQUIRED_AT_MS + 1_000 + LEASE_DURATION_MS),
    &mut retirement,
  )
  .unwrap();
  assert_eq!(takeover.fencing_token, 2);
  drop(owner);

  let reopened = reopen(&path);
  let progress =
    reopened.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let progress = decode_migration_progress_control(&progress.bytes, algorithm).unwrap();
  assert_eq!(progress.sequence, 3);
  assert_eq!(progress.body.state, MigrationProgressStateV1::Running);
  assert_eq!(progress.body.fencing_token, 2);
  assert_eq!(progress.body.effective_config_fingerprint.len(), algorithm.hash_length());
  assert_eq!(progress.body.system_family_registry_fingerprint.len(), algorithm.hash_length());
}

#[test]
fn renewal_and_progress_reject_an_owner_fenced_by_another_holder() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  replace_lease(&publisher, algorithm, &mut retirement, ACQUIRED_AT_MS + 1_000, |lease| {
    lease.holder_boot_id = [0x62; 16];
  });

  assert_eq!(owner.renew(renewal_request(ACQUIRED_AT_MS + 2_000), &mut retirement).unwrap_err().code(), "migration_owner_fenced");
  let progress = progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Running, 0, ACQUIRED_AT_MS + 2_000);
  assert_eq!(owner.transition_progress(progress, &mut retirement).unwrap_err().code(), "migration_owner_fenced");
  let selected_progress =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(selected_progress.control_sequence, 1);
  let selected_lease =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(selected_lease.control_sequence, 2);
}

#[test]
fn migration_owner_remains_disconnected_from_live_service_authority() {
  let source = std::fs::read_to_string(format!("{}/src/engine/v4/migration_owner.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  for forbidden in ["StorageEngine", "DirectoryOps", "server::", "axum", "task_worker", "GarbageCollector"] {
    assert!(!source.contains(forbidden), "migration owner acquired forbidden live authority {forbidden}");
  }
  assert!(source.contains("V4FirstAuthorityPublisher"));
  assert!(source.contains("MigrationPreflightPermitV1"));
  assert!(source.contains("SystemControlKindV1::MigrationLease"));
  assert!(source.contains("SystemControlKindV1::MigrationProgress"));
  assert!(source.contains("load_mutable_system_control_selected_pair"));
  assert!(source.contains("flags: 0"), "initial acquisition must not claim source GC suspension");
  assert_ne!(SystemControlSlotV1::A, SystemControlSlotV1::B);

  let first_authority = std::fs::read_to_string(format!("{}/src/engine/v4/first_authority.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  let pair_read = first_authority
    .split_once("pub fn load_mutable_system_control_selected_pair(")
    .and_then(|(_, remainder)| remainder.split_once("pub fn publish_mutable_system_control(").map(|(pair_read, _)| pair_read))
    .expect("atomic mutable-control pair reader");
  assert_eq!(pair_read.matches("self.root_state.lock()").count(), 1);
  assert_eq!(pair_read.matches("load_mutable_system_control_pair(").count(), 2);
  assert!(pair_read.find("self.root_state.lock()").unwrap() < pair_read.find("load_mutable_system_control_pair(").unwrap());
}

#[test]
fn final_freeze_destination_expectation_is_checked_inside_first_authority_serialization() {
  let owner = std::fs::read_to_string(format!("{}/src/engine/v4/migration_owner.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  let completion = owner
    .split_once("pub fn complete_final_freeze(")
    .and_then(|(_, remainder)| remainder.split_once("pub fn observe_capture_state(").map(|(completion, _)| completion))
    .expect("specialized final-freeze completion method boundary");
  assert!(completion.contains("validate_for_completion"));
  assert!(completion.contains("publish_progress_body_with_authority_expectation"));
  assert!(
    completion.find("validate_for_completion").unwrap() < completion.rfind("publish_progress_body_with_authority_expectation").unwrap()
  );

  let first_authority = std::fs::read_to_string(format!("{}/src/engine/v4/first_authority.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  let guarded = first_authority
    .split_once("fn publish_mutable_system_control_with_observer(")
    .and_then(|(_, remainder)| remainder.split_once("pub fn publish_index_operation_control(").map(|(guarded, _)| guarded))
    .expect("guarded mutable-control publisher boundary");
  let retirement = guarded.find("retirement_owner.flush").unwrap();
  let lock = guarded.find("self.root_state.lock()").unwrap();
  let observation = guarded.find("self.observe()").unwrap();
  let expectation = guarded.find("if let Some(expected) = authority_expectation").unwrap();
  let kv = guarded.find("self.lock_kv()").unwrap();
  assert!(retirement < lock && lock < observation && observation < expectation && expectation < kv);
}

#[test]
fn acquisition_has_no_fallible_post_publication_readback_window() {
  let source = std::fs::read_to_string(format!("{}/src/engine/v4/migration_owner.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  let acquisition = source
    .split_once("pub fn acquire(")
    .and_then(|(_, remainder)| remainder.split_once("pub fn renew(").map(|(acquisition, _)| acquisition))
    .expect("migration acquisition method boundary");
  assert_eq!(acquisition.matches("load_migration_controls(").count(), 1);
  assert!(!acquisition.contains("load_mutable_system_control("));
}

#[test]
fn release_and_takeover_prepare_fallible_state_before_their_first_publication() {
  let source = std::fs::read_to_string(format!("{}/src/engine/v4/migration_owner.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  let release = source
    .split_once("pub fn release(")
    .and_then(|(_, remainder)| remainder.split_once("pub fn takeover(").map(|(release, _)| release))
    .expect("migration release method boundary");
  assert_eq!(release.matches("require_migration_controls(").count(), 1);
  assert!(release.rfind("encode_migration_lease_control").unwrap() < release.find("publish_control(").unwrap());

  let takeover = source
    .split_once("pub fn takeover(")
    .and_then(|(_, remainder)| remainder.split_once("pub(crate) fn advance_cutover_progress(").map(|(takeover, _)| takeover))
    .expect("migration takeover method boundary");
  assert_eq!(takeover.matches("load_migration_controls(").count(), 1);
  assert_eq!(takeover.matches("require_migration_controls(").count(), 0);
  assert!(takeover.contains("initial_progress_body("));
  assert!(takeover.rfind("encode_migration_progress_control").unwrap() < takeover.find("publish_control(").unwrap());
}

#[test]
fn renewal_extends_the_exact_held_lease_and_retries_idempotently_after_reopen() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let request = renewal_request(ACQUIRED_AT_MS + 10_000);

  let receipt = owner.renew(request, &mut retirement).unwrap();
  assert_eq!(receipt.control_sequence, 2);
  assert!(!receipt.idempotent);
  let retry = owner.renew(request, &mut retirement).unwrap();
  assert_eq!(retry.control_sequence, 2);
  assert!(retry.idempotent);
  drop(owner);

  let reopened = reopen(&path);
  let lease = reopened.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let lease = decode_migration_lease_control(&lease.bytes, algorithm).unwrap();
  assert_eq!(lease.sequence, 2);
  assert_eq!(lease.body.renewed_at_ms, request.renewed_at_ms);
  assert_eq!(lease.body.expires_at_ms, request.renewed_at_ms + request.lease_duration_ms);
  assert_eq!(lease.body.fencing_token, 1);
}

#[test]
fn progress_transitions_are_sequential_monotonic_and_file_backed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED;
  });

  let running = progress_transition(
    algorithm,
    MigrationPhaseV1::Preflight,
    MigrationProgressStateV1::Running,
    MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    ACQUIRED_AT_MS + 1_000,
  );
  assert_eq!(owner.transition_progress(running.clone(), &mut retirement).unwrap().control_sequence, 3);
  assert!(owner.transition_progress(running, &mut retirement).unwrap().idempotent);
  let later_running = progress_transition(
    algorithm,
    MigrationPhaseV1::Preflight,
    MigrationProgressStateV1::Running,
    MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    ACQUIRED_AT_MS + 1_100,
  );
  assert!(owner.transition_progress_after_restart(later_running.clone(), &mut retirement).unwrap().idempotent);
  let still_running =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(still_running.control_sequence, 3, "covered restart progress must not republish the control");
  let mut uncovered_running = later_running;
  uncovered_running.copied_bytes += 1;
  assert_eq!(
    owner.transition_progress_after_restart(uncovered_running, &mut retirement).unwrap_err().code(),
    "migration_restart_progress_incomplete",
  );

  let complete = progress_transition(
    algorithm,
    MigrationPhaseV1::Preflight,
    MigrationProgressStateV1::Complete,
    MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    ACQUIRED_AT_MS + 2_000,
  );
  owner.transition_progress(complete, &mut retirement).unwrap();
  let copy = progress_transition(
    algorithm,
    MigrationPhaseV1::Copy,
    MigrationProgressStateV1::Pending,
    MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    ACQUIRED_AT_MS + 3_000,
  );
  let receipt = owner.transition_progress(copy, &mut retirement).unwrap();
  assert_eq!(receipt.phase, MigrationPhaseV1::Copy);
  assert_eq!(receipt.state, MigrationProgressStateV1::Pending);
  let memory_before_recovery = memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes;
  let journal = publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 16, 16, 16, 1 << 20).unwrap().unwrap();
  assert_eq!(journal.segment_count, 3);
  assert_eq!(journal.record_count, 3);
  assert_eq!(journal.last_segment_ordinal, 3);
  assert_eq!(
    publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 0, 16, 16, 1 << 20).unwrap_err().code(),
    "retirement_journal_recovery_bounds",
  );
  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    publisher.reconstruct_retirement_journal_summary(&canceled, &memory, 16, 16, 16, 1 << 20).unwrap_err().code(),
    "retirement_journal_recovery_cancelled",
  );
  assert_eq!(
    publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 1, 1, 16, 1 << 20).unwrap_err().code(),
    "retirement_journal_recovery_artifact_limit",
  );
  assert_eq!(
    publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 16, 1, 16, 1 << 20).unwrap_err().code(),
    "retirement_journal_recovery_segment_limit",
  );
  assert_eq!(
    publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 16, 16, 1, 1 << 20).unwrap_err().code(),
    "retirement_journal_record_limit",
  );
  assert_eq!(
    publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 16, 16, 16, 1).unwrap_err().code(),
    "retirement_journal_recovery_memory",
  );
  assert_eq!(
    memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes,
    memory_before_recovery,
    "every recovery success and refusal must release its transient reservations",
  );
  drop(owner);

  let reopened = reopen(&path);
  let progress =
    reopened.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let progress = decode_migration_progress_control(&progress.bytes, algorithm).unwrap();
  assert_eq!(progress.body.phase, MigrationPhaseV1::Copy);
  assert_eq!(progress.body.state, MigrationProgressStateV1::Pending);
  assert_eq!(progress.body.copied_through_write_sequence, 11);
  assert!(progress.body.checkpoint_artifact.iter().all(|byte| *byte == 0));
}

#[test]
fn retirement_journal_recovery_distinguishes_empty_authority_from_a_forked_chain() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  assert!(
    publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 16, 16, 16, 1 << 20).unwrap().is_none(),
    "a fresh authority has no retirement chain to resume",
  );

  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let mut first_chain = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut first_chain).unwrap();
  replace_progress(&publisher, algorithm, &mut first_chain, |progress| {
    progress.flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED;
  });
  owner
    .transition_progress(
      progress_transition(
        algorithm,
        MigrationPhaseV1::Preflight,
        MigrationProgressStateV1::Running,
        MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
        ACQUIRED_AT_MS + 1_000,
      ),
      &mut first_chain,
    )
    .unwrap();

  let mut forked_chain = retirement_owner(algorithm, &cancellation, &memory);
  owner
    .transition_progress(
      progress_transition(
        algorithm,
        MigrationPhaseV1::Preflight,
        MigrationProgressStateV1::Complete,
        MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
        ACQUIRED_AT_MS + 2_000,
      ),
      &mut forked_chain,
    )
    .unwrap();
  assert_eq!(
    publisher.reconstruct_retirement_journal_summary(&cancellation, &memory, 16, 16, 16, 1 << 20).unwrap_err().code(),
    "retirement_journal_unexpected_reset",
  );
}

#[test]
fn renewal_and_progress_refuse_regression_expiry_and_unowned_flag_claims_without_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();

  let shortened = MigrationLeaseRenewalRequestV1 { lease_duration_ms: 1, ..renewal_request(ACQUIRED_AT_MS + 1_000) };
  assert_eq!(owner.renew(shortened, &mut retirement).unwrap_err().code(), "migration_renewal_not_extended");
  let expired = renewal_request(ACQUIRED_AT_MS + LEASE_DURATION_MS);
  assert_eq!(owner.renew(expired, &mut retirement).unwrap_err().code(), "migration_lease_expired");
  let publication_before_renewal =
    MigrationLeaseRenewalRequestV1 { publication_timestamp_ms: (ACQUIRED_AT_MS + 999) as u64, ..renewal_request(ACQUIRED_AT_MS + 1_000) };
  assert_eq!(owner.renew(publication_before_renewal, &mut retirement).unwrap_err().code(), "migration_publication_before_transition");
  let expired_by_publication = MigrationLeaseRenewalRequestV1 {
    renewed_at_ms: ACQUIRED_AT_MS + LEASE_DURATION_MS - 1,
    publication_timestamp_ms: (ACQUIRED_AT_MS + LEASE_DURATION_MS) as u64,
    ..renewal_request(ACQUIRED_AT_MS + LEASE_DURATION_MS - 1)
  };
  assert_eq!(owner.renew(expired_by_publication, &mut retirement).unwrap_err().code(), "migration_lease_expired");

  let unauthorized_flag = progress_transition(
    algorithm,
    MigrationPhaseV1::Preflight,
    MigrationProgressStateV1::Running,
    MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    ACQUIRED_AT_MS + 1_000,
  );
  assert_eq!(owner.transition_progress(unauthorized_flag, &mut retirement).unwrap_err().code(), "migration_progress_flag_authority");
  let unauthorized_reconcile_latch = progress_transition(
    algorithm,
    MigrationPhaseV1::Preflight,
    MigrationProgressStateV1::Running,
    MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE,
    ACQUIRED_AT_MS + 1_000,
  );
  assert_eq!(
    owner.transition_progress(unauthorized_reconcile_latch, &mut retirement).unwrap_err().code(),
    "migration_progress_flag_authority"
  );
  let skipped_phase =
    progress_transition(algorithm, MigrationPhaseV1::Reconcile, MigrationProgressStateV1::Pending, 0, ACQUIRED_AT_MS + 1_000);
  assert_eq!(owner.transition_progress(skipped_phase, &mut retirement).unwrap_err().code(), "migration_progress_phase_sequence");
  let publication_before_progress = MigrationProgressTransitionRequestV1 {
    publication_timestamp_ms: (ACQUIRED_AT_MS + 999) as u64,
    ..progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Running, 0, ACQUIRED_AT_MS + 1_000)
  };
  assert_eq!(
    owner.transition_progress(publication_before_progress, &mut retirement).unwrap_err().code(),
    "migration_publication_before_transition"
  );
  let expired_by_progress_publication = MigrationProgressTransitionRequestV1 {
    updated_at_ms: ACQUIRED_AT_MS + LEASE_DURATION_MS - 1,
    publication_timestamp_ms: (ACQUIRED_AT_MS + LEASE_DURATION_MS) as u64,
    ..progress_transition(
      algorithm,
      MigrationPhaseV1::Preflight,
      MigrationProgressStateV1::Running,
      0,
      ACQUIRED_AT_MS + LEASE_DURATION_MS - 1,
    )
  };
  assert_eq!(owner.transition_progress(expired_by_progress_publication, &mut retirement).unwrap_err().code(), "migration_lease_expired");

  let progress =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(progress.control_sequence, 1);
  let lease = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(lease.control_sequence, 1);
}

#[test]
fn capture_checkpoint_and_full_reconciliation_latch_have_one_specialized_monotonic_authority() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let checkpoint_hash = digest_parts(algorithm, &[b"selected capture checkpoint"]);
  let checkpoint = MigrationCaptureCheckpointPublicationRequestV1 {
    captured_through_publication_sequence: 7,
    checkpoint_artifact: checkpoint_hash.clone(),
    updated_at_ms: ACQUIRED_AT_MS + 1_000,
    publication_timestamp_ms: (ACQUIRED_AT_MS + 1_100) as u64,
    monotonic_now_ms: 31_000,
  };
  let checkpoint_receipt = owner.publish_capture_checkpoint(checkpoint.clone(), &mut retirement).unwrap();
  assert_eq!(checkpoint_receipt.control_sequence, 2);
  assert!(owner.publish_capture_checkpoint(checkpoint.clone(), &mut retirement).unwrap().idempotent);
  let regressed_checkpoint = MigrationCaptureCheckpointPublicationRequestV1 {
    captured_through_publication_sequence: 6,
    checkpoint_artifact: digest_parts(algorithm, &[b"regressed capture checkpoint"]),
    updated_at_ms: checkpoint.updated_at_ms + 1,
    publication_timestamp_ms: checkpoint.publication_timestamp_ms + 1,
    monotonic_now_ms: checkpoint.monotonic_now_ms + 1,
  };
  assert_eq!(
    owner.publish_capture_checkpoint(regressed_checkpoint, &mut retirement).unwrap_err().code(),
    "migration_progress_scalar_regression"
  );

  let failure_evidence = digest_parts(algorithm, &[b"capture queue exhausted"]);
  let invalid_latch = MigrationFullReconciliationLatchRequestV1 {
    last_error_evidence: vec![0; algorithm.hash_length()],
    updated_at_ms: ACQUIRED_AT_MS + 2_000,
    publication_timestamp_ms: (ACQUIRED_AT_MS + 2_100) as u64,
    monotonic_now_ms: 32_000,
  };
  assert_eq!(
    owner.latch_needs_full_reconciliation(invalid_latch, &mut retirement).unwrap_err().code(),
    "migration_capture_failure_evidence"
  );
  let latch = MigrationFullReconciliationLatchRequestV1 {
    last_error_evidence: failure_evidence.clone(),
    updated_at_ms: ACQUIRED_AT_MS + 2_000,
    publication_timestamp_ms: (ACQUIRED_AT_MS + 2_100) as u64,
    monotonic_now_ms: 32_000,
  };
  let latch_receipt = owner.latch_needs_full_reconciliation(latch.clone(), &mut retirement).unwrap();
  assert_eq!(latch_receipt.control_sequence, 3);
  assert!(owner.latch_needs_full_reconciliation(latch, &mut retirement).unwrap().idempotent);
  let checkpoint_after_latch = MigrationCaptureCheckpointPublicationRequestV1 {
    captured_through_publication_sequence: 8,
    checkpoint_artifact: digest_parts(algorithm, &[b"checkpoint after capture loss"]),
    updated_at_ms: ACQUIRED_AT_MS + 3_000,
    publication_timestamp_ms: (ACQUIRED_AT_MS + 3_100) as u64,
    monotonic_now_ms: 33_000,
  };
  assert_eq!(
    owner.publish_capture_checkpoint(checkpoint_after_latch, &mut retirement).unwrap_err().code(),
    "migration_capture_already_inexact"
  );
  let clear_latch =
    progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Pending, 0, ACQUIRED_AT_MS + 3_000);
  assert_eq!(owner.transition_progress(clear_latch, &mut retirement).unwrap_err().code(), "migration_progress_flag_regression");
  drop(owner);

  let reopened = reopen(&path);
  let progress =
    reopened.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let progress = decode_migration_progress_control(&progress.bytes, algorithm).unwrap();
  assert_eq!(progress.body.flags & MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE, MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE);
  assert_eq!(progress.body.captured_through_publication_sequence, 7);
  assert_eq!(progress.body.checkpoint_artifact, checkpoint_hash);
  assert_eq!(progress.body.last_error_evidence, failure_evidence);
}

#[test]
fn replay_checkpoint_is_monotonic_exactly_idempotent_and_bound_to_capture() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher, permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  owner
    .publish_capture_checkpoint(
      MigrationCaptureCheckpointPublicationRequestV1 {
        captured_through_publication_sequence: 7,
        checkpoint_artifact: digest_parts(algorithm, &[b"replay checkpoint capture"]),
        updated_at_ms: ACQUIRED_AT_MS + 1_000,
        publication_timestamp_ms: (ACQUIRED_AT_MS + 1_100) as u64,
        monotonic_now_ms: 31_000,
      },
      &mut retirement,
    )
    .unwrap();
  let checkpoint = MigrationReplayCheckpointPublicationRequestV1 {
    reconciled_through_publication_sequence: 5,
    destination_header_sequence: 17,
    updated_at_ms: ACQUIRED_AT_MS + 2_000,
    publication_timestamp_ms: (ACQUIRED_AT_MS + 2_100) as u64,
    monotonic_now_ms: 32_000,
  };
  assert!(!owner.publish_replay_checkpoint(checkpoint, &mut retirement).unwrap().idempotent);
  assert!(owner.publish_replay_checkpoint(checkpoint, &mut retirement).unwrap().idempotent);

  let conflict = MigrationReplayCheckpointPublicationRequestV1 { destination_header_sequence: 18, ..checkpoint };
  assert_eq!(owner.publish_replay_checkpoint(conflict, &mut retirement).unwrap_err().code(), "migration_replay_checkpoint_conflict");
  let regression = MigrationReplayCheckpointPublicationRequestV1 {
    reconciled_through_publication_sequence: 4,
    destination_header_sequence: 16,
    ..checkpoint
  };
  assert_eq!(owner.publish_replay_checkpoint(regression, &mut retirement).unwrap_err().code(), "migration_progress_scalar_regression");
  let beyond_capture = MigrationReplayCheckpointPublicationRequestV1 {
    reconciled_through_publication_sequence: 8,
    destination_header_sequence: 19,
    ..checkpoint
  };
  assert_eq!(owner.publish_replay_checkpoint(beyond_capture, &mut retirement).unwrap_err().code(), "migration_replay_beyond_capture");
  let missing_destination = MigrationReplayCheckpointPublicationRequestV1 { destination_header_sequence: 0, ..checkpoint };
  assert_eq!(
    owner.publish_replay_checkpoint(missing_destination, &mut retirement).unwrap_err().code(),
    "migration_replay_destination_sequence"
  );
  let missing_source = MigrationReplayCheckpointPublicationRequestV1 { reconciled_through_publication_sequence: 0, ..checkpoint };
  assert_eq!(owner.publish_replay_checkpoint(missing_source, &mut retirement).unwrap_err().code(), "migration_replay_destination_sequence");
}

#[test]
fn progress_state_machine_refuses_every_regression_and_unproven_phase_boundary() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();

  let paused = progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Paused, 0, ACQUIRED_AT_MS + 1_000);
  assert_eq!(owner.transition_progress(paused, &mut retirement).unwrap_err().code(), "migration_progress_state_transition");
  let failed_without_evidence =
    progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Failed, 0, ACQUIRED_AT_MS + 1_000);
  assert_eq!(
    owner.transition_progress(failed_without_evidence, &mut retirement).unwrap_err().code(),
    "migration_progress_failure_evidence"
  );
  let copy_before_complete =
    progress_transition(algorithm, MigrationPhaseV1::Copy, MigrationProgressStateV1::Pending, 0, ACQUIRED_AT_MS + 1_000);
  assert_eq!(owner.transition_progress(copy_before_complete, &mut retirement).unwrap_err().code(), "migration_progress_phase_boundary");

  let running = progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Running, 0, ACQUIRED_AT_MS + 2_000);
  owner.transition_progress(running.clone(), &mut retirement).unwrap();
  let time_regression = MigrationProgressTransitionRequestV1 {
    updated_at_ms: running.updated_at_ms - 1,
    publication_timestamp_ms: running.publication_timestamp_ms + 1,
    monotonic_now_ms: running.monotonic_now_ms + 1,
    ..running.clone()
  };
  assert_eq!(owner.transition_progress(time_regression, &mut retirement).unwrap_err().code(), "migration_progress_time_regression");

  for scalar_regression in [
    MigrationProgressTransitionRequestV1 { destination_header_sequence: 6, ..running.clone() },
    MigrationProgressTransitionRequestV1 { copied_through_write_sequence: 10, ..running.clone() },
    MigrationProgressTransitionRequestV1 { namespace_count: 16, ..running.clone() },
    MigrationProgressTransitionRequestV1 { entity_count: 18, ..running.clone() },
    MigrationProgressTransitionRequestV1 { copied_bytes: 22, ..running.clone() },
  ] {
    assert_eq!(owner.transition_progress(scalar_regression, &mut retirement).unwrap_err().code(), "migration_progress_scalar_regression");
  }

  let complete = progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Complete, 0, ACQUIRED_AT_MS + 3_000);
  owner.transition_progress(complete, &mut retirement).unwrap();
  let copy_without_gc =
    progress_transition(algorithm, MigrationPhaseV1::Copy, MigrationProgressStateV1::Pending, 0, ACQUIRED_AT_MS + 4_000);
  assert_eq!(owner.transition_progress(copy_without_gc, &mut retirement).unwrap_err().code(), "migration_progress_gc_suspension_required");

  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.phase = MigrationPhaseV1::FinalFreeze;
    progress.state = MigrationProgressStateV1::Complete;
    progress.flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED;
    progress.updated_at_ms = ACQUIRED_AT_MS + 5_000;
  });
  let destination_verify_without_freeze = progress_transition(
    algorithm,
    MigrationPhaseV1::DestinationVerify,
    MigrationProgressStateV1::Pending,
    MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    ACQUIRED_AT_MS + 6_000,
  );
  assert_eq!(
    owner.transition_progress(destination_verify_without_freeze, &mut retirement).unwrap_err().code(),
    "migration_progress_specialized_authority"
  );

  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.phase = MigrationPhaseV1::DestinationVerify;
    progress.state = MigrationProgressStateV1::Complete;
    progress.flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD;
    progress.updated_at_ms = ACQUIRED_AT_MS + 7_000;
  });
  let cutover_without_verification = progress_transition(
    algorithm,
    MigrationPhaseV1::Cutover,
    MigrationProgressStateV1::Pending,
    MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD,
    ACQUIRED_AT_MS + 8_000,
  );
  assert_eq!(
    owner.transition_progress(cutover_without_verification, &mut retirement).unwrap_err().code(),
    "migration_progress_specialized_authority"
  );

  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.state = MigrationProgressStateV1::Canceled;
    progress.updated_at_ms = ACQUIRED_AT_MS + 9_000;
  });
  let reopen_canceled = progress_transition(
    algorithm,
    MigrationPhaseV1::DestinationVerify,
    MigrationProgressStateV1::Running,
    MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD,
    ACQUIRED_AT_MS + 10_000,
  );
  assert_eq!(owner.transition_progress(reopen_canceled, &mut retirement).unwrap_err().code(), "migration_progress_specialized_authority");
}

#[test]
fn generic_progress_cannot_complete_final_freeze_or_enter_a_later_phase() {
  for target in [
    (MigrationPhaseV1::FinalFreeze, MigrationProgressStateV1::Complete),
    (MigrationPhaseV1::DestinationVerify, MigrationProgressStateV1::Pending),
  ] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, _path, publisher) = create_publisher(algorithm);
    let publisher = Arc::new(publisher);
    let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
    let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
    let (owner, _) =
      MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
    replace_progress(&publisher, algorithm, &mut retirement, |progress| {
      progress.phase = MigrationPhaseV1::FinalFreeze;
      progress.state =
        if target.0 == MigrationPhaseV1::FinalFreeze { MigrationProgressStateV1::Running } else { MigrationProgressStateV1::Complete };
      progress.flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD;
      progress.updated_at_ms = ACQUIRED_AT_MS + 5_000;
    });
    let request = progress_transition(
      algorithm,
      target.0,
      target.1,
      MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD,
      ACQUIRED_AT_MS + 6_000,
    );

    assert_eq!(owner.transition_progress(request, &mut retirement).unwrap_err().code(), "migration_progress_specialized_authority");
  }
}

#[test]
fn generic_progress_cannot_establish_the_selected_legacy_root_map_hash() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let mut request =
    progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Running, 0, ACQUIRED_AT_MS + 1_000);
  request.legacy_root_map_control_payload_hash = digest_parts(algorithm, &[b"forged selected root map"]);

  assert_eq!(owner.transition_progress(request, &mut retirement).unwrap_err().code(), "migration_progress_specialized_authority");
  let selected =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let progress = decode_migration_progress_control(&selected.bytes, algorithm).unwrap();
  assert!(progress.body.legacy_root_map_control_payload_hash.iter().all(|byte| *byte == 0));
}

#[test]
fn restart_progress_replays_earlier_zero_evidence_targets_after_root_map_selection() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.phase = MigrationPhaseV1::DestinationVerify;
    progress.state = MigrationProgressStateV1::Running;
    progress.flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD;
    progress.destination_header_sequence = 7;
    progress.copied_through_write_sequence = 11;
    progress.reconciled_through_publication_sequence = 11;
    progress.namespace_count = 17;
    progress.entity_count = 19;
    progress.copied_bytes = 23;
    progress.legacy_root_map_control_payload_hash = digest_parts(algorithm, &[b"selected root map"]);
    progress.updated_at_ms = ACQUIRED_AT_MS + 2_000;
  });
  let selected_before =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let mut replay = progress_transition(
    algorithm,
    MigrationPhaseV1::Preflight,
    MigrationProgressStateV1::Running,
    MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    ACQUIRED_AT_MS + 3_000,
  );
  replay.destination_header_sequence = 0;
  replay.copied_through_write_sequence = 0;
  replay.namespace_count = 0;
  replay.entity_count = 0;
  replay.copied_bytes = 0;

  assert!(owner.transition_progress_after_restart(replay.clone(), &mut retirement).unwrap().idempotent);
  let mut malformed = replay.clone();
  malformed.legacy_root_map_control_payload_hash.clear();
  assert_eq!(owner.transition_progress_after_restart(malformed, &mut retirement).unwrap_err().code(), "migration_control_length");
  let mut missing_gc_suspension = replay.clone();
  missing_gc_suspension.phase = MigrationPhaseV1::Copy;
  missing_gc_suspension.state = MigrationProgressStateV1::Pending;
  missing_gc_suspension.flags = 0;
  assert_eq!(
    owner.transition_progress_after_restart(missing_gc_suspension, &mut retirement).unwrap_err().code(),
    "migration_progress_gc_suspension_required",
  );
  let mut terminal_target = replay;
  terminal_target.state = MigrationProgressStateV1::Failed;
  terminal_target.last_error_evidence = digest_parts(algorithm, &[b"invalid automatic failure replay"]);
  assert_eq!(
    owner.transition_progress_after_restart(terminal_target, &mut retirement).unwrap_err().code(),
    "migration_restart_target_state",
  );
  let selected_after =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(selected_after.control_sequence, selected_before.control_sequence);
  assert_eq!(selected_after.bytes, selected_before.bytes);
}

#[test]
fn restart_progress_refuses_terminal_persisted_state_even_when_the_requested_phase_is_later() {
  for state in [MigrationProgressStateV1::Paused, MigrationProgressStateV1::Failed, MigrationProgressStateV1::Canceled] {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, _path, publisher) = create_publisher(algorithm);
    let publisher = Arc::new(publisher);
    let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
    let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
    let (owner, _) =
      MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
    replace_progress(&publisher, algorithm, &mut retirement, |progress| {
      progress.state = state;
      progress.last_error_evidence = if state == MigrationProgressStateV1::Failed {
        digest_parts(algorithm, &[b"persisted failure"])
      } else {
        vec![0; algorithm.hash_length()]
      };
      progress.updated_at_ms = ACQUIRED_AT_MS + 2_000;
    });
    let request = progress_transition(
      algorithm,
      MigrationPhaseV1::Copy,
      MigrationProgressStateV1::Pending,
      MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
      ACQUIRED_AT_MS + 3_000,
    );

    assert_eq!(
      owner.transition_progress_after_restart(request, &mut retirement).unwrap_err().code(),
      "migration_restart_terminal_progress",
      "terminal state {state:?}",
    );
  }
}

#[test]
fn terminal_progress_and_established_evidence_cannot_be_reopened_or_cleared() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();

  let checkpoint_hash = digest_parts(algorithm, &[b"retained capture checkpoint"]);
  owner
    .publish_capture_checkpoint(
      MigrationCaptureCheckpointPublicationRequestV1 {
        captured_through_publication_sequence: 1,
        checkpoint_artifact: checkpoint_hash.clone(),
        updated_at_ms: ACQUIRED_AT_MS + 500,
        publication_timestamp_ms: (ACQUIRED_AT_MS + 600) as u64,
        monotonic_now_ms: 30_500,
      },
      &mut retirement,
    )
    .unwrap();
  let running = progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Running, 0, ACQUIRED_AT_MS + 1_000);
  owner.transition_progress(running, &mut retirement).unwrap();
  let selected =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(decode_migration_progress_control(&selected.bytes, algorithm).unwrap().body.checkpoint_artifact, checkpoint_hash);

  let mut failed = progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Failed, 0, ACQUIRED_AT_MS + 3_000);
  failed.last_error_evidence = digest_parts(algorithm, &[b"migration failure"]);
  owner.transition_progress(failed, &mut retirement).unwrap();
  let reopened = progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Running, 0, ACQUIRED_AT_MS + 4_000);
  assert_eq!(owner.transition_progress(reopened.clone(), &mut retirement).unwrap_err().code(), "migration_progress_terminal");
  assert_eq!(owner.transition_progress(reopened, &mut retirement).unwrap_err().code(), "migration_progress_terminal");
}

#[test]
fn early_terminal_release_is_two_step_idempotent_and_file_backed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher, permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let mut failed = progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Failed, 0, ACQUIRED_AT_MS + 1_000);
  failed.last_error_evidence = digest_parts(algorithm, &[b"release failure evidence"]);
  owner.transition_progress(failed, &mut retirement).unwrap();
  let request = release_request((ACQUIRED_AT_MS + 2_000) as u64);

  let receipt = owner.release(request, &mut retirement).unwrap();
  assert_eq!(receipt.control_sequence, 3);
  assert_eq!(receipt.fencing_token, 1);
  assert!(!receipt.resumed_releasing);
  assert!(!receipt.idempotent);
  let retry = owner.release(request, &mut retirement).unwrap();
  assert_eq!(retry.control_sequence, 3);
  assert!(retry.idempotent);
  drop(owner);

  let reopened = reopen(&path);
  let lease = reopened.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let lease = decode_migration_lease_control(&lease.bytes, algorithm).unwrap();
  assert_eq!(lease.sequence, 3);
  assert_eq!(lease.body.state, MigrationLeaseStateV1::Released);
  assert_eq!(lease.body.fencing_token, 1);
}

#[test]
fn release_resumes_a_releasing_crash_prefix_even_after_expiry() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let canceled = progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Canceled, 0, ACQUIRED_AT_MS + 1_000);
  owner.transition_progress(canceled, &mut retirement).unwrap();
  replace_lease(&publisher, algorithm, &mut retirement, ACQUIRED_AT_MS + 2_000, |lease| {
    lease.state = MigrationLeaseStateV1::Releasing;
  });

  let receipt = owner.release(release_request((ACQUIRED_AT_MS + LEASE_DURATION_MS + 3_000) as u64), &mut retirement).unwrap();
  assert_eq!(receipt.control_sequence, 3);
  assert!(receipt.resumed_releasing);
  assert!(!receipt.idempotent);
  let lease = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(decode_migration_lease_control(&lease.bytes, algorithm).unwrap().body.state, MigrationLeaseStateV1::Released);
}

#[test]
fn release_rejects_active_late_and_foreign_state_without_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let request = release_request((ACQUIRED_AT_MS + 2_000) as u64);

  for (invalid, code) in [
    (MigrationLeaseReleaseRequestV1 { publication_timestamp_ms: 0, ..request }, "migration_publication_times"),
    (MigrationLeaseReleaseRequestV1 { monotonic_now_ms: 0, ..request }, "migration_publication_times"),
    (MigrationLeaseReleaseRequestV1 { publication_timestamp_ms: i64::MAX as u64, ..request }, "migration_publication_time_range"),
    (MigrationLeaseReleaseRequestV1 { monotonic_now_ms: u64::MAX, ..request }, "migration_release_time_overflow"),
  ] {
    assert_eq!(owner.release(invalid, &mut retirement).unwrap_err().code(), code);
  }
  assert_eq!(owner.release(request, &mut retirement).unwrap_err().code(), "migration_release_progress_not_terminal");
  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.phase = MigrationPhaseV1::FinalFreeze;
    progress.state = MigrationProgressStateV1::Canceled;
    progress.flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED;
    progress.updated_at_ms = ACQUIRED_AT_MS + 1_000;
  });
  assert_eq!(owner.release(request, &mut retirement).unwrap_err().code(), "migration_release_after_final_freeze");
  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.phase = MigrationPhaseV1::Preflight;
    progress.updated_at_ms = ACQUIRED_AT_MS + 2_000;
  });
  replace_lease(&publisher, algorithm, &mut retirement, ACQUIRED_AT_MS + 13_000, |lease| {
    lease.holder_boot_id = [0x62; 16];
  });
  assert_eq!(owner.release(request, &mut retirement).unwrap_err().code(), "migration_owner_fenced");
  replace_lease(&publisher, algorithm, &mut retirement, ACQUIRED_AT_MS + 24_000, |lease| {
    lease.holder_boot_id = HOLDER_BOOT_ID;
    lease.state = MigrationLeaseStateV1::Expired;
  });
  assert_eq!(
    owner.release(release_request((ACQUIRED_AT_MS + 25_000) as u64), &mut retirement).unwrap_err().code(),
    "migration_lease_expired_state"
  );
  let lease = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(lease.control_sequence, 3);
}

#[test]
fn expired_takeover_advances_the_token_rebinds_progress_and_retries_exactly() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (old_owner, _) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let request = takeover_request([0x72; 16], 1, ACQUIRED_AT_MS + LEASE_DURATION_MS);

  let (new_owner, receipt) = MigrationStateOwnerV1::takeover(publisher.clone(), permit.clone(), request, &mut retirement).unwrap();
  assert_eq!(new_owner.fencing_token(), 2);
  assert_eq!(new_owner.holder_boot_id(), request.new_holder_boot_id);
  assert_eq!(receipt.lease_control_sequence, 2);
  assert_eq!(receipt.progress_control_sequence, 2);
  assert_eq!(receipt.fencing_token, 2);
  assert!(!receipt.resumed_rebind);
  assert!(!receipt.idempotent);
  assert_eq!(
    old_owner.renew(renewal_request(ACQUIRED_AT_MS + LEASE_DURATION_MS + 1_000), &mut retirement).unwrap_err().code(),
    "migration_owner_fenced"
  );

  let (_, retry) = MigrationStateOwnerV1::takeover(publisher, permit, request, &mut retirement).unwrap();
  assert!(retry.idempotent);
  drop(new_owner);
  drop(old_owner);

  let reopened = reopen(&path);
  let lease = reopened.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let progress =
    reopened.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let lease = decode_migration_lease_control(&lease.bytes, algorithm).unwrap();
  let progress = decode_migration_progress_control(&progress.bytes, algorithm).unwrap();
  assert_eq!(lease.body.fencing_token, 2);
  assert_eq!(lease.body.holder_boot_id, request.new_holder_boot_id);
  assert_eq!(progress.body.fencing_token, 2);
}

#[test]
fn takeover_accepts_a_durably_expired_lease_prefix() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  drop(owner);
  replace_lease(&publisher, algorithm, &mut retirement, ACQUIRED_AT_MS + 1_000, |lease| {
    lease.state = MigrationLeaseStateV1::Expired;
  });
  let request = takeover_request([0x72; 16], 1, ACQUIRED_AT_MS + LEASE_DURATION_MS + 1_000);

  let (owner, receipt) = MigrationStateOwnerV1::takeover(publisher.clone(), permit, request, &mut retirement).unwrap();
  assert_eq!(owner.fencing_token(), 2);
  assert_eq!(receipt.lease_control_sequence, 3);
  assert_eq!(receipt.progress_control_sequence, 2);
  let lease = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(decode_migration_lease_control(&lease.bytes, algorithm).unwrap().body.state, MigrationLeaseStateV1::Held);
}

#[test]
fn takeover_resumes_or_supersedes_abandoned_lease_only_crash_prefixes() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (old_owner, _) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let first_takeover_at = ACQUIRED_AT_MS + LEASE_DURATION_MS;
  let first_request = takeover_request([0x72; 16], 1, first_takeover_at);
  replace_lease(&publisher, algorithm, &mut retirement, first_takeover_at, |lease| {
    lease.holder_boot_id = first_request.new_holder_boot_id;
    lease.fencing_token = 2;
    lease.acquired_at_ms = first_takeover_at;
    lease.state = MigrationLeaseStateV1::Held;
  });
  assert_eq!(
    old_owner.renew(renewal_request(first_takeover_at + 1_000), &mut retirement).unwrap_err().code(),
    "migration_progress_rebind_required"
  );
  assert_eq!(
    old_owner.release(release_request((first_takeover_at + 1_000) as u64), &mut retirement).unwrap_err().code(),
    "migration_progress_rebind_required"
  );
  let reacquire = MigrationAcquisitionRequestV1 {
    holder_boot_id: first_request.new_holder_boot_id,
    acquired_at_ms: first_takeover_at,
    lease_duration_ms: LEASE_DURATION_MS,
    publication_timestamp_ms: first_request.publication_timestamp_ms,
    monotonic_now_ms: first_request.monotonic_now_ms,
  };
  assert_eq!(
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), reacquire, &mut retirement).unwrap_err().code(),
    "migration_progress_rebind_required"
  );
  drop(old_owner);

  let (resumed_owner, resumed) =
    MigrationStateOwnerV1::takeover(publisher.clone(), permit.clone(), first_request, &mut retirement).unwrap();
  assert_eq!(resumed_owner.fencing_token(), 2);
  assert_eq!(resumed.lease_control_sequence, 2);
  assert_eq!(resumed.progress_control_sequence, 2);
  assert!(resumed.resumed_rebind);
  assert!(!resumed.idempotent);
  drop(resumed_owner);

  let (_second_directory, _second_path, second_publisher) = create_publisher(algorithm);
  let second_publisher = Arc::new(second_publisher);
  let (_, second_permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let mut second_retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (second_old_owner, _) = MigrationStateOwnerV1::acquire(
    second_publisher.clone(),
    second_permit.clone(),
    acquisition_request(HOLDER_BOOT_ID),
    &mut second_retirement,
  )
  .unwrap();
  drop(second_old_owner);
  replace_lease(&second_publisher, algorithm, &mut second_retirement, first_takeover_at, |lease| {
    lease.holder_boot_id = first_request.new_holder_boot_id;
    lease.fencing_token = 2;
    lease.acquired_at_ms = first_takeover_at;
    lease.state = MigrationLeaseStateV1::Held;
  });
  let second_takeover_at = first_takeover_at + LEASE_DURATION_MS;
  replace_lease(&second_publisher, algorithm, &mut second_retirement, second_takeover_at, |lease| {
    lease.holder_boot_id = [0x73; 16];
    lease.fencing_token = 3;
    lease.acquired_at_ms = second_takeover_at;
    lease.state = MigrationLeaseStateV1::Held;
  });
  let third_takeover_at = second_takeover_at + LEASE_DURATION_MS;
  let third_request = takeover_request([0x74; 16], 3, third_takeover_at);
  let (owner, receipt) =
    MigrationStateOwnerV1::takeover(second_publisher.clone(), second_permit, third_request, &mut second_retirement).unwrap();
  assert_eq!(owner.fencing_token(), 4);
  assert_eq!(receipt.fencing_token, 4);
  assert!(!receipt.resumed_rebind);
  let progress =
    second_publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(decode_migration_progress_control(&progress.bytes, algorithm).unwrap().body.fencing_token, 4);
}

#[test]
fn concurrent_expired_takeovers_select_exactly_one_new_holder() {
  for iteration in 0..16 {
    let algorithm = HashAlgorithm::Blake3_256;
    let (_directory, _path, publisher) = create_publisher(algorithm);
    let publisher = Arc::new(publisher);
    let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
    let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
    let (owner, _) =
      MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
    drop(owner);

    let barrier = Arc::new(Barrier::new(3));
    let mut contenders = Vec::new();
    for holder in [[0x72; 16], [0x73; 16]] {
      let contender_publisher = publisher.clone();
      let contender_permit = permit.clone();
      let contender_barrier = barrier.clone();
      contenders.push(thread::spawn(move || {
        let cancellation = CancellationToken::new();
        let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
        let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
        contender_barrier.wait();
        MigrationStateOwnerV1::takeover(
          contender_publisher,
          contender_permit,
          takeover_request(holder, 1, ACQUIRED_AT_MS + LEASE_DURATION_MS),
          &mut retirement,
        )
        .map(|(owner, receipt)| (owner.holder_boot_id(), receipt.fencing_token))
        .map_err(|error| error.code())
      }));
    }
    barrier.wait();
    let outcomes: Vec<_> = contenders.into_iter().map(|contender| contender.join().unwrap()).collect();
    assert_eq!(
      outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
      1,
      "iteration {iteration} produced unexpected takeover outcomes: {outcomes:?}"
    );
    assert_eq!(
      outcomes.iter().filter(|outcome| outcome.as_ref().err().copied() == Some("migration_takeover_fenced")).count(),
      1,
      "iteration {iteration} produced unexpected takeover outcomes: {outcomes:?}"
    );

    let lease = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
    let progress =
      publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
    let lease = decode_migration_lease_control(&lease.bytes, algorithm).unwrap();
    let progress = decode_migration_progress_control(&progress.bytes, algorithm).unwrap();
    assert_eq!(lease.body.fencing_token, 2);
    assert_eq!(progress.body.fencing_token, 2);
    assert_eq!(lease.body.holder_boot_id, outcomes.iter().find_map(|outcome| outcome.as_ref().ok().map(|(holder, _)| *holder)).unwrap());
  }
}

#[test]
fn takeover_rejects_invalid_active_and_inconsistent_state_before_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_directory, _path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  drop(owner);

  let active = takeover_request([0x72; 16], 1, ACQUIRED_AT_MS + 1_000);
  for (invalid, code) in [
    (MigrationTakeoverRequestV1 { new_holder_boot_id: [0; 16], ..active }, "migration_holder_boot_identity"),
    (MigrationTakeoverRequestV1 { expected_fencing_token: u64::MAX, ..active }, "migration_takeover_fencing_exhausted"),
    (MigrationTakeoverRequestV1 { takeover_at_ms: -1, ..active }, "migration_takeover_times"),
    (MigrationTakeoverRequestV1 { lease_duration_ms: 0, ..active }, "migration_takeover_times"),
    (MigrationTakeoverRequestV1 { takeover_at_ms: i64::MAX, lease_duration_ms: 1, ..active }, "migration_lease_time_overflow"),
    (MigrationTakeoverRequestV1 { publication_timestamp_ms: 0, ..active }, "migration_publication_times"),
    (MigrationTakeoverRequestV1 { monotonic_now_ms: 0, ..active }, "migration_publication_times"),
    (MigrationTakeoverRequestV1 { publication_timestamp_ms: i64::MAX as u64, ..active }, "migration_publication_time_range"),
    (MigrationTakeoverRequestV1 { monotonic_now_ms: u64::MAX, ..active }, "migration_takeover_time_overflow"),
  ] {
    assert_eq!(MigrationStateOwnerV1::takeover(publisher.clone(), permit.clone(), invalid, &mut retirement).unwrap_err().code(), code);
  }
  assert_eq!(
    MigrationStateOwnerV1::takeover(publisher.clone(), permit.clone(), active, &mut retirement).unwrap_err().code(),
    "migration_takeover_lease_active"
  );
  let zero_token = MigrationTakeoverRequestV1 { expected_fencing_token: 0, ..active };
  assert_eq!(
    MigrationStateOwnerV1::takeover(publisher.clone(), permit.clone(), zero_token, &mut retirement).unwrap_err().code(),
    "migration_takeover_fencing"
  );
  let before_transition = MigrationTakeoverRequestV1 {
    publication_timestamp_ms: (ACQUIRED_AT_MS + LEASE_DURATION_MS - 1) as u64,
    ..takeover_request([0x72; 16], 1, ACQUIRED_AT_MS + LEASE_DURATION_MS)
  };
  assert_eq!(
    MigrationStateOwnerV1::takeover(publisher.clone(), permit.clone(), before_transition, &mut retirement).unwrap_err().code(),
    "migration_publication_before_transition"
  );
  let dead_target =
    MigrationTakeoverRequestV1 { lease_duration_ms: 1, ..takeover_request([0x72; 16], 1, ACQUIRED_AT_MS + LEASE_DURATION_MS) };
  assert_eq!(
    MigrationStateOwnerV1::takeover(publisher.clone(), permit.clone(), dead_target, &mut retirement).unwrap_err().code(),
    "migration_takeover_expired_target"
  );
  let stale_token = takeover_request([0x72; 16], 2, ACQUIRED_AT_MS + LEASE_DURATION_MS);
  assert_eq!(
    MigrationStateOwnerV1::takeover(publisher.clone(), permit.clone(), stale_token, &mut retirement).unwrap_err().code(),
    "migration_takeover_fenced"
  );

  replace_progress(&publisher, algorithm, &mut retirement, |progress| {
    progress.fencing_token = 2;
  });
  let inconsistent = takeover_request([0x72; 16], 1, ACQUIRED_AT_MS + LEASE_DURATION_MS);
  assert_eq!(
    MigrationStateOwnerV1::takeover(publisher.clone(), permit, inconsistent, &mut retirement).unwrap_err().code(),
    "migration_progress_token_ahead"
  );
  replace_lease(&publisher, algorithm, &mut retirement, ACQUIRED_AT_MS + 1_000, |lease| {
    lease.state = MigrationLeaseStateV1::Releasing;
  });
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  assert_eq!(
    MigrationStateOwnerV1::takeover(publisher.clone(), permit, inconsistent, &mut retirement).unwrap_err().code(),
    "migration_takeover_lease_state"
  );
  let lease = publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(lease.control_sequence, 2);
}

#[test]
fn selected_migration_suspends_every_source_gc_mutation_but_not_diagnostics() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (source, source_directory) = create_temp_engine_for_tests();
  let source_path = source_directory.path().join("test.aeordb");
  let source_identity = platform_file_identity(&source_path).unwrap();
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) =
    admit_migration_preflight_v1(&preflight_request_with_source_identity(algorithm, DESTINATION_PHYSICAL_ID, source_identity)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();

  let (suspension, receipt) = MigrationSourceGcSuspensionOwnerV1::suspend(
    &source,
    &owner,
    MigrationSourceGcSuspensionRequestV1 {
      suspended_at_ms: ACQUIRED_AT_MS + 500,
      publication_timestamp_ms: ACQUIRED_AT_MS as u64 + 600,
      monotonic_now_ms: 10_500,
    },
    &mut retirement,
  )
  .unwrap();
  assert_eq!(receipt.fencing_token, 1);
  let selected_progress =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_ne!(
    decode_migration_progress_control(&selected_progress.bytes, algorithm).unwrap().body.flags
      & MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    0
  );

  let context = RequestContext::system();
  let directory = DirectoryOps::new(&source);
  directory.store_file_buffered(&context, "/still-writable.txt", b"normal writes continue", Some("text/plain")).unwrap();
  assert_eq!(directory.read_file_buffered("/still-writable.txt").unwrap(), b"normal writes continue");
  for invocation in [
    GcRunInvocationV1::Cli,
    GcRunInvocationV1::Http,
    GcRunInvocationV1::Task,
    GcRunInvocationV1::Scheduled,
    GcRunInvocationV1::RepairFollowUp,
    GcRunInvocationV1::Embedded,
  ] {
    let error = execute_gc_run(&source, &context, GcExecutionRequestV1::new(invocation, false, CancellationToken::new())).unwrap_err();
    assert!(matches!(error, EngineError::MigrationGcSuspended { fencing_token: 1, .. }), "unexpected {invocation:?} error: {error}");
  }

  let live = gc_mark(&source).unwrap();
  let source_bytes_before_diagnostics = fs::read(&source_path).unwrap();
  let snapshots_before_diagnostics: Vec<_> = VersionManager::new(&source)
    .list_snapshots()
    .unwrap()
    .into_iter()
    .map(|snapshot| (snapshot.name, snapshot.root_hash, snapshot.created_at, snapshot.metadata))
    .collect();
  assert!(matches!(gc_sweep(&source, &live, false).unwrap_err(), EngineError::MigrationGcSuspended { fencing_token: 1, .. }));
  assert!(matches!(prune_expired_snapshots(&source, &context).unwrap_err(), EngineError::MigrationGcSuspended { fencing_token: 1, .. }));
  let retention_hook_called = AtomicBool::new(false);
  assert!(matches!(
    prune_expired_snapshots_with_post_capture_hook(&source, &context, || {
      retention_hook_called.store(true, Ordering::Release);
    })
    .unwrap_err(),
    EngineError::MigrationGcSuspended { fencing_token: 1, .. }
  ));
  assert!(!retention_hook_called.load(Ordering::Acquire), "retention work began before source-GC admission");
  assert!(run_gc(&source, &context, true).unwrap().dry_run);
  assert!(gc_sweep(&source, &live, true).is_ok());
  assert_eq!(fs::read(&source_path).unwrap(), source_bytes_before_diagnostics, "diagnostic GC mutated source bytes");
  let snapshots_after_diagnostics: Vec<_> = VersionManager::new(&source)
    .list_snapshots()
    .unwrap()
    .into_iter()
    .map(|snapshot| (snapshot.name, snapshot.root_hash, snapshot.created_at, snapshot.metadata))
    .collect();
  assert_eq!(snapshots_after_diagnostics, snapshots_before_diagnostics, "diagnostic GC mutated snapshot state");

  owner
    .transition_progress(
      progress_transition(
        algorithm,
        MigrationPhaseV1::Preflight,
        MigrationProgressStateV1::Canceled,
        MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
        ACQUIRED_AT_MS + 1_000,
      ),
      &mut retirement,
    )
    .unwrap();
  let (foreign_release_source, _foreign_release_directory) = create_temp_engine_for_tests();
  let lease_before_wrong_source =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(
    suspension
      .release_after_early_terminal(&foreign_release_source, &owner, release_request(ACQUIRED_AT_MS as u64 + 2_000), &mut retirement,)
      .unwrap_err()
      .code(),
    "migration_source_gc_file_identity"
  );
  let lease_after_wrong_source =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationLease, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  assert_eq!(lease_after_wrong_source.bytes, lease_before_wrong_source.bytes);
  assert!(matches!(run_gc(&source, &context, false).unwrap_err(), EngineError::MigrationGcSuspended { fencing_token: 1, .. }));
  assert!(!run_gc(&foreign_release_source, &context, false).unwrap().dry_run);
  let release =
    suspension.release_after_early_terminal(&source, &owner, release_request(ACQUIRED_AT_MS as u64 + 2_000), &mut retirement).unwrap();
  assert_eq!(release.fencing_token, 1);
  let release_retry =
    suspension.release_after_early_terminal(&source, &owner, release_request(ACQUIRED_AT_MS as u64 + 2_000), &mut retirement).unwrap();
  assert!(release_retry.lease_idempotent);
  assert!(release_retry.interlock_idempotent);
  assert!(!run_gc(&source, &context, false).unwrap().dry_run);
}

#[test]
fn source_gc_suspension_drains_an_admitted_mutation_and_closes_new_admission() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (source, source_directory) = create_temp_engine_for_tests();
  let source_identity = platform_file_identity(source_directory.path().join("test.aeordb")).unwrap();
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) =
    admit_migration_preflight_v1(&preflight_request_with_source_identity(algorithm, DESTINATION_PHYSICAL_ID, source_identity)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(
    publisher,
    permit,
    acquisition_request(HOLDER_BOOT_ID),
    &mut retirement_owner(algorithm, &cancellation, &memory),
  )
  .unwrap();

  let gc_source = source.clone();
  let (gc_entered_tx, gc_entered_rx) = mpsc::channel();
  let (finish_gc_tx, finish_gc_rx) = mpsc::channel();
  let gc_thread = thread::spawn(move || {
    run_gc_with_post_start_hook(&gc_source, &RequestContext::system(), false, || {
      gc_entered_tx.send(()).unwrap();
      finish_gc_rx.recv().unwrap();
    })
  });
  gc_entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

  let suspension_source = source.clone();
  let (suspended_tx, suspended_rx) = mpsc::channel();
  let suspension_thread = thread::spawn(move || {
    let mut retirement = retirement;
    let result = MigrationSourceGcSuspensionOwnerV1::suspend(
      &suspension_source,
      &owner,
      MigrationSourceGcSuspensionRequestV1 {
        suspended_at_ms: ACQUIRED_AT_MS + 500,
        publication_timestamp_ms: ACQUIRED_AT_MS as u64 + 600,
        monotonic_now_ms: 10_500,
      },
      &mut retirement,
    );
    suspended_tx.send(result).unwrap();
  });

  assert!(suspended_rx.recv_timeout(Duration::from_millis(50)).is_err(), "suspension bypassed an admitted destructive GC run");
  assert!(matches!(
    run_gc(&source, &RequestContext::system(), false).unwrap_err(),
    EngineError::MigrationGcSuspended { fencing_token: 1, .. }
  ));
  finish_gc_tx.send(()).unwrap();
  assert!(gc_thread.join().unwrap().is_ok());
  let (_suspension, receipt) = suspended_rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
  assert_eq!(receipt.fencing_token, 1);
  suspension_thread.join().unwrap();
}

#[test]
fn source_gc_suspension_race_failure_stays_latched_but_is_fenced_and_recoverable() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (source, source_directory) = create_temp_engine_for_tests();
  let source_identity = platform_file_identity(source_directory.path().join("test.aeordb")).unwrap();
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) =
    admit_migration_preflight_v1(&preflight_request_with_source_identity(algorithm, DESTINATION_PHYSICAL_ID, source_identity)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let owner = Arc::new(
    MigrationStateOwnerV1::acquire(
      publisher,
      permit,
      acquisition_request(HOLDER_BOOT_ID),
      &mut retirement_owner(algorithm, &cancellation, &memory),
    )
    .unwrap()
    .0,
  );

  let gc_source = source.clone();
  let (gc_entered_tx, gc_entered_rx) = mpsc::channel();
  let (finish_gc_tx, finish_gc_rx) = mpsc::channel();
  let gc_thread = thread::spawn(move || {
    run_gc_with_post_start_hook(&gc_source, &RequestContext::system(), false, || {
      gc_entered_tx.send(()).unwrap();
      finish_gc_rx.recv().unwrap();
    })
  });
  gc_entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

  let suspension_source = source.clone();
  let suspension_owner = owner.clone();
  let suspension_cancellation = CancellationToken::new();
  let suspension_memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let (suspended_tx, suspended_rx) = mpsc::channel();
  let suspension_thread = thread::spawn(move || {
    let mut retirement = retirement_owner(algorithm, &suspension_cancellation, &suspension_memory);
    suspended_tx
      .send(MigrationSourceGcSuspensionOwnerV1::suspend(
        &suspension_source,
        &suspension_owner,
        MigrationSourceGcSuspensionRequestV1 {
          suspended_at_ms: ACQUIRED_AT_MS + 500,
          publication_timestamp_ms: ACQUIRED_AT_MS as u64 + 600,
          monotonic_now_ms: 10_500,
        },
        &mut retirement,
      ))
      .unwrap();
  });
  assert!(suspended_rx.recv_timeout(Duration::from_millis(50)).is_err());

  owner
    .transition_progress(
      progress_transition(algorithm, MigrationPhaseV1::Preflight, MigrationProgressStateV1::Canceled, 0, ACQUIRED_AT_MS + 1_000),
      &mut retirement_owner(algorithm, &cancellation, &memory),
    )
    .unwrap();
  finish_gc_tx.send(()).unwrap();
  assert!(gc_thread.join().unwrap().is_ok());
  assert_eq!(suspended_rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap_err().code(), "migration_source_gc_phase");
  suspension_thread.join().unwrap();

  assert!(matches!(
    run_gc(&source, &RequestContext::system(), false).unwrap_err(),
    EngineError::MigrationGcSuspended { fencing_token: 1, .. }
  ));
  let recovered = MigrationSourceGcSuspensionOwnerV1::recover_latched(&source, &owner).unwrap();
  recovered
    .release_after_early_terminal(
      &source,
      &owner,
      release_request(ACQUIRED_AT_MS as u64 + 2_000),
      &mut retirement_owner(algorithm, &cancellation, &memory),
    )
    .unwrap();
  assert!(!run_gc(&source, &RequestContext::system(), false).unwrap().dry_run);
}

#[test]
fn source_gc_suspension_retries_rebinds_on_takeover_and_fences_the_old_holder() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (source, source_directory) = create_temp_engine_for_tests();
  let source_identity = platform_file_identity(source_directory.path().join("test.aeordb")).unwrap();
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) =
    admit_migration_preflight_v1(&preflight_request_with_source_identity(algorithm, DESTINATION_PHYSICAL_ID, source_identity)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (old_owner, _) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let suspension_request = MigrationSourceGcSuspensionRequestV1 {
    suspended_at_ms: ACQUIRED_AT_MS + 500,
    publication_timestamp_ms: ACQUIRED_AT_MS as u64 + 600,
    monotonic_now_ms: 10_500,
  };
  let (old_suspension, first) =
    MigrationSourceGcSuspensionOwnerV1::suspend(&source, &old_owner, suspension_request, &mut retirement).unwrap();
  let (_, retry) = MigrationSourceGcSuspensionOwnerV1::suspend(&source, &old_owner, suspension_request, &mut retirement).unwrap();
  assert!(retry.interlock_idempotent);
  assert!(retry.progress_idempotent);
  assert_eq!(retry.progress_control_sequence, first.progress_control_sequence);

  assert!(matches!(
    run_gc(&source, &RequestContext::system(), false).unwrap_err(),
    EngineError::MigrationGcSuspended { fencing_token: 1, .. }
  ));
  let takeover_at_ms = ACQUIRED_AT_MS + LEASE_DURATION_MS;
  let new_holder = [0x72; 16];
  let (new_owner, _) =
    MigrationStateOwnerV1::takeover(publisher, permit, takeover_request(new_holder, 1, takeover_at_ms), &mut retirement).unwrap();
  let (new_suspension, rebound) = MigrationSourceGcSuspensionOwnerV1::suspend(
    &source,
    &new_owner,
    MigrationSourceGcSuspensionRequestV1 {
      suspended_at_ms: takeover_at_ms + 200,
      publication_timestamp_ms: takeover_at_ms as u64 + 300,
      monotonic_now_ms: 120_000,
    },
    &mut retirement,
  )
  .unwrap();
  assert_eq!(rebound.fencing_token, 2);
  assert!(!rebound.interlock_idempotent);
  assert!(matches!(
    run_gc(&source, &RequestContext::system(), false).unwrap_err(),
    EngineError::MigrationGcSuspended { fencing_token: 2, .. }
  ));
  assert_eq!(
    old_suspension
      .release_after_early_terminal(&source, &old_owner, release_request(takeover_at_ms as u64 + 500), &mut retirement,)
      .unwrap_err()
      .code(),
    "migration_owner_fenced"
  );

  let mut canceled = progress_transition(
    algorithm,
    MigrationPhaseV1::Preflight,
    MigrationProgressStateV1::Canceled,
    MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
    takeover_at_ms + 1_000,
  );
  canceled.monotonic_now_ms = 130_000;
  new_owner.transition_progress(canceled, &mut retirement).unwrap();
  new_suspension
    .release_after_early_terminal(
      &source,
      &new_owner,
      MigrationLeaseReleaseRequestV1 { publication_timestamp_ms: takeover_at_ms as u64 + 2_000, monotonic_now_ms: 140_000 },
      &mut retirement,
    )
    .unwrap();
  assert!(!run_gc(&source, &RequestContext::system(), false).unwrap().dry_run);
}

#[test]
fn source_gc_suspension_rejects_wrong_source_and_invalid_clocks_before_latching() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (selected_source, selected_directory) = create_temp_engine_for_tests();
  let (foreign_source, _foreign_directory) = create_temp_engine_for_tests();
  let source_identity = platform_file_identity(selected_directory.path().join("test.aeordb")).unwrap();
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let (_, permit) =
    admit_migration_preflight_v1(&preflight_request_with_source_identity(algorithm, DESTINATION_PHYSICAL_ID, source_identity)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) =
    MigrationStateOwnerV1::acquire(Arc::new(publisher), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let request = MigrationSourceGcSuspensionRequestV1 {
    suspended_at_ms: ACQUIRED_AT_MS + 500,
    publication_timestamp_ms: ACQUIRED_AT_MS as u64 + 600,
    monotonic_now_ms: 10_500,
  };
  assert_eq!(
    MigrationSourceGcSuspensionOwnerV1::recover_latched(&selected_source, &owner).unwrap_err().code(),
    "migration_source_gc_not_latched"
  );
  assert_eq!(
    MigrationSourceGcSuspensionOwnerV1::suspend(&foreign_source, &owner, request, &mut retirement).unwrap_err().code(),
    "migration_source_gc_file_identity"
  );
  assert_eq!(
    MigrationSourceGcSuspensionOwnerV1::suspend(
      &selected_source,
      &owner,
      MigrationSourceGcSuspensionRequestV1 { publication_timestamp_ms: 0, ..request },
      &mut retirement,
    )
    .unwrap_err()
    .code(),
    "migration_source_gc_times"
  );
  assert_eq!(
    MigrationSourceGcSuspensionOwnerV1::suspend(
      &selected_source,
      &owner,
      MigrationSourceGcSuspensionRequestV1 { publication_timestamp_ms: i64::MAX as u64 + 1, ..request },
      &mut retirement,
    )
    .unwrap_err()
    .code(),
    "migration_source_gc_time_range"
  );
  assert_eq!(
    MigrationSourceGcSuspensionOwnerV1::suspend(
      &selected_source,
      &owner,
      MigrationSourceGcSuspensionRequestV1 { publication_timestamp_ms: ACQUIRED_AT_MS as u64 + 400, ..request },
      &mut retirement,
    )
    .unwrap_err()
    .code(),
    "migration_source_gc_publication_before_suspension"
  );
  assert!(!run_gc(&foreign_source, &RequestContext::system(), false).unwrap().dry_run);
  assert!(!run_gc(&selected_source, &RequestContext::system(), false).unwrap().dry_run);
}

#[test]
fn migration_restart_reopens_the_source_with_gc_suspended_before_exposure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (source, source_directory) = create_temp_engine_for_tests();
  let source_path = source_directory.path().join("test.aeordb");
  let source_identity = platform_file_identity(&source_path).unwrap();
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let (_, permit) =
    admit_migration_preflight_v1(&preflight_request_with_source_identity(algorithm, DESTINATION_PHYSICAL_ID, source_identity)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, _) =
    MigrationStateOwnerV1::acquire(Arc::new(publisher), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let request = MigrationSourceGcSuspensionRequestV1 {
    suspended_at_ms: ACQUIRED_AT_MS + 500,
    publication_timestamp_ms: ACQUIRED_AT_MS as u64 + 600,
    monotonic_now_ms: 10_500,
  };
  MigrationSourceGcSuspensionOwnerV1::suspend(&source, &owner, request, &mut retirement).unwrap();
  owner
    .transition_progress(
      progress_transition(
        algorithm,
        MigrationPhaseV1::Preflight,
        MigrationProgressStateV1::Running,
        MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
        ACQUIRED_AT_MS + 1_000,
      ),
      &mut retirement,
    )
    .unwrap();
  owner
    .transition_progress(
      progress_transition(
        algorithm,
        MigrationPhaseV1::Preflight,
        MigrationProgressStateV1::Complete,
        MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
        ACQUIRED_AT_MS + 2_000,
      ),
      &mut retirement,
    )
    .unwrap();
  owner
    .transition_progress(
      progress_transition(
        algorithm,
        MigrationPhaseV1::Copy,
        MigrationProgressStateV1::Pending,
        MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
        ACQUIRED_AT_MS + 3_000,
      ),
      &mut retirement,
    )
    .unwrap();
  source.shutdown().unwrap();
  drop(source);

  let reopen_request =
    MigrationSourceGcSuspensionRequestV1 { publication_timestamp_ms: ACQUIRED_AT_MS as u64 + 3_500, monotonic_now_ms: 13_500, ..request };
  let (reopened, suspension, receipt) =
    MigrationSourceGcSuspensionOwnerV1::reopen_source_suspended(source_path.to_str().unwrap(), &owner, reopen_request, &mut retirement)
      .unwrap();
  assert!(!receipt.interlock_idempotent, "the reopened engine must install a new in-process interlock");
  assert!(receipt.progress_idempotent, "restart must retain the already durable later-phase AMPR claim");
  assert!(matches!(
    run_gc(&reopened, &RequestContext::system(), false).unwrap_err(),
    EngineError::MigrationGcSuspended { fencing_token: 1, .. }
  ));

  owner
    .transition_progress(
      progress_transition(
        algorithm,
        MigrationPhaseV1::Copy,
        MigrationProgressStateV1::Canceled,
        MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
        ACQUIRED_AT_MS + 4_000,
      ),
      &mut retirement,
    )
    .unwrap();
  suspension.release_after_early_terminal(&reopened, &owner, release_request(ACQUIRED_AT_MS as u64 + 5_000), &mut retirement).unwrap();
  reopened.shutdown().unwrap();
}

#[test]
fn source_gc_suspension_has_one_interlock_and_no_wall_clock_auto_release() {
  let gc = include_str!("../../src/engine/gc.rs");
  let lifecycle = include_str!("../../src/engine/lifecycle_config.rs");
  let interlock = include_str!("../../src/engine/v4/migration_source_gc.rs");
  assert_eq!(
    gc.matches("admit_migration_sensitive_gc_mutation()?").count(),
    2,
    "the shared executor and direct sweep must be the only destructive-GC admissions"
  );
  assert_eq!(
    lifecycle.matches("admit_migration_sensitive_gc_mutation()?").count(),
    1,
    "public retention pruning must use the same engine-owned interlock"
  );
  for doorway in ["pub fn run_gc(", "pub fn run_gc_with_cancellation(", "pub fn execute_gc_run("] {
    let start = gc.find(doorway).unwrap_or_else(|| panic!("missing GC doorway {doorway}"));
    assert!(gc[start..].contains("execute_gc_run"), "GC doorway {doorway} bypasses the shared executor");
  }
  assert!(gc.contains("prune_expired_snapshots_admitted"), "destructive GC reacquires or bypasses retention admission");
  assert!(!interlock.contains("SystemTime"));
  assert!(!interlock.contains("Utc::now"));
  assert!(!interlock.contains("Instant::now"));
  assert!(interlock.contains("release_after_early_terminal"), "suspension lacks an explicit fenced release path");
}

#[test]
fn bounded_capture_runtime_checkpoints_real_source_writes_and_releases_queue_memory_after_final_drain() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let retirement_memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &retirement_memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let owner = Arc::new(owner);
  let (source, _source_directory) = create_temp_engine_for_tests();
  let workspace_directory = tempfile::tempdir().unwrap();
  let source_memory = source.memory_coordinator();
  let baseline = source_memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().clone();

  let mut runtime = MigrationCaptureRuntimeV1::start(
    Arc::clone(&source),
    Arc::clone(&owner),
    capture_runtime_options(workspace_directory.path(), 8, 8 << 20),
    cancellation.clone(),
    capture_clock(1_000),
    &mut retirement,
  )
  .unwrap();
  assert_eq!(runtime.status().state, MigrationCaptureRuntimeStateV1::Capturing);
  assert_eq!(runtime.status().checkpoint_sequence, 1);
  assert!(runtime.status().queue_reservation_bytes > 0);
  let active = source_memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().clone();
  assert!(active.active_reservations >= baseline.active_reservations + 2, "queue and workspace memory must both be retained");

  let operations = DirectoryOps::new(&source);
  operations.store_file_buffered(&RequestContext::system(), "/capture-a.txt", b"a", Some("text/plain")).unwrap();
  operations.store_file_buffered(&RequestContext::system(), "/capture-b.txt", b"b", Some("text/plain")).unwrap();
  runtime.poll(capture_clock(1_005), &mut retirement).unwrap();
  assert_eq!(runtime.status().checkpoint_sequence, 1, "a pre-deadline drain must not publish an early checkpoint");
  assert!(runtime.status().captured_through_publication_sequence > runtime.status().starting_publication_sequence);
  runtime.poll(capture_clock(1_010), &mut retirement).unwrap();
  assert_eq!(runtime.status().checkpoint_sequence, 2);

  let selected =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let selected = decode_migration_progress_control(&selected.bytes, algorithm).unwrap();
  assert_eq!(selected.body.captured_through_publication_sequence, runtime.status().captured_through_publication_sequence);
  assert_eq!(selected.body.checkpoint_artifact, runtime.status().selected_checkpoint_artifact);

  operations.store_file_buffered(&RequestContext::system(), "/capture-final.txt", b"final", Some("text/plain")).unwrap();
  runtime.stop(capture_clock(1_015), &mut retirement).unwrap();
  assert_eq!(runtime.status().state, MigrationCaptureRuntimeStateV1::Stopped);
  assert_eq!(operations.read_file_buffered("/capture-final.txt").unwrap(), b"final");
  let stopped = source_memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().clone();
  assert_eq!(
    stopped.active_reservations,
    baseline.active_reservations + 1,
    "workspace state remains, but the queue budget must be released"
  );
  drop(runtime);
  let dropped = source_memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().clone();
  assert_eq!(dropped.active_reservations, baseline.active_reservations);
  source.shutdown().unwrap();
}

#[test]
fn capture_queue_exhaustion_never_rolls_back_source_and_durably_requires_full_reconciliation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let retirement_memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &retirement_memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let owner = Arc::new(owner);
  let (source, _source_directory) = create_temp_engine_for_tests();
  let workspace_directory = tempfile::tempdir().unwrap();
  let mut runtime = MigrationCaptureRuntimeV1::start(
    Arc::clone(&source),
    Arc::clone(&owner),
    capture_runtime_options(workspace_directory.path(), 1, 8 << 20),
    cancellation,
    capture_clock(1_000),
    &mut retirement,
  )
  .unwrap();

  let operations = DirectoryOps::new(&source);
  operations.store_file_buffered(&RequestContext::system(), "/kept-one.json", br#"{"one":1}"#, Some("application/json")).unwrap();
  operations.store_file_buffered(&RequestContext::system(), "/kept-two.json", br#"{"two":2}"#, Some("application/json")).unwrap();
  runtime.poll(capture_clock(1_005), &mut retirement).unwrap();

  assert_eq!(runtime.status().state, MigrationCaptureRuntimeStateV1::NeedsFullReconcile);
  assert!(runtime.status().durable_reconciliation_latched);
  assert_eq!(operations.read_file_buffered("/kept-one.json").unwrap(), br#"{"one":1}"#);
  assert_eq!(operations.read_file_buffered("/kept-two.json").unwrap(), br#"{"two":2}"#);
  let selected =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let selected = decode_migration_progress_control(&selected.bytes, algorithm).unwrap();
  assert_ne!(selected.body.flags & MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE, 0);
  assert!(selected.body.last_error_evidence.iter().any(|byte| *byte != 0));
  source.shutdown().unwrap();
}

#[test]
fn capture_workspace_capacity_failure_stops_only_optional_capture_and_preserves_source_success() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let retirement_memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &retirement_memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let owner = Arc::new(owner);
  let (source, _source_directory) = create_temp_engine_for_tests();
  let workspace_directory = tempfile::tempdir().unwrap();
  let mut runtime = MigrationCaptureRuntimeV1::start(
    Arc::clone(&source),
    Arc::clone(&owner),
    capture_runtime_options(workspace_directory.path(), 8, 500),
    cancellation,
    capture_clock(1_000),
    &mut retirement,
  )
  .unwrap();
  assert_eq!(runtime.status().state, MigrationCaptureRuntimeStateV1::Capturing);

  let operations = DirectoryOps::new(&source);
  operations.store_file_buffered(&RequestContext::system(), "/capacity-survives.txt", b"source wins", Some("text/plain")).unwrap();
  runtime.poll(capture_clock(1_005), &mut retirement).unwrap();

  assert_eq!(runtime.status().state, MigrationCaptureRuntimeStateV1::NeedsFullReconcile);
  assert_eq!(runtime.status().failure_code, Some("migration_capture_workspace_capacity"));
  assert!(runtime.status().durable_reconciliation_latched);
  assert_eq!(operations.read_file_buffered("/capacity-survives.txt").unwrap(), b"source wins");
  source.shutdown().unwrap();
}

#[test]
fn capture_clock_regression_stops_before_drain_and_retains_only_an_in_memory_latch_when_ampr_refuses_time_travel() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let retirement_memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &retirement_memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher, permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let (source, _source_directory) = create_temp_engine_for_tests();
  let workspace_directory = tempfile::tempdir().unwrap();
  let mut runtime = MigrationCaptureRuntimeV1::start(
    Arc::clone(&source),
    Arc::new(owner),
    capture_runtime_options(workspace_directory.path(), 8, 8 << 20),
    cancellation,
    capture_clock(1_000),
    &mut retirement,
  )
  .unwrap();

  DirectoryOps::new(&source)
    .store_file_buffered(&RequestContext::system(), "/clock-source.txt", b"still committed", Some("text/plain"))
    .unwrap();
  runtime.poll(capture_clock(999), &mut retirement).unwrap();

  assert_eq!(runtime.status().state, MigrationCaptureRuntimeStateV1::NeedsFullReconcile);
  assert_eq!(runtime.status().failure_code, Some("migration_capture_runtime_clock_regression"));
  assert!(!runtime.status().durable_reconciliation_latched);
  assert_eq!(DirectoryOps::new(&source).read_file_buffered("/clock-source.txt").unwrap(), b"still committed");
  source.shutdown().unwrap();
}

#[test]
fn restart_reopens_only_the_ampr_selected_checkpoint_and_latches_an_unselected_crash_tail() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let retirement_memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &retirement_memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher, permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let owner = Arc::new(owner);
  let (source, _source_directory) = create_temp_engine_for_tests();
  let workspace_directory = tempfile::tempdir().unwrap();
  let mut runtime = MigrationCaptureRuntimeV1::start(
    Arc::clone(&source),
    Arc::clone(&owner),
    capture_runtime_options(workspace_directory.path(), 8, 8 << 20),
    cancellation.clone(),
    capture_clock(1_000),
    &mut retirement,
  )
  .unwrap();
  DirectoryOps::new(&source).store_file_buffered(&RequestContext::system(), "/unselected-tail.txt", b"tail", Some("text/plain")).unwrap();
  runtime.poll(capture_clock(1_005), &mut retirement).unwrap();
  assert_eq!(runtime.status().checkpoint_sequence, 1);
  let recovery_request = runtime.recovery_request().unwrap();
  drop(runtime);

  let recovered = MigrationCaptureRuntimeV1::recover_selected(
    owner,
    recovery_request,
    cancellation,
    capture_clock(1_006),
    &source.memory_coordinator(),
    &mut retirement,
  )
  .unwrap();
  assert!(recovered.needs_full_reconciliation());
  assert!(recovered.durable_reconciliation_latched());
  assert_eq!(recovered.failure_code(), Some("migration_capture_recovery_unselected_tail"));
  let selected_workspace = recovered.workspace().expect("the AMPR-selected checkpoint remains replayable");
  assert_eq!(selected_workspace.segment_count(), 0, "the unselected segment must not be promoted during restart");
  assert!(selected_workspace.has_unselected_tail());
  assert_eq!(DirectoryOps::new(&source).read_file_buffered("/unselected-tail.txt").unwrap(), b"tail");
  source.shutdown().unwrap();
}

#[test]
fn restart_rejects_a_damaged_ampr_selected_checkpoint_and_durably_requires_reconciliation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let retirement_memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &retirement_memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let owner = Arc::new(owner);
  let (source, _source_directory) = create_temp_engine_for_tests();
  let operations = DirectoryOps::new(&source);
  operations.store_file_buffered(&RequestContext::system(), "/selected-source.txt", b"source survives", Some("text/plain")).unwrap();
  let workspace_directory = tempfile::tempdir().unwrap();
  let runtime = MigrationCaptureRuntimeV1::start(
    Arc::clone(&source),
    Arc::clone(&owner),
    capture_runtime_options(workspace_directory.path(), 8, 8 << 20),
    cancellation.clone(),
    capture_clock(1_000),
    &mut retirement,
  )
  .unwrap();
  let selected_sequence = runtime.status().checkpoint_sequence;
  let recovery_request = runtime.recovery_request().unwrap();
  let manifest_path = recovery_request.workspace_path().join("checkpoints").join(format!("{selected_sequence:016x}")).join("manifest.amcm");
  drop(runtime);

  let mut damaged = fs::read(&manifest_path).unwrap();
  let damaged_offset = damaged.len() / 2;
  damaged[damaged_offset] ^= 1;
  fs::write(&manifest_path, damaged).unwrap();
  let recovered = MigrationCaptureRuntimeV1::recover_selected(
    Arc::clone(&owner),
    recovery_request,
    cancellation,
    capture_clock(1_001),
    &source.memory_coordinator(),
    &mut retirement,
  )
  .unwrap();

  assert!(recovered.workspace().is_none());
  assert!(recovered.needs_full_reconciliation());
  assert!(recovered.durable_reconciliation_latched());
  assert_eq!(recovered.failure_code(), Some("migration_capture_workspace_checkpoint"));
  assert_eq!(operations.read_file_buffered("/selected-source.txt").unwrap(), b"source survives");
  let selected =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let selected = decode_migration_progress_control(&selected.bytes, algorithm).unwrap();
  assert_ne!(selected.body.flags & MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE, 0);
  source.shutdown().unwrap();
}

#[test]
fn holder_boot_change_keeps_the_selected_capture_replayable_but_makes_exact_reconciliation_mandatory() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let retirement_memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &retirement_memory);
  let (owner, _) =
    MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let (source, _source_directory) = create_temp_engine_for_tests();
  let workspace_directory = tempfile::tempdir().unwrap();
  let runtime = MigrationCaptureRuntimeV1::start(
    Arc::clone(&source),
    Arc::new(owner),
    capture_runtime_options(workspace_directory.path(), 8, 8 << 20),
    cancellation.clone(),
    capture_clock(1_000),
    &mut retirement,
  )
  .unwrap();
  let recovery_request = runtime.recovery_request().unwrap();
  drop(runtime);

  let takeover_at_ms = ACQUIRED_AT_MS + LEASE_DURATION_MS;
  let (new_owner, _) =
    MigrationStateOwnerV1::takeover(publisher, permit, takeover_request([0xa1; 16], 1, takeover_at_ms), &mut retirement).unwrap();
  let recovery_clock = MigrationCaptureRuntimeClockV1::new(
    takeover_at_ms + 1_000,
    u64::try_from(takeover_at_ms).unwrap() + 1_100,
    u64::try_from(takeover_at_ms - ACQUIRED_AT_MS).unwrap() + 60_000,
  )
  .unwrap();
  let recovered = MigrationCaptureRuntimeV1::recover_selected(
    Arc::new(new_owner),
    recovery_request,
    cancellation,
    recovery_clock,
    &source.memory_coordinator(),
    &mut retirement,
  )
  .unwrap();
  assert!(recovered.needs_full_reconciliation());
  assert!(recovered.durable_reconciliation_latched());
  assert_eq!(recovered.failure_code(), Some("migration_capture_recovery_boot_changed"));
  assert!(recovered.workspace().is_some(), "the previously selected capture remains available to replay");
  source.shutdown().unwrap();
}

#[test]
fn fenced_checkpoint_and_latch_failures_remain_an_in_memory_reconciliation_stop_without_harming_source() {
  let algorithm = HashAlgorithm::Blake3_256;
  let (_destination_directory, _destination_path, publisher) = create_publisher(algorithm);
  let publisher = Arc::new(publisher);
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let retirement_memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &retirement_memory);
  let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  let owner = Arc::new(owner);
  let (source, _source_directory) = create_temp_engine_for_tests();
  let workspace_directory = tempfile::tempdir().unwrap();
  let mut runtime = MigrationCaptureRuntimeV1::start(
    Arc::clone(&source),
    Arc::clone(&owner),
    capture_runtime_options(workspace_directory.path(), 8, 8 << 20),
    cancellation,
    capture_clock(1_000),
    &mut retirement,
  )
  .unwrap();

  replace_lease(&publisher, algorithm, &mut retirement, ACQUIRED_AT_MS + 1_001, |lease| {
    lease.holder_boot_id = [0x99; 16];
  });
  let operations = DirectoryOps::new(&source);
  operations.store_file_buffered(&RequestContext::system(), "/survives-fencing.txt", b"durable", Some("text/plain")).unwrap();
  runtime.poll(capture_clock(1_010), &mut retirement).unwrap();

  assert_eq!(runtime.status().state, MigrationCaptureRuntimeStateV1::NeedsFullReconcile);
  assert!(!runtime.status().durable_reconciliation_latched);
  assert!(runtime.status().failure_evidence.iter().any(|byte| *byte != 0));
  assert_eq!(operations.read_file_buffered("/survives-fencing.txt").unwrap(), b"durable");
  let selected =
    publisher.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let selected = decode_migration_progress_control(&selected.bytes, algorithm).unwrap();
  assert_eq!(selected.body.flags & MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE, 0);
  source.shutdown().unwrap();
}
