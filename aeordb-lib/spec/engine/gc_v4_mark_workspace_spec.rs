use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::gc_mark::{
  MarkWorkspaceObjectKindV1, decode_mark_workspace_manifest, decode_mark_workspace_object, validate_mark_workspace_object,
};
use aeordb::engine::v4::gc_mark_workspace::{DurableMarkWorkspaceV1, MarkWorkspaceBasisV1, MarkWorkspaceIdentityV1, MarkWorkspaceOptionsV1};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn sequence<const N: usize>(start: u8) -> [u8; N] {
  let mut bytes = [0u8; N];
  for (index, byte) in bytes.iter_mut().enumerate() {
    *byte = start.wrapping_add(u8::try_from(index).unwrap());
  }
  bytes
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap())
}

fn identity(algorithm: HashAlgorithm) -> MarkWorkspaceIdentityV1 {
  MarkWorkspaceIdentityV1::new(sequence::<16>(0x31), sequence::<16>(0x51), 77, 7, algorithm).unwrap()
}

fn basis(algorithm: HashAlgorithm) -> MarkWorkspaceBasisV1 {
  MarkWorkspaceBasisV1::new(
    1,
    1_700_000_100_000,
    1_700_000_100_500,
    sequence_vec(0x51, algorithm.hash_length()),
    sequence_vec(0x11, algorithm.hash_length()),
    sequence::<32>(0x71),
  )
  .unwrap()
}

fn sequence_vec(start: u8, length: usize) -> Vec<u8> {
  (0..length).map(|index| start.wrapping_add(u8::try_from(index).unwrap())).collect()
}

fn options(root: &Path) -> MarkWorkspaceOptionsV1 {
  MarkWorkspaceOptionsV1::new(Some(root.to_path_buf()), 64 * 1024 * 1024, 0).unwrap()
}

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4")
}

fn fixture_label(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("workspace fixture test uses only the frozen 32/64-bit profiles"),
  }
}

fn object_fixture(algorithm: HashAlgorithm, kind: MarkWorkspaceObjectKindV1) -> Vec<u8> {
  fs::read(fixture_root().join("gc-mark-workspace-object-v1").join(format!("agwo-{}-{}-valid.bin", fixture_label(algorithm), kind.name())))
    .unwrap()
}

fn manifest_fixture(algorithm: HashAlgorithm, empty: bool) -> Vec<u8> {
  let suffix = if empty { "-empty" } else { "" };
  fs::read(
    fixture_root()
      .join("gc-mark-workspace-manifest-v1")
      .join(format!("agcw-{}-mark-workspace-manifest{suffix}.bin", fixture_label(algorithm))),
  )
  .unwrap()
}

fn database_file(root: &Path, name: &str) -> PathBuf {
  let path = root.join(name);
  fs::write(&path, b"database identity placeholder").unwrap();
  path
}

#[test]
fn writer_matches_every_independent_agwo_and_agcw_fixture_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let directory = tempdir().unwrap();
    let database = database_file(directory.path(), "taraani.aeordb");
    let scratch = directory.path().join(format!("scratch-{}", fixture_label(algorithm)));
    fs::create_dir(&scratch).unwrap();
    let memory = memory_coordinator();
    let mut writer = DurableMarkWorkspaceV1::create(
      &database,
      identity(algorithm),
      basis(algorithm),
      options(&scratch),
      CancellationToken::new(),
      &memory,
    )
    .unwrap();

    for (index, kind) in MarkWorkspaceObjectKindV1::ALL.into_iter().enumerate() {
      let ordinal = u64::try_from(index).unwrap() + 1;
      let fixture = object_fixture(algorithm, kind);
      let descriptor_digest = writer.write_object(kind, ordinal, &fixture[80..fixture.len() - 4]).unwrap().digest();
      let object_path = writer.object_path(kind, ordinal);
      let stored = fs::read(&object_path).unwrap();
      assert_eq!(stored, fixture);
      assert_eq!(descriptor_digest, *blake3::hash(&stored).as_bytes());
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&object_path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(object_path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
      }
    }

    let closure = writer.complete().unwrap();
    assert_eq!(closure.workspace_path(), writer.workspace_path());
    let manifest_bytes = fs::read(closure.manifest_path()).unwrap();
    assert_eq!(manifest_bytes, manifest_fixture(algorithm, false));
    assert_eq!(closure.manifest_digest(), *blake3::hash(&manifest_bytes).as_bytes());
    assert_eq!(closure.object_count(), 6);
    assert!(closure.logical_record_count() > 0);
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(fs::metadata(closure.checkpoint_directory()).unwrap().permissions().mode() & 0o777, 0o700);
      assert_eq!(fs::metadata(closure.manifest_path()).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let manifest = decode_mark_workspace_manifest(&manifest_bytes, algorithm).unwrap();
    for descriptor in &manifest.descriptors {
      let object_bytes = fs::read(closure.checkpoint_directory().join(descriptor.name)).unwrap();
      let object = decode_mark_workspace_object(&object_bytes, algorithm).unwrap();
      validate_mark_workspace_object(&manifest, descriptor, &object, &object_bytes).unwrap();
    }

    assert!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes > 0);
    drop(writer);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::GarbageCollection).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn empty_checkpoint_manifest_matches_the_independent_fixture_and_is_idempotent() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let directory = tempdir().unwrap();
    let database = database_file(directory.path(), "empty.aeordb");
    let scratch = directory.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let memory = memory_coordinator();
    let mut writer = DurableMarkWorkspaceV1::create(
      &database,
      identity(algorithm),
      basis(algorithm),
      options(&scratch),
      CancellationToken::new(),
      &memory,
    )
    .unwrap();

    let first = writer.complete().unwrap();
    let second = writer.complete().unwrap();
    assert_eq!(first, second);
    assert_eq!(fs::read(first.manifest_path()).unwrap(), manifest_fixture(algorithm, true));
    assert_eq!(first.object_count(), 0);
    assert_eq!(first.object_stored_bytes(), 0);
  }
}

#[test]
fn identity_paths_permissions_and_preexisting_workspaces_fail_closed() {
  assert_eq!(
    MarkWorkspaceIdentityV1::new([0; 16], sequence::<16>(0x51), 77, 7, HashAlgorithm::Blake3_256).unwrap_err().code(),
    "mark_workspace_identity"
  );
  assert_eq!(MarkWorkspaceBasisV1::new(0, 1, 2, vec![1; 32], vec![2; 32], [3; 32]).unwrap_err().code(), "mark_workspace_identity");

  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "private.aeordb");
  let memory = memory_coordinator();
  let canceled = CancellationToken::new();
  canceled.cancel();
  let error = DurableMarkWorkspaceV1::create(
    &database,
    identity(HashAlgorithm::Blake3_256),
    basis(HashAlgorithm::Blake3_256),
    MarkWorkspaceOptionsV1::new(None, 64 * 1024 * 1024, 0).unwrap(),
    canceled,
    &memory,
  )
  .unwrap_err();
  assert_eq!(error.code(), "mark_workspace_cancelled");
  assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);

  let writer = DurableMarkWorkspaceV1::create(
    &database,
    identity(HashAlgorithm::Blake3_256),
    basis(HashAlgorithm::Blake3_256),
    MarkWorkspaceOptionsV1::new(None, 64 * 1024 * 1024, 0).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert_eq!(
    writer.workspace_path().file_name().unwrap().to_str().unwrap(),
    ".private.aeordb-gc-3132333435363738393a3b3c3d3e3f40-5152535455565758595a5b5c5d5e5f60"
  );
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(writer.workspace_path()).unwrap().permissions().mode() & 0o777, 0o700);
  }
  drop(writer);

  let collision = DurableMarkWorkspaceV1::create(
    &database,
    identity(HashAlgorithm::Blake3_256),
    basis(HashAlgorithm::Blake3_256),
    MarkWorkspaceOptionsV1::new(None, 64 * 1024 * 1024, 0).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap_err();
  assert_eq!(collision.code(), "mark_workspace_path");
}

#[cfg(unix)]
#[test]
fn no_follow_paths_never_replace_a_symlink_or_crash_prefix() {
  use std::os::unix::fs::symlink;

  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "nofollow.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let algorithm = HashAlgorithm::Blake3_256;
  let mut writer =
    DurableMarkWorkspaceV1::create(&database, identity(algorithm), basis(algorithm), options(&scratch), CancellationToken::new(), &memory)
      .unwrap();
  let fixture = object_fixture(algorithm, MarkWorkspaceObjectKindV1::Bitmap);
  let body = &fixture[80..fixture.len() - 4];
  writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, body).unwrap();

  let protected = directory.path().join("protected");
  fs::write(&protected, b"do not replace").unwrap();
  let symlink_path = writer.object_path(MarkWorkspaceObjectKindV1::Bitmap, 2);
  symlink(&protected, &symlink_path).unwrap();
  assert_eq!(writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 2, body).unwrap_err().code(), "mark_workspace_path");
  assert_eq!(fs::read(&protected).unwrap(), b"do not replace");
  assert!(writer.is_failed());
  assert!(!writer.manifest_path().exists());

  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "prefix.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let mut writer =
    DurableMarkWorkspaceV1::create(&database, identity(algorithm), basis(algorithm), options(&scratch), CancellationToken::new(), &memory)
      .unwrap();
  writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, body).unwrap();
  let prefix_path = writer.object_path(MarkWorkspaceObjectKindV1::Bitmap, 2);
  fs::write(&prefix_path, b"AGWO crash prefix").unwrap();
  assert_eq!(writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 2, body).unwrap_err().code(), "mark_workspace_path");
  assert_eq!(fs::read(prefix_path).unwrap(), b"AGWO crash prefix");
  assert!(!writer.manifest_path().exists());
}

#[test]
fn malformed_capacity_pressure_and_cancellation_refuse_before_object_publication() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "refuse.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory_coordinator();
  let mut writer =
    DurableMarkWorkspaceV1::create(&database, identity(algorithm), basis(algorithm), options(&scratch), CancellationToken::new(), &memory)
      .unwrap();
  assert_eq!(writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, b"bad").unwrap_err().code(), "mark_workspace_format");
  assert!(!writer.object_path(MarkWorkspaceObjectKindV1::Bitmap, 1).exists());
  assert_eq!(writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 0, &[0; 32]).unwrap_err().code(), "mark_workspace_identity");

  let fixture = object_fixture(algorithm, MarkWorkspaceObjectKindV1::Bitmap);
  let body = &fixture[80..fixture.len() - 4];
  writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, body).unwrap();
  let original = fs::read(writer.object_path(MarkWorkspaceObjectKindV1::Bitmap, 1)).unwrap();
  assert_eq!(writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, body).unwrap_err().code(), "mark_workspace_state");
  assert_eq!(fs::read(writer.object_path(MarkWorkspaceObjectKindV1::Bitmap, 1)).unwrap(), original);

  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "capacity.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let mut capped = DurableMarkWorkspaceV1::create(
    &database,
    identity(algorithm),
    basis(algorithm),
    MarkWorkspaceOptionsV1::new(Some(scratch), 100, 0).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert_eq!(capped.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, body).unwrap_err().code(), "mark_workspace_capacity");
  assert!(!capped.object_path(MarkWorkspaceObjectKindV1::Bitmap, 1).exists());

  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "reserve.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let reserve_error = DurableMarkWorkspaceV1::create(
    &database,
    identity(algorithm),
    basis(algorithm),
    MarkWorkspaceOptionsV1::new(Some(scratch), 64 * 1024 * 1024, u64::MAX).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap_err();
  assert_eq!(reserve_error.code(), "mark_workspace_capacity");

  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "memory.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let constrained = MemoryCoordinator::new(MemoryPolicy::new(1, 2, 1, 1).unwrap());
  let mut pressured = DurableMarkWorkspaceV1::create(
    &database,
    identity(algorithm),
    basis(algorithm),
    options(&scratch),
    CancellationToken::new(),
    &constrained,
  )
  .unwrap();
  assert_eq!(pressured.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, body).unwrap_err().code(), "mark_workspace_memory");
  assert!(!pressured.object_path(MarkWorkspaceObjectKindV1::Bitmap, 1).exists());

  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "cancel.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let cancellation = CancellationToken::new();
  let mut canceled =
    DurableMarkWorkspaceV1::create(&database, identity(algorithm), basis(algorithm), options(&scratch), cancellation.clone(), &memory)
      .unwrap();
  cancellation.cancel();
  assert_eq!(canceled.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, body).unwrap_err().code(), "mark_workspace_cancelled");
  assert!(!canceled.object_path(MarkWorkspaceObjectKindV1::Bitmap, 1).exists());
}

#[test]
fn manifest_replace_failure_latches_without_selecting_an_incomplete_closure() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "manifest-failure.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory_coordinator();
  let mut writer =
    DurableMarkWorkspaceV1::create(&database, identity(algorithm), basis(algorithm), options(&scratch), CancellationToken::new(), &memory)
      .unwrap();
  let fixture = object_fixture(algorithm, MarkWorkspaceObjectKindV1::Bitmap);
  writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, &fixture[80..fixture.len() - 4]).unwrap();
  fs::create_dir(writer.manifest_path()).unwrap();

  let error = writer.complete().unwrap_err();
  assert_eq!(error.code(), "mark_workspace_durability");
  assert!(writer.is_failed());
  assert!(writer.manifest_path().is_dir());
  assert!(writer.object_path(MarkWorkspaceObjectKindV1::Bitmap, 1).is_file());
  assert_eq!(writer.complete().unwrap_err().code(), "mark_workspace_state");
}

#[test]
fn manifest_install_never_clobbers_an_existing_file_or_publishes_tampered_objects() {
  let algorithm = HashAlgorithm::Blake3_256;
  let memory = memory_coordinator();
  let fixture = object_fixture(algorithm, MarkWorkspaceObjectKindV1::Bitmap);
  let body = &fixture[80..fixture.len() - 4];

  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "manifest-collision.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let mut collision_writer =
    DurableMarkWorkspaceV1::create(&database, identity(algorithm), basis(algorithm), options(&scratch), CancellationToken::new(), &memory)
      .unwrap();
  collision_writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, body).unwrap();
  fs::write(collision_writer.manifest_path(), b"preexisting immutable closure").unwrap();

  assert_eq!(collision_writer.complete().unwrap_err().code(), "mark_workspace_durability");
  assert_eq!(fs::read(collision_writer.manifest_path()).unwrap(), b"preexisting immutable closure");
  assert!(collision_writer.is_failed());

  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "tampered-object.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let mut tampered_writer =
    DurableMarkWorkspaceV1::create(&database, identity(algorithm), basis(algorithm), options(&scratch), CancellationToken::new(), &memory)
      .unwrap();
  tampered_writer.write_object(MarkWorkspaceObjectKindV1::Bitmap, 1, body).unwrap();
  let object_path = tampered_writer.object_path(MarkWorkspaceObjectKindV1::Bitmap, 1);
  let mut stored = fs::read(&object_path).unwrap();
  stored[80] ^= 1;
  fs::write(&object_path, stored).unwrap();

  assert_eq!(tampered_writer.complete().unwrap_err().code(), "mark_workspace_format");
  assert!(!tampered_writer.manifest_path().exists());
  assert!(tampered_writer.is_failed());
}

#[cfg(unix)]
#[test]
fn preexisting_owned_workspace_directories_must_remain_private() {
  use std::os::unix::fs::PermissionsExt;

  let directory = tempdir().unwrap();
  let database = database_file(directory.path(), "public-owned-directory.aeordb");
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let database_directory = scratch.join("3132333435363738393a3b3c3d3e3f40");
  fs::create_dir(&database_directory).unwrap();
  fs::set_permissions(&database_directory, fs::Permissions::from_mode(0o755)).unwrap();

  let error = DurableMarkWorkspaceV1::create(
    &database,
    identity(HashAlgorithm::Blake3_256),
    basis(HashAlgorithm::Blake3_256),
    options(&scratch),
    CancellationToken::new(),
    &memory_coordinator(),
  )
  .unwrap_err();
  assert_eq!(error.code(), "mark_workspace_path");
  assert!(error.to_string().contains("private"));
}

fn rust_sources(root: &Path, sources: &mut Vec<PathBuf>) {
  for entry in fs::read_dir(root).unwrap() {
    let path = entry.unwrap().path();
    if path.is_dir() {
      rust_sources(&path, sources);
    } else if path.extension().is_some_and(|extension| extension == "rs") {
      sources.push(path);
    }
  }
}

#[test]
fn workspace_writer_remains_disconnected_from_live_gc_service_and_selection_paths() {
  let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
  let owner_path = source_root.join("engine/v4/gc_mark_workspace.rs");
  let mut sources = Vec::new();
  rust_sources(&source_root, &mut sources);
  let callers: Vec<_> = sources
    .into_iter()
    .filter(|path| path != &owner_path)
    .filter(|path| {
      let source = fs::read_to_string(path).unwrap_or_default();
      source.contains("DurableMarkWorkspaceV1::") || source.contains("gc_mark_workspace::DurableMarkWorkspaceV1")
    })
    .map(|path| path.strip_prefix(&source_root).unwrap().to_owned())
    .collect();
  assert!(callers.is_empty(), "P4-3 workspace writer activated before checkpoint authority: {callers:?}");

  let source = fs::read_to_string(owner_path).unwrap();
  for forbidden in ["engine::gc", "V4ControlStore", "FirstAuthority", "publish", "VoidManager", "candidate", "sweep", "authorizes_reclaim"]
  {
    assert!(!source.contains(forbidden), "workspace writer contains forbidden authority token {forbidden}");
  }
}
