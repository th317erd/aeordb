//! Native selected-root source for partitioned authoritative query truth.

use std::cmp::Ordering;
use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};

use super::config_value::{CanonicalConfigValueV1, CanonicalValueBounds, decode_canonical_value};
use super::index_artifact_cursor::{
  ArtifactCursorReadErrorV1, ArtifactCursorSourceV1, ArtifactPageCursorErrorV1, ArtifactPageCursorRequestV1, ArtifactPageCursorRootV1,
  ArtifactPageNeighborModeV1, ArtifactPageSeekV1, RetainedArtifactBytesV1, load_artifact_page_cursor_v1,
};
use super::index_definition_runtime::{IndexDefinitionErrorClassV1, IndexDefinitionRuntimeV1};
use super::index_page::{OrderedIndexRoleV1, decode_ordered_page};
use super::index_partial_acceleration::{
  IndexChangedDocumentScanReceiptV1, IndexChangedDocumentScanRequestV1, IndexChangedDocumentSourceV1, IndexChangedDocumentV1,
  IndexChangedDocumentVisitorV1, IndexPartialAccelerationLimitsV1, IndexPartialCandidateRecheckerV1, IndexPartialRecheckOutcomeV1,
  IndexPartialRecheckRequestV1, IndexPartialScanErrorV1, IndexPartialSourceErrorV1,
};
use super::index_record::{ScopeDocumentRecordV1, decode_scope_document_record, decode_scope_reverse_record};
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
use super::query_candidate_composition::QueryCandidateCompositionLimitsV1;
use super::query_complete_candidate::{
  QueryCandidateArtifactRootV1, QueryCandidateRecheckReceiptV1, QueryCandidateRecheckRequestV1, QueryCompleteCandidateErrorClassV1,
  QueryCompleteCandidateErrorV1, QueryCompleteCandidateLimitsV1, QueryCompleteCandidateSourceV1, QueryCompletePostingRootReceiptV1,
  QueryCompletePostingRootRequestV1, QueryCompleteScopeRootReceiptV1, QueryCompleteScopeRootRequestV1,
};
use super::query_executor::{
  QueryAuthoritativeDocumentVisitorV1, QueryAuthoritativeFieldPartitionCursorV1, QueryAuthoritativeFieldPartitionSourceV1,
  QueryAuthoritativeFieldSourceV1, QueryAuthoritativeScopeSourceV1, QueryAuthoritativeValueVisitorV1, QueryExecutionDocumentV1,
  QueryExecutionErrorClassV1, QueryExecutionErrorV1, QueryExecutionFieldDocumentV1, QueryExecutionFieldPartitionOpenRequestV1,
  QueryExecutionFieldPartitionReceiptV1, QueryExecutionFieldReadReceiptV1, QueryExecutionFieldReadRequestV1, QueryExecutionFieldStateV1,
  QueryExecutionLimitsV1, QueryExecutionMatchSinkV1, QueryExecutionScanErrorV1, QueryExecutionScopeScanReceiptV1,
  QueryExecutionScopeScanRequestV1, QueryExecutionSourceErrorClassV1, QueryExecutionSourceErrorV1, QueryExecutionStreamReceiptV1,
  RootAwarePartitionedQueryExecutionRequestV1, RootAwareQueryDocumentEvaluationRequestV1, RootAwareQueryExecutionV1,
  RootAwareQueryScopeExecutionRequestV1, evaluate_authoritative_query_document_v1, execute_authoritative_partitioned_query_into_v1,
  execute_authoritative_partitioned_query_v1, execute_authoritative_scope_query_v1, map_source_error,
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
  CompiledQueryCoverageV1, CompiledQueryIndexCandidateV1, CompiledRootAwareQueryPlanV1, QueryLogicalExplainFieldV1, QueryLogicalExplainV1,
  QueryPlanningCoverageGenerationV1, QueryPlanningErrorClassV1, QueryPlanningErrorV1, RootAwareQueryFieldCatalogV1,
  authorization_safe_query_explain_v1,
};
use super::query_scope_execution::{
  QueryExactScopeExecutionErrorV1, QueryExactScopeExecutionRequestV1, QueryExactScopeExecutionV1, QueryExactScopeStreamExecutionV1,
  execute_exact_query_scope_into_v1, execute_exact_query_scope_v1,
};
use super::read_view::ResolvedReadViewV1;
use super::read_view_authorization::ResolvedPathAuthorizationV1;
use super::read_view_native::{
  NativeReadViewSourceV1, NativeSelectedArtifactRootRequestV1, NativeSelectedNamespaceLimitsV1, NativeSelectedNamespaceReadErrorClassV1,
  NativeSelectedNamespaceReadErrorV1, NativeSelectedNamespaceReaderV1, NativeSelectedSemanticCatalogV1, NativePreparedSelectedSourceV1,
  NativeSelectedSourceEvaluationV1, NativeSelectedSourceLimitsV1, NativeSelectedSourceOutcomeV1, NativeSelectedSourceParserV1,
};
use super::scope::{EffectiveScopeCandidateV1, EffectiveScopeResolverV1, is_internal_index_path_v1, validate_canonical_absolute_path};

const MAXIMUM_PARTITION_SCOPES: usize = 1_024;
const MAXIMUM_SCOPE_RESOLVER_BYTES: u64 = 64 * 1024 * 1024;
const PARTITION_CURSOR_FIXED_BYTES: u64 = 16 * 1024;
const PARTITION_SOURCE_MAXIMUM_RETAINED_BYTES: u64 = 256 * 1024 * 1024;
const AUXILIARY_SOURCE_FIXED_BYTES: u64 = 16 * 1024;
const AUXILIARY_ALLOCATION_OVERHEAD_BYTES: usize = 16;
const LOGICAL_EXPLAIN_FIXED_BYTES: u64 = 4 * 1024;
const DEFAULT_PREPARED_SOURCE_CACHE_ENTRIES: usize = 16;
const MAXIMUM_PREPARED_SOURCE_CACHE_ENTRIES: usize = 1_024;
const PREPARED_SOURCE_CACHE_ALLOCATION_OVERHEAD_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeAuthoritativeAuxiliaryLimitsV1 {
  maximum_fields: usize,
  maximum_scope_bindings: usize,
  maximum_binding_bytes: u64,
  maximum_path_bytes: u64,
  maximum_prepared_source_cache_entries: usize,
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
    Ok(Self {
      maximum_fields,
      maximum_scope_bindings,
      maximum_binding_bytes,
      maximum_path_bytes,
      maximum_prepared_source_cache_entries: DEFAULT_PREPARED_SOURCE_CACHE_ENTRIES,
    })
  }

  pub fn with_prepared_source_cache_entries(
    mut self,
    maximum_prepared_source_cache_entries: usize,
  ) -> Result<Self, QueryExecutionSourceErrorV1> {
    validate_prepared_source_cache_entries(maximum_prepared_source_cache_entries)?;
    self.maximum_prepared_source_cache_entries = maximum_prepared_source_cache_entries;
    Ok(self)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeAuthoritativeFieldPartitionLimitsV1 {
  namespace: NativeSelectedNamespaceLimitsV1,
  ordering: NativeQueryOrderingWorkspaceLimitsV1,
  maximum_prepared_source_cache_entries: usize,
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
    Ok(Self { namespace, ordering, maximum_prepared_source_cache_entries: DEFAULT_PREPARED_SOURCE_CACHE_ENTRIES })
  }

  pub fn with_prepared_source_cache_entries(
    mut self,
    maximum_prepared_source_cache_entries: usize,
  ) -> Result<Self, QueryExecutionSourceErrorV1> {
    validate_prepared_source_cache_entries(maximum_prepared_source_cache_entries)?;
    self.maximum_prepared_source_cache_entries = maximum_prepared_source_cache_entries;
    Ok(self)
  }
}

fn validate_prepared_source_cache_entries(maximum_entries: usize) -> Result<(), QueryExecutionSourceErrorV1> {
  if maximum_entries == 0 || maximum_entries > MAXIMUM_PREPARED_SOURCE_CACHE_ENTRIES {
    return Err(source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_prepared_cache_entries",
      "prepared source cache entry bound must be nonzero and remain within the protocol maximum",
    ));
  }
  Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeLogicalExplainLimitsV1 {
  maximum_retained_bytes: u64,
}

impl NativeLogicalExplainLimitsV1 {
  pub fn new(maximum_retained_bytes: u64) -> Result<Self, QueryExecutionSourceErrorV1> {
    if maximum_retained_bytes == 0 {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_query_explain_limits",
        "native logical EXPLAIN retained-byte limit must be nonzero",
      ));
    }
    Ok(Self { maximum_retained_bytes })
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativePreparedSourceCacheMetricsV1 {
  preparations: u64,
  hits: u64,
  misses: u64,
  evictions: u64,
}

impl NativePreparedSourceCacheMetricsV1 {
  pub const fn preparations(self) -> u64 {
    self.preparations
  }

  pub const fn hits(self) -> u64 {
    self.hits
  }

  pub const fn misses(self) -> u64 {
    self.misses
  }

  pub const fn evictions(self) -> u64 {
    self.evictions
  }
}

pub struct NativeAuthorizedLogicalExplainV1 {
  logical: QueryLogicalExplainV1,
  _memory: MemoryReservation,
}

impl NativeAuthorizedLogicalExplainV1 {
  pub const fn logical(&self) -> &QueryLogicalExplainV1 {
    &self.logical
  }
}

#[derive(Default)]
struct NativePreparedSourceCacheCountersV1 {
  preparations: AtomicU64,
  hits: AtomicU64,
  misses: AtomicU64,
  evictions: AtomicU64,
}

impl NativePreparedSourceCacheCountersV1 {
  fn snapshot(&self) -> NativePreparedSourceCacheMetricsV1 {
    NativePreparedSourceCacheMetricsV1 {
      preparations: self.preparations.load(AtomicOrdering::Relaxed),
      hits: self.hits.load(AtomicOrdering::Relaxed),
      misses: self.misses.load(AtomicOrdering::Relaxed),
      evictions: self.evictions.load(AtomicOrdering::Relaxed),
    }
  }
}

struct NativePreparedSourceCacheEntryV1<'definition> {
  catalog_index: usize,
  source_limits: NativeSelectedSourceLimitsV1,
  prepared: NativePreparedSelectedSourceV1<'definition>,
  last_used: u64,
}

struct NativePreparedSourceCacheV1<'definition> {
  inner: &'definition NativeAuthoritativeFieldPartitionInnerV1,
  entries: Vec<NativePreparedSourceCacheEntryV1<'definition>>,
  maximum_entries: usize,
  clock: u64,
  _memory: MemoryReservation,
}

impl<'definition> NativePreparedSourceCacheV1<'definition> {
  fn new(
    inner: &'definition NativeAuthoritativeFieldPartitionInnerV1,
    maximum_entries: usize,
  ) -> Result<Self, QueryExecutionSourceErrorV1> {
    validate_prepared_source_cache_entries(maximum_entries)?;
    let slot_bytes = maximum_entries
      .checked_mul(size_of::<NativePreparedSourceCacheEntryV1<'_>>() + PREPARED_SOURCE_CACHE_ALLOCATION_OVERHEAD_BYTES)
      .ok_or_else(|| {
        source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_prepared_cache_memory", "prepared cache bytes overflowed")
      })?;
    let slot_bytes = u64::try_from(slot_bytes).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_prepared_cache_memory",
        format!("prepared cache bytes do not fit the retained-memory counter: {error}"),
      )
    })?;
    let memory =
      inner.source.memory_coordinator().reserve(MemoryOwner::Query, slot_bytes.max(1), AdmissionClass::Workload).map_err(|error| {
        source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_prepared_cache_memory", error.to_string())
      })?;
    let mut entries = Vec::new();
    entries.try_reserve_exact(maximum_entries).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_prepared_cache_allocation",
        format!("cannot allocate bounded prepared cache: {error}"),
      )
    })?;
    let retained_slot_bytes = entries.capacity().checked_mul(size_of::<NativePreparedSourceCacheEntryV1<'_>>()).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_prepared_cache_memory", "prepared cache capacity overflowed")
    })?;
    let retained_slot_bytes = u64::try_from(retained_slot_bytes).map_err(|error| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_prepared_cache_memory",
        format!("prepared cache capacity does not fit the retained-memory counter: {error}"),
      )
    })?;
    if retained_slot_bytes > memory.bytes() {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_prepared_cache_memory",
        "prepared cache allocation exceeds its admitted slot bound",
      ));
    }
    Ok(Self { inner, entries, maximum_entries, clock: 0, _memory: memory })
  }

  fn evaluate(
    &mut self,
    reader: &NativeSelectedNamespaceReaderV1<'_>,
    catalog_index: usize,
    row: &super::read_view_native::NativeSelectedNamespaceFileRowV1,
    scope_id: &[u8],
    source_limits: NativeSelectedSourceLimitsV1,
  ) -> Result<NativeSelectedSourceEvaluationV1, QueryExecutionSourceErrorV1> {
    self.clock = self.clock.checked_add(1).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_prepared_cache_clock", "prepared cache clock overflowed")
    })?;
    if let Some(index) = self.entries.iter().position(|entry| {
      entry.catalog_index == catalog_index && entry.source_limits == source_limits && entry.prepared.scope_id() == scope_id
    }) {
      self.entries[index].last_used = self.clock;
      self.inner.prepared_source_cache_counters.hits.fetch_add(1, AtomicOrdering::Relaxed);
      return self.entries[index].prepared.evaluate(reader, row, NativeSelectedSourceParserV1::Native, None).map_err(map_native_error);
    }

    self.inner.prepared_source_cache_counters.misses.fetch_add(1, AtomicOrdering::Relaxed);
    if self.entries.len() == self.maximum_entries {
      let lru = self
        .entries
        .iter()
        .enumerate()
        .min_by_key(|(_, entry)| entry.last_used)
        .map(|(index, _)| index)
        .ok_or_else(|| source_error(QueryExecutionSourceErrorClassV1::Internal, "native_prepared_cache_state", "full cache is empty"))?;
      self.entries.swap_remove(lru);
      self.inner.prepared_source_cache_counters.evictions.fetch_add(1, AtomicOrdering::Relaxed);
    }
    let catalog = self.inner.semantic_catalog.catalogs().get(catalog_index).ok_or_else(|| {
      source_error(
        QueryExecutionSourceErrorClassV1::Internal,
        "native_prepared_cache_catalog",
        "prepared cache catalog index is outside selected semantic authority",
      )
    })?;
    let prepared = reader.prepare_authoritative_source_runtime(catalog, scope_id, source_limits).map_err(map_native_error)?;
    self.entries.push(NativePreparedSourceCacheEntryV1 { catalog_index, source_limits, prepared, last_used: self.clock });
    self.inner.prepared_source_cache_counters.preparations.fetch_add(1, AtomicOrdering::Relaxed);
    self
      .entries
      .last()
      .ok_or_else(|| source_error(QueryExecutionSourceErrorClassV1::Internal, "native_prepared_cache_state", "prepared entry disappeared"))?
      .prepared
      .evaluate(reader, row, NativeSelectedSourceParserV1::Native, None)
      .map_err(map_native_error)
  }
}

struct NativeAuthoritativeFieldPartitionInnerV1 {
  source: NativeReadViewSourceV1,
  view: ResolvedReadViewV1<ResolvedPathAuthorizationV1>,
  semantic_catalog: NativeSelectedSemanticCatalogV1,
  workspace: NativeQueryOrderingWorkspaceV1,
  query_path: String,
  namespace_limits: NativeSelectedNamespaceLimitsV1,
  maximum_prepared_source_cache_entries: usize,
  prepared_source_cache_counters: NativePreparedSourceCacheCountersV1,
}

pub struct NativeAuthoritativeFieldPartitionSourceV1 {
  inner: Arc<NativeAuthoritativeFieldPartitionInnerV1>,
}

pub struct NativeQueryCandidateArtifactSourceV1<'source> {
  inner: &'source NativeAuthoritativeFieldPartitionInnerV1,
  lookup: Option<NativeQueryOrderingLookupV1>,
  prepared_sources: Option<NativePreparedSourceCacheV1<'source>>,
}

pub struct NativeQueryPartialComplementSourceV1<'source> {
  inner: &'source NativeAuthoritativeFieldPartitionInnerV1,
  artifacts: NativeQueryCandidateArtifactSourceV1<'source>,
  scope_id: [u8; 64],
  scope_id_length: usize,
  limits: QueryCompleteCandidateLimitsV1,
}

pub struct NativeQueryPartialRecheckerV1<'source, 'plan> {
  inner: &'source NativeAuthoritativeFieldPartitionInnerV1,
  plan: &'plan CompiledRootAwareQueryPlanV1,
  scope_id: [u8; 64],
  scope_id_length: usize,
  lookup: Option<NativeQueryOrderingLookupV1>,
  limits: QueryExecutionLimitsV1,
  prepared_sources: Option<NativePreparedSourceCacheV1<'source>>,
}

struct NativeAuthoritativeRowFieldSourceV1<'source, 'cache, 'row, 'scope> {
  inner: &'source NativeAuthoritativeFieldPartitionInnerV1,
  prepared_sources: &'cache mut NativePreparedSourceCacheV1<'source>,
  row: &'row super::read_view_native::NativeSelectedNamespaceFileRowV1,
  effective_scope_id: &'scope [u8],
  source_limits: NativeSelectedSourceLimitsV1,
}

struct NativeAuthoritativeFieldEvaluationV1 {
  state: QueryExecutionFieldStateV1,
  values: Vec<Vec<u8>>,
}

#[derive(Debug)]
struct NativeAuxiliaryScopeBindingV1<'definition> {
  scope_id: Vec<u8>,
  catalog_scope_index: usize,
  runtime: IndexDefinitionRuntimeV1<'definition, 'definition>,
}

#[derive(Debug)]
struct NativeAuxiliaryFieldBindingV1<'definition> {
  field_name: String,
  comparator: super::position::PositionComparatorV1,
  comparison_semantics: u16,
  collation_semantics: u16,
  behavior_fingerprint: [u8; 32],
  catalog_index: usize,
  scopes: Vec<NativeAuxiliaryScopeBindingV1<'definition>>,
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

pub struct NativeAuthoritativeAuxiliarySourceV1<'source, 'plan> {
  inner: &'source NativeAuthoritativeFieldPartitionInnerV1,
  plan: &'plan CompiledRootAwareQueryPlanV1,
  lookup: NativeQueryOrderingLookupV1,
  fields: Vec<NativeAuxiliaryFieldBindingV1<'source>>,
  maximum_path_bytes: u64,
  prepared_sources: NativePreparedSourceCacheV1<'source>,
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
        maximum_prepared_source_cache_entries: limits.maximum_prepared_source_cache_entries,
        prepared_source_cache_counters: NativePreparedSourceCacheCountersV1::default(),
      }),
    })
  }

  pub fn document_count(&self) -> u64 {
    self.inner.workspace.record_count()
  }

  pub fn workspace_bytes(&self) -> u64 {
    self.inner.workspace.workspace_bytes()
  }

  pub fn prepared_source_cache_metrics(&self) -> NativePreparedSourceCacheMetricsV1 {
    self.inner.prepared_source_cache_counters.snapshot()
  }

  pub(super) fn query_hit_hash_algorithm_v1(&self) -> HashAlgorithm {
    self.inner.view.hash_algorithm()
  }

  pub(super) fn query_hit_selected_namespace_root_v1(&self) -> &[u8] {
    &self.inner.view.root_metadata().hash
  }

  pub(super) fn query_hit_query_path_v1(&self) -> &str {
    &self.inner.query_path
  }

  pub(super) fn query_hit_view_cancellation_v1(&self) -> &CancellationToken {
    self.inner.view.cancellation()
  }

  pub(super) fn query_hit_memory_coordinator_v1(&self) -> &Arc<MemoryCoordinator> {
    self.inner.source.memory_coordinator()
  }

  pub(super) fn open_query_hit_namespace_reader_v1(&self) -> Result<NativeSelectedNamespaceReaderV1<'_>, QueryExecutionSourceErrorV1> {
    self.inner.source.selected_namespace_reader(&self.inner.view, self.inner.namespace_limits).map_err(map_native_error)
  }

  pub(super) fn open_query_hit_ordering_lookup_v1(&self) -> Result<NativeQueryOrderingLookupV1, QueryExecutionSourceErrorV1> {
    self.inner.workspace.open_lookup().map_err(map_workspace_error)
  }

  pub fn logical_explain_v1(
    &self,
    plan: &CompiledRootAwareQueryPlanV1,
    cancellation: &CancellationToken,
    limits: NativeLogicalExplainLimitsV1,
  ) -> Result<NativeAuthorizedLogicalExplainV1, QueryExecutionSourceErrorV1> {
    require_not_cancelled(cancellation)?;
    require_not_cancelled(self.inner.view.cancellation())?;
    validate_native_plan_authority(
      self.inner.as_ref(),
      plan,
      "native_query_explain_plan_authority",
      "compiled query plan does not bind the authorized native selected-root EXPLAIN source",
    )?;
    let structural_bytes = logical_explain_structural_bytes(plan)?;
    if structural_bytes > limits.maximum_retained_bytes {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_query_explain_retained_bytes",
        "logical EXPLAIN cannot fit within its retained-byte limit",
      ));
    }
    let mut memory = self
      .inner
      .source
      .memory_coordinator()
      .reserve(MemoryOwner::Query, limits.maximum_retained_bytes, AdmissionClass::Workload)
      .map_err(|error| source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_query_explain_memory", error.to_string()))?;
    let logical = authorization_safe_query_explain_v1(plan).map_err(map_query_planning_error)?;
    let retained_bytes = logical_explain_retained_bytes(&logical)?;
    let release = memory.bytes().checked_sub(retained_bytes).ok_or_else(|| {
      source_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "native_query_explain_retained_bytes",
        "logical EXPLAIN retained more bytes than its admitted limit",
      )
    })?;
    memory.shrink(release).map_err(|error| {
      source_error(QueryExecutionSourceErrorClassV1::Internal, "native_query_explain_memory_accounting", error.to_string())
    })?;
    require_not_cancelled(cancellation)?;
    require_not_cancelled(self.inner.view.cancellation())?;
    Ok(NativeAuthorizedLogicalExplainV1 { logical, _memory: memory })
  }

  pub fn execute_authoritative_query_v1(
    &self,
    plan: &CompiledRootAwareQueryPlanV1,
    cancellation: &CancellationToken,
    limits: QueryExecutionLimitsV1,
  ) -> Result<RootAwareQueryExecutionV1, QueryExecutionErrorV1> {
    let mut authoritative = Self { inner: Arc::clone(&self.inner) };
    execute_authoritative_partitioned_query_v1(RootAwarePartitionedQueryExecutionRequestV1 {
      plan,
      catalogs: self.inner.semantic_catalog.catalogs(),
      source: &mut authoritative,
      memory: self.inner.source.memory_coordinator().as_ref(),
      cancellation,
      limits,
    })
  }

  pub fn execute_authoritative_query_into_v1(
    &self,
    plan: &CompiledRootAwareQueryPlanV1,
    cancellation: &CancellationToken,
    limits: QueryExecutionLimitsV1,
    sink: &mut dyn QueryExecutionMatchSinkV1,
  ) -> Result<QueryExecutionStreamReceiptV1, QueryExecutionErrorV1> {
    let mut authoritative = Self { inner: Arc::clone(&self.inner) };
    execute_authoritative_partitioned_query_into_v1(
      RootAwarePartitionedQueryExecutionRequestV1 {
        plan,
        catalogs: self.inner.semantic_catalog.catalogs(),
        source: &mut authoritative,
        memory: self.inner.source.memory_coordinator().as_ref(),
        cancellation,
        limits,
      },
      sink,
    )
  }

  pub fn execute_authoritative_scope_query_v1(
    &self,
    plan: &CompiledRootAwareQueryPlanV1,
    scope_id: &[u8],
    cancellation: &CancellationToken,
    limits: QueryExecutionLimitsV1,
  ) -> Result<RootAwareQueryExecutionV1, QueryExecutionErrorV1> {
    let mut authoritative = Self { inner: Arc::clone(&self.inner) };
    execute_authoritative_scope_query_v1(RootAwareQueryScopeExecutionRequestV1 {
      plan,
      catalogs: self.inner.semantic_catalog.catalogs(),
      scope_id,
      source: &mut authoritative,
      memory: self.inner.source.memory_coordinator().as_ref(),
      cancellation,
      limits,
    })
  }

  #[allow(clippy::too_many_arguments)]
  pub fn execute_exact_scope_query_v1(
    &self,
    plan: &CompiledRootAwareQueryPlanV1,
    scope_id: &[u8],
    cancellation: &CancellationToken,
    execution_limits: QueryExecutionLimitsV1,
    candidate_limits: QueryCompleteCandidateLimitsV1,
    acceleration_limits: IndexPartialAccelerationLimitsV1,
    composition_limits: QueryCandidateCompositionLimitsV1,
  ) -> Result<QueryExactScopeExecutionV1, QueryExactScopeExecutionErrorV1> {
    let mut authoritative = Self { inner: Arc::clone(&self.inner) };
    let mut complete = self.open_candidate_artifact_source();
    let mut partial = self.open_candidate_artifact_source();
    let mut complement = self.open_partial_complement_source(scope_id, candidate_limits).map_err(map_native_exact_scope_setup_error)?;
    let mut rechecker = self.open_partial_rechecker(plan, scope_id, execution_limits).map_err(map_native_exact_scope_setup_error)?;
    execute_exact_query_scope_v1(QueryExactScopeExecutionRequestV1 {
      plan,
      catalogs: self.inner.semantic_catalog.catalogs(),
      scope_id,
      authoritative_source: &mut authoritative,
      complete_source: Some(&mut complete),
      partial_source: Some(&mut partial),
      complement: Some(&mut complement),
      rechecker: Some(&mut rechecker),
      memory: self.inner.source.memory_coordinator().as_ref(),
      cancellation,
      execution_limits,
      candidate_limits,
      acceleration_limits,
      composition_limits,
    })
  }

  #[allow(clippy::too_many_arguments)]
  pub fn execute_exact_scope_query_into_v1(
    &self,
    plan: &CompiledRootAwareQueryPlanV1,
    scope_id: &[u8],
    cancellation: &CancellationToken,
    execution_limits: QueryExecutionLimitsV1,
    candidate_limits: QueryCompleteCandidateLimitsV1,
    acceleration_limits: IndexPartialAccelerationLimitsV1,
    composition_limits: QueryCandidateCompositionLimitsV1,
    sink: &mut dyn QueryExecutionMatchSinkV1,
  ) -> Result<QueryExactScopeStreamExecutionV1, QueryExactScopeExecutionErrorV1> {
    let mut authoritative = Self { inner: Arc::clone(&self.inner) };
    let mut complete = self.open_candidate_artifact_source();
    let mut partial = self.open_candidate_artifact_source();
    let mut complement = self.open_partial_complement_source(scope_id, candidate_limits).map_err(map_native_exact_scope_setup_error)?;
    let mut rechecker = self.open_partial_rechecker(plan, scope_id, execution_limits).map_err(map_native_exact_scope_setup_error)?;
    execute_exact_query_scope_into_v1(
      QueryExactScopeExecutionRequestV1 {
        plan,
        catalogs: self.inner.semantic_catalog.catalogs(),
        scope_id,
        authoritative_source: &mut authoritative,
        complete_source: Some(&mut complete),
        partial_source: Some(&mut partial),
        complement: Some(&mut complement),
        rechecker: Some(&mut rechecker),
        memory: self.inner.source.memory_coordinator().as_ref(),
        cancellation,
        execution_limits,
        candidate_limits,
        acceleration_limits,
        composition_limits,
      },
      sink,
    )
  }

  pub fn open_auxiliary_source<'source, 'plan>(
    &'source self,
    plan: &'plan CompiledRootAwareQueryPlanV1,
    limits: NativeAuthoritativeAuxiliaryLimitsV1,
  ) -> Result<NativeAuthoritativeAuxiliarySourceV1<'source, 'plan>, QueryExecutionSourceErrorV1> {
    validate_auxiliary_plan(self.inner.as_ref(), plan)?;
    let structural_binding_bytes = auxiliary_binding_bytes(plan, limits)?;
    let mut memory = self
      .inner
      .source
      .memory_coordinator()
      .reserve(MemoryOwner::Query, limits.maximum_binding_bytes, AdmissionClass::Workload)
      .map_err(|error| source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_memory", error.to_string()))?;
    let (fields, retained_binding_bytes) = compile_auxiliary_bindings(self.inner.as_ref(), plan, structural_binding_bytes, limits)?;
    let release = memory.bytes().checked_sub(retained_binding_bytes).ok_or_else(|| {
      source_error(
        QueryExecutionSourceErrorClassV1::Internal,
        "native_auxiliary_memory_accounting",
        "compiled auxiliary bindings exceeded their admitted reservation",
      )
    })?;
    memory
      .shrink(release)
      .map_err(|error| source_error(QueryExecutionSourceErrorClassV1::Internal, "native_auxiliary_memory_accounting", error.to_string()))?;
    let lookup = self.inner.workspace.open_lookup().map_err(map_workspace_error)?;
    let prepared_sources = NativePreparedSourceCacheV1::new(self.inner.as_ref(), limits.maximum_prepared_source_cache_entries)?;
    Ok(NativeAuthoritativeAuxiliarySourceV1 {
      inner: self.inner.as_ref(),
      plan,
      lookup,
      fields,
      maximum_path_bytes: limits.maximum_path_bytes,
      prepared_sources,
      _memory: memory,
    })
  }

  pub fn open_candidate_artifact_source(&self) -> NativeQueryCandidateArtifactSourceV1<'_> {
    NativeQueryCandidateArtifactSourceV1 { inner: self.inner.as_ref(), lookup: None, prepared_sources: None }
  }

  pub fn open_partial_complement_source<'source>(
    &'source self,
    scope_id: &[u8],
    limits: QueryCompleteCandidateLimitsV1,
  ) -> Result<NativeQueryPartialComplementSourceV1<'source>, QueryExecutionSourceErrorV1> {
    let (scope_id, scope_id_length) = retain_fixed_identity(scope_id, self.inner.view.hash_algorithm(), "partial complement ScopeId")?;
    Ok(NativeQueryPartialComplementSourceV1 {
      inner: self.inner.as_ref(),
      artifacts: self.open_candidate_artifact_source(),
      scope_id,
      scope_id_length,
      limits,
    })
  }

  pub fn open_partial_rechecker<'source, 'plan>(
    &'source self,
    plan: &'plan CompiledRootAwareQueryPlanV1,
    scope_id: &[u8],
    limits: QueryExecutionLimitsV1,
  ) -> Result<NativeQueryPartialRecheckerV1<'source, 'plan>, QueryExecutionSourceErrorV1> {
    validate_auxiliary_plan(self.inner.as_ref(), plan)?;
    let (scope_id, scope_id_length) = retain_fixed_identity(scope_id, self.inner.view.hash_algorithm(), "partial recheck ScopeId")?;
    Ok(NativeQueryPartialRecheckerV1 {
      inner: self.inner.as_ref(),
      plan,
      scope_id,
      scope_id_length,
      lookup: None,
      limits,
      prepared_sources: None,
    })
  }
}

impl ArtifactCursorSourceV1 for NativeQueryCandidateArtifactSourceV1<'_> {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<RetainedArtifactBytesV1, ArtifactCursorReadErrorV1> {
    let reader =
      self.inner.source.selected_namespace_reader(&self.inner.view, self.inner.namespace_limits).map_err(map_native_artifact_error)?;
    reader.read_index_artifact_bytes(key, maximum_bytes)
  }
}

impl NativeQueryCandidateArtifactSourceV1<'_> {
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
    let root = self.resolve_scope_root(request.scope_id, request.selected_namespace_root, None, OrderedIndexRoleV1::ScopeOrdinal)?;
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
    role: OrderedIndexRoleV1,
  ) -> Result<Option<QueryCandidateArtifactRootV1>, QueryExecutionSourceErrorV1> {
    if !matches!(role, OrderedIndexRoleV1::ScopeOrdinal | OrderedIndexRoleV1::ScopeReverse) {
      return Err(source_error(
        QueryExecutionSourceErrorClassV1::Internal,
        "native_candidate_scope_role",
        "dependent scope-root resolution requires ScopeOrdinal or ScopeReverse",
      ));
    }
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
        let root = self.load_root(catalog, scope_id, generation, role)?;
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
        "matched scope generations produced no dependent root resolution outcome",
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
        "matching planner-selected generations disagree on their dependent scope root",
      ));
    }
  }
  Ok(())
}

impl QueryPartialCandidateArtifactSourceV1 for NativeQueryCandidateArtifactSourceV1<'_> {
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
      .resolve_scope_root(
        request.scope_id,
        request.source_namespace_root,
        Some(request.source_publication_sequence),
        OrderedIndexRoleV1::ScopeOrdinal,
      )
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

impl QueryCompleteCandidateSourceV1 for NativeQueryCandidateArtifactSourceV1<'_> {
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
    if self.prepared_sources.is_none() {
      self.prepared_sources = Some(
        NativePreparedSourceCacheV1::new(self.inner, self.inner.maximum_prepared_source_cache_entries)
          .map_err(QueryExecutionScanErrorV1::Source)?,
      );
    }
    let prepared_sources = self.prepared_sources.as_mut().ok_or_else(|| {
      QueryExecutionScanErrorV1::Source(source_error(
        QueryExecutionSourceErrorClassV1::Internal,
        "native_candidate_prepared_cache_state",
        "complete candidate prepared cache was not retained after initialization",
      ))
    })?;
    let mut fields = NativeAuthoritativeRowFieldSourceV1 {
      inner: self.inner,
      prepared_sources,
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

#[derive(Clone, Copy)]
struct NativePartialTargetIdentityV1 {
  file_key: [u8; 64],
  record_revision: [u8; 64],
  hash_length: usize,
}

impl NativePartialTargetIdentityV1 {
  fn file_key(&self) -> &[u8] {
    &self.file_key[..self.hash_length]
  }

  fn record_revision(&self) -> &[u8] {
    &self.record_revision[..self.hash_length]
  }
}

struct NativePartialHistoricalIdentityV1 {
  file_key: [u8; 64],
  record_revision: [u8; 64],
  hash_length: usize,
  within_query_path: bool,
}

impl NativePartialHistoricalIdentityV1 {
  fn file_key(&self) -> &[u8] {
    &self.file_key[..self.hash_length]
  }

  fn record_revision(&self) -> &[u8] {
    &self.record_revision[..self.hash_length]
  }
}

struct NativePartialComplementBudgetV1<'a> {
  limits: QueryCompleteCandidateLimitsV1,
  request_cancellation: &'a CancellationToken,
  view_cancellation: &'a CancellationToken,
  work_steps: u64,
  page_seeks: u64,
  historical_documents: u64,
  identity_bytes: u64,
}

impl NativePartialComplementBudgetV1<'_> {
  fn require_not_cancelled(&self) -> Result<(), IndexPartialSourceErrorV1> {
    if self.request_cancellation.is_cancelled() || self.view_cancellation.is_cancelled() {
      Err(IndexPartialSourceErrorV1::cancelled("native_partial_complement_cancelled", "native changed-document complement was cancelled"))
    } else {
      Ok(())
    }
  }

  fn charge_work(&mut self, amount: u64) -> Result<(), IndexPartialSourceErrorV1> {
    self.require_not_cancelled()?;
    self.work_steps = self
      .work_steps
      .checked_add(amount)
      .ok_or_else(|| IndexPartialSourceErrorV1::resource_limit("native_partial_complement_work", "complement work counter overflowed"))?;
    if self.work_steps > self.limits.maximum_work_steps() {
      return Err(IndexPartialSourceErrorV1::resource_limit(
        "native_partial_complement_work",
        "changed-document complement exceeded its admitted work-step bound",
      ));
    }
    Ok(())
  }

  fn charge_page(&mut self) -> Result<(), IndexPartialSourceErrorV1> {
    self.charge_work(1)?;
    self.page_seeks = self.page_seeks.checked_add(1).ok_or_else(|| {
      IndexPartialSourceErrorV1::resource_limit("native_partial_complement_pages", "complement page-seek counter overflowed")
    })?;
    if self.page_seeks > self.limits.maximum_page_seeks() {
      return Err(IndexPartialSourceErrorV1::resource_limit(
        "native_partial_complement_pages",
        "changed-document complement exceeded its admitted page-seek bound",
      ));
    }
    Ok(())
  }

  fn charge_historical(&mut self, identity_bytes: u64) -> Result<(), IndexPartialSourceErrorV1> {
    self.charge_work(1)?;
    self.historical_documents = self.historical_documents.checked_add(1).ok_or_else(|| {
      IndexPartialSourceErrorV1::resource_limit("native_partial_complement_historical_count", "historical document counter overflowed")
    })?;
    if self.historical_documents > self.limits.maximum_candidate_documents() {
      return Err(IndexPartialSourceErrorV1::resource_limit(
        "native_partial_complement_historical_limit",
        "historical ScopeReverse identities exceed the admitted candidate-document bound",
      ));
    }
    self.identity_bytes = self.identity_bytes.checked_add(identity_bytes).ok_or_else(|| {
      IndexPartialSourceErrorV1::resource_limit("native_partial_complement_identity_bytes", "historical identity byte counter overflowed")
    })?;
    if self.identity_bytes > self.limits.maximum_identity_bytes() {
      return Err(IndexPartialSourceErrorV1::resource_limit(
        "native_partial_complement_identity_limit",
        "historical ScopeOrdinal identities exceed the admitted identity-byte bound",
      ));
    }
    Ok(())
  }
}

impl IndexChangedDocumentSourceV1 for NativeQueryPartialComplementSourceV1<'_> {
  fn scan_changed_documents(
    &mut self,
    request: IndexChangedDocumentScanRequestV1<'_>,
    visitor: &mut dyn IndexChangedDocumentVisitorV1,
  ) -> Result<IndexChangedDocumentScanReceiptV1, IndexPartialScanErrorV1> {
    self.validate_request(&request)?;
    let view_cancellation = self.inner.view.cancellation().clone();
    let mut budget = NativePartialComplementBudgetV1 {
      limits: self.limits,
      request_cancellation: request.cancellation,
      view_cancellation: &view_cancellation,
      work_steps: 0,
      page_seeks: 0,
      historical_documents: 0,
      identity_bytes: 0,
    };
    let ordinal_root = self
      .artifacts
      .resolve_scope_root(
        self.scope_id(),
        request.source_namespace_root,
        Some(request.covered_through_publication_sequence),
        OrderedIndexRoleV1::ScopeOrdinal,
      )
      .map_err(map_execution_partial_error)?;
    let reverse_root = self
      .artifacts
      .resolve_scope_root(
        self.scope_id(),
        request.source_namespace_root,
        Some(request.covered_through_publication_sequence),
        OrderedIndexRoleV1::ScopeReverse,
      )
      .map_err(map_execution_partial_error)?;
    validate_partial_scope_root_pair(self.scope_id(), ordinal_root.as_ref(), reverse_root.as_ref())?;

    let mut target = self.inner.workspace.open_cursor().map_err(map_workspace_error).map_err(map_execution_partial_error)?;
    let mut prior_target = None;
    let mut target_head = next_partial_target(&mut target, self.scope_id(), request.cancellation, &mut budget, &mut prior_target)?;
    let mut prior_historical = None;
    let mut changed_document_count = 0u64;

    if let (Some(ordinal_root), Some(reverse_root)) = (ordinal_root.as_ref(), reverse_root.as_ref()) {
      for page_ordinal in 0..reverse_root.summary().page_count {
        budget.charge_page()?;
        let cursor_request = partial_cursor_request(
          request.hash_algorithm,
          reverse_root,
          OrderedIndexRoleV1::ScopeReverse,
          ArtifactPageSeekV1::PageOrdinal(page_ordinal),
          self.limits,
        );
        let reverse_cursor = load_artifact_page_cursor_v1(&cursor_request, &mut self.artifacts, &|| {
          request.cancellation.is_cancelled() || self.inner.view.cancellation().is_cancelled()
        })
        .map_err(map_artifact_cursor_partial_error)?
        .ok_or_else(|| {
          IndexPartialSourceErrorV1::corrupt(
            "native_partial_reverse_page_missing",
            "a nonempty ScopeReverse root omitted one of its declared pages",
          )
        })?;
        let reverse_page = decode_ordered_page(reverse_cursor.page(), request.hash_algorithm).map_err(|error| {
          IndexPartialSourceErrorV1::corrupt("native_partial_reverse_page", format!("cannot decode ScopeReverse page: {error}"))
        })?;
        for record in reverse_page.records.iter() {
          budget.charge_work(1)?;
          let record = record.map_err(|error| {
            IndexPartialSourceErrorV1::corrupt("native_partial_reverse_record", format!("cannot decode ScopeReverse row: {error}"))
          })?;
          let reverse = decode_scope_reverse_record(record.encoded, request.hash_algorithm).map_err(|error| {
            IndexPartialSourceErrorV1::corrupt("native_partial_reverse_record", format!("cannot decode ScopeReverse row: {error}"))
          })?;
          if prior_historical.as_ref().is_some_and(|prior: &NativePartialTargetIdentityV1| prior.file_key() >= reverse.file_key) {
            return Err(
              IndexPartialSourceErrorV1::corrupt(
                "native_partial_reverse_order",
                "historical ScopeReverse rows are not in strict FileKey order",
              )
              .into(),
            );
          }
          let historical = self.resolve_historical_identity(request.hash_algorithm, ordinal_root, &reverse, &mut budget)?;
          prior_historical = Some(fixed_target_identity(historical.file_key(), historical.record_revision())?);
          if !historical.within_query_path {
            continue;
          }
          while target_head.as_ref().is_some_and(|target| target.file_key() < historical.file_key()) {
            let current = target_head
              .ok_or_else(|| IndexPartialSourceErrorV1::internal("native_partial_target_state", "validated target head disappeared"))?;
            changed_document_count = emit_native_changed_document(
              visitor,
              current.file_key(),
              None,
              Some(current.record_revision()),
              changed_document_count,
              request.maximum_changed_documents,
              &budget,
            )?;
            target_head = next_partial_target(&mut target, self.scope_id(), request.cancellation, &mut budget, &mut prior_target)?;
          }
          match target_head.as_ref().map(|target| target.file_key().cmp(historical.file_key())) {
            Some(Ordering::Equal) => {
              let current = target_head.ok_or_else(|| {
                IndexPartialSourceErrorV1::internal("native_partial_target_state", "validated equal target head disappeared")
              })?;
              if current.record_revision() != historical.record_revision() {
                changed_document_count = emit_native_changed_document(
                  visitor,
                  historical.file_key(),
                  Some(historical.record_revision()),
                  Some(current.record_revision()),
                  changed_document_count,
                  request.maximum_changed_documents,
                  &budget,
                )?;
              }
              target_head = next_partial_target(&mut target, self.scope_id(), request.cancellation, &mut budget, &mut prior_target)?;
            }
            Some(Ordering::Greater) | None => {
              changed_document_count = emit_native_changed_document(
                visitor,
                historical.file_key(),
                Some(historical.record_revision()),
                None,
                changed_document_count,
                request.maximum_changed_documents,
                &budget,
              )?;
            }
            Some(Ordering::Less) => {
              return Err(
                IndexPartialSourceErrorV1::internal(
                  "native_partial_merge_state",
                  "target FileKey remained below the historical FileKey after merge advancement",
                )
                .into(),
              );
            }
          }
        }
      }
      if budget.historical_documents != reverse_root.summary().live_count {
        return Err(
          IndexPartialSourceErrorV1::corrupt(
            "native_partial_reverse_live_count",
            "ScopeReverse traversal disagrees with its captured root live count",
          )
          .into(),
        );
      }
    }
    while let Some(current) = target_head {
      changed_document_count = emit_native_changed_document(
        visitor,
        current.file_key(),
        None,
        Some(current.record_revision()),
        changed_document_count,
        request.maximum_changed_documents,
        &budget,
      )?;
      target_head = next_partial_target(&mut target, self.scope_id(), request.cancellation, &mut budget, &mut prior_target)?;
    }
    budget.require_not_cancelled()?;
    Ok(IndexChangedDocumentScanReceiptV1 {
      source_namespace_root: clone_partial_bytes(request.source_namespace_root, "complement source NamespaceRoot")?,
      target_namespace_root: clone_partial_bytes(request.target_namespace_root, "complement target NamespaceRoot")?,
      covered_through_publication_sequence: request.covered_through_publication_sequence,
      target_publication_sequence: request.target_publication_sequence,
      changed_document_count,
      complete: true,
    })
  }
}

impl NativeQueryPartialComplementSourceV1<'_> {
  fn scope_id(&self) -> &[u8] {
    &self.scope_id[..self.scope_id_length]
  }

  fn validate_request(&self, request: &IndexChangedDocumentScanRequestV1<'_>) -> Result<(), IndexPartialSourceErrorV1> {
    require_not_cancelled(request.cancellation).map_err(map_execution_partial_error)?;
    require_not_cancelled(self.inner.view.cancellation()).map_err(map_execution_partial_error)?;
    validate_identity(request.generation_manifest_hash, request.hash_algorithm, "partial generation manifest")
      .map_err(map_execution_partial_error)?;
    validate_identity(request.source_namespace_root, request.hash_algorithm, "partial source NamespaceRoot")
      .map_err(map_execution_partial_error)?;
    validate_identity(request.target_namespace_root, request.hash_algorithm, "partial target NamespaceRoot")
      .map_err(map_execution_partial_error)?;
    if request.hash_algorithm != self.inner.view.hash_algorithm()
      || request.target_namespace_root != self.inner.view.root_metadata().hash
      || request.target_publication_sequence != self.inner.view.authority().admission.publication_sequence
      || request.covered_through_publication_sequence >= request.target_publication_sequence
      || request.maximum_changed_documents == 0
    {
      return Err(IndexPartialSourceErrorV1::corrupt(
        "native_partial_complement_authority",
        "changed-document request does not bind a strict historical-to-selected-root interval",
      ));
    }
    Ok(())
  }

  fn resolve_historical_identity(
    &mut self,
    algorithm: HashAlgorithm,
    ordinal_root: &QueryCandidateArtifactRootV1,
    reverse: &super::index_record::ScopeReverseRecordV1<'_>,
    budget: &mut NativePartialComplementBudgetV1<'_>,
  ) -> Result<NativePartialHistoricalIdentityV1, IndexPartialSourceErrorV1> {
    budget.charge_page()?;
    let ordinal_key = reverse.document_ordinal.to_le_bytes();
    let request = partial_cursor_request(
      algorithm,
      ordinal_root,
      OrderedIndexRoleV1::ScopeOrdinal,
      ArtifactPageSeekV1::OrderLowerBound(&ordinal_key),
      self.limits,
    );
    let cursor = load_artifact_page_cursor_v1(&request, &mut self.artifacts, &|| {
      budget.request_cancellation.is_cancelled() || budget.view_cancellation.is_cancelled()
    })
    .map_err(map_artifact_cursor_partial_error)?
    .ok_or_else(|| {
      IndexPartialSourceErrorV1::corrupt(
        "native_partial_ordinal_missing",
        "ScopeReverse identity resolves beyond the captured ScopeOrdinal root",
      )
    })?;
    let page = decode_ordered_page(cursor.page(), algorithm).map_err(|error| {
      IndexPartialSourceErrorV1::corrupt("native_partial_ordinal_page", format!("cannot decode ScopeOrdinal page: {error}"))
    })?;
    let mut selected: Option<ScopeDocumentRecordV1<'_>> = None;
    for record in page.records.iter() {
      budget.charge_work(1)?;
      let record = record.map_err(|error| {
        IndexPartialSourceErrorV1::corrupt("native_partial_ordinal_record", format!("cannot decode ScopeOrdinal row: {error}"))
      })?;
      if record.document_ordinal < reverse.document_ordinal {
        continue;
      }
      if record.document_ordinal == reverse.document_ordinal {
        selected = Some(decode_scope_document_record(record.encoded, algorithm).map_err(|error| {
          IndexPartialSourceErrorV1::corrupt("native_partial_ordinal_record", format!("cannot decode ScopeOrdinal row: {error}"))
        })?);
      }
      break;
    }
    let selected = selected.ok_or_else(|| {
      IndexPartialSourceErrorV1::corrupt("native_partial_ordinal_missing", "ScopeReverse identity has no exact ScopeOrdinal row")
    })?;
    if selected.tombstone || selected.file_key != reverse.file_key || selected.document_ordinal != reverse.document_ordinal {
      return Err(IndexPartialSourceErrorV1::corrupt(
        "native_partial_scope_bijection",
        "ScopeReverse identity does not map to the same live ScopeOrdinal row",
      ));
    }
    let identity_bytes = selected
      .file_key
      .len()
      .checked_add(selected.record_revision_hash.len())
      .and_then(|bytes| bytes.checked_add(selected.path.len()))
      .ok_or_else(|| {
        IndexPartialSourceErrorV1::resource_limit(
          "native_partial_complement_identity_bytes",
          "historical identity byte length overflowed usize",
        )
      })?;
    let identity_bytes = u64::try_from(identity_bytes).map_err(|error| {
      IndexPartialSourceErrorV1::resource_limit(
        "native_partial_complement_identity_bytes",
        format!("historical identity bytes exceed u64: {error}"),
      )
    })?;
    budget.charge_historical(identity_bytes)?;
    let (file_key, hash_length) =
      retain_fixed_identity(selected.file_key, algorithm, "historical FileKey").map_err(map_execution_partial_error)?;
    let (record_revision, revision_length) =
      retain_fixed_identity(selected.record_revision_hash, algorithm, "historical RecordRevision").map_err(map_execution_partial_error)?;
    if revision_length != hash_length {
      return Err(IndexPartialSourceErrorV1::internal(
        "native_partial_identity_width",
        "validated historical identities retained different hash widths",
      ));
    }
    Ok(NativePartialHistoricalIdentityV1 {
      file_key,
      record_revision,
      hash_length,
      within_query_path: path_is_within(&self.inner.query_path, selected.path),
    })
  }
}

impl IndexPartialCandidateRecheckerV1 for NativeQueryPartialRecheckerV1<'_, '_> {
  fn recheck(&mut self, request: IndexPartialRecheckRequestV1<'_>) -> Result<IndexPartialRecheckOutcomeV1, IndexPartialSourceErrorV1> {
    require_not_cancelled(request.cancellation).map_err(map_execution_partial_error)?;
    require_not_cancelled(self.inner.view.cancellation()).map_err(map_execution_partial_error)?;
    validate_identity(request.file_key, request.hash_algorithm, "partial recheck FileKey").map_err(map_execution_partial_error)?;
    if let Some(revision) = request.basis_revision_hash {
      validate_identity(revision, request.hash_algorithm, "partial recheck basis RecordRevision").map_err(map_execution_partial_error)?;
    }
    if let Some(revision) = request.expected_target_revision_hash {
      validate_identity(revision, request.hash_algorithm, "partial recheck target RecordRevision").map_err(map_execution_partial_error)?;
    }
    if request.hash_algorithm != self.plan.hash_algorithm()
      || request.target_namespace_root != self.plan.selected_namespace_root()
      || request.target_publication_sequence != self.plan.publication_sequence()
      || request.query_fingerprint != self.plan.query_fingerprint()
    {
      return Err(IndexPartialSourceErrorV1::corrupt(
        "native_partial_recheck_authority",
        "partial recheck request does not bind the captured compiled selected-root query",
      ));
    }
    let retained_scope_id = self.scope_id;
    let scope_id = &retained_scope_id[..self.scope_id_length];
    if self.lookup.is_none() {
      self.lookup = Some(self.inner.workspace.open_lookup().map_err(map_workspace_error).map_err(map_execution_partial_error)?);
    }
    let lookup = self.lookup.as_mut().ok_or_else(|| {
      IndexPartialSourceErrorV1::internal(
        "native_partial_recheck_lookup_state",
        "selected-root workspace lookup was not retained after successful initialization",
      )
    })?;
    let Some(ordered) =
      lookup.find_row(request.file_key, request.cancellation).map_err(map_workspace_error).map_err(map_execution_partial_error)?
    else {
      return Ok(IndexPartialRecheckOutcomeV1::Absent);
    };
    if ordered.scope_id() != Some(scope_id) {
      return Ok(IndexPartialRecheckOutcomeV1::Absent);
    }
    let reader = self
      .inner
      .source
      .selected_namespace_reader(&self.inner.view, self.inner.namespace_limits)
      .map_err(map_native_error)
      .map_err(map_execution_partial_error)?;
    let row = reader
      .restore_ordered_file_row(ordered.file_key(), ordered.record_revision(), ordered.entity_version(), ordered.encoded_file_record())
      .map_err(map_native_error)
      .map_err(map_execution_partial_error)?;
    if !path_is_within(&self.inner.query_path, row.path()) {
      return Err(IndexPartialSourceErrorV1::corrupt(
        "native_partial_recheck_path",
        "selected-root recheck row is outside the captured query path",
      ));
    }
    if self.prepared_sources.is_none() {
      self.prepared_sources = Some(
        NativePreparedSourceCacheV1::new(self.inner, self.inner.maximum_prepared_source_cache_entries)
          .map_err(map_execution_partial_error)?,
      );
    }
    let prepared_sources = self.prepared_sources.as_mut().ok_or_else(|| {
      IndexPartialSourceErrorV1::internal(
        "native_partial_recheck_prepared_cache_state",
        "partial recheck prepared cache was not retained after initialization",
      )
    })?;
    let mut fields = NativeAuthoritativeRowFieldSourceV1 {
      inner: self.inner,
      prepared_sources,
      row: &row,
      effective_scope_id: scope_id,
      source_limits: selected_source_limits().map_err(map_execution_partial_error)?,
    };
    let matches = evaluate_authoritative_query_document_v1(RootAwareQueryDocumentEvaluationRequestV1 {
      plan: self.plan,
      catalogs: self.inner.semantic_catalog.catalogs(),
      scope_id,
      document: QueryExecutionDocumentV1 { file_key: row.file_key(), record_revision: row.record_revision(), path: row.path() },
      fields: &mut fields,
      memory: self.inner.source.memory_coordinator().as_ref(),
      cancellation: request.cancellation,
      limits: self.limits,
    })
    .map_err(map_query_execution_partial_error)?;
    require_not_cancelled(request.cancellation).map_err(map_execution_partial_error)?;
    require_not_cancelled(self.inner.view.cancellation()).map_err(map_execution_partial_error)?;
    Ok(IndexPartialRecheckOutcomeV1::Present {
      record_revision_hash: clone_partial_bytes(row.record_revision(), "partial recheck RecordRevision")?,
      matches,
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
    let mut prepared_sources = NativePreparedSourceCacheV1::new(self.inner.as_ref(), self.inner.maximum_prepared_source_cache_entries)
      .map_err(QueryExecutionScanErrorV1::Source)?;
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
        inner: self.inner.as_ref(),
        prepared_sources: &mut prepared_sources,
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

impl QueryAuthoritativeFieldSourceV1 for NativeAuthoritativeRowFieldSourceV1<'_, '_, '_, '_> {
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
    let evaluation = evaluate_authoritative_field(
      self.inner,
      self.prepared_sources,
      &reader,
      catalog_index,
      self.row,
      self.effective_scope_id,
      self.source_limits,
    )
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

impl NativeAuthoritativeAuxiliarySourceV1<'_, '_> {
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
    &mut self,
    binding_index: usize,
    row: &super::read_view_native::NativeSelectedNamespaceFileRowV1,
    effective_scope_id: Option<&[u8]>,
    maximum_values: u64,
    maximum_bytes: u64,
  ) -> Result<NativeAuxiliaryFieldEvaluationV1, QueryExecutionSourceErrorV1> {
    let binding = self.fields.get(binding_index).ok_or_else(|| {
      source_error(
        QueryExecutionSourceErrorClassV1::Internal,
        "native_auxiliary_field_index",
        "native auxiliary binding index is outside the compiled field set",
      )
    })?;
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
    let evaluation = self.prepared_sources.evaluate(&reader, binding.catalog_index, row, effective_scope_id, selected_source_limits()?)?;
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
        let logical = convert_auxiliary_values(binding, scope_binding, &values, maximum_values, maximum_bytes)?;
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

  fn field_binding(&self, field_name: &str) -> Result<&NativeAuxiliaryFieldBindingV1<'_>, QueryExecutionSourceErrorV1> {
    self.fields.iter().find(|field| field.field_name == field_name).ok_or_else(|| {
      source_error(
        QueryExecutionSourceErrorClassV1::Corrupt,
        "native_auxiliary_field_binding",
        format!("compiled auxiliary field {field_name:?} has no plan-bound native definition"),
      )
    })
  }

  fn field_binding_index(&self, field_name: &str) -> Result<usize, QueryExecutionSourceErrorV1> {
    self.fields.iter().position(|field| field.field_name == field_name).ok_or_else(|| {
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
  source: &NativeAuthoritativeRowFieldSourceV1<'_, '_, '_, '_>,
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
  prepared_sources: &mut NativePreparedSourceCacheV1<'_>,
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
  let evaluation = prepared_sources.evaluate(reader, catalog_index, row, effective_scope_id, source_limits)?;
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
  fn open_field_partition<'source>(
    &'source self,
    request: QueryExecutionFieldPartitionOpenRequestV1<'_>,
  ) -> Result<Box<dyn QueryAuthoritativeFieldPartitionCursorV1 + 'source>, QueryExecutionSourceErrorV1> {
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
    let prepared_sources = NativePreparedSourceCacheV1::new(self.inner.as_ref(), self.inner.maximum_prepared_source_cache_entries)?;
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
      inner: self.inner.as_ref(),
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
      prepared_sources,
      exhausted: false,
      finished: false,
      failed: false,
      _memory: cursor_memory,
    }))
  }
}

struct NativeAuthoritativeFieldPartitionCursorV1<'source> {
  inner: &'source NativeAuthoritativeFieldPartitionInnerV1,
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
  prepared_sources: NativePreparedSourceCacheV1<'source>,
  exhausted: bool,
  finished: bool,
  failed: bool,
  _memory: MemoryReservation,
}

impl NativeAuthoritativeFieldPartitionCursorV1<'_> {
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
      let evaluation = evaluate_authoritative_field(
        self.inner,
        &mut self.prepared_sources,
        &reader,
        self.catalog_index,
        &row,
        selected_scope_id,
        self.source_limits,
      )?;
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

impl QueryAuthoritativeFieldPartitionCursorV1 for NativeAuthoritativeFieldPartitionCursorV1<'_> {
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

impl PositionUniverseSourceV1 for NativeAuthoritativeAuxiliarySourceV1<'_, '_> {
  fn resolve_position(
    &mut self,
    request: PositionUniverseLookupRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<PositionUniverseLookupResultV1, PositionUniverseSourceErrorV1> {
    self.resolve_position_inner(request, cancellation).map_err(map_position_source_error)
  }
}

impl NativeAuthoritativeAuxiliarySourceV1<'_, '_> {
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
      let binding_index = self.field_binding_index(&sort.field)?;
      if self.fields[binding_index].comparator != comparator {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_auxiliary_position_comparator",
          "query order comparator disagrees with its plan-bound field definition",
        ));
      }
      let maximum_values = request.maximum_row_bytes() / size_of::<LogicalOrderComponentOwnedV1>() as u64;
      let evaluation =
        self.evaluate_field(binding_index, &row, effective_scope_id.as_deref(), maximum_values, request.maximum_row_bytes())?;
      let component = match evaluation.state {
        QueryExecutionFieldStateV1::Missing | QueryExecutionFieldStateV1::DeterministicUnindexable => {
          LogicalOrderComponentOwnedV1::missing()
        }
        QueryExecutionFieldStateV1::Values => select_position_component(&self.fields[binding_index], sort.direction, evaluation.values)?,
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

impl QueryAggregateInputSourceV1 for NativeAuthoritativeAuxiliarySourceV1<'_, '_> {
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
      let binding_index = self.field_binding_index(expected.field_name())?;
      let maximum_values = request.limits().maximum_values_per_field().min(remaining_values);
      let evaluation =
        self.evaluate_field(binding_index, &row, effective_scope_id.as_deref(), maximum_values, request.limits().maximum_row_bytes())?;
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
  validate_native_plan_authority(
    inner,
    plan,
    "native_auxiliary_plan_authority",
    "compiled query plan does not bind the native selected-root source",
  )
}

fn validate_native_plan_authority(
  inner: &NativeAuthoritativeFieldPartitionInnerV1,
  plan: &CompiledRootAwareQueryPlanV1,
  code: &'static str,
  context: &'static str,
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
    return Err(source_error(QueryExecutionSourceErrorClassV1::Corrupt, code, context));
  }
  Ok(())
}

fn logical_explain_structural_bytes(plan: &CompiledRootAwareQueryPlanV1) -> Result<u64, QueryExecutionSourceErrorV1> {
  let field_count = plan.predicates().len().checked_add(plan.auxiliary_fields().len()).ok_or_else(|| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_query_explain_retained_bytes", "field count overflowed")
  })?;
  let field_count = u64::try_from(field_count).map_err(|error| {
    source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_query_explain_retained_bytes",
      format!("field count does not fit retained-byte accounting: {error}"),
    )
  })?;
  let mut bytes = field_count
    .checked_mul(size_of::<QueryLogicalExplainFieldV1>() as u64)
    .and_then(|field_bytes| LOGICAL_EXPLAIN_FIXED_BYTES.checked_add(field_bytes))
    .ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_query_explain_retained_bytes", "field slots overflowed")
    })?;
  for predicate in plan.predicates() {
    bytes = bytes
      .checked_add(predicate.field_name().len() as u64)
      .and_then(|value| value.checked_add(predicate.operation_name().len() as u64))
      .ok_or_else(|| {
        source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_query_explain_retained_bytes", "field bytes overflowed")
      })?;
  }
  for auxiliary in plan.auxiliary_fields() {
    let operation_bytes = match auxiliary.operation() {
      super::query_planner::CompiledQueryAuxiliaryOperationV1::Sort(_) => "sort".len(),
      super::query_planner::CompiledQueryAuxiliaryOperationV1::Aggregate(_) => "aggregate".len(),
      super::query_planner::CompiledQueryAuxiliaryOperationV1::Group => "group".len(),
    };
    bytes = bytes.checked_add(auxiliary.field_name().len() as u64).and_then(|value| value.checked_add(operation_bytes as u64)).ok_or_else(
      || source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_query_explain_retained_bytes", "auxiliary bytes overflowed"),
    )?;
  }
  Ok(bytes)
}

fn logical_explain_retained_bytes(explain: &QueryLogicalExplainV1) -> Result<u64, QueryExecutionSourceErrorV1> {
  let field_bytes = explain.fields.capacity().checked_mul(size_of::<QueryLogicalExplainFieldV1>()).ok_or_else(|| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_query_explain_retained_bytes", "field capacity overflowed")
  })?;
  let field_bytes = u64::try_from(field_bytes).map_err(|error| {
    source_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "native_query_explain_retained_bytes",
      format!("field capacity does not fit retained-byte accounting: {error}"),
    )
  })?;
  let mut bytes = LOGICAL_EXPLAIN_FIXED_BYTES.checked_add(field_bytes).ok_or_else(|| {
    source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_query_explain_retained_bytes", "field capacity overflowed")
  })?;
  for field in &explain.fields {
    bytes = bytes
      .checked_add(field.field.capacity() as u64)
      .and_then(|value| value.checked_add(field.operation.capacity() as u64))
      .ok_or_else(|| {
        source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_query_explain_retained_bytes", "field capacity overflowed")
      })?;
  }
  Ok(bytes)
}

fn map_query_planning_error(error: QueryPlanningErrorV1) -> QueryExecutionSourceErrorV1 {
  let class = match error.class() {
    QueryPlanningErrorClassV1::InvalidRequest => QueryExecutionSourceErrorClassV1::Corrupt,
    QueryPlanningErrorClassV1::ResourceLimit => QueryExecutionSourceErrorClassV1::ResourceLimit,
    QueryPlanningErrorClassV1::HistoricalViewUnavailable => QueryExecutionSourceErrorClassV1::Unavailable,
    QueryPlanningErrorClassV1::CorruptSource => QueryExecutionSourceErrorClassV1::Corrupt,
    QueryPlanningErrorClassV1::Cancelled => QueryExecutionSourceErrorClassV1::Cancelled,
  };
  source_error(class, error.code(), error.context())
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
    let scope_slots = field.scopes().len().checked_mul(size_of::<NativeAuxiliaryScopeBindingV1<'static>>()).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_binding_bytes", "scope slot bytes overflowed")
    })?;
    let scope_id_bytes = field.scopes().len().checked_mul(plan.hash_algorithm().hash_length()).ok_or_else(|| {
      source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_auxiliary_binding_bytes", "ScopeId bytes overflowed")
    })?;
    bytes = bytes
      .checked_add(size_of::<NativeAuxiliaryFieldBindingV1<'static>>() as u64)
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

fn compile_auxiliary_bindings<'definition>(
  inner: &'definition NativeAuthoritativeFieldPartitionInnerV1,
  plan: &CompiledRootAwareQueryPlanV1,
  structural_binding_bytes: u64,
  limits: NativeAuthoritativeAuxiliaryLimitsV1,
) -> Result<(Vec<NativeAuxiliaryFieldBindingV1<'definition>>, u64), QueryExecutionSourceErrorV1> {
  let mut fields: Vec<NativeAuxiliaryFieldBindingV1<'definition>> = Vec::new();
  let mut retained_binding_bytes = structural_binding_bytes;
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
      let remaining_runtime_bytes = limits.maximum_binding_bytes.checked_sub(retained_binding_bytes).ok_or_else(|| {
        source_error(
          QueryExecutionSourceErrorClassV1::ResourceLimit,
          "native_auxiliary_binding_limit",
          "compiled auxiliary runtimes exceed the retained-byte limit",
        )
      })?;
      let runtime_inline_bytes = size_of::<IndexDefinitionRuntimeV1<'static, 'static>>() as u64;
      let maximum_runtime_bytes = remaining_runtime_bytes.checked_add(runtime_inline_bytes).ok_or_else(|| {
        source_error(
          QueryExecutionSourceErrorClassV1::ResourceLimit,
          "native_auxiliary_binding_bytes",
          "compiled auxiliary runtime bound overflowed",
        )
      })?;
      let runtime = IndexDefinitionRuntimeV1::from_encoded_bounded(
        &catalog_scope.encoded_value_store_definition,
        &candidate.encoded_field_definition,
        plan.hash_algorithm(),
        maximum_runtime_bytes,
      )
      .map_err(map_index_definition_error)?;
      let runtime_retained_bytes = runtime.maximum_retained_bytes().map_err(map_index_definition_error)?;
      let runtime_allocated_bytes = runtime_retained_bytes.checked_sub(runtime_inline_bytes).ok_or_else(|| {
        source_error(
          QueryExecutionSourceErrorClassV1::Internal,
          "native_auxiliary_memory_accounting",
          "compiled auxiliary runtime retained fewer bytes than its inline representation",
        )
      })?;
      retained_binding_bytes = retained_binding_bytes.checked_add(runtime_allocated_bytes).ok_or_else(|| {
        source_error(
          QueryExecutionSourceErrorClassV1::ResourceLimit,
          "native_auxiliary_binding_bytes",
          "compiled auxiliary runtime bytes overflowed",
        )
      })?;
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
        runtime,
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
  Ok((fields, retained_binding_bytes))
}

fn validate_aggregate_request(
  source: &NativeAuthoritativeAuxiliarySourceV1<'_, '_>,
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
  binding: &NativeAuxiliaryFieldBindingV1<'_>,
  scope_binding: &NativeAuxiliaryScopeBindingV1<'_>,
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
  let compiled = scope_binding.runtime.compile_source_values(&non_null).map_err(map_index_definition_error)?;
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
  binding: &NativeAuxiliaryFieldBindingV1<'_>,
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

fn map_native_exact_scope_setup_error(error: QueryExecutionSourceErrorV1) -> QueryExactScopeExecutionErrorV1 {
  QueryExactScopeExecutionErrorV1::Execution(map_source_error(error))
}

fn map_query_execution_partial_error(error: QueryExecutionErrorV1) -> IndexPartialSourceErrorV1 {
  match error.class() {
    QueryExecutionErrorClassV1::InvalidRequest | QueryExecutionErrorClassV1::CorruptSource => {
      IndexPartialSourceErrorV1::corrupt(error.code(), error.context())
    }
    QueryExecutionErrorClassV1::ResourceLimit => IndexPartialSourceErrorV1::resource_limit(error.code(), error.context()),
    QueryExecutionErrorClassV1::HistoricalViewUnavailable => IndexPartialSourceErrorV1::unavailable(error.code(), error.context()),
    QueryExecutionErrorClassV1::Cancelled => IndexPartialSourceErrorV1::cancelled(error.code(), error.context()),
    QueryExecutionErrorClassV1::Internal => IndexPartialSourceErrorV1::internal(error.code(), error.context()),
  }
}

fn map_artifact_cursor_partial_error(error: ArtifactPageCursorErrorV1) -> IndexPartialSourceErrorV1 {
  let code = error.code();
  let context = error.to_string();
  match error {
    ArtifactPageCursorErrorV1::Cancelled => IndexPartialSourceErrorV1::cancelled(code, context),
    ArtifactPageCursorErrorV1::SourcePressure(_) | ArtifactPageCursorErrorV1::Allocation(_) => {
      IndexPartialSourceErrorV1::resource_limit(code, context)
    }
    ArtifactPageCursorErrorV1::SourceOperational(_) => IndexPartialSourceErrorV1::unavailable(code, context),
    ArtifactPageCursorErrorV1::InvalidLimits(_)
    | ArtifactPageCursorErrorV1::MissingArtifact { .. }
    | ArtifactPageCursorErrorV1::SourceCorrupt(_)
    | ArtifactPageCursorErrorV1::Malformed(_) => IndexPartialSourceErrorV1::corrupt(code, context),
  }
}

fn retain_fixed_identity(identity: &[u8], algorithm: HashAlgorithm, role: &str) -> Result<([u8; 64], usize), QueryExecutionSourceErrorV1> {
  validate_identity(identity, algorithm, role)?;
  let mut retained = [0u8; 64];
  retained[..identity.len()].copy_from_slice(identity);
  Ok((retained, identity.len()))
}

fn fixed_target_identity(file_key: &[u8], record_revision: &[u8]) -> Result<NativePartialTargetIdentityV1, IndexPartialSourceErrorV1> {
  if file_key.is_empty() || file_key.len() > 64 || record_revision.len() != file_key.len() {
    return Err(IndexPartialSourceErrorV1::corrupt(
      "native_partial_target_identity",
      "target FileKey and RecordRevision have inconsistent fixed hash widths",
    ));
  }
  let mut retained_file_key = [0u8; 64];
  retained_file_key[..file_key.len()].copy_from_slice(file_key);
  let mut retained_revision = [0u8; 64];
  retained_revision[..record_revision.len()].copy_from_slice(record_revision);
  Ok(NativePartialTargetIdentityV1 { file_key: retained_file_key, record_revision: retained_revision, hash_length: file_key.len() })
}

fn validate_partial_scope_root_pair(
  scope_id: &[u8],
  ordinal: Option<&QueryCandidateArtifactRootV1>,
  reverse: Option<&QueryCandidateArtifactRootV1>,
) -> Result<(), IndexPartialSourceErrorV1> {
  match (ordinal, reverse) {
    (None, None) => Ok(()),
    (Some(ordinal), Some(reverse))
      if ordinal.owner_id() == scope_id
        && reverse.owner_id() == scope_id
        && ordinal.generation() == reverse.generation()
        && ordinal.summary().live_count == reverse.summary().live_count
        && reverse.summary().tombstone_count == 0 =>
    {
      Ok(())
    }
    (Some(_), Some(_)) => Err(IndexPartialSourceErrorV1::corrupt(
      "native_partial_scope_root_pair",
      "captured ScopeOrdinal and ScopeReverse roots do not describe one exact live scope closure",
    )),
    (Some(_), None) | (None, Some(_)) => Err(IndexPartialSourceErrorV1::corrupt(
      "native_partial_scope_root_absence",
      "captured scope closure has only one of its ScopeOrdinal/ScopeReverse roots",
    )),
  }
}

fn partial_cursor_request<'a>(
  algorithm: HashAlgorithm,
  root: &'a QueryCandidateArtifactRootV1,
  role: OrderedIndexRoleV1,
  seek: ArtifactPageSeekV1<'a>,
  limits: QueryCompleteCandidateLimitsV1,
) -> ArtifactPageCursorRequestV1<'a> {
  ArtifactPageCursorRequestV1 {
    root: ArtifactPageCursorRootV1 {
      hash_algorithm: algorithm,
      root_key: root.root_key(),
      owner_id: root.owner_id(),
      role,
      maximum_generation: root.generation(),
      expected_summary: Some(root.summary()),
    },
    seek,
    neighbors: ArtifactPageNeighborModeV1::None,
    limits: limits.cursor(),
  }
}

fn next_partial_target(
  cursor: &mut NativeQueryOrderingCursorV1,
  scope_id: &[u8],
  cancellation: &CancellationToken,
  budget: &mut NativePartialComplementBudgetV1<'_>,
  prior: &mut Option<NativePartialTargetIdentityV1>,
) -> Result<Option<NativePartialTargetIdentityV1>, IndexPartialSourceErrorV1> {
  loop {
    budget.charge_work(1)?;
    let Some(row) = cursor.next_row(cancellation).map_err(map_workspace_error).map_err(map_execution_partial_error)? else {
      return Ok(None);
    };
    let identity = fixed_target_identity(row.file_key(), row.record_revision())?;
    if prior.as_ref().is_some_and(|prior| prior.file_key() >= identity.file_key()) {
      return Err(IndexPartialSourceErrorV1::corrupt(
        "native_partial_target_order",
        "selected-root workspace rows are not in strict FileKey order",
      ));
    }
    *prior = Some(identity);
    if row.scope_id() == Some(scope_id) {
      return Ok(Some(identity));
    }
  }
}

fn emit_native_changed_document(
  visitor: &mut dyn IndexChangedDocumentVisitorV1,
  file_key: &[u8],
  basis_revision_hash: Option<&[u8]>,
  target_revision_hash: Option<&[u8]>,
  changed_document_count: u64,
  maximum_changed_documents: u64,
  budget: &NativePartialComplementBudgetV1<'_>,
) -> Result<u64, IndexPartialScanErrorV1> {
  budget.require_not_cancelled()?;
  if basis_revision_hash.is_none() && target_revision_hash.is_none()
    || basis_revision_hash.is_some() && basis_revision_hash == target_revision_hash
  {
    return Err(
      IndexPartialSourceErrorV1::corrupt(
        "native_partial_changed_identity",
        "changed-document merge produced an unchanged or doubly absent identity",
      )
      .into(),
    );
  }
  let next = changed_document_count
    .checked_add(1)
    .ok_or_else(|| IndexPartialSourceErrorV1::resource_limit("native_partial_changed_count", "changed-document count overflowed"))?;
  if next > maximum_changed_documents {
    return Err(
      IndexPartialSourceErrorV1::resource_limit(
        "native_partial_changed_limit",
        "exact changed-document complement exceeds its requested document bound",
      )
      .into(),
    );
  }
  visitor.visit(IndexChangedDocumentV1 { file_key, basis_revision_hash, target_revision_hash })?;
  budget.require_not_cancelled()?;
  Ok(next)
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

pub(super) fn path_is_within(parent: &str, child: &str) -> bool {
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

pub(super) fn map_workspace_error(error: NativeQueryOrderingWorkspaceErrorV1) -> QueryExecutionSourceErrorV1 {
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

pub(super) fn map_native_error(error: NativeSelectedNamespaceReadErrorV1) -> QueryExecutionSourceErrorV1 {
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
