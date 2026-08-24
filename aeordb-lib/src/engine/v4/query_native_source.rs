//! Native selected-root source for partitioned authoritative query truth.

use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryOwner, MemoryReservation};

use super::query_executor::{
  QueryAuthoritativeFieldPartitionCursorV1, QueryAuthoritativeFieldPartitionSourceV1, QueryExecutionFieldDocumentV1,
  QueryExecutionFieldPartitionOpenRequestV1, QueryExecutionFieldPartitionReceiptV1, QueryExecutionFieldStateV1,
  QueryExecutionSourceErrorClassV1, QueryExecutionSourceErrorV1,
};
use super::query_native_workspace::{
  NativeQueryOrderingCursorV1, NativeQueryOrderingWorkspaceBuilderV1, NativeQueryOrderingWorkspaceErrorClassV1,
  NativeQueryOrderingWorkspaceErrorV1, NativeQueryOrderingWorkspaceLimitsV1, NativeQueryOrderingWorkspaceV1,
};
use super::query_planner::RootAwareQueryFieldCatalogV1;
use super::read_view::ResolvedReadViewV1;
use super::read_view_authorization::ResolvedPathAuthorizationV1;
use super::read_view_native::{
  NativeReadViewSourceV1, NativeSelectedNamespaceLimitsV1, NativeSelectedNamespaceReadErrorClassV1, NativeSelectedNamespaceReadErrorV1,
  NativeSelectedSemanticCatalogV1, NativeSelectedSourceLimitsV1, NativeSelectedSourceOutcomeV1, NativeSelectedSourceParserV1,
};
use super::scope::{EffectiveScopeCandidateV1, EffectiveScopeResolverV1, is_internal_index_path_v1, validate_canonical_absolute_path};

const MAXIMUM_PARTITION_SCOPES: usize = 1_024;
const MAXIMUM_SCOPE_RESOLVER_BYTES: u64 = 64 * 1024 * 1024;
const PARTITION_CURSOR_FIXED_BYTES: u64 = 16 * 1024;
const PARTITION_SOURCE_MAXIMUM_RETAINED_BYTES: u64 = 256 * 1024 * 1024;

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
      let catalog = &self.inner.semantic_catalog.catalogs()[self.catalog_index];
      let evaluation = reader
        .prepare_authoritative_source(catalog, selected_scope_id, self.source_limits)
        .and_then(|evaluator| evaluator.evaluate(&row, NativeSelectedSourceParserV1::Native, None))
        .map_err(map_native_error)?;
      if evaluation.selected_root() != self.inner.view.root_metadata().hash
        || evaluation.semantic_state_root() != self.inner.view.authority().semantic_state.object_id
        || evaluation.scope_id() != selected_scope_id
        || evaluation.file_key() != row.file_key()
        || evaluation.record_revision() != row.record_revision()
      {
        return Err(source_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "native_partition_source_receipt",
          "selected source evaluation receipt disagrees with its ordered document",
        ));
      }
      let outcome = evaluation.into_outcome();
      let (state, values) = match outcome {
        NativeSelectedSourceOutcomeV1::Missing => (QueryExecutionFieldStateV1::Missing, Vec::new()),
        NativeSelectedSourceOutcomeV1::Values(values) => (QueryExecutionFieldStateV1::Values, values),
        NativeSelectedSourceOutcomeV1::ParserUnindexable(_) | NativeSelectedSourceOutcomeV1::SourceUnindexable { .. } => {
          (QueryExecutionFieldStateV1::DeterministicUnindexable, Vec::new())
        }
        NativeSelectedSourceOutcomeV1::OutOfScope => {
          return Err(source_error(
            QueryExecutionSourceErrorClassV1::Corrupt,
            "native_partition_scope_resolution",
            "effective-scope winner was out of scope during authoritative evaluation",
          ));
        }
      };
      let next_scope_count = self.scope_document_counts[scope_index].checked_add(1).ok_or_else(|| {
        source_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "native_partition_scope_count", "scope document count overflowed")
      })?;
      (Some(try_clone_bytes(selected_scope_id, "document ScopeId")?), state, values, Some((scope_index, next_scope_count)), None)
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
