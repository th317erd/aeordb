use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coordinator::{
  FrozenIndexBatchV1, IndexCoordinatorOptionsV1, IndexCoordinatorV1, IndexFlushReasonV1, IndexMutationRequestV1,
};
use aeordb::engine::v4::index_coordinator_recovery::{
  IndexCheckpointRootV1, IndexRecoveryErrorV1, IndexRecoveryOptionsV1, IndexRecoveryOutcomeV1, IndexRecoveryOwnerV1, IndexRecoveryReasonV1,
  IndexRecoveryStoreErrorV1, IndexRecoveryStoreV1, recover_index_checkpoint_v1,
};
use aeordb::engine::v4::index_page::OrderedIndexRoleV1;
use aeordb::engine::v4::index_operation_control::IndexOperationKindV1;
use aeordb::engine::v4::index_producer_coordinator::{
  IndexProducerAdmissionV1, IndexProducerCoordinatorOptionsV1, IndexProducerCoordinatorV1, IndexProducerTaskKindV1,
  IndexProducerTaskRequestV1,
};
use aeordb::engine::v4::index_recovery_store::{NativeIndexOperationDescriptorV1, NativeIndexRecoveryStoreV1};
use aeordb::engine::v4::index_record::{ScopeReverseRecordV1, encode_scope_reverse_record};
use aeordb::engine::v4::index_runtime_dirty_overlay_recovery::{
  IndexRuntimeDirtyOverlayRecoveryErrorV1, IndexRuntimeDirtyOverlayRecoveryOutcomeV1, IndexRuntimeDirtyOverlayRecoveryReasonV1,
  recover_index_runtime_dirty_overlay_v1,
};
use aeordb::engine::v4::index_runtime_batch_publisher::{
  DurableIndexRuntimeBatchPublisherV1, INDEX_RUNTIME_DIRTY_OVERLAY_RESUME_KEY_V1, IndexRuntimeCheckpointStoreV1,
};
use aeordb::engine::v4::index_runtime_owner::{IndexRuntimeBatchPublisherV1, IndexRuntimePublicationErrorClassV1};
use aeordb::engine::v4::index_runtime_workspace_store::{
  DurableIndexRuntimeWorkspaceV1, IndexRuntimeWorkspaceIdentityV1, IndexRuntimeWorkspaceOptionsV1,
};
use aeordb::engine::v4::index_task::{
  ExternalWorkspaceDescriptorWriteV1, IndexTaskAttachmentRoleV1, IndexTaskAttachmentWriteV1, IndexTaskCheckpointWriteV1, IndexTaskKindV1,
  IndexTaskStateV1, decode_index_task_checkpoint, encode_index_task_checkpoint,
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
  selected_loads: u64,
  move_selected_on_load: Option<(u64, IndexCheckpointRootV1)>,
  cancel_on_selected_load: Option<(u64, CancellationToken)>,
  cancel_on_immutable_load: Option<CancellationToken>,
  refuse_loads: bool,
}

impl Default for FakeState {
  fn default() -> Self {
    Self {
      immutable: BTreeMap::new(),
      selected: None,
      selector_behavior: SelectorBehavior::Commit,
      selector_calls: 0,
      selected_loads: 0,
      move_selected_on_load: None,
      cancel_on_selected_load: None,
      cancel_on_immutable_load: None,
      refuse_loads: false,
    }
  }
}

struct FakeStore {
  state: Arc<Mutex<FakeState>>,
  hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  destination_id: [u8; 16],
}

impl FakeStore {
  fn new(state: Arc<Mutex<FakeState>>) -> Self {
    Self::for_algorithm(state, ALGORITHM)
  }

  fn for_algorithm(state: Arc<Mutex<FakeState>>, hash_algorithm: HashAlgorithm) -> Self {
    Self { state, hash_algorithm, database_id: DATABASE_ID, destination_id: DESTINATION_ID }
  }
}

impl IndexRuntimeCheckpointStoreV1 for FakeStore {
  fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  fn destination_physical_instance_id(&self) -> [u8; 16] {
    self.destination_id
  }
}

impl IndexRecoveryStoreV1 for FakeStore {
  fn immutable_length(&mut self, key: &[u8]) -> Result<Option<u64>, IndexRecoveryStoreErrorV1> {
    Ok(self.state.lock().unwrap().immutable.get(key).map(|bytes| bytes.len() as u64))
  }

  fn load_immutable(&mut self, key: &[u8], expected_length: u64) -> Result<Option<Vec<u8>>, IndexRecoveryStoreErrorV1> {
    let (loaded, cancellation) = {
      let state = self.state.lock().unwrap();
      (state.immutable.get(key).filter(|bytes| bytes.len() as u64 == expected_length).cloned(), state.cancel_on_immutable_load.clone())
    };
    if let Some(cancellation) = cancellation {
      cancellation.cancel();
    }
    Ok(loaded)
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
    let mut state = self.state.lock().unwrap();
    if state.refuse_loads {
      return Err(IndexRecoveryStoreErrorV1::new("fake_unreadable", "selected root is unreadable"));
    }
    state.selected_loads += 1;
    if let Some((load, next)) = state.move_selected_on_load.clone() {
      if state.selected_loads == load {
        state.selected = Some(next);
      }
    }
    if let Some((load, cancellation)) = state.cancel_on_selected_load.clone() {
      if state.selected_loads == load {
        cancellation.cancel();
      }
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
  cancellation: CancellationToken,
}

impl Fixture {
  fn new(selector_behavior: SelectorBehavior) -> (Self, FrozenIndexBatchV1) {
    Self::new_for(ALGORITHM, selector_behavior)
  }

  fn new_for(hash_algorithm: HashAlgorithm, selector_behavior: SelectorBehavior) -> (Self, FrozenIndexBatchV1) {
    let directory = tempdir().unwrap();
    let database = directory.path().join("source.aeordb");
    fs::write(&database, b"source").unwrap();
    let scratch = directory.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let workspace_path = scratch.join(hex::encode(DATABASE_ID)).join(hex::encode(WORKSPACE_ID));
    let memory = memory();
    let mut coordinator = coordinator_for(&memory, hash_algorithm);
    admit_for(&mut coordinator, hash_algorithm, 1, 41, 1_001);
    let batch = coordinator.begin_flush(1_010, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
    let cancellation = CancellationToken::new();
    let workspace = DurableIndexRuntimeWorkspaceV1::create(
      &database,
      IndexRuntimeWorkspaceIdentityV1::new(DATABASE_ID, DESTINATION_ID, WORKSPACE_ID, RUNTIME_ID, hash_algorithm).unwrap(),
      IndexRuntimeWorkspaceOptionsV1::new(Some(scratch), 16 * 1024 * 1024, 0, 32).unwrap(),
      cancellation.clone(),
      &memory,
    )
    .unwrap();
    let owner = owner_for(hash_algorithm);
    let source_root = digest_parts(hash_algorithm, &[b"source-root"]);
    let state = Arc::new(Mutex::new(FakeState { selector_behavior, ..FakeState::default() }));
    let clock = Arc::new(MockClock::new(1, 1_725_000_000_000));
    let virtual_clock: Arc<dyn VirtualClock> = clock.clone();
    let publisher = DurableIndexRuntimeBatchPublisherV1::new_unselected(
      hash_algorithm,
      owner,
      source_root,
      1,
      1_725_000_000_000,
      workspace,
      FakeStore::for_algorithm(Arc::clone(&state), hash_algorithm),
      cancellation.clone(),
      virtual_clock,
    )
    .unwrap();
    (Self { _directory: directory, workspace_path, coordinator, publisher, state, clock, cancellation }, batch)
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
fn producer_spill_uses_the_same_selector_and_is_idempotent_across_restart() {
  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  assert_eq!(fixture.publisher.publish(&first).unwrap().checkpoint_sequence, 1);
  fixture.coordinator.complete_success(&first).unwrap();

  let producer_memory = memory();
  let mut producer = producer_coordinator(&producer_memory, ALGORITHM, 1, 3);
  let root = digest_parts(ALGORITHM, &[b"producer-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"producer-semantic"]);
  let retained = producer_task([0x61; 16], 50, &root, &semantic);
  let spilled = producer_task([0x62; 16], 51, &root, &semantic);
  producer.admit(retained, 1_100).unwrap();
  let first_spill = producer.admit_or_spill(spilled, 1_101, &mut fixture.publisher).unwrap();
  let IndexProducerAdmissionV1::Spilled { receipt: first_receipt } = first_spill else {
    panic!("admission pressure did not spill through the durable publisher");
  };
  assert_eq!(first_receipt.spill_id(), [0x62; 16]);
  assert_eq!(first_receipt.artifact_key().len(), ALGORITHM.hash_length());
  assert_eq!(fixture.publisher.workspace_head().unwrap().manifest_sequence(), 2);
  assert_eq!(fixture.publisher.workspace_head().unwrap().runtime_batch_count(), 1);
  assert_eq!(fixture.publisher.workspace_head().unwrap().producer_task_count(), 1);

  let duplicate = producer.admit_or_spill(spilled, 1_102, &mut fixture.publisher).unwrap();
  let IndexProducerAdmissionV1::Spilled { receipt: duplicate_receipt } = duplicate else {
    panic!("duplicate pressure task was not resolved as the existing durable spill");
  };
  assert_eq!(duplicate_receipt, first_receipt);
  assert_eq!(fixture.publisher.workspace_head().unwrap().manifest_sequence(), 2);
  assert_eq!(fixture.state.lock().unwrap().selector_calls, 2);

  let state = Arc::clone(&fixture.state);
  drop(fixture.publisher);
  let recovery_memory = memory();
  let cancellation = CancellationToken::new();
  let mut recovery_store = FakeStore::new(Arc::clone(&state));
  let recovered = recover_index_runtime_dirty_overlay_v1(
    &mut recovery_store,
    ALGORITHM,
    DATABASE_ID,
    DESTINATION_ID,
    &owner(),
    IndexRuntimeWorkspaceOptionsV1::new(None, 16 * 1024 * 1024, 0, 32).unwrap(),
    &recovery_memory,
    &cancellation,
  )
  .unwrap();
  let IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Resumable(recovered) = recovered else {
    panic!("mixed runtime/producer workspace was not resumable");
  };
  let virtual_clock: Arc<dyn VirtualClock> = fixture.clock.clone();
  let mut resumed = DurableIndexRuntimeBatchPublisherV1::new_resumed(recovered, FakeStore::new(state), virtual_clock).unwrap();
  let after_restart = producer.admit_or_spill(spilled, 1_103, &mut resumed).unwrap();
  let IndexProducerAdmissionV1::Spilled { receipt: restarted_receipt } = after_restart else {
    panic!("restart duplicate did not resolve as the existing durable spill");
  };
  assert_eq!(restarted_receipt, first_receipt);
  assert_eq!(resumed.workspace_head().unwrap().manifest_sequence(), 2);

  fixture.clock.advance(10);
  admit(&mut fixture.coordinator, 2, 52, 1_110);
  let second = fixture.coordinator.begin_flush(1_120, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  assert_eq!(resumed.publish(&second).unwrap().checkpoint_sequence, 3);
  assert_eq!(resumed.workspace_head().unwrap().runtime_batch_count(), 2);
  assert_eq!(resumed.workspace_head().unwrap().producer_task_count(), 1);
}

#[test]
fn restart_recovers_an_unselected_producer_prefix_with_its_durable_timestamp() {
  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  assert_eq!(fixture.publisher.publish(&first).unwrap().checkpoint_sequence, 1);
  fixture.coordinator.complete_success(&first).unwrap();
  fixture.state.lock().unwrap().selector_behavior = SelectorBehavior::Refuse;

  let producer_memory = memory();
  let mut producer = producer_coordinator(&producer_memory, ALGORITHM, 4, 1);
  let root = digest_parts(ALGORITHM, &[b"crash-prefix-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"crash-prefix-semantic"]);
  let task = producer_task([0x68; 16], 58, &root, &semantic);
  producer.admit(task, 2_000).unwrap();
  let lease = producer.lease_next(2_000, false).unwrap().unwrap();
  assert!(producer.retry_task(&lease, 1, 2_001, false, &mut fixture.publisher).is_err());
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(fixture.publisher.workspace_head().unwrap().manifest_sequence(), 2);

  let state = Arc::clone(&fixture.state);
  drop(fixture.publisher);
  let recovery_memory = memory();
  let cancellation = CancellationToken::new();
  let mut recovery_store = FakeStore::new(Arc::clone(&state));
  let recovered = recover_index_runtime_dirty_overlay_v1(
    &mut recovery_store,
    ALGORITHM,
    DATABASE_ID,
    DESTINATION_ID,
    &owner(),
    IndexRuntimeWorkspaceOptionsV1::new(None, 16 * 1024 * 1024, 0, 32).unwrap(),
    &recovery_memory,
    &cancellation,
  )
  .unwrap();
  let IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Resumable(recovered) = recovered else {
    panic!("selected predecessor did not recover beside the unselected producer prefix");
  };
  assert_eq!(recovered.workspace_head().unwrap().manifest_sequence(), 1);

  fixture.clock.advance(50_000);
  state.lock().unwrap().selector_behavior = SelectorBehavior::Commit;
  let virtual_clock: Arc<dyn VirtualClock> = fixture.clock.clone();
  let mut resumed = DurableIndexRuntimeBatchPublisherV1::new_resumed(recovered, FakeStore::new(Arc::clone(&state)), virtual_clock).unwrap();
  let retry = producer.lease_next(3_001, false).unwrap().unwrap();
  let completion = producer.retry_task(&retry, 1, 3_002, false, &mut resumed).unwrap();
  let aeordb::engine::v4::index_producer_coordinator::IndexProducerCompletionV1::Spilled { receipt, .. } = completion else {
    panic!("restart did not select the exact durable producer prefix");
  };
  assert_eq!(receipt.spill_id(), [0x68; 16]);
  assert_eq!(resumed.workspace_head().unwrap().manifest_sequence(), 2);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("objects/tasks")).unwrap().count(), 1);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("manifests")).unwrap().count(), 2);
  let selected = state.lock().unwrap().selected.clone().unwrap();
  let checkpoint = state.lock().unwrap().immutable.get(&selected.checkpoint_key).cloned().unwrap();
  assert_eq!(decode_index_task_checkpoint(&checkpoint, ALGORITHM).unwrap().updated_at_ms, 1_725_000_000_000);
}

#[test]
fn historical_producer_duplicate_resolves_through_the_later_selected_head() {
  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  assert_eq!(fixture.publisher.publish(&first).unwrap().checkpoint_sequence, 1);
  fixture.coordinator.complete_success(&first).unwrap();

  let producer_memory = memory();
  let mut producer = producer_coordinator(&producer_memory, ALGORITHM, 1, 3);
  let root = digest_parts(ALGORITHM, &[b"historical-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"historical-semantic"]);
  let retained = producer_task([0x69; 16], 59, &root, &semantic);
  let spilled = producer_task([0x6a; 16], 60, &root, &semantic);
  producer.admit(retained, 2_100).unwrap();
  let IndexProducerAdmissionV1::Spilled { receipt: original } = producer.admit_or_spill(spilled, 2_101, &mut fixture.publisher).unwrap()
  else {
    panic!("producer operation was not spilled");
  };

  fixture.clock.advance(10);
  admit(&mut fixture.coordinator, 2, 61, 2_110);
  let successor = fixture.coordinator.begin_flush(2_120, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  assert_eq!(fixture.publisher.publish(&successor).unwrap().checkpoint_sequence, 3);
  let selected = fixture.state.lock().unwrap().selected.clone().unwrap();
  assert_ne!(selected.checkpoint_key, original.artifact_key());

  let IndexProducerAdmissionV1::Spilled { receipt: duplicate } = producer.admit_or_spill(spilled, 2_121, &mut fixture.publisher).unwrap()
  else {
    panic!("historical producer duplicate was not resolved as durable");
  };
  assert_eq!(duplicate.spill_id(), original.spill_id());
  assert_eq!(duplicate.artifact_key(), selected.checkpoint_key);
  assert_eq!(fixture.publisher.workspace_head().unwrap().manifest_sequence(), 3);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("objects/tasks")).unwrap().count(), 1);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("manifests")).unwrap().count(), 3);
  assert_eq!(fixture.state.lock().unwrap().selector_calls, 3);
}

#[test]
fn historical_producer_lookup_rejects_payload_conflicts_and_selected_chain_tamper() {
  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  assert_eq!(fixture.publisher.publish(&first).unwrap().checkpoint_sequence, 1);
  fixture.coordinator.complete_success(&first).unwrap();

  let producer_memory = memory();
  let mut producer = producer_coordinator(&producer_memory, ALGORITHM, 1, 3);
  let root = digest_parts(ALGORITHM, &[b"historical-adversarial-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"historical-adversarial-semantic"]);
  let retained = producer_task([0x6b; 16], 61, &root, &semantic);
  let spilled = producer_task([0x6c; 16], 62, &root, &semantic);
  producer.admit(retained, 2_200).unwrap();
  assert!(matches!(producer.admit_or_spill(spilled, 2_201, &mut fixture.publisher).unwrap(), IndexProducerAdmissionV1::Spilled { .. }));

  fixture.clock.advance(10);
  admit(&mut fixture.coordinator, 2, 63, 2_210);
  let successor = fixture.coordinator.begin_flush(2_220, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  assert_eq!(fixture.publisher.publish(&successor).unwrap().checkpoint_sequence, 3);
  let selector_calls = fixture.state.lock().unwrap().selector_calls;

  let conflicting_semantic = digest_parts(ALGORITHM, &[b"conflicting-semantic"]);
  let conflicting = producer_task([0x6c; 16], 62, &root, &conflicting_semantic);
  let conflicting_memory = memory();
  let mut conflicting_producer = producer_coordinator(&conflicting_memory, ALGORITHM, 1, 3);
  conflicting_producer.admit(producer_task([0x6d; 16], 64, &root, &semantic), 2_300).unwrap();
  assert!(conflicting_producer.admit_or_spill(conflicting, 2_301, &mut fixture.publisher).is_err());
  assert_eq!(fixture.state.lock().unwrap().selector_calls, selector_calls);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("objects/tasks")).unwrap().count(), 1);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("manifests")).unwrap().count(), 3);

  let selected_manifest = fixture.workspace_path.join("manifests/0000000000000003.aiwm");
  let mut tampered = fs::read(&selected_manifest).unwrap();
  tampered[88] ^= 0x80;
  fs::write(selected_manifest, tampered).unwrap();
  let tamper_memory = memory();
  let mut tamper_producer = producer_coordinator(&tamper_memory, ALGORITHM, 1, 3);
  tamper_producer.admit(producer_task([0x6e; 16], 65, &root, &semantic), 2_400).unwrap();
  assert!(tamper_producer.admit_or_spill(spilled, 2_401, &mut fixture.publisher).is_err());
  assert_eq!(fixture.state.lock().unwrap().selector_calls, selector_calls);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("objects/tasks")).unwrap().count(), 1);
  assert_eq!(fs::read_dir(fixture.workspace_path.join("manifests")).unwrap().count(), 3);
}

#[test]
fn refused_producer_spill_retains_the_task_until_the_exact_retry_is_selected() {
  let (mut fixture, _batch) = Fixture::new(SelectorBehavior::Refuse);
  let producer_memory = memory();
  let mut producer = producer_coordinator(&producer_memory, ALGORITHM, 4, 1);
  let root = digest_parts(ALGORITHM, &[b"retry-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"retry-semantic"]);
  let task = producer_task([0x71; 16], 61, &root, &semantic);
  producer.admit(task, 2_000).unwrap();
  let lease = producer.lease_next(2_000, false).unwrap().unwrap();
  let error = producer.retry_task(&lease, 1, 2_001, false, &mut fixture.publisher).unwrap_err();
  assert!(error.to_string().contains("spill failed"));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(fixture.publisher.workspace_head().unwrap().producer_task_count(), 1);

  fixture.state.lock().unwrap().selector_behavior = SelectorBehavior::Commit;
  let retry = producer.lease_next(3_001, false).unwrap().unwrap();
  let completion = producer.retry_task(&retry, 1, 3_002, false, &mut fixture.publisher).unwrap();
  let aeordb::engine::v4::index_producer_coordinator::IndexProducerCompletionV1::Spilled { receipt, .. } = completion else {
    panic!("exact retained task retry did not complete its durable spill");
  };
  assert_eq!(receipt.spill_id(), [0x71; 16]);
  assert_eq!(producer.snapshot().pending_tasks, 0);
  assert_eq!(fixture.publisher.workspace_head().unwrap().manifest_sequence(), 1);
}

#[test]
fn canceled_producer_spill_retains_retry_exhausted_work_without_selecting_an_artifact() {
  let (mut fixture, _batch) = Fixture::new(SelectorBehavior::Commit);
  let producer_memory = memory();
  let mut producer = producer_coordinator(&producer_memory, ALGORITHM, 4, 1);
  let root = digest_parts(ALGORITHM, &[b"cancel-root"]);
  let semantic = digest_parts(ALGORITHM, &[b"cancel-semantic"]);
  producer.admit(producer_task([0x76; 16], 66, &root, &semantic), 2_500).unwrap();
  let lease = producer.lease_next(2_500, false).unwrap().unwrap();
  fixture.cancellation.cancel();
  let error = producer.retry_task(&lease, 1, 2_501, false, &mut fixture.publisher).unwrap_err();
  assert!(error.to_string().contains("producer_spill_cancelled"));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert!(fixture.publisher.workspace_head().is_none());
  assert_eq!(fixture.state.lock().unwrap().selector_calls, 0);
}

#[test]
fn producer_spill_uses_the_widest_checkpoint_key_without_a_second_publisher() {
  let algorithm = HashAlgorithm::Sha512;
  let (mut fixture, _batch) = Fixture::new_for(algorithm, SelectorBehavior::Commit);
  let producer_memory = memory();
  let mut producer = producer_coordinator(&producer_memory, algorithm, 1, 3);
  let root = digest_parts(algorithm, &[b"wide-root"]);
  let semantic = digest_parts(algorithm, &[b"wide-semantic"]);
  producer.admit(producer_task([0x81; 16], 70, &root, &semantic), 4_000).unwrap();
  let result = producer.admit_or_spill(producer_task([0x82; 16], 71, &root, &semantic), 4_001, &mut fixture.publisher).unwrap();
  let IndexProducerAdmissionV1::Spilled { receipt } = result else {
    panic!("wide producer task was not durably spilled");
  };
  assert_eq!(receipt.spill_id(), [0x82; 16]);
  assert_eq!(receipt.artifact_key().len(), 64);
  assert_eq!(fixture.state.lock().unwrap().selector_calls, 1);
}

#[test]
fn selected_dirty_overlay_recovers_without_fabricating_journal_coverage_and_resumes_publication() {
  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  assert_eq!(fixture.publisher.publish(&first).unwrap().checkpoint_sequence, 1);
  fixture.coordinator.complete_success(&first).unwrap();

  let state = Arc::clone(&fixture.state);
  drop(fixture.publisher);
  let memory = memory();
  let cancellation = CancellationToken::new();
  let mut recovery_store = FakeStore::new(Arc::clone(&state));
  let recovered = recover_index_runtime_dirty_overlay_v1(
    &mut recovery_store,
    ALGORITHM,
    DATABASE_ID,
    DESTINATION_ID,
    &owner(),
    IndexRuntimeWorkspaceOptionsV1::new(None, 16 * 1024 * 1024, 0, 32).unwrap(),
    &memory,
    &cancellation,
  )
  .unwrap();
  let IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Resumable(recovered) = recovered else {
    panic!("selected runtime dirty overlay was not resumable");
  };
  assert_eq!(recovered.selected().checkpoint_sequence, 1);
  assert_eq!(recovered.workspace_head().unwrap().runtime_batch_count(), 1);
  let retained_bytes = reserved_index_bytes(&memory);
  assert!(
    retained_bytes > std::mem::size_of_val(recovered.as_ref()) as u64,
    "recovered state does not account for its retained heap allocations"
  );

  let virtual_clock: Arc<dyn VirtualClock> = fixture.clock.clone();
  let mut publisher = DurableIndexRuntimeBatchPublisherV1::new_resumed(recovered, FakeStore::new(state), virtual_clock).unwrap();
  assert_eq!(reserved_index_bytes(&memory), retained_bytes, "resumed publisher dropped its live recovery-state reservation");
  fixture.clock.advance(10);
  admit(&mut fixture.coordinator, 2, 42, 1_020);
  let second = fixture.coordinator.begin_flush(1_030, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  assert_eq!(publisher.publish(&second).unwrap().checkpoint_sequence, 2);
  drop(publisher);
  assert_eq!(reserved_index_bytes(&memory), 0, "publisher drop did not release its recovered-state reservation");
}

#[test]
fn journal_recovery_does_not_misrepresent_a_selected_dirty_overlay_as_complete_coverage() {
  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  fixture.publisher.publish(&first).unwrap();
  let state = Arc::clone(&fixture.state);
  drop(fixture.publisher);
  let memory = memory();
  let mut store = FakeStore::new(state);
  let outcome = recover_index_checkpoint_v1(
    &mut store,
    ALGORITHM,
    &owner(),
    IndexRecoveryOptionsV1::new(32, 16 * 1024 * 1024, 32, 16 * 1024 * 1024).unwrap(),
    &memory,
    &CancellationToken::new(),
  )
  .unwrap();
  assert!(matches!(outcome, IndexRecoveryOutcomeV1::ReconciliationRequired { reason: IndexRecoveryReasonV1::JournalMissing, .. }));
}

#[test]
fn dirty_overlay_recovery_rejects_every_near_match_contract_shape() {
  for deviation in [
    DirtyCheckpointDeviation::TaskKind,
    DirtyCheckpointDeviation::State,
    DirtyCheckpointDeviation::Phase,
    DirtyCheckpointDeviation::Capabilities,
    DirtyCheckpointDeviation::StartedAt,
    DirtyCheckpointDeviation::TargetRoot,
    DirtyCheckpointDeviation::Journal,
    DirtyCheckpointDeviation::NextDocumentOrdinal,
    DirtyCheckpointDeviation::ResumeKey,
    DirtyCheckpointDeviation::Progress,
    DirtyCheckpointDeviation::ZeroProgress,
    DirtyCheckpointDeviation::Attachment,
    DirtyCheckpointDeviation::External,
    DirtyCheckpointDeviation::ExternalSequence,
    DirtyCheckpointDeviation::ExternalBytes,
  ] {
    let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
    fixture.publisher.publish(&first).unwrap();
    replace_selected_checkpoint(&fixture.state, deviation);
    let state = Arc::clone(&fixture.state);
    drop(fixture.publisher);
    let outcome = recover_dirty_overlay(FakeStore::new(state), &memory(), &CancellationToken::new(), 16 * 1024 * 1024, 32).unwrap();
    assert_recovery_reason(outcome, IndexRuntimeDirtyOverlayRecoveryReasonV1::CheckpointContractMismatch);
  }
}

#[test]
fn dirty_overlay_recovery_rejects_missing_truncated_tampered_and_over_limit_workspace_closure() {
  for corruption in ["missing", "truncated", "tampered"] {
    let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
    fixture.publisher.publish(&first).unwrap();
    let runtime_object = fs::read_dir(fixture.workspace_path.join("objects/runtime")).unwrap().next().unwrap().unwrap().path();
    match corruption {
      "missing" => fs::remove_file(&runtime_object).unwrap(),
      "truncated" => {
        let mut bytes = fs::read(&runtime_object).unwrap();
        bytes.truncate(bytes.len() - 1);
        fs::write(&runtime_object, bytes).unwrap();
      }
      "tampered" => {
        let mut bytes = fs::read(&runtime_object).unwrap();
        let tampered = bytes.len() - 5;
        bytes[tampered] ^= 0x80;
        fs::write(&runtime_object, bytes).unwrap();
      }
      _ => unreachable!("fixed corruption matrix"),
    }
    let state = Arc::clone(&fixture.state);
    drop(fixture.publisher);
    let outcome = recover_dirty_overlay(FakeStore::new(state), &memory(), &CancellationToken::new(), 16 * 1024 * 1024, 32).unwrap();
    let reason = match outcome {
      IndexRuntimeDirtyOverlayRecoveryOutcomeV1::ReconciliationRequired { reason, .. } => reason,
      _ => panic!("{corruption} workspace closure was accepted"),
    };
    assert!(matches!(
      reason,
      IndexRuntimeDirtyOverlayRecoveryReasonV1::WorkspaceMissing | IndexRuntimeDirtyOverlayRecoveryReasonV1::WorkspaceCorrupt
    ));
  }

  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  fixture.publisher.publish(&first).unwrap();
  fixture.coordinator.complete_success(&first).unwrap();
  fixture.clock.advance(10);
  admit(&mut fixture.coordinator, 2, 42, 1_020);
  let second = fixture.coordinator.begin_flush(1_030, Some(IndexFlushReasonV1::Explicit), false).unwrap().unwrap();
  fixture.publisher.publish(&second).unwrap();
  let durable_bytes = fixture.publisher.workspace_head().unwrap().durable_bytes();
  let state = Arc::clone(&fixture.state);
  drop(fixture.publisher);
  let count_limited =
    recover_dirty_overlay(FakeStore::new(Arc::clone(&state)), &memory(), &CancellationToken::new(), 16 * 1024 * 1024, 1).unwrap();
  assert_recovery_reason(count_limited, IndexRuntimeDirtyOverlayRecoveryReasonV1::RecoveryLimitExceeded);
  let byte_limited = recover_dirty_overlay(FakeStore::new(state), &memory(), &CancellationToken::new(), durable_bytes - 1, 32).unwrap();
  assert_recovery_reason(byte_limited, IndexRuntimeDirtyOverlayRecoveryReasonV1::RecoveryLimitExceeded);
}

#[test]
fn dirty_overlay_recovery_preserves_transient_free_space_pressure_as_a_typed_error() {
  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  fixture.publisher.publish(&first).unwrap();
  let state = Arc::clone(&fixture.state);
  drop(fixture.publisher);

  let mut store = FakeStore::new(state);
  let outcome = recover_index_runtime_dirty_overlay_v1(
    &mut store,
    ALGORITHM,
    DATABASE_ID,
    DESTINATION_ID,
    &owner(),
    IndexRuntimeWorkspaceOptionsV1::new(None, 16 * 1024 * 1024, u64::MAX, 32).unwrap(),
    &memory(),
    &CancellationToken::new(),
  );
  assert!(matches!(outcome, Err(IndexRuntimeDirtyOverlayRecoveryErrorV1::Workspace(_))));
}

#[test]
fn dirty_overlay_recovery_honors_cancellation_and_releases_memory_after_pressure() {
  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  fixture.publisher.publish(&first).unwrap();
  let state = Arc::clone(&fixture.state);
  drop(fixture.publisher);

  let canceled = CancellationToken::new();
  canceled.cancel();
  assert!(matches!(
    recover_dirty_overlay(FakeStore::new(Arc::clone(&state)), &memory(), &canceled, 16 * 1024 * 1024, 32).unwrap(),
    IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Canceled
  ));

  let midflight = CancellationToken::new();
  state.lock().unwrap().cancel_on_immutable_load = Some(midflight.clone());
  assert!(matches!(
    recover_dirty_overlay(FakeStore::new(Arc::clone(&state)), &memory(), &midflight, 16 * 1024 * 1024, 32).unwrap(),
    IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Canceled
  ));
  state.lock().unwrap().cancel_on_immutable_load = None;

  let final_read_cancellation = CancellationToken::new();
  {
    let mut state = state.lock().unwrap();
    let cancel_at = state.selected_loads + 2;
    state.cancel_on_selected_load = Some((cancel_at, final_read_cancellation.clone()));
  }
  assert!(matches!(
    recover_dirty_overlay(FakeStore::new(Arc::clone(&state)), &memory(), &final_read_cancellation, 16 * 1024 * 1024, 32,).unwrap(),
    IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Canceled
  ));
  state.lock().unwrap().cancel_on_selected_load = None;

  let checkpoint_bytes = {
    let state = state.lock().unwrap();
    let selected = state.selected.as_ref().unwrap();
    state.immutable.get(&selected.checkpoint_key).unwrap().len() as u64
  };
  let pressured_memory = memory_with_limit(checkpoint_bytes - 1);
  let error = match recover_dirty_overlay(FakeStore::new(state), &pressured_memory, &CancellationToken::new(), 16 * 1024 * 1024, 32) {
    Ok(_) => panic!("dirty-overlay recovery ignored memory pressure"),
    Err(error) => error,
  };
  assert!(matches!(error, IndexRuntimeDirtyOverlayRecoveryErrorV1::Checkpoint(IndexRecoveryErrorV1::Memory(_))));
  assert_eq!(reserved_index_bytes(&pressured_memory), 0);
}

#[test]
fn dirty_overlay_recovery_classifies_absent_missing_and_unreadable_checkpoint_authority() {
  let absent = recover_dirty_overlay(
    FakeStore::new(Arc::new(Mutex::new(FakeState::default()))),
    &memory(),
    &CancellationToken::new(),
    16 * 1024 * 1024,
    32,
  )
  .unwrap();
  assert_recovery_reason(absent, IndexRuntimeDirtyOverlayRecoveryReasonV1::Checkpoint(IndexRecoveryReasonV1::CheckpointSelectionMissing));

  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  fixture.publisher.publish(&first).unwrap();
  let state = Arc::clone(&fixture.state);
  drop(fixture.publisher);
  {
    let mut state = state.lock().unwrap();
    let selected = state.selected.as_ref().unwrap().checkpoint_key.clone();
    state.immutable.remove(&selected);
  }
  let missing =
    recover_dirty_overlay(FakeStore::new(Arc::clone(&state)), &memory(), &CancellationToken::new(), 16 * 1024 * 1024, 32).unwrap();
  assert_recovery_reason(missing, IndexRuntimeDirtyOverlayRecoveryReasonV1::Checkpoint(IndexRecoveryReasonV1::CheckpointMissing));

  state.lock().unwrap().refuse_loads = true;
  let unreadable = recover_dirty_overlay(FakeStore::new(state), &memory(), &CancellationToken::new(), 16 * 1024 * 1024, 32);
  assert!(matches!(unreadable, Err(IndexRuntimeDirtyOverlayRecoveryErrorV1::Checkpoint(IndexRecoveryErrorV1::Store(_)))));
}

#[test]
fn dirty_overlay_recovery_and_resumed_construction_refuse_selector_movement() {
  let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
  fixture.publisher.publish(&first).unwrap();
  let state = Arc::clone(&fixture.state);
  drop(fixture.publisher);
  let foreign = IndexCheckpointRootV1::new(99, vec![0x99; ALGORITHM.hash_length()]).unwrap();
  {
    let mut state = state.lock().unwrap();
    let move_at = state.selected_loads + 2;
    state.move_selected_on_load = Some((move_at, foreign.clone()));
  }
  let outcome =
    recover_dirty_overlay(FakeStore::new(Arc::clone(&state)), &memory(), &CancellationToken::new(), 16 * 1024 * 1024, 32).unwrap();
  assert_recovery_reason(outcome, IndexRuntimeDirtyOverlayRecoveryReasonV1::SelectionChanged);

  {
    let mut state = state.lock().unwrap();
    state.selected = state.immutable.keys().next_back().map(|key| IndexCheckpointRootV1::new(1, key.clone()).unwrap());
    state.move_selected_on_load = None;
  }
  let cancellation = CancellationToken::new();
  let recovered = recover_dirty_overlay(FakeStore::new(Arc::clone(&state)), &memory(), &cancellation, 16 * 1024 * 1024, 32).unwrap();
  let IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Resumable(recovered) = recovered else {
    panic!("second recovery did not return resumable state");
  };
  state.lock().unwrap().selected = Some(foreign);
  let virtual_clock: Arc<dyn VirtualClock> = fixture.clock;
  assert!(DurableIndexRuntimeBatchPublisherV1::new_resumed(recovered, FakeStore::new(state), virtual_clock).is_err());
}

#[test]
fn resumed_publisher_rejects_foreign_store_identity_and_post_recovery_cancellation() {
  for refusal in ["foreign-store", "canceled"] {
    let (mut fixture, first) = Fixture::new(SelectorBehavior::Commit);
    fixture.publisher.publish(&first).unwrap();
    let state = Arc::clone(&fixture.state);
    drop(fixture.publisher);
    let cancellation = CancellationToken::new();
    let memory = memory();
    let recovered = recover_dirty_overlay(FakeStore::new(Arc::clone(&state)), &memory, &cancellation, 16 * 1024 * 1024, 32).unwrap();
    let IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Resumable(recovered) = recovered else {
      panic!("setup recovery was not resumable");
    };
    let mut store = FakeStore::new(state);
    if refusal == "foreign-store" {
      store.destination_id = [0x91; 16];
    } else {
      cancellation.cancel();
    }
    let virtual_clock: Arc<dyn VirtualClock> = fixture.clock;
    assert!(DurableIndexRuntimeBatchPublisherV1::new_resumed(recovered, store, virtual_clock).is_err(), "accepted {refusal}");
    assert_eq!(reserved_index_bytes(&memory), 0, "failed resumed construction leaked its recovery-state reservation");
  }
}

#[test]
fn dirty_overlay_recovery_and_resume_support_the_widest_hash_profile() {
  let algorithm = HashAlgorithm::Sha3_512;
  let (mut fixture, first) = Fixture::new_for(algorithm, SelectorBehavior::Commit);
  assert_eq!(fixture.publisher.publish(&first).unwrap().checkpoint_sequence, 1);
  let state = Arc::clone(&fixture.state);
  drop(fixture.publisher);
  let memory = memory();
  let recovered = recover_dirty_overlay(
    FakeStore::for_algorithm(Arc::clone(&state), algorithm),
    &memory,
    &CancellationToken::new(),
    16 * 1024 * 1024,
    32,
  )
  .unwrap();
  let IndexRuntimeDirtyOverlayRecoveryOutcomeV1::Resumable(recovered) = recovered else {
    panic!("64-byte hash profile did not recover");
  };
  assert_eq!(recovered.selected().checkpoint_key.len(), 64);
  assert_eq!(recovered.source_root().len(), 64);
  let virtual_clock: Arc<dyn VirtualClock> = fixture.clock;
  assert!(DurableIndexRuntimeBatchPublisherV1::new_resumed(recovered, FakeStore::for_algorithm(state, algorithm), virtual_clock,).is_ok());
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
  let recovery = include_str!("../../src/engine/v4/index_runtime_dirty_overlay_recovery.rs");
  let workspace = include_str!("../../src/engine/v4/index_runtime_workspace_store.rs");
  let immutable = source.find("self.store.put_immutable(&checkpoint)").unwrap();
  let selector = source.find("self.store.publish_selected_synced").unwrap();
  assert!(immutable < selector, "runtime checkpoint selector appears before immutable publication");
  assert!(!source.contains("V4FirstAuthorityPublisher"));
  assert!(!source.contains("latest_object_kind"));
  assert_eq!(workspace.matches("fn append_object(").count(), 1);
  assert_eq!(workspace.matches("fn selected_contains_producer_task(").count(), 1);
  assert!(!workspace.contains("payload_length != usize::MAX"));
  assert!(!recovery.contains("StorageEngine"));
  assert_eq!(source.matches("IndexProducerSpillStoreV1 for DurableIndexRuntimeBatchPublisherV1").count(), 1);
  for forbidden in [
    include_str!("../../src/engine/storage_engine.rs"),
    include_str!("../../src/engine/v4/index_runtime_installation.rs"),
    include_str!("../../src/engine/v4/index_runtime_owner.rs"),
  ] {
    assert!(!forbidden.contains("DurableIndexRuntimeBatchPublisherV1"));
    assert!(!forbidden.contains("recover_index_runtime_dirty_overlay_v1"));
  }
}

#[derive(Clone, Copy, Debug)]
enum DirtyCheckpointDeviation {
  TaskKind,
  State,
  Phase,
  Capabilities,
  StartedAt,
  TargetRoot,
  Journal,
  NextDocumentOrdinal,
  ResumeKey,
  Progress,
  ZeroProgress,
  Attachment,
  External,
  ExternalSequence,
  ExternalBytes,
}

fn replace_selected_checkpoint(state: &Arc<Mutex<FakeState>>, deviation: DirtyCheckpointDeviation) {
  let replacement = {
    let state = state.lock().unwrap();
    let selected = state.selected.as_ref().unwrap();
    let checkpoint = decode_index_task_checkpoint(state.immutable.get(&selected.checkpoint_key).unwrap(), ALGORITHM).unwrap();
    let external = checkpoint.external.unwrap();
    let mut required_capabilities = [0u8; 32];
    required_capabilities.copy_from_slice(checkpoint.required_capabilities);
    if matches!(deviation, DirtyCheckpointDeviation::Capabilities) {
      required_capabilities = [0; 32];
    }
    let target_root = digest_parts(ALGORITHM, &[b"foreign-target"]);
    let attachment_owner = digest_parts(ALGORITHM, &[b"unexpected-attachment-owner"]);
    let attachment_hash = digest_parts(ALGORITHM, &[b"unexpected-attachment"]);
    let journal_head = digest_parts(ALGORITHM, &[b"unexpected-journal"]);
    let attachments = [IndexTaskAttachmentWriteV1 {
      role: IndexTaskAttachmentRoleV1::CandidateFieldManifest,
      owner_id: &attachment_owner,
      artifact_hash: &attachment_hash,
      birth_generation: checkpoint.generation,
    }];
    let external_path = external.path.to_string();
    encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
      hash_algorithm: ALGORITHM,
      task_id: checkpoint.task_id,
      checkpoint_sequence: checkpoint.checkpoint_sequence,
      generation: checkpoint.generation,
      task_kind: if matches!(deviation, DirtyCheckpointDeviation::TaskKind) { IndexTaskKindV1::FieldBuild } else { checkpoint.task_kind },
      state: if matches!(deviation, DirtyCheckpointDeviation::State) { IndexTaskStateV1::FailedRetryable } else { checkpoint.state },
      phase: if matches!(deviation, DirtyCheckpointDeviation::Phase) { checkpoint.phase + 1 } else { checkpoint.phase },
      required_capabilities: &required_capabilities,
      started_at_ms: if matches!(deviation, DirtyCheckpointDeviation::StartedAt) { 0 } else { checkpoint.started_at_ms },
      updated_at_ms: checkpoint.updated_at_ms,
      source_root: checkpoint.source_root,
      target_root: matches!(deviation, DirtyCheckpointDeviation::TargetRoot).then_some(target_root.as_slice()),
      primary_id: Some(checkpoint.primary_id),
      journal_head: matches!(deviation, DirtyCheckpointDeviation::Journal).then_some(journal_head.as_slice()),
      journal_floor_sequence: 0,
      journal_audited_through: u64::from(matches!(deviation, DirtyCheckpointDeviation::Journal)),
      next_document_ordinal: u64::from(matches!(deviation, DirtyCheckpointDeviation::NextDocumentOrdinal)),
      completed_work: if matches!(deviation, DirtyCheckpointDeviation::ZeroProgress) { 0 } else { checkpoint.completed_work },
      total_work_hint: if matches!(deviation, DirtyCheckpointDeviation::ZeroProgress) {
        0
      } else if matches!(deviation, DirtyCheckpointDeviation::Progress) {
        checkpoint.total_work_hint + 1
      } else {
        checkpoint.total_work_hint
      },
      resume_key: if matches!(deviation, DirtyCheckpointDeviation::ResumeKey) { b"foreign-runtime-resume" } else { checkpoint.resume_key },
      attachments: if matches!(deviation, DirtyCheckpointDeviation::Attachment) { &attachments } else { &[] },
      external: (!matches!(deviation, DirtyCheckpointDeviation::External)).then_some(ExternalWorkspaceDescriptorWriteV1 {
        workspace_id: external.workspace_id,
        manifest_digest: external.manifest_digest,
        durable_sequence: if matches!(deviation, DirtyCheckpointDeviation::ExternalSequence) {
          external.durable_sequence + 1
        } else {
          external.durable_sequence
        },
        durable_bytes: if matches!(deviation, DirtyCheckpointDeviation::ExternalBytes) { 0 } else { external.durable_bytes },
        path: &external_path,
      }),
    })
    .unwrap()
  };
  let mut state = state.lock().unwrap();
  let root = IndexCheckpointRootV1::new(1, replacement.key.clone()).unwrap();
  state.immutable.insert(replacement.key, replacement.value);
  state.selected = Some(root);
}

fn recover_dirty_overlay(
  mut store: FakeStore,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  maximum_stored_bytes: u64,
  maximum_object_count: u64,
) -> Result<IndexRuntimeDirtyOverlayRecoveryOutcomeV1, IndexRuntimeDirtyOverlayRecoveryErrorV1> {
  let hash_algorithm = store.hash_algorithm;
  let database_id = store.database_id;
  let destination_id = store.destination_id;
  recover_index_runtime_dirty_overlay_v1(
    &mut store,
    hash_algorithm,
    database_id,
    destination_id,
    &owner_for(hash_algorithm),
    IndexRuntimeWorkspaceOptionsV1::new(None, maximum_stored_bytes, 0, maximum_object_count).unwrap(),
    memory,
    cancellation,
  )
}

fn assert_recovery_reason(outcome: IndexRuntimeDirtyOverlayRecoveryOutcomeV1, expected: IndexRuntimeDirtyOverlayRecoveryReasonV1) {
  let IndexRuntimeDirtyOverlayRecoveryOutcomeV1::ReconciliationRequired { reason, evidence } = outcome else {
    panic!("expected dirty-overlay reconciliation");
  };
  assert_eq!(reason, expected);
  assert!(evidence.as_ref().is_none_or(|evidence| evidence.len() <= 4 * 1024));
}

fn memory_with_limit(limit: u64) -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(limit - 1, limit, 1, 1).unwrap())
}

fn reserved_index_bytes(memory: &MemoryCoordinator) -> u64 {
  memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes
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
  owner_for(ALGORITHM)
}

fn owner_for(hash_algorithm: HashAlgorithm) -> IndexRecoveryOwnerV1 {
  IndexRecoveryOwnerV1::new(DATABASE_ID, digest_parts(hash_algorithm, &[b"runtime-index"]), OPERATION_ID).unwrap()
}

fn coordinator(memory: &MemoryCoordinator) -> IndexCoordinatorV1 {
  coordinator_for(memory, ALGORITHM)
}

fn coordinator_for(memory: &MemoryCoordinator, hash_algorithm: HashAlgorithm) -> IndexCoordinatorV1 {
  IndexCoordinatorV1::new(
    RUNTIME_ID,
    hash_algorithm,
    memory.clone(),
    IndexCoordinatorOptionsV1::new(1024 * 1024, 16, 1_000, 1024 * 1024).unwrap(),
    1_000,
  )
  .unwrap()
}

fn producer_coordinator(
  memory: &MemoryCoordinator,
  hash_algorithm: HashAlgorithm,
  max_pending_tasks: u32,
  max_attempts: u16,
) -> IndexProducerCoordinatorV1 {
  IndexProducerCoordinatorV1::new(
    hash_algorithm,
    memory.clone(),
    IndexProducerCoordinatorOptionsV1::new(max_pending_tasks, 1024 * 1024, max_attempts, 10, 1_000, 32, 1_024, 4 * 1024 * 1024).unwrap(),
  )
  .unwrap()
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

fn admit(coordinator: &mut IndexCoordinatorV1, ordinal: u64, publication_sequence: u64, now_ms: u64) {
  admit_for(coordinator, ALGORITHM, ordinal, publication_sequence, now_ms);
}

fn admit_for(coordinator: &mut IndexCoordinatorV1, hash_algorithm: HashAlgorithm, ordinal: u64, publication_sequence: u64, now_ms: u64) {
  let index_id = digest_parts(hash_algorithm, &[b"index", &ordinal.to_le_bytes()]);
  let file_key = digest_parts(hash_algorithm, &[b"file", &ordinal.to_le_bytes()]);
  let encoded_record =
    encode_scope_reverse_record(&ScopeReverseRecordV1 { document_ordinal: ordinal, file_key: &file_key }, hash_algorithm).unwrap();
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
