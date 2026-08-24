//! Exact scope-local query execution across authoritative and derived paths.
//!
//! Candidate composition and immutable index artifacts select work only. Every
//! derived refusal covered by this module is retried through the authoritative
//! selected-root evaluator, and the discarded path remains available as a
//! diagnostic instead of being mistaken for an empty result.

use std::error::Error;
use std::fmt;
use std::slice;

use tokio_util::sync::CancellationToken;

use crate::engine::memory_coordinator::MemoryCoordinator;

use super::index_partial_acceleration::{
  ExactPartialIndexAccelerationV1, IndexChangedDocumentSourceV1, IndexMatchedDocumentIdentityV1, IndexPartialAccelerationDiagnosticV1,
  IndexPartialAccelerationErrorV1, IndexPartialAccelerationLimitsV1, IndexPartialAccelerationOutcomeV1, IndexPartialCandidateRecheckerV1,
  IndexPartialAccelerationFallbackReasonV1,
};
use super::query_candidate_composition::{
  QueryBooleanCandidatePlanKindV1, QueryCandidateCompositionErrorClassV1, QueryCandidateCompositionErrorV1,
  QueryCandidateCompositionLimitsV1, compose_boolean_candidate_plan_v1,
};
use super::query_complete_candidate::{
  QueryCompleteCandidateExecutionV1, QueryCompleteCandidateLimitsV1, QueryCompleteCandidateScopeExecutionRequestV1,
  QueryCompleteCandidateSourceV1, execute_complete_candidate_scope_query_v1,
};
use super::query_executor::{
  QueryAuthoritativeScopeSourceV1, QueryExecutionErrorClassV1, QueryExecutionErrorV1, QueryExecutionLimitsV1, QueryExecutionMatchV1,
  RootAwareQueryExecutionV1, RootAwareQueryScopeExecutionRequestV1, execute_authoritative_scope_query_v1,
};
use super::query_partial_candidate::{
  QueryComposedPartialCandidateExecutionRequestV1, QueryPartialCandidateArtifactSourceV1, execute_composed_partial_candidates_v1,
};
use super::query_planner::{CompiledRootAwareQueryPlanV1, RootAwareQueryFieldCatalogV1};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryExactScopeExecutionPathV1 {
  Authoritative,
  Complete,
  Partial,
  CompositionFallback,
  CompleteFallback,
  PartialFallback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryExactScopeFallbackDiagnosticV1 {
  Composition(QueryCandidateCompositionErrorV1),
  Complete(QueryExecutionErrorV1),
  Partial { reason: IndexPartialAccelerationFallbackReasonV1, diagnostic: IndexPartialAccelerationDiagnosticV1 },
}

#[derive(Debug)]
pub enum QueryExactScopeExecutionErrorV1 {
  InvalidRequest { code: &'static str, context: &'static str },
  Composition(QueryCandidateCompositionErrorV1),
  Execution(QueryExecutionErrorV1),
  Partial(IndexPartialAccelerationErrorV1),
  AuthoritativeFallbackFailed { diagnostic: QueryExactScopeFallbackDiagnosticV1, authoritative: QueryExecutionErrorV1 },
}

impl fmt::Display for QueryExactScopeExecutionErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::InvalidRequest { code, context } => write!(formatter, "{code}: {context}"),
      Self::Composition(error) => write!(formatter, "{error}"),
      Self::Execution(error) => write!(formatter, "{error}"),
      Self::Partial(error) => write!(formatter, "{error}"),
      Self::AuthoritativeFallbackFailed { diagnostic, authoritative } => {
        write!(formatter, "authoritative fallback failed after {diagnostic:?}: {authoritative}")
      }
    }
  }
}

impl Error for QueryExactScopeExecutionErrorV1 {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::InvalidRequest { .. } => None,
      Self::Composition(error) => Some(error),
      Self::Execution(error) => Some(error),
      Self::Partial(error) => Some(error),
      Self::AuthoritativeFallbackFailed { authoritative, .. } => Some(authoritative),
    }
  }
}

pub struct QueryExactScopeExecutionRequestV1<'a> {
  pub plan: &'a CompiledRootAwareQueryPlanV1,
  pub catalogs: &'a [RootAwareQueryFieldCatalogV1],
  pub scope_id: &'a [u8],
  pub authoritative_source: &'a mut dyn QueryAuthoritativeScopeSourceV1,
  pub complete_source: Option<&'a mut dyn QueryCompleteCandidateSourceV1>,
  pub partial_source: Option<&'a mut dyn QueryPartialCandidateArtifactSourceV1>,
  pub complement: Option<&'a mut dyn IndexChangedDocumentSourceV1>,
  pub rechecker: Option<&'a mut dyn IndexPartialCandidateRecheckerV1>,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub execution_limits: QueryExecutionLimitsV1,
  pub candidate_limits: QueryCompleteCandidateLimitsV1,
  pub acceleration_limits: IndexPartialAccelerationLimitsV1,
  pub composition_limits: QueryCandidateCompositionLimitsV1,
}

enum QueryExactScopeRetainedExecutionV1 {
  Authoritative(RootAwareQueryExecutionV1),
  Complete(QueryCompleteCandidateExecutionV1),
  Partial(ExactPartialIndexAccelerationV1),
}

pub struct QueryExactScopeExecutionV1 {
  scope_id: [u8; 64],
  scope_id_length: usize,
  path: QueryExactScopeExecutionPathV1,
  fallback_diagnostic: Option<QueryExactScopeFallbackDiagnosticV1>,
  retained: QueryExactScopeRetainedExecutionV1,
}

impl fmt::Debug for QueryExactScopeExecutionV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("QueryExactScopeExecutionV1")
      .field("scope_id", &hex::encode(self.scope_id()))
      .field("path", &self.path)
      .field("fallback_diagnostic", &self.fallback_diagnostic)
      .field("match_count", &self.match_count())
      .finish_non_exhaustive()
  }
}

impl QueryExactScopeExecutionV1 {
  pub fn scope_id(&self) -> &[u8] {
    &self.scope_id[..self.scope_id_length]
  }

  pub fn selected_namespace_root(&self) -> &[u8] {
    match &self.retained {
      QueryExactScopeRetainedExecutionV1::Authoritative(execution) => execution.selected_namespace_root(),
      QueryExactScopeRetainedExecutionV1::Complete(execution) => execution.execution().selected_namespace_root(),
      QueryExactScopeRetainedExecutionV1::Partial(execution) => execution.proof().target_namespace_root(),
    }
  }

  pub const fn retained_bytes(&self) -> u64 {
    match &self.retained {
      QueryExactScopeRetainedExecutionV1::Authoritative(execution) => execution.retained_bytes(),
      QueryExactScopeRetainedExecutionV1::Complete(execution) => execution.execution().retained_bytes(),
      QueryExactScopeRetainedExecutionV1::Partial(execution) => execution.retained_bytes(),
    }
  }

  pub const fn path(&self) -> QueryExactScopeExecutionPathV1 {
    self.path
  }

  pub const fn fallback_diagnostic(&self) -> Option<&QueryExactScopeFallbackDiagnosticV1> {
    self.fallback_diagnostic.as_ref()
  }

  pub fn match_count(&self) -> usize {
    match &self.retained {
      QueryExactScopeRetainedExecutionV1::Authoritative(execution) => execution.matches().len(),
      QueryExactScopeRetainedExecutionV1::Complete(execution) => execution.execution().matches().len(),
      QueryExactScopeRetainedExecutionV1::Partial(execution) => execution.matches().len(),
    }
  }

  pub fn identities(&self) -> QueryExactScopeIdentityIterV1<'_> {
    match &self.retained {
      QueryExactScopeRetainedExecutionV1::Authoritative(execution) => {
        QueryExactScopeIdentityIterV1::Authoritative(execution.matches().iter())
      }
      QueryExactScopeRetainedExecutionV1::Complete(execution) => {
        QueryExactScopeIdentityIterV1::Authoritative(execution.execution().matches().iter())
      }
      QueryExactScopeRetainedExecutionV1::Partial(execution) => QueryExactScopeIdentityIterV1::Partial(execution.matches().iter()),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryExactScopeIdentityRefV1<'a> {
  file_key: &'a [u8],
  record_revision: &'a [u8],
}

impl QueryExactScopeIdentityRefV1<'_> {
  pub const fn file_key(&self) -> &[u8] {
    self.file_key
  }

  pub const fn record_revision(&self) -> &[u8] {
    self.record_revision
  }
}

pub enum QueryExactScopeIdentityIterV1<'a> {
  Authoritative(slice::Iter<'a, QueryExecutionMatchV1>),
  Partial(slice::Iter<'a, IndexMatchedDocumentIdentityV1>),
}

impl<'a> Iterator for QueryExactScopeIdentityIterV1<'a> {
  type Item = QueryExactScopeIdentityRefV1<'a>;

  fn next(&mut self) -> Option<Self::Item> {
    match self {
      Self::Authoritative(rows) => {
        rows.next().map(|row| QueryExactScopeIdentityRefV1 { file_key: row.file_key(), record_revision: row.record_revision() })
      }
      Self::Partial(rows) => {
        rows.next().map(|row| QueryExactScopeIdentityRefV1 { file_key: row.file_key(), record_revision: row.record_revision_hash() })
      }
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    let length = match self {
      Self::Authoritative(rows) => rows.len(),
      Self::Partial(rows) => rows.len(),
    };
    (length, Some(length))
  }
}

impl ExactSizeIterator for QueryExactScopeIdentityIterV1<'_> {}

pub fn execute_exact_query_scope_v1(
  request: QueryExactScopeExecutionRequestV1<'_>,
) -> Result<QueryExactScopeExecutionV1, QueryExactScopeExecutionErrorV1> {
  let QueryExactScopeExecutionRequestV1 {
    plan,
    catalogs,
    scope_id,
    authoritative_source,
    complete_source,
    partial_source,
    complement,
    rechecker,
    memory,
    cancellation,
    execution_limits,
    candidate_limits,
    acceleration_limits,
    composition_limits,
  } = request;
  let retained_scope_id = retain_scope_id(plan, scope_id)?;

  let candidate_plan = match compose_boolean_candidate_plan_v1(plan, scope_id, memory, cancellation, composition_limits) {
    Ok(plan) => plan,
    Err(error) if error.class() == QueryCandidateCompositionErrorClassV1::ResourceLimit => {
      let diagnostic = QueryExactScopeFallbackDiagnosticV1::Composition(error);
      return execute_authoritative_fallback(
        plan,
        catalogs,
        scope_id,
        authoritative_source,
        memory,
        cancellation,
        execution_limits,
        QueryExactScopeExecutionPathV1::CompositionFallback,
        diagnostic,
        retained_scope_id,
      );
    }
    Err(error) => return Err(QueryExactScopeExecutionErrorV1::Composition(error)),
  };

  match candidate_plan.kind() {
    QueryBooleanCandidatePlanKindV1::Authoritative => {
      let execution = execute_authoritative_scope(plan, catalogs, scope_id, authoritative_source, memory, cancellation, execution_limits)
        .map_err(QueryExactScopeExecutionErrorV1::Execution)?;
      Ok(exact_scope_execution(
        retained_scope_id,
        QueryExactScopeExecutionPathV1::Authoritative,
        None,
        QueryExactScopeRetainedExecutionV1::Authoritative(execution),
      ))
    }
    QueryBooleanCandidatePlanKindV1::Complete => {
      let source = complete_source.ok_or(QueryExactScopeExecutionErrorV1::InvalidRequest {
        code: "query_scope_complete_source",
        context: "complete scope execution requires a complete-candidate source",
      })?;
      match execute_complete_candidate_scope_query_v1(QueryCompleteCandidateScopeExecutionRequestV1 {
        plan,
        catalogs,
        scope_id,
        source,
        memory,
        cancellation,
        execution_limits,
        candidate_limits,
      }) {
        Ok(execution) => Ok(exact_scope_execution(
          retained_scope_id,
          QueryExactScopeExecutionPathV1::Complete,
          None,
          QueryExactScopeRetainedExecutionV1::Complete(execution),
        )),
        Err(error) if complete_failure_can_retry_authoritatively(&error) => execute_authoritative_fallback(
          plan,
          catalogs,
          scope_id,
          authoritative_source,
          memory,
          cancellation,
          execution_limits,
          QueryExactScopeExecutionPathV1::CompleteFallback,
          QueryExactScopeFallbackDiagnosticV1::Complete(error),
          retained_scope_id,
        ),
        Err(error) => Err(QueryExactScopeExecutionErrorV1::Execution(error)),
      }
    }
    QueryBooleanCandidatePlanKindV1::Partial => {
      let source = partial_source.ok_or(QueryExactScopeExecutionErrorV1::InvalidRequest {
        code: "query_scope_partial_source",
        context: "partial scope execution requires a partial-candidate source",
      })?;
      let complement = complement.ok_or(QueryExactScopeExecutionErrorV1::InvalidRequest {
        code: "query_scope_complement_source",
        context: "partial scope execution requires an exact changed-document complement source",
      })?;
      let rechecker = rechecker.ok_or(QueryExactScopeExecutionErrorV1::InvalidRequest {
        code: "query_scope_partial_rechecker",
        context: "partial scope execution requires a selected-root candidate rechecker",
      })?;
      match execute_composed_partial_candidates_v1(QueryComposedPartialCandidateExecutionRequestV1 {
        plan,
        candidate_plan: &candidate_plan,
        source,
        complement,
        rechecker,
        memory,
        cancellation,
        candidate_limits,
        acceleration_limits,
      })
      .map_err(QueryExactScopeExecutionErrorV1::Partial)?
      {
        IndexPartialAccelerationOutcomeV1::Exact(execution) => Ok(exact_scope_execution(
          retained_scope_id,
          QueryExactScopeExecutionPathV1::Partial,
          None,
          QueryExactScopeRetainedExecutionV1::Partial(execution),
        )),
        IndexPartialAccelerationOutcomeV1::AuthoritativeOnly { reason, diagnostic } => execute_authoritative_fallback(
          plan,
          catalogs,
          scope_id,
          authoritative_source,
          memory,
          cancellation,
          execution_limits,
          QueryExactScopeExecutionPathV1::PartialFallback,
          QueryExactScopeFallbackDiagnosticV1::Partial { reason, diagnostic },
          retained_scope_id,
        ),
      }
    }
  }
}

#[allow(clippy::too_many_arguments)]
fn execute_authoritative_fallback(
  plan: &CompiledRootAwareQueryPlanV1,
  catalogs: &[RootAwareQueryFieldCatalogV1],
  scope_id: &[u8],
  source: &mut dyn QueryAuthoritativeScopeSourceV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  limits: QueryExecutionLimitsV1,
  path: QueryExactScopeExecutionPathV1,
  diagnostic: QueryExactScopeFallbackDiagnosticV1,
  retained_scope_id: ([u8; 64], usize),
) -> Result<QueryExactScopeExecutionV1, QueryExactScopeExecutionErrorV1> {
  match execute_authoritative_scope(plan, catalogs, scope_id, source, memory, cancellation, limits) {
    Ok(execution) => {
      Ok(exact_scope_execution(retained_scope_id, path, Some(diagnostic), QueryExactScopeRetainedExecutionV1::Authoritative(execution)))
    }
    Err(authoritative) => Err(QueryExactScopeExecutionErrorV1::AuthoritativeFallbackFailed { diagnostic, authoritative }),
  }
}

fn exact_scope_execution(
  retained_scope_id: ([u8; 64], usize),
  path: QueryExactScopeExecutionPathV1,
  fallback_diagnostic: Option<QueryExactScopeFallbackDiagnosticV1>,
  retained: QueryExactScopeRetainedExecutionV1,
) -> QueryExactScopeExecutionV1 {
  QueryExactScopeExecutionV1 { scope_id: retained_scope_id.0, scope_id_length: retained_scope_id.1, path, fallback_diagnostic, retained }
}

fn retain_scope_id(plan: &CompiledRootAwareQueryPlanV1, scope_id: &[u8]) -> Result<([u8; 64], usize), QueryExactScopeExecutionErrorV1> {
  if scope_id.len() != plan.hash_algorithm().hash_length() || scope_id.len() > 64 || scope_id.iter().all(|byte| *byte == 0) {
    return Err(QueryExactScopeExecutionErrorV1::InvalidRequest {
      code: "query_scope_identity",
      context: "scope identity is not one nonzero database hash",
    });
  }
  if plan.predicates().first().is_some_and(|predicate| !predicate.scopes().iter().any(|scope| scope.scope_id() == scope_id)) {
    return Err(QueryExactScopeExecutionErrorV1::InvalidRequest {
      code: "query_scope_unknown",
      context: "scope identity is not an effective scope in the compiled query",
    });
  }
  let mut retained = [0u8; 64];
  retained[..scope_id.len()].copy_from_slice(scope_id);
  Ok((retained, scope_id.len()))
}

fn execute_authoritative_scope(
  plan: &CompiledRootAwareQueryPlanV1,
  catalogs: &[RootAwareQueryFieldCatalogV1],
  scope_id: &[u8],
  source: &mut dyn QueryAuthoritativeScopeSourceV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  limits: QueryExecutionLimitsV1,
) -> Result<RootAwareQueryExecutionV1, QueryExecutionErrorV1> {
  execute_authoritative_scope_query_v1(RootAwareQueryScopeExecutionRequestV1 {
    plan,
    catalogs,
    scope_id,
    source,
    memory,
    cancellation,
    limits,
  })
}

fn complete_failure_can_retry_authoritatively(error: &QueryExecutionErrorV1) -> bool {
  matches!(
    error.class(),
    QueryExecutionErrorClassV1::ResourceLimit
      | QueryExecutionErrorClassV1::HistoricalViewUnavailable
      | QueryExecutionErrorClassV1::CorruptSource
  )
}
