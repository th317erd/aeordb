use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::index_coordinator::{IndexCoordinatorOptionsV1, IndexCoordinatorV1};
use aeordb::engine::v4::index_producer_admission::admit_mutation_journal_tasks;
use aeordb::engine::v4::index_producer_collector::{
  IndexParserExecutionErrorV1, IndexParserExecutionRequestV1, IndexParserExecutorV1, IndexParserOutcomeV1, IndexProducerCollectorOptionsV1,
  IndexProducerCollectorV1,
};
use aeordb::engine::v4::index_producer_coordinator::{
  IndexProducerCompletionV1, IndexProducerCoordinatorErrorV1, IndexProducerCoordinatorOptionsV1, IndexProducerCoordinatorV1,
  IndexProducerSpillErrorV1, IndexProducerSpillReasonV1, IndexProducerSpillReceiptV1, IndexProducerSpillStoreV1, IndexProducerTaskViewV1,
};
use aeordb::engine::v4::index_producer_executor::IndexProducerExecutorV1;
use aeordb::engine::v4::index_producer_source::{
  IndexFileRevisionReadErrorV1, IndexFileRevisionSourceV1, IndexProducerSourceErrorV1, IndexSemanticScopeLimitsV1,
  IndexSemanticScopeReadErrorV1, IndexSemanticScopeReadV1, IndexSemanticScopeResolutionV1, IndexSemanticScopeSourceV1,
  LoadedIndexFileRevisionV1, OwnedIndexFieldDefinitionV1, OwnedIndexScopeDefinitionV1, OwnedIndexValueStoreDefinitionV1,
  ResolvedIndexDocumentTransitionV1, ResolvedIndexScopeWorkV1,
};
use aeordb::engine::v4::index_producer_worker::{
  IndexProducerMutationWorkRequestV1, IndexProducerMutationWorkerV1, IndexProducerWorkerErrorV1, IndexProducerWorkerOutcomeV1,
};
use aeordb::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_mutation_journal,
  encode_mutation_journal,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn hash(label: &[u8]) -> Vec<u8> {
  aeordb::engine::v4::hash::digest_parts(ALGORITHM, &[b"producer-worker:", label])
}

fn memory(hard_limit_bytes: u64) -> MemoryCoordinator {
  let emergency = (hard_limit_bytes / 4).max(1);
  MemoryCoordinator::new(MemoryPolicy::new(hard_limit_bytes - emergency - 1, hard_limit_bytes, 1, emergency).unwrap())
}

fn fixture(folder: &str, name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{folder}/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn file(path: &str, content: u8) -> FileRecord {
  FileRecord {
    path: path.to_string(),
    content_type: Some("application/json".to_string()),
    total_size: 32,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_001,
    metadata: Vec::new(),
    content_hash: vec![content; 32],
    chunk_hashes: vec![vec![content.wrapping_add(1); 32]],
  }
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

#[derive(Default)]
struct RevisionSource {
  records: BTreeMap<(Vec<u8>, String), LoadedIndexFileRevisionV1>,
  failure: Option<IndexFileRevisionReadErrorV1>,
}

impl IndexFileRevisionSourceV1 for RevisionSource {
  fn load_file_revision(
    &self,
    namespace_root: &[u8],
    path: &str,
  ) -> Result<Option<LoadedIndexFileRevisionV1>, IndexFileRevisionReadErrorV1> {
    if let Some(error) = &self.failure {
      return Err(error.clone());
    }
    Ok(self.records.get(&(namespace_root.to_vec(), path.to_string())).cloned())
  }
}

#[derive(Clone)]
struct SemanticSource {
  result: Result<IndexSemanticScopeResolutionV1, IndexSemanticScopeReadErrorV1>,
}

impl IndexSemanticScopeSourceV1 for SemanticSource {
  fn resolve_scopes(
    &self,
    _semantic_state_root: &[u8],
    _transition: &ResolvedIndexDocumentTransitionV1,
    _limits: IndexSemanticScopeLimitsV1,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    let resolution = self.result.clone()?;
    let reservation = memory(8 * 1_024 * 1_024)
      .reserve(MemoryOwner::Task, 2 * 1_024 * 1_024, AdmissionClass::Workload)
      .map_err(|error| IndexSemanticScopeReadErrorV1::retryable("test_memory", error.to_string()))?;
    IndexSemanticScopeReadV1::new(resolution, reservation)
  }
}

struct RetainedSemanticSource {
  resolution: IndexSemanticScopeResolutionV1,
  memory: MemoryCoordinator,
  reserved_bytes: u64,
}

impl IndexSemanticScopeSourceV1 for RetainedSemanticSource {
  fn resolve_scopes(
    &self,
    _semantic_state_root: &[u8],
    _transition: &ResolvedIndexDocumentTransitionV1,
    _limits: IndexSemanticScopeLimitsV1,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    let reservation = self
      .memory
      .reserve(MemoryOwner::Task, self.reserved_bytes, AdmissionClass::Workload)
      .map_err(|error| IndexSemanticScopeReadErrorV1::retryable("test_memory", error.to_string()))?;
    IndexSemanticScopeReadV1::new(self.resolution.clone(), reservation)
  }
}

fn scope_work(semantic_state_root: &[u8], ordinal: u64) -> ResolvedIndexScopeWorkV1 {
  scope_work_with_value(semantic_state_root, ordinal, "avst-blake3-256-metadata-hash-corrected-valid.bin")
}

fn scope_work_with_value(semantic_state_root: &[u8], ordinal: u64, value_fixture: &str) -> ResolvedIndexScopeWorkV1 {
  let scope = fixture("scope-definition-v1", "ascp-blake3-256-root-direct-valid.bin");
  let scope_id = decode_scope_definition(&scope, ALGORITHM).unwrap().scope_id;
  let mut value = fixture("value-store-definition-v1", value_fixture);
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
    Err(IndexParserExecutionErrorV1::host_failure("unexpected_parser", "metadata-only definition invoked the parser"))
  }
}

struct ReservationObservingParser {
  memory: MemoryCoordinator,
  minimum_reserved_bytes: u64,
  observed: AtomicBool,
}

impl IndexParserExecutorV1 for ReservationObservingParser {
  fn parse(&self, _request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    let reserved_bytes = self
      .memory
      .snapshot()
      .map_err(|error| IndexParserExecutionErrorV1::host_failure("memory_snapshot", error.to_string()))?
      .owner(MemoryOwner::Task)
      .map_or(0, |owner| owner.reserved_bytes);
    if reserved_bytes < self.minimum_reserved_bytes {
      return Err(IndexParserExecutionErrorV1::host_failure(
        "semantic_reservation_released",
        format!("parser observed {reserved_bytes} task bytes, expected at least {}", self.minimum_reserved_bytes),
      ));
    }
    self.observed.store(true, Ordering::SeqCst);
    Ok(IndexParserOutcomeV1::Parsed(CanonicalConfigValueV1::Map(BTreeMap::from([(
      "messages".to_string(),
      CanonicalConfigValueV1::Array(vec![CanonicalConfigValueV1::Map(BTreeMap::from([(
        "user".to_string(),
        CanonicalConfigValueV1::String("retained".to_string()),
      )]))]),
    )]))))
  }
}

struct SpillStore;

impl IndexProducerSpillStoreV1 for SpillStore {
  fn spill(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    IndexProducerSpillReceiptV1::new(task.operation_id(), hash(b"spill"))
  }
}

struct RefusingSpillStore;

impl IndexProducerSpillStoreV1 for RefusingSpillStore {
  fn spill(
    &mut self,
    _task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    Err(IndexProducerSpillErrorV1::new("spill_refused", "injected spill refusal"))
  }
}

fn producer_with_attempts(max_attempts: u16) -> IndexProducerCoordinatorV1 {
  IndexProducerCoordinatorV1::new(
    ALGORITHM,
    memory(8 * 1_024 * 1_024),
    IndexProducerCoordinatorOptionsV1::new(8, 2 * 1_024 * 1_024, max_attempts, 10, 1_000, 16, 256, 2 * 1_024 * 1_024).unwrap(),
  )
  .unwrap()
}

fn producer() -> IndexProducerCoordinatorV1 {
  producer_with_attempts(3)
}

fn mutations(max_bytes: u64) -> IndexCoordinatorV1 {
  IndexCoordinatorV1::new(
    [0x44; 16],
    ALGORITHM,
    memory(8 * 1_024 * 1_024),
    IndexCoordinatorOptionsV1::new(max_bytes, 262_144, 30_000, 256 * 1024).unwrap(),
    1,
  )
  .unwrap()
}

fn worker_with_retry(source_retry_after_ms: u64) -> Result<IndexProducerMutationWorkerV1, IndexProducerWorkerErrorV1> {
  let collector = IndexProducerCollectorV1::new(
    ALGORITHM,
    memory(32 * 1_024 * 1_024),
    IndexProducerCollectorOptionsV1::new(8, 16, 32, 2 * 1_024 * 1_024, 256, 2 * 1_024 * 1_024, 50).unwrap(),
  )
  .unwrap();
  IndexProducerMutationWorkerV1::new(
    ALGORITHM,
    IndexProducerExecutorV1::new(collector),
    IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 * 1_024 * 1_024).unwrap(),
    source_retry_after_ms,
  )
}

fn worker() -> IndexProducerMutationWorkerV1 {
  worker_with_retry(25).unwrap()
}

fn revision_source(encoded: &[u8]) -> RevisionSource {
  let journal = decode_mutation_journal(encoded, ALGORITHM).unwrap();
  let record = journal.records.iter().next().unwrap().unwrap();
  let mut source = RevisionSource::default();
  source.records.insert(
    (record.root_after.to_vec(), record.after_path.unwrap().to_string()),
    LoadedIndexFileRevisionV1 {
      revision_hash: record.after_revision.unwrap().to_vec(),
      file_record: file(record.after_path.unwrap(), 0x42),
    },
  );
  source
}

fn admitted(encoded: &[u8]) -> (IndexProducerCoordinatorV1, aeordb::engine::v4::index_producer_coordinator::IndexProducerLeaseV1) {
  let journal = decode_mutation_journal(encoded, ALGORITHM).unwrap();
  let mut producer = producer();
  admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 100, &|| false, &mut SpillStore).unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  (producer, lease)
}

#[test]
fn one_worker_composes_exact_sources_scopes_and_leased_execution() {
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let revisions = revision_source(&encoded);
  let semantic_root = journal.semantic_state_root.to_vec();
  let semantics = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: semantic_root.clone(),
      scope_work: vec![scope_work(&semantic_root, 3)],
    }),
  };
  let (mut producer, lease) = admitted(&encoded);
  let mut mutations = mutations(2 * 1_024 * 1_024);

  let outcome = worker()
    .execute(
      &mut producer,
      &mut mutations,
      IndexProducerMutationWorkRequestV1 {
        lease: &lease,
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 101,
        is_cancelled: &|| false,
      },
      &mut SpillStore,
    )
    .unwrap();

  assert!(matches!(outcome, IndexProducerWorkerOutcomeV1::Completed(IndexProducerCompletionV1::Completed { .. })));
  assert_eq!(producer.snapshot().pending_tasks, 0);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(mutations.snapshot().active_records, 4);
}

#[test]
fn semantic_task_memory_remains_reserved_through_parser_execution_and_releases_after_completion() {
  const SEMANTIC_BYTES: u64 = 2 * 1_024 * 1_024;
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let revisions = revision_source(&encoded);
  let semantic_root = journal.semantic_state_root.to_vec();
  let semantic_memory = memory(8 * 1_024 * 1_024);
  let semantics = RetainedSemanticSource {
    resolution: IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: semantic_root.clone(),
      scope_work: vec![scope_work_with_value(&semantic_root, 3, "avst-blake3-256-json-corrected-valid.bin")],
    },
    memory: semantic_memory.clone(),
    reserved_bytes: SEMANTIC_BYTES,
  };
  let parser = ReservationObservingParser {
    memory: semantic_memory.clone(),
    minimum_reserved_bytes: SEMANTIC_BYTES,
    observed: AtomicBool::new(false),
  };
  let (mut producer, lease) = admitted(&encoded);
  let mut mutations = mutations(2 * 1_024 * 1_024);

  let outcome = worker()
    .execute(
      &mut producer,
      &mut mutations,
      IndexProducerMutationWorkRequestV1 {
        lease: &lease,
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &parser,
        mapper: None,
        now_ms: 101,
        is_cancelled: &|| false,
      },
      &mut SpillStore,
    )
    .unwrap();

  assert!(matches!(outcome, IndexProducerWorkerOutcomeV1::Completed(IndexProducerCompletionV1::Completed { .. })));
  assert!(parser.observed.load(Ordering::SeqCst));
  assert_eq!(semantic_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn content_only_completion_is_explicit_and_never_enters_the_collector() {
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let revisions = revision_source(&encoded);
  let semantic_root = journal.semantic_state_root.to_vec();
  let semantics = SemanticSource { result: Ok(IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root: semantic_root.clone() }) };
  let (mut producer, lease) = admitted(&encoded);
  let mut mutations = mutations(2 * 1_024 * 1_024);

  let outcome = worker()
    .execute(
      &mut producer,
      &mut mutations,
      IndexProducerMutationWorkRequestV1 {
        lease: &lease,
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 101,
        is_cancelled: &|| false,
      },
      &mut SpillStore,
    )
    .unwrap();

  assert!(matches!(
    outcome,
    IndexProducerWorkerOutcomeV1::ContentOnly { semantic_state_root, completion: IndexProducerCompletionV1::Completed { .. } }
      if semantic_state_root == semantic_root
  ));
  assert_eq!(producer.snapshot().pending_tasks, 0);
  assert_eq!(mutations.snapshot().active_records, 0);
}

#[test]
fn retryable_source_failure_uses_task_backoff_instead_of_hot_looping() {
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let revisions = RevisionSource {
    records: BTreeMap::new(),
    failure: Some(IndexFileRevisionReadErrorV1::retryable("storage_busy", "injected source outage")),
  };
  let semantic_root = journal.semantic_state_root.to_vec();
  let semantics = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: semantic_root.clone(),
      scope_work: vec![scope_work(&semantic_root, 3)],
    }),
  };
  let (mut producer, lease) = admitted(&encoded);
  let mut mutations = mutations(2 * 1_024 * 1_024);

  let outcome = worker()
    .execute(
      &mut producer,
      &mut mutations,
      IndexProducerMutationWorkRequestV1 {
        lease: &lease,
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 101,
        is_cancelled: &|| false,
      },
      &mut SpillStore,
    )
    .unwrap();

  assert!(matches!(
    outcome,
    IndexProducerWorkerOutcomeV1::SourceRetry {
      completion: IndexProducerCompletionV1::RetryScheduled { attempt: 1, next_retry_at_ms: 126, .. },
      ..
    }
  ));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert!(producer.lease_next(125, false).unwrap().is_none());
  assert!(producer.lease_next(126, false).unwrap().is_some());
}

#[test]
fn terminal_source_failure_and_cancellation_release_the_lease_without_consuming_work() {
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let revisions = RevisionSource {
    records: BTreeMap::new(),
    failure: Some(IndexFileRevisionReadErrorV1::corrupt("revision_corrupt", "injected revision corruption")),
  };
  let semantic_root = journal.semantic_state_root.to_vec();
  let semantics = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: semantic_root.clone(),
      scope_work: vec![scope_work(&semantic_root, 3)],
    }),
  };
  let (mut producer, lease) = admitted(&encoded);
  let mut mutations = mutations(2 * 1_024 * 1_024);

  let error = worker()
    .execute(
      &mut producer,
      &mut mutations,
      IndexProducerMutationWorkRequestV1 {
        lease: &lease,
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 101,
        is_cancelled: &|| false,
      },
      &mut SpillStore,
    )
    .unwrap_err();
  assert!(matches!(error, IndexProducerWorkerErrorV1::Source(_)));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);

  let lease = producer.lease_next(101, false).unwrap().unwrap();
  let error = worker()
    .execute(
      &mut producer,
      &mut mutations,
      IndexProducerMutationWorkRequestV1 {
        lease: &lease,
        journal: &journal,
        revision_source: &revision_source(&encoded),
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 102,
        is_cancelled: &|| true,
      },
      &mut SpillStore,
    )
    .unwrap_err();
  assert_eq!(error, IndexProducerWorkerErrorV1::Cancelled);
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(mutations.snapshot().active_records, 0);
}

#[test]
fn semantic_source_retry_and_executor_pressure_follow_the_same_lease_owner() {
  assert!(matches!(worker_with_retry(0), Err(IndexProducerWorkerErrorV1::InvalidOptions(_))));

  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let revisions = revision_source(&encoded);
  let semantics =
    SemanticSource { result: Err(IndexSemanticScopeReadErrorV1::retryable("semantic_busy", "injected semantic source outage")) };
  let (mut producer, lease) = admitted(&encoded);
  let mut admitted_mutations = mutations(2 * 1_024 * 1_024);
  let outcome = worker()
    .execute(
      &mut producer,
      &mut admitted_mutations,
      IndexProducerMutationWorkRequestV1 {
        lease: &lease,
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 101,
        is_cancelled: &|| false,
      },
      &mut SpillStore,
    )
    .unwrap();
  assert!(matches!(
    outcome,
    IndexProducerWorkerOutcomeV1::SourceRetry {
      completion: IndexProducerCompletionV1::RetryScheduled { attempt: 1, next_retry_at_ms: 126, .. },
      ..
    }
  ));
  assert_eq!(producer.snapshot().leased_tasks, 0);

  let semantic_root = journal.semantic_state_root.to_vec();
  let semantics = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: semantic_root.clone(),
      scope_work: vec![scope_work(&semantic_root, 3)],
    }),
  };
  let lease = producer.lease_next(126, false).unwrap().unwrap();
  let mut pressured_mutations = mutations(1);
  let error = worker()
    .execute(
      &mut producer,
      &mut pressured_mutations,
      IndexProducerMutationWorkRequestV1 {
        lease: &lease,
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 127,
        is_cancelled: &|| false,
      },
      &mut SpillStore,
    )
    .unwrap_err();
  assert!(matches!(error, IndexProducerWorkerErrorV1::Execution(_)));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(pressured_mutations.snapshot().active_records, 0);
}

#[test]
fn source_retry_failure_preserves_both_source_and_spill_evidence() {
  let encoded = encoded_journal("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let revisions = revision_source(&encoded);
  let semantics =
    SemanticSource { result: Err(IndexSemanticScopeReadErrorV1::retryable("semantic_busy", "injected semantic source outage")) };
  let mut producer = producer_with_attempts(1);
  admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 100, &|| false, &mut SpillStore).unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let mut mutations = mutations(2 * 1_024 * 1_024);

  let error = worker()
    .execute(
      &mut producer,
      &mut mutations,
      IndexProducerMutationWorkRequestV1 {
        lease: &lease,
        journal: &journal,
        revision_source: &revisions,
        semantic_source: &semantics,
        parser: &UnexpectedParser,
        mapper: None,
        now_ms: 101,
        is_cancelled: &|| false,
      },
      &mut RefusingSpillStore,
    )
    .unwrap_err();

  match error {
    IndexProducerWorkerErrorV1::RetryAfterSource { source, retry } => {
      assert!(matches!(
        *source,
        IndexProducerSourceErrorV1::SemanticRead(ref source) if source.code() == "semantic_busy"
      ));
      assert!(matches!(retry, IndexProducerCoordinatorErrorV1::SpillFailed { code: "spill_refused", .. }));
    }
    other => panic!("unexpected combined source/spill error: {other}"),
  }
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(mutations.snapshot().active_records, 0);
}

#[test]
fn mutation_worker_remains_disconnected_until_runtime_ownership_lands() {
  let manifest = std::fs::read_to_string(format!("{}/src/engine/v4/mod.rs", env!("CARGO_MANIFEST_DIR"))).unwrap();
  assert_eq!(manifest.matches("pub mod index_producer_worker;").count(), 1);
  for path in [
    "src/engine/storage_engine.rs",
    "src/engine/directory_ops.rs",
    "src/engine/task_worker.rs",
    "src/engine/namespace_mutation.rs",
    "src/server/mod.rs",
  ] {
    let source = std::fs::read_to_string(format!("{}/../aeordb-lib/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    assert!(!source.contains("IndexProducerMutationWorkerV1"), "{path} activated the v4 mutation worker before P6-2d");
    assert!(!source.contains("index_producer_worker"), "{path} imports the v4 mutation worker before P6-2d");
  }
}
