//! Planner-bound adapter from partial query artifacts to exact acceleration.
//!
//! This module owns no complement or recheck semantics. It converts one
//! planner-selected partial Posting generation and its source ScopeOrdinal
//! identities into the existing exact partial-acceleration source contract.

use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::MemoryCoordinator;

use super::index_artifact_cursor::ArtifactCursorSourceV1;
use super::index_coverage_planner::IndexCoveragePlanV1;
use super::index_partial_acceleration::{
  IndexAcceleratorCandidateScanReceiptV1, IndexAcceleratorCandidateScanRequestV1, IndexAcceleratorCandidateSourceV1,
  IndexAcceleratorCandidateV1, IndexAcceleratorCandidateVisitorV1, IndexChangedDocumentSourceV1, IndexPartialAccelerationErrorV1,
  IndexPartialAccelerationLimitsV1, IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationRequestV1, IndexPartialCandidateRecheckerV1,
  IndexPartialScanErrorV1, IndexPartialSourceErrorV1, execute_partial_index_acceleration_v1,
};
use super::query_complete_candidate::{
  QueryCandidateArtifactRootV1, QueryCompleteCandidateErrorClassV1, QueryCompleteCandidateErrorV1, QueryCompleteCandidateLimitsV1,
  QueryCompleteScopeResolutionRequestV1, QueryPartialPostingScanRequestV1, QueryScopeOrdinalSelectionV1,
  resolve_complete_scope_identities_v1, scan_partial_posting_ordinals_v1,
};
use super::query_planner::{CompiledQueryCoverageV1, CompiledQueryIndexCandidateV1, CompiledRootAwareQueryPlanV1, QueryPlanDriverV1};

#[derive(Clone, Copy)]
pub struct QueryPartialPostingRootRequestV1<'a> {
  pub target_namespace_root: &'a [u8],
  pub target_publication_sequence: u64,
  pub source_namespace_root: &'a [u8],
  pub source_publication_sequence: u64,
  pub scope_id: &'a [u8],
  pub candidate: &'a CompiledQueryIndexCandidateV1,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPartialPostingRootReceiptV1 {
  pub target_namespace_root: Vec<u8>,
  pub target_publication_sequence: u64,
  pub source_namespace_root: Vec<u8>,
  pub source_publication_sequence: u64,
  pub scope_id: Vec<u8>,
  pub index_id: Vec<u8>,
  pub generation: u64,
  pub generation_manifest_hash: Vec<u8>,
  pub root: Option<QueryCandidateArtifactRootV1>,
  pub complete: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct QueryPartialScopeRootRequestV1<'a> {
  pub source_namespace_root: &'a [u8],
  pub source_publication_sequence: u64,
  pub scope_id: &'a [u8],
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPartialScopeRootReceiptV1 {
  pub source_namespace_root: Vec<u8>,
  pub source_publication_sequence: u64,
  pub scope_id: Vec<u8>,
  pub root: Option<QueryCandidateArtifactRootV1>,
  pub complete: bool,
}

pub trait QueryPartialCandidateArtifactSourceV1: ArtifactCursorSourceV1 {
  fn resolve_partial_posting_root(
    &mut self,
    request: QueryPartialPostingRootRequestV1<'_>,
  ) -> Result<QueryPartialPostingRootReceiptV1, IndexPartialSourceErrorV1>;

  fn resolve_partial_scope_root(
    &mut self,
    request: QueryPartialScopeRootRequestV1<'_>,
  ) -> Result<QueryPartialScopeRootReceiptV1, IndexPartialSourceErrorV1>;
}

pub struct QueryPartialCandidateExecutionRequestV1<'a> {
  pub plan: &'a CompiledRootAwareQueryPlanV1,
  pub predicate_index: usize,
  pub scope_id: &'a [u8],
  pub candidate_index: usize,
  pub source: &'a mut dyn QueryPartialCandidateArtifactSourceV1,
  pub complement: &'a mut dyn IndexChangedDocumentSourceV1,
  pub rechecker: &'a mut dyn IndexPartialCandidateRecheckerV1,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub candidate_limits: QueryCompleteCandidateLimitsV1,
  pub acceleration_limits: IndexPartialAccelerationLimitsV1,
}

pub fn execute_planned_partial_candidate_v1(
  request: QueryPartialCandidateExecutionRequestV1<'_>,
) -> Result<IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationErrorV1> {
  let candidate = selected_partial_candidate(request.plan, request.predicate_index, request.scope_id, request.candidate_index)?;
  if request.acceleration_limits.maximum_accelerator_candidates() > request.candidate_limits.maximum_candidate_documents() {
    return Err(IndexPartialAccelerationErrorV1::invalid(
      "query_partial_candidate_limits",
      "partial acceleration admits more candidates than its artifact scanner",
    ));
  }
  let generation = candidate.selected_generation().ok_or_else(|| {
    IndexPartialAccelerationErrorV1::invalid("query_partial_candidate_generation", "partial query candidate has no selected generation")
  })?;
  let coverage_generation = generation.as_coverage_generation();
  let coverage_plan = IndexCoveragePlanV1::PartialCandidate {
    generation: coverage_generation,
    target_namespace_root: request.plan.selected_namespace_root(),
    target_publication_sequence: request.plan.publication_sequence(),
  };
  let mut candidates = PlannedPartialCandidateSourceV1 {
    hash_algorithm: request.plan.hash_algorithm(),
    target_namespace_root: request.plan.selected_namespace_root(),
    target_publication_sequence: request.plan.publication_sequence(),
    source_namespace_root: &generation.source_namespace_root,
    source_publication_sequence: generation.coverage_publication_sequence,
    scope_id: request.scope_id,
    candidate,
    query_fingerprint: request.plan.query_fingerprint(),
    source: request.source,
    memory: request.memory,
    cancellation: request.cancellation,
    limits: request.candidate_limits,
    maximum_candidates: request.acceleration_limits.maximum_accelerator_candidates(),
  };
  execute_partial_index_acceleration_v1(IndexPartialAccelerationRequestV1 {
    hash_algorithm: request.plan.hash_algorithm(),
    plan: &coverage_plan,
    query_fingerprint: request.plan.query_fingerprint(),
    candidates: &mut candidates,
    complement: request.complement,
    rechecker: request.rechecker,
    memory: request.memory,
    cancellation: request.cancellation,
    limits: request.acceleration_limits,
  })
}

fn selected_partial_candidate<'a>(
  plan: &'a CompiledRootAwareQueryPlanV1,
  predicate_index: usize,
  scope_id: &[u8],
  candidate_index: usize,
) -> Result<&'a CompiledQueryIndexCandidateV1, IndexPartialAccelerationErrorV1> {
  let predicate = plan.predicates().get(predicate_index).ok_or_else(|| {
    IndexPartialAccelerationErrorV1::invalid("query_partial_predicate_index", "partial candidate predicate index is out of bounds")
  })?;
  let scope = predicate.scopes().iter().find(|scope| scope.scope_id() == scope_id).ok_or_else(|| {
    IndexPartialAccelerationErrorV1::invalid("query_partial_scope", "partial candidate scope is absent from the compiled predicate")
  })?;
  let selected = match scope.driver() {
    QueryPlanDriverV1::Index { candidate_index: selected, coverage: CompiledQueryCoverageV1::PartialExact, .. } => {
      *selected == candidate_index
    }
    QueryPlanDriverV1::IndexUnion { coverage: CompiledQueryCoverageV1::PartialExact, .. } => {
      return Err(IndexPartialAccelerationErrorV1::invalid(
        "query_partial_candidate_union",
        "one-candidate partial execution cannot prove a planner-selected index union complete",
      ));
    }
    QueryPlanDriverV1::Authoritative { .. } | QueryPlanDriverV1::Index { .. } | QueryPlanDriverV1::IndexUnion { .. } => false,
  };
  let candidate = scope
    .candidates()
    .get(candidate_index)
    .ok_or_else(|| IndexPartialAccelerationErrorV1::invalid("query_partial_candidate_index", "partial candidate index is out of bounds"))?;
  if !selected || candidate.coverage() != CompiledQueryCoverageV1::PartialExact || !candidate.proven_candidate_superset() {
    return Err(IndexPartialAccelerationErrorV1::invalid(
      "query_partial_candidate_driver",
      "candidate is not a planner-selected exact partial superset",
    ));
  }
  Ok(candidate)
}

struct PlannedPartialCandidateSourceV1<'a> {
  hash_algorithm: HashAlgorithm,
  target_namespace_root: &'a [u8],
  target_publication_sequence: u64,
  source_namespace_root: &'a [u8],
  source_publication_sequence: u64,
  scope_id: &'a [u8],
  candidate: &'a CompiledQueryIndexCandidateV1,
  query_fingerprint: &'a [u8],
  source: &'a mut dyn QueryPartialCandidateArtifactSourceV1,
  memory: &'a MemoryCoordinator,
  cancellation: &'a CancellationToken,
  limits: QueryCompleteCandidateLimitsV1,
  maximum_candidates: u64,
}

impl IndexAcceleratorCandidateSourceV1 for PlannedPartialCandidateSourceV1<'_> {
  fn scan_candidates(
    &mut self,
    request: IndexAcceleratorCandidateScanRequestV1<'_>,
    visitor: &mut dyn IndexAcceleratorCandidateVisitorV1,
  ) -> Result<IndexAcceleratorCandidateScanReceiptV1, IndexPartialScanErrorV1> {
    validate_acceleration_request(self, request)?;
    let posting_receipt = self.source.resolve_partial_posting_root(QueryPartialPostingRootRequestV1 {
      target_namespace_root: self.target_namespace_root,
      target_publication_sequence: self.target_publication_sequence,
      source_namespace_root: self.source_namespace_root,
      source_publication_sequence: self.source_publication_sequence,
      scope_id: self.scope_id,
      candidate: self.candidate,
      cancellation: self.cancellation,
    })?;
    validate_posting_receipt(self, &posting_receipt)?;
    let postings = scan_partial_posting_ordinals_v1(
      QueryPartialPostingScanRequestV1 {
        hash_algorithm: self.hash_algorithm,
        source_namespace_root: self.source_namespace_root,
        scope_id: self.scope_id,
        candidate: self.candidate,
        posting_root: posting_receipt.root.as_ref(),
        memory: self.memory,
        cancellation: self.cancellation,
        limits: self.limits,
      },
      self.source,
    )
    .map_err(map_candidate_error)?;
    if postings.document_ordinals().is_empty() {
      return candidate_receipt(request, 0);
    }
    let scope_receipt = self.source.resolve_partial_scope_root(QueryPartialScopeRootRequestV1 {
      source_namespace_root: self.source_namespace_root,
      source_publication_sequence: self.source_publication_sequence,
      scope_id: self.scope_id,
      cancellation: self.cancellation,
    })?;
    validate_scope_receipt(self, &scope_receipt)?;
    let identities = resolve_complete_scope_identities_v1(
      QueryCompleteScopeResolutionRequestV1 {
        hash_algorithm: self.hash_algorithm,
        selected_namespace_root: self.source_namespace_root,
        scope_id: self.scope_id,
        scope_ordinal_root: scope_receipt.root.as_ref(),
        selection: QueryScopeOrdinalSelectionV1::CandidateOrdinals(postings.document_ordinals()),
        memory: self.memory,
        cancellation: self.cancellation,
        limits: self.limits,
      },
      self.source,
    )
    .map_err(map_candidate_error)?;
    drop(postings);
    for identity in identities.identities() {
      visitor
        .visit(IndexAcceleratorCandidateV1 { file_key: identity.file_key(), indexed_revision_hash: identity.record_revision() })
        .map_err(IndexPartialScanErrorV1::Visitor)?;
    }
    let candidate_count = u64::try_from(identities.identities().len()).map_err(|error| {
      IndexPartialSourceErrorV1::internal("query_partial_candidate_count", format!("candidate count exceeds u64: {error}"))
    })?;
    candidate_receipt(request, candidate_count)
  }
}

fn candidate_receipt(
  request: IndexAcceleratorCandidateScanRequestV1<'_>,
  candidate_count: u64,
) -> Result<IndexAcceleratorCandidateScanReceiptV1, IndexPartialScanErrorV1> {
  Ok(IndexAcceleratorCandidateScanReceiptV1 {
    generation: request.generation,
    generation_manifest_hash: copy_receipt_bytes(request.generation_manifest_hash, "generation manifest")?,
    source_namespace_root: copy_receipt_bytes(request.source_namespace_root, "source NamespaceRoot")?,
    query_fingerprint: copy_receipt_bytes(request.query_fingerprint, "query fingerprint")?,
    candidate_count,
    complete: true,
  })
}

fn validate_acceleration_request(
  source: &PlannedPartialCandidateSourceV1<'_>,
  request: IndexAcceleratorCandidateScanRequestV1<'_>,
) -> Result<(), IndexPartialSourceErrorV1> {
  let generation = source.candidate.selected_generation().ok_or_else(|| {
    IndexPartialSourceErrorV1::internal("query_partial_candidate_generation", "adapter lost its planner-selected generation")
  })?;
  if request.hash_algorithm != source.hash_algorithm
    || request.generation != generation.generation
    || request.generation_manifest_hash != generation.manifest_hash
    || request.source_namespace_root != source.source_namespace_root
    || request.query_fingerprint != source.query_fingerprint
    || request.maximum_candidates != source.maximum_candidates
  {
    return Err(IndexPartialSourceErrorV1::internal(
      "query_partial_candidate_request",
      "partial executor request disagrees with its planner-bound artifact adapter",
    ));
  }
  if request.cancellation.is_cancelled() {
    return Err(IndexPartialSourceErrorV1::cancelled("query_partial_candidate_cancelled", "partial artifact scan was cancelled"));
  }
  Ok(())
}

fn validate_posting_receipt(
  source: &PlannedPartialCandidateSourceV1<'_>,
  receipt: &QueryPartialPostingRootReceiptV1,
) -> Result<(), IndexPartialSourceErrorV1> {
  let generation = source.candidate.selected_generation().ok_or_else(|| {
    IndexPartialSourceErrorV1::internal("query_partial_candidate_generation", "adapter lost its planner-selected generation")
  })?;
  if !receipt.complete
    || receipt.target_namespace_root != source.target_namespace_root
    || receipt.target_publication_sequence != source.target_publication_sequence
    || receipt.source_namespace_root != source.source_namespace_root
    || receipt.source_publication_sequence != source.source_publication_sequence
    || receipt.scope_id != source.scope_id
    || receipt.index_id != source.candidate.index_id()
    || receipt.generation != generation.generation
    || receipt.generation_manifest_hash != generation.manifest_hash
  {
    return Err(IndexPartialSourceErrorV1::corrupt(
      "query_partial_posting_receipt",
      "partial Posting root receipt does not bind the exact planner-selected root interval and generation",
    ));
  }
  Ok(())
}

fn validate_scope_receipt(
  source: &PlannedPartialCandidateSourceV1<'_>,
  receipt: &QueryPartialScopeRootReceiptV1,
) -> Result<(), IndexPartialSourceErrorV1> {
  if !receipt.complete
    || receipt.source_namespace_root != source.source_namespace_root
    || receipt.source_publication_sequence != source.source_publication_sequence
    || receipt.scope_id != source.scope_id
  {
    return Err(IndexPartialSourceErrorV1::corrupt(
      "query_partial_scope_receipt",
      "partial ScopeOrdinal root receipt does not bind the exact source root and scope",
    ));
  }
  Ok(())
}

fn map_candidate_error(error: QueryCompleteCandidateErrorV1) -> IndexPartialScanErrorV1 {
  let source = match error.class() {
    QueryCompleteCandidateErrorClassV1::InvalidRequest | QueryCompleteCandidateErrorClassV1::CorruptSource => {
      IndexPartialSourceErrorV1::corrupt(error.code(), error.context())
    }
    QueryCompleteCandidateErrorClassV1::ResourceLimit => IndexPartialSourceErrorV1::resource_limit(error.code(), error.context()),
    QueryCompleteCandidateErrorClassV1::HistoricalViewUnavailable => IndexPartialSourceErrorV1::unavailable(error.code(), error.context()),
    QueryCompleteCandidateErrorClassV1::Cancelled => IndexPartialSourceErrorV1::cancelled(error.code(), error.context()),
    QueryCompleteCandidateErrorClassV1::Internal => IndexPartialSourceErrorV1::internal(error.code(), error.context()),
  };
  IndexPartialScanErrorV1::Source(source)
}

fn copy_receipt_bytes(value: &[u8], label: &'static str) -> Result<Vec<u8>, IndexPartialSourceErrorV1> {
  let mut copy = Vec::new();
  copy.try_reserve_exact(value.len()).map_err(|error| {
    IndexPartialSourceErrorV1::resource_limit("query_partial_receipt_allocation", format!("cannot retain {label}: {error}"))
  })?;
  copy.extend_from_slice(value);
  Ok(copy)
}
