use std::fs::{self, File};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use aeordb::engine::file_header::read_active_header;
use aeordb::engine::memory_coordinator::MemoryPressure;
use aeordb::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity};
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::migration_final_reconciliation::{MigrationSourceWriteFreezeRequestV1, acquire_migration_source_write_freeze_v1};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::{
  EntryType, FileRecord, HashAlgorithm, NamespaceMutationBatch, NamespaceMutationCoordinator, NamespaceMutationKind,
  NamespaceMutationSourceIdentity, StorageEngine,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const GIB: u64 = 1024 * 1024 * 1024;
const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn id(first: u8) -> [u8; 16] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn digest(first: u8) -> [u8; 32] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
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

struct Fixture {
  _directory: TempDir,
  path: std::path::PathBuf,
  engine: Arc<StorageEngine>,
  permit: MigrationPreflightPermitV1,
}

impl Fixture {
  fn new() -> Self {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.aeordb");
    let engine = Arc::new(StorageEngine::create(path.to_str().unwrap()).unwrap());
    let permit = permit_for(&path, directory.path(), None);
    Self { _directory: directory, path, engine, permit }
  }
}

fn permit_for(
  source_path: &std::path::Path,
  destination_parent: &std::path::Path,
  source_identity_override: Option<PlatformFileIdentityDescriptorV1>,
) -> MigrationPreflightPermitV1 {
  let mut file = File::open(source_path).unwrap();
  let (header, slot) = read_active_header(&mut file).unwrap();
  let source_identity = source_identity_override.unwrap_or_else(|| platform_file_identity(source_path).unwrap());
  let destination_parent_identity = platform_file_identity(destination_parent).unwrap();
  let file_size = fs::metadata(source_path).unwrap().len();
  let registry = embedded_system_family_registry(header.hash_algo).unwrap();
  let baseline = CapabilitySetV1::v4_baseline();
  let checksum = digest(0x70);
  let capacity = |role| MigrationCapacityObservationV1 {
    role,
    volume_identity: destination_parent_identity.volume_identity,
    path_identity: destination_parent_identity,
    filesystem_capacity_bytes: 64 * GIB,
    available_bytes: 48 * GIB,
    required_bytes: if role == CapacityRoleV1::Capture { GIB } else { file_size.max(1) },
    minimum_remaining_bytes: GIB,
  };
  let request = MigrationPreflightRequestV1 {
    identity: MigrationIdentityEvidenceV1 {
      database_id: id(0x10),
      migration_id: id(0x20),
      source_physical_instance_id: id(0x30),
      destination_physical_instance_id: id(0x40),
      source_path_digest: digest(0x10),
      destination_path_digest: digest(0x30),
      source_file_identity: source_identity,
      destination_parent_identity,
    },
    source: MigrationSourceEvidenceV1 {
      hash_algorithm: header.hash_algo,
      file_size,
      complete_file_checksum: checksum,
      selected_header_slot: slot as u8,
      selected_header_sequence: header.sequence,
      selected_header_digest: digest(0x80),
      head_hash: header.head_hash,
    },
    verification: StrictVerificationEvidenceV1 {
      state: StrictVerificationStateV1::CompleteClean,
      source_file_size: file_size,
      source_header_sequence: header.sequence,
      source_complete_file_checksum: checksum,
      issue_count: 0,
      evidence_digest: digest(0xa0),
    },
    recovery: MigrationRecoveryEvidenceV1 {
      inspection_complete: true,
      source_header_sequence: header.sequence,
      durability_latched: false,
      repair_active: false,
      external_spill_count: 0,
      repair_ticket_count: 0,
      path_latch_count: 0,
      evidence_digest: digest(0xb0),
    },
    inventory: SourceAuthorityInventoryV1 {
      complete: true,
      source_header_sequence: header.sequence,
      unresolved_family_count: 0,
      counts: AuthorityInventoryCountsV1 { protected_families: u64::from(registry.family_count), roots: 1, ..Default::default() },
      authority_digest: digest(0xc0),
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
    capacity: [
      capacity(CapacityRoleV1::Destination),
      capacity(CapacityRoleV1::Workspace),
      capacity(CapacityRoleV1::Backup),
      capacity(CapacityRoleV1::Capture),
    ],
    native: MigrationNativeEvidenceV1 { source: native(0xd0), destination: native(0xe0) },
    memory: MigrationMemoryEvidenceV1 {
      source_budget_bytes: GIB,
      destination_budget_bytes: GIB,
      coordinator_accounted_bytes: GIB,
      coordinator_ordinary_limit_bytes: 8 * GIB,
      host_available_bytes: 8 * GIB,
      host_available_floor_bytes: GIB,
      pressure: MemoryPressure::Normal,
      evidence_digest: digest(0xf0),
    },
    configuration: MigrationConfigurationEvidenceV1 {
      generation: 1,
      capture_max_bytes: GIB,
      capture_free_reserve_bytes: GIB,
      checkpoint_after_seconds: 30,
      effective_configuration_fingerprint: vec![0x17; header.hash_algo.hash_length()],
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

fn request<'a>(
  fixture: &'a Fixture,
  cancellation: &'a CancellationToken,
  acquisition_timeout: Duration,
) -> MigrationSourceWriteFreezeRequestV1<'a> {
  MigrationSourceWriteFreezeRequestV1 { permit: &fixture.permit, source: &fixture.engine, cancellation, acquisition_timeout }
}

#[test]
fn final_freeze_captures_exact_live_authority_without_mutating_source() {
  let fixture = Fixture::new();
  let before = fs::read(&fixture.path).unwrap();
  let physical_identity = platform_file_identity(&fixture.path).unwrap();
  let mut file = File::open(&fixture.path).unwrap();
  let (header, _) = read_active_header(&mut file).unwrap();
  let frontier = fixture.engine.durability_snapshot().unwrap().hard_frontier;
  let cancellation = CancellationToken::new();

  let freeze = acquire_migration_source_write_freeze_v1(request(&fixture, &cancellation, Duration::from_secs(2))).unwrap();
  assert_eq!(freeze.authority().physical_identity, physical_identity);
  assert_eq!(freeze.authority().header_sequence, header.sequence);
  assert_eq!(freeze.authority().namespace_root, header.head_hash);
  assert_eq!(freeze.authority().hard_publication_frontier, frontier);
  assert_eq!(freeze.authority().hash_algorithm, ALGORITHM);
  assert_eq!(
    freeze.authority().system_family_registry_fingerprint,
    embedded_system_family_registry(ALGORITHM).unwrap().operational_fingerprint
  );
  freeze.validate_unchanged().unwrap();
  assert_eq!(fs::read(&fixture.path).unwrap(), before);
}

#[test]
fn final_freeze_allows_owner_reads_but_rejects_owner_writes() {
  let fixture = Fixture::new();
  let before = fs::read(&fixture.path).unwrap();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(request(&fixture, &cancellation, Duration::from_secs(2))).unwrap();

  assert_eq!(fixture.engine.head_hash().unwrap(), freeze.authority().namespace_root);
  let key = fixture.engine.compute_hash(b"owner write must be refused").unwrap();
  let error = fixture.engine.store_entry(EntryType::Chunk, &key, b"forbidden").unwrap_err();
  assert!(error.to_string().contains("writes are prohibited"), "{error}");
  let error = fixture.engine.flush_index_buffer().unwrap_err();
  assert!(error.to_string().contains("writes are prohibited"), "{error}");
  let error = fixture.engine.recover_after_emergency_spill_replay().unwrap_err();
  assert!(error.to_string().contains("writes are prohibited"), "{error}");
  freeze.validate_unchanged().unwrap();
  assert_eq!(fs::read(&fixture.path).unwrap(), before);
}

#[test]
fn final_freeze_blocks_raw_and_namespace_writers_until_release() {
  let fixture = Fixture::new();
  let cancellation = CancellationToken::new();
  let freeze = acquire_migration_source_write_freeze_v1(request(&fixture, &cancellation, Duration::from_secs(2))).unwrap();
  let (started_tx, started_rx) = mpsc::channel();
  let (result_tx, result_rx) = mpsc::channel();

  let raw_engine = Arc::clone(&fixture.engine);
  let raw_started = started_tx.clone();
  let raw_result = result_tx.clone();
  let raw = std::thread::spawn(move || {
    let key = raw_engine.compute_hash(b"blocked raw writer").unwrap();
    raw_started.send(()).unwrap();
    raw_result.send(raw_engine.store_entry(EntryType::Chunk, &key, b"raw")).unwrap();
  });

  let namespace_engine = Arc::clone(&fixture.engine);
  let namespace = std::thread::spawn(move || {
    let locator = namespace_engine.compute_hash(b"blocked namespace locator").unwrap();
    let mut record = FileRecord::new("/blocked-system-entry".to_string(), Some("application/json".to_string()), 0, Vec::new());
    record.content_hash = namespace_engine.compute_hash(b"").unwrap();
    let value = record.serialize(namespace_engine.hash_algo().hash_length()).unwrap();
    let mut batch = NamespaceMutationBatch::new(NamespaceMutationKind::SystemWrite);
    batch
      .replace_locator_with_version(
        EntryType::FileRecord,
        locator.clone(),
        value,
        0,
        aeordb::engine::file_record::CURRENT_FILE_RECORD_VERSION,
      )
      .unwrap();
    batch
      .add_source_identity(NamespaceMutationSourceIdentity {
        path: "/blocked-system-entry".to_string(),
        entry_type: Some(EntryType::FileRecord.to_u8()),
        previous_identity: None,
        new_identity: Some(locator),
      })
      .unwrap();
    started_tx.send(()).unwrap();
    result_tx.send(NamespaceMutationCoordinator::new(&namespace_engine).execute(batch).map(|_| 0)).unwrap();
  });

  started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  assert!(result_rx.recv_timeout(Duration::from_millis(100)).is_err());
  drop(freeze);
  assert!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
  assert!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
  raw.join().unwrap();
  namespace.join().unwrap();
}

#[test]
fn invalid_requests_fail_before_source_bytes_change() {
  let fixture = Fixture::new();
  let before = fs::read(&fixture.path).unwrap();

  let canceled = CancellationToken::new();
  canceled.cancel();
  let error = acquire_migration_source_write_freeze_v1(request(&fixture, &canceled, Duration::from_secs(1))).err().unwrap();
  assert_eq!(error.code(), "migration_final_freeze_canceled");

  let active = CancellationToken::new();
  let error = acquire_migration_source_write_freeze_v1(request(&fixture, &active, Duration::ZERO)).err().unwrap();
  assert_eq!(error.code(), "migration_final_freeze_timeout");
  let error = acquire_migration_source_write_freeze_v1(request(&fixture, &active, Duration::from_secs(24 * 60 * 60 + 1))).err().unwrap();
  assert_eq!(error.code(), "migration_final_freeze_timeout");

  let mut wrong_identity = platform_file_identity(&fixture.path).unwrap();
  wrong_identity.file_identity[0] ^= 0x80;
  if wrong_identity.flags & (1 << 1) != 0 {
    wrong_identity.birth_identity[0] ^= 0x80;
  }
  let wrong_permit = permit_for(&fixture.path, fixture._directory.path(), Some(wrong_identity));
  let error = acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
    permit: &wrong_permit,
    source: &fixture.engine,
    cancellation: &active,
    acquisition_timeout: Duration::from_secs(1),
  })
  .err()
  .unwrap();
  assert_eq!(error.code(), "migration_final_freeze_source_identity");
  assert_eq!(fs::read(&fixture.path).unwrap(), before);
}

#[test]
fn blocked_freeze_acquisition_honors_cancellation_and_timeout() {
  let fixture = Fixture::new();
  let first_cancellation = CancellationToken::new();
  let first = acquire_migration_source_write_freeze_v1(request(&fixture, &first_cancellation, Duration::from_secs(2))).unwrap();

  let engine = Arc::clone(&fixture.engine);
  let permit = fixture.permit.clone();
  let cancellation = CancellationToken::new();
  let thread_cancellation = cancellation.clone();
  let canceled = std::thread::spawn(move || {
    acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
      permit: &permit,
      source: &engine,
      cancellation: &thread_cancellation,
      acquisition_timeout: Duration::from_secs(2),
    })
    .err()
    .unwrap()
    .code()
  });
  std::thread::sleep(Duration::from_millis(100));
  cancellation.cancel();
  assert_eq!(canceled.join().unwrap(), "migration_final_freeze_canceled");

  let engine = Arc::clone(&fixture.engine);
  let permit = fixture.permit.clone();
  let timed_out = std::thread::spawn(move || {
    let cancellation = CancellationToken::new();
    acquire_migration_source_write_freeze_v1(MigrationSourceWriteFreezeRequestV1 {
      permit: &permit,
      source: &engine,
      cancellation: &cancellation,
      acquisition_timeout: Duration::from_millis(100),
    })
    .err()
    .unwrap()
    .code()
  });
  assert_eq!(timed_out.join().unwrap(), "migration_final_freeze_timeout");
  first.validate_unchanged().unwrap();
}
