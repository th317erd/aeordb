//! Bounded boolean composition of planner-proven query candidate supersets.
//!
//! This module selects candidate work only. It does not read artifacts,
//! evaluate document values, or grant candidate data result authority.

use std::error::Error;
use std::fmt;
use std::mem::size_of;

use tokio_util::sync::CancellationToken;

use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::query_planner::{
  CompiledQueryCoverageV1, CompiledQueryExpressionV1, CompiledQueryScopePlanV1, CompiledRootAwareQueryPlanV1, QueryPlanDriverV1,
};

const COMPOSITION_BASE_BYTES: u64 = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryCandidateCompositionLimitsV1 {
  maximum_selections: u64,
  maximum_retained_bytes: u64,
}

impl QueryCandidateCompositionLimitsV1 {
  pub fn new(maximum_selections: u64, maximum_retained_bytes: u64) -> Result<Self, QueryCandidateCompositionErrorV1> {
    if maximum_selections == 0 || maximum_retained_bytes == 0 {
      return Err(QueryCandidateCompositionErrorV1::invalid(
        "query_candidate_composition_limits",
        "candidate-composition selection and retained-byte limits must be nonzero",
      ));
    }
    Ok(Self { maximum_selections, maximum_retained_bytes })
  }

  pub const fn maximum_selections(self) -> u64 {
    self.maximum_selections
  }

  pub const fn maximum_retained_bytes(self) -> u64 {
    self.maximum_retained_bytes
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryCandidateCompositionErrorClassV1 {
  InvalidRequest,
  ResourceLimit,
  CorruptPlan,
  Cancelled,
  Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCandidateCompositionErrorV1 {
  class: QueryCandidateCompositionErrorClassV1,
  code: &'static str,
  context: String,
}

impl QueryCandidateCompositionErrorV1 {
  pub const fn class(&self) -> QueryCandidateCompositionErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }

  fn invalid(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryCandidateCompositionErrorClassV1::InvalidRequest, code, context: context.into() }
  }

  fn resource(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryCandidateCompositionErrorClassV1::ResourceLimit, code, context: context.into() }
  }

  fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryCandidateCompositionErrorClassV1::CorruptPlan, code, context: context.into() }
  }

  fn cancelled() -> Self {
    Self {
      class: QueryCandidateCompositionErrorClassV1::Cancelled,
      code: "query_candidate_composition_cancelled",
      context: "boolean candidate composition was cancelled".to_string(),
    }
  }

  fn internal(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryCandidateCompositionErrorClassV1::Internal, code, context: context.into() }
  }
}

impl fmt::Display for QueryCandidateCompositionErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for QueryCandidateCompositionErrorV1 {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryBooleanCandidatePlanKindV1 {
  Authoritative,
  Complete,
  Partial,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryCandidateSelectionV1 {
  predicate_index: usize,
  candidate_index: usize,
}

impl QueryCandidateSelectionV1 {
  pub const fn predicate_index(self) -> usize {
    self.predicate_index
  }

  pub const fn candidate_index(self) -> usize {
    self.candidate_index
  }
}

pub struct QueryBooleanCandidatePlanV1 {
  kind: QueryBooleanCandidatePlanKindV1,
  scope_id: Vec<u8>,
  source_namespace_root: Vec<u8>,
  covered_through_publication_sequence: Option<u64>,
  selections: Vec<QueryCandidateSelectionV1>,
  estimated_work: u64,
  retained_bytes: u64,
  _memory: Option<MemoryReservation>,
}

impl fmt::Debug for QueryBooleanCandidatePlanV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("QueryBooleanCandidatePlanV1")
      .field("kind", &self.kind)
      .field("scope_id", &hex::encode(&self.scope_id))
      .field("source_namespace_root", &hex::encode(&self.source_namespace_root))
      .field("covered_through_publication_sequence", &self.covered_through_publication_sequence)
      .field("selections", &self.selections)
      .field("estimated_work", &self.estimated_work)
      .field("retained_bytes", &self.retained_bytes)
      .finish_non_exhaustive()
  }
}

impl QueryBooleanCandidatePlanV1 {
  pub const fn kind(&self) -> QueryBooleanCandidatePlanKindV1 {
    self.kind
  }

  pub fn scope_id(&self) -> Option<&[u8]> {
    (self.kind == QueryBooleanCandidatePlanKindV1::Partial).then_some(self.scope_id.as_slice())
  }

  pub fn source_namespace_root(&self) -> Option<&[u8]> {
    (self.kind == QueryBooleanCandidatePlanKindV1::Partial).then_some(self.source_namespace_root.as_slice())
  }

  pub const fn covered_through_publication_sequence(&self) -> Option<u64> {
    self.covered_through_publication_sequence
  }

  pub fn selections(&self) -> &[QueryCandidateSelectionV1] {
    &self.selections
  }

  pub const fn estimated_work(&self) -> u64 {
    self.estimated_work
  }

  pub const fn retained_bytes(&self) -> u64 {
    self.retained_bytes
  }
}

#[derive(Clone, Copy)]
struct PartialBasisV1<'a> {
  source_namespace_root: &'a [u8],
  covered_through_publication_sequence: u64,
}

impl PartialEq for PartialBasisV1<'_> {
  fn eq(&self, other: &Self) -> bool {
    self.source_namespace_root == other.source_namespace_root
      && self.covered_through_publication_sequence == other.covered_through_publication_sequence
  }
}

#[derive(Clone, Copy)]
enum NodeCandidateKindV1<'a> {
  Universe,
  Complete,
  Partial(PartialBasisV1<'a>),
}

#[derive(Clone, Copy)]
struct NodeCandidatePlanV1<'a> {
  kind: NodeCandidateKindV1<'a>,
  estimated_work: u64,
  selection_start: usize,
  selection_count: usize,
}

pub fn compose_boolean_candidate_plan_v1(
  plan: &CompiledRootAwareQueryPlanV1,
  scope_id: &[u8],
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  limits: QueryCandidateCompositionLimitsV1,
) -> Result<QueryBooleanCandidatePlanV1, QueryCandidateCompositionErrorV1> {
  require_not_cancelled(cancellation)?;
  validate_scope_id(plan, scope_id)?;
  let selection_capacity = usize::try_from(limits.maximum_selections).map_err(|error| {
    QueryCandidateCompositionErrorV1::invalid(
      "query_candidate_composition_selection_limit",
      format!("selection limit exceeds the platform address space: {error}"),
    )
  })?;
  let requested_retained_bytes = composition_retained_bytes(plan, scope_id, selection_capacity)?;
  if requested_retained_bytes > limits.maximum_retained_bytes {
    return Err(QueryCandidateCompositionErrorV1::invalid(
      "query_candidate_composition_memory_limit",
      format!("candidate composition requires {requested_retained_bytes} bytes but admits only {}", limits.maximum_retained_bytes),
    ));
  }
  let mut reservation = reserve_query_memory(memory, requested_retained_bytes)?;
  let mut selections = Vec::new();
  selections.try_reserve_exact(selection_capacity).map_err(|error| {
    QueryCandidateCompositionErrorV1::resource(
      "query_candidate_composition_allocation",
      format!("cannot reserve the bounded candidate-selection workspace: {error}"),
    )
  })?;
  let retained_bytes = composition_retained_bytes(plan, scope_id, selections.capacity())?;
  if retained_bytes > limits.maximum_retained_bytes {
    return Err(QueryCandidateCompositionErrorV1::resource(
      "query_candidate_composition_allocator_capacity",
      format!("allocator capacity requires {retained_bytes} bytes but the composition admits only {}", limits.maximum_retained_bytes),
    ));
  }
  if let Some(additional) = retained_bytes.checked_sub(requested_retained_bytes).filter(|additional| *additional > 0) {
    reservation.grow(additional).map_err(map_memory_error)?;
  }
  let node = compose_expression(plan, plan.expression(), scope_id, &mut selections, limits.maximum_selections, cancellation)?;
  require_not_cancelled(cancellation)?;
  if node.selection_start != 0 || node.selection_count != selections.len() {
    return Err(QueryCandidateCompositionErrorV1::internal(
      "query_candidate_composition_workspace",
      "top-level candidate selection does not occupy the complete retained workspace",
    ));
  }

  match node.kind {
    NodeCandidateKindV1::Universe => {
      drop(selections);
      drop(reservation);
      Ok(empty_plan(QueryBooleanCandidatePlanKindV1::Authoritative, node.estimated_work))
    }
    NodeCandidateKindV1::Complete => {
      drop(selections);
      drop(reservation);
      Ok(empty_plan(QueryBooleanCandidatePlanKindV1::Complete, node.estimated_work))
    }
    NodeCandidateKindV1::Partial(basis) => {
      selections.sort_unstable_by_key(|selection| (selection.predicate_index, selection.candidate_index));
      selections.dedup();
      if selections.is_empty() {
        return Err(QueryCandidateCompositionErrorV1::internal(
          "query_candidate_composition_empty_partial",
          "partial candidate composition retained no selected candidates",
        ));
      }
      let scope_id = try_clone_bytes(scope_id, "scope id")?;
      let source_namespace_root = try_clone_bytes(basis.source_namespace_root, "source NamespaceRoot")?;
      Ok(QueryBooleanCandidatePlanV1 {
        kind: QueryBooleanCandidatePlanKindV1::Partial,
        scope_id,
        source_namespace_root,
        covered_through_publication_sequence: Some(basis.covered_through_publication_sequence),
        selections,
        estimated_work: node.estimated_work,
        retained_bytes,
        _memory: Some(reservation),
      })
    }
  }
}

fn empty_plan(kind: QueryBooleanCandidatePlanKindV1, estimated_work: u64) -> QueryBooleanCandidatePlanV1 {
  QueryBooleanCandidatePlanV1 {
    kind,
    scope_id: Vec::new(),
    source_namespace_root: Vec::new(),
    covered_through_publication_sequence: None,
    selections: Vec::new(),
    estimated_work,
    retained_bytes: 0,
    _memory: None,
  }
}

fn compose_expression<'a>(
  plan: &'a CompiledRootAwareQueryPlanV1,
  expression: &'a CompiledQueryExpressionV1,
  scope_id: &[u8],
  selections: &mut Vec<QueryCandidateSelectionV1>,
  maximum_selections: u64,
  cancellation: &CancellationToken,
) -> Result<NodeCandidatePlanV1<'a>, QueryCandidateCompositionErrorV1> {
  require_not_cancelled(cancellation)?;
  match expression {
    CompiledQueryExpressionV1::Field(predicate_index) => compose_field(plan, *predicate_index, scope_id, selections, maximum_selections),
    CompiledQueryExpressionV1::And(children) => compose_and(plan, children, scope_id, selections, maximum_selections, cancellation),
    CompiledQueryExpressionV1::Or(children) => compose_or(plan, children, scope_id, selections, maximum_selections, cancellation),
    CompiledQueryExpressionV1::Not(_) => Ok(NodeCandidatePlanV1 {
      kind: NodeCandidateKindV1::Universe,
      estimated_work: u64::MAX,
      selection_start: selections.len(),
      selection_count: 0,
    }),
  }
}

fn compose_field<'a>(
  plan: &'a CompiledRootAwareQueryPlanV1,
  predicate_index: usize,
  scope_id: &[u8],
  selections: &mut Vec<QueryCandidateSelectionV1>,
  maximum_selections: u64,
) -> Result<NodeCandidatePlanV1<'a>, QueryCandidateCompositionErrorV1> {
  let predicate = plan.predicates().get(predicate_index).ok_or_else(|| {
    QueryCandidateCompositionErrorV1::corrupt(
      "query_candidate_composition_predicate",
      "compiled expression predicate index is out of bounds",
    )
  })?;
  let scope = predicate.scopes().iter().find(|scope| scope.scope_id() == scope_id).ok_or_else(|| {
    QueryCandidateCompositionErrorV1::corrupt(
      "query_candidate_composition_scope",
      "compiled predicate omits the effective scope being composed",
    )
  })?;
  let start = selections.len();
  match scope.driver() {
    QueryPlanDriverV1::Authoritative { estimated_work } => Ok(NodeCandidatePlanV1 {
      kind: NodeCandidateKindV1::Universe,
      estimated_work: *estimated_work,
      selection_start: start,
      selection_count: 0,
    }),
    QueryPlanDriverV1::Index { candidate_index: _, coverage: CompiledQueryCoverageV1::Complete, estimated_work }
    | QueryPlanDriverV1::IndexUnion { candidate_indexes: _, coverage: CompiledQueryCoverageV1::Complete, estimated_work } => {
      Ok(NodeCandidatePlanV1 {
        kind: NodeCandidateKindV1::Complete,
        estimated_work: *estimated_work,
        selection_start: start,
        selection_count: 0,
      })
    }
    QueryPlanDriverV1::Index { candidate_index, coverage: CompiledQueryCoverageV1::PartialExact, estimated_work } => {
      let basis = partial_candidate_basis(plan, scope, predicate_index, *candidate_index)?;
      append_selection(selections, predicate_index, *candidate_index, maximum_selections)?;
      Ok(NodeCandidatePlanV1 {
        kind: NodeCandidateKindV1::Partial(basis),
        estimated_work: *estimated_work,
        selection_start: start,
        selection_count: 1,
      })
    }
    QueryPlanDriverV1::IndexUnion { candidate_indexes, coverage: CompiledQueryCoverageV1::PartialExact, estimated_work } => {
      if candidate_indexes.is_empty() {
        return Err(QueryCandidateCompositionErrorV1::corrupt(
          "query_candidate_composition_union",
          "compiled partial index union is empty",
        ));
      }
      let mut basis = None;
      for candidate_index in candidate_indexes {
        let candidate_basis = partial_candidate_basis(plan, scope, predicate_index, *candidate_index)?;
        if basis.is_some_and(|basis| basis != candidate_basis) {
          selections.truncate(start);
          return Ok(NodeCandidatePlanV1 {
            kind: NodeCandidateKindV1::Universe,
            estimated_work: *estimated_work,
            selection_start: start,
            selection_count: 0,
          });
        }
        basis = Some(candidate_basis);
        append_selection(selections, predicate_index, *candidate_index, maximum_selections)?;
      }
      Ok(NodeCandidatePlanV1 {
        kind: NodeCandidateKindV1::Partial(basis.ok_or_else(|| {
          QueryCandidateCompositionErrorV1::internal("query_candidate_composition_union", "partial union lost its basis")
        })?),
        estimated_work: *estimated_work,
        selection_start: start,
        selection_count: selections.len() - start,
      })
    }
    QueryPlanDriverV1::Index { coverage: CompiledQueryCoverageV1::AuthoritativeOnly, .. }
    | QueryPlanDriverV1::IndexUnion { coverage: CompiledQueryCoverageV1::AuthoritativeOnly, .. } => {
      Err(QueryCandidateCompositionErrorV1::corrupt(
        "query_candidate_composition_driver",
        "compiled driver selected an authoritative-only index candidate",
      ))
    }
  }
}

fn compose_and<'a>(
  plan: &'a CompiledRootAwareQueryPlanV1,
  children: &'a [CompiledQueryExpressionV1],
  scope_id: &[u8],
  selections: &mut Vec<QueryCandidateSelectionV1>,
  maximum_selections: u64,
  cancellation: &CancellationToken,
) -> Result<NodeCandidatePlanV1<'a>, QueryCandidateCompositionErrorV1> {
  if children.is_empty() {
    return Err(QueryCandidateCompositionErrorV1::corrupt("query_candidate_composition_and", "compiled AND expression has no children"));
  }
  let start = selections.len();
  let mut best: Option<NodeCandidatePlanV1<'_>> = None;
  for child in children {
    let preserved = match best {
      Some(candidate) => candidate.selection_count,
      None => 0,
    };
    selections.truncate(start + preserved);
    let child_plan = compose_expression(plan, child, scope_id, selections, maximum_selections, cancellation)?;
    if finite_kind(child_plan.kind) && best.is_none_or(|current| candidate_precedes(child_plan, current)) {
      if child_plan.selection_count > 0 {
        let child_end = child_plan.selection_start.checked_add(child_plan.selection_count).ok_or_else(|| {
          QueryCandidateCompositionErrorV1::internal("query_candidate_composition_workspace", "selection range overflowed")
        })?;
        selections.copy_within(child_plan.selection_start..child_end, start);
      }
      selections.truncate(start + child_plan.selection_count);
      best = Some(NodeCandidatePlanV1 { selection_start: start, ..child_plan });
    } else {
      selections.truncate(start + preserved);
    }
  }
  match best {
    Some(best) => Ok(best),
    None => {
      Ok(NodeCandidatePlanV1 { kind: NodeCandidateKindV1::Universe, estimated_work: u64::MAX, selection_start: start, selection_count: 0 })
    }
  }
}

fn compose_or<'a>(
  plan: &'a CompiledRootAwareQueryPlanV1,
  children: &'a [CompiledQueryExpressionV1],
  scope_id: &[u8],
  selections: &mut Vec<QueryCandidateSelectionV1>,
  maximum_selections: u64,
  cancellation: &CancellationToken,
) -> Result<NodeCandidatePlanV1<'a>, QueryCandidateCompositionErrorV1> {
  if children.is_empty() {
    return Err(QueryCandidateCompositionErrorV1::corrupt("query_candidate_composition_or", "compiled OR expression has no children"));
  }
  let start = selections.len();
  let mut combined_kind = None;
  let mut estimated_work = 0u64;
  for child in children {
    let child_plan = compose_expression(plan, child, scope_id, selections, maximum_selections, cancellation)?;
    estimated_work = estimated_work.saturating_add(child_plan.estimated_work);
    combined_kind = match (combined_kind, child_plan.kind) {
      (_, NodeCandidateKindV1::Universe) => {
        selections.truncate(start);
        return Ok(NodeCandidatePlanV1 { kind: NodeCandidateKindV1::Universe, estimated_work, selection_start: start, selection_count: 0 });
      }
      (None, kind) => Some(kind),
      (Some(NodeCandidateKindV1::Complete), NodeCandidateKindV1::Complete) => Some(NodeCandidateKindV1::Complete),
      (Some(NodeCandidateKindV1::Partial(basis)), NodeCandidateKindV1::Partial(child_basis)) if basis == child_basis => {
        Some(NodeCandidateKindV1::Partial(basis))
      }
      _ => {
        selections.truncate(start);
        return Ok(NodeCandidatePlanV1 { kind: NodeCandidateKindV1::Universe, estimated_work, selection_start: start, selection_count: 0 });
      }
    };
  }
  let kind = combined_kind
    .ok_or_else(|| QueryCandidateCompositionErrorV1::internal("query_candidate_composition_or", "OR composition lost every child"))?;
  if matches!(kind, NodeCandidateKindV1::Complete) {
    selections.truncate(start);
  }
  Ok(NodeCandidatePlanV1 { kind, estimated_work, selection_start: start, selection_count: selections.len() - start })
}

fn partial_candidate_basis<'a>(
  plan: &'a CompiledRootAwareQueryPlanV1,
  scope: &'a CompiledQueryScopePlanV1,
  predicate_index: usize,
  candidate_index: usize,
) -> Result<PartialBasisV1<'a>, QueryCandidateCompositionErrorV1> {
  let candidate = scope.candidates().get(candidate_index).ok_or_else(|| {
    QueryCandidateCompositionErrorV1::corrupt(
      "query_candidate_composition_candidate",
      format!("predicate {predicate_index} selected candidate index {candidate_index} outside its compiled candidates"),
    )
  })?;
  if candidate.coverage() != CompiledQueryCoverageV1::PartialExact || !candidate.proven_candidate_superset() {
    return Err(QueryCandidateCompositionErrorV1::corrupt(
      "query_candidate_composition_candidate",
      "compiled partial driver does not reference a proven partial candidate superset",
    ));
  }
  let generation = candidate.selected_generation().ok_or_else(|| {
    QueryCandidateCompositionErrorV1::corrupt(
      "query_candidate_composition_generation",
      "compiled partial candidate omits its selected generation",
    )
  })?;
  let width = plan.hash_algorithm().hash_length();
  if generation.source_namespace_root.len() != width
    || generation.source_namespace_root.iter().all(|byte| *byte == 0)
    || generation.coverage_publication_sequence == 0
    || generation.coverage_publication_sequence >= plan.publication_sequence()
  {
    return Err(QueryCandidateCompositionErrorV1::corrupt(
      "query_candidate_composition_generation",
      "compiled partial generation has an invalid source root or coverage sequence",
    ));
  }
  Ok(PartialBasisV1 {
    source_namespace_root: &generation.source_namespace_root,
    covered_through_publication_sequence: generation.coverage_publication_sequence,
  })
}

fn append_selection(
  selections: &mut Vec<QueryCandidateSelectionV1>,
  predicate_index: usize,
  candidate_index: usize,
  maximum_selections: u64,
) -> Result<(), QueryCandidateCompositionErrorV1> {
  let retained = u64::try_from(selections.len()).map_err(|error| {
    QueryCandidateCompositionErrorV1::resource(
      "query_candidate_composition_selection_limit",
      format!("selection count exceeds u64: {error}"),
    )
  })?;
  if retained >= maximum_selections {
    return Err(QueryCandidateCompositionErrorV1::resource(
      "query_candidate_composition_selection_limit",
      "boolean candidate composition exceeds its admitted selection count",
    ));
  }
  selections.push(QueryCandidateSelectionV1 { predicate_index, candidate_index });
  Ok(())
}

fn finite_kind(kind: NodeCandidateKindV1<'_>) -> bool {
  !matches!(kind, NodeCandidateKindV1::Universe)
}

fn candidate_precedes(left: NodeCandidatePlanV1<'_>, right: NodeCandidatePlanV1<'_>) -> bool {
  left.estimated_work < right.estimated_work
    || (left.estimated_work == right.estimated_work
      && matches!(left.kind, NodeCandidateKindV1::Complete)
      && matches!(right.kind, NodeCandidateKindV1::Partial(_)))
}

fn validate_scope_id(plan: &CompiledRootAwareQueryPlanV1, scope_id: &[u8]) -> Result<(), QueryCandidateCompositionErrorV1> {
  let width = plan.hash_algorithm().hash_length();
  if scope_id.len() != width || scope_id.iter().all(|byte| *byte == 0) {
    return Err(QueryCandidateCompositionErrorV1::invalid(
      "query_candidate_composition_scope",
      "scope id is all-zero or does not match the database hash width",
    ));
  }
  Ok(())
}

fn composition_retained_bytes(
  plan: &CompiledRootAwareQueryPlanV1,
  scope_id: &[u8],
  selection_capacity: usize,
) -> Result<u64, QueryCandidateCompositionErrorV1> {
  let selection_width = u64::try_from(size_of::<QueryCandidateSelectionV1>()).map_err(|error| {
    QueryCandidateCompositionErrorV1::invalid(
      "query_candidate_composition_memory_limit",
      format!("candidate-selection width exceeds u64: {error}"),
    )
  })?;
  let selection_count = u64::try_from(selection_capacity).map_err(|error| {
    QueryCandidateCompositionErrorV1::invalid(
      "query_candidate_composition_memory_limit",
      format!("candidate-selection capacity exceeds u64: {error}"),
    )
  })?;
  let selection_bytes = selection_count.checked_mul(selection_width).ok_or_else(|| {
    QueryCandidateCompositionErrorV1::invalid("query_candidate_composition_memory_limit", "candidate-selection memory bound overflowed")
  })?;
  let scope_bytes = u64::try_from(scope_id.len()).map_err(|error| {
    QueryCandidateCompositionErrorV1::invalid("query_candidate_composition_memory_limit", format!("scope id length exceeds u64: {error}"))
  })?;
  let hash_bytes = u64::try_from(plan.hash_algorithm().hash_length()).map_err(|error| {
    QueryCandidateCompositionErrorV1::invalid("query_candidate_composition_memory_limit", format!("hash width exceeds u64: {error}"))
  })?;
  COMPOSITION_BASE_BYTES
    .checked_add(selection_bytes)
    .and_then(|bytes| bytes.checked_add(scope_bytes))
    .and_then(|bytes| bytes.checked_add(hash_bytes))
    .ok_or_else(|| {
      QueryCandidateCompositionErrorV1::invalid(
        "query_candidate_composition_memory_limit",
        "candidate-composition retained-byte bound overflowed",
      )
    })
}

fn reserve_query_memory(memory: &MemoryCoordinator, bytes: u64) -> Result<MemoryReservation, QueryCandidateCompositionErrorV1> {
  memory.reserve(MemoryOwner::Query, bytes, AdmissionClass::Workload).map_err(map_memory_error)
}

fn map_memory_error(error: MemoryCoordinatorError) -> QueryCandidateCompositionErrorV1 {
  match error {
    MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. } => {
      QueryCandidateCompositionErrorV1::resource("query_candidate_composition_memory", error.to_string())
    }
    _ => QueryCandidateCompositionErrorV1::internal("query_candidate_composition_memory_authority", error.to_string()),
  }
}

fn try_clone_bytes(value: &[u8], label: &'static str) -> Result<Vec<u8>, QueryCandidateCompositionErrorV1> {
  let mut copy = Vec::new();
  copy.try_reserve_exact(value.len()).map_err(|error| {
    QueryCandidateCompositionErrorV1::resource("query_candidate_composition_allocation", format!("cannot retain {label}: {error}"))
  })?;
  copy.extend_from_slice(value);
  Ok(copy)
}

fn require_not_cancelled(cancellation: &CancellationToken) -> Result<(), QueryCandidateCompositionErrorV1> {
  if cancellation.is_cancelled() {
    Err(QueryCandidateCompositionErrorV1::cancelled())
  } else {
    Ok(())
  }
}
