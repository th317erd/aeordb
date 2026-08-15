use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy, MemoryPressure};
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, MutableSystemControlPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::migration_control::{
  MigrationLeaseBodyV1, MigrationLeaseStateV1, MigrationPhaseV1, MigrationProgressBodyV1, MigrationProgressStateV1,
  decode_migration_lease_control, decode_migration_progress_control, encode_migration_lease_control, encode_migration_progress_control,
};
use aeordb::engine::v4::migration_owner::{MigrationAcquisitionRequestV1, MigrationStateOwnerV1};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::v4::system_control::{SystemControlKindV1, SystemControlSlotV1};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::{DiskKVStore, HashAlgorithm};
use aeordb::engine::native_durability::PlatformFileIdentityDescriptorV1;
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
      source_file_identity: identity(0x50, 0x10),
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
  ];
  let expected = [
    "migration_holder_boot_identity",
    "migration_lease_times",
    "migration_lease_times",
    "migration_lease_time_overflow",
    "migration_publication_times",
    "migration_publication_times",
    "migration_progress_time_overflow",
    "migration_progress_time_overflow",
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
        encoded_control: &lease,
        publication_timestamp_ms: 1_700_000_000_300,
        monotonic_now_ms: 10_000,
      },
      &mut retirement,
    )
    .unwrap();
  let request = MigrationAcquisitionRequestV1 { acquired_at_ms: ACQUIRED_AT_MS + LEASE_DURATION_MS, ..acquisition_request(HOLDER_BOOT_ID) };
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
  let (_, permit) = admit_migration_preflight_v1(&preflight_request(algorithm, DESTINATION_PHYSICAL_ID)).unwrap();
  let cancellation = CancellationToken::new();
  let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
  let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
  let (owner, receipt) =
    MigrationStateOwnerV1::acquire(Arc::new(publisher), permit, acquisition_request(HOLDER_BOOT_ID), &mut retirement).unwrap();
  assert_eq!(receipt.fencing_token, 1);
  drop(owner);

  let reopened = reopen(&path);
  let progress =
    reopened.load_mutable_system_control(SystemControlKindV1::MigrationProgress, &DATABASE_ID, &MIGRATION_ID).unwrap().unwrap();
  let progress = decode_migration_progress_control(&progress.bytes, algorithm).unwrap();
  assert_eq!(progress.body.effective_config_fingerprint.len(), algorithm.hash_length());
  assert_eq!(progress.body.system_family_registry_fingerprint.len(), algorithm.hash_length());
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
  assert!(!source.contains("MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED"));
  assert_ne!(SystemControlSlotV1::A, SystemControlSlotV1::B);
}

#[test]
fn acquisition_has_no_fallible_post_publication_readback_window() {
  let source = std::fs::read_to_string(format!("{}/src/engine/v4/migration_owner.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  assert_eq!(
    source.matches("load_lease(").count(),
    2,
    "lease acquisition must load once before publication and nowhere after a durable success"
  );
  assert_eq!(
    source.matches("load_progress(").count(),
    2,
    "progress acquisition must load once before publication and nowhere after a durable success"
  );
}
