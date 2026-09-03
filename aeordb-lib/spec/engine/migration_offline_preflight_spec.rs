use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use aeordb::engine::config_resolver::CommandLineConfigOverrides;
use aeordb::engine::v4::database_header::{ReadOnlyDatabaseHeader, read_database_header_read_only};
use aeordb::engine::v4::migration_offline_preflight::{OfflineMigrationPreflightRequestV1, collect_offline_migration_preflight_v1};
use aeordb::engine::v4::migration_run_manifest::{MigrationRunBoundsV1, MigrationRunManifestCreateRequestV1, create_migration_run_manifest_v1};
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn id(first: u8) -> [u8; 16] {
  std::array::from_fn(|offset| first.wrapping_add(offset as u8))
}

fn migration_overrides() -> CommandLineConfigOverrides {
  CommandLineConfigOverrides::from_registered(BTreeMap::from([
    ("--migration-capture-max-bytes".to_string(), OsString::from("1GiB")),
    ("--migration-capture-free-reserve-bytes".to_string(), OsString::from("1GiB")),
    ("--migration-checkpoint-after-seconds".to_string(), OsString::from("30")),
  ]))
  .unwrap()
}

fn bounds() -> MigrationRunBoundsV1 {
  MigrationRunBoundsV1 {
    maximum_memory_bytes: 64 * MIB,
    maximum_work_items: 100_000,
    maximum_decoded_chunk_bytes: 8 * MIB,
    maximum_directory_depth: 64,
    maximum_authority_roots: 128,
    maximum_authority_records: 1_024,
    root_map_maximum_stored_bytes: 16 * MIB,
    root_map_maximum_staged_rows: 1_024,
    root_map_minimum_free_bytes: GIB,
    root_map_maximum_sort_memory_bytes: 8 * MIB,
    root_map_maximum_open_runs: 4,
    root_map_maximum_page_rows: 128,
    root_map_maximum_publication_batch_bytes: MIB,
    prior_lookup_maximum_memory_bytes: 8 * MIB,
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
    operations.ensure_root_directory(&RequestContext::system()).unwrap();
    operations
      .store_file_buffered(&RequestContext::system(), "/preflight.txt", b"offline migration preflight", Some("text/plain"))
      .unwrap();
    engine.shutdown().unwrap();
    Self {
      _temporary: temporary,
      source,
      destination: root.join("destination-v4.aeordb"),
      workspace: root.join("migration-workspace"),
      executable: std::env::current_exe().unwrap(),
    }
  }

  fn request<'a>(&'a self, cancellation: &'a CancellationToken) -> OfflineMigrationPreflightRequestV1<'a> {
    OfflineMigrationPreflightRequestV1 {
      source: &self.source,
      destination: &self.destination,
      workspace: &self.workspace,
      executable: &self.executable,
      source_commit: [0x21; 20],
      database_id: id(0x10),
      migration_id: id(0x20),
      source_physical_instance_id: id(0x30),
      destination_physical_instance_id: id(0x40),
      configuration_overrides: migration_overrides(),
      bounds: bounds(),
      acquisition_timeout: Duration::from_secs(10),
      cancellation,
      resume_manifest: None,
    }
  }
}

fn file_blake3(path: &Path) -> [u8; 32] {
  *blake3::hash(&fs::read(path).unwrap()).as_bytes()
}

#[test]
fn real_v3_preflight_is_source_invariant_and_reproducible_across_manifest_restart() {
  let fixture = Fixture::new();
  let cancellation = CancellationToken::new();
  let source_before = file_blake3(&fixture.source);

  let first = collect_offline_migration_preflight_v1(fixture.request(&cancellation)).unwrap();

  assert_eq!(first.permit().source_complete_file_checksum(), source_before);
  assert!(first.report().findings().is_empty());
  assert_eq!(file_blake3(&fixture.source), source_before);
  assert!(!fixture.workspace.exists());
  assert!(!fixture.destination.exists());

  let manifest = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: first.permit(),
    holder_boot_id: id(0x50),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &cancellation,
  })
  .unwrap();
  let second = collect_offline_migration_preflight_v1(fixture.request(&cancellation)).unwrap();

  manifest.validate_permit(second.permit()).unwrap();
  assert_eq!(file_blake3(&fixture.source), source_before);
  assert!(!fixture.destination.exists());
}

#[test]
fn malformed_source_and_precancellation_fail_without_creating_run_artifacts() {
  let fixture = Fixture::new();
  fs::write(&fixture.source, b"not an AeorDB database").unwrap();
  let cancellation = CancellationToken::new();
  let error = collect_offline_migration_preflight_v1(fixture.request(&cancellation)).unwrap_err();
  assert_eq!(error.code(), "offline_migration_source_header");
  assert!(!fixture.workspace.exists());
  assert!(!fixture.destination.exists());

  let fixture = Fixture::new();
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let error = collect_offline_migration_preflight_v1(fixture.request(&cancellation)).unwrap_err();
  assert_eq!(error.code(), "offline_migration_cancelled");
  assert!(!fixture.workspace.exists());
  assert!(!fixture.destination.exists());
}

#[test]
fn capacity_overcommit_is_a_preflight_refusal_without_destination_creation() {
  let fixture = Fixture::new();
  let cancellation = CancellationToken::new();
  let mut request = fixture.request(&cancellation);
  request.bounds.root_map_maximum_stored_bytes = 4 * 1024 * GIB;

  let error = collect_offline_migration_preflight_v1(request).unwrap_err();

  assert_eq!(error.code(), "offline_migration_preflight_refused");
  assert!(!fixture.workspace.exists());
  assert!(!fixture.destination.exists());
}

#[test]
fn recovery_required_source_is_refused_without_changing_any_database_byte() {
  let fixture = Fixture::new();
  let mut source = OpenOptions::new().read(true).write(true).open(&fixture.source).unwrap();
  let ReadOnlyDatabaseHeader::V3 { header, .. } = read_database_header_read_only(&mut source).unwrap() else {
    panic!("fixture must remain a v3 database");
  };
  source.seek(SeekFrom::Start(header.kv_block_offset)).unwrap();
  let mut page_magic_byte = [0u8; 1];
  source.read_exact(&mut page_magic_byte).unwrap();
  source.seek(SeekFrom::Start(header.kv_block_offset)).unwrap();
  source.write_all(&[page_magic_byte[0] ^ 0xff]).unwrap();
  source.sync_all().unwrap();
  drop(source);

  let source_before = file_blake3(&fixture.source);
  let cancellation = CancellationToken::new();
  let error = collect_offline_migration_preflight_v1(fixture.request(&cancellation)).unwrap_err();

  assert_eq!(error.code(), "offline_migration_source_open");
  assert_eq!(file_blake3(&fixture.source), source_before);
  assert!(!fixture.workspace.exists());
  assert!(!fixture.destination.exists());
}

#[cfg(unix)]
#[test]
fn preflight_accepts_a_clean_source_without_write_permission() {
  use std::os::unix::fs::PermissionsExt;

  let fixture = Fixture::new();
  let mut permissions = fs::metadata(&fixture.source).unwrap().permissions();
  permissions.set_mode(0o444);
  fs::set_permissions(&fixture.source, permissions).unwrap();
  let source_before = file_blake3(&fixture.source);
  let cancellation = CancellationToken::new();

  collect_offline_migration_preflight_v1(fixture.request(&cancellation)).unwrap();

  assert_eq!(file_blake3(&fixture.source), source_before);
  assert!(!fixture.workspace.exists());
  assert!(!fixture.destination.exists());
}

#[cfg(unix)]
#[test]
fn existing_workspace_beneath_a_symlinked_parent_is_refused() {
  use std::os::unix::fs::symlink;

  let fixture = Fixture::new();
  let root = fixture.destination.parent().unwrap();
  let real_parent = root.join("real-workspace-parent");
  let linked_parent = root.join("linked-workspace-parent");
  fs::create_dir(&real_parent).unwrap();
  fs::create_dir(real_parent.join("workspace")).unwrap();
  symlink(&real_parent, &linked_parent).unwrap();
  let aliased_workspace = linked_parent.join("workspace");
  let cancellation = CancellationToken::new();
  let mut request = fixture.request(&cancellation);
  request.workspace = &aliased_workspace;

  let error = collect_offline_migration_preflight_v1(request).unwrap_err();

  assert_eq!(error.code(), "offline_migration_workspace_path");
  assert!(!fixture.destination.exists());
}
