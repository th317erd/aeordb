use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::index_coverage_planner::IndexSemanticQueryAvailabilityV1;
use aeordb::engine::v4::position::{PositionComparatorV1, PositionComponentStateV1};
use aeordb::engine::v4::position_order::LogicalOrderComponentOwnedV1;
use aeordb::engine::v4::query_aggregate_execution::{
  CompiledQueryAggregateInputV1, QueryAggregateInputFieldV1, QueryAggregateInputLimitsV1, QueryAggregateInputLookupRequestV1,
  QueryAggregateInputLookupResultV1, QueryAggregateInputRowV1, QueryAggregateInputSourceV1, resolve_query_aggregate_input_v1,
};
use aeordb::engine::v4::query_executor::{QueryExecutionFieldStateV1, QueryExecutionSourceErrorClassV1, QueryExecutionSourceErrorV1};
use aeordb::engine::v4::query_planner::{
  QueryAggregateFieldV1, QueryAggregateKindV1, QueryExpressionV1, QueryPlanningContextV1, QueryPlanningIndexCandidateV1,
  QueryPlanningIndexEstimatesV1, QueryPlanningRequestV1, QueryPlanningScopeV1, RootAwareQueryFieldCatalogV1,
  default_query_planning_limits_v1, plan_root_aware_query_v1,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x11; 16];
const PHYSICAL_INSTANCE_ID: [u8; 16] = [0x22; 16];

fn fixture(path: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("aggregate input fixture does not cover {algorithm:?}"),
  }
}

fn aggregate_plan(algorithm: HashAlgorithm) -> aeordb::engine::v4::query_planner::CompiledRootAwareQueryPlanV1 {
  let algorithm_name = algorithm_name(algorithm);
  let encoded_scope = fixture(&format!("scope-definition-v1/ascp-{algorithm_name}-root-direct-valid.bin"));
  let scope_definition = decode_scope_definition(&encoded_scope, algorithm).unwrap();
  let mut value_store = fixture(&format!("value-store-definition-v1/avst-{algorithm_name}-metadata-hash-corrected-valid.bin"));
  let hash_width = algorithm.hash_length();
  value_store[32..32 + hash_width].copy_from_slice(&scope_definition.scope_id);
  let field_name = "@size";
  let fixed_start = 32 + hash_width;
  let field_start = fixed_start + 80;
  let old_field_length = u32::from_le_bytes(value_store[fixed_start..fixed_start + 4].try_into().unwrap()) as usize;
  value_store.splice(field_start..field_start + old_field_length, field_name.as_bytes().iter().copied());
  let value_store_length = value_store.len() as u32;
  value_store[8..12].copy_from_slice(&value_store_length.to_le_bytes());
  value_store[fixed_start..fixed_start + 4].copy_from_slice(&(field_name.len() as u32).to_le_bytes());
  let selector_start = field_start + field_name.len();
  value_store[selector_start + 32..selector_start + 34].copy_from_slice(&5u16.to_le_bytes());
  let value_definition = decode_value_store_definition(&value_store, algorithm).unwrap();

  let mut field = fixture(&format!("field-index-definition-v1/afix-{algorithm_name}-u64_order_v1-valid.bin"));
  field[32..32 + hash_width].copy_from_slice(&value_definition.value_store_id);
  let field_definition = decode_field_index_definition(&field, algorithm).unwrap();
  let root = vec![0x33; hash_width];
  let semantic_root = vec![0x44; hash_width];
  let scope = QueryPlanningScopeV1 {
    scope_id: scope_definition.scope_id,
    value_store_id: value_definition.value_store_id,
    encoded_scope_definition: encoded_scope,
    encoded_value_store_definition: value_store,
    semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
    authoritative_document_count: 10,
    indexes: vec![QueryPlanningIndexCandidateV1 {
      index_id: field_definition.index_id,
      encoded_field_definition: field,
      selected_generation: None,
      estimates: QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap(),
      nvt_hint_available: false,
    }],
  };
  let catalogs = [RootAwareQueryFieldCatalogV1 {
    database_id: DATABASE_ID,
    physical_instance_id: PHYSICAL_INSTANCE_ID,
    selected_namespace_root: root.clone(),
    semantic_state_root: semantic_root.clone(),
    publication_sequence: 41,
    field_name: field_name.to_string(),
    complete: true,
    scopes: vec![scope],
  }];
  let context = QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &root, &semantic_root, 41).unwrap();
  let expression = QueryExpressionV1::And(Vec::new());
  let aggregates = [
    QueryAggregateFieldV1 { field_name: field_name.to_string(), kind: QueryAggregateKindV1::Average },
    QueryAggregateFieldV1 { field_name: field_name.to_string(), kind: QueryAggregateKindV1::Maximum },
  ];
  let groups = [field_name.to_string()];
  plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &aggregates,
    group_fields: &groups,
    result_limit: 20,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap()
}

#[derive(Default)]
struct ModelAggregateSource {
  result: Option<Result<QueryAggregateInputLookupResultV1, QueryExecutionSourceErrorV1>>,
  calls: usize,
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
    assert_eq!(request.fields().len(), 1, "duplicate aggregate/group declarations must share one lookup");
    assert_eq!(request.fields()[0].field_name(), "@size");
    assert_eq!(request.fields()[0].comparator(), PositionComparatorV1::U64);
    self.result.take().unwrap()
  }
}

fn found_row(
  input: &CompiledQueryAggregateInputV1,
  file_key: &[u8],
  revision: &[u8],
  values: Vec<LogicalOrderComponentOwnedV1>,
) -> QueryAggregateInputLookupResultV1 {
  QueryAggregateInputLookupResultV1::Found(QueryAggregateInputRowV1 {
    selected_namespace_root: input.selected_namespace_root().to_vec(),
    file_key: file_key.to_vec(),
    record_revision: revision.to_vec(),
    fields: vec![QueryAggregateInputFieldV1 {
      field_name: "@size".to_string(),
      scope_id: input.fields()[0].scope_ids()[0].to_vec(),
      state: QueryExecutionFieldStateV1::Values,
      values,
    }],
  })
}

#[test]
fn selected_root_aggregate_input_deduplicates_fields_and_validates_complete_rows() {
  let plan = aggregate_plan(HashAlgorithm::Blake3_256);
  let limits = QueryAggregateInputLimitsV1::new(32, 1_024, 4_096, 1 << 20).unwrap();
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, limits).unwrap();
  assert_eq!(input.fields().len(), 1);
  assert_eq!(input.fields()[0].operations().len(), 3);

  let file_key = vec![0x55; 32];
  let revision = vec![0x66; 32];
  let row = found_row(
    &input,
    &file_key,
    &revision,
    vec![
      LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 7u64.to_le_bytes().to_vec()),
      LogicalOrderComponentOwnedV1::typed_null(),
      LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 9u64.to_le_bytes().to_vec()),
    ],
  );
  let mut source = ModelAggregateSource { result: Some(Ok(row)), calls: 0 };
  let cancellation = CancellationToken::new();
  let resolved =
    resolve_query_aggregate_input_v1(QueryAggregateInputLookupRequestV1::new(&input, &file_key, &revision), &mut source, &cancellation)
      .unwrap();
  let QueryAggregateInputLookupResultV1::Found(resolved) = resolved else {
    panic!("expected selected-root row")
  };
  assert_eq!(resolved.fields[0].values.len(), 3);
  assert_eq!(resolved.fields[0].values[1].state, PositionComponentStateV1::TypedNull);
  assert_eq!(source.calls, 1);
}

#[test]
fn selected_root_aggregate_input_rejects_absent_malformed_oversized_and_cancelled_rows() {
  let plan = aggregate_plan(HashAlgorithm::Blake3_256);
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 2, 2, 512).unwrap()).unwrap();
  let file_key = vec![0x55; 32];
  let revision = vec![0x66; 32];

  let cases = [
    QueryAggregateInputLookupResultV1::Absent,
    QueryAggregateInputLookupResultV1::Found(QueryAggregateInputRowV1 {
      selected_namespace_root: vec![0x99; 32],
      file_key: file_key.clone(),
      record_revision: revision.clone(),
      fields: Vec::new(),
    }),
    found_row(
      &input,
      &file_key,
      &revision,
      vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::I64, 7i64.to_le_bytes().to_vec())],
    ),
    found_row(
      &input,
      &file_key,
      &revision,
      vec![
        LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 1u64.to_le_bytes().to_vec()),
        LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 2u64.to_le_bytes().to_vec()),
        LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 3u64.to_le_bytes().to_vec()),
      ],
    ),
  ];
  for row in cases {
    let mut source = ModelAggregateSource { result: Some(Ok(row)), calls: 0 };
    let error = resolve_query_aggregate_input_v1(
      QueryAggregateInputLookupRequestV1::new(&input, &file_key, &revision),
      &mut source,
      &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(error.class(), QueryExecutionSourceErrorClassV1::Corrupt | QueryExecutionSourceErrorClassV1::ResourceLimit));
  }

  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let mut source = ModelAggregateSource { result: Some(Ok(QueryAggregateInputLookupResultV1::Absent)), calls: 0 };
  let error =
    resolve_query_aggregate_input_v1(QueryAggregateInputLookupRequestV1::new(&input, &file_key, &revision), &mut source, &cancellation)
      .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::Cancelled);
  assert_eq!(source.calls, 0);
}

#[test]
fn aggregate_input_rejects_field_state_scope_and_hidden_capacity_lies() {
  let plan = aggregate_plan(HashAlgorithm::Blake3_256);
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 4, 4, 512).unwrap()).unwrap();
  let file_key = vec![0x55; 32];
  let revision = vec![0x66; 32];

  let mut wrong_name = match found_row(&input, &file_key, &revision, Vec::new()) {
    QueryAggregateInputLookupResultV1::Found(row) => row,
    QueryAggregateInputLookupResultV1::Absent => unreachable!(),
  };
  wrong_name.fields[0].field_name = "@updated_at".to_string();
  wrong_name.fields[0].state = QueryExecutionFieldStateV1::Missing;

  let mut wrong_scope = wrong_name.clone();
  wrong_scope.fields[0].field_name = "@size".to_string();
  wrong_scope.fields[0].scope_id = vec![0x77; 32];

  let mut missing_with_value = wrong_name.clone();
  missing_with_value.fields[0].field_name = "@size".to_string();
  missing_with_value.fields[0].values = vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 1u64.to_le_bytes().to_vec())];

  let mut values_without_value = wrong_name.clone();
  values_without_value.fields[0].field_name = "@size".to_string();
  values_without_value.fields[0].state = QueryExecutionFieldStateV1::Values;

  let mut missing_component = missing_with_value.clone();
  missing_component.fields[0].state = QueryExecutionFieldStateV1::Values;
  missing_component.fields[0].values[0] = LogicalOrderComponentOwnedV1::missing();

  let mut oversized_payload = Vec::new();
  oversized_payload.try_reserve_exact(1_024).unwrap();
  oversized_payload.extend_from_slice(&7u64.to_le_bytes());
  let hidden_capacity =
    found_row(&input, &file_key, &revision, vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, oversized_payload)]);

  for row in [
    QueryAggregateInputLookupResultV1::Found(wrong_name),
    QueryAggregateInputLookupResultV1::Found(wrong_scope),
    QueryAggregateInputLookupResultV1::Found(missing_with_value),
    QueryAggregateInputLookupResultV1::Found(values_without_value),
    QueryAggregateInputLookupResultV1::Found(missing_component),
    hidden_capacity,
  ] {
    let mut source = ModelAggregateSource { result: Some(Ok(row)), calls: 0 };
    let error = resolve_query_aggregate_input_v1(
      QueryAggregateInputLookupRequestV1::new(&input, &file_key, &revision),
      &mut source,
      &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(matches!(error.class(), QueryExecutionSourceErrorClassV1::Corrupt | QueryExecutionSourceErrorClassV1::ResourceLimit));
  }

  let unindexable = QueryAggregateInputLookupResultV1::Found(QueryAggregateInputRowV1 {
    selected_namespace_root: input.selected_namespace_root().to_vec(),
    file_key: file_key.clone(),
    record_revision: revision.clone(),
    fields: vec![QueryAggregateInputFieldV1 {
      field_name: "@size".to_string(),
      scope_id: input.fields()[0].scope_ids()[0].to_vec(),
      state: QueryExecutionFieldStateV1::DeterministicUnindexable,
      values: Vec::new(),
    }],
  });
  let mut source = ModelAggregateSource { result: Some(Ok(unindexable)), calls: 0 };
  assert!(matches!(
    resolve_query_aggregate_input_v1(
      QueryAggregateInputLookupRequestV1::new(&input, &file_key, &revision),
      &mut source,
      &CancellationToken::new(),
    )
    .unwrap(),
    QueryAggregateInputLookupResultV1::Found(_)
  ));
}

#[test]
fn aggregate_input_preserves_source_failure_classes_and_stays_storage_neutral() {
  let plan = aggregate_plan(HashAlgorithm::Blake3_256);
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 4, 4, 1 << 20).unwrap()).unwrap();
  let file_key = vec![0x55; 32];
  let revision = vec![0x66; 32];
  for class in [
    QueryExecutionSourceErrorClassV1::Unavailable,
    QueryExecutionSourceErrorClassV1::ResourceLimit,
    QueryExecutionSourceErrorClassV1::Corrupt,
    QueryExecutionSourceErrorClassV1::Cancelled,
    QueryExecutionSourceErrorClassV1::Internal,
  ] {
    let mut source = ModelAggregateSource {
      result: Some(Err(QueryExecutionSourceErrorV1::new(class, "injected_aggregate_source", "injected source failure"))),
      calls: 0,
    };
    let error = resolve_query_aggregate_input_v1(
      QueryAggregateInputLookupRequestV1::new(&input, &file_key, &revision),
      &mut source,
      &CancellationToken::new(),
    )
    .unwrap_err();
    assert_eq!(error.class(), class);
    assert_eq!(error.code(), "injected_aggregate_source");
  }

  let source = include_str!("../../src/engine/v4/query_aggregate_execution.rs");
  for forbidden in ["StorageEngine", "DiskKVStore", "IndexArtifact", "IndexNvt", "server::", "crate::server"] {
    assert!(!source.contains(forbidden), "aggregate input boundary acquired forbidden dependency {forbidden}");
  }
  assert_eq!(
    source.matches("validate_logical_order_component_v1").count(),
    2,
    "aggregate input must use the shared component authority once"
  );
}
