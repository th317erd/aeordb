use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aeordb::engine::config_resolver::CommandLineConfigOverrides;
use aeordb::engine::v4::database_header::{ReadOnlyDatabaseHeader, read_database_header_read_only};
use aeordb::engine::v4::first_authority::V4FirstAuthorityPublisher;
use aeordb::engine::v4::migration_control::{MigrationPhaseV1, MigrationProgressStateV1};
use aeordb::engine::v4::migration_offline_run::{
  OfflineMigrationRunClockV1, OfflineMigrationRunIdentityV1, OfflineMigrationRunMilestoneObserverV1, OfflineMigrationRunMilestoneV1,
  OfflineMigrationRunRequestV1, execute_offline_migration_v1,
};
use aeordb::engine::v4::migration_run_manifest::{MIGRATION_RUN_MANIFEST_FILE_NAME, MigrationRunBoundsV1};
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine, VersionManager};
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;

fn id(first: u8) -> [u8; 16] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn overrides() -> CommandLineConfigOverrides {
  CommandLineConfigOverrides::from_registered(BTreeMap::from([
    ("--migration-capture-max-bytes".to_string(), OsString::from("1GiB")),
    ("--migration-capture-free-reserve-bytes".to_string(), OsString::from("1GiB")),
    ("--migration-checkpoint-after-seconds".to_string(), OsString::from("30")),
  ]))
  .unwrap()
}

fn bounds() -> MigrationRunBoundsV1 {
  MigrationRunBoundsV1 {
    maximum_memory_bytes: 128 * MIB,
    maximum_work_items: 1_000_000,
    maximum_decoded_chunk_bytes: 8 * MIB,
    maximum_directory_depth: 128,
    maximum_authority_roots: 128,
    maximum_authority_records: 4_096,
    root_map_maximum_stored_bytes: 32 * MIB,
    root_map_maximum_staged_rows: 4_096,
    root_map_minimum_free_bytes: 0,
    root_map_maximum_sort_memory_bytes: 16 * MIB,
    root_map_maximum_open_runs: 4,
    root_map_maximum_page_rows: 128,
    root_map_maximum_publication_batch_bytes: MIB,
    prior_lookup_maximum_memory_bytes: 16 * MIB,
    lease_duration_ms: 60_000,
  }
}

struct Fixture {
  _temporary: tempfile::TempDir,
  source: PathBuf,
  destination: PathBuf,
  workspace: PathBuf,
  executable: PathBuf,
}

impl Fixture {
  fn new() -> Self {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let source = root.join("source-v3.aeordb");
    let engine = StorageEngine::create(source.to_str().unwrap()).unwrap();
    let operations = DirectoryOps::new(&engine);
    let context = RequestContext::system();
    operations.ensure_root_directory(&context).unwrap();
    operations.store_file_buffered(&context, "/before.txt", b"retained snapshot bytes", Some("text/plain")).unwrap();
    operations
      .store_file_buffered(&context, "/.aeordb-system/users/portable-user.json", b"portable user", Some("application/json"))
      .unwrap();
    operations
      .store_file_buffered(&context, "/.aeordb-system/config/node-local-secret.json", b"node-local secret", Some("application/json"))
      .unwrap();
    VersionManager::new(&engine).create_snapshot(&context, "before", HashMap::new()).unwrap();
    operations
      .store_file_buffered(&context, "/nested/.aeordb-logs/node-local.log", b"omit this node-local log", Some("text/plain"))
      .unwrap();
    operations.store_file_buffered(&context, "/nested/current.bin", b"current bytes\0\xff", Some("application/octet-stream")).unwrap();
    engine.shutdown().unwrap();
    Self {
      _temporary: temporary,
      source,
      destination: root.join("destination-v4.aeordb"),
      workspace: root.join("migration-workspace"),
      executable: std::env::current_exe().unwrap().canonicalize().unwrap(),
    }
  }

  fn request<'a>(&'a self, cancellation: &'a CancellationToken) -> OfflineMigrationRunRequestV1<'a> {
    OfflineMigrationRunRequestV1 {
      source: &self.source,
      destination: &self.destination,
      workspace: &self.workspace,
      executable: &self.executable,
      source_commit: [0x21; 20],
      identity: OfflineMigrationRunIdentityV1 {
        database_id: id(0x10),
        migration_id: id(0x20),
        source_physical_instance_id: id(0x30),
        destination_physical_instance_id: id(0x40),
        holder_boot_id: id(0x50),
      },
      configuration_overrides: overrides(),
      bounds: bounds(),
      acquisition_timeout: Duration::from_secs(10),
      clock: OfflineMigrationRunClockV1 { wall_time_ms: 1_700_000_000_000, monotonic_time_ms: 10_000 },
      cancellation,
      resume: false,
      milestone_observer: None,
    }
  }
}

struct PauseAtMilestone {
  target: OfflineMigrationRunMilestoneV1,
  paused: bool,
}

impl OfflineMigrationRunMilestoneObserverV1 for PauseAtMilestone {
  fn should_pause_after(&mut self, milestone: OfflineMigrationRunMilestoneV1) -> bool {
    if !self.paused && milestone == self.target {
      self.paused = true;
      true
    } else {
      false
    }
  }
}

fn file_blake3(path: &Path) -> [u8; 32] {
  *blake3::hash(&fs::read(path).unwrap()).as_bytes()
}

#[test]
fn real_offline_run_reaches_verified_shadow_without_changing_the_v3_source() {
  let fixture = Fixture::new();
  let cancellation = CancellationToken::new();
  let source_before = file_blake3(&fixture.source);
  let source_size = fs::metadata(&fixture.source).unwrap().len();

  let receipt = execute_offline_migration_v1(fixture.request(&cancellation)).unwrap();

  assert_eq!(receipt.phase, MigrationPhaseV1::DestinationVerify);
  assert_eq!(receipt.state, MigrationProgressStateV1::Complete);
  assert!(receipt.destination_full_verified);
  assert_eq!(receipt.source_complete_file_checksum, source_before);
  assert_eq!(file_blake3(&fixture.source), source_before);
  assert_eq!(fs::metadata(&fixture.source).unwrap().len(), source_size);
  assert!(fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME).is_file());
  assert!(fixture.destination.is_file());

  let mut destination = fs::File::open(&fixture.destination).unwrap();
  let ReadOnlyDatabaseHeader::V4(header) = read_database_header_read_only(&mut destination).unwrap() else {
    panic!("offline migration destination must be v4");
  };
  assert_eq!(header.header.database_id, id(0x10));
  assert_eq!(header.header.physical_instance_id, id(0x40));
  assert_ne!(header.header.physical_instance_id, id(0x30));
  assert_eq!(receipt.verified_root_count, 2);
  assert!(receipt.verified_entity_count >= 7);
  assert_eq!(receipt.verified_content_bytes, 51);
}

#[test]
fn completed_offline_run_resumes_through_a_new_live_proof_without_changing_either_file() {
  let fixture = Fixture::new();
  let first_cancellation = CancellationToken::new();
  let first = execute_offline_migration_v1(fixture.request(&first_cancellation)).unwrap();
  let source_before_resume = file_blake3(&fixture.source);
  let destination_before_resume = file_blake3(&fixture.destination);
  let destination_size = fs::metadata(&fixture.destination).unwrap().len();

  let resumed_cancellation = CancellationToken::new();
  let mut resumed_request = fixture.request(&resumed_cancellation);
  resumed_request.resume = true;
  resumed_request.clock = OfflineMigrationRunClockV1 { wall_time_ms: 1_700_000_010_000, monotonic_time_ms: 20_000 };
  let resumed = execute_offline_migration_v1(resumed_request).unwrap();

  assert_eq!(resumed, first);
  assert_eq!(file_blake3(&fixture.source), source_before_resume);
  assert_eq!(file_blake3(&fixture.destination), destination_before_resume);
  assert_eq!(fs::metadata(&fixture.destination).unwrap().len(), destination_size);
}

#[test]
fn early_durable_milestones_resume_to_verified_without_changing_the_v3_source() {
  for milestone in [
    OfflineMigrationRunMilestoneV1::ManifestDurable,
    OfflineMigrationRunMilestoneV1::DestinationInitialized,
    OfflineMigrationRunMilestoneV1::MigrationControlsAcquired,
    OfflineMigrationRunMilestoneV1::SourceGcSuspended,
  ] {
    assert_durable_milestone_resumes(milestone);
  }
}

#[test]
fn first_retirement_bearing_transition_resumes_without_forking_the_journal() {
  assert_durable_milestone_resumes(OfflineMigrationRunMilestoneV1::PreflightRunning);
}

fn assert_durable_milestone_resumes(milestone: OfflineMigrationRunMilestoneV1) {
  let fixture = Fixture::new();
  let source_before = file_blake3(&fixture.source);
  let source_size = fs::metadata(&fixture.source).unwrap().len();
  let first_cancellation = CancellationToken::new();
  let mut observer = PauseAtMilestone { target: milestone, paused: false };
  let mut first_request = fixture.request(&first_cancellation);
  first_request.milestone_observer = Some(&mut observer);

  let error = execute_offline_migration_v1(first_request).unwrap_err();
  assert_eq!(error.code(), "offline_migration_milestone_pause", "milestone {milestone:?}");
  assert!(observer.paused);
  assert!(fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME).is_file());
  assert_eq!(fixture.destination.exists(), milestone != OfflineMigrationRunMilestoneV1::ManifestDurable);
  assert_eq!(file_blake3(&fixture.source), source_before);
  assert_eq!(fs::metadata(&fixture.source).unwrap().len(), source_size);

  let resumed_cancellation = CancellationToken::new();
  let mut resumed_request = fixture.request(&resumed_cancellation);
  resumed_request.resume = true;
  resumed_request.clock = OfflineMigrationRunClockV1 { wall_time_ms: 1_700_000_010_000, monotonic_time_ms: 20_000 };
  let receipt = execute_offline_migration_v1(resumed_request).unwrap_or_else(|error| panic!("milestone {milestone:?}: {error:?}"));
  assert_eq!(receipt.phase, MigrationPhaseV1::DestinationVerify);
  assert_eq!(receipt.state, MigrationProgressStateV1::Complete);
  assert!(receipt.destination_full_verified);
  assert_eq!(file_blake3(&fixture.source), source_before);
  assert_eq!(fs::metadata(&fixture.source).unwrap().len(), source_size);

  if milestone == OfflineMigrationRunMilestoneV1::PreflightRunning {
    let publisher = V4FirstAuthorityPublisher::open(&fixture.destination).unwrap();
    let memory = MemoryCoordinator::new(MemoryPolicy::new(256 * MIB, 384 * MIB, 1, 32 * MIB).unwrap());
    let journal = publisher
      .reconstruct_retirement_journal_summary(&CancellationToken::new(), &memory, 4_096, 4_096, 1_000_000, 128 * MIB)
      .unwrap()
      .expect("a completed migration must retain one retirement chain");
    assert_eq!(journal.segment_count, journal.last_segment_ordinal);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0);
  }

  let destination_before_retry = file_blake3(&fixture.destination);
  let retry_cancellation = CancellationToken::new();
  let mut retry_request = fixture.request(&retry_cancellation);
  retry_request.resume = true;
  retry_request.clock = OfflineMigrationRunClockV1 { wall_time_ms: 1_700_000_020_000, monotonic_time_ms: 30_000 };
  assert_eq!(execute_offline_migration_v1(retry_request).unwrap(), receipt);
  assert_eq!(file_blake3(&fixture.destination), destination_before_retry);
  assert_eq!(file_blake3(&fixture.source), source_before);
}

#[test]
fn resume_refuses_a_foreign_v4_destination_without_changing_either_file() {
  let fixture = Fixture::new();
  let first_cancellation = CancellationToken::new();
  execute_offline_migration_v1(fixture.request(&first_cancellation)).unwrap();

  let foreign = Fixture::new();
  let foreign_cancellation = CancellationToken::new();
  let mut foreign_request = foreign.request(&foreign_cancellation);
  foreign_request.identity.destination_physical_instance_id = id(0x41);
  execute_offline_migration_v1(foreign_request).unwrap();
  fs::copy(&foreign.destination, &fixture.destination).unwrap();
  let source_before_resume = file_blake3(&fixture.source);
  let destination_before_resume = file_blake3(&fixture.destination);
  let destination_size = fs::metadata(&fixture.destination).unwrap().len();

  let resumed_cancellation = CancellationToken::new();
  let mut resumed_request = fixture.request(&resumed_cancellation);
  resumed_request.resume = true;
  let error = execute_offline_migration_v1(resumed_request).unwrap_err();

  assert_eq!(error.code(), "offline_migration_resume_destination");
  assert_eq!(file_blake3(&fixture.source), source_before_resume);
  assert_eq!(file_blake3(&fixture.destination), destination_before_resume);
  assert_eq!(fs::metadata(&fixture.destination).unwrap().len(), destination_size);
}

#[test]
fn resume_requires_the_exact_manifest_and_sealed_root_map_without_changing_database_files() {
  let missing = Fixture::new();
  let source_before = file_blake3(&missing.source);
  let cancellation = CancellationToken::new();
  let mut request = missing.request(&cancellation);
  request.resume = true;
  let error = execute_offline_migration_v1(request).unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_directory");
  assert_eq!(file_blake3(&missing.source), source_before);
  assert!(!missing.destination.exists());

  let mismatched = Fixture::new();
  let first_cancellation = CancellationToken::new();
  execute_offline_migration_v1(mismatched.request(&first_cancellation)).unwrap();
  let source_before = file_blake3(&mismatched.source);
  let destination_before = file_blake3(&mismatched.destination);
  let resumed_cancellation = CancellationToken::new();
  let mut request = mismatched.request(&resumed_cancellation);
  request.resume = true;
  request.source_commit = [0x22; 20];
  let error = execute_offline_migration_v1(request).unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_permit");
  assert_eq!(file_blake3(&mismatched.source), source_before);
  assert_eq!(file_blake3(&mismatched.destination), destination_before);

  let damaged = Fixture::new();
  let first_cancellation = CancellationToken::new();
  execute_offline_migration_v1(damaged.request(&first_cancellation)).unwrap();
  let source_before = file_blake3(&damaged.source);
  let destination_before = file_blake3(&damaged.destination);
  let closure =
    damaged.workspace.join(hex::encode(id(0x10))).join(hex::encode(id(0x20))).join("root-map-0000000000000001").join("closure.armc");
  fs::OpenOptions::new().append(true).open(closure).unwrap().write_all(b"unexpected tail").unwrap();
  let resumed_cancellation = CancellationToken::new();
  let mut request = damaged.request(&resumed_cancellation);
  request.resume = true;
  let error = execute_offline_migration_v1(request).unwrap_err();
  assert_eq!(error.code(), "migration_root_map_capacity");
  assert_eq!(file_blake3(&damaged.source), source_before);
  assert_eq!(file_blake3(&damaged.destination), destination_before);
}

#[test]
fn malformed_source_and_existing_destination_refuse_before_creating_run_state() {
  let fixture = Fixture::new();
  let cancellation = CancellationToken::new();
  fs::write(&fixture.source, b"not an aeordb file").unwrap();
  let error = execute_offline_migration_v1(fixture.request(&cancellation)).unwrap_err();
  assert_eq!(error.code(), "offline_migration_source_header");
  assert!(!fixture.workspace.exists());
  assert!(!fixture.destination.exists());

  let fixture = Fixture::new();
  fs::write(&fixture.destination, b"foreign").unwrap();
  let error = execute_offline_migration_v1(fixture.request(&cancellation)).unwrap_err();
  assert_eq!(error.code(), "offline_migration_destination");
  assert!(!fixture.workspace.exists());
  assert_eq!(fs::read(&fixture.destination).unwrap(), b"foreign");
}
