use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coverage_planner::IndexSemanticQueryAvailabilityV1;
use aeordb::engine::v4::position::{PositionComparatorV1, PositionComponentStateV1, PositionRouteV1};
use aeordb::engine::v4::position_order::{LogicalOrderComponentOwnedV1, LogicalOrderRowOwnedV1};
use aeordb::engine::v4::position_resolver::{
  PositionUniverseLookupRequestV1, PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1, PositionUniverseSourceV1,
};
use aeordb::engine::v4::query_aggregate_execution::{
  CompiledQueryAggregateInputV1, QueryAggregateInputFieldV1, QueryAggregateInputLimitsV1, QueryAggregateInputLookupRequestV1,
  QueryAggregateInputLookupResultV1, QueryAggregateInputRowV1, QueryAggregateInputSourceV1, QueryAggregateNumericV1,
  QueryAggregateReducedValueRefV1, QueryGroupedAggregateLimitsV1, QueryGroupedAggregateSinkV1,
};
use aeordb::engine::v4::query_executor::{
  QueryAuthoritativeFieldPartitionCursorV1, QueryAuthoritativeFieldPartitionSourceV1, QueryExecutionByteLimitsV1,
  QueryExecutionCountLimitsV1, QueryExecutionErrorClassV1, QueryExecutionErrorOriginV1, QueryExecutionFieldDocumentV1,
  QueryExecutionFieldPartitionOpenRequestV1, QueryExecutionFieldPartitionReceiptV1, QueryExecutionFieldStateV1, QueryExecutionLimitsV1,
  QueryExecutionSourceErrorClassV1, QueryExecutionSourceErrorV1, RootAwarePartitionedQueryExecutionRequestV1,
  execute_authoritative_partitioned_query_into_v1,
};
use aeordb::engine::v4::query_order_execution::{QueryOrderedTopKLimitsV1, QueryOrderedTopKSinkV1};
use aeordb::engine::v4::query_planner::{
  CompiledRootAwareQueryPlanV1, QueryAggregateFieldV1, QueryAggregateKindV1, QueryExpressionV1, QueryPlanningContextV1,
  QueryPlanningIndexCandidateV1, QueryPlanningIndexEstimatesV1, QueryPlanningRequestV1, QueryPlanningScopeV1, QueryPredicateOperationV1,
  QueryPredicateV1, RootAwareQueryFieldCatalogV1, default_query_planning_limits_v1, plan_root_aware_query_v1,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x11; 16];
const PHYSICAL_INSTANCE_ID: [u8; 16] = [0x22; 16];
const PUBLICATION_SEQUENCE: u64 = 41;
const RESULT_LIMIT: usize = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PartitionShape {
  Shared,
  Nonidentical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PartitionFault {
  None,
  DuplicateFilenameIdentity,
  MissingFilenameIdentity,
  DishonestCreatedAtReceipt,
  CancelDuringCreatedAt,
}

#[derive(Clone)]
struct ModelField {
  state: QueryExecutionFieldStateV1,
  values: Vec<LogicalOrderComponentOwnedV1>,
}

#[derive(Clone)]
struct ModelDocument {
  file_key: Vec<u8>,
  revision: Vec<u8>,
  path: String,
  filename: String,
  created_at: i64,
  size: ModelField,
  updated_at: ModelField,
}

struct PipelineFixture {
  plan: CompiledRootAwareQueryPlanV1,
  catalogs: Vec<RootAwareQueryFieldCatalogV1>,
  documents: Vec<ModelDocument>,
  direct_scope_id: Vec<u8>,
  alternate_scope_id: Vec<u8>,
  shape: PartitionShape,
}

struct PartitionSource {
  root: Vec<u8>,
  documents: Vec<ModelDocument>,
  field_scopes: BTreeMap<String, Vec<Vec<u8>>>,
  fault: PartitionFault,
}

struct PartitionCursor {
  root: Vec<u8>,
  field_name: String,
  requested_scope_ids: Vec<Vec<u8>>,
  rows: Vec<QueryExecutionFieldDocumentV1>,
  next_index: usize,
  dishonest_receipt: bool,
  cancel_at: Option<usize>,
}

struct ModelPositionSource {
  algorithm: HashAlgorithm,
  root: Vec<u8>,
  rows: BTreeMap<(Vec<u8>, Vec<u8>), String>,
  calls: usize,
}

struct ModelAggregateSource {
  root: Vec<u8>,
  rows: BTreeMap<(Vec<u8>, Vec<u8>), ModelDocument>,
  calls: usize,
}

#[derive(Default)]
struct ExpectedGroup {
  document_count: u64,
  unsigned_values: Vec<u64>,
  signed_values: Vec<i64>,
}

fn fixture(path: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("query result pipeline fixture does not cover {algorithm:?}"),
  }
}

fn metadata_id(field_name: &str) -> u16 {
  match field_name {
    "@filename" => 2,
    "@size" => 5,
    "@created_at" => 6,
    "@updated_at" => 7,
    _ => panic!("unsupported metadata fixture field {field_name}"),
  }
}

fn definitions(algorithm: HashAlgorithm, field_name: &str, comparator: &str, encoded_scope: &[u8]) -> (Vec<u8>, Vec<u8>) {
  let algorithm_name = algorithm_name(algorithm);
  let scope = decode_scope_definition(encoded_scope, algorithm).unwrap();
  let mut value_store = fixture(&format!("value-store-definition-v1/avst-{algorithm_name}-metadata-hash-corrected-valid.bin"));
  let hash_width = algorithm.hash_length();
  value_store[32..32 + hash_width].copy_from_slice(&scope.scope_id);
  let fixed_start = 32 + hash_width;
  let field_start = fixed_start + 80;
  let old_field_length = u32::from_le_bytes(value_store[fixed_start..fixed_start + 4].try_into().unwrap()) as usize;
  value_store.splice(field_start..field_start + old_field_length, field_name.as_bytes().iter().copied());
  let value_store_length = value_store.len() as u32;
  value_store[8..12].copy_from_slice(&value_store_length.to_le_bytes());
  value_store[fixed_start..fixed_start + 4].copy_from_slice(&(field_name.len() as u32).to_le_bytes());
  let selector_start = field_start + field_name.len();
  value_store[selector_start + 32..selector_start + 34].copy_from_slice(&metadata_id(field_name).to_le_bytes());
  let value_definition = decode_value_store_definition(&value_store, algorithm).unwrap();

  let mut field = fixture(&format!("field-index-definition-v1/afix-{algorithm_name}-{comparator}-valid.bin"));
  field[32..32 + hash_width].copy_from_slice(&value_definition.value_store_id);
  decode_field_index_definition(&field, algorithm).unwrap();
  (value_store, field)
}

fn catalog(
  algorithm: HashAlgorithm,
  root: &[u8],
  semantic_root: &[u8],
  field_name: &str,
  comparator: &str,
  encoded_scopes: Vec<Vec<u8>>,
) -> RootAwareQueryFieldCatalogV1 {
  let mut scopes = encoded_scopes
    .into_iter()
    .map(|encoded_scope| {
      let scope_definition = decode_scope_definition(&encoded_scope, algorithm).unwrap();
      let (value_store, field) = definitions(algorithm, field_name, comparator, &encoded_scope);
      let value_definition = decode_value_store_definition(&value_store, algorithm).unwrap();
      let field_definition = decode_field_index_definition(&field, algorithm).unwrap();
      QueryPlanningScopeV1 {
        scope_id: scope_definition.scope_id,
        value_store_id: value_definition.value_store_id,
        encoded_scope_definition: encoded_scope,
        encoded_value_store_definition: value_store,
        semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
        authoritative_document_count: 128,
        indexes: vec![QueryPlanningIndexCandidateV1 {
          index_id: field_definition.index_id,
          encoded_field_definition: field,
          selected_generation: None,
          estimates: QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap(),
          nvt_hint_available: false,
        }],
      }
    })
    .collect::<Vec<_>>();
  scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  RootAwareQueryFieldCatalogV1 {
    database_id: DATABASE_ID,
    physical_instance_id: PHYSICAL_INSTANCE_ID,
    selected_namespace_root: root.to_vec(),
    semantic_state_root: semantic_root.to_vec(),
    publication_sequence: PUBLICATION_SEQUENCE,
    field_name: field_name.to_string(),
    complete: true,
    scopes,
  }
}

fn component_u64(value: u64) -> LogicalOrderComponentOwnedV1 {
  LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, value.to_le_bytes().to_vec())
}

fn component_i64(value: i64) -> LogicalOrderComponentOwnedV1 {
  LogicalOrderComponentOwnedV1::present(PositionComparatorV1::I64, value.to_le_bytes().to_vec())
}

fn documents(algorithm: HashAlgorithm) -> Vec<ModelDocument> {
  let mut documents = (0..48usize)
    .map(|index| {
      let path = format!("/docs/{:02}-item.json", 47 - index);
      let file_key = digest_parts(algorithm, &[b"file:", path.as_bytes()]);
      let revision = vec![(index as u8).wrapping_add(0x40); algorithm.hash_length()];
      let filename = if index % 11 == 0 { "drop".to_string() } else { format!("item-{index:02}") };
      let created_at = if index % 13 == 0 { 0 } else { index as i64 + 1 };

      let size_state = if index % 9 == 0 {
        QueryExecutionFieldStateV1::Missing
      } else if index % 10 == 0 {
        QueryExecutionFieldStateV1::DeterministicUnindexable
      } else {
        QueryExecutionFieldStateV1::Values
      };
      let mut size_values = Vec::new();
      if size_state == QueryExecutionFieldStateV1::Values {
        let primary = if index == 1 { u64::MAX } else { ((index * 17) % 23) as u64 };
        size_values.push(component_u64(primary));
        if index % 3 == 0 {
          size_values.push(LogicalOrderComponentOwnedV1::typed_null());
        }
        if index % 5 == 0 {
          size_values.push(component_u64(primary));
        }
        if index % 7 == 0 {
          size_values.push(component_u64(primary.saturating_add(100)));
        }
      }

      let updated_state = if index % 8 == 0 {
        QueryExecutionFieldStateV1::Missing
      } else if index % 12 == 0 {
        QueryExecutionFieldStateV1::DeterministicUnindexable
      } else {
        QueryExecutionFieldStateV1::Values
      };
      let mut updated_values = Vec::new();
      if updated_state == QueryExecutionFieldStateV1::Values {
        let primary = match index {
          1 => i64::MIN,
          2 => i64::MAX,
          _ => (index as i64 % 9) - 4,
        };
        updated_values.push(component_i64(primary));
        if index % 4 == 0 {
          updated_values.push(LogicalOrderComponentOwnedV1::typed_null());
        }
        if index % 6 == 0 {
          updated_values.push(component_i64(primary.saturating_sub(20)));
        }
      }

      ModelDocument {
        file_key,
        revision,
        path,
        filename,
        created_at,
        size: ModelField { state: size_state, values: size_values },
        updated_at: ModelField { state: updated_state, values: updated_values },
      }
    })
    .collect::<Vec<_>>();
  documents.sort_by(|left, right| left.file_key.cmp(&right.file_key));
  documents
}

fn pipeline_fixture(algorithm: HashAlgorithm, shape: PartitionShape) -> PipelineFixture {
  let algorithm_name = algorithm_name(algorithm);
  let root = vec![0x33; algorithm.hash_length()];
  let semantic_root = vec![0x44; algorithm.hash_length()];
  let direct = fixture(&format!("scope-definition-v1/ascp-{algorithm_name}-root-direct-valid.bin"));
  let alternate = fixture(&format!("scope-definition-v1/ascp-{algorithm_name}-normalized-glob-valid.bin"));
  let direct_scope_id = decode_scope_definition(&direct, algorithm).unwrap().scope_id;
  let alternate_scope_id = decode_scope_definition(&alternate, algorithm).unwrap().scope_id;
  let created_scopes = match shape {
    PartitionShape::Shared => vec![direct.clone()],
    PartitionShape::Nonidentical => vec![direct.clone(), alternate],
  };
  let catalogs = vec![
    catalog(algorithm, &root, &semantic_root, "@filename", "typed_exact_blake3_v1", vec![direct.clone()]),
    catalog(algorithm, &root, &semantic_root, "@created_at", "i64_order_v1", created_scopes),
    catalog(algorithm, &root, &semantic_root, "@size", "u64_order_v1", vec![direct.clone()]),
    catalog(algorithm, &root, &semantic_root, "@updated_at", "i64_order_v1", vec![direct]),
  ];
  let expression = QueryExpressionV1::And(vec![
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@created_at".to_string(),
      operation: QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Signed(0)),
    }),
    QueryExpressionV1::Not(Box::new(QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@filename".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("drop".to_string())),
    }))),
  ]);
  let aggregate_fields = ["@size", "@updated_at"]
    .into_iter()
    .flat_map(|field_name| {
      [
        QueryAggregateKindV1::Count,
        QueryAggregateKindV1::Sum,
        QueryAggregateKindV1::Average,
        QueryAggregateKindV1::Minimum,
        QueryAggregateKindV1::Maximum,
      ]
      .into_iter()
      .map(move |kind| QueryAggregateFieldV1 { field_name: field_name.to_string(), kind })
    })
    .collect::<Vec<_>>();
  let group_fields = ["@updated_at".to_string(), "@size".to_string()];
  let context =
    QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &root, &semantic_root, PUBLICATION_SEQUENCE).unwrap();
  let plan = plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &aggregate_fields,
    group_fields: &group_fields,
    result_limit: RESULT_LIMIT,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap();
  let execution_catalogs =
    catalogs.into_iter().filter(|catalog| matches!(catalog.field_name.as_str(), "@created_at" | "@filename")).collect();
  PipelineFixture { plan, catalogs: execution_catalogs, documents: documents(algorithm), direct_scope_id, alternate_scope_id, shape }
}

fn canonical(value: CanonicalConfigValueV1) -> Vec<u8> {
  encode_canonical_value(&value, CanonicalValueBounds::SOURCE_VALUE).unwrap()
}

impl PartitionSource {
  fn new(fixture: &PipelineFixture, fault: PartitionFault) -> Self {
    let direct = fixture.direct_scope_id.clone();
    let created = fixture
      .documents
      .iter()
      .enumerate()
      .map(|(index, _)| match fixture.shape {
        PartitionShape::Shared => direct.clone(),
        PartitionShape::Nonidentical if index % 2 == 1 => fixture.alternate_scope_id.clone(),
        PartitionShape::Nonidentical => direct.clone(),
      })
      .collect::<Vec<_>>();
    Self {
      root: fixture.plan.selected_namespace_root().to_vec(),
      documents: fixture.documents.clone(),
      field_scopes: BTreeMap::from([
        ("@created_at".to_string(), created),
        ("@filename".to_string(), vec![direct; fixture.documents.len()]),
      ]),
      fault,
    }
  }
}

impl QueryAuthoritativeFieldPartitionSourceV1 for PartitionSource {
  fn open_field_partition(
    &mut self,
    request: QueryExecutionFieldPartitionOpenRequestV1<'_>,
  ) -> Result<Box<dyn QueryAuthoritativeFieldPartitionCursorV1>, QueryExecutionSourceErrorV1> {
    assert_eq!(request.selected_namespace_root, self.root);
    assert_eq!(request.publication_sequence, PUBLICATION_SEQUENCE);
    assert_eq!(request.query_path, "/");
    let requested_scope_ids = request.scope_ids.iter().map(|scope_id| scope_id.to_vec()).collect::<Vec<_>>();
    let assignments = self.field_scopes.get(request.field_name).expect("predicate field scope assignments");
    let mut rows = self
      .documents
      .iter()
      .zip(assignments)
      .map(|(document, scope_id)| {
        let canonical_values = match request.field_name {
          "@created_at" => vec![canonical(CanonicalConfigValueV1::Signed(document.created_at))],
          "@filename" => vec![canonical(CanonicalConfigValueV1::String(document.filename.clone()))],
          other => panic!("unexpected partition field {other}"),
        };
        QueryExecutionFieldDocumentV1 {
          scope_id: scope_id.clone(),
          file_key: document.file_key.clone(),
          record_revision: document.revision.clone(),
          path: document.path.clone(),
          state: QueryExecutionFieldStateV1::Values,
          canonical_values,
        }
      })
      .collect::<Vec<_>>();
    if request.field_name == "@filename" {
      match self.fault {
        PartitionFault::DuplicateFilenameIdentity => rows.insert(1, rows[0].clone()),
        PartitionFault::MissingFilenameIdentity => {
          rows.remove(1);
        }
        _ => {}
      }
    }
    let dishonest_receipt = request.field_name == "@created_at" && self.fault == PartitionFault::DishonestCreatedAtReceipt;
    let cancel_at = (request.field_name == "@created_at" && self.fault == PartitionFault::CancelDuringCreatedAt).then_some(8);
    Ok(Box::new(PartitionCursor {
      root: self.root.clone(),
      field_name: request.field_name.to_string(),
      requested_scope_ids,
      rows,
      next_index: 0,
      dishonest_receipt,
      cancel_at,
    }))
  }
}

impl QueryAuthoritativeFieldPartitionCursorV1 for PartitionCursor {
  fn next_document(
    &mut self,
    cancellation: &CancellationToken,
  ) -> Result<Option<QueryExecutionFieldDocumentV1>, QueryExecutionSourceErrorV1> {
    if self.cancel_at == Some(self.next_index) {
      cancellation.cancel();
    }
    if cancellation.is_cancelled() {
      return Err(QueryExecutionSourceErrorV1::new(
        QueryExecutionSourceErrorClassV1::Cancelled,
        "fixture_pipeline_cancelled",
        "partition pipeline fixture was cancelled",
      ));
    }
    let row = self.rows.get(self.next_index).cloned();
    self.next_index += usize::from(row.is_some());
    Ok(row)
  }

  fn finish(&mut self) -> Result<QueryExecutionFieldPartitionReceiptV1, QueryExecutionSourceErrorV1> {
    assert_eq!(self.next_index, self.rows.len(), "executor finished a live partition cursor");
    let mut scope_document_counts = self
      .requested_scope_ids
      .iter()
      .map(|scope_id| self.rows.iter().filter(|row| row.scope_id == *scope_id).count() as u64)
      .collect::<Vec<_>>();
    if self.dishonest_receipt {
      scope_document_counts[0] += 1;
    }
    Ok(QueryExecutionFieldPartitionReceiptV1 {
      selected_namespace_root: self.root.clone(),
      publication_sequence: PUBLICATION_SEQUENCE,
      field_name: self.field_name.clone(),
      scope_ids: self.requested_scope_ids.clone(),
      scope_document_counts,
      document_count: self.rows.len() as u64,
      complete: true,
    })
  }
}

impl ModelPositionSource {
  fn new(fixture: &PipelineFixture) -> Self {
    Self {
      algorithm: fixture.plan.hash_algorithm(),
      root: fixture.plan.selected_namespace_root().to_vec(),
      rows: fixture
        .documents
        .iter()
        .map(|document| ((document.file_key.clone(), document.revision.clone()), document.path.clone()))
        .collect(),
      calls: 0,
    }
  }
}

impl PositionUniverseSourceV1 for ModelPositionSource {
  fn resolve_position(
    &mut self,
    request: PositionUniverseLookupRequestV1<'_>,
    _cancellation: &CancellationToken,
  ) -> Result<PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1> {
    self.calls += 1;
    assert_eq!(request.database_id(), DATABASE_ID);
    assert_eq!(request.physical_instance_id(), PHYSICAL_INSTANCE_ID);
    assert_eq!(request.selected_root(), self.root);
    assert_eq!(request.order().hash_algorithm(), self.algorithm);
    assert_eq!(request.route(), PositionRouteV1::Query);
    let Some(path) = self.rows.get(&(request.file_key_tie().to_vec(), request.record_revision_tie().to_vec())) else {
      return Ok(PositionUniverseLookupResultV1::Absent);
    };
    Ok(PositionUniverseLookupResultV1::Found(LogicalOrderRowOwnedV1 {
      route: PositionRouteV1::Query,
      components: vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::Utf8Binary, path.as_bytes().to_vec())],
      file_key_tie: request.file_key_tie().to_vec(),
      record_revision_tie: request.record_revision_tie().to_vec(),
    }))
  }
}

impl ModelAggregateSource {
  fn new(fixture: &PipelineFixture) -> Self {
    Self {
      root: fixture.plan.selected_namespace_root().to_vec(),
      rows: fixture.documents.iter().map(|document| ((document.file_key.clone(), document.revision.clone()), document.clone())).collect(),
      calls: 0,
    }
  }
}

impl QueryAggregateInputSourceV1 for ModelAggregateSource {
  fn resolve_aggregate_input(
    &mut self,
    request: QueryAggregateInputLookupRequestV1<'_>,
    _cancellation: &CancellationToken,
  ) -> Result<QueryAggregateInputLookupResultV1, QueryExecutionSourceErrorV1> {
    self.calls += 1;
    assert_eq!(request.database_id(), DATABASE_ID);
    assert_eq!(request.physical_instance_id(), PHYSICAL_INSTANCE_ID);
    assert_eq!(request.selected_namespace_root(), self.root);
    let Some(document) = self.rows.get(&(request.file_key().to_vec(), request.record_revision().to_vec())) else {
      return Ok(QueryAggregateInputLookupResultV1::Absent);
    };
    let fields = request
      .fields()
      .iter()
      .map(|definition| {
        let model = match definition.field_name() {
          "@size" => &document.size,
          "@updated_at" => &document.updated_at,
          other => panic!("unexpected aggregate input field {other}"),
        };
        QueryAggregateInputFieldV1 {
          field_name: definition.field_name().to_string(),
          scope_id: definition.scope_ids()[0].clone(),
          state: model.state,
          values: model.values.clone(),
        }
      })
      .collect();
    Ok(QueryAggregateInputLookupResultV1::Found(QueryAggregateInputRowV1 {
      selected_namespace_root: self.root.clone(),
      file_key: document.file_key.clone(),
      record_revision: document.revision.clone(),
      fields,
    }))
  }
}

fn execution_limits() -> QueryExecutionLimitsV1 {
  QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(256, 1_024, 256, 2_000_000).unwrap(),
    QueryExecutionByteLimitsV1::new(1 << 20, 8 << 20, 16 << 20).unwrap(),
  )
}

fn memory(hard_limit: u64) -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(hard_limit - (1 << 20), hard_limit, 1, 1 << 20).unwrap())
}

fn matches(document: &ModelDocument) -> bool {
  document.created_at > 0 && document.filename != "drop"
}

fn independent_comparator_tag(comparator: PositionComparatorV1) -> u16 {
  match comparator {
    PositionComparatorV1::BytesBinary => 2,
    PositionComparatorV1::Utf8Binary => 3,
    PositionComparatorV1::U64 => 4,
    PositionComparatorV1::I64 => 5,
    PositionComparatorV1::FiniteF64 => 6,
    PositionComparatorV1::TimestampMs => 7,
    PositionComparatorV1::Boolean => 8,
  }
}

fn independent_group_tuple(fields: &[&ModelField]) -> Vec<u8> {
  let mut output = Vec::from(b"AGTP".as_slice());
  output.extend_from_slice(&1u16.to_le_bytes());
  output.extend_from_slice(&(fields.len() as u16).to_le_bytes());
  for field in fields {
    output.push(match field.state {
      QueryExecutionFieldStateV1::Values => 0,
      QueryExecutionFieldStateV1::Missing => 1,
      QueryExecutionFieldStateV1::DeterministicUnindexable => 2,
    });
    output.extend_from_slice(&[0, 0, 0]);
    output.extend_from_slice(&(field.values.len() as u32).to_le_bytes());
    let values_length = field.values.iter().map(|value| 8 + value.payload.len()).sum::<usize>();
    output.extend_from_slice(&(values_length as u32).to_le_bytes());
    for value in &field.values {
      let (tag, state) = match value.state {
        PositionComponentStateV1::Present => (independent_comparator_tag(value.comparator.unwrap()), 0),
        PositionComponentStateV1::TypedNull => (0, 1),
        PositionComponentStateV1::Missing => panic!("group fixture contains per-value missing state"),
      };
      output.extend_from_slice(&tag.to_le_bytes());
      output.push(state);
      output.push(0);
      output.extend_from_slice(&(value.payload.len() as u32).to_le_bytes());
      output.extend_from_slice(&value.payload);
    }
  }
  output
}

fn expected_groups(fixture: &PipelineFixture, input: &CompiledQueryAggregateInputV1) -> (usize, Vec<(Vec<u8>, ExpectedGroup)>, u64) {
  let group_names = input.group_field_indices().iter().map(|index| input.fields()[*index].field_name()).collect::<Vec<_>>();
  assert_eq!(group_names, ["@updated_at", "@size"]);
  let mut expected = BTreeMap::<Vec<u8>, ExpectedGroup>::new();
  let mut aggregate_values_examined = 0u64;
  for document in fixture.documents.iter().filter(|document| matches(document)) {
    let tuple = independent_group_tuple(&[&document.updated_at, &document.size]);
    let group = expected.entry(tuple).or_default();
    group.document_count += 1;
    group.unsigned_values.extend(document.size.values.iter().filter_map(|value| {
      (value.state == PositionComponentStateV1::Present).then(|| u64::from_le_bytes(value.payload.as_slice().try_into().unwrap()))
    }));
    group.signed_values.extend(document.updated_at.values.iter().filter_map(|value| {
      (value.state == PositionComponentStateV1::Present).then(|| i64::from_le_bytes(value.payload.as_slice().try_into().unwrap()))
    }));
    aggregate_values_examined += (document.size.values.len() + document.updated_at.values.len()) as u64;
  }
  let total = expected.len();
  let mut expected = expected.into_iter().collect::<Vec<_>>();
  expected.sort_by(|(left_tuple, left), (right_tuple, right)| {
    right.document_count.cmp(&left.document_count).then_with(|| left_tuple.cmp(right_tuple))
  });
  expected.truncate(RESULT_LIMIT);
  (total, expected, aggregate_values_examined)
}

fn assert_unsigned_group_reducers(
  result: &aeordb::engine::v4::query_aggregate_execution::QueryGroupedAggregateResultV1,
  index: usize,
  expected: &ExpectedGroup,
) {
  assert_eq!(
    result.group_value(index, "@size", QueryAggregateKindV1::Count),
    Some(QueryAggregateReducedValueRefV1::Count(expected.unsigned_values.len() as u64))
  );
  if expected.unsigned_values.is_empty() {
    for kind in [QueryAggregateKindV1::Sum, QueryAggregateKindV1::Average, QueryAggregateKindV1::Minimum, QueryAggregateKindV1::Maximum] {
      assert_eq!(result.group_value(index, "@size", kind), Some(QueryAggregateReducedValueRefV1::Empty));
    }
    return;
  }
  let sum = expected.unsigned_values.iter().map(|value| u128::from(*value)).sum::<u128>();
  assert_eq!(
    result.group_value(index, "@size", QueryAggregateKindV1::Sum),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::UnsignedRatio { numerator: sum, denominator: 1 }))
  );
  assert_eq!(
    result.group_value(index, "@size", QueryAggregateKindV1::Average),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::UnsignedRatio {
      numerator: sum,
      denominator: expected.unsigned_values.len() as u64,
    }))
  );
  for (kind, expected_value) in [
    (QueryAggregateKindV1::Minimum, expected.unsigned_values.iter().min().unwrap()),
    (QueryAggregateKindV1::Maximum, expected.unsigned_values.iter().max().unwrap()),
  ] {
    let Some(QueryAggregateReducedValueRefV1::Ordered(actual)) = result.group_value(index, "@size", kind) else {
      panic!("ordered aggregate is absent")
    };
    assert_eq!(actual.payload, expected_value.to_le_bytes());
  }
}

fn assert_signed_group_reducers(
  result: &aeordb::engine::v4::query_aggregate_execution::QueryGroupedAggregateResultV1,
  index: usize,
  expected: &ExpectedGroup,
) {
  assert_eq!(
    result.group_value(index, "@updated_at", QueryAggregateKindV1::Count),
    Some(QueryAggregateReducedValueRefV1::Count(expected.signed_values.len() as u64))
  );
  if expected.signed_values.is_empty() {
    for kind in [QueryAggregateKindV1::Sum, QueryAggregateKindV1::Average, QueryAggregateKindV1::Minimum, QueryAggregateKindV1::Maximum] {
      assert_eq!(result.group_value(index, "@updated_at", kind), Some(QueryAggregateReducedValueRefV1::Empty));
    }
    return;
  }
  let sum = expected.signed_values.iter().map(|value| i128::from(*value)).sum::<i128>();
  assert_eq!(
    result.group_value(index, "@updated_at", QueryAggregateKindV1::Sum),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::SignedRatio { numerator: sum, denominator: 1 }))
  );
  assert_eq!(
    result.group_value(index, "@updated_at", QueryAggregateKindV1::Average),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::SignedRatio {
      numerator: sum,
      denominator: expected.signed_values.len() as u64,
    }))
  );
  for (kind, expected_value) in [
    (QueryAggregateKindV1::Minimum, expected.signed_values.iter().min().unwrap()),
    (QueryAggregateKindV1::Maximum, expected.signed_values.iter().max().unwrap()),
  ] {
    let Some(QueryAggregateReducedValueRefV1::Ordered(actual)) = result.group_value(index, "@updated_at", kind) else {
      panic!("ordered signed aggregate is absent")
    };
    assert_eq!(actual.payload, expected_value.to_le_bytes());
  }
}

fn execute_top_k(fixture: &PipelineFixture) {
  let coordinator = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut partition_source = PartitionSource::new(fixture, PartitionFault::None);
  let mut position_source = ModelPositionSource::new(fixture);
  let mut sink = QueryOrderedTopKSinkV1::new(
    &fixture.plan,
    &mut position_source,
    &coordinator,
    &cancellation,
    QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap(),
  )
  .unwrap();
  let receipt = execute_authoritative_partitioned_query_into_v1(
    RootAwarePartitionedQueryExecutionRequestV1 {
      plan: &fixture.plan,
      catalogs: &fixture.catalogs,
      source: &mut partition_source,
      memory: &coordinator,
      cancellation: &cancellation,
      limits: execution_limits(),
    },
    &mut sink,
  )
  .unwrap();
  let mut expected = fixture.documents.iter().filter(|document| matches(document)).collect::<Vec<_>>();
  expected.sort_by(|left, right| {
    left.path.cmp(&right.path).then_with(|| left.file_key.cmp(&right.file_key)).then_with(|| left.revision.cmp(&right.revision))
  });
  assert_eq!(receipt.match_count(), expected.len() as u64);
  let total_matches = expected.len();
  expected.truncate(RESULT_LIMIT);
  let result = sink.finish().unwrap();
  assert_eq!(result.total_match_count(), total_matches as u64);
  assert_eq!(result.has_more(), total_matches > RESULT_LIMIT);
  assert_eq!(result.examined_documents(), fixture.documents.len() as u64);
  assert_eq!(result.examined_field_values(), (fixture.documents.len() * 2) as u64);
  assert_eq!(result.rows().len(), expected.len());
  for (actual, expected) in result.rows().iter().zip(expected) {
    assert_eq!(actual.components.last().unwrap().payload, expected.path.as_bytes());
    assert_eq!(actual.file_key_tie, expected.file_key);
    assert_eq!(actual.record_revision_tie, expected.revision);
  }
  assert!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes > 0);
  drop(result);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  assert_eq!(position_source.calls, total_matches);
}

fn execute_grouped_aggregates(fixture: &PipelineFixture) {
  let input =
    CompiledQueryAggregateInputV1::from_plan(&fixture.plan, QueryAggregateInputLimitsV1::new(16, 16, 32, 1 << 20).unwrap()).unwrap();
  let (total_groups, expected, aggregate_values_examined) = expected_groups(fixture, &input);
  let coordinator = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut partition_source = PartitionSource::new(fixture, PartitionFault::None);
  let mut aggregate_source = ModelAggregateSource::new(fixture);
  let mut sink = QueryGroupedAggregateSinkV1::new(
    &input,
    &mut aggregate_source,
    &coordinator,
    &cancellation,
    QueryGroupedAggregateLimitsV1::new(128, 1 << 20, 32 << 20).unwrap(),
  )
  .unwrap();
  let receipt = execute_authoritative_partitioned_query_into_v1(
    RootAwarePartitionedQueryExecutionRequestV1 {
      plan: &fixture.plan,
      catalogs: &fixture.catalogs,
      source: &mut partition_source,
      memory: &coordinator,
      cancellation: &cancellation,
      limits: execution_limits(),
    },
    &mut sink,
  )
  .unwrap();
  let matching_documents = fixture.documents.iter().filter(|document| matches(document)).count();
  assert_eq!(receipt.match_count(), matching_documents as u64);
  let result = sink.finish().unwrap();
  assert_eq!(result.total_document_count(), matching_documents as u64);
  assert_eq!(result.total_group_count(), total_groups as u64);
  assert_eq!(result.groups().len(), expected.len());
  assert_eq!(result.has_more(), total_groups > RESULT_LIMIT);
  assert_eq!(result.examined_documents(), fixture.documents.len() as u64);
  assert_eq!(result.examined_field_values(), (fixture.documents.len() * 2) as u64);
  assert_eq!(result.aggregate_values_examined(), aggregate_values_examined);
  for (index, ((expected_tuple, expected_group), actual)) in expected.iter().zip(result.groups()).enumerate() {
    assert_eq!(actual.canonical_group_tuple(), expected_tuple);
    assert_eq!(actual.document_count(), expected_group.document_count);
    assert_eq!(actual.position_row().route, PositionRouteV1::AggregateGroups);
    assert_eq!(actual.position_row().file_key_tie, digest_parts(fixture.plan.hash_algorithm(), &[expected_tuple]));
    assert_eq!(actual.position_row().record_revision_tie, fixture.plan.selected_namespace_root());
    assert_unsigned_group_reducers(&result, index, expected_group);
    assert_signed_group_reducers(&result, index, expected_group);
  }
  assert!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes > 0);
  drop(result);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  assert_eq!(aggregate_source.calls, matching_documents);
}

#[test]
fn composed_partition_pipeline_matches_independent_top_k_and_grouped_models() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for shape in [PartitionShape::Shared, PartitionShape::Nonidentical] {
      let fixture = pipeline_fixture(algorithm, shape);
      execute_top_k(&fixture);
      execute_grouped_aggregates(&fixture);
    }
  }
}

#[test]
fn composed_partition_pipeline_rolls_back_identity_and_receipt_corruption_before_retry() {
  let fixture = pipeline_fixture(HashAlgorithm::Blake3_256, PartitionShape::Nonidentical);
  let expected_matches = fixture.documents.iter().filter(|document| matches(document)).count();
  for fault in
    [PartitionFault::DuplicateFilenameIdentity, PartitionFault::MissingFilenameIdentity, PartitionFault::DishonestCreatedAtReceipt]
  {
    let coordinator = memory(64 << 20);
    let cancellation = CancellationToken::new();
    let mut position_source = ModelPositionSource::new(&fixture);
    let mut sink = QueryOrderedTopKSinkV1::new(
      &fixture.plan,
      &mut position_source,
      &coordinator,
      &cancellation,
      QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap(),
    )
    .unwrap();
    let mut corrupt_source = PartitionSource::new(&fixture, fault);
    let error = execute_authoritative_partitioned_query_into_v1(
      RootAwarePartitionedQueryExecutionRequestV1 {
        plan: &fixture.plan,
        catalogs: &fixture.catalogs,
        source: &mut corrupt_source,
        memory: &coordinator,
        cancellation: &cancellation,
        limits: execution_limits(),
      },
      &mut sink,
    )
    .unwrap_err();
    assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource, "fault {fault:?}");
    assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Execution);

    let mut honest_source = PartitionSource::new(&fixture, PartitionFault::None);
    let retry = execute_authoritative_partitioned_query_into_v1(
      RootAwarePartitionedQueryExecutionRequestV1 {
        plan: &fixture.plan,
        catalogs: &fixture.catalogs,
        source: &mut honest_source,
        memory: &coordinator,
        cancellation: &cancellation,
        limits: execution_limits(),
      },
      &mut sink,
    )
    .unwrap();
    assert_eq!(retry.match_count(), expected_matches as u64);
    let result = sink.finish().unwrap();
    assert_eq!(result.total_match_count(), expected_matches as u64, "fault {fault:?} leaked staged matches");
    drop(result);
    assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn composed_partition_pipeline_cancellation_and_overlapping_pressure_release_exactly() {
  let fixture = pipeline_fixture(HashAlgorithm::Blake3_256, PartitionShape::Nonidentical);
  let coordinator = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut position_source = ModelPositionSource::new(&fixture);
  let mut sink = QueryOrderedTopKSinkV1::new(
    &fixture.plan,
    &mut position_source,
    &coordinator,
    &cancellation,
    QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap(),
  )
  .unwrap();
  let mut cancelling_source = PartitionSource::new(&fixture, PartitionFault::CancelDuringCreatedAt);
  let error = execute_authoritative_partitioned_query_into_v1(
    RootAwarePartitionedQueryExecutionRequestV1 {
      plan: &fixture.plan,
      catalogs: &fixture.catalogs,
      source: &mut cancelling_source,
      memory: &coordinator,
      cancellation: &cancellation,
      limits: execution_limits(),
    },
    &mut sink,
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::Cancelled);
  assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Execution);
  drop(sink);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let pressured = memory(20 << 20);
  let pressure_cancellation = CancellationToken::new();
  let mut pressure_position_source = ModelPositionSource::new(&fixture);
  let mut pressure_sink = QueryOrderedTopKSinkV1::new(
    &fixture.plan,
    &mut pressure_position_source,
    &pressured,
    &pressure_cancellation,
    QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap(),
  )
  .unwrap();
  let mut honest_source = PartitionSource::new(&fixture, PartitionFault::None);
  let error = execute_authoritative_partitioned_query_into_v1(
    RootAwarePartitionedQueryExecutionRequestV1 {
      plan: &fixture.plan,
      catalogs: &fixture.catalogs,
      source: &mut honest_source,
      memory: &pressured,
      cancellation: &pressure_cancellation,
      limits: execution_limits(),
    },
    &mut pressure_sink,
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit);
  assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Execution);
  drop(pressure_sink);
  assert_eq!(pressure_position_source.calls, 0);
  assert_eq!(pressured.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}
