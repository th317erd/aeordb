use std::cmp::Ordering;
use std::collections::BTreeMap;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_coverage_planner::IndexSemanticQueryAvailabilityV1;
use aeordb::engine::v4::position::{PositionComparatorV1, PositionRouteV1};
use aeordb::engine::v4::position_order::{LogicalOrderComponentOwnedV1, LogicalOrderRowOwnedV1};
use aeordb::engine::v4::position_resolver::{
  PositionUniverseLookupRequestV1, PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1, PositionUniverseSourceV1,
};
use aeordb::engine::v4::query_executor::{
  QueryExecutionMatchPathV1, QueryExecutionMatchRefV1, QueryExecutionMatchSinkV1, QueryExecutionSinkBatchReceiptV1,
  QueryExecutionSinkBatchV1, QueryExecutionSinkErrorClassV1,
};
use aeordb::engine::v4::query_order_execution::{QueryOrderedTopKLimitsV1, QueryOrderedTopKSinkV1};
use aeordb::engine::v4::query_planner::{
  QueryExpressionV1, QueryPlanningContextV1, QueryPlanningIndexCandidateV1, QueryPlanningIndexEstimatesV1, QueryPlanningRequestV1,
  QueryPlanningScopeV1, QuerySortDirectionV1, QuerySortFieldV1, RootAwareQueryFieldCatalogV1, default_query_planning_limits_v1,
  plan_root_aware_query_v1,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;
use tokio_util::sync::CancellationToken;

const DATABASE_ID: [u8; 16] = [0x11; 16];
const PHYSICAL_INSTANCE_ID: [u8; 16] = [0x22; 16];
const ROOT: [u8; 32] = [0x33; 32];

fn fixture(path: &str) -> Vec<u8> {
  std::fs::read(format!("{}/spec/fixtures/v4/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn algorithm_name(algorithm: HashAlgorithm) -> &'static str {
  match algorithm {
    HashAlgorithm::Blake3_256 => "blake3-256",
    HashAlgorithm::Sha512 => "sha512",
    _ => panic!("query order fixture does not cover {algorithm:?}"),
  }
}

fn ordered_plan_for(
  algorithm: HashAlgorithm,
  result_limit: usize,
  direction: QuerySortDirectionV1,
) -> aeordb::engine::v4::query_planner::CompiledRootAwareQueryPlanV1 {
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
  let total_length = value_store.len() as u32;
  value_store[8..12].copy_from_slice(&total_length.to_le_bytes());
  value_store[fixed_start..fixed_start + 4].copy_from_slice(&(field_name.len() as u32).to_le_bytes());
  let selector_start = field_start + field_name.len();
  value_store[selector_start + 32..selector_start + 34].copy_from_slice(&5u16.to_le_bytes());
  let value_definition = decode_value_store_definition(&value_store, algorithm).unwrap();

  let mut field = fixture(&format!("field-index-definition-v1/afix-{algorithm_name}-u64_order_v1-valid.bin"));
  field[32..32 + hash_width].copy_from_slice(&value_definition.value_store_id);
  let field_definition = decode_field_index_definition(&field, algorithm).unwrap();
  let scope = QueryPlanningScopeV1 {
    scope_id: scope_definition.scope_id,
    value_store_id: value_definition.value_store_id,
    encoded_scope_definition: encoded_scope,
    encoded_value_store_definition: value_store,
    semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
    authoritative_document_count: 100,
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
    selected_namespace_root: vec![0x33; hash_width],
    semantic_state_root: vec![0x44; hash_width],
    publication_sequence: 41,
    field_name: field_name.to_string(),
    complete: true,
    scopes: vec![scope],
  }];
  let root = vec![0x33; hash_width];
  let semantic_root = vec![0x44; hash_width];
  let context = QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &root, &semantic_root, 41).unwrap();
  let expression = QueryExpressionV1::And(Vec::new());
  let sort_fields = [QuerySortFieldV1 { field_name: field_name.to_string(), direction }];
  plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &sort_fields,
    aggregate_fields: &[],
    group_fields: &[],
    result_limit,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap()
}

fn ordered_plan(result_limit: usize, direction: QuerySortDirectionV1) -> aeordb::engine::v4::query_planner::CompiledRootAwareQueryPlanV1 {
  ordered_plan_for(HashAlgorithm::Blake3_256, result_limit, direction)
}

fn memory(hard_limit: u64) -> MemoryCoordinator {
  MemoryCoordinator::new(MemoryPolicy::new(hard_limit - (1 << 20), hard_limit, 1, 1 << 20).unwrap())
}

#[derive(Clone)]
struct MatchFixture {
  file_key: Vec<u8>,
  revision: Vec<u8>,
  path: String,
}

fn order_row(value: u64, path: &str, revision_seed: u8) -> (MatchFixture, LogicalOrderRowOwnedV1) {
  let file_key = digest_parts(HashAlgorithm::Blake3_256, &[b"file:", path.as_bytes()]);
  let revision = vec![revision_seed; 32];
  let matched = MatchFixture { file_key: file_key.clone(), revision: revision.clone(), path: path.to_string() };
  let row = LogicalOrderRowOwnedV1 {
    route: PositionRouteV1::Query,
    components: vec![
      LogicalOrderComponentOwnedV1::present(PositionComparatorV1::U64, value.to_le_bytes().to_vec()),
      LogicalOrderComponentOwnedV1::present(PositionComparatorV1::Utf8Binary, path.as_bytes().to_vec()),
    ],
    file_key_tie: file_key,
    record_revision_tie: revision,
  };
  (matched, row)
}

#[derive(Default)]
struct ModelPositionSource {
  rows: BTreeMap<Vec<u8>, LogicalOrderRowOwnedV1>,
  error: Option<PositionUniverseSourceErrorV1>,
  calls: usize,
  cancel_on_call: Option<usize>,
}

impl PositionUniverseSourceV1 for ModelPositionSource {
  fn resolve_position(
    &mut self,
    request: PositionUniverseLookupRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1> {
    self.calls += 1;
    if self.cancel_on_call == Some(self.calls) {
      cancellation.cancel();
    }
    if let Some(error) = self.error.clone() {
      return Err(error);
    }
    Ok(self.rows.get(request.file_key_tie()).cloned().map_or(PositionUniverseLookupResultV1::Absent, PositionUniverseLookupResultV1::Found))
  }
}

#[derive(Clone, Debug)]
enum RawModelValue {
  Present(Vec<u64>),
  TypedNull,
  Missing,
}

impl RawModelValue {
  const fn state_rank(&self) -> u8 {
    match self {
      Self::Present(_) => 0,
      Self::TypedNull => 1,
      Self::Missing => 2,
    }
  }

  fn selected(&self, direction: QuerySortDirectionV1) -> Option<u64> {
    let Self::Present(values) = self else {
      return None;
    };
    match direction {
      QuerySortDirectionV1::Ascending => values.iter().copied().min(),
      QuerySortDirectionV1::Descending => values.iter().copied().max(),
    }
  }
}

#[derive(Clone)]
struct RawModelRow {
  matched: MatchFixture,
  value: RawModelValue,
}

struct SelectingPositionSource {
  algorithm: HashAlgorithm,
  selected_root: Vec<u8>,
  direction: QuerySortDirectionV1,
  rows: BTreeMap<(Vec<u8>, Vec<u8>), RawModelRow>,
  calls: usize,
}

impl PositionUniverseSourceV1 for SelectingPositionSource {
  fn resolve_position(
    &mut self,
    request: PositionUniverseLookupRequestV1<'_>,
    _cancellation: &CancellationToken,
  ) -> Result<PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1> {
    self.calls += 1;
    assert_eq!(request.database_id(), DATABASE_ID);
    assert_eq!(request.physical_instance_id(), PHYSICAL_INSTANCE_ID);
    assert_eq!(request.selected_root(), self.selected_root);
    assert_eq!(request.order().hash_algorithm(), self.algorithm);
    assert_eq!(request.route(), PositionRouteV1::Query);
    assert!(!request.order_fingerprint().iter().all(|byte| *byte == 0));
    assert!(request.maximum_row_bytes() >= 128);

    let Some(raw) = self.rows.get(&(request.file_key_tie().to_vec(), request.record_revision_tie().to_vec())) else {
      return Ok(PositionUniverseLookupResultV1::Absent);
    };
    let value = match &raw.value {
      RawModelValue::Present(_) => LogicalOrderComponentOwnedV1::present(
        PositionComparatorV1::U64,
        raw.value.selected(self.direction).expect("nonempty model values").to_le_bytes().to_vec(),
      ),
      RawModelValue::TypedNull => LogicalOrderComponentOwnedV1::typed_null(),
      RawModelValue::Missing => LogicalOrderComponentOwnedV1::missing(),
    };
    Ok(PositionUniverseLookupResultV1::Found(LogicalOrderRowOwnedV1 {
      route: PositionRouteV1::Query,
      components: vec![
        value,
        LogicalOrderComponentOwnedV1::present(PositionComparatorV1::Utf8Binary, raw.matched.path.as_bytes().to_vec()),
      ],
      file_key_tie: raw.matched.file_key.clone(),
      record_revision_tie: raw.matched.revision.clone(),
    }))
  }
}

fn raw_model_row(algorithm: HashAlgorithm, path: &str, revision_seed: u8, value: RawModelValue) -> RawModelRow {
  let file_key = digest_parts(algorithm, &[b"file:", path.as_bytes()]);
  let matched = MatchFixture { file_key, revision: vec![revision_seed; algorithm.hash_length()], path: path.to_string() };
  RawModelRow { matched, value }
}

fn oracle_compare(direction: QuerySortDirectionV1, left: &RawModelRow, right: &RawModelRow) -> Ordering {
  left
    .value
    .state_rank()
    .cmp(&right.value.state_rank())
    .then_with(|| match (left.value.selected(direction), right.value.selected(direction)) {
      (Some(left), Some(right)) if direction == QuerySortDirectionV1::Ascending => left.cmp(&right),
      (Some(left), Some(right)) => right.cmp(&left),
      _ => Ordering::Equal,
    })
    .then_with(|| left.matched.path.cmp(&right.matched.path))
    .then_with(|| left.matched.file_key.cmp(&right.matched.file_key))
    .then_with(|| left.matched.revision.cmp(&right.matched.revision))
}

fn push(sink: &mut dyn QueryExecutionMatchSinkV1, matched: &MatchFixture, path: QueryExecutionMatchPathV1<'_>) {
  sink.push_match(QueryExecutionMatchRefV1 { file_key: &matched.file_key, record_revision: &matched.revision, path }).unwrap();
}

#[test]
fn transactional_top_k_retains_only_the_best_selected_root_rows() {
  let plan = ordered_plan(2, QuerySortDirectionV1::Ascending);
  let (nine, nine_row) = order_row(9, "/nine", 9);
  let (one, one_row) = order_row(1, "/one", 1);
  let (five, five_row) = order_row(5, "/five", 5);
  let mut source = ModelPositionSource {
    rows: BTreeMap::from([(nine.file_key.clone(), nine_row), (one.file_key.clone(), one_row), (five.file_key.clone(), five_row)]),
    ..ModelPositionSource::default()
  };
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink =
    QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap())
      .unwrap();

  sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 100 }).unwrap();
  push(&mut sink, &nine, QueryExecutionMatchPathV1::Canonical(&nine.path));
  sink.rollback_batch();

  sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 100 }).unwrap();
  push(&mut sink, &nine, QueryExecutionMatchPathV1::Canonical(&nine.path));
  push(&mut sink, &one, QueryExecutionMatchPathV1::RequiresSelectedRootLookup);
  push(&mut sink, &five, QueryExecutionMatchPathV1::Canonical(&five.path));
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: &ROOT,
      scope_id: None,
      match_count: 3,
      examined_documents: 7,
      examined_field_values: 11,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  assert_eq!(result.total_match_count(), 3);
  assert!(result.has_more());
  assert_eq!(result.examined_documents(), 7);
  assert_eq!(result.rows().len(), 2);
  assert_eq!(result.rows()[0].components[0].payload, 1u64.to_le_bytes());
  assert_eq!(result.rows()[1].components[0].payload, 5u64.to_le_bytes());
  assert!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes > 0);
  drop(result);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  assert_eq!(source.calls, 4, "rolled-back and committed batches must both resolve exact selected-root rows");
}

#[test]
fn top_k_preserves_selected_root_failure_classes_and_never_commits_bad_rows() {
  let plan = ordered_plan(1, QuerySortDirectionV1::Descending);
  let (matched, row) = order_row(7, "/seven", 7);

  for (source_error, expected) in [
    (
      Some(PositionUniverseSourceErrorV1::unavailable("historical source missing")),
      QueryExecutionSinkErrorClassV1::HistoricalViewUnavailable,
    ),
    (Some(PositionUniverseSourceErrorV1::corrupt("selected row corrupt")), QueryExecutionSinkErrorClassV1::CorruptSource),
    (Some(PositionUniverseSourceErrorV1::cancelled()), QueryExecutionSinkErrorClassV1::Cancelled),
    (None, QueryExecutionSinkErrorClassV1::CorruptSource),
  ] {
    let mut source = ModelPositionSource { rows: BTreeMap::new(), error: source_error, calls: 0, cancel_on_call: None };
    let memory = memory(64 << 20);
    let cancellation = CancellationToken::new();
    let mut sink =
      QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap())
        .unwrap();
    sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 10 }).unwrap();
    let error = sink
      .push_match(QueryExecutionMatchRefV1 {
        file_key: &matched.file_key,
        record_revision: &matched.revision,
        path: QueryExecutionMatchPathV1::Canonical(&matched.path),
      })
      .unwrap_err();
    assert_eq!(error.class(), expected);
    sink.rollback_batch();
    drop(sink);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }

  let mut source = ModelPositionSource { rows: BTreeMap::from([(matched.file_key.clone(), row)]), ..ModelPositionSource::default() };
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink =
    QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap())
      .unwrap();
  sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 10 }).unwrap();
  let error = sink
    .push_match(QueryExecutionMatchRefV1 {
      file_key: &matched.file_key,
      record_revision: &matched.revision,
      path: QueryExecutionMatchPathV1::Canonical("/stale"),
    })
    .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::CorruptSource);
  sink.rollback_batch();
}

#[test]
fn top_k_refuses_memory_and_row_bounds_without_leaking_query_reservations() {
  let plan = ordered_plan(2, QuerySortDirectionV1::Ascending);
  let mut source = ModelPositionSource::default();
  let constrained = memory(4 << 20);
  let cancellation = CancellationToken::new();
  let error = match QueryOrderedTopKSinkV1::new(
    &plan,
    &mut source,
    &constrained,
    &cancellation,
    QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap(),
  ) {
    Ok(_) => panic!("oversized top-K reservation unexpectedly succeeded"),
    Err(error) => error,
  };
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::ResourceLimit);
  assert_eq!(constrained.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let (matched, row) = order_row(1, "/one", 1);
  source.rows.insert(matched.file_key.clone(), row);
  let memory = memory(64 << 20);
  let mut sink =
    QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(32, 8 << 20).unwrap()).unwrap();
  sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 10 }).unwrap();
  let error = sink
    .push_match(QueryExecutionMatchRefV1 {
      file_key: &matched.file_key,
      record_revision: &matched.revision,
      path: QueryExecutionMatchPathV1::RequiresSelectedRootLookup,
    })
    .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::ResourceLimit);
  sink.rollback_batch();
  drop(sink);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn failed_top_k_commit_is_rollbackable_and_scope_identity_survives_retry() {
  let plan = ordered_plan(1, QuerySortDirectionV1::Ascending);
  let (matched, row) = order_row(7, "/seven", 7);
  let mut source = ModelPositionSource { rows: BTreeMap::from([(matched.file_key.clone(), row)]), ..ModelPositionSource::default() };
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let scope = [0x55; 32];
  let mut sink =
    QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap())
      .unwrap();

  sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: Some(&scope), maximum_matches: 10 }).unwrap();
  push(&mut sink, &matched, QueryExecutionMatchPathV1::Canonical(&matched.path));
  let error = sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: &ROOT,
      scope_id: Some(&scope),
      match_count: 0,
      examined_documents: 1,
      examined_field_values: 2,
    })
    .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::Internal);
  sink.rollback_batch();

  sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: Some(&scope), maximum_matches: 10 }).unwrap();
  push(&mut sink, &matched, QueryExecutionMatchPathV1::Canonical(&matched.path));
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: &ROOT,
      scope_id: Some(&scope),
      match_count: 1,
      examined_documents: 3,
      examined_field_values: 5,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  assert_eq!(result.scope_id(), Some(scope.as_slice()));
  assert_eq!(result.examined_field_values(), 5);
  assert_eq!(result.rows().len(), 1);
  drop(result);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  assert_eq!(source.calls, 2);
}

#[test]
fn empty_top_k_commits_and_cancelled_or_uncommitted_sinks_release_memory() {
  let plan = ordered_plan(2, QuerySortDirectionV1::Ascending);
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut source = ModelPositionSource::default();
  let mut sink =
    QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap())
      .unwrap();
  sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 10 }).unwrap();
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: &ROOT,
      scope_id: None,
      match_count: 0,
      examined_documents: 0,
      examined_field_values: 0,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  assert!(result.rows().is_empty());
  assert_eq!(result.total_match_count(), 0);
  assert!(!result.has_more());
  drop(result);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let mut sink =
    QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap())
      .unwrap();
  let error =
    sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 10 }).unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::Cancelled);
  drop(sink);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let cancellation = CancellationToken::new();
  let sink =
    QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap())
      .unwrap();
  let error = sink.finish().unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::Internal);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn bounded_top_k_matches_an_independent_full_sort_for_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    for direction in [QuerySortDirectionV1::Ascending, QuerySortDirectionV1::Descending] {
      let plan = ordered_plan_for(algorithm, 5, direction);
      let rows = vec![
        raw_model_row(algorithm, "/same", 1, RawModelValue::Present(vec![7, 90, 3])),
        raw_model_row(algorithm, "/same", 2, RawModelValue::Present(vec![7, 90, 3])),
        raw_model_row(algorithm, "/zero", 3, RawModelValue::Present(vec![u64::MAX, 0, 11])),
        raw_model_row(algorithm, "/maximum", 4, RawModelValue::Present(vec![u64::MAX])),
        raw_model_row(algorithm, "/forty-two", 5, RawModelValue::Present(vec![42, 42])),
        raw_model_row(algorithm, "/null-a", 6, RawModelValue::TypedNull),
        raw_model_row(algorithm, "/null-b", 7, RawModelValue::TypedNull),
        raw_model_row(algorithm, "/missing", 8, RawModelValue::Missing),
      ];
      let mut expected = rows.clone();
      expected.sort_by(|left, right| oracle_compare(direction, left, right));
      let expected: Vec<_> =
        expected.iter().take(plan.result_limit()).map(|row| (row.matched.file_key.clone(), row.matched.revision.clone())).collect();
      let mut rotated: Vec<_> = (0..rows.len()).collect();
      rotated.rotate_left(3);
      let permutations = [(0..rows.len()).collect::<Vec<_>>(), (0..rows.len()).rev().collect(), rotated];

      for permutation in permutations {
        let mut source = SelectingPositionSource {
          algorithm,
          selected_root: plan.selected_namespace_root().to_vec(),
          direction,
          rows: rows.iter().cloned().map(|row| ((row.matched.file_key.clone(), row.matched.revision.clone()), row)).collect(),
          calls: 0,
        };
        let memory = memory(64 << 20);
        let cancellation = CancellationToken::new();
        let scope = vec![0x91; algorithm.hash_length()];
        let mut sink =
          QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap())
            .unwrap();
        sink
          .begin_batch(QueryExecutionSinkBatchV1 {
            selected_namespace_root: plan.selected_namespace_root(),
            scope_id: Some(&scope),
            maximum_matches: rows.len() as u64,
          })
          .unwrap();
        for (ordinal, index) in permutation.into_iter().enumerate() {
          let row = &rows[index];
          let path = if ordinal % 2 == 0 {
            QueryExecutionMatchPathV1::Canonical(&row.matched.path)
          } else {
            QueryExecutionMatchPathV1::RequiresSelectedRootLookup
          };
          push(&mut sink, &row.matched, path);
        }
        sink
          .commit_batch(QueryExecutionSinkBatchReceiptV1 {
            selected_namespace_root: plan.selected_namespace_root(),
            scope_id: Some(&scope),
            match_count: rows.len() as u64,
            examined_documents: 101,
            examined_field_values: 202,
          })
          .unwrap();
        let result = sink.finish().unwrap();
        let actual: Vec<_> = result.rows().iter().map(|row| (row.file_key_tie.clone(), row.record_revision_tie.clone())).collect();
        assert_eq!(actual, expected, "{algorithm:?} {direction:?}");
        assert_eq!(result.selected_namespace_root(), plan.selected_namespace_root());
        assert_eq!(result.scope_id(), Some(scope.as_slice()));
        assert_eq!(result.total_match_count(), rows.len() as u64);
        assert!(result.has_more());
        assert_eq!(source.calls, rows.len());
        assert_eq!(
          memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes,
          result.retained_bytes(),
          "the committed result must retain exactly the bytes it reports"
        );
        drop(result);
        assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
      }
    }
  }
}

#[test]
fn malformed_selected_root_rows_and_mid_lookup_cancellation_fail_closed() {
  let plan = ordered_plan(1, QuerySortDirectionV1::Ascending);
  let (matched, valid) = order_row(7, "/seven", 7);
  let mut malformed = Vec::new();

  let mut wrong_key = valid.clone();
  wrong_key.file_key_tie[0] ^= 1;
  malformed.push(wrong_key);
  let mut wrong_revision = valid.clone();
  wrong_revision.record_revision_tie[0] ^= 1;
  malformed.push(wrong_revision);
  let mut invalid_numeric = valid.clone();
  invalid_numeric.components[0].payload.pop();
  malformed.push(invalid_numeric);
  let mut stale_path_identity = valid.clone();
  stale_path_identity.components[1].payload = b"/another-root-path".to_vec();
  malformed.push(stale_path_identity);
  let mut wrong_width = valid.clone();
  wrong_width.file_key_tie.push(0x99);
  malformed.push(wrong_width);

  for row in malformed {
    let mut source = ModelPositionSource { rows: BTreeMap::from([(matched.file_key.clone(), row)]), ..ModelPositionSource::default() };
    let memory = memory(64 << 20);
    let cancellation = CancellationToken::new();
    let mut sink =
      QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap())
        .unwrap();
    sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 1 }).unwrap();
    let error = sink
      .push_match(QueryExecutionMatchRefV1 {
        file_key: &matched.file_key,
        record_revision: &matched.revision,
        path: QueryExecutionMatchPathV1::RequiresSelectedRootLookup,
      })
      .unwrap_err();
    assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::CorruptSource);
    sink.rollback_batch();
    drop(sink);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }

  let mut source = ModelPositionSource {
    rows: BTreeMap::from([(matched.file_key.clone(), valid)]),
    cancel_on_call: Some(1),
    ..ModelPositionSource::default()
  };
  let memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut sink =
    QueryOrderedTopKSinkV1::new(&plan, &mut source, &memory, &cancellation, QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap())
      .unwrap();
  sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 1 }).unwrap();
  let error = sink
    .push_match(QueryExecutionMatchRefV1 {
      file_key: &matched.file_key,
      record_revision: &matched.revision,
      path: QueryExecutionMatchPathV1::RequiresSelectedRootLookup,
    })
    .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::Cancelled);
  sink.rollback_batch();
  drop(sink);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn stale_commit_receipts_never_publish_and_cumulative_pressure_releases_exactly() {
  let plan = ordered_plan(1, QuerySortDirectionV1::Ascending);
  let (matched, row) = order_row(7, "/seven", 7);
  let mut source = ModelPositionSource { rows: BTreeMap::from([(matched.file_key.clone(), row)]), ..ModelPositionSource::default() };
  let receipt_memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let scope = [0x55; 32];
  let stale_root = [0x99; 32];
  let stale_scope = [0x66; 32];
  let mut sink = QueryOrderedTopKSinkV1::new(
    &plan,
    &mut source,
    &receipt_memory,
    &cancellation,
    QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap(),
  )
  .unwrap();
  sink.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: Some(&scope), maximum_matches: 1 }).unwrap();
  push(&mut sink, &matched, QueryExecutionMatchPathV1::Canonical(&matched.path));
  for receipt in [
    QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: &stale_root,
      scope_id: Some(&scope),
      match_count: 1,
      examined_documents: 1,
      examined_field_values: 1,
    },
    QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: &ROOT,
      scope_id: Some(&stale_scope),
      match_count: 1,
      examined_documents: 1,
      examined_field_values: 1,
    },
    QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: &ROOT,
      scope_id: Some(&scope),
      match_count: 0,
      examined_documents: 1,
      examined_field_values: 1,
    },
  ] {
    assert_eq!(sink.commit_batch(receipt).unwrap_err().class(), QueryExecutionSinkErrorClassV1::Internal);
  }
  sink
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: &ROOT,
      scope_id: Some(&scope),
      match_count: 1,
      examined_documents: 2,
      examined_field_values: 3,
    })
    .unwrap();
  let result = sink.finish().unwrap();
  assert_eq!(result.rows().len(), 1);
  drop(result);
  assert_eq!(receipt_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let plan = ordered_plan(2, QuerySortDirectionV1::Ascending);
  let (one, one_row) = order_row(1, "/one", 1);
  let (two, two_row) = order_row(2, "/two", 2);
  let mut probe_source =
    ModelPositionSource { rows: BTreeMap::from([(one.file_key.clone(), one_row.clone())]), ..ModelPositionSource::default() };
  let pressure_memory = memory(64 << 20);
  let cancellation = CancellationToken::new();
  let mut probe = QueryOrderedTopKSinkV1::new(
    &plan,
    &mut probe_source,
    &pressure_memory,
    &cancellation,
    QueryOrderedTopKLimitsV1::new(1 << 20, 8 << 20).unwrap(),
  )
  .unwrap();
  probe.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 2 }).unwrap();
  push(&mut probe, &one, QueryExecutionMatchPathV1::Canonical(&one.path));
  probe
    .commit_batch(QueryExecutionSinkBatchReceiptV1 {
      selected_namespace_root: &ROOT,
      scope_id: None,
      match_count: 1,
      examined_documents: 1,
      examined_field_values: 1,
    })
    .unwrap();
  let exact_one_row_bytes = probe.finish().unwrap().retained_bytes();
  assert_eq!(pressure_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let mut pressured_source = ModelPositionSource {
    rows: BTreeMap::from([(one.file_key.clone(), one_row), (two.file_key.clone(), two_row)]),
    ..ModelPositionSource::default()
  };
  let mut pressured = QueryOrderedTopKSinkV1::new(
    &plan,
    &mut pressured_source,
    &pressure_memory,
    &cancellation,
    QueryOrderedTopKLimitsV1::new(1 << 20, exact_one_row_bytes).unwrap(),
  )
  .unwrap();
  pressured.begin_batch(QueryExecutionSinkBatchV1 { selected_namespace_root: &ROOT, scope_id: None, maximum_matches: 2 }).unwrap();
  push(&mut pressured, &one, QueryExecutionMatchPathV1::Canonical(&one.path));
  let error = pressured
    .push_match(QueryExecutionMatchRefV1 {
      file_key: &two.file_key,
      record_revision: &two.revision,
      path: QueryExecutionMatchPathV1::Canonical(&two.path),
    })
    .unwrap_err();
  assert_eq!(error.class(), QueryExecutionSinkErrorClassV1::ResourceLimit);
  pressured.rollback_batch();
  drop(pressured);
  assert_eq!(pressure_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn top_k_execution_has_one_storage_neutral_authority_path() {
  let source = include_str!("../../src/engine/v4/query_order_execution.rs");
  assert_eq!(source.matches("impl QueryExecutionMatchSinkV1 for QueryOrderedTopKSinkV1").count(), 1);
  assert_eq!(source.matches("resolve_position_universe_row_v1(request").count(), 1);
  assert_eq!(source.matches("compare_logical_order_rows_v1(order").count(), 1);
  for forbidden in ["StorageEngine", "storage_engine", "server::", "tokio::spawn", "std::thread", "index_artifact_native"] {
    assert!(!source.contains(forbidden), "top-K execution must remain storage-neutral: {forbidden}");
  }
}
