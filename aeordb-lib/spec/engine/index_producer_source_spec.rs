use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use aeordb::engine::HashAlgorithm;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::index_producer_admission::{admit_mutation_journal_tasks, derive_mutation_operation_id};
use aeordb::engine::v4::index_producer_coordinator::{
  IndexProducerCoordinatorOptionsV1, IndexProducerCoordinatorV1, IndexProducerSpillErrorV1, IndexProducerSpillReasonV1,
  IndexProducerSpillReceiptV1, IndexProducerSpillStoreV1, IndexProducerTaskViewV1,
};
use aeordb::engine::v4::index_producer_source::{
  IndexFileRevisionReadErrorV1, IndexFileRevisionSourceV1, IndexProducerSourceErrorV1, IndexSemanticScopeLimitsV1,
  IndexSemanticScopeReadErrorClassV1, IndexSemanticScopeReadErrorV1, IndexSemanticScopeReadRequestV1, IndexSemanticScopeReadV1,
  IndexSemanticScopeResolutionV1, IndexSemanticScopeSourceV1, LoadedIndexFileRevisionV1, OwnedIndexFieldDefinitionV1,
  OwnedIndexScopeDefinitionV1, OwnedIndexValueStoreDefinitionV1, ResolvedIndexDocumentTransitionV1, ResolvedIndexDocumentV1,
  ResolvedIndexScopeWorkV1, resolve_leased_mutation_record, resolve_mutation_document_transition,
  resolve_semantic_scope_work as resolve_semantic_scope_work_with_sequence,
};
use aeordb::engine::v4::index_task::{
  JournalOwnerKindV1, MutationJournalWriteV1, MutationKindV1, MutationRecordWriteV1, MutationSideWriteV1, decode_mutation_journal,
  encode_mutation_journal,
};

const ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn hash(label: &[u8]) -> Vec<u8> {
  aeordb::engine::v4::hash::digest_parts(ALGORITHM, &[b"producer-source:", label])
}

struct RecordingSemanticSource {
  observed: Mutex<Option<([u8; 16], u64, Vec<u8>, String, IndexSemanticScopeLimitsV1)>>,
}

impl IndexSemanticScopeSourceV1 for RecordingSemanticSource {
  fn resolve_scopes(
    &self,
    request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    assert!(!(request.is_cancelled)());
    let path = request
      .transition
      .after
      .as_ref()
      .or(request.transition.before.as_ref())
      .expect("validated semantic transition")
      .file_record
      .path
      .clone();
    *self.observed.lock().unwrap() =
      Some((request.operation_id, request.source_publication_sequence, request.semantic_state_root.to_vec(), path, request.limits));
    let resolution = IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root: request.semantic_state_root.to_vec() };
    let memory = MemoryCoordinator::new(MemoryPolicy::new(6_000, 8_000, 1, 1_000).unwrap());
    let reservation = memory
      .reserve(MemoryOwner::Task, request.semantic_state_root.len() as u64, AdmissionClass::Workload)
      .map_err(|error| IndexSemanticScopeReadErrorV1::retryable("test_memory", error.to_string()))?;
    IndexSemanticScopeReadV1::new(resolution, reservation)
  }
}

fn fixture(folder: &str, name: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{folder}/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn file(path: &str, content: u8) -> FileRecord {
  FileRecord {
    path: path.to_string(),
    content_type: Some("application/json".to_string()),
    total_size: 16,
    created_at: 100,
    updated_at: 101,
    metadata: Vec::new(),
    content_hash: vec![content; 32],
    chunk_hashes: vec![vec![content.wrapping_add(1); 32]],
  }
}

fn encoded_journal(path: &str) -> Vec<u8> {
  let before_root = hash(b"before-root");
  let after_root = hash(b"after-root");
  let semantic = hash(b"semantic");
  let mutation = hash(b"mutation");
  let before_revision = hash(b"before-revision");
  let after_revision = hash(b"after-revision");
  encode_mutation_journal(&MutationJournalWriteV1 {
    hash_algorithm: ALGORITHM,
    owner_id: [0x31; 16],
    owner_kind: JournalOwnerKindV1::Task,
    generation: 1,
    segment_ordinal: 0,
    chain_reset: true,
    previous_segment: &[0; 32],
    semantic_state_root: &semantic,
    runtime_boot_id: [0x32; 16],
    records: &[MutationRecordWriteV1 {
      kind: MutationKindV1::Update,
      sequence: 7,
      mutation_id: &mutation,
      batch_ordinal: 0,
      batch_count: 1,
      root_before: &before_root,
      root_after: &after_root,
      before: Some(MutationSideWriteV1 { path, revision: &before_revision }),
      after: Some(MutationSideWriteV1 { path, revision: &after_revision }),
      committed_at_ms: 100,
    }],
  })
  .unwrap()
  .value
}

fn producer() -> IndexProducerCoordinatorV1 {
  let memory = MemoryCoordinator::new(MemoryPolicy::new(6 * 1_024 * 1_024, 8 * 1_024 * 1_024, 1, 1 * 1_024 * 1_024).unwrap());
  IndexProducerCoordinatorV1::new(
    ALGORITHM,
    memory,
    IndexProducerCoordinatorOptionsV1::new(8, 2 * 1_024 * 1_024, 3, 10, 1_000, 16, 256, 2 * 1_024 * 1_024).unwrap(),
  )
  .unwrap()
}

struct Spill;

impl IndexProducerSpillStoreV1 for Spill {
  fn spill(
    &mut self,
    task: IndexProducerTaskViewV1<'_>,
    _reason: IndexProducerSpillReasonV1,
  ) -> Result<IndexProducerSpillReceiptV1, IndexProducerSpillErrorV1> {
    IndexProducerSpillReceiptV1::new(task.operation_id(), hash(b"spill"))
  }
}

#[derive(Default)]
struct RevisionSource {
  records: BTreeMap<(Vec<u8>, String), LoadedIndexFileRevisionV1>,
  failure: Option<IndexFileRevisionReadErrorV1>,
}

#[derive(Clone)]
struct SemanticSource {
  result: Result<IndexSemanticScopeResolutionV1, IndexSemanticScopeReadErrorV1>,
}

impl IndexSemanticScopeSourceV1 for SemanticSource {
  fn resolve_scopes(
    &self,
    _request: IndexSemanticScopeReadRequestV1<'_>,
  ) -> Result<IndexSemanticScopeReadV1, IndexSemanticScopeReadErrorV1> {
    let resolution = self.result.clone()?;
    let memory = MemoryCoordinator::new(MemoryPolicy::new(6 * 1_024 * 1_024, 8 * 1_024 * 1_024, 1, 1 * 1_024 * 1_024).unwrap());
    let reservation = memory
      .reserve(MemoryOwner::Task, 2 * 1_024 * 1_024, AdmissionClass::Workload)
      .map_err(|error| IndexSemanticScopeReadErrorV1::retryable("test_memory", error.to_string()))?;
    IndexSemanticScopeReadV1::new(resolution, reservation)
  }
}

fn scope_work(semantic_state_root: &[u8], ordinal: u64, scope_fixture: &str) -> ResolvedIndexScopeWorkV1 {
  let scope = fixture("scope-definition-v1", scope_fixture);
  let scope_id = aeordb::engine::v4::scope::decode_scope_definition(&scope, ALGORITHM).unwrap().scope_id;
  let mut value = fixture("value-store-definition-v1", "avst-blake3-256-metadata-hash-corrected-valid.bin");
  value[32..64].copy_from_slice(&scope_id);
  let value_id = aeordb::engine::v4::value_store::decode_value_store_definition(&value, ALGORITHM).unwrap().value_store_id;
  let mut field = fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  field[32..64].copy_from_slice(&value_id);
  let field_id = aeordb::engine::v4::field_definition::decode_field_index_definition(&field, ALGORITHM).unwrap().index_id;
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

fn resolved_transition() -> ResolvedIndexDocumentTransitionV1 {
  ResolvedIndexDocumentTransitionV1 {
    before: None,
    after: Some(ResolvedIndexDocumentV1 {
      namespace_root: hash(b"after-root"),
      revision_hash: hash(b"after-revision"),
      file_record: file("/workspace/docs/doc.json", 0x42),
    }),
  }
}

fn semantic_limits() -> IndexSemanticScopeLimitsV1 {
  IndexSemanticScopeLimitsV1::new(8, 16, 32, 2 * 1_024 * 1_024).unwrap()
}

fn resolve_semantic_scope_work(
  hash_algorithm: HashAlgorithm,
  operation_id: [u8; 16],
  semantic_state_root: &[u8],
  transition: &ResolvedIndexDocumentTransitionV1,
  source: &dyn IndexSemanticScopeSourceV1,
  limits: IndexSemanticScopeLimitsV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<IndexSemanticScopeReadV1, IndexProducerSourceErrorV1> {
  resolve_semantic_scope_work_with_sequence(hash_algorithm, operation_id, 7, semantic_state_root, transition, source, limits, is_cancelled)
}

#[test]
fn semantic_scope_source_receives_exact_operation_transition_limits_and_cancellation() {
  let operation_id = [0x5a; 16];
  let root = hash(b"semantic-request");
  let limits = semantic_limits();
  let source = RecordingSemanticSource { observed: Mutex::new(None) };
  let cancellation_calls = AtomicUsize::new(0);
  let is_cancelled = || {
    cancellation_calls.fetch_add(1, Ordering::SeqCst);
    false
  };

  let resolved =
    resolve_semantic_scope_work(ALGORITHM, operation_id, &root, &resolved_transition(), &source, limits, &is_cancelled).unwrap();

  assert!(matches!(resolved.resolution(), IndexSemanticScopeResolutionV1::ContentOnly { .. }));
  assert_eq!(source.observed.lock().unwrap().as_ref(), Some(&(operation_id, 7, root, "/workspace/docs/doc.json".to_string(), limits)));
  assert!(cancellation_calls.load(Ordering::SeqCst) >= 3);
}

#[test]
fn semantic_scope_reads_require_task_owned_memory_for_their_complete_lifetime() {
  let root = hash(b"semantic");
  let coordinator = MemoryCoordinator::new(MemoryPolicy::new(6_000, 8_000, 1, 1_000).unwrap());
  let wrong_owner = coordinator.reserve(MemoryOwner::Query, root.len() as u64, AdmissionClass::Workload).unwrap();
  assert!(
    IndexSemanticScopeReadV1::new(IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root: root.clone() }, wrong_owner).is_err()
  );
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let under_reserved = coordinator.reserve(MemoryOwner::Task, (root.len() - 1) as u64, AdmissionClass::Workload).unwrap();
  assert!(IndexSemanticScopeReadV1::new(IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root: root.clone() }, under_reserved)
    .is_err());
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);

  let reservation = coordinator.reserve(MemoryOwner::Task, root.len() as u64, AdmissionClass::Workload).unwrap();
  let read = IndexSemanticScopeReadV1::new(IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root: root }, reservation).unwrap();
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 32);
  drop(read);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
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

fn leased_fixture(
  path: &str,
) -> (Vec<u8>, IndexProducerCoordinatorV1, aeordb::engine::v4::index_producer_coordinator::IndexProducerLeaseV1) {
  let encoded = encoded_journal(path);
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let mut producer = producer();
  admit_mutation_journal_tasks(ALGORITHM, &mut producer, &journal, 200, &|| false, &mut Spill).unwrap();
  let lease = producer.lease_next(200, false).unwrap().unwrap();
  (encoded, producer, lease)
}

#[test]
fn leased_task_resolves_exactly_one_matching_journal_record() {
  let (encoded, producer, lease) = leased_fixture("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let record = resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| false).unwrap();
  let expected = derive_mutation_operation_id(ALGORITHM, record.mutation_id, record.batch_ordinal).unwrap();
  assert_eq!(lease.operation_id(), expected);
  assert_eq!(record.before_path, Some("/doc.json"));
  assert_eq!(record.after_path, Some("/doc.json"));
}

#[test]
fn exact_before_and_after_revisions_become_a_borrowable_collector_transition() {
  let (encoded, producer, lease) = leased_fixture("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let record = resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| false).unwrap();
  let mut source = RevisionSource::default();
  source.records.insert(
    (record.root_before.to_vec(), "/doc.json".to_string()),
    LoadedIndexFileRevisionV1 { revision_hash: record.before_revision.unwrap().to_vec(), file_record: file("/doc.json", 0x41) },
  );
  source.records.insert(
    (record.root_after.to_vec(), "/doc.json".to_string()),
    LoadedIndexFileRevisionV1 { revision_hash: record.after_revision.unwrap().to_vec(), file_record: file("/doc.json", 0x42) },
  );

  let resolved = resolve_mutation_document_transition(ALGORITHM, &record, &source, &|| false).unwrap();
  let borrowed = resolved.as_collector_transition();
  assert_eq!(borrowed.before.unwrap().file_record.content_hash, vec![0x41; 32]);
  assert_eq!(borrowed.after.unwrap().file_record.content_hash, vec![0x42; 32]);
  assert_eq!(borrowed.before.unwrap().namespace_root, record.root_before);
  assert_eq!(borrowed.after.unwrap().record_revision_hash, record.after_revision.unwrap());
}

#[test]
fn wrong_journal_or_missing_revision_never_falls_through_to_an_empty_transition() {
  let (encoded, producer, lease) = leased_fixture("/doc.json");
  let other = encoded_journal("/other.json");
  let other_journal = decode_mutation_journal(&other, ALGORITHM).unwrap();
  assert!(matches!(
    resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &other_journal, &|| false),
    Err(IndexProducerSourceErrorV1::TaskMismatch(_))
  ));

  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let record = resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| false).unwrap();
  assert!(matches!(
    resolve_mutation_document_transition(ALGORITHM, &record, &RevisionSource::default(), &|| false),
    Err(IndexProducerSourceErrorV1::MissingRevision { .. })
  ));
}

#[test]
fn revision_identity_or_path_mismatch_is_corruption_not_success() {
  let (encoded, producer, lease) = leased_fixture("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  let record = resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| false).unwrap();
  let mut source = RevisionSource::default();
  source.records.insert(
    (record.root_before.to_vec(), "/doc.json".to_string()),
    LoadedIndexFileRevisionV1 { revision_hash: hash(b"wrong"), file_record: file("/other.json", 0x41) },
  );
  assert!(matches!(
    resolve_mutation_document_transition(ALGORITHM, &record, &source, &|| false),
    Err(IndexProducerSourceErrorV1::RevisionMismatch { .. }) | Err(IndexProducerSourceErrorV1::PathMismatch { .. })
  ));
}

#[test]
fn cancellation_and_reader_failures_remain_typed() {
  let (encoded, producer, lease) = leased_fixture("/doc.json");
  let journal = decode_mutation_journal(&encoded, ALGORITHM).unwrap();
  assert_eq!(
    resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| true).unwrap_err(),
    IndexProducerSourceErrorV1::Cancelled
  );
  let record = resolve_leased_mutation_record(ALGORITHM, &producer, &lease, &journal, &|| false).unwrap();
  let source = RevisionSource {
    records: BTreeMap::new(),
    failure: Some(IndexFileRevisionReadErrorV1::retryable("storage_busy", "injected source outage")),
  };
  assert!(matches!(
    resolve_mutation_document_transition(ALGORITHM, &record, &source, &|| false),
    Err(IndexProducerSourceErrorV1::RevisionRead(error)) if error.code() == "storage_busy"
  ));
  let source = RevisionSource {
    records: BTreeMap::new(),
    failure: Some(IndexFileRevisionReadErrorV1::cancelled("shutdown", "injected worker shutdown")),
  };
  assert_eq!(
    resolve_mutation_document_transition(ALGORITHM, &record, &source, &|| false).unwrap_err(),
    IndexProducerSourceErrorV1::Cancelled
  );
}

#[test]
fn complete_semantic_resolution_preserves_all_scope_local_ordinals() {
  let root = hash(b"semantic");
  let source = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: root.clone(),
      scope_work: vec![
        scope_work(&root, 3, "ascp-blake3-256-root-direct-valid.bin"),
        scope_work(&root, 9, "ascp-blake3-256-normalized-glob-valid.bin"),
      ],
    }),
  };

  let resolved =
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &source, semantic_limits(), &|| false).unwrap();
  let (resolved, _reservation) = resolved.into_parts();
  let IndexSemanticScopeResolutionV1::Complete { semantic_state_root, scope_work } = resolved else {
    panic!("complete semantic source became content-only");
  };
  assert_eq!(semantic_state_root, root);
  assert_eq!(scope_work.iter().map(|scope| scope.document_ordinal).collect::<Vec<_>>(), vec![3, 9]);
  assert_ne!(scope_work[0].scope.scope_id, scope_work[1].scope.scope_id);
  for work in &scope_work {
    assert_eq!(work.semantic_state_root, semantic_state_root);
    let borrowed = work.as_collector_scope_work();
    assert_eq!(borrowed.document_ordinal, work.document_ordinal);
    assert_eq!(borrowed.scope_bundle.expected_scope_id, work.scope.scope_id);
    assert_eq!(borrowed.scope_bundle.value_stores.len(), 1);
    assert_eq!(borrowed.scope_bundle.value_stores[0].field_indexes.len(), 1);
  }
}

#[test]
fn content_only_semantics_remain_explicit_instead_of_becoming_empty_complete_coverage() {
  let root = hash(b"semantic");
  let source = SemanticSource { result: Ok(IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root: root.clone() }) };

  assert_eq!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &source, semantic_limits(), &|| false)
      .unwrap()
      .resolution(),
    &IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root: root }
  );
}

#[test]
fn semantic_resolution_rejects_wrong_roots_duplicate_owners_and_zero_ordinals() {
  let root = hash(b"semantic");
  let wrong_root = hash(b"wrong-semantic");
  let wrong_root_source = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: wrong_root.clone(),
      scope_work: vec![scope_work(&wrong_root, 3, "ascp-blake3-256-root-direct-valid.bin")],
    }),
  };
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &wrong_root_source, semantic_limits(), &|| false),
    Err(IndexProducerSourceErrorV1::SemanticRootMismatch { .. })
  ));

  let mut wrong_nested_root = scope_work(&root, 3, "ascp-blake3-256-root-direct-valid.bin");
  wrong_nested_root.semantic_state_root = wrong_root;
  let wrong_nested_root_source = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete { semantic_state_root: root.clone(), scope_work: vec![wrong_nested_root] }),
  };
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &wrong_nested_root_source, semantic_limits(), &|| false),
    Err(IndexProducerSourceErrorV1::SemanticRootMismatch { .. })
  ));

  let duplicate = scope_work(&root, 3, "ascp-blake3-256-root-direct-valid.bin");
  let duplicate_source = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: root.clone(),
      scope_work: vec![duplicate.clone(), duplicate],
    }),
  };
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &duplicate_source, semantic_limits(), &|| false),
    Err(IndexProducerSourceErrorV1::DuplicateSemanticOwner { .. })
  ));

  let zero_source = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: root.clone(),
      scope_work: vec![scope_work(&root, 0, "ascp-blake3-256-root-direct-valid.bin")],
    }),
  };
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &zero_source, semantic_limits(), &|| false),
    Err(IndexProducerSourceErrorV1::InvalidDocumentOrdinal { .. })
  ));
}

#[test]
fn semantic_resolution_enforces_aggregate_limits_before_collection() {
  let root = hash(b"semantic");
  let two_scopes = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: root.clone(),
      scope_work: vec![
        scope_work(&root, 3, "ascp-blake3-256-root-direct-valid.bin"),
        scope_work(&root, 9, "ascp-blake3-256-normalized-glob-valid.bin"),
      ],
    }),
  };
  let one_scope = IndexSemanticScopeLimitsV1::new(1, 16, 32, 2 * 1_024 * 1_024).unwrap();
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &two_scopes, one_scope, &|| false),
    Err(IndexProducerSourceErrorV1::SemanticLimitExceeded { resource: "scopes", .. })
  ));

  let one = scope_work(&root, 3, "ascp-blake3-256-root-direct-valid.bin");
  let encoded_bytes = one.scope.encoded_definition.len()
    + one.scope.value_stores[0].encoded_definition.len()
    + one.scope.value_stores[0].field_indexes[0].encoded_definition.len();
  let exact = IndexSemanticScopeLimitsV1::new(1, 1, 1, encoded_bytes as u64).unwrap();
  let too_small = IndexSemanticScopeLimitsV1::new(1, 1, 1, encoded_bytes as u64 - 1).unwrap();
  let source = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete { semantic_state_root: root.clone(), scope_work: vec![one.clone()] }),
  };
  resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &source, exact, &|| false).unwrap();
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &source, too_small, &|| false),
    Err(IndexProducerSourceErrorV1::SemanticLimitExceeded { resource: "definition bytes", .. })
  ));

  let mut excess_values = scope_work(&root, 3, "ascp-blake3-256-root-direct-valid.bin");
  excess_values.scope.value_stores.push(excess_values.scope.value_stores[0].clone());
  let source = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete { semantic_state_root: root.clone(), scope_work: vec![excess_values] }),
  };
  let one_value_store = IndexSemanticScopeLimitsV1::new(1, 1, 32, 2 * 1_024 * 1_024).unwrap();
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &source, one_value_store, &|| false),
    Err(IndexProducerSourceErrorV1::SemanticLimitExceeded { resource: "value stores", .. })
  ));

  let mut excess_fields = scope_work(&root, 3, "ascp-blake3-256-root-direct-valid.bin");
  let duplicate_field = excess_fields.scope.value_stores[0].field_indexes[0].clone();
  excess_fields.scope.value_stores[0].field_indexes.push(duplicate_field);
  let source = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete { semantic_state_root: root.clone(), scope_work: vec![excess_fields] }),
  };
  let one_field = IndexSemanticScopeLimitsV1::new(1, 1, 1, 2 * 1_024 * 1_024).unwrap();
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &source, one_field, &|| false),
    Err(IndexProducerSourceErrorV1::SemanticLimitExceeded { resource: "field indexes", .. })
  ));
}

#[test]
fn malformed_semantic_definitions_and_source_failures_remain_typed() {
  let root = hash(b"semantic");
  let mut malformed = scope_work(&root, 3, "ascp-blake3-256-root-direct-valid.bin");
  malformed.scope.encoded_definition[0] ^= 0xff;
  let source = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete { semantic_state_root: root.clone(), scope_work: vec![malformed] }),
  };
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &source, semantic_limits(), &|| false),
    Err(IndexProducerSourceErrorV1::Format(_))
  ));

  let retryable =
    SemanticSource { result: Err(IndexSemanticScopeReadErrorV1::retryable("semantic_busy", "injected semantic catalog outage")) };
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &retryable, semantic_limits(), &|| false),
    Err(IndexProducerSourceErrorV1::SemanticRead(error)) if error.code() == "semantic_busy"
  ));
  let cancelled = SemanticSource { result: Err(IndexSemanticScopeReadErrorV1::cancelled("shutdown", "worker stopping")) };
  assert_eq!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &cancelled, semantic_limits(), &|| false).unwrap_err(),
    IndexProducerSourceErrorV1::Cancelled
  );
  assert_eq!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &retryable, semantic_limits(), &|| true).unwrap_err(),
    IndexProducerSourceErrorV1::Cancelled
  );

  let corrupt = SemanticSource { result: Err(IndexSemanticScopeReadErrorV1::corrupt("catalog_corrupt", "bad catalog edge")) };
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &corrupt, semantic_limits(), &|| false),
    Err(IndexProducerSourceErrorV1::SemanticRead(error))
      if error.class() == IndexSemanticScopeReadErrorClassV1::Corrupt && error.code() == "catalog_corrupt"
  ));
}

#[test]
fn semantic_resolution_rejects_invalid_inputs_and_cancels_during_validation() {
  assert!(matches!(IndexSemanticScopeLimitsV1::new(0, 1, 1, 1), Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(_))));

  let root = hash(b"semantic");
  let source = SemanticSource { result: Ok(IndexSemanticScopeResolutionV1::ContentOnly { semantic_state_root: root.clone() }) };
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &[0; 32], &resolved_transition(), &source, semantic_limits(), &|| false),
    Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(_))
  ));
  assert!(matches!(
    resolve_semantic_scope_work_with_sequence(
      ALGORITHM,
      [1; 16],
      0,
      &root,
      &resolved_transition(),
      &source,
      semantic_limits(),
      &|| false
    ),
    Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(message)) if message.contains("publication sequence")
  ));
  assert!(matches!(
    resolve_semantic_scope_work(
      ALGORITHM,
      [1; 16],
      &root,
      &ResolvedIndexDocumentTransitionV1 { before: None, after: None },
      &source,
      semantic_limits(),
      &|| false
    ),
    Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(_))
  ));

  let source = SemanticSource {
    result: Ok(IndexSemanticScopeResolutionV1::Complete {
      semantic_state_root: root.clone(),
      scope_work: vec![scope_work(&root, 3, "ascp-blake3-256-root-direct-valid.bin")],
    }),
  };
  let calls = Cell::new(0u8);
  let cancellation = || {
    let call = calls.get().saturating_add(1);
    calls.set(call);
    call >= 3
  };
  assert_eq!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &source, semantic_limits(), &cancellation).unwrap_err(),
    IndexProducerSourceErrorV1::Cancelled
  );
}

#[test]
fn semantic_definition_closure_mismatch_fails_before_collection() {
  let root = hash(b"semantic");
  let mut work = scope_work(&root, 3, "ascp-blake3-256-root-direct-valid.bin");
  work.scope.value_stores[0].value_store_id = hash(b"wrong-value-owner");
  let source =
    SemanticSource { result: Ok(IndexSemanticScopeResolutionV1::Complete { semantic_state_root: root.clone(), scope_work: vec![work] }) };
  assert!(matches!(
    resolve_semantic_scope_work(ALGORITHM, [1; 16], &root, &resolved_transition(), &source, semantic_limits(), &|| false),
    Err(IndexProducerSourceErrorV1::InvalidSemanticResolution(message)) if message.contains("ValueStoreDefinition closure mismatch")
  ));
}
