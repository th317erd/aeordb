use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

use aeordb::engine::durability_coordinator::DurabilityCoordinator;
use aeordb::engine::kv_stages::initial_block_size;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::database_header::{DATABASE_HEADER_V4_DATA_OFFSET, DatabaseHeaderV4, encode_database_header_slot};
use aeordb::engine::v4::first_authority::{FirstAuthorityPublicationRequestV1, PreparedNamespaceTreeV0, V4FirstAuthorityPublisher};
use aeordb::engine::v4::gc_retirement::{RetirementJournalBufferOptionsV1, RetirementJournalOwnerV1};
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_artifact::{
  EncodedImmutableIndexArtifactV1, ImmutableIndexArtifactKindV1, ImmutableIndexArtifactWriteV1, encode_immutable_index_artifact,
};
use aeordb::engine::v4::index_coordinator_recovery::{
  IndexCheckpointRootV1, IndexRecoveryOptionsV1, IndexRecoveryOwnerV1, IndexRecoveryStoreV1,
};
use aeordb::engine::v4::index_operation_control::IndexOperationKindV1;
use aeordb::engine::v4::index_recovery_store::{
  IndexScopeOrdinalStoreRegistryErrorV1, IndexScopeOrdinalStoreRegistryOptionsV1, IndexScopeOrdinalStoreRegistryV1,
  NativeIndexOperationDescriptorV1, NativeIndexRecoveryStoreV1, SharedRetirementJournalOwnerV1,
};
use aeordb::engine::v4::index_scope_ordinal_authority::{IndexScopeOrdinalStateStoreV1, IndexScopeOrdinalStoreObservationRequestV1};
use aeordb::engine::v4::index_task::{IndexTaskCheckpointWriteV1, IndexTaskKindV1, IndexTaskStateV1, encode_index_task_checkpoint};
use aeordb::engine::v4::namespace::{SemanticAvailabilityV1, SemanticStateWriteV1, SemanticUnavailableReasonV1, encode_semantic_state_object};
use aeordb::engine::{DiskKVStore, HashAlgorithm, MockClock, VirtualClock};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x31; 16];
const OPERATION_ID: [u8; 16] = [0xc4; 16];
const REQUIRED_CAPABILITIES: [u8; 32] = [0; 32];

struct Fixture {
  _directory: TempDir,
  path: PathBuf,
  publisher: Arc<V4FirstAuthorityPublisher>,
  descriptor: NativeIndexOperationDescriptorV1,
  retirement: SharedRetirementJournalOwnerV1,
  memory: Arc<MemoryCoordinator>,
  cancellation: CancellationToken,
  clock: Arc<MockClock>,
}

impl Fixture {
  fn new(name: &str) -> Self {
    let (directory, path, publisher) = publisher(name);
    publisher.publish(&first_authority_request()).unwrap();
    let memory = Arc::new(memory());
    let cancellation = CancellationToken::new();
    let retirement = Arc::new(Mutex::new(
      RetirementJournalOwnerV1::new_chain(
        HashAlgorithm::Blake3_256,
        DATABASE_ID,
        1,
        901,
        RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
        &cancellation,
        &memory,
      )
      .unwrap(),
    ));
    let descriptor = descriptor(0xc1, OPERATION_ID, 0xc3);
    Self {
      _directory: directory,
      path,
      publisher: Arc::new(publisher),
      descriptor,
      retirement,
      memory,
      cancellation,
      clock: Arc::new(MockClock::new(1, 1_700_000_000_200)),
    }
  }

  fn store(&self) -> NativeIndexRecoveryStoreV1 {
    let clock: Arc<dyn VirtualClock> = self.clock.clone();
    NativeIndexRecoveryStoreV1::new(self.descriptor.clone(), self.publisher.clone(), self.retirement.clone(), clock).unwrap()
  }

  fn owner(&self) -> IndexRecoveryOwnerV1 {
    IndexRecoveryOwnerV1::new(DATABASE_ID, self.descriptor.index_id().to_vec(), OPERATION_ID).unwrap()
  }

  fn registry(&self, maximum_entries: usize, maximum_resident_bytes: u64) -> IndexScopeOrdinalStoreRegistryV1 {
    let clock: Arc<dyn VirtualClock> = self.clock.clone();
    IndexScopeOrdinalStoreRegistryV1::new(
      IndexScopeOrdinalStoreRegistryOptionsV1::new(maximum_entries, maximum_resident_bytes).unwrap(),
      HashAlgorithm::Blake3_256,
      DATABASE_ID,
      recovery_options(),
      self.publisher.clone(),
      self.retirement.clone(),
      self.memory.clone(),
      self.cancellation.clone(),
      clock,
    )
    .unwrap()
  }
}

#[test]
fn native_store_batches_immutable_artifacts_and_selects_a_b_a_with_restart() {
  let fixture = Fixture::new("native-recovery-store");
  let mut store = fixture.store();
  let owner = fixture.owner();
  let unrelated = raw_artifact(0xe1, 1);
  let first = checkpoint(&fixture.descriptor, 1, 1);

  store.put_immutable_batch(&[&unrelated, &first]).unwrap();
  store.sync_immutable().unwrap();
  assert_eq!(store.immutable_length(&unrelated.key).unwrap(), Some(unrelated.value.len() as u64));
  assert_eq!(store.load_immutable(&unrelated.key, unrelated.value.len() as u64).unwrap(), Some(unrelated.value.clone()));
  let first_root = IndexCheckpointRootV1::new(1, first.key.clone()).unwrap();
  store.publish_selected_synced(&owner, None, &first_root).unwrap();
  assert_eq!(store.load_selected(&owner).unwrap(), Some(first_root.clone()));

  let second = checkpoint(&fixture.descriptor, 2, 1);
  fixture.clock.advance(1);
  store.put_immutable(&second).unwrap();
  let second_root = IndexCheckpointRootV1::new(2, second.key.clone()).unwrap();
  store.publish_selected_synced(&owner, Some(&first_root), &second_root).unwrap();
  assert_eq!(store.load_selected(&owner).unwrap(), Some(second_root.clone()));

  let third = checkpoint(&fixture.descriptor, 3, 1);
  fixture.clock.advance(1);
  store.put_immutable(&third).unwrap();
  let third_root = IndexCheckpointRootV1::new(3, third.key.clone()).unwrap();
  store.publish_selected_synced(&owner, Some(&second_root), &third_root).unwrap();
  assert_eq!(store.load_selected(&owner).unwrap(), Some(third_root.clone()));
  let retirement_status = fixture.retirement.lock().unwrap().status();
  assert_eq!(retirement_status.pending_records, 0);
  assert!(retirement_status.last_hard_publication_sequence > 0);

  let before_retry = fixture.publisher.observe().unwrap();
  store.publish_selected_synced(&owner, Some(&third_root), &third_root).unwrap();
  assert_eq!(fixture.publisher.observe().unwrap(), before_retry);

  drop(store);
  let Fixture { _directory, path, publisher, descriptor, retirement, memory: fixture_memory, cancellation, clock } = fixture;
  drop((publisher, retirement, fixture_memory, cancellation, clock));
  let reopened_publisher = Arc::new(reopen(&path));
  let reopened_memory = Arc::new(memory());
  let reopened_cancellation = CancellationToken::new();
  let reopened_retirement = Arc::new(Mutex::new(
    RetirementJournalOwnerV1::new_chain(
      HashAlgorithm::Blake3_256,
      DATABASE_ID,
      1,
      902,
      RetirementJournalBufferOptionsV1::new(1, 1024 * 1024, 30_000),
      &reopened_cancellation,
      &reopened_memory,
    )
    .unwrap(),
  ));
  let clock: Arc<dyn VirtualClock> = Arc::new(MockClock::new(2, 1_700_000_000_300));
  let mut reopened = NativeIndexRecoveryStoreV1::new(descriptor, reopened_publisher, reopened_retirement, clock).unwrap();
  assert_eq!(reopened.load_selected(&owner).unwrap(), Some(third_root));
}

#[test]
fn native_store_rejects_stale_wrong_owner_and_malformed_checkpoint_without_changing_selection() {
  let fixture = Fixture::new("native-recovery-errors");
  let mut store = fixture.store();
  let owner = fixture.owner();
  let first = checkpoint(&fixture.descriptor, 1, 1);
  store.put_immutable(&first).unwrap();
  let first_root = IndexCheckpointRootV1::new(1, first.key.clone()).unwrap();
  store.publish_selected_synced(&owner, None, &first_root).unwrap();

  let second = checkpoint(&fixture.descriptor, 2, 1);
  store.put_immutable(&second).unwrap();
  let second_root = IndexCheckpointRootV1::new(2, second.key.clone()).unwrap();
  let stale = IndexCheckpointRootV1::new(1, vec![0xee; HashAlgorithm::Blake3_256.hash_length()]).unwrap();
  assert!(store.publish_selected_synced(&owner, Some(&stale), &second_root).is_err());
  assert_eq!(store.load_selected(&owner).unwrap(), Some(first_root.clone()));

  let wrong_owner = IndexRecoveryOwnerV1::new(DATABASE_ID, vec![0xd1; 32], OPERATION_ID).unwrap();
  assert!(store.load_selected(&wrong_owner).is_err());

  let wrong_scope = checkpoint(&descriptor(0xd1, OPERATION_ID, 0xd3), 2, 1);
  store.put_immutable(&wrong_scope).unwrap();
  let wrong_root = IndexCheckpointRootV1::new(2, wrong_scope.key).unwrap();
  assert!(store.publish_selected_synced(&owner, Some(&first_root), &wrong_root).is_err());
  assert_eq!(store.load_selected(&owner).unwrap(), Some(first_root));
}

#[test]
fn registry_reuses_exact_identity_and_refuses_conflicting_descriptor() {
  let fixture = Fixture::new("native-registry-reuse");
  let registry = fixture.registry(4, 1024 * 1024);
  let first = registry.acquire(fixture.descriptor.clone()).unwrap();
  let second = registry.acquire(fixture.descriptor.clone()).unwrap();
  assert!(Arc::ptr_eq(&first, &second));
  let snapshot = registry.snapshot().unwrap();
  assert_eq!(snapshot.entries, 1);
  assert_eq!(snapshot.hits, 1);
  assert_eq!(snapshot.misses, 1);
  assert_eq!(snapshot.pinned_entries, 1);

  let conflict = descriptor(0xc1, OPERATION_ID, 0xd3);
  assert!(matches!(registry.acquire(conflict), Err(IndexScopeOrdinalStoreRegistryErrorV1::DescriptorConflict)));
  assert_eq!(registry.snapshot().unwrap().entries, 1);
}

#[test]
fn registry_cache_hit_does_not_reconstruct_native_adapter() {
  let fixture = Fixture::new("native-registry-fast-hit");
  let registry = fixture.registry(4, 1024 * 1024);
  let first = registry.acquire(fixture.descriptor.clone()).unwrap();
  fixture.clock.set_time(0);
  let second = registry.acquire(fixture.descriptor.clone()).unwrap();
  assert!(Arc::ptr_eq(&first, &second));
  assert!(matches!(registry.acquire(descriptor(0xd1, [0xd2; 16], 0xd3)), Err(IndexScopeOrdinalStoreRegistryErrorV1::Store(_))));
}

#[test]
fn registry_never_evicts_pinned_adapters_and_evicts_lru_after_release() {
  let fixture = Fixture::new("native-registry-pins");
  let registry = fixture.registry(1, 1024 * 1024);
  let first = registry.acquire(fixture.descriptor.clone()).unwrap();
  let second_descriptor = descriptor(0xd1, [0xd2; 16], 0xd3);
  assert!(matches!(registry.acquire(second_descriptor.clone()), Err(IndexScopeOrdinalStoreRegistryErrorV1::AllCandidatesPinned)));
  assert_eq!(registry.snapshot().unwrap().entries, 1);

  drop(first);
  let second = registry.acquire(second_descriptor).unwrap();
  let snapshot = registry.snapshot().unwrap();
  assert_eq!(snapshot.entries, 1);
  assert_eq!(snapshot.evictions, 1);
  assert_eq!(snapshot.pinned_entries, 1);
  drop(second);
  assert_eq!(registry.evict_all_unpinned().unwrap(), 1);
  assert_eq!(registry.snapshot().unwrap().entries, 0);
  assert_eq!(registry.snapshot().unwrap().resident_bytes, 0);
  assert_eq!(fixture.memory.snapshot().unwrap().owner(MemoryOwner::IndexCleanCache).unwrap().reserved_bytes, 0);
}

#[test]
fn registry_enforces_entry_byte_and_shared_memory_bounds() {
  let fixture = Fixture::new("native-registry-memory");
  let too_small = fixture.registry(1, 1);
  assert!(matches!(too_small.acquire(fixture.descriptor.clone()), Err(IndexScopeOrdinalStoreRegistryErrorV1::Invalid(_))));

  let tiny_memory = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(64, 128, 1, 32).unwrap()));
  let clock: Arc<dyn VirtualClock> = fixture.clock.clone();
  let registry = IndexScopeOrdinalStoreRegistryV1::new(
    IndexScopeOrdinalStoreRegistryOptionsV1::new(2, 1024 * 1024).unwrap(),
    HashAlgorithm::Blake3_256,
    DATABASE_ID,
    recovery_options(),
    fixture.publisher.clone(),
    fixture.retirement.clone(),
    tiny_memory,
    fixture.cancellation.clone(),
    clock,
  )
  .unwrap();
  assert!(matches!(registry.acquire(fixture.descriptor.clone()), Err(IndexScopeOrdinalStoreRegistryErrorV1::Memory(_))));
  assert_eq!(registry.snapshot().unwrap().entries, 0);
  assert_eq!(registry.snapshot().unwrap().resident_bytes, 0);
}

#[test]
fn native_store_rereads_selected_control_and_fails_closed_on_clock_rollback() {
  let fixture = Fixture::new("native-recovery-fresh-selection");
  let owner = fixture.owner();
  let mut writer = fixture.store();
  let mut observer = fixture.store();
  let first = checkpoint(&fixture.descriptor, 1, 1);
  writer.put_immutable(&first).unwrap();
  let first_root = IndexCheckpointRootV1::new(1, first.key).unwrap();
  writer.publish_selected_synced(&owner, None, &first_root).unwrap();
  assert_eq!(observer.load_selected(&owner).unwrap(), Some(first_root.clone()));

  let second = checkpoint(&fixture.descriptor, 2, 1);
  fixture.clock.advance(1);
  writer.put_immutable(&second).unwrap();
  let second_root = IndexCheckpointRootV1::new(2, second.key).unwrap();
  writer.publish_selected_synced(&owner, Some(&first_root), &second_root).unwrap();
  assert_eq!(observer.load_selected(&owner).unwrap(), Some(second_root.clone()));

  let third = checkpoint(&fixture.descriptor, 3, 1);
  fixture.clock.advance(1);
  writer.put_immutable(&third).unwrap();
  let third_root = IndexCheckpointRootV1::new(3, third.key).unwrap();
  writer.publish_selected_synced(&owner, Some(&second_root), &third_root).unwrap();
  fixture.clock.set_time(1_700_000_000_201);
  let fourth = checkpoint(&fixture.descriptor, 4, 1);
  writer.put_immutable(&fourth).unwrap();
  let fourth_root = IndexCheckpointRootV1::new(4, fourth.key).unwrap();
  assert!(writer.publish_selected_synced(&owner, Some(&third_root), &fourth_root).is_err());
  assert_eq!(observer.load_selected(&owner).unwrap(), Some(third_root));
}

#[test]
fn registry_serializes_same_identity_races_to_one_adapter() {
  let fixture = Fixture::new("native-registry-race");
  let registry = Arc::new(fixture.registry(8, 8 * 1024 * 1024));
  let barrier = Arc::new(Barrier::new(9));
  let mut workers = Vec::new();
  for _ in 0..8 {
    let registry = Arc::clone(&registry);
    let barrier = Arc::clone(&barrier);
    let descriptor = fixture.descriptor.clone();
    workers.push(thread::spawn(move || {
      barrier.wait();
      registry.acquire(descriptor).unwrap()
    }));
  }
  barrier.wait();
  let adapters: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
  for adapter in &adapters[1..] {
    assert!(Arc::ptr_eq(&adapters[0], adapter));
  }
  let snapshot = registry.snapshot().unwrap();
  assert_eq!(snapshot.entries, 1);
  assert_eq!(snapshot.misses, 1);
  assert_eq!(snapshot.hits, 7);
}

#[test]
fn registry_refuses_new_adapters_after_cancellation() {
  let fixture = Fixture::new("native-registry-cancelled");
  let registry = fixture.registry(4, 1024 * 1024);
  fixture.cancellation.cancel();
  assert!(matches!(registry.acquire(fixture.descriptor.clone()), Err(IndexScopeOrdinalStoreRegistryErrorV1::Canceled)));
  assert_eq!(registry.snapshot().unwrap().entries, 0);
}

#[test]
fn acquired_adapter_observes_shared_cancellation_and_poisoned_retirement_owner_is_rejected() {
  let fixture = Fixture::new("native-registry-adapter-cancelled");
  let registry = fixture.registry(4, 1024 * 1024);
  let adapter = registry.acquire(fixture.descriptor.clone()).unwrap();
  fixture.cancellation.cancel();
  let semantic_root = vec![0xa1; HashAlgorithm::Blake3_256.hash_length()];
  let error = adapter
    .observe_selected(IndexScopeOrdinalStoreObservationRequestV1 {
      scope_id: fixture.descriptor.index_id(),
      semantic_state_root: &semantic_root,
      operation_id: fixture.descriptor.operation_id(),
      before_file_key: None,
      after_file_key: None,
    })
    .unwrap_err();
  assert_eq!(error.code(), "scope_ordinal_store_cancelled");

  let retirement = Arc::clone(&fixture.retirement);
  assert!(thread::spawn(move || {
    let _guard = retirement.lock().unwrap();
    panic!("poison shared retirement owner");
  })
  .join()
  .is_err());
  let clock: Arc<dyn VirtualClock> = fixture.clock.clone();
  let error = NativeIndexRecoveryStoreV1::new(fixture.descriptor.clone(), fixture.publisher.clone(), fixture.retirement.clone(), clock)
    .err()
    .expect("poisoned retirement owner must reject native store construction");
  assert_eq!(error.code(), "native_index_retirement_poisoned");
}

#[test]
fn descriptor_and_registry_options_reject_noncanonical_inputs() {
  assert!(IndexScopeOrdinalStoreRegistryOptionsV1::new(0, 1).is_err());
  assert!(IndexScopeOrdinalStoreRegistryOptionsV1::new(1, 0).is_err());
  assert!(NativeIndexOperationDescriptorV1::new(
    HashAlgorithm::Blake3_256,
    [0; 16],
    vec![0xc1; 32],
    OPERATION_ID,
    IndexOperationKindV1::Build,
    vec![0xc3; 32],
    None,
    None,
  )
  .is_err());
  assert!(NativeIndexOperationDescriptorV1::new(
    HashAlgorithm::Blake3_256,
    DATABASE_ID,
    vec![0xc1; 31],
    OPERATION_ID,
    IndexOperationKindV1::Build,
    vec![0xc3; 32],
    None,
    None,
  )
  .is_err());
}

fn descriptor(index_fill: u8, operation_id: [u8; 16], definition_fill: u8) -> NativeIndexOperationDescriptorV1 {
  NativeIndexOperationDescriptorV1::new(
    HashAlgorithm::Blake3_256,
    DATABASE_ID,
    vec![index_fill; 32],
    operation_id,
    IndexOperationKindV1::Build,
    vec![definition_fill; 32],
    None,
    None,
  )
  .unwrap()
}

fn checkpoint(descriptor: &NativeIndexOperationDescriptorV1, sequence: u64, generation: u64) -> EncodedImmutableIndexArtifactV1 {
  let source_root = vec![0xa1; 32];
  encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
    hash_algorithm: HashAlgorithm::Blake3_256,
    task_id: descriptor.operation_id(),
    checkpoint_sequence: sequence,
    generation,
    task_kind: IndexTaskKindV1::ScopeBuild,
    state: IndexTaskStateV1::Running,
    phase: 2,
    required_capabilities: &REQUIRED_CAPABILITIES,
    started_at_ms: 1_700_000_000_100,
    updated_at_ms: 1_700_000_000_100 + sequence,
    source_root: &source_root,
    target_root: None,
    primary_id: Some(descriptor.index_id()),
    journal_head: None,
    journal_floor_sequence: 0,
    journal_audited_through: 0,
    next_document_ordinal: 1,
    completed_work: sequence,
    total_work_hint: 100,
    resume_key: &[],
    attachments: &[],
    external: None,
  })
  .unwrap()
}

fn raw_artifact(fill: u8, generation: u64) -> EncodedImmutableIndexArtifactV1 {
  let identity = vec![fill; 32];
  encode_immutable_index_artifact(&ImmutableIndexArtifactWriteV1 {
    kind: ImmutableIndexArtifactKindV1::PostingPage,
    hash_algorithm: HashAlgorithm::Blake3_256,
    generation,
    identity: &identity,
    body: &[0x91, 0x92],
  })
  .unwrap()
}

fn recovery_options() -> IndexRecoveryOptionsV1 {
  IndexRecoveryOptionsV1::new(128, 16 * 1024 * 1024, 128, 16 * 1024 * 1024).unwrap()
}

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(64 * 1024 * 1024, 96 * 1024 * 1024, 1, 8 * 1024 * 1024).unwrap())
}

fn initial_header(kv_block_length: u64) -> DatabaseHeaderV4 {
  DatabaseHeaderV4 {
    hash_algorithm: HashAlgorithm::Blake3_256,
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
  }
}

fn publisher(name: &str) -> (TempDir, PathBuf, V4FirstAuthorityPublisher) {
  let directory = tempfile::tempdir().unwrap();
  let path = directory.path().join(format!("{name}.aeordb"));
  let mut file = OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
  let kv_block_length = initial_block_size();
  let header = initial_header(kv_block_length);
  let slot = encode_database_header_slot(&header).unwrap();
  file.seek(SeekFrom::Start(0)).unwrap();
  file.write_all(&slot).unwrap();
  file.write_all(&slot).unwrap();
  let coordinator = Arc::new(DurabilityCoordinator::new());
  let kv = DiskKVStore::create_with_coordinator(
    file.try_clone().unwrap(),
    HashAlgorithm::Blake3_256,
    header.kv_block_offset,
    header.hot_tail_offset,
    0,
    coordinator.clone(),
  )
  .unwrap();
  file.sync_all().unwrap();
  (directory, path, V4FirstAuthorityPublisher::new(kv, coordinator).unwrap())
}

fn reopen(path: &Path) -> V4FirstAuthorityPublisher {
  V4FirstAuthorityPublisher::open(path).unwrap()
}

fn first_authority_request() -> FirstAuthorityPublicationRequestV1 {
  let semantic_state = encode_semantic_state_object(
    &SemanticStateWriteV1 {
      required_capabilities: [0; 32],
      availability: SemanticAvailabilityV1::ContentOnly { reason: SemanticUnavailableReasonV1::LegacyGlobalStateNotCaptured },
    },
    HashAlgorithm::Blake3_256,
  )
  .unwrap();
  FirstAuthorityPublicationRequestV1 {
    database_id: DATABASE_ID,
    transaction_id: [0x61; 16],
    created_at_ms: 1_700_000_000_100,
    namespace_tree: PreparedNamespaceTreeV0 { root_hash: digest_parts(HashAlgorithm::Blake3_256, &[b"dirc:"]), stored_value: Vec::new() },
    semantic_state,
    required_capabilities: [0; 32],
    typed_closure_digest: digest_parts(HashAlgorithm::Blake3_256, &[b"typed test closure"]),
    authority_identity: b"HEAD".to_vec(),
  }
}
