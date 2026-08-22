use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, mpsc};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use aeordb::engine::{HashAlgorithm, MockClock, VirtualClock};
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{AdmissionClass, HostMemorySample, MemoryCoordinator, MemoryObservation, MemoryOwner, MemoryPolicy};
use aeordb::engine::namespace_mutation::{NamespaceMutationAcknowledgement, NamespaceMutationKind, NamespaceMutationSourceIdentity};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::coverage_runtime::{SoftMutationAdmissionV1, SoftMutationHubOptionsV1, SoftMutationHubV1, SoftMutationLossReasonV1};
use aeordb::engine::v4::index_coordinator::{FrozenIndexBatchV1, IndexCoordinatorOptionsV1, IndexFlushReasonV1};
use aeordb::engine::v4::index_maintenance_scan::{
  IndexMaintenanceScanDocumentV1, IndexMaintenanceScanLimitsV1, IndexMaintenanceScanPageV1, IndexMaintenanceScanReadErrorV1,
  IndexMaintenanceScanReadV1, IndexMaintenanceScanRequestV1, IndexMaintenanceScanSourceV1,
  derive_index_maintenance_document_operation_id_v1,
};
use aeordb::engine::v4::index_producer_collector::{
  IndexParserExecutionErrorV1, IndexParserExecutionRequestV1, IndexParserExecutorV1, IndexParserOutcomeV1, IndexProducerCollectorOptionsV1,
};
use aeordb::engine::v4::index_producer_admission::derive_mutation_operation_id;
use aeordb::engine::v4::index_producer_coordinator::{
  IndexProducerCoordinatorOptionsV1, IndexProducerSpillErrorV1, IndexProducerSpillReasonV1, IndexProducerSpillReceiptV1,
  IndexProducerSpillStoreV1, IndexProducerTaskKindV1, IndexProducerTaskRequestV1, IndexProducerTaskViewV1,
};
use aeordb::engine::v4::index_producer_journal_source::{
  IndexProducerJournalReadErrorV1, IndexProducerJournalReadRequestV1, IndexProducerJournalReadV1, IndexProducerJournalSourceV1,
};
use aeordb::engine::v4::index_producer_source::{
  IndexFileRevisionReadErrorV1, IndexFileRevisionReadV1, IndexFileRevisionSourceV1, IndexSemanticScopeLimitsV1,
  IndexSemanticScopeReadErrorV1, IndexSemanticScopeReadRequestV1, IndexSemanticScopeReadV1, IndexSemanticScopeResolutionV1,
  IndexSemanticScopeSourceV1, LoadedIndexFileRevisionV1, OwnedIndexFieldDefinitionV1, OwnedIndexScopeDefinitionV1,
  OwnedIndexValueStoreDefinitionV1, ResolvedIndexScopeWorkV1,
};
use aeordb::engine::v4::index_producer_worker::IndexProducerWorkerOutcomeV1;
use aeordb::engine::v4::index_runtime_owner::{
  IndexRuntimeBatchPublisherV1, IndexRuntimeErrorV1, IndexRuntimeFlushOutcomeV1, IndexRuntimeLifecycleV1, IndexRuntimeOwnerOptionsV1,
  IndexRuntimeOwnerV1, IndexRuntimeProducerWorkRequestV1, IndexRuntimePublicationErrorClassV1, IndexRuntimePublicationErrorV1,
  IndexRuntimePublicationReceiptV1, IndexRuntimeRecoveryDecisionV1, IndexRuntimeWorkOutcomeV1,
};

use aeordb::engine::v4::index_runtime_cadence::{IndexRuntimeCadenceErrorV1, IndexRuntimeCadenceV1};
use aeordb::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_mutation_journal,
  encode_mutation_journal,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use tokio_util::sync::CancellationToken;

#[test]
fn runtime_lifecycle_wire_ids_are_stable_and_unique() {
  assert_eq!(IndexRuntimeLifecycleV1::Recovering.stable_id(), 1);
  assert_eq!(IndexRuntimeLifecycleV1::Running.stable_id(), 2);
  assert_eq!(IndexRuntimeLifecycleV1::Degraded.stable_id(), 3);
  assert_eq!(IndexRuntimeLifecycleV1::Draining.stable_id(), 4);
  assert_eq!(IndexRuntimeLifecycleV1::Stopped.stable_id(), 5);
}

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
  fn load_file_revision(&self, namespace_root: &[u8], path: &str) -> Result<Option<IndexFileRevisionReadV1>, IndexFileRevisionReadErrorV1> {
    self.records.get(&(namespace_root.to_vec(), path.to_string())).cloned().map(test_revision_read).transpose()
  }
}

fn test_revision_read(revision: LoadedIndexFileRevisionV1) -> Result<IndexFileRevisionReadV1, IndexFileRevisionReadErrorV1> {
  let reservation = memory()
    .reserve(MemoryOwner::Task, 2 * 1_024 * 1_024, AdmissionClass::Workload)
    .map_err(|error| IndexFileRevisionReadErrorV1::retryable("test_memory", error.to_string()))?;
  IndexFileRevisionReadV1::new(revision, reservation)
}

struct SemanticSource {
  semantic_state_root: Vec<u8>,
}

struct RecordingSemanticSource {
  semantic_state_root: Vec<u8>,
  observed: Arc<Mutex<Vec<([u8; 16], u64, String)>>>,
}

impl IndexSemanticScopeSourceV1 for SemanticSource {
  fn resolve_scopes(
    &self,
    request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    let path = request
      .transition
      .after
      .as_ref()
      .or(request.transition.before.as_ref())
      .map(|document| document.file_record.path.as_str())
      .unwrap_or("/");
    let ordinal = path.bytes().fold(3u64, |value, byte| value.saturating_add(u64::from(byte)));
    let resolution = IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: self.semantic_state_root.clone(),
      scope_work: vec![scope_work(&self.semantic_state_root, ordinal)],
    };
    let reservation = memory()
      .reserve(MemoryOwner::Task, 2 * 1_024 * 1_024, AdmissionClass::Workload)
      .map_err(|error| IndexSemanticScopeReadErrorV1::retryable("test_memory", error.to_string()))?;
    IndexSemanticScopeReadV1::new(resolution, reservation)
  }
}

impl IndexSemanticScopeSourceV1 for RecordingSemanticSource {
  fn resolve_scopes(
    &self,
    request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    let path = request
      .transition
      .after
      .as_ref()
      .or(request.transition.before.as_ref())
      .map(|document| document.file_record.path.clone())
      .unwrap_or_else(|| "/".to_string());
    self.observed.lock().unwrap().push((request.operation_id, request.source_publication_sequence, path));
    SemanticSource { semantic_state_root: self.semantic_state_root.clone() }.resolve_scopes(request)
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

struct JournalSource {
  encoded: Vec<u8>,
  observed: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl JournalSource {
  fn new(encoded: &[u8]) -> Self {
    Self { encoded: encoded.to_vec(), observed: Arc::new(Mutex::new(Vec::new())) }
  }
}

impl IndexProducerJournalSourceV1 for JournalSource {
  fn load_journal(
    &self,
    request: IndexProducerJournalReadRequestV1<'_>,
  ) -> Result<IndexProducerJournalReadV1, IndexProducerJournalReadErrorV1> {
    self.observed.lock().unwrap().push(request.journal_head.to_vec());
    let reservation = memory()
      .reserve(MemoryOwner::Task, 2 * 1_024 * 1_024, AdmissionClass::Workload)
      .map_err(|error| IndexProducerJournalReadErrorV1::retryable("test_memory", error.to_string()))?;
    IndexProducerJournalReadV1::new(&request, self.encoded.clone(), reservation)
  }
}

struct UnexpectedJournalSource;

impl IndexProducerJournalSourceV1 for UnexpectedJournalSource {
  fn load_journal(
    &self,
    _request: IndexProducerJournalReadRequestV1<'_>,
  ) -> Result<IndexProducerJournalReadV1, IndexProducerJournalReadErrorV1> {
    panic!("journal source must not serve an authoritative scan task")
  }
}

struct UnexpectedMaintenanceSource;

impl IndexMaintenanceScanSourceV1 for UnexpectedMaintenanceSource {
  fn scan(&self, _request: IndexMaintenanceScanRequestV1<'_>) -> Result<IndexMaintenanceScanReadV1, IndexMaintenanceScanReadErrorV1> {
    panic!("maintenance source must not serve a journal-transition task")
  }
}

#[derive(Clone, Copy)]
enum JournalFailure {
  Cancelled,
  Retryable,
  Corrupt,
}

struct FailingJournalSource(JournalFailure);

impl IndexProducerJournalSourceV1 for FailingJournalSource {
  fn load_journal(
    &self,
    _request: IndexProducerJournalReadRequestV1<'_>,
  ) -> Result<IndexProducerJournalReadV1, IndexProducerJournalReadErrorV1> {
    Err(match self.0 {
      JournalFailure::Cancelled => IndexProducerJournalReadErrorV1::cancelled("test_cancelled", "injected journal cancellation"),
      JournalFailure::Retryable => IndexProducerJournalReadErrorV1::retryable("test_busy", "injected retryable journal read"),
      JournalFailure::Corrupt => IndexProducerJournalReadErrorV1::corrupt("test_corrupt", "injected corrupt journal evidence"),
    })
  }
}

struct PanickingJournalSource;

impl IndexProducerJournalSourceV1 for PanickingJournalSource {
  fn load_journal(
    &self,
    _request: IndexProducerJournalReadRequestV1<'_>,
  ) -> Result<IndexProducerJournalReadV1, IndexProducerJournalReadErrorV1> {
    panic!("injected journal source panic")
  }
}

struct MaintenanceSource {
  pages: Mutex<VecDeque<(Vec<IndexMaintenanceScanDocumentV1>, bool)>>,
  observed: Arc<Mutex<Vec<(Vec<u8>, String, Option<String>)>>>,
}

impl IndexMaintenanceScanSourceV1 for MaintenanceSource {
  fn scan(&self, request: IndexMaintenanceScanRequestV1<'_>) -> Result<IndexMaintenanceScanReadV1, IndexMaintenanceScanReadErrorV1> {
    self.observed.lock().unwrap().push((
      request.namespace_root.to_vec(),
      request.scope.to_string(),
      request.resume_after.map(str::to_string),
    ));
    let (documents, complete) = self
      .pages
      .lock()
      .unwrap()
      .pop_front()
      .ok_or_else(|| IndexMaintenanceScanReadErrorV1::corrupt("test_page_missing", "test source has no remaining page"))?;
    let next_resume_after = (!complete).then(|| documents.last().unwrap().file_record.path.clone());
    let retained_bytes = 1_024 * 1_024;
    let reservation = memory()
      .reserve(MemoryOwner::Task, retained_bytes, AdmissionClass::Workload)
      .map_err(|error| IndexMaintenanceScanReadErrorV1::retryable("test_memory", error.to_string()))?;
    IndexMaintenanceScanReadV1::new(
      ALGORITHM,
      &request,
      IndexMaintenanceScanPageV1 { documents, next_resume_after, complete, retained_bytes },
      reservation,
    )
    .map_err(|error| IndexMaintenanceScanReadErrorV1::corrupt("test_page", error.to_string()))
  }
}

#[derive(Clone, Copy)]
enum MaintenanceFailure {
  Cancelled,
  Retryable,
  Corrupt,
}

struct FailingMaintenanceSource(MaintenanceFailure);

impl IndexMaintenanceScanSourceV1 for FailingMaintenanceSource {
  fn scan(&self, _request: IndexMaintenanceScanRequestV1<'_>) -> Result<IndexMaintenanceScanReadV1, IndexMaintenanceScanReadErrorV1> {
    Err(match self.0 {
      MaintenanceFailure::Cancelled => IndexMaintenanceScanReadErrorV1::cancelled("test_cancelled", "injected scan cancellation"),
      MaintenanceFailure::Retryable => IndexMaintenanceScanReadErrorV1::retryable("test_busy", "injected retryable scan refusal"),
      MaintenanceFailure::Corrupt => IndexMaintenanceScanReadErrorV1::corrupt("test_corrupt", "injected corrupt scan evidence"),
    })
  }
}

struct BlockingMaintenanceSource {
  entered: mpsc::Sender<()>,
  release: Mutex<mpsc::Receiver<()>>,
  document: IndexMaintenanceScanDocumentV1,
}

impl IndexMaintenanceScanSourceV1 for BlockingMaintenanceSource {
  fn scan(&self, request: IndexMaintenanceScanRequestV1<'_>) -> Result<IndexMaintenanceScanReadV1, IndexMaintenanceScanReadErrorV1> {
    self.entered.send(()).unwrap();
    self.release.lock().unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
    let retained_bytes = 1_024 * 1_024;
    let reservation = memory()
      .reserve(MemoryOwner::Task, retained_bytes, AdmissionClass::Workload)
      .map_err(|error| IndexMaintenanceScanReadErrorV1::retryable("test_memory", error.to_string()))?;
    IndexMaintenanceScanReadV1::new(
      ALGORITHM,
      &request,
      IndexMaintenanceScanPageV1 { documents: vec![self.document.clone()], next_resume_after: None, complete: true, retained_bytes },
      reservation,
    )
    .map_err(|error| IndexMaintenanceScanReadErrorV1::corrupt("test_page", error.to_string()))
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

#[derive(Clone, Copy)]
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

struct IdentityPublisher {
  behavior: PublishBehavior,
  observed: Arc<Mutex<Vec<(u64, u64)>>>,
}

#[derive(Default)]
struct ProgressPublisher {
  calls: u64,
  records: u64,
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

impl IndexRuntimeBatchPublisherV1 for IdentityPublisher {
  fn publish(&mut self, batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    self.observed.lock().unwrap().push((batch.batch_id(), batch.attempt_id()));
    Publisher { behavior: self.behavior, calls: 0 }.publish(batch)
  }
}

impl IndexRuntimeBatchPublisherV1 for ProgressPublisher {
  fn publish(&mut self, batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    self.calls += 1;
    self.records += batch.records().len() as u64;
    Ok(IndexRuntimePublicationReceiptV1 {
      batch_id: batch.batch_id(),
      attempt_id: batch.attempt_id(),
      published_records: batch.records().len() as u64,
      publication_bytes: batch.publication_bytes(),
      checkpoint_sequence: 10 + self.calls,
    })
  }
}

fn execute_one(owner: &IndexRuntimeOwnerV1, encoded: &[u8], now_ms: u64) -> IndexRuntimeWorkOutcomeV1 {
  let journal = decode_mutation_journal(encoded, ALGORITHM).unwrap();
  let revisions = revision_source(encoded);
  let semantics = SemanticSource { semantic_state_root: journal.semantic_state_root.to_vec() };
  execute_journal_work(owner, encoded, &revisions, &semantics, now_ms, &|| false, &mut Spill).unwrap()
}

fn execute_journal_work(
  owner: &IndexRuntimeOwnerV1,
  encoded: &[u8],
  revision_source: &dyn IndexFileRevisionSourceV1,
  semantic_source: &dyn IndexSemanticScopeSourceV1,
  now_ms: u64,
  is_cancelled: &dyn Fn() -> bool,
  spill_store: &mut dyn IndexProducerSpillStoreV1,
) -> Result<IndexRuntimeWorkOutcomeV1, IndexRuntimeErrorV1> {
  let journal_source = JournalSource::new(encoded);
  execute_journal_source_work(owner, &journal_source, revision_source, semantic_source, now_ms, is_cancelled, spill_store)
}

fn execute_journal_source_work(
  owner: &IndexRuntimeOwnerV1,
  journal_source: &dyn IndexProducerJournalSourceV1,
  revision_source: &dyn IndexFileRevisionSourceV1,
  semantic_source: &dyn IndexSemanticScopeSourceV1,
  now_ms: u64,
  is_cancelled: &dyn Fn() -> bool,
  spill_store: &mut dyn IndexProducerSpillStoreV1,
) -> Result<IndexRuntimeWorkOutcomeV1, IndexRuntimeErrorV1> {
  owner.execute_next_producer(
    IndexRuntimeProducerWorkRequestV1 {
      journal_source,
      maintenance_source: &UnexpectedMaintenanceSource,
      maintenance_limits: maintenance_limits(),
      revision_source,
      semantic_source,
      parser: &UnexpectedParser,
      mapper: None,
      now_ms,
      is_cancelled,
    },
    spill_store,
  )
}

fn execute_maintenance_work(
  owner: &IndexRuntimeOwnerV1,
  source: &dyn IndexMaintenanceScanSourceV1,
  limits: IndexMaintenanceScanLimitsV1,
  semantic_source: &dyn IndexSemanticScopeSourceV1,
  now_ms: u64,
  is_cancelled: &dyn Fn() -> bool,
  spill_store: &mut dyn IndexProducerSpillStoreV1,
) -> Result<IndexRuntimeWorkOutcomeV1, IndexRuntimeErrorV1> {
  owner.execute_next_producer(
    IndexRuntimeProducerWorkRequestV1 {
      journal_source: &UnexpectedJournalSource,
      maintenance_source: source,
      maintenance_limits: limits,
      revision_source: &RevisionSource::default(),
      semantic_source,
      parser: &UnexpectedParser,
      mapper: None,
      now_ms,
      is_cancelled,
    },
    spill_store,
  )
}

fn maintenance_limits() -> IndexMaintenanceScanLimitsV1 {
  IndexMaintenanceScanLimitsV1::new(4, 2 * 1_024 * 1_024, 16 * 1_024).unwrap()
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

fn maintenance_task<'a>(root: &'a [u8], semantic: &'a [u8], kind: IndexProducerTaskKindV1) -> IndexProducerTaskRequestV1<'a> {
  maintenance_task_with(root, semantic, kind, [0x56; 16], 8, "/")
}

fn maintenance_task_with<'a>(
  root: &'a [u8],
  semantic: &'a [u8],
  kind: IndexProducerTaskKindV1,
  operation_id: [u8; 16],
  publication_sequence: u64,
  scope: &'a str,
) -> IndexProducerTaskRequestV1<'a> {
  IndexProducerTaskRequestV1 {
    operation_id,
    kind,
    publication_sequence,
    namespace_root_before: root,
    namespace_root_after: root,
    semantic_state_root: semantic,
    journal_head: None,
    scope: Some(scope),
  }
}

#[test]
fn unified_dispatcher_executes_both_journal_transition_task_kinds() {
  for (ordinal, kind) in [IndexProducerTaskKindV1::MutationWindow, IndexProducerTaskKindV1::Reconcile].into_iter().enumerate() {
    let owner = owner();
    owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
    let encoded = encoded_journal("/doc.json");
    let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
    let record = journal.records.iter().next().unwrap().unwrap();
    let operation_id = derive_mutation_operation_id(ALGORITHM, record.mutation_id, record.batch_ordinal).unwrap();
    owner
      .admit_task(
        IndexProducerTaskRequestV1 {
          operation_id,
          kind,
          publication_sequence: record.sequence,
          namespace_root_before: record.root_before,
          namespace_root_after: record.root_after,
          semantic_state_root: journal.semantic_state_root,
          journal_head: Some(&journal.key),
          scope: None,
        },
        100 + ordinal as u64,
        &mut Spill,
      )
      .unwrap();
    let revisions = revision_source(&encoded);
    let semantics = SemanticSource { semantic_state_root: journal.semantic_state_root.to_vec() };

    let outcome = execute_journal_work(&owner, &encoded, &revisions, &semantics, 110 + ordinal as u64, &|| false, &mut Spill).unwrap();

    assert!(matches!(outcome, IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::Completed(_))));
    let snapshot = owner.snapshot().unwrap();
    assert_eq!(snapshot.producer.pending_tasks, 0, "journal kind {kind:?} was not completed");
    assert_eq!(snapshot.producer.leased_tasks, 0);
    assert_eq!(snapshot.mutations.active_records, 4);
  }
}

#[test]
fn unified_dispatcher_executes_every_authoritative_scan_task_kind() {
  let kinds = [
    IndexProducerTaskKindV1::Build,
    IndexProducerTaskKindV1::Rebuild,
    IndexProducerTaskKindV1::Retire,
    IndexProducerTaskKindV1::Repair,
    IndexProducerTaskKindV1::ExplicitMutation,
    IndexProducerTaskKindV1::LegacyMigration,
  ];
  for (ordinal, kind) in kinds.into_iter().enumerate() {
    let owner = owner();
    owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
    let root = hash(format!("authoritative-root-{ordinal}").as_bytes());
    let semantic = hash(format!("authoritative-semantic-{ordinal}").as_bytes());
    owner
      .admit_task(maintenance_task_with(&root, &semantic, kind, [0x61 + ordinal as u8; 16], 10 + ordinal as u64, "/docs"), 1, &mut Spill)
      .unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let source = MaintenanceSource {
      pages: Mutex::new(VecDeque::from([(
        vec![IndexMaintenanceScanDocumentV1 {
          revision_hash: hash(format!("authoritative-revision-{ordinal}").as_bytes()),
          file_record: file("/docs/a.json"),
        }],
        true,
      )])),
      observed: Arc::clone(&observed),
    };
    let semantics = SemanticSource { semantic_state_root: semantic };

    let outcome = execute_maintenance_work(&owner, &source, maintenance_limits(), &semantics, 2, &|| false, &mut Spill).unwrap();

    assert!(matches!(outcome, IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::MaintenancePage { complete: true, .. })));
    let snapshot = owner.snapshot().unwrap();
    assert_eq!(snapshot.producer.pending_tasks, 0, "authoritative kind {kind:?} was not completed");
    assert_eq!(snapshot.producer.leased_tasks, 0);
    assert_eq!(observed.lock().unwrap().len(), 1);
  }
}

#[test]
fn artifact_compaction_is_retained_and_degrades_without_invoking_an_unrelated_source() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let root = hash(b"compact-root");
  let semantic = hash(b"compact-semantic");
  owner.admit_task(maintenance_task(&root, &semantic, IndexProducerTaskKindV1::Compact), 1, &mut Spill).unwrap();

  let result = owner.execute_next_producer(
    IndexRuntimeProducerWorkRequestV1 {
      journal_source: &UnexpectedJournalSource,
      maintenance_source: &UnexpectedMaintenanceSource,
      maintenance_limits: maintenance_limits(),
      revision_source: &RevisionSource::default(),
      semantic_source: &SemanticSource { semantic_state_root: semantic },
      parser: &UnexpectedParser,
      mapper: None,
      now_ms: 2,
      is_cancelled: &|| false,
    },
    &mut Spill,
  );

  assert!(matches!(result, Err(IndexRuntimeErrorV1::Work(message)) if message.contains("Compact")));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.producer.pending_tasks, 1);
  assert_eq!(snapshot.producer.leased_tasks, 0);
  assert_eq!(snapshot.degraded.unwrap().code, "worker_terminal");
}

#[test]
fn mixed_canonical_queue_dispatches_each_task_only_to_its_own_source() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/journal.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let maintenance_root = hash(b"mixed-maintenance-root");
  owner
    .admit_task(
      maintenance_task_with(&maintenance_root, journal.semantic_state_root, IndexProducerTaskKindV1::Build, [0x71; 16], 8, "/"),
      1,
      &mut Spill,
    )
    .unwrap();
  owner.admit_mutation_journal(&journal, 1, &|| false, &mut Spill).unwrap();
  let maintenance_observed = Arc::new(Mutex::new(Vec::new()));
  let maintenance_source = MaintenanceSource {
    pages: Mutex::new(VecDeque::from([(
      vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"mixed-revision"), file_record: file("/maintenance.json") }],
      true,
    )])),
    observed: Arc::clone(&maintenance_observed),
  };
  let journal_source = JournalSource::new(&encoded);
  let journal_observed = Arc::clone(&journal_source.observed);
  let revisions = revision_source(&encoded);
  let semantics = SemanticSource { semantic_state_root: journal.semantic_state_root.to_vec() };
  let never_cancelled = || false;
  let request = |now_ms| IndexRuntimeProducerWorkRequestV1 {
    journal_source: &journal_source,
    maintenance_source: &maintenance_source,
    maintenance_limits: maintenance_limits(),
    revision_source: &revisions,
    semantic_source: &semantics,
    parser: &UnexpectedParser,
    mapper: None,
    now_ms,
    is_cancelled: &never_cancelled,
  };

  let first = owner.execute_next_producer(request(2), &mut Spill).unwrap();
  assert!(matches!(first, IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::Completed(_))));
  assert!(maintenance_observed.lock().unwrap().is_empty());
  assert_eq!(*journal_observed.lock().unwrap(), vec![journal.key.to_vec()]);

  let second = owner.execute_next_producer(request(3), &mut Spill).unwrap();
  assert!(matches!(second, IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::MaintenancePage { .. })));
  assert_eq!(maintenance_observed.lock().unwrap().len(), 1);
  assert_eq!(*journal_observed.lock().unwrap(), vec![journal.key.to_vec()]);
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.producer.pending_tasks, 0);
  assert_eq!(snapshot.producer.leased_tasks, 0);
}

#[test]
fn journal_source_retry_cancellation_corruption_and_panic_preserve_fail_closed_lease_direction() {
  for failure in [JournalFailure::Retryable, JournalFailure::Cancelled, JournalFailure::Corrupt] {
    let owner = owner();
    owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
    let encoded = encoded_journal("/doc.json");
    let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
    owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
    let revisions = revision_source(&encoded);
    let semantics = SemanticSource { semantic_state_root: journal.semantic_state_root.to_vec() };

    let result = execute_journal_source_work(&owner, &FailingJournalSource(failure), &revisions, &semantics, 101, &|| false, &mut Spill);
    let snapshot = owner.snapshot().unwrap();
    assert_eq!(snapshot.producer.pending_tasks, 1);
    assert_eq!(snapshot.producer.leased_tasks, 0);
    match failure {
      JournalFailure::Retryable => {
        assert!(matches!(result, Ok(IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::SourceRetry { .. }))));
        assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Running);
        assert_eq!(snapshot.producer.scheduled_retries, 1);
      }
      JournalFailure::Cancelled => {
        assert!(matches!(result, Err(IndexRuntimeErrorV1::Canceled)));
        assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Running);
      }
      JournalFailure::Corrupt => {
        assert!(matches!(result, Err(IndexRuntimeErrorV1::Work(message)) if message.contains("test_corrupt")));
        assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
      }
    }
  }

  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/panic.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  let revisions = revision_source(&encoded);
  let semantics = SemanticSource { semantic_state_root: journal.semantic_state_root.to_vec() };
  assert!(matches!(
    execute_journal_source_work(&owner, &PanickingJournalSource, &revisions, &semantics, 101, &|| false, &mut Spill),
    Err(IndexRuntimeErrorV1::Work(_))
  ));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.degraded.unwrap().code, "worker_panic");
  assert_eq!(snapshot.producer.pending_tasks, 1);
  assert_eq!(snapshot.producer.leased_tasks, 0);
}

#[test]
fn malformed_loaded_journal_is_rejected_by_the_owner_and_retains_the_exact_task() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  let revisions = revision_source(&encoded);
  let semantics = SemanticSource { semantic_state_root: journal.semantic_state_root.to_vec() };

  let result = execute_journal_source_work(&owner, &JournalSource::new(&[0x99; 128]), &revisions, &semantics, 101, &|| false, &mut Spill);

  assert!(matches!(result, Err(IndexRuntimeErrorV1::Work(message)) if message.contains("journal_format")));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.producer.pending_tasks, 1);
  assert_eq!(snapshot.producer.leased_tasks, 0);
}

#[test]
fn journal_plan_pressure_retries_before_source_io_and_releases_temporary_memory() {
  let coordinator = memory();
  let owner = IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, coordinator.clone(), options(), 1).unwrap();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/pressured.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  let retained_task_bytes = reserved(&coordinator, MemoryOwner::Task);
  coordinator
    .observe_legacy(
      MemoryOwner::IndexCleanCache,
      MemoryObservation {
        resident_bytes: 60 * 1_024 * 1_024,
        clean_bytes: 60 * 1_024 * 1_024,
        dirty_bytes: 0,
        evictable_bytes: 60 * 1_024 * 1_024,
        pinned_bytes: 0,
        spill_bytes: 0,
        items: 1,
        hits: 0,
        misses: 0,
        evictions: 0,
      },
    )
    .unwrap();
  let source = JournalSource::new(&encoded);
  let observed = Arc::clone(&source.observed);
  let revisions = revision_source(&encoded);
  let semantics = SemanticSource { semantic_state_root: journal.semantic_state_root.to_vec() };

  let retry = execute_journal_source_work(&owner, &source, &revisions, &semantics, 101, &|| false, &mut Spill).unwrap();
  assert!(matches!(retry, IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::SourceRetry { .. })));
  assert!(observed.lock().unwrap().is_empty());
  assert_eq!(reserved(&coordinator, MemoryOwner::Task), retained_task_bytes);
  assert_eq!(owner.snapshot().unwrap().lifecycle, IndexRuntimeLifecycleV1::Running);

  coordinator.observe_legacy(MemoryOwner::IndexCleanCache, MemoryObservation::default()).unwrap();
  let completed = execute_journal_source_work(&owner, &source, &revisions, &semantics, 130, &|| false, &mut Spill).unwrap();
  assert!(matches!(completed, IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::Completed(_))));
  assert_eq!(*observed.lock().unwrap(), vec![journal.key.to_vec()]);
  assert_eq!(reserved(&coordinator, MemoryOwner::Task), 0);
}

#[test]
fn runtime_executes_root_pinned_page_through_the_existing_worker_and_completes_the_task() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 7 }).unwrap();
  let root = hash(b"maintenance-root");
  let semantic = hash(b"maintenance-semantic");
  let revision = hash(b"maintenance-revision");
  owner.admit_task(maintenance_task(&root, &semantic, IndexProducerTaskKindV1::Build), 1, &mut Spill).unwrap();
  let observed = Arc::new(Mutex::new(Vec::new()));
  let source = MaintenanceSource {
    pages: Mutex::new(VecDeque::from([(
      vec![IndexMaintenanceScanDocumentV1 { revision_hash: revision.clone(), file_record: file("/a.json") }],
      true,
    )])),
    observed: Arc::clone(&observed),
  };
  let semantic_observed = Arc::new(Mutex::new(Vec::new()));
  let semantics = RecordingSemanticSource { semantic_state_root: semantic.clone(), observed: Arc::clone(&semantic_observed) };

  let outcome = execute_maintenance_work(&owner, &source, maintenance_limits(), &semantics, 2, &|| false, &mut Spill).unwrap();

  assert!(matches!(outcome, IndexRuntimeWorkOutcomeV1::Completed(_)));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.producer.pending_tasks, 0);
  assert_eq!(snapshot.producer.leased_tasks, 0);
  assert!(snapshot.mutations.active_records > 0, "maintenance outcome {outcome:?}, snapshot {snapshot:?}");
  assert_eq!(*observed.lock().unwrap(), vec![(root, "/".to_string(), None)]);
  assert_eq!(
    *semantic_observed.lock().unwrap(),
    vec![(
      derive_index_maintenance_document_operation_id_v1(
        ALGORITHM,
        [0x56; 16],
        IndexProducerTaskKindV1::Build,
        &hash(b"maintenance-root"),
        &revision,
        "/a.json",
      )
      .unwrap(),
      8,
      "/a.json".to_string(),
    )]
  );
}

#[test]
fn runtime_releases_incomplete_page_lease_and_resumes_after_the_last_committed_document() {
  let coordinator = memory();
  let owner = IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, coordinator.clone(), options(), 1).unwrap();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 7 }).unwrap();
  let root = hash(b"paged-maintenance-root");
  let semantic = hash(b"paged-maintenance-semantic");
  owner.admit_task(maintenance_task(&root, &semantic, IndexProducerTaskKindV1::Build), 1, &mut Spill).unwrap();
  let retained_task_bytes = reserved(&coordinator, MemoryOwner::Task);
  let observed = Arc::new(Mutex::new(Vec::new()));
  let source = MaintenanceSource {
    pages: Mutex::new(VecDeque::from([
      (vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"paged-revision-a"), file_record: file("/a.json") }], false),
      (vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"paged-revision-b"), file_record: file("/b.json") }], true),
    ])),
    observed: Arc::clone(&observed),
  };
  let semantics = SemanticSource { semantic_state_root: semantic.clone() };
  let limits = IndexMaintenanceScanLimitsV1::new(1, 2 * 1_024 * 1_024, 16 * 1_024).unwrap();

  let first = execute_maintenance_work(&owner, &source, limits, &semantics, 2, &|| false, &mut Spill).unwrap();
  assert!(matches!(
    first,
    IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::MaintenancePage {
      processed_documents: 1,
      complete: false,
      completion: None,
    })
  ));
  let after_first = owner.snapshot().unwrap();
  assert_eq!(after_first.producer.pending_tasks, 1);
  assert_eq!(after_first.producer.leased_tasks, 0);
  assert_eq!(after_first.mutations.active_records, 4);
  assert_eq!(reserved(&coordinator, MemoryOwner::Task), retained_task_bytes + u64::try_from("/a.json".len()).unwrap());

  let second = execute_maintenance_work(&owner, &source, limits, &semantics, 3, &|| false, &mut Spill).unwrap();
  assert!(matches!(
    second,
    IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::MaintenancePage {
      processed_documents: 1,
      complete: true,
      completion: Some(_),
    })
  ));
  let after_second = owner.snapshot().unwrap();
  assert_eq!(after_second.producer.pending_tasks, 0);
  assert_eq!(after_second.mutations.active_records, 8);
  assert_eq!(reserved(&coordinator, MemoryOwner::Task), 0);
  assert_eq!(*observed.lock().unwrap(), vec![(root.clone(), "/".to_string(), None), (root, "/".to_string(), Some("/a.json".to_string()))]);
}

#[test]
fn runtime_retirement_scan_uses_the_same_worker_to_replace_upserts_with_removals() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 7 }).unwrap();
  let root = hash(b"retirement-root");
  let semantic = hash(b"retirement-semantic");
  let revision = hash(b"retirement-revision");
  let semantics = SemanticSource { semantic_state_root: semantic.clone() };
  let limits = IndexMaintenanceScanLimitsV1::new(1, 2 * 1_024 * 1_024, 16 * 1_024).unwrap();

  owner.admit_task(maintenance_task_with(&root, &semantic, IndexProducerTaskKindV1::Build, [0x56; 16], 8, "/"), 1, &mut Spill).unwrap();
  let build = MaintenanceSource {
    pages: Mutex::new(VecDeque::from([(
      vec![IndexMaintenanceScanDocumentV1 { revision_hash: revision.clone(), file_record: file("/a.json") }],
      true,
    )])),
    observed: Arc::new(Mutex::new(Vec::new())),
  };
  execute_maintenance_work(&owner, &build, limits, &semantics, 2, &|| false, &mut Spill).unwrap();
  let after_build = owner.snapshot().unwrap();
  assert_eq!(after_build.mutations.active_records, 4);
  assert_eq!(after_build.mutations.active_mutations, 4);

  owner.admit_task(maintenance_task_with(&root, &semantic, IndexProducerTaskKindV1::Retire, [0x57; 16], 9, "/"), 3, &mut Spill).unwrap();
  let retire = MaintenanceSource {
    pages: Mutex::new(VecDeque::from([(
      vec![IndexMaintenanceScanDocumentV1 { revision_hash: revision, file_record: file("/a.json") }],
      true,
    )])),
    observed: Arc::new(Mutex::new(Vec::new())),
  };
  let retired = execute_maintenance_work(&owner, &retire, limits, &semantics, 4, &|| false, &mut Spill).unwrap();
  assert!(matches!(retired, IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::MaintenancePage { complete: true, .. })));
  let after_retire = owner.snapshot().unwrap();
  assert_eq!(after_retire.producer.pending_tasks, 0);
  assert_eq!(after_retire.mutations.active_records, 4);
  assert!(after_retire.mutations.active_mutations > after_build.mutations.active_mutations);
}

#[test]
fn maintenance_scan_retry_cancellation_and_corruption_preserve_the_fail_closed_runtime_direction() {
  let root = hash(b"maintenance-failure-root");
  let semantic = hash(b"maintenance-failure-semantic");
  let semantics = SemanticSource { semantic_state_root: semantic.clone() };
  let limits = IndexMaintenanceScanLimitsV1::new(1, 2 * 1_024 * 1_024, 16 * 1_024).unwrap();

  let retrying = owner();
  retrying.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 7 }).unwrap();
  retrying.admit_task(maintenance_task(&root, &semantic, IndexProducerTaskKindV1::Build), 1, &mut Spill).unwrap();
  let retry = execute_maintenance_work(
    &retrying,
    &FailingMaintenanceSource(MaintenanceFailure::Retryable),
    limits,
    &semantics,
    2,
    &|| false,
    &mut Spill,
  )
  .unwrap();
  assert!(matches!(retry, IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::SourceRetry { .. })));
  let retry_snapshot = retrying.snapshot().unwrap();
  assert_eq!(retry_snapshot.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(retry_snapshot.producer.pending_tasks, 1);
  assert_eq!(retry_snapshot.producer.leased_tasks, 0);
  assert_eq!(retry_snapshot.producer.scheduled_retries, 1);

  let cancelled = owner();
  cancelled.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 7 }).unwrap();
  cancelled.admit_task(maintenance_task(&root, &semantic, IndexProducerTaskKindV1::Build), 1, &mut Spill).unwrap();
  assert!(matches!(
    execute_maintenance_work(
      &cancelled,
      &FailingMaintenanceSource(MaintenanceFailure::Cancelled),
      limits,
      &semantics,
      2,
      &|| false,
      &mut Spill,
    ),
    Err(IndexRuntimeErrorV1::Canceled)
  ));
  let cancelled_snapshot = cancelled.snapshot().unwrap();
  assert_eq!(cancelled_snapshot.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(cancelled_snapshot.producer.pending_tasks, 1);
  assert_eq!(cancelled_snapshot.producer.leased_tasks, 0);

  let corrupt = owner();
  corrupt.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 7 }).unwrap();
  corrupt.admit_task(maintenance_task(&root, &semantic, IndexProducerTaskKindV1::Build), 1, &mut Spill).unwrap();
  assert!(matches!(
    execute_maintenance_work(
      &corrupt,
      &FailingMaintenanceSource(MaintenanceFailure::Corrupt),
      limits,
      &semantics,
      2,
      &|| false,
      &mut Spill,
    ),
    Err(IndexRuntimeErrorV1::Work(message)) if message.contains("test_corrupt")
  ));
  let corrupt_snapshot = corrupt.snapshot().unwrap();
  assert_eq!(corrupt_snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(corrupt_snapshot.producer.pending_tasks, 1);
  assert_eq!(corrupt_snapshot.producer.leased_tasks, 0);
  assert_eq!(corrupt_snapshot.mutations.active_records, 0);
}

#[test]
fn maintenance_scan_plan_pressure_retries_before_source_io_and_releases_temporary_memory() {
  let coordinator = memory();
  let owner = IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, coordinator.clone(), options(), 1).unwrap();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 7 }).unwrap();
  let root = hash(b"pressured-maintenance-root");
  let semantic = hash(b"pressured-maintenance-semantic");
  owner.admit_task(maintenance_task(&root, &semantic, IndexProducerTaskKindV1::Build), 1, &mut Spill).unwrap();
  let retained_task_bytes = reserved(&coordinator, MemoryOwner::Task);
  coordinator
    .observe_legacy(
      MemoryOwner::IndexCleanCache,
      MemoryObservation {
        resident_bytes: 60 * 1_024 * 1_024,
        clean_bytes: 60 * 1_024 * 1_024,
        dirty_bytes: 0,
        evictable_bytes: 60 * 1_024 * 1_024,
        pinned_bytes: 0,
        spill_bytes: 0,
        items: 1,
        hits: 0,
        misses: 0,
        evictions: 0,
      },
    )
    .unwrap();
  let observed = Arc::new(Mutex::new(Vec::new()));
  let source = MaintenanceSource {
    pages: Mutex::new(VecDeque::from([(
      vec![IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"pressured-revision"), file_record: file("/a.json") }],
      true,
    )])),
    observed: Arc::clone(&observed),
  };
  let semantics = SemanticSource { semantic_state_root: semantic };
  let limits = IndexMaintenanceScanLimitsV1::new(1, 2 * 1_024 * 1_024, 16 * 1_024).unwrap();

  let retry = execute_maintenance_work(&owner, &source, limits, &semantics, 2, &|| false, &mut Spill).unwrap();
  assert!(matches!(retry, IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::SourceRetry { .. })));
  assert!(observed.lock().unwrap().is_empty());
  assert_eq!(reserved(&coordinator, MemoryOwner::Task), retained_task_bytes);
  assert_eq!(owner.snapshot().unwrap().lifecycle, IndexRuntimeLifecycleV1::Running);

  coordinator.observe_legacy(MemoryOwner::IndexCleanCache, MemoryObservation::default()).unwrap();
  let completed = execute_maintenance_work(&owner, &source, limits, &semantics, 30, &|| false, &mut Spill).unwrap();
  assert!(matches!(completed, IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::MaintenancePage { complete: true, .. })));
  assert_eq!(observed.lock().unwrap().len(), 1);
  assert_eq!(reserved(&coordinator, MemoryOwner::Task), 0);
}

#[test]
fn blocked_maintenance_scan_does_not_hold_the_runtime_owner_mutex() {
  let owner = Arc::new(owner());
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 7 }).unwrap();
  let root = hash(b"blocked-maintenance-root");
  let semantic = hash(b"blocked-maintenance-semantic");
  owner.admit_task(maintenance_task(&root, &semantic, IndexProducerTaskKindV1::Build), 1, &mut Spill).unwrap();
  let (entered_tx, entered_rx) = mpsc::channel();
  let (release_tx, release_rx) = mpsc::channel();
  let source = Arc::new(BlockingMaintenanceSource {
    entered: entered_tx,
    release: Mutex::new(release_rx),
    document: IndexMaintenanceScanDocumentV1 { revision_hash: hash(b"blocked-maintenance-revision"), file_record: file("/a.json") },
  });
  let semantics = Arc::new(SemanticSource { semantic_state_root: semantic });
  let worker_owner = Arc::clone(&owner);
  let worker_source = Arc::clone(&source);
  let worker_semantics = Arc::clone(&semantics);
  let worker = std::thread::spawn(move || {
    execute_maintenance_work(
      &worker_owner,
      worker_source.as_ref(),
      IndexMaintenanceScanLimitsV1::new(1, 2 * 1_024 * 1_024, 16 * 1_024).unwrap(),
      worker_semantics.as_ref(),
      2,
      &|| false,
      &mut Spill,
    )
  });
  entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

  let (snapshot_tx, snapshot_rx) = mpsc::channel();
  let snapshot_owner = Arc::clone(&owner);
  let snapshot = std::thread::spawn(move || snapshot_tx.send(snapshot_owner.cached_snapshot()).unwrap());
  assert_eq!(snapshot_rx.recv_timeout(Duration::from_millis(250)).unwrap().producer.leased_tasks, 1);

  let (drain_tx, drain_rx) = mpsc::channel();
  let drain_owner = Arc::clone(&owner);
  let drain = std::thread::spawn(move || drain_tx.send(drain_owner.begin_draining()).unwrap());
  assert!(drain_rx.recv_timeout(Duration::from_millis(250)).unwrap().is_ok());

  release_tx.send(()).unwrap();
  assert!(matches!(
    worker.join().unwrap(),
    Ok(IndexRuntimeWorkOutcomeV1::Completed(IndexProducerWorkerOutcomeV1::MaintenancePage { complete: true, .. }))
  ));
  snapshot.join().unwrap();
  drain.join().unwrap();
  let final_snapshot = owner.snapshot().unwrap();
  assert_eq!(final_snapshot.lifecycle, IndexRuntimeLifecycleV1::Draining);
  assert_eq!(final_snapshot.producer.pending_tasks, 0);
  assert_eq!(final_snapshot.producer.leased_tasks, 0);
  assert_eq!(final_snapshot.mutations.active_records, 4);
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

struct PanickingSpill;

impl IndexProducerSpillStoreV1 for Spill {
  fn spill(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    IndexProducerSpillReceiptV1::new(task.operation_id(), vec![0x61; 32])
  }
}

impl IndexProducerSpillStoreV1 for PanickingSpill {
  fn spill(
    &mut self,
    _task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    panic!("injected finalization spill panic")
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
fn one_owner_executes_the_real_worker_and_retries_the_exact_frozen_batch_before_stopping() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  assert!(matches!(execute_one(&owner, &encoded, 101), IndexRuntimeWorkOutcomeV1::Completed(_)));
  assert_eq!(owner.snapshot().unwrap().mutations.active_records, 4);

  let observed = Arc::new(Mutex::new(Vec::new()));
  let mut retryable = IdentityPublisher { behavior: PublishBehavior::Retryable, observed: Arc::clone(&observed) };
  assert!(matches!(owner.flush(102, Some(IndexFlushReasonV1::Explicit), false, &mut retryable), Err(IndexRuntimeErrorV1::Publication(_))));
  let retained = owner.snapshot().unwrap();
  assert_eq!(retained.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(retained.mutations.active_records, 0);
  assert_eq!(retained.mutations.frozen_records, 4);
  assert_eq!(observed.lock().unwrap().len(), 1);

  let mut success = IdentityPublisher { behavior: PublishBehavior::Success, observed: Arc::clone(&observed) };
  assert_eq!(
    owner.flush(150, Some(IndexFlushReasonV1::Explicit), false, &mut success).unwrap(),
    IndexRuntimeFlushOutcomeV1::Deferred { retry_at_ms: 202 }
  );
  assert!(matches!(
    owner.flush(202, Some(IndexFlushReasonV1::Explicit), false, &mut success).unwrap(),
    IndexRuntimeFlushOutcomeV1::Published { records: 4, .. }
  ));
  let observed = observed.lock().unwrap();
  assert_eq!(observed.len(), 2);
  assert_eq!(observed[0].0, observed[1].0, "preselection retry changed the frozen batch identity");
  assert_ne!(observed[0].1, observed[1].1, "preselection retry did not issue a fresh in-memory attempt handle");
  owner.begin_draining().unwrap();
  owner.finish_draining().unwrap();
  assert_eq!(owner.snapshot().unwrap().lifecycle, IndexRuntimeLifecycleV1::Stopped);
}

#[test]
fn shutdown_drain_bypasses_normal_retry_delay_for_the_exact_frozen_batch() {
  let owner = owner();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/shutdown-retry.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  execute_one(&owner, &encoded, 101);

  let observed = Arc::new(Mutex::new(Vec::new()));
  let mut retryable = IdentityPublisher { behavior: PublishBehavior::Retryable, observed: Arc::clone(&observed) };
  assert!(matches!(owner.flush(102, Some(IndexFlushReasonV1::Explicit), false, &mut retryable), Err(IndexRuntimeErrorV1::Publication(_))));

  owner.begin_draining().unwrap();
  let mut success = IdentityPublisher { behavior: PublishBehavior::Success, observed: Arc::clone(&observed) };
  assert!(matches!(
    owner.flush(103, Some(IndexFlushReasonV1::Shutdown), false, &mut success).unwrap(),
    IndexRuntimeFlushOutcomeV1::Published { records: 4, .. }
  ));
  owner.finish_draining().unwrap();

  let observed = observed.lock().unwrap();
  assert_eq!(observed.len(), 2);
  assert_eq!(observed[0].0, observed[1].0, "shutdown retry changed the frozen batch identity");
  assert_ne!(observed[0].1, observed[1].1, "shutdown retry reused a stale in-memory attempt handle");
}

#[test]
fn one_cadence_drain_publishes_every_bounded_batch_before_stopping() {
  let mut bounded = options();
  bounded.mutations = IndexCoordinatorOptionsV1::new(4 * 1_024 * 1_024, 262_144, 30_000, 800).unwrap();
  let owner = Arc::new(IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, memory(), bounded, 1).unwrap());
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/shutdown-multi-batch.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  execute_one(&owner, &encoded, 101);
  assert_eq!(owner.cached_snapshot().mutations.active_records, 4);

  let cadence = IndexRuntimeCadenceV1::new(
    Arc::clone(&owner),
    ProgressPublisher::default(),
    CancellationToken::new(),
    Arc::new(MockClock::new(71, 102)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  let drained = cadence.drain_and_stop().unwrap();

  assert!(drained.published_batches > 1, "the bounded test fixture did not exercise the multi-batch loop");
  assert_eq!(drained.published_records, 4);
  assert_eq!(drained.highest_checkpoint_sequence, 10 + drained.published_batches);
  assert_eq!(owner.cached_snapshot().lifecycle, IndexRuntimeLifecycleV1::Stopped);
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
fn retry_deadline_overflow_retains_the_batch_and_latches_degraded() {
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
  assert_eq!(snapshot.mutations.active_records, 0);
  assert_eq!(snapshot.mutations.frozen_records, 4);
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

struct BlockingSuccessfulSemanticSource {
  entered: mpsc::Sender<()>,
  release: Mutex<mpsc::Receiver<()>>,
  semantic_state_root: Vec<u8>,
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

impl IndexSemanticScopeSourceV1 for BlockingSuccessfulSemanticSource {
  fn resolve_scopes(
    &self,
    request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    self.entered.send(()).unwrap();
    self.release.lock().unwrap().recv().unwrap();
    SemanticSource { semantic_state_root: self.semantic_state_root.clone() }.resolve_scopes(request)
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
    let revisions = revision_source(&encoded);
    execute_journal_work(&worker_owner, &encoded, &revisions, worker_semantics.as_ref(), 101, &|| false, &mut Spill)
  });
  entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

  let (snapshot_tx, snapshot_rx) = mpsc::channel();
  let snapshot_owner = Arc::clone(&owner);
  let snapshot = std::thread::spawn(move || snapshot_tx.send(snapshot_owner.cached_snapshot()).unwrap());
  let cached = snapshot_rx.recv_timeout(Duration::from_secs(1)).expect("cached runtime observability blocked behind worker state");
  assert_eq!(cached.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(cached.producer.pending_tasks, 0);
  assert_eq!(cached.producer.leased_tasks, 1);

  release_tx.send(()).unwrap();
  assert!(worker.join().unwrap().is_ok());
  snapshot.join().unwrap();
}

#[test]
fn owner_drain_does_not_wait_for_blocked_collection_and_preserves_the_active_lease() {
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
    let revisions = revision_source(&encoded);
    execute_journal_work(&worker_owner, &encoded, &revisions, worker_semantics.as_ref(), 101, &|| false, &mut Spill)
  });
  entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

  let (drain_tx, drain_rx) = mpsc::channel();
  let drain_owner = Arc::clone(&owner);
  let drain = std::thread::spawn(move || drain_tx.send(drain_owner.begin_draining()).unwrap());
  let drain_while_blocked = drain_rx.recv_timeout(Duration::from_millis(250));

  release_tx.send(()).unwrap();
  let worker_result = worker.join().unwrap();
  drain.join().unwrap();

  assert!(drain_while_blocked.is_ok(), "runtime drain blocked behind source/parser/collector work");
  assert!(worker_result.is_ok());
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Draining);
  assert_eq!(snapshot.producer.leased_tasks, 0);
  assert_eq!(snapshot.producer.pending_tasks, 1);
}

#[test]
fn concurrent_degradation_discards_unlocked_collection_and_releases_only_its_lease() {
  let coordinator = memory();
  let owner = Arc::new(IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, coordinator.clone(), options(), 1).unwrap());
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  let retained_before_collection = reserved(&coordinator, MemoryOwner::IndexDirtyBuffers);
  let (entered_tx, entered_rx) = mpsc::channel();
  let (release_tx, release_rx) = mpsc::channel();
  let semantics = Arc::new(BlockingSuccessfulSemanticSource {
    entered: entered_tx,
    release: Mutex::new(release_rx),
    semantic_state_root: journal.semantic_state_root.to_vec(),
  });
  let worker_owner = Arc::clone(&owner);
  let worker_semantics = Arc::clone(&semantics);
  let worker = std::thread::spawn(move || {
    let revisions = revision_source(&encoded);
    execute_journal_work(&worker_owner, &encoded, &revisions, worker_semantics.as_ref(), 101, &|| false, &mut Spill)
  });
  entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

  let oversized_path = format!("/{}", "x".repeat(70 * 1_024));
  assert!(matches!(owner.offer_acknowledgement(&acknowledgement(oversized_path, 8)), SoftMutationAdmissionV1::ReconciliationRequired(_)));
  let mut publisher = Publisher { behavior: PublishBehavior::Success, calls: 0 };
  assert!(matches!(
    owner.flush(102, None, false, &mut publisher),
    Err(IndexRuntimeErrorV1::NotRunning { lifecycle: IndexRuntimeLifecycleV1::Degraded })
  ));
  assert_eq!(publisher.calls, 0);

  release_tx.send(()).unwrap();
  assert!(matches!(worker.join().unwrap(), Err(IndexRuntimeErrorV1::NotRunning { lifecycle: IndexRuntimeLifecycleV1::Degraded })));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.producer.leased_tasks, 0);
  assert_eq!(snapshot.producer.pending_tasks, 1);
  assert_eq!(snapshot.mutations.active_records, 0);
  assert_eq!(snapshot.mutations.frozen_records, 0);
  assert_eq!(reserved(&coordinator, MemoryOwner::IndexDirtyBuffers), retained_before_collection);
}

#[test]
fn runtime_source_has_one_unlocked_collection_and_one_owner_locked_finalization_path() {
  let source = std::fs::read_to_string(format!("{}/src/engine/v4/index_runtime_owner.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  let start = source.find("  fn execute_journal_plan(").unwrap();
  let end = source[start..].find("  fn execute_maintenance_plan(").map(|offset| start + offset).unwrap();
  let implementation = &source[start..end];
  let source_read = implementation.find("request.journal_source.load_journal(").unwrap();
  let collection = implementation.find("self.worker.collect_resolved_mutation(").unwrap();
  let reacquire = implementation.find("self.reacquire_producer_state(lease)").unwrap();
  let finalization = implementation.find("self.worker.finish_mutation_collection(").unwrap();

  assert_eq!(implementation.matches("request.journal_source.load_journal(").count(), 1);
  assert_eq!(implementation.matches("self.worker.collect_resolved_mutation(").count(), 1);
  assert_eq!(implementation.matches("self.worker.finish_mutation_collection(").count(), 1);
  assert_eq!(implementation.matches("self.reacquire_producer_state(lease)").count(), 1);
  assert!(!implementation.contains("self.state.lock()"));
  assert!(!implementation.contains("self.worker.execute("));
  assert!(source_read < collection);
  assert!(collection < reacquire);
  assert!(reacquire < finalization);
}

#[test]
fn runtime_maintenance_source_has_one_unlocked_scan_and_only_the_shared_worker_collection_path() {
  let source = std::fs::read_to_string(format!("{}/src/engine/v4/index_runtime_owner.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  let start = source.find("  fn execute_maintenance_plan(").unwrap();
  let end = source[start..].find("  fn finish_unlocked_source_failure(").map(|offset| start + offset).unwrap();
  let implementation = &source[start..end];
  let scan = implementation.find("request.maintenance_source.scan(").unwrap();
  let collection = implementation.find("self.worker.collect_maintenance_document(").unwrap();
  let reacquire = implementation[collection..].find("self.reacquire_producer_state(lease)").map(|offset| collection + offset).unwrap();
  let finalization = implementation.find("self.worker.finish_maintenance_collection(").unwrap();

  assert_eq!(implementation.matches("request.maintenance_source.scan(").count(), 1);
  assert_eq!(implementation.matches("self.worker.collect_maintenance_document(").count(), 1);
  assert_eq!(implementation.matches("self.worker.finish_maintenance_collection(").count(), 1);
  assert_eq!(implementation.matches("self.worker.finish_maintenance_page(").count(), 1);
  assert!(!implementation.contains("self.state.lock()"));
  assert!(!implementation.contains("IndexProducerCollectorV1"));
  assert!(!implementation.contains("self.worker.collect_resolved_mutation("));
  assert!(!implementation.contains("self.worker.execute("));
  assert!(scan < collection);
  assert!(collection < reacquire);
  assert!(reacquire < finalization);
}

#[test]
fn runtime_owner_has_one_exhaustive_producer_service_dispatcher() {
  let source = std::fs::read_to_string(format!("{}/src/engine/v4/index_runtime_owner.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();

  assert_eq!(source.matches(".lease_next(").count(), 1, "producer service must select one canonical task exactly once");
  assert_eq!(source.matches("pub fn execute_next_producer(").count(), 1);
  assert_eq!(source.matches("index_producer_service_mode_v1(").count(), 1);
  assert!(!source.contains("pub fn execute_next_mutation("));
  assert!(!source.contains("pub fn execute_next_maintenance("));
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
    let result = execute_journal_work(&owner, &encoded, &revisions, &FailingSemanticSource { retryable }, 101, &|| false, &mut Spill);
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
    execute_journal_work(&owner, &encoded, &revisions, &semantics, u64::MAX, &|| false, &mut Spill,),
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
    execute_journal_work(&owner, &encoded, &revisions, &semantics, 101, &|| true, &mut Spill,),
    Err(IndexRuntimeErrorV1::Canceled)
  ));
  assert_eq!(owner.snapshot().unwrap().producer.pending_tasks, 1);
  execute_one(&owner, &encoded, 102);

  let observed = Arc::new(Mutex::new(Vec::new()));
  let mut cancelled = IdentityPublisher { behavior: PublishBehavior::Cancelled, observed: Arc::clone(&observed) };
  assert!(matches!(owner.flush(103, Some(IndexFlushReasonV1::Explicit), false, &mut cancelled), Err(IndexRuntimeErrorV1::Canceled)));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Running);
  assert_eq!(snapshot.mutations.active_records, 0);
  assert_eq!(snapshot.mutations.frozen_records, 4);
  let mut success = IdentityPublisher { behavior: PublishBehavior::Success, observed: Arc::clone(&observed) };
  assert!(matches!(
    owner.flush(104, Some(IndexFlushReasonV1::Explicit), false, &mut success).unwrap(),
    IndexRuntimeFlushOutcomeV1::Published { records: 4, .. }
  ));
  let observed = observed.lock().unwrap();
  assert_eq!(observed.len(), 2);
  assert_eq!(observed[0].0, observed[1].0, "canceled preselection changed the frozen batch identity");
  assert_ne!(observed[0].1, observed[1].1, "canceled preselection did not issue a fresh attempt handle");
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
    execute_journal_work(&owner, &encoded, &revisions, &PanickingSemanticSource, 102, &|| false, &mut Spill,),
    Err(IndexRuntimeErrorV1::Work(_))
  ));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.degraded.unwrap().code, "worker_panic");
  assert_eq!(snapshot.producer.pending_tasks, 1);
  assert_eq!(snapshot.producer.leased_tasks, 0);
}

#[test]
fn finalization_panic_does_not_poison_the_owner_or_strand_the_lease() {
  let mut panic_options = options();
  panic_options.producer = IndexProducerCoordinatorOptionsV1::new(32, 2 * 1_024 * 1_024, 1, 10, 1_000, 16, 256, 2 * 1_024 * 1_024).unwrap();
  let owner = IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, memory(), panic_options, 1).unwrap();
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let revisions = revision_source(&encoded);
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();

  assert!(matches!(
    execute_journal_work(&owner, &encoded, &revisions, &FailingSemanticSource { retryable: true }, 101, &|| false, &mut PanickingSpill,),
    Err(IndexRuntimeErrorV1::Work(_))
  ));
  let snapshot = owner.snapshot().unwrap();
  assert_eq!(snapshot.lifecycle, IndexRuntimeLifecycleV1::Degraded);
  assert_eq!(snapshot.degraded.unwrap().code, "worker_panic");
  assert_eq!(snapshot.producer.pending_tasks, 1);
  assert_eq!(snapshot.producer.leased_tasks, 0);
}

struct CadencePublisher {
  failures_remaining: Arc<AtomicUsize>,
  observed: Arc<Mutex<Vec<(IndexFlushReasonV1, u64, u64)>>>,
}

impl IndexRuntimeBatchPublisherV1 for CadencePublisher {
  fn publish(&mut self, batch: &FrozenIndexBatchV1) -> Result<IndexRuntimePublicationReceiptV1, IndexRuntimePublicationErrorV1> {
    self.observed.lock().unwrap().push((batch.reason(), batch.batch_id(), batch.attempt_id()));
    if self
      .failures_remaining
      .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| if remaining > 0 { Some(remaining - 1) } else { None })
      .is_ok()
    {
      return Err(IndexRuntimePublicationErrorV1::new(
        IndexRuntimePublicationErrorClassV1::RetryableBeforeSelection,
        "cadence_retry",
        "injected retryable cadence publication failure",
      ));
    }
    Ok(IndexRuntimePublicationReceiptV1 {
      batch_id: batch.batch_id(),
      attempt_id: batch.attempt_id(),
      published_records: batch.records().len() as u64,
      publication_bytes: batch.publication_bytes(),
      checkpoint_sequence: 11,
    })
  }
}

fn prepared_cadence_owner(runtime_options: IndexRuntimeOwnerOptionsV1, coordinator_memory: MemoryCoordinator) -> Arc<IndexRuntimeOwnerV1> {
  let owner = Arc::new(IndexRuntimeOwnerV1::new([0x44; 16], ALGORITHM, coordinator_memory, runtime_options, 1).unwrap());
  owner.complete_recovery(IndexRuntimeRecoveryDecisionV1::Ready { recovered_scopes: 1, highest_checkpoint_sequence: 1 }).unwrap();
  let encoded = encoded_journal("/cadence.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  owner.admit_mutation_journal(&journal, 100, &|| false, &mut Spill).unwrap();
  assert!(matches!(execute_one(&owner, &encoded, 101), IndexRuntimeWorkOutcomeV1::Completed(_)));
  owner
}

fn cadence_publisher(failures: usize) -> (CadencePublisher, Arc<AtomicUsize>, Arc<Mutex<Vec<(IndexFlushReasonV1, u64, u64)>>>) {
  let failures_remaining = Arc::new(AtomicUsize::new(failures));
  let observed = Arc::new(Mutex::new(Vec::new()));
  (CadencePublisher { failures_remaining: Arc::clone(&failures_remaining), observed: Arc::clone(&observed) }, failures_remaining, observed)
}

#[test]
fn one_cadence_selects_count_age_and_shared_memory_pressure() {
  let mut count_options = options();
  count_options.mutations = IndexCoordinatorOptionsV1::new(4 * 1_024 * 1_024, 1, 30_000, 256 * 1_024).unwrap();
  let count_owner = prepared_cadence_owner(count_options, memory());
  let (publisher, _, observed) = cadence_publisher(0);
  let cadence =
    IndexRuntimeCadenceV1::new(count_owner, publisher, CancellationToken::new(), Arc::new(MockClock::new(1, 102)) as Arc<dyn VirtualClock>)
      .unwrap();
  assert!(matches!(cadence.flush_if_due().unwrap(), IndexRuntimeFlushOutcomeV1::Published { records: 4, .. }));
  assert_eq!(observed.lock().unwrap()[0].0, IndexFlushReasonV1::MutationCount);

  let age_owner = prepared_cadence_owner(options(), memory());
  let (publisher, _, observed) = cadence_publisher(0);
  let cadence = IndexRuntimeCadenceV1::new(
    age_owner,
    publisher,
    CancellationToken::new(),
    Arc::new(MockClock::new(2, 30_101)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  assert!(matches!(cadence.flush_if_due().unwrap(), IndexRuntimeFlushOutcomeV1::Published { records: 4, .. }));
  assert_eq!(observed.lock().unwrap()[0].0, IndexFlushReasonV1::Age);

  let pressure_memory = memory();
  let pressure_owner = prepared_cadence_owner(options(), pressure_memory.clone());
  pressure_memory.update_host_sample(HostMemorySample { rss_bytes: 48 * 1_024 * 1_024, ..Default::default() }).unwrap();
  let (publisher, _, observed) = cadence_publisher(0);
  let cadence = IndexRuntimeCadenceV1::new(
    pressure_owner,
    publisher,
    CancellationToken::new(),
    Arc::new(MockClock::new(3, 102)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  assert!(matches!(cadence.flush_if_due().unwrap(), IndexRuntimeFlushOutcomeV1::Published { records: 4, .. }));
  assert_eq!(observed.lock().unwrap()[0].0, IndexFlushReasonV1::MemoryPressure);
}

#[test]
fn cadence_cancellation_and_retry_retain_the_exact_batch() {
  let mut runtime_options = options();
  runtime_options.mutations = IndexCoordinatorOptionsV1::new(4 * 1_024 * 1_024, 1, 30_000, 256 * 1_024).unwrap();
  let cancelled_owner = prepared_cadence_owner(runtime_options, memory());
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let (publisher, _, observed) = cadence_publisher(0);
  let cancelled = IndexRuntimeCadenceV1::new(
    Arc::clone(&cancelled_owner),
    publisher,
    cancellation,
    Arc::new(MockClock::new(4, 102)) as Arc<dyn VirtualClock>,
  )
  .unwrap();
  assert!(matches!(cancelled.flush_if_due(), Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Canceled))));
  assert!(observed.lock().unwrap().is_empty());
  assert_eq!(cancelled_owner.snapshot().unwrap().mutations.active_records, 4);

  let retry_owner = prepared_cadence_owner(runtime_options, memory());
  let (publisher, failures, observed) = cadence_publisher(1);
  let clock = Arc::new(MockClock::new(5, 102));
  let retry =
    IndexRuntimeCadenceV1::new(Arc::clone(&retry_owner), publisher, CancellationToken::new(), Arc::clone(&clock) as Arc<dyn VirtualClock>)
      .unwrap();
  assert!(matches!(retry.flush_if_due(), Err(IndexRuntimeCadenceErrorV1::Runtime(IndexRuntimeErrorV1::Publication(_)))));
  assert_eq!(failures.load(Ordering::SeqCst), 0);
  assert_eq!(retry_owner.snapshot().unwrap().mutations.frozen_records, 4);
  clock.advance(100);
  assert!(matches!(retry.flush_if_due().unwrap(), IndexRuntimeFlushOutcomeV1::Published { records: 4, .. }));
  let observed = observed.lock().unwrap();
  assert_eq!(observed.len(), 2);
  assert_eq!(observed[0].1, observed[1].1, "cadence retry changed the frozen batch identity");
  assert_ne!(observed[0].2, observed[1].2, "cadence retry reused an attempt identity");
}

#[test]
fn concurrent_cadence_ticks_serialize_one_mutable_publisher() {
  let mut runtime_options = options();
  runtime_options.mutations = IndexCoordinatorOptionsV1::new(4 * 1_024 * 1_024, 1, 30_000, 256 * 1_024).unwrap();
  let owner = prepared_cadence_owner(runtime_options, memory());
  let (publisher, _, observed) = cadence_publisher(0);
  let cadence = Arc::new(
    IndexRuntimeCadenceV1::new(owner, publisher, CancellationToken::new(), Arc::new(MockClock::new(6, 102)) as Arc<dyn VirtualClock>)
      .unwrap(),
  );
  let first_cadence = Arc::clone(&cadence);
  let first = std::thread::spawn(move || first_cadence.flush_if_due());
  let second_cadence = Arc::clone(&cadence);
  let second = std::thread::spawn(move || second_cadence.flush_if_due());
  let outcomes = [first.join().unwrap().unwrap(), second.join().unwrap().unwrap()];
  assert_eq!(outcomes.iter().filter(|outcome| matches!(outcome, IndexRuntimeFlushOutcomeV1::Published { .. })).count(), 1);
  assert_eq!(outcomes.iter().filter(|outcome| matches!(outcome, IndexRuntimeFlushOutcomeV1::Idle)).count(), 1);
  assert_eq!(observed.lock().unwrap().len(), 1);
}
