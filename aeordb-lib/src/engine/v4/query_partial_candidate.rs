//! Planner-bound adapter from partial query artifacts to exact acceleration.
//!
//! This module owns no complement or recheck semantics. It converts one
//! planner-selected partial Posting generation and its source ScopeOrdinal
//! identities into the existing exact partial-acceleration source contract.

use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::MemoryCoordinator;

use super::hash::IncrementalDigestV1;
use super::index_artifact_cursor::ArtifactCursorSourceV1;
use super::index_coverage_planner::{IndexCoverageGenerationHealthV1, IndexCoverageGenerationV1, IndexCoveragePlanV1};
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
use super::query_candidate_composition::{QueryBooleanCandidatePlanKindV1, QueryBooleanCandidatePlanV1, QueryCandidateSelectionV1};
use super::query_planner::{CompiledQueryCoverageV1, CompiledQueryIndexCandidateV1, CompiledRootAwareQueryPlanV1, QueryPlanDriverV1};

const COMPOSITE_MANIFEST_DOMAIN_V1: &[u8] = b"aeordb:query-partial-composite-manifest:v1\0";
const COMPOSITE_GENERATION_DOMAIN_V1: &[u8] = b"aeordb:query-partial-composite-generation:v1\0";
const COMPOSITE_EPOCH_DOMAIN_V1: &[u8] = b"aeordb:query-partial-composite-epoch:v1\0";

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

pub struct QueryComposedPartialCandidateExecutionRequestV1<'a> {
  pub plan: &'a CompiledRootAwareQueryPlanV1,
  pub candidate_plan: &'a QueryBooleanCandidatePlanV1,
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

pub fn execute_composed_partial_candidates_v1(
  request: QueryComposedPartialCandidateExecutionRequestV1<'_>,
) -> Result<IndexPartialAccelerationOutcomeV1, IndexPartialAccelerationErrorV1> {
  if request.acceleration_limits.maximum_accelerator_candidates() > request.candidate_limits.maximum_candidate_documents() {
    return Err(IndexPartialAccelerationErrorV1::invalid(
      "query_composed_partial_candidate_limits",
      "composed partial acceleration admits more candidates than its artifact scanner",
    ));
  }
  if request.candidate_plan.kind() != QueryBooleanCandidatePlanKindV1::Partial {
    return Err(IndexPartialAccelerationErrorV1::invalid(
      "query_composed_partial_candidate_plan",
      "composed partial execution requires a compiler-derived partial candidate plan",
    ));
  }
  let scope_id = request.candidate_plan.scope_id().ok_or_else(|| {
    IndexPartialAccelerationErrorV1::invalid("query_composed_partial_candidate_scope", "partial candidate plan omits its scope")
  })?;
  let source_namespace_root = request.candidate_plan.source_namespace_root().ok_or_else(|| {
    IndexPartialAccelerationErrorV1::invalid(
      "query_composed_partial_candidate_source_root",
      "partial candidate plan omits its source NamespaceRoot",
    )
  })?;
  let source_publication_sequence = request.candidate_plan.covered_through_publication_sequence().ok_or_else(|| {
    IndexPartialAccelerationErrorV1::invalid(
      "query_composed_partial_candidate_sequence",
      "partial candidate plan omits its source publication sequence",
    )
  })?;
  if request.candidate_plan.query_fingerprint() != Some(request.plan.query_fingerprint()) {
    return Err(IndexPartialAccelerationErrorV1::invalid(
      "query_composed_partial_candidate_fingerprint",
      "candidate composition belongs to a different compiled query",
    ));
  }
  let identity =
    composite_coverage_identity(request.plan, request.candidate_plan, scope_id, source_namespace_root, source_publication_sequence)?;
  let generation = IndexCoverageGenerationV1 {
    generation: identity.generation,
    owner_id: &identity.manifest_hash,
    manifest_hash: &identity.manifest_hash,
    source_namespace_root,
    coverage_epoch_id: &identity.coverage_epoch_id,
    coverage_publication_sequence: source_publication_sequence,
    definition_fingerprint: &identity.manifest_hash,
    dependency_fingerprint: &identity.manifest_hash,
    health: IndexCoverageGenerationHealthV1::Healthy,
  };
  let coverage_plan = IndexCoveragePlanV1::PartialCandidate {
    generation,
    target_namespace_root: request.plan.selected_namespace_root(),
    target_publication_sequence: request.plan.publication_sequence(),
  };
  let mut candidates = ComposedPartialCandidateSourceV1 {
    plan: request.plan,
    candidate_plan: request.candidate_plan,
    synthetic_generation: identity.generation,
    synthetic_manifest_hash: &identity.manifest_hash,
    scope_id,
    source_namespace_root,
    source_publication_sequence,
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

struct CompositeCoverageIdentityV1 {
  generation: u64,
  manifest_hash: Vec<u8>,
  coverage_epoch_id: [u8; 16],
}

fn composite_coverage_identity(
  plan: &CompiledRootAwareQueryPlanV1,
  candidate_plan: &QueryBooleanCandidatePlanV1,
  scope_id: &[u8],
  source_namespace_root: &[u8],
  source_publication_sequence: u64,
) -> Result<CompositeCoverageIdentityV1, IndexPartialAccelerationErrorV1> {
  if candidate_plan.selections().is_empty() {
    return Err(IndexPartialAccelerationErrorV1::invalid(
      "query_composed_partial_candidate_empty",
      "partial candidate composition contains no selected branches",
    ));
  }
  let mut digest = IncrementalDigestV1::new(plan.hash_algorithm());
  digest.update(COMPOSITE_MANIFEST_DOMAIN_V1);
  digest.update(&plan.hash_algorithm().to_u16().to_le_bytes());
  digest.update(plan.query_fingerprint());
  digest.update(scope_id);
  digest.update(source_namespace_root);
  digest.update(&source_publication_sequence.to_le_bytes());
  digest.update(plan.selected_namespace_root());
  digest.update(&plan.publication_sequence().to_le_bytes());
  let selection_count = u64::try_from(candidate_plan.selections().len()).map_err(|error| {
    IndexPartialAccelerationErrorV1::invalid(
      "query_composed_partial_candidate_count",
      format!("composed candidate count exceeds u64: {error}"),
    )
  })?;
  digest.update(&selection_count.to_le_bytes());
  let mut previous = None;
  for selection in candidate_plan.selections() {
    let tuple = (selection.predicate_index(), selection.candidate_index());
    if previous.is_some_and(|previous| previous >= tuple) {
      return Err(IndexPartialAccelerationErrorV1::invalid(
        "query_composed_partial_candidate_order",
        "composed candidate bindings are not strictly ordered and unique",
      ));
    }
    previous = Some(tuple);
    let candidate = selected_composed_partial_candidate(plan, scope_id, *selection)?;
    let generation = candidate.selected_generation().ok_or_else(|| {
      IndexPartialAccelerationErrorV1::invalid(
        "query_composed_partial_candidate_generation",
        "composed partial candidate omits its selected generation",
      )
    })?;
    if generation.source_namespace_root != source_namespace_root || generation.coverage_publication_sequence != source_publication_sequence
    {
      return Err(IndexPartialAccelerationErrorV1::invalid(
        "query_composed_partial_candidate_basis",
        "composed partial candidate does not share the declared immutable-root basis",
      ));
    }
    let predicate_index = u64::try_from(selection.predicate_index()).map_err(|error| {
      IndexPartialAccelerationErrorV1::invalid("query_composed_partial_candidate_index", format!("predicate index exceeds u64: {error}"))
    })?;
    let candidate_index = u64::try_from(selection.candidate_index()).map_err(|error| {
      IndexPartialAccelerationErrorV1::invalid("query_composed_partial_candidate_index", format!("candidate index exceeds u64: {error}"))
    })?;
    digest.update(&predicate_index.to_le_bytes());
    digest.update(&candidate_index.to_le_bytes());
    digest.update(candidate.index_id());
    digest.update(&generation.generation.to_le_bytes());
    digest.update(&generation.owner_id);
    digest.update(&generation.manifest_hash);
    digest.update(&generation.source_namespace_root);
    digest.update(&generation.coverage_epoch_id);
    digest.update(&generation.coverage_publication_sequence.to_le_bytes());
    digest.update(&generation.definition_fingerprint);
    digest.update(&generation.dependency_fingerprint);
  }
  let manifest_hash = digest.finalize();
  if manifest_hash.len() != plan.hash_algorithm().hash_length() || manifest_hash.iter().all(|byte| *byte == 0) {
    return Err(IndexPartialAccelerationErrorV1::internal(
      "query_composed_partial_candidate_manifest",
      "composite candidate manifest digest has an invalid width or all-zero value",
    ));
  }
  let generation_hash = composite_identity_component(plan.hash_algorithm(), COMPOSITE_GENERATION_DOMAIN_V1, &manifest_hash);
  let generation_bytes = generation_hash.get(..8).ok_or_else(|| {
    IndexPartialAccelerationErrorV1::internal(
      "query_composed_partial_candidate_generation",
      "composite generation digest is shorter than eight bytes",
    )
  })?;
  let generation_bytes: [u8; 8] = generation_bytes.try_into().map_err(|error| {
    IndexPartialAccelerationErrorV1::internal(
      "query_composed_partial_candidate_generation",
      format!("cannot decode composite generation identity: {error}"),
    )
  })?;
  let generation = u64::from_le_bytes(generation_bytes);
  if generation == 0 {
    return Err(IndexPartialAccelerationErrorV1::internal(
      "query_composed_partial_candidate_generation",
      "composite generation digest produced the reserved zero identity",
    ));
  }
  let epoch_hash = composite_identity_component(plan.hash_algorithm(), COMPOSITE_EPOCH_DOMAIN_V1, &manifest_hash);
  let epoch_bytes = epoch_hash.get(..16).ok_or_else(|| {
    IndexPartialAccelerationErrorV1::internal(
      "query_composed_partial_candidate_epoch",
      "composite epoch digest is shorter than sixteen bytes",
    )
  })?;
  let mut coverage_epoch_id = [0u8; 16];
  coverage_epoch_id.copy_from_slice(epoch_bytes);
  if coverage_epoch_id.iter().all(|byte| *byte == 0) {
    return Err(IndexPartialAccelerationErrorV1::internal(
      "query_composed_partial_candidate_epoch",
      "composite epoch digest produced the reserved zero identity",
    ));
  }
  Ok(CompositeCoverageIdentityV1 { generation, manifest_hash, coverage_epoch_id })
}

fn composite_identity_component(algorithm: HashAlgorithm, domain: &[u8], manifest_hash: &[u8]) -> Vec<u8> {
  let mut digest = IncrementalDigestV1::new(algorithm);
  digest.update(domain);
  digest.update(&algorithm.to_u16().to_le_bytes());
  digest.update(manifest_hash);
  digest.finalize()
}

fn selected_composed_partial_candidate<'a>(
  plan: &'a CompiledRootAwareQueryPlanV1,
  scope_id: &[u8],
  selection: QueryCandidateSelectionV1,
) -> Result<&'a CompiledQueryIndexCandidateV1, IndexPartialAccelerationErrorV1> {
  let predicate = plan.predicates().get(selection.predicate_index()).ok_or_else(|| {
    IndexPartialAccelerationErrorV1::invalid(
      "query_composed_partial_predicate_index",
      "composed candidate predicate index is out of bounds",
    )
  })?;
  let scope = predicate.scopes().iter().find(|scope| scope.scope_id() == scope_id).ok_or_else(|| {
    IndexPartialAccelerationErrorV1::invalid(
      "query_composed_partial_scope",
      "composed candidate scope is absent from the compiled predicate",
    )
  })?;
  let selected = match scope.driver() {
    QueryPlanDriverV1::Index { candidate_index, coverage: CompiledQueryCoverageV1::PartialExact, .. } => {
      *candidate_index == selection.candidate_index()
    }
    QueryPlanDriverV1::IndexUnion { candidate_indexes, coverage: CompiledQueryCoverageV1::PartialExact, .. } => {
      candidate_indexes.contains(&selection.candidate_index())
    }
    QueryPlanDriverV1::Authoritative { .. } | QueryPlanDriverV1::Index { .. } | QueryPlanDriverV1::IndexUnion { .. } => false,
  };
  let candidate = scope.candidates().get(selection.candidate_index()).ok_or_else(|| {
    IndexPartialAccelerationErrorV1::invalid("query_composed_partial_candidate_index", "composed partial candidate index is out of bounds")
  })?;
  if !selected || candidate.coverage() != CompiledQueryCoverageV1::PartialExact || !candidate.proven_candidate_superset() {
    return Err(IndexPartialAccelerationErrorV1::invalid(
      "query_composed_partial_candidate_driver",
      "candidate is not a compiler-selected exact partial superset",
    ));
  }
  Ok(candidate)
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

struct ComposedPartialCandidateSourceV1<'a> {
  plan: &'a CompiledRootAwareQueryPlanV1,
  candidate_plan: &'a QueryBooleanCandidatePlanV1,
  synthetic_generation: u64,
  synthetic_manifest_hash: &'a [u8],
  scope_id: &'a [u8],
  source_namespace_root: &'a [u8],
  source_publication_sequence: u64,
  source: &'a mut dyn QueryPartialCandidateArtifactSourceV1,
  memory: &'a MemoryCoordinator,
  cancellation: &'a CancellationToken,
  limits: QueryCompleteCandidateLimitsV1,
  maximum_candidates: u64,
}

impl IndexAcceleratorCandidateSourceV1 for ComposedPartialCandidateSourceV1<'_> {
  fn scan_candidates(
    &mut self,
    request: IndexAcceleratorCandidateScanRequestV1<'_>,
    visitor: &mut dyn IndexAcceleratorCandidateVisitorV1,
  ) -> Result<IndexAcceleratorCandidateScanReceiptV1, IndexPartialScanErrorV1> {
    if request.hash_algorithm != self.plan.hash_algorithm()
      || request.generation != self.synthetic_generation
      || request.generation_manifest_hash != self.synthetic_manifest_hash
      || request.source_namespace_root != self.source_namespace_root
      || request.query_fingerprint != self.plan.query_fingerprint()
      || request.maximum_candidates != self.maximum_candidates
    {
      return Err(
        IndexPartialSourceErrorV1::internal(
          "query_composed_partial_candidate_request",
          "partial executor request disagrees with its compiler-bound composite artifact adapter",
        )
        .into(),
      );
    }
    if request.cancellation.is_cancelled() || self.cancellation.is_cancelled() {
      return Err(
        IndexPartialSourceErrorV1::cancelled("query_composed_partial_candidate_cancelled", "composed partial artifact scan was cancelled")
          .into(),
      );
    }
    let mut candidate_count = 0u64;
    for selection in self.candidate_plan.selections() {
      let candidate = selected_composed_partial_candidate(self.plan, self.scope_id, *selection).map_err(|error| {
        IndexPartialSourceErrorV1::internal(
          error.code(),
          format!("compiler-bound candidate composition became invalid: {}", error.context()),
        )
      })?;
      let mut branch = PlannedPartialCandidateSourceV1 {
        hash_algorithm: self.plan.hash_algorithm(),
        target_namespace_root: self.plan.selected_namespace_root(),
        target_publication_sequence: self.plan.publication_sequence(),
        source_namespace_root: self.source_namespace_root,
        source_publication_sequence: self.source_publication_sequence,
        scope_id: self.scope_id,
        candidate,
        query_fingerprint: self.plan.query_fingerprint(),
        source: &mut *self.source,
        memory: self.memory,
        cancellation: self.cancellation,
        limits: self.limits,
        maximum_candidates: self.maximum_candidates,
      };
      scan_partial_candidate_branch(&mut branch, visitor, &mut candidate_count)?;
    }
    candidate_receipt(request, candidate_count)
  }
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
    let mut candidate_count = 0u64;
    scan_partial_candidate_branch(self, visitor, &mut candidate_count)?;
    candidate_receipt(request, candidate_count)
  }
}

fn scan_partial_candidate_branch(
  source: &mut PlannedPartialCandidateSourceV1<'_>,
  visitor: &mut dyn IndexAcceleratorCandidateVisitorV1,
  candidate_count: &mut u64,
) -> Result<(), IndexPartialScanErrorV1> {
  let posting_receipt = source.source.resolve_partial_posting_root(QueryPartialPostingRootRequestV1 {
    target_namespace_root: source.target_namespace_root,
    target_publication_sequence: source.target_publication_sequence,
    source_namespace_root: source.source_namespace_root,
    source_publication_sequence: source.source_publication_sequence,
    scope_id: source.scope_id,
    candidate: source.candidate,
    cancellation: source.cancellation,
  })?;
  validate_posting_receipt(source, &posting_receipt)?;
  let postings = scan_partial_posting_ordinals_v1(
    QueryPartialPostingScanRequestV1 {
      hash_algorithm: source.hash_algorithm,
      source_namespace_root: source.source_namespace_root,
      scope_id: source.scope_id,
      candidate: source.candidate,
      posting_root: posting_receipt.root.as_ref(),
      memory: source.memory,
      cancellation: source.cancellation,
      limits: source.limits,
    },
    source.source,
  )
  .map_err(map_candidate_error)?;
  if postings.document_ordinals().is_empty() {
    return Ok(());
  }
  let scope_receipt = source.source.resolve_partial_scope_root(QueryPartialScopeRootRequestV1 {
    source_namespace_root: source.source_namespace_root,
    source_publication_sequence: source.source_publication_sequence,
    scope_id: source.scope_id,
    cancellation: source.cancellation,
  })?;
  validate_scope_receipt(source, &scope_receipt)?;
  let identities = resolve_complete_scope_identities_v1(
    QueryCompleteScopeResolutionRequestV1 {
      hash_algorithm: source.hash_algorithm,
      selected_namespace_root: source.source_namespace_root,
      scope_id: source.scope_id,
      scope_ordinal_root: scope_receipt.root.as_ref(),
      selection: QueryScopeOrdinalSelectionV1::CandidateOrdinals(postings.document_ordinals()),
      memory: source.memory,
      cancellation: source.cancellation,
      limits: source.limits,
    },
    source.source,
  )
  .map_err(map_candidate_error)?;
  drop(postings);
  for identity in identities.identities() {
    if *candidate_count >= source.maximum_candidates {
      return Err(
        IndexPartialSourceErrorV1::resource_limit(
          "query_partial_candidate_limit",
          "partial candidate branches emitted more identities than the admitted aggregate bound",
        )
        .into(),
      );
    }
    visitor
      .visit(IndexAcceleratorCandidateV1 { file_key: identity.file_key(), indexed_revision_hash: identity.record_revision() })
      .map_err(IndexPartialScanErrorV1::Visitor)?;
    *candidate_count = candidate_count
      .checked_add(1)
      .ok_or_else(|| IndexPartialSourceErrorV1::internal("query_partial_candidate_count", "candidate count overflowed u64"))?;
  }
  Ok(())
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
