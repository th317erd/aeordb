use std::fs::OpenOptions;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::MemoryPressure;
use crate::engine::native_durability::PlatformFileIdentityDescriptorV1;
use crate::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use crate::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use crate::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use crate::engine::v4::system_family::embedded_system_family_registry;

use super::*;

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

fn permit(destination: &MigrationDestinationPathObservationV1) -> MigrationPreflightPermitV1 {
  let algorithm = HashAlgorithm::Blake3_256;
  let baseline = CapabilitySetV1::v4_baseline();
  let registry = embedded_system_family_registry(algorithm).unwrap();
  let source_checksum = digest(0x71);
  let mut capacity = std::array::from_fn(|index| {
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
  capacity[0].volume_identity = destination.parent_identity().volume_identity;
  capacity[0].path_identity = destination.parent_identity();
  admit_migration_preflight_v1(&MigrationPreflightRequestV1 {
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
      counts: AuthorityInventoryCountsV1 { protected_families: 46, roots: 1, ..AuthorityInventoryCountsV1::default() },
      authority_digest: digest(0xc0),
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
    capacity,
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
  })
  .unwrap()
  .1
}

struct FailAtPhase {
  target: MigrationDestinationInitializationPhaseV1,
}

impl MigrationDestinationInitializationObserverV1 for FailAtPhase {
  fn phase_completed(&mut self, phase: MigrationDestinationInitializationPhaseV1) -> Result<(), String> {
    if phase == self.target {
      Err(format!("injected after {phase:?}"))
    } else {
      Ok(())
    }
  }
}

struct CancelAtPhase {
  target: MigrationDestinationInitializationPhaseV1,
  cancellation: CancellationToken,
}

impl MigrationDestinationInitializationObserverV1 for CancelAtPhase {
  fn phase_completed(&mut self, phase: MigrationDestinationInitializationPhaseV1) -> Result<(), String> {
    if phase == self.target {
      self.cancellation.cancel();
    }
    Ok(())
  }
}

#[test]
fn every_post_create_crash_prefix_returns_exact_shadow_evidence_and_never_cleans_the_artifact() {
  for phase in MigrationDestinationInitializationPhaseV1::ALL {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.aeordb");
    fs::write(&source, b"untouched-source-evidence").unwrap();
    let source_before = fs::read(&source).unwrap();
    let path = directory.path().join(format!("shadow-{}.aeordb", phase as u8));
    let destination = observe_migration_destination_path_v1(&path).unwrap();
    let permit = permit(&destination);
    let cancellation = CancellationToken::new();
    let mut observer = FailAtPhase { target: phase };

    let error = initialize_migration_destination_with_observer_v1(
      MigrationDestinationInitializationRequestV1 {
        permit: &permit,
        destination: &destination,
        created_at_ms: 1_700_000_000_000,
        writer_fence_epoch: 7,
        cancellation: &cancellation,
      },
      &mut observer,
    )
    .unwrap_err();

    assert_eq!(error.code(), "migration_destination_fault_injected");
    let artifact = error.artifact().expect("post-create failure must identify the shadow");
    assert_eq!(artifact.path(), destination.path());
    assert_eq!(artifact.path_digest(), permit.destination_path_digest());
    assert_eq!(artifact.expected_database_id(), permit.database_id());
    assert_eq!(artifact.expected_physical_instance_id(), permit.destination_physical_instance_id());
    assert!(artifact.file_identity_error().is_none());
    assert!(artifact.file_identity().is_some());
    assert!(path.exists());
    assert_eq!(fs::read(&source).unwrap(), source_before);

    if phase >= MigrationDestinationInitializationPhaseV1::FirstAuthorityPublished {
      assert!(artifact.first_authority().is_some());
      let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
      let observation = super::super::header_publication::observe_database_header_v4(&file).unwrap();
      let header = observation.selected.header;
      let hot_tail =
        crate::engine::hot_tail::read_hot_tail_checked(&mut file, header.hot_tail_offset, header.hash_algorithm.hash_length()).unwrap();
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
      let publisher = V4FirstAuthorityPublisher::new(kv, coordinator).unwrap();
      assert_eq!(publisher.observe().unwrap().selected.header.physical_instance_id, permit.destination_physical_instance_id());
    } else {
      assert!(artifact.first_authority().is_none());
    }
  }
}

#[test]
fn cancellation_is_honored_at_every_safe_prepublication_boundary() {
  for phase in [
    MigrationDestinationInitializationPhaseV1::Created,
    MigrationDestinationInitializationPhaseV1::KvInitialized,
    MigrationDestinationInitializationPhaseV1::HeadersVerified,
  ] {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(format!("canceled-{}.aeordb", phase as u8));
    let destination = observe_migration_destination_path_v1(&path).unwrap();
    let permit = permit(&destination);
    let cancellation = CancellationToken::new();
    let mut observer = CancelAtPhase { target: phase, cancellation: cancellation.clone() };

    let error = initialize_migration_destination_with_observer_v1(
      MigrationDestinationInitializationRequestV1 {
        permit: &permit,
        destination: &destination,
        created_at_ms: 1_700_000_000_000,
        writer_fence_epoch: 7,
        cancellation: &cancellation,
      },
      &mut observer,
    )
    .unwrap_err();

    assert_eq!(error.code(), "migration_destination_canceled");
    let artifact = error.artifact().expect("post-create cancellation must identify the shadow");
    assert!(artifact.file_identity().is_some());
    assert!(artifact.file_identity_error().is_none());
    assert!(path.exists());
  }
}

#[cfg(unix)]
#[test]
fn destination_path_substitution_reports_the_original_open_file_identity() {
  struct ReplaceDestinationPath {
    path: PathBuf,
    moved: PathBuf,
    original_identity: Option<PlatformFileIdentityDescriptorV1>,
  }

  impl MigrationDestinationInitializationObserverV1 for ReplaceDestinationPath {
    fn phase_completed(&mut self, phase: MigrationDestinationInitializationPhaseV1) -> Result<(), String> {
      if phase == MigrationDestinationInitializationPhaseV1::Created {
        self.original_identity =
          Some(crate::engine::native_durability::platform_file_identity(&self.path).map_err(|error| error.to_string())?);
        fs::rename(&self.path, &self.moved).map_err(|error| error.to_string())?;
        fs::write(&self.path, b"replacement").map_err(|error| error.to_string())?;
      }
      Ok(())
    }
  }

  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join("shadow.aeordb");
  let moved = directory.path().join("moved-shadow.aeordb");
  let destination = observe_migration_destination_path_v1(&path).unwrap();
  let permit = permit(&destination);
  let cancellation = CancellationToken::new();
  let mut observer = ReplaceDestinationPath { path: path.clone(), moved, original_identity: None };

  let error = initialize_migration_destination_with_observer_v1(
    MigrationDestinationInitializationRequestV1 {
      permit: &permit,
      destination: &destination,
      created_at_ms: 1_700_000_000_000,
      writer_fence_epoch: 7,
      cancellation: &cancellation,
    },
    &mut observer,
  )
  .unwrap_err();

  assert_eq!(error.code(), "migration_destination_file_replaced");
  assert_eq!(error.artifact().unwrap().file_identity(), observer.original_identity);
  assert_eq!(fs::read(&path).unwrap(), b"replacement");
}
