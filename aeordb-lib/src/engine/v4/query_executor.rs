//! Bounded selected-root query execution.
//!
//! This module owns the authoritative semantic truth path. Immutable index
//! candidates may narrow later execution, but every retained document is
//! evaluated through the same compiled selected-root definitions before it can
//! become observable.

use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::mem::size_of;

use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::fuzzy::{auto_fuzziness, damerau_levenshtein_controlled, jaro_winkler_controlled};
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, decode_canonical_value};
use super::index_definition_runtime::{IndexDefinitionErrorClassV1, IndexDefinitionRuntimeV1};
use super::index_semantic_registry::OPERATION_PHONETIC;
use super::query_planner::{
  CompiledQueryExpressionV1, CompiledQueryPredicatePlanV1, CompiledRootAwareQueryPlanV1, QUERY_MAXIMUM_PATH_BYTES_V1,
  QueryFuzzyAlgorithmV1, QueryPredicateOperationV1, QueryValueMatchV1, RootAwareQueryFieldCatalogV1,
};
use super::text_fold::fold_characters;

const EXECUTION_FIXED_RETAINED_BYTES: u64 = 4 * 1_024;
const EXECUTION_FIXED_SEMANTIC_BYTES: u64 = 64 * 1_024;
const EXECUTION_PER_DOCUMENT_SEMANTIC_BYTES: u64 = 16 * 1_024;
const RESULT_ROW_FIXED_BYTES: u64 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryExecutionCountLimitsV1 {
  maximum_documents: u64,
  maximum_field_values: u64,
  maximum_matches: u64,
  maximum_work_steps: u64,
}

impl QueryExecutionCountLimitsV1 {
  pub fn new(
    maximum_documents: u64,
    maximum_field_values: u64,
    maximum_matches: u64,
    maximum_work_steps: u64,
  ) -> Result<Self, QueryExecutionErrorV1> {
    if maximum_documents == 0 || maximum_field_values == 0 || maximum_matches == 0 || maximum_work_steps == 0 {
      return Err(QueryExecutionErrorV1::invalid(
        "query_execution_count_limits",
        "query document, field-value, match, and work limits must be nonzero",
      ));
    }
    Ok(Self { maximum_documents, maximum_field_values, maximum_matches, maximum_work_steps })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryExecutionByteLimitsV1 {
  maximum_canonical_value_bytes: u64,
  maximum_retained_bytes: u64,
  maximum_semantic_scratch_bytes: u64,
}

impl QueryExecutionByteLimitsV1 {
  pub fn new(
    maximum_canonical_value_bytes: u64,
    maximum_retained_bytes: u64,
    maximum_semantic_scratch_bytes: u64,
  ) -> Result<Self, QueryExecutionErrorV1> {
    if maximum_canonical_value_bytes == 0 || maximum_retained_bytes == 0 || maximum_semantic_scratch_bytes == 0 {
      return Err(QueryExecutionErrorV1::invalid(
        "query_execution_byte_limits",
        "query canonical-value, retained, and semantic-scratch limits must be nonzero",
      ));
    }
    Ok(Self { maximum_canonical_value_bytes, maximum_retained_bytes, maximum_semantic_scratch_bytes })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryExecutionLimitsV1 {
  counts: QueryExecutionCountLimitsV1,
  bytes: QueryExecutionByteLimitsV1,
}

impl QueryExecutionLimitsV1 {
  pub const fn new(counts: QueryExecutionCountLimitsV1, bytes: QueryExecutionByteLimitsV1) -> Self {
    Self { counts, bytes }
  }
}

#[derive(Clone, Copy, Debug)]
pub struct QueryExecutionDocumentV1<'a> {
  pub file_key: &'a [u8],
  pub record_revision: &'a [u8],
  pub path: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryExecutionFieldStateV1 {
  Missing,
  Values,
  DeterministicUnindexable,
}

#[derive(Clone, Copy)]
pub struct QueryExecutionFieldReadRequestV1<'a> {
  pub selected_namespace_root: &'a [u8],
  pub scope_id: &'a [u8],
  pub file_key: &'a [u8],
  pub record_revision: &'a [u8],
  pub field_name: &'a str,
  pub maximum_values: u64,
  pub maximum_canonical_value_bytes: u64,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryExecutionFieldReadReceiptV1 {
  pub selected_namespace_root: Vec<u8>,
  pub scope_id: Vec<u8>,
  pub file_key: Vec<u8>,
  pub record_revision: Vec<u8>,
  pub field_name: String,
  pub state: QueryExecutionFieldStateV1,
  pub value_count: u64,
  pub canonical_value_bytes: u64,
  pub complete: bool,
}

pub trait QueryAuthoritativeValueVisitorV1 {
  fn visit(&mut self, canonical_value: &[u8]) -> Result<(), QueryExecutionErrorV1>;
}

pub trait QueryAuthoritativeFieldSourceV1 {
  fn scan_field_values(
    &mut self,
    request: QueryExecutionFieldReadRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeValueVisitorV1,
  ) -> Result<QueryExecutionFieldReadReceiptV1, QueryExecutionScanErrorV1>;
}

pub trait QueryAuthoritativeDocumentVisitorV1 {
  fn visit(
    &mut self,
    document: QueryExecutionDocumentV1<'_>,
    fields: &mut dyn QueryAuthoritativeFieldSourceV1,
  ) -> Result<(), QueryExecutionErrorV1>;
}

#[derive(Clone, Copy)]
pub struct QueryExecutionScopeScanRequestV1<'a> {
  pub selected_namespace_root: &'a [u8],
  pub publication_sequence: u64,
  pub query_path: &'a str,
  pub scope_id: &'a [u8],
  pub maximum_documents: u64,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryExecutionScopeScanReceiptV1 {
  pub selected_namespace_root: Vec<u8>,
  pub publication_sequence: u64,
  pub scope_id: Vec<u8>,
  pub document_count: u64,
  pub complete: bool,
}

pub trait QueryAuthoritativeScopeSourceV1 {
  /// Stream one already-resolved disjoint effective scope in strict FileKey
  /// order. The source owns all transient row and value memory until each
  /// callback returns and must return a complete receipt.
  fn scan_scope(
    &mut self,
    request: QueryExecutionScopeScanRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryExecutionScopeScanReceiptV1, QueryExecutionScanErrorV1>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryExecutionSourceErrorClassV1 {
  Unavailable,
  ResourceLimit,
  Corrupt,
  Cancelled,
  Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryExecutionSourceErrorV1 {
  class: QueryExecutionSourceErrorClassV1,
  code: &'static str,
  context: String,
}

impl QueryExecutionSourceErrorV1 {
  pub fn new(class: QueryExecutionSourceErrorClassV1, code: &'static str, context: impl Into<String>) -> Self {
    Self { class, code, context: context.into() }
  }

  pub const fn class(&self) -> QueryExecutionSourceErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for QueryExecutionSourceErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for QueryExecutionSourceErrorV1 {}

#[derive(Debug)]
pub enum QueryExecutionScanErrorV1 {
  Source(QueryExecutionSourceErrorV1),
  Visitor(QueryExecutionErrorV1),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryExecutionErrorClassV1 {
  InvalidRequest,
  ResourceLimit,
  HistoricalViewUnavailable,
  CorruptSource,
  Cancelled,
  Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryExecutionErrorV1 {
  class: QueryExecutionErrorClassV1,
  code: &'static str,
  context: String,
}

impl QueryExecutionErrorV1 {
  pub const fn class(&self) -> QueryExecutionErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }

  fn invalid(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryExecutionErrorClassV1::InvalidRequest, code, context: context.into() }
  }

  fn resource(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryExecutionErrorClassV1::ResourceLimit, code, context: context.into() }
  }

  fn unavailable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryExecutionErrorClassV1::HistoricalViewUnavailable, code, context: context.into() }
  }

  fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryExecutionErrorClassV1::CorruptSource, code, context: context.into() }
  }

  fn cancelled() -> Self {
    Self {
      class: QueryExecutionErrorClassV1::Cancelled,
      code: "query_execution_cancelled",
      context: "selected-root query execution was cancelled".to_string(),
    }
  }

  fn internal(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryExecutionErrorClassV1::Internal, code, context: context.into() }
  }
}

impl fmt::Display for QueryExecutionErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for QueryExecutionErrorV1 {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryExecutionMatchV1 {
  file_key: Vec<u8>,
  record_revision: Vec<u8>,
  path: String,
}

impl QueryExecutionMatchV1 {
  pub fn file_key(&self) -> &[u8] {
    &self.file_key
  }

  pub fn record_revision(&self) -> &[u8] {
    &self.record_revision
  }

  pub fn path(&self) -> &str {
    &self.path
  }
}

pub struct RootAwareQueryExecutionV1 {
  selected_namespace_root: Vec<u8>,
  matches: Vec<QueryExecutionMatchV1>,
  examined_documents: u64,
  examined_field_values: u64,
  retained_bytes: u64,
  _memory: MemoryReservation,
}

impl fmt::Debug for RootAwareQueryExecutionV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("RootAwareQueryExecutionV1")
      .field("selected_namespace_root", &hex::encode(&self.selected_namespace_root))
      .field("matches", &self.matches)
      .field("examined_documents", &self.examined_documents)
      .field("examined_field_values", &self.examined_field_values)
      .field("retained_bytes", &self.retained_bytes)
      .finish_non_exhaustive()
  }
}

impl RootAwareQueryExecutionV1 {
  pub fn selected_namespace_root(&self) -> &[u8] {
    &self.selected_namespace_root
  }

  pub fn matches(&self) -> &[QueryExecutionMatchV1] {
    &self.matches
  }

  pub const fn examined_documents(&self) -> u64 {
    self.examined_documents
  }

  pub const fn examined_field_values(&self) -> u64 {
    self.examined_field_values
  }

  pub const fn retained_bytes(&self) -> u64 {
    self.retained_bytes
  }
}

pub struct RootAwareQueryExecutionRequestV1<'a> {
  pub plan: &'a CompiledRootAwareQueryPlanV1,
  pub catalogs: &'a [RootAwareQueryFieldCatalogV1],
  pub source: &'a mut dyn QueryAuthoritativeScopeSourceV1,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub limits: QueryExecutionLimitsV1,
}

pub struct RootAwareQueryScopeExecutionRequestV1<'a> {
  pub plan: &'a CompiledRootAwareQueryPlanV1,
  pub catalogs: &'a [RootAwareQueryFieldCatalogV1],
  pub scope_id: &'a [u8],
  pub source: &'a mut dyn QueryAuthoritativeScopeSourceV1,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub limits: QueryExecutionLimitsV1,
}

struct PreparedCandidateV1<'value, 'field> {
  runtime: IndexDefinitionRuntimeV1<'value, 'field>,
}

struct PreparedPredicateCandidateV1<'plan> {
  candidate_index: usize,
  literals: &'plan [super::query_planner::CompiledQueryLiteralV1],
  value_match: QueryValueMatchV1,
}

struct PreparedPredicateV1<'plan> {
  predicate_index: usize,
  operation: &'plan QueryPredicateOperationV1,
  folded_query_text: Option<String>,
  candidates: Vec<PreparedPredicateCandidateV1<'plan>>,
}

struct PreparedFieldV1<'plan, 'value, 'field> {
  field_name: &'plan str,
  candidates: Vec<PreparedCandidateV1<'value, 'field>>,
  predicates: Vec<PreparedPredicateV1<'plan>>,
}

struct PreparedScopeV1<'plan, 'value, 'field> {
  scope_id: Vec<u8>,
  fields: Vec<PreparedFieldV1<'plan, 'value, 'field>>,
}

struct PreparedExecutionV1<'plan, 'value, 'field> {
  scopes: Vec<PreparedScopeV1<'plan, 'value, 'field>>,
  retained_bytes: u64,
}

struct WorkBudgetV1<'a> {
  work: u64,
  limit: u64,
  cancellation: &'a CancellationToken,
}

#[derive(Clone, Copy)]
enum SimilarityThresholdV1 {
  Inclusive(f64),
  Exclusive(f64),
}

impl SimilarityThresholdV1 {
  fn accepts(self, similarity: f64) -> bool {
    match self {
      Self::Inclusive(threshold) => similarity >= threshold,
      Self::Exclusive(threshold) => similarity > threshold,
    }
  }
}

impl WorkBudgetV1<'_> {
  fn charge(&mut self, amount: u64) -> Result<(), QueryExecutionErrorV1> {
    if self.cancellation.is_cancelled() {
      return Err(QueryExecutionErrorV1::cancelled());
    }
    self.work = self
      .work
      .checked_add(amount)
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_work_overflow", "query work counter overflowed"))?;
    if self.work > self.limit {
      return Err(QueryExecutionErrorV1::resource("query_execution_work_limit", "query execution exceeded its admitted work-step limit"));
    }
    Ok(())
  }
}

pub fn execute_authoritative_root_query_v1(
  request: RootAwareQueryExecutionRequestV1<'_>,
) -> Result<RootAwareQueryExecutionV1, QueryExecutionErrorV1> {
  execute_authoritative_query_v1(request, None)
}

pub fn execute_authoritative_scope_query_v1(
  request: RootAwareQueryScopeExecutionRequestV1<'_>,
) -> Result<RootAwareQueryExecutionV1, QueryExecutionErrorV1> {
  let scope_id = request.scope_id;
  execute_authoritative_query_v1(
    RootAwareQueryExecutionRequestV1 {
      plan: request.plan,
      catalogs: request.catalogs,
      source: request.source,
      memory: request.memory,
      cancellation: request.cancellation,
      limits: request.limits,
    },
    Some(scope_id),
  )
}

fn execute_authoritative_query_v1(
  request: RootAwareQueryExecutionRequestV1<'_>,
  scope_id: Option<&[u8]>,
) -> Result<RootAwareQueryExecutionV1, QueryExecutionErrorV1> {
  require_not_cancelled(request.cancellation)?;
  validate_execution_request(&request)?;
  validate_scope_selection(request.plan, scope_id)?;
  let mut result_memory =
    reserve_query_memory(request.memory, request.limits.bytes.maximum_retained_bytes, "query_execution_result_memory")?;
  let _workspace =
    reserve_query_memory(request.memory, request.limits.bytes.maximum_semantic_scratch_bytes, "query_execution_semantic_memory")?;
  let mut work = WorkBudgetV1 { work: 0, limit: request.limits.counts.maximum_work_steps, cancellation: request.cancellation };
  let prepared = prepare_scopes(
    request.plan,
    request.catalogs,
    request.plan.hash_algorithm(),
    request.limits.bytes.maximum_semantic_scratch_bytes,
    &mut work,
    scope_id,
  )?;
  let semantic_dynamic_bytes = request
    .limits
    .bytes
    .maximum_semantic_scratch_bytes
    .checked_sub(prepared.retained_bytes)
    .and_then(|bytes| bytes.checked_sub(EXECUTION_PER_DOCUMENT_SEMANTIC_BYTES))
    .ok_or_else(|| {
      QueryExecutionErrorV1::resource(
        "query_execution_semantic_memory",
        "prepared semantics and per-document state exceed the admitted semantic workspace",
      )
    })?;
  require_not_cancelled(request.cancellation)?;

  let match_capacity = usize::try_from(request.limits.counts.maximum_matches)
    .map_err(|source| QueryExecutionErrorV1::resource("query_execution_match_limit", source.to_string()))?;
  let minimum_result_capacity = result_base_retained_bytes(request.plan.selected_namespace_root(), match_capacity)?;
  if minimum_result_capacity > request.limits.bytes.maximum_retained_bytes {
    return Err(QueryExecutionErrorV1::resource(
      "query_execution_result_capacity",
      "match capacity cannot fit the retained-result byte limit",
    ));
  }
  let mut matches = Vec::new();
  matches.try_reserve_exact(match_capacity).map_err(|source| {
    QueryExecutionErrorV1::resource("query_execution_result_allocation", format!("cannot reserve bounded query matches: {source}"))
  })?;
  let mut retained_result_bytes = result_base_retained_bytes(request.plan.selected_namespace_root(), matches.capacity())?;
  if retained_result_bytes > request.limits.bytes.maximum_retained_bytes {
    return Err(QueryExecutionErrorV1::resource(
      "query_execution_result_capacity",
      "allocated match capacity cannot fit the retained-result byte limit",
    ));
  }
  let mut examined_documents = 0u64;
  let mut examined_field_values = 0u64;

  for scope in &prepared.scopes {
    work.charge(1)?;
    let mut visitor = ScopeVisitorV1 {
      hash_algorithm: request.plan.hash_algorithm(),
      selected_root: request.plan.selected_namespace_root(),
      query_path: request.plan.query_path(),
      expression: request.plan.expression(),
      predicate_count: request.plan.predicates().len(),
      scope,
      limits: request.limits,
      semantic_dynamic_bytes,
      matches: &mut matches,
      retained_result_bytes: &mut retained_result_bytes,
      examined_documents: &mut examined_documents,
      examined_field_values: &mut examined_field_values,
      work: &mut work,
      scope_documents: 0,
      prior_file_key: None,
      failure: None,
    };
    let scan = request.source.scan_scope(
      QueryExecutionScopeScanRequestV1 {
        selected_namespace_root: request.plan.selected_namespace_root(),
        publication_sequence: request.plan.publication_sequence(),
        query_path: request.plan.query_path(),
        scope_id: &scope.scope_id,
        maximum_documents: request.limits.counts.maximum_documents,
        cancellation: request.cancellation,
      },
      &mut visitor,
    );
    if let Some(error) = visitor.failure.take() {
      return Err(error);
    }
    let receipt = scan.map_err(map_scan_error)?;
    validate_scope_receipt(request.plan, scope, visitor.scope_documents, &receipt)?;
  }
  require_not_cancelled(request.cancellation)?;

  let selected_namespace_root = try_clone_bytes(request.plan.selected_namespace_root(), "selected namespace root")?;
  let retained_bytes = result_retained_bytes(&selected_namespace_root, &matches, matches.capacity())?;
  if retained_bytes != retained_result_bytes || retained_bytes > request.limits.bytes.maximum_retained_bytes {
    return Err(QueryExecutionErrorV1::resource(
      "query_execution_retained_bytes",
      "query execution result exceeds its retained-byte limit or disagrees with incremental accounting",
    ));
  }
  shrink_reservation(&mut result_memory, retained_bytes)?;
  Ok(RootAwareQueryExecutionV1 {
    selected_namespace_root,
    matches,
    examined_documents,
    examined_field_values,
    retained_bytes,
    _memory: result_memory,
  })
}

struct ScopeVisitorV1<'authority, 'plan, 'value, 'field, 'scope, 'state, 'budget, 'cancellation> {
  hash_algorithm: HashAlgorithm,
  selected_root: &'authority [u8],
  query_path: &'authority str,
  expression: &'plan CompiledQueryExpressionV1,
  predicate_count: usize,
  scope: &'scope PreparedScopeV1<'plan, 'value, 'field>,
  limits: QueryExecutionLimitsV1,
  semantic_dynamic_bytes: u64,
  matches: &'state mut Vec<QueryExecutionMatchV1>,
  retained_result_bytes: &'state mut u64,
  examined_documents: &'state mut u64,
  examined_field_values: &'state mut u64,
  work: &'budget mut WorkBudgetV1<'cancellation>,
  scope_documents: u64,
  prior_file_key: Option<Vec<u8>>,
  failure: Option<QueryExecutionErrorV1>,
}

impl QueryAuthoritativeDocumentVisitorV1 for ScopeVisitorV1<'_, '_, '_, '_, '_, '_, '_, '_> {
  fn visit(
    &mut self,
    document: QueryExecutionDocumentV1<'_>,
    fields: &mut dyn QueryAuthoritativeFieldSourceV1,
  ) -> Result<(), QueryExecutionErrorV1> {
    if let Some(error) = &self.failure {
      return Err(error.clone());
    }
    let result = self.visit_document(document, fields);
    if let Err(error) = &result {
      self.failure = Some(error.clone());
    }
    result
  }
}

impl ScopeVisitorV1<'_, '_, '_, '_, '_, '_, '_, '_> {
  fn visit_document(
    &mut self,
    document: QueryExecutionDocumentV1<'_>,
    fields: &mut dyn QueryAuthoritativeFieldSourceV1,
  ) -> Result<(), QueryExecutionErrorV1> {
    self.work.charge(1)?;
    validate_document(self.hash_algorithm, self.query_path, document, self.prior_file_key.as_deref())?;
    self.scope_documents = self
      .scope_documents
      .checked_add(1)
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_document_count", "scope document count overflowed"))?;
    *self.examined_documents = self
      .examined_documents
      .checked_add(1)
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_document_count", "document count overflowed"))?;
    if *self.examined_documents > self.limits.counts.maximum_documents {
      return Err(QueryExecutionErrorV1::resource(
        "query_execution_document_limit",
        "authoritative source exceeded the admitted document limit",
      ));
    }
    self.prior_file_key = Some(try_clone_bytes(document.file_key, "prior FileKey")?);

    let mut predicate_matches = Vec::new();
    predicate_matches.try_reserve_exact(self.predicate_count).map_err(|source| {
      QueryExecutionErrorV1::resource("query_execution_predicate_allocation", format!("cannot reserve predicate outcomes: {source}"))
    })?;
    predicate_matches.resize(self.predicate_count, false);

    for field in &self.scope.fields {
      self.work.charge(1)?;
      let cancellation = self.work.cancellation;
      let (receipt, value_count, canonical_bytes) = {
        let mut value_visitor = FieldValueVisitorV1 {
          field,
          predicate_matches: &mut predicate_matches,
          limits: self.limits,
          semantic_dynamic_bytes: self.semantic_dynamic_bytes,
          value_count: 0,
          canonical_bytes: 0,
          work: self.work,
          failure: None,
        };
        let scan = fields.scan_field_values(
          QueryExecutionFieldReadRequestV1 {
            selected_namespace_root: self.selected_root,
            scope_id: &self.scope.scope_id,
            file_key: document.file_key,
            record_revision: document.record_revision,
            field_name: field.field_name,
            maximum_values: self.limits.counts.maximum_field_values,
            maximum_canonical_value_bytes: self.limits.bytes.maximum_canonical_value_bytes,
            cancellation,
          },
          &mut value_visitor,
        );
        if let Some(error) = value_visitor.failure.take() {
          return Err(error);
        }
        let receipt = scan.map_err(map_scan_error)?;
        (receipt, value_visitor.value_count, value_visitor.canonical_bytes)
      };
      validate_field_receipt(self.selected_root, &self.scope.scope_id, document, field.field_name, value_count, canonical_bytes, &receipt)?;
      *self.examined_field_values = self
        .examined_field_values
        .checked_add(value_count)
        .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_value_count", "field-value count overflowed"))?;
      if *self.examined_field_values > self.limits.counts.maximum_field_values {
        return Err(QueryExecutionErrorV1::resource(
          "query_execution_value_limit",
          "query execution exceeded the admitted total field-value limit",
        ));
      }
    }

    if evaluate_expression(self.expression, &predicate_matches)? {
      if self.matches.len() as u64 >= self.limits.counts.maximum_matches {
        return Err(QueryExecutionErrorV1::resource(
          "query_execution_match_limit",
          "authoritative query produced more matches than the admitted internal result bound",
        ));
      }
      let prospective_row_bytes = result_row_bytes(document.file_key.len(), document.record_revision.len(), document.path.len())?;
      let prospective_retained = self
        .retained_result_bytes
        .checked_add(prospective_row_bytes)
        .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_retained_bytes", "incremental result retained bytes overflowed"))?;
      if prospective_retained > self.limits.bytes.maximum_retained_bytes {
        return Err(QueryExecutionErrorV1::resource(
          "query_execution_retained_bytes",
          "the next query result exceeds the admitted retained-byte limit",
        ));
      }
      let row = QueryExecutionMatchV1 {
        file_key: try_clone_bytes(document.file_key, "result FileKey")?,
        record_revision: try_clone_bytes(document.record_revision, "result RecordRevision")?,
        path: try_clone_string(document.path, "result path")?,
      };
      let actual_row_bytes = result_row_bytes(row.file_key.capacity(), row.record_revision.capacity(), row.path.capacity())?;
      let actual_retained = self
        .retained_result_bytes
        .checked_add(actual_row_bytes)
        .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_retained_bytes", "incremental result retained bytes overflowed"))?;
      if actual_retained > self.limits.bytes.maximum_retained_bytes {
        return Err(QueryExecutionErrorV1::resource(
          "query_execution_retained_bytes",
          "the allocated query result exceeds the admitted retained-byte limit",
        ));
      }
      *self.retained_result_bytes = actual_retained;
      self.matches.push(row);
    }
    Ok(())
  }
}

struct FieldValueVisitorV1<'field, 'plan, 'value, 'definition, 'matches, 'budget, 'cancellation> {
  field: &'field PreparedFieldV1<'plan, 'value, 'definition>,
  predicate_matches: &'matches mut [bool],
  limits: QueryExecutionLimitsV1,
  semantic_dynamic_bytes: u64,
  value_count: u64,
  canonical_bytes: u64,
  work: &'budget mut WorkBudgetV1<'cancellation>,
  failure: Option<QueryExecutionErrorV1>,
}

impl QueryAuthoritativeValueVisitorV1 for FieldValueVisitorV1<'_, '_, '_, '_, '_, '_, '_> {
  fn visit(&mut self, canonical_value: &[u8]) -> Result<(), QueryExecutionErrorV1> {
    if let Some(error) = &self.failure {
      return Err(error.clone());
    }
    let result = self.visit_value(canonical_value);
    if let Err(error) = &result {
      self.failure = Some(error.clone());
    }
    result
  }
}

impl FieldValueVisitorV1<'_, '_, '_, '_, '_, '_, '_> {
  fn visit_value(&mut self, canonical_value: &[u8]) -> Result<(), QueryExecutionErrorV1> {
    self.work.charge(1)?;
    self.value_count = self
      .value_count
      .checked_add(1)
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_value_count", "field value count overflowed"))?;
    self.canonical_bytes = self
      .canonical_bytes
      .checked_add(canonical_value.len() as u64)
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_value_bytes", "canonical value byte count overflowed"))?;
    if self.value_count > self.limits.counts.maximum_field_values || self.canonical_bytes > self.limits.bytes.maximum_canonical_value_bytes
    {
      return Err(QueryExecutionErrorV1::resource(
        "query_execution_field_value_limit",
        "one field read exceeded its admitted value count or canonical byte limit",
      ));
    }
    ensure_semantic_scratch(
      canonical_scratch_bytes(canonical_value.len())?,
      self.semantic_dynamic_bytes,
      "query_execution_canonical_scratch",
      "canonical source-value decoding exceeds the available semantic workspace",
    )?;
    decode_canonical_value(canonical_value, CanonicalValueBounds::SOURCE_VALUE)
      .map_err(|source| QueryExecutionErrorV1::corrupt("query_execution_canonical_value", source.to_string()))?;
    let mut folded_source = None;
    for predicate in &self.field.predicates {
      let predicate_match = self
        .predicate_matches
        .get_mut(predicate.predicate_index)
        .ok_or_else(|| QueryExecutionErrorV1::corrupt("query_execution_predicate_index", "prepared predicate index is out of bounds"))?;
      if !*predicate_match
        && value_matches_predicate(self.field, predicate, canonical_value, &mut folded_source, self.semantic_dynamic_bytes, self.work)?
      {
        *predicate_match = true;
      }
    }
    Ok(())
  }
}

fn value_matches_predicate(
  field: &PreparedFieldV1<'_, '_, '_>,
  predicate: &PreparedPredicateV1<'_>,
  canonical_value: &[u8],
  folded_source: &mut Option<String>,
  semantic_dynamic_bytes: u64,
  work: &mut WorkBudgetV1<'_>,
) -> Result<bool, QueryExecutionErrorV1> {
  match predicate.operation {
    QueryPredicateOperationV1::Eq(_) | QueryPredicateOperationV1::In(_) => {
      for binding in &predicate.candidates {
        let candidate = prepared_candidate(field, binding.candidate_index)?;
        for literal in binding.literals {
          work.charge(canonical_value.len().max(literal.canonical_value().len()) as u64)?;
          ensure_semantic_scratch(
            canonical_scratch_bytes(canonical_value.len().max(literal.canonical_value().len()))?,
            semantic_dynamic_bytes,
            "query_execution_canonical_scratch",
            "exact comparison exceeds the available semantic workspace",
          )?;
          if candidate.runtime.converter().exact_values_equal(canonical_value, literal.canonical_value()).map_err(map_semantic_error)? {
            return Ok(true);
          }
        }
      }
      Ok(false)
    }
    QueryPredicateOperationV1::Gt(_) | QueryPredicateOperationV1::Lt(_) | QueryPredicateOperationV1::Between(_, _) => {
      ordered_value_matches(field, predicate, canonical_value, semantic_dynamic_bytes, work)
    }
    QueryPredicateOperationV1::Contains(_) => {
      let query = prepared_query_text(predicate)?;
      let text_scratch = text_scratch_bytes(canonical_value.len(), query.len())?;
      ensure_semantic_scratch(
        text_scratch,
        semantic_dynamic_bytes,
        "query_execution_text_scratch",
        "text predicate exceeds the available semantic workspace",
      )?;
      let source = prepared_folded_source(folded_source, canonical_value, work)?;
      contains_controlled(source, query, work)
    }
    QueryPredicateOperationV1::Similar { threshold, .. } => {
      trigram_dice_matches(field, predicate, canonical_value, SimilarityThresholdV1::Inclusive(*threshold), semantic_dynamic_bytes, 0, work)
    }
    QueryPredicateOperationV1::Phonetic(_) => {
      posting_intersection_matches(field, predicate, canonical_value, OPERATION_PHONETIC, semantic_dynamic_bytes, 0, work)
    }
    QueryPredicateOperationV1::Fuzzy { algorithm, edits, .. } => {
      let query = prepared_query_text(predicate)?;
      let text_scratch = text_scratch_bytes(canonical_value.len(), query.len())?;
      ensure_semantic_scratch(
        text_scratch,
        semantic_dynamic_bytes,
        "query_execution_text_scratch",
        "fuzzy predicate exceeds the available semantic workspace",
      )?;
      let source = prepared_folded_source(folded_source, canonical_value, work)?;
      match algorithm {
        QueryFuzzyAlgorithmV1::DamerauLevenshtein => {
          let distance = damerau_levenshtein_controlled(source, query, || work.charge(1))?;
          let maximum = edits.map_or_else(|| auto_fuzziness(query.chars().count()), usize::from);
          Ok(distance <= maximum)
        }
        QueryFuzzyAlgorithmV1::JaroWinkler => {
          let threshold = edits.map_or(0.8, |edits| 1.0 - f64::from(edits) / query.chars().count().max(1) as f64);
          Ok(jaro_winkler_controlled(source, query, || work.charge(1))? >= threshold)
        }
      }
    }
    QueryPredicateOperationV1::Match(_) => {
      let query = prepared_query_text(predicate)?;
      let text_scratch = text_scratch_bytes(canonical_value.len(), query.len())?;
      ensure_semantic_scratch(
        text_scratch,
        semantic_dynamic_bytes,
        "query_execution_text_scratch",
        "match predicate exceeds the available semantic workspace",
      )?;
      let source = prepared_folded_source(folded_source, canonical_value, work)?;
      work.charge(source.len().max(query.len()) as u64)?;
      if source == query
        || trigram_dice_matches(
          field,
          predicate,
          canonical_value,
          SimilarityThresholdV1::Exclusive(0.3),
          semantic_dynamic_bytes,
          text_scratch,
          work,
        )?
        || posting_intersection_matches(field, predicate, canonical_value, OPERATION_PHONETIC, semantic_dynamic_bytes, text_scratch, work)?
      {
        return Ok(true);
      }
      let distance = damerau_levenshtein_controlled(source, query, || work.charge(1))?;
      Ok(distance <= auto_fuzziness(query.chars().count()))
    }
  }
}

fn ordered_value_matches(
  field: &PreparedFieldV1<'_, '_, '_>,
  predicate: &PreparedPredicateV1<'_>,
  canonical_value: &[u8],
  semantic_dynamic_bytes: u64,
  work: &mut WorkBudgetV1<'_>,
) -> Result<bool, QueryExecutionErrorV1> {
  for binding in &predicate.candidates {
    let candidate = prepared_candidate(field, binding.candidate_index)?;
    if binding.value_match != QueryValueMatchV1::OrderedRange {
      continue;
    }
    let compiled = compile_one_source(&candidate.runtime, canonical_value, semantic_dynamic_bytes, 0, work)?;
    for source_value in &compiled.values {
      for source_posting in &source_value.postings {
        work.charge(1)?;
        let compare = |literal: &super::query_planner::CompiledQueryLiteralV1| -> Result<Ordering, QueryExecutionErrorV1> {
          let posting = literal
            .compiled()
            .postings
            .first()
            .ok_or_else(|| QueryExecutionErrorV1::corrupt("query_execution_literal_posting", "ordered literal has no posting"))?;
          candidate.runtime.converter().compare_posting_keys(&source_posting.posting_key, &posting.posting_key).map_err(map_semantic_error)
        };
        let matched = match predicate.operation {
          QueryPredicateOperationV1::Gt(_) => compare(required_literal(binding, 0)?)? == Ordering::Greater,
          QueryPredicateOperationV1::Lt(_) => compare(required_literal(binding, 0)?)? == Ordering::Less,
          QueryPredicateOperationV1::Between(_, _) => {
            compare(required_literal(binding, 0)?)? != Ordering::Less && compare(required_literal(binding, 1)?)? != Ordering::Greater
          }
          _ => false,
        };
        if matched {
          return Ok(true);
        }
      }
    }
  }
  Ok(false)
}

fn posting_intersection_matches(
  field: &PreparedFieldV1<'_, '_, '_>,
  predicate: &PreparedPredicateV1<'_>,
  canonical_value: &[u8],
  required_operation: u64,
  semantic_dynamic_bytes: u64,
  occupied_scratch_bytes: u64,
  work: &mut WorkBudgetV1<'_>,
) -> Result<bool, QueryExecutionErrorV1> {
  for binding in &predicate.candidates {
    let candidate = prepared_candidate(field, binding.candidate_index)?;
    if candidate.runtime.strategy().operations & required_operation == 0 {
      continue;
    }
    let compiled = compile_one_source(&candidate.runtime, canonical_value, semantic_dynamic_bytes, occupied_scratch_bytes, work)?;
    for source in compiled.values.iter().flat_map(|value| value.postings.iter()) {
      for query in binding.literals.iter().flat_map(|literal| literal.compiled().postings.iter()) {
        work.charge(1)?;
        if source.posting_key == query.posting_key {
          return Ok(true);
        }
      }
    }
  }
  Ok(false)
}

fn trigram_dice_matches(
  field: &PreparedFieldV1<'_, '_, '_>,
  predicate: &PreparedPredicateV1<'_>,
  canonical_value: &[u8],
  threshold: SimilarityThresholdV1,
  semantic_dynamic_bytes: u64,
  occupied_scratch_bytes: u64,
  work: &mut WorkBudgetV1<'_>,
) -> Result<bool, QueryExecutionErrorV1> {
  for binding in &predicate.candidates {
    let candidate = prepared_candidate(field, binding.candidate_index)?;
    if candidate.runtime.strategy().name != "trigram" || binding.literals.is_empty() {
      continue;
    }
    let compiled = compile_one_source(&candidate.runtime, canonical_value, semantic_dynamic_bytes, occupied_scratch_bytes, work)?;
    let source = &compiled
      .values
      .first()
      .ok_or_else(|| {
        QueryExecutionErrorV1::unavailable("query_execution_document_unindexable", "trigram source compilation produced no canonical value")
      })?
      .postings;
    let query = &required_literal(binding, 0)?.compiled().postings;
    let mut intersection = 0usize;
    for source_posting in source {
      for query_posting in query {
        work.charge(1)?;
        if source_posting.posting_key == query_posting.posting_key {
          intersection += 1;
          break;
        }
      }
    }
    let denominator = source
      .len()
      .checked_add(query.len())
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_similarity_count", "trigram denominator overflowed"))?;
    let similarity = if denominator == 0 { 0.0 } else { 2.0 * intersection as f64 / denominator as f64 };
    if threshold.accepts(similarity) {
      return Ok(true);
    }
  }
  Ok(false)
}

fn required_literal<'a>(
  binding: &'a PreparedPredicateCandidateV1<'_>,
  index: usize,
) -> Result<&'a super::query_planner::CompiledQueryLiteralV1, QueryExecutionErrorV1> {
  binding
    .literals
    .get(index)
    .ok_or_else(|| QueryExecutionErrorV1::corrupt("query_execution_literal_index", "prepared query literal index is out of bounds"))
}

fn prepared_candidate<'a, 'value, 'field>(
  field: &'a PreparedFieldV1<'_, 'value, 'field>,
  index: usize,
) -> Result<&'a PreparedCandidateV1<'value, 'field>, QueryExecutionErrorV1> {
  field
    .candidates
    .get(index)
    .ok_or_else(|| QueryExecutionErrorV1::corrupt("query_execution_candidate_index", "prepared predicate candidate index is out of bounds"))
}

fn compile_one_source<'value, 'field>(
  runtime: &IndexDefinitionRuntimeV1<'value, 'field>,
  canonical_value: &[u8],
  semantic_dynamic_bytes: u64,
  occupied_scratch_bytes: u64,
  work: &mut WorkBudgetV1<'_>,
) -> Result<super::index_definition_runtime::CompiledIndexDocumentV1, QueryExecutionErrorV1> {
  work.charge(canonical_value.len() as u64)?;
  let compile_scratch = runtime.maximum_compile_source_value_bytes(canonical_value.len() as u64).map_err(map_definition_error)?;
  let required_scratch = occupied_scratch_bytes
    .checked_add(compile_scratch)
    .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_compile_scratch", "compiled source-value scratch overflowed"))?;
  ensure_semantic_scratch(
    required_scratch,
    semantic_dynamic_bytes,
    "query_execution_compile_scratch",
    "compiled source value exceeds the available semantic workspace",
  )?;
  let mut values = Vec::new();
  values.try_reserve_exact(1).map_err(|source| QueryExecutionErrorV1::resource("query_execution_source_allocation", source.to_string()))?;
  values.push(try_clone_bytes(canonical_value, "canonical source value")?);
  runtime.compile_source_values(&values).map_err(map_definition_error)
}

fn folded_canonical_text(canonical_value: &[u8]) -> Result<String, QueryExecutionErrorV1> {
  let value = decode_canonical_value(canonical_value, CanonicalValueBounds::SOURCE_VALUE)
    .map_err(|source| QueryExecutionErrorV1::corrupt("query_execution_text_value", source.to_string()))?;
  match value {
    CanonicalConfigValueV1::String(value) => folded_text(&value),
    CanonicalConfigValueV1::Bytes(value) => {
      let value = std::str::from_utf8(&value)
        .map_err(|source| QueryExecutionErrorV1::unavailable("query_execution_document_unindexable", source.to_string()))?;
      folded_text(value)
    }
    _ => Err(QueryExecutionErrorV1::unavailable(
      "query_execution_document_unindexable",
      "text predicate received a non-text canonical source value",
    )),
  }
}

fn folded_config_text(value: &CanonicalConfigValueV1) -> Result<String, QueryExecutionErrorV1> {
  match value {
    CanonicalConfigValueV1::String(value) => folded_text(value),
    CanonicalConfigValueV1::Bytes(value) => {
      let value = std::str::from_utf8(value)
        .map_err(|source| QueryExecutionErrorV1::invalid("query_execution_query_text_utf8", source.to_string()))?;
      folded_text(value)
    }
    _ => {
      Err(QueryExecutionErrorV1::invalid("query_execution_query_text_type", "text predicate contains a non-text compiled query literal"))
    }
  }
}

fn folded_text(value: &str) -> Result<String, QueryExecutionErrorV1> {
  let characters =
    fold_characters(value).map_err(|source| QueryExecutionErrorV1::internal("query_execution_text_fold", source.to_string()))?;
  let mut output = String::new();
  output.try_reserve(characters.len().saturating_mul(4)).map_err(|source| {
    QueryExecutionErrorV1::resource("query_execution_text_allocation", format!("cannot reserve folded text: {source}"))
  })?;
  output.extend(characters);
  Ok(output)
}

fn text_operation_value(operation: &QueryPredicateOperationV1) -> Option<&CanonicalConfigValueV1> {
  match operation {
    QueryPredicateOperationV1::Contains(value)
    | QueryPredicateOperationV1::Fuzzy { value, .. }
    | QueryPredicateOperationV1::Match(value) => Some(value),
    _ => None,
  }
}

fn prepare_query_text(operation: &QueryPredicateOperationV1, work: &mut WorkBudgetV1<'_>) -> Result<Option<String>, QueryExecutionErrorV1> {
  let Some(value) = text_operation_value(operation) else {
    return Ok(None);
  };
  let source_bytes = match value {
    CanonicalConfigValueV1::String(value) => value.len(),
    CanonicalConfigValueV1::Bytes(value) => value.len(),
    _ => 1,
  };
  work.charge(source_bytes as u64)?;
  folded_config_text(value).map(Some)
}

fn prepared_query_text<'a>(predicate: &'a PreparedPredicateV1<'_>) -> Result<&'a str, QueryExecutionErrorV1> {
  predicate
    .folded_query_text
    .as_deref()
    .ok_or_else(|| QueryExecutionErrorV1::corrupt("query_execution_prepared_text", "text predicate has no prepared canonical query text"))
}

fn prepared_folded_source<'a>(
  folded_source: &'a mut Option<String>,
  canonical_value: &[u8],
  work: &mut WorkBudgetV1<'_>,
) -> Result<&'a str, QueryExecutionErrorV1> {
  if folded_source.is_none() {
    work.charge(canonical_value.len() as u64)?;
    *folded_source = Some(folded_canonical_text(canonical_value)?);
  }
  folded_source
    .as_deref()
    .ok_or_else(|| QueryExecutionErrorV1::internal("query_execution_prepared_text", "folded source text was not retained"))
}

fn contains_controlled(source: &str, query: &str, work: &mut WorkBudgetV1<'_>) -> Result<bool, QueryExecutionErrorV1> {
  if query.is_empty() {
    return Ok(true);
  }
  let query = query.as_bytes();
  let mut prefix = Vec::new();
  prefix.try_reserve_exact(query.len()).map_err(|source| {
    QueryExecutionErrorV1::resource("query_execution_contains_allocation", format!("cannot reserve substring prefix table: {source}"))
  })?;
  prefix.resize(query.len(), 0usize);
  let mut matched = 0usize;
  for index in 1..query.len() {
    work.charge(1)?;
    while matched > 0 && query[index] != query[matched] {
      work.charge(1)?;
      matched = prefix[matched - 1];
    }
    if query[index] == query[matched] {
      matched += 1;
      prefix[index] = matched;
    }
  }
  matched = 0;
  for byte in source.bytes() {
    work.charge(1)?;
    while matched > 0 && byte != query[matched] {
      work.charge(1)?;
      matched = prefix[matched - 1];
    }
    if byte == query[matched] {
      matched += 1;
      if matched == query.len() {
        return Ok(true);
      }
    }
  }
  Ok(false)
}

fn canonical_scratch_bytes(length: usize) -> Result<u64, QueryExecutionErrorV1> {
  (length as u64)
    .checked_mul(64)
    .and_then(|bytes| bytes.checked_add(4 * 1_024))
    .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_canonical_scratch", "canonical scratch estimate overflowed"))
}

fn text_scratch_bytes(left: usize, right: usize) -> Result<u64, QueryExecutionErrorV1> {
  (left as u64)
    .checked_add(right as u64)
    .and_then(|bytes| bytes.checked_mul(64))
    .and_then(|bytes| bytes.checked_add(4 * 1_024))
    .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_text_scratch", "text scratch estimate overflowed"))
}

fn ensure_semantic_scratch(required: u64, available: u64, code: &'static str, context: &'static str) -> Result<(), QueryExecutionErrorV1> {
  if required > available {
    return Err(QueryExecutionErrorV1::resource(code, context));
  }
  Ok(())
}

fn prepare_scopes<'plan, 'catalog>(
  plan: &'plan CompiledRootAwareQueryPlanV1,
  catalogs: &'catalog [RootAwareQueryFieldCatalogV1],
  hash_algorithm: HashAlgorithm,
  maximum_semantic_bytes: u64,
  work: &mut WorkBudgetV1<'_>,
  selected_scope_id: Option<&[u8]>,
) -> Result<PreparedExecutionV1<'plan, 'catalog, 'catalog>, QueryExecutionErrorV1> {
  work.charge(1)?;
  let first = plan
    .predicates()
    .first()
    .ok_or_else(|| QueryExecutionErrorV1::invalid("query_execution_predicates", "compiled query has no predicates"))?;
  validate_shared_scope_partition(plan, first, work)?;
  let structural_bytes = prepared_structural_bound(plan, selected_scope_id)?;
  ensure_semantic_scratch(
    structural_bytes
      .checked_add(EXECUTION_PER_DOCUMENT_SEMANTIC_BYTES)
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "semantic structure bound overflowed"))?,
    maximum_semantic_bytes,
    "query_execution_semantic_memory",
    "prepared query structures exceed the admitted semantic workspace",
  )?;
  let mut scopes = Vec::new();
  let scope_capacity = if selected_scope_id.is_some() { 1 } else { first.scopes().len() };
  scopes.try_reserve_exact(scope_capacity).map_err(|source| {
    QueryExecutionErrorV1::resource("query_execution_scope_allocation", format!("cannot reserve prepared scopes: {source}"))
  })?;
  let mut definition_bytes = 0u64;
  let mut runtime_bytes = 0u64;

  for first_scope in first.scopes() {
    work.charge(1)?;
    if selected_scope_id.is_some_and(|scope_id| scope_id != first_scope.scope_id()) {
      continue;
    }
    let mut fields = Vec::new();
    fields.try_reserve_exact(plan.predicates().len()).map_err(|source| {
      QueryExecutionErrorV1::resource("query_execution_field_allocation", format!("cannot reserve prepared fields: {source}"))
    })?;
    for (predicate_index, predicate) in plan.predicates().iter().enumerate() {
      work.charge(1)?;
      let scope_plan = find_predicate_scope(predicate, first_scope.scope_id())?;
      let catalog = find_catalog(plan, catalogs, predicate.field_name())?;
      let catalog_scope = catalog
        .scopes
        .iter()
        .find(|scope| scope.scope_id == first_scope.scope_id())
        .ok_or_else(|| QueryExecutionErrorV1::corrupt("query_execution_catalog_scope", "catalog omits a planned effective scope"))?;
      let field_index = if let Some(index) =
        fields.iter().position(|field: &PreparedFieldV1<'plan, 'catalog, 'catalog>| field.field_name == predicate.field_name())
      {
        index
      } else {
        fields.push(PreparedFieldV1 { field_name: predicate.field_name(), candidates: Vec::new(), predicates: Vec::new() });
        fields.len() - 1
      };
      let field = &mut fields[field_index];
      if field.candidates.is_empty() {
        definition_bytes = definition_bytes
          .checked_add(catalog_scope.encoded_value_store_definition.len() as u64)
          .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_definition_bytes", "definition bytes overflowed"))?;
        field.candidates.try_reserve_exact(scope_plan.candidates().len()).map_err(|source| {
          QueryExecutionErrorV1::resource("query_execution_candidate_allocation", format!("cannot reserve prepared candidates: {source}"))
        })?;
      }
      let mut predicate_candidates = Vec::new();
      predicate_candidates.try_reserve_exact(scope_plan.candidates().len()).map_err(|source| {
        QueryExecutionErrorV1::resource("query_execution_candidate_allocation", format!("cannot reserve predicate candidates: {source}"))
      })?;
      for planned in scope_plan.candidates() {
        work.charge(1)?;
        let candidate_index =
          if let Some(index) = field.candidates.iter().position(|candidate| candidate.runtime.index_id() == planned.index_id()) {
            let candidate = &field.candidates[index];
            if candidate.runtime.strategy().name != planned.strategy_name() {
              return Err(QueryExecutionErrorV1::corrupt(
                "query_execution_runtime_reuse",
                "repeated selected field candidate disagrees with its prepared runtime",
              ));
            }
            index
          } else {
            let catalog_candidate = catalog_scope
              .indexes
              .iter()
              .find(|candidate| candidate.index_id == planned.index_id())
              .ok_or_else(|| QueryExecutionErrorV1::corrupt("query_execution_catalog_index", "catalog omits a planned field index"))?;
            let definition_work = catalog_scope
              .encoded_value_store_definition
              .len()
              .checked_add(catalog_candidate.encoded_field_definition.len())
              .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_definition_work", "definition work overflowed"))?;
            work.charge(definition_work as u64)?;
            definition_bytes = definition_bytes
              .checked_add(catalog_candidate.encoded_field_definition.len() as u64)
              .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_definition_bytes", "definition bytes overflowed"))?;
            let remaining_runtime_bytes = maximum_semantic_bytes
              .checked_sub(EXECUTION_PER_DOCUMENT_SEMANTIC_BYTES)
              .and_then(|bytes| bytes.checked_sub(structural_bytes))
              .and_then(|bytes| bytes.checked_sub(definition_bytes))
              .and_then(|bytes| bytes.checked_sub(runtime_bytes))
              .ok_or_else(|| {
                QueryExecutionErrorV1::resource(
                  "query_execution_semantic_memory",
                  "selected semantic definitions exceed the admitted semantic workspace",
                )
              })?;
            let runtime = IndexDefinitionRuntimeV1::from_encoded_bounded(
              &catalog_scope.encoded_value_store_definition,
              &catalog_candidate.encoded_field_definition,
              hash_algorithm,
              remaining_runtime_bytes,
            )
            .map_err(map_definition_error)?;
            if runtime.index_id() != planned.index_id() || runtime.strategy().name != planned.strategy_name() {
              return Err(QueryExecutionErrorV1::corrupt(
                "query_execution_runtime_identity",
                "prepared selected definition differs from the compiled query candidate",
              ));
            }
            runtime_bytes = runtime_bytes
              .checked_add(runtime.maximum_retained_bytes().map_err(map_definition_error)?)
              .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "runtime memory bound overflowed"))?;
            field.candidates.try_reserve(1).map_err(|source| {
              QueryExecutionErrorV1::resource("query_execution_candidate_allocation", format!("cannot grow prepared candidates: {source}"))
            })?;
            field.candidates.push(PreparedCandidateV1 { runtime });
            field.candidates.len() - 1
          };
        predicate_candidates.push(PreparedPredicateCandidateV1 {
          candidate_index,
          literals: planned.compiled_literals(),
          value_match: planned.value_match(),
        });
      }
      if predicate_candidates.is_empty() {
        return Err(QueryExecutionErrorV1::unavailable(
          "query_execution_semantics_unavailable",
          "compiled predicate has no exact selected-root definition runtime",
        ));
      }
      field.predicates.try_reserve(1).map_err(|source| {
        QueryExecutionErrorV1::resource("query_execution_predicate_allocation", format!("cannot reserve prepared predicate: {source}"))
      })?;
      field.predicates.push(PreparedPredicateV1 {
        predicate_index,
        operation: predicate.operation(),
        folded_query_text: prepare_query_text(predicate.operation(), work)?,
        candidates: predicate_candidates,
      });
    }
    fields.sort_by(|left, right| left.field_name.cmp(right.field_name));
    scopes.push(PreparedScopeV1 { scope_id: try_clone_bytes(first_scope.scope_id(), "prepared ScopeId")?, fields });
  }
  if scopes.is_empty() {
    return if selected_scope_id.is_some() {
      Err(QueryExecutionErrorV1::invalid("query_execution_scope_unknown", "requested scope is absent from the compiled query"))
    } else {
      Err(QueryExecutionErrorV1::unavailable("query_execution_scopes_unavailable", "compiled query has no effective selected-root scopes"))
    };
  }
  let retained_bytes = prepared_retained_bytes(&scopes, definition_bytes)?;
  ensure_semantic_scratch(
    retained_bytes
      .checked_add(EXECUTION_PER_DOCUMENT_SEMANTIC_BYTES)
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "prepared semantic bound overflowed"))?,
    maximum_semantic_bytes,
    "query_execution_semantic_memory",
    "prepared query semantics exceed the admitted semantic workspace",
  )?;
  Ok(PreparedExecutionV1 { scopes, retained_bytes })
}

fn validate_shared_scope_partition(
  plan: &CompiledRootAwareQueryPlanV1,
  first: &CompiledQueryPredicatePlanV1,
  work: &mut WorkBudgetV1<'_>,
) -> Result<(), QueryExecutionErrorV1> {
  for predicate in plan.predicates().iter().skip(1) {
    work.charge(1)?;
    if predicate.scopes().len() != first.scopes().len() {
      return Err(QueryExecutionErrorV1::unavailable(
        "query_execution_scope_partition_unavailable",
        "compiled query requires a cross-scope FileKey merge before authoritative execution",
      ));
    }
    for (expected, actual) in first.scopes().iter().zip(predicate.scopes()) {
      work.charge(1)?;
      if expected.scope_id() != actual.scope_id() {
        return Err(QueryExecutionErrorV1::unavailable(
          "query_execution_scope_partition_unavailable",
          "compiled query requires a cross-scope FileKey merge before authoritative execution",
        ));
      }
    }
  }
  Ok(())
}

fn prepared_structural_bound(plan: &CompiledRootAwareQueryPlanV1, selected_scope_id: Option<&[u8]>) -> Result<u64, QueryExecutionErrorV1> {
  let first = plan
    .predicates()
    .first()
    .ok_or_else(|| QueryExecutionErrorV1::invalid("query_execution_predicates", "compiled query has no predicates"))?;
  let mut bytes = EXECUTION_FIXED_SEMANTIC_BYTES;
  let scope_count = first.scopes().iter().filter(|scope| selected_scope_id.is_none_or(|scope_id| scope.scope_id() == scope_id)).count();
  bytes = checked_structure_array::<PreparedScopeV1<'_, '_, '_>>(bytes, scope_count, "prepared scopes")?;
  for first_scope in first.scopes().iter().filter(|scope| selected_scope_id.is_none_or(|scope_id| scope.scope_id() == scope_id)) {
    bytes = bytes
      .checked_add(first_scope.scope_id().len() as u64)
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "prepared ScopeId bound overflowed"))?;
    bytes = checked_structure_array::<PreparedFieldV1<'_, '_, '_>>(bytes, plan.predicates().len(), "prepared fields")?;
    bytes = checked_structure_array::<PreparedPredicateV1<'_>>(bytes, plan.predicates().len(), "prepared predicates")?;
    for predicate in plan.predicates() {
      let scope = find_predicate_scope(predicate, first_scope.scope_id())?;
      bytes = checked_structure_array::<PreparedCandidateV1<'_, '_>>(bytes, scope.candidates().len(), "prepared candidates")?;
      bytes = checked_structure_array::<PreparedPredicateCandidateV1<'_>>(bytes, scope.candidates().len(), "predicate bindings")?;
      if let Some(value) = text_operation_value(predicate.operation()) {
        let input_bytes = match value {
          CanonicalConfigValueV1::String(value) => value.len() as u64,
          CanonicalConfigValueV1::Bytes(value) => value.len() as u64,
          _ => 0,
        };
        bytes = bytes
          .checked_add(
            input_bytes
              .checked_mul(4)
              .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "prepared text bound overflowed"))?,
          )
          .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "prepared text bound overflowed"))?;
      }
    }
  }
  Ok(bytes)
}

fn checked_structure_array<T>(bytes: u64, count: usize, label: &'static str) -> Result<u64, QueryExecutionErrorV1> {
  let count = u64::try_from(count)
    .map_err(|source| QueryExecutionErrorV1::resource("query_execution_semantic_memory", format!("{label} count: {source}")))?;
  bytes
    .checked_add(
      count
        .checked_mul(size_of::<T>() as u64)
        .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", format!("{label} byte bound overflowed")))?,
    )
    .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", format!("{label} byte bound overflowed")))
}

fn prepared_retained_bytes(scopes: &Vec<PreparedScopeV1<'_, '_, '_>>, definition_bytes: u64) -> Result<u64, QueryExecutionErrorV1> {
  let mut bytes = EXECUTION_FIXED_SEMANTIC_BYTES
    .checked_add(definition_bytes)
    .and_then(|value| value.checked_add((scopes.capacity() * size_of::<PreparedScopeV1<'_, '_, '_>>()) as u64))
    .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "prepared scope accounting overflowed"))?;
  for scope in scopes {
    bytes = bytes
      .checked_add(scope.scope_id.capacity() as u64)
      .and_then(|value| value.checked_add((scope.fields.capacity() * size_of::<PreparedFieldV1<'_, '_, '_>>()) as u64))
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "prepared field accounting overflowed"))?;
    for field in &scope.fields {
      bytes = bytes
        .checked_add((field.candidates.capacity() * size_of::<PreparedCandidateV1<'_, '_>>()) as u64)
        .and_then(|value| value.checked_add((field.predicates.capacity() * size_of::<PreparedPredicateV1<'_>>()) as u64))
        .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "prepared candidate accounting overflowed"))?;
      for candidate in &field.candidates {
        bytes = bytes
          .checked_add(candidate.runtime.maximum_retained_bytes().map_err(map_definition_error)?)
          .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "compiled runtime accounting overflowed"))?;
      }
      for predicate in &field.predicates {
        bytes = bytes
          .checked_add((predicate.candidates.capacity() * size_of::<PreparedPredicateCandidateV1<'_>>()) as u64)
          .and_then(|value| value.checked_add(predicate.folded_query_text.as_ref().map_or(0, |text| text.capacity() as u64)))
          .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_semantic_memory", "predicate binding accounting overflowed"))?;
      }
    }
  }
  Ok(bytes)
}

fn find_predicate_scope<'a>(
  predicate: &'a CompiledQueryPredicatePlanV1,
  scope_id: &[u8],
) -> Result<&'a super::query_planner::CompiledQueryScopePlanV1, QueryExecutionErrorV1> {
  predicate
    .scopes()
    .iter()
    .find(|scope| scope.scope_id() == scope_id)
    .ok_or_else(|| QueryExecutionErrorV1::corrupt("query_execution_scope_partition", "predicate effective-scope partitions disagree"))
}

fn find_catalog<'a>(
  plan: &CompiledRootAwareQueryPlanV1,
  catalogs: &'a [RootAwareQueryFieldCatalogV1],
  field_name: &str,
) -> Result<&'a RootAwareQueryFieldCatalogV1, QueryExecutionErrorV1> {
  let mut found = None;
  for catalog in catalogs.iter().filter(|catalog| catalog.field_name == field_name) {
    if found.replace(catalog).is_some() {
      return Err(QueryExecutionErrorV1::corrupt("query_execution_catalog_duplicate", "selected query catalogs repeat a field"));
    }
  }
  let catalog =
    found.ok_or_else(|| QueryExecutionErrorV1::corrupt("query_execution_catalog_missing", "selected query catalog is missing"))?;
  if catalog.database_id != plan.database_id()
    || catalog.physical_instance_id != plan.physical_instance_id()
    || catalog.selected_namespace_root != plan.selected_namespace_root()
    || catalog.semantic_state_root != plan.semantic_state_root()
    || catalog.publication_sequence != plan.publication_sequence()
    || !catalog.complete
  {
    return Err(QueryExecutionErrorV1::corrupt(
      "query_execution_catalog_authority",
      "query catalog does not bind the exact complete compiled-plan authority",
    ));
  }
  Ok(catalog)
}

fn validate_execution_request(request: &RootAwareQueryExecutionRequestV1<'_>) -> Result<(), QueryExecutionErrorV1> {
  let hash_width = request.plan.hash_algorithm().hash_length();
  if request.plan.selected_namespace_root().len() != hash_width
    || request.plan.semantic_state_root().len() != hash_width
    || request.plan.selected_namespace_root().iter().all(|byte| *byte == 0)
    || request.plan.semantic_state_root().iter().all(|byte| *byte == 0)
    || request.plan.publication_sequence() == 0
  {
    return Err(QueryExecutionErrorV1::invalid("query_execution_plan_identity", "compiled plan contains invalid selected-root authority"));
  }
  if !path_is_canonical(request.plan.query_path()) {
    return Err(QueryExecutionErrorV1::invalid("query_execution_path", "compiled query path is not canonical"));
  }
  Ok(())
}

fn validate_scope_selection(plan: &CompiledRootAwareQueryPlanV1, scope_id: Option<&[u8]>) -> Result<(), QueryExecutionErrorV1> {
  let Some(scope_id) = scope_id else {
    return Ok(());
  };
  if scope_id.len() != plan.hash_algorithm().hash_length() || scope_id.iter().all(|byte| *byte == 0) {
    return Err(QueryExecutionErrorV1::invalid("query_execution_scope_identity", "requested scope is not one nonzero database hash"));
  }
  Ok(())
}

fn validate_document(
  hash_algorithm: HashAlgorithm,
  query_path: &str,
  document: QueryExecutionDocumentV1<'_>,
  prior_file_key: Option<&[u8]>,
) -> Result<(), QueryExecutionErrorV1> {
  let hash_width = hash_algorithm.hash_length();
  if document.file_key.len() != hash_width
    || document.record_revision.len() != hash_width
    || document.file_key.iter().all(|byte| *byte == 0)
    || document.record_revision.iter().all(|byte| *byte == 0)
  {
    return Err(QueryExecutionErrorV1::corrupt(
      "query_execution_document_identity",
      "authoritative document has an invalid FileKey or RecordRevision",
    ));
  }
  if prior_file_key.is_some_and(|prior| prior >= document.file_key) {
    return Err(QueryExecutionErrorV1::corrupt(
      "query_execution_document_order",
      "effective-scope documents are not in strict FileKey order",
    ));
  }
  if !path_is_canonical(document.path) || !path_is_within(query_path, document.path) {
    return Err(QueryExecutionErrorV1::corrupt(
      "query_execution_document_path",
      "authoritative document path is noncanonical or outside the query path",
    ));
  }
  Ok(())
}

fn validate_scope_receipt(
  plan: &CompiledRootAwareQueryPlanV1,
  scope: &PreparedScopeV1<'_, '_, '_>,
  observed_documents: u64,
  receipt: &QueryExecutionScopeScanReceiptV1,
) -> Result<(), QueryExecutionErrorV1> {
  if !receipt.complete
    || receipt.selected_namespace_root != plan.selected_namespace_root()
    || receipt.publication_sequence != plan.publication_sequence()
    || receipt.scope_id != scope.scope_id
    || receipt.document_count != observed_documents
  {
    return Err(QueryExecutionErrorV1::corrupt(
      "query_execution_scope_receipt",
      "authoritative scope receipt is incomplete or disagrees with observed selected-root rows",
    ));
  }
  Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_field_receipt(
  selected_root: &[u8],
  scope_id: &[u8],
  document: QueryExecutionDocumentV1<'_>,
  field_name: &str,
  observed_values: u64,
  observed_bytes: u64,
  receipt: &QueryExecutionFieldReadReceiptV1,
) -> Result<(), QueryExecutionErrorV1> {
  if !receipt.complete
    || receipt.selected_namespace_root != selected_root
    || receipt.scope_id != scope_id
    || receipt.file_key != document.file_key
    || receipt.record_revision != document.record_revision
    || receipt.field_name != field_name
    || receipt.value_count != observed_values
    || receipt.canonical_value_bytes != observed_bytes
  {
    return Err(QueryExecutionErrorV1::corrupt(
      "query_execution_field_receipt",
      "authoritative field receipt is incomplete or disagrees with observed canonical values",
    ));
  }
  match receipt.state {
    QueryExecutionFieldStateV1::Missing if observed_values == 0 && observed_bytes == 0 => Ok(()),
    QueryExecutionFieldStateV1::Values if observed_values > 0 => Ok(()),
    QueryExecutionFieldStateV1::DeterministicUnindexable if observed_values == 0 && observed_bytes == 0 => {
      Err(QueryExecutionErrorV1::unavailable(
        "query_execution_document_unindexable",
        "a selected-root document cannot be evaluated under the frozen field semantics",
      ))
    }
    _ => Err(QueryExecutionErrorV1::corrupt(
      "query_execution_field_state",
      "authoritative field state disagrees with its canonical value stream",
    )),
  }
}

fn evaluate_expression(expression: &CompiledQueryExpressionV1, predicates: &[bool]) -> Result<bool, QueryExecutionErrorV1> {
  match expression {
    CompiledQueryExpressionV1::Field(index) => predicates
      .get(*index)
      .copied()
      .ok_or_else(|| QueryExecutionErrorV1::corrupt("query_execution_expression_index", "compiled predicate index is out of bounds")),
    CompiledQueryExpressionV1::And(children) => {
      for child in children {
        if !evaluate_expression(child, predicates)? {
          return Ok(false);
        }
      }
      Ok(true)
    }
    CompiledQueryExpressionV1::Or(children) => {
      for child in children {
        if evaluate_expression(child, predicates)? {
          return Ok(true);
        }
      }
      Ok(false)
    }
    CompiledQueryExpressionV1::Not(child) => Ok(!evaluate_expression(child, predicates)?),
  }
}

fn result_retained_bytes(root: &[u8], matches: &[QueryExecutionMatchV1], match_capacity: usize) -> Result<u64, QueryExecutionErrorV1> {
  let mut retained = result_base_retained_bytes(root, match_capacity)?;
  for row in matches {
    retained = retained
      .checked_add(result_row_bytes(row.file_key.capacity(), row.record_revision.capacity(), row.path.capacity())?)
      .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_retained_bytes", "result row bytes overflowed"))?;
  }
  Ok(retained)
}

fn result_base_retained_bytes(root: &[u8], match_capacity: usize) -> Result<u64, QueryExecutionErrorV1> {
  (match_capacity as u64)
    .checked_mul(size_of::<QueryExecutionMatchV1>() as u64)
    .and_then(|bytes| bytes.checked_add(root.len() as u64))
    .and_then(|bytes| bytes.checked_add(EXECUTION_FIXED_RETAINED_BYTES))
    .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_retained_bytes", "result base retained bytes overflowed"))
}

fn result_row_bytes(file_key_bytes: usize, revision_bytes: usize, path_bytes: usize) -> Result<u64, QueryExecutionErrorV1> {
  RESULT_ROW_FIXED_BYTES
    .checked_add(file_key_bytes as u64)
    .and_then(|bytes| bytes.checked_add(revision_bytes as u64))
    .and_then(|bytes| bytes.checked_add(path_bytes as u64))
    .ok_or_else(|| QueryExecutionErrorV1::resource("query_execution_retained_bytes", "result row retained bytes overflowed"))
}

fn reserve_query_memory(memory: &MemoryCoordinator, bytes: u64, code: &'static str) -> Result<MemoryReservation, QueryExecutionErrorV1> {
  memory.reserve(MemoryOwner::Query, bytes, AdmissionClass::Workload).map_err(|error| match error {
    MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. } => QueryExecutionErrorV1::resource(code, error.to_string()),
    _ => QueryExecutionErrorV1::internal("query_execution_memory_authority", error.to_string()),
  })
}

fn shrink_reservation(reservation: &mut MemoryReservation, retained_bytes: u64) -> Result<(), QueryExecutionErrorV1> {
  let release = reservation.bytes().checked_sub(retained_bytes).ok_or_else(|| {
    QueryExecutionErrorV1::internal("query_execution_memory_accounting", "retained result exceeds its memory reservation")
  })?;
  reservation.shrink(release).map_err(|error| QueryExecutionErrorV1::internal("query_execution_memory_accounting", error.to_string()))
}

fn map_scan_error(error: QueryExecutionScanErrorV1) -> QueryExecutionErrorV1 {
  match error {
    QueryExecutionScanErrorV1::Visitor(error) => error,
    QueryExecutionScanErrorV1::Source(error) => match error.class {
      QueryExecutionSourceErrorClassV1::Unavailable => QueryExecutionErrorV1::unavailable(error.code, error.context),
      QueryExecutionSourceErrorClassV1::ResourceLimit => QueryExecutionErrorV1::resource(error.code, error.context),
      QueryExecutionSourceErrorClassV1::Corrupt => QueryExecutionErrorV1::corrupt(error.code, error.context),
      QueryExecutionSourceErrorClassV1::Cancelled => QueryExecutionErrorV1::cancelled(),
      QueryExecutionSourceErrorClassV1::Internal => QueryExecutionErrorV1::internal(error.code, error.context),
    },
  }
}

fn map_definition_error(error: super::index_definition_runtime::IndexDefinitionErrorV1) -> QueryExecutionErrorV1 {
  match error.class() {
    IndexDefinitionErrorClassV1::ResourceLimit => QueryExecutionErrorV1::resource(error.code(), error.context()),
    IndexDefinitionErrorClassV1::UnsupportedDefinition => QueryExecutionErrorV1::unavailable(error.code(), error.context()),
    IndexDefinitionErrorClassV1::InvalidSourceValue => {
      QueryExecutionErrorV1::unavailable("query_execution_document_unindexable", error.to_string())
    }
    IndexDefinitionErrorClassV1::IdentityMismatch | IndexDefinitionErrorClassV1::SemanticMismatch => {
      QueryExecutionErrorV1::corrupt(error.code(), error.context())
    }
  }
}

fn map_semantic_error(error: super::index_converter::IndexSemanticErrorV1) -> QueryExecutionErrorV1 {
  use super::index_converter::IndexSemanticErrorClassV1;
  match error.class() {
    IndexSemanticErrorClassV1::ResourceLimit => QueryExecutionErrorV1::resource(error.code(), error.context()),
    IndexSemanticErrorClassV1::UnsupportedDefinition => QueryExecutionErrorV1::unavailable(error.code(), error.context()),
    IndexSemanticErrorClassV1::InvalidSourceValue => {
      QueryExecutionErrorV1::unavailable("query_execution_document_unindexable", error.to_string())
    }
    IndexSemanticErrorClassV1::MalformedPostingKey => QueryExecutionErrorV1::corrupt(error.code(), error.context()),
  }
}

fn require_not_cancelled(cancellation: &CancellationToken) -> Result<(), QueryExecutionErrorV1> {
  if cancellation.is_cancelled() {
    Err(QueryExecutionErrorV1::cancelled())
  } else {
    Ok(())
  }
}

fn path_is_within(scope: &str, path: &str) -> bool {
  scope == "/" || scope == path || path.strip_prefix(scope).is_some_and(|suffix| suffix.starts_with('/'))
}

fn path_is_canonical(path: &str) -> bool {
  if path.is_empty()
    || path.len() > QUERY_MAXIMUM_PATH_BYTES_V1
    || path.as_bytes().contains(&0)
    || path.trim() != path
    || !path.starts_with('/')
  {
    return false;
  }
  if path == "/" {
    return true;
  }
  !path.ends_with('/') && path[1..].split('/').all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

fn try_clone_bytes(value: &[u8], label: &'static str) -> Result<Vec<u8>, QueryExecutionErrorV1> {
  let mut output = Vec::new();
  output
    .try_reserve_exact(value.len())
    .map_err(|source| QueryExecutionErrorV1::resource("query_execution_allocation", format!("cannot reserve {label}: {source}")))?;
  output.extend_from_slice(value);
  Ok(output)
}

fn try_clone_string(value: &str, label: &'static str) -> Result<String, QueryExecutionErrorV1> {
  let mut output = String::new();
  output
    .try_reserve_exact(value.len())
    .map_err(|source| QueryExecutionErrorV1::resource("query_execution_allocation", format!("cannot reserve {label}: {source}")))?;
  output.push_str(value);
  Ok(output)
}
