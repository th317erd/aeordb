use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coordinator::{IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1, IndexMutationRequestV1};
use aeordb::engine::v4::index_page::OrderedIndexRoleV1;
use aeordb::engine::v4::index_producer_coordinator::{IndexProducerTaskKindV1, IndexProducerTaskRequestV1};
use aeordb::engine::v4::index_record::{ScopeReverseRecordV1, encode_scope_reverse_record};
use aeordb::engine::v4::index_runtime_workspace::{decode_index_workspace_object_v1, decode_index_workspace_producer_task_payload_v1};
use aeordb::engine::v4::index_runtime_workspace_store::{
  DurableIndexRuntimeWorkspaceV1, IndexRuntimeWorkspaceHeadV1, IndexRuntimeWorkspaceIdentityV1, IndexRuntimeWorkspaceOptionsV1,
  IndexRuntimeWorkspaceReopenOptionsV1, IndexRuntimeWorkspaceSelectedHeadV1, ReopenedIndexRuntimeWorkspaceV1,
};
use tempfile::{TempDir, tempdir};
use tokio_util::sync::CancellationToken;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn memory(limit: u64) -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(limit, limit + 1024 * 1024, 1, 1024 * 1024).unwrap())
}

fn database_file(root: &Path) -> PathBuf {
  let path = root.join("source.aeordb");
  fs::write(&path, b"source database remains unchanged").unwrap();
  path
}

fn identity() -> IndexRuntimeWorkspaceIdentityV1 {
  identity_for(ALGORITHM)
}

fn identity_for(algorithm: HashAlgorithm) -> IndexRuntimeWorkspaceIdentityV1 {
  IndexRuntimeWorkspaceIdentityV1::new([0x11; 16], [0x22; 16], [0x33; 16], [0x44; 16], algorithm).unwrap()
}

fn options(scratch: PathBuf, maximum_stored_bytes: u64, maximum_object_count: u64) -> IndexRuntimeWorkspaceOptionsV1 {
  IndexRuntimeWorkspaceOptionsV1::new(Some(scratch), maximum_stored_bytes, 0, maximum_object_count).unwrap()
}

fn manifest_path(workspace: &Path, sequence: u64) -> PathBuf {
  workspace.join("manifests").join(format!("{sequence:016x}.aiwm"))
}

fn object_path(workspace: &Path, object_id: [u8; 16]) -> PathBuf {
  workspace.join("objects/runtime").join(format!("{}.aiwo", hex::encode(object_id)))
}

fn producer_object_path(workspace: &Path, object_id: [u8; 16]) -> PathBuf {
  workspace.join("objects/tasks").join(format!("{}.aiwo", hex::encode(object_id)))
}

fn producer_task<'a>(
  operation_id: [u8; 16],
  publication_sequence: u64,
  root: &'a [u8],
  semantic: &'a [u8],
) -> IndexProducerTaskRequestV1<'a> {
  IndexProducerTaskRequestV1 {
    operation_id,
    kind: IndexProducerTaskKindV1::Rebuild,
    publication_sequence,
    namespace_root_before: root,
    namespace_root_after: root,
    semantic_state_root: semantic,
    journal_head: None,
    scope: Some("/docs"),
  }
}

fn batch(memory: &MemoryCoordinator) -> (IndexCoordinatorV1, aeordb::engine::v4::index_coordinator::FrozenIndexBatchV1) {
  batch_for(memory, ALGORITHM)
}

fn batch_for(
  memory: &MemoryCoordinator,
  algorithm: HashAlgorithm,
) -> (IndexCoordinatorV1, aeordb::engine::v4::index_coordinator::FrozenIndexBatchV1) {
  let options = IndexCoordinatorOptionsV1::new(1024 * 1024, 8, 1_000, 1024 * 1024).unwrap();
  let mut coordinator = IndexCoordinatorV1::new([0x77; 16], algorithm, memory.clone(), options, 1_000).unwrap();
  for ordinal in 1..=2u64 {
    let index_id = digest_parts(algorithm, &[b"index", &ordinal.to_le_bytes()]);
    let file_key = digest_parts(algorithm, &[b"file", &ordinal.to_le_bytes()]);
    let encoded_record =
      encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: ordinal, file_key: &file_key }, algorithm).unwrap();
    coordinator
      .admit(
        IndexMutationRequestV1 {
          index_id: &index_id,
          role: OrderedIndexRoleV1::ScopeReverse,
          publication_sequence: 40 + ordinal,
          operation_id: [0x50 + ordinal as u8; 16],
          encoded_record: &encoded_record,
        },
        1_000 + ordinal,
      )
      .unwrap();
  }
  let frozen = coordinator.begin_flush(1_010, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  (coordinator, frozen)
}

fn populated_two(
) -> (TempDir, MemoryCoordinator, IndexCoordinatorV1, aeordb::engine::v4::index_coordinator::FrozenIndexBatchV1, IndexRuntimeWorkspaceHeadV1)
{
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (coordinator, batch) = batch(&memory);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  workspace.append_runtime_batch([0x55; 16], 101, &batch).unwrap();
  let head = workspace.append_runtime_batch([0x66; 16], 102, &batch).unwrap();
  (directory, memory, coordinator, batch, head)
}

fn repair_manifest_crc(bytes: &mut [u8]) {
  let crc = crc32fast::hash(&bytes[..204]);
  bytes[204..208].copy_from_slice(&crc.to_le_bytes());
}

fn repair_object_integrity(bytes: &mut [u8]) {
  let payload_length = u64::from_le_bytes(bytes[120..128].try_into().unwrap()) as usize;
  let payload_end = 184 + payload_length;
  let payload_digest = blake3::hash(&bytes[184..payload_end]);
  bytes[152..184].copy_from_slice(payload_digest.as_bytes());
  let crc = crc32fast::hash(&bytes[..payload_end]);
  bytes[payload_end..payload_end + 4].copy_from_slice(&crc.to_le_bytes());
}

fn select_rewritten_head_object(head: &IndexRuntimeWorkspaceHeadV1, object_bytes: &[u8]) -> IndexRuntimeWorkspaceSelectedHeadV1 {
  let object_digest = blake3::hash(object_bytes);
  let manifest_path = manifest_path(head.workspace_path(), head.manifest_sequence());
  let mut manifest = fs::read(&manifest_path).unwrap();
  manifest[140..172].copy_from_slice(object_digest.as_bytes());
  repair_manifest_crc(&mut manifest);
  fs::write(manifest_path, &manifest).unwrap();
  selected_for(head, *blake3::hash(&manifest).as_bytes(), head.durable_bytes())
}

fn selected_for(head: &IndexRuntimeWorkspaceHeadV1, manifest_digest: [u8; 32], durable_bytes: u64) -> IndexRuntimeWorkspaceSelectedHeadV1 {
  IndexRuntimeWorkspaceSelectedHeadV1::new(
    head.workspace_path().to_path_buf(),
    head.selected_descriptor().workspace_id(),
    manifest_digest,
    head.manifest_sequence(),
    durable_bytes,
  )
  .unwrap()
}

fn reopen(
  head: &IndexRuntimeWorkspaceHeadV1,
  selected: IndexRuntimeWorkspaceSelectedHeadV1,
  memory: &MemoryCoordinator,
) -> Result<ReopenedIndexRuntimeWorkspaceV1, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1> {
  ReopenedIndexRuntimeWorkspaceV1::open(
    head.workspace_path(),
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    selected,
    IndexRuntimeWorkspaceReopenOptionsV1::new(8 * 1024 * 1024, 16).unwrap(),
    CancellationToken::new(),
    memory,
  )
}

#[test]
fn private_runtime_workspace_appends_and_reopens_one_exact_streamed_head() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let retained_before = memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes;
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity(),
    IndexRuntimeWorkspaceOptionsV1::new(Some(scratch), 8 * 1024 * 1024, 0, 16).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();

  let head = workspace.append_runtime_batch([0x55; 16], 1_725_000_000_123, &batch).unwrap();
  assert_eq!(head.manifest_sequence(), 1);
  assert_eq!(head.cumulative_object_count(), 1);
  assert!(head.durable_bytes() > 0);
  let workspace_path = head.workspace_path().to_path_buf();
  let selected = head.selected_descriptor();
  drop(workspace);

  let reopened = ReopenedIndexRuntimeWorkspaceV1::open(
    &workspace_path,
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    selected,
    IndexRuntimeWorkspaceReopenOptionsV1::new(8 * 1024 * 1024, 16).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert_eq!(reopened.runtime_id(), identity().runtime_id());
  assert_eq!(reopened.manifest_sequence(), 1);
  assert_eq!(reopened.runtime_batch_count(), 1);
  assert_eq!(reopened.producer_task_count(), 0);
  drop(reopened);

  let owner = memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().clone();
  assert_eq!(owner.reserved_bytes, retained_before);

  assert_eq!(fs::read(&database).unwrap(), b"source database remains unchanged");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(&workspace_path).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(manifest_path(&workspace_path, 1)).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(object_path(&workspace_path, [0x55; 16])).unwrap().permissions().mode() & 0o777, 0o600);
  }
}

#[test]
fn producer_tasks_share_the_cumulative_workspace_and_reopen_body_free() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  let first = workspace.append_runtime_batch([0x55; 16], 100, &batch).unwrap();
  assert_eq!(first.runtime_batch_count(), 1);

  let root = digest_parts(ALGORITHM, &[b"producer-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"producer-semantic"]);
  let task = producer_task([0x66; 16], 91, &root, &semantic);
  let second = workspace.append_producer_task([0x77; 16], 101, &task).unwrap();
  assert_eq!(second.manifest_sequence(), 2);
  assert_eq!(second.cumulative_object_count(), 2);
  assert_eq!(second.runtime_batch_count(), 1);
  assert_eq!(second.producer_task_count(), 1);

  let exact_retry = workspace.append_producer_task([0x77; 16], 101, &task).unwrap();
  assert_eq!(exact_retry.selected_descriptor(), second.selected_descriptor());
  let conflicting = producer_task([0x66; 16], 92, &root, &semantic);
  assert!(workspace.append_producer_task([0x77; 16], 101, &conflicting).is_err());

  let object_bytes = fs::read(producer_object_path(second.workspace_path(), [0x77; 16])).unwrap();
  let object = decode_index_workspace_object_v1(&object_bytes).unwrap();
  let decoded = decode_index_workspace_producer_task_payload_v1(object.payload, ALGORITHM).unwrap();
  assert_eq!(decoded.operation_id, [0x66; 16]);
  assert_eq!(decoded.publication_sequence, 91);
  assert_eq!(decoded.scope, Some("/docs"));
  assert_eq!(object.payload.len(), 56 + 4 * ALGORITHM.hash_length() + "/docs".len());

  let selected = second.selected_descriptor();
  let workspace_path = second.workspace_path().to_path_buf();
  drop(workspace);
  let reopened = ReopenedIndexRuntimeWorkspaceV1::open(
    &workspace_path,
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    selected,
    IndexRuntimeWorkspaceReopenOptionsV1::new(8 * 1024 * 1024, 16).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert_eq!(reopened.manifest_sequence(), 2);
  assert_eq!(reopened.runtime_batch_count(), 1);
  assert_eq!(reopened.producer_task_count(), 1);
}

#[test]
fn producer_task_write_obeys_memory_admission_before_installing_files() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let expected_workspace = scratch.join(hex::encode(identity().database_id())).join(hex::encode(identity().workspace_id()));
  let memory = memory(64);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  let root = digest_parts(ALGORITHM, &[b"producer-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"producer-semantic"]);
  let task = producer_task([0x68; 16], 93, &root, &semantic);
  let error = workspace.append_producer_task([0x78; 16], 103, &task).unwrap_err();
  assert!(matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Memory(_)));
  assert_eq!(fs::read_dir(expected_workspace.join("manifests")).unwrap().count(), 0);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, 0);
}

#[test]
fn default_workspace_is_a_private_database_sidecar() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity(),
    IndexRuntimeWorkspaceOptionsV1::new(None, 8 * 1024 * 1024, 0, 16).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let head = workspace.append_runtime_batch([0x55; 16], 101, &batch).unwrap();
  let expected = directory.path().join(format!(
    ".source.aeordb-index-runtime-{}-{}",
    hex::encode(identity().database_id()),
    hex::encode(identity().workspace_id())
  ));
  assert_eq!(head.workspace_path(), expected);
  assert!(head.workspace_path().is_dir());
  assert_eq!(fs::read(&database).unwrap(), b"source database remains unchanged");
}

#[test]
fn successor_heads_and_exact_retries_are_cumulative_and_idempotent() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();

  let first = workspace.append_runtime_batch([0x55; 16], 101, &batch).unwrap();
  assert!(workspace.append_runtime_batch([0x55; 16], 999, &batch).is_err());
  let retry = workspace.append_runtime_batch([0x55; 16], 101, &batch).unwrap();
  assert_eq!(retry.selected_descriptor(), first.selected_descriptor());
  assert_eq!(fs::read_dir(first.workspace_path().join("manifests")).unwrap().count(), 1);
  let second = workspace.append_runtime_batch([0x66; 16], 102, &batch).unwrap();
  assert_eq!(second.manifest_sequence(), 2);
  assert_eq!(second.cumulative_object_count(), 2);
  assert!(second.durable_bytes() > first.durable_bytes());
  assert_eq!(fs::read_dir(second.workspace_path().join("manifests")).unwrap().count(), 2);
  assert_eq!(fs::read_dir(second.workspace_path().join("objects/runtime")).unwrap().count(), 2);

  let reopened = ReopenedIndexRuntimeWorkspaceV1::open(
    second.workspace_path(),
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    second.selected_descriptor(),
    IndexRuntimeWorkspaceReopenOptionsV1::new(8 * 1024 * 1024, 16).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert_eq!(reopened.manifest_sequence(), 2);
  assert_eq!(reopened.runtime_batch_count(), 2);
}

#[test]
fn object_before_manifest_prefix_allows_only_the_exact_retry() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  let workspace_path =
    directory.path().join("scratch").join(hex::encode(identity().database_id())).join(hex::encode(identity().workspace_id()));
  let conflicting_manifest = manifest_path(&workspace_path, 1);
  fs::write(&conflicting_manifest, b"conflict").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&conflicting_manifest, fs::Permissions::from_mode(0o600)).unwrap();
  }

  assert!(workspace.append_runtime_batch([0x55; 16], 101, &batch).is_err());
  assert!(object_path(&workspace_path, [0x55; 16]).is_file());
  assert!(workspace.append_runtime_batch([0x66; 16], 102, &batch).is_err());
  fs::remove_file(conflicting_manifest).unwrap();
  let recovered = workspace.append_runtime_batch([0x55; 16], 101, &batch).unwrap();
  assert_eq!(recovered.manifest_sequence(), 1);
}

#[test]
fn conflicting_preexisting_object_fails_without_installing_a_manifest() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  let workspace_path =
    directory.path().join("scratch").join(hex::encode(identity().database_id())).join(hex::encode(identity().workspace_id()));
  let conflict = object_path(&workspace_path, [0x55; 16]);
  fs::write(&conflict, b"not the expected object").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&conflict, fs::Permissions::from_mode(0o600)).unwrap();
  }

  assert!(workspace.append_runtime_batch([0x55; 16], 101, &batch).is_err());
  assert_eq!(fs::read(&conflict).unwrap(), b"not the expected object");
  assert_eq!(fs::read_dir(workspace_path.join("manifests")).unwrap().count(), 0);
}

#[test]
fn restart_inventory_rejects_noncanonical_object_names() {
  let (_directory, memory, _coordinator, batch, head) = populated_two();
  let noncanonical = head.workspace_path().join("objects/runtime").join(format!("{}.aiwo", hex::encode_upper([0xab; 16])));
  fs::write(&noncanonical, b"noncanonical inventory entry").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&noncanonical, fs::Permissions::from_mode(0o600)).unwrap();
  }
  let mut resumed = DurableIndexRuntimeWorkspaceV1::resume(
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    head.selected_descriptor(),
    options(head.workspace_path().parent().unwrap().parent().unwrap().to_path_buf(), 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let error = resumed.append_runtime_batch([0x77; 16], 103, &batch).unwrap_err();
  assert!(matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Path(_)));
}

#[test]
fn selected_restart_resumes_the_exact_object_before_manifest_prefix() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity(),
    options(scratch.clone(), 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let first = workspace.append_runtime_batch([0x55; 16], 101, &batch).unwrap();
  let conflicting_manifest = manifest_path(first.workspace_path(), 2);
  fs::write(&conflicting_manifest, b"conflict").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&conflicting_manifest, fs::Permissions::from_mode(0o600)).unwrap();
  }
  assert!(workspace.append_runtime_batch([0x66; 16], 102, &batch).is_err());
  assert!(object_path(first.workspace_path(), [0x66; 16]).is_file());
  drop(workspace);
  fs::remove_file(conflicting_manifest).unwrap();

  let mut resumed = DurableIndexRuntimeWorkspaceV1::resume(
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    first.selected_descriptor(),
    options(scratch, 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert!(resumed.append_runtime_batch([0x55; 16], 101, &batch).is_err());
  let second = resumed.append_runtime_batch([0x66; 16], 102, &batch).unwrap();
  assert_eq!(second.manifest_sequence(), 2);
  assert_eq!(second.cumulative_object_count(), 2);
}

#[test]
fn capacity_count_and_cancellation_refuse_without_advancing_a_head() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let cancellation = CancellationToken::new();
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 1), cancellation.clone(), &memory)
      .unwrap();
  let first = workspace.append_runtime_batch([0x55; 16], 101, &batch).unwrap();
  assert!(workspace.append_runtime_batch([0x66; 16], 102, &batch).is_err());
  assert_eq!(fs::read_dir(first.workspace_path().join("manifests")).unwrap().count(), 1);

  cancellation.cancel();
  assert!(workspace.append_runtime_batch([0x55; 16], 101, &batch).is_err());
  assert!(ReopenedIndexRuntimeWorkspaceV1::open(
    first.workspace_path(),
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    first.selected_descriptor(),
    IndexRuntimeWorkspaceReopenOptionsV1::new(8 * 1024 * 1024, 16).unwrap(),
    cancellation,
    &memory,
  )
  .is_err());
}

#[test]
fn malformed_identity_options_selected_heads_and_paths_fail_before_storage() {
  assert!(IndexRuntimeWorkspaceIdentityV1::new([0; 16], [2; 16], [3; 16], [4; 16], ALGORITHM).is_err());
  assert!(IndexRuntimeWorkspaceOptionsV1::new(None, 0, 0, 1).is_err());
  assert!(IndexRuntimeWorkspaceOptionsV1::new(Some(PathBuf::from("relative")), 1024, 0, 1).is_err());
  assert!(IndexRuntimeWorkspaceOptionsV1::new(Some(PathBuf::from("/tmp/../tmp")), 1024, 0, 1).is_err());
  assert!(IndexRuntimeWorkspaceReopenOptionsV1::new(1024, 0).is_err());
  assert!(IndexRuntimeWorkspaceSelectedHeadV1::new(PathBuf::from("relative"), [3; 16], [4; 32], 1, 1).is_err());
  assert!(IndexRuntimeWorkspaceSelectedHeadV1::new(PathBuf::from("/tmp/../tmp/workspace"), [3; 16], [4; 32], 1, 1).is_err());

  let directory = tempdir().unwrap();
  let memory = memory(1024 * 1024);
  assert!(DurableIndexRuntimeWorkspaceV1::create(
    Path::new("relative.aeordb"),
    identity(),
    IndexRuntimeWorkspaceOptionsV1::new(None, 1024, 0, 1).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .is_err());
  let missing = directory.path().join("missing.aeordb");
  assert!(DurableIndexRuntimeWorkspaceV1::create(
    &missing,
    identity(),
    IndexRuntimeWorkspaceOptionsV1::new(None, 1024, 0, 1).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .is_err());
  let missing_workspace = directory.path().join("missing-workspace");
  let selected = IndexRuntimeWorkspaceSelectedHeadV1::new(missing_workspace.clone(), [3; 16], [4; 32], 1, 1).unwrap();
  let error = match ReopenedIndexRuntimeWorkspaceV1::open(
    &missing_workspace,
    [0; 16],
    [2; 16],
    ALGORITHM,
    selected,
    IndexRuntimeWorkspaceReopenOptionsV1::new(1024, 1).unwrap(),
    CancellationToken::new(),
    &memory,
  ) {
    Ok(_) => panic!("zero reopen identity reached filesystem access"),
    Err(error) => error,
  };
  assert!(matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Invalid(_)));
}

#[test]
fn free_space_and_workspace_byte_caps_refuse_before_installing_an_artifact() {
  let probe_directory = tempdir().unwrap();
  let probe_database = database_file(probe_directory.path());
  let probe_scratch = probe_directory.path().join("scratch");
  fs::create_dir(&probe_scratch).unwrap();
  let probe_memory = memory(16 * 1024 * 1024);
  let (_probe_coordinator, probe_batch) = batch(&probe_memory);
  let mut probe = DurableIndexRuntimeWorkspaceV1::create(
    &probe_database,
    identity(),
    options(probe_scratch, 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &probe_memory,
  )
  .unwrap();
  let probe_head = probe.append_runtime_batch([0x55; 16], 101, &probe_batch).unwrap();
  let exact_physical_bytes = probe_head.durable_bytes() + 208;

  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity(),
    options(scratch, exact_physical_bytes - 1, 16),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert!(workspace.append_runtime_batch([0x55; 16], 101, &batch).is_err());
  let workspace_path =
    directory.path().join("scratch").join(hex::encode(identity().database_id())).join(hex::encode(identity().workspace_id()));
  assert_eq!(fs::read_dir(workspace_path.join("objects/runtime")).unwrap().count(), 0);
  assert_eq!(fs::read_dir(workspace_path.join("manifests")).unwrap().count(), 0);

  let free_directory = tempdir().unwrap();
  let free_database = database_file(free_directory.path());
  let free_scratch = free_directory.path().join("scratch");
  fs::create_dir(&free_scratch).unwrap();
  let error = match DurableIndexRuntimeWorkspaceV1::create(
    &free_database,
    identity(),
    IndexRuntimeWorkspaceOptionsV1::new(Some(free_scratch.clone()), 8 * 1024 * 1024, u64::MAX, 16).unwrap(),
    CancellationToken::new(),
    &memory,
  ) {
    Ok(_) => panic!("workspace ignored unavailable minimum free space"),
    Err(error) => error,
  };
  assert!(matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Resource(_)));
  assert!(!free_scratch.join(hex::encode(identity().database_id())).exists());
}

#[test]
fn selected_reopen_rejects_predecessor_substitution_with_a_valid_head_crc_and_digest() {
  let (_directory, memory, _coordinator, _batch, head) = populated_two();
  let path = manifest_path(head.workspace_path(), 2);
  let mut bytes = fs::read(&path).unwrap();
  bytes[88..120].fill(0xa5);
  repair_manifest_crc(&mut bytes);
  fs::write(&path, &bytes).unwrap();
  let selected = selected_for(&head, *blake3::hash(&bytes).as_bytes(), head.durable_bytes());
  assert!(reopen(&head, selected, &memory).is_err());
}

#[test]
fn selected_reopen_rejects_cumulative_byte_lies_and_foreign_identity_after_crc_repair() {
  for corruption in ["bytes", "identity"] {
    let (_directory, memory, _coordinator, _batch, head) = populated_two();
    let path = manifest_path(head.workspace_path(), 2);
    let mut bytes = fs::read(&path).unwrap();
    let mut durable_bytes = head.durable_bytes();
    if corruption == "bytes" {
      durable_bytes += 1;
      bytes[188..196].copy_from_slice(&durable_bytes.to_le_bytes());
    } else {
      bytes[16..32].fill(0x91);
    }
    repair_manifest_crc(&mut bytes);
    fs::write(&path, &bytes).unwrap();
    let selected = selected_for(&head, *blake3::hash(&bytes).as_bytes(), durable_bytes);
    assert!(reopen(&head, selected, &memory).is_err(), "accepted repaired {corruption} corruption");
  }
}

#[test]
fn selected_reopen_rejects_missing_truncated_and_tampered_artifacts() {
  for corruption in ["missing_object", "truncated_manifest", "tampered_object"] {
    let (_directory, memory, _coordinator, _batch, head) = populated_two();
    match corruption {
      "missing_object" => fs::remove_file(object_path(head.workspace_path(), [0x55; 16])).unwrap(),
      "truncated_manifest" => {
        let path = manifest_path(head.workspace_path(), 1);
        let bytes = fs::read(&path).unwrap();
        fs::write(path, &bytes[..bytes.len() - 1]).unwrap();
      }
      "tampered_object" => {
        let path = object_path(head.workspace_path(), [0x66; 16]);
        let mut bytes = fs::read(&path).unwrap();
        bytes[184] ^= 0x80;
        fs::write(path, bytes).unwrap();
      }
      _ => unreachable!(),
    }
    assert!(reopen(&head, head.selected_descriptor(), &memory).is_err(), "accepted {corruption}");
  }
}

#[test]
fn streaming_reopen_rejects_semantic_and_amplified_frames_even_after_every_integrity_field_is_repaired() {
  for corruption in ["duplicate", "amplified"] {
    let (_directory, memory, _coordinator, _batch, head) = populated_two();
    let object_path = object_path(head.workspace_path(), [0x66; 16]);
    let mut object = fs::read(&object_path).unwrap();
    let first_frame = 184 + 64;
    let first_length = u32::from_le_bytes(object[first_frame..first_frame + 4].try_into().unwrap()) as usize;
    let second_frame = first_frame + first_length;
    if corruption == "duplicate" {
      let second_length = u32::from_le_bytes(object[second_frame..second_frame + 4].try_into().unwrap()) as usize;
      assert_eq!(first_length, second_length);
      let duplicate = object[first_frame..first_frame + first_length].to_vec();
      object[second_frame..second_frame + second_length].copy_from_slice(&duplicate);
    } else {
      object[first_frame..first_frame + 4].copy_from_slice(&(64 * 1024 * 1024u32).to_le_bytes());
    }
    repair_object_integrity(&mut object);
    fs::write(&object_path, &object).unwrap();
    let selected = select_rewritten_head_object(&head, &object);
    let error = match reopen(&head, selected, &memory) {
      Ok(_) => panic!("accepted repaired {corruption} corruption"),
      Err(error) => error,
    };
    assert!(
      matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Format(_)),
      "{corruption} corruption reached a non-format failure: {error}"
    );
  }
}

#[test]
fn producer_task_amplification_is_rejected_before_memory_admission() {
  let (_directory, _memory, _coordinator, _batch, head) = populated_two();
  let runtime_path = object_path(head.workspace_path(), [0x66; 16]);
  let mut object = fs::read(&runtime_path).unwrap();
  let payload = vec![0u8; 20 * 1024];
  object.resize(184 + payload.len() + 4, 0);
  let object_length = object.len() as u64;
  object[6..8].copy_from_slice(&2u16.to_le_bytes());
  object[12..20].copy_from_slice(&object_length.to_le_bytes());
  object[120..128].copy_from_slice(&(payload.len() as u64).to_le_bytes());
  object[128..136].copy_from_slice(&1u64.to_le_bytes());
  object[136..144].copy_from_slice(&42u64.to_le_bytes());
  object[144..152].copy_from_slice(&42u64.to_le_bytes());
  object[184..184 + payload.len()].copy_from_slice(&payload);
  repair_object_integrity(&mut object);
  let task_path = head.workspace_path().join("objects/tasks").join(format!("{}.aiwo", hex::encode([0x66; 16])));
  fs::remove_file(runtime_path).unwrap();
  fs::write(&task_path, &object).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&task_path, fs::Permissions::from_mode(0o600)).unwrap();
  }

  let manifest_path = manifest_path(head.workspace_path(), 2);
  let mut manifest = fs::read(&manifest_path).unwrap();
  let old_object_bytes = u64::from_le_bytes(manifest[172..180].try_into().unwrap());
  let old_cumulative_bytes = u64::from_le_bytes(manifest[188..196].try_into().unwrap());
  let new_object_bytes = object.len() as u64;
  let new_cumulative_bytes = old_cumulative_bytes - old_object_bytes + new_object_bytes;
  manifest[120..122].copy_from_slice(&2u16.to_le_bytes());
  manifest[140..172].copy_from_slice(blake3::hash(&object).as_bytes());
  manifest[172..180].copy_from_slice(&new_object_bytes.to_le_bytes());
  manifest[188..196].copy_from_slice(&new_cumulative_bytes.to_le_bytes());
  repair_manifest_crc(&mut manifest);
  fs::write(manifest_path, &manifest).unwrap();
  let selected = selected_for(&head, *blake3::hash(&manifest).as_bytes(), new_cumulative_bytes);
  let constrained = MemoryCoordinator::new(MemoryPolicy::new(64, 128, 1, 32).unwrap());
  let error = match reopen(&head, selected, &constrained) {
    Ok(_) => panic!("accepted an amplified producer-task payload"),
    Err(error) => error,
  };
  assert!(matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Format(_)));
  let owner = constrained.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
}

#[cfg(unix)]
#[test]
fn selected_reopen_refuses_symlink_substitution_for_objects_and_manifests() {
  use std::os::unix::fs::symlink;

  for role in ["object", "manifest"] {
    let (directory, memory, _coordinator, _batch, head) = populated_two();
    let target = if role == "object" { object_path(head.workspace_path(), [0x66; 16]) } else { manifest_path(head.workspace_path(), 2) };
    let outside = directory.path().join(format!("outside-{role}"));
    fs::copy(&target, &outside).unwrap();
    fs::remove_file(&target).unwrap();
    symlink(&outside, &target).unwrap();
    assert!(reopen(&head, head.selected_descriptor(), &memory).is_err(), "accepted {role} symlink");
  }
}

#[test]
fn selected_reopen_releases_every_frame_reservation_after_memory_refusal() {
  let (_directory, _memory, _coordinator, _batch, head) = populated_two();
  let constrained = MemoryCoordinator::new(MemoryPolicy::new(64, 128, 1, 32).unwrap());
  assert!(reopen(&head, head.selected_descriptor(), &constrained).is_err());
  let owner = constrained.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
}

#[test]
fn widest_hash_profile_streams_and_reopens_without_format_drift() {
  let algorithm = HashAlgorithm::Sha512;
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch_for(&memory, algorithm);
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity_for(algorithm),
    options(scratch, 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let head = workspace.append_runtime_batch([0x88; 16], 201, &batch).unwrap();
  let reopened = ReopenedIndexRuntimeWorkspaceV1::open(
    head.workspace_path(),
    identity_for(algorithm).database_id(),
    identity_for(algorithm).destination_physical_instance_id(),
    algorithm,
    head.selected_descriptor(),
    IndexRuntimeWorkspaceReopenOptionsV1::new(8 * 1024 * 1024, 16).unwrap(),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  assert_eq!(reopened.runtime_batch_count(), 1);
}

#[test]
fn production_store_uses_streaming_codecs_and_has_no_live_activation_caller() {
  let store = include_str!("../../src/engine/v4/index_runtime_workspace_store.rs");
  assert!(store.contains("stream_index_workspace_runtime_batch_payload_v1"));
  assert!(!store.contains("encode_index_workspace_runtime_batch_payload_v1"));
  assert!(!store.contains("encode_index_workspace_object_v1"));
  assert!(store.contains("object_already_installed"));
  let preflight = store
    .split_once("fn preflight_append")
    .and_then(|(_, remainder)| remainder.split_once("fn reconcile_object_inventory"))
    .map(|(preflight, _)| preflight)
    .unwrap();
  assert!(!preflight.contains("read_dir"), "the successful append preflight must not rescan the workspace inventory");

  let storage_engine = include_str!("../../src/engine/storage_engine.rs");
  let runtime_owner = include_str!("../../src/engine/v4/index_runtime_owner.rs");
  assert!(!storage_engine.contains("DurableIndexRuntimeWorkspaceV1"));
  assert!(!runtime_owner.contains("DurableIndexRuntimeWorkspaceV1"));
}
