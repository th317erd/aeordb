use std::collections::BTreeMap;
use std::mem::size_of;
use std::sync::Arc;

use aeordb::engine::HashAlgorithm;
use aeordb::engine::memory_coordinator::{MemoryCoordinator, MemoryOwner, MemoryPolicy};
use aeordb::engine::v4::config_value::CanonicalConfigValueV1;
use aeordb::engine::v4::config_value::encode_canonical_value;
use aeordb::engine::v4::field_definition::decode_field_index_definition;
use aeordb::engine::v4::hash::digest_parts;
use aeordb::engine::v4::index_artifact::EncodedImmutableIndexArtifactV1;
use aeordb::engine::v4::index_artifact_cursor::{
  ArtifactCursorReadErrorV1, ArtifactCursorSourceV1, ArtifactDirectoryRootSummaryV1, ArtifactPageCursorLimitsV1, RetainedArtifactBytesV1,
};
use aeordb::engine::v4::index_coverage_planner::{IndexCoverageGenerationHealthV1, IndexSemanticQueryAvailabilityV1};
use aeordb::engine::v4::index_coverage_registry::{field_definition_fingerprint, field_dependency_fingerprint};
use aeordb::engine::v4::index_page::{
  ArtifactDirectoryEntryWriteV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1, OrderedPageWriteV1, PhysicalHintV1, PostingRecordV1,
  decode_artifact_directory, decode_ordered_page, encode_artifact_directory, encode_ordered_page, encode_posting_record,
};
use aeordb::engine::v4::index_partial_acceleration::{
  IndexChangedDocumentScanReceiptV1, IndexChangedDocumentScanRequestV1, IndexChangedDocumentSourceV1, IndexChangedDocumentV1,
  IndexChangedDocumentVisitorV1, IndexPartialAccelerationErrorClassV1, IndexPartialAccelerationFallbackReasonV1,
  IndexPartialAccelerationLimitsV1, IndexPartialAccelerationOutcomeV1, IndexPartialCandidateRecheckerV1, IndexPartialRecheckOutcomeV1,
  IndexPartialRecheckRequestV1, IndexPartialScanErrorV1, IndexPartialSourceErrorV1,
};
use aeordb::engine::v4::index_record::{ScopeDocumentRecordV1, encode_scope_document_record};
use aeordb::engine::v4::query_complete_candidate::{
  QueryCandidateArtifactRootV1, QueryCandidateRecheckReceiptV1, QueryCandidateRecheckRequestV1, QueryCompleteCandidateErrorClassV1,
  QueryCompleteCandidateExecutionRequestV1, QueryCompleteCandidateLimitsV1, QueryCompleteCandidateSourceV1,
  QueryCompleteCandidateScopeExecutionRequestV1, QueryCompletePostingRootReceiptV1, QueryCompletePostingRootRequestV1,
  QueryCompletePostingScanRequestV1, QueryCompleteScopeResolutionRequestV1, QueryCompleteScopeRootReceiptV1,
  QueryCompleteScopeRootRequestV1, QueryPartialPostingScanRequestV1, QueryScopeOrdinalSelectionV1,
  execute_complete_candidate_root_query_into_v1, execute_complete_candidate_root_query_v1, execute_complete_candidate_scope_query_into_v1,
  resolve_complete_scope_identities_v1, scan_complete_posting_ordinals_v1, scan_partial_posting_ordinals_v1,
};
use aeordb::engine::v4::query_candidate_composition::{
  QueryBooleanCandidatePlanKindV1, QueryCandidateCompositionErrorClassV1, QueryCandidateCompositionLimitsV1,
  compose_boolean_candidate_plan_v1,
};
use aeordb::engine::v4::query_partial_candidate::{
  QueryComposedPartialCandidateExecutionRequestV1, QueryPartialCandidateArtifactSourceV1, QueryPartialCandidateExecutionRequestV1,
  QueryPartialPostingRootReceiptV1, QueryPartialPostingRootRequestV1, QueryPartialScopeRootReceiptV1, QueryPartialScopeRootRequestV1,
  execute_composed_partial_candidates_v1, execute_planned_partial_candidate_v1,
};
use aeordb::engine::v4::query_executor::{
  QueryAuthoritativeDocumentVisitorV1, QueryAuthoritativeFieldSourceV1, QueryAuthoritativeScopeSourceV1, QueryAuthoritativeValueVisitorV1,
  QueryExecutionByteLimitsV1, QueryExecutionCountLimitsV1, QueryExecutionDocumentV1, QueryExecutionErrorClassV1,
  QueryExecutionErrorOriginV1, QueryExecutionFieldReadReceiptV1, QueryExecutionFieldReadRequestV1, QueryExecutionFieldStateV1,
  QueryExecutionLimitsV1, QueryExecutionMatchPathV1, QueryExecutionMatchRefV1, QueryExecutionMatchSinkV1, QueryExecutionMatchV1,
  QueryExecutionScanErrorV1, QueryExecutionScopeScanReceiptV1, QueryExecutionScopeScanRequestV1, QueryExecutionSinkBatchReceiptV1,
  QueryExecutionSinkBatchV1, QueryExecutionSinkErrorClassV1, QueryExecutionSinkErrorV1, QueryExecutionSourceErrorClassV1,
  QueryExecutionSourceErrorV1, RootAwareQueryExecutionRequestV1, execute_authoritative_root_query_v1,
};
use aeordb::engine::v4::query_planner::{
  CompiledQueryCoverageV1, CompiledQueryIndexCandidateV1, CompiledRootAwareQueryPlanV1, QueryCoordinateConstraintV1, QueryExpressionV1,
  QueryPlanningContextV1, QueryPlanningCoverageGenerationV1, QueryPlanningIndexCandidateV1, QueryPlanningIndexEstimatesV1,
  QueryPlanningRequestV1, QueryPlanDriverV1, QueryPlanningScopeV1, QueryPredicateOperationV1, QueryPredicateV1,
  RootAwareQueryFieldCatalogV1, default_query_planning_limits_v1, plan_root_aware_query_v1,
};
use aeordb::engine::v4::query_scope_execution::{
  QueryExactScopeExecutionErrorV1, QueryExactScopeExecutionPathV1, QueryExactScopeExecutionRequestV1, QueryExactScopeFallbackDiagnosticV1,
  execute_exact_query_scope_v1,
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
    _ => panic!("candidate fixture does not cover {algorithm:?}"),
  }
}

fn definitions(algorithm: HashAlgorithm, field_name: &str, converter: &str, metadata_id: u16) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
  let name = algorithm_name(algorithm);
  let scope = fixture(&format!("scope-definition-v1/ascp-{name}-root-direct-valid.bin"));
  let scope_id = decode_scope_definition(&scope, algorithm).unwrap().scope_id;
  let mut value_store = fixture(&format!("value-store-definition-v1/avst-{name}-metadata-hash-corrected-valid.bin"));
  let width = algorithm.hash_length();
  value_store[32..32 + width].copy_from_slice(&scope_id);
  let fixed_start = 32 + width;
  let field_start = fixed_start + 80;
  let old_field_length = u32::from_le_bytes(value_store[fixed_start..fixed_start + 4].try_into().unwrap()) as usize;
  value_store.splice(field_start..field_start + old_field_length, field_name.as_bytes().iter().copied());
  let total_length = value_store.len() as u32;
  value_store[8..12].copy_from_slice(&total_length.to_le_bytes());
  value_store[fixed_start..fixed_start + 4].copy_from_slice(&(field_name.len() as u32).to_le_bytes());
  let selector_start = field_start + field_name.len();
  value_store[selector_start + 32..selector_start + 34].copy_from_slice(&metadata_id.to_le_bytes());
  let value_definition = decode_value_store_definition(&value_store, algorithm).unwrap();

  let mut field = fixture(&format!("field-index-definition-v1/afix-{name}-{converter}-valid.bin"));
  field[32..32 + width].copy_from_slice(&value_definition.value_store_id);
  decode_field_index_definition(&field, algorithm).unwrap();
  (scope, value_store, field)
}

struct PlannedCandidate {
  root: Vec<u8>,
  scope_id: Vec<u8>,
  candidate: CompiledQueryIndexCandidateV1,
  plan: CompiledRootAwareQueryPlanV1,
  catalogs: Vec<RootAwareQueryFieldCatalogV1>,
}

fn planned_candidate(algorithm: HashAlgorithm, operation: QueryPredicateOperationV1) -> PlannedCandidate {
  planned_candidate_with(algorithm, "@filename", "typed_exact_blake3_v1", 2, operation)
}

fn planned_candidate_with(
  algorithm: HashAlgorithm,
  field_name: &str,
  converter: &str,
  metadata_id: u16,
  operation: QueryPredicateOperationV1,
) -> PlannedCandidate {
  planned_candidate_with_source(algorithm, field_name, converter, metadata_id, operation, None)
}

fn planned_candidate_with_source(
  algorithm: HashAlgorithm,
  field_name: &str,
  converter: &str,
  metadata_id: u16,
  operation: QueryPredicateOperationV1,
  generation_source: Option<(Vec<u8>, u64)>,
) -> PlannedCandidate {
  let (scope, value_store, field) = definitions(algorithm, field_name, converter, metadata_id);
  let scope_definition = decode_scope_definition(&scope, algorithm).unwrap();
  let value_definition = decode_value_store_definition(&value_store, algorithm).unwrap();
  let field_definition = decode_field_index_definition(&field, algorithm).unwrap();
  let scope_id = scope_definition.scope_id.clone();
  let value_store_id = value_definition.value_store_id.clone();
  let index_id = field_definition.index_id.clone();
  let root = vec![0x33; algorithm.hash_length()];
  let semantic_root = vec![0x44; algorithm.hash_length()];
  let (source_namespace_root, coverage_publication_sequence) = generation_source.unwrap_or_else(|| (root.clone(), 41));
  let generation = QueryPlanningCoverageGenerationV1 {
    generation: 7,
    owner_id: index_id.clone(),
    manifest_hash: vec![0x71; algorithm.hash_length()],
    source_namespace_root,
    coverage_epoch_id: [0x72; 16],
    coverage_publication_sequence,
    definition_fingerprint: field_definition_fingerprint(algorithm, &field),
    dependency_fingerprint: field_dependency_fingerprint(algorithm, &scope_id, &value_store_id),
    health: IndexCoverageGenerationHealthV1::Healthy,
  };
  let catalog = RootAwareQueryFieldCatalogV1 {
    database_id: DATABASE_ID,
    physical_instance_id: PHYSICAL_INSTANCE_ID,
    selected_namespace_root: root.clone(),
    semantic_state_root: semantic_root.clone(),
    publication_sequence: 41,
    field_name: field_name.to_string(),
    complete: true,
    scopes: vec![QueryPlanningScopeV1 {
      scope_id: scope_id.clone(),
      value_store_id,
      encoded_scope_definition: scope,
      encoded_value_store_definition: value_store,
      semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
      authoritative_document_count: 100,
      indexes: vec![QueryPlanningIndexCandidateV1 {
        index_id,
        encoded_field_definition: field,
        selected_generation: Some(generation),
        estimates: QueryPlanningIndexEstimatesV1::new(1, 8, 4, 2, 0).unwrap(),
        nvt_hint_available: false,
      }],
    }],
  };
  let expression = QueryExpressionV1::Field(QueryPredicateV1 { field_name: field_name.to_string(), operation });
  let context = QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &root, &semantic_root, 41).unwrap();
  let catalogs = vec![catalog];
  let plan = plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 16,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap();
  let candidate = plan.predicates()[0].scopes()[0].candidates()[0].clone();
  PlannedCandidate { root, scope_id, candidate, plan, catalogs }
}

fn planned_partial_candidate(algorithm: HashAlgorithm) -> PlannedCandidate {
  planned_candidate_with_source(
    algorithm,
    "@filename",
    "typed_exact_blake3_v1",
    2,
    QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())),
    Some((vec![0x55; algorithm.hash_length()], 40)),
  )
}

fn planned_partial_size_candidate(algorithm: HashAlgorithm) -> PlannedCandidate {
  planned_candidate_with_source(
    algorithm,
    "@size",
    "u64_order_v1",
    5,
    QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::Unsigned(5)),
    Some((vec![0x55; algorithm.hash_length()], 40)),
  )
}

struct PlannedQuery {
  root: Vec<u8>,
  scope_id: Vec<u8>,
  plan: CompiledRootAwareQueryPlanV1,
  catalogs: Vec<RootAwareQueryFieldCatalogV1>,
}

fn query_catalog(
  algorithm: HashAlgorithm,
  root: &[u8],
  semantic_root: &[u8],
  field_name: &str,
  converter: &str,
  metadata_id: u16,
  manifest_tag: u8,
) -> RootAwareQueryFieldCatalogV1 {
  let (scope, value_store, field) = definitions(algorithm, field_name, converter, metadata_id);
  let scope_definition = decode_scope_definition(&scope, algorithm).unwrap();
  let value_definition = decode_value_store_definition(&value_store, algorithm).unwrap();
  let field_definition = decode_field_index_definition(&field, algorithm).unwrap();
  let generation = QueryPlanningCoverageGenerationV1 {
    generation: 7,
    owner_id: field_definition.index_id.clone(),
    manifest_hash: vec![manifest_tag; algorithm.hash_length()],
    source_namespace_root: root.to_vec(),
    coverage_epoch_id: [manifest_tag; 16],
    coverage_publication_sequence: 41,
    definition_fingerprint: field_definition_fingerprint(algorithm, &field),
    dependency_fingerprint: field_dependency_fingerprint(algorithm, &scope_definition.scope_id, &value_definition.value_store_id),
    health: IndexCoverageGenerationHealthV1::Healthy,
  };
  RootAwareQueryFieldCatalogV1 {
    database_id: DATABASE_ID,
    physical_instance_id: PHYSICAL_INSTANCE_ID,
    selected_namespace_root: root.to_vec(),
    semantic_state_root: semantic_root.to_vec(),
    publication_sequence: 41,
    field_name: field_name.to_string(),
    complete: true,
    scopes: vec![QueryPlanningScopeV1 {
      scope_id: scope_definition.scope_id,
      value_store_id: value_definition.value_store_id,
      encoded_scope_definition: scope,
      encoded_value_store_definition: value_store,
      semantic_availability: IndexSemanticQueryAvailabilityV1::Complete,
      authoritative_document_count: 100,
      indexes: vec![QueryPlanningIndexCandidateV1 {
        index_id: field_definition.index_id,
        encoded_field_definition: field,
        selected_generation: Some(generation),
        estimates: QueryPlanningIndexEstimatesV1::new(1, 8, 4, 3, 0).unwrap(),
        nvt_hint_available: false,
      }],
    }],
  }
}

fn planned_query(expression: QueryExpressionV1) -> PlannedQuery {
  let algorithm = HashAlgorithm::Blake3_256;
  let root = vec![0x33; algorithm.hash_length()];
  let semantic_root = vec![0x44; algorithm.hash_length()];
  let mut catalogs = vec![query_catalog(algorithm, &root, &semantic_root, "@filename", "typed_exact_blake3_v1", 2, 0x71)];
  if expression_uses_field(&expression, "@size") {
    catalogs.push(query_catalog(algorithm, &root, &semantic_root, "@size", "u64_order_v1", 5, 0x73));
  }
  let scope_id = catalogs[0].scopes[0].scope_id.clone();
  assert!(catalogs.iter().all(|catalog| catalog.scopes[0].scope_id == scope_id));
  let context = QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &root, &semantic_root, 41).unwrap();
  let plan = plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 16,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap();
  PlannedQuery { root, scope_id, plan, catalogs }
}

fn planned_query_with_generation_sources(
  expression: QueryExpressionV1,
  filename_source: Option<(Vec<u8>, u64)>,
  size_source: Option<(Vec<u8>, u64)>,
) -> PlannedQuery {
  let algorithm = HashAlgorithm::Blake3_256;
  let root = vec![0x33; algorithm.hash_length()];
  let semantic_root = vec![0x44; algorithm.hash_length()];
  let mut catalogs = vec![query_catalog(algorithm, &root, &semantic_root, "@filename", "typed_exact_blake3_v1", 2, 0x71)];
  if expression_uses_field(&expression, "@size") {
    catalogs.push(query_catalog(algorithm, &root, &semantic_root, "@size", "u64_order_v1", 5, 0x73));
  }
  for catalog in &mut catalogs {
    let source = match catalog.field_name.as_str() {
      "@filename" => filename_source.clone(),
      "@size" => size_source.clone(),
      field => panic!("unexpected fixture field {field}"),
    };
    let generation = &mut catalog.scopes[0].indexes[0].selected_generation;
    match source {
      Some((source_namespace_root, coverage_publication_sequence)) => {
        let generation = generation.as_mut().unwrap();
        generation.source_namespace_root = source_namespace_root;
        generation.coverage_publication_sequence = coverage_publication_sequence;
      }
      None => *generation = None,
    }
  }
  let scope_id = catalogs[0].scopes[0].scope_id.clone();
  assert!(catalogs.iter().all(|catalog| catalog.scopes[0].scope_id == scope_id));
  let context = QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &root, &semantic_root, 41).unwrap();
  let plan = plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 16,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap();
  PlannedQuery { root, scope_id, plan, catalogs }
}

fn planned_phonetic_union_query(query: &str) -> PlannedQuery {
  planned_phonetic_union_query_with_source(query, None)
}

fn planned_phonetic_union_query_with_source(query: &str, generation_source: Option<(Vec<u8>, u64)>) -> PlannedQuery {
  let algorithm = HashAlgorithm::Blake3_256;
  let root = vec![0x33; algorithm.hash_length()];
  let semantic_root = vec![0x44; algorithm.hash_length()];
  let mut catalog = query_catalog(algorithm, &root, &semantic_root, "@filename", "double_metaphone_primary_ascii_v1", 2, 0x71);
  let mut alternate = query_catalog(algorithm, &root, &semantic_root, "@filename", "double_metaphone_alt_ascii_v1", 2, 0x73);
  assert_eq!(catalog.scopes[0].scope_id, alternate.scopes[0].scope_id);
  assert_eq!(catalog.scopes[0].value_store_id, alternate.scopes[0].value_store_id);
  catalog.scopes[0].indexes.append(&mut alternate.scopes[0].indexes);
  catalog.scopes[0].indexes.sort_by(|left, right| left.index_id.cmp(&right.index_id));
  if let Some((source_namespace_root, coverage_publication_sequence)) = generation_source {
    for index in &mut catalog.scopes[0].indexes {
      let generation = index.selected_generation.as_mut().unwrap();
      generation.source_namespace_root = source_namespace_root.clone();
      generation.coverage_publication_sequence = coverage_publication_sequence;
    }
  }
  let expression = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@filename".to_string(),
    operation: QueryPredicateOperationV1::Phonetic(CanonicalConfigValueV1::String(query.to_string())),
  });
  let context = QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &root, &semantic_root, 41).unwrap();
  let catalogs = vec![catalog];
  let plan = plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 16,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap();
  assert!(
    matches!(plan.predicates()[0].scopes()[0].driver(), QueryPlanDriverV1::IndexUnion { candidate_indexes, .. } if candidate_indexes.len() == 2)
  );
  let scope_id = catalogs[0].scopes[0].scope_id.clone();
  PlannedQuery { root, scope_id, plan, catalogs }
}

fn expression_uses_field(expression: &QueryExpressionV1, field_name: &str) -> bool {
  match expression {
    QueryExpressionV1::Field(predicate) => predicate.field_name == field_name,
    QueryExpressionV1::And(children) | QueryExpressionV1::Or(children) => {
      children.iter().any(|child| expression_uses_field(child, field_name))
    }
    QueryExpressionV1::Not(child) => expression_uses_field(child, field_name),
  }
}

fn ordered_page(
  algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  owner_id: &[u8],
  generation: u64,
  page_id: u64,
  records: &[Vec<u8>],
) -> EncodedImmutableIndexArtifactV1 {
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm: algorithm,
    role,
    owner_id,
    generation,
    page_id,
    previous_page_id: 0,
    next_page_id: 0,
    records: &records.iter().map(Vec::as_slice).collect::<Vec<_>>(),
  })
  .unwrap()
}

fn leaf_directory(
  algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  owner_id: &[u8],
  generation: u64,
  pages: &[&EncodedImmutableIndexArtifactV1],
) -> EncodedImmutableIndexArtifactV1 {
  let decoded = pages.iter().map(|page| decode_ordered_page(&page.value, algorithm).unwrap()).collect::<Vec<_>>();
  let entries = decoded
    .iter()
    .map(|page| ArtifactDirectoryEntryWriteV1 {
      lower_fence: page.lower_fence,
      upper_fence: page.upper_fence,
      child_hash: &page.key,
      child_generation: page.generation,
      live_count: u64::from(page.live_count),
      tombstone_count: u64::from(page.tombstone_count),
      page_count: 1,
      logical_bytes: page.logical_live_bytes,
      minimum_page_id: page.page_id,
      maximum_page_id: page.page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    })
    .collect::<Vec<_>>();
  encode_artifact_directory(&ArtifactDirectoryWriteV1 {
    hash_algorithm: algorithm,
    role,
    owner_id,
    generation,
    level: 0,
    entries: &entries,
  })
  .unwrap()
}

#[derive(Default)]
struct Source {
  values: BTreeMap<Vec<u8>, Vec<u8>>,
  reads: usize,
}

impl Source {
  fn insert(&mut self, artifact: &EncodedImmutableIndexArtifactV1) {
    self.values.insert(artifact.key.clone(), artifact.value.clone());
  }
}

impl ArtifactCursorSourceV1 for Source {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<RetainedArtifactBytesV1, ArtifactCursorReadErrorV1> {
    self.reads += 1;
    let value = self.values.get(key).ok_or(ArtifactCursorReadErrorV1::Missing)?;
    if value.len() > maximum_bytes {
      return Err(ArtifactCursorReadErrorV1::ResourcePressure("fixture exceeds supplied read ceiling".to_string()));
    }
    Ok(RetainedArtifactBytesV1::from_bytes(value.clone()))
  }
}

fn memory(limit: u64) -> Arc<MemoryCoordinator> {
  Arc::new(MemoryCoordinator::new(MemoryPolicy::new(limit, limit + 4 * 1_024 * 1_024, 1, 1_024 * 1_024).unwrap()))
}

fn limits() -> QueryCompleteCandidateLimitsV1 {
  QueryCompleteCandidateLimitsV1::new(64, 1_024, 128, 128 * 1_024, 16_384, 2 * 1_024 * 1_024, ArtifactPageCursorLimitsV1::default())
    .unwrap()
}

fn root_authority(
  algorithm: HashAlgorithm,
  owner_id: &[u8],
  generation: u64,
  root: &EncodedImmutableIndexArtifactV1,
) -> QueryCandidateArtifactRootV1 {
  let directory = decode_artifact_directory(&root.value, algorithm).unwrap();
  QueryCandidateArtifactRootV1::new(
    root.key.clone(),
    owner_id.to_vec(),
    generation,
    ArtifactDirectoryRootSummaryV1::from_directory(&directory),
  )
  .unwrap()
}

#[test]
fn complete_postings_resolve_live_scope_identities_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let planned = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
    let query_posting = &planned.candidate.compiled_literals()[0].compiled().postings[0];
    let posting_records = [1u64, 2, 3]
      .into_iter()
      .map(|document_ordinal| {
        encode_posting_record(&PostingRecordV1 {
          tombstone: false,
          coordinate: query_posting.coordinate,
          document_ordinal,
          source_value_ordinal: 0,
          expansion_ordinal: query_posting.expansion_ordinal,
          posting_key: &query_posting.posting_key,
        })
        .unwrap()
      })
      .collect::<Vec<_>>();
    let posting_page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, 1, &posting_records);
    let posting_root = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, &[&posting_page]);

    let paths = ["/alpha.json", "/beta.json", "/gamma.json"];
    let scope_records = paths
      .iter()
      .enumerate()
      .map(|(index, path)| {
        let ordinal = u64::try_from(index).unwrap() + 1;
        let file_key = digest_parts(algorithm, &[b"file:", path.as_bytes()]);
        let revision = digest_parts(algorithm, &[b"revision:", &ordinal.to_le_bytes()]);
        encode_scope_document_record(
          &ScopeDocumentRecordV1 {
            tombstone: false,
            document_ordinal: ordinal,
            file_key: &file_key,
            record_revision_hash: &revision,
            path,
          },
          algorithm,
        )
        .unwrap()
      })
      .collect::<Vec<_>>();
    let scope_page_left = ordered_page(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, 0, &scope_records[..2]);
    let scope_page_right = ordered_page(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, 0, &scope_records[2..]);
    let scope_root =
      leaf_directory(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, &[&scope_page_left, &scope_page_right]);
    let posting_authority = root_authority(algorithm, planned.candidate.index_id(), 7, &posting_root);
    let scope_authority = root_authority(algorithm, &planned.scope_id, 5, &scope_root);
    let mut source = Source::default();
    for artifact in [&posting_page, &posting_root, &scope_page_left, &scope_page_right, &scope_root] {
      source.insert(artifact);
    }
    let memory = memory(16 * 1_024 * 1_024);
    let baseline = memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes;
    let cancellation = CancellationToken::new();
    let postings = scan_complete_posting_ordinals_v1(
      QueryCompletePostingScanRequestV1 {
        hash_algorithm: algorithm,
        selected_namespace_root: &planned.root,
        scope_id: &planned.scope_id,
        candidate: &planned.candidate,
        posting_root: Some(&posting_authority),
        memory: &memory,
        cancellation: &cancellation,
        limits: limits(),
      },
      &mut source,
    )
    .unwrap();
    assert_eq!(postings.document_ordinals(), &[1, 2, 3]);
    assert_eq!(postings.examined_posting_records(), 3);

    let identities = resolve_complete_scope_identities_v1(
      QueryCompleteScopeResolutionRequestV1 {
        hash_algorithm: algorithm,
        selected_namespace_root: &planned.root,
        scope_id: &planned.scope_id,
        scope_ordinal_root: Some(&scope_authority),
        selection: QueryScopeOrdinalSelectionV1::CandidateOrdinals(postings.document_ordinals()),
        memory: &memory,
        cancellation: &cancellation,
        limits: limits(),
      },
      &mut source,
    )
    .unwrap();
    assert_eq!(identities.examined_pages(), 2, "adjacent ScopeOrdinal candidates must share one monotonic cross-page traversal");
    let mut expected = vec![
      (digest_parts(algorithm, &[b"file:", b"/alpha.json"]), "/alpha.json"),
      (digest_parts(algorithm, &[b"file:", b"/beta.json"]), "/beta.json"),
      (digest_parts(algorithm, &[b"file:", b"/gamma.json"]), "/gamma.json"),
    ];
    expected.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
      identities.identities().iter().map(|identity| identity.path()).collect::<Vec<_>>(),
      expected.into_iter().map(|(_, path)| path).collect::<Vec<_>>()
    );
    assert!(identities.identities().windows(2).all(|pair| pair[0].file_key() < pair[1].file_key()));
    drop(identities);
    drop(postings);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, baseline);
  }
}

#[test]
fn partial_posting_scan_requires_the_exact_planner_proven_older_generation() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let planned = planned_partial_candidate(algorithm);
    assert_eq!(planned.candidate.coverage(), aeordb::engine::v4::query_planner::CompiledQueryCoverageV1::PartialExact);
    let generation = planned.candidate.selected_generation().unwrap();
    let query_posting = &planned.candidate.compiled_literals()[0].compiled().postings[0];
    let posting_records = [1u64, 2]
      .into_iter()
      .map(|document_ordinal| {
        encode_posting_record(&PostingRecordV1 {
          tombstone: false,
          coordinate: query_posting.coordinate,
          document_ordinal,
          source_value_ordinal: 0,
          expansion_ordinal: query_posting.expansion_ordinal,
          posting_key: &query_posting.posting_key,
        })
        .unwrap()
      })
      .collect::<Vec<_>>();
    let posting_page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, 1, &posting_records);
    let posting_root = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, &[&posting_page]);
    let authority = root_authority(algorithm, planned.candidate.index_id(), 7, &posting_root);
    let mut source = Source::default();
    source.insert(&posting_page);
    source.insert(&posting_root);
    let memory = memory(16 * 1_024 * 1_024);
    let cancellation = CancellationToken::new();
    let scanned = scan_partial_posting_ordinals_v1(
      QueryPartialPostingScanRequestV1 {
        hash_algorithm: algorithm,
        source_namespace_root: &generation.source_namespace_root,
        scope_id: &planned.scope_id,
        candidate: &planned.candidate,
        posting_root: Some(&authority),
        memory: &memory,
        cancellation: &cancellation,
        limits: limits(),
      },
      &mut source,
    )
    .unwrap();
    assert_eq!(scanned.document_ordinals(), &[1, 2]);
    drop(scanned);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

    let error = scan_complete_posting_ordinals_v1(
      QueryCompletePostingScanRequestV1 {
        hash_algorithm: algorithm,
        selected_namespace_root: &planned.root,
        scope_id: &planned.scope_id,
        candidate: &planned.candidate,
        posting_root: Some(&authority),
        memory: &memory,
        cancellation: &cancellation,
        limits: limits(),
      },
      &mut source,
    )
    .unwrap_err();
    assert_eq!(error.class(), QueryCompleteCandidateErrorClassV1::InvalidRequest);
    assert_eq!(error.code(), "query_candidate_coverage");

    let complete = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
    let complete_generation = complete.candidate.selected_generation().unwrap();
    let error = scan_partial_posting_ordinals_v1(
      QueryPartialPostingScanRequestV1 {
        hash_algorithm: algorithm,
        source_namespace_root: &complete_generation.source_namespace_root,
        scope_id: &complete.scope_id,
        candidate: &complete.candidate,
        posting_root: Some(&authority),
        memory: &memory,
        cancellation: &cancellation,
        limits: limits(),
      },
      &mut source,
    )
    .unwrap_err();
    assert_eq!(error.class(), QueryCompleteCandidateErrorClassV1::InvalidRequest);
    assert_eq!(error.code(), "query_candidate_coverage");

    let error = scan_partial_posting_ordinals_v1(
      QueryPartialPostingScanRequestV1 {
        hash_algorithm: algorithm,
        source_namespace_root: &planned.root,
        scope_id: &planned.scope_id,
        candidate: &planned.candidate,
        posting_root: Some(&authority),
        memory: &memory,
        cancellation: &cancellation,
        limits: limits(),
      },
      &mut source,
    )
    .unwrap_err();
    assert_eq!(error.class(), QueryCompleteCandidateErrorClassV1::InvalidRequest);
    assert_eq!(error.code(), "query_candidate_coverage");

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = scan_partial_posting_ordinals_v1(
      QueryPartialPostingScanRequestV1 {
        hash_algorithm: algorithm,
        source_namespace_root: &generation.source_namespace_root,
        scope_id: &planned.scope_id,
        candidate: &planned.candidate,
        posting_root: Some(&authority),
        memory: &memory,
        cancellation: &cancelled,
        limits: limits(),
      },
      &mut source,
    )
    .unwrap_err();
    assert_eq!(error.class(), QueryCompleteCandidateErrorClassV1::Cancelled);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

    let tiny = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(512, 1_024, 1, 128).unwrap()));
    let error = scan_partial_posting_ordinals_v1(
      QueryPartialPostingScanRequestV1 {
        hash_algorithm: algorithm,
        source_namespace_root: &generation.source_namespace_root,
        scope_id: &planned.scope_id,
        candidate: &planned.candidate,
        posting_root: Some(&authority),
        memory: &tiny,
        cancellation: &cancellation,
        limits: limits(),
      },
      &mut source,
    )
    .unwrap_err();
    assert_eq!(error.class(), QueryCompleteCandidateErrorClassV1::ResourceLimit);
    assert_eq!(tiny.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn complete_candidate_scans_fail_closed_on_cancellation_pressure_and_missing_correctness_artifacts() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let query_posting = &planned.candidate.compiled_literals()[0].compiled().postings[0];
  let record = encode_posting_record(&PostingRecordV1 {
    tombstone: false,
    coordinate: query_posting.coordinate,
    document_ordinal: 1,
    source_value_ordinal: 0,
    expansion_ordinal: query_posting.expansion_ordinal,
    posting_key: &query_posting.posting_key,
  })
  .unwrap();
  let page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, 1, &[record]);
  let root = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, &[&page]);
  let authority = root_authority(algorithm, planned.candidate.index_id(), 7, &root);

  let mut source = Source::default();
  source.insert(&root);
  let error = scan_complete_posting_ordinals_v1(
    QueryCompletePostingScanRequestV1 {
      hash_algorithm: algorithm,
      selected_namespace_root: &planned.root,
      scope_id: &planned.scope_id,
      candidate: &planned.candidate,
      posting_root: Some(&authority),
      memory: &memory(16 * 1_024 * 1_024),
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut source,
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryCompleteCandidateErrorClassV1::CorruptSource);

  source.insert(&page);
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let error = scan_complete_posting_ordinals_v1(
    QueryCompletePostingScanRequestV1 {
      hash_algorithm: algorithm,
      selected_namespace_root: &planned.root,
      scope_id: &planned.scope_id,
      candidate: &planned.candidate,
      posting_root: Some(&authority),
      memory: &memory(16 * 1_024 * 1_024),
      cancellation: &cancellation,
      limits: limits(),
    },
    &mut source,
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryCompleteCandidateErrorClassV1::Cancelled);

  let tiny = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(512, 1_024, 1, 128).unwrap()));
  let error = scan_complete_posting_ordinals_v1(
    QueryCompletePostingScanRequestV1 {
      hash_algorithm: algorithm,
      selected_namespace_root: &planned.root,
      scope_id: &planned.scope_id,
      candidate: &planned.candidate,
      posting_root: Some(&authority),
      memory: &tiny,
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut source,
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryCompleteCandidateErrorClassV1::ResourceLimit);
  assert_eq!(tiny.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[derive(Clone)]
struct PostingFixtureRow {
  coordinate: u64,
  key: Vec<u8>,
  document_ordinal: u64,
  tombstone: bool,
}

fn posting_fixture_records(rows: &mut [PostingFixtureRow]) -> Vec<Vec<u8>> {
  rows.sort_unstable_by(|left, right| {
    left
      .coordinate
      .cmp(&right.coordinate)
      .then_with(|| left.key.cmp(&right.key))
      .then_with(|| left.document_ordinal.cmp(&right.document_ordinal))
  });
  rows
    .iter()
    .map(|row| {
      encode_posting_record(&PostingRecordV1 {
        tombstone: row.tombstone,
        coordinate: row.coordinate,
        document_ordinal: row.document_ordinal,
        source_value_ordinal: 0,
        expansion_ordinal: 0,
        posting_key: &row.key,
      })
      .unwrap()
    })
    .collect()
}

#[test]
fn all_posting_intersection_and_ordered_ranges_remain_conservative_supersets() {
  let algorithm = HashAlgorithm::Blake3_256;
  let contains = planned_candidate_with(
    algorithm,
    "@filename",
    "unicode_trigram_v1",
    2,
    QueryPredicateOperationV1::Contains(CanonicalConfigValueV1::String("alphabet".to_string())),
  );
  let QueryCoordinateConstraintV1::Points(coordinates) = contains.candidate.coordinate_constraint() else {
    panic!("contains candidate should compile to point constraints")
  };
  let mut points = contains
    .candidate
    .compiled_literals()
    .iter()
    .flat_map(|literal| literal.compiled().postings.iter())
    .filter(|posting| coordinates.binary_search(&posting.coordinate).is_ok())
    .map(|posting| (posting.coordinate, posting.posting_key.clone()))
    .collect::<Vec<_>>();
  points.sort_unstable();
  points.dedup();
  assert!(points.len() > 1);
  let mut rows = Vec::new();
  for (coordinate, key) in &points {
    rows.push(PostingFixtureRow { coordinate: *coordinate, key: key.clone(), document_ordinal: 1, tombstone: false });
    rows.push(PostingFixtureRow { coordinate: *coordinate, key: key.clone(), document_ordinal: 3, tombstone: true });
  }
  rows.push(PostingFixtureRow { coordinate: points[0].0, key: points[0].1.clone(), document_ordinal: 2, tombstone: false });
  let records = posting_fixture_records(&mut rows);
  let page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, contains.candidate.index_id(), 7, 1, &records);
  let root = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, contains.candidate.index_id(), 7, &[&page]);
  let authority = root_authority(algorithm, contains.candidate.index_id(), 7, &root);
  let mut source = Source::default();
  source.insert(&root);
  source.insert(&page);
  let candidates = scan_complete_posting_ordinals_v1(
    QueryCompletePostingScanRequestV1 {
      hash_algorithm: algorithm,
      selected_namespace_root: &contains.root,
      scope_id: &contains.scope_id,
      candidate: &contains.candidate,
      posting_root: Some(&authority),
      memory: &memory(16 * 1_024 * 1_024),
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut source,
  )
  .unwrap();
  assert_eq!(candidates.document_ordinals(), &[1]);

  let range = planned_candidate_with(
    algorithm,
    "@filename",
    "utf8_binary_order_v1",
    2,
    QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::String("middle".to_string())),
  );
  let QueryCoordinateConstraintV1::InclusiveRange { start, end, .. } = range.candidate.coordinate_constraint() else {
    panic!("ordered candidate should compile to one coordinate range")
  };
  assert_eq!(*end, u64::MAX);
  let mut range_rows = vec![
    PostingFixtureRow { coordinate: start.saturating_sub(1), key: b"before".to_vec(), document_ordinal: 1, tombstone: false },
    PostingFixtureRow { coordinate: *start, key: b"collision".to_vec(), document_ordinal: 2, tombstone: false },
    PostingFixtureRow { coordinate: start.saturating_add(1), key: b"after".to_vec(), document_ordinal: 3, tombstone: false },
  ];
  let records = posting_fixture_records(&mut range_rows);
  let page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, range.candidate.index_id(), 7, 1, &records);
  let root = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, range.candidate.index_id(), 7, &[&page]);
  let authority = root_authority(algorithm, range.candidate.index_id(), 7, &root);
  let mut source = Source::default();
  source.insert(&root);
  source.insert(&page);
  let candidates = scan_complete_posting_ordinals_v1(
    QueryCompletePostingScanRequestV1 {
      hash_algorithm: algorithm,
      selected_namespace_root: &range.root,
      scope_id: &range.scope_id,
      candidate: &range.candidate,
      posting_root: Some(&authority),
      memory: &memory(16 * 1_024 * 1_024),
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut source,
  )
  .unwrap();
  assert_eq!(candidates.document_ordinals(), &[2, 3], "the boundary collision remains for authoritative recheck");
}

#[test]
fn scope_universe_skips_tombstones_but_candidate_references_to_them_are_corrupt() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let records = [(1u64, "/one.json", false), (2u64, "/deleted.json", true), (3u64, "/three.json", false)]
    .into_iter()
    .map(|(ordinal, path, tombstone)| {
      let file_key = digest_parts(algorithm, &[b"file:", path.as_bytes()]);
      let revision = digest_parts(algorithm, &[b"revision:", &ordinal.to_le_bytes()]);
      encode_scope_document_record(
        &ScopeDocumentRecordV1 { tombstone, document_ordinal: ordinal, file_key: &file_key, record_revision_hash: &revision, path },
        algorithm,
      )
      .unwrap()
    })
    .collect::<Vec<_>>();
  let page = ordered_page(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, 0, &records);
  let root = leaf_directory(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, &[&page]);
  let authority = root_authority(algorithm, &planned.scope_id, 5, &root);
  let mut source = Source::default();
  source.insert(&root);
  source.insert(&page);
  let universe = resolve_complete_scope_identities_v1(
    QueryCompleteScopeResolutionRequestV1 {
      hash_algorithm: algorithm,
      selected_namespace_root: &planned.root,
      scope_id: &planned.scope_id,
      scope_ordinal_root: Some(&authority),
      selection: QueryScopeOrdinalSelectionV1::AllLive,
      memory: &memory(16 * 1_024 * 1_024),
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut source,
  )
  .unwrap();
  assert_eq!(universe.identities().len(), 2);
  assert!(universe.identities().iter().all(|identity| identity.document_ordinal() != 2));

  let error = resolve_complete_scope_identities_v1(
    QueryCompleteScopeResolutionRequestV1 {
      hash_algorithm: algorithm,
      selected_namespace_root: &planned.root,
      scope_id: &planned.scope_id,
      scope_ordinal_root: Some(&authority),
      selection: QueryScopeOrdinalSelectionV1::CandidateOrdinals(&[2]),
      memory: &memory(16 * 1_024 * 1_024),
      cancellation: &CancellationToken::new(),
      limits: limits(),
    },
    &mut source,
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryCompleteCandidateErrorClassV1::CorruptSource);
  assert_eq!(error.code(), "query_candidate_scope_ordinal_tombstone");
}

#[derive(Clone)]
struct ExecutionDocument {
  scope_id: Vec<u8>,
  ordinal: u64,
  file_key: Vec<u8>,
  revision: Vec<u8>,
  path: String,
  fields: BTreeMap<String, Vec<u8>>,
}

struct ExecutionFields<'a> {
  document: &'a ExecutionDocument,
}

impl QueryAuthoritativeFieldSourceV1 for ExecutionFields<'_> {
  fn scan_field_values(
    &mut self,
    request: QueryExecutionFieldReadRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeValueVisitorV1,
  ) -> Result<QueryExecutionFieldReadReceiptV1, QueryExecutionScanErrorV1> {
    let value = self.document.fields.get(request.field_name).unwrap();
    visitor.visit(value).map_err(QueryExecutionScanErrorV1::Visitor)?;
    Ok(QueryExecutionFieldReadReceiptV1 {
      selected_namespace_root: request.selected_namespace_root.to_vec(),
      scope_id: request.scope_id.to_vec(),
      file_key: request.file_key.to_vec(),
      record_revision: request.record_revision.to_vec(),
      field_name: request.field_name.to_string(),
      state: QueryExecutionFieldStateV1::Values,
      value_count: 1,
      canonical_value_bytes: value.len() as u64,
      complete: true,
    })
  }
}

struct AuthoritativeExecutionSource {
  root: Vec<u8>,
  publication_sequence: u64,
  documents: Vec<ExecutionDocument>,
}

impl QueryAuthoritativeScopeSourceV1 for AuthoritativeExecutionSource {
  fn scan_scope(
    &mut self,
    request: QueryExecutionScopeScanRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryExecutionScopeScanReceiptV1, QueryExecutionScanErrorV1> {
    let mut count = 0u64;
    for document in self.documents.iter().filter(|document| document.scope_id == request.scope_id) {
      count += 1;
      let mut fields = ExecutionFields { document };
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
      document_count: count,
      complete: true,
    })
  }
}

struct CandidateExecutionSource {
  artifacts: Source,
  posting_roots: BTreeMap<Vec<u8>, QueryCandidateArtifactRootV1>,
  scope_root: QueryCandidateArtifactRootV1,
  documents: Vec<ExecutionDocument>,
  rechecks: u64,
  posting_root_reads: u64,
  posting_receipt_complete: bool,
  scope_receipt_complete: bool,
  recheck_visitor_calls: u64,
  squelch_recheck_visitor_error: bool,
  incomplete_recheck_after: Option<u64>,
}

impl ArtifactCursorSourceV1 for CandidateExecutionSource {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<RetainedArtifactBytesV1, ArtifactCursorReadErrorV1> {
    self.artifacts.read_immutable_artifact(key, maximum_bytes)
  }
}

impl QueryCompleteCandidateSourceV1 for CandidateExecutionSource {
  fn resolve_complete_posting_root(
    &mut self,
    request: QueryCompletePostingRootRequestV1<'_>,
  ) -> Result<QueryCompletePostingRootReceiptV1, aeordb::engine::v4::query_executor::QueryExecutionSourceErrorV1> {
    let generation = request.candidate.selected_generation().unwrap();
    self.posting_root_reads += 1;
    let root = self.posting_roots.get(request.candidate.index_id()).cloned();
    Ok(QueryCompletePostingRootReceiptV1 {
      selected_namespace_root: request.selected_namespace_root.to_vec(),
      publication_sequence: request.publication_sequence,
      scope_id: request.scope_id.to_vec(),
      index_id: request.candidate.index_id().to_vec(),
      generation: generation.generation,
      generation_manifest_hash: generation.manifest_hash.clone(),
      coverage_source_root: generation.source_namespace_root.clone(),
      root,
      complete: self.posting_receipt_complete,
    })
  }

  fn resolve_complete_scope_root(
    &mut self,
    request: QueryCompleteScopeRootRequestV1<'_>,
  ) -> Result<QueryCompleteScopeRootReceiptV1, aeordb::engine::v4::query_executor::QueryExecutionSourceErrorV1> {
    Ok(QueryCompleteScopeRootReceiptV1 {
      selected_namespace_root: request.selected_namespace_root.to_vec(),
      publication_sequence: request.publication_sequence,
      scope_id: request.scope_id.to_vec(),
      root: Some(self.scope_root.clone()),
      complete: self.scope_receipt_complete,
    })
  }

  fn recheck_complete_candidate(
    &mut self,
    request: QueryCandidateRecheckRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryCandidateRecheckReceiptV1, QueryExecutionScanErrorV1> {
    self.rechecks += 1;
    let document = self.documents.iter().find(|document| document.file_key == request.file_key).unwrap();
    let mut fields = ExecutionFields { document };
    let mut visit = Ok(());
    for _ in 0..self.recheck_visitor_calls {
      if let Err(error) = visitor
        .visit(
          QueryExecutionDocumentV1 { file_key: &document.file_key, record_revision: &document.revision, path: &document.path },
          &mut fields,
        )
        .map_err(QueryExecutionScanErrorV1::Visitor)
      {
        visit = Err(error);
        break;
      }
    }
    if !self.squelch_recheck_visitor_error {
      visit?;
    }
    Ok(QueryCandidateRecheckReceiptV1 {
      selected_namespace_root: request.selected_namespace_root.to_vec(),
      publication_sequence: request.publication_sequence,
      scope_id: request.scope_id.to_vec(),
      file_key: request.file_key.to_vec(),
      indexed_revision: request.indexed_revision.to_vec(),
      indexed_path: request.indexed_path.to_string(),
      document_count: 1,
      complete: self.incomplete_recheck_after.is_none_or(|maximum_complete| self.rechecks <= maximum_complete),
    })
  }
}

fn execution_limits() -> QueryExecutionLimitsV1 {
  QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(128, 1_024, 128, 1_000_000).unwrap(),
    QueryExecutionByteLimitsV1::new(1_024 * 1_024, 2 * 1_024 * 1_024, 4 * 1_024 * 1_024).unwrap(),
  )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompleteCandidateSinkFault {
  None,
  Begin,
  Push,
  Commit,
}

struct CompleteCandidateRecordingSink {
  fault: CompleteCandidateSinkFault,
  active: bool,
  begin_calls: u64,
  selected_namespace_root: Vec<u8>,
  scope_id: Option<Vec<u8>>,
  staged: Vec<(Vec<u8>, Vec<u8>, String)>,
  committed: Vec<(Vec<u8>, Vec<u8>, String)>,
  committed_receipt: Option<(u64, u64, u64)>,
  rollbacks: u64,
}

impl CompleteCandidateRecordingSink {
  fn new(fault: CompleteCandidateSinkFault) -> Self {
    Self {
      fault,
      active: false,
      begin_calls: 0,
      selected_namespace_root: Vec::new(),
      scope_id: None,
      staged: Vec::new(),
      committed: Vec::new(),
      committed_receipt: None,
      rollbacks: 0,
    }
  }

  fn error(&self) -> QueryExecutionSinkErrorV1 {
    QueryExecutionSinkErrorV1::new(
      QueryExecutionSinkErrorClassV1::ResourceLimit,
      "fixture_complete_candidate_sink",
      "injected complete-candidate sink failure",
    )
  }
}

impl QueryExecutionMatchSinkV1 for CompleteCandidateRecordingSink {
  fn begin_batch(&mut self, batch: QueryExecutionSinkBatchV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    assert!(!self.active);
    self.begin_calls += 1;
    if self.fault == CompleteCandidateSinkFault::Begin {
      return Err(self.error());
    }
    self.active = true;
    self.selected_namespace_root = batch.selected_namespace_root.to_vec();
    self.scope_id = batch.scope_id.map(<[u8]>::to_vec);
    self.staged.clear();
    Ok(())
  }

  fn push_match(&mut self, matched: QueryExecutionMatchRefV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    assert!(self.active);
    if self.fault == CompleteCandidateSinkFault::Push {
      return Err(self.error());
    }
    let path = match matched.path {
      QueryExecutionMatchPathV1::Canonical(path) => path.to_string(),
      QueryExecutionMatchPathV1::RequiresSelectedRootLookup => {
        return Err(QueryExecutionSinkErrorV1::new(
          QueryExecutionSinkErrorClassV1::Internal,
          "fixture_complete_candidate_path",
          "complete candidate unexpectedly omitted its rechecked canonical path",
        ));
      }
    };
    self.staged.push((matched.file_key.to_vec(), matched.record_revision.to_vec(), path));
    Ok(())
  }

  fn commit_batch(&mut self, receipt: QueryExecutionSinkBatchReceiptV1<'_>) -> Result<(), QueryExecutionSinkErrorV1> {
    assert!(self.active);
    assert_eq!(receipt.selected_namespace_root, self.selected_namespace_root);
    assert_eq!(receipt.scope_id, self.scope_id.as_deref());
    assert_eq!(receipt.match_count as usize, self.staged.len());
    if self.fault == CompleteCandidateSinkFault::Commit {
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

fn filename_execution_documents(algorithm: HashAlgorithm, scope_id: &[u8], paths: &[&str]) -> Vec<ExecutionDocument> {
  let mut documents = paths
    .iter()
    .enumerate()
    .map(|(index, path)| {
      let ordinal = u64::try_from(index).unwrap() + 1;
      ExecutionDocument {
        scope_id: scope_id.to_vec(),
        ordinal,
        file_key: digest_parts(algorithm, &[b"file:", path.as_bytes()]),
        revision: digest_parts(algorithm, &[b"revision:", &ordinal.to_le_bytes()]),
        path: (*path).to_string(),
        fields: BTreeMap::from([(
          "@filename".to_string(),
          encode_canonical_value(
            &CanonicalConfigValueV1::String(path.trim_start_matches('/').to_string()),
            aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
          )
          .unwrap(),
        )]),
      }
    })
    .collect::<Vec<_>>();
  documents.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));
  documents
}

fn single_complete_candidate_source(
  algorithm: HashAlgorithm,
  planned: &PlannedCandidate,
  documents: Vec<ExecutionDocument>,
  candidate_ordinals: &[u64],
) -> CandidateExecutionSource {
  let query_posting = &planned.candidate.compiled_literals()[0].compiled().postings[0];
  let mut posting_rows = candidate_ordinals
    .iter()
    .map(|document_ordinal| PostingFixtureRow {
      coordinate: query_posting.coordinate,
      key: query_posting.posting_key.clone(),
      document_ordinal: *document_ordinal,
      tombstone: false,
    })
    .collect::<Vec<_>>();
  let posting_records = posting_fixture_records(&mut posting_rows);
  let posting_page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, 1, &posting_records);
  let posting_root = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, &[&posting_page]);
  let mut by_ordinal = documents.iter().collect::<Vec<_>>();
  by_ordinal.sort_unstable_by_key(|document| document.ordinal);
  let scope_records = by_ordinal
    .iter()
    .map(|document| {
      encode_scope_document_record(
        &ScopeDocumentRecordV1 {
          tombstone: false,
          document_ordinal: document.ordinal,
          file_key: &document.file_key,
          record_revision_hash: &document.revision,
          path: &document.path,
        },
        algorithm,
      )
      .unwrap()
    })
    .collect::<Vec<_>>();
  let scope_page = ordered_page(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, 0, &scope_records);
  let scope_root = leaf_directory(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, &[&scope_page]);
  let mut artifacts = Source::default();
  for artifact in [&posting_page, &posting_root, &scope_page, &scope_root] {
    artifacts.insert(artifact);
  }
  CandidateExecutionSource {
    artifacts,
    posting_roots: BTreeMap::from([(
      planned.candidate.index_id().to_vec(),
      root_authority(algorithm, planned.candidate.index_id(), 7, &posting_root),
    )]),
    scope_root: root_authority(algorithm, &planned.scope_id, 5, &scope_root),
    documents,
    rechecks: 0,
    posting_root_reads: 0,
    posting_receipt_complete: true,
    scope_receipt_complete: true,
    recheck_visitor_calls: 1,
    squelch_recheck_visitor_error: false,
    incomplete_recheck_after: None,
  }
}

struct PanicAuthoritativeSource;

impl QueryAuthoritativeScopeSourceV1 for PanicAuthoritativeSource {
  fn scan_scope(
    &mut self,
    _request: QueryExecutionScopeScanRequestV1<'_>,
    _visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryExecutionScopeScanReceiptV1, QueryExecutionScanErrorV1> {
    panic!("terminal accelerator failure incorrectly entered authoritative fallback")
  }
}

struct CorruptAuthoritativeSource;

impl QueryAuthoritativeScopeSourceV1 for CorruptAuthoritativeSource {
  fn scan_scope(
    &mut self,
    _request: QueryExecutionScopeScanRequestV1<'_>,
    _visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryExecutionScopeScanReceiptV1, QueryExecutionScanErrorV1> {
    Err(QueryExecutionScanErrorV1::Source(QueryExecutionSourceErrorV1::new(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "fixture_authoritative_corrupt",
      "authoritative namespace walk failed closure validation",
    )))
  }
}

struct FailingCompleteSource {
  class: QueryExecutionSourceErrorClassV1,
  code: &'static str,
}

impl ArtifactCursorSourceV1 for FailingCompleteSource {
  fn read_immutable_artifact(&mut self, _key: &[u8], _maximum_bytes: usize) -> Result<RetainedArtifactBytesV1, ArtifactCursorReadErrorV1> {
    panic!("internal complete-source failure unexpectedly read an artifact")
  }
}

impl QueryCompleteCandidateSourceV1 for FailingCompleteSource {
  fn resolve_complete_posting_root(
    &mut self,
    _request: QueryCompletePostingRootRequestV1<'_>,
  ) -> Result<QueryCompletePostingRootReceiptV1, QueryExecutionSourceErrorV1> {
    Err(QueryExecutionSourceErrorV1::new(self.class, self.code, "fixture complete source failed"))
  }

  fn resolve_complete_scope_root(
    &mut self,
    _request: QueryCompleteScopeRootRequestV1<'_>,
  ) -> Result<QueryCompleteScopeRootReceiptV1, QueryExecutionSourceErrorV1> {
    panic!("internal complete-source failure unexpectedly resolved ScopeOrdinal")
  }

  fn recheck_complete_candidate(
    &mut self,
    _request: QueryCandidateRecheckRequestV1<'_>,
    _visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryCandidateRecheckReceiptV1, QueryExecutionScanErrorV1> {
    panic!("internal complete-source failure unexpectedly rechecked a candidate")
  }
}

#[test]
fn complete_candidate_execution_rechecks_false_positives_against_authoritative_truth() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let mut documents = ["/alpha.json", "/beta.json", "/gamma.json"]
    .into_iter()
    .enumerate()
    .map(|(index, path)| {
      let ordinal = u64::try_from(index).unwrap() + 1;
      ExecutionDocument {
        scope_id: planned.scope_id.clone(),
        ordinal,
        file_key: digest_parts(algorithm, &[b"file:", path.as_bytes()]),
        revision: digest_parts(algorithm, &[b"revision:", &ordinal.to_le_bytes()]),
        path: path.to_string(),
        fields: BTreeMap::from([(
          "@filename".to_string(),
          encode_canonical_value(
            &CanonicalConfigValueV1::String(path.trim_start_matches('/').to_string()),
            aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
          )
          .unwrap(),
        )]),
      }
    })
    .collect::<Vec<_>>();
  documents.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));
  let query_posting = &planned.candidate.compiled_literals()[0].compiled().postings[0];
  let mut posting_rows = vec![
    PostingFixtureRow {
      coordinate: query_posting.coordinate,
      key: query_posting.posting_key.clone(),
      document_ordinal: 1,
      tombstone: false,
    },
    PostingFixtureRow {
      coordinate: query_posting.coordinate,
      key: query_posting.posting_key.clone(),
      document_ordinal: 2,
      tombstone: false,
    },
  ];
  let posting_records = posting_fixture_records(&mut posting_rows);
  let posting_page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, 1, &posting_records);
  let posting_root = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, &[&posting_page]);
  let scope_records = ["/alpha.json", "/beta.json", "/gamma.json"]
    .into_iter()
    .enumerate()
    .map(|(index, path)| {
      let ordinal = u64::try_from(index).unwrap() + 1;
      let document = documents.iter().find(|document| document.ordinal == ordinal).unwrap();
      encode_scope_document_record(
        &ScopeDocumentRecordV1 {
          tombstone: false,
          document_ordinal: ordinal,
          file_key: &document.file_key,
          record_revision_hash: &document.revision,
          path,
        },
        algorithm,
      )
      .unwrap()
    })
    .collect::<Vec<_>>();
  let scope_page = ordered_page(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, 0, &scope_records);
  let scope_root = leaf_directory(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, &[&scope_page]);
  let mut artifacts = Source::default();
  for artifact in [&posting_page, &posting_root, &scope_page, &scope_root] {
    artifacts.insert(artifact);
  }
  let mut authoritative_source =
    AuthoritativeExecutionSource { root: planned.root.clone(), publication_sequence: 41, documents: documents.clone() };
  let authoritative_memory = memory(32 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let authoritative = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut authoritative_source,
    memory: &authoritative_memory,
    cancellation: &cancellation,
    limits: execution_limits(),
  })
  .unwrap();

  let mut candidate_source = CandidateExecutionSource {
    artifacts,
    posting_roots: BTreeMap::from([(
      planned.candidate.index_id().to_vec(),
      root_authority(algorithm, planned.candidate.index_id(), 7, &posting_root),
    )]),
    scope_root: root_authority(algorithm, &planned.scope_id, 5, &scope_root),
    documents,
    rechecks: 0,
    posting_root_reads: 0,
    posting_receipt_complete: true,
    scope_receipt_complete: true,
    recheck_visitor_calls: 1,
    squelch_recheck_visitor_error: false,
    incomplete_recheck_after: None,
  };
  let candidate_memory = memory(32 * 1_024 * 1_024);
  let candidate = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut candidate_source,
    memory: &candidate_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
  })
  .unwrap();
  assert_eq!(
    candidate.execution().matches().iter().map(|row| (row.file_key(), row.record_revision())).collect::<Vec<_>>(),
    authoritative.matches().iter().map(|row| (row.file_key(), row.record_revision())).collect::<Vec<_>>()
  );
  assert_eq!(candidate.execution().matches().len(), 1);
  assert_eq!(candidate_source.rechecks, 2, "false-positive Posting candidate must be rechecked and rejected");
  assert_eq!(candidate.authoritative_rechecks(), 2);
  drop(candidate);
  assert_eq!(candidate_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  candidate_source.posting_receipt_complete = false;
  let error = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut candidate_source,
    memory: &candidate_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
  assert_eq!(error.code(), "query_candidate_posting_root_receipt");
  assert_eq!(candidate_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  candidate_source.posting_receipt_complete = true;

  candidate_source.scope_receipt_complete = false;
  let error = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut candidate_source,
    memory: &candidate_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
  assert_eq!(error.code(), "query_candidate_scope_root_receipt");
  assert_eq!(candidate_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  candidate_source.scope_receipt_complete = true;

  let pressure_memory =
    Arc::new(MemoryCoordinator::new(MemoryPolicy::new(9 * 1_024 * 1_024, 10 * 1_024 * 1_024, 1, 1_024 * 1_024).unwrap()));
  let error = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut candidate_source,
    memory: &pressure_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_candidate_memory_pressure");
  assert_eq!(pressure_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let original_revision = candidate_source.documents.iter().find(|document| document.ordinal == 1).unwrap().revision.clone();
  candidate_source.documents.iter_mut().find(|document| document.ordinal == 1).unwrap().revision = vec![0xee; algorithm.hash_length()];
  let error = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut candidate_source,
    memory: &candidate_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
  assert_eq!(error.code(), "query_candidate_authoritative_recheck");
  candidate_source.documents.iter_mut().find(|document| document.ordinal == 1).unwrap().revision = original_revision;

  let original_path = candidate_source.documents.iter().find(|document| document.ordinal == 1).unwrap().path.clone();
  candidate_source.documents.iter_mut().find(|document| document.ordinal == 1).unwrap().path = "/renamed.json".to_string();
  let error = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut candidate_source,
    memory: &candidate_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
  assert_eq!(error.code(), "query_candidate_authoritative_recheck");
  assert_eq!(candidate_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  candidate_source.documents.iter_mut().find(|document| document.ordinal == 1).unwrap().path = original_path;

  candidate_source.recheck_visitor_calls = 2;
  let error = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut candidate_source,
    memory: &candidate_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
  assert_eq!(error.code(), "query_candidate_authoritative_recheck");
  assert_eq!(candidate_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  candidate_source.recheck_visitor_calls = 1;

  candidate_source.squelch_recheck_visitor_error = true;
  candidate_source.documents.iter_mut().find(|document| document.ordinal == 1).unwrap().fields.insert("@filename".to_string(), vec![0xff]);
  let error = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut candidate_source,
    memory: &candidate_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
  assert_eq!(error.code(), "query_execution_canonical_value");
  assert_eq!(candidate_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn complete_candidate_streaming_sink_matches_collected_root_and_scope_results() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let documents = filename_execution_documents(algorithm, &planned.scope_id, &["/alpha.json", "/beta.json"]);
  let cancellation = CancellationToken::new();

  let collected_memory = memory(32 * 1_024 * 1_024);
  let mut collected_source = single_complete_candidate_source(algorithm, &planned, documents.clone(), &[1, 2]);
  let collected = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut collected_source,
    memory: &collected_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
  })
  .unwrap();
  let expected = collected
    .execution()
    .matches()
    .iter()
    .map(|matched| (matched.file_key().to_vec(), matched.record_revision().to_vec(), matched.path().to_string()))
    .collect::<Vec<_>>();

  let root_memory = memory(32 * 1_024 * 1_024);
  let mut root_source = single_complete_candidate_source(algorithm, &planned, documents.clone(), &[1, 2]);
  let mut root_sink = CompleteCandidateRecordingSink::new(CompleteCandidateSinkFault::None);
  let root = execute_complete_candidate_root_query_into_v1(
    QueryCompleteCandidateExecutionRequestV1 {
      plan: &planned.plan,
      catalogs: &planned.catalogs,
      source: &mut root_source,
      memory: &root_memory,
      cancellation: &cancellation,
      execution_limits: execution_limits(),
      candidate_limits: limits(),
    },
    &mut root_sink,
  )
  .unwrap();
  assert_eq!(root.receipt().selected_namespace_root(), planned.root);
  assert_eq!(root.receipt().scope_id(), None);
  assert_eq!(root.receipt().match_count(), expected.len() as u64);
  assert_eq!(root_sink.committed, expected);
  assert_eq!(root.examined_posting_records(), collected.examined_posting_records());
  assert_eq!(root.examined_artifact_pages(), collected.examined_artifact_pages());
  assert_eq!(root.resolved_candidate_identities(), collected.resolved_candidate_identities());
  assert_eq!(root.authoritative_rechecks(), collected.authoritative_rechecks());
  assert_eq!(root_sink.committed_receipt, Some((1, 2, 2)));
  assert_eq!(root_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let scope_memory = memory(32 * 1_024 * 1_024);
  let mut scope_source = single_complete_candidate_source(algorithm, &planned, documents, &[1, 2]);
  let mut scope_sink = CompleteCandidateRecordingSink::new(CompleteCandidateSinkFault::None);
  let scope = execute_complete_candidate_scope_query_into_v1(
    QueryCompleteCandidateScopeExecutionRequestV1 {
      plan: &planned.plan,
      catalogs: &planned.catalogs,
      scope_id: &planned.scope_id,
      source: &mut scope_source,
      memory: &scope_memory,
      cancellation: &cancellation,
      execution_limits: execution_limits(),
      candidate_limits: limits(),
    },
    &mut scope_sink,
  )
  .unwrap();
  assert_eq!(scope.receipt().selected_namespace_root(), planned.root);
  assert_eq!(scope.receipt().scope_id(), Some(planned.scope_id.as_slice()));
  assert_eq!(scope_sink.committed, root_sink.committed);
  assert_eq!(scope_sink.committed_receipt, root_sink.committed_receipt);
  assert_eq!(scope.authoritative_rechecks(), root.authoritative_rechecks());
  assert_eq!(scope_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  drop(collected);
  assert_eq!(collected_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn complete_candidate_streaming_sink_rolls_back_rows_staged_before_late_accelerator_failure() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let documents = filename_execution_documents(algorithm, &planned.scope_id, &["/alpha.json", "/beta.json"]);
  let mut source = single_complete_candidate_source(algorithm, &planned, documents, &[1, 2]);
  source.incomplete_recheck_after = Some(1);
  let memory = memory(32 * 1_024 * 1_024);
  let mut sink = CompleteCandidateRecordingSink::new(CompleteCandidateSinkFault::None);

  let error = execute_complete_candidate_root_query_into_v1(
    QueryCompleteCandidateExecutionRequestV1 {
      plan: &planned.plan,
      catalogs: &planned.catalogs,
      source: &mut source,
      memory: &memory,
      cancellation: &CancellationToken::new(),
      execution_limits: execution_limits(),
      candidate_limits: limits(),
    },
    &mut sink,
  )
  .unwrap_err();

  assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
  assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Execution);
  assert_eq!(error.code(), "query_candidate_authoritative_recheck");
  assert_eq!(source.rechecks, 2);
  assert_eq!(sink.begin_calls, 1);
  assert_eq!(sink.rollbacks, 1);
  assert!(sink.staged.is_empty());
  assert!(sink.committed.is_empty());
  assert!(sink.committed_receipt.is_none());
  assert!(!sink.active);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn complete_candidate_streaming_sink_failures_are_terminal_and_atomic() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let documents = filename_execution_documents(algorithm, &planned.scope_id, &["/alpha.json", "/beta.json"]);

  for fault in [CompleteCandidateSinkFault::Begin, CompleteCandidateSinkFault::Push, CompleteCandidateSinkFault::Commit] {
    let mut source = single_complete_candidate_source(algorithm, &planned, documents.clone(), &[1, 2]);
    let memory = memory(32 * 1_024 * 1_024);
    let mut sink = CompleteCandidateRecordingSink::new(fault);
    let error = execute_complete_candidate_root_query_into_v1(
      QueryCompleteCandidateExecutionRequestV1 {
        plan: &planned.plan,
        catalogs: &planned.catalogs,
        source: &mut source,
        memory: &memory,
        cancellation: &CancellationToken::new(),
        execution_limits: execution_limits(),
        candidate_limits: limits(),
      },
      &mut sink,
    )
    .unwrap_err();

    assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit, "fault {fault:?}: {error}");
    assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Sink, "fault {fault:?}: {error}");
    assert_eq!(error.code(), "fixture_complete_candidate_sink");
    assert_eq!(sink.begin_calls, 1);
    assert_eq!(sink.rollbacks, u64::from(fault != CompleteCandidateSinkFault::Begin));
    assert!(sink.staged.is_empty());
    assert!(sink.committed.is_empty());
    assert!(sink.committed_receipt.is_none());
    assert!(!sink.active);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0, "fault {fault:?}");
  }
}

fn predicate(field_name: &str, operation: QueryPredicateOperationV1) -> QueryExpressionV1 {
  QueryExpressionV1::Field(QueryPredicateV1 { field_name: field_name.to_string(), operation })
}

fn candidate_posting_point(candidate: &CompiledQueryIndexCandidateV1) -> (u64, Vec<u8>) {
  candidate_posting_point_optional(candidate).unwrap()
}

fn candidate_posting_point_optional(candidate: &CompiledQueryIndexCandidateV1) -> Option<(u64, Vec<u8>)> {
  match candidate.coordinate_constraint() {
    QueryCoordinateConstraintV1::Points(coordinates) => candidate
      .compiled_literals()
      .iter()
      .flat_map(|literal| literal.compiled().postings.iter())
      .find(|posting| coordinates.binary_search(&posting.coordinate).is_ok())
      .map(|posting| (posting.coordinate, posting.posting_key.clone())),
    QueryCoordinateConstraintV1::InclusiveRange { start, .. } => {
      let posting = &candidate.compiled_literals()[0].compiled().postings[0];
      Some((*start, posting.posting_key.clone()))
    }
    QueryCoordinateConstraintV1::FullScan => panic!("complete candidate fixture cannot use full scan"),
  }
}

#[test]
fn boolean_complete_candidates_match_authoritative_and_not_uses_the_scope_universe() {
  let cases = [
    (
      "and",
      QueryExpressionV1::And(vec![
        predicate("@filename", QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string()))),
        predicate("@size", QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Unsigned(10))),
      ]),
      1usize,
      2u64,
    ),
    (
      "or",
      QueryExpressionV1::Or(vec![
        predicate("@filename", QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string()))),
        predicate("@size", QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Unsigned(10))),
      ]),
      2usize,
      3u64,
    ),
    (
      "not",
      QueryExpressionV1::Not(Box::new(predicate(
        "@filename",
        QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())),
      ))),
      2usize,
      3u64,
    ),
  ];

  for (label, expression, expected_matches, expected_rechecks) in cases {
    let planned = planned_query(expression);
    let algorithm = planned.plan.hash_algorithm();
    let rows = [(1u64, "/alpha.json", 20u64), (2u64, "/beta.json", 5u64), (3u64, "/gamma.json", 30u64)];
    let mut documents = rows
      .iter()
      .map(|(ordinal, path, size)| ExecutionDocument {
        scope_id: planned.scope_id.clone(),
        ordinal: *ordinal,
        file_key: digest_parts(algorithm, &[b"file:", path.as_bytes()]),
        revision: digest_parts(algorithm, &[b"revision:", &ordinal.to_le_bytes()]),
        path: (*path).to_string(),
        fields: BTreeMap::from([
          (
            "@filename".to_string(),
            encode_canonical_value(
              &CanonicalConfigValueV1::String(path.trim_start_matches('/').to_string()),
              aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
            )
            .unwrap(),
          ),
          (
            "@size".to_string(),
            encode_canonical_value(
              &CanonicalConfigValueV1::Unsigned(*size),
              aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
            )
            .unwrap(),
          ),
        ]),
      })
      .collect::<Vec<_>>();
    documents.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));

    let scope_records = rows
      .iter()
      .map(|(ordinal, path, _)| {
        let document = documents.iter().find(|document| document.ordinal == *ordinal).unwrap();
        encode_scope_document_record(
          &ScopeDocumentRecordV1 {
            tombstone: false,
            document_ordinal: *ordinal,
            file_key: &document.file_key,
            record_revision_hash: &document.revision,
            path,
          },
          algorithm,
        )
        .unwrap()
      })
      .collect::<Vec<_>>();
    let scope_page = ordered_page(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, 0, &scope_records);
    let scope_root = leaf_directory(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, &[&scope_page]);
    let mut artifacts = Source::default();
    artifacts.insert(&scope_page);
    artifacts.insert(&scope_root);
    let mut posting_roots = BTreeMap::new();
    for predicate in planned.plan.predicates() {
      let candidate = &predicate.scopes()[0].candidates()[0];
      let (coordinate, key) = candidate_posting_point(candidate);
      let candidate_ordinals = if predicate.field_name() == "@filename" { &[1u64, 2][..] } else { &[1u64, 2, 3][..] };
      let mut posting_rows = candidate_ordinals
        .iter()
        .map(|ordinal| PostingFixtureRow { coordinate, key: key.clone(), document_ordinal: *ordinal, tombstone: false })
        .collect::<Vec<_>>();
      let records = posting_fixture_records(&mut posting_rows);
      let page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, candidate.index_id(), 7, 1, &records);
      let root = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, candidate.index_id(), 7, &[&page]);
      posting_roots.insert(candidate.index_id().to_vec(), root_authority(algorithm, candidate.index_id(), 7, &root));
      artifacts.insert(&page);
      artifacts.insert(&root);
    }

    let mut authoritative_source =
      AuthoritativeExecutionSource { root: planned.root.clone(), publication_sequence: 41, documents: documents.clone() };
    let cancellation = CancellationToken::new();
    let authoritative = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
      plan: &planned.plan,
      catalogs: &planned.catalogs,
      source: &mut authoritative_source,
      memory: &memory(32 * 1_024 * 1_024),
      cancellation: &cancellation,
      limits: execution_limits(),
    })
    .unwrap();
    let mut candidate_source = CandidateExecutionSource {
      artifacts,
      posting_roots,
      scope_root: root_authority(algorithm, &planned.scope_id, 5, &scope_root),
      documents,
      rechecks: 0,
      posting_root_reads: 0,
      posting_receipt_complete: true,
      scope_receipt_complete: true,
      recheck_visitor_calls: 1,
      squelch_recheck_visitor_error: false,
      incomplete_recheck_after: None,
    };
    let accelerated = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
      plan: &planned.plan,
      catalogs: &planned.catalogs,
      source: &mut candidate_source,
      memory: &memory(32 * 1_024 * 1_024),
      cancellation: &cancellation,
      execution_limits: execution_limits(),
      candidate_limits: limits(),
    })
    .unwrap();
    assert_eq!(accelerated.execution().matches(), authoritative.matches(), "{label} result differs from authoritative truth");
    assert_eq!(accelerated.execution().matches().len(), expected_matches, "{label} match count");
    assert_eq!(accelerated.authoritative_rechecks(), expected_rechecks, "{label} recheck count");
    if label == "not" {
      assert_eq!(candidate_source.posting_root_reads, 0, "NOT must not complement a false-positive Posting superset");
      assert_eq!(accelerated.examined_posting_records(), 0);
    } else {
      assert_eq!(candidate_source.posting_root_reads, 2, "{label} must consume both complete predicate candidates");
    }
  }
}

fn assert_complete_index_union_matches_authoritative(
  query: &str,
  rows: [(u64, &str, &str); 3],
  expected_paths: [&str; 2],
  expected_artifact_roots: usize,
  expected_posting_records: u64,
) {
  let planned = planned_phonetic_union_query(query);
  let algorithm = planned.plan.hash_algorithm();
  let mut documents = rows
    .iter()
    .map(|(ordinal, path, filename)| ExecutionDocument {
      scope_id: planned.scope_id.clone(),
      ordinal: *ordinal,
      file_key: digest_parts(algorithm, &[b"file:", path.as_bytes()]),
      revision: digest_parts(algorithm, &[b"revision:", &ordinal.to_le_bytes()]),
      path: (*path).to_string(),
      fields: BTreeMap::from([(
        "@filename".to_string(),
        encode_canonical_value(
          &CanonicalConfigValueV1::String((*filename).to_string()),
          aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
        )
        .unwrap(),
      )]),
    })
    .collect::<Vec<_>>();
  documents.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));

  let scope_records = rows
    .iter()
    .map(|(ordinal, path, _)| {
      let document = documents.iter().find(|document| document.ordinal == *ordinal).unwrap();
      encode_scope_document_record(
        &ScopeDocumentRecordV1 {
          tombstone: false,
          document_ordinal: *ordinal,
          file_key: &document.file_key,
          record_revision_hash: &document.revision,
          path,
        },
        algorithm,
      )
      .unwrap()
    })
    .collect::<Vec<_>>();
  let scope_page = ordered_page(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, 0, &scope_records);
  let scope_root = leaf_directory(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, &[&scope_page]);
  let mut artifacts = Source::default();
  artifacts.insert(&scope_page);
  artifacts.insert(&scope_root);
  let candidates = planned.plan.predicates()[0].scopes()[0].candidates();
  assert_eq!(candidates.len(), 2);
  let mut posting_roots = BTreeMap::new();
  for candidate in candidates {
    let Some((coordinate, key)) = candidate_posting_point_optional(candidate) else {
      continue;
    };
    let mut posting_rows = rows
      .iter()
      .map(|(document_ordinal, _, _)| PostingFixtureRow {
        coordinate,
        key: key.clone(),
        document_ordinal: *document_ordinal,
        tombstone: false,
      })
      .collect::<Vec<_>>();
    let records = posting_fixture_records(&mut posting_rows);
    let page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, candidate.index_id(), 7, 1, &records);
    let root = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, candidate.index_id(), 7, &[&page]);
    posting_roots.insert(candidate.index_id().to_vec(), root_authority(algorithm, candidate.index_id(), 7, &root));
    artifacts.insert(&page);
    artifacts.insert(&root);
  }
  assert_eq!(posting_roots.len(), expected_artifact_roots);

  let cancellation = CancellationToken::new();
  let mut authoritative_source =
    AuthoritativeExecutionSource { root: planned.root.clone(), publication_sequence: 41, documents: documents.clone() };
  let authoritative_memory = memory(32 * 1_024 * 1_024);
  let authoritative = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut authoritative_source,
    memory: &authoritative_memory,
    cancellation: &cancellation,
    limits: execution_limits(),
  })
  .unwrap();
  let mut authoritative_paths = authoritative.matches().iter().map(|row| row.path()).collect::<Vec<_>>();
  authoritative_paths.sort_unstable();
  let mut expected_paths = expected_paths.to_vec();
  expected_paths.sort_unstable();
  assert_eq!(authoritative_paths, expected_paths);

  let mut candidate_source = CandidateExecutionSource {
    artifacts,
    posting_roots,
    scope_root: root_authority(algorithm, &planned.scope_id, 5, &scope_root),
    documents,
    rechecks: 0,
    posting_root_reads: 0,
    posting_receipt_complete: true,
    scope_receipt_complete: true,
    recheck_visitor_calls: 1,
    squelch_recheck_visitor_error: false,
    incomplete_recheck_after: None,
  };
  let candidate_memory = memory(32 * 1_024 * 1_024);
  let accelerated = execute_complete_candidate_root_query_v1(QueryCompleteCandidateExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut candidate_source,
    memory: &candidate_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
  })
  .unwrap();
  assert_eq!(accelerated.execution().matches(), authoritative.matches());
  assert_eq!(candidate_source.posting_root_reads, 2, "IndexUnion must validate every complete candidate generation receipt");
  assert_eq!(accelerated.examined_posting_records(), expected_posting_records);
  assert_eq!(accelerated.authoritative_rechecks(), 3, "overlapping union candidates must be deduplicated before recheck");
  drop(accelerated);
  drop(authoritative);
  assert_eq!(candidate_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  assert_eq!(authoritative_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn complete_index_unions_match_authoritative_truth_with_nonempty_and_empty_branches() {
  assert_complete_index_union_matches_authoritative(
    "Schmidt",
    [(1, "/schmidt-a.txt", "Schmidt"), (2, "/schmidt-b.txt", "Schmidt"), (3, "/brown.txt", "Brown")],
    ["/schmidt-a.txt", "/schmidt-b.txt"],
    2,
    6,
  );
  assert_complete_index_union_matches_authoritative(
    "Smith",
    [(1, "/smith.txt", "Smith"), (2, "/smythe.txt", "Smythe"), (3, "/brown.txt", "Brown")],
    ["/smith.txt", "/smythe.txt"],
    1,
    3,
  );
}

struct PartialArtifactSource {
  artifacts: Source,
  posting_root: Option<QueryCandidateArtifactRootV1>,
  posting_roots: BTreeMap<Vec<u8>, QueryCandidateArtifactRootV1>,
  scope_root: Option<QueryCandidateArtifactRootV1>,
  posting_complete: bool,
  posting_error: Option<IndexPartialSourceErrorV1>,
  posting_manifest_override: Option<Vec<u8>>,
  posting_manifest_overrides: BTreeMap<Vec<u8>, Vec<u8>>,
  scope_error: Option<IndexPartialSourceErrorV1>,
  scope_source_override: Option<Vec<u8>>,
  posting_resolutions: usize,
  scope_resolutions: usize,
}

impl ArtifactCursorSourceV1 for PartialArtifactSource {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<RetainedArtifactBytesV1, ArtifactCursorReadErrorV1> {
    self.artifacts.read_immutable_artifact(key, maximum_bytes)
  }
}

impl QueryPartialCandidateArtifactSourceV1 for PartialArtifactSource {
  fn resolve_partial_posting_root(
    &mut self,
    request: QueryPartialPostingRootRequestV1<'_>,
  ) -> Result<QueryPartialPostingRootReceiptV1, IndexPartialSourceErrorV1> {
    self.posting_resolutions += 1;
    if let Some(error) = self.posting_error.clone() {
      return Err(error);
    }
    let generation = request.candidate.selected_generation().unwrap();
    let root = self.posting_roots.get(request.candidate.index_id()).cloned().or_else(|| self.posting_root.clone());
    let generation_manifest_hash = self
      .posting_manifest_overrides
      .get(request.candidate.index_id())
      .cloned()
      .or_else(|| self.posting_manifest_override.clone())
      .unwrap_or_else(|| generation.manifest_hash.clone());
    Ok(QueryPartialPostingRootReceiptV1 {
      target_namespace_root: request.target_namespace_root.to_vec(),
      target_publication_sequence: request.target_publication_sequence,
      source_namespace_root: request.source_namespace_root.to_vec(),
      source_publication_sequence: request.source_publication_sequence,
      scope_id: request.scope_id.to_vec(),
      index_id: request.candidate.index_id().to_vec(),
      generation: generation.generation,
      generation_manifest_hash,
      root,
      complete: self.posting_complete,
    })
  }

  fn resolve_partial_scope_root(
    &mut self,
    request: QueryPartialScopeRootRequestV1<'_>,
  ) -> Result<QueryPartialScopeRootReceiptV1, IndexPartialSourceErrorV1> {
    self.scope_resolutions += 1;
    if let Some(error) = self.scope_error.clone() {
      return Err(error);
    }
    Ok(QueryPartialScopeRootReceiptV1 {
      source_namespace_root: self.scope_source_override.clone().unwrap_or_else(|| request.source_namespace_root.to_vec()),
      source_publication_sequence: request.source_publication_sequence,
      scope_id: request.scope_id.to_vec(),
      root: self.scope_root.clone(),
      complete: true,
    })
  }
}

#[derive(Clone)]
struct PartialChange {
  file_key: Vec<u8>,
  basis_revision: Option<Vec<u8>>,
  target_revision: Option<Vec<u8>>,
}

#[derive(Default)]
struct PartialComplement {
  changes: Vec<PartialChange>,
  scans: usize,
}

impl IndexChangedDocumentSourceV1 for PartialComplement {
  fn scan_changed_documents(
    &mut self,
    request: IndexChangedDocumentScanRequestV1<'_>,
    visitor: &mut dyn IndexChangedDocumentVisitorV1,
  ) -> Result<IndexChangedDocumentScanReceiptV1, IndexPartialScanErrorV1> {
    self.scans += 1;
    for change in &self.changes {
      visitor
        .visit(IndexChangedDocumentV1 {
          file_key: &change.file_key,
          basis_revision_hash: change.basis_revision.as_deref(),
          target_revision_hash: change.target_revision.as_deref(),
        })
        .map_err(IndexPartialScanErrorV1::Visitor)?;
    }
    Ok(IndexChangedDocumentScanReceiptV1 {
      source_namespace_root: request.source_namespace_root.to_vec(),
      target_namespace_root: request.target_namespace_root.to_vec(),
      covered_through_publication_sequence: request.covered_through_publication_sequence,
      target_publication_sequence: request.target_publication_sequence,
      changed_document_count: self.changes.len() as u64,
      complete: true,
    })
  }
}

#[derive(Default)]
struct PartialRechecker {
  outcomes: BTreeMap<Vec<u8>, IndexPartialRecheckOutcomeV1>,
  calls: Vec<Vec<u8>>,
}

impl IndexPartialCandidateRecheckerV1 for PartialRechecker {
  fn recheck(&mut self, request: IndexPartialRecheckRequestV1<'_>) -> Result<IndexPartialRecheckOutcomeV1, IndexPartialSourceErrorV1> {
    self.calls.push(request.file_key.to_vec());
    self
      .outcomes
      .get(request.file_key)
      .cloned()
      .ok_or_else(|| IndexPartialSourceErrorV1::corrupt("partial_fixture_recheck", "missing fixture recheck outcome"))
  }
}

fn partial_acceleration_limits() -> IndexPartialAccelerationLimitsV1 {
  IndexPartialAccelerationLimitsV1::new(128, 128, 128, 2 * 1_024 * 1_024).unwrap()
}

fn partial_artifact_source(
  algorithm: HashAlgorithm,
  scope_id: &[u8],
  candidate_rows: &[(&CompiledQueryIndexCandidateV1, u64)],
  scope_rows: &[(u64, &[u8], &[u8], &str)],
) -> PartialArtifactSource {
  let mut artifacts = Source::default();
  let mut posting_roots = BTreeMap::new();
  for (candidate, document_ordinal) in candidate_rows {
    let generation = candidate.selected_generation().unwrap();
    let (coordinate, posting_key) = candidate_posting_point(candidate);
    let posting = candidate
      .compiled_literals()
      .iter()
      .flat_map(|literal| literal.compiled().postings.iter())
      .find(|posting| posting.coordinate == coordinate && posting.posting_key == posting_key)
      .unwrap();
    let encoded = encode_posting_record(&PostingRecordV1 {
      tombstone: false,
      coordinate: posting.coordinate,
      document_ordinal: *document_ordinal,
      source_value_ordinal: 0,
      expansion_ordinal: posting.expansion_ordinal,
      posting_key: &posting.posting_key,
    })
    .unwrap();
    let page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, candidate.index_id(), generation.generation, 1, &[encoded]);
    let directory = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, candidate.index_id(), generation.generation, &[&page]);
    artifacts.insert(&page);
    artifacts.insert(&directory);
    posting_roots.insert(candidate.index_id().to_vec(), root_authority(algorithm, candidate.index_id(), generation.generation, &directory));
  }
  let encoded_scope_rows = scope_rows
    .iter()
    .map(|(document_ordinal, file_key, revision, path)| {
      encode_scope_document_record(
        &ScopeDocumentRecordV1 { tombstone: false, document_ordinal: *document_ordinal, file_key, record_revision_hash: revision, path },
        algorithm,
      )
      .unwrap()
    })
    .collect::<Vec<_>>();
  let scope_page = ordered_page(algorithm, OrderedIndexRoleV1::ScopeOrdinal, scope_id, 5, 0, &encoded_scope_rows);
  let scope_directory = leaf_directory(algorithm, OrderedIndexRoleV1::ScopeOrdinal, scope_id, 5, &[&scope_page]);
  artifacts.insert(&scope_page);
  artifacts.insert(&scope_directory);
  PartialArtifactSource {
    artifacts,
    posting_root: None,
    posting_roots,
    scope_root: Some(root_authority(algorithm, scope_id, 5, &scope_directory)),
    posting_complete: true,
    posting_error: None,
    posting_manifest_override: None,
    posting_manifest_overrides: BTreeMap::new(),
    scope_error: None,
    scope_source_override: None,
    posting_resolutions: 0,
    scope_resolutions: 0,
  }
}

#[test]
fn planner_partial_artifacts_feed_the_exact_complement_engine_at_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let planned = planned_partial_size_candidate(algorithm);
    let generation = planned.candidate.selected_generation().unwrap();
    let query_posting = &planned.candidate.compiled_literals()[0].compiled().postings[0];
    let posting_records = [1u64, 2, 3]
      .into_iter()
      .map(|document_ordinal| {
        encode_posting_record(&PostingRecordV1 {
          tombstone: false,
          coordinate: query_posting.coordinate,
          document_ordinal,
          source_value_ordinal: 0,
          expansion_ordinal: query_posting.expansion_ordinal,
          posting_key: &query_posting.posting_key,
        })
        .unwrap()
      })
      .collect::<Vec<_>>();
    let posting_page = ordered_page(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, 1, &posting_records);
    let posting_directory = leaf_directory(algorithm, OrderedIndexRoleV1::Posting, planned.candidate.index_id(), 7, &[&posting_page]);

    let paths = ["/alpha.json", "/beta.json", "/gamma.json"];
    let keys = paths.map(|path| digest_parts(algorithm, &[b"file:", path.as_bytes()]));
    let basis_revisions = [1u64, 2, 3].map(|ordinal| digest_parts(algorithm, &[b"basis:", &ordinal.to_le_bytes()]));
    let scope_records = paths
      .iter()
      .enumerate()
      .map(|(index, path)| {
        encode_scope_document_record(
          &ScopeDocumentRecordV1 {
            tombstone: false,
            document_ordinal: index as u64 + 1,
            file_key: &keys[index],
            record_revision_hash: &basis_revisions[index],
            path,
          },
          algorithm,
        )
        .unwrap()
      })
      .collect::<Vec<_>>();
    let scope_page = ordered_page(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, 0, &scope_records);
    let scope_directory = leaf_directory(algorithm, OrderedIndexRoleV1::ScopeOrdinal, &planned.scope_id, 5, &[&scope_page]);

    let target_beta = digest_parts(algorithm, &[b"target:", b"beta"]);
    let new_key = digest_parts(algorithm, &[b"file:", b"/delta.json"]);
    let new_revision = digest_parts(algorithm, &[b"target:", b"delta"]);
    let mut changes = vec![
      PartialChange {
        file_key: keys[1].clone(),
        basis_revision: Some(basis_revisions[1].clone()),
        target_revision: Some(target_beta.clone()),
      },
      PartialChange { file_key: keys[2].clone(), basis_revision: Some(basis_revisions[2].clone()), target_revision: None },
      PartialChange { file_key: new_key.clone(), basis_revision: None, target_revision: Some(new_revision.clone()) },
    ];
    changes.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));
    let mut complement = PartialComplement { changes, scans: 0 };
    let mut rechecker = PartialRechecker::default();
    rechecker
      .outcomes
      .insert(keys[0].clone(), IndexPartialRecheckOutcomeV1::Present { record_revision_hash: basis_revisions[0].clone(), matches: true });
    rechecker
      .outcomes
      .insert(keys[1].clone(), IndexPartialRecheckOutcomeV1::Present { record_revision_hash: target_beta.clone(), matches: true });
    rechecker.outcomes.insert(keys[2].clone(), IndexPartialRecheckOutcomeV1::Absent);
    rechecker
      .outcomes
      .insert(new_key.clone(), IndexPartialRecheckOutcomeV1::Present { record_revision_hash: new_revision.clone(), matches: true });

    let encoded_size =
      encode_canonical_value(&CanonicalConfigValueV1::Unsigned(5), aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE)
        .unwrap();
    let mut target_documents = vec![
      ExecutionDocument {
        scope_id: planned.scope_id.clone(),
        ordinal: 1,
        file_key: keys[0].clone(),
        revision: basis_revisions[0].clone(),
        path: paths[0].to_string(),
        fields: BTreeMap::from([("@size".to_string(), encoded_size.clone())]),
      },
      ExecutionDocument {
        scope_id: planned.scope_id.clone(),
        ordinal: 2,
        file_key: keys[1].clone(),
        revision: target_beta.clone(),
        path: paths[1].to_string(),
        fields: BTreeMap::from([("@size".to_string(), encoded_size.clone())]),
      },
      ExecutionDocument {
        scope_id: planned.scope_id.clone(),
        ordinal: 4,
        file_key: new_key.clone(),
        revision: new_revision.clone(),
        path: "/delta.json".to_string(),
        fields: BTreeMap::from([("@size".to_string(), encoded_size)]),
      },
    ];
    target_documents.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));
    let authoritative_memory = memory(32 * 1_024 * 1_024);
    let cancellation = CancellationToken::new();
    let mut authoritative_source = AuthoritativeExecutionSource {
      root: planned.root.clone(),
      publication_sequence: planned.plan.publication_sequence(),
      documents: target_documents,
    };
    let authoritative = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
      plan: &planned.plan,
      catalogs: &planned.catalogs,
      source: &mut authoritative_source,
      memory: &authoritative_memory,
      cancellation: &cancellation,
      limits: execution_limits(),
    })
    .unwrap();

    let mut artifacts = Source::default();
    for artifact in [&posting_page, &posting_directory, &scope_page, &scope_directory] {
      artifacts.insert(artifact);
    }
    let mut source = PartialArtifactSource {
      artifacts,
      posting_root: Some(root_authority(algorithm, planned.candidate.index_id(), 7, &posting_directory)),
      posting_roots: BTreeMap::new(),
      scope_root: Some(root_authority(algorithm, &planned.scope_id, 5, &scope_directory)),
      posting_complete: true,
      posting_error: None,
      posting_manifest_override: None,
      posting_manifest_overrides: BTreeMap::new(),
      scope_error: None,
      scope_source_override: None,
      posting_resolutions: 0,
      scope_resolutions: 0,
    };
    let memory = memory(32 * 1_024 * 1_024);
    let outcome = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
      plan: &planned.plan,
      predicate_index: 0,
      scope_id: &planned.scope_id,
      candidate_index: 0,
      source: &mut source,
      complement: &mut complement,
      rechecker: &mut rechecker,
      memory: &memory,
      cancellation: &cancellation,
      candidate_limits: limits(),
      acceleration_limits: partial_acceleration_limits(),
    })
    .unwrap();
    let IndexPartialAccelerationOutcomeV1::Exact(exact) = outcome else {
      panic!("planner-selected partial candidate did not produce exact complement proof")
    };
    assert_eq!(
      exact.matches().iter().map(|row| (row.file_key().to_vec(), row.record_revision_hash().to_vec())).collect::<Vec<_>>(),
      authoritative.matches().iter().map(|row| (row.file_key().to_vec(), row.record_revision().to_vec())).collect::<Vec<_>>()
    );
    assert_eq!(exact.proof().generation_manifest_hash(), generation.manifest_hash);
    assert_eq!(exact.proof().source_namespace_root(), generation.source_namespace_root);
    assert_eq!(exact.proof().target_namespace_root(), planned.root);
    assert_eq!(exact.proof().query_fingerprint(), planned.plan.query_fingerprint());
    assert_eq!(exact.proof().changed_document_count(), 3);
    assert_eq!(exact.overlap_deduplicated_count(), 2);
    assert_eq!(complement.scans, 1);
    assert_eq!(source.scope_resolutions, 1);
    drop(exact);
    drop(authoritative);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
    assert_eq!(authoritative_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

    source.scope_source_override = Some(vec![0xee; algorithm.hash_length()]);
    let outcome = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
      plan: &planned.plan,
      predicate_index: 0,
      scope_id: &planned.scope_id,
      candidate_index: 0,
      source: &mut source,
      complement: &mut complement,
      rechecker: &mut rechecker,
      memory: &memory,
      cancellation: &cancellation,
      candidate_limits: limits(),
      acceleration_limits: partial_acceleration_limits(),
    })
    .unwrap();
    let IndexPartialAccelerationOutcomeV1::AuthoritativeOnly { reason, .. } = outcome else {
      panic!("foreign source ScopeOrdinal receipt exposed a partial result")
    };
    assert_eq!(reason, IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
    source.scope_source_override = None;

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
      plan: &planned.plan,
      predicate_index: 0,
      scope_id: &planned.scope_id,
      candidate_index: 0,
      source: &mut source,
      complement: &mut complement,
      rechecker: &mut rechecker,
      memory: &memory,
      cancellation: &cancelled,
      candidate_limits: limits(),
      acceleration_limits: partial_acceleration_limits(),
    })
    .unwrap_err();
    assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::Cancelled);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

    let pressured = Arc::new(MemoryCoordinator::new(MemoryPolicy::new(512 * 1_024, 1_024 * 1_024, 1, 128 * 1_024).unwrap()));
    let outcome = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
      plan: &planned.plan,
      predicate_index: 0,
      scope_id: &planned.scope_id,
      candidate_index: 0,
      source: &mut source,
      complement: &mut complement,
      rechecker: &mut rechecker,
      memory: &pressured,
      cancellation: &cancellation,
      candidate_limits: limits(),
      acceleration_limits: partial_acceleration_limits(),
    })
    .unwrap();
    let IndexPartialAccelerationOutcomeV1::AuthoritativeOnly { reason, .. } = outcome else {
      panic!("artifact pressure exposed a partial result")
    };
    assert_eq!(reason, IndexPartialAccelerationFallbackReasonV1::CandidateResourceLimit);
    assert_eq!(pressured.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }
}

#[test]
fn malformed_partial_root_receipts_fall_back_but_internal_source_failures_remain_terminal() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_partial_candidate(algorithm);
  let mut source = PartialArtifactSource {
    artifacts: Source::default(),
    posting_root: None,
    posting_roots: BTreeMap::new(),
    scope_root: None,
    posting_complete: false,
    posting_error: None,
    posting_manifest_override: None,
    posting_manifest_overrides: BTreeMap::new(),
    scope_error: None,
    scope_source_override: None,
    posting_resolutions: 0,
    scope_resolutions: 0,
  };
  let mut complement = PartialComplement::default();
  let mut rechecker = PartialRechecker::default();
  let memory = memory(32 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let outcome = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    predicate_index: 0,
    scope_id: &planned.scope_id,
    candidate_index: 0,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap();
  let IndexPartialAccelerationOutcomeV1::AuthoritativeOnly { reason, .. } = outcome else {
    panic!("incomplete partial root receipt became exact")
  };
  assert_eq!(reason, IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt);
  assert_eq!(complement.scans, 0);
  assert_eq!(source.scope_resolutions, 0);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  source.posting_complete = true;
  source.posting_manifest_override = Some(vec![0xee; algorithm.hash_length()]);
  let outcome = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    predicate_index: 0,
    scope_id: &planned.scope_id,
    candidate_index: 0,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap();
  let IndexPartialAccelerationOutcomeV1::AuthoritativeOnly { reason, .. } = outcome else {
    panic!("substituted generation manifest became exact")
  };
  assert_eq!(reason, IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt);
  assert_eq!(complement.scans, 0);
  source.posting_manifest_override = None;

  source.posting_complete = true;
  source.scope_error = Some(IndexPartialSourceErrorV1::internal("unused_scope_root", "empty Posting result must skip scope resolution"));
  let outcome = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    predicate_index: 0,
    scope_id: &planned.scope_id,
    candidate_index: 0,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap();
  let IndexPartialAccelerationOutcomeV1::Exact(exact) = outcome else {
    panic!("proven-empty Posting generation did not remain exact")
  };
  assert!(exact.matches().is_empty());
  drop(exact);
  assert_eq!(source.scope_resolutions, 0);

  source.scope_error = None;
  source.posting_complete = true;
  source.posting_error = Some(IndexPartialSourceErrorV1::internal("partial_source_internal", "fixture authority failed"));
  let error = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    predicate_index: 0,
    scope_id: &planned.scope_id,
    candidate_index: 0,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::Internal);
  assert_eq!(error.code(), "partial_source_internal");
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  source.posting_error = None;
  let error = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    predicate_index: 0,
    scope_id: &planned.scope_id,
    candidate_index: 1,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::InvalidRequest);
  assert_eq!(error.code(), "query_partial_candidate_index");

  let complete = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let error = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
    plan: &complete.plan,
    predicate_index: 0,
    scope_id: &complete.scope_id,
    candidate_index: 0,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::InvalidRequest);
  assert_eq!(error.code(), "query_partial_candidate_driver");

  let too_many_candidates = IndexPartialAccelerationLimitsV1::new(128, 129, 128, 2 * 1_024 * 1_024).unwrap();
  let error = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    predicate_index: 0,
    scope_id: &planned.scope_id,
    candidate_index: 0,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: too_many_candidates,
  })
  .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::InvalidRequest);
  assert_eq!(error.code(), "query_partial_candidate_limits");
}

#[test]
fn one_candidate_partial_adapter_rejects_union_drivers_until_all_branches_are_composed() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_phonetic_union_query_with_source("Schmidt", Some((vec![0x55; algorithm.hash_length()], 40)));
  assert!(matches!(
    planned.plan.predicates()[0].scopes()[0].driver(),
    QueryPlanDriverV1::IndexUnion { coverage: CompiledQueryCoverageV1::PartialExact, .. }
  ));
  let mut source = PartialArtifactSource {
    artifacts: Source::default(),
    posting_root: None,
    posting_roots: BTreeMap::new(),
    scope_root: None,
    posting_complete: false,
    posting_error: None,
    posting_manifest_override: None,
    posting_manifest_overrides: BTreeMap::new(),
    scope_error: None,
    scope_source_override: None,
    posting_resolutions: 0,
    scope_resolutions: 0,
  };
  let mut complement = PartialComplement::default();
  let mut rechecker = PartialRechecker::default();
  let memory = memory(32 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();

  let error = execute_planned_partial_candidate_v1(QueryPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    predicate_index: 0,
    scope_id: &planned.scope_id,
    candidate_index: 0,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap_err();

  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::InvalidRequest);
  assert_eq!(error.code(), "query_partial_candidate_union");
  assert_eq!(complement.scans, 0);
  assert_eq!(source.scope_resolutions, 0);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn partial_candidate_adapter_delegates_exactness_without_live_or_duplicate_authority() {
  let adapter = include_str!("../../src/engine/v4/query_partial_candidate.rs");
  assert!(adapter.contains("execute_partial_index_acceleration_v1"));
  for forbidden in [
    "impl IndexChangedDocumentSourceV1",
    "impl IndexPartialCandidateRecheckerV1",
    "StorageEngine",
    "V4FirstAuthorityPublisher",
    "tokio::spawn",
    "std::thread::spawn",
    "server::",
    "axum::",
  ] {
    assert!(!adapter.contains(forbidden), "partial adapter gained forbidden authority: {forbidden}");
  }
}

#[test]
fn exact_scope_orchestrator_delegates_every_correctness_authority() {
  let source = include_str!("../../src/engine/v4/query_scope_execution.rs");
  for required in [
    "compose_boolean_candidate_plan_v1",
    "execute_authoritative_scope_query_v1",
    "execute_complete_candidate_scope_query_v1",
    "execute_composed_partial_candidates_v1",
  ] {
    assert!(source.contains(required), "scope orchestrator stopped delegating to {required}");
  }
  for forbidden in [
    "StorageEngine",
    "impl QueryAuthoritativeScopeSourceV1",
    "impl IndexChangedDocumentSourceV1",
    "impl IndexPartialCandidateRecheckerV1",
    "evaluate_operation",
    "tokio::spawn",
    "std::thread::spawn",
    "server::",
    "axum::",
  ] {
    assert!(!source.contains(forbidden), "scope orchestrator gained forbidden correctness or runtime authority: {forbidden}");
  }
}

fn composition_limits() -> QueryCandidateCompositionLimitsV1 {
  QueryCandidateCompositionLimitsV1::new(128, 256 * 1_024).unwrap()
}

#[test]
fn boolean_candidate_composition_obeys_and_or_not_superset_algebra() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_root = vec![0x55; algorithm.hash_length()];
  let partial_and_authoritative = QueryExpressionV1::And(vec![
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@filename".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())),
    }),
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@size".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::Unsigned(5)),
    }),
  ]);
  let planned = planned_query_with_generation_sources(partial_and_authoritative.clone(), Some((source_root.clone(), 40)), None);
  let memory = memory(16 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let composed = compose_boolean_candidate_plan_v1(&planned.plan, &planned.scope_id, &memory, &cancellation, composition_limits()).unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Partial);
  assert_eq!(composed.source_namespace_root(), Some(source_root.as_slice()));
  assert_eq!(composed.covered_through_publication_sequence(), Some(40));
  assert_eq!(composed.selections().len(), 1);
  drop(composed);

  let nested = QueryExpressionV1::And(vec![
    QueryExpressionV1::Or(match partial_and_authoritative.clone() {
      QueryExpressionV1::And(children) => children,
      _ => unreachable!(),
    }),
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@filename".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())),
    }),
  ]);
  let nested = planned_query_with_generation_sources(nested, Some((source_root.clone(), 40)), None);
  let composed = compose_boolean_candidate_plan_v1(&nested.plan, &nested.scope_id, &memory, &cancellation, composition_limits()).unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Partial);
  assert_eq!(composed.selections().len(), 1);
  drop(composed);

  let planned = planned_query_with_generation_sources(
    QueryExpressionV1::Or(match partial_and_authoritative {
      QueryExpressionV1::And(children) => children,
      _ => unreachable!(),
    }),
    Some((source_root, 40)),
    None,
  );
  let composed = compose_boolean_candidate_plan_v1(&planned.plan, &planned.scope_id, &memory, &cancellation, composition_limits()).unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Authoritative);
  assert!(composed.selections().is_empty());

  let partial = planned_partial_candidate(algorithm);
  let catalogs = partial.catalogs.clone();
  let expression = QueryExpressionV1::Not(Box::new(QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@filename".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())),
  })));
  let context = QueryPlanningContextV1::new(DATABASE_ID, PHYSICAL_INSTANCE_ID, algorithm, &partial.root, &[0x44; 32], 41).unwrap();
  let plan = plan_root_aware_query_v1(&QueryPlanningRequestV1 {
    context: &context,
    query_path: "/",
    expression: &expression,
    catalogs: &catalogs,
    sort_fields: &[],
    aggregate_fields: &[],
    group_fields: &[],
    result_limit: 16,
    limits: default_query_planning_limits_v1(),
    is_cancelled: &|| false,
  })
  .unwrap();
  let composed = compose_boolean_candidate_plan_v1(&plan, &partial.scope_id, &memory, &cancellation, composition_limits()).unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Authoritative);
  assert!(composed.selections().is_empty());
}

#[test]
fn partial_index_union_keeps_every_branch_and_incompatible_or_bases_fall_back() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_root = vec![0x55; algorithm.hash_length()];
  let union = planned_phonetic_union_query_with_source("Schmidt", Some((source_root.clone(), 40)));
  let query_memory = memory(16 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let composed =
    compose_boolean_candidate_plan_v1(&union.plan, &union.scope_id, &query_memory, &cancellation, composition_limits()).unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Partial);
  assert_eq!(composed.source_namespace_root(), Some(source_root.as_slice()));
  assert_eq!(composed.selections().len(), 2);
  assert_ne!(composed.selections()[0].candidate_index(), composed.selections()[1].candidate_index());

  let expression = QueryExpressionV1::Or(vec![
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@filename".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())),
    }),
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@size".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::Unsigned(5)),
    }),
  ]);
  let incompatible = planned_query_with_generation_sources(
    expression,
    Some((vec![0x55; algorithm.hash_length()], 40)),
    Some((vec![0x56; algorithm.hash_length()], 39)),
  );
  let composed =
    compose_boolean_candidate_plan_v1(&incompatible.plan, &incompatible.scope_id, &query_memory, &cancellation, composition_limits())
      .unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Authoritative);
  assert!(composed.selections().is_empty());
}

#[test]
fn candidate_composition_retains_exact_memory_and_both_hash_widths() {
  for algorithm in [HashAlgorithm::Blake3_256, HashAlgorithm::Sha512] {
    let planned = planned_partial_candidate(algorithm);
    let query_memory = memory(16 * 1_024 * 1_024);
    let composed =
      compose_boolean_candidate_plan_v1(&planned.plan, &planned.scope_id, &query_memory, &CancellationToken::new(), composition_limits())
        .unwrap();
    assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Partial);
    assert_eq!(composed.scope_id(), Some(planned.scope_id.as_slice()));
    assert_eq!(composed.source_namespace_root().unwrap().len(), algorithm.hash_length());
    assert!(composed.retained_bytes() > 0);
    assert_eq!(query_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, composed.retained_bytes());
    drop(composed);
    assert_eq!(query_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }

  let complete =
    planned_candidate(HashAlgorithm::Blake3_256, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let query_memory = memory(16 * 1_024 * 1_024);
  let composed =
    compose_boolean_candidate_plan_v1(&complete.plan, &complete.scope_id, &query_memory, &CancellationToken::new(), composition_limits())
      .unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Complete);
  assert_eq!(composed.retained_bytes(), 0);
  assert_eq!(query_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn same_basis_or_unions_partial_selections_and_enforces_all_bounds() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_root = vec![0x55; algorithm.hash_length()];
  let expression = QueryExpressionV1::Or(vec![
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@filename".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())),
    }),
    QueryExpressionV1::Field(QueryPredicateV1 {
      field_name: "@size".to_string(),
      operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::Unsigned(5)),
    }),
  ]);
  let planned = planned_query_with_generation_sources(expression, Some((source_root.clone(), 40)), Some((source_root.clone(), 40)));
  let query_memory = memory(16 * 1_024 * 1_024);
  let composed =
    compose_boolean_candidate_plan_v1(&planned.plan, &planned.scope_id, &query_memory, &CancellationToken::new(), composition_limits())
      .unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Partial);
  assert_eq!(composed.source_namespace_root(), Some(source_root.as_slice()));
  assert_eq!(composed.selections().len(), 2);
  drop(composed);

  let duplicate_predicate = QueryExpressionV1::Field(QueryPredicateV1 {
    field_name: "@filename".to_string(),
    operation: QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())),
  });
  let duplicate = planned_query_with_generation_sources(
    QueryExpressionV1::Or(vec![duplicate_predicate.clone(), duplicate_predicate]),
    Some((source_root.clone(), 40)),
    None,
  );
  let composed =
    compose_boolean_candidate_plan_v1(&duplicate.plan, &duplicate.scope_id, &query_memory, &CancellationToken::new(), composition_limits())
      .unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Partial);
  assert_eq!(composed.selections().len(), 2, "distinct predicate bindings must remain independently recheckable");
  assert_ne!(composed.selections()[0].predicate_index(), composed.selections()[1].predicate_index());
  drop(composed);

  let error = compose_boolean_candidate_plan_v1(
    &planned.plan,
    &planned.scope_id,
    &query_memory,
    &CancellationToken::new(),
    QueryCandidateCompositionLimitsV1::new(1, 256 * 1_024).unwrap(),
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryCandidateCompositionErrorClassV1::ResourceLimit);
  assert_eq!(error.code(), "query_candidate_composition_selection_limit");
  assert_eq!(query_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let error = compose_boolean_candidate_plan_v1(
    &planned.plan,
    &planned.scope_id,
    &query_memory,
    &CancellationToken::new(),
    QueryCandidateCompositionLimitsV1::new(128, 1).unwrap(),
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryCandidateCompositionErrorClassV1::InvalidRequest);
  assert_eq!(query_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let error =
    compose_boolean_candidate_plan_v1(&planned.plan, &[0; 32], &query_memory, &CancellationToken::new(), composition_limits()).unwrap_err();
  assert_eq!(error.class(), QueryCandidateCompositionErrorClassV1::InvalidRequest);
}

#[test]
fn candidate_composition_is_bounded_cancelled_and_storage_neutral() {
  let planned = planned_partial_candidate(HashAlgorithm::Blake3_256);
  let query_memory = memory(16 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let error =
    compose_boolean_candidate_plan_v1(&planned.plan, &planned.scope_id, &query_memory, &cancellation, composition_limits()).unwrap_err();
  assert_eq!(error.class(), QueryCandidateCompositionErrorClassV1::Cancelled);
  assert_eq!(query_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let tiny_memory = memory(1);
  let error = compose_boolean_candidate_plan_v1(
    &planned.plan,
    &planned.scope_id,
    &tiny_memory,
    &CancellationToken::new(),
    QueryCandidateCompositionLimitsV1::new(1_000_000, 64 * 1_024 * 1_024).unwrap(),
  )
  .unwrap_err();
  assert_eq!(error.class(), QueryCandidateCompositionErrorClassV1::ResourceLimit);
  assert_eq!(tiny_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let source = include_str!("../../src/engine/v4/query_candidate_composition.rs");
  for forbidden in ["StorageEngine", "FieldNvt", "Nvt", "server::", "axum::", "tokio::spawn", "std::thread::spawn"] {
    assert!(!source.contains(forbidden), "candidate composition gained forbidden authority: {forbidden}");
  }
}

#[test]
fn composed_partial_candidates_share_one_exact_complement_and_validate_every_branch() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_root = vec![0x55; algorithm.hash_length()];
  let expression = QueryExpressionV1::Or(vec![
    predicate("@filename", QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string()))),
    predicate("@size", QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::Unsigned(5))),
  ]);
  let planned = planned_query_with_generation_sources(expression, Some((source_root.clone(), 40)), Some((source_root.clone(), 40)));
  let composition_memory = memory(16 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let composed =
    compose_boolean_candidate_plan_v1(&planned.plan, &planned.scope_id, &composition_memory, &cancellation, composition_limits()).unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Partial);
  assert_eq!(composed.selections().len(), 2);

  let filename_candidate = &planned.plan.predicates()[0].scopes()[0].candidates()[0];
  let size_candidate = &planned.plan.predicates()[1].scopes()[0].candidates()[0];
  let candidate_rows = [(filename_candidate, 1u64), (size_candidate, 2u64)];
  let paths = ["/alpha.json", "/beta.json"];
  let keys = paths.map(|path| digest_parts(algorithm, &[b"file:", path.as_bytes()]));
  let revisions = [1u64, 2].map(|ordinal| digest_parts(algorithm, &[b"basis:", &ordinal.to_le_bytes()]));
  let scope_rows =
    [(1u64, keys[0].as_slice(), revisions[0].as_slice(), paths[0]), (2u64, keys[1].as_slice(), revisions[1].as_slice(), paths[1])];
  let mut source = partial_artifact_source(algorithm, &planned.scope_id, &candidate_rows, &scope_rows);

  let encoded_alpha = encode_canonical_value(
    &CanonicalConfigValueV1::String("alpha.json".to_string()),
    aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
  )
  .unwrap();
  let encoded_beta = encode_canonical_value(
    &CanonicalConfigValueV1::String("beta.json".to_string()),
    aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
  )
  .unwrap();
  let encoded_one =
    encode_canonical_value(&CanonicalConfigValueV1::Unsigned(1), aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE)
      .unwrap();
  let encoded_five =
    encode_canonical_value(&CanonicalConfigValueV1::Unsigned(5), aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE)
      .unwrap();
  let mut documents = vec![
    ExecutionDocument {
      scope_id: planned.scope_id.clone(),
      ordinal: 1,
      file_key: keys[0].clone(),
      revision: revisions[0].clone(),
      path: paths[0].to_string(),
      fields: BTreeMap::from([("@filename".to_string(), encoded_alpha), ("@size".to_string(), encoded_one)]),
    },
    ExecutionDocument {
      scope_id: planned.scope_id.clone(),
      ordinal: 2,
      file_key: keys[1].clone(),
      revision: revisions[1].clone(),
      path: paths[1].to_string(),
      fields: BTreeMap::from([("@filename".to_string(), encoded_beta), ("@size".to_string(), encoded_five)]),
    },
  ];
  documents.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));
  let authoritative_memory = memory(16 * 1_024 * 1_024);
  let mut authoritative_source =
    AuthoritativeExecutionSource { root: planned.root.clone(), publication_sequence: planned.plan.publication_sequence(), documents };
  let authoritative = execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    source: &mut authoritative_source,
    memory: &authoritative_memory,
    cancellation: &cancellation,
    limits: execution_limits(),
  })
  .unwrap();

  let mut complement = PartialComplement::default();
  let mut rechecker = PartialRechecker::default();
  for index in 0..2 {
    rechecker
      .outcomes
      .insert(keys[index].clone(), IndexPartialRecheckOutcomeV1::Present { record_revision_hash: revisions[index].clone(), matches: true });
  }
  let execution_memory = memory(32 * 1_024 * 1_024);
  let outcome = execute_composed_partial_candidates_v1(QueryComposedPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    candidate_plan: &composed,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &execution_memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap();
  let IndexPartialAccelerationOutcomeV1::Exact(exact) = outcome else {
    panic!("composed partial candidates did not produce exact target-root truth")
  };
  assert_eq!(
    exact.matches().iter().map(|row| (row.file_key().to_vec(), row.record_revision_hash().to_vec())).collect::<Vec<_>>(),
    authoritative.matches().iter().map(|row| (row.file_key().to_vec(), row.record_revision().to_vec())).collect::<Vec<_>>()
  );
  assert_eq!(exact.observed_candidate_count(), 2);
  assert_eq!(exact.proof().source_namespace_root(), source_root);
  assert_eq!(exact.proof().query_fingerprint(), planned.plan.query_fingerprint());
  assert_ne!(exact.proof().generation_manifest_hash(), filename_candidate.selected_generation().unwrap().manifest_hash);
  assert_ne!(exact.proof().generation_manifest_hash(), size_candidate.selected_generation().unwrap().manifest_hash);
  assert_eq!(source.posting_resolutions, 2);
  assert_eq!(source.scope_resolutions, 2);
  assert_eq!(complement.scans, 1, "a candidate set must use one exact immutable-root complement");
  drop(exact);
  drop(authoritative);
  assert_eq!(execution_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  assert_eq!(authoritative_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  source.posting_manifest_overrides.insert(size_candidate.index_id().to_vec(), vec![0xee; algorithm.hash_length()]);
  let outcome = execute_composed_partial_candidates_v1(QueryComposedPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    candidate_plan: &composed,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &execution_memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap();
  let IndexPartialAccelerationOutcomeV1::AuthoritativeOnly { reason, .. } = outcome else {
    panic!("a substituted branch manifest exposed a composed partial result")
  };
  assert_eq!(reason, IndexPartialAccelerationFallbackReasonV1::CandidateCorrupt);
  assert_eq!(complement.scans, 1);
  source.posting_manifest_overrides.clear();

  let aggregate_limited = IndexPartialAccelerationLimitsV1::new(128, 1, 128, 2 * 1_024 * 1_024).unwrap();
  let outcome = execute_composed_partial_candidates_v1(QueryComposedPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    candidate_plan: &composed,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &execution_memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: aggregate_limited,
  })
  .unwrap();
  let IndexPartialAccelerationOutcomeV1::AuthoritativeOnly { reason, .. } = outcome else {
    panic!("an over-limit composed candidate stream exposed truncated matches")
  };
  assert_eq!(reason, IndexPartialAccelerationFallbackReasonV1::CandidateResourceLimit);
  assert_eq!(complement.scans, 1);
  assert_eq!(execution_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let foreign = planned_partial_size_candidate(algorithm);
  let error = execute_composed_partial_candidates_v1(QueryComposedPartialCandidateExecutionRequestV1 {
    plan: &foreign.plan,
    candidate_plan: &composed,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &execution_memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap_err();
  assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::InvalidRequest);
  assert_eq!(error.code(), "query_composed_partial_candidate_fingerprint");
  assert_eq!(complement.scans, 1);
}

#[test]
fn composed_partial_index_union_streams_every_planner_selected_branch() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_root = vec![0x55; algorithm.hash_length()];
  let planned = planned_phonetic_union_query_with_source("Schmidt", Some((source_root, 40)));
  let composition_memory = memory(16 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let composed =
    compose_boolean_candidate_plan_v1(&planned.plan, &planned.scope_id, &composition_memory, &cancellation, composition_limits()).unwrap();
  assert_eq!(composed.kind(), QueryBooleanCandidatePlanKindV1::Partial);
  assert_eq!(composed.selections().len(), 2);

  let scope = &planned.plan.predicates()[0].scopes()[0];
  let first = &scope.candidates()[composed.selections()[0].candidate_index()];
  let second = &scope.candidates()[composed.selections()[1].candidate_index()];
  let paths = ["/schmidt-a.txt", "/schmidt-b.txt"];
  let keys = paths.map(|path| digest_parts(algorithm, &[b"file:", path.as_bytes()]));
  let revisions = [1u64, 2].map(|ordinal| digest_parts(algorithm, &[b"basis:", &ordinal.to_le_bytes()]));
  let candidate_rows = [(first, 1u64), (second, 2u64)];
  let scope_rows =
    [(1u64, keys[0].as_slice(), revisions[0].as_slice(), paths[0]), (2u64, keys[1].as_slice(), revisions[1].as_slice(), paths[1])];
  let mut source = partial_artifact_source(algorithm, &planned.scope_id, &candidate_rows, &scope_rows);
  let mut complement = PartialComplement::default();
  let mut rechecker = PartialRechecker::default();
  for index in 0..2 {
    rechecker
      .outcomes
      .insert(keys[index].clone(), IndexPartialRecheckOutcomeV1::Present { record_revision_hash: revisions[index].clone(), matches: true });
  }
  let execution_memory = memory(32 * 1_024 * 1_024);
  let outcome = execute_composed_partial_candidates_v1(QueryComposedPartialCandidateExecutionRequestV1 {
    plan: &planned.plan,
    candidate_plan: &composed,
    source: &mut source,
    complement: &mut complement,
    rechecker: &mut rechecker,
    memory: &execution_memory,
    cancellation: &cancellation,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
  })
  .unwrap();
  let IndexPartialAccelerationOutcomeV1::Exact(exact) = outcome else {
    panic!("planner-selected partial IndexUnion did not produce exact target-root truth")
  };
  let mut expected = vec![(keys[0].clone(), revisions[0].clone()), (keys[1].clone(), revisions[1].clone())];
  expected.sort_unstable();
  assert_eq!(
    exact.matches().iter().map(|row| (row.file_key().to_vec(), row.record_revision_hash().to_vec())).collect::<Vec<_>>(),
    expected
  );
  assert_eq!(exact.observed_candidate_count(), 2);
  assert_eq!(source.posting_resolutions, 2);
  assert_eq!(source.scope_resolutions, 2);
  assert_eq!(complement.scans, 1);
  drop(exact);
  assert_eq!(execution_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn disposable_partial_refusal_reruns_authoritative_scope_truth_without_exposing_candidates() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_partial_candidate(algorithm);
  let paths = ["/alpha.json", "/beta.json"];
  let mut documents = paths
    .into_iter()
    .enumerate()
    .map(|(index, path)| {
      let ordinal = u64::try_from(index).unwrap() + 1;
      ExecutionDocument {
        scope_id: planned.scope_id.clone(),
        ordinal,
        file_key: digest_parts(algorithm, &[b"file:", path.as_bytes()]),
        revision: digest_parts(algorithm, &[b"revision:", &ordinal.to_le_bytes()]),
        path: path.to_string(),
        fields: BTreeMap::from([(
          "@filename".to_string(),
          encode_canonical_value(
            &CanonicalConfigValueV1::String(path.trim_start_matches('/').to_string()),
            aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
          )
          .unwrap(),
        )]),
      }
    })
    .collect::<Vec<_>>();
  documents.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));
  let expected = documents
    .iter()
    .filter(|document| document.path == "/alpha.json")
    .map(|document| (document.file_key.clone(), document.revision.clone()))
    .collect::<Vec<_>>();
  let mut authoritative_source =
    AuthoritativeExecutionSource { root: planned.root.clone(), publication_sequence: planned.plan.publication_sequence(), documents };
  let mut partial_source = PartialArtifactSource {
    artifacts: Source::default(),
    posting_root: None,
    posting_roots: BTreeMap::new(),
    scope_root: None,
    posting_complete: false,
    posting_error: Some(IndexPartialSourceErrorV1::unavailable("fixture_candidate_missing", "derived Posting was evicted")),
    posting_manifest_override: None,
    posting_manifest_overrides: BTreeMap::new(),
    scope_error: None,
    scope_source_override: None,
    posting_resolutions: 0,
    scope_resolutions: 0,
  };
  let mut complement = PartialComplement::default();
  let mut rechecker = PartialRechecker::default();
  let memory = memory(32 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();

  let execution = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    scope_id: &planned.scope_id,
    authoritative_source: &mut authoritative_source,
    complete_source: None,
    partial_source: Some(&mut partial_source),
    complement: Some(&mut complement),
    rechecker: Some(&mut rechecker),
    memory: &memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap();

  assert_eq!(execution.path(), QueryExactScopeExecutionPathV1::PartialFallback);
  assert_eq!(
    execution.identities().map(|identity| (identity.file_key().to_vec(), identity.record_revision().to_vec())).collect::<Vec<_>>(),
    expected
  );
  match execution.fallback_diagnostic().unwrap() {
    QueryExactScopeFallbackDiagnosticV1::Partial { reason, diagnostic } => {
      assert_eq!(*reason, IndexPartialAccelerationFallbackReasonV1::CandidateUnavailable);
      assert_eq!(diagnostic.code, "fixture_candidate_missing");
    }
    diagnostic => panic!("unexpected fallback diagnostic: {diagnostic:?}"),
  }
  assert_eq!(partial_source.posting_resolutions, 1);
  assert_eq!(complement.scans, 0, "candidate refusal must not enter exact complement work");
  drop(execution);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn exact_scope_dispatches_authoritative_complete_and_exact_partial_paths() {
  let algorithm = HashAlgorithm::Blake3_256;
  let paths = ["/alpha.json", "/beta.json"];
  let cancellation = CancellationToken::new();

  let authoritative_plan = planned_query_with_generation_sources(
    predicate("@filename", QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string()))),
    None,
    None,
  );
  let authoritative_documents = filename_execution_documents(algorithm, &authoritative_plan.scope_id, &paths);
  let expected = authoritative_documents
    .iter()
    .filter(|document| document.path == "/alpha.json")
    .map(|document| (document.file_key.clone(), document.revision.clone()))
    .collect::<Vec<_>>();
  let mut authoritative_source = AuthoritativeExecutionSource {
    root: authoritative_plan.root.clone(),
    publication_sequence: authoritative_plan.plan.publication_sequence(),
    documents: authoritative_documents,
  };
  let authoritative_memory = memory(32 * 1_024 * 1_024);
  let authoritative = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &authoritative_plan.plan,
    catalogs: &authoritative_plan.catalogs,
    scope_id: &authoritative_plan.scope_id,
    authoritative_source: &mut authoritative_source,
    complete_source: None,
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &authoritative_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap();
  assert_eq!(authoritative.path(), QueryExactScopeExecutionPathV1::Authoritative);
  assert_eq!(authoritative.scope_id(), authoritative_plan.scope_id);
  assert_eq!(authoritative.selected_namespace_root(), authoritative_plan.root);
  assert!(authoritative.retained_bytes() > 0);
  assert_eq!(
    authoritative.identities().map(|identity| (identity.file_key().to_vec(), identity.record_revision().to_vec())).collect::<Vec<_>>(),
    expected
  );
  drop(authoritative);
  assert_eq!(authoritative_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let complete_plan = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let complete_documents = filename_execution_documents(algorithm, &complete_plan.scope_id, &paths);
  let mut complete_source = single_complete_candidate_source(algorithm, &complete_plan, complete_documents, &[1]);
  let mut unused_authority = PanicAuthoritativeSource;
  let complete_memory = memory(32 * 1_024 * 1_024);
  let complete = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &complete_plan.plan,
    catalogs: &complete_plan.catalogs,
    scope_id: &complete_plan.scope_id,
    authoritative_source: &mut unused_authority,
    complete_source: Some(&mut complete_source),
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &complete_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap();
  assert_eq!(complete.path(), QueryExactScopeExecutionPathV1::Complete);
  assert_eq!(complete.scope_id(), complete_plan.scope_id);
  assert_eq!(complete.selected_namespace_root(), complete_plan.root);
  assert!(complete.retained_bytes() > 0);
  assert_eq!(
    complete.identities().map(|identity| (identity.file_key().to_vec(), identity.record_revision().to_vec())).collect::<Vec<_>>(),
    expected
  );
  drop(complete);
  assert_eq!(complete_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let partial_plan = planned_partial_candidate(algorithm);
  let partial_documents = filename_execution_documents(algorithm, &partial_plan.scope_id, &paths);
  let alpha = partial_documents.iter().find(|document| document.ordinal == 1).unwrap();
  let candidate_rows = [(&partial_plan.candidate, 1u64)];
  let mut by_ordinal = partial_documents.iter().collect::<Vec<_>>();
  by_ordinal.sort_unstable_by_key(|document| document.ordinal);
  let scope_rows = by_ordinal
    .iter()
    .map(|document| (document.ordinal, document.file_key.as_slice(), document.revision.as_slice(), document.path.as_str()))
    .collect::<Vec<_>>();
  let mut partial_source = partial_artifact_source(algorithm, &partial_plan.scope_id, &candidate_rows, &scope_rows);
  let mut complement = PartialComplement::default();
  let mut rechecker = PartialRechecker::default();
  rechecker
    .outcomes
    .insert(alpha.file_key.clone(), IndexPartialRecheckOutcomeV1::Present { record_revision_hash: alpha.revision.clone(), matches: true });
  let mut unused_authority = PanicAuthoritativeSource;
  let partial_memory = memory(32 * 1_024 * 1_024);
  let partial = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &partial_plan.plan,
    catalogs: &partial_plan.catalogs,
    scope_id: &partial_plan.scope_id,
    authoritative_source: &mut unused_authority,
    complete_source: None,
    partial_source: Some(&mut partial_source),
    complement: Some(&mut complement),
    rechecker: Some(&mut rechecker),
    memory: &partial_memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap();
  assert_eq!(partial.path(), QueryExactScopeExecutionPathV1::Partial);
  assert_eq!(partial.scope_id(), partial_plan.scope_id);
  assert_eq!(partial.selected_namespace_root(), partial_plan.root);
  assert!(partial.retained_bytes() > 0);
  assert_eq!(
    partial.identities().map(|identity| (identity.file_key().to_vec(), identity.record_revision().to_vec())).collect::<Vec<_>>(),
    expected
  );
  drop(partial);
  assert_eq!(partial_memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn exact_scope_retains_sha512_scope_and_root_identities_without_heap_duplication() {
  let algorithm = HashAlgorithm::Sha512;
  let planned = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let documents = filename_execution_documents(algorithm, &planned.scope_id, &["/alpha.json", "/beta.json"]);
  let mut source = single_complete_candidate_source(algorithm, &planned, documents, &[1]);
  let mut unused_authority = PanicAuthoritativeSource;
  let memory = memory(32 * 1_024 * 1_024);
  let execution = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    scope_id: &planned.scope_id,
    authoritative_source: &mut unused_authority,
    complete_source: Some(&mut source),
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &memory,
    cancellation: &CancellationToken::new(),
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap();
  assert_eq!(execution.path(), QueryExactScopeExecutionPathV1::Complete);
  assert_eq!(execution.scope_id(), planned.scope_id);
  assert_eq!(execution.scope_id().len(), 64);
  assert_eq!(execution.selected_namespace_root(), planned.root);
  assert_eq!(execution.selected_namespace_root().len(), 64);
  assert_eq!(execution.match_count(), 1);
  assert!(execution.retained_bytes() > 0);
  drop(execution);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn complete_disposable_failures_retry_truth_but_internal_failures_remain_terminal() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let documents = filename_execution_documents(algorithm, &planned.scope_id, &["/alpha.json", "/beta.json"]);
  let expected = documents
    .iter()
    .filter(|document| document.path == "/alpha.json")
    .map(|document| (document.file_key.clone(), document.revision.clone()))
    .collect::<Vec<_>>();
  let mut complete_source = single_complete_candidate_source(algorithm, &planned, documents.clone(), &[1]);
  complete_source.posting_receipt_complete = false;
  let mut authoritative_source =
    AuthoritativeExecutionSource { root: planned.root.clone(), publication_sequence: planned.plan.publication_sequence(), documents };
  let memory = memory(32 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let fallback = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    scope_id: &planned.scope_id,
    authoritative_source: &mut authoritative_source,
    complete_source: Some(&mut complete_source),
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap();
  assert_eq!(fallback.path(), QueryExactScopeExecutionPathV1::CompleteFallback);
  assert_eq!(
    fallback.identities().map(|identity| (identity.file_key().to_vec(), identity.record_revision().to_vec())).collect::<Vec<_>>(),
    expected
  );
  match fallback.fallback_diagnostic().unwrap() {
    QueryExactScopeFallbackDiagnosticV1::Complete(error) => {
      assert_eq!(error.class(), QueryExecutionErrorClassV1::CorruptSource);
      assert_eq!(error.code(), "query_candidate_posting_root_receipt");
    }
    diagnostic => panic!("unexpected fallback diagnostic: {diagnostic:?}"),
  }
  drop(fallback);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  for (source_class, expected_class) in [
    (QueryExecutionSourceErrorClassV1::ResourceLimit, QueryExecutionErrorClassV1::ResourceLimit),
    (QueryExecutionSourceErrorClassV1::Unavailable, QueryExecutionErrorClassV1::HistoricalViewUnavailable),
  ] {
    let documents = filename_execution_documents(algorithm, &planned.scope_id, &["/alpha.json", "/beta.json"]);
    let mut complete_source = FailingCompleteSource { class: source_class, code: "fixture_complete_disposable" };
    let mut authoritative_source =
      AuthoritativeExecutionSource { root: planned.root.clone(), publication_sequence: planned.plan.publication_sequence(), documents };
    let fallback = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
      plan: &planned.plan,
      catalogs: &planned.catalogs,
      scope_id: &planned.scope_id,
      authoritative_source: &mut authoritative_source,
      complete_source: Some(&mut complete_source),
      partial_source: None,
      complement: None,
      rechecker: None,
      memory: &memory,
      cancellation: &cancellation,
      execution_limits: execution_limits(),
      candidate_limits: limits(),
      acceleration_limits: partial_acceleration_limits(),
      composition_limits: composition_limits(),
    })
    .unwrap();
    assert_eq!(fallback.path(), QueryExactScopeExecutionPathV1::CompleteFallback);
    match fallback.fallback_diagnostic().unwrap() {
      QueryExactScopeFallbackDiagnosticV1::Complete(error) => {
        assert_eq!(error.class(), expected_class);
        assert_eq!(error.code(), "fixture_complete_disposable");
      }
      diagnostic => panic!("unexpected complete fallback diagnostic: {diagnostic:?}"),
    }
    drop(fallback);
    assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  }

  let mut panic_authority = PanicAuthoritativeSource;
  let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    scope_id: &planned.scope_id,
    authoritative_source: &mut panic_authority,
    complete_source: None,
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap_err();
  match error {
    QueryExactScopeExecutionErrorV1::InvalidRequest { code, .. } => assert_eq!(code, "query_scope_complete_source"),
    error => panic!("unexpected missing complete-source error: {error:?}"),
  }

  let mut internal_source = FailingCompleteSource { class: QueryExecutionSourceErrorClassV1::Internal, code: "fixture_complete_internal" };
  let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    scope_id: &planned.scope_id,
    authoritative_source: &mut panic_authority,
    complete_source: Some(&mut internal_source),
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap_err();
  match error {
    QueryExactScopeExecutionErrorV1::Execution(error) => {
      assert_eq!(error.class(), QueryExecutionErrorClassV1::Internal);
      assert_eq!(error.code(), "fixture_complete_internal");
    }
    error => panic!("unexpected terminal complete-source error: {error:?}"),
  }
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn complete_collected_sink_failure_does_not_retry_or_replay_through_authoritative_fallback() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_candidate(algorithm, QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())));
  let documents = filename_execution_documents(algorithm, &planned.scope_id, &["/alpha.json", "/beta.json"]);
  let mut complete_source = single_complete_candidate_source(algorithm, &planned, documents, &[1, 2]);
  let mut panic_authority = PanicAuthoritativeSource;
  let memory = memory(32 * 1_024 * 1_024);
  let base_retained_bytes = (4 * 1_024 + planned.root.len() + size_of::<QueryExecutionMatchV1>()) as u64;
  let retained_failure_limits = QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(128, 1_024, 1, 1_000_000).unwrap(),
    QueryExecutionByteLimitsV1::new(1_024 * 1_024, base_retained_bytes, 4 * 1_024 * 1_024).unwrap(),
  );

  let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    scope_id: &planned.scope_id,
    authoritative_source: &mut panic_authority,
    complete_source: Some(&mut complete_source),
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &memory,
    cancellation: &CancellationToken::new(),
    execution_limits: retained_failure_limits,
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap_err();

  match error {
    QueryExactScopeExecutionErrorV1::Execution(error) => {
      assert_eq!(error.class(), QueryExecutionErrorClassV1::ResourceLimit);
      assert_eq!(error.origin(), QueryExecutionErrorOriginV1::Sink);
      assert_eq!(error.code(), "query_execution_retained_bytes");
    }
    error => panic!("complete sink failure was replayed or misclassified: {error:?}"),
  }
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn scope_fallback_preserves_both_failures_and_never_retries_terminal_partial_errors() {
  let algorithm = HashAlgorithm::Blake3_256;
  let planned = planned_partial_candidate(algorithm);
  let memory = memory(32 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let mut partial_source = PartialArtifactSource {
    artifacts: Source::default(),
    posting_root: None,
    posting_roots: BTreeMap::new(),
    scope_root: None,
    posting_complete: false,
    posting_error: Some(IndexPartialSourceErrorV1::unavailable("fixture_candidate_missing", "derived Posting was evicted")),
    posting_manifest_override: None,
    posting_manifest_overrides: BTreeMap::new(),
    scope_error: None,
    scope_source_override: None,
    posting_resolutions: 0,
    scope_resolutions: 0,
  };
  let mut complement = PartialComplement::default();
  let mut rechecker = PartialRechecker::default();
  let mut corrupt_authority = CorruptAuthoritativeSource;
  let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    scope_id: &planned.scope_id,
    authoritative_source: &mut corrupt_authority,
    complete_source: None,
    partial_source: Some(&mut partial_source),
    complement: Some(&mut complement),
    rechecker: Some(&mut rechecker),
    memory: &memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap_err();
  assert!(std::error::Error::source(&error).unwrap().to_string().contains("fixture_authoritative_corrupt"));
  match error {
    QueryExactScopeExecutionErrorV1::AuthoritativeFallbackFailed { diagnostic, authoritative } => {
      match diagnostic {
        QueryExactScopeFallbackDiagnosticV1::Partial { reason, diagnostic } => {
          assert_eq!(reason, IndexPartialAccelerationFallbackReasonV1::CandidateUnavailable);
          assert_eq!(diagnostic.code, "fixture_candidate_missing");
        }
        diagnostic => panic!("unexpected retained accelerator diagnostic: {diagnostic:?}"),
      }
      assert_eq!(authoritative.class(), QueryExecutionErrorClassV1::CorruptSource);
      assert_eq!(authoritative.code(), "fixture_authoritative_corrupt");
    }
    error => panic!("unexpected fallback failure: {error:?}"),
  }
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  partial_source.posting_error = Some(IndexPartialSourceErrorV1::internal("fixture_partial_internal", "partial source invariant failed"));
  let mut panic_authority = PanicAuthoritativeSource;
  let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    scope_id: &planned.scope_id,
    authoritative_source: &mut panic_authority,
    complete_source: None,
    partial_source: Some(&mut partial_source),
    complement: Some(&mut complement),
    rechecker: Some(&mut rechecker),
    memory: &memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap_err();
  match error {
    QueryExactScopeExecutionErrorV1::Partial(error) => {
      assert_eq!(error.class(), IndexPartialAccelerationErrorClassV1::Internal);
      assert_eq!(error.code(), "fixture_partial_internal");
    }
    error => panic!("unexpected terminal partial-source error: {error:?}"),
  }
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[test]
fn scope_orchestrator_falls_back_on_composition_pressure_but_not_cancellation_or_missing_sources() {
  let algorithm = HashAlgorithm::Blake3_256;
  let source_root = vec![0x55; algorithm.hash_length()];
  let expression = QueryExpressionV1::Or(vec![
    predicate("@filename", QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string()))),
    predicate("@size", QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::Unsigned(5))),
  ]);
  let planned = planned_query_with_generation_sources(expression, Some((source_root.clone(), 40)), Some((source_root, 40)));
  let encoded_alpha = encode_canonical_value(
    &CanonicalConfigValueV1::String("alpha.json".to_string()),
    aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
  )
  .unwrap();
  let encoded_beta = encode_canonical_value(
    &CanonicalConfigValueV1::String("beta.json".to_string()),
    aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
  )
  .unwrap();
  let encoded_one =
    encode_canonical_value(&CanonicalConfigValueV1::Unsigned(1), aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE)
      .unwrap();
  let encoded_five =
    encode_canonical_value(&CanonicalConfigValueV1::Unsigned(5), aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE)
      .unwrap();
  let mut documents = vec![
    ExecutionDocument {
      scope_id: planned.scope_id.clone(),
      ordinal: 1,
      file_key: digest_parts(algorithm, &[b"file:", b"/alpha.json"]),
      revision: digest_parts(algorithm, &[b"revision:", &1u64.to_le_bytes()]),
      path: "/alpha.json".to_string(),
      fields: BTreeMap::from([("@filename".to_string(), encoded_alpha), ("@size".to_string(), encoded_one)]),
    },
    ExecutionDocument {
      scope_id: planned.scope_id.clone(),
      ordinal: 2,
      file_key: digest_parts(algorithm, &[b"file:", b"/beta.json"]),
      revision: digest_parts(algorithm, &[b"revision:", &2u64.to_le_bytes()]),
      path: "/beta.json".to_string(),
      fields: BTreeMap::from([("@filename".to_string(), encoded_beta), ("@size".to_string(), encoded_five)]),
    },
  ];
  documents.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));
  let expected = documents.iter().map(|document| (document.file_key.clone(), document.revision.clone())).collect::<Vec<_>>();
  let mut authoritative_source =
    AuthoritativeExecutionSource { root: planned.root.clone(), publication_sequence: planned.plan.publication_sequence(), documents };
  let memory = memory(32 * 1_024 * 1_024);
  let cancellation = CancellationToken::new();
  let fallback = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    scope_id: &planned.scope_id,
    authoritative_source: &mut authoritative_source,
    complete_source: None,
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &memory,
    cancellation: &cancellation,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: QueryCandidateCompositionLimitsV1::new(1, 256 * 1_024).unwrap(),
  })
  .unwrap();
  assert_eq!(fallback.path(), QueryExactScopeExecutionPathV1::CompositionFallback);
  assert_eq!(fallback.scope_id(), planned.scope_id);
  assert_eq!(fallback.selected_namespace_root(), planned.root);
  assert!(fallback.retained_bytes() > 0);
  assert_eq!(
    fallback.identities().map(|identity| (identity.file_key().to_vec(), identity.record_revision().to_vec())).collect::<Vec<_>>(),
    expected
  );
  match fallback.fallback_diagnostic().unwrap() {
    QueryExactScopeFallbackDiagnosticV1::Composition(error) => {
      assert_eq!(error.class(), QueryCandidateCompositionErrorClassV1::ResourceLimit);
    }
    diagnostic => panic!("unexpected composition fallback diagnostic: {diagnostic:?}"),
  }
  drop(fallback);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);

  let partial = planned_partial_candidate(algorithm);
  let cancelled = CancellationToken::new();
  cancelled.cancel();
  let mut panic_authority = PanicAuthoritativeSource;
  let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &partial.plan,
    catalogs: &partial.catalogs,
    scope_id: &partial.scope_id,
    authoritative_source: &mut panic_authority,
    complete_source: None,
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &memory,
    cancellation: &cancelled,
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap_err();
  match error {
    QueryExactScopeExecutionErrorV1::Composition(error) => {
      assert_eq!(error.class(), QueryCandidateCompositionErrorClassV1::Cancelled);
    }
    error => panic!("unexpected cancellation error: {error:?}"),
  }

  let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &partial.plan,
    catalogs: &partial.catalogs,
    scope_id: &partial.scope_id,
    authoritative_source: &mut panic_authority,
    complete_source: None,
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &memory,
    cancellation: &CancellationToken::new(),
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap_err();
  match error {
    QueryExactScopeExecutionErrorV1::InvalidRequest { code, .. } => assert_eq!(code, "query_scope_partial_source"),
    error => panic!("unexpected missing-source error: {error:?}"),
  }

  let mut partial_source = PartialArtifactSource {
    artifacts: Source::default(),
    posting_root: None,
    posting_roots: BTreeMap::new(),
    scope_root: None,
    posting_complete: false,
    posting_error: None,
    posting_manifest_override: None,
    posting_manifest_overrides: BTreeMap::new(),
    scope_error: None,
    scope_source_override: None,
    posting_resolutions: 0,
    scope_resolutions: 0,
  };
  let mut complement = PartialComplement::default();
  let mut rechecker = PartialRechecker::default();
  let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &partial.plan,
    catalogs: &partial.catalogs,
    scope_id: &partial.scope_id,
    authoritative_source: &mut panic_authority,
    complete_source: None,
    partial_source: Some(&mut partial_source),
    complement: None,
    rechecker: Some(&mut rechecker),
    memory: &memory,
    cancellation: &CancellationToken::new(),
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap_err();
  match error {
    QueryExactScopeExecutionErrorV1::InvalidRequest { code, .. } => assert_eq!(code, "query_scope_complement_source"),
    error => panic!("unexpected missing-complement error: {error:?}"),
  }
  let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &partial.plan,
    catalogs: &partial.catalogs,
    scope_id: &partial.scope_id,
    authoritative_source: &mut panic_authority,
    complete_source: None,
    partial_source: Some(&mut partial_source),
    complement: Some(&mut complement),
    rechecker: None,
    memory: &memory,
    cancellation: &CancellationToken::new(),
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap_err();
  match error {
    QueryExactScopeExecutionErrorV1::InvalidRequest { code, .. } => assert_eq!(code, "query_scope_partial_rechecker"),
    error => panic!("unexpected missing-rechecker error: {error:?}"),
  }
  assert_eq!(partial_source.posting_resolutions, 0);
  assert_eq!(partial_source.scope_resolutions, 0);
  assert_eq!(complement.scans, 0);

  for invalid_scope in [vec![0; algorithm.hash_length()], vec![1; algorithm.hash_length() - 1]] {
    let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
      plan: &partial.plan,
      catalogs: &partial.catalogs,
      scope_id: &invalid_scope,
      authoritative_source: &mut panic_authority,
      complete_source: None,
      partial_source: None,
      complement: None,
      rechecker: None,
      memory: &memory,
      cancellation: &CancellationToken::new(),
      execution_limits: execution_limits(),
      candidate_limits: limits(),
      acceleration_limits: partial_acceleration_limits(),
      composition_limits: composition_limits(),
    })
    .unwrap_err();
    match error {
      QueryExactScopeExecutionErrorV1::InvalidRequest { code, .. } => assert_eq!(code, "query_scope_identity"),
      error => panic!("unexpected invalid-scope error: {error:?}"),
    }
  }
  let unknown_scope = vec![0x99; algorithm.hash_length()];
  let error = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &partial.plan,
    catalogs: &partial.catalogs,
    scope_id: &unknown_scope,
    authoritative_source: &mut panic_authority,
    complete_source: None,
    partial_source: None,
    complement: None,
    rechecker: None,
    memory: &memory,
    cancellation: &CancellationToken::new(),
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap_err();
  match error {
    QueryExactScopeExecutionErrorV1::InvalidRequest { code, .. } => assert_eq!(code, "query_scope_unknown"),
    error => panic!("unexpected unknown-scope error: {error:?}"),
  }
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
}

#[derive(Clone, Copy)]
enum BooleanFixtureCoverage {
  Authoritative,
  Complete,
  Partial,
}

fn execute_boolean_scope_fixture(
  expression: QueryExpressionV1,
  coverage: BooleanFixtureCoverage,
) -> (QueryExactScopeExecutionPathV1, Vec<String>) {
  let source_root = vec![0x55; HashAlgorithm::Blake3_256.hash_length()];
  let planned = match coverage {
    BooleanFixtureCoverage::Authoritative => planned_query_with_generation_sources(expression, None, None),
    BooleanFixtureCoverage::Complete => planned_query(expression),
    BooleanFixtureCoverage::Partial => {
      planned_query_with_generation_sources(expression, Some((source_root.clone(), 40)), Some((source_root, 40)))
    }
  };
  let algorithm = planned.plan.hash_algorithm();
  let rows = [(1u64, "/alpha.json", 20u64), (2u64, "/beta.json", 5u64), (3u64, "/gamma.json", 30u64)];
  let mut documents = rows
    .iter()
    .map(|(ordinal, path, size)| ExecutionDocument {
      scope_id: planned.scope_id.clone(),
      ordinal: *ordinal,
      file_key: digest_parts(algorithm, &[b"file:", path.as_bytes()]),
      revision: digest_parts(algorithm, &[b"revision:", &ordinal.to_le_bytes()]),
      path: (*path).to_string(),
      fields: BTreeMap::from([
        (
          "@filename".to_string(),
          encode_canonical_value(
            &CanonicalConfigValueV1::String(path.trim_start_matches('/').to_string()),
            aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
          )
          .unwrap(),
        ),
        (
          "@size".to_string(),
          encode_canonical_value(
            &CanonicalConfigValueV1::Unsigned(*size),
            aeordb::engine::v4::config_value::CanonicalValueBounds::SOURCE_VALUE,
          )
          .unwrap(),
        ),
      ]),
    })
    .collect::<Vec<_>>();
  documents.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));
  let path_by_key = documents.iter().map(|document| (document.file_key.clone(), document.path.clone())).collect::<BTreeMap<_, _>>();
  let mut authoritative_source =
    AuthoritativeExecutionSource { root: planned.root.clone(), publication_sequence: planned.plan.publication_sequence(), documents };
  let mut complete_source = FailingCompleteSource { class: QueryExecutionSourceErrorClassV1::Corrupt, code: "fixture_complete_disposable" };
  let mut partial_source = PartialArtifactSource {
    artifacts: Source::default(),
    posting_root: None,
    posting_roots: BTreeMap::new(),
    scope_root: None,
    posting_complete: false,
    posting_error: Some(IndexPartialSourceErrorV1::unavailable("fixture_partial_disposable", "partial fixture is unavailable")),
    posting_manifest_override: None,
    posting_manifest_overrides: BTreeMap::new(),
    scope_error: None,
    scope_source_override: None,
    posting_resolutions: 0,
    scope_resolutions: 0,
  };
  let mut complement = PartialComplement::default();
  let mut rechecker = PartialRechecker::default();
  let complete_source =
    matches!(coverage, BooleanFixtureCoverage::Complete).then_some(&mut complete_source as &mut dyn QueryCompleteCandidateSourceV1);
  let partial_source =
    matches!(coverage, BooleanFixtureCoverage::Partial).then_some(&mut partial_source as &mut dyn QueryPartialCandidateArtifactSourceV1);
  let complement = matches!(coverage, BooleanFixtureCoverage::Partial).then_some(&mut complement as &mut dyn IndexChangedDocumentSourceV1);
  let rechecker =
    matches!(coverage, BooleanFixtureCoverage::Partial).then_some(&mut rechecker as &mut dyn IndexPartialCandidateRecheckerV1);
  let memory = memory(32 * 1_024 * 1_024);
  let execution = execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
    plan: &planned.plan,
    catalogs: &planned.catalogs,
    scope_id: &planned.scope_id,
    authoritative_source: &mut authoritative_source,
    complete_source,
    partial_source,
    complement,
    rechecker,
    memory: &memory,
    cancellation: &CancellationToken::new(),
    execution_limits: execution_limits(),
    candidate_limits: limits(),
    acceleration_limits: partial_acceleration_limits(),
    composition_limits: composition_limits(),
  })
  .unwrap();
  let path = execution.path();
  let paths = execution.identities().map(|identity| path_by_key.get(identity.file_key()).unwrap().clone()).collect::<Vec<_>>();
  drop(execution);
  assert_eq!(memory.snapshot().unwrap().owner(MemoryOwner::Query).unwrap().reserved_bytes, 0);
  (path, paths)
}

#[test]
fn authoritative_complete_and_partial_plans_are_boolean_equivalent_through_the_scope_orchestrator() {
  let cases = [
    (
      QueryExpressionV1::And(vec![
        predicate("@filename", QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string()))),
        predicate("@size", QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Unsigned(10))),
      ]),
      vec!["/alpha.json"],
    ),
    (
      QueryExpressionV1::Or(vec![
        predicate("@filename", QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string()))),
        predicate("@size", QueryPredicateOperationV1::Gt(CanonicalConfigValueV1::Unsigned(10))),
      ]),
      vec!["/alpha.json", "/gamma.json"],
    ),
    (
      QueryExpressionV1::Not(Box::new(predicate(
        "@filename",
        QueryPredicateOperationV1::Eq(CanonicalConfigValueV1::String("alpha.json".to_string())),
      ))),
      vec!["/beta.json", "/gamma.json"],
    ),
  ];

  for (expression, mut expected) in cases {
    expected.sort_unstable();
    for coverage in [BooleanFixtureCoverage::Authoritative, BooleanFixtureCoverage::Complete, BooleanFixtureCoverage::Partial] {
      let (path, mut actual) = execute_boolean_scope_fixture(expression.clone(), coverage);
      actual.sort_unstable();
      assert_eq!(actual, expected);
      if matches!(expression, QueryExpressionV1::Not(_)) || matches!(coverage, BooleanFixtureCoverage::Authoritative) {
        assert_eq!(path, QueryExactScopeExecutionPathV1::Authoritative);
      } else if matches!(coverage, BooleanFixtureCoverage::Complete) {
        assert_eq!(path, QueryExactScopeExecutionPathV1::CompleteFallback);
      } else {
        assert_eq!(path, QueryExactScopeExecutionPathV1::PartialFallback);
      }
    }
  }
}
