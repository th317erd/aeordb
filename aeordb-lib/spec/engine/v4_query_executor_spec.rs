use std::cell::Cell;
use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, decode_canonical_value, encode_canonical_value};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::index_coverage_planner::{IndexCoverageGenerationHealthV1, IndexSemanticQueryAvailabilityV1};
use aeordb::engine::v4::index_coverage_registry::{field_definition_fingerprint, field_dependency_fingerprint};
use aeordb::engine::v4::query_executor::{
  QueryAuthoritativeDocumentVisitorV1, QueryAuthoritativeFieldPartitionCursorV1, QueryAuthoritativeFieldPartitionSourceV1,
  QueryAuthoritativeFieldSourceV1, QueryAuthoritativeScopeSourceV1, QueryAuthoritativeValueVisitorV1, QueryExecutionByteLimitsV1,
  QueryExecutionCountLimitsV1, QueryExecutionDocumentV1, QueryExecutionErrorClassV1, QueryExecutionErrorOriginV1,
  QueryExecutionFieldDocumentV1, QueryExecutionFieldPartitionOpenRequestV1, QueryExecutionFieldPartitionReceiptV1,
  QueryExecutionFieldReadReceiptV1, QueryExecutionFieldReadRequestV1, QueryExecutionFieldStateV1, QueryExecutionLimitsV1,
  QueryExecutionMatchPathV1, QueryExecutionMatchRefV1, QueryExecutionMatchSinkV1, QueryExecutionScanErrorV1,
  QueryExecutionScopeScanReceiptV1, QueryExecutionScopeScanRequestV1, QueryExecutionSinkBatchReceiptV1, QueryExecutionSinkBatchV1,
  QueryExecutionSinkErrorClassV1, QueryExecutionSinkErrorV1, QueryExecutionSourceErrorV1, QueryExecutionSourceErrorClassV1,
  RootAwarePartitionedQueryExecutionRequestV1, RootAwareQueryExecutionRequestV1, RootAwareQueryScopeExecutionRequestV1,
  execute_authoritative_partitioned_query_into_v1, execute_authoritative_partitioned_query_v1, execute_authoritative_root_query_into_v1,
  execute_authoritative_root_query_v1, execute_authoritative_scope_query_into_v1, execute_authoritative_scope_query_v1,
};
use aeordb::engine::v4::query_planner::{
  CompiledRootAwareQueryPlanV1, QueryExpressionV1, QueryFuzzyAlgorithmV1, QueryPlanDriverV1, QueryPlanningContextV1,
  QueryPlanningCoverageGenerationV1, QueryPlanningIndexCandidateV1, QueryPlanningIndexEstimatesV1, QueryPlanningRequestV1,
  QueryPlanningScopeV1, QueryPredicateOperationV1, QueryPredicateV1, RootAwareQueryFieldCatalogV1, default_query_planning_limits_v1,
  plan_root_aware_query_v1,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x11; 16];
const PHYSICAL_INSTANCE_ID: [u8; 16] = [0x22; 16];
const ROOT: [u8; 32] = [0x33; 32];
const SEMANTIC_ROOT: [u8; 32] = [0x44; 32];

#[derive(Clone, Copy)]
enum CoverageFixture {
  Authoritative,
  Complete,
  Partial,
}

#[derive(Clone)]
enum FieldFixture {
  Missing,
  Values(Vec<Vec<u8>>),
  SquelchedValues(Vec<Vec<u8>>),
  Unindexable,
  Incomplete(Vec<Vec<u8>>),
  Dishonest(Vec<Vec<u8>>),
}

#[derive(Clone)]
struct DocumentFixture {
  scope_id: Vec<u8>,
  file_key: Vec<u8>,
  revision: Vec<u8>,
  path: String,
  fields: BTreeMap<String, FieldFixture>,
}

struct ScopeFeed {
  root: Vec<u8>,
  publication_sequence: u64,
  documents: Vec<DocumentFixture>,
  complete: bool,
  receipt_count_delta: i64,
  source_error: Option<QueryExecutionSourceErrorV1>,
  cancel_after_documents: Option<usize>,
  field_reads: Cell<u64>,
}

struct DocumentFieldFeed<'a> {
  document: &'a DocumentFixture,
  field_reads: &'a Cell<u64>,
}

struct SquelchingScopeFeed {
  inner: ScopeFeed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PartitionFeedFault {
  None,
  OmitSecondSizeDocument,
  DuplicateFirstFilenameDocument,
  ReverseFilenameDocuments,
  WrongFilenameScope,
  DishonestFilenameReceipt,
}

struct PartitionFeed {
  root: Vec<u8>,
  publication_sequence: u64,
  documents: Vec<DocumentFixture>,
  field_scopes: BTreeMap<String, Vec<Vec<u8>>>,
  fault: PartitionFeedFault,
  opened_fields: Vec<String>,
}

struct PartitionCursor {
  root: Vec<u8>,
  publication_sequence: u64,
  field_name: String,
  requested_scope_ids: Vec<Vec<u8>>,
  rows: Vec<QueryExecutionFieldDocumentV1>,
  next_index: usize,
  dishonest_receipt: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SinkFault {
  None,
  Begin,
  Push,
  Commit,
}

struct RecordingSink {
  fault: SinkFault,
  error_class: QueryExecutionSinkErrorClassV1,
  active: bool,
  begin_calls: u64,
  scope_id: Option<Vec<u8>>,
  staged: Vec<(Vec<u8>, Vec<u8>, Option<String>)>,
  committed: Vec<(Vec<u8>, Vec<u8>, Option<String>)>,
  committed_receipt: Option<(u64, u64, u64)>,
  rollbacks: u64,
}

impl RecordingSink {
  fn new(fault: SinkFault) -> Self {
    Self {
      fault,
      error_class: QueryExecutionSinkErrorClassV1::ResourceLimit,
      active: false,
      begin_calls: 0,
      scope_id: None,
      staged: Vec::new(),
      committed: Vec::new(),
      committed_receipt: None,
      rollbacks: 0,
    }
  }

  fn with_error_class(fault: SinkFault, error_class: QueryExecutionSinkErrorClassV1) -> Self {
    Self { error_class, ..Self::new(fault) }
  }

  fn error(&self) -> QueryExecutionSinkErrorV1 {
    QueryExecutionSinkErrorV1::new(self.error_class, "fixture_sink_failure", "injected sink failure")
  }
}

impl QueryExecutionMatchSinkV1 for RecordingSink {
  fn begin_batch(&mut self, batch: QueryExecutionSinkBatchV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    assert!(!self.active);
    assert_eq!(batch.selected_namespace_root, ROOT);
    self.begin_calls += 1;
    if self.fault == SinkFault::Begin {
      return Err(self.error());
    }
    self.active = true;
    self.scope_id = batch.scope_id.map(<[u8]>::to_vec);
    self.staged.clear();
    Ok(())
  }

  fn push_match(&mut self, matched: QueryExecutionMatchRefV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    assert!(self.active);
    if self.fault == SinkFault::Push {
      return Err(self.error());
    }
    let path = match matched.path {
      QueryExecutionMatchPathV1::Canonical(path) => Some(path.to_string()),
      QueryExecutionMatchPathV1::RequiresSelectedRootLookup => None,
    };
    self.staged.push((matched.file_key.to_vec(), matched.record_revision.to_vec(), path));
    Ok(())
  }

  fn commit_batch(&mut self, receipt: QueryExecutionSinkBatchReceiptV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    assert!(self.active);
    assert_eq!(receipt.selected_namespace_root, ROOT);
    assert_eq!(receipt.match_count as usize, self.staged.len());
    if self.fault == SinkFault::Commit {
      return Err(self.error());
    }
    self.committed.append(&mut self.staged);
    self.committed_receipt = Some((receipt.match_count, receipt.examined_documents, receipt.examined_field_values));
    self.active = false;
    Ok(())
  }

  fn rollback_batch(&mut self) {
    self.staged.clear();
    if self.active {
      self.rollbacks += 1;
    }
    self.active = false;
  }
}

impl QueryAuthoritativeFieldPartitionSourceV1 for PartitionFeed {
  fn open_field_partition(
    &mut self,
    request: QueryExecutionFieldPartitionOpenRequestV1<'_>,
  ) -> Result<Box<dyn QueryAuthoritativeFieldPartitionCursorV1>, QueryExecutionSourceErrorV1> {
    assert_eq!(request.selected_namespace_root, self.root);
    assert_eq!(request.publication_sequence, self.publication_sequence);
    assert_eq!(request.query_path, "/");
    let requested_scope_ids = request.scope_ids.iter().map(|scope_id| scope_id.to_vec()).collect::<Vec<_>>();
    let assignments = self.field_scopes.get(request.field_name).expect("field scope assignments");
    assert_eq!(assignments.len(), self.documents.len());
    let mut rows = Vec::new();
    for (document_index, (document, scope_id)) in self.documents.iter().zip(assignments).enumerate() {
      if self.fault == PartitionFeedFault::OmitSecondSizeDocument && request.field_name == "@size" && document_index == 1 {
        continue;
      }
      let mut scope_id = scope_id.clone();
      if self.fault == PartitionFeedFault::WrongFilenameScope && request.field_name == "@filename" && document_index == 0 {
        scope_id = vec![0x99; scope_id.len()];
      }
      let (state, canonical_values) = match document.fields.get(request.field_name).cloned().unwrap_or(FieldFixture::Missing) {
        FieldFixture::Missing => (QueryExecutionFieldStateV1::Missing, Vec::new()),
        FieldFixture::Values(values)
        | FieldFixture::SquelchedValues(values)
        | FieldFixture::Incomplete(values)
        | FieldFixture::Dishonest(values) => (QueryExecutionFieldStateV1::Values, values),
        FieldFixture::Unindexable => (QueryExecutionFieldStateV1::DeterministicUnindexable, Vec::new()),
      };
      rows.push(QueryExecutionFieldDocumentV1 {
        scope_id,
        file_key: document.file_key.clone(),
        record_revision: document.revision.clone(),
        path: document.path.clone(),
        state,
        canonical_values,
      });
    }
    if self.fault == PartitionFeedFault::DuplicateFirstFilenameDocument && request.field_name == "@filename" {
      rows.insert(1, rows[0].clone());
    }
    if self.fault == PartitionFeedFault::ReverseFilenameDocuments && request.field_name == "@filename" {
      rows.reverse();
    }
    self.opened_fields.push(request.field_name.to_string());
    Ok(Box::new(PartitionCursor {
      root: self.root.clone(),
      publication_sequence: self.publication_sequence,
      field_name: request.field_name.to_string(),
      requested_scope_ids,
      rows,
      next_index: 0,
      dishonest_receipt: self.fault == PartitionFeedFault::DishonestFilenameReceipt && request.field_name == "@filename",
    }))
  }
}

impl QueryAuthoritativeFieldPartitionCursorV1 for PartitionCursor {
  fn next_document(
    &mut self,
    cancellation: &CancellationToken,
  ) -> Result<Option<QueryExecutionFieldDocumentV1>, QueryExecutionSourceErrorV1> {
    if cancellation.is_cancelled() {
      return Err(QueryExecutionSourceErrorV1::new(
        QueryExecutionSourceErrorClassV1::Cancelled,
        "fixture_partition_cancelled",
        "partition cursor was cancelled",
      ));
    }
    let row = self.rows.get(self.next_index).cloned();
    self.next_index = self.next_index.saturating_add(usize::from(row.is_some()));
    Ok(row)
  }

  fn finish(&mut self) -> Result<QueryExecutionFieldPartitionReceiptV1, QueryExecutionSourceErrorV1> {
    let mut scope_document_counts = self
      .requested_scope_ids
      .iter()
      .map(|scope_id| self.rows.iter().filter(|row| row.scope_id == *scope_id).count() as u64)
      .collect::<Vec<_>>();
    if self.dishonest_receipt {
      scope_document_counts[0] = scope_document_counts[0].saturating_add(1);
    }
    Ok(QueryExecutionFieldPartitionReceiptV1 {
      selected_namespace_root: self.root.clone(),
      publication_sequence: self.publication_sequence,
      field_name: self.field_name.clone(),
      scope_ids: self.requested_scope_ids.clone(),
      scope_document_counts,
      document_count: self.rows.len() as u64,
      complete: true,
    })
  }
}

impl QueryAuthoritativeFieldSourceV1 for DocumentFieldFeed<'_> {
  fn scan_field_values(
    &mut self,
    request: QueryExecutionFieldReadRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeValueVisitorV1,
  ) -> Result<QueryExecutionFieldReadReceiptV1, QueryExecutionScanErrorV1> {
    self.field_reads.set(self.field_reads.get() + 1);
    let outcome = self.document.fields.get(request.field_name).cloned().unwrap_or(FieldFixture::Missing);
    let (state, values, complete, count_delta, squelch_visitor_error) = match outcome {
      FieldFixture::Missing => (QueryExecutionFieldStateV1::Missing, Vec::new(), true, 0, false),
      FieldFixture::Values(values) => (QueryExecutionFieldStateV1::Values, values, true, 0, false),
      FieldFixture::SquelchedValues(values) => (QueryExecutionFieldStateV1::Values, values, true, 0, true),
      FieldFixture::Unindexable => (QueryExecutionFieldStateV1::DeterministicUnindexable, Vec::new(), true, 0, false),
      FieldFixture::Incomplete(values) => (QueryExecutionFieldStateV1::Values, values, false, 0, false),
      FieldFixture::Dishonest(values) => (QueryExecutionFieldStateV1::Values, values, true, 1, false),
    };
    let mut canonical_bytes = 0u64;
    for value in &values {
      canonical_bytes += value.len() as u64;
      let result = visitor.visit(value).map_err(QueryExecutionScanErrorV1::Visitor);
      if !squelch_visitor_error {
        result?;
      }
    }
    Ok(QueryExecutionFieldReadReceiptV1 {
      selected_namespace_root: request.selected_namespace_root.to_vec(),
      scope_id: request.scope_id.to_vec(),
      file_key: request.file_key.to_vec(),
      record_revision: request.record_revision.to_vec(),
      field_name: request.field_name.to_string(),
      state,
      value_count: adjusted_count(values.len() as u64, count_delta),
      canonical_value_bytes: canonical_bytes,
      complete,
    })
  }
}

impl QueryAuthoritativeScopeSourceV1 for ScopeFeed {
  fn scan_scope(
    &mut self,
    request: QueryExecutionScopeScanRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryExecutionScopeScanReceiptV1, QueryExecutionScanErrorV1> {
    if let Some(error) = self.source_error.clone() {
      return Err(QueryExecutionScanErrorV1::Source(error));
    }
    let mut count = 0u64;
    for (index, document) in self.documents.iter().filter(|document| document.scope_id == request.scope_id).enumerate() {
      if self.cancel_after_documents == Some(index) {
        request.cancellation.cancel();
      }
      count += 1;
      let mut fields = DocumentFieldFeed { document, field_reads: &self.field_reads };
      visitor
        .visit(
          QueryExecutionDocumentV1 { file_key: &document.file_key, record_revision: &document.revision, path: &document.path },
          &mut fields,
        )
        .map_err(QueryExecutionScanErrorV1::Visitor)?;
    }
    Ok(QueryExecutionScopeScanReceiptV1 {
      selected_namespace_root: self.root.clone(),
      publication_sequence: self.publication_sequence,
      scope_id: request.scope_id.to_vec(),
      document_count: adjusted_count(count, self.receipt_count_delta),
      complete: self.complete,
    })
  }
}

impl QueryAuthoritativeScopeSourceV1 for SquelchingScopeFeed {
  fn scan_scope(
    &mut self,
    request: QueryExecutionScopeScanRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryExecutionScopeScanReceiptV1, QueryExecutionScanErrorV1> {
    let mut count = 0u64;
    for document in self.inner.documents.iter().filter(|document| document.scope_id == request.scope_id) {
      count += 1;
      let mut fields = DocumentFieldFeed { document, field_reads: &self.inner.field_reads };
      let _result = visitor.visit(
        QueryExecutionDocumentV1 { file_key: &document.file_key, record_revision: &document.revision, path: &document.path },
        &mut fields,
      );
    }
    Ok(QueryExecutionScopeScanReceiptV1 {
      selected_namespace_root: self.inner.root.clone(),
      publication_sequence: self.inner.publication_sequence,
      scope_id: request.scope_id.to_vec(),
      document_count: count,
      complete: true,
    })
  }
}

fn fixture(path: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn scope_fixture() -> Vec<u8> {
  fixture("scope-definition-v1/ascp-blake3-256-root-direct-valid.bin")
}

fn value_store_fixture(name: &str) -> Vec<u8> {
  fixture(&format!("value-store-definition-v1/{name}.bin"))
}

fn field_fixture(name: &str) -> Vec<u8> {
  fixture(&format!("field-index-definition-v1/afix-blake3-256-{name}-valid.bin"))
}

fn definitions(field_name: &str, converter: &str, encoded_scope: &[u8]) -> (Vec<u8>, Vec<u8>) {
  let mut value_store = value_store_fixture("avst-blake3-256-metadata-hash-corrected-valid");
  let scope = decode_scope_definition(encoded_scope, HashAlgorithm::Blake3_256).unwrap();
  value_store[32..64].copy_from_slice(&scope.scope_id);
  let metadata_id = match field_name {
    "@filename" => 2u16,
    "@size" => 5u16,
    _ => panic!("unsupported metadata fixture field {field_name}"),
  };
  let field_start = 144usize;
  let old_field_length = u32::from_le_bytes(value_store[64..68].try_into().unwrap()) as usize;
  value_store.splice(field_start..field_start + old_field_length, field_name.as_bytes().iter().copied());
  let total_length = value_store.len() as u32;
  value_store[8..12].copy_from_slice(&total_length.to_le_bytes());
  value_store[64..68].copy_from_slice(&(field_name.len() as u32).to_le_bytes());
  let selector_start = field_start + field_name.len();
  value_store[selector_start + 32..selector_start + 34].copy_from_slice(&metadata_id.to_le_bytes());
  let value_definition = decode_value_store_definition(&value_store, HashAlgorithm::Blake3_256).unwrap();

  let mut field = field_fixture(converter);
  field[32..64].copy_from_slice(&value_definition.value_store_id);
  decode_field_index_definition(&field, HashAlgorithm::Blake3_256).unwrap();
  (value_store, field)
}

fn generation(scope: &[u8], value_store: &[u8], field: &[u8], coverage: CoverageFixture) -> Option<QueryPlanningCoverageGenerationV1> {
  let (source_root, publication_sequence) = match coverage {
    CoverageFixture::Authoritative => return None,
    CoverageFixture::Complete => (ROOT.to_vec(), 41),
    CoverageFixture::Partial => (vec![0x55; 32], 40),
  };
  let scope_definition = decode_scope_definition(scope, HashAlgorithm::Blake3_256).unwrap();
  let value_definition = decode_value_store_definition(value_store, HashAlgorithm::Blake3_256).unwrap();
  let field_definition = decode_field_index_definition(field, HashAlgorithm::Blake3_256).unwrap();
  Some(QueryPlanningCoverageGenerationV1 {
    generation: 7,
    owner_id: field_definition.index_id,
    manifest_hash: vec![0x71; 32],
    source_namespace_root: source_root,
    coverage_epoch_id: [0x72; 16],
    coverage_publication_sequence: publication_sequence,
    definition_fingerprint: field_definition_fingerprint(HashAlgorithm::Blake3_256, field),
    dependency_fingerprint: field_dependency_fingerprint(
      HashAlgorithm::Blake3_256,
      &scope_definition.scope_id,
      &value_definition.value_store_id,
    ),
    health: IndexCoverageGenerationHealthV1::Healthy,
  })
}

fn catalog(field_name: &str, converter: &str, coverage: CoverageFixture) -> RootAwareQueryFieldCatalogV1 {
  let encoded_scope = scope_fixture();
  let scope = catalog_scope(field_name, converter, coverage, encoded_scope);
  RootAwareQueryFieldCatalogV1 {
    database_id: DATABASE_ID,
    physical_instance_id: PHYSICAL_INSTANCE_ID,
    selected_namespace_root: ROOT.to_vec(),
    semantic_state_root: SEMANTIC_ROOT.to_vec(),
    publication_sequence: 41,
    field_name: field_name.to_string(),
    complete: true,
    scopes: vec![scope],
  }
}

fn catalog_scope(field_name: &str, converter: &str, coverage: CoverageFixture, encoded_scope: Vec<u8>) -> QueryPlanningScopeV1 {
  let scope_definition = decode_scope_definition(&encoded_scope, HashAlgorithm::Blake3_256).unwrap();
  let (value_store, field) = definitions(field_name, converter, &encoded_scope);
  let field_definition = decode_field_index_definition(&field, HashAlgorithm::Blake3_256).unwrap();
  let value_definition = decode_value_store_definition(&value_store, HashAlgorithm::Blake3_256).unwrap();
  let selected_generation = generation(&encoded_scope, &value_store, &field, coverage);
  QueryPlanningScopeV1 {
    scope_id: scope_definition.scope_id,
    value_store_id: value_definition.value_store_id,
    encoded_scope_definition: encoded_scope,
    encoded_value_store_definition: value_store,
    semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
    authoritative_document_count: 100,
    indexes: vec![QueryPlanningIndexCandidateV1 {
      index_id: field_definition.index_id,
      encoded_field_definition: field,
      selected_generation,
      estimates: QueryPlanningIndexEstimatesV1::new(1, 4, 4, 1, 1).unwrap(),
      nvt_hint_available: true,
    }],
  }
}

fn plan(coverage: CoverageFixture) -> (CompiledRootAwareQueryPlanV1, Vec<RootAwareQueryFieldCatalogV1>) {
  plan_with_catalogs(vec![catalog("@filename", "typed_exact_blake3_v1", coverage), catalog("@size", "u64_order_v1", coverage)])
}

fn plan_with_catalogs(catalogs: Vec<RootAwareQueryFieldCatalogV1>) -> (CompiledRootAwareQueryPlanV1, Vec<RootAwareQueryFieldCatalogV1>) {
  let expression = QueryExpressionV1::And(vec![
    QueryExpressionV1::Or(vec![
      QueryExpressionV1::Field(QueryPredicateV1 {
        field_name: "@size".to_string(),
        operation: QueryPredicateOperationV1::Between(CanonicalConfigValueV1::Unsigned(5), CanonicalConfigValueV1::Unsigned(10)),
      }),
      QueryExpressionV1::Field(QueryPredicateV1 {
        field_name: "@filename".to_string(),
        operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("special".to_string())),
      }),
    ]),
    QueryExpressionV1::Not(Box::new(QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@filename".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("draft".to_string())),
    }))),
  ]);
  let context =
    QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, HashAlgorithm::Blake3_256, &ROOT, &SEMANTIC_ROOT, 41).unwrap();
  let request = QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 2,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  };
  (plan_root_aware_query_v1(&request).unwrap(), catalogs)
}

fn canonical(value: CanonicalConfigValueV1) -> Vec<u8> {
  encode_canonical_value(&value, CanonicalValueBounds::SOURCE_VALUE).unwrap()
}

fn documents() -> Vec<DocumentFixture> {
  let scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  [
    (0x10, "/alpha", Some(7), "alpha"),
    (0x20, "/special", Some(20), "special"),
    (0x30, "/draft", Some(7), "draft"),
    (0x40, "/missing", None, "alpha"),
  ]
  .into_iter()
  .map(|(identity, path, size, filename)| {
    let mut fields = BTreeMap::new();
    fields.insert(
      "@filename".to_string(),
      FieldFixture::Values(vec![
        canonical(CanonicalConfigValueV1::String(filename.to_string())),
        canonical(CanonicalConfigValueV1::String(filename.to_string())),
      ]),
    );
    fields.insert(
      "@size".to_string(),
      size.map_or(FieldFixture::Missing, |size| FieldFixture::Values(vec![canonical(CanonicalConfigValueV1::Unsigned(size))])),
    );
    DocumentFixture {
      scope_id: scope_id.clone(),
      file_key: vec![identity; 32],
      revision: vec![identity + 1; 32],
      path: path.to_string(),
      fields,
    }
  })
  .collect()
}

fn reference_truth(documents: &[DocumentFixture]) -> Vec<Vec<u8>> {
  let value_matches = |document: &DocumentFixture, field_name: &str, expected: &CanonicalConfigValueV1| {
    let Some(FieldFixture::Values(values)) = document.fields.get(field_name) else {
      return false;
    };
    values.iter().any(|value| decode_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE).is_ok_and(|value| &value == expected))
  };
  let size_between = |document: &DocumentFixture| {
    let Some(FieldFixture::Values(values)) = document.fields.get("@size") else {
      return false;
    };
    values.iter().any(|value| {
      matches!(
        decode_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE),
        Ok(CanonicalConfigValueV1::Unsigned(value)) if (5..=10).contains(&value)
      )
    })
  };
  documents
    .iter()
    .filter(|document| {
      (size_between(document) || value_matches(document, "@filename", &CanonicalConfigValueV1::String("special".to_string())))
        && !value_matches(document, "@filename", &CanonicalConfigValueV1::String("draft".to_string()))
    })
    .map(|document| document.file_key.clone())
    .collect()
}

fn limits() -> QueryExecutionLimitsV1 {
  QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(100, 100, 100, 1_000_000).unwrap(),
    QueryExecutionByteLimitsV1::new(1 << 20, 8 << 20, 16 << 20).unwrap(),
  )
}

fn memory(limit: u64) -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(limit - (1 << 20), limit, 1, 1 << 20).unwrap())
}

fn partition_feed(
  documents: Vec<DocumentFixture>,
  direct_scope_id: &[u8],
  alternate_scope_id: &[u8],
  fault: PartitionFeedFault,
) -> PartitionFeed {
  let filename_scopes = (0..documents.len())
    .map(|index| if index % 2 == 0 { direct_scope_id.to_vec() } else { alternate_scope_id.to_vec() })
    .collect::<Vec<_>>();
  let size_scopes = vec![direct_scope_id.to_vec(); documents.len()];
  PartitionFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    field_scopes: BTreeMap::from([("@filename".to_string(), filename_scopes), ("@size".to_string(), size_scopes)]),
    documents,
    fault,
    opened_fields: Vec::new(),
  }
}

fn execute(
  plan: &CompiledRootAwareQueryPlanV1,
  catalogs: &[RootAwareQueryFieldCatalogV1],
  feed: &mut ScopeFeed,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
) -> Result<Vec<Vec<u8>>, aeordb::engine::v4::query_executor::QueryExecutionErrorV1> {
  let result = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
    plan,
    catalogs,
    source: feed,
    memory,
    cancellation,
    limits: limits(),
  })?;
  Ok(result.matches().iter().map(|row| row.file_key().to_vec()).collect())
}

fn execute_with_limits(
  plan: &CompiledRootAwareQueryPlanV1,
  catalogs: &[RootAwareQueryFieldCatalogV1],
  feed: &mut ScopeFeed,
  memory: &MemoryCoordinator,
  limits: QueryExecutionLimitsV1,
) -> Result<Vec<Vec<u8>>, aeordb::engine::v4::query_executor::QueryExecutionErrorV1> {
  let result = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
    plan,
    catalogs,
    source: feed,
    memory,
    cancellation: &CancellationToken::new(),
    limits,
  })?;
  Ok(result.matches().iter().map(|row| row.file_key().to_vec()).collect())
}

fn minimum_scope_semantic_bytes(
  plan: &CompiledRootAwareQueryPlanV1,
  catalogs: &[RootAwareQueryFieldCatalogV1],
  scope_id: &[u8],
  documents: &[DocumentFixture],
) -> u64 {
  let succeeds = |semantic_bytes: u64| {
    let memory = memory(64 << 20);
    let mut feed = ScopeFeed {
      root: ROOT.to_vec(),
      publication_sequence: 41,
      documents: documents.to_vec(),
      complete: true,
      receipt_count_delta: 0,
      source_error: None,
      cancel_after_documents: None,
      field_reads: Cell::new(0),
    };
    let limits = QueryExecutionLimitsV1::new(
      QueryExecutionCountLimitsV1::new(100, 100, 100, 1_000_000).unwrap(),
      QueryExecutionByteLimitsV1::new(1 << 20, 8 << 20, semantic_bytes).unwrap(),
    );
    execute_authoritative_scope_query_v1(RootAwareQueryScopeExecutionRequestV1 {
      plan,
      catalogs,
      scope_id,
      source: &mut feed,
      memory: &memory,
      cancellation: &CancellationToken::new(),
      limits,
    })
    .is_ok()
  };
  let mut low = 1u64;
  let mut high = 1 << 20;
  assert!(succeeds(high));
  while low < high {
    let middle = low + (high - low) / 2;
    if succeeds(middle) {
      high = middle;
    } else {
      low = middle + 1;
    }
  }
  low
}

#[test]
fn authoritative_truth_is_identical_for_authoritative_complete_and_partial_plans() {
  let source_documents = documents();
  let expected = reference_truth(&source_documents);
  assert_eq!(expected, vec![vec![0x10; 32], vec![0x20; 32]]);
  for coverage in [CoverageFixture::Authoritative, CoverageFixture::Complete, CoverageFixture::Partial] {
    let (plan, catalogs) = plan(coverage);
    assert!(plan.predicates().iter().all(|predicate| {
      predicate.scopes().iter().all(|scope| match coverage {
        CoverageFixture::Authoritative => matches!(scope.driver(), QueryPlanDriverV1::Authoritative { .. }),
        CoverageFixture::Complete => matches!(scope.driver(), QueryPlanDriverV1::Index { .. }),
        CoverageFixture::Partial => matches!(scope.driver(), QueryPlanDriverV1::Index { .. }),
      })
    }));
    let memory = memory(64 << 20);
    let mut feed = ScopeFeed {
      root: ROOT.to_vec(),
      publication_sequence: 41,
      documents: source_documents.clone(),
      complete: true,
      receipt_count_delta: 0,
      source_error: None,
      cancel_after_documents: None,
      field_reads: Cell::new(0),
    };
    assert_eq!(execute(&plan, &catalogs, &mut feed, &memory, &CancellationToken::new()).unwrap(), expected);
    assert_eq!(feed.field_reads.get(), 8, "each document must scan each distinct field exactly once");
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn authoritative_streaming_sink_commits_only_validated_root_and_scope_batches() {
  let (plan, catalogs) = plan(CoverageFixture::Authoritative);
  let expected = reference_truth(&documents());
  let root_memory = memory(64 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let mut sink = RecordingSink::new(SinkFault::None);

  let receipt = execute_authoritative_root_query_into_v1(
    RootAwareQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      source: &mut feed,
      memory: &root_memory,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut sink,
  )
  .unwrap();

  assert_eq!(receipt.selected_namespace_root(), ROOT);
  assert_eq!(receipt.scope_id(), None);
  assert_eq!(receipt.match_count(), expected.len() as u64);
  assert_eq!(receipt.examined_documents(), 4);
  assert_eq!(receipt.examined_field_values(), 11);
  assert_eq!(sink.begin_calls, 1);
  assert_eq!(sink.scope_id, None);
  assert_eq!(sink.committed.iter().map(|row| row.0.clone()).collect::<Vec<_>>(), expected);
  assert_eq!(sink.committed.iter().map(|row| row.2.as_deref()).collect::<Vec<_>>(), [Some("/alpha"), Some("/special")]);
  assert_eq!(sink.committed_receipt, Some((2, 4, 11)));
  assert_eq!(sink.rollbacks, 0);
  assert!(!sink.active);
  assert_eq!(root_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let scope_memory = memory(64 << 20);
  let mut scope_feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let mut scope_sink = RecordingSink::new(SinkFault::None);
  let scope_receipt = execute_authoritative_scope_query_into_v1(
    RootAwareQueryScopeExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      scope_id: &scope_id,
      source: &mut scope_feed,
      memory: &scope_memory,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut scope_sink,
  )
  .unwrap();
  assert_eq!(scope_receipt.scope_id(), Some(scope_id.as_slice()));
  assert_eq!(scope_sink.scope_id, Some(scope_id));
  assert_eq!(scope_sink.committed.iter().map(|row| row.0.clone()).collect::<Vec<_>>(), expected);
  assert_eq!(scope_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn authoritative_streaming_sink_rolls_back_unvalidated_source_output() {
  let (plan, catalogs) = plan(CoverageFixture::Authoritative);
  let honest_memory = memory(64 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 1,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let mut sink = RecordingSink::new(SinkFault::None);

  let error = execute_authoritative_root_query_into_v1(
    RootAwareQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      source: &mut feed,
      memory: &honest_memory,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut sink,
  )
  .unwrap_err();

  assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
  assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Execution);
  assert_eq!(sink.begin_calls, 1);
  assert_eq!(sink.rollbacks, 1);
  assert!(sink.staged.is_empty());
  assert!(sink.committed.is_empty());
  assert!(sink.committed_receipt.is_none());
  assert!(!sink.active);
  assert_eq!(honest_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn authoritative_streaming_sink_commits_an_exact_empty_batch() {
  let (plan, catalogs) = plan(CoverageFixture::Authoritative);
  let memory = memory(64 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: Vec::new(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let mut sink = RecordingSink::new(SinkFault::None);
  let receipt = execute_authoritative_root_query_into_v1(
    RootAwareQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      source: &mut feed,
      memory: &memory,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut sink,
  )
  .unwrap();
  assert_eq!(receipt.match_count(), 0);
  assert_eq!(receipt.examined_documents(), 0);
  assert_eq!(receipt.examined_field_values(), 0);
  assert_eq!(sink.committed_receipt, Some((0, 0, 0)));
  assert!(sink.staged.is_empty());
  assert!(sink.committed.is_empty());
  assert_eq!(sink.rollbacks, 0);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn authoritative_streaming_sink_does_not_reserve_the_collected_result_budget() {
  let (plan, catalogs) = plan(CoverageFixture::Authoritative);
  let constrained = memory(20 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let mut sink = RecordingSink::new(SinkFault::None);
  execute_authoritative_root_query_into_v1(
    RootAwareQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      source: &mut feed,
      memory: &constrained,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut sink,
  )
  .unwrap();
  assert_eq!(sink.committed.len(), 2);
  assert_eq!(constrained.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let collected_memory = memory(20 << 20);
  let mut collected_feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let error = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
    plan: &plan,
    catalogs: &catalogs,
    source: &mut collected_feed,
    memory: &collected_memory,
    cancellation: &CancellationToken::new(),
    limits: limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_execution_semantic_memory");
  assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Execution);
  assert_eq!(collected_feed.field_reads.get(), 0);
  assert_eq!(collected_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn authoritative_streaming_sink_failures_are_terminal_and_atomic() {
  let (plan, catalogs) = plan(CoverageFixture::Authoritative);
  for fault in [SinkFault::Begin, SinkFault::Push, SinkFault::Commit] {
    let memory = memory(64 << 20);
    let mut feed = ScopeFeed {
      root: ROOT.to_vec(),
      publication_sequence: 41,
      documents: documents(),
      complete: true,
      receipt_count_delta: 0,
      source_error: None,
      cancel_after_documents: None,
      field_reads: Cell::new(0),
    };
    let mut sink = RecordingSink::new(fault);

    let error = execute_authoritative_root_query_into_v1(
      RootAwareQueryExecutionRequestV1 {
        plan: &plan,
        catalogs: &catalogs,
        source: &mut feed,
        memory: &memory,
        cancellation: &CancellationToken::new(),
        limits: limits(),
      },
      &mut sink,
    )
    .unwrap_err();

    assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit, "fault {fault:?}: {error}");
    assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Sink, "fault {fault:?}: {error}");
    assert_eq!(error.code(), "fixture_sink_failure");
    assert_eq!(sink.begin_calls, 1);
    assert_eq!(sink.rollbacks, u64::from(fault != SinkFault::Begin));
    assert!(sink.staged.is_empty());
    assert!(sink.committed.is_empty());
    assert!(sink.committed_receipt.is_none());
    assert!(!sink.active);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0, "fault {fault:?}");
  }
}

#[test]
fn authoritative_streaming_sink_rolls_back_staged_rows_on_cancellation_and_match_limit() {
  let (plan, catalogs) = plan(CoverageFixture::Authoritative);
  let cancellation = CancellationToken::new();
  let cancellation_memory = memory(64 << 20);
  let mut cancelled_feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: Some(1),
    field_reads: Cell::new(0),
  };
  let mut cancelled_sink = RecordingSink::new(SinkFault::None);
  let error = execute_authoritative_root_query_into_v1(
    RootAwareQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      source: &mut cancelled_feed,
      memory: &cancellation_memory,
      cancellation: &cancellation,
      limits: limits(),
    },
    &mut cancelled_sink,
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::Cancelled);
  assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Execution);
  assert_eq!(cancelled_sink.rollbacks, 1);
  assert!(cancelled_sink.staged.is_empty());
  assert!(cancelled_sink.committed.is_empty());
  assert_eq!(cancellation_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let limited_memory = memory(64 << 20);
  let mut limited_feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let mut limited_sink = RecordingSink::new(SinkFault::None);
  let limited = QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(100, 100, 1, 1_000_000).unwrap(),
    QueryExecutionByteLimitsV1::new(1 << 20, 8 << 20, 16 << 20).unwrap(),
  );
  let error = execute_authoritative_root_query_into_v1(
    RootAwareQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      source: &mut limited_feed,
      memory: &limited_memory,
      cancellation: &CancellationToken::new(),
      limits: limited,
    },
    &mut limited_sink,
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit);
  assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Execution);
  assert_eq!(error.code(), "query_execution_match_limit");
  assert_eq!(limited_sink.rollbacks, 1);
  assert!(limited_sink.staged.is_empty());
  assert!(limited_sink.committed.is_empty());
  assert_eq!(limited_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn authoritative_streaming_sink_preserves_every_sink_error_class() {
  let (plan, catalogs) = plan(CoverageFixture::Authoritative);
  for (sink_class, expected) in [
    (QueryExecutionSinkErrorClassV1::HistoricalViewUnavailable, QueryExecutionErrorClassV1::HistoricalViewUnavailable),
    (QueryExecutionSinkErrorClassV1::CorruptSource, QueryExecutionErrorClassV1::CorruptSource),
    (QueryExecutionSinkErrorClassV1::Cancelled, QueryExecutionErrorClassV1::Cancelled),
    (QueryExecutionSinkErrorClassV1::Internal, QueryExecutionErrorClassV1::Internal),
  ] {
    let memory = memory(64 << 20);
    let mut feed = ScopeFeed {
      root: ROOT.to_vec(),
      publication_sequence: 41,
      documents: documents(),
      complete: true,
      receipt_count_delta: 0,
      source_error: None,
      cancel_after_documents: None,
      field_reads: Cell::new(0),
    };
    let mut sink = RecordingSink::with_error_class(SinkFault::Begin, sink_class);
    let error = execute_authoritative_root_query_into_v1(
      RootAwareQueryExecutionRequestV1 {
        plan: &plan,
        catalogs: &catalogs,
        source: &mut feed,
        memory: &memory,
        cancellation: &CancellationToken::new(),
        limits: limits(),
      },
      &mut sink,
    )
    .unwrap_err();
    assert_eq!(error.class(), expected);
    assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Sink);
    assert_eq!(error.code(), "fixture_sink_failure");
    assert_eq!(sink.rollbacks, 0);
    assert!(sink.committed.is_empty());
    assert_eq!(feed.field_reads.get(), 0);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn nonidentical_cross_field_scope_partitions_never_silently_omit_a_valid_scope() {
  let mut filename = catalog("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative);
  filename.scopes.push(catalog_scope(
    "@filename",
    "typed_exact_blake3_v1",
    CoverageFixture::Authoritative,
    fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin"),
  ));
  filename.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let (plan, catalogs) = plan_with_catalogs(vec![filename, catalog("@size", "u64_order_v1", CoverageFixture::Authoritative)]);
  let memory = memory(64 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };

  let error = execute(&plan, &catalogs, &mut feed, &memory, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::HistoricalViewUnavailable);
  assert_eq!(error.code(), "query_execution_scope_partition_unavailable");
  assert_eq!(feed.field_reads.get(), 0, "partition refusal must precede authoritative source work");
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn partitioned_authoritative_execution_joins_nonidentical_field_scopes_by_file_key() {
  let alternate_scope = fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin");
  let alternate_scope_id = decode_scope_definition(&alternate_scope, HashAlgorithm::Blake3_256).unwrap().scope_id;
  let direct_scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut filename = catalog("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative);
  filename.scopes.push(catalog_scope("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative, alternate_scope));
  filename.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let (plan, mut catalogs) = plan_with_catalogs(vec![filename, catalog("@size", "u64_order_v1", CoverageFixture::Authoritative)]);
  catalogs.reverse();
  let documents = documents();
  let expected = reference_truth(&documents);
  let mut source = partition_feed(documents, &direct_scope_id, &alternate_scope_id, PartitionFeedFault::None);
  let memory = memory(64 << 20);

  let execution = execute_authoritative_partitioned_query_v1(RootAwarePartitionedQueryExecutionRequestV1 {
    plan: &plan,
    catalogs: &catalogs,
    source: &mut source,
    memory: &memory,
    cancellation: &CancellationToken::new(),
    limits: limits(),
  })
  .unwrap();

  assert_eq!(execution.matches().iter().map(|row| row.file_key().to_vec()).collect::<Vec<_>>(), expected);
  assert_eq!(source.opened_fields, ["@filename", "@size"]);
  drop(execution);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn partitioned_streaming_sink_commits_only_after_every_field_receipt_validates() {
  let alternate_scope = fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin");
  let alternate_scope_id = decode_scope_definition(&alternate_scope, HashAlgorithm::Blake3_256).unwrap().scope_id;
  let direct_scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut filename = catalog("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative);
  filename.scopes.push(catalog_scope("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative, alternate_scope));
  filename.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let (plan, catalogs) = plan_with_catalogs(vec![filename, catalog("@size", "u64_order_v1", CoverageFixture::Authoritative)]);
  let expected = reference_truth(&documents());

  let honest_memory = memory(64 << 20);
  let mut source = partition_feed(documents(), &direct_scope_id, &alternate_scope_id, PartitionFeedFault::None);
  let mut sink = RecordingSink::new(SinkFault::None);
  let receipt = execute_authoritative_partitioned_query_into_v1(
    RootAwarePartitionedQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      source: &mut source,
      memory: &honest_memory,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut sink,
  )
  .unwrap();
  assert_eq!(receipt.scope_id(), None);
  assert_eq!(receipt.match_count(), expected.len() as u64);
  assert_eq!(sink.committed.iter().map(|row| row.0.clone()).collect::<Vec<_>>(), expected);
  assert_eq!(sink.rollbacks, 0);
  assert_eq!(honest_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let dishonest_memory = memory(64 << 20);
  let mut dishonest_source =
    partition_feed(documents(), &direct_scope_id, &alternate_scope_id, PartitionFeedFault::DishonestFilenameReceipt);
  let mut dishonest_sink = RecordingSink::new(SinkFault::None);
  let error = execute_authoritative_partitioned_query_into_v1(
    RootAwarePartitionedQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      source: &mut dishonest_source,
      memory: &dishonest_memory,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut dishonest_sink,
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
  assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Execution);
  assert_eq!(dishonest_sink.rollbacks, 1);
  assert!(dishonest_sink.staged.is_empty());
  assert!(dishonest_sink.committed.is_empty());
  assert_eq!(dishonest_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn partitioned_authoritative_execution_accepts_an_empty_authoritative_universe() {
  let alternate_scope = fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin");
  let alternate_scope_id = decode_scope_definition(&alternate_scope, HashAlgorithm::Blake3_256).unwrap().scope_id;
  let direct_scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut filename = catalog("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative);
  filename.scopes.push(catalog_scope("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative, alternate_scope));
  filename.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let (plan, catalogs) = plan_with_catalogs(vec![filename, catalog("@size", "u64_order_v1", CoverageFixture::Authoritative)]);
  let mut source = partition_feed(Vec::new(), &direct_scope_id, &alternate_scope_id, PartitionFeedFault::None);
  let memory = memory(64 << 20);

  let execution = execute_authoritative_partitioned_query_v1(RootAwarePartitionedQueryExecutionRequestV1 {
    plan: &plan,
    catalogs: &catalogs,
    source: &mut source,
    memory: &memory,
    cancellation: &CancellationToken::new(),
    limits: limits(),
  })
  .unwrap();

  assert!(execution.matches().is_empty());
  assert_eq!(source.opened_fields, ["@filename", "@size"]);
  drop(execution);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn partitioned_authoritative_execution_rejects_every_incomplete_or_malformed_file_key_join() {
  let alternate_scope = fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin");
  let alternate_scope_id = decode_scope_definition(&alternate_scope, HashAlgorithm::Blake3_256).unwrap().scope_id;
  let direct_scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut filename = catalog("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative);
  filename.scopes.push(catalog_scope("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative, alternate_scope));
  filename.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let (plan, catalogs) = plan_with_catalogs(vec![filename, catalog("@size", "u64_order_v1", CoverageFixture::Authoritative)]);

  for fault in [
    PartitionFeedFault::OmitSecondSizeDocument,
    PartitionFeedFault::DuplicateFirstFilenameDocument,
    PartitionFeedFault::ReverseFilenameDocuments,
    PartitionFeedFault::WrongFilenameScope,
    PartitionFeedFault::DishonestFilenameReceipt,
  ] {
    let memory = memory(64 << 20);
    let mut source = partition_feed(documents(), &direct_scope_id, &alternate_scope_id, fault);
    let error = execute_authoritative_partitioned_query_v1(RootAwarePartitionedQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      source: &mut source,
      memory: &memory,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    })
    .unwrap_err();
    assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource, "fault {fault:?}: {error}");
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0, "fault {fault:?}");
  }

  for malformed_catalogs in [vec![catalogs[0].clone()], vec![catalogs[0].clone(), catalogs[0].clone()]] {
    let memory = memory(64 << 20);
    let mut source = partition_feed(documents(), &direct_scope_id, &alternate_scope_id, PartitionFeedFault::None);
    let error = execute_authoritative_partitioned_query_v1(RootAwarePartitionedQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &malformed_catalogs,
      source: &mut source,
      memory: &memory,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    })
    .unwrap_err();
    assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
    assert_eq!(error.code(), "query_execution_catalog_partition");
    assert!(source.opened_fields.is_empty());
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }

  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let pressure_memory = memory(64 << 20);
  let mut source = partition_feed(documents(), &direct_scope_id, &alternate_scope_id, PartitionFeedFault::None);
  let error = execute_authoritative_partitioned_query_v1(RootAwarePartitionedQueryExecutionRequestV1 {
    plan: &plan,
    catalogs: &catalogs,
    source: &mut source,
    memory: &pressure_memory,
    cancellation: &cancellation,
    limits: limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::Cancelled);
  assert!(source.opened_fields.is_empty());
  assert_eq!(pressure_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn partitioned_execution_fails_before_open_under_pressure_and_preserves_field_state_direction() {
  let alternate_scope = fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin");
  let alternate_scope_id = decode_scope_definition(&alternate_scope, HashAlgorithm::Blake3_256).unwrap().scope_id;
  let direct_scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut filename = catalog("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative);
  filename.scopes.push(catalog_scope("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative, alternate_scope));
  filename.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let (plan, catalogs) = plan_with_catalogs(vec![filename, catalog("@size", "u64_order_v1", CoverageFixture::Authoritative)]);

  let pressured_limits = QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(100, 100, 100, 1_000_000).unwrap(),
    QueryExecutionByteLimitsV1::new(8 << 20, 8 << 20, 1 << 20).unwrap(),
  );
  let pressure_memory = memory(64 << 20);
  let mut source = partition_feed(documents(), &direct_scope_id, &alternate_scope_id, PartitionFeedFault::None);
  let error = execute_authoritative_partitioned_query_v1(RootAwarePartitionedQueryExecutionRequestV1 {
    plan: &plan,
    catalogs: &catalogs,
    source: &mut source,
    memory: &pressure_memory,
    cancellation: &CancellationToken::new(),
    limits: pressured_limits,
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit);
  assert!(source.opened_fields.is_empty());
  assert_eq!(pressure_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  for (field, expected) in [
    (FieldFixture::Values(Vec::new()), QueryExecutionErrorClassV1::CorruptSource),
    (FieldFixture::Unindexable, QueryExecutionErrorClassV1::HistoricalViewUnavailable),
  ] {
    let mut invalid_documents = documents();
    invalid_documents[0].fields.insert("@filename".to_string(), field);
    let memory = memory(64 << 20);
    let mut source = partition_feed(invalid_documents, &direct_scope_id, &alternate_scope_id, PartitionFeedFault::None);
    let error = execute_authoritative_partitioned_query_v1(RootAwarePartitionedQueryExecutionRequestV1 {
      plan: &plan,
      catalogs: &catalogs,
      source: &mut source,
      memory: &memory,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    })
    .unwrap_err();
    assert_eq!(error.class(), expected);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn partitioned_execution_charges_catalog_partition_and_sort_work_before_source_open() {
  let alternate_scope = fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin");
  let alternate_scope_id = decode_scope_definition(&alternate_scope, HashAlgorithm::Blake3_256).unwrap().scope_id;
  let direct_scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut filename = catalog("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative);
  filename.scopes.push(catalog_scope("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative, alternate_scope));
  filename.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let (plan, catalogs) = plan_with_catalogs(vec![filename, catalog("@size", "u64_order_v1", CoverageFixture::Authoritative)]);
  let mut source = partition_feed(Vec::new(), &direct_scope_id, &alternate_scope_id, PartitionFeedFault::None);
  let memory = memory(64 << 20);
  let limits = QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(100, 100, 100, 1_740).unwrap(),
    QueryExecutionByteLimitsV1::new(1 << 20, 8 << 20, 16 << 20).unwrap(),
  );

  let error = execute_authoritative_partitioned_query_v1(RootAwarePartitionedQueryExecutionRequestV1 {
    plan: &plan,
    catalogs: &catalogs,
    source: &mut source,
    memory: &memory,
    cancellation: &CancellationToken::new(),
    limits,
  })
  .unwrap_err();

  assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_execution_work_limit");
  assert!(source.opened_fields.is_empty());
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn scope_execution_reads_only_the_requested_member_of_a_shared_partition() {
  let alternate_scope = fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin");
  let alternate_scope_id = decode_scope_definition(&alternate_scope, HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut filename = catalog("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative);
  filename.scopes.push(catalog_scope("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative, alternate_scope.clone()));
  filename.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let mut size = catalog("@size", "u64_order_v1", CoverageFixture::Authoritative);
  size.scopes.push(catalog_scope("@size", "u64_order_v1", CoverageFixture::Authoritative, alternate_scope));
  size.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let (plan, catalogs) = plan_with_catalogs(vec![filename, size]);
  let direct_scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut direct = documents().remove(0);
  let mut alternate = direct.clone();
  direct.file_key = vec![0x10; 32];
  alternate.scope_id = alternate_scope_id;
  alternate.file_key = vec![0x20; 32];
  alternate.revision = vec![0x21; 32];
  alternate.path = "/alternate".to_string();
  let memory = memory(64 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: vec![direct, alternate],
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };

  let execution = execute_authoritative_scope_query_v1(RootAwareQueryScopeExecutionRequestV1 {
    plan: &plan,
    catalogs: &catalogs,
    scope_id: &direct_scope_id,
    source: &mut feed,
    memory: &memory,
    cancellation: &CancellationToken::new(),
    limits: limits(),
  })
  .unwrap();
  assert_eq!(execution.matches().len(), 1);
  assert_eq!(execution.matches()[0].file_key(), &[0x10; 32]);
  assert_eq!(feed.field_reads.get(), 2, "scope execution must not inspect a sibling effective scope");
  drop(execution);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn scope_execution_admits_only_the_requested_member_of_a_shared_partition() {
  let alternate_scope = fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin");
  let alternate_scope_id = decode_scope_definition(&alternate_scope, HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut filename = catalog("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative);
  filename.scopes.push(catalog_scope("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative, alternate_scope.clone()));
  let mut size = catalog("@size", "u64_order_v1", CoverageFixture::Authoritative);
  size.scopes.push(catalog_scope("@size", "u64_order_v1", CoverageFixture::Authoritative, alternate_scope));
  for suffix in 0..64u8 {
    let mut sibling_scope = fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin");
    let length = sibling_scope.len();
    sibling_scope[length - 2] = b'a' + suffix / 26;
    sibling_scope[length - 1] = b'a' + suffix % 26;
    filename.scopes.push(catalog_scope("@filename", "typed_exact_blake3_v1", CoverageFixture::Authoritative, sibling_scope.clone()));
    size.scopes.push(catalog_scope("@size", "u64_order_v1", CoverageFixture::Authoritative, sibling_scope));
  }
  filename.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  size.scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let (shared_plan, catalogs) = plan_with_catalogs(vec![filename, size]);
  let direct_scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut direct = documents().remove(0);
  let mut alternate = direct.clone();
  direct.file_key = vec![0x10; 32];
  alternate.scope_id = alternate_scope_id;
  alternate.file_key = vec![0x20; 32];
  alternate.revision = vec![0x21; 32];
  alternate.path = "/alternate".to_string();
  let documents = vec![direct, alternate];
  let shared_minimum = minimum_scope_semantic_bytes(&shared_plan, &catalogs, &direct_scope_id, &documents);
  let (single_plan, single_catalogs) = plan(CoverageFixture::Authoritative);
  let single_minimum = minimum_scope_semantic_bytes(&single_plan, &single_catalogs, &direct_scope_id, &documents[..1]);
  assert_eq!(shared_minimum, single_minimum, "sibling scopes must not consume selected-scope admission budget");
}

#[test]
fn execution_limits_fail_before_unbounded_work_and_release_every_reservation() {
  assert_eq!(QueryExecutionCountLimitsV1::new(0, 1, 1, 1).unwrap_err().class(), QueryExecutionErrorClassV1::InvalidRequest);
  assert_eq!(QueryExecutionByteLimitsV1::new(1, 1, 0).unwrap_err().class(), QueryExecutionErrorClassV1::InvalidRequest);

  let (plan, catalogs) = plan(CoverageFixture::Authoritative);
  let cases = [
    (
      QueryExecutionLimitsV1::new(
        QueryExecutionCountLimitsV1::new(100, 100, 1, 1_000_000).unwrap(),
        QueryExecutionByteLimitsV1::new(1 << 20, 8 << 20, 16 << 20).unwrap(),
      ),
      "query_execution_match_limit",
    ),
    (
      QueryExecutionLimitsV1::new(
        QueryExecutionCountLimitsV1::new(100, 100, 100, 1).unwrap(),
        QueryExecutionByteLimitsV1::new(1 << 20, 8 << 20, 16 << 20).unwrap(),
      ),
      "query_execution_work_limit",
    ),
    (
      QueryExecutionLimitsV1::new(
        QueryExecutionCountLimitsV1::new(1, 100, 100, 1_000_000).unwrap(),
        QueryExecutionByteLimitsV1::new(1 << 20, 8 << 20, 16 << 20).unwrap(),
      ),
      "query_execution_document_limit",
    ),
    (
      QueryExecutionLimitsV1::new(
        QueryExecutionCountLimitsV1::new(100, 100, 100, 1_000_000).unwrap(),
        QueryExecutionByteLimitsV1::new(1 << 20, 8 << 20, 1).unwrap(),
      ),
      "query_execution_semantic_memory",
    ),
  ];
  for (limits, expected_code) in cases {
    let memory = memory(64 << 20);
    let mut feed = ScopeFeed {
      root: ROOT.to_vec(),
      publication_sequence: 41,
      documents: documents(),
      complete: true,
      receipt_count_delta: 0,
      source_error: None,
      cancel_after_documents: None,
      field_reads: Cell::new(0),
    };
    let error = execute_with_limits(&plan, &catalogs, &mut feed, &memory, limits).unwrap_err();
    assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit);
    assert_eq!(error.code(), expected_code);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }

  for operation in [
    QueryPredicateOperationV1::Contains(CanonicalConfigValueV1::String("zzzz".to_string())),
    QueryPredicateOperationV1::Fuzzy {
      value: CanonicalConfigValueV1::String("marhta".to_string()),
      algorithm: QueryFuzzyAlgorithmV1::JaroWinkler,
      edits: None,
    },
  ] {
    let (plan, catalogs) = single_predicate_plan(operation, "unicode_trigram_v1");
    let memory = memory(64 << 20);
    let mut feed = ScopeFeed {
      root: ROOT.to_vec(),
      publication_sequence: 41,
      documents: single_document(CanonicalConfigValueV1::String("martha".to_string())),
      complete: true,
      receipt_count_delta: 0,
      source_error: None,
      cancel_after_documents: None,
      field_reads: Cell::new(0),
    };
    let limits = QueryExecutionLimitsV1::new(
      QueryExecutionCountLimitsV1::new(100, 100, 100, 5).unwrap(),
      QueryExecutionByteLimitsV1::new(1 << 20, 8 << 20, 16 << 20).unwrap(),
    );
    let error = execute_with_limits(&plan, &catalogs, &mut feed, &memory, limits).unwrap_err();
    assert_eq!(error.code(), "query_execution_work_limit");
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn retained_result_limit_stops_the_authoritative_stream_before_later_documents() {
  let (plan, catalogs) =
    single_predicate_plan(QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha".to_string())), "typed_exact_blake3_v1");
  let scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut documents = Vec::new();
  for identity in [0x10, 0x20, 0x30] {
    let mut fields = BTreeMap::new();
    fields.insert("@filename".to_string(), FieldFixture::Values(vec![canonical(CanonicalConfigValueV1::String("alpha".to_string()))]));
    documents.push(DocumentFixture {
      scope_id: scope_id.clone(),
      file_key: vec![identity; 32],
      revision: vec![identity + 1; 32],
      path: format!("/{}", "a".repeat(300)),
      fields,
    });
  }
  let memory = memory(64 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents,
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let limits = QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(100, 100, 3, 1_000_000).unwrap(),
    QueryExecutionByteLimitsV1::new(1 << 20, 5_000, 16 << 20).unwrap(),
  );
  let error = execute_with_limits(&plan, &catalogs, &mut feed, &memory, limits).unwrap_err();
  assert_eq!(error.code(), "query_execution_retained_bytes");
  assert_eq!(feed.field_reads.get(), 2, "the row that exhausts retained memory must stop later source work");
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn incomplete_dishonest_unindexable_cancelled_and_pressured_sources_fail_closed() {
  let (plan, catalogs) = plan(CoverageFixture::Complete);
  let run = |feed: &mut ScopeFeed, memory: &MemoryCoordinator, cancellation: &CancellationToken| {
    execute(&plan, &catalogs, feed, memory, cancellation).unwrap_err()
  };

  let mut incomplete = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: false,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  assert_eq!(run(&mut incomplete, &memory(64 << 20), &CancellationToken::new()).class(), QueryExecutionErrorClassV1::CorruptSource);

  let mut dishonest = ScopeFeed { complete: true, receipt_count_delta: 1, ..incomplete };
  assert_eq!(run(&mut dishonest, &memory(64 << 20), &CancellationToken::new()).class(), QueryExecutionErrorClassV1::CorruptSource);

  let mut unindexable_documents = documents();
  unindexable_documents[0].fields.insert("@size".to_string(), FieldFixture::Unindexable);
  let mut unindexable = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: unindexable_documents,
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  assert_eq!(
    run(&mut unindexable, &memory(64 << 20), &CancellationToken::new()).class(),
    QueryExecutionErrorClassV1::HistoricalViewUnavailable
  );

  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let mut cancelled = ScopeFeed { documents: documents(), ..unindexable };
  assert_eq!(run(&mut cancelled, &memory(64 << 20), &cancellation).class(), QueryExecutionErrorClassV1::Cancelled);

  let constrained = memory(2 << 20);
  let mut pressured = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  assert_eq!(run(&mut pressured, &constrained, &CancellationToken::new()).class(), QueryExecutionErrorClassV1::ResourceLimit);
  assert_eq!(constrained.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn typed_source_failures_preserve_operational_direction() {
  let (plan, catalogs) = plan(CoverageFixture::Authoritative);
  for (source_class, expected) in [
    (QueryExecutionSourceErrorClassV1::Unavailable, QueryExecutionErrorClassV1::HistoricalViewUnavailable),
    (QueryExecutionSourceErrorClassV1::ResourceLimit, QueryExecutionErrorClassV1::ResourceLimit),
    (QueryExecutionSourceErrorClassV1::Corrupt, QueryExecutionErrorClassV1::CorruptSource),
    (QueryExecutionSourceErrorClassV1::Cancelled, QueryExecutionErrorClassV1::Cancelled),
    (QueryExecutionSourceErrorClassV1::Internal, QueryExecutionErrorClassV1::Internal),
  ] {
    let mut feed = ScopeFeed {
      root: ROOT.to_vec(),
      publication_sequence: 41,
      documents: Vec::new(),
      complete: true,
      receipt_count_delta: 0,
      source_error: Some(QueryExecutionSourceErrorV1::new(source_class, "injected_source", "injected failure")),
      cancel_after_documents: None,
      field_reads: Cell::new(0),
    };
    let memory = memory(64 << 20);
    let error = execute(&plan, &catalogs, &mut feed, &memory, &CancellationToken::new()).unwrap_err();
    assert_eq!(error.class(), expected);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }
}

fn single_predicate_plan(
  operation: QueryPredicateOperationV1,
  converter: &str,
) -> (CompiledRootAwareQueryPlanV1, Vec<RootAwareQueryFieldCatalogV1>) {
  let expression = QueryExpressionV1::Field(QueryPredicateV1 { field_name: "@filename".to_string(), operation });
  let catalogs = vec![catalog("@filename", converter, CoverageFixture::Complete)];
  let context =
    QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, HashAlgorithm::Blake3_256, &ROOT, &SEMANTIC_ROOT, 41).unwrap();
  let request = QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 1,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  };
  (plan_root_aware_query_v1(&request).unwrap(), catalogs)
}

fn single_document(value: CanonicalConfigValueV1) -> Vec<DocumentFixture> {
  let scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut fields = BTreeMap::new();
  fields.insert("@filename".to_string(), FieldFixture::Values(vec![canonical(value)]));
  vec![DocumentFixture { scope_id, file_key: vec![0x10; 32], revision: vec![0x11; 32], path: "/one".to_string(), fields }]
}

fn single_predicate_matches(operation: QueryPredicateOperationV1, converter: &str, source: CanonicalConfigValueV1) -> bool {
  let (plan, catalogs) = single_predicate_plan(operation, converter);
  let semantic_memory = memory(64 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: single_document(source),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let result = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
    plan: &plan,
    catalogs: &catalogs,
    source: &mut feed,
    memory: &semantic_memory,
    cancellation: &CancellationToken::new(),
    limits: limits(),
  })
  .unwrap();
  result.matches().len() == 1
}

#[test]
fn every_frozen_predicate_family_rechecks_complete_canonical_values() {
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha".to_string())),
    "typed_exact_blake3_v1",
    CanonicalConfigValueV1::String("alpha".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::In(vec![
      CanonicalConfigValueV1::String("beta".to_string()),
      CanonicalConfigValueV1::String("alpha".to_string()),
    ]),
    "typed_exact_blake3_v1",
    CanonicalConfigValueV1::String("alpha".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Unsigned(5)),
    "u64_order_v1",
    CanonicalConfigValueV1::Unsigned(7),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Lt(CanonicalConfigValueV1::Unsigned(10)),
    "u64_order_v1",
    CanonicalConfigValueV1::Unsigned(7),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Between(CanonicalConfigValueV1::Unsigned(7), CanonicalConfigValueV1::Unsigned(7)),
    "u64_order_v1",
    CanonicalConfigValueV1::Unsigned(7),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Contains(CanonicalConfigValueV1::String("café".to_string())),
    "unicode_trigram_v1",
    CanonicalConfigValueV1::String("A CAFÉ table".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Similar { value: CanonicalConfigValueV1::String("hello world".to_string()), threshold: 0.8 },
    "unicode_trigram_v1",
    CanonicalConfigValueV1::String("hello world".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Phonetic(CanonicalConfigValueV1::String("Rupert".to_string())),
    "soundex_ascii_v1",
    CanonicalConfigValueV1::String("Robert".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Phonetic(CanonicalConfigValueV1::String("Smith".to_string())),
    "double_metaphone_primary_ascii_v1",
    CanonicalConfigValueV1::String("Smythe".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Phonetic(CanonicalConfigValueV1::String("Schmidt".to_string())),
    "double_metaphone_alt_ascii_v1",
    CanonicalConfigValueV1::String("Schmidt".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Fuzzy {
      value: CanonicalConfigValueV1::String("abcd".to_string()),
      algorithm: QueryFuzzyAlgorithmV1::DamerauLevenshtein,
      edits: Some(1),
    },
    "unicode_trigram_v1",
    CanonicalConfigValueV1::String("abdc".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Fuzzy {
      value: CanonicalConfigValueV1::String("martha".to_string()),
      algorithm: QueryFuzzyAlgorithmV1::JaroWinkler,
      edits: None,
    },
    "unicode_trigram_v1",
    CanonicalConfigValueV1::String("marhta".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Match(CanonicalConfigValueV1::String("kitten".to_string())),
    "unicode_trigram_v1",
    CanonicalConfigValueV1::String("sitten".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Match(CanonicalConfigValueV1::String("Smith".to_string())),
    "soundex_ascii_v1",
    CanonicalConfigValueV1::String("Saaaaaaaaaaaaaaaaaamith".to_string()),
  ));
  assert!(!single_predicate_matches(
    QueryPredicateOperationV1::Between(CanonicalConfigValueV1::Unsigned(8), CanonicalConfigValueV1::Unsigned(10)),
    "u64_order_v1",
    CanonicalConfigValueV1::Unsigned(7),
  ));
  assert!(!single_predicate_matches(
    QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("beta".to_string())),
    "typed_exact_blake3_v1",
    CanonicalConfigValueV1::String("alpha".to_string()),
  ));
  assert!(single_predicate_matches(
    QueryPredicateOperationV1::Contains(CanonicalConfigValueV1::String(String::new())),
    "unicode_trigram_v1",
    CanonicalConfigValueV1::String("anything".to_string()),
  ));
  assert!(!single_predicate_matches(
    QueryPredicateOperationV1::Contains(CanonicalConfigValueV1::String("needle".to_string())),
    "unicode_trigram_v1",
    CanonicalConfigValueV1::String("haystack".to_string()),
  ));
  assert!(!single_predicate_matches(
    QueryPredicateOperationV1::Similar { value: CanonicalConfigValueV1::String("xyz".to_string()), threshold: 1.0 },
    "unicode_trigram_v1",
    CanonicalConfigValueV1::String("abc".to_string()),
  ));
  assert!(!single_predicate_matches(
    QueryPredicateOperationV1::Phonetic(CanonicalConfigValueV1::String("Rupert".to_string())),
    "soundex_ascii_v1",
    CanonicalConfigValueV1::String("Jackson".to_string()),
  ));
  assert!(!single_predicate_matches(
    QueryPredicateOperationV1::Fuzzy {
      value: CanonicalConfigValueV1::String("kitten".to_string()),
      algorithm: QueryFuzzyAlgorithmV1::DamerauLevenshtein,
      edits: Some(1),
    },
    "unicode_trigram_v1",
    CanonicalConfigValueV1::String("dog".to_string()),
  ));
  assert!(!single_predicate_matches(
    QueryPredicateOperationV1::Match(CanonicalConfigValueV1::String("kitten".to_string())),
    "unicode_trigram_v1",
    CanonicalConfigValueV1::String("dog".to_string()),
  ));
}

#[test]
fn sources_cannot_squelch_field_or_document_visitor_failures() {
  let (plan, catalogs) = plan(CoverageFixture::Complete);
  let mut malformed = documents();
  malformed[0].fields.insert("@size".to_string(), FieldFixture::SquelchedValues(vec![vec![0xff]]));
  let field_memory = memory(64 << 20);
  let mut field_feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: malformed,
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let error = execute(&plan, &catalogs, &mut field_feed, &field_memory, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
  assert_eq!(error.code(), "query_execution_canonical_value");
  assert_eq!(field_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let (plan, catalogs) =
    single_predicate_plan(QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha".to_string())), "typed_exact_blake3_v1");
  let scope_id = decode_scope_definition(&scope_fixture(), HashAlgorithm::Blake3_256).unwrap().scope_id;
  let mut matching = Vec::new();
  for identity in [0x10, 0x20, 0x30] {
    let mut fields = BTreeMap::new();
    fields.insert("@filename".to_string(), FieldFixture::Values(vec![canonical(CanonicalConfigValueV1::String("alpha".to_string()))]));
    matching.push(DocumentFixture {
      scope_id: scope_id.clone(),
      file_key: vec![identity; 32],
      revision: vec![identity + 1; 32],
      path: format!("/{}", "a".repeat(300)),
      fields,
    });
  }
  let result_memory = memory(64 << 20);
  let mut document_feed = SquelchingScopeFeed {
    inner: ScopeFeed {
      root: ROOT.to_vec(),
      publication_sequence: 41,
      documents: matching,
      complete: true,
      receipt_count_delta: 0,
      source_error: None,
      cancel_after_documents: None,
      field_reads: Cell::new(0),
    },
  };
  let limits = QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(100, 100, 3, 1_000_000).unwrap(),
    QueryExecutionByteLimitsV1::new(1 << 20, 5_000, 16 << 20).unwrap(),
  );
  let error = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
    plan: &plan,
    catalogs: &catalogs,
    source: &mut document_feed,
    memory: &result_memory,
    cancellation: &CancellationToken::new(),
    limits,
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_execution_retained_bytes");
  assert_eq!(result_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn malformed_rows_field_receipts_and_midstream_cancellation_release_memory() {
  let (plan, catalogs) = plan(CoverageFixture::Complete);
  let assert_corrupt = |documents: Vec<DocumentFixture>| {
    let memory = memory(64 << 20);
    let mut feed = ScopeFeed {
      root: ROOT.to_vec(),
      publication_sequence: 41,
      documents,
      complete: true,
      receipt_count_delta: 0,
      source_error: None,
      cancel_after_documents: None,
      field_reads: Cell::new(0),
    };
    let error = execute(&plan, &catalogs, &mut feed, &memory, &CancellationToken::new()).unwrap_err();
    assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  };

  let mut malformed = documents();
  malformed[0].fields.insert("@size".to_string(), FieldFixture::Values(vec![vec![0xff]]));
  assert_corrupt(malformed);

  let mut noncanonical_path = documents();
  noncanonical_path[0].path = "//alpha".to_string();
  assert_corrupt(noncanonical_path);

  let mut duplicate = documents();
  duplicate[1].file_key = duplicate[0].file_key.clone();
  assert_corrupt(duplicate);

  let mut incomplete = documents();
  incomplete[0].fields.insert("@size".to_string(), FieldFixture::Incomplete(vec![canonical(CanonicalConfigValueV1::Unsigned(7))]));
  assert_corrupt(incomplete);

  let mut dishonest = documents();
  dishonest[0].fields.insert("@size".to_string(), FieldFixture::Dishonest(vec![canonical(CanonicalConfigValueV1::Unsigned(7))]));
  assert_corrupt(dishonest);

  for (root, publication_sequence) in [(vec![0x99; 32], 41), (ROOT.to_vec(), 42)] {
    let memory = memory(64 << 20);
    let mut feed = ScopeFeed {
      root,
      publication_sequence,
      documents: documents(),
      complete: true,
      receipt_count_delta: 0,
      source_error: None,
      cancel_after_documents: None,
      field_reads: Cell::new(0),
    };
    let error = execute(&plan, &catalogs, &mut feed, &memory, &CancellationToken::new()).unwrap_err();
    assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
    assert_eq!(error.code(), "query_execution_scope_receipt");
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }

  let cancellation = CancellationToken::new();
  let cancellation_memory = memory(64 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: Some(1),
    field_reads: Cell::new(0),
  };
  let error = execute(&plan, &catalogs, &mut feed, &cancellation_memory, &cancellation).unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::Cancelled);
  assert_eq!(cancellation_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let (text_plan, text_catalogs) =
    single_predicate_plan(QueryPredicateOperationV1::Contains(CanonicalConfigValueV1::String("7".to_string())), "unicode_trigram_v1");
  let unindexable_memory = memory(64 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: single_document(CanonicalConfigValueV1::Unsigned(7)),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let error = execute(&text_plan, &text_catalogs, &mut feed, &unindexable_memory, &CancellationToken::new()).unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::HistoricalViewUnavailable);
  assert_eq!(error.code(), "query_execution_document_unindexable");
  assert_eq!(unindexable_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn successful_results_retain_query_memory_until_drop() {
  let (plan, catalogs) = plan(CoverageFixture::Authoritative);
  let memory = memory(64 << 20);
  let mut feed = ScopeFeed {
    root: ROOT.to_vec(),
    publication_sequence: 41,
    documents: documents(),
    complete: true,
    receipt_count_delta: 0,
    source_error: None,
    cancel_after_documents: None,
    field_reads: Cell::new(0),
  };
  let cancellation = CancellationToken::new();
  let result = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
    plan: &plan,
    catalogs: &catalogs,
    source: &mut feed,
    memory: &memory,
    cancellation: &cancellation,
    limits: limits(),
  })
  .unwrap();
  assert_eq!(result.matches().len(), 2);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, result.retained_bytes());
  drop(result);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

fn adjusted_count(value: u64, delta: i64) -> u64 {
  if delta >= 0 {
    value + delta as u64
  } else {
    value - delta.unsigned_abs()
  }
}
