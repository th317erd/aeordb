use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use aeordb::engine::file_record::FileRecord;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_producer_source::{ResolvedIndexDocumentTransitionV1, ResolvedIndexDocumentV1};
use aeordb::engine::v4::index_scope_ordinal_authority::{
  DurableIndexScopeOrdinalAuthorityV1, IndexScopeOrdinalDurableClaimV1, IndexScopeOrdinalPublishOutcomeV1,
  IndexScopeOrdinalPublishRequestV1, IndexScopeOrdinalSelectedObservationV1, IndexScopeOrdinalStateOptionsV1,
  IndexScopeOrdinalStateStoreErrorV1, IndexScopeOrdinalStateStoreV1, IndexScopeOrdinalStoreObservationRequestV1,
};
use aeordb::engine::v4::index_semantic_source::{
  IndexScopeOrdinalAuthorityV1, IndexScopeOrdinalClaimErrorClassV1, IndexScopeOrdinalClaimRequestV1,
};
use aeordb::engine::HashAlgorithm;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;
const SCOPE_ID: [u8; 32] = [0x31; 32];
const SEMANTIC_ROOT: [u8; 32] = [0x41; 32];

fn document(path: &str, root: u8, revision: u8) -> ResolvedIndexDocumentV1 {
  ResolvedIndexDocumentV1 {
    namespace_root: vec![root; 32],
    revision_hash: vec![revision; 32],
    file_record: FileRecord {
      path: path.to_string(),
      content_type: Some("application/json".to_string()),
      total_size: 2,
      created_at: 1,
      updated_at: 2,
      metadata: Vec::new(),
      content_hash: vec![0x51; 32],
      chunk_hashes: vec![vec![0x61; 32]],
    },
  }
}

fn create(path: &str) -> ResolvedIndexDocumentTransitionV1 {
  ResolvedIndexDocumentTransitionV1 { before: None, after: Some(document(path, 0x11, 0x21)) }
}

fn update(path: &str) -> ResolvedIndexDocumentTransitionV1 {
  ResolvedIndexDocumentTransitionV1 { before: Some(document(path, 0x10, 0x20)), after: Some(document(path, 0x11, 0x21)) }
}

fn request<'a>(
  operation_id: [u8; 16],
  transition: &'a ResolvedIndexDocumentTransitionV1,
  before_in_scope: bool,
  after_in_scope: bool,
  is_cancelled: &'a dyn Fn() -> bool,
) -> IndexScopeOrdinalClaimRequestV1<'a> {
  IndexScopeOrdinalClaimRequestV1 {
    operation_id,
    semantic_state_root: &SEMANTIC_ROOT,
    scope_id: &SCOPE_ID,
    transition,
    before_in_scope,
    after_in_scope,
    is_cancelled,
  }
}

#[derive(Debug)]
struct StoreState {
  checkpoint_sequence: u64,
  checkpoint_key: Vec<u8>,
  generation: u64,
  next_document_ordinal: u64,
  claims: BTreeMap<[u8; 16], IndexScopeOrdinalDurableClaimV1>,
  live: BTreeMap<Vec<u8>, u64>,
  publish_count: usize,
  conflicts_remaining: usize,
  fail_observe: Option<IndexScopeOrdinalStateStoreErrorV1>,
  fail_publish: Option<IndexScopeOrdinalStateStoreErrorV1>,
  commit_unknown_remaining: usize,
  selected_scope_id: Vec<u8>,
  selected_semantic_root: Vec<u8>,
}

impl Default for StoreState {
  fn default() -> Self {
    Self {
      checkpoint_sequence: 7,
      checkpoint_key: vec![0x71; 32],
      generation: 9,
      next_document_ordinal: 17,
      claims: BTreeMap::new(),
      live: BTreeMap::new(),
      publish_count: 0,
      conflicts_remaining: 0,
      fail_observe: None,
      fail_publish: None,
      commit_unknown_remaining: 0,
      selected_scope_id: SCOPE_ID.to_vec(),
      selected_semantic_root: SEMANTIC_ROOT.to_vec(),
    }
  }
}

struct RecordingStore {
  state: Mutex<StoreState>,
  first_observation_barrier: Option<Arc<Barrier>>,
  observation_count: AtomicUsize,
  cancel_after_observe: Option<Arc<AtomicBool>>,
}

impl Default for RecordingStore {
  fn default() -> Self {
    Self {
      state: Mutex::new(StoreState::default()),
      first_observation_barrier: None,
      observation_count: AtomicUsize::new(0),
      cancel_after_observe: None,
    }
  }
}

impl RecordingStore {
  fn with_first_observation_barrier() -> Self {
    Self { first_observation_barrier: Some(Arc::new(Barrier::new(2))), ..Self::default() }
  }

  fn cancelling_after_observe(cancellation: Arc<AtomicBool>) -> Self {
    Self { cancel_after_observe: Some(cancellation), ..Self::default() }
  }

  fn file_key(path: &str) -> Vec<u8> {
    digest_parts(ALGORITHM, &[b"file:", path.as_bytes()])
  }

  fn mutate(&self, change: impl FnOnce(&mut StoreState)) {
    change(&mut self.state.lock().unwrap());
  }
}

impl IndexScopeOrdinalStateStoreV1 for RecordingStore {
  fn observe_selected(
    &self,
    request: IndexScopeOrdinalStoreObservationRequestV1<'_>,
  ) -> Result<IndexScopeOrdinalSelectedObservationV1, IndexScopeOrdinalStateStoreErrorV1> {
    let state = self.state.lock().unwrap();
    if let Some(error) = &state.fail_observe {
      return Err(error.clone());
    }
    let observation = IndexScopeOrdinalSelectedObservationV1 {
      checkpoint_sequence: state.checkpoint_sequence,
      checkpoint_key: state.checkpoint_key.clone(),
      generation: state.generation,
      scope_id: state.selected_scope_id.clone(),
      semantic_state_root: state.selected_semantic_root.clone(),
      next_document_ordinal: state.next_document_ordinal,
      pending_claim_count: u32::try_from(state.claims.len()).unwrap(),
      prior_operation_claim: state.claims.get(&request.operation_id).cloned(),
      before_live_ordinal: request.before_file_key.and_then(|key| state.live.get(key).copied()),
      after_live_ordinal: request.after_file_key.and_then(|key| state.live.get(key).copied()),
    };
    drop(state);
    if self.observation_count.fetch_add(1, Ordering::SeqCst) < 2 {
      if let Some(barrier) = &self.first_observation_barrier {
        barrier.wait();
      }
    }
    if let Some(cancellation) = &self.cancel_after_observe {
      cancellation.store(true, Ordering::SeqCst);
    }
    Ok(observation)
  }

  fn publish_selected_synced(
    &self,
    request: IndexScopeOrdinalPublishRequestV1<'_>,
  ) -> Result<IndexScopeOrdinalPublishOutcomeV1, IndexScopeOrdinalStateStoreErrorV1> {
    let mut state = self.state.lock().unwrap();
    state.publish_count += 1;
    if let Some(error) = &state.fail_publish {
      return Err(error.clone());
    }
    if state.conflicts_remaining != 0 {
      state.conflicts_remaining -= 1;
      let alien_ordinal = state.next_document_ordinal;
      state.next_document_ordinal += 1;
      state
        .claims
        .insert([0xfe; 16], IndexScopeOrdinalDurableClaimV1 { request_fingerprint: vec![0xfd; 32], document_ordinal: alien_ordinal });
      state.checkpoint_sequence += 1;
      state.checkpoint_key = vec![state.checkpoint_sequence as u8; 32];
      return Ok(IndexScopeOrdinalPublishOutcomeV1::SelectionChanged);
    }
    if state.checkpoint_sequence != request.expected_checkpoint_sequence
      || state.checkpoint_key != request.expected_checkpoint_key
      || state.generation != request.generation
    {
      return Ok(IndexScopeOrdinalPublishOutcomeV1::SelectionChanged);
    }
    state.checkpoint_sequence += 1;
    state.checkpoint_key = vec![state.checkpoint_sequence as u8; 32];
    state.next_document_ordinal = request.next_document_ordinal;
    state.claims.insert(
      request.operation_id,
      IndexScopeOrdinalDurableClaimV1 {
        request_fingerprint: request.request_fingerprint.to_vec(),
        document_ordinal: request.document_ordinal,
      },
    );
    if state.commit_unknown_remaining != 0 {
      state.commit_unknown_remaining -= 1;
      return Err(IndexScopeOrdinalStateStoreErrorV1::retryable(
        "injected_commit_unknown",
        "selector committed before the store returned an error",
      ));
    }
    Ok(IndexScopeOrdinalPublishOutcomeV1::Committed)
  }
}

fn authority(store: &RecordingStore, attempts: u16) -> DurableIndexScopeOrdinalAuthorityV1<'_, RecordingStore> {
  DurableIndexScopeOrdinalAuthorityV1::new(ALGORITHM, store, IndexScopeOrdinalStateOptionsV1::new(attempts, 64).unwrap())
}

#[test]
fn allocation_is_not_returned_until_claim_and_high_water_are_durably_selected() {
  let store = RecordingStore::default();
  let transition = create("/docs/a.json");
  let ordinal = authority(&store, 4).claim_scope_ordinal(request([1; 16], &transition, false, true, &|| false)).unwrap();

  assert_eq!(ordinal, 17);
  let state = store.state.lock().unwrap();
  assert_eq!(state.next_document_ordinal, 18);
  assert_eq!(state.claims.get(&[1; 16]).unwrap().document_ordinal, 17);
  assert_eq!(state.publish_count, 1);
}

#[test]
fn exact_retry_reuses_the_durable_claim_without_republishing_or_advancing() {
  let store = RecordingStore::default();
  let transition = create("/docs/a.json");
  let first = authority(&store, 4).claim_scope_ordinal(request([1; 16], &transition, false, true, &|| false)).unwrap();
  let retry = authority(&store, 4).claim_scope_ordinal(request([1; 16], &transition, false, true, &|| false)).unwrap();

  assert_eq!((first, retry), (17, 17));
  let state = store.state.lock().unwrap();
  assert_eq!(state.next_document_ordinal, 18);
  assert_eq!(state.publish_count, 1);
}

#[test]
fn selector_conflict_reloads_authority_and_allocates_from_the_new_high_water() {
  let store = RecordingStore::default();
  store.mutate(|state| state.conflicts_remaining = 1);
  let transition = create("/docs/a.json");
  let ordinal = authority(&store, 4).claim_scope_ordinal(request([1; 16], &transition, false, true, &|| false)).unwrap();

  assert_eq!(ordinal, 18);
  let state = store.state.lock().unwrap();
  assert_eq!(state.publish_count, 2);
  assert_eq!(state.next_document_ordinal, 19);
}

#[test]
fn concurrent_operations_cannot_receive_the_same_scope_ordinal() {
  let store = RecordingStore::with_first_observation_barrier();
  let mut ordinals = std::thread::scope(|scope| {
    let first = scope.spawn(|| {
      let transition = create("/docs/a.json");
      authority(&store, 4).claim_scope_ordinal(request([1; 16], &transition, false, true, &|| false)).unwrap()
    });
    let second = scope.spawn(|| {
      let transition = create("/docs/b.json");
      authority(&store, 4).claim_scope_ordinal(request([2; 16], &transition, false, true, &|| false)).unwrap()
    });
    vec![first.join().unwrap(), second.join().unwrap()]
  });
  ordinals.sort_unstable();

  assert_eq!(ordinals, vec![17, 18]);
  let state = store.state.lock().unwrap();
  assert_eq!(state.next_document_ordinal, 19);
  assert_eq!(state.claims.len(), 2);
  assert_eq!(state.publish_count, 3);
}

#[test]
fn operation_identity_reuse_for_a_different_transition_is_corruption() {
  let store = RecordingStore::default();
  let first = create("/docs/a.json");
  authority(&store, 4).claim_scope_ordinal(request([1; 16], &first, false, true, &|| false)).unwrap();
  let conflicting = create("/docs/b.json");
  let error = authority(&store, 4).claim_scope_ordinal(request([1; 16], &conflicting, false, true, &|| false)).unwrap_err();

  assert_eq!(error.class(), IndexScopeOrdinalClaimErrorClassV1::Corrupt);
  assert_eq!(error.code(), "scope_ordinal_operation_conflict");
}

#[test]
fn exact_retry_after_commit_unknown_recovers_the_selected_claim_without_double_allocation() {
  let store = RecordingStore::default();
  store.mutate(|state| state.commit_unknown_remaining = 1);
  let transition = create("/docs/a.json");
  let unknown = authority(&store, 4).claim_scope_ordinal(request([9; 16], &transition, false, true, &|| false)).unwrap_err();
  assert_eq!(unknown.class(), IndexScopeOrdinalClaimErrorClassV1::Retryable);
  assert_eq!(unknown.code(), "injected_commit_unknown");

  let recovered = authority(&store, 4).claim_scope_ordinal(request([9; 16], &transition, false, true, &|| false)).unwrap();
  assert_eq!(recovered, 17);
  let state = store.state.lock().unwrap();
  assert_eq!(state.next_document_ordinal, 18);
  assert_eq!(state.publish_count, 1);
}

#[test]
fn durable_claim_and_live_reverse_mapping_must_agree_on_retry() {
  let store = RecordingStore::default();
  let transition = create("/docs/a.json");
  authority(&store, 4).claim_scope_ordinal(request([10; 16], &transition, false, true, &|| false)).unwrap();
  store.mutate(|state| {
    state.live.insert(RecordingStore::file_key("/docs/a.json"), 11);
  });

  let error = authority(&store, 4).claim_scope_ordinal(request([10; 16], &transition, false, true, &|| false)).unwrap_err();
  assert_eq!(error.class(), IndexScopeOrdinalClaimErrorClassV1::Corrupt);
  assert_eq!(error.code(), "scope_ordinal_claim_mapping_conflict");
}

#[test]
fn preserving_a_live_ordinal_still_persists_an_exact_retry_claim() {
  let store = RecordingStore::default();
  store.mutate(|state| {
    state.live.insert(RecordingStore::file_key("/docs/a.json"), 11);
  });
  let transition = update("/docs/a.json");
  let ordinal = authority(&store, 4).claim_scope_ordinal(request([2; 16], &transition, true, true, &|| false)).unwrap();

  assert_eq!(ordinal, 11);
  let state = store.state.lock().unwrap();
  assert_eq!(state.next_document_ordinal, 17);
  assert_eq!(state.claims.get(&[2; 16]).unwrap().document_ordinal, 11);
}

#[test]
fn stale_selected_identity_and_malformed_live_mappings_fail_closed() {
  let store = RecordingStore::default();
  let transition = update("/docs/a.json");
  store.mutate(|state| state.selected_semantic_root = vec![0x99; 32]);
  let stale = authority(&store, 4).claim_scope_ordinal(request([3; 16], &transition, true, true, &|| false)).unwrap_err();
  assert_eq!(stale.code(), "scope_ordinal_selected_identity");

  store.mutate(|state| {
    state.selected_semantic_root = SEMANTIC_ROOT.to_vec();
    state.live.insert(RecordingStore::file_key("/docs/a.json"), 0);
  });
  let malformed = authority(&store, 4).claim_scope_ordinal(request([3; 16], &transition, true, true, &|| false)).unwrap_err();
  assert_eq!(malformed.code(), "scope_ordinal_selected_mapping");
}

#[test]
fn cancellation_and_conflict_exhaustion_return_without_an_unproven_claim() {
  let store = RecordingStore::default();
  let transition = create("/docs/a.json");
  let cancelled = authority(&store, 4).claim_scope_ordinal(request([4; 16], &transition, false, true, &|| true)).unwrap_err();
  assert_eq!(cancelled.class(), IndexScopeOrdinalClaimErrorClassV1::Cancelled);
  assert_eq!(store.state.lock().unwrap().publish_count, 0);

  store.mutate(|state| state.conflicts_remaining = 3);
  let exhausted = authority(&store, 2).claim_scope_ordinal(request([4; 16], &transition, false, true, &|| false)).unwrap_err();
  assert_eq!(exhausted.class(), IndexScopeOrdinalClaimErrorClassV1::Retryable);
  assert_eq!(exhausted.code(), "scope_ordinal_selection_busy");
  assert!(!store.state.lock().unwrap().claims.contains_key(&[4; 16]));
}

#[test]
fn cancellation_after_observation_prevents_durable_publication() {
  let cancellation = Arc::new(AtomicBool::new(false));
  let store = RecordingStore::cancelling_after_observe(cancellation.clone());
  let transition = create("/docs/a.json");
  let error = authority(&store, 4)
    .claim_scope_ordinal(request([11; 16], &transition, false, true, &|| cancellation.load(Ordering::SeqCst)))
    .unwrap_err();

  assert_eq!(error.class(), IndexScopeOrdinalClaimErrorClassV1::Cancelled);
  let state = store.state.lock().unwrap();
  assert_eq!(state.publish_count, 0);
  assert!(state.claims.is_empty());
}

#[test]
fn pending_claim_pressure_refuses_new_allocation_but_preserves_exact_retry() {
  let store = RecordingStore::default();
  let bounded = DurableIndexScopeOrdinalAuthorityV1::new(ALGORITHM, &store, IndexScopeOrdinalStateOptionsV1::new(4, 1).unwrap());
  let first = create("/docs/a.json");
  assert_eq!(bounded.claim_scope_ordinal(request([7; 16], &first, false, true, &|| false)).unwrap(), 17);
  assert_eq!(bounded.claim_scope_ordinal(request([7; 16], &first, false, true, &|| false)).unwrap(), 17);

  let second = create("/docs/b.json");
  let pressure = bounded.claim_scope_ordinal(request([8; 16], &second, false, true, &|| false)).unwrap_err();
  assert_eq!(pressure.class(), IndexScopeOrdinalClaimErrorClassV1::Retryable);
  assert_eq!(pressure.code(), "scope_ordinal_claim_pressure");
  assert_eq!(store.state.lock().unwrap().next_document_ordinal, 18);
}

#[test]
fn every_store_failure_preserves_its_retryable_or_corrupt_class() {
  let transition = create("/docs/a.json");
  for (observe, class) in [(true, IndexScopeOrdinalClaimErrorClassV1::Retryable), (false, IndexScopeOrdinalClaimErrorClassV1::Corrupt)] {
    let store = RecordingStore::default();
    let error = if observe {
      IndexScopeOrdinalStateStoreErrorV1::retryable("injected_observe", "read failed")
    } else {
      IndexScopeOrdinalStateStoreErrorV1::corrupt("injected_publish", "selected bytes are malformed")
    };
    store.mutate(|state| {
      if observe {
        state.fail_observe = Some(error);
      } else {
        state.fail_publish = Some(error);
      }
    });
    let actual = authority(&store, 4).claim_scope_ordinal(request([5; 16], &transition, false, true, &|| false)).unwrap_err();
    assert_eq!(actual.class(), class);
    assert_eq!(store.state.lock().unwrap().claims.len(), 0);
  }
}

#[test]
fn malformed_requests_and_zero_retry_limit_are_rejected_before_store_access() {
  assert!(IndexScopeOrdinalStateOptionsV1::new(0, 1).is_err());
  assert!(IndexScopeOrdinalStateOptionsV1::new(1, 0).is_err());
  let store = RecordingStore::default();
  let transition = create("/docs/a.json");
  let zero_operation = authority(&store, 4).claim_scope_ordinal(request([0; 16], &transition, false, true, &|| false)).unwrap_err();
  assert_eq!(zero_operation.code(), "scope_ordinal_request_identity");
  let missing_side = authority(&store, 4).claim_scope_ordinal(request([6; 16], &transition, true, false, &|| false)).unwrap_err();
  assert_eq!(missing_side.code(), "scope_ordinal_request_transition");
  assert_eq!(store.state.lock().unwrap().publish_count, 0);
}
