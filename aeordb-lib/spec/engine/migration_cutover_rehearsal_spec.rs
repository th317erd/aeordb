use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::file_header::{FILE_HEADER_SIZE, read_active_header};
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy, MemoryPressure};
use aeordb::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity, sync_directory_native};
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::database_header::{
  DATABASE_HEADER_V4_DATA_OFFSET, DATABASE_HEADER_V4_SLOT_LENGTH, DatabaseHeaderV4, encode_database_header_slot,
};
use aeordb::engine::v4::first_authority::{
  FirstAuthorityPublicationRequestV1, MutableSystemControlExpectationV1, MutableSystemControlPublicationRequestV1, PreparedNamespaceTreeV0,
  V4FirstAuthorityPublisher,
};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::migration_control::{
  MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED, MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED,
  MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD, MigrationPhaseV1, MigrationProgressBodyV1, MigrationProgressStateV1,
  decode_migration_progress_control, encode_migration_progress_control,
};
use aeordb::engine::v4::migration_cutover_control::{CutoverStableFileIdentityEvidenceV1, encode_side_by_side_cutover_control_v1};
use aeordb::engine::v4::migration_cutover_journal::{
  CUTOVER_JOURNAL_FILE_NAME_V1, CutoverJournalPublicationBoundaryV1, CutoverJournalWorkspaceOptionsV1, DurableCutoverJournalWorkspaceV1,
};
use aeordb::engine::v4::migration_cutover_rehearsal::{
  SideBySideCutoverBoundaryV1, SideBySideCutoverClockV1, SideBySideCutoverEvidenceV1, SideBySideCutoverFaultInjectorV1,
  SideBySideCutoverPathsV1, SideBySideCutoverRehearsalErrorV1, SideBySideCutoverRehearsalOwnerV1,
};
use aeordb::engine::v4::migration_owner::{MigrationAcquisitionRequestV1, MigrationStateOwnerV1};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::v4::system_control::SystemControlKindV1;
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use aeordb::engine::{DiskKVStore, HashAlgorithm, StorageEngine};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const GIB: u64 = 1024 * 1024 * 1024;
const DATABASE_ID: [u8; 16] = [0x31; 16];
const MIGRATION_ID: [u8; 16] = [0x71; 16];
const SOURCE_PHYSICAL_ID: [u8; 16] = [0x41; 16];
const DESTINATION_PHYSICAL_ID: [u8; 16] = [0x51; 16];
const HOLDER_BOOT_ID: [u8; 16] = [0x61; 16];
const ACQUIRED_AT_MS: i64 = 1_700_000_000_200;
const LEASE_DURATION_MS: i64 = 10_000_000;

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

fn capacity(role: CapacityRoleV1, parent_identity: PlatformFileIdentityDescriptorV1) -> MigrationCapacityObservationV1 {
  let (required_bytes, minimum_remaining_bytes) = match role {
    CapacityRoleV1::Destination => (8 * GIB, 16 * GIB),
    CapacityRoleV1::Workspace | CapacityRoleV1::Backup => (4 * GIB, 16 * GIB),
    CapacityRoleV1::Capture => (64 * GIB, 16 * GIB),
  };
  MigrationCapacityObservationV1 {
    role,
    volume_identity: parent_identity.volume_identity,
    path_identity: parent_identity,
    filesystem_capacity_bytes: 256 * GIB,
    available_bytes: 192 * GIB,
    required_bytes,
    minimum_remaining_bytes,
  }
}

fn initial_header(algorithm: HashAlgorithm) -> DatabaseHeaderV4 {
  let kv_block_length = initial_block_size();
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

fn create_destination(path: &Path, algorithm: HashAlgorithm) -> Arc<V4FirstAuthorityPublisher> {
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(path).unwrap();
  let header = initial_header(algorithm);
  let slot = encode_database_header_slot(&header).unwrap();
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
  let publisher = Arc::new(V4FirstAuthorityPublisher::new(kv, coordinator).unwrap());
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
      typed_closure_digest: digest_parts(algorithm, &[b"typed cutover rehearsal closure"]),
      authority_identity: b"HEAD".to_vec(),
    })
    .unwrap();
  publisher
}

fn acquisition_request() -> MigrationAcquisitionRequestV1 {
  MigrationAcquisitionRequestV1 {
    holder_boot_id: HOLDER_BOOT_ID,
    acquired_at_ms: ACQUIRED_AT_MS,
    lease_duration_ms: LEASE_DURATION_MS,
    publication_timestamp_ms: 1_700_000_000_300,
    monotonic_now_ms: 10_000,
  }
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

fn complete_file_blake3(path: &Path) -> [u8; 32] {
  let mut file = File::open(path).unwrap();
  let mut buffer = [0u8; 64 * 1024];
  let mut hasher = blake3::Hasher::new();
  loop {
    let count = file.read(&mut buffer).unwrap();
    if count == 0 {
      break;
    }
    hasher.update(&buffer[..count]);
  }
  *hasher.finalize().as_bytes()
}

fn source_evidence(path: &Path) -> (HashAlgorithm, CutoverStableFileIdentityEvidenceV1, [u8; 32], Vec<u8>, usize) {
  let mut file = File::open(path).unwrap();
  let (header, selected_slot) = read_active_header(&mut file).unwrap();
  file.seek(SeekFrom::Start((selected_slot * FILE_HEADER_SIZE) as u64)).unwrap();
  let mut selected_header = [0u8; FILE_HEADER_SIZE];
  file.read_exact(&mut selected_header).unwrap();
  let evidence = CutoverStableFileIdentityEvidenceV1 {
    role: aeordb::engine::v4::migration_cutover_control::CutoverArtifactRoleV1::Source,
    database_id: DATABASE_ID,
    physical_instance_id: SOURCE_PHYSICAL_ID,
    platform_file_identity: platform_file_identity(path).unwrap(),
    format: 3,
    selected_header_sequence: header.sequence,
    selected_header_blake3: *blake3::hash(&selected_header).as_bytes(),
    file_size: file.metadata().unwrap().len(),
  };
  (header.hash_algo, evidence, complete_file_blake3(path), header.head_hash, selected_slot)
}

fn preflight_request(
  algorithm: HashAlgorithm,
  source: &CutoverStableFileIdentityEvidenceV1,
  source_checksum: [u8; 32],
  source_head: Vec<u8>,
  selected_slot: usize,
  destination_parent_identity: PlatformFileIdentityDescriptorV1,
) -> MigrationPreflightRequestV1 {
  let baseline = CapabilitySetV1::v4_baseline();
  let registry = embedded_system_family_registry(algorithm).unwrap();
  MigrationPreflightRequestV1 {
    identity: MigrationIdentityEvidenceV1 {
      database_id: DATABASE_ID,
      migration_id: MIGRATION_ID,
      source_physical_instance_id: SOURCE_PHYSICAL_ID,
      destination_physical_instance_id: DESTINATION_PHYSICAL_ID,
      source_path_digest: digest(0x10),
      destination_path_digest: digest(0x30),
      source_file_identity: source.platform_file_identity,
      destination_parent_identity,
    },
    source: MigrationSourceEvidenceV1 {
      hash_algorithm: algorithm,
      file_size: source.file_size,
      complete_file_checksum: source_checksum,
      selected_header_slot: u8::try_from(selected_slot).unwrap(),
      selected_header_sequence: source.selected_header_sequence,
      selected_header_digest: source.selected_header_blake3,
      head_hash: source_head,
    },
    verification: StrictVerificationEvidenceV1 {
      state: StrictVerificationStateV1::CompleteClean,
      source_file_size: source.file_size,
      source_header_sequence: source.selected_header_sequence,
      source_complete_file_checksum: source_checksum,
      issue_count: 0,
      evidence_digest: digest(0xa0),
    },
    recovery: MigrationRecoveryEvidenceV1 {
      inspection_complete: true,
      source_header_sequence: source.selected_header_sequence,
      durability_latched: false,
      repair_active: false,
      external_spill_count: 0,
      repair_ticket_count: 0,
      path_latch_count: 0,
      evidence_digest: digest(0xb0),
    },
    inventory: SourceAuthorityInventoryV1 {
      complete: true,
      source_header_sequence: source.selected_header_sequence,
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
      capacity(CapacityRoleV1::Destination, destination_parent_identity),
      capacity(CapacityRoleV1::Workspace, destination_parent_identity),
      capacity(CapacityRoleV1::Backup, destination_parent_identity),
      capacity(CapacityRoleV1::Capture, destination_parent_identity),
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
        publication_timestamp_ms: progress.body.updated_at_ms as u64 + 100,
        monotonic_now_ms: 30_000,
      },
      retirement,
    )
    .unwrap();
}

fn destination_evidence(path: &Path, publisher: &V4FirstAuthorityPublisher) -> CutoverStableFileIdentityEvidenceV1 {
  let observation = publisher.observe().unwrap();
  let selected_start = observation.selected.selected_slot * DATABASE_HEADER_V4_SLOT_LENGTH;
  CutoverStableFileIdentityEvidenceV1 {
    role: aeordb::engine::v4::migration_cutover_control::CutoverArtifactRoleV1::Destination,
    database_id: DATABASE_ID,
    physical_instance_id: DESTINATION_PHYSICAL_ID,
    platform_file_identity: platform_file_identity(path).unwrap(),
    format: 4,
    selected_header_sequence: observation.selected.header.slot_sequence,
    selected_header_blake3: *blake3::hash(&observation.region[selected_start..selected_start + DATABASE_HEADER_V4_SLOT_LENGTH]).as_bytes(),
    file_size: path.metadata().unwrap().len(),
  }
}

fn clock(offset: u64) -> SideBySideCutoverClockV1 {
  SideBySideCutoverClockV1 {
    updated_at_ms: ACQUIRED_AT_MS + 1_000 + i64::try_from(offset).unwrap(),
    publication_timestamp_ms: u64::try_from(ACQUIRED_AT_MS).unwrap() + 2_000 + offset,
    monotonic_now_ms: 40_000 + offset,
  }
}

struct CutoverFixture {
  _directory: TempDir,
  algorithm: HashAlgorithm,
  paths: SideBySideCutoverPathsV1,
  evidence: SideBySideCutoverEvidenceV1,
  permit: MigrationPreflightPermitV1,
  initial_owner: Option<MigrationStateOwnerV1>,
  cancellation: CancellationToken,
  memory: MemoryCoordinator,
  retirement: RetirementJournalOwnerV1,
}

impl CutoverFixture {
  fn new() -> Self {
    let directory = tempfile::tempdir().unwrap();
    let service_path = directory.path().join("service.aeordb");
    let destination_path = directory.path().join("destination.v4.aeordb");
    let journal_path = directory.path().join("cutover-journal");
    let source_engine = StorageEngine::create(service_path.to_str().unwrap()).unwrap();
    source_engine.shutdown().unwrap();
    let (algorithm, source_file, source_checksum, source_head, selected_slot) = source_evidence(&service_path);
    let parent_identity = platform_file_identity(directory.path()).unwrap();
    let request = preflight_request(algorithm, &source_file, source_checksum, source_head, selected_slot, parent_identity);
    let (_, permit) = admit_migration_preflight_v1(&request).unwrap();
    let publisher = create_destination(&destination_path, algorithm);
    let cancellation = CancellationToken::new();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(32 << 20, 64 << 20, 1, 8 << 20).unwrap());
    let mut retirement = retirement_owner(algorithm, &cancellation, &memory);
    let (owner, _) = MigrationStateOwnerV1::acquire(publisher.clone(), permit.clone(), acquisition_request(), &mut retirement).unwrap();
    let verified_destination_sequence = publisher.observe().unwrap().selected.header.slot_sequence;
    replace_progress(&publisher, algorithm, &mut retirement, |progress| {
      progress.phase = MigrationPhaseV1::DestinationVerify;
      progress.state = MigrationProgressStateV1::Complete;
      progress.flags = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED
        | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD
        | MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED;
      progress.destination_header_sequence = verified_destination_sequence;
      progress.legacy_root_map_control_payload_hash = vec![0x93; algorithm.hash_length()];
      progress.updated_at_ms = ACQUIRED_AT_MS + 900;
    });
    let destination_file = destination_evidence(&destination_path, &publisher);
    drop(publisher);
    let paths = SideBySideCutoverPathsV1::new(&service_path, &destination_path, &journal_path, MIGRATION_ID).unwrap();
    let evidence = SideBySideCutoverEvidenceV1 {
      source_path_digest: digest(0x10),
      destination_path_digest: digest(0x30),
      source_file,
      source_complete_file_checksum: source_checksum,
      destination_file,
      destination_full_verification_evidence: vec![0x93; algorithm.hash_length()],
    };
    Self { _directory: directory, algorithm, paths, evidence, permit, initial_owner: Some(owner), cancellation, memory, retirement }
  }

  fn destination_authority_path(&self) -> &Path {
    if self.paths.destination_path().exists()
      && platform_file_identity(self.paths.destination_path())
        .is_ok_and(|identity| self.evidence.destination_file.platform_file_identity.represents_same_physical_file_as(identity))
    {
      self.paths.destination_path()
    } else {
      self.paths.service_path()
    }
  }

  fn acquire_owner(&mut self) -> MigrationStateOwnerV1 {
    let authority_path = self.destination_authority_path().to_path_buf();
    let publisher = Arc::new(V4FirstAuthorityPublisher::open(authority_path).unwrap());
    MigrationStateOwnerV1::acquire(publisher, self.permit.clone(), acquisition_request(), &mut self.retirement).unwrap().0
  }

  fn prepare(&mut self) -> SideBySideCutoverRehearsalOwnerV1 {
    self.try_prepare(self.evidence.clone(), self.cancellation.clone()).unwrap()
  }

  fn try_prepare(
    &mut self,
    evidence: SideBySideCutoverEvidenceV1,
    cancellation: CancellationToken,
  ) -> Result<SideBySideCutoverRehearsalOwnerV1, SideBySideCutoverRehearsalErrorV1> {
    let owner = self.initial_owner.take().unwrap_or_else(|| self.acquire_owner());
    SideBySideCutoverRehearsalOwnerV1::prepare(
      owner,
      self.paths.clone(),
      evidence,
      clock(0),
      CutoverJournalWorkspaceOptionsV1::new(0),
      cancellation,
      &self.memory,
      &mut self.retirement,
    )
    .map(|prepared| prepared.0)
  }

  fn prepare_with_fault(&mut self, fault: &mut ExactFault) -> Result<SideBySideCutoverRehearsalOwnerV1, SideBySideCutoverRehearsalErrorV1> {
    let owner = self.initial_owner.take().unwrap_or_else(|| self.acquire_owner());
    SideBySideCutoverRehearsalOwnerV1::prepare_with_fault_injector(
      owner,
      self.paths.clone(),
      self.evidence.clone(),
      clock(0),
      CutoverJournalWorkspaceOptionsV1::new(0),
      self.cancellation.clone(),
      &self.memory,
      &mut self.retirement,
      fault,
    )
    .map(|prepared| prepared.0)
  }

  fn recover(&mut self, offset: u64) -> SideBySideCutoverRehearsalOwnerV1 {
    self.try_recover(offset).unwrap()
  }

  fn try_recover(&mut self, offset: u64) -> Result<SideBySideCutoverRehearsalOwnerV1, SideBySideCutoverRehearsalErrorV1> {
    let owner = self.acquire_owner();
    SideBySideCutoverRehearsalOwnerV1::recover_pre_acceptance(
      owner,
      self.paths.clone(),
      self.evidence.clone(),
      clock(offset),
      CutoverJournalWorkspaceOptionsV1::new(0),
      self.cancellation.clone(),
      &self.memory,
      &mut self.retirement,
    )
  }

  fn assert_destination_installed(&self) {
    assert!(!self.paths.destination_path().exists());
    assert!(self
      .evidence
      .destination_file
      .platform_file_identity
      .represents_same_physical_file_as(platform_file_identity(self.paths.service_path()).unwrap()));
    assert_eq!(complete_file_blake3(self.paths.backup_path()), self.evidence.source_complete_file_checksum);
  }

  fn assert_source_preserved(&self) {
    let expected = self.evidence.source_complete_file_checksum;
    let preserved = [self.paths.service_path(), self.paths.backup_path()]
      .into_iter()
      .filter(|path| path.is_file())
      .any(|path| complete_file_blake3(path) == expected);
    assert!(preserved, "an exact frozen v3 source must remain at service or backup path");
  }
}

#[derive(Clone, Copy)]
struct ExactFault {
  target: SideBySideCutoverBoundaryV1,
  fired: bool,
}

impl SideBySideCutoverFaultInjectorV1 for ExactFault {
  fn inject(&mut self, boundary: SideBySideCutoverBoundaryV1) -> bool {
    if !self.fired && boundary == self.target {
      self.fired = true;
      return true;
    }
    false
  }
}

#[derive(Clone, Copy, Debug)]
enum RecoverableNamespacePrefix {
  Pristine,
  SourceLinked,
  SourceBackedUp,
  DestinationLinked,
}

fn sync_fixture_parent(fixture: &CutoverFixture) {
  sync_directory_native(fixture.paths.service_path().parent().unwrap()).unwrap();
}

fn seed_recoverable_namespace_prefix(fixture: &CutoverFixture, prefix: RecoverableNamespacePrefix) {
  if matches!(prefix, RecoverableNamespacePrefix::Pristine) {
    return;
  }
  fs::hard_link(fixture.paths.service_path(), fixture.paths.backup_path()).unwrap();
  sync_fixture_parent(fixture);
  if matches!(prefix, RecoverableNamespacePrefix::SourceLinked) {
    return;
  }
  fs::remove_file(fixture.paths.service_path()).unwrap();
  sync_fixture_parent(fixture);
  if matches!(prefix, RecoverableNamespacePrefix::SourceBackedUp) {
    return;
  }
  fs::hard_link(fixture.paths.destination_path(), fixture.paths.service_path()).unwrap();
  sync_fixture_parent(fixture);
}

#[test]
fn real_files_cut_over_read_only_and_roll_back_to_the_exact_frozen_v3_source() {
  let mut fixture = CutoverFixture::new();
  let source_checksum = fixture.evidence.source_complete_file_checksum;
  let destination_identity = fixture.evidence.destination_file.platform_file_identity;
  let mut rehearsal = fixture.prepare();
  assert_eq!(complete_file_blake3(fixture.paths.service_path()), source_checksum);

  let receipt = rehearsal.execute(clock(100), &mut fixture.retirement).unwrap();
  assert_eq!(receipt.phase, MigrationPhaseV1::ReadOnlyValidation);
  assert_eq!(complete_file_blake3(fixture.paths.backup_path()), source_checksum);
  assert!(!fixture.paths.destination_path().exists());
  assert!(destination_identity.represents_same_physical_file_as(platform_file_identity(fixture.paths.service_path()).unwrap()));

  let rollback_evidence = vec![0xa5; fixture.algorithm.hash_length()];
  let rollback = rehearsal.rollback_pre_acceptance(rollback_evidence.clone(), clock(200), &mut fixture.retirement).unwrap();
  assert_eq!(rollback.rollback_evidence, rollback_evidence);
  assert_eq!(complete_file_blake3(fixture.paths.service_path()), source_checksum);
  assert!(!fixture.paths.backup_path().exists());
  assert!(destination_identity.represents_same_physical_file_as(platform_file_identity(fixture.paths.destination_path()).unwrap()));

  let retry = rehearsal.rollback_pre_acceptance(rollback_evidence, clock(300), &mut fixture.retirement).unwrap();
  assert_eq!(retry.journal_sequence, rollback.journal_sequence);
  fixture.assert_source_preserved();
}

#[test]
fn every_initial_journal_and_database_control_prefix_recovers_forward_without_source_loss() {
  let boundaries = [
    SideBySideCutoverBoundaryV1::AfterInitialJournal,
    SideBySideCutoverBoundaryV1::BeforeDatabaseControl { phase: MigrationPhaseV1::DestinationVerify },
    SideBySideCutoverBoundaryV1::AfterDatabaseControl { phase: MigrationPhaseV1::DestinationVerify },
  ];
  for (index, boundary) in boundaries.into_iter().enumerate() {
    let mut fixture = CutoverFixture::new();
    let mut fault = ExactFault { target: boundary, fired: false };
    let error = fixture.prepare_with_fault(&mut fault).unwrap_err();
    assert!(fault.fired, "fault boundary was not reached: {boundary:?}; error={error}");
    fixture.assert_source_preserved();

    let mut recovered = fixture.recover(1_000 + index as u64 * 100);
    let receipt = recovered.execute(clock(2_000 + index as u64 * 100), &mut fixture.retirement).unwrap();
    assert_eq!(receipt.phase, MigrationPhaseV1::ReadOnlyValidation);
    fixture.assert_destination_installed();
  }
}

#[test]
fn every_cutover_journal_sync_install_and_reopen_prefix_recovers_forward_without_source_loss() {
  let mut boundaries = Vec::new();
  for phase in [MigrationPhaseV1::Cutover, MigrationPhaseV1::ReadOnlyValidation] {
    for boundary in [
      CutoverJournalPublicationBoundaryV1::BeforeSlotWrite,
      CutoverJournalPublicationBoundaryV1::AfterSlotWrite,
      CutoverJournalPublicationBoundaryV1::AfterFileSync,
      CutoverJournalPublicationBoundaryV1::AfterReadBack,
    ] {
      boundaries.push(SideBySideCutoverBoundaryV1::Journal { phase, boundary });
    }
    boundaries.push(SideBySideCutoverBoundaryV1::BeforeDatabaseControl { phase });
    boundaries.push(SideBySideCutoverBoundaryV1::AfterDatabaseControl { phase });
  }
  boundaries.extend([
    SideBySideCutoverBoundaryV1::BeforeSourceFileSync,
    SideBySideCutoverBoundaryV1::AfterSourceFileSync,
    SideBySideCutoverBoundaryV1::BeforeDestinationFileSync,
    SideBySideCutoverBoundaryV1::AfterDestinationFileSync,
    SideBySideCutoverBoundaryV1::BeforeParentDirectorySync,
    SideBySideCutoverBoundaryV1::AfterParentDirectorySync,
    SideBySideCutoverBoundaryV1::BeforeSourceBackupInstall,
    SideBySideCutoverBoundaryV1::AfterSourceBackupInstall,
    SideBySideCutoverBoundaryV1::BeforeDestinationServiceInstall,
    SideBySideCutoverBoundaryV1::AfterDestinationServiceInstall,
    SideBySideCutoverBoundaryV1::BeforeReopen,
    SideBySideCutoverBoundaryV1::AfterReopen,
  ]);

  for (index, boundary) in boundaries.into_iter().enumerate() {
    let mut fixture = CutoverFixture::new();
    let mut rehearsal = fixture.prepare();
    let mut fault = ExactFault { target: boundary, fired: false };
    let error = rehearsal.execute_with_fault_injector(clock(100), &mut fixture.retirement, &mut fault).unwrap_err();
    assert!(fault.fired, "fault boundary was not reached: {boundary:?}; error={error}");
    fixture.assert_source_preserved();
    drop(rehearsal);

    let recovery_offset = 10_000 + index as u64 * 100;
    let mut recovered = fixture.recover(recovery_offset);
    let receipt = recovered.execute(clock(recovery_offset + 50), &mut fixture.retirement).unwrap();
    assert_eq!(receipt.phase, MigrationPhaseV1::ReadOnlyValidation);
    fixture.assert_destination_installed();
  }
}

#[test]
fn hard_link_install_prefixes_recover_forward_to_the_exact_v4_service() {
  for (index, prefix) in [RecoverableNamespacePrefix::SourceLinked, RecoverableNamespacePrefix::DestinationLinked].into_iter().enumerate() {
    let mut fixture = CutoverFixture::new();
    let rehearsal = fixture.prepare();
    seed_recoverable_namespace_prefix(&fixture, prefix);
    fixture.assert_source_preserved();
    drop(rehearsal);

    let recovery_offset = 15_000 + index as u64 * 100;
    let mut recovered = fixture.recover(recovery_offset);
    let receipt = recovered.execute(clock(recovery_offset + 50), &mut fixture.retirement).unwrap();
    assert_eq!(receipt.phase, MigrationPhaseV1::ReadOnlyValidation, "prefix={prefix:?}");
    fixture.assert_destination_installed();
  }
}

#[test]
fn every_pre_acceptance_rollback_prefix_recovers_and_restores_v3_service() {
  let boundaries = [
    SideBySideCutoverBoundaryV1::Journal {
      phase: MigrationPhaseV1::ReadOnlyValidation,
      boundary: CutoverJournalPublicationBoundaryV1::BeforeSlotWrite,
    },
    SideBySideCutoverBoundaryV1::Journal {
      phase: MigrationPhaseV1::ReadOnlyValidation,
      boundary: CutoverJournalPublicationBoundaryV1::AfterSlotWrite,
    },
    SideBySideCutoverBoundaryV1::Journal {
      phase: MigrationPhaseV1::ReadOnlyValidation,
      boundary: CutoverJournalPublicationBoundaryV1::AfterFileSync,
    },
    SideBySideCutoverBoundaryV1::Journal {
      phase: MigrationPhaseV1::ReadOnlyValidation,
      boundary: CutoverJournalPublicationBoundaryV1::AfterReadBack,
    },
    SideBySideCutoverBoundaryV1::BeforeDatabaseControl { phase: MigrationPhaseV1::ReadOnlyValidation },
    SideBySideCutoverBoundaryV1::AfterDatabaseControl { phase: MigrationPhaseV1::ReadOnlyValidation },
    SideBySideCutoverBoundaryV1::BeforeRollbackDestinationRestore,
    SideBySideCutoverBoundaryV1::AfterRollbackDestinationRestore,
    SideBySideCutoverBoundaryV1::BeforeRollbackSourceRestore,
    SideBySideCutoverBoundaryV1::AfterRollbackSourceRestore,
  ];
  for (index, boundary) in boundaries.into_iter().enumerate() {
    let mut fixture = CutoverFixture::new();
    let mut rehearsal = fixture.prepare();
    rehearsal.execute(clock(100), &mut fixture.retirement).unwrap();
    let rollback_evidence = vec![0xb0 + index as u8; fixture.algorithm.hash_length()];
    let mut fault = ExactFault { target: boundary, fired: false };
    let error = rehearsal
      .rollback_pre_acceptance_with_fault_injector(rollback_evidence.clone(), clock(200), &mut fixture.retirement, &mut fault)
      .unwrap_err();
    assert!(fault.fired, "rollback fault boundary was not reached: {boundary:?}; error={error}");
    fixture.assert_source_preserved();
    drop(rehearsal);

    let mut recovered = fixture.recover(20_000 + index as u64 * 100);
    recovered.rollback_pre_acceptance(rollback_evidence, clock(21_000 + index as u64 * 100), &mut fixture.retirement).unwrap();
    assert_eq!(complete_file_blake3(fixture.paths.service_path()), fixture.evidence.source_complete_file_checksum);
    assert!(!fixture.paths.backup_path().exists());
    assert!(fixture
      .evidence
      .destination_file
      .platform_file_identity
      .represents_same_physical_file_as(platform_file_identity(fixture.paths.destination_path()).unwrap()));
  }
}

#[test]
fn every_pre_install_hard_link_prefix_recovers_backward_to_the_exact_v3_service() {
  for (index, prefix) in [
    RecoverableNamespacePrefix::Pristine,
    RecoverableNamespacePrefix::SourceLinked,
    RecoverableNamespacePrefix::SourceBackedUp,
    RecoverableNamespacePrefix::DestinationLinked,
  ]
  .into_iter()
  .enumerate()
  {
    let mut fixture = CutoverFixture::new();
    let rehearsal = fixture.prepare();
    seed_recoverable_namespace_prefix(&fixture, prefix);
    fixture.assert_source_preserved();
    drop(rehearsal);

    let recovery_offset = 25_000 + index as u64 * 100;
    let mut recovered = fixture.recover(recovery_offset);
    let rollback_evidence = vec![0xc0 + index as u8; fixture.algorithm.hash_length()];
    recovered.rollback_pre_acceptance(rollback_evidence, clock(recovery_offset + 50), &mut fixture.retirement).unwrap();
    assert_eq!(complete_file_blake3(fixture.paths.service_path()), fixture.evidence.source_complete_file_checksum, "prefix={prefix:?}");
    assert!(!fixture.paths.backup_path().exists(), "prefix={prefix:?}");
    assert!(
      fixture
        .evidence
        .destination_file
        .platform_file_identity
        .represents_same_physical_file_as(platform_file_identity(fixture.paths.destination_path()).unwrap()),
      "prefix={prefix:?}"
    );
  }
}

#[test]
fn invalid_stale_canceled_and_colliding_inputs_refuse_before_namespace_mutation() {
  let mut fixture = CutoverFixture::new();
  let mut wrong_path = fixture.evidence.clone();
  wrong_path.source_path_digest = digest(0x11);
  let error = fixture.try_prepare(wrong_path, fixture.cancellation.clone()).unwrap_err();
  assert_eq!(error.code(), "cutover_evidence_binding");
  fixture.assert_source_preserved();
  assert!(!fixture.paths.backup_path().exists());
  assert!(!fixture.paths.journal_workspace_path().exists());

  let mut fixture = CutoverFixture::new();
  let mut stale_source = fixture.evidence.clone();
  stale_source.source_complete_file_checksum[0] ^= 0xff;
  let error = fixture.try_prepare(stale_source, fixture.cancellation.clone()).unwrap_err();
  assert_eq!(error.code(), "cutover_source_file_evidence");
  fixture.assert_source_preserved();
  assert!(!fixture.paths.backup_path().exists());

  let mut fixture = CutoverFixture::new();
  let mut stale_verification = fixture.evidence.clone();
  stale_verification.destination_full_verification_evidence[0] ^= 0xff;
  let error = fixture.try_prepare(stale_verification, fixture.cancellation.clone()).unwrap_err();
  assert_eq!(error.code(), "cutover_destination_verification_binding");
  fixture.assert_source_preserved();
  assert!(!fixture.paths.backup_path().exists());

  let mut fixture = CutoverFixture::new();
  let mut stale_destination = fixture.evidence.clone();
  stale_destination.destination_file.selected_header_sequence += 1;
  let error = fixture.try_prepare(stale_destination, fixture.cancellation.clone()).unwrap_err();
  assert_eq!(error.code(), "cutover_destination_file_evidence");
  fixture.assert_source_preserved();
  assert!(!fixture.paths.backup_path().exists());

  let mut fixture = CutoverFixture::new();
  let replacement = fixture.paths.service_path().parent().unwrap().join("replacement-v3.aeordb");
  fs::copy(fixture.paths.service_path(), &replacement).unwrap();
  fs::rename(&replacement, fixture.paths.service_path()).unwrap();
  let error = fixture.try_prepare(fixture.evidence.clone(), fixture.cancellation.clone()).unwrap_err();
  assert_eq!(error.code(), "cutover_source_file_identity");
  assert_eq!(complete_file_blake3(fixture.paths.service_path()), fixture.evidence.source_complete_file_checksum);
  assert!(!fixture.paths.backup_path().exists());

  let mut fixture = CutoverFixture::new();
  File::create(fixture.paths.backup_path()).unwrap().sync_all().unwrap();
  let error = fixture.try_prepare(fixture.evidence.clone(), fixture.cancellation.clone()).unwrap_err();
  assert_eq!(error.code(), "cutover_backup_collision");
  fixture.assert_source_preserved();

  let mut fixture = CutoverFixture::new();
  let canceled = CancellationToken::new();
  canceled.cancel();
  let error = fixture.try_prepare(fixture.evidence.clone(), canceled).unwrap_err();
  assert_eq!(error.code(), "cutover_rehearsal_cancelled");
  fixture.assert_source_preserved();
  assert!(!fixture.paths.journal_workspace_path().exists());
}

#[test]
fn recovery_refuses_valid_but_unapproved_journal_successors_and_corrupt_journals() {
  let mut fixture = CutoverFixture::new();
  let rehearsal = fixture.prepare();
  let mut unapproved = rehearsal.selected_body().clone();
  unapproved.phase = MigrationPhaseV1::OperatorAcceptance;
  unapproved.journal_sequence += 1;
  unapproved.updated_at_ms += 1;
  drop(rehearsal);
  let encoded = encode_side_by_side_cutover_control_v1(2, &unapproved, fixture.algorithm).unwrap();
  let mut journal = DurableCutoverJournalWorkspaceV1::open_selected(
    fixture.paths.journal_workspace_path(),
    fixture.algorithm,
    CutoverJournalWorkspaceOptionsV1::new(0),
    fixture.cancellation.clone(),
    &fixture.memory,
  )
  .unwrap();
  journal.publish(&encoded).unwrap();
  drop(journal);
  let error = fixture.try_recover(1_000).unwrap_err();
  assert_eq!(error.code(), "cutover_recovery_phase");
  fixture.assert_source_preserved();

  let mut fixture = CutoverFixture::new();
  let rehearsal = fixture.prepare();
  drop(rehearsal);
  let journal_path = fixture.paths.journal_workspace_path().join(CUTOVER_JOURNAL_FILE_NAME_V1);
  let journal = OpenOptions::new().write(true).truncate(true).open(journal_path).unwrap();
  journal.set_len(2_048).unwrap();
  journal.sync_all().unwrap();
  let error = fixture.try_recover(1_000).unwrap_err();
  assert_eq!(error.code(), "cutover_journal_workspace_format");
  fixture.assert_source_preserved();
}

#[test]
fn cutover_paths_require_absolute_distinct_same_parent_artifacts() {
  let relative = SideBySideCutoverPathsV1::new("service", "destination", "journal", MIGRATION_ID).unwrap_err();
  assert_eq!(relative.code(), "cutover_path_absolute");

  let first = tempfile::tempdir().unwrap();
  let second = tempfile::tempdir().unwrap();
  let wrong_parent = SideBySideCutoverPathsV1::new(
    first.path().join("service"),
    second.path().join("destination"),
    first.path().join("journal"),
    MIGRATION_ID,
  )
  .unwrap_err();
  assert_eq!(wrong_parent.code(), "cutover_path_parent");
}

#[test]
fn execution_and_rollback_refuse_cancellation_invalid_evidence_and_clock_overflow_before_mutation() {
  let mut fixture = CutoverFixture::new();
  let mut rehearsal = fixture.prepare();
  fixture.cancellation.cancel();
  let error = rehearsal.execute(clock(100), &mut fixture.retirement).unwrap_err();
  assert_eq!(error.code(), "cutover_rehearsal_cancelled");
  assert_eq!(complete_file_blake3(fixture.paths.service_path()), fixture.evidence.source_complete_file_checksum);
  assert!(!fixture.paths.backup_path().exists());

  let mut fixture = CutoverFixture::new();
  let mut rehearsal = fixture.prepare();
  let error = rehearsal.rollback_pre_acceptance(Vec::new(), clock(100), &mut fixture.retirement).unwrap_err();
  assert_eq!(error.code(), "cutover_rollback_evidence");
  assert_eq!(complete_file_blake3(fixture.paths.service_path()), fixture.evidence.source_complete_file_checksum);
  assert!(!fixture.paths.backup_path().exists());

  let overflow_clock =
    SideBySideCutoverClockV1 { updated_at_ms: i64::MAX, publication_timestamp_ms: i64::MAX as u64, monotonic_now_ms: u64::MAX };
  let error = rehearsal.execute(overflow_clock, &mut fixture.retirement).unwrap_err();
  assert_eq!(error.code(), "cutover_clock_overflow");
  assert_eq!(complete_file_blake3(fixture.paths.service_path()), fixture.evidence.source_complete_file_checksum);
  assert!(!fixture.paths.backup_path().exists());
}
