use std::io::Read;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::StorageEngine;
use aeordb::engine::file_header::FileHeader;
use aeordb::engine::memory_coordinator::{HostMemorySample, MemoryCoordinator, MemoryObservation, MemoryOwner, MemoryPolicy, MemoryPressure};
use aeordb::engine::native_durability::{
  NativeDurabilityCapabilities, NativeDurabilityMechanisms, NativeDurabilityProbeReport, NativeFilesystemInfo, NativeOperationSupport,
  PlatformFileIdentityDescriptorV1, platform_file_identity, probe_native_durability,
};
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::database_header::{ReadOnlyDatabaseHeader, read_database_header_read_only};
use aeordb::engine::v4::deployment_guard::{DeploymentTransitionStateV1, inspect_deployment_transition_state_read_only};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightFindingCodeV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
  evaluate_migration_preflight_v1,
};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::verify::{VerifyReport, verify_checked};

const GIB: u64 = 1024 * 1024 * 1024;
const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
type VerifyReportMutation = fn(&mut VerifyReport);

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

fn contract_digest() -> [u8; 32] {
  hex::decode(CONTRACT_REGISTRY_SHA256).unwrap().try_into().unwrap()
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

fn valid_request() -> MigrationPreflightRequestV1 {
  let baseline = CapabilitySetV1::v4_baseline();
  let registry = embedded_system_family_registry(ALGORITHM).unwrap();
  let source_checksum = digest(0x70);
  MigrationPreflightRequestV1 {
    identity: MigrationIdentityEvidenceV1 {
      database_id: id(0x10),
      migration_id: id(0x20),
      source_physical_instance_id: id(0x30),
      destination_physical_instance_id: id(0x40),
      source_path_digest: digest(0x10),
      destination_path_digest: digest(0x30),
      source_file_identity: identity(0x50, 0x10),
      destination_parent_identity: identity(0x81, 0x31),
    },
    source: MigrationSourceEvidenceV1 {
      hash_algorithm: ALGORITHM,
      file_size: 4 * GIB,
      complete_file_checksum: source_checksum,
      selected_header_slot: 1,
      selected_header_sequence: 41,
      selected_header_digest: digest(0x80),
      head_hash: digest(0x90).to_vec(),
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
      effective_configuration_fingerprint: digest(0x17).to_vec(),
    },
    binary: MigrationBinaryEvidenceV1 {
      source_commit: std::array::from_fn(|offset| 0x21 + offset as u8),
      executable_sha256: digest(0x31),
      contract_registry_sha256: contract_digest(),
      capability_profile: BinaryCapabilityProfileV1::new(baseline, baseline),
      required_reader_capabilities: baseline,
      required_writer_capabilities: baseline,
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
  }
}

fn assert_refused(mutator: impl FnOnce(&mut MigrationPreflightRequestV1), code: MigrationPreflightFindingCodeV1) {
  let mut request = valid_request();
  mutator(&mut request);
  let refusal = admit_migration_preflight_v1(&request).unwrap_err();
  assert!(refusal.report().findings().iter().any(|finding| finding.code == code), "{:?}", refusal.report().findings());
}

#[test]
fn clean_preflight_issues_one_identity_bound_nonconstructible_permit() {
  let request = valid_request();
  let (report, permit) = admit_migration_preflight_v1(&request).unwrap();
  assert!(report.findings().is_empty());
  assert_eq!(permit.database_id(), request.identity.database_id);
  assert_eq!(permit.migration_id(), request.identity.migration_id);
  assert_eq!(permit.source_physical_instance_id(), request.identity.source_physical_instance_id);
  assert_eq!(permit.destination_physical_instance_id(), request.identity.destination_physical_instance_id);
  assert_eq!(permit.source_file_identity(), request.identity.source_file_identity);
  assert_eq!(permit.destination_path_digest(), request.identity.destination_path_digest);
  assert_eq!(permit.destination_parent_identity(), request.identity.destination_parent_identity);
  assert_eq!(permit.hash_algorithm(), request.source.hash_algorithm);
  assert_eq!(permit.source_header_sequence(), 41);
  assert_eq!(permit.source_capture_head(), request.source.head_hash);
  assert_eq!(permit.configuration_generation(), 7);
  assert_eq!(permit.effective_configuration_fingerprint(), request.configuration.effective_configuration_fingerprint);
  assert_eq!(permit.source_authority_digest(), request.inventory.authority_digest);
  assert_eq!(permit.source_authority_counts(), request.inventory.counts);
  assert_eq!(permit.system_family_registry_fingerprint(), request.inventory.system_family_registry_fingerprint);
  assert_eq!(permit.capability_profile(), request.binary.capability_profile);
  assert_eq!(permit.required_reader_capabilities(), request.binary.required_reader_capabilities);
  assert_eq!(permit.required_writer_capabilities(), request.binary.required_writer_capabilities);
  assert_eq!(permit.evidence_fingerprint(), report.evidence_fingerprint());
  assert_ne!(permit.evidence_fingerprint(), [0; 32]);
}

#[test]
fn identity_source_frontier_verification_and_recovery_fail_closed() {
  assert_refused(|request| request.identity.database_id = [0; 16], MigrationPreflightFindingCodeV1::InvalidIdentity);
  assert_refused(
    |request| request.identity.destination_path_digest = request.identity.source_path_digest,
    MigrationPreflightFindingCodeV1::AmbiguousPathIdentity,
  );
  assert_refused(|request| request.verification.source_header_sequence += 1, MigrationPreflightFindingCodeV1::SourceFrontierMismatch);
  assert_refused(
    |request| request.verification.state = StrictVerificationStateV1::Incomplete,
    MigrationPreflightFindingCodeV1::StrictVerificationIncomplete,
  );
  assert_refused(
    |request| {
      request.verification.state = StrictVerificationStateV1::CompleteWithIssues;
      request.verification.issue_count = 1;
    },
    MigrationPreflightFindingCodeV1::StrictVerificationIssues,
  );
  assert_refused(|request| request.recovery.inspection_complete = false, MigrationPreflightFindingCodeV1::RecoveryInspectionIncomplete);
  assert_refused(|request| request.recovery.external_spill_count = 1, MigrationPreflightFindingCodeV1::RecoveryStateActive);
  assert_refused(|request| request.recovery.path_latch_count = 1, MigrationPreflightFindingCodeV1::RecoveryStateActive);
}

#[test]
fn inventory_native_binary_registry_and_configuration_fail_closed() {
  assert_refused(|request| request.inventory.complete = false, MigrationPreflightFindingCodeV1::InventoryIncomplete);
  assert_refused(|request| request.inventory.unresolved_family_count = 1, MigrationPreflightFindingCodeV1::ProtectedStateUnresolved);
  assert_refused(|request| request.native.destination.file_barrier = false, MigrationPreflightFindingCodeV1::NativeCapabilityUnsupported);
  assert_refused(|request| request.native.source.read_back_verified = false, MigrationPreflightFindingCodeV1::NativeCapabilityUnsupported);
  assert_refused(|request| request.binary.source_commit = [0; 20], MigrationPreflightFindingCodeV1::BinaryIdentityInvalid);
  assert_refused(
    |request| request.binary.required_writer_capabilities = CapabilitySetV1::empty(),
    MigrationPreflightFindingCodeV1::CapabilityFloorInvalid,
  );
  assert_refused(
    |request| request.binary.capability_profile = BinaryCapabilityProfileV1::new(CapabilitySetV1::empty(), CapabilitySetV1::empty()),
    MigrationPreflightFindingCodeV1::BinaryCapabilityUnsupported,
  );
  assert_refused(|request| request.binary.contract_registry_sha256[0] ^= 1, MigrationPreflightFindingCodeV1::RegistryMismatch);
  assert_refused(|request| request.inventory.system_family_registry_fingerprint[0] ^= 1, MigrationPreflightFindingCodeV1::RegistryMismatch);
  assert_refused(|request| request.configuration.checkpoint_after_seconds = 29, MigrationPreflightFindingCodeV1::ConfigurationInvalid);
  assert_refused(
    |request| {
      request.configuration.effective_configuration_fingerprint.pop();
    },
    MigrationPreflightFindingCodeV1::ConfigurationInvalid,
  );
  assert_refused(
    |request| request.capacity[3].required_bytes = request.configuration.capture_max_bytes - 1,
    MigrationPreflightFindingCodeV1::ConfigurationInvalid,
  );
}

#[test]
fn capacity_is_aggregated_by_volume_and_rejects_inconsistent_or_overflowing_evidence() {
  let mut request = valid_request();
  for observation in &mut request.capacity {
    observation.volume_identity = id(0x55);
    observation.path_identity.volume_identity = id(0x55);
    observation.available_bytes = 96 * GIB;
  }
  request.identity.destination_parent_identity = request.capacity[0].path_identity;
  assert!(admit_migration_preflight_v1(&request).is_ok());

  request.capacity[0].available_bytes = 80 * GIB;
  assert!(evaluate_migration_preflight_v1(&request)
    .findings()
    .iter()
    .any(|finding| finding.code == MigrationPreflightFindingCodeV1::CapacityVolumeInconsistent));

  let mut request = valid_request();
  for observation in &mut request.capacity {
    observation.volume_identity = id(0x56);
    observation.path_identity.volume_identity = id(0x56);
    observation.available_bytes = 88 * GIB;
  }
  request.identity.destination_parent_identity = request.capacity[0].path_identity;
  assert_refused_request(request, MigrationPreflightFindingCodeV1::CapacityInsufficient);

  let mut request = valid_request();
  request.capacity[0].required_bytes = u64::MAX;
  request.capacity[1].volume_identity = request.capacity[0].volume_identity;
  assert_refused_request(request, MigrationPreflightFindingCodeV1::CapacityOverflow);
}

#[test]
fn every_remaining_malformed_or_resource_refusal_class_is_exercised() {
  assert_refused(
    |request| {
      request.source.head_hash.pop();
    },
    MigrationPreflightFindingCodeV1::SourceEvidenceInvalid,
  );
  assert_refused(
    |request| request.native.destination.qualification_digest = [0; 32],
    MigrationPreflightFindingCodeV1::NativeQualificationIncomplete,
  );
  assert_refused(
    |request| request.capacity[0].path_identity.volume_identity = id(0x77),
    MigrationPreflightFindingCodeV1::CapacityObservationInvalid,
  );
  assert_refused(
    |request| request.capacity[0].role = CapacityRoleV1::Workspace,
    MigrationPreflightFindingCodeV1::CapacityRoleMissingOrDuplicate,
  );
  assert_refused(|request| request.memory.source_budget_bytes = 0, MigrationPreflightFindingCodeV1::MemoryObservationInvalid);
  assert_refused(|request| request.memory.pressure = MemoryPressure::Soft, MigrationPreflightFindingCodeV1::MemoryInsufficient);

  let mut request = valid_request();
  request.identity.source_file_identity.flags = 0;
  request.identity.source_file_identity.birth_identity = [0; 16];
  assert!(admit_migration_preflight_v1(&request).is_ok(), "birth evidence is optional when the native descriptor says it is absent");
}

fn assert_refused_request(request: MigrationPreflightRequestV1, code: MigrationPreflightFindingCodeV1) {
  let refusal = admit_migration_preflight_v1(&request).unwrap_err();
  assert!(refusal.report().findings().iter().any(|finding| finding.code == code), "{:?}", refusal.report().findings());
}

#[test]
fn memory_budgets_are_jointly_admitted_and_real_snapshot_mapping_is_fail_closed() {
  assert_refused(|request| request.memory.destination_budget_bytes = 11 * GIB, MigrationPreflightFindingCodeV1::MemoryInsufficient);
  assert_refused(|request| request.memory.host_available_bytes = 3 * GIB, MigrationPreflightFindingCodeV1::MemoryInsufficient);

  let coordinator = MemoryCoordinator::new(MemoryPolicy::new(8 * GIB, 12 * GIB, GIB, GIB).unwrap());
  coordinator
    .observe_legacy(MemoryOwner::KvResidentPages, MemoryObservation { resident_bytes: GIB, clean_bytes: GIB, ..Default::default() })
    .unwrap();
  coordinator.update_host_sample(HostMemorySample { rss_bytes: GIB, host_available_bytes: Some(10 * GIB), ..Default::default() }).unwrap();
  let snapshot = coordinator.snapshot().unwrap();
  let evidence = MigrationMemoryEvidenceV1::from_snapshot(&snapshot, GIB, 2 * GIB).unwrap();
  assert_eq!(evidence.coordinator_accounted_bytes, GIB);

  let unavailable = MemoryCoordinator::without_policy().snapshot().unwrap();
  assert!(MigrationMemoryEvidenceV1::from_snapshot(&unavailable, GIB, GIB).is_err());
}

#[test]
fn native_probe_mapping_preserves_unsupported_and_readback_evidence() {
  let report = NativeDurabilityProbeReport {
    filesystem: NativeFilesystemInfo { kind: "testfs".to_string(), flags: 3 },
    capabilities: NativeDurabilityCapabilities {
      data_barrier: NativeOperationSupport::Supported,
      file_barrier: NativeOperationSupport::Unsupported { reason: "injected".to_string() },
      parent_directory_sync: NativeOperationSupport::Supported,
      durable_replace: NativeOperationSupport::Supported,
      preallocation: NativeOperationSupport::Supported,
      stable_file_identity: NativeOperationSupport::Supported,
    },
    mechanisms: NativeDurabilityMechanisms { data_barrier: None, file_barrier: None, parent_directory_sync: None, durable_replace: None },
    read_back_verified: false,
    identity_before_rename: Some(identity(0x11, 0x22)),
    identity_after_rename: Some(identity(0x11, 0x22)),
    destination_identity_before_replace: Some(identity(0x12, 0x22)),
    replaced_identity: Some(identity(0x13, 0x22)),
  };
  let mapped = NativeCutoverCapabilitiesV1::from_probe_report(&report);
  assert!(mapped.data_barrier);
  assert!(!mapped.file_barrier);
  assert!(!mapped.read_back_verified);
  assert_ne!(mapped.qualification_digest, [0; 32]);
}

#[test]
fn selected_header_verifier_and_transition_inspector_map_without_restating_their_semantics() {
  let mut header = FileHeader::new(ALGORITHM);
  header.sequence = 41;
  header.head_hash = digest(0x90).to_vec();
  let source = MigrationSourceEvidenceV1::from_v3_header(&header, 1, 4 * GIB, digest(0x70), digest(0x80)).unwrap();
  assert_eq!(source.selected_header_sequence, 41);
  assert_eq!(source.selected_header_slot, 1);

  let clean = aeordb::engine::verify::VerifyReport::new("source.aeordb");
  let mut clean = clean;
  clean.file_size = 4 * GIB;
  let verification = StrictVerificationEvidenceV1::from_complete_report(&clean, 41, digest(0x70));
  assert_eq!(verification.state, StrictVerificationStateV1::CompleteClean);
  assert_eq!(verification.issue_count, 0);

  let mut different_clean_report = clean.clone();
  different_clean_report.chunks = 1;
  let different_verification = StrictVerificationEvidenceV1::from_complete_report(&different_clean_report, 41, digest(0x70));
  assert_ne!(verification.evidence_digest, different_verification.evidence_digest);

  clean.corrupt_header = 1;
  let corrupt = StrictVerificationEvidenceV1::from_complete_report(&clean, 41, digest(0x70));
  assert_eq!(corrupt.state, StrictVerificationStateV1::CompleteWithIssues);
  assert_eq!(corrupt.issue_count, 1);

  let transition = DeploymentTransitionStateV1::inactive_v3();
  let recovery = MigrationRecoveryEvidenceV1::from_deployment_state(&transition, 41, 0, 0).unwrap();
  assert!(recovery.inspection_complete);
  assert!(!recovery.durability_latched);
  assert!(!recovery.repair_active);
}

#[test]
fn canonical_head_clone_admits_only_recoverable_path_key_divergence() {
  let mut request = valid_request();
  let mut locator_only = aeordb::engine::verify::VerifyReport::new("source.aeordb");
  locator_only.file_size = request.source.file_size;
  locator_only.stale_dir_path_keys.push("/Pictures/Abstract".to_string());
  request.verification = StrictVerificationEvidenceV1::from_complete_report(
    &locator_only,
    request.source.selected_header_sequence,
    request.source.complete_file_checksum,
  );

  assert_eq!(request.verification.state, StrictVerificationStateV1::CompleteWithRecoverablePathKeyDivergence);
  assert_eq!(request.verification.issue_count, 1);
  assert!(admit_migration_preflight_v1(&request).is_ok());

  locator_only.corrupt_hash = 1;
  request.verification = StrictVerificationEvidenceV1::from_complete_report(
    &locator_only,
    request.source.selected_header_sequence,
    request.source.complete_file_checksum,
  );
  assert_eq!(request.verification.state, StrictVerificationStateV1::CompleteWithIssues);
  assert_refused(|candidate| candidate.verification = request.verification, MigrationPreflightFindingCodeV1::StrictVerificationIssues);

  assert_refused(
    |candidate| {
      candidate.verification.state = StrictVerificationStateV1::CompleteWithRecoverablePathKeyDivergence;
      candidate.verification.issue_count = 0;
    },
    MigrationPreflightFindingCodeV1::StrictVerificationIssues,
  );

  let blocking_issue_mutations: [(&str, VerifyReportMutation); 12] = [
    ("corrupt_hash", |report| report.corrupt_hash = 1),
    ("corrupt_header", |report| report.corrupt_header = 1),
    ("missing_children", |report| report.missing_children.push("/missing".to_string())),
    ("unlisted_files", |report| report.unlisted_files.push("/unlisted".to_string())),
    ("dangling_file_records", |report| report.dangling_file_records.push("/dangling".to_string())),
    ("btree_directory_issues", |report| report.btree_directory_issues.push("/btree".to_string())),
    ("stale_kv_entries", |report| report.stale_kv_entries = 1),
    ("missing_kv_entries", |report| report.missing_kv_entries = 1),
    ("invalid_kv_offsets", |report| report.invalid_kv_offsets.push("invalid offset".to_string())),
    ("invalid_hot_tail_voids", |report| report.invalid_hot_tail_voids.push("invalid void".to_string())),
    ("verification_errors", |report| report.verification_errors.push("scan failed".to_string())),
    ("broken_snapshots", |report| report.broken_snapshots.push("broken-snapshot".to_string())),
  ];
  for (name, introduce_issue) in blocking_issue_mutations {
    let mut mixed = aeordb::engine::verify::VerifyReport::new("source.aeordb");
    mixed.file_size = request.source.file_size;
    mixed.stale_dir_path_keys.push("/Pictures/Abstract".to_string());
    introduce_issue(&mut mixed);
    let evidence = StrictVerificationEvidenceV1::from_complete_report(
      &mixed,
      request.source.selected_header_sequence,
      request.source.complete_file_checksum,
    );
    assert_eq!(evidence.state, StrictVerificationStateV1::CompleteWithIssues, "{name} must remain blocking");
  }
}

#[test]
fn evidence_fingerprint_is_deterministic_and_capacity_order_independent() {
  let first = valid_request();
  let mut reordered = first.clone();
  reordered.capacity.swap(0, 3);
  reordered.capacity.swap(1, 2);
  let first = evaluate_migration_preflight_v1(&first);
  let reordered = evaluate_migration_preflight_v1(&reordered);
  assert!(first.findings().is_empty());
  assert!(reordered.findings().is_empty());
  assert_eq!(first.evidence_fingerprint(), reordered.evidence_fingerprint());

  let mut malformed = valid_request();
  malformed.capacity[1].role = CapacityRoleV1::Destination;
  malformed.capacity[1].volume_identity = malformed.capacity[0].volume_identity;
  malformed.capacity[1].path_identity.volume_identity = malformed.capacity[0].volume_identity;
  let mut malformed_reordered = malformed.clone();
  malformed_reordered.capacity.swap(0, 1);
  let malformed = evaluate_migration_preflight_v1(&malformed);
  let malformed_reordered = evaluate_migration_preflight_v1(&malformed_reordered);
  assert_eq!(malformed.findings(), malformed_reordered.findings());
  assert_eq!(malformed.evidence_fingerprint(), malformed_reordered.evidence_fingerprint());
}

#[test]
fn disposable_v3_database_closes_real_read_only_preflight_producers() {
  let temporary = tempfile::tempdir().unwrap();
  let database = temporary.path().join("source.aeordb");
  StorageEngine::create(database.to_str().unwrap()).unwrap().shutdown().unwrap();

  let mut database_file = std::fs::File::open(&database).unwrap();
  let selected = read_database_header_read_only(&mut database_file).unwrap();
  let (header, selected_slot) = match selected {
    ReadOnlyDatabaseHeader::V3 { header, selected_slot } => (header, selected_slot),
    ReadOnlyDatabaseHeader::V4(_) => panic!("new source unexpectedly used a v4 header"),
  };
  let mut database_bytes = Vec::new();
  std::fs::File::open(&database).unwrap().read_to_end(&mut database_bytes).unwrap();
  let complete_file_checksum = *blake3::hash(&database_bytes).as_bytes();
  let slot_start = selected_slot * aeordb::engine::file_header::FILE_HEADER_SIZE;
  let selected_header_digest =
    *blake3::hash(&database_bytes[slot_start..slot_start + aeordb::engine::file_header::FILE_HEADER_SIZE]).as_bytes();
  let source = MigrationSourceEvidenceV1::from_v3_header(
    &header,
    selected_slot,
    database_bytes.len().try_into().unwrap(),
    complete_file_checksum,
    selected_header_digest,
  )
  .unwrap();

  let engine = StorageEngine::open(database.to_str().unwrap()).unwrap();
  let verify_report = verify_checked(&engine, database.to_str().unwrap()).unwrap();
  assert!(!verify_report.has_issues());
  engine.shutdown().unwrap();
  let verification =
    StrictVerificationEvidenceV1::from_complete_report(&verify_report, source.selected_header_sequence, complete_file_checksum);
  let transition = inspect_deployment_transition_state_read_only(&database).unwrap();
  let recovery = MigrationRecoveryEvidenceV1::from_deployment_state(&transition, source.selected_header_sequence, 0, 0).unwrap();

  let probe = probe_native_durability(temporary.path()).unwrap();
  let native = NativeCutoverCapabilitiesV1::from_probe_report(&probe);
  assert!(
    native.data_barrier
      && native.file_barrier
      && native.parent_directory_sync
      && native.durable_replace
      && native.preallocation
      && native.stable_file_identity
      && native.read_back_verified
  );
  let source_file_identity = platform_file_identity(&database).unwrap();
  let parent_identity = platform_file_identity(temporary.path()).unwrap();
  let volume = parent_identity.volume_identity;
  let available = fs2::available_space(temporary.path()).unwrap();
  let total = fs2::total_space(temporary.path()).unwrap();
  assert!(available > 2 * GIB, "file-backed preflight requires at least 2 GiB free in TMPDIR");

  let mut request = valid_request();
  request.identity.source_path_digest = *blake3::hash(database.to_string_lossy().as_bytes()).as_bytes();
  request.identity.destination_path_digest = *blake3::hash(temporary.path().join("shadow.aeordb").to_string_lossy().as_bytes()).as_bytes();
  request.identity.source_file_identity = source_file_identity;
  request.identity.destination_parent_identity = parent_identity;
  request.source = source;
  request.verification = verification;
  request.recovery = recovery;
  request.inventory.source_header_sequence = request.source.selected_header_sequence;
  request.native = MigrationNativeEvidenceV1 { source: native, destination: native };
  request.configuration.capture_max_bytes = GIB;
  request.configuration.capture_free_reserve_bytes = GIB;
  let required = [request.source.file_size, 1024 * 1024, request.source.file_size, GIB];
  for (index, observation) in request.capacity.iter_mut().enumerate() {
    observation.volume_identity = volume;
    observation.path_identity = parent_identity;
    observation.filesystem_capacity_bytes = total;
    observation.available_bytes = available;
    observation.required_bytes = required[index];
    observation.minimum_remaining_bytes = GIB;
  }

  let (report, permit) = admit_migration_preflight_v1(&request).unwrap();
  assert!(report.findings().is_empty());
  assert_eq!(permit.source_header_sequence(), header.sequence);
}

#[test]
fn repeated_native_probes_on_one_filesystem_produce_stable_migration_evidence() {
  let temporary = tempfile::tempdir().unwrap();

  let first = NativeCutoverCapabilitiesV1::from_probe_report(&probe_native_durability(temporary.path()).unwrap());
  let second = NativeCutoverCapabilitiesV1::from_probe_report(&probe_native_durability(temporary.path()).unwrap());

  assert_eq!(first, second, "retries must reproduce the native evidence anchored by the immutable run manifest");
}

#[test]
fn preflight_contract_stays_disconnected_from_mutation_and_service_authority() {
  let source = std::fs::read_to_string(format!("{}/src/engine/v4/migration_preflight.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  for forbidden in [
    "StorageEngine",
    "DirectoryOps",
    "V4FirstAuthorityPublisher",
    "ControlStore",
    "std::fs::File",
    "OpenOptions",
    "server::",
    "axum",
    "publish",
    "write_all",
  ] {
    assert!(!source.contains(forbidden), "preflight contract acquired forbidden authority {forbidden}");
  }
  let permit_start = source.find("pub struct MigrationPreflightPermitV1").unwrap();
  let permit_body = &source[permit_start..source[permit_start..].find("}\n").map(|end| permit_start + end + 2).unwrap()];
  assert!(!permit_body.lines().skip(1).any(|line| line.trim_start().starts_with("pub ")), "permit fields became constructible");
}
