//! Storage-neutral, selected-root query compilation and cost planning.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use serde::Serialize;

use crate::engine::HashAlgorithm;
use crate::engine::path_utils::normalize_path;

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, encode_canonical_value};
use super::field_definition::decode_field_index_definition;
use super::hash::IncrementalDigestV1;
use super::index_converter::{CompiledSourceValueV1, IndexSemanticErrorClassV1};
use super::index_coverage_planner::{
  IndexCoverageGenerationHealthV1, IndexCoverageGenerationV1, IndexCoveragePlanV1, IndexCoveragePlanningRequestV1,
  IndexHistoricalViewUnavailableReasonV1, IndexSemanticQueryAvailabilityV1, plan_selected_index_coverage_v1,
};
use super::index_coverage_registry::{field_definition_fingerprint, field_dependency_fingerprint};
use super::index_definition_runtime::{IndexDefinitionErrorClassV1, IndexDefinitionRuntimeV1};
use super::index_semantic_registry::{
  OPERATION_AGGREGATE, OPERATION_BETWEEN, OPERATION_CONTAINS, OPERATION_EQ, OPERATION_FUZZY, OPERATION_GT, OPERATION_IN, OPERATION_LT,
  OPERATION_MATCH, OPERATION_PHONETIC, OPERATION_SIMILAR, OPERATION_SORT,
};
use super::read_view::ResolvedReadViewV1;
use super::scope::decode_scope_definition;
use super::value_store::decode_value_store_definition;

pub const QUERY_MAXIMUM_EXPRESSION_DEPTH_V1: usize = 32;
pub const QUERY_MAXIMUM_EXPRESSION_NODES_V1: usize = 1_024;
pub const QUERY_MAXIMUM_IN_LITERALS_V1: usize = 4_096;
pub const QUERY_MAXIMUM_LITERAL_BYTES_V1: usize = 1_048_576;
pub const QUERY_MAXIMUM_TOTAL_LITERAL_BYTES_V1: usize = 8 * 1_048_576;
pub const QUERY_MAXIMUM_SORT_FIELDS_V1: usize = 32;
pub const QUERY_MAXIMUM_AGGREGATE_FIELDS_V1: usize = 32;
pub const QUERY_MAXIMUM_GROUP_FIELDS_V1: usize = 32;
pub const QUERY_MAXIMUM_FUZZY_EDITS_V1: u8 = 8;
pub const QUERY_MAXIMUM_RETURNED_DOCUMENTS_V1: usize = 1_000;
pub const QUERY_MAXIMUM_FIELD_NAME_BYTES_V1: usize = 4 * 1_024;
pub const QUERY_MAXIMUM_PATH_BYTES_V1: usize = u16::MAX as usize;

const DEFAULT_MAXIMUM_SCOPES_PER_FIELD: usize = 4_096;
const DEFAULT_MAXIMUM_CANDIDATES_PER_SCOPE: usize = 64;
const DEFAULT_MAXIMUM_DEFINITION_BYTES: u64 = 64 * 1_048_576;
const PAGE_WORK_WEIGHT: u64 = 16;
const POSTING_WORK_WEIGHT: u64 = 2;
const CARDINALITY_WORK_WEIGHT: u64 = 1;
const AUTHORITATIVE_WORK_WEIGHT: u64 = 4;
const QUERY_FINGERPRINT_DOMAIN_V1: &[u8] = b"aeordb.query-plan.v1\0";

#[derive(Clone, Copy, Debug)]
pub struct QueryPlanningContextV1<'a> {
  database_id: [u8; 16],
  physical_instance_id: [u8; 16],
  hash_algorithm: HashAlgorithm,
  selected_namespace_root: &'a [u8],
  semantic_state_root: &'a [u8],
  publication_sequence: u64,
}

impl<'a> QueryPlanningContextV1<'a> {
  pub fn new(
    database_id: [u8; 16],
    physical_instance_id: [u8; 16],
    hash_algorithm: HashAlgorithm,
    selected_namespace_root: &'a [u8],
    semantic_state_root: &'a [u8],
    publication_sequence: u64,
  ) -> QueryPlanningResultV1<Self> {
    let hash_width = hash_algorithm.hash_length();
    validate_nonzero_hash(selected_namespace_root, hash_width, "selected namespace root")?;
    validate_nonzero_hash(semantic_state_root, hash_width, "semantic state root")?;
    if database_id.iter().all(|byte| *byte == 0) || physical_instance_id.iter().all(|byte| *byte == 0) || publication_sequence == 0 {
      return Err(invalid_request(
        "query_planning_context_invalid",
        "database, physical instance, and publication identities must be nonzero",
      ));
    }
    Ok(Self { database_id, physical_instance_id, hash_algorithm, selected_namespace_root, semantic_state_root, publication_sequence })
  }

  pub fn from_resolved_view<A>(view: &'a ResolvedReadViewV1<A>) -> QueryPlanningResultV1<Self> {
    Self::new(
      view.database_id(),
      view.physical_instance_id(),
      view.hash_algorithm(),
      &view.root_metadata().hash,
      &view.authority().root.semantic_state_root,
      view.authority().admission.publication_sequence,
    )
  }

  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn physical_instance_id(&self) -> [u8; 16] {
    self.physical_instance_id
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub const fn selected_namespace_root(&self) -> &[u8] {
    self.selected_namespace_root
  }

  pub const fn semantic_state_root(&self) -> &[u8] {
    self.semantic_state_root
  }

  pub const fn publication_sequence(&self) -> u64 {
    self.publication_sequence
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryPlanningLimitsV1 {
  maximum_expression_nodes: usize,
  maximum_expression_depth: usize,
  maximum_scopes_per_field: usize,
  maximum_definition_bytes: u64,
  maximum_in_literals: usize,
  maximum_candidates_per_scope: usize,
  maximum_returned_documents: usize,
}

impl QueryPlanningLimitsV1 {
  pub fn new(
    maximum_expression_nodes: usize,
    maximum_expression_depth: usize,
    maximum_scopes_per_field: usize,
    maximum_definition_bytes: u64,
    maximum_in_literals: usize,
    maximum_candidates_per_scope: usize,
    maximum_returned_documents: usize,
  ) -> QueryPlanningResultV1<Self> {
    if maximum_expression_nodes == 0
      || maximum_expression_nodes > QUERY_MAXIMUM_EXPRESSION_NODES_V1
      || maximum_expression_depth == 0
      || maximum_expression_depth > QUERY_MAXIMUM_EXPRESSION_DEPTH_V1
      || maximum_scopes_per_field == 0
      || maximum_scopes_per_field > DEFAULT_MAXIMUM_SCOPES_PER_FIELD
      || maximum_definition_bytes == 0
      || maximum_definition_bytes > DEFAULT_MAXIMUM_DEFINITION_BYTES
      || maximum_in_literals == 0
      || maximum_in_literals > QUERY_MAXIMUM_IN_LITERALS_V1
      || maximum_candidates_per_scope == 0
      || maximum_candidates_per_scope > DEFAULT_MAXIMUM_CANDIDATES_PER_SCOPE
      || maximum_returned_documents == 0
      || maximum_returned_documents > QUERY_MAXIMUM_RETURNED_DOCUMENTS_V1
    {
      return Err(invalid_request(
        "query_planning_limits_invalid",
        "query planning limits must be nonzero and remain within frozen protocol maxima",
      ));
    }
    Ok(Self {
      maximum_expression_nodes,
      maximum_expression_depth,
      maximum_scopes_per_field,
      maximum_definition_bytes,
      maximum_in_literals,
      maximum_candidates_per_scope,
      maximum_returned_documents,
    })
  }
}

pub fn default_query_planning_limits_v1() -> QueryPlanningLimitsV1 {
  QueryPlanningLimitsV1 {
    maximum_expression_nodes: QUERY_MAXIMUM_EXPRESSION_NODES_V1,
    maximum_expression_depth: QUERY_MAXIMUM_EXPRESSION_DEPTH_V1,
    maximum_scopes_per_field: DEFAULT_MAXIMUM_SCOPES_PER_FIELD,
    maximum_definition_bytes: DEFAULT_MAXIMUM_DEFINITION_BYTES,
    maximum_in_literals: QUERY_MAXIMUM_IN_LITERALS_V1,
    maximum_candidates_per_scope: DEFAULT_MAXIMUM_CANDIDATES_PER_SCOPE,
    maximum_returned_documents: QUERY_MAXIMUM_RETURNED_DOCUMENTS_V1,
  }
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryExpressionV1 {
  Field(QueryPredicateV1),
  And(Vec<QueryExpressionV1>),
  Or(Vec<QueryExpressionV1>),
  Not(Box<QueryExpressionV1>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueryPredicateV1 {
  pub field_name: String,
  pub operation: QueryPredicateOperationV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryFuzzyAlgorithmV1 {
  DamerauLevenshtein,
  JaroWinkler,
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryPredicateOperationV1 {
  Eq(CanonicalConfigValueV1),
  In(Vec<CanonicalConfigValueV1>),
  Gt(CanonicalConfigValueV1),
  Lt(CanonicalConfigValueV1),
  Between(CanonicalConfigValueV1, CanonicalConfigValueV1),
  Contains(CanonicalConfigValueV1),
  Similar { value: CanonicalConfigValueV1, threshold: f64 },
  Phonetic(CanonicalConfigValueV1),
  Fuzzy { value: CanonicalConfigValueV1, algorithm: QueryFuzzyAlgorithmV1, edits: Option<u8> },
  Match(CanonicalConfigValueV1),
}

impl QueryPredicateOperationV1 {
  pub const fn name(&self) -> &'static str {
    match self {
      Self::Eq(_) => "eq",
      Self::In(_) => "in",
      Self::Gt(_) => "gt",
      Self::Lt(_) => "lt",
      Self::Between(_, _) => "between",
      Self::Contains(_) => "contains",
      Self::Similar { .. } => "similar",
      Self::Phonetic(_) => "phonetic",
      Self::Fuzzy { .. } => "fuzzy",
      Self::Match(_) => "match",
    }
  }

  pub const fn operation_bit(&self) -> u64 {
    match self {
      Self::Eq(_) => OPERATION_EQ,
      Self::In(_) => OPERATION_IN,
      Self::Gt(_) => OPERATION_GT,
      Self::Lt(_) => OPERATION_LT,
      Self::Between(_, _) => OPERATION_BETWEEN,
      Self::Contains(_) => OPERATION_CONTAINS,
      Self::Similar { .. } => OPERATION_SIMILAR,
      Self::Phonetic(_) => OPERATION_PHONETIC,
      Self::Fuzzy { .. } => OPERATION_FUZZY,
      Self::Match(_) => OPERATION_MATCH,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuerySortDirectionV1 {
  Ascending,
  Descending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuerySortFieldV1 {
  pub field_name: String,
  pub direction: QuerySortDirectionV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryAggregateKindV1 {
  Sum,
  Average,
  Minimum,
  Maximum,
  Count,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryAggregateFieldV1 {
  pub field_name: String,
  pub kind: QueryAggregateKindV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPlanningCoverageGenerationV1 {
  pub generation: u64,
  pub owner_id: Vec<u8>,
  pub manifest_hash: Vec<u8>,
  pub source_namespace_root: Vec<u8>,
  pub coverage_epoch_id: [u8; 16],
  pub coverage_publication_sequence: u64,
  pub definition_fingerprint: Vec<u8>,
  pub dependency_fingerprint: Vec<u8>,
  pub health: IndexCoverageGenerationHealthV1,
}

impl QueryPlanningCoverageGenerationV1 {
  fn as_coverage_generation(&self) -> IndexCoverageGenerationV1<'_> {
    IndexCoverageGenerationV1 {
      generation: self.generation,
      owner_id: &self.owner_id,
      manifest_hash: &self.manifest_hash,
      source_namespace_root: &self.source_namespace_root,
      coverage_epoch_id: &self.coverage_epoch_id,
      coverage_publication_sequence: self.coverage_publication_sequence,
      definition_fingerprint: &self.definition_fingerprint,
      dependency_fingerprint: &self.dependency_fingerprint,
      health: self.health,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryPlanningIndexEstimatesV1 {
  page_count: u64,
  posting_count: u64,
  distinct_document_count: u64,
  estimated_candidate_count: u64,
  authoritative_fallback_document_count: u64,
}

impl QueryPlanningIndexEstimatesV1 {
  pub fn new(
    page_count: u64,
    posting_count: u64,
    distinct_document_count: u64,
    estimated_candidate_count: u64,
    authoritative_fallback_document_count: u64,
  ) -> QueryPlanningResultV1<Self> {
    if page_count == 0 && (posting_count != 0 || distinct_document_count != 0 || estimated_candidate_count != 0)
      || distinct_document_count > posting_count
      || estimated_candidate_count > posting_count
    {
      return Err(invalid_request("query_index_estimates_invalid", "index estimates are internally inconsistent"));
    }
    Ok(Self { page_count, posting_count, distinct_document_count, estimated_candidate_count, authoritative_fallback_document_count })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPlanningIndexCandidateV1 {
  pub index_id: Vec<u8>,
  pub encoded_field_definition: Vec<u8>,
  pub selected_generation: Option<QueryPlanningCoverageGenerationV1>,
  pub estimates: QueryPlanningIndexEstimatesV1,
  pub nvt_hint_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPlanningScopeV1 {
  pub scope_id: Vec<u8>,
  pub value_store_id: Vec<u8>,
  pub encoded_scope_definition: Vec<u8>,
  pub encoded_value_store_definition: Vec<u8>,
  pub semantic_availability: IndexSemanticQueryAvailabilityV1,
  pub authoritative_document_count: u64,
  pub indexes: Vec<QueryPlanningIndexCandidateV1>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootAwareQueryFieldCatalogV1 {
  pub database_id: [u8; 16],
  pub physical_instance_id: [u8; 16],
  pub selected_namespace_root: Vec<u8>,
  pub semantic_state_root: Vec<u8>,
  pub publication_sequence: u64,
  pub field_name: String,
  pub complete: bool,
  pub scopes: Vec<QueryPlanningScopeV1>,
}

#[derive(Clone, Copy)]
pub struct QueryPlanningRequestV1<'a> {
  pub context: &'a QueryPlanningContextV1<'a>,
  pub query_path: &'a str,
  pub expression: &'a QueryExpressionV1,
  pub catalogs: &'a [RootAwareQueryFieldCatalogV1],
  pub sort_fields: &'a [QuerySortFieldV1],
  pub aggregate_fields: &'a [QueryAggregateFieldV1],
  pub group_fields: &'a [String],
  pub result_limit: usize,
  pub limits: QueryPlanningLimitsV1,
  pub is_cancelled: &'a dyn Fn() -> bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryValueMatchV1 {
  AllPostings,
  AnyPosting,
  OrderedRange,
  AuthoritativeRecheck,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryCoordinateConstraintV1 {
  Points(Vec<u64>),
  InclusiveRange { start: u64, end: u64, widen_start_cell: bool, widen_end_cell: bool },
  FullScan,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledQueryLiteralV1 {
  compiled: CompiledSourceValueV1,
}

impl CompiledQueryLiteralV1 {
  pub fn canonical_value(&self) -> &[u8] {
    &self.compiled.canonical_value
  }

  pub const fn compiled(&self) -> &CompiledSourceValueV1 {
    &self.compiled
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompiledQueryCoverageV1 {
  Complete,
  PartialExact,
  AuthoritativeOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledQueryIndexCandidateV1 {
  index_id: Vec<u8>,
  strategy_name: String,
  selected_generation: Option<QueryPlanningCoverageGenerationV1>,
  compiled_literals: Vec<CompiledQueryLiteralV1>,
  coordinate_constraint: QueryCoordinateConstraintV1,
  value_match: QueryValueMatchV1,
  coverage: CompiledQueryCoverageV1,
  estimated_work: u64,
  proven_candidate_superset: bool,
}

impl CompiledQueryIndexCandidateV1 {
  pub fn index_id(&self) -> &[u8] {
    &self.index_id
  }

  pub fn strategy_name(&self) -> &str {
    &self.strategy_name
  }

  pub const fn selected_generation(&self) -> Option<&QueryPlanningCoverageGenerationV1> {
    self.selected_generation.as_ref()
  }

  pub fn compiled_literals(&self) -> &[CompiledQueryLiteralV1] {
    &self.compiled_literals
  }

  pub const fn coordinate_constraint(&self) -> &QueryCoordinateConstraintV1 {
    &self.coordinate_constraint
  }

  pub const fn value_match(&self) -> QueryValueMatchV1 {
    self.value_match
  }

  pub const fn coverage(&self) -> CompiledQueryCoverageV1 {
    self.coverage
  }

  pub const fn estimated_work(&self) -> u64 {
    self.estimated_work
  }

  pub const fn proven_candidate_superset(&self) -> bool {
    self.proven_candidate_superset
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryPlanDriverV1 {
  Authoritative { estimated_work: u64 },
  Index { candidate_index: usize, coverage: CompiledQueryCoverageV1, estimated_work: u64 },
  IndexUnion { candidate_indexes: Vec<usize>, coverage: CompiledQueryCoverageV1, estimated_work: u64 },
}

impl QueryPlanDriverV1 {
  fn estimated_work(&self) -> u64 {
    match self {
      Self::Authoritative { estimated_work } | Self::Index { estimated_work, .. } | Self::IndexUnion { estimated_work, .. } => {
        *estimated_work
      }
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledQueryScopePlanV1 {
  scope_id: Vec<u8>,
  candidates: Vec<CompiledQueryIndexCandidateV1>,
  driver: QueryPlanDriverV1,
}

impl CompiledQueryScopePlanV1 {
  pub fn scope_id(&self) -> &[u8] {
    &self.scope_id
  }

  pub fn candidates(&self) -> &[CompiledQueryIndexCandidateV1] {
    &self.candidates
  }

  pub const fn driver(&self) -> &QueryPlanDriverV1 {
    &self.driver
  }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledQueryPredicatePlanV1 {
  field_name: String,
  operation: QueryPredicateOperationV1,
  scopes: Vec<CompiledQueryScopePlanV1>,
}

impl CompiledQueryPredicatePlanV1 {
  pub fn field_name(&self) -> &str {
    &self.field_name
  }

  pub const fn operation_name(&self) -> &'static str {
    self.operation.name()
  }

  pub const fn operation(&self) -> &QueryPredicateOperationV1 {
    &self.operation
  }

  pub fn scopes(&self) -> &[CompiledQueryScopePlanV1] {
    &self.scopes
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompiledQueryExpressionV1 {
  Field(usize),
  And(Vec<CompiledQueryExpressionV1>),
  Or(Vec<CompiledQueryExpressionV1>),
  Not(Box<CompiledQueryExpressionV1>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompiledQueryAuxiliaryOperationV1 {
  Sort(QuerySortDirectionV1),
  Aggregate(QueryAggregateKindV1),
  Group,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledQueryAuxiliaryIndexCandidateV1 {
  index_id: Vec<u8>,
  strategy_name: String,
  selected_generation: Option<QueryPlanningCoverageGenerationV1>,
  coverage: CompiledQueryCoverageV1,
  estimated_work: u64,
}

impl CompiledQueryAuxiliaryIndexCandidateV1 {
  pub fn index_id(&self) -> &[u8] {
    &self.index_id
  }

  pub fn strategy_name(&self) -> &str {
    &self.strategy_name
  }

  pub const fn selected_generation(&self) -> Option<&QueryPlanningCoverageGenerationV1> {
    self.selected_generation.as_ref()
  }

  pub const fn coverage(&self) -> CompiledQueryCoverageV1 {
    self.coverage
  }

  pub const fn estimated_work(&self) -> u64 {
    self.estimated_work
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledQueryAuxiliaryScopePlanV1 {
  scope_id: Vec<u8>,
  candidates: Vec<CompiledQueryAuxiliaryIndexCandidateV1>,
  driver: QueryPlanDriverV1,
}

impl CompiledQueryAuxiliaryScopePlanV1 {
  pub fn scope_id(&self) -> &[u8] {
    &self.scope_id
  }

  pub fn candidates(&self) -> &[CompiledQueryAuxiliaryIndexCandidateV1] {
    &self.candidates
  }

  pub const fn driver(&self) -> &QueryPlanDriverV1 {
    &self.driver
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledQueryAuxiliaryFieldPlanV1 {
  field_name: String,
  operation: CompiledQueryAuxiliaryOperationV1,
  scopes: Vec<CompiledQueryAuxiliaryScopePlanV1>,
}

impl CompiledQueryAuxiliaryFieldPlanV1 {
  pub fn field_name(&self) -> &str {
    &self.field_name
  }

  pub const fn operation(&self) -> CompiledQueryAuxiliaryOperationV1 {
    self.operation
  }

  pub fn scopes(&self) -> &[CompiledQueryAuxiliaryScopePlanV1] {
    &self.scopes
  }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledRootAwareQueryPlanV1 {
  database_id: [u8; 16],
  physical_instance_id: [u8; 16],
  hash_algorithm: HashAlgorithm,
  selected_namespace_root: Vec<u8>,
  semantic_state_root: Vec<u8>,
  publication_sequence: u64,
  query_path: String,
  result_limit: usize,
  expression: CompiledQueryExpressionV1,
  predicates: Vec<CompiledQueryPredicatePlanV1>,
  auxiliary_fields: Vec<CompiledQueryAuxiliaryFieldPlanV1>,
  total_literal_bytes: u64,
  estimated_work: u64,
  query_fingerprint: Vec<u8>,
}

impl CompiledRootAwareQueryPlanV1 {
  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn physical_instance_id(&self) -> [u8; 16] {
    self.physical_instance_id
  }

  pub const fn hash_algorithm(&self) -> HashAlgorithm {
    self.hash_algorithm
  }

  pub fn semantic_state_root(&self) -> &[u8] {
    &self.semantic_state_root
  }

  pub const fn publication_sequence(&self) -> u64 {
    self.publication_sequence
  }

  pub fn query_path(&self) -> &str {
    &self.query_path
  }

  pub const fn result_limit(&self) -> usize {
    self.result_limit
  }

  pub fn predicates(&self) -> &[CompiledQueryPredicatePlanV1] {
    &self.predicates
  }

  pub const fn expression(&self) -> &CompiledQueryExpressionV1 {
    &self.expression
  }

  pub fn auxiliary_fields(&self) -> &[CompiledQueryAuxiliaryFieldPlanV1] {
    &self.auxiliary_fields
  }

  pub const fn total_literal_bytes(&self) -> u64 {
    self.total_literal_bytes
  }

  pub const fn estimated_work(&self) -> u64 {
    self.estimated_work
  }

  pub fn query_fingerprint(&self) -> &[u8] {
    &self.query_fingerprint
  }

  pub fn selected_namespace_root(&self) -> &[u8] {
    &self.selected_namespace_root
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryLogicalDriverKindV1 {
  Authoritative,
  Index,
  IndexUnion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryLogicalWorkClassV1 {
  Minimal,
  Low,
  Moderate,
  High,
  Extensive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueryLogicalExplainFieldV1 {
  pub field: String,
  pub operation: String,
  pub driver: QueryLogicalDriverKindV1,
  pub coverage: CompiledQueryCoverageV1,
  pub work: QueryLogicalWorkClassV1,
  pub exact_recheck: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueryLogicalExplainV1 {
  pub root_bound: bool,
  pub fields: Vec<QueryLogicalExplainFieldV1>,
}

pub fn authorization_safe_query_explain_v1(plan: &CompiledRootAwareQueryPlanV1) -> QueryLogicalExplainV1 {
  let mut fields = Vec::new();
  for predicate in &plan.predicates {
    let (driver, coverage, work) = summarize_drivers(predicate.scopes.iter().map(|scope| &scope.driver));
    fields.push(QueryLogicalExplainFieldV1 {
      field: predicate.field_name.clone(),
      operation: predicate.operation_name().to_string(),
      driver,
      coverage,
      work,
      exact_recheck: true,
    });
  }
  for auxiliary in &plan.auxiliary_fields {
    let (driver, coverage, work) = summarize_drivers(auxiliary.scopes.iter().map(|scope| &scope.driver));
    fields.push(QueryLogicalExplainFieldV1 {
      field: auxiliary.field_name.clone(),
      operation: match auxiliary.operation {
        CompiledQueryAuxiliaryOperationV1::Sort(_) => "sort",
        CompiledQueryAuxiliaryOperationV1::Aggregate(_) => "aggregate",
        CompiledQueryAuxiliaryOperationV1::Group => "group",
      }
      .to_string(),
      driver,
      coverage,
      work,
      exact_recheck: true,
    });
  }
  QueryLogicalExplainV1 { root_bound: true, fields }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryPlanningErrorClassV1 {
  InvalidRequest,
  ResourceLimit,
  HistoricalViewUnavailable,
  CorruptSource,
  Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPlanningErrorV1 {
  class: QueryPlanningErrorClassV1,
  code: &'static str,
  context: String,
}

impl QueryPlanningErrorV1 {
  pub const fn class(&self) -> QueryPlanningErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for QueryPlanningErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for QueryPlanningErrorV1 {}

pub type QueryPlanningResultV1<T> = Result<T, QueryPlanningErrorV1>;

pub fn plan_root_aware_query_v1(request: &QueryPlanningRequestV1<'_>) -> QueryPlanningResultV1<CompiledRootAwareQueryPlanV1> {
  check_cancelled(request.is_cancelled)?;
  validate_query_path(request.query_path)?;
  validate_request_collections(request)?;

  let mut admission = AdmissionStateV1::new(request.limits);
  let mut required_fields = BTreeSet::new();
  preflight_expression(request.expression, 0, &mut admission, &mut required_fields, request.context.hash_algorithm())?;
  for field in request.sort_fields {
    required_fields.insert(canonical_field_name(&field.field_name)?.to_string());
  }
  for field in request.aggregate_fields {
    required_fields.insert(canonical_field_name(&field.field_name)?.to_string());
  }
  for field in request.group_fields {
    required_fields.insert(canonical_field_name(field)?.to_string());
  }
  check_cancelled(request.is_cancelled)?;

  let catalogs = validate_catalogs(request, &required_fields, &mut admission)?;
  let mut predicates = Vec::new();
  predicates
    .try_reserve_exact(admission.field_nodes)
    .map_err(|source| resource_error("query_predicate_reserve", format!("cannot reserve bounded predicate plan: {source}")))?;
  let expression = compile_expression(request.expression, request, &catalogs, &mut predicates)?;

  let mut auxiliary_fields = Vec::new();
  compile_auxiliary_fields(request, &catalogs, &mut auxiliary_fields)?;
  check_cancelled(request.is_cancelled)?;

  let mut estimated_work = 0u64;
  for predicate in &predicates {
    for scope in &predicate.scopes {
      estimated_work = estimated_work.saturating_add(scope.driver.estimated_work());
    }
  }
  for auxiliary in &auxiliary_fields {
    for scope in &auxiliary.scopes {
      estimated_work = estimated_work.saturating_add(scope.driver.estimated_work());
    }
  }
  let query_fingerprint = query_fingerprint(request)?;

  Ok(CompiledRootAwareQueryPlanV1 {
    database_id: request.context.database_id(),
    physical_instance_id: request.context.physical_instance_id(),
    hash_algorithm: request.context.hash_algorithm(),
    selected_namespace_root: clone_bytes(request.context.selected_namespace_root(), "selected root")?,
    semantic_state_root: clone_bytes(request.context.semantic_state_root(), "semantic root")?,
    publication_sequence: request.context.publication_sequence(),
    query_path: clone_string(request.query_path, "query path")?,
    result_limit: request.result_limit,
    expression,
    predicates,
    auxiliary_fields,
    total_literal_bytes: admission.total_literal_bytes,
    estimated_work,
    query_fingerprint,
  })
}

fn query_fingerprint(request: &QueryPlanningRequestV1<'_>) -> QueryPlanningResultV1<Vec<u8>> {
  let algorithm = request.context.hash_algorithm();
  let mut digest = IncrementalDigestV1::new(algorithm);
  digest.update(QUERY_FINGERPRINT_DOMAIN_V1);
  digest.update(&algorithm.to_u16().to_le_bytes());
  digest.update(&request.context.database_id());
  digest.update(&request.context.physical_instance_id());
  fingerprint_framed(&mut digest, request.context.selected_namespace_root())?;
  fingerprint_framed(&mut digest, request.context.semantic_state_root())?;
  digest.update(&request.context.publication_sequence().to_le_bytes());
  fingerprint_framed(&mut digest, request.query_path.as_bytes())?;
  digest.update(&fingerprint_count(request.result_limit)?.to_le_bytes());
  fingerprint_expression(&mut digest, request.expression, algorithm)?;

  digest.update(&fingerprint_count(request.sort_fields.len())?.to_le_bytes());
  for field in request.sort_fields {
    fingerprint_framed(&mut digest, canonical_field_name(&field.field_name)?.as_bytes())?;
    digest.update(&[match field.direction {
      QuerySortDirectionV1::Ascending => 0,
      QuerySortDirectionV1::Descending => 1,
    }]);
  }
  digest.update(&fingerprint_count(request.aggregate_fields.len())?.to_le_bytes());
  for field in request.aggregate_fields {
    fingerprint_framed(&mut digest, canonical_field_name(&field.field_name)?.as_bytes())?;
    digest.update(&[match field.kind {
      QueryAggregateKindV1::Sum => 0,
      QueryAggregateKindV1::Average => 1,
      QueryAggregateKindV1::Minimum => 2,
      QueryAggregateKindV1::Maximum => 3,
      QueryAggregateKindV1::Count => 4,
    }]);
  }
  digest.update(&fingerprint_count(request.group_fields.len())?.to_le_bytes());
  for field in request.group_fields {
    fingerprint_framed(&mut digest, canonical_field_name(field)?.as_bytes())?;
  }
  Ok(digest.finalize())
}

fn fingerprint_expression(
  digest: &mut IncrementalDigestV1,
  expression: &QueryExpressionV1,
  algorithm: HashAlgorithm,
) -> QueryPlanningResultV1<()> {
  match expression {
    QueryExpressionV1::Field(predicate) => {
      digest.update(&[0]);
      let field_name = canonical_field_name(&predicate.field_name)?;
      fingerprint_framed(digest, field_name.as_bytes())?;
      fingerprint_operation(digest, field_name, &predicate.operation, algorithm)
    }
    QueryExpressionV1::And(children) | QueryExpressionV1::Or(children) => {
      digest.update(&[if matches!(expression, QueryExpressionV1::And(_)) { 1 } else { 2 }]);
      digest.update(&fingerprint_count(children.len())?.to_le_bytes());
      for child in children {
        fingerprint_expression(digest, child, algorithm)?;
      }
      Ok(())
    }
    QueryExpressionV1::Not(child) => {
      digest.update(&[3]);
      fingerprint_expression(digest, child, algorithm)
    }
  }
}

fn fingerprint_operation(
  digest: &mut IncrementalDigestV1,
  field_name: &str,
  operation: &QueryPredicateOperationV1,
  algorithm: HashAlgorithm,
) -> QueryPlanningResultV1<()> {
  digest.update(&[match operation {
    QueryPredicateOperationV1::Eq(_) => 0,
    QueryPredicateOperationV1::In(_) => 1,
    QueryPredicateOperationV1::Gt(_) => 2,
    QueryPredicateOperationV1::Lt(_) => 3,
    QueryPredicateOperationV1::Between(_, _) => 4,
    QueryPredicateOperationV1::Contains(_) => 5,
    QueryPredicateOperationV1::Similar { .. } => 6,
    QueryPredicateOperationV1::Phonetic(_) => 7,
    QueryPredicateOperationV1::Fuzzy { .. } => 8,
    QueryPredicateOperationV1::Match(_) => 9,
  }]);
  match operation {
    QueryPredicateOperationV1::In(values) => {
      digest.update(&fingerprint_count(values.len())?.to_le_bytes());
      for value in values {
        fingerprint_literal(digest, field_name, value, algorithm)?;
      }
    }
    QueryPredicateOperationV1::Between(left, right) => {
      fingerprint_literal(digest, field_name, left, algorithm)?;
      fingerprint_literal(digest, field_name, right, algorithm)?;
    }
    QueryPredicateOperationV1::Similar { value, threshold } => {
      fingerprint_literal(digest, field_name, value, algorithm)?;
      digest.update(&threshold.to_bits().to_le_bytes());
    }
    QueryPredicateOperationV1::Fuzzy { value, algorithm: fuzzy_algorithm, edits } => {
      fingerprint_literal(digest, field_name, value, algorithm)?;
      digest.update(&[match fuzzy_algorithm {
        QueryFuzzyAlgorithmV1::DamerauLevenshtein => 0,
        QueryFuzzyAlgorithmV1::JaroWinkler => 1,
      }]);
      match edits {
        Some(edits) => digest.update(&[1, *edits]),
        None => digest.update(&[0]),
      }
    }
    QueryPredicateOperationV1::Eq(value)
    | QueryPredicateOperationV1::Gt(value)
    | QueryPredicateOperationV1::Lt(value)
    | QueryPredicateOperationV1::Contains(value)
    | QueryPredicateOperationV1::Phonetic(value)
    | QueryPredicateOperationV1::Match(value) => fingerprint_literal(digest, field_name, value, algorithm)?,
  }
  Ok(())
}

fn fingerprint_literal(
  digest: &mut IncrementalDigestV1,
  field_name: &str,
  value: &CanonicalConfigValueV1,
  algorithm: HashAlgorithm,
) -> QueryPlanningResultV1<()> {
  let encoded = encode_query_literal(field_name, value, algorithm)?;
  fingerprint_framed(digest, &encoded)
}

fn fingerprint_framed(digest: &mut IncrementalDigestV1, value: &[u8]) -> QueryPlanningResultV1<()> {
  digest.update(&fingerprint_count(value.len())?.to_le_bytes());
  digest.update(value);
  Ok(())
}

fn fingerprint_count(value: usize) -> QueryPlanningResultV1<u64> {
  u64::try_from(value).map_err(|source| resource_error("query_fingerprint_count", format!("query fingerprint length overflowed: {source}")))
}

struct AdmissionStateV1 {
  limits: QueryPlanningLimitsV1,
  nodes: usize,
  field_nodes: usize,
  total_literal_bytes: u64,
  definition_bytes: u64,
}

impl AdmissionStateV1 {
  const fn new(limits: QueryPlanningLimitsV1) -> Self {
    Self { limits, nodes: 0, field_nodes: 0, total_literal_bytes: 0, definition_bytes: 0 }
  }
}

fn validate_request_collections(request: &QueryPlanningRequestV1<'_>) -> QueryPlanningResultV1<()> {
  if request.sort_fields.len() > QUERY_MAXIMUM_SORT_FIELDS_V1 {
    return Err(invalid_request("query_sort_field_limit", "sort field count exceeds 32"));
  }
  if request.aggregate_fields.len() > QUERY_MAXIMUM_AGGREGATE_FIELDS_V1 {
    return Err(invalid_request("query_aggregate_field_limit", "aggregate field count exceeds 32"));
  }
  if request.group_fields.len() > QUERY_MAXIMUM_GROUP_FIELDS_V1 {
    return Err(invalid_request("query_group_field_limit", "group field count exceeds 32"));
  }
  if request.result_limit == 0 || request.result_limit > request.limits.maximum_returned_documents {
    return Err(invalid_request("query_result_limit", "result limit is zero or exceeds the admitted maximum"));
  }
  Ok(())
}

fn preflight_expression(
  expression: &QueryExpressionV1,
  depth: usize,
  admission: &mut AdmissionStateV1,
  required_fields: &mut BTreeSet<String>,
  hash_algorithm: HashAlgorithm,
) -> QueryPlanningResultV1<()> {
  if depth > admission.limits.maximum_expression_depth {
    return Err(resource_error("query_expression_depth_limit", "query expression exceeds the admitted depth"));
  }
  admission.nodes =
    admission.nodes.checked_add(1).ok_or_else(|| resource_error("query_expression_node_overflow", "node count overflow"))?;
  if admission.nodes > admission.limits.maximum_expression_nodes {
    return Err(resource_error("query_expression_node_limit", "query expression exceeds the admitted node count"));
  }

  match expression {
    QueryExpressionV1::Field(predicate) => {
      admission.field_nodes =
        admission.field_nodes.checked_add(1).ok_or_else(|| resource_error("query_field_node_overflow", "field-node count overflow"))?;
      let field_name = canonical_field_name(&predicate.field_name)?;
      required_fields.insert(field_name.to_string());
      preflight_operation(field_name, &predicate.operation, admission, hash_algorithm)
    }
    QueryExpressionV1::And(children) | QueryExpressionV1::Or(children) => {
      for child in children {
        preflight_expression(child, depth + 1, admission, required_fields, hash_algorithm)?;
      }
      Ok(())
    }
    QueryExpressionV1::Not(child) => preflight_expression(child, depth + 1, admission, required_fields, hash_algorithm),
  }
}

fn preflight_operation(
  field_name: &str,
  operation: &QueryPredicateOperationV1,
  admission: &mut AdmissionStateV1,
  hash_algorithm: HashAlgorithm,
) -> QueryPlanningResultV1<()> {
  match operation {
    QueryPredicateOperationV1::In(values) => {
      if values.len() > admission.limits.maximum_in_literals {
        return Err(resource_error("query_in_literal_limit", "IN literal count exceeds the admitted maximum"));
      }
      for value in values {
        preflight_literal(field_name, value, admission, hash_algorithm)?;
      }
    }
    QueryPredicateOperationV1::Between(left, right) => {
      preflight_literal(field_name, left, admission, hash_algorithm)?;
      preflight_literal(field_name, right, admission, hash_algorithm)?;
    }
    QueryPredicateOperationV1::Similar { value, threshold } => {
      if !threshold.is_finite() || !(0.0..=1.0).contains(threshold) {
        return Err(invalid_request("query_similarity_threshold_invalid", "similarity threshold must be finite and in [0,1]"));
      }
      preflight_literal(field_name, value, admission, hash_algorithm)?;
    }
    QueryPredicateOperationV1::Fuzzy { value, edits, .. } => {
      if edits.is_some_and(|edits| edits > QUERY_MAXIMUM_FUZZY_EDITS_V1) {
        return Err(invalid_request("query_fuzzy_edits_invalid", "explicit fuzzy edits exceed eight"));
      }
      preflight_literal(field_name, value, admission, hash_algorithm)?;
    }
    QueryPredicateOperationV1::Eq(value)
    | QueryPredicateOperationV1::Gt(value)
    | QueryPredicateOperationV1::Lt(value)
    | QueryPredicateOperationV1::Contains(value)
    | QueryPredicateOperationV1::Phonetic(value)
    | QueryPredicateOperationV1::Match(value) => preflight_literal(field_name, value, admission, hash_algorithm)?,
  }
  Ok(())
}

fn preflight_literal(
  field_name: &str,
  value: &CanonicalConfigValueV1,
  admission: &mut AdmissionStateV1,
  hash_algorithm: HashAlgorithm,
) -> QueryPlanningResultV1<()> {
  let encoded = encode_query_literal(field_name, value, hash_algorithm)?;
  admission.total_literal_bytes = admission
    .total_literal_bytes
    .checked_add(encoded.len() as u64)
    .ok_or_else(|| resource_error("query_literal_total_overflow", "query literal bytes overflow"))?;
  if admission.total_literal_bytes > QUERY_MAXIMUM_TOTAL_LITERAL_BYTES_V1 as u64 {
    return Err(resource_error("query_literal_total_limit", "query literal bytes exceed 8 MiB"));
  }
  Ok(())
}

fn encode_query_literal(field_name: &str, value: &CanonicalConfigValueV1, hash_algorithm: HashAlgorithm) -> QueryPlanningResultV1<Vec<u8>> {
  if matches!(value, CanonicalConfigValueV1::String(value) if value.len() > QUERY_MAXIMUM_LITERAL_BYTES_V1)
    || matches!(value, CanonicalConfigValueV1::Bytes(value) if value.len() > QUERY_MAXIMUM_LITERAL_BYTES_V1)
  {
    return Err(resource_error("query_literal_size_limit", "one query literal exceeds 1 MiB"));
  }
  let value = normalize_query_literal(field_name, value, hash_algorithm)?;
  let encoded = encode_canonical_value(&value, CanonicalValueBounds::SOURCE_VALUE).map_err(|source| {
    let class = if source.class() == super::reader::MalformedInputClass::AllocationAmplification {
      QueryPlanningErrorClassV1::ResourceLimit
    } else {
      QueryPlanningErrorClassV1::InvalidRequest
    };
    planning_error(class, "query_literal_invalid", format!("{}: {}", source.code(), source.context()))
  })?;
  if encoded.len() > QUERY_MAXIMUM_LITERAL_BYTES_V1 {
    return Err(resource_error("query_literal_size_limit", "one query literal exceeds 1 MiB"));
  }
  Ok(encoded)
}

fn validate_catalogs<'a>(
  request: &'a QueryPlanningRequestV1<'_>,
  required_fields: &BTreeSet<String>,
  admission: &mut AdmissionStateV1,
) -> QueryPlanningResultV1<BTreeMap<&'a str, &'a RootAwareQueryFieldCatalogV1>> {
  let mut catalogs = BTreeMap::new();
  for catalog in request.catalogs {
    check_cancelled(request.is_cancelled)?;
    let canonical_name = canonical_field_name(&catalog.field_name)?;
    if canonical_name != catalog.field_name {
      return Err(corrupt_source("query_catalog_field_noncanonical", "definition catalog uses a field alias"));
    }
    if !required_fields.contains(canonical_name) {
      return Err(corrupt_source("query_catalog_unrequested_field", "definition catalog contains an unrequested field"));
    }
    if catalogs.insert(canonical_name, catalog).is_some() {
      return Err(corrupt_source("query_catalog_duplicate_field", "definition catalog repeats a field"));
    }
    validate_catalog_identity(request.context, catalog)?;
    if !catalog.complete {
      return Err(corrupt_source("query_catalog_incomplete", "definition catalog did not complete its selected-root scan"));
    }
    if catalog.scopes.len() > request.limits.maximum_scopes_per_field {
      return Err(resource_error("query_scope_count_limit", "field catalog exceeds the admitted scope count"));
    }
    let mut prior_scope: Option<Vec<u8>> = None;
    for scope in &catalog.scopes {
      validate_scope_catalog(request, canonical_name, scope, admission)?;
      if prior_scope.as_deref().is_some_and(|prior| prior >= scope.scope_id.as_slice()) {
        return Err(corrupt_source("query_scope_order", "field scopes are not unique canonical ScopeId order"));
      }
      prior_scope = Some(clone_bytes(&scope.scope_id, "scope identity")?);
    }
  }
  for required in required_fields {
    if !catalogs.contains_key(required.as_str()) {
      return Err(invalid_request(
        "query_definition_catalog_missing",
        format!("selected semantic root has no complete definition catalog for {required}"),
      ));
    }
  }
  Ok(catalogs)
}

fn validate_catalog_identity(context: &QueryPlanningContextV1<'_>, catalog: &RootAwareQueryFieldCatalogV1) -> QueryPlanningResultV1<()> {
  if catalog.database_id != context.database_id()
    || catalog.physical_instance_id != context.physical_instance_id()
    || catalog.selected_namespace_root != context.selected_namespace_root()
    || catalog.semantic_state_root != context.semantic_state_root()
    || catalog.publication_sequence != context.publication_sequence()
  {
    return Err(corrupt_source("query_catalog_root_mismatch", "definition catalog does not belong to the selected planning context"));
  }
  Ok(())
}

fn validate_scope_catalog(
  request: &QueryPlanningRequestV1<'_>,
  field_name: &str,
  scope: &QueryPlanningScopeV1,
  admission: &mut AdmissionStateV1,
) -> QueryPlanningResultV1<()> {
  admission.definition_bytes = checked_definition_add(admission.definition_bytes, scope.encoded_scope_definition.len())?;
  admission.definition_bytes = checked_definition_add(admission.definition_bytes, scope.encoded_value_store_definition.len())?;
  if admission.definition_bytes > request.limits.maximum_definition_bytes {
    return Err(resource_error("query_definition_bytes_limit", "selected definitions exceed the admitted byte budget"));
  }
  let scope_definition = decode_scope_definition(&scope.encoded_scope_definition, request.context.hash_algorithm())
    .map_err(|source| corrupt_source("query_scope_definition_invalid", format!("{}: {}", source.code(), source.context())))?;
  if scope.scope_id != scope_definition.scope_id {
    return Err(corrupt_source("query_scope_identity_mismatch", "catalog ScopeId differs from its exact definition"));
  }
  let value_definition = decode_value_store_definition(&scope.encoded_value_store_definition, request.context.hash_algorithm())
    .map_err(|source| corrupt_source("query_value_definition_invalid", format!("{}: {}", source.code(), source.context())))?;
  if value_definition.value_store_id != scope.value_store_id
    || value_definition.scope_id != scope.scope_id
    || value_definition.field_name != field_name
  {
    return Err(corrupt_source("query_value_definition_mismatch", "ValueStore definition does not belong to the catalog field and scope"));
  }
  if scope.indexes.len() > request.limits.maximum_candidates_per_scope {
    return Err(resource_error("query_index_candidate_limit", "scope exceeds the admitted strategy count"));
  }
  let mut prior_index: Option<Vec<u8>> = None;
  for index in &scope.indexes {
    admission.definition_bytes = checked_definition_add(admission.definition_bytes, index.encoded_field_definition.len())?;
    if admission.definition_bytes > request.limits.maximum_definition_bytes {
      return Err(resource_error("query_definition_bytes_limit", "selected definitions exceed the admitted byte budget"));
    }
    let field_definition = decode_field_index_definition(&index.encoded_field_definition, request.context.hash_algorithm())
      .map_err(|source| corrupt_source("query_field_definition_invalid", format!("{}: {}", source.code(), source.context())))?;
    if field_definition.index_id != index.index_id || field_definition.value_store_id != value_definition.value_store_id {
      return Err(corrupt_source("query_field_definition_mismatch", "FieldIndex definition does not belong to its catalog candidate"));
    }
    if prior_index.as_deref().is_some_and(|prior| prior >= index.index_id.as_slice()) {
      return Err(corrupt_source("query_index_order", "scope candidates are not unique canonical IndexId order"));
    }
    prior_index = Some(clone_bytes(&index.index_id, "index identity")?);
  }
  Ok(())
}

fn compile_expression(
  expression: &QueryExpressionV1,
  request: &QueryPlanningRequestV1<'_>,
  catalogs: &BTreeMap<&str, &RootAwareQueryFieldCatalogV1>,
  predicates: &mut Vec<CompiledQueryPredicatePlanV1>,
) -> QueryPlanningResultV1<CompiledQueryExpressionV1> {
  check_cancelled(request.is_cancelled)?;
  match expression {
    QueryExpressionV1::Field(predicate) => {
      let index = predicates.len();
      predicates.push(compile_predicate(predicate, request, catalogs)?);
      Ok(CompiledQueryExpressionV1::Field(index))
    }
    QueryExpressionV1::And(children) => {
      let mut compiled = Vec::new();
      compiled
        .try_reserve_exact(children.len())
        .map_err(|source| resource_error("query_and_reserve", format!("cannot reserve bounded AND expression: {source}")))?;
      for child in children {
        compiled.push(compile_expression(child, request, catalogs, predicates)?);
      }
      Ok(CompiledQueryExpressionV1::And(compiled))
    }
    QueryExpressionV1::Or(children) => {
      let mut compiled = Vec::new();
      compiled
        .try_reserve_exact(children.len())
        .map_err(|source| resource_error("query_or_reserve", format!("cannot reserve bounded OR expression: {source}")))?;
      for child in children {
        compiled.push(compile_expression(child, request, catalogs, predicates)?);
      }
      Ok(CompiledQueryExpressionV1::Or(compiled))
    }
    QueryExpressionV1::Not(child) => {
      Ok(CompiledQueryExpressionV1::Not(Box::new(compile_expression(child, request, catalogs, predicates)?)))
    }
  }
}

fn compile_predicate(
  predicate: &QueryPredicateV1,
  request: &QueryPlanningRequestV1<'_>,
  catalogs: &BTreeMap<&str, &RootAwareQueryFieldCatalogV1>,
) -> QueryPlanningResultV1<CompiledQueryPredicatePlanV1> {
  let field_name = canonical_field_name(&predicate.field_name)?;
  let operation = normalize_query_operation(field_name, &predicate.operation, request.context.hash_algorithm())?;
  let catalog = catalogs.get(field_name).ok_or_else(|| {
    invalid_request("query_definition_catalog_missing", format!("selected semantic root has no definition catalog for {field_name}"))
  })?;
  let mut scopes = Vec::new();
  scopes
    .try_reserve_exact(catalog.scopes.len())
    .map_err(|source| resource_error("query_scope_plan_reserve", format!("cannot reserve bounded scope plans: {source}")))?;
  for scope in &catalog.scopes {
    check_cancelled(request.is_cancelled)?;
    scopes.push(compile_predicate_scope(field_name, &operation, scope, request)?);
  }
  Ok(CompiledQueryPredicatePlanV1 { field_name: clone_string(field_name, "field name")?, operation, scopes })
}

fn compile_predicate_scope(
  field_name: &str,
  operation: &QueryPredicateOperationV1,
  scope: &QueryPlanningScopeV1,
  request: &QueryPlanningRequestV1<'_>,
) -> QueryPlanningResultV1<CompiledQueryScopePlanV1> {
  if !matches!(scope.semantic_availability, IndexSemanticQueryAvailabilityV1::Complete) {
    return Err(historical_unavailable("selected root does not retain complete semantics for the requested field"));
  }
  let mut candidates = Vec::new();
  let mut supporting_strategy_count = 0usize;
  for index in &scope.indexes {
    let runtime = IndexDefinitionRuntimeV1::from_encoded(
      &scope.encoded_value_store_definition,
      &index.encoded_field_definition,
      request.context.hash_algorithm(),
    )
    .map_err(map_definition_error)?;
    if runtime.field_definition().operations & operation.operation_bit() == 0 {
      continue;
    }
    supporting_strategy_count += 1;
    candidates.push(compile_index_candidate(operation, scope, index, &runtime, request)?);
  }
  if supporting_strategy_count == 0 {
    return Err(invalid_request(
      "query_operation_unsupported",
      format!("no selected definition for {field_name} supports {}", operation.name()),
    ));
  }

  let authoritative_work = authoritative_work(scope.authoritative_document_count);
  let driver = select_driver(operation, &candidates, authoritative_work)?;
  Ok(CompiledQueryScopePlanV1 { scope_id: clone_bytes(&scope.scope_id, "scope identity")?, candidates, driver })
}

fn compile_index_candidate(
  operation: &QueryPredicateOperationV1,
  scope: &QueryPlanningScopeV1,
  index: &QueryPlanningIndexCandidateV1,
  runtime: &IndexDefinitionRuntimeV1<'_, '_>,
  request: &QueryPlanningRequestV1<'_>,
) -> QueryPlanningResultV1<CompiledQueryIndexCandidateV1> {
  let compiled_literals = compile_literals(operation, runtime)?;
  let (coordinate_constraint, value_match, proven_candidate_superset) =
    compile_coordinate_constraint(operation, &compiled_literals, runtime)?;
  let coverage = compile_coverage(scope, index, runtime, request)?;
  let estimated_work =
    if coverage == CompiledQueryCoverageV1::AuthoritativeOnly { u64::MAX } else { index_work(index.estimates, coverage) };
  Ok(CompiledQueryIndexCandidateV1 {
    index_id: clone_bytes(&index.index_id, "index identity")?,
    strategy_name: clone_string(runtime.strategy().name, "strategy name")?,
    selected_generation: retained_selected_generation(index, coverage)?,
    compiled_literals,
    coordinate_constraint,
    value_match,
    coverage,
    estimated_work,
    proven_candidate_superset,
  })
}

fn compile_literals(
  operation: &QueryPredicateOperationV1,
  runtime: &IndexDefinitionRuntimeV1<'_, '_>,
) -> QueryPlanningResultV1<Vec<CompiledQueryLiteralV1>> {
  let values: Vec<&CanonicalConfigValueV1> = match operation {
    QueryPredicateOperationV1::In(values) => values.iter().collect(),
    QueryPredicateOperationV1::Between(left, right) => vec![left, right],
    QueryPredicateOperationV1::Similar { value, .. } | QueryPredicateOperationV1::Fuzzy { value, .. } => vec![value],
    QueryPredicateOperationV1::Eq(value)
    | QueryPredicateOperationV1::Gt(value)
    | QueryPredicateOperationV1::Lt(value)
    | QueryPredicateOperationV1::Contains(value)
    | QueryPredicateOperationV1::Phonetic(value)
    | QueryPredicateOperationV1::Match(value) => vec![value],
  };
  let mut compiled = Vec::new();
  compiled
    .try_reserve_exact(values.len())
    .map_err(|source| resource_error("query_compiled_literal_reserve", format!("cannot reserve compiled literals: {source}")))?;
  for value in values {
    let output = runtime.converter().compile_query_literal(value).map_err(map_semantic_error)?;
    compiled.push(CompiledQueryLiteralV1 { compiled: output });
  }
  if matches!(operation, QueryPredicateOperationV1::In(_)) {
    compiled.sort_by(|left, right| left.compiled.canonical_value.cmp(&right.compiled.canonical_value));
    compiled.dedup_by(|left, right| left.compiled.canonical_value == right.compiled.canonical_value);
  }
  Ok(compiled)
}

fn compile_coordinate_constraint(
  operation: &QueryPredicateOperationV1,
  literals: &[CompiledQueryLiteralV1],
  runtime: &IndexDefinitionRuntimeV1<'_, '_>,
) -> QueryPlanningResultV1<(QueryCoordinateConstraintV1, QueryValueMatchV1, bool)> {
  match operation {
    QueryPredicateOperationV1::Eq(_) | QueryPredicateOperationV1::In(_) => {
      let points = posting_coordinates(literals, |_| true);
      Ok((QueryCoordinateConstraintV1::Points(points), QueryValueMatchV1::AnyPosting, true))
    }
    QueryPredicateOperationV1::Gt(_) => {
      let coordinate = one_posting_coordinate(literals)?;
      Ok((
        QueryCoordinateConstraintV1::InclusiveRange { start: coordinate, end: u64::MAX, widen_start_cell: true, widen_end_cell: false },
        QueryValueMatchV1::OrderedRange,
        true,
      ))
    }
    QueryPredicateOperationV1::Lt(_) => {
      let coordinate = one_posting_coordinate(literals)?;
      Ok((
        QueryCoordinateConstraintV1::InclusiveRange { start: 0, end: coordinate, widen_start_cell: false, widen_end_cell: true },
        QueryValueMatchV1::OrderedRange,
        true,
      ))
    }
    QueryPredicateOperationV1::Between(_, _) => {
      if literals.len() != 2 || literals.iter().any(|literal| literal.compiled.postings.len() != 1) {
        return Err(corrupt_source("query_between_compilation", "ordered BETWEEN did not compile to two scalar postings"));
      }
      let left = &literals[0].compiled.postings[0];
      let right = &literals[1].compiled.postings[0];
      if runtime.converter().compare_posting_keys(&left.posting_key, &right.posting_key).map_err(map_semantic_error)? == Ordering::Greater {
        return Err(invalid_request("query_between_order_invalid", "BETWEEN lower endpoint exceeds upper endpoint"));
      }
      Ok((
        QueryCoordinateConstraintV1::InclusiveRange {
          start: left.coordinate,
          end: right.coordinate,
          widen_start_cell: true,
          widen_end_cell: true,
        },
        QueryValueMatchV1::OrderedRange,
        true,
      ))
    }
    QueryPredicateOperationV1::Contains(_) => {
      let points = posting_coordinates(literals, |key| key.first() == Some(&0x02));
      let complete = !points.is_empty();
      Ok((QueryCoordinateConstraintV1::Points(points), QueryValueMatchV1::AllPostings, complete))
    }
    QueryPredicateOperationV1::Similar { threshold, .. } => {
      let points = posting_coordinates(literals, |key| key.first() == Some(&0x01));
      let complete = *threshold > 0.0 && !points.is_empty();
      Ok((QueryCoordinateConstraintV1::Points(points), QueryValueMatchV1::AnyPosting, complete))
    }
    QueryPredicateOperationV1::Phonetic(_) => {
      let points = posting_coordinates(literals, |_| true);
      // A phonetic converter that emits no query key proves an empty branch;
      // it does not make the other configured phonetic branches incomplete.
      Ok((QueryCoordinateConstraintV1::Points(points), QueryValueMatchV1::AnyPosting, true))
    }
    QueryPredicateOperationV1::Fuzzy { .. } | QueryPredicateOperationV1::Match(_) => {
      Ok((QueryCoordinateConstraintV1::FullScan, QueryValueMatchV1::AuthoritativeRecheck, false))
    }
  }
}

fn posting_coordinates(filter: &[CompiledQueryLiteralV1], include: impl Fn(&[u8]) -> bool) -> Vec<u64> {
  let mut points = filter
    .iter()
    .flat_map(|literal| literal.compiled.postings.iter())
    .filter(|posting| include(&posting.posting_key))
    .map(|posting| posting.coordinate)
    .collect::<Vec<_>>();
  points.sort_unstable();
  points.dedup();
  points
}

fn one_posting_coordinate(literals: &[CompiledQueryLiteralV1]) -> QueryPlanningResultV1<u64> {
  match literals {
    [literal] if literal.compiled.postings.len() == 1 => Ok(literal.compiled.postings[0].coordinate),
    _ => Err(corrupt_source("query_ordered_compilation", "ordered predicate did not compile to one scalar posting")),
  }
}

fn compile_coverage(
  scope: &QueryPlanningScopeV1,
  index: &QueryPlanningIndexCandidateV1,
  runtime: &IndexDefinitionRuntimeV1<'_, '_>,
  request: &QueryPlanningRequestV1<'_>,
) -> QueryPlanningResultV1<CompiledQueryCoverageV1> {
  let definition_fingerprint = field_definition_fingerprint(request.context.hash_algorithm(), &index.encoded_field_definition);
  let dependency_fingerprint = field_dependency_fingerprint(request.context.hash_algorithm(), &scope.scope_id, runtime.value_store_id());
  let generation = index.selected_generation.as_ref().map(QueryPlanningCoverageGenerationV1::as_coverage_generation);
  let plan = plan_selected_index_coverage_v1(&IndexCoveragePlanningRequestV1 {
    hash_algorithm: request.context.hash_algorithm(),
    requested_namespace_root: request.context.selected_namespace_root(),
    requested_publication_sequence: request.context.publication_sequence(),
    required_owner_id: runtime.index_id(),
    required_definition_fingerprint: &definition_fingerprint,
    required_dependency_fingerprint: &dependency_fingerprint,
    semantic_availability: scope.semantic_availability,
    selected_generation: generation,
  })
  .map_err(|source| corrupt_source(source.code(), source.context()))?;
  match plan {
    IndexCoveragePlanV1::Complete { .. } => Ok(CompiledQueryCoverageV1::Complete),
    IndexCoveragePlanV1::PartialCandidate { .. } => Ok(CompiledQueryCoverageV1::PartialExact),
    IndexCoveragePlanV1::AuthoritativeOnly { .. } => Ok(CompiledQueryCoverageV1::AuthoritativeOnly),
    IndexCoveragePlanV1::HistoricalViewUnavailable { reason } => Err(historical_unavailable(match reason {
      IndexHistoricalViewUnavailableReasonV1::ContentOnly(_) => "selected root retains content-only semantics",
      IndexHistoricalViewUnavailableReasonV1::DependencyUnavailable => "selected root semantic dependencies are unavailable",
    })),
  }
}

fn select_driver(
  operation: &QueryPredicateOperationV1,
  candidates: &[CompiledQueryIndexCandidateV1],
  authoritative_work: u64,
) -> QueryPlanningResultV1<QueryPlanDriverV1> {
  if matches!(operation, QueryPredicateOperationV1::Phonetic(_)) {
    let eligible = candidates
      .iter()
      .enumerate()
      .filter(|(_, candidate)| candidate.proven_candidate_superset && candidate.coverage != CompiledQueryCoverageV1::AuthoritativeOnly)
      .collect::<Vec<_>>();
    if eligible.len() == candidates.len() && !eligible.is_empty() {
      let mut estimated_work = 0u64;
      let mut candidate_indexes = Vec::new();
      let mut coverage = CompiledQueryCoverageV1::Complete;
      for (candidate_index, candidate) in eligible {
        estimated_work = estimated_work.saturating_add(candidate.estimated_work);
        candidate_indexes.push(candidate_index);
        if candidate.coverage == CompiledQueryCoverageV1::PartialExact {
          coverage = CompiledQueryCoverageV1::PartialExact;
        }
      }
      if estimated_work < authoritative_work {
        return Ok(QueryPlanDriverV1::IndexUnion { candidate_indexes, coverage, estimated_work });
      }
    }
    return Ok(QueryPlanDriverV1::Authoritative { estimated_work: authoritative_work });
  }

  let mut driver = QueryPlanDriverV1::Authoritative { estimated_work: authoritative_work };
  for (candidate_index, candidate) in candidates.iter().enumerate() {
    if !candidate.proven_candidate_superset || candidate.coverage == CompiledQueryCoverageV1::AuthoritativeOnly {
      continue;
    }
    if candidate.estimated_work < driver.estimated_work() {
      driver = QueryPlanDriverV1::Index { candidate_index, coverage: candidate.coverage, estimated_work: candidate.estimated_work };
    }
  }
  Ok(driver)
}

fn compile_auxiliary_fields(
  request: &QueryPlanningRequestV1<'_>,
  catalogs: &BTreeMap<&str, &RootAwareQueryFieldCatalogV1>,
  output: &mut Vec<CompiledQueryAuxiliaryFieldPlanV1>,
) -> QueryPlanningResultV1<()> {
  let capacity = request
    .sort_fields
    .len()
    .checked_add(request.aggregate_fields.len())
    .and_then(|count| count.checked_add(request.group_fields.len()))
    .ok_or_else(|| resource_error("query_auxiliary_count_overflow", "auxiliary field count overflow"))?;
  output
    .try_reserve_exact(capacity)
    .map_err(|source| resource_error("query_auxiliary_reserve", format!("cannot reserve bounded auxiliary plans: {source}")))?;
  for field in request.sort_fields {
    output.push(compile_auxiliary_field(
      &field.field_name,
      CompiledQueryAuxiliaryOperationV1::Sort(field.direction),
      OPERATION_SORT,
      request,
      catalogs,
    )?);
  }
  for field in request.aggregate_fields {
    output.push(compile_auxiliary_field(
      &field.field_name,
      CompiledQueryAuxiliaryOperationV1::Aggregate(field.kind),
      OPERATION_AGGREGATE,
      request,
      catalogs,
    )?);
  }
  for field in request.group_fields {
    output.push(compile_auxiliary_field(field, CompiledQueryAuxiliaryOperationV1::Group, OPERATION_AGGREGATE, request, catalogs)?);
  }
  Ok(())
}

fn compile_auxiliary_field(
  raw_field_name: &str,
  operation: CompiledQueryAuxiliaryOperationV1,
  operation_bit: u64,
  request: &QueryPlanningRequestV1<'_>,
  catalogs: &BTreeMap<&str, &RootAwareQueryFieldCatalogV1>,
) -> QueryPlanningResultV1<CompiledQueryAuxiliaryFieldPlanV1> {
  let field_name = canonical_field_name(raw_field_name)?;
  let catalog = catalogs.get(field_name).ok_or_else(|| {
    invalid_request("query_definition_catalog_missing", format!("selected semantic root has no definition catalog for {field_name}"))
  })?;
  let mut scopes = Vec::new();
  scopes
    .try_reserve_exact(catalog.scopes.len())
    .map_err(|source| resource_error("query_auxiliary_scope_reserve", format!("cannot reserve auxiliary scopes: {source}")))?;
  for scope in &catalog.scopes {
    if !matches!(scope.semantic_availability, IndexSemanticQueryAvailabilityV1::Complete) {
      return Err(historical_unavailable("selected root lacks complete sort or aggregate semantics"));
    }
    let authoritative = authoritative_work(scope.authoritative_document_count);
    let mut selected = QueryPlanDriverV1::Authoritative { estimated_work: authoritative };
    let mut candidates = Vec::new();
    candidates
      .try_reserve_exact(scope.indexes.len())
      .map_err(|source| resource_error("query_auxiliary_candidate_reserve", format!("cannot reserve auxiliary candidates: {source}")))?;
    for index in &scope.indexes {
      let runtime = IndexDefinitionRuntimeV1::from_encoded(
        &scope.encoded_value_store_definition,
        &index.encoded_field_definition,
        request.context.hash_algorithm(),
      )
      .map_err(map_definition_error)?;
      if runtime.field_definition().operations & operation_bit == 0 {
        continue;
      }
      let coverage = compile_coverage(scope, index, &runtime, request)?;
      let estimated_work =
        if coverage == CompiledQueryCoverageV1::AuthoritativeOnly { u64::MAX } else { index_work(index.estimates, coverage) };
      let candidate_index = candidates.len();
      candidates.push(CompiledQueryAuxiliaryIndexCandidateV1 {
        index_id: clone_bytes(&index.index_id, "auxiliary index identity")?,
        strategy_name: clone_string(runtime.strategy().name, "auxiliary strategy name")?,
        selected_generation: retained_selected_generation(index, coverage)?,
        coverage,
        estimated_work,
      });
      if coverage != CompiledQueryCoverageV1::AuthoritativeOnly && estimated_work < selected.estimated_work() {
        selected = QueryPlanDriverV1::Index { candidate_index, coverage, estimated_work };
      }
    }
    if candidates.is_empty() {
      return Err(invalid_request(
        "query_operation_unsupported",
        format!("no selected definition for {field_name} supports the requested auxiliary operation"),
      ));
    }
    scopes.push(CompiledQueryAuxiliaryScopePlanV1 {
      scope_id: clone_bytes(&scope.scope_id, "auxiliary scope identity")?,
      candidates,
      driver: selected,
    });
  }
  Ok(CompiledQueryAuxiliaryFieldPlanV1 { field_name: clone_string(field_name, "auxiliary field")?, operation, scopes })
}

fn retained_selected_generation(
  index: &QueryPlanningIndexCandidateV1,
  coverage: CompiledQueryCoverageV1,
) -> QueryPlanningResultV1<Option<QueryPlanningCoverageGenerationV1>> {
  if coverage == CompiledQueryCoverageV1::AuthoritativeOnly {
    return Ok(None);
  }
  index
    .selected_generation
    .clone()
    .map(Some)
    .ok_or_else(|| corrupt_source("query_selected_generation_missing", "usable index coverage has no selected generation"))
}

fn normalize_query_operation(
  field_name: &str,
  operation: &QueryPredicateOperationV1,
  hash_algorithm: HashAlgorithm,
) -> QueryPlanningResultV1<QueryPredicateOperationV1> {
  Ok(match operation {
    QueryPredicateOperationV1::Eq(value) => QueryPredicateOperationV1::Eq(normalize_query_literal(field_name, value, hash_algorithm)?),
    QueryPredicateOperationV1::In(values) => {
      let mut normalized = Vec::new();
      normalized
        .try_reserve_exact(values.len())
        .map_err(|source| resource_error("query_normalized_literal_reserve", format!("cannot reserve normalized IN literals: {source}")))?;
      for value in values {
        normalized.push(normalize_query_literal(field_name, value, hash_algorithm)?);
      }
      QueryPredicateOperationV1::In(normalized)
    }
    QueryPredicateOperationV1::Gt(value) => QueryPredicateOperationV1::Gt(normalize_query_literal(field_name, value, hash_algorithm)?),
    QueryPredicateOperationV1::Lt(value) => QueryPredicateOperationV1::Lt(normalize_query_literal(field_name, value, hash_algorithm)?),
    QueryPredicateOperationV1::Between(left, right) => QueryPredicateOperationV1::Between(
      normalize_query_literal(field_name, left, hash_algorithm)?,
      normalize_query_literal(field_name, right, hash_algorithm)?,
    ),
    QueryPredicateOperationV1::Contains(value) => {
      QueryPredicateOperationV1::Contains(normalize_query_literal(field_name, value, hash_algorithm)?)
    }
    QueryPredicateOperationV1::Similar { value, threshold } => {
      QueryPredicateOperationV1::Similar { value: normalize_query_literal(field_name, value, hash_algorithm)?, threshold: *threshold }
    }
    QueryPredicateOperationV1::Phonetic(value) => {
      QueryPredicateOperationV1::Phonetic(normalize_query_literal(field_name, value, hash_algorithm)?)
    }
    QueryPredicateOperationV1::Fuzzy { value, algorithm, edits } => QueryPredicateOperationV1::Fuzzy {
      value: normalize_query_literal(field_name, value, hash_algorithm)?,
      algorithm: *algorithm,
      edits: *edits,
    },
    QueryPredicateOperationV1::Match(value) => {
      QueryPredicateOperationV1::Match(normalize_query_literal(field_name, value, hash_algorithm)?)
    }
  })
}

fn normalize_query_literal(
  field_name: &str,
  value: &CanonicalConfigValueV1,
  hash_algorithm: HashAlgorithm,
) -> QueryPlanningResultV1<CanonicalConfigValueV1> {
  if field_name != "@hash" {
    return Ok(value.clone());
  }
  let CanonicalConfigValueV1::String(text) = value else {
    return Err(invalid_request("query_hash_literal_invalid", "@hash requires a hexadecimal string literal"));
  };
  let expected_length = hash_algorithm
    .hash_length()
    .checked_mul(2)
    .ok_or_else(|| resource_error("query_hash_literal_length_overflow", "hash literal width overflow"))?;
  if text.len() != expected_length || !text.as_bytes().iter().all(u8::is_ascii_hexdigit) {
    return Err(invalid_request("query_hash_literal_invalid", format!("@hash requires exactly {expected_length} hexadecimal characters")));
  }
  let decoded =
    hex::decode(text).map_err(|source| invalid_request("query_hash_literal_invalid", format!("cannot decode @hash: {source}")))?;
  Ok(CanonicalConfigValueV1::Bytes(decoded))
}

fn canonical_field_name(field_name: &str) -> QueryPlanningResultV1<&str> {
  canonical_query_field_name_v1(field_name)
}

pub fn canonical_query_field_name_v1(field_name: &str) -> QueryPlanningResultV1<&str> {
  if field_name.len() > QUERY_MAXIMUM_FIELD_NAME_BYTES_V1 {
    return Err(resource_error("query_field_name_limit", "query field name exceeds 4 KiB"));
  }
  if field_name.is_empty() || field_name.trim() != field_name || field_name.as_bytes().contains(&0) {
    return Err(invalid_request("query_field_name_invalid", "query field name is empty or noncanonical"));
  }
  Ok(if field_name == "@file_name" { "@filename" } else { field_name })
}

fn validate_query_path(path: &str) -> QueryPlanningResultV1<()> {
  if path.len() > QUERY_MAXIMUM_PATH_BYTES_V1 {
    return Err(resource_error("query_path_limit", "query path exceeds 65,535 bytes"));
  }
  if path.is_empty() || path.as_bytes().contains(&0) || normalize_path(path) != path {
    return Err(invalid_request("query_path_invalid", "query path is not canonical"));
  }
  Ok(())
}

fn index_work(estimates: QueryPlanningIndexEstimatesV1, coverage: CompiledQueryCoverageV1) -> u64 {
  let pages = estimates.page_count.saturating_mul(PAGE_WORK_WEIGHT);
  let posting_scan_count = estimates.posting_count.min(estimates.estimated_candidate_count.saturating_mul(4));
  let cardinality_count = estimates.distinct_document_count.min(estimates.estimated_candidate_count);
  let postings = posting_scan_count.saturating_mul(POSTING_WORK_WEIGHT);
  let cardinality = cardinality_count.saturating_mul(CARDINALITY_WORK_WEIGHT);
  let mut work = pages.saturating_add(postings).saturating_add(cardinality);
  if coverage == CompiledQueryCoverageV1::PartialExact {
    work = work.saturating_add(estimates.authoritative_fallback_document_count.saturating_mul(AUTHORITATIVE_WORK_WEIGHT));
  }
  work.saturating_add(1)
}

fn authoritative_work(document_count: u64) -> u64 {
  document_count.saturating_mul(AUTHORITATIVE_WORK_WEIGHT).saturating_add(1)
}

fn summarize_drivers<'a>(
  drivers: impl Iterator<Item = &'a QueryPlanDriverV1>,
) -> (QueryLogicalDriverKindV1, CompiledQueryCoverageV1, QueryLogicalWorkClassV1) {
  let mut kind = QueryLogicalDriverKindV1::Index;
  let mut coverage = CompiledQueryCoverageV1::Complete;
  let mut work = 0u64;
  for driver in drivers {
    work = work.saturating_add(driver.estimated_work());
    match driver {
      QueryPlanDriverV1::Authoritative { .. } => {
        kind = QueryLogicalDriverKindV1::Authoritative;
        coverage = CompiledQueryCoverageV1::AuthoritativeOnly;
      }
      QueryPlanDriverV1::Index { coverage: driver_coverage, .. } => {
        if kind != QueryLogicalDriverKindV1::Authoritative && *driver_coverage == CompiledQueryCoverageV1::PartialExact {
          coverage = CompiledQueryCoverageV1::PartialExact;
        }
      }
      QueryPlanDriverV1::IndexUnion { coverage: driver_coverage, .. } => {
        if kind != QueryLogicalDriverKindV1::Authoritative {
          kind = QueryLogicalDriverKindV1::IndexUnion;
          if *driver_coverage == CompiledQueryCoverageV1::PartialExact {
            coverage = CompiledQueryCoverageV1::PartialExact;
          }
        }
      }
    }
  }
  let work = match work {
    0..=64 => QueryLogicalWorkClassV1::Minimal,
    65..=1_024 => QueryLogicalWorkClassV1::Low,
    1_025..=65_536 => QueryLogicalWorkClassV1::Moderate,
    65_537..=1_048_576 => QueryLogicalWorkClassV1::High,
    _ => QueryLogicalWorkClassV1::Extensive,
  };
  (kind, coverage, work)
}

fn checked_definition_add(current: u64, length: usize) -> QueryPlanningResultV1<u64> {
  current
    .checked_add(length as u64)
    .ok_or_else(|| resource_error("query_definition_bytes_overflow", "selected definition byte count overflow"))
}

fn clone_bytes(value: &[u8], label: &'static str) -> QueryPlanningResultV1<Vec<u8>> {
  let mut copy = Vec::new();
  copy
    .try_reserve_exact(value.len())
    .map_err(|source| resource_error("query_value_reserve", format!("cannot reserve {label}: {source}")))?;
  copy.extend_from_slice(value);
  Ok(copy)
}

fn clone_string(value: &str, label: &'static str) -> QueryPlanningResultV1<String> {
  let mut copy = String::new();
  copy
    .try_reserve_exact(value.len())
    .map_err(|source| resource_error("query_value_reserve", format!("cannot reserve {label}: {source}")))?;
  copy.push_str(value);
  Ok(copy)
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> QueryPlanningResultV1<()> {
  if is_cancelled() {
    return Err(planning_error(QueryPlanningErrorClassV1::Cancelled, "query_planning_cancelled", "query planning was cancelled"));
  }
  Ok(())
}

fn validate_nonzero_hash(value: &[u8], width: usize, label: &'static str) -> QueryPlanningResultV1<()> {
  if value.len() != width || value.iter().all(|byte| *byte == 0) {
    return Err(invalid_request("query_planning_context_invalid", format!("{label} has the wrong width or is all zero")));
  }
  Ok(())
}

fn map_definition_error(source: super::index_definition_runtime::IndexDefinitionErrorV1) -> QueryPlanningErrorV1 {
  match source.class() {
    IndexDefinitionErrorClassV1::ResourceLimit => resource_error(source.code(), source.context()),
    IndexDefinitionErrorClassV1::InvalidSourceValue => invalid_request(source.code(), source.context()),
    IndexDefinitionErrorClassV1::IdentityMismatch
    | IndexDefinitionErrorClassV1::SemanticMismatch
    | IndexDefinitionErrorClassV1::UnsupportedDefinition => corrupt_source(source.code(), source.context()),
  }
}

fn map_semantic_error(source: super::index_converter::IndexSemanticErrorV1) -> QueryPlanningErrorV1 {
  match source.class() {
    IndexSemanticErrorClassV1::ResourceLimit => resource_error(source.code(), source.context()),
    IndexSemanticErrorClassV1::InvalidSourceValue => invalid_request(source.code(), source.context()),
    IndexSemanticErrorClassV1::UnsupportedDefinition | IndexSemanticErrorClassV1::MalformedPostingKey => {
      corrupt_source(source.code(), source.context())
    }
  }
}

fn invalid_request(code: &'static str, context: impl Into<String>) -> QueryPlanningErrorV1 {
  planning_error(QueryPlanningErrorClassV1::InvalidRequest, code, context)
}

fn resource_error(code: &'static str, context: impl Into<String>) -> QueryPlanningErrorV1 {
  planning_error(QueryPlanningErrorClassV1::ResourceLimit, code, context)
}

fn corrupt_source(code: &'static str, context: impl Into<String>) -> QueryPlanningErrorV1 {
  planning_error(QueryPlanningErrorClassV1::CorruptSource, code, context)
}

fn historical_unavailable(context: impl Into<String>) -> QueryPlanningErrorV1 {
  planning_error(QueryPlanningErrorClassV1::HistoricalViewUnavailable, "historical_view_unavailable", context)
}

fn planning_error(class: QueryPlanningErrorClassV1, code: &'static str, context: impl Into<String>) -> QueryPlanningErrorV1 {
  QueryPlanningErrorV1 { class, code, context: context.into() }
}
