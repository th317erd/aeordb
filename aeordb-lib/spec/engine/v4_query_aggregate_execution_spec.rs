use std::collections::VecDeque;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coverage_planner::IndexSemanticQueryAvailabilityV1;
use aeordb::engine::v4::position::{PositionComparatorV1, PositionComponentStateV1, PositionRouteV1};
use aeordb::engine::v4::position_order::LogicalOrderComponentOwnedV1;
use aeordb::engine::v4::query_aggregate_execution::{
  CompiledQueryAggregateInputV1, QueryAggregateInputFieldV1, QueryAggregateInputLimitsV1, QueryAggregateInputLookupRequestV1,
  QueryAggregateInputLookupResultV1, QueryAggregateInputRowV1, QueryAggregateInputSourceV1, QueryAggregateNumericV1,
  QueryAggregateReducedValueRefV1, QueryGroupedAggregateLimitsV1, QueryGroupedAggregateSinkV1, QueryUngroupedAggregateLimitsV1,
  QueryUngroupedAggregateSinkV1, resolve_query_aggregate_input_v1,
};
use aeordb::engine::v4::query_executor::{
  QueryExecutionFieldStateV1, QueryExecutionMatchPathV1, QueryExecutionMatchRefV1, QueryExecutionMatchSinkV1,
  QueryExecutionSinkBatchReceiptV1, QueryExecutionSinkBatchV1, QueryExecutionSinkErrorClassV1, QueryExecutionSourceErrorClassV1,
  QueryExecutionSourceErrorV1,
};
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

fn aggregate_catalog(
  algorithm: HashAlgorithm,
  field_name: &str,
  comparator: &str,
  root: &[u8],
  semantic_root: &[u8],
) -> RootAwareQueryFieldCatalogV1 {
  let algorithm_name = algorithm_name(algorithm);
  let encoded_scope = fixture(&format!("scope-definition-v1/ascp-{algorithm_name}-root-direct-valid.bin"));
  let scope_definition = decode_scope_definition(&encoded_scope, algorithm).unwrap();
  let mut value_store = fixture(&format!("value-store-definition-v1/avst-{algorithm_name}-metadata-hash-corrected-valid.bin"));
  let hash_width = algorithm.hash_length();
  value_store[32..32 + hash_width].copy_from_slice(&scope_definition.scope_id);
  let fixed_start = 32 + hash_width;
  let field_start = fixed_start + 80;
  let old_field_length = u32::from_le_bytes(value_store[fixed_start..fixed_start + 4].try_into().unwrap()) as usize;
  value_store.splice(field_start..field_start + old_field_length, field_name.as_bytes().iter().copied());
  let value_store_length = value_store.len() as u32;
  value_store[8..12].copy_from_slice(&value_store_length.to_le_bytes());
  value_store[fixed_start..fixed_start + 4].copy_from_slice(&(field_name.len() as u32).to_le_bytes());
  let selector_start = field_start + field_name.len();
  let metadata_id = match field_name {
    "@size" => 5u16,
    "@created_at" => 6u16,
    "@updated_at" => 7u16,
    _ => panic!("unsupported aggregate metadata fixture field {field_name}"),
  };
  value_store[selector_start + 32..selector_start + 34].copy_from_slice(&metadata_id.to_le_bytes());
  let value_definition = decode_value_store_definition(&value_store, algorithm).unwrap();

  let mut field = fixture(&format!("field-index-definition-v1/afix-{algorithm_name}-{comparator}-valid.bin"));
  field[32..32 + hash_width].copy_from_slice(&value_definition.value_store_id);
  let field_definition = decode_field_index_definition(&field, algorithm).unwrap();
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
  RootAwareQueryFieldCatalogV1 {
    database_id: DATABASE_ID,
    physical_instance_id: PHYSICAL_INSTANCE_ID,
    selected_namespace_root: root.to_vec(),
    semantic_state_root: semantic_root.to_vec(),
    publication_sequence: 41,
    field_name: field_name.to_string(),
    complete: true,
    scopes: vec![scope],
  }
}

fn aggregate_plan_for(
  algorithm: HashAlgorithm,
  comparator: &str,
  aggregate_kinds: &[QueryAggregateKindV1],
  grouped: bool,
) -> aeordb::engine::v4::query_planner::CompiledRootAwareQueryPlanV1 {
  aggregate_plan_for_limit(algorithm, comparator, aggregate_kinds, grouped, 20)
}

fn aggregate_plan_for_limit(
  algorithm: HashAlgorithm,
  comparator: &str,
  aggregate_kinds: &[QueryAggregateKindV1],
  grouped: bool,
  result_limit: usize,
) -> aeordb::engine::v4::query_planner::CompiledRootAwareQueryPlanV1 {
  let hash_width = algorithm.hash_length();
  let root = vec![0x33; hash_width];
  let semantic_root = vec![0x44; hash_width];
  let field_name = "@size";
  let catalogs = [aggregate_catalog(algorithm, field_name, comparator, &root, &semantic_root)];
  let context = QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &root, &semantic_root, 41).unwrap();
  let expression = QueryExpressionV1::And(Vec::new());
  let aggregates =
    aggregate_kinds.iter().map(|kind| QueryAggregateFieldV1 { field_name: field_name.to_string(), kind: *kind }).collect::<Vec<_>>();
  let groups = grouped.then(|| vec![field_name.to_string()]).unwrap_or_default();
  plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &aggregates,
    group_fields: &groups,
    result_limit,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap()
}

fn aggregate_plan(algorithm: HashAlgorithm) -> aeordb::engine::v4::query_planner::CompiledRootAwareQueryPlanV1 {
  aggregate_plan_for(algorithm, "u64_order_v1", &[QueryAggregateKindV1::Average, QueryAggregateKindV1::Maximum], true)
}

fn multi_field_group_plan(algorithm: HashAlgorithm) -> aeordb::engine::v4::query_planner::CompiledRootAwareQueryPlanV1 {
  let hash_width = algorithm.hash_length();
  let root = vec![0x33; hash_width];
  let semantic_root = vec![0x44; hash_width];
  let catalogs = [
    aggregate_catalog(algorithm, "@size", "u64_order_v1", &root, &semantic_root),
    aggregate_catalog(algorithm, "@updated_at", "i64_order_v1", &root, &semantic_root),
  ];
  let context = QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &root, &semantic_root, 41).unwrap();
  let expression = QueryExpressionV1::And(Vec::new());
  let aggregates = [
    QueryAggregateFieldV1 { field_name: "@size".to_string(), kind: QueryAggregateKindV1::Average },
    QueryAggregateFieldV1 { field_name: "@size".to_string(), kind: QueryAggregateKindV1::Maximum },
  ];
  plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &aggregates,
    group_fields: &["@updated_at".to_string(), "@size".to_string()],
    result_limit: 4,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap()
}

fn ungrouped_plan(algorithm: HashAlgorithm) -> aeordb::engine::v4::query_planner::CompiledRootAwareQueryPlanV1 {
  ungrouped_plan_for_comparator(algorithm, "u64_order_v1")
}

fn ungrouped_plan_for_comparator(
  algorithm: HashAlgorithm,
  comparator: &str,
) -> aeordb::engine::v4::query_planner::CompiledRootAwareQueryPlanV1 {
  aggregate_plan_for(
    algorithm,
    comparator,
    &[
      QueryAggregateKindV1::Count,
      QueryAggregateKindV1::Sum,
      QueryAggregateKindV1::Average,
      QueryAggregateKindV1::Minimum,
      QueryAggregateKindV1::Maximum,
    ],
    false,
  )
}

fn memory(hard_limit: u64) -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(hard_limit - (1 << 20), hard_limit, 1, 1 << 20).unwrap())
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

fn found_state_row(
  input: &CompiledQueryAggregateInputV1,
  file_key: &[u8],
  revision: &[u8],
  state: QueryExecutionFieldStateV1,
  values: Vec<LogicalOrderComponentOwnedV1>,
) -> QueryAggregateInputLookupResultV1 {
  QueryAggregateInputLookupResultV1::Found(QueryAggregateInputRowV1 {
    selected_namespace_root: input.selected_namespace_root().to_vec(),
    file_key: file_key.to_vec(),
    record_revision: revision.to_vec(),
    fields: vec![QueryAggregateInputFieldV1 {
      field_name: "@size".to_string(),
      scope_id: input.fields()[0].scope_ids()[0].to_vec(),
      state,
      values,
    }],
  })
}

fn found_two_field_row(
  input: &CompiledQueryAggregateInputV1,
  file_key: &[u8],
  revision: &[u8],
  size: u64,
  updated_at: i64,
) -> QueryAggregateInputLookupResultV1 {
  assert_eq!(input.fields().len(), 2);
  assert_eq!(input.fields()[0].field_name(), "@size");
  assert_eq!(input.fields()[1].field_name(), "@updated_at");
  QueryAggregateInputLookupResultV1::Found(QueryAggregateInputRowV1 {
    selected_namespace_root: input.selected_namespace_root().to_vec(),
    file_key: file_key.to_vec(),
    record_revision: revision.to_vec(),
    fields: vec![
      QueryAggregateInputFieldV1 {
        field_name: "@size".to_string(),
        scope_id: input.fields()[0].scope_ids()[0].to_vec(),
        state: QueryExecutionFieldStateV1::Values,
        values: vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, size.to_le_bytes().to_vec())],
      },
      QueryAggregateInputFieldV1 {
        field_name: "@updated_at".to_string(),
        scope_id: input.fields()[1].scope_ids()[0].to_vec(),
        state: QueryExecutionFieldStateV1::Values,
        values: vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::I64, updated_at.to_le_bytes().to_vec())],
      },
    ],
  })
}

struct QueueAggregateSource {
  results: VecDeque<Result<QueryAggregateInputLookupResultV1, QueryExecutionSourceErrorV1>>,
  calls: usize,
}

impl QueryAggregateInputSourceV1 for QueueAggregateSource {
  fn resolve_aggregate_input(
    &mut self,
    _request: QueryAggregateInputLookupRequestV1<'_>,
    _cancellation: &CancellationToken,
  ) -> Result<QueryAggregateInputLookupResultV1, QueryExecutionSourceErrorV1> {
    self.calls += 1;
    self.results.pop_front().expect("aggregate source received an unexpected lookup")
  }
}

fn identity(algorithm: HashAlgorithm, seed: u8) -> (Vec<u8>, Vec<u8>) {
  (vec![seed; algorithm.hash_length()], vec![seed.wrapping_add(0x40); algorithm.hash_length()])
}

fn push_match(
  sink: &mut dyn QueryExecutionMatchSinkV1,
  file_key: &[u8],
  revision: &[u8],
) -> Result<(), aeordb::engine::v4::query_executor::QueryExecutionSinkErrorV1> {
  sink.push_match(QueryExecutionMatchRefV1 {
    file_key,
    record_revision: revision,
    path: QueryExecutionMatchPathV1::RequiresSelectedRootLookup,
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

#[test]
fn ungrouped_reducer_streams_documents_and_all_present_values_transactionally() {
  let algorithm = HashAlgorithm::Blake3_256;
  let plan = ungrouped_plan(algorithm);
  let input =
    CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 1_024, 4_096, 1 << 20).unwrap()).unwrap();
  let identities = (1..=4).map(|seed| identity(algorithm, seed)).collect::<Vec<_>>();
  let mut source = QueueAggregateSource {
    results: VecDeque::from([
      Ok(found_row(
        &input,
        &identities[0].0,
        &identities[0].1,
        vec![
          LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 7u64.to_le_bytes().to_vec()),
          LogicalOrderComponentOwnedV1::typed_null(),
          LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 9u64.to_le_bytes().to_vec()),
        ],
      )),
      Ok(found_state_row(&input, &identities[1].0, &identities[1].1, QueryExecutionFieldStateV1::Missing, Vec::new())),
      Ok(found_row(
        &input,
        &identities[2].0,
        &identities[2].1,
        vec![
          LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 4u64.to_le_bytes().to_vec()),
          LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 4u64.to_le_bytes().to_vec()),
        ],
      )),
      Ok(found_state_row(&input, &identities[3].0, &identities[3].1, QueryExecutionFieldStateV1::DeterministicUnindexable, Vec::new())),
    ]),
    calls: 0,
  };
  let coordinator = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink = QueryUngroupedAggregateSinkV1::new(
    &input,
    &mut source,
    &coordinator,
    &cancellation,
    QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap(),
  )
  .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: input.selected_namespace_root(), scope_id: None, maximum_matches: 4 })
    .unwrap();
  for (file_key, revision) in &identities {
    push_match(&mut sink, file_key, revision).unwrap();
  }
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: input.selected_namespace_root(),
      scope_id: None,
      match_count: 4,
      examined_documents: 8,
      examined_field_values: 13,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  assert_eq!(result.document_count(), 4);
  assert_eq!(result.examined_documents(), 8);
  assert_eq!(result.examined_field_values(), 13);
  assert_eq!(result.aggregate_values_examined(), 5, "typed null remains examined but is not reduced");
  let field = result.field("@size").unwrap();
  assert_eq!(field.value(QueryAggregateKindV1::Count), Some(QueryAggregateReducedValueRefV1::Count(4)));
  assert_eq!(
    field.value(QueryAggregateKindV1::Sum),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::UnsignedRatio { numerator: 24, denominator: 1 }))
  );
  assert_eq!(
    field.value(QueryAggregateKindV1::Average),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::UnsignedRatio { numerator: 24, denominator: 4 }))
  );
  let Some(QueryAggregateReducedValueRefV1::Ordered(minimum)) = field.value(QueryAggregateKindV1::Minimum) else {
    panic!("minimum is absent")
  };
  let Some(QueryAggregateReducedValueRefV1::Ordered(maximum)) = field.value(QueryAggregateKindV1::Maximum) else {
    panic!("maximum is absent")
  };
  assert_eq!(minimum.payload, 4u64.to_le_bytes());
  assert_eq!(maximum.payload, 9u64.to_le_bytes());
  assert!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes > 0);
  drop(result);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  assert_eq!(source.calls, 4);
}

#[test]
fn ungrouped_reducer_preserves_integer_precision_and_empty_field_results() {
  let algorithm = HashAlgorithm::Sha512;
  let plan = ungrouped_plan(algorithm);
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  let first = identity(algorithm, 1);
  let second = identity(algorithm, 2);
  let mut source = QueueAggregateSource {
    results: VecDeque::from([
      Ok(found_row(
        &input,
        &first.0,
        &first.1,
        vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, u64::MAX.to_le_bytes().to_vec())],
      )),
      Ok(found_row(
        &input,
        &second.0,
        &second.1,
        vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 1u64.to_le_bytes().to_vec())],
      )),
    ]),
    calls: 0,
  };
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink =
    QueryUngroupedAggregateSinkV1::new(&input, &mut source, &memory, &cancellation, QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap())
      .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: input.selected_namespace_root(), scope_id: None, maximum_matches: 2 })
    .unwrap();
  push_match(&mut sink, &first.0, &first.1).unwrap();
  push_match(&mut sink, &second.0, &second.1).unwrap();
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: input.selected_namespace_root(),
      scope_id: None,
      match_count: 2,
      examined_documents: 2,
      examined_field_values: 2,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  let exact = (u64::MAX as u128) + 1;
  let field = result.field("@size").unwrap();
  assert_eq!(
    field.value(QueryAggregateKindV1::Sum),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::UnsignedRatio { numerator: exact, denominator: 1 }))
  );
  assert_eq!(
    field.value(QueryAggregateKindV1::Average),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::UnsignedRatio { numerator: exact, denominator: 2 }))
  );
  drop(result);

  let missing = identity(algorithm, 3);
  let mut source = QueueAggregateSource {
    results: VecDeque::from([Ok(found_state_row(&input, &missing.0, &missing.1, QueryExecutionFieldStateV1::Missing, Vec::new()))]),
    calls: 0,
  };
  let mut sink =
    QueryUngroupedAggregateSinkV1::new(&input, &mut source, &memory, &cancellation, QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap())
      .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: input.selected_namespace_root(), scope_id: None, maximum_matches: 1 })
    .unwrap();
  push_match(&mut sink, &missing.0, &missing.1).unwrap();
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: input.selected_namespace_root(),
      scope_id: None,
      match_count: 1,
      examined_documents: 1,
      examined_field_values: 0,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  let field = result.field("@size").unwrap();
  assert_eq!(field.value(QueryAggregateKindV1::Count), Some(QueryAggregateReducedValueRefV1::Count(0)));
  for kind in [QueryAggregateKindV1::Sum, QueryAggregateKindV1::Average, QueryAggregateKindV1::Minimum, QueryAggregateKindV1::Maximum] {
    assert_eq!(field.value(kind), Some(QueryAggregateReducedValueRefV1::Empty));
  }
  drop(result);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn ungrouped_reducer_uses_signed_and_compensated_finite_numeric_semantics() {
  let algorithm = HashAlgorithm::Blake3_256;
  let signed_plan = ungrouped_plan_for_comparator(algorithm, "i64_order_v1");
  let signed_input =
    CompiledQueryAggregateInputV1::from_plan(&signed_plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  let signed_identity = identity(algorithm, 1);
  let mut signed_source = QueueAggregateSource {
    results: VecDeque::from([Ok(found_row(
      &signed_input,
      &signed_identity.0,
      &signed_identity.1,
      [-10i64, 3, 7]
        .into_iter()
        .map(|value| LogicalOrderComponentOwnedV1::present(PositionComparatorV1::I64, value.to_le_bytes().to_vec()))
        .collect(),
    ))]),
    calls: 0,
  };
  let coordinator = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink = QueryUngroupedAggregateSinkV1::new(
    &signed_input,
    &mut signed_source,
    &coordinator,
    &cancellation,
    QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap(),
  )
  .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 {
      selected_namespace_root: signed_input.selected_namespace_root(),
      scope_id: None,
      maximum_matches: 1,
    })
    .unwrap();
  push_match(&mut sink, &signed_identity.0, &signed_identity.1).unwrap();
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: signed_input.selected_namespace_root(),
      scope_id: None,
      match_count: 1,
      examined_documents: 1,
      examined_field_values: 3,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  let field = result.field("@size").unwrap();
  assert_eq!(
    field.value(QueryAggregateKindV1::Sum),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::SignedRatio { numerator: 0, denominator: 1 }))
  );
  assert_eq!(
    field.value(QueryAggregateKindV1::Average),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::SignedRatio { numerator: 0, denominator: 3 }))
  );
  let Some(QueryAggregateReducedValueRefV1::Ordered(minimum)) = field.value(QueryAggregateKindV1::Minimum) else {
    panic!("signed minimum is absent")
  };
  let Some(QueryAggregateReducedValueRefV1::Ordered(maximum)) = field.value(QueryAggregateKindV1::Maximum) else {
    panic!("signed maximum is absent")
  };
  assert_eq!(i64::from_le_bytes(minimum.payload.as_slice().try_into().unwrap()), -10);
  assert_eq!(i64::from_le_bytes(maximum.payload.as_slice().try_into().unwrap()), 7);
  drop(result);

  let float_plan = ungrouped_plan_for_comparator(algorithm, "f64_finite_order_v1");
  let float_input =
    CompiledQueryAggregateInputV1::from_plan(&float_plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  let float_identity = identity(algorithm, 2);
  let float_values = [1.0e16f64, 1.0, -1.0e16];
  let mut float_source = QueueAggregateSource {
    results: VecDeque::from([Ok(found_row(
      &float_input,
      &float_identity.0,
      &float_identity.1,
      float_values
        .into_iter()
        .map(|value| LogicalOrderComponentOwnedV1::present(PositionComparatorV1::FiniteF64, value.to_le_bytes().to_vec()))
        .collect(),
    ))]),
    calls: 0,
  };
  let mut sink = QueryUngroupedAggregateSinkV1::new(
    &float_input,
    &mut float_source,
    &coordinator,
    &cancellation,
    QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap(),
  )
  .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 {
      selected_namespace_root: float_input.selected_namespace_root(),
      scope_id: None,
      maximum_matches: 1,
    })
    .unwrap();
  push_match(&mut sink, &float_identity.0, &float_identity.1).unwrap();
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: float_input.selected_namespace_root(),
      scope_id: None,
      match_count: 1,
      examined_documents: 1,
      examined_field_values: 3,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  let field = result.field("@size").unwrap();
  let Some(QueryAggregateReducedValueRefV1::Numeric(sum)) = field.value(QueryAggregateKindV1::Sum) else {
    panic!("finite sum is absent")
  };
  let Some(QueryAggregateReducedValueRefV1::Numeric(average)) = field.value(QueryAggregateKindV1::Average) else {
    panic!("finite average is absent")
  };
  assert_eq!(sum.finite_f64(), Some(1.0));
  assert_eq!(average.finite_f64(), Some(1.0 / 3.0));
  drop(result);

  let overflow_identity = identity(algorithm, 3);
  let mut overflow_source = QueueAggregateSource {
    results: VecDeque::from([Ok(found_row(
      &float_input,
      &overflow_identity.0,
      &overflow_identity.1,
      [f64::MAX, f64::MAX]
        .into_iter()
        .map(|value| LogicalOrderComponentOwnedV1::present(PositionComparatorV1::FiniteF64, value.to_le_bytes().to_vec()))
        .collect(),
    ))]),
    calls: 0,
  };
  let mut sink = QueryUngroupedAggregateSinkV1::new(
    &float_input,
    &mut overflow_source,
    &coordinator,
    &cancellation,
    QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap(),
  )
  .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 {
      selected_namespace_root: float_input.selected_namespace_root(),
      scope_id: None,
      maximum_matches: 1,
    })
    .unwrap();
  let error = push_match(&mut sink, &overflow_identity.0, &overflow_identity.1).unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_aggregate_numeric_overflow");
  sink.rollback_batch();
  drop(sink);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn ungrouped_reducer_rolls_back_source_and_receipt_failures_without_duplicate_state() {
  let algorithm = HashAlgorithm::Blake3_256;
  let plan = ungrouped_plan(algorithm);
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  let first = identity(algorithm, 1);
  let second = identity(algorithm, 2);
  let row = |identity: &(Vec<u8>, Vec<u8>), value: u64| {
    found_row(
      &input,
      &identity.0,
      &identity.1,
      vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, value.to_le_bytes().to_vec())],
    )
  };
  let mut source = QueueAggregateSource {
    results: VecDeque::from([
      Ok(row(&first, 50)),
      Err(QueryExecutionSourceErrorV1::new(
        QueryExecutionSourceErrorClassV1::Unavailable,
        "injected_aggregate_unavailable",
        "selected root unavailable",
      )),
      Ok(row(&first, 20)),
      Ok(row(&second, 30)),
      Ok(row(&first, 2)),
      Ok(row(&second, 3)),
    ]),
    calls: 0,
  };
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink =
    QueryUngroupedAggregateSinkV1::new(&input, &mut source, &memory, &cancellation, QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap())
      .unwrap();
  let batch = QueryExecutionSinkBatchV1 { selected_namespace_root: input.selected_namespace_root(), scope_id: None, maximum_matches: 2 };
  sink.begin_batch(batch).unwrap();
  push_match(&mut sink, &first.0, &first.1).unwrap();
  let error = push_match(&mut sink, &second.0, &second.1).unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::HistoricalViewUnavailable);
  sink.rollback_batch();

  sink.begin_batch(batch).unwrap();
  push_match(&mut sink, &first.0, &first.1).unwrap();
  push_match(&mut sink, &second.0, &second.1).unwrap();
  let error = sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: input.selected_namespace_root(),
      scope_id: None,
      match_count: 1,
      examined_documents: 2,
      examined_field_values: 2,
    })
    .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::Internal);
  sink.rollback_batch();

  sink.begin_batch(batch).unwrap();
  push_match(&mut sink, &first.0, &first.1).unwrap();
  push_match(&mut sink, &second.0, &second.1).unwrap();
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: input.selected_namespace_root(),
      scope_id: None,
      match_count: 2,
      examined_documents: 2,
      examined_field_values: 2,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  assert_eq!(result.document_count(), 2);
  assert_eq!(
    result.field("@size").unwrap().value(QueryAggregateKindV1::Sum),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::UnsignedRatio { numerator: 5, denominator: 1 }))
  );
  drop(result);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  assert_eq!(source.calls, 6);
}

#[test]
fn ungrouped_reducer_rejects_groups_cancellation_pressure_and_invalid_state() {
  let algorithm = HashAlgorithm::Blake3_256;
  let grouped_plan = aggregate_plan(algorithm);
  let grouped_input =
    CompiledQueryAggregateInputV1::from_plan(&grouped_plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  let coordinator = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut source = QueueAggregateSource { results: VecDeque::new(), calls: 0 };
  let error = match QueryUngroupedAggregateSinkV1::new(
    &grouped_input,
    &mut source,
    &coordinator,
    &cancellation,
    QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap(),
  ) {
    Ok(_) => panic!("ungrouped reducer accepted a grouped plan"),
    Err(error) => error,
  };
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::CorruptSource);

  let plan = ungrouped_plan(algorithm);
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  let constrained = memory(4 << 20);
  let error = match QueryUngroupedAggregateSinkV1::new(
    &input,
    &mut source,
    &constrained,
    &cancellation,
    QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap(),
  ) {
    Ok(_) => panic!("aggregate retained-state reservation unexpectedly succeeded"),
    Err(error) => error,
  };
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::ResourceLimit);
  assert_eq!(constrained.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let extrema_plan = aggregate_plan_for(
    algorithm,
    "utf8_binary_order_v1",
    &[QueryAggregateKindV1::Count, QueryAggregateKindV1::Minimum, QueryAggregateKindV1::Maximum],
    false,
  );
  let extrema_input =
    CompiledQueryAggregateInputV1::from_plan(&extrema_plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  let mut empty_source = QueueAggregateSource { results: VecDeque::new(), calls: 0 };
  let mut empty_sink = QueryUngroupedAggregateSinkV1::new(
    &extrema_input,
    &mut empty_source,
    &coordinator,
    &cancellation,
    QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap(),
  )
  .unwrap();
  empty_sink
    .begin_batch(QueryExecutionSinkBatchV1 {
      selected_namespace_root: extrema_input.selected_namespace_root(),
      scope_id: None,
      maximum_matches: 1,
    })
    .unwrap();
  empty_sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: extrema_input.selected_namespace_root(),
      scope_id: None,
      match_count: 0,
      examined_documents: 0,
      examined_field_values: 0,
    })
    .unwrap();
  let base_retained = empty_sink.finish().unwrap().retained_bytes();
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let extrema_identity = identity(algorithm, 9);
  let payload = vec![b'x'; 4_096];
  let one_extreme_bytes = 16 + payload.len() as u64;
  let mut extrema_source = QueueAggregateSource {
    results: VecDeque::from([Ok(found_row(
      &extrema_input,
      &extrema_identity.0,
      &extrema_identity.1,
      vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::Utf8Binary, payload)],
    ))]),
    calls: 0,
  };
  let mut extrema_sink = QueryUngroupedAggregateSinkV1::new(
    &extrema_input,
    &mut extrema_source,
    &coordinator,
    &cancellation,
    QueryUngroupedAggregateLimitsV1::new(base_retained + one_extreme_bytes).unwrap(),
  )
  .unwrap();
  extrema_sink
    .begin_batch(QueryExecutionSinkBatchV1 {
      selected_namespace_root: extrema_input.selected_namespace_root(),
      scope_id: None,
      maximum_matches: 1,
    })
    .unwrap();
  let error = push_match(&mut extrema_sink, &extrema_identity.0, &extrema_identity.1).unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::ResourceLimit);
  extrema_sink.rollback_batch();
  drop(extrema_sink);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let matched = identity(algorithm, 1);
  source.results.push_back(Ok(found_row(
    &input,
    &matched.0,
    &matched.1,
    vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, 1u64.to_le_bytes().to_vec())],
  )));
  let cancellation = CancellationToken::new();
  let mut sink = QueryUngroupedAggregateSinkV1::new(
    &input,
    &mut source,
    &coordinator,
    &cancellation,
    QueryUngroupedAggregateLimitsV1::new(8 << 20).unwrap(),
  )
  .unwrap();
  assert_eq!(push_match(&mut sink, &matched.0, &matched.1).unwrap_err().class(), QueryExecutionSinkErrorClassV1::Internal);
  sink
    .begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: input.selected_namespace_root(), scope_id: None, maximum_matches: 1 })
    .unwrap();
  cancellation.cancel();
  assert_eq!(push_match(&mut sink, &matched.0, &matched.1).unwrap_err().class(), QueryExecutionSinkErrorClassV1::Cancelled);
  sink.rollback_batch();
  drop(sink);
  assert_eq!(coordinator.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn grouped_reducer_streams_bounded_count_ordered_top_k_without_retaining_documents() {
  let algorithm = HashAlgorithm::Blake3_256;
  let plan = aggregate_plan_for_limit(algorithm, "u64_order_v1", &[QueryAggregateKindV1::Average, QueryAggregateKindV1::Maximum], true, 2);
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  let identities = (1..=5).map(|seed| identity(algorithm, seed)).collect::<Vec<_>>();
  let values = [30u64, 10, 20, 10, 20];
  let results = identities
    .iter()
    .zip(values)
    .map(|((file_key, revision), value)| {
      Ok(found_row(
        &input,
        file_key,
        revision,
        vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, value.to_le_bytes().to_vec())],
      ))
    })
    .collect();
  let mut source = QueueAggregateSource { results, calls: 0 };
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink = QueryGroupedAggregateSinkV1::new(
    &input,
    &mut source,
    &memory,
    &cancellation,
    QueryGroupedAggregateLimitsV1::new(8, 1 << 20, 16 << 20).unwrap(),
  )
  .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: input.selected_namespace_root(), scope_id: None, maximum_matches: 5 })
    .unwrap();
  for (file_key, revision) in &identities {
    push_match(&mut sink, file_key, revision).unwrap();
  }
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: input.selected_namespace_root(),
      scope_id: None,
      match_count: 5,
      examined_documents: 7,
      examined_field_values: 5,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  assert_eq!(result.total_document_count(), 5);
  assert_eq!(result.total_group_count(), 3);
  assert!(result.has_more());
  assert_eq!(result.groups().len(), 2);
  for (index, expected_value) in [10u64, 20].into_iter().enumerate() {
    let group = &result.groups()[index];
    assert_eq!(group.document_count(), 2);
    assert_eq!(group.position_row().route, PositionRouteV1::AggregateGroups);
    assert_eq!(u64::from_le_bytes(group.position_row().components[0].payload.as_slice().try_into().unwrap()), 2);
    assert_eq!(group.position_row().components[1].payload, group.canonical_group_tuple());
    assert_eq!(group.position_row().file_key_tie, digest_parts(algorithm, &[group.canonical_group_tuple()]));
    assert_eq!(group.position_row().record_revision_tie, input.selected_namespace_root());
    assert_eq!(&group.canonical_group_tuple()[..4], b"AGTP");
    assert_eq!(
      result.group_value(index, "@size", QueryAggregateKindV1::Average),
      Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::UnsignedRatio {
        numerator: u128::from(expected_value) * 2,
        denominator: 2,
      }))
    );
    let Some(QueryAggregateReducedValueRefV1::Ordered(maximum)) = result.group_value(index, "@size", QueryAggregateKindV1::Maximum) else {
      panic!("group maximum is absent")
    };
    assert_eq!(maximum.payload, expected_value.to_le_bytes());
  }
  assert_eq!(result.examined_documents(), 7);
  assert_eq!(result.examined_field_values(), 5);
  assert_eq!(result.aggregate_values_examined(), 5);
  assert!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes > 0);
  drop(result);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  assert_eq!(source.calls, 5);
}

#[test]
fn grouped_reducer_preserves_complete_multivalue_and_field_state_group_identity() {
  let algorithm = HashAlgorithm::Blake3_256;
  let plan = aggregate_plan(algorithm);
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  let identities = (1..=5).map(|seed| identity(algorithm, seed)).collect::<Vec<_>>();
  let sequence = |values: &[u64]| {
    values
      .iter()
      .map(|value| LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, value.to_le_bytes().to_vec()))
      .collect::<Vec<_>>()
  };
  let mut first = sequence(&[7]);
  first.push(LogicalOrderComponentOwnedV1::typed_null());
  first.extend(sequence(&[9]));
  let mut reversed = sequence(&[9]);
  reversed.push(LogicalOrderComponentOwnedV1::typed_null());
  reversed.extend(sequence(&[7]));
  let mut source = QueueAggregateSource {
    results: VecDeque::from([
      Ok(found_row(&input, &identities[0].0, &identities[0].1, first.clone())),
      Ok(found_row(&input, &identities[1].0, &identities[1].1, first)),
      Ok(found_row(&input, &identities[2].0, &identities[2].1, reversed)),
      Ok(found_state_row(&input, &identities[3].0, &identities[3].1, QueryExecutionFieldStateV1::Missing, Vec::new())),
      Ok(found_state_row(&input, &identities[4].0, &identities[4].1, QueryExecutionFieldStateV1::DeterministicUnindexable, Vec::new())),
    ]),
    calls: 0,
  };
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink = QueryGroupedAggregateSinkV1::new(
    &input,
    &mut source,
    &memory,
    &cancellation,
    QueryGroupedAggregateLimitsV1::new(32, 1 << 20, 16 << 20).unwrap(),
  )
  .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: input.selected_namespace_root(), scope_id: None, maximum_matches: 5 })
    .unwrap();
  for (file_key, revision) in &identities {
    push_match(&mut sink, file_key, revision).unwrap();
  }
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: input.selected_namespace_root(),
      scope_id: None,
      match_count: 5,
      examined_documents: 5,
      examined_field_values: 9,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  assert_eq!(result.total_group_count(), 4, "source order and stable field states remain part of one complete group identity");
  assert_eq!(result.groups()[0].document_count(), 2);
  let mut tuples = result.groups().iter().map(|group| group.canonical_group_tuple()).collect::<Vec<_>>();
  tuples.sort_unstable();
  tuples.dedup();
  assert_eq!(tuples.len(), 4);
  assert_eq!(result.aggregate_values_examined(), 9);
  assert!(
    result
      .groups()
      .iter()
      .filter(|group| result.group_value_by_tuple(group.canonical_group_tuple(), "@size", QueryAggregateKindV1::Average)
        == Some(QueryAggregateReducedValueRefV1::Empty))
      .count()
      >= 2
  );
  drop(result);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn grouped_reducer_preserves_declared_multi_field_tuple_order_and_identity() {
  let algorithm = HashAlgorithm::Blake3_256;
  let plan = multi_field_group_plan(algorithm);
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  assert_eq!(input.fields().iter().map(|field| field.field_name()).collect::<Vec<_>>(), ["@size", "@updated_at"]);

  let identities = (1..=4).map(|seed| identity(algorithm, seed)).collect::<Vec<_>>();
  let rows = [(10u64, 20i64), (10, 20), (10, 30), (20, 20)];
  let results = identities
    .iter()
    .zip(rows)
    .map(|((file_key, revision), (size, updated_at))| Ok(found_two_field_row(&input, file_key, revision, size, updated_at)))
    .collect();
  let mut source = QueueAggregateSource { results, calls: 0 };
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink = QueryGroupedAggregateSinkV1::new(
    &input,
    &mut source,
    &memory,
    &cancellation,
    QueryGroupedAggregateLimitsV1::new(8, 1 << 20, 16 << 20).unwrap(),
  )
  .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: input.selected_namespace_root(), scope_id: None, maximum_matches: 4 })
    .unwrap();
  for (file_key, revision) in &identities {
    push_match(&mut sink, file_key, revision).unwrap();
  }
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: input.selected_namespace_root(),
      scope_id: None,
      match_count: 4,
      examined_documents: 4,
      examined_field_values: 8,
    })
    .unwrap();

  let result = sink.finish().unwrap();
  assert_eq!(result.group_fields().iter().map(|field| field.field_name()).collect::<Vec<_>>(), ["@updated_at", "@size"]);
  assert_eq!(result.total_document_count(), 4);
  assert_eq!(result.total_group_count(), 3);
  assert_eq!(result.groups().len(), 3);
  assert_eq!(result.groups()[0].document_count(), 2);
  assert_eq!(&result.groups()[0].canonical_group_tuple()[..4], b"AGTP");
  assert_eq!(u16::from_le_bytes(result.groups()[0].canonical_group_tuple()[6..8].try_into().unwrap()), 2);
  assert_eq!(u16::from_le_bytes(result.groups()[0].canonical_group_tuple()[20..22].try_into().unwrap()), 5);
  assert_eq!(u16::from_le_bytes(result.groups()[0].canonical_group_tuple()[48..50].try_into().unwrap()), 4);
  assert_eq!(
    result.group_value(0, "@size", QueryAggregateKindV1::Average),
    Some(QueryAggregateReducedValueRefV1::Numeric(QueryAggregateNumericV1::UnsignedRatio { numerator: 20, denominator: 2 }))
  );
  assert_ne!(result.groups()[0].canonical_group_tuple(), result.groups()[1].canonical_group_tuple());
  assert_ne!(result.groups()[0].canonical_group_tuple(), result.groups()[2].canonical_group_tuple());
  assert_eq!(source.calls, 4);
  drop(result);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn grouped_reducer_rolls_back_distinct_group_tuple_and_cancellation_pressure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let plan = aggregate_plan_for_limit(algorithm, "u64_order_v1", &[QueryAggregateKindV1::Average], true, 1);
  let input = CompiledQueryAggregateInputV1::from_plan(&plan, QueryAggregateInputLimitsV1::new(32, 8, 32, 1 << 20).unwrap()).unwrap();
  let first = identity(algorithm, 1);
  let second = identity(algorithm, 2);
  let row = |identity: &(Vec<u8>, Vec<u8>), value: u64| {
    found_row(
      &input,
      &identity.0,
      &identity.1,
      vec![LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, value.to_le_bytes().to_vec())],
    )
  };
  let mut source = QueueAggregateSource { results: VecDeque::from([Ok(row(&first, 1)), Ok(row(&second, 2))]), calls: 0 };
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink = QueryGroupedAggregateSinkV1::new(
    &input,
    &mut source,
    &memory,
    &cancellation,
    QueryGroupedAggregateLimitsV1::new(1, 1 << 20, 8 << 20).unwrap(),
  )
  .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: input.selected_namespace_root(), scope_id: None, maximum_matches: 2 })
    .unwrap();
  push_match(&mut sink, &first.0, &first.1).unwrap();
  let error = push_match(&mut sink, &second.0, &second.1).unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_aggregate_group_limit");
  sink.rollback_batch();
  drop(sink);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let mut source = QueueAggregateSource { results: VecDeque::from([Ok(row(&first, 1))]), calls: 0 };
  let cancellation = CancellationToken::new();
  let mut sink = QueryGroupedAggregateSinkV1::new(
    &input,
    &mut source,
    &memory,
    &cancellation,
    QueryGroupedAggregateLimitsV1::new(1, 8, 8 << 20).unwrap(),
  )
  .unwrap();
  sink
    .begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: input.selected_namespace_root(), scope_id: None, maximum_matches: 1 })
    .unwrap();
  let error = push_match(&mut sink, &first.0, &first.1).unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_aggregate_group_tuple_limit");
  sink.rollback_batch();
  cancellation.cancel();
  assert_eq!(
    sink
      .begin_batch(QueryExecutionSinkBatchV1 {
        selected_namespace_root: input.selected_namespace_root(),
        scope_id: None,
        maximum_matches: 1,
      })
      .unwrap_err()
      .class(),
    QueryExecutionSinkErrorClassV1::Cancelled
  );
  drop(sink);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}
