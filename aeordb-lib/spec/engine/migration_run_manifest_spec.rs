use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::MemoryPressure;
use aeordb::engine::native_durability::{PlatformFileIdentityDescriptorV1, platform_file_identity};
use aeordb::engine::v4::admission::{BinaryCapabilityProfileV1, CapabilitySetV1};
use aeordb::engine::v4::contract_generated::CONTRACT_REGISTRY_SHA256;
use aeordb::engine::v4::migration_destination::{MigrationDestinationPathObservationV1, observe_migration_destination_path_v1};
use aeordb::engine::v4::migration_preflight::{
  AuthorityInventoryCountsV1, CapacityRoleV1, MigrationBinaryEvidenceV1, MigrationCapacityObservationV1, MigrationConfigurationEvidenceV1,
  MigrationIdentityEvidenceV1, MigrationMemoryEvidenceV1, MigrationNativeEvidenceV1, MigrationPreflightPermitV1,
  MigrationPreflightRequestV1, MigrationRecoveryEvidenceV1, MigrationSourceEvidenceV1, NativeCutoverCapabilitiesV1,
  SourceAuthorityInventoryV1, StrictVerificationEvidenceV1, StrictVerificationStateV1, admit_migration_preflight_v1,
};
use aeordb::engine::v4::migration_run_manifest::{
  MIGRATION_RUN_MANIFEST_FILE_NAME, MigrationRunBoundsV1, MigrationRunManifestCreateRequestV1, create_migration_run_manifest_v1,
  open_migration_run_manifest_v1,
};
use aeordb::engine::v4::system_family::embedded_system_family_registry;
use tokio_util::sync::CancellationToken;

const GIB: u64 = 1024 * 1024 * 1024;
const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

#[cfg(not(windows))]
fn unresolved_parent_path(root: &Path, child: &str, file_name: &str) -> PathBuf {
  root.join(child).join("..").join(file_name)
}

#[cfg(windows)]
fn unresolved_parent_path(root: &Path, child: &str, file_name: &str) -> PathBuf {
  use std::ffi::OsString;
  use std::os::windows::ffi::{OsStrExt, OsStringExt};

  let mut path: Vec<u16> = root.as_os_str().encode_wide().collect();
  path.extend("\\".encode_utf16());
  path.extend(child.encode_utf16());
  path.extend("\\..\\".encode_utf16());
  path.extend(file_name.encode_utf16());
  OsString::from_wide(&path).into()
}

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

fn native_path_digest(path: &Path) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new();
  hasher.update(b"aeordb.migration-destination-path.v1\0");
  #[cfg(unix)]
  {
    use std::os::unix::ffi::OsStrExt;
    hasher.update(&[1]);
    hasher.update(path.as_os_str().as_bytes());
  }
  #[cfg(windows)]
  {
    use std::os::windows::ffi::OsStrExt;
    hasher.update(&[2]);
    for unit in path.as_os_str().encode_wide() {
      hasher.update(&unit.to_le_bytes());
    }
  }
  *hasher.finalize().as_bytes()
}

fn capacity(role: CapacityRoleV1, identity: PlatformFileIdentityDescriptorV1) -> MigrationCapacityObservationV1 {
  MigrationCapacityObservationV1 {
    role,
    volume_identity: identity.volume_identity,
    path_identity: identity,
    filesystem_capacity_bytes: 256 * GIB,
    available_bytes: 192 * GIB,
    required_bytes: if role == CapacityRoleV1::Capture { 64 * GIB } else { 4 * GIB },
    minimum_remaining_bytes: 16 * GIB,
  }
}

fn permit_with_migration_id(
  source: &Path,
  destination: &MigrationDestinationPathObservationV1,
  migration_id: [u8; 16],
) -> MigrationPreflightPermitV1 {
  let source_identity = platform_file_identity(source).unwrap();
  let source_size = fs::metadata(source).unwrap().len();
  let source_checksum = digest(0x70);
  let registry = embedded_system_family_registry(ALGORITHM).unwrap();
  let baseline = CapabilitySetV1::v4_baseline();
  let request = MigrationPreflightRequestV1 {
    identity: MigrationIdentityEvidenceV1 {
      database_id: id(0x10),
      migration_id,
      source_physical_instance_id: id(0x30),
      destination_physical_instance_id: id(0x40),
      source_path_digest: native_path_digest(source),
      destination_path_digest: destination.path_digest(),
      source_file_identity: source_identity,
      destination_parent_identity: destination.parent_identity(),
    },
    source: MigrationSourceEvidenceV1 {
      hash_algorithm: ALGORITHM,
      file_size: source_size,
      complete_file_checksum: source_checksum,
      selected_header_slot: 1,
      selected_header_sequence: 41,
      selected_header_digest: digest(0x80),
      head_hash: digest(0x90).to_vec(),
    },
    verification: StrictVerificationEvidenceV1 {
      state: StrictVerificationStateV1::CompleteClean,
      source_file_size: source_size,
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
        protected_families: u64::from(registry.family_count),
        modules: 2,
        snapshots: 3,
        forks: 1,
        symlinks: 4,
        history_roots: 4,
        peers: 2,
        sync_states: 2,
        tasks: 5,
        plugins: 2,
        roots: 8,
      },
      authority_digest: digest(0xc0),
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
    capacity: [
      capacity(CapacityRoleV1::Destination, destination.parent_identity()),
      capacity(CapacityRoleV1::Workspace, PlatformFileIdentityDescriptorV1 { file_identity: id(0x72), ..destination.parent_identity() }),
      capacity(CapacityRoleV1::Backup, PlatformFileIdentityDescriptorV1 { file_identity: id(0x73), ..destination.parent_identity() }),
      capacity(CapacityRoleV1::Capture, PlatformFileIdentityDescriptorV1 { file_identity: id(0x74), ..destination.parent_identity() }),
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
      effective_configuration_fingerprint: vec![0x17; ALGORITHM.hash_length()],
    },
    binary: MigrationBinaryEvidenceV1 {
      source_commit: [0x21; 20],
      executable_sha256: digest(0x31),
      contract_registry_sha256: hex::decode(CONTRACT_REGISTRY_SHA256).unwrap().try_into().unwrap(),
      capability_profile: BinaryCapabilityProfileV1::new(BinaryCapabilityProfileV1::current().supported_reader_capabilities, baseline),
      required_reader_capabilities: baseline,
      required_writer_capabilities: baseline,
      system_family_registry_fingerprint: registry.operational_fingerprint.clone(),
    },
  };
  admit_migration_preflight_v1(&request).unwrap().1
}

fn permit(source: &Path, destination: &MigrationDestinationPathObservationV1) -> MigrationPreflightPermitV1 {
  permit_with_migration_id(source, destination, id(0x20))
}

fn bounds() -> MigrationRunBoundsV1 {
  MigrationRunBoundsV1 {
    maximum_memory_bytes: 256 * 1024 * 1024,
    maximum_work_items: 1_000_000,
    maximum_decoded_chunk_bytes: 64 * 1024 * 1024,
    maximum_directory_depth: 256,
    maximum_authority_roots: 1_000,
    maximum_authority_records: 10_000,
    root_map_maximum_stored_bytes: GIB,
    root_map_maximum_staged_rows: 10_000,
    root_map_minimum_free_bytes: GIB,
    root_map_maximum_sort_memory_bytes: 64 * 1024 * 1024,
    root_map_maximum_open_runs: 8,
    root_map_maximum_page_rows: 512,
    root_map_maximum_publication_batch_bytes: 8 * 1024 * 1024,
    prior_lookup_maximum_memory_bytes: 64 * 1024 * 1024,
    lease_duration_ms: 60 * 60 * 1_000,
  }
}

struct Fixture {
  _directory: tempfile::TempDir,
  source: PathBuf,
  destination: PathBuf,
  workspace: PathBuf,
  permit: MigrationPreflightPermitV1,
}

fn new_fixture() -> Fixture {
  let directory = tempfile::tempdir().unwrap();
  let root = directory.path().canonicalize().unwrap();
  let source = root.join("source-v3.aeordb");
  let destination_parent = root.join("destination");
  fs::create_dir(&destination_parent).unwrap();
  let destination = destination_parent.join("destination-v4.aeordb");
  let workspace = root.join("migration-workspace");
  fs::write(&source, b"sealed disposable v3 source evidence").unwrap();
  let observation = observe_migration_destination_path_v1(&destination).unwrap();
  let permit = permit(&source, &observation);
  Fixture { _directory: directory, source, destination, workspace, permit }
}

fn create(fixture: &Fixture, run_bounds: MigrationRunBoundsV1) -> aeordb::engine::v4::migration_run_manifest::MigrationRunManifestV1 {
  create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: run_bounds,
    cancellation: &CancellationToken::new(),
  })
  .unwrap()
}

#[cfg(unix)]
fn make_private(path: &Path) {
  use std::os::unix::fs::PermissionsExt;
  fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn rewrite_manifest_value(fixture: &Fixture, mutate: impl FnOnce(&mut serde_json::Value), repair_checksum: bool) {
  let path = fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME);
  let mut document: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
  mutate(&mut document);
  if repair_checksum {
    let body = serde_json::to_vec(document.get("body").unwrap()).unwrap();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aeordb.offline-migration-run-manifest.v1\0");
    hasher.update(&body);
    document["body_blake3"] = serde_json::Value::String(hex::encode(hasher.finalize().as_bytes()));
  }
  fs::write(path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
}

fn replace_json_field_value(document: &mut String, field: &str, replacement: &str) {
  let label = format!("\"{field}\": ");
  let value_start = document.find(&label).unwrap_or_else(|| panic!("manifest fixture does not contain field {field:?}")) + label.len();
  let remainder = &document[value_start..];
  let comma = remainder.find(',').unwrap_or(usize::MAX);
  let newline = remainder.find('\n').unwrap_or(usize::MAX);
  let terminator = comma.min(newline);
  assert_ne!(terminator, usize::MAX, "manifest field has no terminator");
  let value_end = value_start + terminator;
  document.replace_range(value_start..value_end, replacement);
}

fn compact_json_object(document: &str, start: usize) -> (Vec<u8>, usize) {
  let bytes = document.as_bytes();
  assert_eq!(bytes[start], b'{');
  let mut compact = Vec::new();
  let mut depth = 0usize;
  let mut in_string = false;
  let mut escaped = false;
  for (offset, byte) in bytes[start..].iter().copied().enumerate() {
    if in_string {
      compact.push(byte);
      if escaped {
        escaped = false;
      } else if byte == b'\\' {
        escaped = true;
      } else if byte == b'"' {
        in_string = false;
      }
      continue;
    }
    match byte {
      b'"' => {
        in_string = true;
        compact.push(byte);
      }
      b'{' => {
        depth += 1;
        compact.push(byte);
      }
      b'}' => {
        depth -= 1;
        compact.push(byte);
        if depth == 0 {
          return (compact, start + offset);
        }
      }
      b' ' | b'\n' | b'\r' | b'\t' => {}
      _ => compact.push(byte),
    }
  }
  panic!("manifest body object is incomplete")
}

fn repair_manifest_checksum(document: &mut String) {
  let body_label = document.find("\"body\":").unwrap();
  let body_start = body_label + document[body_label..].find('{').unwrap();
  let (body, _) = compact_json_object(document, body_start);
  let mut hasher = blake3::Hasher::new();
  hasher.update(b"aeordb.offline-migration-run-manifest.v1\0");
  hasher.update(&body);
  let checksum = hex::encode(hasher.finalize().as_bytes());
  let checksum_label = "\"body_blake3\": \"";
  let checksum_start = document.find(checksum_label).unwrap() + checksum_label.len();
  let checksum_end = checksum_start + document[checksum_start..].find('"').unwrap();
  document.replace_range(checksum_start..checksum_end, &checksum);
}

fn rewrite_manifest_text(fixture: &Fixture, mutate: impl FnOnce(&mut String)) {
  let path = fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME);
  let mut document = String::from_utf8(fs::read(&path).unwrap()).unwrap();
  mutate(&mut document);
  repair_manifest_checksum(&mut document);
  fs::write(path, document).unwrap();
}

fn assert_create_error_code(
  fixture: &Fixture,
  run_bounds: MigrationRunBoundsV1,
  holder_boot_id: [u8; 16],
  created_at_ms: u64,
  cancellation: &CancellationToken,
  code: &str,
) {
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id,
    created_at_ms,
    bounds: run_bounds,
    cancellation,
  })
  .unwrap_err();
  assert_eq!(error.code(), code);
  assert!(!fixture.workspace.exists(), "rejected request created a workspace");
  assert!(!fixture.destination.exists(), "rejected request created a destination");
}

#[test]
fn durable_manifest_reopens_exact_admitted_run_without_touching_source_or_destination() {
  let fixture = new_fixture();
  let cancellation = CancellationToken::new();
  let source_before = fs::read(&fixture.source).unwrap();
  let expected_bounds = bounds();
  let created = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: expected_bounds,
    cancellation: &cancellation,
  })
  .unwrap();

  assert_eq!(created.path(), fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME));
  assert_eq!(created.database_id(), fixture.permit.database_id());
  assert_eq!(created.migration_id(), fixture.permit.migration_id());
  assert_eq!(created.holder_boot_id(), id(0x60));
  assert_eq!(created.source(), fixture.source);
  assert_eq!(created.destination(), fixture.destination);
  assert_eq!(created.bounds(), expected_bounds);
  created.validate_permit(&fixture.permit).unwrap();

  let reopened = open_migration_run_manifest_v1(&fixture.workspace, &cancellation).unwrap();
  assert_eq!(reopened, created);
  reopened.validate_permit(&fixture.permit).unwrap();
  assert_eq!(reopened.source_complete_file_checksum(), fixture.permit.source_complete_file_checksum());
  assert_eq!(reopened.preflight_evidence_fingerprint(), fixture.permit.evidence_fingerprint());
  assert_eq!(fs::read(&fixture.source).unwrap(), source_before);
  assert!(!fixture.destination.exists());

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(reopened.path()).unwrap().permissions().mode() & 0o077, 0);
  }
}

#[test]
fn invalid_bounds_identity_and_precanceled_run_fail_before_filesystem_mutation() {
  const MAX_MEMORY: u64 = 1024 * 1024 * 1024;
  const MAX_WORK: u64 = 1 << 40;
  const MAX_CHUNK: u64 = 64 * 1024 * 1024;
  const MAX_RECORDS: u64 = 1_000_000;
  const MAX_ROOT_MAP: u64 = 4 * 1024 * 1024 * 1024 * 1024;
  const MAX_PAGE_ROWS: u32 = 1_000_000;
  const MAX_PUBLICATION: u64 = 64 * 1024 * 1024;
  const MAX_LEASE_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

  let mut cases = Vec::new();
  macro_rules! invalid {
    ($name:literal, $field:ident, $value:expr) => {{
      let mut value = bounds();
      value.$field = $value;
      cases.push(($name, value));
    }};
  }
  invalid!("memory-zero", maximum_memory_bytes, 0);
  invalid!("memory-excess", maximum_memory_bytes, MAX_MEMORY + 1);
  invalid!("work-zero", maximum_work_items, 0);
  invalid!("work-excess", maximum_work_items, MAX_WORK + 1);
  invalid!("chunk-zero", maximum_decoded_chunk_bytes, 0);
  invalid!("chunk-excess", maximum_decoded_chunk_bytes, MAX_CHUNK + 1);
  invalid!("depth-zero", maximum_directory_depth, 0);
  invalid!("depth-excess", maximum_directory_depth, 1_001);
  invalid!("roots-zero", maximum_authority_roots, 0);
  invalid!("roots-excess", maximum_authority_roots, MAX_RECORDS + 1);
  invalid!("records-zero", maximum_authority_records, 0);
  invalid!("records-excess", maximum_authority_records, MAX_RECORDS + 1);
  invalid!("stored-zero", root_map_maximum_stored_bytes, 0);
  invalid!("stored-excess", root_map_maximum_stored_bytes, MAX_ROOT_MAP + 1);
  invalid!("staged-zero", root_map_maximum_staged_rows, 0);
  invalid!("staged-excess", root_map_maximum_staged_rows, MAX_RECORDS + 1);
  invalid!("free-excess", root_map_minimum_free_bytes, MAX_ROOT_MAP + 1);
  invalid!("sort-zero", root_map_maximum_sort_memory_bytes, 0);
  invalid!("sort-excess", root_map_maximum_sort_memory_bytes, MAX_MEMORY + 1);
  invalid!("runs-low", root_map_maximum_open_runs, 1);
  invalid!("runs-excess", root_map_maximum_open_runs, 65);
  invalid!("rows-zero", root_map_maximum_page_rows, 0);
  invalid!("rows-excess", root_map_maximum_page_rows, MAX_PAGE_ROWS + 1);
  invalid!("publication-zero", root_map_maximum_publication_batch_bytes, 0);
  invalid!("publication-excess", root_map_maximum_publication_batch_bytes, MAX_PUBLICATION + 1);
  invalid!("lookup-zero", prior_lookup_maximum_memory_bytes, 0);
  invalid!("lookup-excess", prior_lookup_maximum_memory_bytes, MAX_MEMORY + 1);
  invalid!("lease-zero", lease_duration_ms, 0);
  invalid!("lease-excess", lease_duration_ms, MAX_LEASE_MS + 1);
  invalid!("roots-above-records", maximum_authority_roots, 20_000);
  invalid!("staged-below-roots", root_map_maximum_staged_rows, 999);
  invalid!("lookup-above-total", prior_lookup_maximum_memory_bytes, 257 * 1024 * 1024);
  let mut decoded_above_total = bounds();
  decoded_above_total.maximum_memory_bytes = 32 * 1024 * 1024;
  cases.push(("decoded-above-total", decoded_above_total));
  let mut sort_above_total = bounds();
  sort_above_total.maximum_memory_bytes = 32 * 1024 * 1024;
  cases.push(("sort-above-total", sort_above_total));
  let mut publication_above_total = bounds();
  publication_above_total.maximum_memory_bytes = 4 * 1024 * 1024;
  cases.push(("publication-above-total", publication_above_total));
  let mut records_above_work = bounds();
  records_above_work.maximum_work_items = 9_999;
  cases.push(("records-above-work", records_above_work));

  for (name, invalid_bounds) in cases {
    let fixture = new_fixture();
    let cancellation = CancellationToken::new();
    let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
      workspace: &fixture.workspace,
      source: &fixture.source,
      destination: &fixture.destination,
      permit: &fixture.permit,
      holder_boot_id: id(0x60),
      created_at_ms: 1_700_000_000_000,
      bounds: invalid_bounds,
      cancellation: &cancellation,
    })
    .unwrap_err();
    assert_eq!(error.code(), "migration_run_manifest_bounds", "case {name}");
    assert!(!fixture.workspace.exists(), "case {name} created a workspace");
    assert!(!fixture.destination.exists(), "case {name} created a destination");
  }

  for (holder_boot_id, created_at_ms) in [([0; 16], 1_700_000_000_000), (id(0x60), 0), (id(0x60), i64::MAX as u64 + 1)] {
    let fixture = new_fixture();
    assert_create_error_code(
      &fixture,
      bounds(),
      holder_boot_id,
      created_at_ms,
      &CancellationToken::new(),
      "migration_run_manifest_identity",
    );
  }

  let fixture = new_fixture();
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  assert_create_error_code(&fixture, bounds(), id(0x60), 1_700_000_000_000, &cancellation, "migration_run_manifest_canceled");
}

#[test]
fn stale_source_destination_and_permit_bindings_fail_before_workspace_creation() {
  let fixture = new_fixture();
  fs::write(&fixture.source, b"source size changed after preflight").unwrap();
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_source_identity");
  assert!(!fixture.workspace.exists());

  let fixture = new_fixture();
  fs::write(&fixture.destination, b"preexisting destination").unwrap();
  let destination_before = fs::read(&fixture.destination).unwrap();
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_destination");
  assert!(!fixture.workspace.exists());
  assert_eq!(fs::read(&fixture.destination).unwrap(), destination_before);

  let fixture = new_fixture();
  let alternate_destination = fixture.destination.with_file_name("other-v4.aeordb");
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &alternate_destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_paths");
  assert!(!fixture.workspace.exists());

  let fixture = new_fixture();
  let foreign_root = tempfile::tempdir().unwrap();
  let foreign_root = foreign_root.path().canonicalize().unwrap();
  let foreign_source = foreign_root.join("foreign-v3.aeordb");
  fs::write(&foreign_source, b"sealed disposable v3 source evidence").unwrap();
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &foreign_source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert!(matches!(error.code(), "migration_run_manifest_paths" | "migration_run_manifest_source_identity"));
  assert!(!fixture.workspace.exists());
}

#[test]
fn malformed_source_and_workspace_paths_fail_without_creating_destination_state() {
  let fixture = new_fixture();
  let noncanonical_source = unresolved_parent_path(fixture.source.parent().unwrap(), "destination", "source-v3.aeordb");
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &noncanonical_source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_paths");
  assert!(!fixture.workspace.exists());
  assert!(!fixture.destination.exists());

  let fixture = new_fixture();
  let source_directory = fixture.source.with_file_name("source-directory");
  fs::create_dir(&source_directory).unwrap();
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &source_directory,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_source_type");
  assert!(!fixture.workspace.exists());
  assert!(!fixture.destination.exists());

  let fixture = new_fixture();
  fs::write(&fixture.workspace, b"not a workspace directory").unwrap();
  let workspace_before = fs::read(&fixture.workspace).unwrap();
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_directory");
  assert_eq!(fs::read(&fixture.workspace).unwrap(), workspace_before);
  assert!(!fixture.destination.exists());
}

#[test]
fn workspace_state_is_private_empty_no_follow_and_collision_safe() {
  let fixture = new_fixture();
  let created = create(&fixture, bounds());
  assert_eq!(created.workspace(), fixture.workspace);
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(&fixture.workspace).unwrap().permissions().mode() & 0o077, 0);
  }

  let fixture = new_fixture();
  fs::create_dir(&fixture.workspace).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fixture.workspace, fs::Permissions::from_mode(0o755)).unwrap();
  }
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_workspace");
  assert!(!fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME).exists());

  let fixture = new_fixture();
  create(&fixture, bounds());
  fs::remove_file(fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME)).unwrap();
  fs::write(fixture.workspace.join("unexpected-entry"), b"do not adopt").unwrap();
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_workspace_state");
  assert_eq!(fs::read(fixture.workspace.join("unexpected-entry")).unwrap(), b"do not adopt");

  let fixture = new_fixture();
  create(&fixture, bounds());
  let manifest_path = fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME);
  fs::remove_file(&manifest_path).unwrap();
  fs::write(&manifest_path, b"prior collision bytes").unwrap();
  let before = fs::read(&manifest_path).unwrap();
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert!(matches!(error.code(), "migration_run_manifest_create" | "migration_run_manifest_workspace_state"));
  assert_eq!(fs::read(&manifest_path).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn symlinked_source_destination_and_workspace_are_rejected_without_following() {
  use std::os::unix::fs::{PermissionsExt, symlink};

  let fixture = new_fixture();
  let source_link = fixture.source.with_file_name("source-link.aeordb");
  symlink(&fixture.source, &source_link).unwrap();
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &source_link,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_source_type");
  assert!(!fixture.workspace.exists());

  let fixture = new_fixture();
  let link_target = fixture.destination.with_file_name("destination-link-target");
  symlink(&link_target, &fixture.destination).unwrap();
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert_eq!(error.code(), "migration_run_manifest_destination");
  assert!(!fixture.workspace.exists());

  let fixture = new_fixture();
  let real_workspace = fixture.workspace.with_file_name("real-workspace");
  fs::create_dir(&real_workspace).unwrap();
  fs::set_permissions(&real_workspace, fs::Permissions::from_mode(0o700)).unwrap();
  symlink(&real_workspace, &fixture.workspace).unwrap();
  let error = create_migration_run_manifest_v1(MigrationRunManifestCreateRequestV1 {
    workspace: &fixture.workspace,
    source: &fixture.source,
    destination: &fixture.destination,
    permit: &fixture.permit,
    holder_boot_id: id(0x60),
    created_at_ms: 1_700_000_000_000,
    bounds: bounds(),
    cancellation: &CancellationToken::new(),
  })
  .unwrap_err();
  assert!(matches!(error.code(), "migration_run_manifest_directory" | "migration_run_manifest_paths"));
  assert!(fs::read_dir(&real_workspace).unwrap().next().is_none());
}

#[test]
fn reopen_rejects_corrupt_noncanonical_unknown_and_oversized_manifests() {
  let fixture = new_fixture();
  create(&fixture, bounds());
  let path = fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME);

  fs::write(&path, b"{").unwrap();
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_decode"
  );

  let fixture = new_fixture();
  create(&fixture, bounds());
  rewrite_manifest_value(&fixture, |document| document["version"] = serde_json::Value::from(2), false);
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_version"
  );

  let fixture = new_fixture();
  create(&fixture, bounds());
  rewrite_manifest_value(&fixture, |document| document["body"]["created_at_ms"] = serde_json::Value::from(7), false);
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_checksum"
  );

  let fixture = new_fixture();
  create(&fixture, bounds());
  rewrite_manifest_value(
    &fixture,
    |document| document.as_object_mut().unwrap().insert("unknown".to_string(), serde_json::Value::Bool(true)).map(drop).unwrap_or(()),
    false,
  );
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_decode"
  );

  let fixture = new_fixture();
  create(&fixture, bounds());
  rewrite_manifest_value(
    &fixture,
    |document| {
      document["body"].as_object_mut().unwrap().insert("unknown".to_string(), serde_json::Value::Bool(true));
    },
    true,
  );
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_decode"
  );

  let fixture = new_fixture();
  create(&fixture, bounds());
  rewrite_manifest_value(
    &fixture,
    |document| {
      let uppercase = document["body_blake3"].as_str().unwrap().to_ascii_uppercase();
      document["body_blake3"] = serde_json::Value::String(uppercase);
    },
    false,
  );
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_hex"
  );

  let fixture = new_fixture();
  create(&fixture, bounds());
  fs::write(fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME), vec![b'x'; 1024 * 1024 + 1]).unwrap();
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_size"
  );
}

#[test]
fn checksum_valid_semantic_tampering_is_rejected_by_field_capability_and_bound_validation() {
  let cases = vec![
    ("created_at_ms", "0".to_string(), "migration_run_manifest_fields"),
    ("holder_boot_id", format!("\"{}\"", "00".repeat(16)), "migration_run_manifest_fields"),
    ("hash_algorithm", "99".to_string(), "migration_run_manifest_hash"),
    ("maximum_memory_bytes", "0".to_string(), "migration_run_manifest_bounds"),
    ("roots", "0".to_string(), "migration_run_manifest_fields"),
    ("roots", "1001".to_string(), "migration_run_manifest_bounds"),
    ("modules", "9990".to_string(), "migration_run_manifest_bounds"),
    ("modules", "10001".to_string(), "migration_run_manifest_bounds"),
    ("required_reader_capabilities", format!("\"{}\"", "00".repeat(32)), "migration_run_manifest_fields"),
    ("required_writer_capabilities", format!("\"ff{}\"", "00".repeat(31)), "migration_run_manifest_fields"),
    ("supported_reader_capabilities", format!("\"ffffff01{}\"", "00".repeat(28)), "migration_run_manifest_capabilities"),
  ];

  for (field, replacement, expected_code) in cases {
    let fixture = new_fixture();
    create(&fixture, bounds());
    rewrite_manifest_text(&fixture, |document| replace_json_field_value(document, field, &replacement));
    let error = open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err();
    assert_eq!(error.code(), expected_code, "field {field}");
  }

  #[cfg(unix)]
  {
    let fixture = new_fixture();
    create(&fixture, bounds());
    rewrite_manifest_text(&fixture, |document| replace_json_field_value(document, "workspace", "\"relative-workspace\""));
    assert_eq!(
      open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
      "migration_run_manifest_paths"
    );
  }
}

#[test]
fn reopen_rejects_missing_empty_directory_and_symlink_manifest_paths() {
  let fixture = new_fixture();
  create(&fixture, bounds());
  let path = fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME);
  fs::remove_file(&path).unwrap();
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_open"
  );

  create(&fixture, bounds());
  fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(0).unwrap();
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_size"
  );

  fs::remove_file(&path).unwrap();
  fs::create_dir(&path).unwrap();
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_open"
  );

  #[cfg(unix)]
  {
    use std::os::unix::fs::symlink;
    fs::remove_dir(&path).unwrap();
    let target = fixture.workspace.join("manifest-target");
    fs::write(&target, b"not followed").unwrap();
    make_private(&target);
    symlink(&target, &path).unwrap();
    assert_eq!(
      open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
      "migration_run_manifest_open"
    );
  }
}

#[test]
fn reopen_revalidates_source_destination_parent_alias_and_permit() {
  let fixture = new_fixture();
  let created = create(&fixture, bounds());
  fs::write(&fixture.source, b"different source length after manifest creation").unwrap();
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_source_identity"
  );
  assert!(!fixture.destination.exists());
  drop(created);

  let fixture = new_fixture();
  let created = create(&fixture, bounds());
  let displaced_source = fixture.source.with_file_name("source-displaced.aeordb");
  fs::rename(&fixture.source, &displaced_source).unwrap();
  fs::write(&fixture.source, b"sealed disposable v3 source evidence").unwrap();
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_source_identity"
  );
  drop(created);

  let fixture = new_fixture();
  let created = create(&fixture, bounds());
  fs::write(&fixture.destination, b"ordinary destination progress").unwrap();
  assert_eq!(open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap(), created);

  let fixture = new_fixture();
  let created = create(&fixture, bounds());
  fs::hard_link(&fixture.source, &fixture.destination).unwrap();
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_destination_identity"
  );
  drop(created);

  let fixture = new_fixture();
  let created = create(&fixture, bounds());
  let destination_parent = fixture.destination.parent().unwrap().to_path_buf();
  let displaced_parent = destination_parent.with_file_name("destination-displaced");
  fs::rename(&destination_parent, &displaced_parent).unwrap();
  fs::create_dir(&destination_parent).unwrap();
  assert_eq!(
    open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
    "migration_run_manifest_destination_identity"
  );
  drop(created);

  let fixture = new_fixture();
  let created = create(&fixture, bounds());
  let observation = observe_migration_destination_path_v1(&fixture.destination).unwrap();
  let foreign_permit = permit_with_migration_id(&fixture.source, &observation, id(0x21));
  assert_eq!(created.validate_permit(&foreign_permit).unwrap_err().code(), "migration_run_manifest_permit");
}

#[test]
fn reopen_honors_cancellation_and_rejects_public_manifest_permissions() {
  let fixture = new_fixture();
  create(&fixture, bounds());
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  assert_eq!(open_migration_run_manifest_v1(&fixture.workspace, &cancellation).unwrap_err().code(), "migration_run_manifest_canceled");

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture.workspace.join(MIGRATION_RUN_MANIFEST_FILE_NAME);
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
      open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
      "migration_run_manifest_permissions"
    );

    fs::set_permissions(&fixture.workspace, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
      open_migration_run_manifest_v1(&fixture.workspace, &CancellationToken::new()).unwrap_err().code(),
      "migration_run_manifest_workspace"
    );
  }
}
