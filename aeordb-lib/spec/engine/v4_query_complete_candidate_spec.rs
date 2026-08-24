use std::collections::BTreeMap;
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
use aeordb::engine::v4::index_record::{ScopeDocumentRecordV1, encode_scope_document_record};
use aeordb::engine::v4::query_complete_candidate::{
  QueryCandidateArtifactRootV1, QueryCandidateRecheckReceiptV1, QueryCandidateRecheckRequestV1, QueryCompleteCandidateErrorClassV1,
  QueryCompleteCandidateExecutionRequestV1, QueryCompleteCandidateLimitsV1, QueryCompleteCandidateSourceV1,
  QueryCompletePostingRootReceiptV1, QueryCompletePostingRootRequestV1, QueryCompletePostingScanRequestV1,
  QueryCompleteScopeResolutionRequestV1, QueryCompleteScopeRootReceiptV1, QueryCompleteScopeRootRequestV1,
  QueryPartialPostingScanRequestV1, QueryScopeOrdinalSelectionV1, execute_complete_candidate_root_query_v1,
  resolve_complete_scope_identities_v1, scan_complete_posting_ordinals_v1, scan_partial_posting_ordinals_v1,
};
use aeordb::engine::v4::query_executor::{
  QueryAuthoritativeDocumentVisitorV1, QueryAuthoritativeFieldSourceV1, QueryAuthoritativeScopeSourceV1, QueryAuthoritativeValueVisitorV1,
  QueryExecutionByteLimitsV1, QueryExecutionCountLimitsV1, QueryExecutionDocumentV1, QueryExecutionErrorClassV1,
  QueryExecutionFieldReadReceiptV1, QueryExecutionFieldReadRequestV1, QueryExecutionFieldStateV1, QueryExecutionLimitsV1,
  QueryExecutionScanErrorV1, QueryExecutionScopeScanReceiptV1, QueryExecutionScopeScanRequestV1, RootAwareQueryExecutionRequestV1,
  execute_authoritative_root_query_v1,
};
use aeordb::engine::v4::query_planner::{
  CompiledQueryIndexCandidateV1, CompiledRootAwareQueryPlanV1, QueryCoordinateConstraintV1, QueryExpressionV1, QueryPlanningContextV1,
  QueryPlanningCoverageGenerationV1, QueryPlanningIndexCandidateV1, QueryPlanningIndexEstimatesV1, QueryPlanningRequestV1,
  QueryPlanDriverV1, QueryPlanningScopeV1, QueryPredicateOperationV1, QueryPredicateV1, RootAwareQueryFieldCatalogV1,
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

fn planned_phonetic_union_query(query: &str) -> PlannedQuery {
  let algorithm = HashAlgorithm::Blake3_256;
  let root = vec![0x33; algorithm.hash_length()];
  let semantic_root = vec![0x44; algorithm.hash_length()];
  let mut catalog = query_catalog(algorithm, &root, &semantic_root, "@filename", "double_metaphone_primary_ascii_v1", 2, 0x71);
  let mut alternate = query_catalog(algorithm, &root, &semantic_root, "@filename", "double_metaphone_alt_ascii_v1", 2, 0x73);
  assert_eq!(catalog.scopes[0].scope_id, alternate.scopes[0].scope_id);
  assert_eq!(catalog.scopes[0].value_store_id, alternate.scopes[0].value_store_id);
  catalog.scopes[0].indexes.append(&mut alternate.scopes[0].indexes);
  catalog.scopes[0].indexes.sort_by(|left, right| left.index_id.cmp(&right.index_id));
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
      complete: true,
    })
  }
}

fn execution_limits() -> QueryExecutionLimitsV1 {
  QueryExecutionLimitsV1::new(
    QueryExecutionCountLimitsV1::new(128, 1_024, 128, 1_000_000).unwrap(),
    QueryExecutionByteLimitsV1::new(1_024 * 1_024, 2 * 1_024 * 1_024, 4 * 1_024 * 1_024).unwrap(),
  )
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
