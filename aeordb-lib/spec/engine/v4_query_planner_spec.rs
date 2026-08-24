use aeordb::engine::HashAlgorithm;
use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::index_coverage_planner::{IndexCoverageGenerationHealthV1, IndexSemanticQueryAvailabilityV1};
use aeordb::engine::v4::index_coverage_registry::{field_definition_fingerprint, field_dependency_fingerprint};
use aeordb::engine::v4::query_planner::{
  CompiledQueryAuxiliaryOperationV1, CompiledQueryCoverageV1, QueryAggregateFieldV1, QueryAggregateKindV1, QueryCoordinateConstraintV1,
  QueryExpressionV1, QueryFuzzyAlgorithmV1, QueryLogicalDriverKindV1, QueryPlanDriverV1, QueryPlanningContextV1,
  QueryPlanningCoverageGenerationV1, QueryPlanningErrorClassV1, QueryPlanningIndexCandidateV1, QueryPlanningIndexEstimatesV1,
  QueryPlanningLimitsV1, QueryPlanningRequestV1, QueryPlanningScopeV1, QueryPredicateOperationV1, QueryPredicateV1, QuerySortDirectionV1,
  QuerySortFieldV1, QueryValueMatchV1, RootAwareQueryFieldCatalogV1, authorization_safe_query_explain_v1, default_query_planning_limits_v1,
  plan_root_aware_query_v1,
};
use aeordb::engine::v4::position::{PositionComparatorV1, PositionRouteV1};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::value_store::decode_value_store_definition;

const DATABASE_ID: [u8; 16] = [0x11; 16];
const PHYSICAL_INSTANCE_ID: [u8; 16] = [0x22; 16];
const ROOT: [u8; 32] = [0x33; 32];
const SEMANTIC_ROOT: [u8; 32] = [0x44; 32];

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

fn definitions(field_name: &str, converter: &str) -> (Vec<u8>, Vec<u8>) {
  definitions_for_scope(field_name, converter, &scope_fixture())
}

fn definitions_for_scope(field_name: &str, converter: &str, encoded_scope: &[u8]) -> (Vec<u8>, Vec<u8>) {
  let mut value_store = value_store_fixture("avst-blake3-256-metadata-hash-corrected-valid");
  let scope = decode_scope_definition(encoded_scope, HashAlgorithm::Blake3_256).unwrap();
  value_store[32..64].copy_from_slice(&scope.scope_id);
  let metadata_id = match field_name {
    "@filename" => 2u16,
    "@size" => 5u16,
    "@hash" => 8u16,
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
  assert_eq!(value_definition.field_name, field_name);

  let mut field = field_fixture(converter);
  field[32..64].copy_from_slice(&value_definition.value_store_id);
  let field_definition = decode_field_index_definition(&field, HashAlgorithm::Blake3_256).unwrap();
  assert_eq!(field_definition.value_store_id, value_definition.value_store_id);
  (value_store, field)
}

fn context() -> QueryPlanningContextV1<'static> {
  QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, HashAlgorithm::Blake3_256, &ROOT, &SEMANTIC_ROOT, 41).unwrap()
}

fn generation(
  scope: &[u8],
  value_store: &[u8],
  field: &[u8],
  source_root: &[u8],
  publication_sequence: u64,
) -> QueryPlanningCoverageGenerationV1 {
  let scope_definition = decode_scope_definition(scope, HashAlgorithm::Blake3_256).unwrap();
  let value_definition = decode_value_store_definition(value_store, HashAlgorithm::Blake3_256).unwrap();
  let field_definition = decode_field_index_definition(field, HashAlgorithm::Blake3_256).unwrap();
  QueryPlanningCoverageGenerationV1 {
    generation: 7,
    owner_id: field_definition.index_id,
    manifest_hash: vec![0x71; 32],
    source_namespace_root: source_root.to_vec(),
    coverage_epoch_id: [0x72; 16],
    coverage_publication_sequence: publication_sequence,
    definition_fingerprint: field_definition_fingerprint(HashAlgorithm::Blake3_256, field),
    dependency_fingerprint: field_dependency_fingerprint(
      HashAlgorithm::Blake3_256,
      &scope_definition.scope_id,
      &value_definition.value_store_id,
    ),
    health: IndexCoverageGenerationHealthV1::Healthy,
  }
}

fn candidate(
  field: Vec<u8>,
  selected_generation: Option<QueryPlanningCoverageGenerationV1>,
  estimates: QueryPlanningIndexEstimatesV1,
) -> QueryPlanningIndexCandidateV1 {
  let field_definition = decode_field_index_definition(&field, HashAlgorithm::Blake3_256).unwrap();
  QueryPlanningIndexCandidateV1 {
    index_id: field_definition.index_id,
    encoded_field_definition: field,
    selected_generation,
    estimates,
    nvt_hint_available: true,
  }
}

fn scope(value_store: Vec<u8>, indexes: Vec<QueryPlanningIndexCandidateV1>, authoritative_document_count: u64) -> QueryPlanningScopeV1 {
  scope_for_definition(scope_fixture(), value_store, indexes, authoritative_document_count)
}

fn scope_for_definition(
  encoded_scope_definition: Vec<u8>,
  value_store: Vec<u8>,
  indexes: Vec<QueryPlanningIndexCandidateV1>,
  authoritative_document_count: u64,
) -> QueryPlanningScopeV1 {
  let scope_definition = decode_scope_definition(&encoded_scope_definition, HashAlgorithm::Blake3_256).unwrap();
  let value_store_id = decode_value_store_definition(&value_store, HashAlgorithm::Blake3_256).unwrap().value_store_id;
  QueryPlanningScopeV1 {
    scope_id: scope_definition.scope_id,
    value_store_id,
    encoded_scope_definition,
    encoded_value_store_definition: value_store,
    semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
    authoritative_document_count,
    indexes,
  }
}

#[test]
fn query_order_is_compiled_from_selected_root_auxiliary_semantics() {
  let planning_context = context();
  let empty_expression = QueryExpressionV1::And(Vec::new());
  let default_plan =
    plan_root_aware_query_v1(&request(&planning_context, &empty_expression, &[], default_query_planning_limits_v1(), &|| false)).unwrap();
  assert_eq!(default_plan.query_order().route(), PositionRouteV1::Query);
  assert_eq!(default_plan.query_order().component_count(), 1);

  let encoded_scope = scope_fixture();
  let (value_store, field) = definitions("@size", "u64_order_v1");
  let generation = generation(&encoded_scope, &value_store, &field, &ROOT, 41);
  let query_scope =
    scope(value_store, vec![candidate(field, Some(generation), QueryPlanningIndexEstimatesV1::new(1, 10, 10, 2, 0).unwrap())], 10);
  let catalogs = [catalog("@size", vec![query_scope])];
  let sort_fields = [QuerySortFieldV1 { field_name: "@size".to_string(), direction: QuerySortDirectionV1::Descending }];
  let mut ordered_request = request(&planning_context, &empty_expression, &catalogs, default_query_planning_limits_v1(), &|| false);
  ordered_request.sort_fields = &sort_fields;
  let ordered = plan_root_aware_query_v1(&ordered_request).unwrap();
  let semantics = ordered.auxiliary_fields()[0].order_semantics();
  assert_eq!(semantics.comparator(), PositionComparatorV1::U64);
  assert_eq!(semantics.comparison_semantics(), 0x0004);
  assert_eq!(semantics.collation_semantics(), 0);
  assert!(semantics.behavior_fingerprint().iter().any(|byte| *byte != 0));
  assert_eq!(ordered.query_order().route(), PositionRouteV1::Query);
  assert_eq!(ordered.query_order().component_count(), 2);
  assert_ne!(ordered.query_order().fingerprint(), default_plan.query_order().fingerprint());
}

#[test]
fn ambiguous_or_cross_scope_auxiliary_comparators_are_rejected_before_execution() {
  let planning_context = context();
  let expression = QueryExpressionV1::And(Vec::new());
  let sort_fields = [QuerySortFieldV1 { field_name: "@size".to_string(), direction: QuerySortDirectionV1::Ascending }];
  let estimates = QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap();

  let (value_store, u64_field) = definitions("@size", "u64_order_v1");
  let (_, i64_field) = definitions("@size", "i64_order_v1");
  let one_scope = scope(value_store, vec![candidate(u64_field, None, estimates), candidate(i64_field, None, estimates)], 1);
  let one_scope_catalogs = [catalog("@size", vec![one_scope])];
  let mut ambiguous_request = request(&planning_context, &expression, &one_scope_catalogs, default_query_planning_limits_v1(), &|| false);
  ambiguous_request.sort_fields = &sort_fields;
  let error = plan_root_aware_query_v1(&ambiguous_request).unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::HistoricalViewUnavailable);
  assert_eq!(error.code(), "query_auxiliary_semantics_incompatible");

  let root_scope = scope_fixture();
  let glob_scope = fixture("scope-definition-v1/ascp-blake3-256-normalized-glob-valid.bin");
  let (root_value_store, root_field) = definitions_for_scope("@size", "u64_order_v1", &root_scope);
  let (compatible_glob_value_store, compatible_glob_field) = definitions_for_scope("@size", "u64_order_v1", &glob_scope);
  let mut compatible_scopes = vec![
    scope_for_definition(root_scope.clone(), root_value_store.clone(), vec![candidate(root_field.clone(), None, estimates)], 1),
    scope_for_definition(glob_scope.clone(), compatible_glob_value_store, vec![candidate(compatible_glob_field, None, estimates)], 1),
  ];
  compatible_scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let compatible_catalogs = [catalog("@size", compatible_scopes)];
  let mut compatible_request = request(&planning_context, &expression, &compatible_catalogs, default_query_planning_limits_v1(), &|| false);
  compatible_request.sort_fields = &sort_fields;
  assert_eq!(plan_root_aware_query_v1(&compatible_request).unwrap().query_order().component_count(), 2);

  let (glob_value_store, glob_field) = definitions_for_scope("@size", "i64_order_v1", &glob_scope);
  let mut scopes = vec![
    scope_for_definition(root_scope, root_value_store, vec![candidate(root_field, None, estimates)], 1),
    scope_for_definition(glob_scope, glob_value_store, vec![candidate(glob_field, None, estimates)], 1),
  ];
  scopes.sort_by(|left, right| left.scope_id.cmp(&right.scope_id));
  let cross_scope_catalogs = [catalog("@size", scopes)];
  let mut cross_scope_request =
    request(&planning_context, &expression, &cross_scope_catalogs, default_query_planning_limits_v1(), &|| false);
  cross_scope_request.sort_fields = &sort_fields;
  let error = plan_root_aware_query_v1(&cross_scope_request).unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::HistoricalViewUnavailable);
  assert_eq!(error.code(), "query_auxiliary_semantics_incompatible");
}

fn catalog(field_name: &str, scopes: Vec<QueryPlanningScopeV1>) -> RootAwareQueryFieldCatalogV1 {
  RootAwareQueryFieldCatalogV1 {
    database_id: DATABASE_ID,
    physical_instance_id: PHYSICAL_INSTANCE_ID,
    selected_namespace_root: ROOT.to_vec(),
    semantic_state_root: SEMANTIC_ROOT.to_vec(),
    publication_sequence: 41,
    field_name: field_name.to_string(),
    complete: true,
    scopes,
  }
}

fn request<'a>(
  context: &'a QueryPlanningContextV1<'a>,
  expression: &'a QueryExpressionV1,
  catalogs: &'a [RootAwareQueryFieldCatalogV1],
  limits: QueryPlanningLimitsV1,
  is_cancelled: &'a dyn Fn() -> bool,
) -> QueryPlanningRequestV1<'a> {
  QueryPlanningRequestV1 {
    context,
    query_path: "/",
    expression,
    catalogs,
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 20,
    limits,
    is_cancelled,
  }
}

#[test]
fn selected_definitions_compile_hash_alias_numeric_ranges_and_deduplicated_in_values() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (filename_value_store, filename_field) = definitions("@filename", "typed_exact_blake3_v1");
  let filename_generation = generation(&encoded_scope, &filename_value_store, &filename_field, &ROOT, 41);
  let filename_scope = scope(
    filename_value_store,
    vec![candidate(filename_field, Some(filename_generation), QueryPlanningIndexEstimatesV1::new(2, 20, 20, 1, 0).unwrap())],
    20,
  );
  let alias_expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@file_name".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("wallpaper.jpg".to_string())),
  });
  let alias_catalogs = [catalog("@filename", vec![filename_scope])];
  let alias_plan =
    plan_root_aware_query_v1(&request(&planning_context, &alias_expression, &alias_catalogs, default_query_planning_limits_v1(), &|| {
      false
    }))
    .unwrap();
  assert_eq!(alias_plan.predicates()[0].field_name(), "@filename");

  let (hash_value_store, hash_field) = definitions("@hash", "typed_exact_blake3_v1");
  let hash_generation = generation(&encoded_scope, &hash_value_store, &hash_field, &ROOT, 41);
  let hash_scope = scope(
    hash_value_store,
    vec![candidate(hash_field, Some(hash_generation), QueryPlanningIndexEstimatesV1::new(2, 20, 20, 20, 0).unwrap())],
    20,
  );

  let hash_text = "ab".repeat(32);
  let hash_expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String(hash_text)),
  });
  let hash_catalogs = [catalog("@hash", vec![hash_scope])];
  let hash_plan =
    plan_root_aware_query_v1(&request(&planning_context, &hash_expression, &hash_catalogs, default_query_planning_limits_v1(), &|| false))
      .unwrap();
  let hash_candidate = &hash_plan.predicates()[0].scopes()[0].candidates()[0];
  assert!(matches!(
    hash_plan.predicates()[0].operation(),
    QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::Bytes(value)) if value == &[0xab; 32]
  ));
  assert!(hash_candidate.selected_generation().is_some());
  assert_eq!(
    hash_candidate.compiled_literals()[0].canonical_value(),
    &[0x08, 32, 0, 0, 0].into_iter().chain([0xab; 32]).collect::<Vec<_>>()
  );
  assert!(matches!(hash_candidate.coordinate_constraint(), QueryCoordinateConstraintV1::Points(points) if points.len() == 1));

  let (size_value_store, size_field) = definitions("@size", "u64_order_v1");
  let size_generation = generation(&encoded_scope, &size_value_store, &size_field, &ROOT, 41);
  let size_scope = scope(
    size_value_store,
    vec![candidate(size_field, Some(size_generation), QueryPlanningIndexEstimatesV1::new(4, 100, 80, 20, 0).unwrap())],
    100,
  );
  let size_expression = QueryExpressionV1::And(vec![
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@size".to_string(),
      operation: QueryPredicateOperationV1::Between(CanonicalConfigValueV1::Unsigned(5), CanonicalConfigValueV1::Unsigned(10)),
    }),
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@size".to_string(),
      operation: QueryPredicateOperationV1::In(vec![
        CanonicalConfigValueV1::Unsigned(7),
        CanonicalConfigValueV1::Unsigned(7),
        CanonicalConfigValueV1::Unsigned(9),
      ]),
    }),
  ]);
  let size_catalogs = [catalog("@size", vec![size_scope])];
  let size_plan =
    plan_root_aware_query_v1(&request(&planning_context, &size_expression, &size_catalogs, default_query_planning_limits_v1(), &|| false))
      .unwrap();
  assert!(matches!(
    size_plan.predicates()[0].scopes()[0].candidates()[0].coordinate_constraint(),
    QueryCoordinateConstraintV1::InclusiveRange { widen_start_cell: true, widen_end_cell: true, .. }
  ));
  assert_eq!(size_plan.predicates()[1].scopes()[0].candidates()[0].compiled_literals().len(), 2);
}

#[test]
fn measured_work_chooses_an_exact_driver_without_promoting_partial_coverage() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (value_store, exact_field) = definitions("@hash", "typed_exact_blake3_v1");
  let (_, ordered_field) = definitions("@hash", "bytes_binary_order_v1");
  let previous_root = [0x55; 32];
  let partial_generation = generation(&encoded_scope, &value_store, &exact_field, &previous_root, 40);
  let complete_generation = generation(&encoded_scope, &value_store, &ordered_field, &ROOT, 41);
  let mut indexes = vec![
    candidate(exact_field, Some(partial_generation), QueryPlanningIndexEstimatesV1::new(10, 1_000, 1_000, 800, 800).unwrap()),
    candidate(ordered_field, Some(complete_generation), QueryPlanningIndexEstimatesV1::new(4, 40, 40, 8, 0).unwrap()),
  ];
  indexes.sort_by(|left, right| left.index_id.cmp(&right.index_id));
  let query_scope = scope(value_store, indexes, 1_000);
  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("cd".repeat(32))),
  });
  let catalogs = [catalog("@hash", vec![query_scope])];
  let plan =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap();
  let scope_plan = &plan.predicates()[0].scopes()[0];
  assert!(scope_plan.candidates().iter().any(|candidate| candidate.coverage() == CompiledQueryCoverageV1::PartialExact));
  let complete_index =
    scope_plan.candidates().iter().position(|candidate| candidate.coverage() == CompiledQueryCoverageV1::Complete).unwrap();
  assert!(matches!(scope_plan.driver(), QueryPlanDriverV1::Index { candidate_index, .. } if *candidate_index == complete_index));
}

#[test]
fn compiler_owned_query_fingerprint_binds_roots_expression_path_limit_and_auxiliary_policy() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (value_store, field) = definitions("@size", "u64_order_v1");
  let selected_generation = generation(&encoded_scope, &value_store, &field, &ROOT, 41);
  let query_scope = scope(
    value_store,
    vec![candidate(field, Some(selected_generation), QueryPlanningIndexEstimatesV1::new(1, 10, 10, 2, 0).unwrap())],
    100,
  );
  let catalogs = [catalog("@size", vec![query_scope])];
  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@size".to_string(),
    operation: QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Unsigned(5)),
  });
  let base =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap();
  let repeated =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap();
  assert_eq!(base.query_fingerprint(), repeated.query_fingerprint());
  assert_eq!(base.query_fingerprint().len(), HashAlgorithm::Blake3_256.hash_length());
  assert!(base.query_fingerprint().iter().any(|byte| *byte != 0));

  let changed_value = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@size".to_string(),
    operation: QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Unsigned(6)),
  });
  let changed_operator = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@size".to_string(),
    operation: QueryPredicateOperationV1::Lt(CanonicalConfigValueV1::Unsigned(5)),
  });
  let changed_shape = QueryExpressionV1::Not(Box::new(expression.clone()));
  for changed in [&changed_value, &changed_operator, &changed_shape] {
    let plan =
      plan_root_aware_query_v1(&request(&planning_context, changed, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap();
    assert_ne!(base.query_fingerprint(), plan.query_fingerprint());
  }

  let mut changed_request = request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false);
  changed_request.query_path = "/docs";
  let changed_path = plan_root_aware_query_v1(&changed_request).unwrap();
  assert_ne!(base.query_fingerprint(), changed_path.query_fingerprint());
  changed_request.query_path = "/";
  changed_request.result_limit = 21;
  let changed_limit = plan_root_aware_query_v1(&changed_request).unwrap();
  assert_ne!(base.query_fingerprint(), changed_limit.query_fingerprint());

  let ascending = [QuerySortFieldV1 { field_name: "@size".to_string(), direction: QuerySortDirectionV1::Ascending }];
  let descending = [QuerySortFieldV1 { field_name: "@size".to_string(), direction: QuerySortDirectionV1::Descending }];
  changed_request.result_limit = 20;
  changed_request.sort_fields = &ascending;
  let ascending_plan = plan_root_aware_query_v1(&changed_request).unwrap();
  changed_request.sort_fields = &descending;
  let descending_plan = plan_root_aware_query_v1(&changed_request).unwrap();
  assert_ne!(base.query_fingerprint(), ascending_plan.query_fingerprint());
  assert_ne!(ascending_plan.query_fingerprint(), descending_plan.query_fingerprint());

  let alternate_root = [0x55; 32];
  let alternate_semantic_root = [0x66; 32];
  let alternate_context = QueryPlanningContextV1::new(
    DATABASE_ID,
    PHYSICAL_INSTANCE_ID,
    HashAlgorithm::Blake3_256,
    &alternate_root,
    &alternate_semantic_root,
    42,
  )
  .unwrap();
  let mut alternate_catalogs = catalogs.clone();
  alternate_catalogs[0].selected_namespace_root = alternate_root.to_vec();
  alternate_catalogs[0].semantic_state_root = alternate_semantic_root.to_vec();
  alternate_catalogs[0].publication_sequence = 42;
  let alternate_plan =
    plan_root_aware_query_v1(&request(&alternate_context, &expression, &alternate_catalogs, default_query_planning_limits_v1(), &|| false))
      .unwrap();
  assert_ne!(base.query_fingerprint(), alternate_plan.query_fingerprint());
}

#[test]
fn request_admission_rejects_malformed_literals_operations_and_all_protocol_bounds() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (value_store, field) = definitions("@hash", "typed_exact_blake3_v1");
  let generation = generation(&encoded_scope, &value_store, &field, &ROOT, 41);
  let query_scope =
    scope(value_store, vec![candidate(field, Some(generation), QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap())], 1);
  let catalogs = [catalog("@hash", vec![query_scope])];

  for malformed in ["aa", &"g0".repeat(32)] {
    let expression = QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@hash".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String(malformed.to_string())),
    });
    let error =
      plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false))
        .unwrap_err();
    assert_eq!(error.class(), QueryPlanningErrorClassV1::InvalidRequest);
    assert_eq!(error.code(), "query_hash_literal_invalid");
  }

  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::String("ab".repeat(32))),
  });
  let error = plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false))
    .unwrap_err();
  assert_eq!(error.code(), "query_operation_unsupported");

  let oversized_in = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::In(vec![CanonicalConfigValueV1::String("ab".repeat(32)); 4_097]),
  });
  let error =
    plan_root_aware_query_v1(&request(&planning_context, &oversized_in, &catalogs, default_query_planning_limits_v1(), &|| false))
      .unwrap_err();
  assert_eq!(error.code(), "query_in_literal_limit");

  let mut nested = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("ab".repeat(32))),
  });
  for _ in 0..33 {
    nested = QueryExpressionV1::Not(Box::new(nested));
  }
  let error =
    plan_root_aware_query_v1(&request(&planning_context, &nested, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap_err();
  assert_eq!(error.code(), "query_expression_depth_limit");

  let limits = QueryPlanningLimitsV1::new(2, 32, 1, 8 * 1_024 * 1_024, 4_096, 64, 1_000).unwrap();
  let expression = QueryExpressionV1::And(vec![
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@hash".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("ab".repeat(32))),
    }),
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@hash".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("cd".repeat(32))),
    }),
  ]);
  let error = plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, limits, &|| false)).unwrap_err();
  assert_eq!(error.code(), "query_expression_node_limit");
}

#[test]
fn catalogs_are_exact_selected_root_receipts_and_historical_unavailability_never_becomes_empty_success() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (value_store, field) = definitions("@hash", "typed_exact_blake3_v1");
  let generation = generation(&encoded_scope, &value_store, &field, &ROOT, 41);
  let mut query_scope =
    scope(value_store, vec![candidate(field, Some(generation), QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap())], 1);
  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("ab".repeat(32))),
  });

  let mut wrong_root = catalog("@hash", vec![query_scope.clone()]);
  wrong_root.selected_namespace_root = vec![0x99; 32];
  let error =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &[wrong_root], default_query_planning_limits_v1(), &|| false))
      .unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::CorruptSource);
  assert_eq!(error.code(), "query_catalog_root_mismatch");

  let mut incomplete = catalog("@hash", vec![query_scope.clone()]);
  incomplete.complete = false;
  let error =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &[incomplete], default_query_planning_limits_v1(), &|| false))
      .unwrap_err();
  assert_eq!(error.code(), "query_catalog_incomplete");

  query_scope.semantic_availability = IndexSemanticQueryAvailabilityV1::DependencyUnavailable;
  let error = plan_root_aware_query_v1(&request(
    &planning_context,
    &expression,
    &[catalog("@hash", vec![query_scope])],
    default_query_planning_limits_v1(),
    &|| false,
  ))
  .unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::HistoricalViewUnavailable);
  assert_eq!(error.code(), "historical_view_unavailable");
}

#[test]
fn sort_aggregate_and_explain_are_definition_aware_bounded_and_authorization_safe() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (value_store, field) = definitions("@size", "u64_order_v1");
  let generation = generation(&encoded_scope, &value_store, &field, &ROOT, 41);
  let query_scope =
    scope(value_store, vec![candidate(field, Some(generation), QueryPlanningIndexEstimatesV1::new(4, 100, 80, 8, 0).unwrap())], 100);
  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@size".to_string(),
    operation: QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Unsigned(5)),
  });
  let catalogs = [catalog("@size", vec![query_scope])];
  let sort_fields = [QuerySortFieldV1 { field_name: "@size".to_string(), direction: QuerySortDirectionV1::Descending }];
  let aggregate_fields = [QueryAggregateFieldV1 { field_name: "@size".to_string(), kind: QueryAggregateKindV1::Average }];
  let mut query_request = request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false);
  query_request.sort_fields = &sort_fields;
  query_request.aggregate_fields = &aggregate_fields;
  let group_fields = ["@size".to_string()];
  query_request.group_fields = &group_fields;
  let plan = plan_root_aware_query_v1(&query_request).unwrap();
  assert!(matches!(plan.predicates()[0].scopes()[0].driver(), QueryPlanDriverV1::Index { .. }));
  assert_eq!(plan.result_limit(), 20);
  assert_eq!(plan.query_path(), "/");
  assert_eq!(plan.database_id(), DATABASE_ID);
  assert_eq!(plan.physical_instance_id(), PHYSICAL_INSTANCE_ID);
  assert_eq!(plan.semantic_state_root(), SEMANTIC_ROOT);
  assert_eq!(plan.publication_sequence(), 41);
  assert_eq!(plan.auxiliary_fields().len(), 3);
  assert_eq!(plan.auxiliary_fields()[0].operation(), CompiledQueryAuxiliaryOperationV1::Sort(QuerySortDirectionV1::Descending));
  assert_eq!(plan.auxiliary_fields()[1].operation(), CompiledQueryAuxiliaryOperationV1::Aggregate(QueryAggregateKindV1::Average));
  assert_eq!(plan.auxiliary_fields()[2].operation(), CompiledQueryAuxiliaryOperationV1::Group);
  for field in plan.auxiliary_fields() {
    assert_eq!(field.scopes().len(), 1);
    assert_eq!(field.scopes()[0].candidates().len(), 1);
    assert!(field.scopes()[0].candidates()[0].selected_generation().is_some());
    assert!(matches!(field.scopes()[0].driver(), QueryPlanDriverV1::Index { candidate_index: 0, .. }));
  }

  let explain = serde_json::to_value(authorization_safe_query_explain_v1(&plan)).unwrap();
  assert_eq!(explain["fields"][0]["field"], "@size");
  assert_eq!(explain["fields"][0]["driver"], serde_json::to_value(QueryLogicalDriverKindV1::Index).unwrap());
  let text = serde_json::to_string(&explain).unwrap();
  for forbidden in ["0500000000000000", "manifest", "page", "offset", "nvt", "cardinality", "/hidden"] {
    assert!(!text.to_ascii_lowercase().contains(forbidden), "EXPLAIN leaked {forbidden}: {text}");
  }

  let too_many_sorts = vec![sort_fields[0].clone(); 33];
  query_request.sort_fields = &too_many_sorts;
  let error = plan_root_aware_query_v1(&query_request).unwrap_err();
  assert_eq!(error.code(), "query_sort_field_limit");
}

#[test]
fn aggregate_admission_rejects_nonnumeric_reducers_and_duplicate_declarations() {
  let planning_context = context();
  let expression = QueryExpressionV1::And(Vec::new());
  let (value_store, field) = definitions("@filename", "utf8_binary_order_v1");
  let query_scope = scope(value_store, vec![candidate(field, None, QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap())], 1);
  let catalogs = [catalog("@filename", vec![query_scope])];
  let sum = [QueryAggregateFieldV1 { field_name: "@filename".to_string(), kind: QueryAggregateKindV1::Sum }];
  let mut query_request = request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false);
  query_request.aggregate_fields = &sum;
  let error = plan_root_aware_query_v1(&query_request).unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::InvalidRequest);
  assert_eq!(error.code(), "query_aggregate_numeric_required");

  let duplicate_aggregates = [sum[0].clone(), sum[0].clone()];
  query_request.aggregate_fields = &duplicate_aggregates;
  let error = plan_root_aware_query_v1(&query_request).unwrap_err();
  assert_eq!(error.code(), "query_duplicate_aggregate");

  query_request.aggregate_fields = &[];
  let duplicate_groups = ["@filename".to_string(), "@file_name".to_string()];
  query_request.group_fields = &duplicate_groups;
  let error = plan_root_aware_query_v1(&query_request).unwrap_err();
  assert_eq!(error.code(), "query_duplicate_group");

  query_request.group_fields = &[];
  let duplicate_sorts = [
    QuerySortFieldV1 { field_name: "@filename".to_string(), direction: QuerySortDirectionV1::Ascending },
    QuerySortFieldV1 { field_name: "@file_name".to_string(), direction: QuerySortDirectionV1::Descending },
  ];
  query_request.sort_fields = &duplicate_sorts;
  let error = plan_root_aware_query_v1(&query_request).unwrap_err();
  assert_eq!(error.code(), "query_duplicate_sort");
}

#[test]
fn cancellation_and_resource_refusal_remain_typed_non_success() {
  let planning_context = context();
  let expression = QueryExpressionV1::And(Vec::new());
  let cancellation_error =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &[], default_query_planning_limits_v1(), &|| true)).unwrap_err();
  assert_eq!(cancellation_error.class(), QueryPlanningErrorClassV1::Cancelled);

  assert!(QueryPlanningIndexEstimatesV1::new(0, 1, 1, 1, 0).is_err());
  assert!(QueryPlanningLimitsV1::new(0, 1, 1, 1, 1, 1, 1).is_err());

  let oversized = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("x".repeat(1_048_577))),
  });
  let error =
    plan_root_aware_query_v1(&request(&planning_context, &oversized, &[], default_query_planning_limits_v1(), &|| false)).unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_literal_size_limit");
}

#[test]
fn text_candidate_modes_never_claim_completeness_without_a_proven_superset() {
  let exact = QueryValueMatchV1::AllPostings;
  let any = QueryValueMatchV1::AnyPosting;
  assert_ne!(exact, any);

  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "text".to_string(),
    operation: QueryPredicateOperationV1::Fuzzy {
      value: CanonicalConfigValueV1::String("abc".to_string()),
      algorithm: aeordb::engine::v4::query_planner::QueryFuzzyAlgorithmV1::JaroWinkler,
      edits: None,
    },
  });
  let error = plan_root_aware_query_v1(&request(&context(), &expression, &[], default_query_planning_limits_v1(), &|| false)).unwrap_err();
  assert_eq!(error.code(), "query_definition_catalog_missing");
}

#[test]
fn frozen_limits_reject_unbounded_planning_inputs_before_catalog_or_path_work() {
  for invalid in [
    QueryPlanningLimitsV1::new(1_025, 32, 1, 1, 1, 1, 1),
    QueryPlanningLimitsV1::new(1, 33, 1, 1, 1, 1, 1),
    QueryPlanningLimitsV1::new(1, 1, 4_097, 1, 1, 1, 1),
    QueryPlanningLimitsV1::new(1, 1, 1, 64 * 1_048_576 + 1, 1, 1, 1),
    QueryPlanningLimitsV1::new(1, 1, 1, 1, 4_097, 1, 1),
    QueryPlanningLimitsV1::new(1, 1, 1, 1, 1, 65, 1),
    QueryPlanningLimitsV1::new(1, 1, 1, 1, 1, 1, 1_001),
  ] {
    assert_eq!(invalid.unwrap_err().code(), "query_planning_limits_invalid");
  }

  let oversized_field = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "x".repeat(4_097),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::Null),
  });
  let error =
    plan_root_aware_query_v1(&request(&context(), &oversized_field, &[], default_query_planning_limits_v1(), &|| false)).unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_field_name_limit");

  let expression = QueryExpressionV1::And(Vec::new());
  let planning_context = context();
  let mut query_request = request(&planning_context, &expression, &[], default_query_planning_limits_v1(), &|| false);
  let oversized_path = format!("/{}", "x".repeat(u16::MAX as usize));
  query_request.query_path = &oversized_path;
  let error = plan_root_aware_query_v1(&query_request).unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_path_limit");

  let nine_large_values = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "text".to_string(),
    operation: QueryPredicateOperationV1::In(vec![CanonicalConfigValueV1::String("x".repeat(1_048_571)); 9]),
  });
  let error = plan_root_aware_query_v1(&request(&planning_context, &nine_large_values, &[], default_query_planning_limits_v1(), &|| false))
    .unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_literal_total_limit");
}

#[test]
fn sha512_hash_literals_compile_to_full_width_bytes() {
  let algorithm = HashAlgorithm::Sha512;
  let encoded_scope = fixture("scope-definition-v1/ascp-sha512-root-direct-valid.bin");
  let scope_definition = decode_scope_definition(&encoded_scope, algorithm).unwrap();
  let mut value_store = value_store_fixture("avst-sha512-metadata-hash-corrected-valid");
  value_store[32..96].copy_from_slice(&scope_definition.scope_id);
  let value_definition = decode_value_store_definition(&value_store, algorithm).unwrap();
  let mut field = fixture("field-index-definition-v1/afix-sha512-typed_exact_blake3_v1-valid.bin");
  field[32..96].copy_from_slice(&value_definition.value_store_id);
  let field_definition = decode_field_index_definition(&field, algorithm).unwrap();
  let root = vec![0x33; 64];
  let semantic_root = vec![0x44; 64];
  let planning_context = QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &root, &semantic_root, 41).unwrap();
  let generation = QueryPlanningCoverageGenerationV1 {
    generation: 7,
    owner_id: field_definition.index_id.clone(),
    manifest_hash: vec![0x71; 64],
    source_namespace_root: root.clone(),
    coverage_epoch_id: [0x72; 16],
    coverage_publication_sequence: 41,
    definition_fingerprint: field_definition_fingerprint(algorithm, &field),
    dependency_fingerprint: field_dependency_fingerprint(algorithm, &scope_definition.scope_id, &value_definition.value_store_id),
    health: IndexCoverageGenerationHealthV1::Healthy,
  };
  let catalogs = [RootAwareQueryFieldCatalogV1 {
    database_id: DATABASE_ID,
    physical_instance_id: PHYSICAL_INSTANCE_ID,
    selected_namespace_root: root.clone(),
    semantic_state_root: semantic_root.clone(),
    publication_sequence: 41,
    field_name: "@hash".to_string(),
    complete: true,
    scopes: vec![QueryPlanningScopeV1 {
      scope_id: scope_definition.scope_id,
      value_store_id: value_definition.value_store_id,
      encoded_scope_definition: encoded_scope,
      encoded_value_store_definition: value_store,
      semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
      authoritative_document_count: 1,
      indexes: vec![QueryPlanningIndexCandidateV1 {
        index_id: field_definition.index_id,
        encoded_field_definition: field,
        selected_generation: Some(generation),
        estimates: QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap(),
        nvt_hint_available: false,
      }],
    }],
  }];
  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("ab".repeat(64))),
  });
  let plan =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap();
  assert_eq!(plan.hash_algorithm(), algorithm);
  assert_eq!(plan.query_fingerprint().len(), algorithm.hash_length());
  let canonical = plan.predicates()[0].scopes()[0].candidates()[0].compiled_literals()[0].canonical_value();
  assert_eq!(&canonical[..5], &[0x08, 64, 0, 0, 0]);
  assert_eq!(&canonical[5..], &[0xab; 64]);
}

#[test]
fn coverage_states_and_nvt_availability_never_gain_result_authority() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (value_store, field) = definitions("@hash", "typed_exact_blake3_v1");
  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("ab".repeat(32))),
  });

  let no_generation =
    scope(value_store.clone(), vec![candidate(field.clone(), None, QueryPlanningIndexEstimatesV1::new(u64::MAX, 0, 0, 0, 0).unwrap())], 10);
  let catalogs = [catalog("@hash", vec![no_generation])];
  let plan =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap();
  assert_eq!(plan.predicates()[0].scopes()[0].candidates()[0].coverage(), CompiledQueryCoverageV1::AuthoritativeOnly);
  assert!(matches!(plan.predicates()[0].scopes()[0].driver(), QueryPlanDriverV1::Authoritative { .. }));

  let mut incompatible_generation = generation(&encoded_scope, &value_store, &field, &ROOT, 41);
  incompatible_generation.definition_fingerprint = vec![0x99; 32];
  let mut without_hint =
    candidate(field.clone(), Some(incompatible_generation), QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap());
  without_hint.nvt_hint_available = false;
  let without_hint_catalogs = [catalog("@hash", vec![scope(value_store.clone(), vec![without_hint.clone()], 10)])];
  let without_hint_plan =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &without_hint_catalogs, default_query_planning_limits_v1(), &|| {
      false
    }))
    .unwrap();
  without_hint.nvt_hint_available = true;
  let with_hint_catalogs = [catalog("@hash", vec![scope(value_store.clone(), vec![without_hint], 10)])];
  let with_hint_plan =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &with_hint_catalogs, default_query_planning_limits_v1(), &|| false))
      .unwrap();
  assert_eq!(without_hint_plan, with_hint_plan);
  assert_eq!(with_hint_plan.predicates()[0].scopes()[0].candidates()[0].coverage(), CompiledQueryCoverageV1::AuthoritativeOnly);

  let previous_root = [0x55; 32];
  let mut degraded = generation(&encoded_scope, &value_store, &field, &previous_root, 40);
  degraded.health = IndexCoverageGenerationHealthV1::Degraded;
  let degraded_catalogs = [catalog(
    "@hash",
    vec![scope(
      value_store.clone(),
      vec![candidate(field.clone(), Some(degraded), QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 1).unwrap())],
      10,
    )],
  )];
  let degraded_plan =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &degraded_catalogs, default_query_planning_limits_v1(), &|| false))
      .unwrap();
  assert_eq!(degraded_plan.predicates()[0].scopes()[0].candidates()[0].coverage(), CompiledQueryCoverageV1::AuthoritativeOnly);

  let mut corrupt = generation(&encoded_scope, &value_store, &field, &ROOT, 41);
  corrupt.manifest_hash.fill(0);
  let corrupt_catalogs = [catalog(
    "@hash",
    vec![scope(value_store, vec![candidate(field, Some(corrupt), QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap())], 10)],
  )];
  let error =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &corrupt_catalogs, default_query_planning_limits_v1(), &|| false))
      .unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::CorruptSource);
  assert_eq!(error.code(), "index_coverage_corrupt_generation");
}

#[test]
fn cost_estimate_overflow_saturates_without_rejecting_an_exact_query() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (value_store, field) = definitions("@hash", "typed_exact_blake3_v1");
  let selected_generation = generation(&encoded_scope, &value_store, &field, &ROOT, 41);
  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("ab".repeat(32))),
  });

  let oversized_index = scope(
    value_store.clone(),
    vec![candidate(
      field.clone(),
      Some(selected_generation.clone()),
      QueryPlanningIndexEstimatesV1::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 0).unwrap(),
    )],
    10,
  );
  let catalogs = [catalog("@hash", vec![oversized_index])];
  let plan =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap();
  assert!(matches!(plan.predicates()[0].scopes()[0].driver(), QueryPlanDriverV1::Authoritative { .. }));

  let oversized_authoritative = scope(
    value_store,
    vec![candidate(field, Some(selected_generation), QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap())],
    u64::MAX,
  );
  let catalogs = [catalog("@hash", vec![oversized_authoritative])];
  let plan =
    plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap();
  assert!(matches!(plan.predicates()[0].scopes()[0].driver(), QueryPlanDriverV1::Index { .. }));
}

#[test]
fn text_strategies_use_only_mathematically_proven_candidate_supersets() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (value_store, trigram_field) = definitions("@filename", "unicode_trigram_v1");
  let trigram_generation = generation(&encoded_scope, &value_store, &trigram_field, &ROOT, 41);
  let trigram_scope = scope(
    value_store.clone(),
    vec![candidate(trigram_field.clone(), Some(trigram_generation), QueryPlanningIndexEstimatesV1::new(1, 10, 10, 3, 0).unwrap())],
    1_000,
  );
  for (operation, expect_index) in [
    (QueryPredicateOperationV1::Contains(CanonicalConfigValueV1::String("wallpaper".to_string())), true),
    (QueryPredicateOperationV1::Contains(CanonicalConfigValueV1::String("ab".to_string())), false),
    (QueryPredicateOperationV1::Similar { value: CanonicalConfigValueV1::String("wallpaper".to_string()), threshold: 0.0 }, false),
    (
      QueryPredicateOperationV1::Fuzzy {
        value: CanonicalConfigValueV1::String("wallpaper".to_string()),
        algorithm: QueryFuzzyAlgorithmV1::DamerauLevenshtein,
        edits: Some(2),
      },
      false,
    ),
  ] {
    let expression = QueryExpressionV1::Field(QueryPredicateV1 { field_name: "@filename".to_string(), operation });
    let catalogs = [catalog("@filename", vec![trigram_scope.clone()])];
    let plan =
      plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap();
    assert_eq!(matches!(plan.predicates()[0].scopes()[0].driver(), QueryPlanDriverV1::Index { .. }), expect_index);
  }

  let (_, primary_field) = definitions("@filename", "double_metaphone_primary_ascii_v1");
  let (_, alternate_field) = definitions("@filename", "double_metaphone_alt_ascii_v1");
  let mut candidates = vec![
    candidate(
      primary_field.clone(),
      Some(generation(&encoded_scope, &value_store, &primary_field, &ROOT, 41)),
      QueryPlanningIndexEstimatesV1::new(1, 10, 10, 3, 0).unwrap(),
    ),
    candidate(
      alternate_field.clone(),
      Some(generation(&encoded_scope, &value_store, &alternate_field, &ROOT, 41)),
      QueryPlanningIndexEstimatesV1::new(1, 10, 10, 3, 0).unwrap(),
    ),
  ];
  candidates.sort_by(|left, right| left.index_id.cmp(&right.index_id));
  let phonetic_expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@filename".to_string(),
    operation: QueryPredicateOperationV1::Phonetic(CanonicalConfigValueV1::String("Smith".to_string())),
  });
  let phonetic_catalogs = [catalog("@filename", vec![scope(value_store, candidates, 1_000)])];
  let plan = plan_root_aware_query_v1(&request(
    &planning_context,
    &phonetic_expression,
    &phonetic_catalogs,
    default_query_planning_limits_v1(),
    &|| false,
  ))
  .unwrap();
  assert!(
    matches!(plan.predicates()[0].scopes()[0].driver(), QueryPlanDriverV1::IndexUnion { candidate_indexes, .. } if candidate_indexes.len() == 2),
    "phonetic plan did not retain both exact branches: {plan:#?}"
  );
}

#[test]
fn malformed_catalog_identity_order_and_definition_closure_fail_closed() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (value_store, field) = definitions("@hash", "typed_exact_blake3_v1");
  let selected_generation = generation(&encoded_scope, &value_store, &field, &ROOT, 41);
  let query_scope = scope(
    value_store.clone(),
    vec![candidate(field.clone(), Some(selected_generation), QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap())],
    1,
  );
  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@hash".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("ab".repeat(32))),
  });
  let error_for = |catalogs: &[RootAwareQueryFieldCatalogV1], limits| {
    plan_root_aware_query_v1(&request(&planning_context, &expression, catalogs, limits, &|| false)).unwrap_err()
  };

  let base = catalog("@hash", vec![query_scope.clone()]);
  let error = error_for(&[base.clone(), base.clone()], default_query_planning_limits_v1());
  assert_eq!(error.code(), "query_catalog_duplicate_field");

  let error = error_for(&[base.clone(), catalog("@size", Vec::new())], default_query_planning_limits_v1());
  assert_eq!(error.code(), "query_catalog_unrequested_field");

  let mut alias = base.clone();
  alias.field_name = "@file_name".to_string();
  let error = error_for(&[alias], default_query_planning_limits_v1());
  assert_eq!(error.code(), "query_catalog_field_noncanonical");

  for (index, mut wrong_identity) in [base.clone(), base.clone(), base.clone(), base.clone()].into_iter().enumerate() {
    match index {
      1 => wrong_identity.database_id[0] ^= 1,
      2 => wrong_identity.physical_instance_id[0] ^= 1,
      3 => wrong_identity.semantic_state_root[0] ^= 1,
      _ => wrong_identity.publication_sequence += 1,
    }
    let error = error_for(&[wrong_identity], default_query_planning_limits_v1());
    assert_eq!(error.code(), "query_catalog_root_mismatch");
  }

  let mut duplicate_scope = base.clone();
  duplicate_scope.scopes.push(query_scope.clone());
  let error = error_for(&[duplicate_scope], default_query_planning_limits_v1());
  assert_eq!(error.code(), "query_scope_order");

  let mut wrong_scope = base.clone();
  wrong_scope.scopes[0].scope_id[0] ^= 1;
  let error = error_for(&[wrong_scope], default_query_planning_limits_v1());
  assert_eq!(error.code(), "query_scope_identity_mismatch");

  let mut wrong_value = base.clone();
  wrong_value.scopes[0].encoded_value_store_definition = definitions("@size", "u64_order_v1").0;
  let error = error_for(&[wrong_value], default_query_planning_limits_v1());
  assert_eq!(error.code(), "query_value_definition_mismatch");

  let mut wrong_value_identity = base.clone();
  wrong_value_identity.scopes[0].value_store_id[0] ^= 1;
  let error = error_for(&[wrong_value_identity], default_query_planning_limits_v1());
  assert_eq!(error.code(), "query_value_definition_mismatch");

  let mut wrong_index = base.clone();
  wrong_index.scopes[0].indexes[0].index_id[0] ^= 1;
  let error = error_for(&[wrong_index], default_query_planning_limits_v1());
  assert_eq!(error.code(), "query_field_definition_mismatch");

  let mut duplicate_index = base.clone();
  let repeated_index = duplicate_index.scopes[0].indexes[0].clone();
  duplicate_index.scopes[0].indexes.push(repeated_index);
  let error = error_for(&[duplicate_index], default_query_planning_limits_v1());
  assert_eq!(error.code(), "query_index_order");

  let one_definition_byte = QueryPlanningLimitsV1::new(1_024, 32, 4_096, 1, 4_096, 64, 1_000).unwrap();
  let error = error_for(&[base.clone()], one_definition_byte);
  assert_eq!(error.class(), QueryPlanningErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_definition_bytes_limit");

  let one_scope = QueryPlanningLimitsV1::new(1_024, 32, 1, 64 * 1_048_576, 4_096, 64, 1_000).unwrap();
  let error = error_for(&[catalog("@hash", vec![query_scope.clone(), query_scope.clone()])], one_scope);
  assert_eq!(error.code(), "query_scope_count_limit");

  let (_, ordered_field) = definitions("@hash", "bytes_binary_order_v1");
  let mut indexes = vec![
    candidate(field, None, QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap()),
    candidate(ordered_field, None, QueryPlanningIndexEstimatesV1::new(1, 1, 1, 1, 0).unwrap()),
  ];
  indexes.sort_by(|left, right| left.index_id.cmp(&right.index_id));
  let one_candidate = QueryPlanningLimitsV1::new(1_024, 32, 4_096, 64 * 1_048_576, 4_096, 1, 1_000).unwrap();
  let error = error_for(&[catalog("@hash", vec![scope(value_store, indexes, 1)])], one_candidate);
  assert_eq!(error.code(), "query_index_candidate_limit");
}

#[test]
fn operation_path_result_and_auxiliary_boundaries_are_typed() {
  let planning_context = context();
  let expression = QueryExpressionV1::And(Vec::new());
  for invalid_path in ["", "relative", "/a/../b", "/double//slash", "/nul\0path"] {
    let mut query_request = request(&planning_context, &expression, &[], default_query_planning_limits_v1(), &|| false);
    query_request.query_path = invalid_path;
    assert_eq!(plan_root_aware_query_v1(&query_request).unwrap_err().code(), "query_path_invalid");
  }
  for result_limit in [0, 1_001] {
    let mut query_request = request(&planning_context, &expression, &[], default_query_planning_limits_v1(), &|| false);
    query_request.result_limit = result_limit;
    assert_eq!(plan_root_aware_query_v1(&query_request).unwrap_err().code(), "query_result_limit");
  }

  let invalid_field_operations = [
    QueryPredicateOperationV1::Similar { value: CanonicalConfigValueV1::String("x".to_string()), threshold: f64::NAN },
    QueryPredicateOperationV1::Similar { value: CanonicalConfigValueV1::String("x".to_string()), threshold: -0.1 },
    QueryPredicateOperationV1::Similar { value: CanonicalConfigValueV1::String("x".to_string()), threshold: 1.1 },
  ];
  for operation in invalid_field_operations {
    let expression = QueryExpressionV1::Field(QueryPredicateV1 { field_name: "text".to_string(), operation });
    assert_eq!(
      plan_root_aware_query_v1(&request(&planning_context, &expression, &[], default_query_planning_limits_v1(), &|| false,))
        .unwrap_err()
        .code(),
      "query_similarity_threshold_invalid"
    );
  }
  let fuzzy = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "text".to_string(),
    operation: QueryPredicateOperationV1::Fuzzy {
      value: CanonicalConfigValueV1::String("x".to_string()),
      algorithm: QueryFuzzyAlgorithmV1::DamerauLevenshtein,
      edits: Some(9),
    },
  });
  assert_eq!(
    plan_root_aware_query_v1(&request(&planning_context, &fuzzy, &[], default_query_planning_limits_v1(), &|| false)).unwrap_err().code(),
    "query_fuzzy_edits_invalid"
  );

  let sort = QuerySortFieldV1 { field_name: "@size".to_string(), direction: QuerySortDirectionV1::Ascending };
  let aggregate = QueryAggregateFieldV1 { field_name: "@size".to_string(), kind: QueryAggregateKindV1::Count };
  let groups = vec!["@size".to_string(); 33];
  let sorts = vec![sort; 33];
  let aggregates = vec![aggregate; 33];
  let mut query_request = request(&planning_context, &expression, &[], default_query_planning_limits_v1(), &|| false);
  query_request.sort_fields = &sorts;
  assert_eq!(plan_root_aware_query_v1(&query_request).unwrap_err().code(), "query_sort_field_limit");
  query_request.sort_fields = &[];
  query_request.aggregate_fields = &aggregates;
  assert_eq!(plan_root_aware_query_v1(&query_request).unwrap_err().code(), "query_aggregate_field_limit");
  query_request.aggregate_fields = &[];
  query_request.group_fields = &groups;
  assert_eq!(plan_root_aware_query_v1(&query_request).unwrap_err().code(), "query_group_field_limit");
}

#[test]
fn ordered_ranges_preserve_exact_endpoints_and_delegate_neighbor_widening_to_the_page_cursor() {
  let planning_context = context();
  let encoded_scope = scope_fixture();
  let (value_store, field) = definitions("@size", "u64_order_v1");
  let selected_generation = generation(&encoded_scope, &value_store, &field, &ROOT, 41);
  let query_scope = scope(
    value_store,
    vec![candidate(field, Some(selected_generation), QueryPlanningIndexEstimatesV1::new(1, 10, 10, 2, 0).unwrap())],
    100,
  );
  let catalogs = [catalog("@size", vec![query_scope])];
  for (operation, expected_flags) in [
    (QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Unsigned(5)), (true, false)),
    (QueryPredicateOperationV1::Lt(CanonicalConfigValueV1::Unsigned(5)), (false, true)),
    (QueryPredicateOperationV1::Between(CanonicalConfigValueV1::Unsigned(5), CanonicalConfigValueV1::Unsigned(10)), (true, true)),
  ] {
    let expression = QueryExpressionV1::Field(QueryPredicateV1 { field_name: "@size".to_string(), operation });
    let plan =
      plan_root_aware_query_v1(&request(&planning_context, &expression, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap();
    assert!(matches!(
      plan.predicates()[0].scopes()[0].candidates()[0].coordinate_constraint(),
      QueryCoordinateConstraintV1::InclusiveRange {
        widen_start_cell,
        widen_end_cell,
        ..
      } if (*widen_start_cell, *widen_end_cell) == expected_flags
    ));
  }

  let reversed = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@size".to_string(),
    operation: QueryPredicateOperationV1::Between(CanonicalConfigValueV1::Unsigned(10), CanonicalConfigValueV1::Unsigned(5)),
  });
  let error =
    plan_root_aware_query_v1(&request(&planning_context, &reversed, &catalogs, default_query_planning_limits_v1(), &|| false)).unwrap_err();
  assert_eq!(error.class(), QueryPlanningErrorClassV1::InvalidRequest);
  assert_eq!(error.code(), "query_between_order_invalid");
}

#[test]
fn planner_remains_storage_neutral_and_disconnected_from_live_v3_authority() {
  let source = include_str!("../../src/engine/v4/query_planner.rs");
  for forbidden in
    ["std::fs", "tokio::", "server::", "DirectoryOps", "IndexManager", "QueryEngine", "json_query_value_to_bytes", "thread::spawn"]
  {
    assert!(!source.contains(forbidden), "storage-neutral planner contains forbidden dependency {forbidden}");
  }

  let repository_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
  let mut pending = vec![repository_root.join("src"), repository_root.join("../aeordb-cli/src")];
  while let Some(path) = pending.pop() {
    for entry in std::fs::read_dir(path).unwrap() {
      let path = entry.unwrap().path();
      if path.is_dir() {
        pending.push(path);
      } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") && !path.ends_with("engine/v4/query_planner.rs") {
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(
          !source.contains("plan_root_aware_query_v1") && !source.contains("authorization_safe_query_explain_v1"),
          "v4 planner activated before cutover in {}",
          path.display()
        );
      }
    }
  }
}
