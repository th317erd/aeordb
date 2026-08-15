use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::index_artifact::{
  EncodedImmutableIndexArtifactV1, IndexManifestBodyV1, IndexManifestWriteV1, decode_index_manifest, encode_index_manifest,
};
use aeordb::engine::v4::index_coordinator_recovery::{
  IndexCheckpointRootV1, IndexRecoveryOptionsV1, IndexRecoveryOwnerV1, IndexRecoveryStoreErrorV1, IndexRecoveryStoreV1,
};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, PhysicalHintV1, decode_artifact_directory,
  decode_ordered_page, encode_artifact_directory,
};
use aeordb::engine::v4::index_scope_ordinal_authority::{
  IndexScopeOrdinalPublishOutcomeV1, IndexScopeOrdinalPublishRequestV1, IndexScopeOrdinalStateStoreErrorClassV1,
  IndexScopeOrdinalStateStoreV1, IndexScopeOrdinalStoreObservationRequestV1,
};
use aeordb::engine::v4::index_scope_ordinal_checkpoint::{decode_scope_ordinal_claim_resume_v1, encode_scope_ordinal_claim_resume_v1};
use aeordb::engine::v4::index_scope_ordinal_checkpoint_store::RecoveryIndexScopeOrdinalStateStoreV1;
use aeordb::engine::v4::index_task::{
  IndexTaskAttachmentRoleV1, IndexTaskAttachmentWriteV1, IndexTaskCheckpointWriteV1, IndexTaskKindV1, IndexTaskStateV1, JournalOwnerKindV1,
  MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_index_task_checkpoint,
  encode_index_task_checkpoint, encode_mutation_journal,
};
use aeordb::engine::{HashAlgorithm, MockClock};
use tokio_util::sync::CancellationToken;

const SYSTEM_JOURNAL_OWNER: [u8; 16] = *b"AEORIDXJOURNALV1";
const COVERAGE_SEQUENCE: u64 = 900;

fn fixture_root() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/fixtures/v4/index-artifact-v1")
}

fn profile_name(hash_algorithm: HashAlgorithm) -> &'static str {
  match hash_algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("scope checkpoint store fixtures exist only for the frozen v4 profiles"),
  }
}

fn fixture_bytes(hash_algorithm: HashAlgorithm, suffix: &str) -> Vec<u8> {
  fs::read(fixture_root().join(format!("aidx-{}-{suffix}", profile_name(hash_algorithm)))).unwrap()
}

fn hash(hash_algorithm: HashAlgorithm, byte: u8) -> Vec<u8> {
  vec![byte; hash_algorithm.hash_length()]
}

fn memory(soft: u64, hard: u64) -> Arc<MemoryCoordinator> {
  Arc::new(MemoryCoordinator::new(MemoryPolicy::new(soft, hard, 1, 1).unwrap()))
}

fn normal_memory() -> Arc<MemoryCoordinator> {
  memory(32 * 1_024 * 1_024, 64 * 1_024 * 1_024)
}

fn recovery_options() -> IndexRecoveryOptionsV1 {
  IndexRecoveryOptionsV1::new(16, 16 * 1_024 * 1_024, 16, 16 * 1_024 * 1_024).unwrap()
}

#[derive(Default)]
struct StoreState {
  artifacts: BTreeMap<Vec<u8>, Vec<u8>>,
  selected: BTreeMap<(Vec<u8>, [u8; 16]), IndexCheckpointRootV1>,
  events: Vec<String>,
  fail_event: Option<String>,
  fail_after_publish_remaining: usize,
  selected_load_count: usize,
  replace_selection_on_load: Option<(usize, IndexCheckpointRootV1)>,
}

#[derive(Clone, Default)]
struct RecordingStore {
  state: Arc<Mutex<StoreState>>,
}

impl RecordingStore {
  fn mutate(&self, change: impl FnOnce(&mut StoreState)) {
    change(&mut self.state.lock().unwrap());
  }

  fn clear_events(&self) {
    self.mutate(|state| state.events.clear());
  }

  fn events(&self) -> Vec<String> {
    self.state.lock().unwrap().events.clone()
  }

  fn selected(&self, owner: &IndexRecoveryOwnerV1) -> IndexCheckpointRootV1 {
    self.state.lock().unwrap().selected.get(&(owner.index_id().to_vec(), owner.operation_id())).cloned().unwrap()
  }

  fn artifact(&self, key: &[u8]) -> Vec<u8> {
    self.state.lock().unwrap().artifacts.get(key).cloned().unwrap()
  }

  fn record_event(state: &mut StoreState, event: String) -> Result<(), IndexRecoveryStoreErrorV1> {
    state.events.push(event.clone());
    if state.fail_event.as_deref() == Some(event.as_str()) {
      return Err(IndexRecoveryStoreErrorV1::new("injected_store_failure", event));
    }
    Ok(())
  }
}

impl IndexRecoveryStoreV1 for RecordingStore {
  fn immutable_length(&mut self, key: &[u8]) -> Result<Option<u64>, IndexRecoveryStoreErrorV1> {
    let mut state = self.state.lock().unwrap();
    Self::record_event(&mut state, format!("length:{}", hex::encode(key)))?;
    Ok(state.artifacts.get(key).map(|bytes| bytes.len() as u64))
  }

  fn load_immutable(&mut self, key: &[u8], expected_length: u64) -> Result<Option<Vec<u8>>, IndexRecoveryStoreErrorV1> {
    let mut state = self.state.lock().unwrap();
    Self::record_event(&mut state, format!("load:{}", hex::encode(key)))?;
    Ok(state.artifacts.get(key).filter(|bytes| bytes.len() as u64 == expected_length).cloned())
  }

  fn put_immutable(&mut self, artifact: &EncodedImmutableIndexArtifactV1) -> Result<(), IndexRecoveryStoreErrorV1> {
    let mut state = self.state.lock().unwrap();
    Self::record_event(&mut state, format!("put:{}", hex::encode(&artifact.key)))?;
    if let Some(existing) = state.artifacts.get(&artifact.key) {
      if existing != &artifact.value {
        return Err(IndexRecoveryStoreErrorV1::new("immutable_collision", "same immutable key has different bytes"));
      }
      return Ok(());
    }
    state.artifacts.insert(artifact.key.clone(), artifact.value.clone());
    Ok(())
  }

  fn sync_immutable(&mut self) -> Result<(), IndexRecoveryStoreErrorV1> {
    let mut state = self.state.lock().unwrap();
    Self::record_event(&mut state, "sync".to_string())
  }

  fn load_selected(&mut self, owner: &IndexRecoveryOwnerV1) -> Result<Option<IndexCheckpointRootV1>, IndexRecoveryStoreErrorV1> {
    let mut state = self.state.lock().unwrap();
    Self::record_event(&mut state, "load-selected".to_string())?;
    state.selected_load_count += 1;
    if state.replace_selection_on_load.as_ref().is_some_and(|(at, _)| *at == state.selected_load_count) {
      let (_, replacement) = state.replace_selection_on_load.take().unwrap();
      state.selected.insert((owner.index_id().to_vec(), owner.operation_id()), replacement);
    }
    Ok(state.selected.get(&(owner.index_id().to_vec(), owner.operation_id())).cloned())
  }

  fn publish_selected_synced(
    &mut self,
    owner: &IndexRecoveryOwnerV1,
    expected: Option<&IndexCheckpointRootV1>,
    next: &IndexCheckpointRootV1,
  ) -> Result<(), IndexRecoveryStoreErrorV1> {
    let mut state = self.state.lock().unwrap();
    Self::record_event(&mut state, "publish-selected".to_string())?;
    let key = (owner.index_id().to_vec(), owner.operation_id());
    if state.selected.get(&key) != expected {
      return Err(IndexRecoveryStoreErrorV1::new("selection_conflict", "selected checkpoint changed"));
    }
    state.selected.insert(key, next.clone());
    if state.fail_after_publish_remaining != 0 {
      state.fail_after_publish_remaining -= 1;
      return Err(IndexRecoveryStoreErrorV1::new("injected_commit_unknown", "selector advanced before the injected error"));
    }
    Ok(())
  }
}

#[derive(Clone, Copy, Default)]
struct GraphOptions {
  malformed_resume: bool,
  stale_resume_watermark: bool,
  reverse_directory_levels: u8,
  wrong_child_generation: bool,
}

struct TestGraph {
  hash_algorithm: HashAlgorithm,
  store: RecordingStore,
  owner: IndexRecoveryOwnerV1,
  selected: IndexCheckpointRootV1,
  generation: u64,
  semantic_state_root: Vec<u8>,
  live_file_key: Vec<u8>,
  missing_file_key: Vec<u8>,
  live_ordinal: u64,
  reverse_page_key: Vec<u8>,
  next_document_ordinal: u64,
}

impl TestGraph {
  fn adapter(
    &self,
    memory: Arc<MemoryCoordinator>,
    cancellation: CancellationToken,
  ) -> RecoveryIndexScopeOrdinalStateStoreV1<RecordingStore> {
    RecoveryIndexScopeOrdinalStateStoreV1::new(
      self.hash_algorithm,
      self.owner.clone(),
      recovery_options(),
      memory,
      cancellation,
      Arc::new(MockClock::new(7, 1_800_000_000_100)),
      self.store.clone(),
    )
    .unwrap()
  }

  fn observation(&self, operation_id: [u8; 16]) -> IndexScopeOrdinalStoreObservationRequestV1<'_> {
    IndexScopeOrdinalStoreObservationRequestV1 {
      scope_id: self.owner.index_id(),
      semantic_state_root: &self.semantic_state_root,
      operation_id,
      before_file_key: Some(&self.live_file_key),
      after_file_key: Some(&self.missing_file_key),
    }
  }

  fn publish<'a>(&'a self, operation_id: [u8; 16], request_fingerprint: &'a [u8]) -> IndexScopeOrdinalPublishRequestV1<'a> {
    IndexScopeOrdinalPublishRequestV1 {
      expected_checkpoint_sequence: self.selected.checkpoint_sequence,
      expected_checkpoint_key: &self.selected.checkpoint_key,
      generation: self.generation,
      scope_id: self.owner.index_id(),
      semantic_state_root: &self.semantic_state_root,
      operation_id,
      request_fingerprint,
      document_ordinal: self.next_document_ordinal,
      next_document_ordinal: self.next_document_ordinal + 1,
      source_publication_sequence: COVERAGE_SEQUENCE + 1,
    }
  }
}

fn build_graph(hash_algorithm: HashAlgorithm, options: GraphOptions) -> TestGraph {
  let store = RecordingStore::default();
  let ordinal_directory_bytes = fixture_bytes(hash_algorithm, "scope-ordinal-directory-leaf-valid.bin");
  let reverse_leaf_bytes = fixture_bytes(hash_algorithm, "scope-reverse-directory-leaf-valid.bin");
  let ordinal_page_bytes = fixture_bytes(hash_algorithm, "scope-ordinal-page-valid.bin");
  let reverse_page_bytes = fixture_bytes(hash_algorithm, "scope-reverse-page-valid.bin");
  let ordinal_directory = decode_artifact_directory(&ordinal_directory_bytes, hash_algorithm).unwrap();
  let reverse_leaf = decode_artifact_directory(&reverse_leaf_bytes, hash_algorithm).unwrap();
  let reverse_page = decode_ordered_page(&reverse_page_bytes, hash_algorithm).unwrap();
  assert_eq!(ordinal_directory.owner_id, reverse_leaf.owner_id);
  assert_eq!(reverse_leaf.role, OrderedIndexRoleV1::ScopeReverse);
  let live = reverse_page.records.iter().next().unwrap().unwrap();
  let live_file_key = live.file_key.unwrap().to_vec();
  let live_ordinal = live.document_ordinal;
  let reverse_page_key = reverse_page.key.clone();

  let mut reverse_root_key = reverse_leaf.key.clone();
  let mut reverse_root_generation = reverse_leaf.generation;
  let mut reverse_root_bytes = reverse_leaf_bytes.clone();
  if options.reverse_directory_levels != 0 {
    assert_eq!(options.reverse_directory_levels, 1);
    let child_generation =
      if options.wrong_child_generation { reverse_leaf.generation.checked_add(1).unwrap() } else { reverse_leaf.generation };
    let entries = [ArtifactDirectoryEntryWriteV1 {
      lower_fence: reverse_leaf.lower_fence,
      upper_fence: reverse_leaf.upper_fence,
      child_hash: &reverse_leaf.key,
      child_generation,
      live_count: reverse_leaf.live_count,
      tombstone_count: reverse_leaf.tombstone_count,
      page_count: reverse_leaf.page_count,
      logical_bytes: reverse_leaf.logical_bytes,
      minimum_page_id: reverse_leaf.minimum_page_id,
      maximum_page_id: reverse_leaf.maximum_page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }];
    let encoded = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      hash_algorithm,
      role: OrderedIndexRoleV1::ScopeReverse,
      owner_id: reverse_leaf.owner_id,
      generation: reverse_leaf.generation.checked_add(2).unwrap(),
      level: 1,
      entries: &entries,
    })
    .unwrap();
    reverse_root_key = encoded.key;
    reverse_root_generation = reverse_leaf.generation.checked_add(2).unwrap();
    reverse_root_bytes = encoded.value;
  }

  let scope_fixture_bytes = fixture_bytes(hash_algorithm, "scope-catalog-manifest-populated.bin");
  let scope_fixture = decode_index_manifest(&scope_fixture_bytes, hash_algorithm).unwrap();
  let IndexManifestBodyV1::ScopeCatalog(mut scope_body) = scope_fixture.details else {
    panic!("scope fixture decoded as another manifest kind");
  };
  scope_body.coverage.coverage_publication_sequence = COVERAGE_SEQUENCE;
  scope_body.ordinal_directory_root = Some(&ordinal_directory.key);
  scope_body.reverse_directory_root = Some(&reverse_root_key);
  scope_body.live_document_count = ordinal_directory.live_count;
  scope_body.retained_tombstone_count = ordinal_directory.tombstone_count;
  scope_body.ordinal_page_count = ordinal_directory.page_count;
  scope_body.reverse_page_count = reverse_leaf.page_count;
  scope_body.next_document_ordinal = scope_body.next_document_ordinal.max(live_ordinal + 1);
  let next_document_ordinal = scope_body.next_document_ordinal;
  let manifest_generation = scope_fixture.generation.max(ordinal_directory.generation).max(reverse_root_generation);
  let checkpoint_generation = manifest_generation.max(100);
  let scope_manifest = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm,
    generation: manifest_generation,
    owner_id: ordinal_directory.owner_id,
    body: IndexManifestBodyV1::ScopeCatalog(scope_body),
  })
  .unwrap();

  let source_root = hash(hash_algorithm, 0x11);
  let target_root = hash(hash_algorithm, 0x12);
  let semantic_state_root = hash(hash_algorithm, 0x13);
  let mutation_id = hash(hash_algorithm, 0x14);
  let revision = hash(hash_algorithm, 0x15);
  let journal = encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm,
    owner_id: SYSTEM_JOURNAL_OWNER,
    owner_kind: JournalOwnerKindV1::System,
    generation: checkpoint_generation,
    segment_ordinal: 1,
    chain_reset: true,
    previous_segment: &vec![0; hash_algorithm.hash_length()],
    semantic_state_root: &semantic_state_root,
    runtime_boot_id: [0x16; 16],
    records: &[MutationRecordWriteV1 {
      kind: MutationKindV1::Create,
      sequence: COVERAGE_SEQUENCE,
      mutation_id: &mutation_id,
      batch_ordinal: 0,
      batch_count: 1,
      root_before: &source_root,
      root_after: &target_root,
      before: None,
      after: Some(MutationSideWriteV1 { path: "/docs/guide.md", revision: &revision }),
      committed_at_ms: 1_800_000_000_000,
    }],
  })
  .unwrap();

  let operation_id = [0x21; 16];
  let owner = IndexRecoveryOwnerV1::new([0x22; 16], ordinal_directory.owner_id.to_vec(), operation_id).unwrap();
  let journal_attachment_owner = hash(hash_algorithm, 0x91);
  let mut attachments = vec![
    IndexTaskAttachmentWriteV1 {
      role: IndexTaskAttachmentRoleV1::ScopeOrdinalDirectoryRoot,
      owner_id: ordinal_directory.owner_id,
      artifact_hash: &ordinal_directory.key,
      birth_generation: ordinal_directory.generation,
    },
    IndexTaskAttachmentWriteV1 {
      role: IndexTaskAttachmentRoleV1::ScopeReverseDirectoryRoot,
      owner_id: ordinal_directory.owner_id,
      artifact_hash: &reverse_root_key,
      birth_generation: reverse_root_generation,
    },
    IndexTaskAttachmentWriteV1 {
      role: IndexTaskAttachmentRoleV1::CandidateScopeManifest,
      owner_id: ordinal_directory.owner_id,
      artifact_hash: &scope_manifest.key,
      birth_generation: manifest_generation,
    },
    IndexTaskAttachmentWriteV1 {
      role: IndexTaskAttachmentRoleV1::MutationJournalHead,
      owner_id: &journal_attachment_owner,
      artifact_hash: &journal.key,
      birth_generation: checkpoint_generation,
    },
  ];
  attachments.sort_by_key(|attachment| attachment.role);
  let applied_through_sequence = if options.stale_resume_watermark { COVERAGE_SEQUENCE - 1 } else { COVERAGE_SEQUENCE };
  let resume = if options.malformed_resume {
    b"not-a-sorc-payload".to_vec()
  } else {
    encode_scope_ordinal_claim_resume_v1(hash_algorithm, applied_through_sequence, &[]).unwrap()
  };
  let checkpoint = encode_index_task_checkpoint(&IndexTaskCheckpointWriteV1 {
    hash_algorithm,
    task_id: operation_id,
    checkpoint_sequence: 1,
    generation: checkpoint_generation,
    task_kind: IndexTaskKindV1::ScopeBuild,
    state: IndexTaskStateV1::Running,
    phase: 1,
    required_capabilities: &[0; 32],
    started_at_ms: 1_800_000_000_000,
    updated_at_ms: 1_800_000_000_001,
    source_root: &source_root,
    target_root: Some(&target_root),
    primary_id: Some(owner.index_id()),
    journal_head: Some(&journal.key),
    journal_floor_sequence: COVERAGE_SEQUENCE,
    journal_audited_through: COVERAGE_SEQUENCE,
    next_document_ordinal,
    completed_work: 1,
    total_work_hint: 2,
    resume_key: &resume,
    attachments: &attachments,
    external: None,
  })
  .unwrap();
  let selected = IndexCheckpointRootV1::new(1, checkpoint.key.clone()).unwrap();

  store.mutate(|state| {
    for (key, value) in [
      (ordinal_directory.key.clone(), ordinal_directory_bytes),
      (reverse_leaf.key.clone(), reverse_leaf_bytes),
      (reverse_root_key.clone(), reverse_root_bytes),
      (decode_ordered_page(&ordinal_page_bytes, hash_algorithm).unwrap().key, ordinal_page_bytes),
      (reverse_page.key.clone(), reverse_page_bytes),
      (scope_manifest.key.clone(), scope_manifest.value),
      (journal.key.clone(), journal.value),
      (checkpoint.key.clone(), checkpoint.value),
    ] {
      state.artifacts.insert(key, value);
    }
    state.selected.insert((owner.index_id().to_vec(), owner.operation_id()), selected.clone());
  });

  TestGraph {
    hash_algorithm,
    store,
    owner,
    selected,
    generation: checkpoint_generation,
    semantic_state_root,
    live_file_key,
    missing_file_key: hash(hash_algorithm, 0xee),
    live_ordinal,
    reverse_page_key,
    next_document_ordinal,
  }
}

fn reserved_index_bytes(memory: &MemoryCoordinator) -> u64 {
  memory.snapshot().unwrap().owner(MemoryOwner::IndexDirtyBuffers).unwrap().reserved_bytes
}

#[test]
fn selected_checkpoint_and_bounded_reverse_lookup_are_the_only_ordinal_authority() {
  for hash_algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let graph = build_graph(hash_algorithm, GraphOptions::default());
    let memory = normal_memory();
    let adapter = graph.adapter(memory.clone(), CancellationToken::new());
    let observed = adapter.observe_selected(graph.observation([0x31; 16])).unwrap();
    assert_eq!(observed.checkpoint_sequence, 1);
    assert_eq!(observed.checkpoint_key, graph.selected.checkpoint_key);
    assert_eq!(observed.scope_id, graph.owner.index_id());
    assert_eq!(observed.semantic_state_root, graph.semantic_state_root);
    assert_eq!(observed.before_live_ordinal, Some(graph.live_ordinal));
    assert_eq!(observed.after_live_ordinal, None);
    assert_eq!(observed.next_document_ordinal, graph.next_document_ordinal);
    assert_eq!(observed.pending_claim_count, 0);
    assert_eq!(observed.prior_operation_claim, None);
    assert_eq!(reserved_index_bytes(&memory), 0);
  }
}

#[test]
fn selector_last_publish_persists_one_sorted_claim_and_reopens_it_without_reallocation() {
  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions::default());
  let memory = normal_memory();
  let adapter = graph.adapter(memory.clone(), CancellationToken::new());
  graph.store.clear_events();
  let fingerprint = hash(graph.hash_algorithm, 0x41);
  let operation_id = [0x42; 16];
  assert_eq!(
    adapter.publish_selected_synced(graph.publish(operation_id, &fingerprint)).unwrap(),
    IndexScopeOrdinalPublishOutcomeV1::Committed
  );

  let events = graph.store.events();
  let sync = events.iter().rposition(|event| event == "sync").unwrap();
  let publish = events.iter().rposition(|event| event == "publish-selected").unwrap();
  assert!(sync < publish);
  let selected = graph.store.selected(&graph.owner);
  assert_eq!(selected.checkpoint_sequence, 2);
  let checkpoint_bytes = graph.store.artifact(&selected.checkpoint_key);
  let checkpoint = decode_index_task_checkpoint(&checkpoint_bytes, graph.hash_algorithm).unwrap();
  assert_eq!(checkpoint.next_document_ordinal, graph.next_document_ordinal + 1);
  let resume = decode_scope_ordinal_claim_resume_v1(checkpoint.resume_key, graph.hash_algorithm).unwrap();
  assert_eq!(resume.applied_through_sequence, COVERAGE_SEQUENCE);
  assert_eq!(resume.claims.len(), 1);
  assert_eq!(resume.claims[0].operation_id, operation_id);
  assert_eq!(resume.claims[0].request_fingerprint, fingerprint);
  assert_eq!(resume.claims[0].document_ordinal, graph.next_document_ordinal);
  assert_eq!(resume.claims[0].source_publication_sequence, COVERAGE_SEQUENCE + 1);

  let observed = adapter
    .observe_selected(IndexScopeOrdinalStoreObservationRequestV1 {
      scope_id: graph.owner.index_id(),
      semantic_state_root: &graph.semantic_state_root,
      operation_id,
      before_file_key: None,
      after_file_key: None,
    })
    .unwrap();
  let prior = observed.prior_operation_claim.unwrap();
  assert_eq!(prior.document_ordinal, graph.next_document_ordinal);
  assert_eq!(prior.request_fingerprint, fingerprint);
  assert_eq!(observed.pending_claim_count, 1);
  assert_eq!(reserved_index_bytes(&memory), 0);
}

#[test]
fn commit_unknown_reopens_the_exact_successor_as_committed_once() {
  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions::default());
  graph.store.mutate(|state| state.fail_after_publish_remaining = 1);
  let adapter = graph.adapter(normal_memory(), CancellationToken::new());
  graph.store.clear_events();
  let fingerprint = hash(graph.hash_algorithm, 0x51);
  let operation_id = [0x52; 16];
  assert_eq!(
    adapter.publish_selected_synced(graph.publish(operation_id, &fingerprint)).unwrap(),
    IndexScopeOrdinalPublishOutcomeV1::Committed
  );
  assert_eq!(graph.store.selected(&graph.owner).checkpoint_sequence, 2);
  assert_eq!(graph.store.events().iter().filter(|event| event.as_str() == "publish-selected").count(), 1);

  let observed = adapter
    .observe_selected(IndexScopeOrdinalStoreObservationRequestV1 {
      scope_id: graph.owner.index_id(),
      semantic_state_root: &graph.semantic_state_root,
      operation_id,
      before_file_key: None,
      after_file_key: None,
    })
    .unwrap();
  assert_eq!(observed.prior_operation_claim.unwrap().request_fingerprint, fingerprint);
  assert_eq!(
    adapter.publish_selected_synced(graph.publish(operation_id, &fingerprint)).unwrap(),
    IndexScopeOrdinalPublishOutcomeV1::SelectionChanged
  );
}

#[test]
fn selector_races_and_pre_selector_store_failures_never_report_committed() {
  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions::default());
  let foreign = IndexCheckpointRootV1::new(2, hash(graph.hash_algorithm, 0x61)).unwrap();
  graph.store.mutate(|state| state.replace_selection_on_load = Some((2, foreign)));
  let adapter = graph.adapter(normal_memory(), CancellationToken::new());
  let error = adapter.observe_selected(graph.observation([0x62; 16])).unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Retryable);
  assert_eq!(error.code(), "scope_ordinal_selection_changed");

  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions::default());
  graph.store.mutate(|state| state.fail_event = Some("sync".to_string()));
  let adapter = graph.adapter(normal_memory(), CancellationToken::new());
  graph.store.clear_events();
  let fingerprint = hash(graph.hash_algorithm, 0x63);
  let error = adapter.publish_selected_synced(graph.publish([0x64; 16], &fingerprint)).unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Retryable);
  assert_eq!(graph.store.selected(&graph.owner), graph.selected);
  assert!(!graph.store.events().iter().any(|event| event == "publish-selected"));

  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions::default());
  let adapter = graph.adapter(normal_memory(), CancellationToken::new());
  let fingerprint = hash(graph.hash_algorithm, 0x65);
  let mut stale = graph.publish([0x66; 16], &fingerprint);
  let foreign_checkpoint_key = hash(graph.hash_algorithm, 0x67);
  stale.expected_checkpoint_key = &foreign_checkpoint_key;
  assert_eq!(adapter.publish_selected_synced(stale).unwrap(), IndexScopeOrdinalPublishOutcomeV1::SelectionChanged);
}

#[test]
fn malformed_resume_watermark_and_reverse_children_fail_closed_as_corrupt() {
  for (options, expected_code) in [
    (GraphOptions { malformed_resume: true, ..GraphOptions::default() }, "scope_ordinal_resume_format"),
    (GraphOptions { stale_resume_watermark: true, ..GraphOptions::default() }, "scope_ordinal_resume_watermark"),
  ] {
    let graph = build_graph(HashAlgorithm::Blake3_256, options);
    let adapter = graph.adapter(normal_memory(), CancellationToken::new());
    let error = adapter.observe_selected(graph.observation([0x71; 16])).unwrap_err();
    assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Corrupt);
    assert_eq!(error.code(), expected_code);
  }

  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions::default());
  graph.store.mutate(|state| {
    state.artifacts.remove(&graph.reverse_page_key);
  });
  let adapter = graph.adapter(normal_memory(), CancellationToken::new());
  let error = adapter.observe_selected(graph.observation([0x72; 16])).unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Corrupt);
  assert_eq!(error.code(), "scope_ordinal_artifact_missing");

  let graph = build_graph(
    HashAlgorithm::Blake3_256,
    GraphOptions { reverse_directory_levels: 1, wrong_child_generation: true, ..GraphOptions::default() },
  );
  let adapter = graph.adapter(normal_memory(), CancellationToken::new());
  let error = adapter.observe_selected(graph.observation([0x73; 16])).unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Corrupt);
  assert_eq!(error.code(), "scope_ordinal_directory_child_closure");
}

#[test]
fn reverse_lookup_walks_a_multi_level_directory_without_retaining_prior_nodes() {
  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions { reverse_directory_levels: 1, ..GraphOptions::default() });
  let memory = normal_memory();
  let adapter = graph.adapter(memory.clone(), CancellationToken::new());
  let observed = adapter.observe_selected(graph.observation([0x81; 16])).unwrap();
  assert_eq!(observed.before_live_ordinal, Some(graph.live_ordinal));
  assert_eq!(reserved_index_bytes(&memory), 0);
}

#[test]
fn cancellation_memory_pressure_and_store_errors_are_retryable_and_release_reservations() {
  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions::default());
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let adapter = graph.adapter(normal_memory(), cancellation);
  let error = adapter.observe_selected(graph.observation([0x91; 16])).unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Retryable);
  assert_eq!(error.code(), "scope_ordinal_store_cancelled");

  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions::default());
  let constrained = memory(255, 256);
  let adapter = graph.adapter(constrained.clone(), CancellationToken::new());
  let error = adapter.observe_selected(graph.observation([0x92; 16])).unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Retryable);
  assert_eq!(error.code(), "scope_ordinal_recovery_retryable");
  assert_eq!(reserved_index_bytes(&constrained), 0);

  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions::default());
  graph.store.mutate(|state| state.fail_event = Some("load-selected".to_string()));
  let adapter = graph.adapter(normal_memory(), CancellationToken::new());
  let error = adapter.observe_selected(graph.observation([0x93; 16])).unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Retryable);
  assert_eq!(error.code(), "scope_ordinal_recovery_retryable");
}

#[test]
fn public_adapter_rejects_foreign_or_malformed_requests_before_storage_access() {
  let graph = build_graph(HashAlgorithm::Blake3_256, GraphOptions::default());
  let adapter = graph.adapter(normal_memory(), CancellationToken::new());
  graph.store.clear_events();
  let error = adapter
    .observe_selected(IndexScopeOrdinalStoreObservationRequestV1 {
      scope_id: graph.owner.index_id(),
      semantic_state_root: &[1; 31],
      operation_id: [0xa1; 16],
      before_file_key: None,
      after_file_key: None,
    })
    .unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Corrupt);
  assert_eq!(error.code(), "scope_ordinal_observation_identity");
  assert!(graph.store.events().is_empty());

  let fingerprint = hash(graph.hash_algorithm, 0xa2);
  let mut invalid = graph.publish([0xa3; 16], &fingerprint);
  invalid.expected_checkpoint_sequence = 0;
  let error = adapter.publish_selected_synced(invalid).unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Corrupt);
  assert_eq!(error.code(), "scope_ordinal_publish_identity");
  assert!(graph.store.events().is_empty());

  let mut non_advancing = graph.publish([0xa4; 16], &fingerprint);
  non_advancing.next_document_ordinal = non_advancing.document_ordinal;
  let error = adapter.publish_selected_synced(non_advancing).unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Corrupt);
  assert_eq!(error.code(), "scope_ordinal_publish_identity");
  assert!(graph.store.events().is_empty());

  let invalid_owner = IndexRecoveryOwnerV1::new([1; 16], vec![2; 31], [3; 16]).unwrap();
  let error = RecoveryIndexScopeOrdinalStateStoreV1::new(
    HashAlgorithm::Blake3_256,
    invalid_owner,
    recovery_options(),
    normal_memory(),
    CancellationToken::new(),
    Arc::new(MockClock::new(1, 1)),
    RecordingStore::default(),
  )
  .err()
  .unwrap();
  assert_eq!(error.class(), IndexScopeOrdinalStateStoreErrorClassV1::Corrupt);
  assert_eq!(error.code(), "scope_ordinal_store_owner");
}
