use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::hot_tail::read_hot_tail_checked;
use aeordb::engine::memory_coordinator::MemoryPressure;
use aeordb::engine::native_durability::PlatformFileIdentityDescriptorV1;
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::migration_destination::{
  MigrationDestinationInitializationRequestV1, MigrationDestinationPathObservationV1, initialize_migration_destination_v1,
  observe_migration_destination_path_v1,
};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::namespace::decode_namespace_root;
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::v4::{database_header::DATABASE_HEADER_V4_DATA_OFFSET, hash::digest_parts};
use aeordb::engine::{DiskKVStore, HashAlgorithm};
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
      database_id: id(0x10),
      migration_id: id(0x20),
      source_physical_instance_id: id(0x30),
      destination_physical_instance_id: id(0x40),
      source_path_digest: digest(0x10),
      destination_path_digest: destination.path_digest(),
      source_file_identity: identity(0x50, 0x70),
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

fn reopen(path: &Path) -> aeordb::engine::v4::first_authority::V4FirstAuthorityPublisher {
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
  aeordb::engine::v4::first_authority::V4FirstAuthorityPublisher::new(kv, coordinator).unwrap()
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
