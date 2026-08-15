use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, encode_mutation_journal,
};
use aeordb::engine::v4::migration_capture::{
  MIGRATION_CAPTURE_FLAG_NEEDS_FULL_RECONCILE, MIGRATION_CAPTURE_FLAG_OPTIONAL_CAPTURE_STOPPED, MigrationCaptureManifestStateV1,
  MigrationCaptureManifestWriteV1,
};
use aeordb::engine::v4::migration_capture_workspace::{
  DurableMigrationCaptureWorkspaceV1, MigrationCaptureWorkspaceBasisV1, MigrationCaptureWorkspaceIdentityV1,
  MigrationCaptureWorkspaceOptionsV1, MigrationCaptureWorkspaceReopenOptionsV1, ReopenedMigrationCaptureWorkspaceV1,
};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

fn sequence<const N: usize>(start: u8) -> [u8; N] {
  std::array::from_fn(|offset| start.wrapping_add(u8::try_from(offset).unwrap()))
}

fn hash(algorithm: HashAlgorithm, start: u8) -> Vec<u8> {
  (0..algorithm.hash_length()).map(|offset| start.wrapping_add(u8::try_from(offset).unwrap())).collect()
}

fn memory_coordinator() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(128 * 1024 * 1024, 192 * 1024 * 1024, 1, 32 * 1024 * 1024).unwrap())
}

fn database_file(root: &Path) -> PathBuf {
  let path = root.join("source.aeordb");
  fs::write(&path, b"source bytes must remain invariant").unwrap();
  path
}

fn identity(algorithm: HashAlgorithm) -> MigrationCaptureWorkspaceIdentityV1 {
  MigrationCaptureWorkspaceIdentityV1::new(
    sequence::<16>(0x10),
    sequence::<16>(0x20),
    sequence::<16>(0x30),
    sequence::<16>(0x40),
    sequence::<16>(0x50),
    9,
    2,
    algorithm,
  )
  .unwrap()
}

fn basis(algorithm: HashAlgorithm) -> MigrationCaptureWorkspaceBasisV1 {
  MigrationCaptureWorkspaceBasisV1::new(
    1_700_000_000_000,
    99,
    hash(algorithm, 0x61),
    hash(algorithm, 0x90),
    hash(algorithm, 0xa0),
    sequence::<32>(0xb0),
  )
  .unwrap()
}

fn options(root: &Path, maximum_stored_bytes: u64) -> MigrationCaptureWorkspaceOptionsV1 {
  MigrationCaptureWorkspaceOptionsV1::new(Some(root.to_path_buf()), maximum_stored_bytes, 0).unwrap()
}

fn record<'a>(
  algorithm: HashAlgorithm,
  sequence: u64,
  root_before: &'a [u8],
  root_after: &'a [u8],
  mutation_id: &'a [u8],
  revision: &'a [u8],
) -> MutationRecordWriteV1<'a> {
  assert_eq!(mutation_id.len(), algorithm.hash_length());
  MutationRecordWriteV1 {
    kind: MutationKindV1::Update,
    sequence,
    mutation_id,
    batch_ordinal: 0,
    batch_count: 1,
    root_before,
    root_after,
    before: Some(MutationSideWriteV1 { path: "/workspace/a.json", revision }),
    after: Some(MutationSideWriteV1 { path: "/workspace/a.json", revision }),
    committed_at_ms: 1_700_000_000_000 + sequence,
  }
}

fn segment(
  algorithm: HashAlgorithm,
  ordinal: u64,
  publication_sequence: u64,
  root_before: &[u8],
  root_after: &[u8],
  previous_segment: &[u8],
) -> aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1 {
  segment_with_identity(
    algorithm,
    ordinal,
    publication_sequence,
    root_before,
    root_after,
    previous_segment,
    sequence::<16>(0x20),
    2,
    sequence::<16>(0x50),
  )
}

#[allow(clippy::too_many_arguments)]
fn segment_with_identity(
  algorithm: HashAlgorithm,
  ordinal: u64,
  publication_sequence: u64,
  root_before: &[u8],
  root_after: &[u8],
  previous_segment: &[u8],
  owner_id: [u8; 16],
  generation: u64,
  runtime_boot_id: [u8; 16],
) -> aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1 {
  let mutation_id = hash(algorithm, 0xc0u8.wrapping_add(u8::try_from(ordinal).unwrap()));
  let revision = hash(algorithm, 0xd0u8.wrapping_add(u8::try_from(ordinal).unwrap()));
  let record = record(algorithm, publication_sequence, root_before, root_after, &mutation_id, &revision);
  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: algorithm,
    owner_id,
    owner_kind: JournalOwnerKindV1::Task,
    generation,
    segment_ordinal: ordinal,
    chain_reset: ordinal == 1,
    previous_segment,
    semantic_state_root: root_after,
    runtime_boot_id,
    records: &[record],
  })
  .unwrap()
}

fn manifest(
  writer: &DurableMigrationCaptureWorkspaceV1,
  algorithm: HashAlgorithm,
  checkpoint_sequence: u64,
  previous_manifest: Vec<u8>,
) -> MigrationCaptureManifestWriteV1 {
  let summary = writer.summary();
  MigrationCaptureManifestWriteV1 {
    database_id: sequence::<16>(0x10),
    migration_id: sequence::<16>(0x20),
    source_physical_instance_id: sequence::<16>(0x30),
    destination_physical_instance_id: sequence::<16>(0x40),
    fencing_token: 9,
    capture_generation: 2,
    checkpoint_sequence,
    state: MigrationCaptureManifestStateV1::Capturing,
    flags: 0,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_001_000,
    captured_through_publication_sequence: summary.captured_through_publication_sequence(),
    observed_through_publication_sequence: summary.captured_through_publication_sequence(),
    first_segment_ordinal: summary.first_segment_ordinal(),
    last_segment_ordinal: summary.last_segment_ordinal(),
    segment_count: summary.segment_count(),
    segment_stored_bytes: summary.segment_stored_bytes(),
    source_root_before: summary.source_root_before().to_vec(),
    source_root_after: summary.source_root_after().to_vec(),
    segment_head: summary.segment_head().to_vec(),
    previous_manifest,
    effective_config_fingerprint: hash(algorithm, 0x90),
    system_family_registry_fingerprint: hash(algorithm, 0xa0),
    failure_evidence: vec![0; algorithm.hash_length()],
    source_authority_digest: sequence::<32>(0xb0),
  }
}

fn populated_writer(algorithm: HashAlgorithm, root: &Path) -> (PathBuf, MemoryCoordinator, DurableMigrationCaptureWorkspaceV1) {
  let database = database_file(root);
  let scratch = root.join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let mut writer = DurableMigrationCaptureWorkspaceV1::create(
    &database,
    identity(algorithm),
    basis(algorithm),
    options(&scratch, 64 * 1024 * 1024),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let root_a = hash(algorithm, 0x61);
  let root_b = hash(algorithm, 0x62);
  let root_c = hash(algorithm, 0x63);
  let first = segment(algorithm, 1, 100, &root_a, &root_b, &vec![0; algorithm.hash_length()]);
  writer.append_segment(&first.value).unwrap();
  let second = segment(algorithm, 2, 101, &root_b, &root_c, &first.key);
  writer.append_segment(&second.value).unwrap();
  (database, memory, writer)
}

#[test]
fn real_workspace_checkpoints_and_reopens_a_constant_memory_chain_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let directory = tempdir().unwrap();
    let (database, memory, mut writer) = populated_writer(algorithm, directory.path());
    let source_before = fs::read(&database).unwrap();
    let request = manifest(&writer, algorithm, 1, vec![0; algorithm.hash_length()]);
    let closure = writer.publish_checkpoint(&request).unwrap();
    assert_eq!(fs::read(&database).unwrap(), source_before);
    assert_eq!(closure.segment_count(), 2);
    assert_eq!(closure.checkpoint_sequence(), 1);

    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      assert_eq!(fs::metadata(closure.workspace_path()).unwrap().permissions().mode() & 0o777, 0o700);
      assert_eq!(fs::metadata(closure.manifest_path()).unwrap().permissions().mode() & 0o777, 0o600);
    }

    let workspace_path = closure.workspace_path().to_path_buf();
    let manifest_identity = closure.manifest_identity().to_vec();
    drop(writer);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().reserved_bytes, 0);
    let reopened = ReopenedMigrationCaptureWorkspaceV1::open(
      &workspace_path,
      1,
      &manifest_identity,
      identity(algorithm),
      basis(algorithm),
      MigrationCaptureWorkspaceReopenOptionsV1::new(64 * 1024 * 1024).unwrap(),
      CancellationToken::new(),
      &memory,
    )
    .unwrap();
    assert_eq!(reopened.segment_count(), 2);
    assert_eq!(reopened.captured_through_publication_sequence(), 101);
    let mut visited = Vec::new();
    reopened
      .for_each_segment(|journal| {
        visited.push((journal.segment_ordinal, journal.first_sequence, journal.last_sequence));
        Ok(())
      })
      .unwrap();
    assert_eq!(visited, vec![(1, 100, 100), (2, 101, 101)]);
    drop(reopened);
    let owner = memory.snapshot().unwrap().owner(MemoryOwner::Migration).unwrap().clone();
    assert_eq!(owner.reserved_bytes, 0);
    assert!(owner.peak_reserved_bytes < 20 * 1024 * 1024);
  }
}

#[test]
fn empty_capture_checkpoint_preserves_the_exact_starting_frontier() {
  let algorithm = HashAlgorithm::Blake3_256;
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let mut writer = DurableMigrationCaptureWorkspaceV1::create(
    &database,
    identity(algorithm),
    basis(algorithm),
    options(&scratch, 64 * 1024 * 1024),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let request = manifest(&writer, algorithm, 1, vec![0; algorithm.hash_length()]);
  let closure = writer.publish_checkpoint(&request).unwrap();
  assert_eq!(closure.segment_count(), 0);
  let workspace_path = closure.workspace_path().to_path_buf();
  let selected = closure.manifest_identity().to_vec();
  drop(writer);

  let reopened = ReopenedMigrationCaptureWorkspaceV1::open(
    &workspace_path,
    1,
    &selected,
    identity(algorithm),
    basis(algorithm),
    MigrationCaptureWorkspaceReopenOptionsV1::new(64 * 1024 * 1024).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert_eq!(reopened.segment_count(), 0);
  assert_eq!(reopened.captured_through_publication_sequence(), 99);
  reopened.for_each_segment(|_| panic!("empty capture cannot visit a segment")).unwrap();
}

#[test]
fn append_rejects_nonsequential_and_discontinuous_journals_before_creation() {
  let algorithm = HashAlgorithm::Blake3_256;
  let directory = tempdir().unwrap();
  let (_database, _memory, mut writer) = populated_writer(algorithm, directory.path());
  let next_path = writer.segment_path(3);
  let root_c = hash(algorithm, 0x63);
  let root_d = hash(algorithm, 0x64);

  let wrong_ordinal = segment(algorithm, 4, 102, &root_c, &root_d, writer.summary().segment_head());
  assert_eq!(writer.append_segment(&wrong_ordinal.value).unwrap_err().code(), "migration_capture_workspace_chain");
  assert!(!next_path.exists());

  let wrong_root = segment(algorithm, 3, 102, &hash(algorithm, 0x70), &root_d, writer.summary().segment_head());
  assert_eq!(writer.append_segment(&wrong_root.value).unwrap_err().code(), "migration_capture_workspace_chain");
  assert!(!next_path.exists());

  let wrong_previous = segment(algorithm, 3, 102, &root_c, &root_d, &hash(algorithm, 0x71));
  assert_eq!(writer.append_segment(&wrong_previous.value).unwrap_err().code(), "migration_capture_workspace_chain");
  assert!(!next_path.exists());

  for foreign in [
    segment_with_identity(
      algorithm,
      3,
      102,
      &root_c,
      &root_d,
      writer.summary().segment_head(),
      sequence::<16>(0x21),
      2,
      sequence::<16>(0x50),
    ),
    segment_with_identity(
      algorithm,
      3,
      102,
      &root_c,
      &root_d,
      writer.summary().segment_head(),
      sequence::<16>(0x20),
      3,
      sequence::<16>(0x50),
    ),
    segment_with_identity(
      algorithm,
      3,
      102,
      &root_c,
      &root_d,
      writer.summary().segment_head(),
      sequence::<16>(0x20),
      2,
      sequence::<16>(0x51),
    ),
  ] {
    assert_eq!(writer.append_segment(&foreign.value).unwrap_err().code(), "migration_capture_workspace_chain");
    assert!(!next_path.exists());
  }

  let root_e = hash(algorithm, 0x65);
  let mutation_a = hash(algorithm, 0xe1);
  let mutation_b = hash(algorithm, 0xe2);
  let revision_a = hash(algorithm, 0xf1);
  let revision_b = hash(algorithm, 0xf2);
  let first_record = record(algorithm, 102, &root_c, &root_d, &mutation_a, &revision_a);
  let second_record = record(algorithm, 104, &root_d, &root_e, &mutation_b, &revision_b);
  let gapped = encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: algorithm,
    owner_id: sequence::<16>(0x20),
    owner_kind: JournalOwnerKindV1::Task,
    generation: 2,
    segment_ordinal: 3,
    chain_reset: false,
    previous_segment: writer.summary().segment_head(),
    semantic_state_root: &root_e,
    runtime_boot_id: sequence::<16>(0x50),
    records: &[first_record, second_record],
  })
  .unwrap();
  assert_eq!(writer.append_segment(&gapped.value).unwrap_err().code(), "migration_capture_workspace_chain");
  assert!(!next_path.exists());

  let disconnected_root = hash(algorithm, 0x72);
  let second_record = record(algorithm, 103, &disconnected_root, &root_e, &mutation_b, &revision_b);
  let disconnected = encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: algorithm,
    owner_id: sequence::<16>(0x20),
    owner_kind: JournalOwnerKindV1::Task,
    generation: 2,
    segment_ordinal: 3,
    chain_reset: false,
    previous_segment: writer.summary().segment_head(),
    semantic_state_root: &root_e,
    runtime_boot_id: sequence::<16>(0x50),
    records: &[first_record, second_record],
  })
  .unwrap();
  assert_eq!(writer.append_segment(&disconnected.value).unwrap_err().code(), "migration_capture_workspace_chain");
  assert!(!next_path.exists());
}

#[test]
fn checkpoint_refuses_identity_summary_and_predecessor_mismatches() {
  let algorithm = HashAlgorithm::Blake3_256;
  let directory = tempdir().unwrap();
  let (_database, _memory, mut writer) = populated_writer(algorithm, directory.path());

  let mut request = manifest(&writer, algorithm, 1, vec![0; algorithm.hash_length()]);
  request.segment_count += 1;
  assert_eq!(writer.publish_checkpoint(&request).unwrap_err().code(), "migration_capture_workspace_checkpoint");

  let first = manifest(&writer, algorithm, 1, vec![0; algorithm.hash_length()]);
  let first_closure = writer.publish_checkpoint(&first).unwrap();
  let wrong_predecessor = manifest(&writer, algorithm, 2, hash(algorithm, 0xe0));
  assert_eq!(writer.publish_checkpoint(&wrong_predecessor).unwrap_err().code(), "migration_capture_workspace_checkpoint");
  let second = manifest(&writer, algorithm, 2, first_closure.manifest_identity().to_vec());
  let second_closure = writer.publish_checkpoint(&second).unwrap();
  assert_eq!(second_closure.checkpoint_sequence(), 2);

  let mut latched = manifest(&writer, algorithm, 3, second_closure.manifest_identity().to_vec());
  latched.state = MigrationCaptureManifestStateV1::NeedsFullReconcile;
  latched.flags = MIGRATION_CAPTURE_FLAG_NEEDS_FULL_RECONCILE | MIGRATION_CAPTURE_FLAG_OPTIONAL_CAPTURE_STOPPED;
  latched.observed_through_publication_sequence += 5;
  latched.failure_evidence = hash(algorithm, 0xe1);
  assert_eq!(writer.publish_checkpoint(&latched).unwrap().checkpoint_sequence(), 3);
}

#[test]
fn reopen_validates_and_accounts_for_the_complete_checkpoint_predecessor_chain() {
  let algorithm = HashAlgorithm::Blake3_256;
  let directory = tempdir().unwrap();
  let (_database, memory, mut writer) = populated_writer(algorithm, directory.path());
  let first_request = manifest(&writer, algorithm, 1, vec![0; algorithm.hash_length()]);
  let first = writer.publish_checkpoint(&first_request).unwrap();
  let second_request = manifest(&writer, algorithm, 2, first.manifest_identity().to_vec());
  let second = writer.publish_checkpoint(&second_request).unwrap();
  let workspace_path = second.workspace_path().to_path_buf();
  let selected = second.manifest_identity().to_vec();
  drop(writer);

  assert_eq!(
    ReopenedMigrationCaptureWorkspaceV1::open(
      &workspace_path,
      2,
      &selected,
      identity(algorithm),
      basis(algorithm),
      MigrationCaptureWorkspaceReopenOptionsV1::new(second.stored_bytes() - 1).unwrap(),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "migration_capture_workspace_capacity"
  );

  let first_manifest = first.manifest_path().to_path_buf();
  let mut corrupted = fs::read(&first_manifest).unwrap();
  corrupted[32] ^= 1;
  fs::write(&first_manifest, corrupted).unwrap();
  assert_eq!(
    ReopenedMigrationCaptureWorkspaceV1::open(
      &workspace_path,
      2,
      &selected,
      identity(algorithm),
      basis(algorithm),
      MigrationCaptureWorkspaceReopenOptionsV1::new(64 * 1024 * 1024).unwrap(),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "migration_capture_workspace_checkpoint"
  );
}

#[test]
fn cancellation_capacity_and_existing_paths_fail_closed_without_touching_source() {
  let algorithm = HashAlgorithm::Blake3_256;
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let source_before = fs::read(&database).unwrap();
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();

  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    DurableMigrationCaptureWorkspaceV1::create(
      &database,
      identity(algorithm),
      basis(algorithm),
      options(&scratch, 64 * 1024 * 1024),
      canceled,
      &memory,
    )
    .unwrap_err()
    .code(),
    "migration_capture_workspace_cancelled"
  );
  assert_eq!(fs::read(&database).unwrap(), source_before);

  let mut writer = DurableMigrationCaptureWorkspaceV1::create(
    &database,
    identity(algorithm),
    basis(algorithm),
    options(&scratch, 1),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let root_a = hash(algorithm, 0x61);
  let root_b = hash(algorithm, 0x62);
  let first = segment(algorithm, 1, 100, &root_a, &root_b, &vec![0; algorithm.hash_length()]);
  assert_eq!(writer.append_segment(&first.value).unwrap_err().code(), "migration_capture_workspace_capacity");
  assert_eq!(fs::read(&database).unwrap(), source_before);

  assert_eq!(
    DurableMigrationCaptureWorkspaceV1::create(
      &database,
      identity(algorithm),
      basis(algorithm),
      options(&scratch, 64 * 1024 * 1024),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "migration_capture_workspace_path"
  );
}

#[test]
fn cancellation_after_creation_refuses_segment_and_checkpoint_publication() {
  let algorithm = HashAlgorithm::Blake3_256;
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let cancellation = CancellationToken::new();
  let mut writer = DurableMigrationCaptureWorkspaceV1::create(
    &database,
    identity(algorithm),
    basis(algorithm),
    options(&scratch, 64 * 1024 * 1024),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  cancellation.cancel();
  let root_a = hash(algorithm, 0x61);
  let root_b = hash(algorithm, 0x62);
  let first = segment(algorithm, 1, 100, &root_a, &root_b, &vec![0; algorithm.hash_length()]);
  assert_eq!(writer.append_segment(&first.value).unwrap_err().code(), "migration_capture_workspace_cancelled");
  assert!(!writer.segment_path(1).exists());
  let request = manifest(&writer, algorithm, 1, vec![0; algorithm.hash_length()]);
  assert_eq!(writer.publish_checkpoint(&request).unwrap_err().code(), "migration_capture_workspace_cancelled");
}

#[test]
fn immutable_segment_collision_never_clobbers_existing_bytes_and_latches_the_writer() {
  let algorithm = HashAlgorithm::Blake3_256;
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let source_before = fs::read(&database).unwrap();
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory_coordinator();
  let mut writer = DurableMigrationCaptureWorkspaceV1::create(
    &database,
    identity(algorithm),
    basis(algorithm),
    options(&scratch, 64 * 1024 * 1024),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let segment_path = writer.segment_path(1);
  fs::write(&segment_path, b"preexisting crash prefix").unwrap();
  let root_a = hash(algorithm, 0x61);
  let root_b = hash(algorithm, 0x62);
  let first = segment(algorithm, 1, 100, &root_a, &root_b, &vec![0; algorithm.hash_length()]);
  assert_eq!(writer.append_segment(&first.value).unwrap_err().code(), "migration_capture_workspace_durability");
  assert_eq!(fs::read(&segment_path).unwrap(), b"preexisting crash prefix");
  assert_eq!(fs::read(&database).unwrap(), source_before);
  assert_eq!(writer.append_segment(&first.value).unwrap_err().code(), "migration_capture_workspace_state");
}

#[test]
fn reopen_rejects_wrong_identity_truncation_and_memory_pressure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let directory = tempdir().unwrap();
  let (_database, memory, mut writer) = populated_writer(algorithm, directory.path());
  let request = manifest(&writer, algorithm, 1, vec![0; algorithm.hash_length()]);
  let closure = writer.publish_checkpoint(&request).unwrap();
  let workspace_path = closure.workspace_path().to_path_buf();
  let manifest_identity = closure.manifest_identity().to_vec();
  let segment_path = writer.segment_path(1);
  drop(writer);

  let canceled = CancellationToken::new();
  canceled.cancel();
  assert_eq!(
    ReopenedMigrationCaptureWorkspaceV1::open(
      &workspace_path,
      1,
      &manifest_identity,
      identity(algorithm),
      basis(algorithm),
      MigrationCaptureWorkspaceReopenOptionsV1::new(64 * 1024 * 1024).unwrap(),
      canceled,
      &memory,
    )
    .unwrap_err()
    .code(),
    "migration_capture_workspace_cancelled"
  );

  let wrong_identity = hash(algorithm, 0xf0);
  assert_eq!(
    ReopenedMigrationCaptureWorkspaceV1::open(
      &workspace_path,
      1,
      &wrong_identity,
      identity(algorithm),
      basis(algorithm),
      MigrationCaptureWorkspaceReopenOptionsV1::new(64 * 1024 * 1024).unwrap(),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "migration_capture_workspace_checkpoint"
  );

  let original = fs::read(&segment_path).unwrap();
  fs::write(&segment_path, &original[..original.len() - 1]).unwrap();
  assert_eq!(
    ReopenedMigrationCaptureWorkspaceV1::open(
      &workspace_path,
      1,
      &manifest_identity,
      identity(algorithm),
      basis(algorithm),
      MigrationCaptureWorkspaceReopenOptionsV1::new(64 * 1024 * 1024).unwrap(),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "migration_capture_workspace_format"
  );
  fs::write(&segment_path, &original).unwrap();

  let constrained = MemoryCoordinator::new(MemoryPolicy::new(1, 2, 1, 1).unwrap());
  assert_eq!(
    ReopenedMigrationCaptureWorkspaceV1::open(
      &workspace_path,
      1,
      &manifest_identity,
      identity(algorithm),
      basis(algorithm),
      MigrationCaptureWorkspaceReopenOptionsV1::new(64 * 1024 * 1024).unwrap(),
      CancellationToken::new(),
      &constrained,
    )
    .unwrap_err()
    .code(),
    "migration_capture_workspace_memory"
  );
}

#[cfg(unix)]
#[test]
fn reopen_accounts_for_private_crash_prefixes_and_rejects_unknown_entries() {
  use std::io::Write as _;
  use std::os::unix::fs::OpenOptionsExt;

  let algorithm = HashAlgorithm::Blake3_256;
  let directory = tempdir().unwrap();
  let (_database, memory, mut writer) = populated_writer(algorithm, directory.path());
  let request = manifest(&writer, algorithm, 1, vec![0; algorithm.hash_length()]);
  let closure = writer.publish_checkpoint(&request).unwrap();
  let workspace_path = closure.workspace_path().to_path_buf();
  let manifest_identity = closure.manifest_identity().to_vec();
  let clean_stored_bytes = closure.stored_bytes();
  drop(writer);

  let crash_prefix = workspace_path.join("segments/.capture-00000000-0000-4000-8000-000000000001.pending");
  let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&crash_prefix).unwrap();
  file.write_all(&[0x5a; 4_096]).unwrap();
  file.sync_all().unwrap();
  drop(file);
  assert_eq!(
    ReopenedMigrationCaptureWorkspaceV1::open(
      &workspace_path,
      1,
      &manifest_identity,
      identity(algorithm),
      basis(algorithm),
      MigrationCaptureWorkspaceReopenOptionsV1::new(clean_stored_bytes + 4_095).unwrap(),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "migration_capture_workspace_capacity"
  );

  fs::rename(&crash_prefix, workspace_path.join("segments/not-a-capture-artifact")).unwrap();
  assert_eq!(
    ReopenedMigrationCaptureWorkspaceV1::open(
      &workspace_path,
      1,
      &manifest_identity,
      identity(algorithm),
      basis(algorithm),
      MigrationCaptureWorkspaceReopenOptionsV1::new(64 * 1024 * 1024).unwrap(),
      CancellationToken::new(),
      &memory,
    )
    .unwrap_err()
    .code(),
    "migration_capture_workspace_path"
  );
}

#[test]
fn capture_workspace_remains_disconnected_from_live_service_and_source_authority() {
  let package = Path::new(env!("CARGO_MANIFEST_DIR"));
  let source = fs::read_to_string(package.join("src/engine/v4/migration_capture_workspace.rs")).unwrap();
  for forbidden in ["StorageEngine", "DirectoryOps", "server::", "axum", "task_worker", "update_head(", "publish_namespace_root"] {
    assert!(!source.contains(forbidden), "capture workspace acquired premature live authority {forbidden}");
  }
  assert!(source.contains("validate_regular_database_path"));
  assert!(source.contains("MemoryOwner::Migration"));
  assert!(source.contains("durable_install_new_native"));
}
