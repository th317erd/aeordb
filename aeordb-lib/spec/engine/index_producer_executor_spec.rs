use aeordb::engine::HashAlgorithm;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::index_coordinator::{IndexCoordinatorOptionsV1, IndexCoordinatorV1};
use aeordb::engine::v4::index_producer_collector::{
  IndexCollectorDocumentTransitionV1, IndexCollectorDocumentV1, IndexCollectorFieldDefinitionV1, IndexCollectorScopeDefinitionV1,
  IndexCollectorValueStoreDefinitionV1, IndexParserExecutionErrorV1, IndexParserExecutionRequestV1, IndexParserExecutorV1,
  IndexParserOutcomeV1, IndexProducerCollectorOptionsV1, IndexProducerCollectorV1,
};
use aeordb::engine::v4::index_producer_coordinator::{
  IndexProducerCoordinatorOptionsV1, IndexProducerCoordinatorV1, IndexProducerSpillErrorV1, IndexProducerSpillReasonV1,
  IndexProducerSpillReceiptV1, IndexProducerSpillStoreV1, IndexProducerTaskKindV1, IndexProducerTaskRequestV1, IndexProducerTaskViewV1,
};
use aeordb::engine::v4::index_producer_executor::{IndexProducerExecutionErrorV1, IndexProducerExecutionInputV1, IndexProducerExecutorV1};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn memory(hard_limit_bytes: u64) -> MemoryCoordinator {
  let emergency = (hard_limit_bytes / 4).max(1);
  MemoryCoordinator::new(MemoryPolicy::new(hard_limit_bytes - emergency - 1, hard_limit_bytes, 1, emergency).unwrap())
}

fn fixture(folder: &str, name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{folder}/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

struct Definitions {
  scope: Vec<u8>,
  scope_id: Vec<u8>,
  value: Vec<u8>,
  value_id: Vec<u8>,
  field: Vec<u8>,
  field_id: Vec<u8>,
}

fn definitions_from_scope(scope: Vec<u8>) -> Definitions {
  let scope_id = decode_scope_definition(&scope, ALGORITHM).unwrap().scope_id;
  let mut value = fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid.bin");
  value[32..64].copy_from_slice(&scope_id);
  let value_id = decode_value_store_definition(&value, ALGORITHM).unwrap().value_store_id;
  let mut field = fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  field[32..64].copy_from_slice(&value_id);
  let field_id = decode_field_index_definition(&field, ALGORITHM).unwrap().index_id;
  Definitions { scope, scope_id, value, value_id, field, field_id }
}

fn definitions_for_scope(fixture_name: &str) -> Definitions {
  definitions_from_scope(fixture("scope-definition-v1", fixture_name))
}

fn definitions() -> Definitions {
  definitions_for_scope("ascp-blake3-256-root-direct-valid.bin")
}

fn direct_scope(owner_path: &str) -> Vec<u8> {
  let fixture = fixture("scope-definition-v1", "ascp-blake3-256-root-direct-valid.bin");
  let mut encoded = fixture[..64].to_vec();
  encoded.extend_from_slice(owner_path.as_bytes());
  let encoded_length = encoded.len() as u32;
  encoded[8..12].copy_from_slice(&encoded_length.to_le_bytes());
  encoded[32..36].copy_from_slice(&(owner_path.len() as u32).to_le_bytes());
  encoded
}

fn scope_bundle(definitions: &Definitions) -> IndexCollectorScopeDefinitionV1<'_> {
  IndexCollectorScopeDefinitionV1 {
    expected_scope_id: &definitions.scope_id,
    encoded_definition: &definitions.scope,
    value_stores: vec![IndexCollectorValueStoreDefinitionV1 {
      expected_value_store_id: &definitions.value_id,
      encoded_definition: &definitions.value,
      field_indexes: vec![IndexCollectorFieldDefinitionV1 {
        expected_index_id: &definitions.field_id,
        encoded_definition: &definitions.field,
      }],
    }],
  }
}

fn hash(label: &[u8]) -> Vec<u8> {
  aeordb::engine::v4::hash::digest_parts(ALGORITHM, &[b"executor:", label])
}

fn file(path: &str) -> FileRecord {
  FileRecord {
    path: path.to_string(),
    content_type: Some("application/json".to_string()),
    total_size: 32,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_001,
    metadata: Vec::new(),
    content_hash: vec![0x44; 32],
    chunk_hashes: vec![vec![0x45; 32]],
  }
}

struct UnexpectedParser;

impl IndexParserExecutorV1 for UnexpectedParser {
  fn parse(&self, _request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    Err(IndexParserExecutionErrorV1::host_failure("unexpected_parser", "metadata-only definition invoked the parser"))
  }
}

#[derive(Default)]
struct SpillStore;

impl IndexProducerSpillStoreV1 for SpillStore {
  fn spill(
    &mut self,
    _task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    IndexProducerSpillReceiptV1::new([0x55; 16], hash(b"spill"))
  }
}

fn producer(memory: MemoryCoordinator) -> IndexProducerCoordinatorV1 {
  IndexProducerCoordinatorV1::new(
    ALGORITHM,
    memory,
    IndexProducerCoordinatorOptionsV1::new(8, 128 * 1024, 3, 10, 1_000, 16, 256, 2 * 1_024 * 1_024).unwrap(),
  )
  .unwrap()
}

fn mutations(memory: MemoryCoordinator, max_bytes: u64) -> IndexCoordinatorV1 {
  IndexCoordinatorV1::new([0x44; 16], ALGORITHM, memory, IndexCoordinatorOptionsV1::new(max_bytes, 262_144, 30_000, 256 * 1024).unwrap(), 1)
    .unwrap()
}

fn collector(memory: MemoryCoordinator) -> IndexProducerCollectorV1 {
  IndexProducerCollectorV1::new(
    ALGORITHM,
    memory,
    IndexProducerCollectorOptionsV1::new(16, 16, 16, 2 * 1_024 * 1_024, 256, 2 * 1_024 * 1_024, 50).unwrap(),
  )
  .unwrap()
}

fn admit_mutation_task(producer: &mut IndexProducerCoordinatorV1, before: &[u8], after: &[u8], semantic: &[u8]) {
  producer
    .admit(
      IndexProducerTaskRequestV1 {
        operation_id: [0x22; 16],
        kind: IndexProducerTaskKindV1::MutationWindow,
        publication_sequence: 7,
        namespace_root_before: before,
        namespace_root_after: after,
        semantic_state_root: semantic,
        journal_head: Some(hash(b"journal").as_slice()),
        scope: None,
      },
      100,
    )
    .unwrap();
}

#[test]
fn exact_leased_transition_collects_and_completes_while_memory_remains_accounted() {
  let definitions = definitions();
  let before_root = hash(b"before");
  let after_root = hash(b"after");
  let semantic = hash(b"semantic");
  let revision = hash(b"revision");
  let record = file("/doc.json");
  let task_memory = memory(8 * 1_024 * 1_024);
  let dirty_memory = memory(8 * 1_024 * 1_024);
  let mut producer = producer(task_memory.clone());
  let mut mutations = mutations(dirty_memory, 2 * 1_024 * 1_024);
  admit_mutation_task(&mut producer, &before_root, &after_root, &semantic);
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let executor = IndexProducerExecutorV1::new(collector(task_memory.clone()));

  let completion = executor
    .execute_transition(
      &mut producer,
      &mut mutations,
      &lease,
      IndexProducerExecutionInputV1 {
        semantic_state_root: &semantic,
        scope_bundles: vec![scope_bundle(&definitions)],
        transition: IndexCollectorDocumentTransitionV1 {
          document_ordinal: 3,
          before: None,
          after: Some(IndexCollectorDocumentV1 { namespace_root: &after_root, record_revision_hash: &revision, file_record: &record }),
        },
      },
      &UnexpectedParser,
      None,
      101,
      &|| false,
      &mut SpillStore,
    )
    .unwrap();

  assert!(matches!(completion, aeordb::engine::v4::index_producer_coordinator::IndexProducerCompletionV1::Completed { .. }));
  assert_eq!(producer.snapshot().pending_tasks, 0);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(mutations.snapshot().active_records, 4);
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn mismatched_roots_and_scope_fail_before_collection_and_release_the_lease_for_retry() {
  let definitions = definitions();
  let root = hash(b"root");
  let wrong_root = hash(b"wrong");
  let semantic = hash(b"semantic");
  let revision = hash(b"revision");
  let record = file("/doc.json");
  let task_memory = memory(8 * 1_024 * 1_024);
  let mut producer = producer(task_memory.clone());
  producer
    .admit(
      IndexProducerTaskRequestV1 {
        operation_id: [0x33; 16],
        kind: IndexProducerTaskKindV1::Rebuild,
        publication_sequence: 8,
        namespace_root_before: &root,
        namespace_root_after: &root,
        semantic_state_root: &semantic,
        journal_head: None,
        scope: Some("/docs"),
      },
      100,
    )
    .unwrap();
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let executor = IndexProducerExecutorV1::new(collector(task_memory.clone()));
  let mut mutations = mutations(memory(8 * 1_024 * 1_024), 2 * 1_024 * 1_024);

  let error = executor
    .execute_transition(
      &mut producer,
      &mut mutations,
      &lease,
      IndexProducerExecutionInputV1 {
        semantic_state_root: &semantic,
        scope_bundles: vec![scope_bundle(&definitions)],
        transition: IndexCollectorDocumentTransitionV1 {
          document_ordinal: 3,
          before: None,
          after: Some(IndexCollectorDocumentV1 { namespace_root: &wrong_root, record_revision_hash: &revision, file_record: &record }),
        },
      },
      &UnexpectedParser,
      None,
      101,
      &|| false,
      &mut SpillStore,
    )
    .unwrap_err();
  assert!(matches!(error, IndexProducerExecutionErrorV1::TaskMismatch(_)));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(mutations.snapshot().active_records, 0);

  let lease = producer.lease_next(101, false).unwrap().unwrap();
  let error = executor
    .execute_transition(
      &mut producer,
      &mut mutations,
      &lease,
      IndexProducerExecutionInputV1 {
        semantic_state_root: &wrong_root,
        scope_bundles: vec![scope_bundle(&definitions)],
        transition: IndexCollectorDocumentTransitionV1 {
          document_ordinal: 3,
          before: None,
          after: Some(IndexCollectorDocumentV1 { namespace_root: &root, record_revision_hash: &revision, file_record: &record }),
        },
      },
      &UnexpectedParser,
      None,
      102,
      &|| false,
      &mut SpillStore,
    )
    .unwrap_err();
  assert!(matches!(error, IndexProducerExecutionErrorV1::TaskMismatch(_)));
  assert_eq!(producer.snapshot().leased_tasks, 0);

  let lease = producer.lease_next(102, false).unwrap().unwrap();
  let error = executor
    .execute_transition(
      &mut producer,
      &mut mutations,
      &lease,
      IndexProducerExecutionInputV1 {
        semantic_state_root: &semantic,
        scope_bundles: vec![scope_bundle(&definitions)],
        transition: IndexCollectorDocumentTransitionV1 {
          document_ordinal: 3,
          before: None,
          after: Some(IndexCollectorDocumentV1 { namespace_root: &root, record_revision_hash: &revision, file_record: &record }),
        },
      },
      &UnexpectedParser,
      None,
      103,
      &|| false,
      &mut SpillStore,
    )
    .unwrap_err();
  assert!(matches!(error, IndexProducerExecutionErrorV1::TaskMismatch(_)));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
}

#[test]
fn cancellation_and_mutation_pressure_release_the_lease_without_consuming_the_task() {
  let definitions = definitions();
  let before_root = hash(b"before");
  let after_root = hash(b"after");
  let semantic = hash(b"semantic");
  let revision = hash(b"revision");
  let record = file("/doc.json");
  let task_memory = memory(8 * 1_024 * 1_024);
  let mut producer = producer(task_memory.clone());
  admit_mutation_task(&mut producer, &before_root, &after_root, &semantic);
  let executor = IndexProducerExecutorV1::new(collector(task_memory));
  let mut mutations = mutations(memory(8 * 1_024 * 1_024), 1);

  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let cancelled = executor.execute_transition(
    &mut producer,
    &mut mutations,
    &lease,
    IndexProducerExecutionInputV1 {
      semantic_state_root: &semantic,
      scope_bundles: vec![scope_bundle(&definitions)],
      transition: IndexCollectorDocumentTransitionV1 {
        document_ordinal: 3,
        before: None,
        after: Some(IndexCollectorDocumentV1 { namespace_root: &after_root, record_revision_hash: &revision, file_record: &record }),
      },
    },
    &UnexpectedParser,
    None,
    101,
    &|| true,
    &mut SpillStore,
  );
  assert!(matches!(cancelled, Err(IndexProducerExecutionErrorV1::Cancelled)));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);

  let lease = producer.lease_next(101, false).unwrap().unwrap();
  let pressured = executor.execute_transition(
    &mut producer,
    &mut mutations,
    &lease,
    IndexProducerExecutionInputV1 {
      semantic_state_root: &semantic,
      scope_bundles: vec![scope_bundle(&definitions)],
      transition: IndexCollectorDocumentTransitionV1 {
        document_ordinal: 3,
        before: None,
        after: Some(IndexCollectorDocumentV1 { namespace_root: &after_root, record_revision_hash: &revision, file_record: &record }),
      },
    },
    &UnexpectedParser,
    None,
    102,
    &|| false,
    &mut SpillStore,
  );
  assert!(matches!(pressured, Err(IndexProducerExecutionErrorV1::Completion(_))));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(mutations.snapshot().active_records, 0);
}

#[test]
fn collector_memory_refusal_releases_the_lease_and_every_task_reservation() {
  let definitions = definitions();
  let before_root = hash(b"before");
  let after_root = hash(b"after");
  let semantic = hash(b"semantic");
  let revision = hash(b"revision");
  let record = file("/doc.json");
  let producer_memory = memory(8 * 1_024 * 1_024);
  let collector_memory = memory(256);
  let mut producer = producer(producer_memory.clone());
  admit_mutation_task(&mut producer, &before_root, &after_root, &semantic);
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let executor = IndexProducerExecutorV1::new(collector(collector_memory.clone()));
  let mut mutations = mutations(memory(8 * 1_024 * 1_024), 2 * 1_024 * 1_024);

  let error = executor
    .execute_transition(
      &mut producer,
      &mut mutations,
      &lease,
      IndexProducerExecutionInputV1 {
        semantic_state_root: &semantic,
        scope_bundles: vec![scope_bundle(&definitions)],
        transition: IndexCollectorDocumentTransitionV1 {
          document_ordinal: 3,
          before: None,
          after: Some(IndexCollectorDocumentV1 { namespace_root: &after_root, record_revision_hash: &revision, file_record: &record }),
        },
      },
      &UnexpectedParser,
      None,
      101,
      &|| false,
      &mut SpillStore,
    )
    .unwrap_err();

  assert!(matches!(error, IndexProducerExecutionErrorV1::Collector(_)));
  assert_eq!(producer.snapshot().pending_tasks, 1);
  assert_eq!(producer.snapshot().leased_tasks, 0);
  assert_eq!(collector_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
  assert!(producer_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes > 0);
}

#[test]
fn one_lease_collects_every_applicable_scope_before_consuming_the_task() {
  let root_scope = definitions_from_scope(direct_scope("/workspace/docs/guides/sub"));
  let glob_scope = definitions_for_scope("ascp-blake3-256-normalized-glob-valid.bin");
  let before_root = hash(b"before");
  let after_root = hash(b"after");
  let semantic = hash(b"semantic");
  let revision = hash(b"revision");
  let record = file("/workspace/docs/guides/sub/topic.md");
  let task_memory = memory(8 * 1_024 * 1_024);
  let mut producer = producer(task_memory.clone());
  let mut mutations = mutations(memory(8 * 1_024 * 1_024), 2 * 1_024 * 1_024);
  admit_mutation_task(&mut producer, &before_root, &after_root, &semantic);
  let lease = producer.lease_next(100, false).unwrap().unwrap();
  let executor = IndexProducerExecutorV1::new(collector(task_memory.clone()));

  let completion = executor
    .execute_transition(
      &mut producer,
      &mut mutations,
      &lease,
      IndexProducerExecutionInputV1 {
        semantic_state_root: &semantic,
        scope_bundles: vec![scope_bundle(&root_scope), scope_bundle(&glob_scope)],
        transition: IndexCollectorDocumentTransitionV1 {
          document_ordinal: 3,
          before: None,
          after: Some(IndexCollectorDocumentV1 { namespace_root: &after_root, record_revision_hash: &revision, file_record: &record }),
        },
      },
      &UnexpectedParser,
      None,
      101,
      &|| false,
      &mut SpillStore,
    )
    .unwrap();

  assert!(matches!(completion, aeordb::engine::v4::index_producer_coordinator::IndexProducerCompletionV1::Completed { .. }));
  assert_eq!(producer.snapshot().pending_tasks, 0);
  assert_eq!(mutations.snapshot().active_records, 8);
  assert_eq!(task_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}
