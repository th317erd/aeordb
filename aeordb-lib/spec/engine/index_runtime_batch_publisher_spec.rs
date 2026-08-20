use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coordinator::{
  FrozenIndexBatchV1, IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1, IndexMutationRequestV1,
};
use aeordb::engine::v4::index_coordinator_recovery::{
  IndexCheckpointRootV1, IndexRecoveryOwnerV1, IndexRecoveryStoreErrorV1, IndexRecoveryStoreV1,
};
use aeordb::engine::v4::index_page::OrderedIndexRoleV1;
use aeordb::engine::v4::index_operation_control::IndexOperationKindV1;
use aeordb::engine::v4::index_recovery_store::{NativeIndexOperationDescriptorV1, NativeIndexRecoveryStoreV1};
use aeordb::engine::v4::index_record::{ScopeReverseRecordV1, encode_scope_reverse_record};
use aeordb::engine::v4::index_runtime_batch_publisher::{
  DurableIndexRuntimeBatchPublisherV1, INDEX_RUNTIME_DIRTY_OVERLAY_RESUME_KEY_V1, IndexRuntimeCheckpointStoreV1,
};
use aeordb::engine::v4::index_runtime_owner::{IndexRuntimeBatchPublisherV1, IndexRuntimePublicationErrorClassV1};
use aeordb::engine::v4::index_runtime_workspace_store::{
  DurableIndexRuntimeWorkspaceV1, IndexRuntimeWorkspaceIdentityV1, IndexRuntimeWorkspaceOptionsV1,
};
use aeordb::engine::v4::index_task::{
  ExternalWorkspaceDescriptorWriteV1, IndexTaskCheckpointWriteV1, IndexTaskKindV1, IndexTaskStateV1, decode_index_task_checkpoint,
  encode_index_task_checkpoint,
};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::{DiskKVStore, HashAlgorithm, MockClock, VirtualClock};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
const DATABASE_ID: [u8; 16] = [0x11; 16];
const DESTINATION_ID: [u8; 16] = [0x22; 16];
const WORKSPACE_ID: [u8; 16] = [0x33; 16];
const RUNTIME_ID: [u8; 16] = [0x44; 16];
const OPERATION_ID: [u8; 16] = [0x55; 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectorBehavior {
  Commit,
  Refuse,
  RefuseWithAmplifiedContext,
  CommitThenError,
  CommitThenDropCheckpoint,
  UnreadableAfterError,
}

struct FakeState {
  immutable: BTreeMap<Vec<u8>, Vec<u8>>,
  selected: Option<IndexCheckpointRootV1>,
  selector_behavior: SelectorBehavior,
  selector_calls: u64,
  refuse_loads: bool,
}

impl Default for FakeState {
  fn default() -> Self {
    Self { immutable: BTreeMap::new(), selected: None, selector_behavior: SelectorBehavior::Commit, selector_calls: 0, refuse_loads: false }
  }
}

struct FakeStore {
  state: Arc<Mutex<FakeState>>,
}

impl IndexRuntimeCheckpointStoreV1 for FakeStore {
  fn hash_algorithm(&self) -> HashAlgorithm {
    ALGORITHM
  }

  fn database_id(&self) -> [u8; 16] {
    DATABASE_ID
  }

  fn destination_physical_instance_id(&self) -> [u8; 16] {
    DESTINATION_ID
  }
}

impl IndexRecoveryStoreV1 for FakeStore {
  fn immutable_length(&mut self, key: &[u8]) -> Result<Option<u64>, IndexRecoveryStoreErrorV1> {
    Ok(self.state.lock().unwrap().immutable.get(key).map(|bytes| bytes.len() as u64))
  }

  fn load_immutable(&mut self, key: &[u8], expected_length: u64) -> Result<Option<Vec<u8>>, IndexRecoveryStoreErrorV1> {
    Ok(self.state.lock().unwrap().immutable.get(key).filter(|bytes| bytes.len() as u64 == expected_length).cloned())
  }

  fn put_immutable(
    &mut self,
    artifact: &aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1,
  ) -> Result<(), IndexRecoveryStoreErrorV1> {
    let mut state = self.state.lock().unwrap();
    match state.immutable.get(&artifact.key) {
      Some(existing) if existing == &artifact.value => Ok(()),
      Some(_) => Err(IndexRecoveryStoreErrorV1::new("fake_collision", "immutable key collision")),
      None => {
        state.immutable.insert(artifact.key.clone(), artifact.value.clone());
        Ok(())
      }
    }
  }

  fn sync_immutable(&mut self) -> Result<(), IndexRecoveryStoreErrorV1> {
    Ok(())
  }

  fn load_selected(&mut self, _owner: &IndexRecoveryOwnerV1) -> Result<Option<IndexCheckpointRootV1>, IndexRecoveryStoreErrorV1> {
    let state = self.state.lock().unwrap();
    if state.refuse_loads {
      return Err(IndexRecoveryStoreErrorV1::new("fake_unreadable", "selected root is unreadable"));
    }
    Ok(state.selected.clone())
  }

  fn publish_selected_synced(
    &mut self,
    _owner: &IndexRecoveryOwnerV1,
    expected: Option<&IndexCheckpointRootV1>,
    next: &IndexCheckpointRootV1,
  ) -> Result<(), IndexRecoveryStoreErrorV1> {
    let mut state = self.state.lock().unwrap();
    state.selector_calls += 1;
    if state.selected.as_ref() != expected {
      return Err(IndexRecoveryStoreErrorV1::new("fake_stale", "selector changed"));
    }
    match state.selector_behavior {
      SelectorBehavior::Commit => {
        state.selected = Some(next.clone());
        Ok(())
      }
      SelectorBehavior::Refuse => Err(IndexRecoveryStoreErrorV1::new("fake_refusal", "injected preselection refusal")),
      SelectorBehavior::RefuseWithAmplifiedContext => Err(IndexRecoveryStoreErrorV1::new("fake_refusal", "x".repeat(16 * 1024))),
      SelectorBehavior::CommitThenError => {
        state.selected = Some(next.clone());
        Err(IndexRecoveryStoreErrorV1::new("fake_postcommit", "injected postcommit observer failure"))
      }
      SelectorBehavior::CommitThenDropCheckpoint => {
        state.selected = Some(next.clone());
        state.immutable.remove(&next.checkpoint_key);
        Err(IndexRecoveryStoreErrorV1::new("fake_postcommit_missing", "injected missing committed checkpoint"))
      }
      SelectorBehavior::UnreadableAfterError => {
        state.refuse_loads = true;
        Err(IndexRecoveryStoreErrorV1::new("fake_unknown", "injected selector uncertainty"))
      }
    }
  }
}

struct Fixture {
  _directory: tempfile::TempDir,
  workspace_path: std::path::PathBuf,
  coordinator: IndexCoordinatorV1,
  publisher: DurableIndexRuntimeBatchPublisherV1<FakeStore>,
  state: Arc<Mutex<FakeState>>,
  clock: Arc<MockClock>,
}

impl Fixture {
  fn new(selector_behavior: SelectorBehavior) -> (Self, FrozenIndexBatchV1) {
    let directory = tempdir().unwrap();
    let database = directory.path().join("source.aeordb");
    fs::write(&database, b"source").unwrap();
    let scratch = directory.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let workspace_path = scratch.join(hex::encode(DATABASE_ID)).join(hex::encode(WORKSPACE_ID));
    let memory = memory();
    let mut coordinator = coordinator(&memory);
    admit(&mut coordinator, 1, 41, 1_001);
    let batch = coordinator.begin_flush(1_010, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
    let cancellation = CancellationToken::new();
    let workspace = DurableIndexRuntimeWorkspaceV1::create(
      &database,
      IndexRuntimeWorkspaceIdentityV1::new(DATABASE_ID, DESTINATION_ID, WORKSPACE_ID, RUNTIME_ID, ALGORITHM).unwrap(),
      IndexRuntimeWorkspaceOptionsV1::new(Some(scratch), 16 * 1024 * 1024, 0, 32).unwrap(),
      cancellation.clone(),
      &memory,
    )
    .unwrap();
    let owner = owner();
    let source_root = digest_parts(ALGORITHM, &[b"source-root"]);
    let state = Arc::new(Mutex::new(FakeState { selector_behavior, ..FakeState::default() }));
    let clock = Arc::new(MockClock::new(1, 1_725_000_000_000));
    let virtual_clock: Arc<dyn VirtualClock> = clock.clone();
    let publisher = DurableIndexRuntimeBatchPublisherV1::new_unselected(
      ALGORITHM,
      owner,
      source_root,
      1,
      1_725_000_000_000,
      workspace,
      FakeStore { state: Arc::clone(&state) },
      cancellation,
      virtual_clock,
    )
    .unwrap();
    (Self { _directory: directory, workspace_path, coordinator, publisher, state, clock }, batch)
  }
}

#[test]
fn cumulative_runtime_batches_publish_truthful_external_heads_selector_last() {
  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  let first_receipt = fixture.publisher.publish(&first).unwrap();
  assert_eq!(first_receipt.checkpoint_sequence, 1);
  assert_eq!(first_receipt.published_records, 1);
  fixture.coordinator.complete_success(&first).unwrap();

  fixture.clock.advance(10);
  admit(&mut fixture.coordinator, 2, 42, 1_020);
  let second = fixture.coordinator.begin_flush(1_030, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  let second_receipt = fixture.publisher.publish(&second).unwrap();
  assert_eq!(second_receipt.checkpoint_sequence, 2);

  let state = fixture.state.lock().unwrap();
  assert_eq!(state.selector_calls, 2);
  let selected = state.selected.as_ref().unwrap();
  assert_eq!(selected.checkpoint_sequence, 2);
  let checkpoint = decode_index_task_checkpoint(state.immutable.get(&selected.checkpoint_key).unwrap(), ALGORITHM).unwrap();
  assert_eq!(checkpoint.task_id, OPERATION_ID);
  assert_eq!(checkpoint.primary_id, owner().index_id());
  assert_eq!(checkpoint.journal_head, vec![0; ALGORITHM.hash_length()]);
  assert_eq!(checkpoint.journal_floor_sequence, 0);
  assert_eq!(checkpoint.journal_audited_through, 0);
  assert_eq!(checkpoint.resume_key, INDEX_RUNTIME_DIRTY_OVERLAY_RESUME_KEY_V1);
  assert_eq!(checkpoint.completed_work, 2);
  assert_eq!(checkpoint.total_work_hint, 2);
  assert_eq!(checkpoint.attachments.len(), 0);
  assert_ne!(checkpoint.required_capabilities[0] & 0x80, 0, "checkpoint does not require IndexArtifactV1");
  assert_ne!(checkpoint.required_capabilities[2] & 0x80, 0, "checkpoint does not require DurableTaskPinV1");
  let external = checkpoint.external.unwrap();
  assert_eq!(external.workspace_id, WORKSPACE_ID);
  assert_eq!(external.durable_sequence, 2);
  assert!(external.durable_bytes > 0);
  assert!(Path::new(external.path).is_absolute());
}

#[test]
fn preselection_refusal_reuses_one_exact_workspace_prefix() {
  let (mut fixture, batch) = Fixture::new(SelectorBehavior::Refuse);
  let first = fixture.publisher.publish(&batch).unwrap_err();
  assert_eq!(first.class(), IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection);
  let workspace = fixture.publisher.workspace_head().unwrap().selected_descriptor();
  {
    let mut state = fixture.state.lock().unwrap();
    assert!(state.selected.is_none());
    state.selector_behavior = SelectorBehavior::Commit;
  }
  fixture.clock.advance(10_000);
  let receipt = fixture.publisher.publish(&batch).unwrap();
  assert_eq!(receipt.checkpoint_sequence, 1);
  let retried = fixture.publisher.workspace_head().unwrap().selected_descriptor();
  assert_eq!(retried, workspace);
  let runtime_objects = fs::read_dir(workspace.workspace_path().join("objects/runtime")).unwrap().count();
  let manifests = fs::read_dir(workspace.workspace_path().join("manifests")).unwrap().count();
  assert_eq!((runtime_objects, manifests), (1, 1));
}

#[test]
fn object_before_manifest_failure_reuses_the_prepared_timestamp() {
  let (mut fixture, batch) = Fixture::new(SelectorBehavior::Commit);
  let manifest = fixture.workspace_path.join("manifests/0000000000000001.aiwm");
  fs::write(&manifest, b"injected conflicting manifest").unwrap();

  assert_eq!(fixture.publisher.publish(&batch).unwrap_err().class(), IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("objects/runtime")).unwrap().count(), 1);
  fs::remove_file(manifest).unwrap();
  fixture.clock.advance(10_000);

  assert_eq!(fixture.publisher.publish(&batch).unwrap().checkpoint_sequence, 1);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("objects/runtime")).unwrap().count(), 1);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("manifests")).unwrap().count(), 1);
}

#[test]
fn preselection_store_errors_are_bounded_before_the_runtime_observes_them() {
  let (mut fixture, batch) = Fixture::new(SelectorBehavior::RefuseWithAmplifiedContext);
  let error = fixture.publisher.publish(&batch).unwrap_err();
  assert_eq!(error.class(), IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection);
  assert!(!error.context().is_empty());
  assert!(error.context().len() <= 4 * 1024);
  assert!(fixture.state.lock().unwrap().selected.is_none());
}

#[test]
fn selector_errors_resolve_only_the_exact_successor_or_fail_commit_unknown() {
  let (mut committed, batch) = Fixture::new(SelectorBehavior::CommitThenError);
  assert_eq!(committed.publisher.publish(&batch).unwrap().checkpoint_sequence, 1);

  let (mut unknown, batch) = Fixture::new(SelectorBehavior::UnreadableAfterError);
  let error = unknown.publisher.publish(&batch).unwrap_err();
  assert_eq!(error.class(), IndexRuntimePublicationErrorClassV1::CommitUnknown);
  assert!(unknown.publisher.workspace_head().is_some());

  let (mut missing, batch) = Fixture::new(SelectorBehavior::CommitThenDropCheckpoint);
  let error = missing.publisher.publish(&batch).unwrap_err();
  assert_eq!(error.class(), IndexRuntimePublicationErrorClassV1::CommitUnknown);
}

#[test]
fn external_workspace_descriptor_accepts_canonical_posix_and_windows_paths() {
  for path in ["/var/lib/aeordb/runtime/workspace", "C:/Users/wyatt/AppData/Local/AeorDB/workspace"] {
    let source_root = digest_parts(ALGORITHM, &[b"source-root"]);
    let primary_id = owner().index_id().to_vec();
    let checkpoint = encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
      hash_algorithm: ALGORITHM,
      task_id: OPERATION_ID,
      checkpoint_sequence: 1,
      generation: 1,
      task_kind: IndexTaskKindV1::Reconcile,
      state: IndexTaskStateV1::Running,
      phase: 4,
      required_capabilities: &[0; 32],
      started_at_ms: 1,
      updated_at_ms: 1,
      source_root: &source_root,
      target_root: None,
      primary_id: Some(&primary_id),
      journal_head: None,
      journal_floor_sequence: 0,
      journal_audited_through: 0,
      next_document_ordinal: 0,
      completed_work: 1,
      total_work_hint: 1,
      resume_key: INDEX_RUNTIME_DIRTY_OVERLAY_RESUME_KEY_V1,
      attachments: &[],
      external: Some(ExternalWorkspaceDescriptorWriteV1 {
        workspace_id: WORKSPACE_ID,
        manifest_digest: [0xaa; 32],
        durable_sequence: 1,
        durable_bytes: 1,
        path,
      }),
    })
    .unwrap();
    assert_eq!(decode_index_task_checkpoint(&checkpoint.value, ALGORITHM).unwrap().external.unwrap().path, path);
  }
  for path in ["relative/workspace", "/var//workspace", "/var/../workspace", "c:/workspace", "C:\\workspace", "/"] {
    assert!(external_checkpoint(path).is_err(), "accepted noncanonical native path {path:?}");
  }
}

#[test]
fn real_native_authority_selects_the_external_runtime_checkpoint_last() {
  let (directory, database, first_authority) = native_first_authority("runtime-batch-native");
  first_authority.publish(&first_authority_request()).unwrap();
  let first_authority = Arc::new(first_authority);
  let memory = memory();
  let cancellation = CancellationToken::new();
  let retirement = Arc::new(Mutex::new(
    RetirementJournalOwnerV1::new_chain(
      ALGORITHM,
      DATABASE_ID,
      1,
      901,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &cancellation,
      &memory,
    )
    .unwrap(),
  ));
  let index_id = digest_parts(ALGORITHM, &[b"runtime-index"]);
  let descriptor = NativeIndexOperationDescriptorV1::new(
    ALGORITHM,
    DATABASE_ID,
    index_id.clone(),
    OPERATION_ID,
    IndexOperationKindV1::Reconcile,
    digest_parts(ALGORITHM, &[b"runtime-definition"]),
    None,
    None,
  )
  .unwrap();
  let clock = Arc::new(MockClock::new(1, 1_725_000_000_000));
  let virtual_clock: Arc<dyn VirtualClock> = clock.clone();
  let native_store = NativeIndexRecoveryStoreV1::new(descriptor, Arc::clone(&first_authority), retirement, virtual_clock.clone()).unwrap();
  let scratch = directory.path().join("scratch");
  fs::create_dir(&scratch).unwrap();
  let workspace = DurableIndexRuntimeWorkspaceV1::create(
    &database,
    IndexRuntimeWorkspaceIdentityV1::new(DATABASE_ID, [0x51; 16], WORKSPACE_ID, RUNTIME_ID, ALGORITHM).unwrap(),
    IndexRuntimeWorkspaceOptionsV1::new(Some(scratch), 16 * 1024 * 1024, 0, 32).unwrap(),
    cancellation.clone(),
    &memory,
  )
  .unwrap();
  let mut coordinator = coordinator(&memory);
  admit(&mut coordinator, 1, 41, 1_001);
  let batch = coordinator.begin_flush(1_010, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  let mut runtime_publisher = DurableIndexRuntimeBatchPublisherV1::new_unselected(
    ALGORITHM,
    IndexRecoveryOwnerV1::new(DATABASE_ID, index_id.clone(), OPERATION_ID).unwrap(),
    digest_parts(ALGORITHM, &[b"source-root"]),
    1,
    1_725_000_000_000,
    workspace,
    native_store,
    cancellation,
    virtual_clock,
  )
  .unwrap();
  let receipt = runtime_publisher.publish(&batch).unwrap();
  assert_eq!(receipt.checkpoint_sequence, 1);
  coordinator.complete_success(&batch).unwrap();

  clock.advance(10);
  admit(&mut coordinator, 2, 42, 1_020);
  let second = coordinator.begin_flush(1_030, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  let second_receipt = runtime_publisher.publish(&second).unwrap();
  assert_eq!(second_receipt.checkpoint_sequence, 2);

  let selected = first_authority.load_index_operation_control(&DATABASE_ID, &index_id, &OPERATION_ID).unwrap().unwrap();
  assert_eq!(selected.control_sequence, 2);
  let length = first_authority.index_artifact_length(&selected.checkpoint_artifact).unwrap().unwrap();
  let bytes = first_authority.load_index_artifact(&selected.checkpoint_artifact, length).unwrap().unwrap();
  let checkpoint = decode_index_task_checkpoint(&bytes, ALGORITHM).unwrap();
  assert_eq!(checkpoint.key, selected.checkpoint_artifact);
  assert_eq!(checkpoint.external.unwrap().durable_sequence, 2);
  assert_eq!(runtime_publisher.workspace_head().unwrap().manifest_sequence(), 2);
}

#[test]
fn runtime_publisher_uses_one_existing_selector_and_remains_disconnected() {
  let source = include_str!("../../src/engine/v4/index_runtime_batch_publisher.rs");
  let immutable = source.find("self.store.put_immutable(&checkpoint)").unwrap();
  let selector = source.find("self.store.publish_selected_synced").unwrap();
  assert!(immutable < selector, "runtime checkpoint selector appears before immutable publication");
  assert!(!source.contains("V4FirstAuthorityPublisher"));
  assert!(!include_str!("../../src/engine/storage_engine.rs").contains("DurableIndexRuntimeBatchPublisherV1"));
  assert!(!include_str!("../../src/engine/v4/index_runtime_installation.rs").contains("DurableIndexRuntimeBatchPublisherV1"));
  assert!(!include_str!("../../src/engine/v4/index_runtime_owner.rs").contains("DurableIndexRuntimeBatchPublisherV1"));
}

fn external_checkpoint(
  path: &str,
) -> aeordb::engine::v4::reader::FormatResult<aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1> {
  let source_root = digest_parts(ALGORITHM, &[b"source-root"]);
  let primary_id = owner().index_id().to_vec();
  encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
    hash_algorithm: ALGORITHM,
    task_id: OPERATION_ID,
    checkpoint_sequence: 1,
    generation: 1,
    task_kind: IndexTaskKindV1::Reconcile,
    state: IndexTaskStateV1::Running,
    phase: 4,
    required_capabilities: &[0; 32],
    started_at_ms: 1,
    updated_at_ms: 1,
    source_root: &source_root,
    target_root: None,
    primary_id: Some(&primary_id),
    journal_head: None,
    journal_floor_sequence: 0,
    journal_audited_through: 0,
    next_document_ordinal: 0,
    completed_work: 1,
    total_work_hint: 1,
    resume_key: b"dirty",
    attachments: &[],
    external: Some(ExternalWorkspaceDescriptorWriteV1 {
      workspace_id: WORKSPACE_ID,
      manifest_digest: [0xaa; 32],
      durable_sequence: 1,
      durable_bytes: 1,
      path,
    }),
  })
}

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(32 * 1024 * 1024, 40 * 1024 * 1024, 1, 4 * 1024 * 1024).unwrap())
}

fn native_first_authority(name: &str) -> (tempfile::TempDir, std::path::PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempdir().unwrap();
  let path = directory.path().join(format!("{name}.aeordb"));
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let kv_block_length = initial_block_size();
  let header = DatabaseHeaderV4 {
    hash_algorithm: ALGORITHM,
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
    head_hash: vec![0; 32],
    base_hash: vec![0; 32],
    target_hash: vec![0; 32],
    required_writer_capabilities: [0; 32],
    system_family_registry_version: 1,
    system_family_registry_fingerprint: vec![0x41; 32],
    writer_fence_epoch: 1,
    physical_instance_id: [0x51; 16],
  };
  let slot = encode_database_header_slot(&header).unwrap();
  file.seek(SeekFrom::Start(0)).unwrap();
  file.write_all(&slot).unwrap();
  file.write_all(&slot).unwrap();
  let durability = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::create_with_coordinator(
    file.try_clone().unwrap(),
    ALGORITHM,
    header.kv_block_offset,
    header.hot_tail_offset,
    0,
    Arc::clone(&durability),
  )
  .unwrap();
  file.sync_all().unwrap();
  (directory, path, V4FirstAuthorityPublisher::new(kv, durability).unwrap())
}

fn first_authority_request() -> FirstAuthorityPublicationRequestV1 {
  let semantic_state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
    },
    ALGORITHM,
  )
  .unwrap();
  FirstAuthorityPublicationRequestV1 {
    database_id: DATABASE_ID,
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(ALGORITHM, &[b"dirc:"]), stored_value: Vec::new() },
    semantic_state,
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(ALGORITHM, &[b"typed test closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}

fn owner() -> IndexRecoveryOwnerV1 {
  IndexRecoveryOwnerV1::new(DATABASE_ID, digest_parts(ALGORITHM, &[b"runtime-index"]), OPERATION_ID).unwrap()
}

fn coordinator(memory: &MemoryCoordinator) -> IndexCoordinatorV1 {
  IndexCoordinatorV1::new(
    RUNTIME_ID,
    ALGORITHM,
    memory.clone(),
    IndexCoordinatorOptionsV1::new(1024 * 1024, 16, 1_000, 1024 * 1024).unwrap(),
    1_000,
  )
  .unwrap()
}

fn admit(coordinator: &mut IndexCoordinatorV1, ordinal: u64, publication_sequence: u64, now_ms: u64) {
  let index_id = digest_parts(ALGORITHM, &[b"index", &ordinal.to_le_bytes()]);
  let file_key = digest_parts(ALGORITHM, &[b"file", &ordinal.to_le_bytes()]);
  let encoded_record =
    encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: ordinal, file_key: &file_key }, ALGORITHM).unwrap();
  coordinator
    .admit(
      IndexMutationRequestV1 {
        index_id: &index_id,
        role: OrderedIndexRoleV1::ScopeReverse,
        publication_sequence,
        operation_id: [0x70 + ordinal as u8; 16],
        encoded_record: &encoded_record,
      },
      now_ms,
    )
    .unwrap();
}
