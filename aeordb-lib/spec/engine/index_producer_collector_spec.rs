use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::index_page::{OrderedIndexRoleV1, decode_posting_record};
use aeordb::engine::v4::index_producer_collector::{
  CollectedIndexProducerReportV1, IndexCollectorDocumentTransitionV1, IndexCollectorDocumentV1, IndexCollectorFieldDefinitionV1,
  IndexCollectorScopeDefinitionV1, IndexCollectorValueStoreDefinitionV1, IndexParserDeterministicFailureV1, IndexParserExecutionErrorV1,
  IndexParserExecutionRequestV1, IndexParserExecutorV1, IndexParserOutcomeV1, IndexProducerCollectorErrorV1,
  IndexProducerCollectorOptionsV1, IndexProducerCollectorV1,
};
use aeordb::engine::v4::index_producer_coordinator::{IndexProducerOwnerDispositionV1, IndexProducerOwnerOutcomeV1};
use aeordb::engine::v4::index_record::{
  decode_canonical_value_record, decode_document_state_record, decode_scope_document_record, DocumentStateOwnerV1,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;

const HASH_ALGORITHM: HashAlgorithm = HashAlgorithm::Blake3_256;

fn memory(hard_limit_bytes: u64) -> MemoryCoordinator {
  let emergency = (hard_limit_bytes / 4).max(1);
  MemoryCoordinator::new(MemoryPolicy::new(hard_limit_bytes - emergency - 1, hard_limit_bytes, 1, emergency).unwrap())
}

fn options() -> IndexProducerCollectorOptionsV1 {
  IndexProducerCollectorOptionsV1::new(16, 16, 2 * 1_024 * 1_024, 256, 2 * 1_024 * 1_024, 50).unwrap()
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

fn definitions(value_fixture: &str, field_fixture: &str) -> Definitions {
  definitions_for(HASH_ALGORITHM, "ascp-blake3-256-root-direct-valid.bin", value_fixture, field_fixture)
}

fn definitions_for(algorithm: HashAlgorithm, scope_fixture: &str, value_fixture: &str, field_fixture: &str) -> Definitions {
  let scope = fixture("scope-definition-v1", scope_fixture);
  let scope_id = decode_scope_definition(&scope, algorithm).unwrap().scope_id;

  let mut value = fixture("value-store-definition-v1", value_fixture);
  value[32..32 + algorithm.hash_length()].copy_from_slice(&scope_id);
  let value_id = decode_value_store_definition(&value, algorithm).unwrap().value_store_id;

  let mut field = fixture("field-index-definition-v1", field_fixture);
  field[32..32 + algorithm.hash_length()].copy_from_slice(&value_id);
  let field_id = decode_field_index_definition(&field, algorithm).unwrap().index_id;
  Definitions { scope, scope_id, value, value_id, field, field_id }
}

fn scope_bundle<'a>(definitions: &'a Definitions) -> IndexCollectorScopeDefinitionV1<'a> {
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
  hash_with(HASH_ALGORITHM, label)
}

fn hash_with(algorithm: HashAlgorithm, label: &[u8]) -> Vec<u8> {
  aeordb::engine::v4::hash::digest_parts(algorithm, &[b"collector:", label])
}

fn file(path: &str, content_hash: u8, size: u64) -> FileRecord {
  FileRecord {
    path: path.to_string(),
    content_type: Some("application/json".to_string()),
    total_size: size,
    created_at: 1_700_000_000_000,
    updated_at: 1_700_000_000_001,
    metadata: Vec::new(),
    content_hash: vec![content_hash; 32],
    chunk_hashes: vec![vec![content_hash.wrapping_add(1); 32]],
  }
}

fn document<'a>(root: &'a [u8], revision: &'a [u8], record: &'a FileRecord) -> IndexCollectorDocumentV1<'a> {
  IndexCollectorDocumentV1 { namespace_root: root, record_revision_hash: revision, file_record: record }
}

fn outcome<'a>(report: &'a CollectedIndexProducerReportV1, owner: &[u8]) -> &'a IndexProducerOwnerOutcomeV1 {
  report.report().outcomes.iter().find(|outcome| outcome.owner_id == owner).expect("owner outcome")
}

#[derive(Clone)]
enum ParserBehavior {
  ParsedByRevision { first_revision: Vec<u8>, first: CanonicalConfigValueV1, second: CanonicalConfigValueV1 },
  Deterministic(IndexParserDeterministicFailureV1),
  DependencyUnavailable,
  HostFailure,
  Cancelled,
}

struct Parser {
  calls: AtomicUsize,
  observed: Mutex<Vec<(Vec<u8>, Vec<u8>, String)>>,
  behavior: ParserBehavior,
}

impl Parser {
  fn new(behavior: ParserBehavior) -> Self {
    Self { calls: AtomicUsize::new(0), observed: Mutex::new(Vec::new()), behavior }
  }
}

impl IndexParserExecutorV1 for Parser {
  fn parse(&self, request: IndexParserExecutionRequestV1<'_>) -> Result<IndexParserOutcomeV1, IndexParserExecutionErrorV1> {
    self.calls.fetch_add(1, Ordering::SeqCst);
    self.observed.lock().unwrap().push((
      request.namespace_root().to_vec(),
      request.record_revision_hash().to_vec(),
      request.path().to_string(),
    ));
    match &self.behavior {
      ParserBehavior::ParsedByRevision { first_revision, first, second } => {
        let parsed = if request.record_revision_hash() == first_revision { first } else { second };
        Ok(IndexParserOutcomeV1::Parsed(parsed.clone()))
      }
      ParserBehavior::Deterministic(failure) => Ok(IndexParserOutcomeV1::DeterministicUnindexable(failure.clone())),
      ParserBehavior::DependencyUnavailable => {
        Err(IndexParserExecutionErrorV1::dependency_unavailable("parser_dependency_unavailable", "injected exact dependency outage"))
      }
      ParserBehavior::HostFailure => Err(IndexParserExecutionErrorV1::host_failure("parser_host_failure", "injected parser host failure")),
      ParserBehavior::Cancelled => Err(IndexParserExecutionErrorV1::cancelled("parser_cancelled", "injected parser cancellation")),
    }
  }
}

fn json_document(value: &str) -> CanonicalConfigValueV1 {
  CanonicalConfigValueV1::Map(BTreeMap::from([(
    "messages".to_string(),
    CanonicalConfigValueV1::Array(vec![CanonicalConfigValueV1::Map(BTreeMap::from([(
      "user".to_string(),
      CanonicalConfigValueV1::String(value.to_string()),
    )]))]),
  )]))
}

#[test]
fn metadata_create_emits_exact_scope_value_and_posting_records_without_parser_work() {
  let definitions = definitions("avst-blake3-256-metadata-hash-corrected-valid.bin", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  let root = hash(b"root");
  let revision = hash(b"revision");
  let record = file("/doc.json", 0x44, 32);
  let parser = Parser::new(ParserBehavior::DependencyUnavailable);
  let memory = memory(32 * 1_024 * 1_024);
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, memory.clone(), options()).unwrap();

  let report = collector
    .collect(
      scope_bundle(&definitions),
      IndexCollectorDocumentTransitionV1 { document_ordinal: 7, before: None, after: Some(document(&root, &revision, &record)) },
      &parser,
      None,
      &|| false,
    )
    .unwrap();

  assert_eq!(parser.calls.load(Ordering::SeqCst), 0);
  let scope = outcome(&report, &definitions.scope_id);
  assert!(matches!(scope.disposition, IndexProducerOwnerDispositionV1::Ready));
  assert_eq!(scope.mutations.len(), 2);
  let scope_record = scope.mutations.iter().find(|mutation| mutation.role == OrderedIndexRoleV1::ScopeOrdinal).unwrap();
  let scope_record = decode_scope_document_record(&scope_record.encoded_record, HASH_ALGORITHM).unwrap();
  assert_eq!(scope_record.document_ordinal, 7);
  assert_eq!(scope_record.record_revision_hash, revision);
  assert_eq!(scope_record.path, "/doc.json");

  let value = outcome(&report, &definitions.value_id);
  assert_eq!(value.mutations.len(), 1);
  let value_record = decode_canonical_value_record(&value.mutations[0].encoded_record, HASH_ALGORITHM).unwrap();
  assert_eq!(value_record.document_ordinal, 7);
  assert_eq!(value_record.record_revision_hash, revision);
  assert_eq!(
    value_record.canonical_value.unwrap(),
    encode_canonical_value(&CanonicalConfigValueV1::Bytes(vec![0x44; 32]), CanonicalValueBounds::SOURCE_VALUE).unwrap()
  );

  let field = outcome(&report, &definitions.field_id);
  assert_eq!(field.mutations.len(), 1);
  let posting = decode_posting_record(&field.mutations[0].encoded_record).unwrap();
  assert!(!posting.tombstone);
  assert_eq!(posting.document_ordinal, 7);
  assert!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes > 0);
  drop(report);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn exact_before_after_parsing_replaces_values_and_tombstones_only_removed_postings() {
  let definitions = definitions("avst-blake3-256-json-corrected-valid.bin", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  let before_root = hash(b"before-root");
  let after_root = hash(b"after-root");
  let before_revision = hash(b"before-revision");
  let after_revision = hash(b"after-revision");
  let before_record = file("/messages.json", 0x20, 64);
  let after_record = file("/messages.json", 0x21, 65);
  let parser = Parser::new(ParserBehavior::ParsedByRevision {
    first_revision: before_revision.clone(),
    first: json_document("first"),
    second: json_document("second"),
  });
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, memory(32 * 1_024 * 1_024), options()).unwrap();

  let report = collector
    .collect(
      scope_bundle(&definitions),
      IndexCollectorDocumentTransitionV1 {
        document_ordinal: 9,
        before: Some(document(&before_root, &before_revision, &before_record)),
        after: Some(document(&after_root, &after_revision, &after_record)),
      },
      &parser,
      None,
      &|| false,
    )
    .unwrap();

  assert_eq!(parser.calls.load(Ordering::SeqCst), 2);
  assert_eq!(
    *parser.observed.lock().unwrap(),
    vec![(before_root, before_revision, "/messages.json".to_string()), (after_root, after_revision.clone(), "/messages.json".to_string()),]
  );
  let values = outcome(&report, &definitions.value_id);
  assert_eq!(values.mutations.len(), 1, "the new live row replaces the same value order key");
  let value = decode_canonical_value_record(&values.mutations[0].encoded_record, HASH_ALGORITHM).unwrap();
  assert!(!value.tombstone);
  assert_eq!(value.record_revision_hash, after_revision);

  let postings = outcome(&report, &definitions.field_id);
  assert_eq!(postings.mutations.len(), 2);
  let decoded = postings.mutations.iter().map(|mutation| decode_posting_record(&mutation.encoded_record).unwrap()).collect::<Vec<_>>();
  assert_eq!(decoded.iter().filter(|record| record.tombstone).count(), 1);
  assert_eq!(decoded.iter().filter(|record| !record.tombstone).count(), 1);
}

#[test]
fn deterministic_parser_failure_is_value_state_while_operational_failure_is_retryable_without_state() {
  let definitions = definitions("avst-blake3-256-json-corrected-valid.bin", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  let root = hash(b"root");
  let revision = hash(b"revision");
  let record = file("/messages.json", 0x20, 64);
  let evidence = encode_canonical_value(
    &CanonicalConfigValueV1::Map(BTreeMap::from([("code".to_string(), CanonicalConfigValueV1::String("malformed_document".to_string()))])),
    CanonicalValueBounds::CONFIG,
  )
  .unwrap();
  let deterministic =
    Parser::new(ParserBehavior::Deterministic(IndexParserDeterministicFailureV1::malformed_document(evidence.clone(), 64)));
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, memory(32 * 1_024 * 1_024), options()).unwrap();
  let transition =
    IndexCollectorDocumentTransitionV1 { document_ordinal: 4, before: None, after: Some(document(&root, &revision, &record)) };
  let report = collector.collect(scope_bundle(&definitions), transition, &deterministic, None, &|| false).unwrap();
  let value = outcome(&report, &definitions.value_id);
  assert!(matches!(value.disposition, IndexProducerOwnerDispositionV1::FrozenUnindexable { stage: 1, reason: 1, .. }));
  assert_eq!(value.mutations.len(), 1);
  let state = decode_document_state_record(&value.mutations[0].encoded_record, DocumentStateOwnerV1::ValueStore, HASH_ALGORITHM).unwrap();
  assert_eq!(state.evidence, evidence);
  assert_eq!(state.observed_canonical_bytes, 0);
  assert_eq!(state.observed_work_units, 64);
  assert!(outcome(&report, &definitions.field_id).mutations.is_empty());

  let operational = Parser::new(ParserBehavior::DependencyUnavailable);
  let report = collector.collect(scope_bundle(&definitions), transition, &operational, None, &|| false).unwrap();
  for owner in [&definitions.value_id, &definitions.field_id] {
    let outcome = outcome(&report, owner);
    assert!(matches!(outcome.disposition, IndexProducerOwnerDispositionV1::Retryable { stable_reason: 4, retry_after_ms: 50, .. }));
    assert!(outcome.mutations.is_empty());
  }
}

#[test]
fn frozen_evidence_and_records_use_the_database_sha512_profile() {
  let algorithm = HashAlgorithm::Sha512;
  let definitions = definitions_for(
    algorithm,
    "ascp-sha512-root-direct-valid.bin",
    "avst-sha512-json-corrected-valid.bin",
    "afix-sha512-typed_exact_blake3_v1-valid.bin",
  );
  let root = hash_with(algorithm, b"root");
  let revision = hash_with(algorithm, b"revision");
  let record = file("/messages.json", 0x20, 64);
  let evidence = encode_canonical_value(
    &CanonicalConfigValueV1::Map(BTreeMap::from([("code".to_string(), CanonicalConfigValueV1::String("malformed_document".to_string()))])),
    CanonicalValueBounds::CONFIG,
  )
  .unwrap();
  let parser = Parser::new(ParserBehavior::Deterministic(IndexParserDeterministicFailureV1::malformed_document(evidence.clone(), 1)));
  let collector = IndexProducerCollectorV1::new(algorithm, memory(32 * 1_024 * 1_024), options()).unwrap();
  let report = collector
    .collect(
      scope_bundle(&definitions),
      IndexCollectorDocumentTransitionV1 { document_ordinal: 11, before: None, after: Some(document(&root, &revision, &record)) },
      &parser,
      None,
      &|| false,
    )
    .unwrap();

  let value = outcome(&report, &definitions.value_id);
  let IndexProducerOwnerDispositionV1::FrozenUnindexable { evidence_hash: Some(evidence_hash), .. } = &value.disposition else {
    panic!("expected frozen SHA-512 disposition");
  };
  assert_eq!(evidence_hash.len(), algorithm.hash_length());
  assert_eq!(evidence_hash, &aeordb::engine::v4::hash::digest_parts(algorithm, &[b"aeordb.index.document-state-evidence.v1\0", &evidence]));
  decode_document_state_record(&value.mutations[0].encoded_record, DocumentStateOwnerV1::ValueStore, algorithm).unwrap();
}

#[test]
fn field_limit_failure_is_independent_and_malformed_field_degrades_only_that_owner() {
  let mut definitions = definitions("avst-blake3-256-metadata-hash-corrected-valid.bin", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  let strategy_name_length = u16::from_le_bytes(definitions.field[104..106].try_into().unwrap()) as usize;
  let converter_start = 168 + strategy_name_length;
  definitions.field[converter_start + 64..converter_start + 72].copy_from_slice(&1u64.to_le_bytes());
  definitions.field_id = decode_field_index_definition(&definitions.field, HASH_ALGORITHM).unwrap().index_id;
  let root = hash(b"root");
  let revision = hash(b"revision");
  let record = file("/doc.json", 0x44, 32);
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, memory(32 * 1_024 * 1_024), options()).unwrap();
  let parser = Parser::new(ParserBehavior::DependencyUnavailable);
  let transition =
    IndexCollectorDocumentTransitionV1 { document_ordinal: 6, before: None, after: Some(document(&root, &revision, &record)) };
  let report = collector.collect(scope_bundle(&definitions), transition, &parser, None, &|| false).unwrap();

  assert!(matches!(outcome(&report, &definitions.value_id).disposition, IndexProducerOwnerDispositionV1::Ready));
  let field = outcome(&report, &definitions.field_id);
  assert!(matches!(field.disposition, IndexProducerOwnerDispositionV1::FrozenUnindexable { stage: 5, reason: 12, .. }));
  assert_eq!(field.mutations.len(), 1);
  decode_document_state_record(&field.mutations[0].encoded_record, DocumentStateOwnerV1::FieldIndex, HASH_ALGORITHM).unwrap();

  definitions.field[12] = 1;
  let report = collector.collect(scope_bundle(&definitions), transition, &parser, None, &|| false).unwrap();
  assert!(matches!(outcome(&report, &definitions.value_id).disposition, IndexProducerOwnerDispositionV1::Ready));
  assert!(matches!(
    outcome(&report, &definitions.field_id).disposition,
    IndexProducerOwnerDispositionV1::Degraded { stable_reason: 14, .. }
  ));
  assert!(outcome(&report, &definitions.field_id).mutations.is_empty());
}

#[test]
fn source_byte_limit_freezes_the_value_store_with_the_frozen_reason() {
  let mut definitions = definitions("avst-blake3-256-metadata-hash-corrected-valid.bin", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  definitions.value[112..120].copy_from_slice(&1u64.to_le_bytes());
  definitions.value_id = decode_value_store_definition(&definitions.value, HASH_ALGORITHM).unwrap().value_store_id;
  definitions.field[32..64].copy_from_slice(&definitions.value_id);
  definitions.field_id = decode_field_index_definition(&definitions.field, HASH_ALGORITHM).unwrap().index_id;
  let root = hash(b"root");
  let revision = hash(b"revision");
  let record = file("/doc.json", 0x44, 32);
  let parser = Parser::new(ParserBehavior::DependencyUnavailable);
  let memory = memory(32 * 1_024 * 1_024);
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, memory.clone(), options()).unwrap();
  let report = collector
    .collect(
      scope_bundle(&definitions),
      IndexCollectorDocumentTransitionV1 { document_ordinal: 12, before: None, after: Some(document(&root, &revision, &record)) },
      &parser,
      None,
      &|| false,
    )
    .unwrap();

  let value = outcome(&report, &definitions.value_id);
  assert!(matches!(value.disposition, IndexProducerOwnerDispositionV1::FrozenUnindexable { stage: 4, reason: 8, .. }));
  let state = decode_document_state_record(&value.mutations[0].encoded_record, DocumentStateOwnerV1::ValueStore, HASH_ALGORITHM).unwrap();
  assert_eq!((state.stage, state.reason), (4, 8));
  drop(report);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn parser_cancellation_and_host_failure_are_typed_and_release_memory() {
  let definitions = definitions("avst-blake3-256-json-corrected-valid.bin", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  let root = hash(b"root");
  let revision = hash(b"revision");
  let record = file("/messages.json", 0x20, 64);
  let transition =
    IndexCollectorDocumentTransitionV1 { document_ordinal: 13, before: None, after: Some(document(&root, &revision, &record)) };
  let memory = memory(32 * 1_024 * 1_024);
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, memory.clone(), options()).unwrap();

  let cancelled = Parser::new(ParserBehavior::Cancelled);
  assert!(matches!(
    collector.collect(scope_bundle(&definitions), transition, &cancelled, None, &|| false),
    Err(IndexProducerCollectorErrorV1::Cancelled)
  ));
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);

  let host_failure = Parser::new(ParserBehavior::HostFailure);
  let report = collector.collect(scope_bundle(&definitions), transition, &host_failure, None, &|| false).unwrap();
  for owner in [&definitions.value_id, &definitions.field_id] {
    assert!(matches!(
      outcome(&report, owner).disposition,
      IndexProducerOwnerDispositionV1::Retryable { stable_reason: 11, retry_after_ms: 50, .. }
    ));
  }
  drop(report);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn malformed_parser_evidence_fails_closed_without_retaining_memory() {
  let definitions = definitions("avst-blake3-256-json-corrected-valid.bin", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  let root = hash(b"root");
  let revision = hash(b"revision");
  let record = file("/messages.json", 0x20, 64);
  let parser = Parser::new(ParserBehavior::Deterministic(IndexParserDeterministicFailureV1::malformed_document(Vec::new(), 0)));
  let memory = memory(32 * 1_024 * 1_024);
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, memory.clone(), options()).unwrap();

  assert!(matches!(
    collector.collect(
      scope_bundle(&definitions),
      IndexCollectorDocumentTransitionV1 { document_ordinal: 14, before: None, after: Some(document(&root, &revision, &record)) },
      &parser,
      None,
      &|| false,
    ),
    Err(IndexProducerCollectorErrorV1::InvalidRequest(_))
  ));
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn invalid_transition_identity_and_duplicate_owners_fail_before_parser_work() {
  assert!(IndexProducerCollectorOptionsV1::new(0, 1, 1, 1, 1, 1).is_err());
  let definitions = definitions("avst-blake3-256-metadata-hash-corrected-valid.bin", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  let root = hash(b"root");
  let revision = hash(b"revision");
  let record = file("/doc.json", 0x44, 32);
  let parser = Parser::new(ParserBehavior::DependencyUnavailable);
  let memory = memory(32 * 1_024 * 1_024);
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, memory.clone(), options()).unwrap();

  for transition in [
    IndexCollectorDocumentTransitionV1 { document_ordinal: 0, before: None, after: Some(document(&root, &revision, &record)) },
    IndexCollectorDocumentTransitionV1 { document_ordinal: 1, before: None, after: None },
    IndexCollectorDocumentTransitionV1 {
      document_ordinal: 1,
      before: None,
      after: Some(document(&root[..root.len() - 1], &revision, &record)),
    },
  ] {
    assert!(matches!(
      collector.collect(scope_bundle(&definitions), transition, &parser, None, &|| false),
      Err(IndexProducerCollectorErrorV1::InvalidRequest(_))
    ));
  }

  let mut noncanonical = record.clone();
  noncanonical.path = "/nested/../doc.json".to_string();
  assert!(matches!(
    collector.collect(
      scope_bundle(&definitions),
      IndexCollectorDocumentTransitionV1 { document_ordinal: 1, before: None, after: Some(document(&root, &revision, &noncanonical)) },
      &parser,
      None,
      &|| false,
    ),
    Err(IndexProducerCollectorErrorV1::InvalidRequest(_))
  ));

  let mut duplicate = scope_bundle(&definitions);
  duplicate.value_stores[0].field_indexes[0].expected_index_id = &definitions.value_id;
  assert!(matches!(
    collector.collect(
      duplicate,
      IndexCollectorDocumentTransitionV1 { document_ordinal: 1, before: None, after: Some(document(&root, &revision, &record)) },
      &parser,
      None,
      &|| false,
    ),
    Err(IndexProducerCollectorErrorV1::InvalidRequest(_))
  ));
  assert_eq!(parser.calls.load(Ordering::SeqCst), 0);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn out_of_scope_document_is_an_empty_report_without_parser_work() {
  let definitions = definitions("avst-blake3-256-json-corrected-valid.bin", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  let root = hash(b"root");
  let revision = hash(b"revision");
  let record = file("/nested/messages.json", 0x20, 64);
  let parser = Parser::new(ParserBehavior::HostFailure);
  let memory = memory(32 * 1_024 * 1_024);
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, memory.clone(), options()).unwrap();
  let report = collector
    .collect(
      scope_bundle(&definitions),
      IndexCollectorDocumentTransitionV1 { document_ordinal: 15, before: None, after: Some(document(&root, &revision, &record)) },
      &parser,
      None,
      &|| false,
    )
    .unwrap();
  assert!(report.report().outcomes.is_empty());
  assert_eq!(parser.calls.load(Ordering::SeqCst), 0);
  drop(report);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}

#[test]
fn cancellation_and_report_memory_pressure_fail_without_retained_state() {
  let definitions = definitions("avst-blake3-256-metadata-hash-corrected-valid.bin", "afix-blake3-256-typed_exact_blake3_v1-valid.bin");
  let root = hash(b"root");
  let revision = hash(b"revision");
  let record = file("/doc.json", 0x44, 32);
  let transition =
    IndexCollectorDocumentTransitionV1 { document_ordinal: 1, before: None, after: Some(document(&root, &revision, &record)) };
  let parser = Parser::new(ParserBehavior::DependencyUnavailable);

  let roomy_memory = memory(32 * 1_024 * 1_024);
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, roomy_memory.clone(), options()).unwrap();
  assert!(matches!(
    collector.collect(scope_bundle(&definitions), transition, &parser, None, &|| true),
    Err(IndexProducerCollectorErrorV1::Cancelled)
  ));
  assert_eq!(roomy_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);

  let tiny_memory = memory(256);
  let collector = IndexProducerCollectorV1::new(HASH_ALGORITHM, tiny_memory.clone(), options()).unwrap();
  assert!(matches!(
    collector.collect(scope_bundle(&definitions), transition, &parser, None, &|| false),
    Err(IndexProducerCollectorErrorV1::ResourcePressure(_))
  ));
  assert_eq!(tiny_memory.snapshot().unwrap().owner(MemoryOwner::Task).unwrap().reserved_bytes, 0);
}
