use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::MemoryPressure;
use aeordb::engine::native_durability::PlatformFileIdentityDescriptorV1;
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::migration_clone::{
  MigrationBaseCloneItemV1, MigrationBaseClonePlannerV1, MigrationBaseCloneSourceClosureV1, MigrationCloneDecisionV1,
};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::system_family::SystemFamilySubjectV1;
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

fn permit(algorithm: HashAlgorithm) -> MigrationPreflightPermitV1 {
  let baseline = CapabilitySetV1::v4_baseline();
  let registry = aeordb::engine::v4::system_family::embedded_system_family_registry(algorithm).unwrap();
  let source_checksum = digest(0x71);
  let capacities = std::array::from_fn(|index| {
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
  let request = MigrationPreflightRequestV1 {
    identity: MigrationIdentityEvidenceV1 {
      database_id: id(0x10),
      migration_id: id(0x20),
      source_physical_instance_id: id(0x30),
      destination_physical_instance_id: id(0x40),
      source_path_digest: digest(0x10),
      destination_path_digest: digest(0x30),
      source_file_identity: identity(0x50, 0x70),
      destination_parent_identity: capacities[0].path_identity,
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

fn closure(permit: &MigrationPreflightPermitV1) -> MigrationBaseCloneSourceClosureV1<'_> {
  MigrationBaseCloneSourceClosureV1 {
    database_id: permit.database_id(),
    source_physical_instance_id: permit.source_physical_instance_id(),
    source_header_sequence: permit.source_header_sequence(),
    source_capture_head: permit.source_capture_head(),
    source_authority_digest: permit.source_authority_digest(),
    source_authority_counts: permit.source_authority_counts(),
  }
}

#[test]
fn planner_streams_every_selected_migration_policy_without_retaining_items() {
  let permit = permit(HashAlgorithm::Blake3_256);
  let cancellation = CancellationToken::new();
  let mut planner = MigrationBaseClonePlannerV1::new(&permit, &cancellation, 6).unwrap();

  assert_eq!(
    planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/docs/readme.md"), logical_bytes: 11 }).unwrap(),
    MigrationCloneDecisionV1::CopyOrdinary,
  );
  assert_eq!(
    planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/.aeordb-system"), logical_bytes: 0 }).unwrap(),
    MigrationCloneDecisionV1::TraverseStructuralContainer,
  );
  assert_eq!(
    planner
      .classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/.aeordb-config/indexes.json"), logical_bytes: 13 })
      .unwrap(),
    MigrationCloneDecisionV1::CopyKnown { family_id: 0x0001 },
  );
  assert_eq!(
    planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::EntryType(10), logical_bytes: 17 }).unwrap(),
    MigrationCloneDecisionV1::InitializeDestination { family_id: 0x0051 },
  );
  assert_eq!(
    planner
      .classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/.aeordb-indexes/text.idx"), logical_bytes: 19 })
      .unwrap(),
    MigrationCloneDecisionV1::RebuildDestination { family_id: 0x0060 },
  );
  assert_eq!(
    planner
      .classify(MigrationBaseCloneItemV1 {
        subject: SystemFamilySubjectV1::Path("/.aeordb-system/plugins/example/plugin.json"),
        logical_bytes: 23
      })
      .unwrap(),
    MigrationCloneDecisionV1::ConvertWithOwner { family_id: 0x0030 },
  );

  let summary = planner.finish(closure(&permit)).unwrap();
  assert_eq!(summary.processed_items, 6);
  assert_eq!(summary.copy_items, 2);
  assert_eq!(summary.copy_logical_bytes, 24);
  assert_eq!(summary.structural_containers, 1);
  assert_eq!(summary.destination_local_items, 1);
  assert_eq!(summary.rebuild_items, 1);
  assert_eq!(summary.owner_conversion_items, 1);
  assert_eq!(summary.omitted_items, 0);
  assert_eq!(summary.source_authority_digest, permit.source_authority_digest());
}

#[test]
fn planner_fails_closed_on_unknowns_limits_cancellation_overflow_and_frontier_drift() {
  let permit = permit(HashAlgorithm::Blake3_256);
  let cancellation = CancellationToken::new();
  assert_eq!(MigrationBaseClonePlannerV1::new(&permit, &cancellation, 0).unwrap_err().code(), "migration_clone_limit_invalid");

  let mut planner = MigrationBaseClonePlannerV1::new(&permit, &cancellation, 1).unwrap();
  planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/docs/a"), logical_bytes: 1 }).unwrap();
  assert_eq!(
    planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/docs/b"), logical_bytes: 1 }).unwrap_err().code(),
    "migration_clone_item_limit",
  );
  assert_eq!(planner.finish(closure(&permit)).unwrap_err().code(), "migration_clone_failed");

  let mut planner = MigrationBaseClonePlannerV1::new(&permit, &cancellation, 1).unwrap();
  assert_eq!(
    planner
      .classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/docs/.aeordb-future/value"), logical_bytes: 1 })
      .unwrap_err()
      .code(),
    "unknown_protected_system_family",
  );
  assert_eq!(planner.finish(closure(&permit)).unwrap_err().code(), "migration_clone_failed");

  let canceled = CancellationToken::new();
  canceled.cancel();
  let planner = MigrationBaseClonePlannerV1::new(&permit, &canceled, 1).unwrap_err();
  assert_eq!(planner.code(), "migration_clone_canceled");

  let canceled = CancellationToken::new();
  let mut planner = MigrationBaseClonePlannerV1::new(&permit, &canceled, 1).unwrap();
  canceled.cancel();
  assert_eq!(
    planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/docs/a"), logical_bytes: 1 }).unwrap_err().code(),
    "migration_clone_canceled",
  );
  assert_eq!(planner.finish(closure(&permit)).unwrap_err().code(), "migration_clone_failed");

  let mut planner = MigrationBaseClonePlannerV1::new(&permit, &cancellation, 2).unwrap();
  planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/docs/large"), logical_bytes: u64::MAX }).unwrap();
  assert_eq!(
    planner
      .classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/docs/overflow"), logical_bytes: 1 })
      .unwrap_err()
      .code(),
    "migration_clone_arithmetic_overflow",
  );

  let mut planner = MigrationBaseClonePlannerV1::new(&permit, &cancellation, 1).unwrap();
  planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::Path("/docs/a"), logical_bytes: 1 }).unwrap();
  let mut drifted = closure(&permit);
  drifted.source_header_sequence += 1;
  assert_eq!(planner.finish(drifted).unwrap_err().code(), "migration_clone_source_basis_mismatch");
}

#[test]
fn planner_supports_both_hash_widths_and_keeps_gc_operational_state_out_of_the_clone() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let permit = permit(algorithm);
    let cancellation = CancellationToken::new();
    let mut planner = MigrationBaseClonePlannerV1::new(&permit, &cancellation, 4).unwrap();
    assert_eq!(
      planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::EntryType(10), logical_bytes: 99 }).unwrap(),
      MigrationCloneDecisionV1::InitializeDestination { family_id: 0x0051 },
    );
    assert_eq!(
      planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::ExternalWorkspaceKind(3), logical_bytes: 101 }).unwrap(),
      MigrationCloneDecisionV1::InitializeDestination { family_id: 0x0071 },
    );
    assert_eq!(
      planner
        .classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::KvKey(b"aeordb.task.v1\0job"), logical_bytes: 103 })
        .unwrap(),
      MigrationCloneDecisionV1::InitializeDestination { family_id: 0x0042 },
    );
    assert_eq!(
      planner.classify(MigrationBaseCloneItemV1 { subject: SystemFamilySubjectV1::ControlTag(2), logical_bytes: 107 }).unwrap(),
      MigrationCloneDecisionV1::InitializeDestination { family_id: 0x0042 },
    );
    let summary = planner.finish(closure(&permit)).unwrap();
    assert_eq!(summary.copy_items, 0);
    assert_eq!(summary.copy_logical_bytes, 0);
    assert_eq!(summary.destination_local_items, 4);
  }
}

#[test]
fn planner_remains_a_disconnected_constant_state_policy_consumer() {
  let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let source = std::fs::read_to_string(package.join("src/engine/v4/migration_clone.rs")).unwrap();
  for required in [
    "MigrationPolicyV1::RequiredCopy",
    "MigrationPolicyV1::DestinationLocal",
    "MigrationPolicyV1::RebuildDestination",
    "MigrationPolicyV1::OwnerConverter",
    "MigrationPolicyV1::OmitDeclared",
    "MigrationPolicyV1::FailUnknown",
  ] {
    assert!(source.contains(required), "migration planner omitted policy branch {required}");
  }
  for forbidden in [
    "StorageEngine",
    "DirectoryOps",
    "V4FirstAuthorityPublisher",
    "std::fs",
    "Vec<",
    "HashMap",
    "HashSet",
    "server::",
    "axum",
    "task_worker",
  ] {
    assert!(!source.contains(forbidden), "migration planner acquired forbidden runtime or unbounded state {forbidden}");
  }
  let module = std::fs::read_to_string(package.join("src/engine/v4/mod.rs")).unwrap();
  assert!(module.contains("pub mod migration_clone;"));
}
