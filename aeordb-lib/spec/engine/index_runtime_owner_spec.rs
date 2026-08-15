use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, mpsc};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::namespace_mutation::{NamespaceMutationAcknowledgement, NamespaceMutationKind, NamespaceMutationSourceIdentity};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::coverage_runtime::{SoftMutationAdmissionV1, SoftMutationHubOptionsV1, SoftMutationHubV1, SoftMutationLossReasonV1};
use aeordb::engine::v4::index_coordinator::{FrozenIndexBatchV1, IndexCoordinatorOptionsV1, IndexFlushReasonV1};
use aeordb::engine::v4::index_producer_collector::{
  IndexParserExecutionErrorV1, IndexParserExecutionRequestV1, IndexParserExecutorV1, IndexParserOutcomeV1, IndexProducerCollectorOptionsV1,
};
use aeordb::engine::v4::index_producer_coordinator::{
  IndexProducerCoordinatorOptionsV1, IndexProducerSpillErrorV1, IndexProducerSpillReasonV1, IndexProducerSpillReceiptV1,
  IndexProducerSpillStoreV1, IndexProducerTaskKindV1, IndexProducerTaskRequestV1, IndexProducerTaskViewV1,
};
use aeordb::engine::v4::index_producer_source::{
  IndexFileRevisionReadErrorV1, IndexFileRevisionSourceV1, IndexSemanticScopeLimitsV1, IndexSemanticScopeReadErrorV1,
  IndexSemanticScopeReadRequestV1, IndexSemanticScopeReadV1, IndexSemanticScopeResolutionV1, IndexSemanticScopeSourceV1,
  LoadedIndexFileRevisionV1, OwnedIndexFieldDefinitionV1, OwnedIndexScopeDefinitionV1, OwnedIndexValueStoreDefinitionV1,
  ResolvedIndexScopeWorkV1,
};
use aeordb::engine::v4::index_runtime_owner::{
  IndexRuntimeBatchPublisherV1, IndexRuntimeErrorV1, IndexRuntimeFlushOutcomeV1, IndexRuntimeLifecycleV1,
  IndexRuntimeMutationWorkRequestV1, IndexRuntimeOwnerOptionsV1, IndexRuntimeOwnerV1, IndexRuntimePublicationErrorClassV1,
  IndexRuntimePublicationErrorV1, IndexRuntimePublicationReceiptV1, IndexRuntimeRecoveryDecisionV1, IndexRuntimeWorkOutcomeV1,
};
use aeordb::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_mutation_journal,
  encode_mutation_journal,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn memory() -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(48 * 1_024 * 1_024, 64 * 1_024 * 1_024, 1, 8 * 1_024 * 1_024).unwrap())
}

fn options() -> IndexRuntimeOwnerOptionsV1 {
  IndexRuntimeOwnerOptionsV1 {
    soft_hub: SoftMutationHubOptionsV1::new(32, 256 * 1_024, 64 * 1_024).unwrap(),
    producer: IndexProducerCoordinatorOptionsV1::new(32, 2 * 1_024 * 1_024, 3, 10, 1_000, 16, 256, 2 * 1_024 * 1_024).unwrap(),
    mutations: IndexCoordinatorOptionsV1::new(4 * 1_024 * 1_024, 262_144, 30_000, 256 * 1_024).unwrap(),
    collector: IndexProducerCollectorOptionsV1::new(8, 16, 32, 2 * 1_024 * 1_024, 256, 2 * 1_024 * 1_024, 50).unwrap(),
    semantic: IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 * 1_024 * 1_024).unwrap(),
    source_retry_after_ms: 25,
    publication_retry_after_ms: 100,
  }
}

fn owner() -> IndexRuntimeOwnerV1 {
  IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, memory(), options(), 1).unwrap()
}

#[test]
fn runtime_owner_and_storage_boundary_share_exactly_one_soft_hub() {
  let shared_hub = Arc::new(SoftMutationHubV1::new(options().soft_hub).unwrap());
  let owner = IndexRuntimeOwnerV1::new_with_soft_hub([0x44; 16], ALGORITHM, memory(), options(), 1, Arc::clone(&shared_hub)).unwrap();

  assert_eq!(shared_hub.offer_acknowledgement(&acknowledgement("/from-engine.json".to_string(), 7)), SoftMutationAdmissionV1::Accepted);
  assert_eq!(owner.snapshot().unwrap().soft_hub.queued_notices, 1);
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 7 }).unwrap();
  let recovered = owner.cached_snapshot();
  assert_eq!(recovered.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(recovered.soft_hub.queued_notices, 1);

  assert_eq!(owner.offer_acknowledgement(&acknowledgement("/from-owner.json".to_string(), 8)), SoftMutationAdmissionV1::Accepted);
  assert_eq!(owner.cached_snapshot().soft_hub.queued_notices, 2);
  let snapshot = shared_hub.snapshot().unwrap();
  assert_eq!(snapshot.queued_notices, 2);
  assert_eq!(snapshot.latest_queued_publication_sequence, Some(8));
}

#[test]
fn shared_soft_hub_capacity_mismatch_is_rejected_without_memory_reservation() {
  let coordinator = memory();
  let mismatched = Arc::new(SoftMutationHubV1::new(SoftMutationHubOptionsV1::new(31, 256 * 1_024, 64 * 1_024).unwrap()).unwrap());
  assert!(matches!(
    IndexRuntimeOwnerV1::new_with_soft_hub([0x44; 16], ALGORITHM, coordinator.clone(), options(), 1, mismatched),
    Err(IndexRuntimeErrorV1::Invalid(_))
  ));
  assert_eq!(reserved(&coordinator, MemoryOwner::IndexDirtyBuffers), 0);
}

fn reserved(memory: &MemoryCoordinator, owner: MemoryOwner) -> u64 {
  memory.snapshot().unwrap().owner(owner).map_or(0, |state| state.reserved_bytes)
}

fn hash(label: &[u8]) -> Vec<u8> {
  aeordb::engine::v4::hash::digest_parts(ALGORITHM, &[b"runtime-owner:", label])
}

fn fixture(folder: &str, name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{folder}/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn encoded_journal(path: &str) -> Vec<u8> {
  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: ALGORITHM,
    owner_id: [0x31; 16],
    owner_kind: JournalOwnerKindV1::Task,
    generation: 1,
    segment_ordinal: 0,
    chain_reset: true,
    previous_segment: &[0; 32],
    semantic_state_root: &hash(b"semantic"),
    runtime_boot_id: [0x32; 16],
    records: &[MutationRecordWriteV1 {
      kind: MutationKindV1::Create,
      sequence: 7,
      mutation_id: &hash(b"mutation"),
      batch_ordinal: 0,
      batch_count: 1,
      root_before: &hash(b"before-root"),
      root_after: &hash(b"after-root"),
      before: None,
      after: Some(MutationSideWriteV1 { path, revision: &hash(b"after-revision") }),
      committed_at_ms: 100,
    }],
  })
  .unwrap()
  .value
}

fn file(path: &str) -> FileRecord {
  FileRecord {
    path: path.to_string(),
    content_type: Some("application/json".to_string()),
    total_size: 32,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_001,
    metadata: Vec::new(),
    content_hash: vec![0x41; 32],
    chunk_hashes: vec![vec![0x42; 32]],
  }
}

#[derive(Default)]
struct RevisionSource {
  records: BTreeMap<(Vec<u8>, String), LoadedIndexFileRevisionV1>,
}

impl IndexFileRevisionSourceV1 for RevisionSource {
  fn load_file_revision(
    &self,
    namespace_root: &[u8],
    path: &str,
  ) -> Result<Option<LoadedIndexFileRevisionV1>, IndexFileRevisionReadErrorV1> {
    Ok(self.records.get(&(namespace_root.to_vec(), path.to_string())).cloned())
  }
}

struct SemanticSource {
  semantic_state_root: Vec<u8>,
}

impl IndexSemanticScopeSourceV1 for SemanticSource {
  fn resolve_scopes(
    &self,
    _request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    let resolution = IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: self.semantic_state_root.clone(),
      scope_work: vec![scope_work(&self.semantic_state_root, 3)],
    };
    let reservation = memory()
      .reserve(MemoryOwner::Task, 2 * 1_024 * 1_024, AdmissionClass::Workload)
      .map_err(|error| IndexSemanticScopeReadErrorV1::retryable("test_memory", error.to_string()))?;
    IndexSemanticScopeReadV1::new(resolution, reservation)
  }
}

fn scope_work(semantic_state_root: &[u8], ordinal: u64) -> ResolvedIndexScopeWorkV1 {
  let scope = fixture("scope-definition-v1", "ascp-blake3-256-root-direct-valid.bin");
  let scope_id = decode_scope_definition(&scope, ALGORITHM).unwrap().scope_id;
  let mut value = fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid.bin");
  value[32..64].copy_from_slice(&scope_id);
  let value_id = decode_value_store_definition(&value, ALGORITHM).unwrap().value_store_id;
  let mut field = fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  field[32..64].copy_from_slice(&value_id);
  let field_id = decode_field_index_definition(&field, ALGORITHM).unwrap().index_id;
  ResolvedIndexScopeWorkV1 {
    semantic_state_root: semantic_state_root.to_vec(),
    document_ordinal: ordinal,
    scope: OwnedIndexScopeDefinitionV1 {
      scope_id,
      encoded_definition: scope,
      value_stores: vec![OwnedIndexValueStoreDefinitionV1 {
        value_store_id: value_id,
        encoded_definition: value,
        field_indexes: vec![OwnedIndexFieldDefinitionV1 { index_id: field_id, encoded_definition: field }],
      }],
    },
  }
}

struct UnexpectedParser;

impl IndexParserExecutorV1 for UnexpectedParser {
  fn parse(&self, _request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    Err(IndexParserExecutionErrorV1::host_failure("unexpected_parser", "metadata-only definition invoked parser"))
  }
}

fn revision_source(encoded: &[u8]) -> RevisionSource {
  let journal = decode_mutation_journal(encoded, ALGORITHM).unwrap();
  let record = journal.records.iter().next().unwrap().unwrap();
  let mut source = RevisionSource::default();
  source.records.insert(
    (record.root_after.to_vec(), record.after_path.unwrap().to_string()),
    LoadedIndexFileRevisionV1 { revision_hash: record.after_revision.unwrap().to_vec(), file_record: file(record.after_path.unwrap()) },
  );
  source
}

enum PublishBehavior {
  Success,
  Retryable,
  CommitUnknown,
  Dishonest,
  StaleCheckpoint,
  MalformedError,
  OversizedErrorCode,
  Panic,
  Cancelled,
}

struct Publisher {
  behavior: PublishBehavior,
  calls: usize,
}

struct BlockingPublisher {
  entered: mpsc::Sender<()>,
  release: mpsc::Receiver<()>,
}

impl IndexRuntimeBatchPublisherV1 for BlockingPublisher {
  fn publish(&mut self, batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    self.entered.send(()).unwrap();
    self.release.recv_timeout(Duration::from_secs(2)).unwrap();
    Ok(IndexRuntimePublicationReceiptV1 {
      batch_id: batch.batch_id(),
      attempt_id: batch.attempt_id(),
      published_records: batch.records().len() as u64,
      publication_bytes: batch.publication_bytes(),
      checkpoint_sequence: 12,
    })
  }
}

impl IndexRuntimeBatchPublisherV1 for Publisher {
  fn publish(&mut self, batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    self.calls += 1;
    match self.behavior {
      PublishBehavior::Success => Ok(IndexRuntimePublicationReceiptV1 {
        batch_id: batch.batch_id(),
        attempt_id: batch.attempt_id(),
        published_records: batch.records().len() as u64,
        publication_bytes: batch.publication_bytes(),
        checkpoint_sequence: 11,
      }),
      PublishBehavior::Retryable => Err(IndexRuntimePublicationErrorV1::new(
        IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection,
        "store_busy",
        "injected pre-selection refusal",
      )),
      PublishBehavior::CommitUnknown => Err(IndexRuntimePublicationErrorV1::new(
        IndexRuntimePublicationErrorClassV1::CommitUnknown,
        "selector_unknown",
        "injected selector uncertainty",
      )),
      PublishBehavior::Dishonest => Ok(IndexRuntimePublicationReceiptV1 {
        batch_id: batch.batch_id(),
        attempt_id: batch.attempt_id(),
        published_records: 0,
        publication_bytes: batch.publication_bytes(),
        checkpoint_sequence: 11,
      }),
      PublishBehavior::StaleCheckpoint => Ok(IndexRuntimePublicationReceiptV1 {
        batch_id: batch.batch_id(),
        attempt_id: batch.attempt_id(),
        published_records: batch.records().len() as u64,
        publication_bytes: batch.publication_bytes(),
        checkpoint_sequence: 1,
      }),
      PublishBehavior::MalformedError => Err(IndexRuntimePublicationErrorV1::new(
        IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection,
        "",
        "missing stable error code",
      )),
      PublishBehavior::OversizedErrorCode => Err(IndexRuntimePublicationErrorV1::new(
        IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection,
        Box::leak("x".repeat(129).into_boxed_str()),
        "amplified stable error code",
      )),
      PublishBehavior::Panic => panic!("injected publisher panic"),
      PublishBehavior::Cancelled => Err(IndexRuntimePublicationErrorV1::new(
        IndexRuntimePublicationErrorClassV1::CancelledBeforeSelection,
        "shutdown",
        "injected cancellation before selection",
      )),
    }
  }
}

fn execute_one(owner: &IndexRuntimeOwnerV1, encoded: &[u8], now_ms: u64) -> IndexRuntimeWorkOutcomeV1 {
  let journal = decode_mutation_journal(encoded, ALGORITHM).unwrap();
  let revisions = revision_source(encoded);
  let semantics = SemanticSource { semantic_state_root: journal.semantic_state_root.to_vec() };
  owner
    .execute_next_mutation(
      IndexRuntimeMutationWorkRequestV1 {
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms,
        is_cancelled: &|| false,
      },
      &mut Spill,
    )
    .unwrap()
}

fn task<'a>(root_before: &'a [u8], root_after: &'a [u8], semantic: &'a [u8], journal: &'a [u8]) -> IndexProducerTaskRequestV1<'a> {
  IndexProducerTaskRequestV1 {
    operation_id: [0x55; 16],
    kind: IndexProducerTaskKindV1::MutationWindow,
    publication_sequence: 7,
    namespace_root_before: root_before,
    namespace_root_after: root_after,
    semantic_state_root: semantic,
    journal_head: Some(journal),
    scope: None,
  }
}

fn acknowledgement(path: String, publication_sequence: u64) -> NamespaceMutationAcknowledgement {
  NamespaceMutationAcknowledgement {
    operation_id: uuid::Uuid::from_bytes([0x71; 16]),
    kind: NamespaceMutationKind::FileWrite,
    publication_sequence,
    previous_root_hash: vec![0x11; 32],
    root_hash: vec![0x12; 32],
    source_identities: vec![NamespaceMutationSourceIdentity {
      path,
      entry_type: Some(1),
      previous_identity: None,
      new_identity: Some(vec![0x13; 32]),
    }],
    locator_replacements: Vec::new(),
  }
}

struct Spill;

impl IndexProducerSpillStoreV1 for Spill {
  fn spill(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    IndexProducerSpillReceiptV1::new(task.operation_id(), vec![0x61; 32])
  }
}

#[test]
fn recovery_is_a_fail_closed_gate_for_task_admission() {
  let owner = owner();
  let root_before = [0x11; 32];
  let root_after = [0x12; 32];
  let semantic = [0x13; 32];
  let journal = [0x14; 32];

  assert_eq!(owner.snapshot().unwrap().lifecycle, IndexRuntimeLifecycleV1::Recovering);
  assert!(matches!(
    owner.admit_task(task(&root_before, &root_after, &semantic, &journal), 2, &mut Spill),
    Err(IndexRuntimeErrorV1::NotRunning { lifecycle: IndexRuntimeLifecycleV1::Recovering })
  ));

  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 2, highest_checkpoint_sequence: 9 }).unwrap();
  owner.admit_task(task(&root_before, &root_after, &semantic, &journal), 3, &mut Spill).unwrap();
  assert_eq!(owner.cached_snapshot().producer.pending_tasks, 1);
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(snapshot.producer.pending_tasks, 1);
}

#[test]
fn reconciliation_required_latches_degraded_and_cannot_be_downgraded_to_ready() {
  let owner = owner();
  owner
    .complete_recovery(IndexRuntimeRecoveryDecisionV1::ReconciliationRequired {
      code: "checkpoint_missing",
      context: "selected checkpoint was absent".to_string(),
    })
    .unwrap();
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.degraded.unwrap().code, "checkpoint_missing");
  assert!(matches!(
    owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }),
    Err(IndexRuntimeErrorV1::RecoveryAlreadyResolved { lifecycle: IndexRuntimeLifecycleV1::Degraded })
  ));
}

#[test]
fn soft_loss_during_recovery_prevents_a_ready_transition() {
  let owner = owner();
  assert!(!matches!(
    owner.offer_acknowledgement(&acknowledgement(format!("/{}", "x".repeat(70 * 1_024)), 8)),
    SoftMutationAdmissionV1::Accepted
  ));

  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.degraded.unwrap().code, "soft_mutation_loss_during_recovery");
}

#[test]
fn graceful_stop_refuses_queued_work_and_stops_only_after_complete_drain() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let root_before = [0x21; 32];
  let root_after = [0x22; 32];
  let semantic = [0x23; 32];
  let journal = [0x24; 32];
  owner.admit_task(task(&root_before, &root_after, &semantic, &journal), 2, &mut Spill).unwrap();

  owner.begin_draining().unwrap();
  assert!(matches!(owner.finish_draining(), Err(IndexRuntimeErrorV1::DrainIncomplete { pending_tasks: 1, .. })));
  assert_eq!(owner.snapshot().unwrap().lifecycle, IndexRuntimeLifecycleV1::Draining);
  assert!(matches!(
    owner.admit_task(task(&root_before, &root_after, &semantic, &journal), 3, &mut Spill),
    Err(IndexRuntimeErrorV1::NotRunning { lifecycle: IndexRuntimeLifecycleV1::Draining })
  ));
}

#[test]
fn graceful_stop_refuses_an_unconsumed_soft_notice() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  assert!(matches!(owner.offer_acknowledgement(&acknowledgement("/doc.json".to_string(), 8)), SoftMutationAdmissionV1::Accepted));

  owner.begin_draining().unwrap();
  assert!(matches!(
    owner.finish_draining(),
    Err(IndexRuntimeErrorV1::DrainIncomplete { pending_soft_notices: 1, soft_reconciliation_required: false, .. })
  ));
  assert_eq!(owner.snapshot().unwrap().lifecycle, IndexRuntimeLifecycleV1::Draining);
}

#[test]
fn stopped_owner_refuses_soft_acknowledgements_instead_of_stranding_them() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 0, highest_checkpoint_sequence: 0 }).unwrap();
  owner.begin_draining().unwrap();
  owner.finish_draining().unwrap();

  assert_eq!(
    owner.offer_acknowledgement(&acknowledgement("/after-stop.json".to_string(), 9)),
    SoftMutationAdmissionV1::ReconciliationRequired(SoftMutationLossReasonV1::QueueUnavailable)
  );
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Stopped);
  assert_eq!(snapshot.soft_hub.queued_notices, 0);
  assert!(snapshot.soft_hub.reconciliation_required);
}

#[test]
fn one_owner_executes_the_real_worker_and_restores_retryable_publication_before_stopping() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  assert!(matches!(execute_one(&owner, &encoded, 101), IndexRuntimeWorkOutcomeV1::Completed(_)));
  assert_eq!(owner.snapshot().unwrap().mutations.active_records, 4);

  let mut retryable = Publisher { behavior: PublishBehavior::Retryable, calls: 0 };
  assert!(matches!(owner.flush(102, Some(IndexFlushReasonV1::Explicit), false, &mut retryable), Err(IndexRuntimeErrorV1::Publication(_))));
  let restored = owner.snapshot().unwrap();
  assert_eq!(restored.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(restored.mutations.active_records, 4);
  assert_eq!(restored.mutations.frozen_records, 0);
  assert_eq!(retryable.calls, 1);

  let mut success = Publisher { behavior: PublishBehavior::Success, calls: 0 };
  assert_eq!(
    owner.flush(150, Some(IndexFlushReasonV1::Explicit), false, &mut success).unwrap(),
    IndexRuntimeFlushOutcomeV1::Deferred { retry_at_ms: 202 }
  );
  assert!(matches!(
    owner.flush(202, Some(IndexFlushReasonV1::Explicit), false, &mut success).unwrap(),
    IndexRuntimeFlushOutcomeV1::Published { records: 4, .. }
  ));
  assert_eq!(success.calls, 1);
  owner.begin_draining().unwrap();
  owner.finish_draining().unwrap();
  assert_eq!(owner.snapshot().unwrap().lifecycle, IndexRuntimeLifecycleV1::Stopped);
}

#[test]
fn uncertain_malformed_or_dishonest_publication_keeps_the_exact_frozen_batch_and_latches_degraded() {
  for behavior in [
    PublishBehavior::CommitUnknown,
    PublishBehavior::Dishonest,
    PublishBehavior::StaleCheckpoint,
    PublishBehavior::MalformedError,
    PublishBehavior::OversizedErrorCode,
    PublishBehavior::Panic,
  ] {
    let owner = owner();
    owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
    let encoded = encoded_journal("/doc.json");
    let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
    owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
    execute_one(&owner, &encoded, 101);

    let mut publisher = Publisher { behavior, calls: 0 };
    assert!(matches!(
      owner.flush(102, Some(IndexFlushReasonV1::Explicit), false, &mut publisher),
      Err(IndexRuntimeErrorV1::Publication(_))
    ));
    let snapshot = owner.snapshot().unwrap();
    assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
    assert_eq!(snapshot.mutations.active_records, 0);
    assert_eq!(snapshot.mutations.frozen_records, 4);
    assert!(snapshot.degraded.is_some());
  }
}

#[test]
fn retry_deadline_overflow_restores_the_batch_and_latches_degraded() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  execute_one(&owner, &encoded, 101);

  let mut publisher = Publisher { behavior: PublishBehavior::Retryable, calls: 0 };
  assert!(matches!(
    owner.flush(u64::MAX, Some(IndexFlushReasonV1::Explicit), false, &mut publisher),
    Err(IndexRuntimeErrorV1::Publication(_))
  ));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.mutations.active_records, 4);
  assert_eq!(snapshot.mutations.frozen_records, 0);
  assert_eq!(snapshot.degraded.unwrap().code, "publication_retry_deadline");
}

#[test]
fn publication_io_does_not_hold_runtime_state_or_allow_a_second_publisher() {
  let owner = Arc::new(owner());
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  execute_one(&owner, &encoded, 101);

  let (entered_tx, entered_rx) = mpsc::channel();
  let (release_tx, release_rx) = mpsc::channel();
  let publishing_owner = Arc::clone(&owner);
  let publisher_thread = std::thread::spawn(move || {
    let mut publisher = BlockingPublisher { entered: entered_tx, release: release_rx };
    publishing_owner.flush(102, Some(IndexFlushReasonV1::Explicit), false, &mut publisher)
  });
  entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
  let cached_during = owner.cached_snapshot();
  assert!(cached_during.publication_in_flight);
  assert_eq!(cached_during.mutations.active_records, 0);
  assert_eq!(cached_during.mutations.frozen_records, 4);

  let (snapshot_tx, snapshot_rx) = mpsc::channel();
  let snapshot_owner = Arc::clone(&owner);
  let snapshot_thread = std::thread::spawn(move || snapshot_tx.send(snapshot_owner.snapshot()).unwrap());
  let during = snapshot_rx.recv_timeout(Duration::from_secs(1));
  assert!(during.is_ok(), "runtime snapshot blocked behind publication I/O");
  assert_eq!(during.unwrap().unwrap().mutations.frozen_records, 4);

  let mut second = Publisher { behavior: PublishBehavior::Success, calls: 0 };
  assert!(matches!(
    owner.flush(103, Some(IndexFlushReasonV1::Explicit), false, &mut second),
    Err(IndexRuntimeErrorV1::PublicationInProgress { .. })
  ));
  assert_eq!(second.calls, 0);

  release_tx.send(()).unwrap();
  assert!(matches!(publisher_thread.join().unwrap().unwrap(), IndexRuntimeFlushOutcomeV1::Published { records: 4, .. }));
  let cached_after = owner.cached_snapshot();
  assert!(!cached_after.publication_in_flight);
  assert_eq!(cached_after.mutations.frozen_records, 0);
  snapshot_thread.join().unwrap();
}

#[test]
fn owner_reserves_the_soft_queue_capacity_for_its_complete_lifetime() {
  let coordinator = memory();
  assert_eq!(reserved(&coordinator, MemoryOwner::IndexDirtyBuffers), 0);
  let owner = IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, coordinator.clone(), options(), 1).unwrap();
  assert!(reserved(&coordinator, MemoryOwner::IndexDirtyBuffers) > 256 * 1_024);
  drop(owner);
  assert_eq!(reserved(&coordinator, MemoryOwner::IndexDirtyBuffers), 0);

  let tiny = MemoryCoordinator::new(MemoryPolicy::new(128 * 1_024, 192 * 1_024, 1, 32 * 1_024).unwrap());
  assert!(matches!(IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, tiny.clone(), options(), 1), Err(IndexRuntimeErrorV1::Memory(_))));
  assert_eq!(reserved(&tiny, MemoryOwner::IndexDirtyBuffers), 0);
}

#[test]
fn content_only_recovery_can_start_and_stop_without_fabricating_a_checkpoint() {
  let content_only = owner();
  content_only.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 0, highest_checkpoint_sequence: 0 }).unwrap();
  let snapshot = content_only.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(snapshot.highest_checkpoint_sequence, 0);
  content_only.begin_draining().unwrap();
  assert_eq!(content_only.cached_snapshot().lifecycle, IndexRuntimeLifecycleV1::Draining);
  content_only.finish_draining().unwrap();
  assert_eq!(content_only.cached_snapshot().lifecycle, IndexRuntimeLifecycleV1::Stopped);
  assert_eq!(content_only.snapshot().unwrap().lifecycle, IndexRuntimeLifecycleV1::Stopped);

  let invalid = owner();
  assert!(matches!(
    invalid.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 0, highest_checkpoint_sequence: 1 }),
    Err(IndexRuntimeErrorV1::Invalid(_))
  ));
  assert_eq!(invalid.snapshot().unwrap().lifecycle, IndexRuntimeLifecycleV1::Recovering);
}

#[test]
fn soft_handoff_loss_is_observed_before_any_task_can_enter_the_runtime() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let acknowledgement = acknowledgement(format!("/{}", "x".repeat(70 * 1_024)), 8);
  assert!(!matches!(
    owner.offer_acknowledgement(&acknowledgement),
    aeordb::engine::v4::coverage_runtime::SoftMutationAdmissionV1::Accepted
  ));

  let root_before = [0x21; 32];
  let root_after = [0x22; 32];
  let semantic = [0x23; 32];
  let journal = [0x24; 32];
  assert!(matches!(
    owner.admit_task(task(&root_before, &root_after, &semantic, &journal), 9, &mut Spill),
    Err(IndexRuntimeErrorV1::NotRunning { lifecycle: IndexRuntimeLifecycleV1::Degraded })
  ));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.degraded.unwrap().code, "soft_mutation_loss");
  assert_eq!(snapshot.producer.pending_tasks, 0);
}

#[test]
fn soft_handoff_loss_blocks_publication_before_storage_io() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  execute_one(&owner, &encoded, 101);
  assert!(!matches!(
    owner.offer_acknowledgement(&acknowledgement(format!("/{}", "x".repeat(70 * 1_024)), 9)),
    SoftMutationAdmissionV1::Accepted
  ));

  let mut publisher = Publisher { behavior: PublishBehavior::Success, calls: 0 };
  assert!(matches!(
    owner.flush(102, Some(IndexFlushReasonV1::Explicit), false, &mut publisher),
    Err(IndexRuntimeErrorV1::NotRunning { lifecycle: IndexRuntimeLifecycleV1::Degraded })
  ));
  assert_eq!(publisher.calls, 0);
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.mutations.active_records, 4);
  assert_eq!(snapshot.mutations.frozen_records, 0);
  assert_eq!(snapshot.degraded.unwrap().code, "soft_mutation_loss");
}

struct FailingSemanticSource {
  retryable: bool,
}

struct PanickingSemanticSource;

struct BlockingSemanticSource {
  entered: mpsc::Sender<()>,
  release: Mutex<mpsc::Receiver<()>>,
}

impl IndexSemanticScopeSourceV1 for BlockingSemanticSource {
  fn resolve_scopes(
    &self,
    _request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    self.entered.send(()).unwrap();
    self.release.lock().unwrap().recv().unwrap();
    Err(IndexSemanticScopeReadErrorV1::retryable("store_busy", "injected blocked semantic read"))
  }
}

impl IndexSemanticScopeSourceV1 for PanickingSemanticSource {
  fn resolve_scopes(
    &self,
    _request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    panic!("injected semantic source panic")
  }
}

#[test]
fn cached_observability_does_not_wait_for_worker_owned_runtime_state() {
  let owner = Arc::new(owner());
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  let (entered_tx, entered_rx) = mpsc::channel();
  let (release_tx, release_rx) = mpsc::channel();
  let semantics = Arc::new(BlockingSemanticSource { entered: entered_tx, release: Mutex::new(release_rx) });
  let worker_owner = Arc::clone(&owner);
  let worker_semantics = Arc::clone(&semantics);
  let worker = std::thread::spawn(move || {
    let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
    let revisions = revision_source(&encoded);
    worker_owner.execute_next_mutation(
      IndexRuntimeMutationWorkRequestV1 {
        journal: &journal,
        revision_source: &revisions,
        semantic_source: worker_semantics.as_ref(),
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 101,
        is_cancelled: &|| false,
      },
      &mut Spill,
    )
  });
  entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

  let (snapshot_tx, snapshot_rx) = mpsc::channel();
  let snapshot_owner = Arc::clone(&owner);
  let snapshot = std::thread::spawn(move || snapshot_tx.send(snapshot_owner.cached_snapshot()).unwrap());
  let cached = snapshot_rx.recv_timeout(Duration::from_secs(1)).expect("cached runtime observability blocked behind worker state");
  assert_eq!(cached.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(cached.producer.pending_tasks, 1);

  release_tx.send(()).unwrap();
  assert!(worker.join().unwrap().is_ok());
  snapshot.join().unwrap();
}

impl IndexSemanticScopeSourceV1 for FailingSemanticSource {
  fn resolve_scopes(
    &self,
    _request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    if self.retryable {
      Err(IndexSemanticScopeReadErrorV1::retryable("store_busy", "injected retryable semantic read"))
    } else {
      Err(IndexSemanticScopeReadErrorV1::corrupt("semantic_corrupt", "injected terminal semantic corruption"))
    }
  }
}

#[test]
fn source_retry_stays_running_while_terminal_source_corruption_latches_degraded() {
  for retryable in [true, false] {
    let owner = owner();
    owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
    let encoded = encoded_journal("/doc.json");
    let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
    let revisions = revision_source(&encoded);
    owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
    let result = owner.execute_next_mutation(
      IndexRuntimeMutationWorkRequestV1 {
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &FailingSemanticSource { retryable },
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 101,
        is_cancelled: &|| false,
      },
      &mut Spill,
    );
    let snapshot = owner.snapshot().unwrap();
    if retryable {
      assert!(matches!(result.unwrap(), IndexRuntimeWorkOutcomeV1::Completed(_)));
      assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Running);
      assert_eq!(snapshot.producer.pending_tasks, 1);
      assert_eq!(snapshot.producer.scheduled_retries, 1);
    } else {
      assert!(matches!(result, Err(IndexRuntimeErrorV1::Work(_))));
      assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
      assert_eq!(snapshot.producer.pending_tasks, 1);
      assert_eq!(snapshot.producer.leased_tasks, 0);
    }
  }
}

#[test]
fn worker_retry_deadline_overflow_latches_degraded_after_releasing_the_lease() {
  let mut pressure_options = options();
  pressure_options.collector = IndexProducerCollectorOptionsV1::new(8, 16, 32, 2 * 1_024 * 1_024, 256, 1, 50).unwrap();
  let owner = IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, memory(), pressure_options, 1).unwrap();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let revisions = revision_source(&encoded);
  let semantics = SemanticSource { semantic_state_root: journal.semantic_state_root.to_vec() };
  owner.admit_mutation_journal(&journal, u64::MAX - 1, &|| false, &mut Spill).unwrap();

  assert!(matches!(
    owner.execute_next_mutation(
      IndexRuntimeMutationWorkRequestV1 {
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: u64::MAX,
        is_cancelled: &|| false,
      },
      &mut Spill,
    ),
    Err(IndexRuntimeErrorV1::Work(_))
  ));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.producer.pending_tasks, 1);
  assert_eq!(snapshot.producer.leased_tasks, 0);
  assert_eq!(snapshot.degraded.unwrap().code, "worker_retry_deadline");
}

#[test]
fn cancellation_never_consumes_a_task_or_a_preselection_batch() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let revisions = revision_source(&encoded);
  let semantics = SemanticSource { semantic_state_root: journal.semantic_state_root.to_vec() };
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  assert!(matches!(
    owner.execute_next_mutation(
      IndexRuntimeMutationWorkRequestV1 {
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 101,
        is_cancelled: &|| true,
      },
      &mut Spill,
    ),
    Err(IndexRuntimeErrorV1::Canceled)
  ));
  assert_eq!(owner.snapshot().unwrap().producer.pending_tasks, 1);
  execute_one(&owner, &encoded, 102);

  let mut cancelled = Publisher { behavior: PublishBehavior::Cancelled, calls: 0 };
  assert!(matches!(owner.flush(103, Some(IndexFlushReasonV1::Explicit), false, &mut cancelled), Err(IndexRuntimeErrorV1::Canceled)));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(snapshot.mutations.active_records, 4);
  assert_eq!(snapshot.mutations.frozen_records, 0);
}

#[test]
fn cancellation_during_journal_validation_is_typed_and_worker_panics_retain_the_task() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let cancellation_checks = AtomicUsize::new(0);
  assert!(matches!(
    owner.admit_mutation_journal(&journal, 100, &|| cancellation_checks.fetch_add(1, Ordering::SeqCst) >= 1, &mut Spill,),
    Err(IndexRuntimeErrorV1::Canceled)
  ));
  assert_eq!(owner.snapshot().unwrap().producer.pending_tasks, 0);

  owner.admit_mutation_journal(&journal, 101, &|| false, &mut Spill).unwrap();
  let revisions = revision_source(&encoded);
  assert!(matches!(
    owner.execute_next_mutation(
      IndexRuntimeMutationWorkRequestV1 {
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &PanickingSemanticSource,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 102,
        is_cancelled: &|| false,
      },
      &mut Spill,
    ),
    Err(IndexRuntimeErrorV1::Work(_))
  ));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.degraded.unwrap().code, "worker_panic");
  assert_eq!(snapshot.producer.pending_tasks, 1);
  assert_eq!(snapshot.producer.leased_tasks, 0);
}
