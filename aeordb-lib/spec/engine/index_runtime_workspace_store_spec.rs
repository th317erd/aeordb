use std::fs;
use std::path::{Path, PathBuf};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coordinator::{IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1, IndexMutationRequestV1};
use aeordb::engine::v4::index_page::OrderedIndexRoleV1;
use aeordb::engine::v4::index_producer_coordinator::{IndexProducerTaskKindV1, IndexProducerTaskRequestV1};
use aeordb::engine::v4::index_record::{ScopeReverseRecordV1, encode_scope_reverse_record};
use aeordb::engine::v4::index_runtime_workspace::{
  IndexWorkspaceRuntimeBatchPayload, decode_index_workspace_manifest_v1, decode_index_workspace_object_v1,
  decode_index_workspace_producer_task_payload_v1, decode_index_workspace_runtime_batch_payload,
};
use aeordb::engine::v4::index_runtime_workspace_payload_v2::{
  IndexWorkspaceMembershipStateV2, IndexWorkspaceMembershipTransitionWriteV2, IndexWorkspaceMutationOperationV2,
  IndexWorkspaceOwnerClassV2, IndexWorkspaceRuntimeBatchWriteV2, IndexWorkspaceRuntimeMutationWriteV2,
};
use aeordb::engine::v4::index_runtime_workspace_store::{
  DurableIndexRuntimeWorkspaceV1, IndexRuntimeRecoveredTaskSinkErrorV1, IndexRuntimeRecoveredTaskSinkV1, IndexRuntimeWorkspaceHeadV1,
  IndexRuntimeWorkspaceIdentityV1, IndexRuntimeWorkspaceOptionsV1, IndexRuntimeWorkspaceReopenOptionsV1,
  IndexRuntimeWorkspaceSelectedHeadV1, ReopenedIndexRuntimeWorkspaceV1,
};
use aeordb::engine::v4::index_runtime_workspace_rotation::{IndexRuntimeImmutableCoverageProofV1, IndexRuntimeWorkspaceRotationErrorV1};
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

#[derive(Default)]
struct RecoveredTasks {
  tasks: Vec<([u8; 16], IndexProducerTaskKindV1, u64, Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>, Option<String>)>,
  reject_after: Option<usize>,
}

struct MemoryObservingRecoveredTaskSink<'a> {
  memory: &'a MemoryCoordinator,
  reserved_bytes_during_callback: Vec<u64>,
}

impl IndexRuntimeRecoveredTaskSinkV1 for MemoryObservingRecoveredTaskSink<'_> {
  fn admit_recovered_task(&mut self, _task: IndexProducerTaskRequestV1<'_>) -> Result<(), IndexRuntimeRecoveredTaskSinkErrorV1> {
    let snapshot =
      self.memory.snapshot().map_err(|error| IndexRuntimeRecoveredTaskSinkErrorV1::new("memory_snapshot_failed", error.to_string()))?;
    let owner = snapshot
      .owner(MemoryOwner::IndexDirtyBuffers)
      .ok_or_else(|| IndexRuntimeRecoveredTaskSinkErrorV1::new("memory_owner_missing", "index dirty-buffer owner is absent"))?;
    self.reserved_bytes_during_callback.push(owner.reserved_bytes);
    Ok(())
  }
}

impl IndexRuntimeRecoveredTaskSinkV1 for RecoveredTasks {
  fn admit_recovered_task(&mut self, task: IndexProducerTaskRequestV1<'_>) -> Result<(), IndexRuntimeRecoveredTaskSinkErrorV1> {
    if self.reject_after == Some(self.tasks.len()) {
      return Err(IndexRuntimeRecoveredTaskSinkErrorV1::new("injected_recovery_refusal", "injected selected-task refusal"));
    }
    self.tasks.push((
      task.operation_id,
      task.kind,
      task.publication_sequence,
      task.namespace_root_before.to_vec(),
      task.namespace_root_after.to_vec(),
      task.semantic_state_root.to_vec(),
      task.journal_head.map(<[u8]>::to_vec),
      task.scope.map(str::to_owned),
    ));
    Ok(())
  }
}

fn batch(memory: &MemoryCoordinator) -> (IndexCoordinatorV1, aeordb::engine::v4::index_coordinator::FrozenIndexBatchV1) {
  batch_for(memory, ALGORITHM)
}

fn batch_for(
  memory: &MemoryCoordinator,
  algorithm: HashAlgorithm,
) -> (IndexCoordinatorV1, aeordb::engine::v4::index_coordinator::FrozenIndexBatchV1) {
  batch_at(memory, algorithm, 41)
}

fn batch_at(
  memory: &MemoryCoordinator,
  algorithm: HashAlgorithm,
  first_publication_sequence: u64,
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
          publication_sequence: first_publication_sequence + ordinal - 1,
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

fn v2_batch(algorithm: HashAlgorithm) -> IndexWorkspaceRuntimeBatchWriteV2<'static> {
  let width = algorithm.hash_length();
  let owner = leaked(vec![0x21; width]);
  let old_key = leaked(vec![0x31; width]);
  let new_key = leaked(vec![0x32; width]);
  let old_record =
    leaked(encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: 3, file_key: old_key }, algorithm).unwrap());
  let new_record =
    leaked(encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: 3, file_key: new_key }, algorithm).unwrap());
  let mutations = leaked_values(vec![
    IndexWorkspaceRuntimeMutationWriteV2 {
      index_id: owner,
      role: OrderedIndexRoleV1::ScopeReverse,
      operation: IndexWorkspaceMutationOperationV2::RemoveExisting,
      publication_sequence: 40,
      operation_id: [0x11; 16],
      order_key: old_key,
      encoded_record: old_record,
    },
    IndexWorkspaceRuntimeMutationWriteV2 {
      index_id: owner,
      role: OrderedIndexRoleV1::ScopeReverse,
      operation: IndexWorkspaceMutationOperationV2::Upsert,
      publication_sequence: 41,
      operation_id: [0x12; 16],
      order_key: new_key,
      encoded_record: new_record,
    },
  ]);
  let transitions = leaked_values(vec![IndexWorkspaceMembershipTransitionWriteV2 {
    owner_id: owner,
    owner_class: IndexWorkspaceOwnerClassV2::ScopeCatalog,
    publication_sequence: 41,
    operation_id: [0x12; 16],
    document_ordinal: 3,
    before: IndexWorkspaceMembershipStateV2 { live: true, unindexable: false },
    after: IndexWorkspaceMembershipStateV2 { live: true, unindexable: false },
  }]);
  IndexWorkspaceRuntimeBatchWriteV2 {
    hash_algorithm: algorithm,
    coordinator_id: [0x77; 16],
    batch_id: 2,
    reason: IndexFlushReasonV1::Explicit,
    mutations,
    transitions,
  }
}

fn leaked(bytes: Vec<u8>) -> &'static [u8] {
  Box::leak(bytes.into_boxed_slice())
}

fn leaked_values<T>(values: Vec<T>) -> &'static [T] {
  Box::leak(values.into_boxed_slice())
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

#[test]
fn selected_resume_streams_producer_tasks_once_and_skips_runtime_batches() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let root = digest_parts(ALGORITHM, &[b"root"]);
  let semantic = digest_parts(ALGORITHM, &[b"semantic"]);
  let first = producer_task([0x61; 16], 50, &root, &semantic);
  let second = producer_task([0x62; 16], 51, &root, &semantic);
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity(),
    options(scratch.clone(), 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  workspace.append_producer_task([0x71; 16], 101, &first).unwrap();
  workspace.append_runtime_batch([0x72; 16], 102, &batch).unwrap();
  let selected = workspace.append_producer_task([0x73; 16], 103, &second).unwrap().selected_descriptor();
  drop(workspace);

  let mut recovered = RecoveredTasks::default();
  let resumed = DurableIndexRuntimeWorkspaceV1::resume_with_recovered_task_sink(
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    selected,
    options(scratch, 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
    &mut recovered,
  )
  .unwrap();

  assert_eq!(resumed.head().unwrap().cumulative_object_count(), 3);
  assert_eq!(recovered.tasks.len(), 2);
  recovered.tasks.sort_unstable_by_key(|task| task.2);
  assert_eq!(recovered.tasks[0].0, [0x61; 16]);
  assert_eq!(recovered.tasks[1].0, [0x62; 16]);
  assert_eq!(recovered.tasks[0].7.as_deref(), Some("/docs"));
  assert_eq!(recovered.tasks[1].7.as_deref(), Some("/docs"));
}

#[test]
fn selected_resume_surfaces_sink_refusal_without_returning_a_partial_workspace() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let root = digest_parts(ALGORITHM, &[b"root"]);
  let semantic = digest_parts(ALGORITHM, &[b"semantic"]);
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity(),
    options(scratch.clone(), 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  workspace.append_producer_task([0x71; 16], 101, &producer_task([0x61; 16], 50, &root, &semantic)).unwrap();
  let selected =
    workspace.append_producer_task([0x72; 16], 102, &producer_task([0x62; 16], 51, &root, &semantic)).unwrap().selected_descriptor();
  drop(workspace);
  let mut recovered = RecoveredTasks { tasks: Vec::new(), reject_after: Some(1) };

  let error = DurableIndexRuntimeWorkspaceV1::resume_with_recovered_task_sink(
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    selected,
    options(scratch, 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
    &mut recovered,
  )
  .err()
  .expect("sink refusal must abort selected workspace resume");

  assert!(error.to_string().contains("injected_recovery_refusal"));
  assert_eq!(recovered.tasks.len(), 1);
}

#[test]
fn selected_resume_returns_no_workspace_when_an_older_object_is_corrupt_after_a_task_callback() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let root = digest_parts(ALGORITHM, &[b"root"]);
  let semantic = digest_parts(ALGORITHM, &[b"semantic"]);
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity(),
    options(scratch.clone(), 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  workspace.append_runtime_batch([0x71; 16], 101, &batch).unwrap();
  let selected =
    workspace.append_producer_task([0x72; 16], 102, &producer_task([0x61; 16], 50, &root, &semantic)).unwrap().selected_descriptor();
  let runtime_object = object_path(workspace.workspace_path(), [0x71; 16]);
  drop(workspace);
  let mut bytes = fs::read(&runtime_object).unwrap();
  let last = bytes.len() - 1;
  bytes[last] ^= 0xff;
  fs::write(runtime_object, bytes).unwrap();
  let mut recovered = RecoveredTasks::default();

  let error = DurableIndexRuntimeWorkspaceV1::resume_with_recovered_task_sink(
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    selected,
    options(scratch, 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
    &mut recovered,
  )
  .err()
  .expect("older corruption must abort after the newer task callback");

  assert!(error.to_string().contains("digest") || error.to_string().contains("CRC"));
  assert_eq!(recovered.tasks.len(), 1);
}

#[test]
fn selected_resume_retains_payload_memory_through_the_recovery_callback() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let root = digest_parts(ALGORITHM, &[b"root"]);
  let semantic = digest_parts(ALGORITHM, &[b"semantic"]);
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity(),
    options(scratch.clone(), 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let selected =
    workspace.append_producer_task([0x71; 16], 101, &producer_task([0x61; 16], 50, &root, &semantic)).unwrap().selected_descriptor();
  drop(workspace);
  let baseline = memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes;
  let mut sink = MemoryObservingRecoveredTaskSink { memory: &memory, reserved_bytes_during_callback: Vec::new() };

  let resumed = DurableIndexRuntimeWorkspaceV1::resume_with_recovered_task_sink(
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    selected,
    options(scratch, 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
    &mut sink,
  )
  .unwrap();

  assert_eq!(sink.reserved_bytes_during_callback.len(), 1);
  assert!(sink.reserved_bytes_during_callback[0] > baseline);
  drop(resumed);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, baseline);
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
fn fresh_create_reuses_only_an_exact_empty_workspace_layout() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let create = || {
    DurableIndexRuntimeWorkspaceV1::create(
      &database,
      identity(),
      options(scratch.clone(), 8 * 1024 * 1024, 16),
      CancellationToken::new(),
      &memory,
    )
  };

  let workspace = create().unwrap();
  let workspace_path = workspace.workspace_path().to_path_buf();
  drop(workspace);
  assert!(create().is_ok(), "an exact empty workspace skeleton must be restartable");

  fs::write(workspace_path.join("manifests/unselected.aiwm"), b"unselected state").unwrap();
  let error = create().err().expect("retained workspace state must refuse fresh creation");
  assert!(matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::State(_)));
}

#[test]
fn fresh_create_restores_missing_empty_directories_but_rejects_an_expected_file() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let create = || {
    DurableIndexRuntimeWorkspaceV1::create(
      &database,
      identity(),
      options(scratch.clone(), 8 * 1024 * 1024, 16),
      CancellationToken::new(),
      &memory,
    )
  };

  let workspace = create().unwrap();
  let workspace_path = workspace.workspace_path().to_path_buf();
  drop(workspace);
  fs::remove_dir(workspace_path.join("objects/tasks")).unwrap();
  let reopened = create().unwrap();
  assert!(workspace_path.join("objects/tasks").is_dir());
  drop(reopened);

  fs::remove_dir(workspace_path.join("manifests")).unwrap();
  fs::write(workspace_path.join("manifests"), b"not a directory").unwrap();
  let error = create().err().expect("an expected directory replaced by a file must fail closed");
  assert!(matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Workspace(_)));
}

#[cfg(unix)]
#[test]
fn fresh_create_rejects_symlinked_and_non_utf8_empty_workspace_entries() {
  use std::ffi::OsString;
  use std::os::unix::ffi::OsStringExt;
  use std::os::unix::fs::symlink;

  for case in ["symlink", "non-utf8"] {
    let directory = tempdir().unwrap();
    let database = database_file(directory.path());
    let scratch = directory.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let memory = memory(16 * 1024 * 1024);
    let create = || {
      DurableIndexRuntimeWorkspaceV1::create(
        &database,
        identity(),
        options(scratch.clone(), 8 * 1024 * 1024, 16),
        CancellationToken::new(),
        &memory,
      )
    };

    let workspace = create().unwrap();
    let workspace_path = workspace.workspace_path().to_path_buf();
    drop(workspace);
    if case == "symlink" {
      fs::remove_dir(workspace_path.join("manifests")).unwrap();
      let outside = directory.path().join("outside");
      fs::create_dir(&outside).unwrap();
      symlink(&outside, workspace_path.join("manifests")).unwrap();
      let error = create().err().expect("an allowed entry name replaced by a symlink must fail closed");
      assert!(matches!(
        error,
        aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Workspace(_)
          | aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::State(_)
      ));
    } else {
      let name = OsString::from_vec(vec![0xff]);
      assert!(name.clone().into_string().is_err());
      match fs::write(workspace_path.join(name), b"unexpected state") {
        Ok(()) => {
          let error = create().err().expect("a non-UTF-8 retained entry must fail closed");
          assert!(matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::State(_)));
        }
        Err(error) => {
          #[cfg(target_os = "macos")]
          assert_eq!(error.raw_os_error(), Some(libc::EILSEQ), "macOS must reject the raw filename before engine inspection");
          #[cfg(not(target_os = "macos"))]
          panic!("the native filesystem unexpectedly rejected the non-UTF-8 fixture before engine inspection: {error}");
        }
      }
    }
  }
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
fn private_runtime_workspace_streams_and_reopens_v2_batches_for_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let directory = tempdir().unwrap();
    let database = database_file(directory.path());
    let scratch = directory.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let memory = memory(16 * 1024 * 1024);
    let batch = v2_batch(algorithm);
    let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
      &database,
      identity_for(algorithm),
      options(scratch, 8 * 1024 * 1024, 16),
      CancellationToken::new(),
      &memory,
    )
    .unwrap();
    let head = workspace.append_runtime_batch_v2([0x91; 16], 1_725_000_000_123, &batch).unwrap();
    let retry = workspace.append_runtime_batch_v2([0x91; 16], 1_725_000_000_123, &batch).unwrap();
    assert_eq!(retry.selected_descriptor(), head.selected_descriptor());

    let object = fs::read(object_path(head.workspace_path(), [0x91; 16])).unwrap();
    let object = decode_index_workspace_object_v1(&object).unwrap();
    assert!(matches!(
      decode_index_workspace_runtime_batch_payload(object.payload, algorithm).unwrap(),
      IndexWorkspaceRuntimeBatchPayload::V2(_)
    ));
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
}

#[test]
fn runtime_v2_workspace_rejects_hash_profile_drift_before_installing_artifacts() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  assert!(workspace.append_runtime_batch_v2([0x91; 16], 101, &v2_batch(HashAlgorithm::Sha512)).is_err());
  assert_eq!(fs::read_dir(workspace.workspace_path().join("manifests")).unwrap().count(), 0);
  assert_eq!(fs::read_dir(workspace.workspace_path().join("objects/runtime")).unwrap().count(), 0);
}

#[test]
fn runtime_v2_reopen_rejects_unbound_mutation_after_all_integrity_fields_are_repaired() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let batch = v2_batch(ALGORITHM);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  let head = workspace.append_runtime_batch_v2([0x91; 16], 101, &batch).unwrap();
  drop(workspace);

  let path = object_path(head.workspace_path(), [0x91; 16]);
  let mut object = fs::read(&path).unwrap();
  let first_mutation = 184 + 64;
  let second_mutation = first_mutation + u32::from_le_bytes(object[first_mutation..first_mutation + 4].try_into().unwrap()) as usize;
  let first_transition = second_mutation + u32::from_le_bytes(object[second_mutation..second_mutation + 4].try_into().unwrap()) as usize;
  object[first_transition + 48] ^= 0x80;
  repair_object_integrity(&mut object);
  fs::write(path, &object).unwrap();
  let selected = select_rewritten_head_object(&head, &object);
  let error = match reopen(&head, selected, &memory) {
    Ok(_) => panic!("accepted a runtime v2 mutation with no matching transition"),
    Err(error) => error,
  };
  assert!(matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Format(_)));
  let owner = memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().clone();
  assert_eq!(owner.reserved_bytes, 0);
  assert_eq!(owner.active_reservations, 0);
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
fn selected_workspace_rotation_inventory_uses_validated_files_and_exact_pending_tasks() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  workspace.append_runtime_batch([0x55; 16], 100, &batch).unwrap();

  let root = digest_parts(ALGORITHM, &[b"rotation-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"rotation-semantic"]);
  workspace.append_producer_task([0x71; 16], 101, &producer_task([0x61; 16], 43, &root, &semantic)).unwrap();
  workspace.append_producer_task([0x72; 16], 102, &producer_task([0x81; 16], 50, &root, &semantic)).unwrap();
  let coverage = IndexRuntimeImmutableCoverageProofV1 {
    runtime_id: identity().runtime_id(),
    generation: 7,
    source_namespace_root: &root,
    coverage_epoch_id: [0x91; 16],
    covered_through_publication_sequence: 43,
  };
  let reserved_before = memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes;

  let summary = workspace.plan_rotation(7, &root, coverage, &[[0x81; 16]]).unwrap();
  assert_eq!(summary.observed_objects, 3);
  assert_eq!(summary.discarded_objects, 2);
  assert_eq!(summary.retained_runtime_batches, 0);
  assert_eq!(summary.retained_pending_tasks, 1);
  assert_eq!(summary.retained_objects(), 1);

  let error = workspace.plan_rotation(7, &root, coverage, &[]).unwrap_err();
  assert!(matches!(
    error,
    aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Rotation(
      IndexRuntimeWorkspaceRotationErrorV1::UnprovenCompletedTask { operation_id }
    ) if operation_id == [0x81; 16]
  ));
  let owner = memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().clone();
  assert_eq!(owner.reserved_bytes, reserved_before);
}

#[test]
fn selected_workspace_rotation_rejects_unselected_manifest_artifacts() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, batch) = batch(&memory);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  let head = workspace.append_runtime_batch([0x55; 16], 100, &batch).unwrap();
  let root = digest_parts(ALGORITHM, &[b"rotation-root"]);
  let coverage = IndexRuntimeImmutableCoverageProofV1 {
    runtime_id: identity().runtime_id(),
    generation: 7,
    source_namespace_root: &root,
    coverage_epoch_id: [0x91; 16],
    covered_through_publication_sequence: 43,
  };
  let selected_manifest = fs::read(manifest_path(head.workspace_path(), 1)).unwrap();
  fs::write(manifest_path(head.workspace_path(), 2), selected_manifest).unwrap();

  let error = workspace.plan_rotation(7, &root, coverage, &[]).unwrap_err();
  assert!(matches!(
    error,
    aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::State(ref context)
      if context.contains("unselected or missing manifest")
  ));
}

#[test]
fn rotation_successor_streams_only_unresolved_batches_and_exact_pending_tasks() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_represented_coordinator, represented) = batch_at(&memory, ALGORITHM, 10);
  let (_unresolved_coordinator, unresolved) = batch_at(&memory, ALGORITHM, 50);
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity(),
    options(scratch.clone(), 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  workspace.append_runtime_batch([0x51; 16], 100, &represented).unwrap();
  let root = digest_parts(ALGORITHM, &[b"rotation-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"rotation-semantic"]);
  workspace.append_producer_task([0x52; 16], 101, &producer_task([0x61; 16], 20, &root, &semantic)).unwrap();
  workspace.append_runtime_batch([0x53; 16], 102, &unresolved).unwrap();
  workspace.append_producer_task([0x54; 16], 103, &producer_task([0x81; 16], 60, &root, &semantic)).unwrap();
  let predecessor = workspace.head().unwrap().selected_descriptor();
  let unresolved_source = fs::read(object_path(workspace.workspace_path(), [0x53; 16])).unwrap();
  let pending_source = fs::read(producer_object_path(workspace.workspace_path(), [0x54; 16])).unwrap();
  let coverage = IndexRuntimeImmutableCoverageProofV1 {
    runtime_id: identity().runtime_id(),
    generation: 7,
    source_namespace_root: &root,
    coverage_epoch_id: [0x91; 16],
    covered_through_publication_sequence: 45,
  };
  let reserved_before_rotation = memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes;

  let rotated = workspace.build_rotation_successor(9, 7, &root, coverage, &[[0x81; 16]]).unwrap();
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes, reserved_before_rotation);
  assert_eq!(rotated.rotation_sequence(), 9);
  assert_eq!(rotated.predecessor_selected(), &predecessor);
  assert_ne!(rotated.successor_workspace().workspace_path(), workspace.workspace_path());
  assert!(rotated
    .successor_workspace()
    .workspace_path()
    .ends_with(format!("{}-r0000000000000009", hex::encode(identity().workspace_id()))));
  let summary = rotated.summary();
  assert_eq!(summary.observed_objects, 4);
  assert_eq!(summary.discarded_objects, 2);
  assert_eq!(summary.retained_runtime_batches, 1);
  assert_eq!(summary.retained_pending_tasks, 1);
  let successor_head = rotated.successor_workspace().head().unwrap();
  assert_eq!(successor_head.manifest_sequence(), 2);
  assert_eq!(successor_head.runtime_batch_count(), 1);
  assert_eq!(successor_head.producer_task_count(), 1);

  let unresolved_target = fs::read(object_path(rotated.successor_workspace().workspace_path(), [0x53; 16])).unwrap();
  let pending_target = fs::read(producer_object_path(rotated.successor_workspace().workspace_path(), [0x54; 16])).unwrap();
  assert_eq!(fs::read(object_path(workspace.workspace_path(), [0x53; 16])).unwrap(), unresolved_source);
  assert_eq!(fs::read(producer_object_path(workspace.workspace_path(), [0x54; 16])).unwrap(), pending_source);
  let unresolved_source = decode_index_workspace_object_v1(&unresolved_source).unwrap();
  let unresolved_target = decode_index_workspace_object_v1(&unresolved_target).unwrap();
  assert_eq!(unresolved_source.payload, unresolved_target.payload);
  assert_eq!(unresolved_source.object_sequence, 3);
  assert_eq!(unresolved_target.object_sequence, 1);
  let pending_source = decode_index_workspace_object_v1(&pending_source).unwrap();
  let pending_target = decode_index_workspace_object_v1(&pending_target).unwrap();
  assert_eq!(pending_source.payload, pending_target.payload);
  assert_eq!(pending_source.object_sequence, 4);
  assert_eq!(pending_target.object_sequence, 2);

  let selected = successor_head.selected_descriptor();
  let successor_path = rotated.successor_workspace().workspace_path().to_path_buf();
  drop(rotated);
  fs::remove_file(manifest_path(&successor_path, 2)).unwrap();
  let retried = workspace.build_rotation_successor(9, 7, &root, coverage, &[[0x81; 16]]).unwrap();
  assert_eq!(retried.successor_workspace().head().unwrap().selected_descriptor(), selected);
  let mut recovered = RecoveredTasks::default();
  let reopened = DurableIndexRuntimeWorkspaceV1::resume_with_recovered_task_sink(
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    selected,
    options(scratch, 8 * 1024 * 1024, 16),
    CancellationToken::new(),
    &memory,
    &mut recovered,
  )
  .unwrap();
  assert_eq!(reopened.head().unwrap().manifest_sequence(), 2);
  assert_eq!(recovered.tasks.len(), 1);
  assert_eq!(recovered.tasks[0].0, [0x81; 16]);
  drop(reopened);

  let rotated_workspace = retried.into_successor_workspace();
  assert!(rotated_workspace.build_rotation_successor(9, 7, &root, coverage, &[[0x81; 16]]).is_err());
  assert!(rotated_workspace.build_rotation_successor(8, 7, &root, coverage, &[[0x81; 16]]).is_err());
  let next = rotated_workspace.build_rotation_successor(10, 7, &root, coverage, &[[0x81; 16]]).unwrap();
  assert!(next.successor_workspace().workspace_path().ends_with(format!("{}-r000000000000000a", hex::encode(identity().workspace_id()))));
}

#[test]
fn rotated_restart_replay_is_bounded_by_pending_tasks_not_completed_history() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let mut workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    identity(),
    options(scratch.clone(), 16 * 1024 * 1024, 128),
    CancellationToken::new(),
    &memory,
  )
  .unwrap();
  let root = digest_parts(ALGORITHM, &[b"rotation-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"rotation-semantic"]);
  let mut pending_operation_ids = Vec::new();
  for sequence in 1..=66u8 {
    let mut operation_id = [0x61; 16];
    operation_id[15] = sequence;
    let mut object_id = [0x71; 16];
    object_id[15] = sequence;
    workspace
      .append_producer_task(object_id, 100 + u64::from(sequence), &producer_task(operation_id, u64::from(sequence), &root, &semantic))
      .unwrap();
    if sequence > 64 {
      pending_operation_ids.push(operation_id);
    }
  }
  let coverage = IndexRuntimeImmutableCoverageProofV1 {
    runtime_id: identity().runtime_id(),
    generation: 7,
    source_namespace_root: &root,
    coverage_epoch_id: [0x93; 16],
    covered_through_publication_sequence: 64,
  };

  let rotated = workspace.build_rotation_successor(9, 7, &root, coverage, &pending_operation_ids).unwrap();
  assert_eq!(rotated.summary().observed_objects, 66);
  assert_eq!(rotated.summary().discarded_objects, 64);
  assert_eq!(rotated.summary().retained_pending_tasks, 2);
  let selected = rotated.successor_workspace().head().unwrap().selected_descriptor();
  drop(rotated);
  let mut recovered = RecoveredTasks::default();
  let reopened = DurableIndexRuntimeWorkspaceV1::resume_with_recovered_task_sink(
    identity().database_id(),
    identity().destination_physical_instance_id(),
    ALGORITHM,
    selected,
    options(scratch, 16 * 1024 * 1024, 128),
    CancellationToken::new(),
    &memory,
    &mut recovered,
  )
  .unwrap();
  assert_eq!(reopened.head().unwrap().producer_task_count(), 2);
  assert_eq!(recovered.tasks.len(), 2);
  let mut recovered_operation_ids = recovered.tasks.iter().map(|task| task.0).collect::<Vec<_>>();
  recovered_operation_ids.sort_unstable();
  assert_eq!(recovered_operation_ids, pending_operation_ids);
}

#[test]
fn rotation_successor_represents_an_empty_clean_workspace_without_a_synthetic_object() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, represented) = batch_at(&memory, ALGORITHM, 10);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  workspace.append_runtime_batch([0x51; 16], 100, &represented).unwrap();
  let root = digest_parts(ALGORITHM, &[b"rotation-root"]);
  let coverage = IndexRuntimeImmutableCoverageProofV1 {
    runtime_id: identity().runtime_id(),
    generation: 7,
    source_namespace_root: &root,
    coverage_epoch_id: [0x91; 16],
    covered_through_publication_sequence: 20,
  };

  assert!(workspace.build_rotation_successor(0, 7, &root, coverage, &[]).is_err());
  let rotated = workspace.build_rotation_successor(10, 7, &root, coverage, &[]).unwrap();
  assert_eq!(rotated.summary().retained_objects(), 0);
  assert!(rotated.successor_workspace().head().is_none());
  assert_eq!(fs::read_dir(rotated.successor_workspace().workspace_path().join("manifests")).unwrap().count(), 0);
  assert_eq!(fs::read_dir(rotated.successor_workspace().workspace_path().join("objects/runtime")).unwrap().count(), 0);
  assert_eq!(fs::read_dir(rotated.successor_workspace().workspace_path().join("objects/tasks")).unwrap().count(), 0);
  let successor_path = rotated.successor_workspace().workspace_path().to_path_buf();
  drop(rotated);

  let retried = workspace.build_rotation_successor(10, 7, &root, coverage, &[]).unwrap();
  assert_eq!(retried.successor_workspace().workspace_path(), successor_path);
  assert!(retried.successor_workspace().head().is_none());
}

#[test]
fn rotation_successor_conflicts_and_cancellation_never_mutate_the_predecessor() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, unresolved) = batch_at(&memory, ALGORITHM, 50);
  let cancellation = CancellationToken::new();
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), cancellation.clone(), &memory)
      .unwrap();
  workspace.append_runtime_batch([0x51; 16], 100, &unresolved).unwrap();
  let root = digest_parts(ALGORITHM, &[b"rotation-root"]);
  let coverage = IndexRuntimeImmutableCoverageProofV1 {
    runtime_id: identity().runtime_id(),
    generation: 7,
    source_namespace_root: &root,
    coverage_epoch_id: [0x91; 16],
    covered_through_publication_sequence: 45,
  };
  let source_path = object_path(workspace.workspace_path(), [0x51; 16]);
  let source_before = fs::read(&source_path).unwrap();
  let rotated = workspace.build_rotation_successor(9, 7, &root, coverage, &[]).unwrap();
  let target_path = object_path(rotated.successor_workspace().workspace_path(), [0x51; 16]);
  let mut corrupt_target = fs::read(&target_path).unwrap();
  corrupt_target[200] ^= 0xff;
  fs::write(&target_path, corrupt_target).unwrap();
  drop(rotated);

  assert!(workspace.build_rotation_successor(9, 7, &root, coverage, &[]).is_err());
  assert_eq!(fs::read(&source_path).unwrap(), source_before);
  cancellation.cancel();
  assert!(matches!(
    workspace.build_rotation_successor(10, 7, &root, coverage, &[]),
    Err(aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Canceled)
  ));
  let expected_canceled_path =
    workspace.workspace_path().parent().unwrap().join(format!("{}-r000000000000000a", hex::encode(identity().workspace_id())));
  assert!(!expected_canceled_path.exists());
  assert_eq!(fs::read(source_path).unwrap(), source_before);
}

#[test]
fn rotation_memory_pressure_preserves_the_predecessor_and_retries_after_release() {
  let directory = tempdir().unwrap();
  let database = database_file(directory.path());
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let memory = memory(16 * 1024 * 1024);
  let (_coordinator, unresolved) = batch_at(&memory, ALGORITHM, 50);
  let mut workspace =
    DurableIndexRuntimeWorkspaceV1::create(&database, identity(), options(scratch, 8 * 1024 * 1024, 16), CancellationToken::new(), &memory)
      .unwrap();
  workspace.append_runtime_batch([0x51; 16], 100, &unresolved).unwrap();
  let predecessor = workspace.head().unwrap().selected_descriptor();
  let source_path = object_path(workspace.workspace_path(), [0x51; 16]);
  let source_before = fs::read(&source_path).unwrap();
  let root = digest_parts(ALGORITHM, &[b"rotation-root"]);
  let coverage = IndexRuntimeImmutableCoverageProofV1 {
    runtime_id: identity().runtime_id(),
    generation: 7,
    source_namespace_root: &root,
    coverage_epoch_id: [0x92; 16],
    covered_through_publication_sequence: 45,
  };
  let before_pressure = memory.snapshot().unwrap();
  let policy = before_pressure.policy.unwrap();
  let pressure_bytes = policy.soft_limit_bytes.checked_sub(before_pressure.accounted_bytes).unwrap();
  let pressure = memory.reserve(MemoryOwner::Query, pressure_bytes, AdmissionClass::Workload).unwrap();

  let error = match workspace.build_rotation_successor(9, 7, &root, coverage, &[]) {
    Ok(_) => panic!("rotation unexpectedly succeeded under shared memory pressure"),
    Err(error) => error,
  };
  assert!(matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Memory(_)));
  assert_eq!(workspace.head().unwrap().selected_descriptor(), predecessor);
  assert_eq!(fs::read(&source_path).unwrap(), source_before);
  drop(pressure);
  assert_eq!(memory.snapshot().unwrap().reserved_bytes, before_pressure.reserved_bytes);

  let rotated = workspace.build_rotation_successor(9, 7, &root, coverage, &[]).unwrap();
  assert_eq!(rotated.predecessor_selected(), &predecessor);
  assert_eq!(rotated.successor_workspace().head().unwrap().manifest_sequence(), 1);
  assert_eq!(fs::read(source_path).unwrap(), source_before);
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
  let second = resumed.append_runtime_batch([0x66; 16], 999, &batch).unwrap();
  assert_eq!(second.manifest_sequence(), 2);
  assert_eq!(second.cumulative_object_count(), 2);
  let manifest = decode_index_workspace_manifest_v1(&fs::read(manifest_path(second.workspace_path(), 2)).unwrap()).unwrap();
  assert_eq!(manifest.created_at_ms, 102);
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
  fs::rename(runtime_path, &task_path).unwrap();
  fs::write(&task_path, &object).unwrap();

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
  assert!(
    matches!(error, aeordb::engine::v4::index_runtime_workspace_store::IndexRuntimeWorkspaceStoreErrorV1::Format(_)),
    "amplified producer task reached a non-format failure: {error}"
  );
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
  assert!(store.contains("stream_index_workspace_runtime_batch_payload_v2"));
  assert!(!store.contains("encode_index_workspace_runtime_batch_payload_v1"));
  assert!(!store.contains("encode_index_workspace_object_v1"));
  assert!(store.contains("object_already_installed"));
  let preflight = store
    .split_once("fn preflight_append")
    .and_then(|(_, remainder)| remainder.split_once("fn reconcile_object_inventory"))
    .map(|(preflight, _)| preflight)
    .unwrap();
  assert!(!preflight.contains("read_dir"), "the successful append preflight must not rescan the workspace inventory");
  let rotation_copy = store
    .split_once("fn copy_revalidated_payload")
    .and_then(|(_, remainder)| remainder.split_once("fn write_object_file"))
    .map(|(copy, _)| copy)
    .unwrap();
  assert!(rotation_copy.contains("[0u8; IO_CHUNK_BYTES]"));
  assert!(rotation_copy.contains("write_hashed"));
  assert!(!rotation_copy.contains("fs::read"));
  assert!(!rotation_copy.contains("read_to_end"));

  let storage_engine = include_str!("../../src/engine/storage_engine.rs");
  let runtime_owner = include_str!("../../src/engine/v4/index_runtime_owner.rs");
  assert!(!storage_engine.contains("DurableIndexRuntimeWorkspaceV1"));
  assert!(!runtime_owner.contains("DurableIndexRuntimeWorkspaceV1"));
}
