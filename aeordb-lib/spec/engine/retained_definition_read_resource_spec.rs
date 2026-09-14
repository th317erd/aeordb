//! Actual allocation-failure propagation through retained definition readers.
use super::{ALGORITHM, fixture, json_selector_definition, measure};
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::index_artifact::{
  IndexManifestBodyV1, IndexManifestWriteV1, decode_index_manifest, encode_index_manifest, validate_correctness_manifest_chain,
};
use aeordb::engine::v4::scope::decode_scope_definition;
use aeordb::engine::v4::source_selector::JsonPathSegmentV1;
use aeordb::engine::v4::value_store::decode_value_store_definition;

fn manifest_chain() -> [Vec<u8>; 3] {
  manifest_chain_with_selector_segments(1)
}

pub(super) fn manifest_chain_with_selector_segments(count: usize) -> [Vec<u8>; 3] {
  let scope_fixture = fixture("index-artifact-v1", "aidx-blake3-256-scope-catalog-manifest-empty");
  let scope_view = decode_index_manifest(&scope_fixture, ALGORITHM).unwrap();
  let IndexManifestBodyV1::ScopeCatalog(mut body) = scope_view.details else {
    panic!("ScopeCatalog fixture")
  };
  body.coverage.coverage_publication_sequence = body.coverage.coverage_publication_sequence.max(1);
  let scope = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: ALGORITHM,
    generation: scope_view.generation,
    owner_id: scope_view.owner_id,
    body: IndexManifestBodyV1::ScopeCatalog(body),
  })
  .unwrap()
  .value;
  let scope_view = decode_index_manifest(&scope, ALGORITHM).unwrap();
  let old_value = fixture("index-artifact-v1", "aidx-blake3-256-value-store-manifest-empty");
  let old_value_view = decode_index_manifest(&old_value, ALGORITHM).unwrap();
  let segments = vec![JsonPathSegmentV1::ObjectKey("x"); count];
  let mut definition = json_selector_definition(false, &segments);
  definition[32..64].copy_from_slice(scope_view.owner_id);
  let value_id = decode_value_store_definition(&definition, ALGORITHM).unwrap().value_store_id;
  let IndexManifestBodyV1::ValueStore(mut value_body) = old_value_view.details else {
    panic!("ValueStore fixture")
  };
  value_body.value_store_definition = &definition;
  value_body.coverage.coverage_publication_sequence = value_body.coverage.coverage_publication_sequence.max(1);
  value_body.scope_catalog_manifest = &scope_view.key;
  let value = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: ALGORITHM,
    generation: old_value_view.generation,
    owner_id: &value_id,
    body: IndexManifestBodyV1::ValueStore(value_body),
  })
  .unwrap();
  let old_field = fixture("index-artifact-v1", "aidx-blake3-256-field-index-manifest-empty");
  let old_field_view = decode_index_manifest(&old_field, ALGORITHM).unwrap();
  let IndexManifestBodyV1::FieldIndex(mut field_body) = old_field_view.details else {
    panic!("FieldIndex fixture")
  };
  let mut field_definition = field_body.field_index_definition.to_vec();
  field_definition[32..64].copy_from_slice(&value_id);
  let field_id = decode_field_index_definition(&field_definition, ALGORITHM).unwrap().index_id;
  field_body.field_index_definition = &field_definition;
  field_body.coverage.coverage_publication_sequence = field_body.coverage.coverage_publication_sequence.max(1);
  field_body.value_store_manifest = &value.key;
  let field = encode_index_manifest(&IndexManifestWriteV1 {
    hash_algorithm: ALGORITHM,
    generation: old_field_view.generation,
    owner_id: &field_id,
    body: IndexManifestBodyV1::FieldIndex(field_body),
  })
  .unwrap();
  validate_correctness_manifest_chain(
    &scope_view,
    &decode_index_manifest(&value.value, ALGORITHM).unwrap(),
    &decode_index_manifest(&field.value, ALGORITHM).unwrap(),
    ALGORITHM,
  )
  .unwrap();
  [scope, value.value, field.value]
}

#[test]
fn nested_manifest_decoder_preserves_real_selector_allocation_failure() {
  let [_, value, _] = manifest_chain();
  let (result, allocations) = measure(std::mem::size_of::<JsonPathSegmentV1<'_>>(), || decode_index_manifest(&value, ALGORITHM));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert!(result.unwrap_err().is_allocation_failure());
  decode_index_manifest(&value, ALGORITHM).unwrap();
}

#[test]
fn manifest_chain_revalidation_preserves_real_selector_allocation_failure() {
  let [scope, value, field] = manifest_chain();
  let scope = decode_index_manifest(&scope, ALGORITHM).unwrap();
  let value = decode_index_manifest(&value, ALGORITHM).unwrap();
  let field = decode_index_manifest(&field, ALGORITHM).unwrap();
  let (result, allocations) =
    measure(std::mem::size_of::<JsonPathSegmentV1<'_>>(), || validate_correctness_manifest_chain(&scope, &value, &field, ALGORITHM));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert!(result.unwrap_err().is_allocation_failure());
  validate_correctness_manifest_chain(&scope, &value, &field, ALGORITHM).unwrap();
}

#[test]
fn query_definition_preflight_preserves_real_selector_allocation_failure() {
  use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
  use aeordb::engine::v4::index_coverage_planner::IndexSemanticQueryAvailabilityV1;
  use aeordb::engine::v4::query_planner::*;
  let scope = fixture("scope-definition-v1", "ascp-blake3-256-root-direct-valid");
  let scope_id = decode_scope_definition(&scope, ALGORITHM).unwrap().scope_id;
  let mut value = json_selector_definition(false, &[JsonPathSegmentV1::ObjectKey("x")]);
  value[32..64].copy_from_slice(&scope_id);
  let definition = decode_value_store_definition(&value, ALGORITHM).unwrap();
  let field_name = definition.field_name.to_string();
  let mut field = fixture("field-index-definition-v1", "afix-blake3-256-typed_exact_blake3_v1-valid");
  field[32..64].copy_from_slice(&definition.value_store_id);
  let field_id = decode_field_index_definition(&field, ALGORITHM).unwrap().index_id;
  let context = QueryPlanningContextV1::new([1; 16], [2; 16], ALGORITHM, &[3; 32], &[4; 32], 1).unwrap();
  let catalogs = [RootAwareQueryFieldCatalogV1 {
    database_id: [1; 16],
    physical_instance_id: [2; 16],
    selected_namespace_root: vec![3; 32],
    semantic_state_root: vec![4; 32],
    publication_sequence: 1,
    field_name: field_name.clone(),
    complete: true,
    scopes: vec![QueryPlanningScopeV1 {
      scope_id,
      value_store_id: definition.value_store_id,
      encoded_scope_definition: scope,
      encoded_value_store_definition: value,
      semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
      authoritative_document_count: 1,
      indexes: vec![QueryPlanningIndexCandidateV1 {
        index_id: field_id,
        encoded_field_definition: field,
        selected_generation: None,
        estimates: QueryPlanningIndexEstimatesV1::new(0, 0, 0, 0, 1).unwrap(),
        nvt_hint_available: false,
      }],
    }],
  }];
  let expression =
    QueryExpressionV1::Field(QueryPredicateV1 { field_name, operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::Null) });
  let request = QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 20,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  };
  plan_root_aware_query_v1(&request).unwrap();
  let (result, allocations) = measure(std::mem::size_of::<JsonPathSegmentV1<'_>>(), || plan_root_aware_query_v1(&request));
  assert!(allocations.injected_failure, "{allocations:?}");
  assert_eq!(result.unwrap_err().class(), QueryPlanningErrorClassV1::ResourceLimit);
  plan_root_aware_query_v1(&request).unwrap();
}
