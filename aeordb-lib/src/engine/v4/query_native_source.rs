//! Native selected-root source for partitioned authoritative query truth.

use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner, MemoryReservation};

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, decode_canonical_value};
use super::index_artifact_cursor::{ArtifactCursorReadErrorV1, ArtifactCursorSourceV1, RetainedArtifactBytesV1};
use super::index_definition_runtime::{IndexDefinitionErrorClassV1, IndexDefinitionRuntimeV1};
use super::index_page::OrderedIndexRoleV1;
use super::index_partial_acceleration::IndexPartialSourceErrorV1;
use super::position::{CompiledPositionComparatorV1, PositionRouteV1, PositionSortDirectionV1};
use super::position_order::{
  LogicalOrderComponentOwnedV1, LogicalOrderRowOwnedV1, compare_logical_order_components_v1, logical_order_row_allocated_bytes_v1,
};
use super::position_resolver::{
  PositionUniverseLookupRequestV1, PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1, PositionUniverseSourceV1,
};
use super::query_aggregate_execution::{
  QueryAggregateInputFieldV1, QueryAggregateInputLookupRequestV1, QueryAggregateInputLookupResultV1, QueryAggregateInputRowV1,
  QueryAggregateInputSourceV1, query_aggregate_input_row_allocated_bytes_v1,
};
use super::query_complete_candidate::{
  QueryCandidateArtifactRootV1, QueryCandidateRecheckReceiptV1, QueryCandidateRecheckRequestV1, QueryCompleteCandidateErrorClassV1,
  QueryCompleteCandidateErrorV1, QueryCompleteCandidateSourceV1, QueryCompletePostingRootReceiptV1, QueryCompletePostingRootRequestV1,
  QueryCompleteScopeRootReceiptV1, QueryCompleteScopeRootRequestV1,
};
use super::query_executor::{
  QueryAuthoritativeDocumentVisitorV1, QueryAuthoritativeFieldPartitionCursorV1, QueryAuthoritativeFieldPartitionSourceV1,
  QueryAuthoritativeFieldSourceV1, QueryAuthoritativeScopeSourceV1, QueryAuthoritativeValueVisitorV1, QueryExecutionDocumentV1,
  QueryExecutionErrorV1, QueryExecutionFieldDocumentV1, QueryExecutionFieldPartitionOpenRequestV1, QueryExecutionFieldPartitionReceiptV1,
  QueryExecutionFieldReadReceiptV1, QueryExecutionFieldReadRequestV1, QueryExecutionFieldStateV1, QueryExecutionLimitsV1,
  QueryExecutionMatchSinkV1, QueryExecutionScanErrorV1, QueryExecutionScopeScanReceiptV1, QueryExecutionScopeScanRequestV1,
  QueryExecutionSourceErrorClassV1, QueryExecutionSourceErrorV1, QueryExecutionStreamReceiptV1,
  RootAwarePartitionedQueryExecutionRequestV1, RootAwareQueryExecutionV1, RootAwareQueryScopeExecutionRequestV1,
  execute_authoritative_partitioned_query_into_v1, execute_authoritative_partitioned_query_v1, execute_authoritative_scope_query_v1,
};
use super::query_native_workspace::{
  NativeQueryOrderingCursorV1, NativeQueryOrderingLookupV1, NativeQueryOrderingWorkspaceBuilderV1,
  NativeQueryOrderingWorkspaceErrorClassV1, NativeQueryOrderingWorkspaceErrorV1, NativeQueryOrderingWorkspaceLimitsV1,
  NativeQueryOrderingWorkspaceV1,
};
use super::query_partial_candidate::{
  QueryPartialCandidateArtifactSourceV1, QueryPartialPostingRootReceiptV1, QueryPartialPostingRootRequestV1,
  QueryPartialScopeRootReceiptV1, QueryPartialScopeRootRequestV1,
};
use super::query_planner::{
  CompiledQueryCoverageV1, CompiledQueryIndexCandidateV1, CompiledRootAwareQueryPlanV1, QueryPlanningCoverageGenerationV1,
  RootAwareQueryFieldCatalogV1,
};
use super::read_view::ResolvedReadViewV1;
use super::read_view_authorization::ResolvedPathAuthorizationV1;
use super::read_view_native::{
  NativeReadViewSourceV1, NativeSelectedArtifactRootRequestV1, NativeSelectedNamespaceLimitsV1, NativeSelectedNamespaceReadErrorClassV1,
  NativeSelectedNamespaceReadErrorV1, NativeSelectedNamespaceReaderV1, NativeSelectedSemanticCatalogV1, NativeSelectedSourceLimitsV1,
  NativeSelectedSourceOutcomeV1, NativeSelectedSourceParserV1,
};
use super::scope::{EffectiveScopeCandidateV1, EffectiveScopeResolverV1, is_internal_index_path_v1, validate_canonical_absolute_path};

const MAXIMUM_PARTITION_SCOPES: usize = 1_024;
const MAXIMUM_SCOPE_RESOLVER_BYTES: u64 = 64 * 1024 * 1024;
const PARTITION_CURSOR_FIXED_BYTES: u64 = 16 * 1024;
const PARTITION_SOURCE_MAXIMUM_RETAINED_BYTES: u64 = 256 * 1024 * 1024;
const AUXILIARY_SOURCE_FIXED_BYTES: u64 = 16 * 1024;
const AUXILIARY_ALLOCATION_OVERHEAD_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeAuthoritativeAuxiliaryLimitsV1 {
  maximum_fields: usize,
  maximum_scope_bindings: usize,
  maximum_binding_bytes: u64,
  maximum_path_bytes: u64,
}

impl NativeAuthoritativeAuxiliaryLimitsV1 {
  pub fn new(
    maximum_fields: usize,
    maximum_scope_bindings: usize,
    maximum_binding_bytes: u64,
    maximum_path_bytes: u64,
  ) -> Result<Self, QueryExecutionSourceErrorV1> {
    if maximum_fields == 0 || maximum_scope_bindings == 0 || maximum_binding_bytes == 0 || maximum_path_bytes == 0 {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_auxiliary_limits",
        "native auxiliary field, scope, retained-byte, and path limits must be nonzero",
      ));
    }
    Ok(Self { maximum_fields, maximum_scope_bindings, maximum_binding_bytes, maximum_path_bytes })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeAuthoritativeFieldPartitionLimitsV1 {
  namespace: NativeSelectedNamespaceLimitsV1,
  ordering: NativeQueryOrderingWorkspaceLimitsV1,
}

impl NativeAuthoritativeFieldPartitionLimitsV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    namespace: NativeSelectedNamespaceLimitsV1,
    maximum_documents: u64,
    maximum_workspace_bytes: u64,
    maximum_sort_bytes: u64,
    maximum_records_per_run: usize,
    merge_fan_in: usize,
  ) -> Result<Self, QueryExecutionSourceErrorV1> {
    let ordering = NativeQueryOrderingWorkspaceLimitsV1::new(
      maximum_documents,
      maximum_workspace_bytes,
      maximum_sort_bytes,
      maximum_records_per_run,
      merge_fan_in,
    )
    .map_err(map_workspace_error)?;
    Ok(Self { namespace, ordering })
  }
}

struct NativeAuthoritativeFieldPartitionInnerV1 {
  source: NativeReadViewSourceV1,
  view: ResolvedReadViewV1<ResolvedPathAuthorizationV1>,
  semantic_catalog: NativeSelectedSemanticCatalogV1,
  workspace: NativeQueryOrderingWorkspaceV1,
  query_path: String,
  namespace_limits: NativeSelectedNamespaceLimitsV1,
}

pub struct NativeAuthoritativeFieldPartitionSourceV1 {
  inner: Arc<NativeAuthoritativeFieldPartitionInnerV1>,
}

pub struct NativeQueryCandidateArtifactSourceV1 {
  inner: Arc<NativeAuthoritativeFieldPartitionInnerV1>,
  lookup: Option<NativeQueryOrderingLookupV1>,
}

struct NativeAuthoritativeRowFieldSourceV1<'row, 'scope> {
  inner: Arc<NativeAuthoritativeFieldPartitionInnerV1>,
  row: &'row super::read_view_native::NativeSelectedNamespaceFileRowV1,
  effective_scope_id: &'scope [u8],
  source_limits: NativeSelectedSourceLimitsV1,
}

struct NativeAuthoritativeFieldEvaluationV1 {
  state: QueryExecutionFieldStateV1,
  values: Vec<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct NativeAuxiliaryScopeBindingV1 {
  scope_id: Vec<u8>,
  catalog_scope_index: usize,
  catalog_index_index: usize,
}

#[derive(Clone, Debug)]
struct NativeAuxiliaryFieldBindingV1 {
  field_name: String,
  comparator: super::position::PositionComparatorV1,
  comparison_semantics: u16,
  collation_semantics: u16,
  behavior_fingerprint: [u8; 32],
  catalog_index: usize,
  scopes: Vec<NativeAuxiliaryScopeBindingV1>,
}

struct NativeRestoredAuxiliaryDocumentV1 {
  row: super::read_view_native::NativeSelectedNamespaceFileRowV1,
  effective_scope_id: Option<Vec<u8>>,
}

struct NativeAuxiliaryFieldEvaluationV1 {
  scope_id: Option<Vec<u8>>,
  state: QueryExecutionFieldStateV1,
  values: Vec<LogicalOrderComponentOwnedV1>,
}

pub struct NativeAuthoritativeAuxiliarySourceV1<'plan> {
  inner: Arc<NativeAuthoritativeFieldPartitionInnerV1>,
  plan: &'plan CompiledRootAwareQueryPlanV1,
  lookup: NativeQueryOrderingLookupV1,
  fields: Vec<NativeAuxiliaryFieldBindingV1>,
  maximum_path_bytes: u64,
  _memory: MemoryReservation,
}

impl NativeAuthoritativeFieldPartitionSourceV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn build(
    source: NativeReadViewSourceV1,
    view: ResolvedReadViewV1<ResolvedPathAuthorizationV1>,
    semantic_catalog: NativeSelectedSemanticCatalogV1,
    query_path: &str,
    scratch_parent: &Path,
    limits: NativeAuthoritativeFieldPartitionLimitsV1,
    cancellation: &CancellationToken,
  ) -> Result<Self, QueryExecutionSourceErrorV1> {
    require_not_cancelled(cancellation)?;
    validate_canonical_absolute_path(query_path)
      .map_err(|error| source_error(QueryExecutionSourceErrorClassV1::Corrupt, "native_partition_query_path", error.to_string()))?;
    if semantic_catalog.selected_root() != view.root_metadata().hash
      || semantic_catalog.semantic_state_root() != view.authority().semantic_state.object_id
    {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_partition_catalog_authority",
        "selected semantic catalog does not bind the captured read view",
      ));
    }
    validate_catalogs(&view, semantic_catalog.catalogs())?;
    let resolver_bytes = scope_resolver_bytes(semantic_catalog.scope_definitions().len(), view.hash_algorithm())?;
    let _resolver_memory = source
      .memory_coordinator()
      .reserve(MemoryOwner::Query, resolver_bytes, AdmissionClass::Workload)
      .map_err(|error| source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_scope_memory", error.to_string()))?;
    let mut candidates = Vec::new();
    candidates.try_reserve_exact(semantic_catalog.scope_definitions().len()).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_partition_scope_allocation",
        format!("cannot allocate effective-scope candidates: {error}"),
      )
    })?;
    candidates.extend(
      semantic_catalog
        .scope_definitions()
        .iter()
        .map(|scope| EffectiveScopeCandidateV1 { scope_id: scope.scope_id(), encoded_definition: scope.encoded_definition() }),
    );
    let resolver = EffectiveScopeResolverV1::from_encoded(view.hash_algorithm(), &candidates)
      .map_err(|error| source_error(QueryExecutionSourceErrorClassV1::Corrupt, "native_partition_scope_catalog", error.to_string()))?;
    let reader = source.selected_namespace_reader(&view, limits.namespace).map_err(map_native_error)?;
    let mut builder = NativeQueryOrderingWorkspaceBuilderV1::new(
      scratch_parent,
      view.hash_algorithm(),
      Arc::clone(source.memory_coordinator()),
      cancellation.clone(),
      limits.ordering,
    )
    .map_err(map_workspace_error)?;
    let mut resume_after: Option<String> = None;
    loop {
      require_not_cancelled(cancellation)?;
      let page = reader.scan_files(query_path, resume_after.as_deref()).map_err(map_native_error)?;
      if page.selected_root() != view.root_metadata().hash
        || page.semantic_state_root() != view.authority().semantic_state.object_id
        || page.publication_sequence() != view.authority().admission.publication_sequence
      {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_partition_page_authority",
          "selected namespace page does not bind the source read view",
        ));
      }
      for row in page.rows() {
        require_not_cancelled(cancellation)?;
        if is_internal_index_path_v1(row.path()) {
          continue;
        }
        let winner = resolver.resolve(row.path()).map_err(|error| {
          source_error(QueryExecutionSourceErrorClassV1::Corrupt, "native_partition_scope_resolution", error.to_string())
        })?;
        let scope_id = winner.map(|index| semantic_catalog.scope_definitions()[index].scope_id());
        builder
          .append_parts(row.file_key(), scope_id, row.record_revision(), row.entity_version(), row.file_record())
          .map_err(map_workspace_error)?;
      }
      if page.complete() {
        break;
      }
      let next = page.next_resume_after().ok_or_else(|| {
        source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_partition_page_receipt",
          "incomplete selected namespace page omitted its resume path",
        )
      })?;
      resume_after = Some(try_clone_string(next, "namespace resume path")?);
    }
    drop(reader);
    drop(resolver);
    drop(candidates);
    let workspace = builder.finish().map_err(map_workspace_error)?;
    let query_path = try_clone_string(query_path, "query path")?;
    Ok(Self {
      inner: Arc::new(NativeAuthoritativeFieldPartitionInnerV1 {
        source,
        view,
        semantic_catalog,
        workspace,
        query_path,
        namespace_limits: limits.namespace,
      }),
    })
  }

  pub fn document_count(&self) -> u64 {
    self.inner.workspace.record_count()
  }

  pub fn workspace_bytes(&self) -> u64 {
    self.inner.workspace.workspace_bytes()
  }

  pub fn execute_authoritative_query_v1(
    &mut self,
    plan: &CompiledRootAwareQueryPlanV1,
    cancellation: &CancellationToken,
    limits: QueryExecutionLimitsV1,
  ) -> Result<RootAwareQueryExecutionV1, QueryExecutionErrorV1> {
    let inner = Arc::clone(&self.inner);
    execute_authoritative_partitioned_query_v1(RootAwarePartitionedQueryExecutionRequestV1 {
      plan,
      catalogs: inner.semantic_catalog.catalogs(),
      source: self,
      memory: inner.source.memory_coordinator().as_ref(),
      cancellation,
      limits,
    })
  }

  pub fn execute_authoritative_query_into_v1(
    &mut self,
    plan: &CompiledRootAwareQueryPlanV1,
    cancellation: &CancellationToken,
    limits: QueryExecutionLimitsV1,
    sink: &mut dyn QueryExecutionMatchSinkV1,
  ) -> Result<QueryExecutionStreamReceiptV1, QueryExecutionErrorV1> {
    let inner = Arc::clone(&self.inner);
    execute_authoritative_partitioned_query_into_v1(
      RootAwarePartitionedQueryExecutionRequestV1 {
        plan,
        catalogs: inner.semantic_catalog.catalogs(),
        source: self,
        memory: inner.source.memory_coordinator().as_ref(),
        cancellation,
        limits,
      },
      sink,
    )
  }

  pub fn execute_authoritative_scope_query_v1(
    &mut self,
    plan: &CompiledRootAwareQueryPlanV1,
    scope_id: &[u8],
    cancellation: &CancellationToken,
    limits: QueryExecutionLimitsV1,
  ) -> Result<RootAwareQueryExecutionV1, QueryExecutionErrorV1> {
    let inner = Arc::clone(&self.inner);
    execute_authoritative_scope_query_v1(RootAwareQueryScopeExecutionRequestV1 {
      plan,
      catalogs: inner.semantic_catalog.catalogs(),
      scope_id,
      source: self,
      memory: inner.source.memory_coordinator().as_ref(),
      cancellation,
      limits,
    })
  }

  pub fn open_auxiliary_source<'plan>(
    &self,
    plan: &'plan CompiledRootAwareQueryPlanV1,
    limits: NativeAuthoritativeAuxiliaryLimitsV1,
  ) -> Result<NativeAuthoritativeAuxiliarySourceV1<'plan>, QueryExecutionSourceErrorV1> {
    validate_auxiliary_plan(&self.inner, plan)?;
    let binding_bytes = auxiliary_binding_bytes(plan, limits)?;
    let memory = self
      .inner
      .source
      .memory_coordinator()
      .reserve(MemoryOwner::Query, binding_bytes, AdmissionClass::Workload)
      .map_err(|error| source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_memory", error.to_string()))?;
    let fields = compile_auxiliary_bindings(&self.inner, plan, limits)?;
    let lookup = self.inner.workspace.open_lookup().map_err(map_workspace_error)?;
    Ok(NativeAuthoritativeAuxiliarySourceV1 {
      inner: Arc::clone(&self.inner),
      plan,
      lookup,
      fields,
      maximum_path_bytes: limits.maximum_path_bytes,
      _memory: memory,
    })
  }

  pub fn open_candidate_artifact_source(&self) -> NativeQueryCandidateArtifactSourceV1 {
    NativeQueryCandidateArtifactSourceV1 { inner: Arc::clone(&self.inner), lookup: None }
  }
}

impl ArtifactCursorSourceV1 for NativeQueryCandidateArtifactSourceV1 {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<RetainedArtifactBytesV1, ArtifactCursorReadErrorV1> {
    let reader =
      self.inner.source.selected_namespace_reader(&self.inner.view, self.inner.namespace_limits).map_err(map_native_artifact_error)?;
    reader.read_index_artifact_bytes(key, maximum_bytes)
  }
}

impl NativeQueryCandidateArtifactSourceV1 {
  pub fn resolve_complete_posting_root_v1(
    &mut self,
    request: QueryCompletePostingRootRequestV1<'_>,
  ) -> Result<QueryCompletePostingRootReceiptV1, QueryExecutionSourceErrorV1> {
    self.validate_target_request(request.selected_namespace_root, request.publication_sequence, request.scope_id, request.cancellation)?;
    let generation =
      validate_candidate_interval(request.candidate, CompiledQueryCoverageV1::Complete, request.selected_namespace_root, None)?;
    let catalog = self.candidate_catalog(request.scope_id, request.candidate, generation)?;
    let root = self.load_root(catalog, request.scope_id, generation, OrderedIndexRoleV1::Posting)?;
    Ok(QueryCompletePostingRootReceiptV1 {
      selected_namespace_root: try_clone_bytes(request.selected_namespace_root, "complete selected NamespaceRoot")?,
      publication_sequence: request.publication_sequence,
      scope_id: try_clone_bytes(request.scope_id, "complete ScopeId")?,
      index_id: try_clone_bytes(request.candidate.index_id(), "complete IndexId")?,
      generation: generation.generation,
      generation_manifest_hash: try_clone_bytes(&generation.manifest_hash, "complete generation manifest")?,
      coverage_source_root: try_clone_bytes(&generation.source_namespace_root, "complete coverage NamespaceRoot")?,
      root,
      complete: true,
    })
  }

  pub fn resolve_complete_scope_root_v1(
    &mut self,
    request: QueryCompleteScopeRootRequestV1<'_>,
  ) -> Result<QueryCompleteScopeRootReceiptV1, QueryExecutionSourceErrorV1> {
    self.validate_target_request(request.selected_namespace_root, request.publication_sequence, request.scope_id, request.cancellation)?;
    let root = self.resolve_scope_root(request.scope_id, request.selected_namespace_root, None)?;
    Ok(QueryCompleteScopeRootReceiptV1 {
      selected_namespace_root: try_clone_bytes(request.selected_namespace_root, "complete selected NamespaceRoot")?,
      publication_sequence: request.publication_sequence,
      scope_id: try_clone_bytes(request.scope_id, "complete ScopeId")?,
      root,
      complete: true,
    })
  }

  fn validate_target_request(
    &self,
    selected_namespace_root: &[u8],
    publication_sequence: u64,
    scope_id: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<(), QueryExecutionSourceErrorV1> {
    require_not_cancelled(cancellation)?;
    require_not_cancelled(self.inner.view.cancellation())?;
    validate_identity(scope_id, self.inner.view.hash_algorithm(), "candidate ScopeId")?;
    if selected_namespace_root != self.inner.view.root_metadata().hash
      || publication_sequence != self.inner.view.authority().admission.publication_sequence
    {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_candidate_target_authority",
        "candidate artifact request does not bind the captured selected root and publication",
      ));
    }
    Ok(())
  }

  fn candidate_catalog<'a>(
    &'a self,
    scope_id: &[u8],
    candidate: &CompiledQueryIndexCandidateV1,
    generation: &QueryPlanningCoverageGenerationV1,
  ) -> Result<&'a RootAwareQueryFieldCatalogV1, QueryExecutionSourceErrorV1> {
    let mut selected = None;
    for catalog in self.inner.semantic_catalog.catalogs() {
      let position = catalog.scopes.partition_point(|scope| scope.scope_id.as_slice() < scope_id);
      let Some(scope) = catalog.scopes.get(position).filter(|scope| scope.scope_id == scope_id) else {
        continue;
      };
      let matches = scope
        .indexes
        .iter()
        .filter(|index| index.index_id == candidate.index_id() && index.selected_generation.as_ref() == Some(generation))
        .count();
      if matches > 1 || matches == 1 && selected.replace(catalog).is_some() {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_candidate_catalog_ambiguous",
          "planner-selected generation has multiple semantic catalog authorities",
        ));
      }
    }
    selected.ok_or_else(|| {
      source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_candidate_catalog_missing",
        "planner-selected generation is absent from the captured semantic catalog",
      )
    })
  }

  fn load_root(
    &self,
    catalog: &RootAwareQueryFieldCatalogV1,
    scope_id: &[u8],
    generation: &QueryPlanningCoverageGenerationV1,
    role: OrderedIndexRoleV1,
  ) -> Result<Option<QueryCandidateArtifactRootV1>, QueryExecutionSourceErrorV1> {
    let reader = self.inner.source.selected_namespace_reader(&self.inner.view, self.inner.namespace_limits).map_err(map_native_error)?;
    reader
      .load_index_artifact_root(&NativeSelectedArtifactRootRequestV1 { catalog, scope_id, selected_generation: generation, role })
      .map_err(map_native_error)?
      .map(candidate_artifact_root)
      .transpose()
  }

  fn resolve_scope_root(
    &self,
    scope_id: &[u8],
    source_namespace_root: &[u8],
    source_publication_sequence: Option<u64>,
  ) -> Result<Option<QueryCandidateArtifactRootV1>, QueryExecutionSourceErrorV1> {
    validate_identity(source_namespace_root, self.inner.view.hash_algorithm(), "coverage NamespaceRoot")?;
    let mut resolved: Option<Option<QueryCandidateArtifactRootV1>> = None;
    let mut matched = false;
    for catalog in self.inner.semantic_catalog.catalogs() {
      let position = catalog.scopes.partition_point(|scope| scope.scope_id.as_slice() < scope_id);
      let Some(scope) = catalog.scopes.get(position).filter(|scope| scope.scope_id == scope_id) else {
        continue;
      };
      for candidate in &scope.indexes {
        let Some(generation) = candidate.selected_generation.as_ref() else {
          continue;
        };
        if generation.source_namespace_root != source_namespace_root
          || source_publication_sequence.is_some_and(|sequence| generation.coverage_publication_sequence != sequence)
        {
          continue;
        }
        matched = true;
        let root = self.load_root(catalog, scope_id, generation, OrderedIndexRoleV1::ScopeOrdinal)?;
        merge_candidate_scope_root(&mut resolved, root)?;
      }
    }
    if !matched {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Unavailable,
        "native_candidate_scope_generation_missing",
        "captured semantic catalog has no selected generation for the requested scope interval",
      ));
    }
    resolved.ok_or_else(|| {
      source_error(
        QueryExecutionSourceErrorClassV1::Internal,
        "native_candidate_scope_resolution",
        "matched scope generations produced no root resolution outcome",
      )
    })
  }
}

fn merge_candidate_scope_root(
  resolved: &mut Option<Option<QueryCandidateArtifactRootV1>>,
  root: Option<QueryCandidateArtifactRootV1>,
) -> Result<(), QueryExecutionSourceErrorV1> {
  match resolved.as_ref() {
    None => *resolved = Some(root),
    Some(previous) if previous == &root => {}
    Some(_) => {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_candidate_scope_root_disagreement",
        "matching planner-selected generations disagree on their dependent ScopeOrdinal root",
      ));
    }
  }
  Ok(())
}

impl QueryPartialCandidateArtifactSourceV1 for NativeQueryCandidateArtifactSourceV1 {
  fn resolve_partial_posting_root(
    &mut self,
    request: QueryPartialPostingRootRequestV1<'_>,
  ) -> Result<QueryPartialPostingRootReceiptV1, IndexPartialSourceErrorV1> {
    self
      .validate_target_request(request.target_namespace_root, request.target_publication_sequence, request.scope_id, request.cancellation)
      .map_err(map_execution_partial_error)?;
    let generation = validate_candidate_interval(
      request.candidate,
      CompiledQueryCoverageV1::PartialExact,
      request.source_namespace_root,
      Some(request.source_publication_sequence),
    )
    .map_err(map_execution_partial_error)?;
    let catalog = self.candidate_catalog(request.scope_id, request.candidate, generation).map_err(map_execution_partial_error)?;
    let root = self.load_root(catalog, request.scope_id, generation, OrderedIndexRoleV1::Posting).map_err(map_execution_partial_error)?;
    Ok(QueryPartialPostingRootReceiptV1 {
      target_namespace_root: clone_partial_bytes(request.target_namespace_root, "partial target NamespaceRoot")?,
      target_publication_sequence: request.target_publication_sequence,
      source_namespace_root: clone_partial_bytes(request.source_namespace_root, "partial source NamespaceRoot")?,
      source_publication_sequence: request.source_publication_sequence,
      scope_id: clone_partial_bytes(request.scope_id, "partial ScopeId")?,
      index_id: clone_partial_bytes(request.candidate.index_id(), "partial IndexId")?,
      generation: generation.generation,
      generation_manifest_hash: clone_partial_bytes(&generation.manifest_hash, "partial generation manifest")?,
      root,
      complete: true,
    })
  }

  fn resolve_partial_scope_root(
    &mut self,
    request: QueryPartialScopeRootRequestV1<'_>,
  ) -> Result<QueryPartialScopeRootReceiptV1, IndexPartialSourceErrorV1> {
    require_not_cancelled(request.cancellation).map_err(map_execution_partial_error)?;
    require_not_cancelled(self.inner.view.cancellation()).map_err(map_execution_partial_error)?;
    validate_identity(request.scope_id, self.inner.view.hash_algorithm(), "partial ScopeId").map_err(map_execution_partial_error)?;
    let root = self
      .resolve_scope_root(request.scope_id, request.source_namespace_root, Some(request.source_publication_sequence))
      .map_err(map_execution_partial_error)?;
    Ok(QueryPartialScopeRootReceiptV1 {
      source_namespace_root: clone_partial_bytes(request.source_namespace_root, "partial source NamespaceRoot")?,
      source_publication_sequence: request.source_publication_sequence,
      scope_id: clone_partial_bytes(request.scope_id, "partial ScopeId")?,
      root,
      complete: true,
    })
  }
}

impl QueryCompleteCandidateSourceV1 for NativeQueryCandidateArtifactSourceV1 {
  fn resolve_complete_posting_root(
    &mut self,
    request: QueryCompletePostingRootRequestV1<'_>,
  ) -> Result<QueryCompletePostingRootReceiptV1, QueryExecutionSourceErrorV1> {
    self.resolve_complete_posting_root_v1(request)
  }

  fn resolve_complete_scope_root(
    &mut self,
    request: QueryCompleteScopeRootRequestV1<'_>,
  ) -> Result<QueryCompleteScopeRootReceiptV1, QueryExecutionSourceErrorV1> {
    self.resolve_complete_scope_root_v1(request)
  }

  fn recheck_complete_candidate(
    &mut self,
    request: QueryCandidateRecheckRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryCandidateRecheckReceiptV1, QueryExecutionScanErrorV1> {
    self
      .validate_target_request(request.selected_namespace_root, request.publication_sequence, request.scope_id, request.cancellation)
      .map_err(QueryExecutionScanErrorV1::Source)?;
    validate_identity(request.file_key, self.inner.view.hash_algorithm(), "candidate FileKey")
      .map_err(QueryExecutionScanErrorV1::Source)?;
    validate_identity(request.indexed_revision, self.inner.view.hash_algorithm(), "candidate RecordRevision")
      .map_err(QueryExecutionScanErrorV1::Source)?;
    validate_canonical_absolute_path(request.indexed_path)
      .map_err(|error| source_error(QueryExecutionSourceErrorClassV1::Corrupt, "native_candidate_path", error.to_string()))
      .map_err(QueryExecutionScanErrorV1::Source)?;
    if !path_is_within(&self.inner.query_path, request.indexed_path) {
      return Err(QueryExecutionScanErrorV1::Source(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_candidate_path",
        "complete candidate path is outside the captured query path",
      )));
    }
    if self.lookup.is_none() {
      self.lookup = Some(self.inner.workspace.open_lookup().map_err(map_workspace_error).map_err(QueryExecutionScanErrorV1::Source)?);
    }
    let lookup = self.lookup.as_mut().ok_or_else(|| {
      QueryExecutionScanErrorV1::Source(source_error(
        QueryExecutionSourceErrorClassV1::Internal,
        "native_candidate_lookup_state",
        "candidate workspace lookup was not retained after successful initialization",
      ))
    })?;
    let ordered = lookup
      .find_row(request.file_key, request.cancellation)
      .map_err(map_workspace_error)
      .map_err(QueryExecutionScanErrorV1::Source)?
      .ok_or_else(|| {
        QueryExecutionScanErrorV1::Source(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_candidate_document_missing",
          "complete index candidate is absent from the captured selected-root workspace",
        ))
      })?;
    if ordered.scope_id() != Some(request.scope_id) || ordered.record_revision() != request.indexed_revision {
      return Err(QueryExecutionScanErrorV1::Source(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_candidate_identity_stale",
        "complete index candidate scope or revision differs from selected-root authority",
      )));
    }
    let reader = self
      .inner
      .source
      .selected_namespace_reader(&self.inner.view, self.inner.namespace_limits)
      .map_err(map_native_error)
      .map_err(QueryExecutionScanErrorV1::Source)?;
    let row = reader
      .restore_ordered_file_row(ordered.file_key(), ordered.record_revision(), ordered.entity_version(), ordered.encoded_file_record())
      .map_err(map_native_error)
      .map_err(QueryExecutionScanErrorV1::Source)?;
    if row.path() != request.indexed_path || !path_is_within(&self.inner.query_path, row.path()) {
      return Err(QueryExecutionScanErrorV1::Source(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_candidate_path_stale",
        "complete index candidate path differs from selected-root authority",
      )));
    }
    let mut fields = NativeAuthoritativeRowFieldSourceV1 {
      inner: Arc::clone(&self.inner),
      row: &row,
      effective_scope_id: request.scope_id,
      source_limits: selected_source_limits().map_err(QueryExecutionScanErrorV1::Source)?,
    };
    visitor
      .visit(QueryExecutionDocumentV1 { file_key: row.file_key(), record_revision: row.record_revision(), path: row.path() }, &mut fields)
      .map_err(QueryExecutionScanErrorV1::Visitor)?;
    require_not_cancelled(request.cancellation).map_err(QueryExecutionScanErrorV1::Source)?;
    require_not_cancelled(self.inner.view.cancellation()).map_err(QueryExecutionScanErrorV1::Source)?;
    Ok(QueryCandidateRecheckReceiptV1 {
      selected_namespace_root: try_clone_bytes(request.selected_namespace_root, "candidate selected NamespaceRoot")
        .map_err(QueryExecutionScanErrorV1::Source)?,
      publication_sequence: request.publication_sequence,
      scope_id: try_clone_bytes(request.scope_id, "candidate ScopeId").map_err(QueryExecutionScanErrorV1::Source)?,
      file_key: try_clone_bytes(request.file_key, "candidate FileKey").map_err(QueryExecutionScanErrorV1::Source)?,
      indexed_revision: try_clone_bytes(request.indexed_revision, "candidate RecordRevision").map_err(QueryExecutionScanErrorV1::Source)?,
      indexed_path: try_clone_string(request.indexed_path, "candidate path").map_err(QueryExecutionScanErrorV1::Source)?,
      document_count: 1,
      complete: true,
    })
  }
}

impl QueryAuthoritativeScopeSourceV1 for NativeAuthoritativeFieldPartitionSourceV1 {
  fn scan_scope(
    &mut self,
    request: QueryExecutionScopeScanRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryExecutionScopeScanReceiptV1, QueryExecutionScanErrorV1> {
    validate_scope_scan_request(&self.inner, &request).map_err(QueryExecutionScanErrorV1::Source)?;
    let source_limits = selected_source_limits().map_err(QueryExecutionScanErrorV1::Source)?;
    let mut ordering = self.inner.workspace.open_cursor().map_err(map_workspace_error).map_err(QueryExecutionScanErrorV1::Source)?;
    let mut document_count = 0u64;
    while let Some(ordered) =
      ordering.next_row(request.cancellation).map_err(map_workspace_error).map_err(QueryExecutionScanErrorV1::Source)?
    {
      if ordered.scope_id() != Some(request.scope_id) {
        continue;
      }
      let next_document_count = document_count.checked_add(1).ok_or_else(|| {
        QueryExecutionScanErrorV1::Source(source_error(
          QueryExecutionSourceErrorClassV1::ResourceLimit,
          "native_scope_document_count",
          "native scope document count overflowed",
        ))
      })?;
      if next_document_count > request.maximum_documents {
        return Err(QueryExecutionScanErrorV1::Source(source_error(
          QueryExecutionSourceErrorClassV1::ResourceLimit,
          "native_scope_document_limit",
          "native authoritative scope exceeds its requested document bound",
        )));
      }
      let reader = self
        .inner
        .source
        .selected_namespace_reader(&self.inner.view, self.inner.namespace_limits)
        .map_err(map_native_error)
        .map_err(QueryExecutionScanErrorV1::Source)?;
      let row = reader
        .restore_ordered_file_row(ordered.file_key(), ordered.record_revision(), ordered.entity_version(), ordered.encoded_file_record())
        .map_err(map_native_error)
        .map_err(QueryExecutionScanErrorV1::Source)?;
      if !path_is_within(&self.inner.query_path, row.path()) {
        return Err(QueryExecutionScanErrorV1::Source(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_scope_document_path",
          "selected-root scope document is outside the captured query path",
        )));
      }
      let mut fields = NativeAuthoritativeRowFieldSourceV1 {
        inner: Arc::clone(&self.inner),
        row: &row,
        effective_scope_id: request.scope_id,
        source_limits,
      };
      visitor
        .visit(QueryExecutionDocumentV1 { file_key: row.file_key(), record_revision: row.record_revision(), path: row.path() }, &mut fields)
        .map_err(QueryExecutionScanErrorV1::Visitor)?;
      require_not_cancelled(request.cancellation).map_err(QueryExecutionScanErrorV1::Source)?;
      require_not_cancelled(self.inner.view.cancellation()).map_err(QueryExecutionScanErrorV1::Source)?;
      document_count = next_document_count;
    }
    Ok(QueryExecutionScopeScanReceiptV1 {
      selected_namespace_root: try_clone_bytes(request.selected_namespace_root, "scope selected NamespaceRoot")
        .map_err(QueryExecutionScanErrorV1::Source)?,
      publication_sequence: request.publication_sequence,
      scope_id: try_clone_bytes(request.scope_id, "scope ScopeId").map_err(QueryExecutionScanErrorV1::Source)?,
      document_count,
      complete: true,
    })
  }
}

impl QueryAuthoritativeFieldSourceV1 for NativeAuthoritativeRowFieldSourceV1<'_, '_> {
  fn scan_field_values(
    &mut self,
    request: QueryExecutionFieldReadRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeValueVisitorV1,
  ) -> Result<QueryExecutionFieldReadReceiptV1, QueryExecutionScanErrorV1> {
    validate_row_field_request(self, &request).map_err(QueryExecutionScanErrorV1::Source)?;
    let catalog_index =
      unique_field_catalog(self.inner.semantic_catalog.catalogs(), request.field_name).map_err(QueryExecutionScanErrorV1::Source)?;
    let reader = self
      .inner
      .source
      .selected_namespace_reader(&self.inner.view, self.inner.namespace_limits)
      .map_err(map_native_error)
      .map_err(QueryExecutionScanErrorV1::Source)?;
    let evaluation =
      evaluate_authoritative_field(&self.inner, &reader, catalog_index, self.row, self.effective_scope_id, self.source_limits)
        .map_err(QueryExecutionScanErrorV1::Source)?;
    validate_values(&evaluation.values, request.maximum_values, request.maximum_canonical_value_bytes)
      .map_err(QueryExecutionScanErrorV1::Source)?;
    let canonical_value_bytes = evaluation.values.iter().try_fold(0u64, |total, value| {
      total.checked_add(value.len() as u64).ok_or_else(|| {
        QueryExecutionScanErrorV1::Source(source_error(
          QueryExecutionSourceErrorClassV1::ResourceLimit,
          "native_row_field_value_bytes",
          "canonical field value byte count overflowed",
        ))
      })
    })?;
    for value in &evaluation.values {
      require_not_cancelled(request.cancellation).map_err(QueryExecutionScanErrorV1::Source)?;
      visitor.visit(value).map_err(QueryExecutionScanErrorV1::Visitor)?;
    }
    require_not_cancelled(request.cancellation).map_err(QueryExecutionScanErrorV1::Source)?;
    require_not_cancelled(self.inner.view.cancellation()).map_err(QueryExecutionScanErrorV1::Source)?;
    Ok(QueryExecutionFieldReadReceiptV1 {
      selected_namespace_root: try_clone_bytes(request.selected_namespace_root, "field selected NamespaceRoot")
        .map_err(QueryExecutionScanErrorV1::Source)?,
      scope_id: try_clone_bytes(request.scope_id, "field ScopeId").map_err(QueryExecutionScanErrorV1::Source)?,
      file_key: try_clone_bytes(request.file_key, "field FileKey").map_err(QueryExecutionScanErrorV1::Source)?,
      record_revision: try_clone_bytes(request.record_revision, "field RecordRevision").map_err(QueryExecutionScanErrorV1::Source)?,
      field_name: try_clone_string(request.field_name, "field name").map_err(QueryExecutionScanErrorV1::Source)?,
      state: evaluation.state,
      value_count: evaluation.values.len() as u64,
      canonical_value_bytes,
      complete: true,
    })
  }
}

impl NativeAuthoritativeAuxiliarySourceV1<'_> {
  fn restore_document(
    &mut self,
    file_key: &[u8],
    record_revision: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<Option<NativeRestoredAuxiliaryDocumentV1>, QueryExecutionSourceErrorV1> {
    require_not_cancelled(cancellation)?;
    require_not_cancelled(self.inner.view.cancellation())?;
    validate_identity(file_key, self.inner.view.hash_algorithm(), "requested FileKey")?;
    validate_identity(record_revision, self.inner.view.hash_algorithm(), "requested RecordRevision")?;
    let Some(ordered) = self.lookup.find_row(file_key, cancellation).map_err(map_workspace_error)? else {
      return Ok(None);
    };
    if ordered.record_revision() != record_revision {
      return Ok(None);
    }
    let scope_id = ordered.scope_id().map(|scope_id| try_clone_bytes(scope_id, "effective ScopeId")).transpose()?;
    let reader = self.inner.source.selected_namespace_reader(&self.inner.view, self.inner.namespace_limits).map_err(map_native_error)?;
    let row = reader
      .restore_ordered_file_row(ordered.file_key(), ordered.record_revision(), ordered.entity_version(), ordered.encoded_file_record())
      .map_err(map_native_error)?;
    if !path_is_within(&self.inner.query_path, row.path()) || row.path().len() as u64 > self.maximum_path_bytes {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_auxiliary_document_path",
        "selected-root auxiliary document is outside the query path or exceeds its admitted path bound",
      ));
    }
    Ok(Some(NativeRestoredAuxiliaryDocumentV1 { row, effective_scope_id: scope_id }))
  }

  fn evaluate_field(
    &self,
    binding: &NativeAuxiliaryFieldBindingV1,
    row: &super::read_view_native::NativeSelectedNamespaceFileRowV1,
    effective_scope_id: Option<&[u8]>,
    maximum_values: u64,
    maximum_bytes: u64,
  ) -> Result<NativeAuxiliaryFieldEvaluationV1, QueryExecutionSourceErrorV1> {
    let Some(effective_scope_id) = effective_scope_id else {
      return Ok(NativeAuxiliaryFieldEvaluationV1 { scope_id: None, state: QueryExecutionFieldStateV1::Missing, values: Vec::new() });
    };
    let scope_position = binding.scopes.partition_point(|scope| scope.scope_id.as_slice() < effective_scope_id);
    let Some(scope_binding) = binding.scopes.get(scope_position).filter(|scope| scope.scope_id == effective_scope_id) else {
      return Ok(NativeAuxiliaryFieldEvaluationV1 { scope_id: None, state: QueryExecutionFieldStateV1::Missing, values: Vec::new() });
    };
    let catalog = &self.inner.semantic_catalog.catalogs()[binding.catalog_index];
    let catalog_scope = &catalog.scopes[scope_binding.catalog_scope_index];
    let reader = self.inner.source.selected_namespace_reader(&self.inner.view, self.inner.namespace_limits).map_err(map_native_error)?;
    let evaluation = reader
      .prepare_authoritative_source(catalog, effective_scope_id, selected_source_limits()?)
      .and_then(|evaluator| evaluator.evaluate(row, NativeSelectedSourceParserV1::Native, None))
      .map_err(map_native_error)?;
    if evaluation.selected_root() != self.inner.view.root_metadata().hash
      || evaluation.semantic_state_root() != self.inner.view.authority().semantic_state.object_id
      || evaluation.scope_id() != effective_scope_id
      || evaluation.value_store_id() != catalog_scope.value_store_id
      || evaluation.file_key() != row.file_key()
      || evaluation.record_revision() != row.record_revision()
    {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_auxiliary_source_receipt",
        "selected source evaluation receipt disagrees with its plan-bound document and definition",
      ));
    }
    match evaluation.into_outcome() {
      NativeSelectedSourceOutcomeV1::Missing => Ok(NativeAuxiliaryFieldEvaluationV1 {
        scope_id: Some(try_clone_bytes(effective_scope_id, "aggregate ScopeId")?),
        state: QueryExecutionFieldStateV1::Missing,
        values: Vec::new(),
      }),
      NativeSelectedSourceOutcomeV1::Values(values) => {
        let logical = convert_auxiliary_values(
          binding,
          catalog_scope,
          scope_binding,
          self.plan.hash_algorithm(),
          &values,
          maximum_values,
          maximum_bytes,
        )?;
        Ok(NativeAuxiliaryFieldEvaluationV1 {
          scope_id: Some(try_clone_bytes(effective_scope_id, "aggregate ScopeId")?),
          state: QueryExecutionFieldStateV1::Values,
          values: logical,
        })
      }
      NativeSelectedSourceOutcomeV1::ParserUnindexable(_) | NativeSelectedSourceOutcomeV1::SourceUnindexable { .. } => {
        Ok(NativeAuxiliaryFieldEvaluationV1 {
          scope_id: Some(try_clone_bytes(effective_scope_id, "aggregate ScopeId")?),
          state: QueryExecutionFieldStateV1::DeterministicUnindexable,
          values: Vec::new(),
        })
      }
      NativeSelectedSourceOutcomeV1::OutOfScope => Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_auxiliary_scope_resolution",
        "effective-scope winner was out of scope during auxiliary evaluation",
      )),
    }
  }

  fn field_binding(&self, field_name: &str) -> Result<&NativeAuxiliaryFieldBindingV1, QueryExecutionSourceErrorV1> {
    self.fields.iter().find(|field| field.field_name == field_name).ok_or_else(|| {
      source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_auxiliary_field_binding",
        format!("compiled auxiliary field {field_name:?} has no plan-bound native definition"),
      )
    })
  }
}

fn validate_scope_scan_request(
  inner: &NativeAuthoritativeFieldPartitionInnerV1,
  request: &QueryExecutionScopeScanRequestV1<'_>,
) -> Result<(), QueryExecutionSourceErrorV1> {
  require_not_cancelled(request.cancellation)?;
  require_not_cancelled(inner.view.cancellation())?;
  validate_identity(request.scope_id, inner.view.hash_algorithm(), "requested ScopeId")?;
  validate_canonical_absolute_path(request.query_path)
    .map_err(|error| source_error(QueryExecutionSourceErrorClassV1::Corrupt, "native_scope_query_path", error.to_string()))?;
  if request.selected_namespace_root != inner.view.root_metadata().hash
    || request.publication_sequence != inner.view.authority().admission.publication_sequence
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_scope_authority",
      "authoritative scope request does not bind the captured selected root and publication",
    ));
  }
  if request.query_path != inner.query_path {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_scope_query_path",
      "authoritative scope request differs from the captured query path",
    ));
  }
  if request.maximum_documents == 0 {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_scope_document_limit",
      "authoritative scope document limit must be nonzero",
    ));
  }
  if !inner.semantic_catalog.scope_definitions().iter().any(|scope| scope.scope_id() == request.scope_id) {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_scope_definition",
      "requested ScopeId is absent from the captured semantic authority",
    ));
  }
  Ok(())
}

fn validate_row_field_request(
  source: &NativeAuthoritativeRowFieldSourceV1<'_, '_>,
  request: &QueryExecutionFieldReadRequestV1<'_>,
) -> Result<(), QueryExecutionSourceErrorV1> {
  require_not_cancelled(request.cancellation)?;
  require_not_cancelled(source.inner.view.cancellation())?;
  validate_identity(request.scope_id, source.inner.view.hash_algorithm(), "field ScopeId")?;
  validate_identity(request.file_key, source.inner.view.hash_algorithm(), "field FileKey")?;
  validate_identity(request.record_revision, source.inner.view.hash_algorithm(), "field RecordRevision")?;
  if request.selected_namespace_root != source.inner.view.root_metadata().hash
    || request.scope_id != source.effective_scope_id
    || request.file_key != source.row.file_key()
    || request.record_revision != source.row.record_revision()
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_row_field_authority",
      "field request does not bind the selected-root row and effective scope",
    ));
  }
  if request.field_name.is_empty() {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_row_field_name",
      "authoritative field request has an empty field name",
    ));
  }
  if request.maximum_values == 0 || request.maximum_canonical_value_bytes == 0 {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_row_field_limits",
      "authoritative field value and byte limits must be nonzero",
    ));
  }
  Ok(())
}

fn evaluate_authoritative_field(
  inner: &NativeAuthoritativeFieldPartitionInnerV1,
  reader: &NativeSelectedNamespaceReaderV1<'_>,
  catalog_index: usize,
  row: &super::read_view_native::NativeSelectedNamespaceFileRowV1,
  effective_scope_id: &[u8],
  source_limits: NativeSelectedSourceLimitsV1,
) -> Result<NativeAuthoritativeFieldEvaluationV1, QueryExecutionSourceErrorV1> {
  let catalog = inner.semantic_catalog.catalogs().get(catalog_index).ok_or_else(|| {
    source_error(
      QueryExecutionSourceErrorClassV1::Internal,
      "native_row_field_catalog_index",
      "selected field catalog index is outside the captured catalog",
    )
  })?;
  let scope_position = catalog.scopes.partition_point(|scope| scope.scope_id.as_slice() < effective_scope_id);
  let catalog_scope = catalog.scopes.get(scope_position).filter(|scope| scope.scope_id == effective_scope_id).ok_or_else(|| {
    source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_row_field_scope_catalog",
      "effective ScopeId is absent from the selected field catalog",
    )
  })?;
  let evaluation = reader
    .prepare_authoritative_source(catalog, effective_scope_id, source_limits)
    .and_then(|evaluator| evaluator.evaluate(row, NativeSelectedSourceParserV1::Native, None))
    .map_err(map_native_error)?;
  if evaluation.selected_root() != inner.view.root_metadata().hash
    || evaluation.semantic_state_root() != inner.view.authority().semantic_state.object_id
    || evaluation.scope_id() != effective_scope_id
    || evaluation.value_store_id() != catalog_scope.value_store_id
    || evaluation.file_key() != row.file_key()
    || evaluation.record_revision() != row.record_revision()
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_row_field_source_receipt",
      "selected source evaluation receipt disagrees with its captured row and field definition",
    ));
  }
  match evaluation.into_outcome() {
    NativeSelectedSourceOutcomeV1::Missing => {
      Ok(NativeAuthoritativeFieldEvaluationV1 { state: QueryExecutionFieldStateV1::Missing, values: Vec::new() })
    }
    NativeSelectedSourceOutcomeV1::Values(values) => {
      Ok(NativeAuthoritativeFieldEvaluationV1 { state: QueryExecutionFieldStateV1::Values, values })
    }
    NativeSelectedSourceOutcomeV1::ParserUnindexable(_) | NativeSelectedSourceOutcomeV1::SourceUnindexable { .. } => {
      Ok(NativeAuthoritativeFieldEvaluationV1 { state: QueryExecutionFieldStateV1::DeterministicUnindexable, values: Vec::new() })
    }
    NativeSelectedSourceOutcomeV1::OutOfScope => Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_row_field_scope_resolution",
      "effective-scope winner was out of scope during authoritative evaluation",
    )),
  }
}

impl QueryAuthoritativeFieldPartitionSourceV1 for NativeAuthoritativeFieldPartitionSourceV1 {
  fn open_field_partition(
    &mut self,
    request: QueryExecutionFieldPartitionOpenRequestV1<'_>,
  ) -> Result<Box<dyn QueryAuthoritativeFieldPartitionCursorV1>, QueryExecutionSourceErrorV1> {
    require_not_cancelled(request.cancellation)?;
    validate_open_request(&self.inner, &request)?;
    let catalog_index = unique_field_catalog(self.inner.semantic_catalog.catalogs(), request.field_name)?;
    let catalog = &self.inner.semantic_catalog.catalogs()[catalog_index];
    let mut scope_ids = Vec::new();
    scope_ids.try_reserve_exact(request.scope_ids.len()).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_partition_scope_allocation",
        format!("cannot retain requested ScopeIds: {error}"),
      )
    })?;
    let mut prior: Option<&[u8]> = None;
    for scope_id in request.scope_ids {
      validate_identity(scope_id, self.inner.view.hash_algorithm(), "requested ScopeId")?;
      if prior.is_some_and(|prior| prior >= *scope_id) {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_partition_scope_order",
          "requested field ScopeIds are not strict and unique",
        ));
      }
      let scope_position = catalog.scopes.partition_point(|scope| scope.scope_id.as_slice() < *scope_id);
      if catalog.scopes.get(scope_position).is_none_or(|scope| scope.scope_id.as_slice() != *scope_id) {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_partition_scope_catalog",
          "requested ScopeId is absent from the selected field catalog",
        ));
      }
      scope_ids.push(try_clone_bytes(scope_id, "requested ScopeId")?);
      prior = Some(scope_id);
    }
    if scope_ids.is_empty() || scope_ids.len() > MAXIMUM_PARTITION_SCOPES {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_partition_scope_count",
        "field partition requires a nonempty bounded ScopeId set",
      ));
    }
    let source_limits = selected_source_limits()?;
    let cursor_memory_bytes = cursor_memory_bytes(&request, self.inner.view.hash_algorithm(), scope_ids.len())?;
    let cursor_memory =
      self.inner.source.memory_coordinator().reserve(MemoryOwner::Query, cursor_memory_bytes, AdmissionClass::Workload).map_err(
        |error| source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_cursor_memory", error.to_string()),
      )?;
    let cursor = self.inner.workspace.open_cursor().map_err(map_workspace_error)?;
    let mut scope_document_counts = Vec::new();
    scope_document_counts.try_reserve_exact(scope_ids.len()).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_partition_scope_allocation",
        format!("cannot allocate ScopeId counters: {error}"),
      )
    })?;
    scope_document_counts.resize(scope_ids.len(), 0);
    Ok(Box::new(NativeAuthoritativeFieldPartitionCursorV1 {
      inner: Arc::clone(&self.inner),
      ordering: cursor,
      field_name: try_clone_string(request.field_name, "field name")?,
      catalog_index,
      scope_ids,
      scope_document_counts,
      unconfigured_document_count: 0,
      document_count: 0,
      maximum_documents: request.maximum_documents,
      maximum_values_per_document: request.maximum_values_per_document,
      maximum_canonical_value_bytes_per_document: request.maximum_canonical_value_bytes_per_document,
      maximum_path_bytes: request.maximum_path_bytes,
      source_limits,
      exhausted: false,
      finished: false,
      failed: false,
      _memory: cursor_memory,
    }))
  }
}

struct NativeAuthoritativeFieldPartitionCursorV1 {
  inner: Arc<NativeAuthoritativeFieldPartitionInnerV1>,
  ordering: NativeQueryOrderingCursorV1,
  field_name: String,
  catalog_index: usize,
  scope_ids: Vec<Vec<u8>>,
  scope_document_counts: Vec<u64>,
  unconfigured_document_count: u64,
  document_count: u64,
  maximum_documents: u64,
  maximum_values_per_document: u64,
  maximum_canonical_value_bytes_per_document: u64,
  maximum_path_bytes: u64,
  source_limits: NativeSelectedSourceLimitsV1,
  exhausted: bool,
  finished: bool,
  failed: bool,
  _memory: MemoryReservation,
}

impl NativeAuthoritativeFieldPartitionCursorV1 {
  fn next_document_inner(
    &mut self,
    cancellation: &CancellationToken,
  ) -> Result<Option<QueryExecutionFieldDocumentV1>, QueryExecutionSourceErrorV1> {
    require_not_cancelled(cancellation)?;
    require_not_cancelled(self.inner.view.cancellation())?;
    if self.finished {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Internal,
        "native_partition_cursor_finished",
        "field partition cursor was read after finish",
      ));
    }
    if self.exhausted {
      return Ok(None);
    }
    let Some(ordered) = self.ordering.next_row(cancellation).map_err(map_workspace_error)? else {
      self.exhausted = true;
      return Ok(None);
    };
    let reader = self.inner.source.selected_namespace_reader(&self.inner.view, self.inner.namespace_limits).map_err(map_native_error)?;
    let row = reader
      .restore_ordered_file_row(ordered.file_key(), ordered.record_revision(), ordered.entity_version(), ordered.encoded_file_record())
      .map_err(map_native_error)?;
    if !path_is_within(&self.inner.query_path, row.path()) || row.path().len() as u64 > self.maximum_path_bytes {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_partition_document_path",
        "ordered document is outside the query path or exceeds its requested path bound",
      ));
    }
    let scope_index = ordered.scope_id().and_then(|scope_id| {
      let position = self.scope_ids.partition_point(|candidate| candidate.as_slice() < scope_id);
      self.scope_ids.get(position).is_some_and(|candidate| candidate.as_slice() == scope_id).then_some(position)
    });
    let (scope_id, state, canonical_values, next_scope_count, next_unconfigured_count) = if let Some(scope_index) = scope_index {
      let selected_scope_id = &self.scope_ids[scope_index];
      let evaluation = evaluate_authoritative_field(&self.inner, &reader, self.catalog_index, &row, selected_scope_id, self.source_limits)?;
      let next_scope_count = self.scope_document_counts[scope_index].checked_add(1).ok_or_else(|| {
        source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_scope_count", "scope document count overflowed")
      })?;
      (
        Some(try_clone_bytes(selected_scope_id, "document ScopeId")?),
        evaluation.state,
        evaluation.values,
        Some((scope_index, next_scope_count)),
        None,
      )
    } else {
      let next_unconfigured_count = self.unconfigured_document_count.checked_add(1).ok_or_else(|| {
        source_error(
          QueryExecutionSourceErrorClassV1::ResourceLimit,
          "native_partition_unconfigured_count",
          "unconfigured document count overflowed",
        )
      })?;
      (None, QueryExecutionFieldStateV1::Missing, Vec::new(), None, Some(next_unconfigured_count))
    };
    validate_values(&canonical_values, self.maximum_values_per_document, self.maximum_canonical_value_bytes_per_document)?;
    let next_document_count = self.document_count.checked_add(1).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_document_count", "document count overflowed")
    })?;
    if next_document_count > self.maximum_documents {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_partition_document_limit",
        "native field partition exceeds its requested document bound",
      ));
    }
    let document = QueryExecutionFieldDocumentV1 {
      scope_id,
      file_key: try_clone_bytes(row.file_key(), "document FileKey")?,
      record_revision: try_clone_bytes(row.record_revision(), "document RecordRevision")?,
      path: try_clone_string(row.path(), "document path")?,
      state,
      canonical_values,
    };
    if let Some((scope_index, next_scope_count)) = next_scope_count {
      self.scope_document_counts[scope_index] = next_scope_count;
    }
    if let Some(next_unconfigured_count) = next_unconfigured_count {
      self.unconfigured_document_count = next_unconfigured_count;
    }
    self.document_count = next_document_count;
    Ok(Some(document))
  }

  fn finish_inner(&mut self) -> Result<QueryExecutionFieldPartitionReceiptV1, QueryExecutionSourceErrorV1> {
    if self.finished || !self.exhausted {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_partition_finish_state",
        "field partition may finish exactly once and only after exhaustion",
      ));
    }
    let receipt = QueryExecutionFieldPartitionReceiptV1 {
      selected_namespace_root: try_clone_bytes(&self.inner.view.root_metadata().hash, "receipt selected root")?,
      publication_sequence: self.inner.view.authority().admission.publication_sequence,
      field_name: try_clone_string(&self.field_name, "receipt field name")?,
      scope_ids: try_clone_nested_bytes(&self.scope_ids, "receipt ScopeIds")?,
      scope_document_counts: try_clone_u64s(&self.scope_document_counts, "receipt scope counts")?,
      unconfigured_document_count: self.unconfigured_document_count,
      document_count: self.document_count,
      complete: true,
    };
    self.finished = true;
    Ok(receipt)
  }
}

impl QueryAuthoritativeFieldPartitionCursorV1 for NativeAuthoritativeFieldPartitionCursorV1 {
  fn next_document(
    &mut self,
    cancellation: &CancellationToken,
  ) -> Result<Option<QueryExecutionFieldDocumentV1>, QueryExecutionSourceErrorV1> {
    if self.failed {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Internal,
        "native_partition_cursor_failed",
        "field partition cursor cannot continue after a prior failure",
      ));
    }
    match self.next_document_inner(cancellation) {
      Ok(document) => Ok(document),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  fn finish(&mut self) -> Result<QueryExecutionFieldPartitionReceiptV1, QueryExecutionSourceErrorV1> {
    if self.failed {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Internal,
        "native_partition_cursor_failed",
        "field partition cursor cannot finish after a prior failure",
      ));
    }
    match self.finish_inner() {
      Ok(receipt) => Ok(receipt),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }
}

impl PositionUniverseSourceV1 for NativeAuthoritativeAuxiliarySourceV1<'_> {
  fn resolve_position(
    &mut self,
    request: PositionUniverseLookupRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1> {
    self.resolve_position_inner(request, cancellation).map_err(map_position_source_error)
  }
}

impl NativeAuthoritativeAuxiliarySourceV1<'_> {
  fn resolve_position_inner(
    &mut self,
    request: PositionUniverseLookupRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<PositionUniverseLookupResultV1, QueryExecutionSourceErrorV1> {
    require_not_cancelled(cancellation)?;
    if request.database_id() != self.plan.database_id()
      || request.physical_instance_id() != self.plan.physical_instance_id()
      || request.selected_root() != self.plan.selected_namespace_root()
      || request.route() != PositionRouteV1::Query
      || request.order_fingerprint() != self.plan.query_order().fingerprint()
    {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_auxiliary_position_authority",
        "position lookup does not bind the plan used to open its native source",
      ));
    }
    let Some(NativeRestoredAuxiliaryDocumentV1 { row, effective_scope_id }) =
      self.restore_document(request.file_key_tie(), request.record_revision_tie(), cancellation)?
    else {
      return Ok(PositionUniverseLookupResultV1::Absent);
    };
    let _row_memory =
      self.inner.source.memory_coordinator().reserve(MemoryOwner::Query, request.maximum_row_bytes(), AdmissionClass::Workload).map_err(
        |error| source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_position_memory", error.to_string()),
      )?;
    let mut components = Vec::new();
    components.try_reserve_exact(request.order().component_count()).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_auxiliary_position_allocation",
        format!("cannot allocate bounded logical position components: {error}"),
      )
    })?;
    for sort in request.order().sort() {
      require_not_cancelled(cancellation)?;
      if sort.field == "@path" {
        components.push(LogicalOrderComponentOwnedV1::present(
          super::position::PositionComparatorV1::Utf8Binary,
          try_clone_bytes(row.path().as_bytes(), "position path")?,
        ));
        continue;
      }
      let CompiledPositionComparatorV1::Payload(comparator) = sort.comparator else {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_auxiliary_position_comparator",
          "query order contains a framing-only comparator",
        ));
      };
      let binding = self.field_binding(&sort.field)?;
      if binding.comparator != comparator {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_auxiliary_position_comparator",
          "query order comparator disagrees with its plan-bound field definition",
        ));
      }
      let maximum_values = request.maximum_row_bytes() / size_of::<LogicalOrderComponentOwnedV1>() as u64;
      let evaluation = self.evaluate_field(binding, &row, effective_scope_id.as_deref(), maximum_values, request.maximum_row_bytes())?;
      let component = match evaluation.state {
        QueryExecutionFieldStateV1::Missing | QueryExecutionFieldStateV1::DeterministicUnindexable => {
          LogicalOrderComponentOwnedV1::missing()
        }
        QueryExecutionFieldStateV1::Values => select_position_component(binding, sort.direction, evaluation.values)?,
      };
      components.push(component);
    }
    let result = LogicalOrderRowOwnedV1 {
      route: PositionRouteV1::Query,
      components,
      file_key_tie: try_clone_bytes(row.file_key(), "position FileKey")?,
      record_revision_tie: try_clone_bytes(row.record_revision(), "position RecordRevision")?,
    };
    let row_bytes = logical_order_row_allocated_bytes_v1(&result).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_auxiliary_position_row_bytes",
        format!("cannot account native position row: {error}"),
      )
    })?;
    if row_bytes > request.maximum_row_bytes() {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_auxiliary_position_row_limit",
        format!("native position row requires {row_bytes} bytes, cap is {}", request.maximum_row_bytes()),
      ));
    }
    Ok(PositionUniverseLookupResultV1::Found(result))
  }
}

impl QueryAggregateInputSourceV1 for NativeAuthoritativeAuxiliarySourceV1<'_> {
  fn resolve_aggregate_input(
    &mut self,
    request: QueryAggregateInputLookupRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<QueryAggregateInputLookupResultV1, QueryExecutionSourceErrorV1> {
    require_not_cancelled(cancellation)?;
    validate_aggregate_request(self, request)?;
    let Some(NativeRestoredAuxiliaryDocumentV1 { row, effective_scope_id }) =
      self.restore_document(request.file_key(), request.record_revision(), cancellation)?
    else {
      return Ok(QueryAggregateInputLookupResultV1::Absent);
    };
    let _row_memory = self
      .inner
      .source
      .memory_coordinator()
      .reserve(MemoryOwner::Query, request.limits().maximum_row_bytes(), AdmissionClass::Workload)
      .map_err(|error| {
        source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_aggregate_memory", error.to_string())
      })?;
    let mut fields = Vec::new();
    fields.try_reserve_exact(request.fields().len()).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_auxiliary_aggregate_allocation",
        format!("cannot allocate bounded aggregate fields: {error}"),
      )
    })?;
    let mut remaining_values = request.limits().maximum_total_values();
    for expected in request.fields() {
      require_not_cancelled(cancellation)?;
      let binding = self.field_binding(expected.field_name())?;
      let maximum_values = request.limits().maximum_values_per_field().min(remaining_values);
      let evaluation =
        self.evaluate_field(binding, &row, effective_scope_id.as_deref(), maximum_values, request.limits().maximum_row_bytes())?;
      remaining_values = remaining_values.checked_sub(evaluation.values.len() as u64).ok_or_else(|| {
        source_error(
          QueryExecutionSourceErrorClassV1::ResourceLimit,
          "native_auxiliary_aggregate_values",
          "native aggregate fields exceed their admitted total value count",
        )
      })?;
      fields.push(QueryAggregateInputFieldV1 {
        field_name: try_clone_string(expected.field_name(), "aggregate field name")?,
        scope_id: evaluation.scope_id,
        state: evaluation.state,
        values: evaluation.values,
      });
    }
    let result = QueryAggregateInputRowV1 {
      selected_namespace_root: try_clone_bytes(self.plan.selected_namespace_root(), "aggregate selected root")?,
      file_key: try_clone_bytes(row.file_key(), "aggregate FileKey")?,
      record_revision: try_clone_bytes(row.record_revision(), "aggregate RecordRevision")?,
      fields,
    };
    let row_bytes = query_aggregate_input_row_allocated_bytes_v1(&result)?;
    if row_bytes > request.limits().maximum_row_bytes() {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_auxiliary_aggregate_row_limit",
        format!("native aggregate row requires {row_bytes} bytes, cap is {}", request.limits().maximum_row_bytes()),
      ));
    }
    Ok(QueryAggregateInputLookupResultV1::Found(result))
  }
}

fn validate_auxiliary_plan(
  inner: &NativeAuthoritativeFieldPartitionInnerV1,
  plan: &CompiledRootAwareQueryPlanV1,
) -> Result<(), QueryExecutionSourceErrorV1> {
  if plan.database_id() != inner.view.database_id()
    || plan.physical_instance_id() != inner.view.physical_instance_id()
    || plan.hash_algorithm() != inner.view.hash_algorithm()
    || plan.selected_namespace_root() != inner.view.root_metadata().hash
    || plan.semantic_state_root() != inner.view.authority().semantic_state.object_id
    || plan.publication_sequence() != inner.view.authority().admission.publication_sequence
    || plan.query_path() != inner.query_path
    || plan.query_order().route() != PositionRouteV1::Query
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_auxiliary_plan_authority",
      "compiled query plan does not bind the native selected-root source",
    ));
  }
  Ok(())
}

fn auxiliary_binding_bytes(
  plan: &CompiledRootAwareQueryPlanV1,
  limits: NativeAuthoritativeAuxiliaryLimitsV1,
) -> Result<u64, QueryExecutionSourceErrorV1> {
  if plan.auxiliary_fields().len() > limits.maximum_fields {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_auxiliary_field_limit",
      "compiled query exceeds the native auxiliary field limit",
    ));
  }
  let mut scope_count = 0usize;
  let mut bytes = AUXILIARY_SOURCE_FIXED_BYTES;
  for field in plan.auxiliary_fields() {
    scope_count = scope_count.checked_add(field.scopes().len()).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_scope_limit", "auxiliary scope count overflowed")
    })?;
    let scope_slots = field.scopes().len().checked_mul(size_of::<NativeAuxiliaryScopeBindingV1>()).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_binding_bytes", "scope slot bytes overflowed")
    })?;
    let scope_id_bytes = field.scopes().len().checked_mul(plan.hash_algorithm().hash_length()).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_binding_bytes", "ScopeId bytes overflowed")
    })?;
    bytes = bytes
      .checked_add(size_of::<NativeAuxiliaryFieldBindingV1>() as u64)
      .and_then(|bytes| bytes.checked_add(field.field_name().len() as u64))
      .and_then(|bytes| bytes.checked_add(scope_slots as u64))
      .and_then(|bytes| bytes.checked_add(scope_id_bytes as u64))
      .ok_or_else(|| {
        source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_binding_bytes", "binding bytes overflowed")
      })?;
  }
  if scope_count > limits.maximum_scope_bindings || bytes > limits.maximum_binding_bytes {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_auxiliary_binding_limit",
      "compiled query exceeds its native auxiliary scope or retained-byte limit",
    ));
  }
  Ok(bytes)
}

fn compile_auxiliary_bindings(
  inner: &NativeAuthoritativeFieldPartitionInnerV1,
  plan: &CompiledRootAwareQueryPlanV1,
  limits: NativeAuthoritativeAuxiliaryLimitsV1,
) -> Result<Vec<NativeAuxiliaryFieldBindingV1>, QueryExecutionSourceErrorV1> {
  let mut fields: Vec<NativeAuxiliaryFieldBindingV1> = Vec::new();
  fields.try_reserve_exact(plan.auxiliary_fields().len().min(limits.maximum_fields)).map_err(|error| {
    source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_auxiliary_binding_allocation",
      format!("cannot allocate bounded field bindings: {error}"),
    )
  })?;
  for auxiliary in plan.auxiliary_fields() {
    if let Some(existing) = fields.iter().find(|binding| binding.field_name == auxiliary.field_name()) {
      if existing.comparator != auxiliary.order_semantics().comparator()
        || existing.comparison_semantics != auxiliary.order_semantics().comparison_semantics()
        || existing.collation_semantics != auxiliary.order_semantics().collation_semantics()
        || existing.behavior_fingerprint != *auxiliary.order_semantics().behavior_fingerprint()
        || existing.scopes.len() != auxiliary.scopes().len()
        || existing.scopes.iter().zip(auxiliary.scopes()).any(|(left, right)| left.scope_id != right.scope_id())
      {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_auxiliary_duplicate_field",
          "duplicate auxiliary operations disagree on selected-root field semantics",
        ));
      }
      continue;
    }
    let catalog_index = unique_field_catalog(inner.semantic_catalog.catalogs(), auxiliary.field_name())?;
    let catalog = &inner.semantic_catalog.catalogs()[catalog_index];
    let mut scopes = Vec::new();
    scopes.try_reserve_exact(auxiliary.scopes().len()).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_auxiliary_binding_allocation",
        format!("cannot allocate bounded scope bindings: {error}"),
      )
    })?;
    for planned_scope in auxiliary.scopes() {
      let catalog_scope_index = catalog.scopes.partition_point(|scope| scope.scope_id.as_slice() < planned_scope.scope_id());
      let catalog_scope =
        catalog.scopes.get(catalog_scope_index).filter(|scope| scope.scope_id == planned_scope.scope_id()).ok_or_else(|| {
          source_error(
            QueryExecutionSourceErrorClassV1::Corrupt,
            "native_auxiliary_scope_binding",
            "plan auxiliary ScopeId is absent from its selected semantic catalog",
          )
        })?;
      let selected_candidate = planned_scope.candidates().first().ok_or_else(|| {
        source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_auxiliary_candidate_binding",
          "plan auxiliary scope has no order-preserving definition candidate",
        )
      })?;
      let catalog_index_index =
        catalog_scope.indexes.iter().position(|candidate| candidate.index_id == selected_candidate.index_id()).ok_or_else(|| {
          source_error(
            QueryExecutionSourceErrorClassV1::Corrupt,
            "native_auxiliary_candidate_binding",
            "plan auxiliary IndexId is absent from its selected semantic catalog",
          )
        })?;
      let candidate = &catalog_scope.indexes[catalog_index_index];
      let runtime = IndexDefinitionRuntimeV1::from_encoded(
        &catalog_scope.encoded_value_store_definition,
        &candidate.encoded_field_definition,
        plan.hash_algorithm(),
      )
      .map_err(map_index_definition_error)?;
      if runtime.index_id() != selected_candidate.index_id()
        || runtime.strategy().name != selected_candidate.strategy_name()
        || super::position::PositionComparatorV1::from_name(runtime.converter().definition().name)
          != Some(auxiliary.order_semantics().comparator())
        || runtime.converter().definition().comparison_semantics != auxiliary.order_semantics().comparison_semantics()
        || runtime.converter().definition().collation_semantics != auxiliary.order_semantics().collation_semantics()
        || runtime.converter().registry().behavior_fingerprint != *auxiliary.order_semantics().behavior_fingerprint()
      {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_auxiliary_semantic_binding",
          "plan auxiliary semantics disagree with the exact selected definition",
        ));
      }
      scopes.push(NativeAuxiliaryScopeBindingV1 {
        scope_id: try_clone_bytes(planned_scope.scope_id(), "auxiliary ScopeId")?,
        catalog_scope_index,
        catalog_index_index,
      });
    }
    fields.push(NativeAuxiliaryFieldBindingV1 {
      field_name: try_clone_string(auxiliary.field_name(), "auxiliary field name")?,
      comparator: auxiliary.order_semantics().comparator(),
      comparison_semantics: auxiliary.order_semantics().comparison_semantics(),
      collation_semantics: auxiliary.order_semantics().collation_semantics(),
      behavior_fingerprint: *auxiliary.order_semantics().behavior_fingerprint(),
      catalog_index,
      scopes,
    });
  }
  Ok(fields)
}

fn validate_aggregate_request(
  source: &NativeAuthoritativeAuxiliarySourceV1<'_>,
  request: QueryAggregateInputLookupRequestV1<'_>,
) -> Result<(), QueryExecutionSourceErrorV1> {
  if request.database_id() != source.plan.database_id()
    || request.physical_instance_id() != source.plan.physical_instance_id()
    || request.selected_namespace_root() != source.plan.selected_namespace_root()
    || request.query_path() != source.plan.query_path()
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_auxiliary_aggregate_authority",
      "aggregate lookup does not bind the plan used to open its native source",
    ));
  }
  for expected in request.fields() {
    let binding = source.field_binding(expected.field_name())?;
    if binding.comparator != expected.comparator()
      || binding.comparison_semantics != expected.comparison_semantics()
      || binding.collation_semantics != expected.collation_semantics()
      || binding.behavior_fingerprint != *expected.behavior_fingerprint()
      || binding.scopes.len() != expected.scope_ids().len()
      || binding.scopes.iter().zip(expected.scope_ids()).any(|(left, right)| left.scope_id != *right)
    {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_auxiliary_aggregate_semantics",
        "aggregate input semantics disagree with the plan-bound native definition",
      ));
    }
  }
  Ok(())
}

fn convert_auxiliary_values(
  binding: &NativeAuxiliaryFieldBindingV1,
  catalog_scope: &super::query_planner::QueryPlanningScopeV1,
  scope_binding: &NativeAuxiliaryScopeBindingV1,
  hash_algorithm: HashAlgorithm,
  canonical_values: &[Vec<u8>],
  maximum_values: u64,
  maximum_bytes: u64,
) -> Result<Vec<LogicalOrderComponentOwnedV1>, QueryExecutionSourceErrorV1> {
  if canonical_values.is_empty() || canonical_values.len() as u64 > maximum_values {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_auxiliary_value_count",
      "selected source values are empty or exceed the auxiliary value limit",
    ));
  }
  let logical_slot_bytes = canonical_values
    .len()
    .checked_mul(size_of::<LogicalOrderComponentOwnedV1>())
    .and_then(|bytes| bytes.checked_add(canonical_values.len().checked_mul(AUXILIARY_ALLOCATION_OVERHEAD_BYTES)?))
    .ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_value_bytes", "logical value slots overflowed")
    })? as u64;
  if logical_slot_bytes > maximum_bytes {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_auxiliary_value_bytes",
      "logical value slots exceed the auxiliary byte limit",
    ));
  }
  let candidate = &catalog_scope.indexes[scope_binding.catalog_index_index];
  let runtime = IndexDefinitionRuntimeV1::from_encoded(
    &catalog_scope.encoded_value_store_definition,
    &candidate.encoded_field_definition,
    hash_algorithm,
  )
  .map_err(map_index_definition_error)?;
  let mut non_null = Vec::new();
  non_null.try_reserve_exact(canonical_values.len()).map_err(|error| {
    source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_auxiliary_value_allocation",
      format!("cannot allocate bounded canonical conversion input: {error}"),
    )
  })?;
  let mut nulls = Vec::new();
  nulls.try_reserve_exact(canonical_values.len()).map_err(|error| {
    source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_auxiliary_value_allocation",
      format!("cannot allocate bounded null-state map: {error}"),
    )
  })?;
  let mut canonical_bytes = 0u64;
  for value in canonical_values {
    canonical_bytes = canonical_bytes.checked_add(value.len() as u64).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_value_bytes", "canonical value bytes overflowed")
    })?;
    if canonical_bytes > maximum_bytes {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_auxiliary_value_bytes",
        "canonical source values exceed the auxiliary byte limit",
      ));
    }
    let decoded = decode_canonical_value(value, CanonicalValueBounds::SOURCE_VALUE).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_auxiliary_canonical_value",
        format!("selected source returned malformed canonical data: {error}"),
      )
    })?;
    let is_null = decoded == CanonicalConfigValueV1::Null;
    nulls.push(is_null);
    if !is_null {
      non_null.push(try_clone_bytes(value, "canonical source value")?);
    }
  }
  let compiled = runtime.compile_source_values(&non_null).map_err(map_index_definition_error)?;
  if compiled.values.len() != non_null.len()
    || compiled.values.iter().any(|value| value.postings.len() != 1 || value.postings[0].expansion_ordinal != 0)
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_auxiliary_order_conversion",
      "an order-preserving definition did not emit exactly one scalar posting per source value",
    ));
  }
  let mut logical = Vec::new();
  logical.try_reserve_exact(canonical_values.len()).map_err(|error| {
    source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_auxiliary_value_allocation",
      format!("cannot allocate bounded logical values: {error}"),
    )
  })?;
  let mut compiled_values = compiled.values.into_iter();
  let mut output_bytes = logical_slot_bytes;
  for is_null in nulls {
    if is_null {
      logical.push(LogicalOrderComponentOwnedV1::typed_null());
      continue;
    }
    let value = compiled_values.next().ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::Corrupt, "native_auxiliary_order_conversion", "compiled source value disappeared")
    })?;
    let posting = value.postings.into_iter().next().ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::Corrupt, "native_auxiliary_order_conversion", "compiled posting disappeared")
    })?;
    output_bytes = output_bytes.checked_add(posting.posting_key.len() as u64).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_value_bytes", "logical value bytes overflowed")
    })?;
    if output_bytes > maximum_bytes {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_auxiliary_value_bytes",
        "logical source values exceed the auxiliary byte limit",
      ));
    }
    logical.push(LogicalOrderComponentOwnedV1::present(binding.comparator, posting.posting_key));
  }
  Ok(logical)
}

fn select_position_component(
  binding: &NativeAuxiliaryFieldBindingV1,
  direction: PositionSortDirectionV1,
  mut values: Vec<LogicalOrderComponentOwnedV1>,
) -> Result<LogicalOrderComponentOwnedV1, QueryExecutionSourceErrorV1> {
  let mut selected = values.pop().ok_or_else(|| {
    source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_auxiliary_position_values",
      "present auxiliary field returned no logical values",
    )
  })?;
  for candidate in values {
    let ordering =
      compare_logical_order_components_v1(binding.comparator, &candidate, &selected, &binding.field_name).map_err(|error| {
        source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_auxiliary_position_value",
          format!("cannot compare plan-bound position values: {error}"),
        )
      })?;
    let replace = match direction {
      PositionSortDirectionV1::Ascending => ordering == std::cmp::Ordering::Less,
      PositionSortDirectionV1::Descending => ordering == std::cmp::Ordering::Greater,
    };
    if replace {
      selected = candidate;
    }
  }
  Ok(selected)
}

fn map_index_definition_error(error: super::index_definition_runtime::IndexDefinitionErrorV1) -> QueryExecutionSourceErrorV1 {
  let class = match error.class() {
    IndexDefinitionErrorClassV1::ResourceLimit => QueryExecutionSourceErrorClassV1::ResourceLimit,
    IndexDefinitionErrorClassV1::UnsupportedDefinition => QueryExecutionSourceErrorClassV1::Unavailable,
    IndexDefinitionErrorClassV1::IdentityMismatch
    | IndexDefinitionErrorClassV1::SemanticMismatch
    | IndexDefinitionErrorClassV1::InvalidSourceValue => QueryExecutionSourceErrorClassV1::Corrupt,
  };
  source_error(class, error.code(), error.context())
}

fn validate_candidate_interval<'a>(
  candidate: &'a CompiledQueryIndexCandidateV1,
  expected_coverage: CompiledQueryCoverageV1,
  source_namespace_root: &[u8],
  source_publication_sequence: Option<u64>,
) -> Result<&'a QueryPlanningCoverageGenerationV1, QueryExecutionSourceErrorV1> {
  let generation = candidate.selected_generation().ok_or_else(|| {
    source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_candidate_generation_missing",
      "candidate artifact request has no planner-selected generation",
    )
  })?;
  if candidate.coverage() != expected_coverage
    || !candidate.proven_candidate_superset()
    || generation.source_namespace_root != source_namespace_root
    || source_publication_sequence.is_some_and(|sequence| generation.coverage_publication_sequence != sequence)
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_candidate_generation_interval",
      "candidate artifact request disagrees with its planner-selected coverage interval",
    ));
  }
  Ok(generation)
}

fn candidate_artifact_root(
  root: super::read_view_native::NativeSelectedArtifactRootV1,
) -> Result<QueryCandidateArtifactRootV1, QueryExecutionSourceErrorV1> {
  let (root_key, owner_id, generation, summary) = root.into_parts();
  QueryCandidateArtifactRootV1::new(root_key, owner_id, generation, summary).map_err(map_candidate_root_error)
}

fn map_candidate_root_error(error: QueryCompleteCandidateErrorV1) -> QueryExecutionSourceErrorV1 {
  let class = match error.class() {
    QueryCompleteCandidateErrorClassV1::InvalidRequest | QueryCompleteCandidateErrorClassV1::CorruptSource => {
      QueryExecutionSourceErrorClassV1::Corrupt
    }
    QueryCompleteCandidateErrorClassV1::ResourceLimit => QueryExecutionSourceErrorClassV1::ResourceLimit,
    QueryCompleteCandidateErrorClassV1::HistoricalViewUnavailable => QueryExecutionSourceErrorClassV1::Unavailable,
    QueryCompleteCandidateErrorClassV1::Cancelled => QueryExecutionSourceErrorClassV1::Cancelled,
    QueryCompleteCandidateErrorClassV1::Internal => QueryExecutionSourceErrorClassV1::Internal,
  };
  source_error(class, error.code(), error.context())
}

fn map_execution_partial_error(error: QueryExecutionSourceErrorV1) -> IndexPartialSourceErrorV1 {
  match error.class() {
    QueryExecutionSourceErrorClassV1::Unavailable => IndexPartialSourceErrorV1::unavailable(error.code(), error.context()),
    QueryExecutionSourceErrorClassV1::ResourceLimit => IndexPartialSourceErrorV1::resource_limit(error.code(), error.context()),
    QueryExecutionSourceErrorClassV1::Corrupt => IndexPartialSourceErrorV1::corrupt(error.code(), error.context()),
    QueryExecutionSourceErrorClassV1::Cancelled => IndexPartialSourceErrorV1::cancelled(error.code(), error.context()),
    QueryExecutionSourceErrorClassV1::Internal => IndexPartialSourceErrorV1::internal(error.code(), error.context()),
  }
}

fn clone_partial_bytes(value: &[u8], role: &'static str) -> Result<Vec<u8>, IndexPartialSourceErrorV1> {
  try_clone_bytes(value, role).map_err(map_execution_partial_error)
}

fn map_native_artifact_error(error: NativeSelectedNamespaceReadErrorV1) -> ArtifactCursorReadErrorV1 {
  match error.class() {
    NativeSelectedNamespaceReadErrorClassV1::InvalidRequest | NativeSelectedNamespaceReadErrorClassV1::Corrupt => {
      ArtifactCursorReadErrorV1::Corrupt(error.to_string())
    }
    NativeSelectedNamespaceReadErrorClassV1::ResourceLimit => ArtifactCursorReadErrorV1::ResourcePressure(error.to_string()),
    NativeSelectedNamespaceReadErrorClassV1::Unavailable => ArtifactCursorReadErrorV1::Operational(error.to_string()),
    NativeSelectedNamespaceReadErrorClassV1::Cancelled => ArtifactCursorReadErrorV1::Cancelled,
  }
}

fn map_position_source_error(error: QueryExecutionSourceErrorV1) -> PositionUniverseSourceErrorV1 {
  match error.class() {
    QueryExecutionSourceErrorClassV1::Unavailable => PositionUniverseSourceErrorV1::unavailable(error.to_string()),
    QueryExecutionSourceErrorClassV1::ResourceLimit => PositionUniverseSourceErrorV1::resource(error.to_string()),
    QueryExecutionSourceErrorClassV1::Corrupt | QueryExecutionSourceErrorClassV1::Internal => {
      PositionUniverseSourceErrorV1::corrupt(error.to_string())
    }
    QueryExecutionSourceErrorClassV1::Cancelled => PositionUniverseSourceErrorV1::cancelled(),
  }
}

fn validate_catalogs(
  view: &ResolvedReadViewV1<ResolvedPathAuthorizationV1>,
  catalogs: &[RootAwareQueryFieldCatalogV1],
) -> Result<(), QueryExecutionSourceErrorV1> {
  let mut prior_field: Option<&str> = None;
  for catalog in catalogs {
    if catalog.database_id != view.database_id()
      || catalog.physical_instance_id != view.physical_instance_id()
      || catalog.selected_namespace_root != view.root_metadata().hash
      || catalog.semantic_state_root != view.authority().semantic_state.object_id
      || catalog.publication_sequence != view.authority().admission.publication_sequence
      || !catalog.complete
      || prior_field.is_some_and(|prior| prior >= catalog.field_name.as_str())
    {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_partition_catalog_authority",
        "selected field catalogs are foreign, incomplete, duplicated, or out of order",
      ));
    }
    let mut prior_scope: Option<&[u8]> = None;
    for scope in &catalog.scopes {
      validate_identity(&scope.scope_id, view.hash_algorithm(), "catalog ScopeId")?;
      validate_identity(&scope.value_store_id, view.hash_algorithm(), "catalog ValueStoreId")?;
      if prior_scope.is_some_and(|prior| prior >= scope.scope_id.as_slice()) {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_partition_scope_catalog",
          "selected field catalog ScopeIds are not strict and unique",
        ));
      }
      prior_scope = Some(&scope.scope_id);
    }
    prior_field = Some(&catalog.field_name);
  }
  Ok(())
}

fn validate_open_request(
  inner: &NativeAuthoritativeFieldPartitionInnerV1,
  request: &QueryExecutionFieldPartitionOpenRequestV1<'_>,
) -> Result<(), QueryExecutionSourceErrorV1> {
  if request.selected_namespace_root != inner.view.root_metadata().hash
    || request.publication_sequence != inner.view.authority().admission.publication_sequence
    || request.query_path != inner.query_path
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_partition_request_authority",
      "field partition request does not bind the source root, publication, and query path",
    ));
  }
  if request.maximum_documents < inner.workspace.record_count()
    || request.maximum_values_per_document == 0
    || request.maximum_canonical_value_bytes_per_document == 0
    || request.maximum_path_bytes == 0
  {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_partition_request_limits",
      "field partition request cannot admit the complete workspace or one bounded field row",
    ));
  }
  Ok(())
}

fn unique_field_catalog(catalogs: &[RootAwareQueryFieldCatalogV1], field_name: &str) -> Result<usize, QueryExecutionSourceErrorV1> {
  let position = catalogs.partition_point(|catalog| catalog.field_name.as_str() < field_name);
  if catalogs.get(position).is_none_or(|catalog| catalog.field_name != field_name) {
    Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_partition_field_catalog",
      "requested query field is absent from the selected semantic catalog",
    ))
  } else {
    Ok(position)
  }
}

fn selected_source_limits() -> Result<NativeSelectedSourceLimitsV1, QueryExecutionSourceErrorV1> {
  // ValueStore compilation has a fixed protocol envelope independent of the
  // query's smaller returned-value ceiling. The cursor separately admits and
  // enforces that returned-value bound before retaining a row.
  match NativeSelectedSourceLimitsV1::new(PARTITION_SOURCE_MAXIMUM_RETAINED_BYTES) {
    Ok(limits) => Ok(limits),
    Err(error) => Err(map_native_error(error)),
  }
}

fn cursor_memory_bytes(
  request: &QueryExecutionFieldPartitionOpenRequestV1<'_>,
  algorithm: HashAlgorithm,
  scope_count: usize,
) -> Result<u64, QueryExecutionSourceErrorV1> {
  let value_slots = request.maximum_values_per_document.checked_mul(size_of::<Vec<u8>>() as u64).ok_or_else(|| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_cursor_memory", "value slots overflowed")
  })?;
  let scope_count = u64::try_from(scope_count).map_err(|error| {
    source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_partition_cursor_memory",
      format!("scope count exceeds u64: {error}"),
    )
  })?;
  let scope_bytes =
    scope_count.checked_mul((algorithm.hash_length() + size_of::<Vec<u8>>() + size_of::<u64>()) as u64).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_cursor_memory", "scope bytes overflowed")
    })?;
  PARTITION_CURSOR_FIXED_BYTES
    .checked_add(value_slots)
    .and_then(|bytes| bytes.checked_add(request.maximum_canonical_value_bytes_per_document))
    .and_then(|bytes| bytes.checked_add(request.maximum_path_bytes))
    .and_then(|bytes| bytes.checked_add((algorithm.hash_length() * 3) as u64))
    .and_then(|bytes| bytes.checked_add(scope_bytes))
    .ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_cursor_memory", "cursor bound overflowed")
    })
}

fn scope_resolver_bytes(scope_count: usize, algorithm: HashAlgorithm) -> Result<u64, QueryExecutionSourceErrorV1> {
  let per_scope = size_of::<EffectiveScopeCandidateV1<'_>>()
    .checked_add(size_of::<(usize, super::scope::ScopeDefinitionV1<'_>)>())
    .and_then(|bytes| bytes.checked_add(algorithm.hash_length()))
    .ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_scope_memory", "scope bytes overflowed")
    })?;
  let retained =
    scope_count.checked_mul(per_scope).and_then(|bytes| bytes.checked_add(PARTITION_CURSOR_FIXED_BYTES as usize)).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_scope_memory", "scope bound overflowed")
    })?;
  let retained = u64::try_from(retained).map_err(|error| {
    source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_partition_scope_memory",
      format!("scope bound exceeds u64: {error}"),
    )
  })?;
  if retained == 0 || retained > MAXIMUM_SCOPE_RESOLVER_BYTES {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_partition_scope_memory",
      "effective-scope resolver exceeds its fixed memory maximum",
    ));
  }
  Ok(retained)
}

fn validate_values(values: &[Vec<u8>], maximum_values: u64, maximum_bytes: u64) -> Result<(), QueryExecutionSourceErrorV1> {
  if values.len() as u64 > maximum_values {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_partition_value_count",
      "selected source returned too many canonical values",
    ));
  }
  let bytes = values.iter().try_fold(0u64, |total, value| total.checked_add(value.len() as u64)).ok_or_else(|| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_value_bytes", "canonical value bytes overflowed")
  })?;
  if bytes > maximum_bytes {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_partition_value_bytes",
      "selected source returned too many canonical value bytes",
    ));
  }
  Ok(())
}

fn validate_identity(identity: &[u8], algorithm: HashAlgorithm, role: &str) -> Result<(), QueryExecutionSourceErrorV1> {
  if identity.len() != algorithm.hash_length() || identity.iter().all(|byte| *byte == 0) {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "native_partition_identity",
      format!("{role} has the wrong width or is all zero"),
    ));
  }
  Ok(())
}

fn path_is_within(parent: &str, child: &str) -> bool {
  parent == "/" || parent == child || child.strip_prefix(parent).is_some_and(|suffix| suffix.starts_with('/'))
}

fn require_not_cancelled(cancellation: &CancellationToken) -> Result<(), QueryExecutionSourceErrorV1> {
  if cancellation.is_cancelled() {
    Err(source_error(QueryExecutionSourceErrorClassV1::Cancelled, "native_partition_cancelled", "native partition work was cancelled"))
  } else {
    Ok(())
  }
}

fn try_clone_bytes(bytes: &[u8], role: &str) -> Result<Vec<u8>, QueryExecutionSourceErrorV1> {
  let mut retained = Vec::new();
  retained.try_reserve_exact(bytes.len()).map_err(|error| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_allocation", format!("cannot retain {role}: {error}"))
  })?;
  retained.extend_from_slice(bytes);
  Ok(retained)
}

fn try_clone_string(value: &str, role: &str) -> Result<String, QueryExecutionSourceErrorV1> {
  String::from_utf8(try_clone_bytes(value.as_bytes(), role)?).map_err(|error| {
    source_error(QueryExecutionSourceErrorClassV1::Corrupt, "native_partition_string", format!("cannot retain {role}: {error}"))
  })
}

fn try_clone_nested_bytes(values: &[Vec<u8>], role: &str) -> Result<Vec<Vec<u8>>, QueryExecutionSourceErrorV1> {
  let mut retained = Vec::new();
  retained.try_reserve_exact(values.len()).map_err(|error| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_allocation", format!("cannot retain {role}: {error}"))
  })?;
  for value in values {
    retained.push(try_clone_bytes(value, role)?);
  }
  Ok(retained)
}

fn try_clone_u64s(values: &[u64], role: &str) -> Result<Vec<u64>, QueryExecutionSourceErrorV1> {
  let mut retained = Vec::new();
  retained.try_reserve_exact(values.len()).map_err(|error| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_allocation", format!("cannot retain {role}: {error}"))
  })?;
  retained.extend_from_slice(values);
  Ok(retained)
}

fn map_workspace_error(error: NativeQueryOrderingWorkspaceErrorV1) -> QueryExecutionSourceErrorV1 {
  let class = match error.class() {
    NativeQueryOrderingWorkspaceErrorClassV1::Invalid | NativeQueryOrderingWorkspaceErrorClassV1::Corrupt => {
      QueryExecutionSourceErrorClassV1::Corrupt
    }
    NativeQueryOrderingWorkspaceErrorClassV1::Resource => QueryExecutionSourceErrorClassV1::ResourceLimit,
    NativeQueryOrderingWorkspaceErrorClassV1::Unavailable => QueryExecutionSourceErrorClassV1::Unavailable,
    NativeQueryOrderingWorkspaceErrorClassV1::Cancelled => QueryExecutionSourceErrorClassV1::Cancelled,
  };
  source_error(class, error.code(), error.context())
}

fn map_native_error(error: NativeSelectedNamespaceReadErrorV1) -> QueryExecutionSourceErrorV1 {
  let class = match error.class() {
    NativeSelectedNamespaceReadErrorClassV1::InvalidRequest | NativeSelectedNamespaceReadErrorClassV1::Corrupt => {
      QueryExecutionSourceErrorClassV1::Corrupt
    }
    NativeSelectedNamespaceReadErrorClassV1::ResourceLimit => QueryExecutionSourceErrorClassV1::ResourceLimit,
    NativeSelectedNamespaceReadErrorClassV1::Unavailable => QueryExecutionSourceErrorClassV1::Unavailable,
    NativeSelectedNamespaceReadErrorClassV1::Cancelled => QueryExecutionSourceErrorClassV1::Cancelled,
  };
  source_error(class, error.code(), error.context())
}

fn source_error(class: QueryExecutionSourceErrorClassV1, code: &'static str, context: impl Into<String>) -> QueryExecutionSourceErrorV1 {
  QueryExecutionSourceErrorV1::new(class, code, context)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::engine::v4::index_artifact_cursor::ArtifactDirectoryRootSummaryV1;

  fn root(byte: u8) -> QueryCandidateArtifactRootV1 {
    QueryCandidateArtifactRootV1::new(
      vec![byte; 32],
      vec![0x22; 32],
      7,
      ArtifactDirectoryRootSummaryV1 {
        live_count: 1,
        tombstone_count: 0,
        page_count: 1,
        logical_bytes: 8,
        minimum_page_id: 0,
        maximum_page_id: 0,
      },
    )
    .unwrap()
  }

  #[test]
  fn candidate_scope_root_merge_accepts_exact_agreement_and_rejects_absence_or_identity_disagreement() {
    let mut absent = None;
    merge_candidate_scope_root(&mut absent, None).unwrap();
    merge_candidate_scope_root(&mut absent, None).unwrap();
    assert_eq!(absent, Some(None));

    let expected = root(0x31);
    let mut present = None;
    merge_candidate_scope_root(&mut present, Some(expected.clone())).unwrap();
    merge_candidate_scope_root(&mut present, Some(expected)).unwrap();
    let error = merge_candidate_scope_root(&mut present, Some(root(0x32))).unwrap_err();
    assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_candidate_scope_root_disagreement");

    let error = merge_candidate_scope_root(&mut absent, Some(root(0x33))).unwrap_err();
    assert_eq!(error.class(), QueryExecutionSourceErrorClassV1::Corrupt);
    assert_eq!(error.code(), "native_candidate_scope_root_disagreement");
  }
}
