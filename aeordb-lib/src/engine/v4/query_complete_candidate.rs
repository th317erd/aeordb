//! Bounded complete-generation Posting and ScopeOrdinal candidate scans.
//!
//! These immutable artifacts may narrow selected-root query work, but they are
//! never result authority. The caller must exact-recheck every returned
//! FileKey, revision, path, and source value before making a row observable.

use std::cmp::Ordering;
use std::error::Error;
use std::fmt;
use std::mem::size_of;

use tokio_util::sync::CancellationToken;

use crate::engine::HashAlgorithm;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryCoordinatorError, MemoryOwner, MemoryReservation};

use super::index_artifact_cursor::{
  ArtifactCursorSourceV1, ArtifactDirectoryRootSummaryV1, ArtifactPageCursorErrorV1, ArtifactPageCursorLimitsV1,
  ArtifactPageCursorRequestV1, ArtifactPageCursorRootV1, ArtifactPageNeighborModeV1, ArtifactPageSeekV1, load_artifact_page_cursor_v1,
};
use super::index_page::{OrderedIndexRoleV1, decode_ordered_page, decode_posting_record};
use super::index_record::decode_scope_document_record;
use super::query_planner::{
  CompiledQueryCoverageV1, CompiledQueryExpressionV1, CompiledQueryIndexCandidateV1, CompiledRootAwareQueryPlanV1,
  QueryCoordinateConstraintV1, QueryPlanDriverV1, QueryValueMatchV1, RootAwareQueryFieldCatalogV1,
};
use super::query_executor::{
  QueryAuthoritativeDocumentVisitorV1, QueryAuthoritativeScopeSourceV1, QueryExecutionErrorV1, QueryExecutionLimitsV1,
  QueryExecutionMatchSinkV1, QueryExecutionScanErrorV1, QueryExecutionScopeScanReceiptV1, QueryExecutionScopeScanRequestV1,
  QueryExecutionSourceErrorClassV1, QueryExecutionSourceErrorV1, QueryExecutionStreamReceiptV1, RootAwareQueryExecutionRequestV1,
  RootAwareQueryExecutionV1, RootAwareQueryScopeExecutionRequestV1, execute_authoritative_root_query_into_v1,
  execute_authoritative_root_query_v1, execute_authoritative_scope_query_into_v1, execute_authoritative_scope_query_v1,
};

const MAXIMUM_PAGE_SEEKS_V1: u64 = 1_048_576;
const MAXIMUM_POSTING_RECORDS_V1: u64 = 16_777_216;
const MAXIMUM_CANDIDATES_V1: u64 = 4_194_304;
const MAXIMUM_IDENTITY_BYTES_V1: u64 = 512 * 1_024 * 1_024;
const MAXIMUM_WORK_STEPS_V1: u64 = 268_435_456;
const MAXIMUM_RETAINED_BYTES_V1: u64 = 1024 * 1_024 * 1_024;
const RESULT_FIXED_BYTES_V1: u64 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryCompleteCandidateErrorClassV1 {
  InvalidRequest,
  ResourceLimit,
  HistoricalViewUnavailable,
  CorruptSource,
  Cancelled,
  Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCompleteCandidateErrorV1 {
  class: QueryCompleteCandidateErrorClassV1,
  code: &'static str,
  context: String,
}

impl QueryCompleteCandidateErrorV1 {
  pub const fn class(&self) -> QueryCompleteCandidateErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }

  fn invalid(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryCompleteCandidateErrorClassV1::InvalidRequest, code, context: context.into() }
  }

  fn resource(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryCompleteCandidateErrorClassV1::ResourceLimit, code, context: context.into() }
  }

  fn unavailable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryCompleteCandidateErrorClassV1::HistoricalViewUnavailable, code, context: context.into() }
  }

  fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryCompleteCandidateErrorClassV1::CorruptSource, code, context: context.into() }
  }

  fn cancelled() -> Self {
    Self {
      class: QueryCompleteCandidateErrorClassV1::Cancelled,
      code: "query_complete_candidate_cancelled",
      context: "complete candidate scan was cancelled".to_string(),
    }
  }

  fn internal(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: QueryCompleteCandidateErrorClassV1::Internal, code, context: context.into() }
  }
}

impl fmt::Display for QueryCompleteCandidateErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for QueryCompleteCandidateErrorV1 {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCandidateArtifactRootV1 {
  root_key: Vec<u8>,
  owner_id: Vec<u8>,
  generation: u64,
  summary: ArtifactDirectoryRootSummaryV1,
}

impl QueryCandidateArtifactRootV1 {
  pub fn new(
    root_key: Vec<u8>,
    owner_id: Vec<u8>,
    generation: u64,
    summary: ArtifactDirectoryRootSummaryV1,
  ) -> Result<Self, QueryCompleteCandidateErrorV1> {
    if root_key.is_empty() || owner_id.is_empty() || generation == 0 {
      return Err(QueryCompleteCandidateErrorV1::invalid(
        "query_candidate_artifact_root",
        "candidate artifact root key, owner, and generation must be nonzero",
      ));
    }
    if summary.page_count == 0 || summary.live_count.checked_add(summary.tombstone_count).is_none() {
      return Err(QueryCompleteCandidateErrorV1::invalid(
        "query_candidate_artifact_summary",
        "candidate artifact root summary is empty or overflows",
      ));
    }
    Ok(Self { root_key, owner_id, generation, summary })
  }

  pub fn root_key(&self) -> &[u8] {
    &self.root_key
  }

  pub fn owner_id(&self) -> &[u8] {
    &self.owner_id
  }

  pub const fn generation(&self) -> u64 {
    self.generation
  }

  pub const fn summary(&self) -> ArtifactDirectoryRootSummaryV1 {
    self.summary
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryCompleteCandidateLimitsV1 {
  maximum_page_seeks: u64,
  maximum_posting_records: u64,
  maximum_candidate_documents: u64,
  maximum_identity_bytes: u64,
  maximum_work_steps: u64,
  maximum_retained_bytes: u64,
  cursor: ArtifactPageCursorLimitsV1,
}

impl QueryCompleteCandidateLimitsV1 {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    maximum_page_seeks: u64,
    maximum_posting_records: u64,
    maximum_candidate_documents: u64,
    maximum_identity_bytes: u64,
    maximum_work_steps: u64,
    maximum_retained_bytes: u64,
    cursor: ArtifactPageCursorLimitsV1,
  ) -> Result<Self, QueryCompleteCandidateErrorV1> {
    if maximum_page_seeks == 0
      || maximum_page_seeks > MAXIMUM_PAGE_SEEKS_V1
      || maximum_posting_records == 0
      || maximum_posting_records > MAXIMUM_POSTING_RECORDS_V1
      || maximum_candidate_documents == 0
      || maximum_candidate_documents > MAXIMUM_CANDIDATES_V1
      || maximum_identity_bytes == 0
      || maximum_identity_bytes > MAXIMUM_IDENTITY_BYTES_V1
      || maximum_work_steps == 0
      || maximum_work_steps > MAXIMUM_WORK_STEPS_V1
      || maximum_retained_bytes == 0
      || maximum_retained_bytes > MAXIMUM_RETAINED_BYTES_V1
    {
      return Err(QueryCompleteCandidateErrorV1::invalid(
        "query_complete_candidate_limits",
        "complete candidate limits are zero or exceed their frozen protocol ceilings",
      ));
    }
    Ok(Self {
      maximum_page_seeks,
      maximum_posting_records,
      maximum_candidate_documents,
      maximum_identity_bytes,
      maximum_work_steps,
      maximum_retained_bytes,
      cursor,
    })
  }

  pub const fn maximum_candidate_documents(self) -> u64 {
    self.maximum_candidate_documents
  }
}

#[derive(Clone, Copy)]
pub struct QueryCompletePostingScanRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub selected_namespace_root: &'a [u8],
  pub scope_id: &'a [u8],
  pub candidate: &'a CompiledQueryIndexCandidateV1,
  pub posting_root: Option<&'a QueryCandidateArtifactRootV1>,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub limits: QueryCompleteCandidateLimitsV1,
}

#[derive(Clone, Copy)]
pub struct QueryPartialPostingScanRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub source_namespace_root: &'a [u8],
  pub scope_id: &'a [u8],
  pub candidate: &'a CompiledQueryIndexCandidateV1,
  pub posting_root: Option<&'a QueryCandidateArtifactRootV1>,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub limits: QueryCompleteCandidateLimitsV1,
}

pub struct QueryCompletePostingCandidatesV1 {
  document_ordinals: Vec<u64>,
  examined_posting_records: u64,
  examined_pages: u64,
  retained_bytes: u64,
  _memory: MemoryReservation,
}

impl fmt::Debug for QueryCompletePostingCandidatesV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("QueryCompletePostingCandidatesV1")
      .field("document_ordinals", &self.document_ordinals)
      .field("examined_posting_records", &self.examined_posting_records)
      .field("examined_pages", &self.examined_pages)
      .field("retained_bytes", &self.retained_bytes)
      .finish_non_exhaustive()
  }
}

impl QueryCompletePostingCandidatesV1 {
  pub fn document_ordinals(&self) -> &[u64] {
    &self.document_ordinals
  }

  pub const fn examined_posting_records(&self) -> u64 {
    self.examined_posting_records
  }

  pub const fn examined_pages(&self) -> u64 {
    self.examined_pages
  }

  pub const fn retained_bytes(&self) -> u64 {
    self.retained_bytes
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryScopeOrdinalSelectionV1<'a> {
  CandidateOrdinals(&'a [u64]),
  AllLive,
}

#[derive(Clone, Copy)]
pub struct QueryCompleteScopeResolutionRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub selected_namespace_root: &'a [u8],
  pub scope_id: &'a [u8],
  pub scope_ordinal_root: Option<&'a QueryCandidateArtifactRootV1>,
  pub selection: QueryScopeOrdinalSelectionV1<'a>,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub limits: QueryCompleteCandidateLimitsV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCompleteCandidateIdentityV1 {
  document_ordinal: u64,
  file_key: Vec<u8>,
  record_revision: Vec<u8>,
  path: String,
}

impl QueryCompleteCandidateIdentityV1 {
  pub const fn document_ordinal(&self) -> u64 {
    self.document_ordinal
  }

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

pub struct QueryCompleteCandidateIdentitiesV1 {
  identities: Vec<QueryCompleteCandidateIdentityV1>,
  examined_scope_records: u64,
  examined_pages: u64,
  retained_bytes: u64,
  _memory: MemoryReservation,
}

impl fmt::Debug for QueryCompleteCandidateIdentitiesV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("QueryCompleteCandidateIdentitiesV1")
      .field("identities", &self.identities)
      .field("examined_scope_records", &self.examined_scope_records)
      .field("examined_pages", &self.examined_pages)
      .field("retained_bytes", &self.retained_bytes)
      .finish_non_exhaustive()
  }
}

impl QueryCompleteCandidateIdentitiesV1 {
  pub fn identities(&self) -> &[QueryCompleteCandidateIdentityV1] {
    &self.identities
  }

  pub const fn examined_scope_records(&self) -> u64 {
    self.examined_scope_records
  }

  pub const fn examined_pages(&self) -> u64 {
    self.examined_pages
  }

  pub const fn retained_bytes(&self) -> u64 {
    self.retained_bytes
  }
}

#[derive(Clone, Copy)]
pub struct QueryCompletePostingRootRequestV1<'a> {
  pub selected_namespace_root: &'a [u8],
  pub publication_sequence: u64,
  pub scope_id: &'a [u8],
  pub candidate: &'a CompiledQueryIndexCandidateV1,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCompletePostingRootReceiptV1 {
  pub selected_namespace_root: Vec<u8>,
  pub publication_sequence: u64,
  pub scope_id: Vec<u8>,
  pub index_id: Vec<u8>,
  pub generation: u64,
  pub generation_manifest_hash: Vec<u8>,
  pub coverage_source_root: Vec<u8>,
  pub root: Option<QueryCandidateArtifactRootV1>,
  pub complete: bool,
}

#[derive(Clone, Copy)]
pub struct QueryCompleteScopeRootRequestV1<'a> {
  pub selected_namespace_root: &'a [u8],
  pub publication_sequence: u64,
  pub scope_id: &'a [u8],
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCompleteScopeRootReceiptV1 {
  pub selected_namespace_root: Vec<u8>,
  pub publication_sequence: u64,
  pub scope_id: Vec<u8>,
  pub root: Option<QueryCandidateArtifactRootV1>,
  pub complete: bool,
}

#[derive(Clone, Copy)]
pub struct QueryCandidateRecheckRequestV1<'a> {
  pub selected_namespace_root: &'a [u8],
  pub publication_sequence: u64,
  pub scope_id: &'a [u8],
  pub file_key: &'a [u8],
  pub indexed_revision: &'a [u8],
  pub indexed_path: &'a str,
  pub cancellation: &'a CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCandidateRecheckReceiptV1 {
  pub selected_namespace_root: Vec<u8>,
  pub publication_sequence: u64,
  pub scope_id: Vec<u8>,
  pub file_key: Vec<u8>,
  pub indexed_revision: Vec<u8>,
  pub indexed_path: String,
  pub document_count: u64,
  pub complete: bool,
}

pub trait QueryCompleteCandidateSourceV1: ArtifactCursorSourceV1 {
  /// Resolve the exact complete FieldIndex Posting root selected by the
  /// compiled plan. The returned receipt is validated before any artifact I/O.
  fn resolve_complete_posting_root(
    &mut self,
    request: QueryCompletePostingRootRequestV1<'_>,
  ) -> Result<QueryCompletePostingRootReceiptV1, QueryExecutionSourceErrorV1>;

  /// Resolve the exact selected ScopeCatalog ordinal root for one effective
  /// scope. An absent root is a complete empty scope only with a complete
  /// receipt.
  fn resolve_complete_scope_root(
    &mut self,
    request: QueryCompleteScopeRootRequestV1<'_>,
  ) -> Result<QueryCompleteScopeRootReceiptV1, QueryExecutionSourceErrorV1>;

  /// Re-resolve one immutable index identity against selected NamespaceRoot
  /// authority and invoke the visitor exactly once with the same
  /// FileKey/revision/path. Field values supplied to that visitor must be
  /// authoritative selected-root values.
  fn recheck_complete_candidate(
    &mut self,
    request: QueryCandidateRecheckRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryCandidateRecheckReceiptV1, QueryExecutionScanErrorV1>;
}

pub struct QueryCompleteCandidateExecutionRequestV1<'a> {
  pub plan: &'a CompiledRootAwareQueryPlanV1,
  pub catalogs: &'a [RootAwareQueryFieldCatalogV1],
  pub source: &'a mut dyn QueryCompleteCandidateSourceV1,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub execution_limits: QueryExecutionLimitsV1,
  pub candidate_limits: QueryCompleteCandidateLimitsV1,
}

pub struct QueryCompleteCandidateScopeExecutionRequestV1<'a> {
  pub plan: &'a CompiledRootAwareQueryPlanV1,
  pub catalogs: &'a [RootAwareQueryFieldCatalogV1],
  pub scope_id: &'a [u8],
  pub source: &'a mut dyn QueryCompleteCandidateSourceV1,
  pub memory: &'a MemoryCoordinator,
  pub cancellation: &'a CancellationToken,
  pub execution_limits: QueryExecutionLimitsV1,
  pub candidate_limits: QueryCompleteCandidateLimitsV1,
}

pub struct QueryCompleteCandidateExecutionV1 {
  execution: RootAwareQueryExecutionV1,
  examined_posting_records: u64,
  examined_artifact_pages: u64,
  resolved_candidate_identities: u64,
  authoritative_rechecks: u64,
}

impl fmt::Debug for QueryCompleteCandidateExecutionV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("QueryCompleteCandidateExecutionV1")
      .field("execution", &self.execution)
      .field("examined_posting_records", &self.examined_posting_records)
      .field("examined_artifact_pages", &self.examined_artifact_pages)
      .field("resolved_candidate_identities", &self.resolved_candidate_identities)
      .field("authoritative_rechecks", &self.authoritative_rechecks)
      .finish()
  }
}

impl QueryCompleteCandidateExecutionV1 {
  pub const fn execution(&self) -> &RootAwareQueryExecutionV1 {
    &self.execution
  }

  pub const fn examined_posting_records(&self) -> u64 {
    self.examined_posting_records
  }

  pub const fn examined_artifact_pages(&self) -> u64 {
    self.examined_artifact_pages
  }

  pub const fn resolved_candidate_identities(&self) -> u64 {
    self.resolved_candidate_identities
  }

  pub const fn authoritative_rechecks(&self) -> u64 {
    self.authoritative_rechecks
  }
}

pub struct QueryCompleteCandidateStreamExecutionV1 {
  receipt: QueryExecutionStreamReceiptV1,
  examined_posting_records: u64,
  examined_artifact_pages: u64,
  resolved_candidate_identities: u64,
  authoritative_rechecks: u64,
}

impl fmt::Debug for QueryCompleteCandidateStreamExecutionV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("QueryCompleteCandidateStreamExecutionV1")
      .field("receipt", &self.receipt)
      .field("examined_posting_records", &self.examined_posting_records)
      .field("examined_artifact_pages", &self.examined_artifact_pages)
      .field("resolved_candidate_identities", &self.resolved_candidate_identities)
      .field("authoritative_rechecks", &self.authoritative_rechecks)
      .finish()
  }
}

impl QueryCompleteCandidateStreamExecutionV1 {
  pub const fn receipt(&self) -> &QueryExecutionStreamReceiptV1 {
    &self.receipt
  }

  pub const fn examined_posting_records(&self) -> u64 {
    self.examined_posting_records
  }

  pub const fn examined_artifact_pages(&self) -> u64 {
    self.examined_artifact_pages
  }

  pub const fn resolved_candidate_identities(&self) -> u64 {
    self.resolved_candidate_identities
  }

  pub const fn authoritative_rechecks(&self) -> u64 {
    self.authoritative_rechecks
  }
}

#[derive(Default)]
struct QueryCompleteCandidateExecutionStatsV1 {
  examined_posting_records: u64,
  examined_artifact_pages: u64,
  resolved_candidate_identities: u64,
  authoritative_rechecks: u64,
}

struct CompleteCandidateScopeAdapterV1<'a> {
  plan: &'a CompiledRootAwareQueryPlanV1,
  source: &'a mut dyn QueryCompleteCandidateSourceV1,
  memory: &'a MemoryCoordinator,
  cancellation: &'a CancellationToken,
  limits: QueryCompleteCandidateLimitsV1,
  stats: QueryCompleteCandidateExecutionStatsV1,
}

enum ExpressionCandidateSetV1 {
  Universe,
  Ordinals(Vec<u64>),
}

pub fn execute_complete_candidate_root_query_v1(
  request: QueryCompleteCandidateExecutionRequestV1<'_>,
) -> Result<QueryCompleteCandidateExecutionV1, QueryExecutionErrorV1> {
  execute_complete_candidate_query_v1(request, None)
}

pub fn execute_complete_candidate_root_query_into_v1(
  request: QueryCompleteCandidateExecutionRequestV1<'_>,
  sink: &mut dyn QueryExecutionMatchSinkV1,
) -> Result<QueryCompleteCandidateStreamExecutionV1, QueryExecutionErrorV1> {
  execute_complete_candidate_query_into_v1(request, None, sink)
}

pub fn execute_complete_candidate_scope_query_v1(
  request: QueryCompleteCandidateScopeExecutionRequestV1<'_>,
) -> Result<QueryCompleteCandidateExecutionV1, QueryExecutionErrorV1> {
  let scope_id = request.scope_id;
  execute_complete_candidate_query_v1(
    QueryCompleteCandidateExecutionRequestV1 {
      plan: request.plan,
      catalogs: request.catalogs,
      source: request.source,
      memory: request.memory,
      cancellation: request.cancellation,
      execution_limits: request.execution_limits,
      candidate_limits: request.candidate_limits,
    },
    Some(scope_id),
  )
}

pub fn execute_complete_candidate_scope_query_into_v1(
  request: QueryCompleteCandidateScopeExecutionRequestV1<'_>,
  sink: &mut dyn QueryExecutionMatchSinkV1,
) -> Result<QueryCompleteCandidateStreamExecutionV1, QueryExecutionErrorV1> {
  let scope_id = request.scope_id;
  execute_complete_candidate_query_into_v1(
    QueryCompleteCandidateExecutionRequestV1 {
      plan: request.plan,
      catalogs: request.catalogs,
      source: request.source,
      memory: request.memory,
      cancellation: request.cancellation,
      execution_limits: request.execution_limits,
      candidate_limits: request.candidate_limits,
    },
    Some(scope_id),
    sink,
  )
}

fn execute_complete_candidate_query_v1(
  request: QueryCompleteCandidateExecutionRequestV1<'_>,
  scope_id: Option<&[u8]>,
) -> Result<QueryCompleteCandidateExecutionV1, QueryExecutionErrorV1> {
  let mut adapter = CompleteCandidateScopeAdapterV1 {
    plan: request.plan,
    source: request.source,
    memory: request.memory,
    cancellation: request.cancellation,
    limits: request.candidate_limits,
    stats: QueryCompleteCandidateExecutionStatsV1::default(),
  };
  let execution = if let Some(scope_id) = scope_id {
    execute_authoritative_scope_query_v1(RootAwareQueryScopeExecutionRequestV1 {
      plan: request.plan,
      catalogs: request.catalogs,
      scope_id,
      source: &mut adapter,
      memory: request.memory,
      cancellation: request.cancellation,
      limits: request.execution_limits,
    })?
  } else {
    execute_authoritative_root_query_v1(RootAwareQueryExecutionRequestV1 {
      plan: request.plan,
      catalogs: request.catalogs,
      source: &mut adapter,
      memory: request.memory,
      cancellation: request.cancellation,
      limits: request.execution_limits,
    })?
  };
  Ok(QueryCompleteCandidateExecutionV1 {
    execution,
    examined_posting_records: adapter.stats.examined_posting_records,
    examined_artifact_pages: adapter.stats.examined_artifact_pages,
    resolved_candidate_identities: adapter.stats.resolved_candidate_identities,
    authoritative_rechecks: adapter.stats.authoritative_rechecks,
  })
}

fn execute_complete_candidate_query_into_v1(
  request: QueryCompleteCandidateExecutionRequestV1<'_>,
  scope_id: Option<&[u8]>,
  sink: &mut dyn QueryExecutionMatchSinkV1,
) -> Result<QueryCompleteCandidateStreamExecutionV1, QueryExecutionErrorV1> {
  let mut adapter = CompleteCandidateScopeAdapterV1 {
    plan: request.plan,
    source: request.source,
    memory: request.memory,
    cancellation: request.cancellation,
    limits: request.candidate_limits,
    stats: QueryCompleteCandidateExecutionStatsV1::default(),
  };
  let receipt = if let Some(scope_id) = scope_id {
    execute_authoritative_scope_query_into_v1(
      RootAwareQueryScopeExecutionRequestV1 {
        plan: request.plan,
        catalogs: request.catalogs,
        scope_id,
        source: &mut adapter,
        memory: request.memory,
        cancellation: request.cancellation,
        limits: request.execution_limits,
      },
      sink,
    )?
  } else {
    execute_authoritative_root_query_into_v1(
      RootAwareQueryExecutionRequestV1 {
        plan: request.plan,
        catalogs: request.catalogs,
        source: &mut adapter,
        memory: request.memory,
        cancellation: request.cancellation,
        limits: request.execution_limits,
      },
      sink,
    )?
  };
  Ok(QueryCompleteCandidateStreamExecutionV1 {
    receipt,
    examined_posting_records: adapter.stats.examined_posting_records,
    examined_artifact_pages: adapter.stats.examined_artifact_pages,
    resolved_candidate_identities: adapter.stats.resolved_candidate_identities,
    authoritative_rechecks: adapter.stats.authoritative_rechecks,
  })
}

impl QueryAuthoritativeScopeSourceV1 for CompleteCandidateScopeAdapterV1<'_> {
  fn scan_scope(
    &mut self,
    request: QueryExecutionScopeScanRequestV1<'_>,
    visitor: &mut dyn QueryAuthoritativeDocumentVisitorV1,
  ) -> Result<QueryExecutionScopeScanReceiptV1, QueryExecutionScanErrorV1> {
    validate_scope_scan_request(self.plan, request)?;
    validate_expression_workspace(self.plan.expression(), self.limits)?;
    let _workspace = reserve_execution_workspace(self.memory, self.limits.maximum_retained_bytes)?;
    let candidate_set = expression_candidate_set(
      self.plan,
      self.plan.expression(),
      request.scope_id,
      self.source,
      self.memory,
      self.cancellation,
      self.limits,
      &mut self.stats,
    )?;
    let scope_receipt = self
      .source
      .resolve_complete_scope_root(QueryCompleteScopeRootRequestV1 {
        selected_namespace_root: self.plan.selected_namespace_root(),
        publication_sequence: self.plan.publication_sequence(),
        scope_id: request.scope_id,
        cancellation: self.cancellation,
      })
      .map_err(QueryExecutionScanErrorV1::Source)?;
    validate_scope_root_receipt(self.plan, request.scope_id, &scope_receipt)?;
    let selection = match &candidate_set {
      ExpressionCandidateSetV1::Universe => QueryScopeOrdinalSelectionV1::AllLive,
      ExpressionCandidateSetV1::Ordinals(ordinals) => QueryScopeOrdinalSelectionV1::CandidateOrdinals(ordinals),
    };
    let identities = resolve_complete_scope_identities_v1(
      QueryCompleteScopeResolutionRequestV1 {
        hash_algorithm: self.plan.hash_algorithm(),
        selected_namespace_root: self.plan.selected_namespace_root(),
        scope_id: request.scope_id,
        scope_ordinal_root: scope_receipt.root.as_ref(),
        selection,
        memory: self.memory,
        cancellation: self.cancellation,
        limits: self.limits,
      },
      self.source,
    )
    .map_err(map_candidate_scan_error)?;
    self.stats.examined_artifact_pages =
      checked_stat_add(self.stats.examined_artifact_pages, identities.examined_pages(), "query_candidate_page_stat")?;
    self.stats.resolved_candidate_identities =
      checked_stat_add(self.stats.resolved_candidate_identities, identities.identities().len() as u64, "query_candidate_identity_stat")?;

    let mut visited = 0u64;
    for identity in identities.identities() {
      if self.cancellation.is_cancelled() {
        return Err(source_scan_error(
          QueryExecutionSourceErrorClassV1::Cancelled,
          "query_complete_candidate_cancelled",
          "complete candidate recheck was cancelled",
        ));
      }
      let mut recheck = RecheckVisitorV1 { expected: identity, downstream: visitor, visits: 0, mismatch: false, failure: None };
      let receipt = self.source.recheck_complete_candidate(
        QueryCandidateRecheckRequestV1 {
          selected_namespace_root: self.plan.selected_namespace_root(),
          publication_sequence: self.plan.publication_sequence(),
          scope_id: request.scope_id,
          file_key: identity.file_key(),
          indexed_revision: identity.record_revision(),
          indexed_path: identity.path(),
          cancellation: self.cancellation,
        },
        &mut recheck,
      );
      if let Some(error) = recheck.failure.take() {
        return Err(QueryExecutionScanErrorV1::Visitor(error));
      }
      let receipt = receipt?;
      validate_recheck_receipt(self.plan, request.scope_id, identity, &recheck, &receipt)?;
      visited = visited.checked_add(1).ok_or_else(|| {
        source_scan_error(
          QueryExecutionSourceErrorClassV1::ResourceLimit,
          "query_candidate_recheck_overflow",
          "candidate recheck count overflowed",
        )
      })?;
      self.stats.authoritative_rechecks = checked_stat_add(self.stats.authoritative_rechecks, 1, "query_candidate_recheck_stat")?;
    }
    Ok(QueryExecutionScopeScanReceiptV1 {
      selected_namespace_root: self.plan.selected_namespace_root().to_vec(),
      publication_sequence: self.plan.publication_sequence(),
      scope_id: request.scope_id.to_vec(),
      document_count: visited,
      complete: true,
    })
  }
}

struct RecheckVisitorV1<'a, 'b> {
  expected: &'a QueryCompleteCandidateIdentityV1,
  downstream: &'b mut dyn QueryAuthoritativeDocumentVisitorV1,
  visits: u64,
  mismatch: bool,
  failure: Option<QueryExecutionErrorV1>,
}

impl QueryAuthoritativeDocumentVisitorV1 for RecheckVisitorV1<'_, '_> {
  fn visit(
    &mut self,
    document: super::query_executor::QueryExecutionDocumentV1<'_>,
    fields: &mut dyn super::query_executor::QueryAuthoritativeFieldSourceV1,
  ) -> Result<(), QueryExecutionErrorV1> {
    if let Some(error) = &self.failure {
      return Err(error.clone());
    }
    self.visits = self.visits.saturating_add(1);
    if self.visits != 1
      || document.file_key != self.expected.file_key
      || document.record_revision != self.expected.record_revision
      || document.path != self.expected.path
    {
      self.mismatch = true;
      return Ok(());
    }
    let result = self.downstream.visit(document, fields);
    if let Err(error) = &result {
      self.failure = Some(error.clone());
    }
    result
  }
}

#[allow(clippy::too_many_arguments)]
fn expression_candidate_set(
  plan: &CompiledRootAwareQueryPlanV1,
  expression: &CompiledQueryExpressionV1,
  scope_id: &[u8],
  source: &mut dyn QueryCompleteCandidateSourceV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  limits: QueryCompleteCandidateLimitsV1,
  stats: &mut QueryCompleteCandidateExecutionStatsV1,
) -> Result<ExpressionCandidateSetV1, QueryExecutionScanErrorV1> {
  if cancellation.is_cancelled() {
    return Err(source_scan_error(
      QueryExecutionSourceErrorClassV1::Cancelled,
      "query_complete_candidate_cancelled",
      "candidate expression evaluation was cancelled",
    ));
  }
  match expression {
    CompiledQueryExpressionV1::Field(predicate_index) => {
      let predicate = plan.predicates().get(*predicate_index).ok_or_else(|| {
        source_scan_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "query_candidate_predicate_index",
          "compiled expression predicate index is out of bounds",
        )
      })?;
      let scope = predicate.scopes().iter().find(|scope| scope.scope_id() == scope_id).ok_or_else(|| {
        source_scan_error(
          QueryExecutionSourceErrorClassV1::Corrupt,
          "query_candidate_scope_plan",
          "compiled predicate omits the effective scope being executed",
        )
      })?;
      match scope.driver() {
        QueryPlanDriverV1::Authoritative { .. } => Ok(ExpressionCandidateSetV1::Universe),
        QueryPlanDriverV1::Index { candidate_index, coverage: CompiledQueryCoverageV1::Complete, .. } => {
          let candidate = scope.candidates().get(*candidate_index).ok_or_else(|| {
            source_scan_error(
              QueryExecutionSourceErrorClassV1::Corrupt,
              "query_candidate_driver_index",
              "compiled candidate driver index is out of bounds",
            )
          })?;
          scan_one_complete_candidate(plan, scope_id, candidate, source, memory, cancellation, limits, stats)
            .map(ExpressionCandidateSetV1::Ordinals)
        }
        QueryPlanDriverV1::IndexUnion { candidate_indexes, coverage: CompiledQueryCoverageV1::Complete, .. } => {
          let mut union = Vec::new();
          for candidate_index in candidate_indexes {
            let candidate = scope.candidates().get(*candidate_index).ok_or_else(|| {
              source_scan_error(
                QueryExecutionSourceErrorClassV1::Corrupt,
                "query_candidate_driver_index",
                "compiled union candidate index is out of bounds",
              )
            })?;
            let ordinals = scan_one_complete_candidate(plan, scope_id, candidate, source, memory, cancellation, limits, stats)?;
            union = merge_sorted_ordinals(union, ordinals, SetOperationV1::Union, limits.maximum_candidate_documents)
              .map_err(map_candidate_scan_error)?;
          }
          Ok(ExpressionCandidateSetV1::Ordinals(union))
        }
        QueryPlanDriverV1::Index { .. } | QueryPlanDriverV1::IndexUnion { .. } => Ok(ExpressionCandidateSetV1::Universe),
      }
    }
    CompiledQueryExpressionV1::And(children) => {
      let mut intersection = None;
      for child in children {
        match expression_candidate_set(plan, child, scope_id, source, memory, cancellation, limits, stats)? {
          ExpressionCandidateSetV1::Universe => {}
          ExpressionCandidateSetV1::Ordinals(ordinals) => {
            intersection = Some(match intersection {
              None => ordinals,
              Some(current) => merge_sorted_ordinals(current, ordinals, SetOperationV1::Intersection, limits.maximum_candidate_documents)
                .map_err(map_candidate_scan_error)?,
            });
          }
        }
      }
      Ok(intersection.map_or(ExpressionCandidateSetV1::Universe, ExpressionCandidateSetV1::Ordinals))
    }
    CompiledQueryExpressionV1::Or(children) => {
      let mut union = Vec::new();
      for child in children {
        match expression_candidate_set(plan, child, scope_id, source, memory, cancellation, limits, stats)? {
          ExpressionCandidateSetV1::Universe => return Ok(ExpressionCandidateSetV1::Universe),
          ExpressionCandidateSetV1::Ordinals(ordinals) => {
            union = merge_sorted_ordinals(union, ordinals, SetOperationV1::Union, limits.maximum_candidate_documents)
              .map_err(map_candidate_scan_error)?;
          }
        }
      }
      Ok(ExpressionCandidateSetV1::Ordinals(union))
    }
    // A Posting set is a conservative superset. Complementing it would remove
    // false positives that may satisfy NOT after exact recheck, so NOT always
    // starts from the complete live scope universe.
    CompiledQueryExpressionV1::Not(_) => Ok(ExpressionCandidateSetV1::Universe),
  }
}

#[allow(clippy::too_many_arguments)]
fn scan_one_complete_candidate(
  plan: &CompiledRootAwareQueryPlanV1,
  scope_id: &[u8],
  candidate: &CompiledQueryIndexCandidateV1,
  source: &mut dyn QueryCompleteCandidateSourceV1,
  memory: &MemoryCoordinator,
  cancellation: &CancellationToken,
  limits: QueryCompleteCandidateLimitsV1,
  stats: &mut QueryCompleteCandidateExecutionStatsV1,
) -> Result<Vec<u64>, QueryExecutionScanErrorV1> {
  let receipt = source
    .resolve_complete_posting_root(QueryCompletePostingRootRequestV1 {
      selected_namespace_root: plan.selected_namespace_root(),
      publication_sequence: plan.publication_sequence(),
      scope_id,
      candidate,
      cancellation,
    })
    .map_err(QueryExecutionScanErrorV1::Source)?;
  validate_posting_root_receipt(plan, scope_id, candidate, &receipt)?;
  let scanned = scan_complete_posting_ordinals_v1(
    QueryCompletePostingScanRequestV1 {
      hash_algorithm: plan.hash_algorithm(),
      selected_namespace_root: plan.selected_namespace_root(),
      scope_id,
      candidate,
      posting_root: receipt.root.as_ref(),
      memory,
      cancellation,
      limits,
    },
    source,
  )
  .map_err(map_candidate_scan_error)?;
  stats.examined_posting_records =
    checked_stat_add(stats.examined_posting_records, scanned.examined_posting_records(), "query_candidate_posting_stat")?;
  stats.examined_artifact_pages = checked_stat_add(stats.examined_artifact_pages, scanned.examined_pages(), "query_candidate_page_stat")?;
  let mut ordinals = Vec::new();
  ordinals.try_reserve_exact(scanned.document_ordinals().len()).map_err(|error| {
    source_scan_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "query_candidate_ordinal_allocation",
      format!("cannot retain expression candidate ordinals: {error}"),
    )
  })?;
  ordinals.extend_from_slice(scanned.document_ordinals());
  Ok(ordinals)
}

fn validate_scope_scan_request(
  plan: &CompiledRootAwareQueryPlanV1,
  request: QueryExecutionScopeScanRequestV1<'_>,
) -> Result<(), QueryExecutionScanErrorV1> {
  if request.selected_namespace_root != plan.selected_namespace_root()
    || request.publication_sequence != plan.publication_sequence()
    || request.query_path != plan.query_path()
    || request.scope_id.len() != plan.hash_algorithm().hash_length()
    || request.scope_id.iter().all(|byte| *byte == 0)
  {
    return Err(source_scan_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "query_candidate_scope_request",
      "authoritative executor scope request disagrees with its compiled plan",
    ));
  }
  Ok(())
}

fn validate_posting_root_receipt(
  plan: &CompiledRootAwareQueryPlanV1,
  scope_id: &[u8],
  candidate: &CompiledQueryIndexCandidateV1,
  receipt: &QueryCompletePostingRootReceiptV1,
) -> Result<(), QueryExecutionScanErrorV1> {
  let generation = candidate.selected_generation().ok_or_else(|| {
    source_scan_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "query_candidate_generation",
      "complete candidate driver has no selected generation",
    )
  })?;
  if !receipt.complete
    || receipt.selected_namespace_root != plan.selected_namespace_root()
    || receipt.publication_sequence != plan.publication_sequence()
    || receipt.scope_id != scope_id
    || receipt.index_id != candidate.index_id()
    || receipt.generation != generation.generation
    || receipt.generation_manifest_hash != generation.manifest_hash
    || receipt.coverage_source_root != generation.source_namespace_root
  {
    return Err(source_scan_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "query_candidate_posting_root_receipt",
      "Posting root receipt does not close over the exact planner-selected generation",
    ));
  }
  Ok(())
}

fn validate_scope_root_receipt(
  plan: &CompiledRootAwareQueryPlanV1,
  scope_id: &[u8],
  receipt: &QueryCompleteScopeRootReceiptV1,
) -> Result<(), QueryExecutionScanErrorV1> {
  if !receipt.complete
    || receipt.selected_namespace_root != plan.selected_namespace_root()
    || receipt.publication_sequence != plan.publication_sequence()
    || receipt.scope_id != scope_id
  {
    return Err(source_scan_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "query_candidate_scope_root_receipt",
      "ScopeOrdinal root receipt does not close over the exact selected scope",
    ));
  }
  Ok(())
}

fn validate_recheck_receipt(
  plan: &CompiledRootAwareQueryPlanV1,
  scope_id: &[u8],
  identity: &QueryCompleteCandidateIdentityV1,
  visitor: &RecheckVisitorV1<'_, '_>,
  receipt: &QueryCandidateRecheckReceiptV1,
) -> Result<(), QueryExecutionScanErrorV1> {
  if visitor.mismatch
    || visitor.visits != 1
    || !receipt.complete
    || receipt.document_count != 1
    || receipt.selected_namespace_root != plan.selected_namespace_root()
    || receipt.publication_sequence != plan.publication_sequence()
    || receipt.scope_id != scope_id
    || receipt.file_key != identity.file_key
    || receipt.indexed_revision != identity.record_revision
    || receipt.indexed_path != identity.path
  {
    return Err(source_scan_error(
      QueryExecutionSourceErrorClassV1::Corrupt,
      "query_candidate_authoritative_recheck",
      "selected-root candidate recheck is absent, stale, duplicated, incomplete, or dishonest",
    ));
  }
  Ok(())
}

fn validate_expression_workspace(
  expression: &CompiledQueryExpressionV1,
  limits: QueryCompleteCandidateLimitsV1,
) -> Result<(), QueryExecutionScanErrorV1> {
  let nodes = expression_node_count(expression).ok_or_else(|| {
    source_scan_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "query_candidate_expression_overflow",
      "candidate expression node count overflowed",
    )
  })?;
  let vectors = nodes.checked_add(2).ok_or_else(|| {
    source_scan_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "query_candidate_expression_overflow",
      "candidate expression vector count overflowed",
    )
  })?;
  let bytes = limits
    .maximum_candidate_documents
    .checked_mul(size_of::<u64>() as u64)
    .and_then(|value| value.checked_mul(vectors))
    .and_then(|value| value.checked_add(RESULT_FIXED_BYTES_V1))
    .ok_or_else(|| {
      source_scan_error(
        QueryExecutionSourceErrorClassV1::ResourceLimit,
        "query_candidate_expression_overflow",
        "candidate expression workspace overflowed",
      )
    })?;
  if bytes > limits.maximum_retained_bytes {
    return Err(source_scan_error(
      QueryExecutionSourceErrorClassV1::ResourceLimit,
      "query_candidate_expression_workspace",
      "candidate expression vectors cannot fit their admitted workspace",
    ));
  }
  Ok(())
}

fn expression_node_count(expression: &CompiledQueryExpressionV1) -> Option<u64> {
  match expression {
    CompiledQueryExpressionV1::Field(_) => Some(1),
    CompiledQueryExpressionV1::Not(child) => expression_node_count(child)?.checked_add(1),
    CompiledQueryExpressionV1::And(children) | CompiledQueryExpressionV1::Or(children) => {
      children.iter().try_fold(1u64, |count, child| count.checked_add(expression_node_count(child)?))
    }
  }
}

fn reserve_execution_workspace(memory: &MemoryCoordinator, bytes: u64) -> Result<MemoryReservation, QueryExecutionScanErrorV1> {
  memory.reserve(MemoryOwner::Query, bytes, AdmissionClass::Workload).map_err(|error| match error {
    MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. } => {
      source_scan_error(QueryExecutionSourceErrorClassV1::ResourceLimit, "query_candidate_expression_memory", error.to_string())
    }
    _ => source_scan_error(QueryExecutionSourceErrorClassV1::Internal, "query_candidate_memory_authority", error.to_string()),
  })
}

fn checked_stat_add(current: u64, amount: u64, code: &'static str) -> Result<u64, QueryExecutionScanErrorV1> {
  current
    .checked_add(amount)
    .ok_or_else(|| source_scan_error(QueryExecutionSourceErrorClassV1::ResourceLimit, code, "candidate execution statistic overflowed"))
}

fn map_candidate_scan_error(error: QueryCompleteCandidateErrorV1) -> QueryExecutionScanErrorV1 {
  let class = match error.class {
    QueryCompleteCandidateErrorClassV1::InvalidRequest | QueryCompleteCandidateErrorClassV1::CorruptSource => {
      QueryExecutionSourceErrorClassV1::Corrupt
    }
    QueryCompleteCandidateErrorClassV1::ResourceLimit => QueryExecutionSourceErrorClassV1::ResourceLimit,
    QueryCompleteCandidateErrorClassV1::HistoricalViewUnavailable => QueryExecutionSourceErrorClassV1::Unavailable,
    QueryCompleteCandidateErrorClassV1::Cancelled => QueryExecutionSourceErrorClassV1::Cancelled,
    QueryCompleteCandidateErrorClassV1::Internal => QueryExecutionSourceErrorClassV1::Internal,
  };
  source_scan_error(class, error.code, error.context)
}

fn source_scan_error(class: QueryExecutionSourceErrorClassV1, code: &'static str, context: impl Into<String>) -> QueryExecutionScanErrorV1 {
  QueryExecutionScanErrorV1::Source(QueryExecutionSourceErrorV1::new(class, code, context))
}

struct ScanBudgetV1<'a> {
  limits: QueryCompleteCandidateLimitsV1,
  cancellation: &'a CancellationToken,
  work_steps: u64,
  page_seeks: u64,
  posting_records: u64,
  scope_records: u64,
  identity_bytes: u64,
}

impl ScanBudgetV1<'_> {
  fn charge_work(&mut self, amount: u64) -> Result<(), QueryCompleteCandidateErrorV1> {
    require_not_cancelled(self.cancellation)?;
    self.work_steps = self
      .work_steps
      .checked_add(amount)
      .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_work_overflow", "candidate work counter overflowed"))?;
    if self.work_steps > self.limits.maximum_work_steps {
      return Err(QueryCompleteCandidateErrorV1::resource(
        "query_candidate_work_limit",
        "candidate execution exceeded its admitted work-step limit",
      ));
    }
    Ok(())
  }

  fn charge_page(&mut self) -> Result<(), QueryCompleteCandidateErrorV1> {
    self.charge_work(1)?;
    self.page_seeks = self
      .page_seeks
      .checked_add(1)
      .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_page_overflow", "candidate page counter overflowed"))?;
    if self.page_seeks > self.limits.maximum_page_seeks {
      return Err(QueryCompleteCandidateErrorV1::resource(
        "query_candidate_page_limit",
        "candidate execution exceeded its admitted page-seek limit",
      ));
    }
    Ok(())
  }

  fn charge_posting(&mut self) -> Result<(), QueryCompleteCandidateErrorV1> {
    self.charge_work(1)?;
    self.posting_records = self
      .posting_records
      .checked_add(1)
      .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_posting_overflow", "candidate posting counter overflowed"))?;
    if self.posting_records > self.limits.maximum_posting_records {
      return Err(QueryCompleteCandidateErrorV1::resource(
        "query_candidate_posting_limit",
        "candidate execution exceeded its admitted Posting-record limit",
      ));
    }
    Ok(())
  }

  fn charge_scope_record(&mut self) -> Result<(), QueryCompleteCandidateErrorV1> {
    self.charge_work(1)?;
    self.scope_records = self
      .scope_records
      .checked_add(1)
      .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_scope_record_overflow", "scope record counter overflowed"))?;
    Ok(())
  }

  fn charge_identity(&mut self, record: &super::index_record::ScopeDocumentRecordV1<'_>) -> Result<(), QueryCompleteCandidateErrorV1> {
    let bytes = (record.file_key.len() as u64)
      .checked_add(record.record_revision_hash.len() as u64)
      .and_then(|value| value.checked_add(record.path.len() as u64))
      .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_identity_overflow", "identity payload bytes overflowed"))?;
    self.identity_bytes = self
      .identity_bytes
      .checked_add(bytes)
      .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_identity_overflow", "identity payload bytes overflowed"))?;
    if self.identity_bytes > self.limits.maximum_identity_bytes {
      return Err(QueryCompleteCandidateErrorV1::resource(
        "query_candidate_identity_limit",
        "resolved candidate identities exceed their payload byte limit",
      ));
    }
    Ok(())
  }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PostingPointV1 {
  coordinate: u64,
  posting_key: Vec<u8>,
}

pub fn scan_complete_posting_ordinals_v1(
  request: QueryCompletePostingScanRequestV1<'_>,
  source: &mut dyn ArtifactCursorSourceV1,
) -> Result<QueryCompletePostingCandidatesV1, QueryCompleteCandidateErrorV1> {
  scan_posting_ordinals_v1(
    QueryPostingScanRequestV1 {
      hash_algorithm: request.hash_algorithm,
      coverage_namespace_root: request.selected_namespace_root,
      scope_id: request.scope_id,
      candidate: request.candidate,
      posting_root: request.posting_root,
      memory: request.memory,
      cancellation: request.cancellation,
      limits: request.limits,
      required_coverage: CompiledQueryCoverageV1::Complete,
    },
    source,
  )
}

pub fn scan_partial_posting_ordinals_v1(
  request: QueryPartialPostingScanRequestV1<'_>,
  source: &mut dyn ArtifactCursorSourceV1,
) -> Result<QueryCompletePostingCandidatesV1, QueryCompleteCandidateErrorV1> {
  scan_posting_ordinals_v1(
    QueryPostingScanRequestV1 {
      hash_algorithm: request.hash_algorithm,
      coverage_namespace_root: request.source_namespace_root,
      scope_id: request.scope_id,
      candidate: request.candidate,
      posting_root: request.posting_root,
      memory: request.memory,
      cancellation: request.cancellation,
      limits: request.limits,
      required_coverage: CompiledQueryCoverageV1::PartialExact,
    },
    source,
  )
}

#[derive(Clone, Copy)]
struct QueryPostingScanRequestV1<'a> {
  hash_algorithm: HashAlgorithm,
  coverage_namespace_root: &'a [u8],
  scope_id: &'a [u8],
  candidate: &'a CompiledQueryIndexCandidateV1,
  posting_root: Option<&'a QueryCandidateArtifactRootV1>,
  memory: &'a MemoryCoordinator,
  cancellation: &'a CancellationToken,
  limits: QueryCompleteCandidateLimitsV1,
  required_coverage: CompiledQueryCoverageV1,
}

fn scan_posting_ordinals_v1(
  request: QueryPostingScanRequestV1<'_>,
  source: &mut dyn ArtifactCursorSourceV1,
) -> Result<QueryCompletePostingCandidatesV1, QueryCompleteCandidateErrorV1> {
  require_not_cancelled(request.cancellation)?;
  validate_identity(request.hash_algorithm, request.coverage_namespace_root, "coverage NamespaceRoot")?;
  validate_identity(request.hash_algorithm, request.scope_id, "ScopeId")?;
  validate_candidate_coverage(request.coverage_namespace_root, request.candidate, request.required_coverage)?;
  let generation = request.candidate.selected_generation().ok_or_else(|| {
    QueryCompleteCandidateErrorV1::invalid("query_candidate_generation", "candidate scan requires a planner-selected generation")
  })?;
  validate_posting_workspace(request.limits)?;
  let mut memory = reserve_query_memory(request.memory, request.limits.maximum_retained_bytes)?;
  let mut budget = ScanBudgetV1 {
    limits: request.limits,
    cancellation: request.cancellation,
    work_steps: 0,
    page_seeks: 0,
    posting_records: 0,
    scope_records: 0,
    identity_bytes: 0,
  };
  let mut ordinals = Vec::new();
  if let Some(root) = request.posting_root {
    validate_root(request.hash_algorithm, root, request.candidate.index_id(), generation.generation)?;
    ordinals = scan_candidate_constraint(request.hash_algorithm, request.candidate, root, source, &mut budget)?;
  }
  if ordinals.len() as u64 > request.limits.maximum_candidate_documents {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_document_limit",
      "complete Posting scan produced more candidate documents than admitted",
    ));
  }
  let retained_bytes = posting_result_bytes(ordinals.capacity())?;
  if retained_bytes > request.limits.maximum_retained_bytes {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_retained_limit",
      "complete Posting result exceeds its retained-byte limit",
    ));
  }
  shrink_reservation(&mut memory, retained_bytes)?;
  require_not_cancelled(request.cancellation)?;
  Ok(QueryCompletePostingCandidatesV1 {
    document_ordinals: ordinals,
    examined_posting_records: budget.posting_records,
    examined_pages: budget.page_seeks,
    retained_bytes,
    _memory: memory,
  })
}

pub fn resolve_complete_scope_identities_v1(
  request: QueryCompleteScopeResolutionRequestV1<'_>,
  source: &mut dyn ArtifactCursorSourceV1,
) -> Result<QueryCompleteCandidateIdentitiesV1, QueryCompleteCandidateErrorV1> {
  require_not_cancelled(request.cancellation)?;
  validate_identity(request.hash_algorithm, request.selected_namespace_root, "selected NamespaceRoot")?;
  validate_identity(request.hash_algorithm, request.scope_id, "ScopeId")?;
  validate_selection(request.selection, request.limits.maximum_candidate_documents)?;
  validate_identity_workspace(request.limits)?;
  let mut memory = reserve_query_memory(request.memory, request.limits.maximum_retained_bytes)?;
  let mut budget = ScanBudgetV1 {
    limits: request.limits,
    cancellation: request.cancellation,
    work_steps: 0,
    page_seeks: 0,
    posting_records: 0,
    scope_records: 0,
    identity_bytes: 0,
  };
  let mut identities = Vec::new();
  match (request.scope_ordinal_root, request.selection) {
    (None, QueryScopeOrdinalSelectionV1::CandidateOrdinals([])) => {}
    (None, QueryScopeOrdinalSelectionV1::AllLive) => {}
    (None, QueryScopeOrdinalSelectionV1::CandidateOrdinals(_)) => {
      return Err(QueryCompleteCandidateErrorV1::corrupt(
        "query_candidate_scope_root_missing",
        "complete Posting candidates refer to an absent ScopeOrdinal root",
      ));
    }
    (Some(root), selection) => {
      validate_root(request.hash_algorithm, root, request.scope_id, root.generation)?;
      identities = match selection {
        QueryScopeOrdinalSelectionV1::CandidateOrdinals(ordinals) => {
          resolve_candidate_ordinals(request.hash_algorithm, root, ordinals, source, &mut budget)?
        }
        QueryScopeOrdinalSelectionV1::AllLive => {
          let identities = scan_scope_universe(request.hash_algorithm, root, source, &mut budget)?;
          if identities.len() as u64 != root.summary.live_count {
            return Err(QueryCompleteCandidateErrorV1::corrupt(
              "query_candidate_scope_live_count",
              "complete ScopeOrdinal scan disagrees with the selected root live count",
            ));
          }
          identities
        }
      };
    }
  }
  sort_and_validate_identities(&mut identities, request.limits.maximum_candidate_documents)?;
  let retained_bytes = identity_result_bytes(identities.capacity(), &identities)?;
  if retained_bytes > request.limits.maximum_retained_bytes || identity_payload_bytes(&identities)? > request.limits.maximum_identity_bytes
  {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_identity_limit",
      "resolved candidate identities exceed their retained or payload byte limit",
    ));
  }
  shrink_reservation(&mut memory, retained_bytes)?;
  require_not_cancelled(request.cancellation)?;
  Ok(QueryCompleteCandidateIdentitiesV1 {
    identities,
    examined_scope_records: budget.scope_records,
    examined_pages: budget.page_seeks,
    retained_bytes,
    _memory: memory,
  })
}

fn validate_candidate_coverage(
  coverage_namespace_root: &[u8],
  candidate: &CompiledQueryIndexCandidateV1,
  required_coverage: CompiledQueryCoverageV1,
) -> Result<(), QueryCompleteCandidateErrorV1> {
  let Some(generation) = candidate.selected_generation() else {
    return Err(QueryCompleteCandidateErrorV1::invalid(
      "query_candidate_generation",
      "candidate scan requires a planner-selected generation",
    ));
  };
  if candidate.coverage() != required_coverage
    || !matches!(required_coverage, CompiledQueryCoverageV1::Complete | CompiledQueryCoverageV1::PartialExact)
    || !candidate.proven_candidate_superset()
    || generation.source_namespace_root != coverage_namespace_root
  {
    return Err(QueryCompleteCandidateErrorV1::invalid(
      "query_candidate_coverage",
      "candidate does not provide the required planner-proven superset for the exact coverage root",
    ));
  }
  if matches!(candidate.value_match(), QueryValueMatchV1::AuthoritativeRecheck)
    || matches!(candidate.coordinate_constraint(), QueryCoordinateConstraintV1::FullScan)
  {
    return Err(QueryCompleteCandidateErrorV1::invalid("query_candidate_constraint", "candidate has no complete Posting constraint"));
  }
  Ok(())
}

fn scan_candidate_constraint(
  hash_algorithm: HashAlgorithm,
  candidate: &CompiledQueryIndexCandidateV1,
  root: &QueryCandidateArtifactRootV1,
  source: &mut dyn ArtifactCursorSourceV1,
  budget: &mut ScanBudgetV1<'_>,
) -> Result<Vec<u64>, QueryCompleteCandidateErrorV1> {
  match (candidate.value_match(), candidate.coordinate_constraint()) {
    (QueryValueMatchV1::AnyPosting, QueryCoordinateConstraintV1::Points(coordinates)) => {
      let points = candidate_points(candidate, coordinates)?;
      let mut union = Vec::new();
      for point in points {
        let one = scan_posting_point(hash_algorithm, root, &point, source, budget)?;
        union = merge_sorted_ordinals(union, one, SetOperationV1::Union, budget.limits.maximum_candidate_documents)?;
      }
      Ok(union)
    }
    (QueryValueMatchV1::AllPostings, QueryCoordinateConstraintV1::Points(coordinates)) => {
      let points = candidate_points(candidate, coordinates)?;
      let mut points = points.into_iter();
      let Some(first) = points.next() else {
        return Ok(Vec::new());
      };
      let mut intersection = scan_posting_point(hash_algorithm, root, &first, source, budget)?;
      for point in points {
        let one = scan_posting_point(hash_algorithm, root, &point, source, budget)?;
        intersection = merge_sorted_ordinals(intersection, one, SetOperationV1::Intersection, budget.limits.maximum_candidate_documents)?;
        if intersection.is_empty() {
          break;
        }
      }
      Ok(intersection)
    }
    (
      QueryValueMatchV1::OrderedRange,
      QueryCoordinateConstraintV1::InclusiveRange { start, end, widen_start_cell: _, widen_end_cell: _ },
    ) => scan_posting_range(hash_algorithm, root, *start, *end, source, budget),
    _ => Err(QueryCompleteCandidateErrorV1::invalid(
      "query_candidate_constraint",
      "planner candidate value-match and coordinate constraint disagree",
    )),
  }
}

fn candidate_points(
  candidate: &CompiledQueryIndexCandidateV1,
  constrained_coordinates: &[u64],
) -> Result<Vec<PostingPointV1>, QueryCompleteCandidateErrorV1> {
  let mut points = Vec::new();
  for literal in candidate.compiled_literals() {
    for posting in
      literal.compiled().postings.iter().filter(|posting| sorted_coordinates_contain(constrained_coordinates, posting.coordinate))
    {
      let mut key = Vec::new();
      key.try_reserve_exact(posting.posting_key.len()).map_err(|error| {
        QueryCompleteCandidateErrorV1::resource("query_candidate_point_allocation", format!("cannot reserve Posting key: {error}"))
      })?;
      key.extend_from_slice(&posting.posting_key);
      points.try_reserve(1).map_err(|error| {
        QueryCompleteCandidateErrorV1::resource("query_candidate_point_allocation", format!("cannot reserve Posting point: {error}"))
      })?;
      points.push(PostingPointV1 { coordinate: posting.coordinate, posting_key: key });
    }
  }
  points.sort_unstable();
  points.dedup();
  Ok(points)
}

fn sorted_coordinates_contain(coordinates: &[u64], target: u64) -> bool {
  let index = coordinates.partition_point(|coordinate| *coordinate < target);
  coordinates.get(index) == Some(&target)
}

fn scan_posting_point(
  hash_algorithm: HashAlgorithm,
  root: &QueryCandidateArtifactRootV1,
  point: &PostingPointV1,
  source: &mut dyn ArtifactCursorSourceV1,
  budget: &mut ScanBudgetV1<'_>,
) -> Result<Vec<u64>, QueryCompleteCandidateErrorV1> {
  let lower = posting_lower_bound(point.coordinate, &point.posting_key)?;
  let mut ordinals = scan_posting_pages(hash_algorithm, root, ArtifactPageSeekV1::OrderLowerBound(&lower), source, budget, |record| {
    match record.coordinate.cmp(&point.coordinate).then_with(|| record.posting_key.cmp(&point.posting_key)) {
      Ordering::Less => PostingDecisionV1::Skip,
      Ordering::Equal => PostingDecisionV1::Include,
      Ordering::Greater => PostingDecisionV1::Stop,
    }
  })?;
  ordinals.sort_unstable();
  ordinals.dedup();
  validate_ordinal_count(&ordinals, budget.limits.maximum_candidate_documents)?;
  Ok(ordinals)
}

fn scan_posting_range(
  hash_algorithm: HashAlgorithm,
  root: &QueryCandidateArtifactRootV1,
  start: u64,
  end: u64,
  source: &mut dyn ArtifactCursorSourceV1,
  budget: &mut ScanBudgetV1<'_>,
) -> Result<Vec<u64>, QueryCompleteCandidateErrorV1> {
  if start > end {
    return Err(QueryCompleteCandidateErrorV1::corrupt("query_candidate_range", "planner supplied a reversed Posting coordinate range"));
  }
  // Posting keys are nonempty byte strings; one zero byte is their canonical
  // lower bound and remains before every valid key sharing this coordinate.
  let lower = posting_lower_bound(start, &[0])?;
  let mut ordinals = scan_posting_pages(hash_algorithm, root, ArtifactPageSeekV1::OrderLowerBound(&lower), source, budget, |record| {
    if record.coordinate < start {
      PostingDecisionV1::Skip
    } else if record.coordinate <= end {
      PostingDecisionV1::Include
    } else {
      PostingDecisionV1::Stop
    }
  })?;
  ordinals.sort_unstable();
  ordinals.dedup();
  validate_ordinal_count(&ordinals, budget.limits.maximum_candidate_documents)?;
  Ok(ordinals)
}

enum PostingDecisionV1 {
  Skip,
  Include,
  Stop,
}

fn scan_posting_pages(
  hash_algorithm: HashAlgorithm,
  root: &QueryCandidateArtifactRootV1,
  initial_seek: ArtifactPageSeekV1<'_>,
  source: &mut dyn ArtifactCursorSourceV1,
  budget: &mut ScanBudgetV1<'_>,
  mut decide: impl FnMut(&super::index_page::PostingRecordV1<'_>) -> PostingDecisionV1,
) -> Result<Vec<u64>, QueryCompleteCandidateErrorV1> {
  let mut seek = initial_seek;
  let mut output = Vec::new();
  loop {
    budget.charge_page()?;
    let request = cursor_request(hash_algorithm, root, OrderedIndexRoleV1::Posting, seek, budget.limits.cursor);
    let Some(cursor) = load_artifact_page_cursor_v1(&request, source, &|| budget.cancellation.is_cancelled()).map_err(map_cursor_error)?
    else {
      break;
    };
    let page = decode_ordered_page(cursor.page(), hash_algorithm)
      .map_err(|error| QueryCompleteCandidateErrorV1::corrupt("query_candidate_posting_page", error.to_string()))?;
    let mut stop = false;
    for encoded in page.records.iter() {
      budget.charge_posting()?;
      let encoded = encoded.map_err(|error| QueryCompleteCandidateErrorV1::corrupt("query_candidate_posting_record", error.to_string()))?;
      let record = decode_posting_record(encoded.encoded)
        .map_err(|error| QueryCompleteCandidateErrorV1::corrupt("query_candidate_posting_record", error.to_string()))?;
      match decide(&record) {
        PostingDecisionV1::Skip => {}
        PostingDecisionV1::Include if !record.tombstone => push_ordinal(&mut output, record.document_ordinal, budget.limits)?,
        PostingDecisionV1::Include => {}
        PostingDecisionV1::Stop => {
          stop = true;
          break;
        }
      }
    }
    if stop {
      break;
    }
    let next = cursor
      .page_ordinal()
      .checked_add(1)
      .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_page_overflow", "Posting page ordinal overflowed"))?;
    if next >= cursor.root_page_count() {
      break;
    }
    seek = ArtifactPageSeekV1::PageOrdinal(next);
  }
  Ok(output)
}

fn resolve_candidate_ordinals(
  hash_algorithm: HashAlgorithm,
  root: &QueryCandidateArtifactRootV1,
  ordinals: &[u64],
  source: &mut dyn ArtifactCursorSourceV1,
  budget: &mut ScanBudgetV1<'_>,
) -> Result<Vec<QueryCompleteCandidateIdentityV1>, QueryCompleteCandidateErrorV1> {
  let mut identities = Vec::new();
  identities.try_reserve_exact(ordinals.len()).map_err(|error| {
    QueryCompleteCandidateErrorV1::resource("query_candidate_identity_allocation", format!("cannot reserve candidate identities: {error}"))
  })?;
  let Some(first_ordinal) = ordinals.first() else {
    return Ok(identities);
  };
  let first_key = first_ordinal.to_le_bytes();
  let mut seek = ArtifactPageSeekV1::OrderLowerBound(&first_key);
  let mut candidate_index = 0usize;
  loop {
    budget.charge_page()?;
    let request = cursor_request(hash_algorithm, root, OrderedIndexRoleV1::ScopeOrdinal, seek, budget.limits.cursor);
    let cursor = load_artifact_page_cursor_v1(&request, source, &|| budget.cancellation.is_cancelled())
      .map_err(map_cursor_error)?
      .ok_or_else(|| {
        QueryCompleteCandidateErrorV1::corrupt(
          "query_candidate_scope_ordinal_missing",
          format!("complete Posting candidate ordinal {} is absent from ScopeOrdinal authority", ordinals[candidate_index]),
        )
      })?;
    let page = decode_ordered_page(cursor.page(), hash_algorithm)
      .map_err(|error| QueryCompleteCandidateErrorV1::corrupt("query_candidate_scope_page", error.to_string()))?;
    for record in page.records.iter() {
      budget.charge_scope_record()?;
      let record = record.map_err(|error| QueryCompleteCandidateErrorV1::corrupt("query_candidate_scope_record", error.to_string()))?;
      let ordinal = ordinals[candidate_index];
      if record.document_ordinal < ordinal {
        continue;
      }
      if record.document_ordinal > ordinal {
        return Err(QueryCompleteCandidateErrorV1::corrupt(
          "query_candidate_scope_ordinal_missing",
          format!("complete Posting candidate ordinal {ordinal} is absent before the next ScopeOrdinal record"),
        ));
      }
      let decoded = decode_scope_document_record(record.encoded, hash_algorithm)
        .map_err(|error| QueryCompleteCandidateErrorV1::corrupt("query_candidate_scope_record", error.to_string()))?;
      if decoded.tombstone {
        return Err(QueryCompleteCandidateErrorV1::corrupt(
          "query_candidate_scope_ordinal_tombstone",
          format!("complete Posting candidate ordinal {ordinal} resolves to a ScopeOrdinal tombstone"),
        ));
      }
      budget.charge_identity(&decoded)?;
      identities.push(copy_identity(&decoded)?);
      candidate_index += 1;
      if candidate_index == ordinals.len() {
        return Ok(identities);
      }
    }
    let next = cursor
      .page_ordinal()
      .checked_add(1)
      .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_page_overflow", "ScopeOrdinal page ordinal overflowed"))?;
    if next >= cursor.root_page_count() {
      return Err(QueryCompleteCandidateErrorV1::corrupt(
        "query_candidate_scope_ordinal_missing",
        format!("complete Posting candidate ordinal {} is absent after the final ScopeOrdinal page", ordinals[candidate_index]),
      ));
    }
    seek = ArtifactPageSeekV1::PageOrdinal(next);
  }
}

fn scan_scope_universe(
  hash_algorithm: HashAlgorithm,
  root: &QueryCandidateArtifactRootV1,
  source: &mut dyn ArtifactCursorSourceV1,
  budget: &mut ScanBudgetV1<'_>,
) -> Result<Vec<QueryCompleteCandidateIdentityV1>, QueryCompleteCandidateErrorV1> {
  let mut identities = Vec::new();
  for page_ordinal in 0..root.summary.page_count {
    budget.charge_page()?;
    let request = cursor_request(
      hash_algorithm,
      root,
      OrderedIndexRoleV1::ScopeOrdinal,
      ArtifactPageSeekV1::PageOrdinal(page_ordinal),
      budget.limits.cursor,
    );
    let cursor = load_artifact_page_cursor_v1(&request, source, &|| budget.cancellation.is_cancelled())
      .map_err(map_cursor_error)?
      .ok_or_else(|| {
        QueryCompleteCandidateErrorV1::corrupt(
          "query_candidate_scope_page_missing",
          format!("ScopeOrdinal page ordinal {page_ordinal} is absent from a nonempty root"),
        )
      })?;
    let page = decode_ordered_page(cursor.page(), hash_algorithm)
      .map_err(|error| QueryCompleteCandidateErrorV1::corrupt("query_candidate_scope_page", error.to_string()))?;
    for record in page.records.iter() {
      budget.charge_scope_record()?;
      let record = record.map_err(|error| QueryCompleteCandidateErrorV1::corrupt("query_candidate_scope_record", error.to_string()))?;
      let decoded = decode_scope_document_record(record.encoded, hash_algorithm)
        .map_err(|error| QueryCompleteCandidateErrorV1::corrupt("query_candidate_scope_record", error.to_string()))?;
      if !decoded.tombstone {
        if identities.len() as u64 >= budget.limits.maximum_candidate_documents {
          return Err(QueryCompleteCandidateErrorV1::resource(
            "query_candidate_document_limit",
            "ScopeOrdinal universe exceeds the admitted candidate-document limit",
          ));
        }
        identities.try_reserve(1).map_err(|error| {
          QueryCompleteCandidateErrorV1::resource(
            "query_candidate_identity_allocation",
            format!("cannot grow candidate identities: {error}"),
          )
        })?;
        budget.charge_identity(&decoded)?;
        identities.push(copy_identity(&decoded)?);
      }
    }
  }
  Ok(identities)
}

fn copy_identity(
  record: &super::index_record::ScopeDocumentRecordV1<'_>,
) -> Result<QueryCompleteCandidateIdentityV1, QueryCompleteCandidateErrorV1> {
  let mut file_key = Vec::new();
  file_key.try_reserve_exact(record.file_key.len()).map_err(|error| {
    QueryCompleteCandidateErrorV1::resource("query_candidate_identity_allocation", format!("cannot reserve FileKey: {error}"))
  })?;
  file_key.extend_from_slice(record.file_key);
  let mut record_revision = Vec::new();
  record_revision.try_reserve_exact(record.record_revision_hash.len()).map_err(|error| {
    QueryCompleteCandidateErrorV1::resource("query_candidate_identity_allocation", format!("cannot reserve RecordRevision: {error}"))
  })?;
  record_revision.extend_from_slice(record.record_revision_hash);
  let mut path = String::new();
  path.try_reserve_exact(record.path.len()).map_err(|error| {
    QueryCompleteCandidateErrorV1::resource("query_candidate_identity_allocation", format!("cannot reserve path: {error}"))
  })?;
  path.push_str(record.path);
  Ok(QueryCompleteCandidateIdentityV1 { document_ordinal: record.document_ordinal, file_key, record_revision, path })
}

fn sort_and_validate_identities(
  identities: &mut [QueryCompleteCandidateIdentityV1],
  maximum: u64,
) -> Result<(), QueryCompleteCandidateErrorV1> {
  if identities.len() as u64 > maximum {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_document_limit",
      "resolved ScopeOrdinal identities exceed the admitted candidate-document limit",
    ));
  }
  identities.sort_unstable_by(|left, right| left.file_key.cmp(&right.file_key));
  if identities.windows(2).any(|pair| pair[0].file_key == pair[1].file_key) {
    return Err(QueryCompleteCandidateErrorV1::corrupt(
      "query_candidate_scope_file_key_duplicate",
      "selected ScopeOrdinal identities repeat a live FileKey",
    ));
  }
  Ok(())
}

fn validate_selection(selection: QueryScopeOrdinalSelectionV1<'_>, maximum: u64) -> Result<(), QueryCompleteCandidateErrorV1> {
  if let QueryScopeOrdinalSelectionV1::CandidateOrdinals(ordinals) = selection {
    if ordinals.len() as u64 > maximum || ordinals.contains(&0) {
      return Err(QueryCompleteCandidateErrorV1::invalid(
        "query_candidate_ordinal_selection",
        "candidate ordinal selection is zero or exceeds its admitted count",
      ));
    }
    if ordinals.windows(2).any(|pair| pair[0] >= pair[1]) {
      return Err(QueryCompleteCandidateErrorV1::invalid(
        "query_candidate_ordinal_order",
        "candidate ordinals must be strict ascending unique values",
      ));
    }
  }
  Ok(())
}

fn validate_root(
  hash_algorithm: HashAlgorithm,
  root: &QueryCandidateArtifactRootV1,
  expected_owner: &[u8],
  expected_generation: u64,
) -> Result<(), QueryCompleteCandidateErrorV1> {
  validate_identity(hash_algorithm, &root.root_key, "artifact root key")?;
  validate_identity(hash_algorithm, &root.owner_id, "artifact owner")?;
  if root.owner_id != expected_owner || root.generation != expected_generation {
    return Err(QueryCompleteCandidateErrorV1::corrupt(
      "query_candidate_artifact_authority",
      "candidate artifact root owner or generation disagrees with selected authority",
    ));
  }
  Ok(())
}

fn validate_identity(hash_algorithm: HashAlgorithm, value: &[u8], label: &'static str) -> Result<(), QueryCompleteCandidateErrorV1> {
  if value.len() != hash_algorithm.hash_length() || value.iter().all(|byte| *byte == 0) {
    return Err(QueryCompleteCandidateErrorV1::invalid("query_candidate_identity", format!("{label} is not one nonzero database hash")));
  }
  Ok(())
}

fn cursor_request<'a>(
  hash_algorithm: HashAlgorithm,
  root: &'a QueryCandidateArtifactRootV1,
  role: OrderedIndexRoleV1,
  seek: ArtifactPageSeekV1<'a>,
  limits: ArtifactPageCursorLimitsV1,
) -> ArtifactPageCursorRequestV1<'a> {
  ArtifactPageCursorRequestV1 {
    root: ArtifactPageCursorRootV1 {
      hash_algorithm,
      root_key: &root.root_key,
      owner_id: &root.owner_id,
      role,
      maximum_generation: root.generation,
      expected_summary: Some(root.summary),
    },
    seek,
    neighbors: ArtifactPageNeighborModeV1::Both,
    limits,
  }
}

fn posting_lower_bound(coordinate: u64, posting_key: &[u8]) -> Result<Vec<u8>, QueryCompleteCandidateErrorV1> {
  let length = 24usize.checked_add(posting_key.len()).ok_or_else(|| {
    QueryCompleteCandidateErrorV1::resource("query_candidate_posting_key_length", "Posting lower-bound length overflowed")
  })?;
  let mut value = Vec::new();
  value.try_reserve_exact(length).map_err(|error| {
    QueryCompleteCandidateErrorV1::resource(
      "query_candidate_posting_key_allocation",
      format!("cannot reserve Posting lower bound: {error}"),
    )
  })?;
  value.resize(length, 0);
  value[0..8].copy_from_slice(&coordinate.to_le_bytes());
  value[8..8 + posting_key.len()].copy_from_slice(posting_key);
  value[8 + posting_key.len()..16 + posting_key.len()].copy_from_slice(&1u64.to_le_bytes());
  Ok(value)
}

fn push_ordinal(output: &mut Vec<u64>, ordinal: u64, limits: QueryCompleteCandidateLimitsV1) -> Result<(), QueryCompleteCandidateErrorV1> {
  if ordinal == 0 {
    return Err(QueryCompleteCandidateErrorV1::corrupt("query_candidate_posting_ordinal", "Posting record contains zero document ordinal"));
  }
  if output.len() as u64 >= limits.maximum_posting_records {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_posting_limit",
      "candidate Posting accumulation exceeds its admitted record count",
    ));
  }
  output.try_reserve(1).map_err(|error| {
    QueryCompleteCandidateErrorV1::resource("query_candidate_ordinal_allocation", format!("cannot grow candidate ordinals: {error}"))
  })?;
  output.push(ordinal);
  let bytes = (output.capacity() as u64)
    .checked_mul(size_of::<u64>() as u64)
    .and_then(|bytes| bytes.checked_add(RESULT_FIXED_BYTES_V1))
    .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_retained_overflow", "candidate ordinal bytes overflowed"))?;
  if bytes > limits.maximum_retained_bytes {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_retained_limit",
      "candidate ordinal accumulation exceeds its admitted retained bytes",
    ));
  }
  Ok(())
}

enum SetOperationV1 {
  Union,
  Intersection,
}

fn merge_sorted_ordinals(
  left: Vec<u64>,
  right: Vec<u64>,
  operation: SetOperationV1,
  maximum: u64,
) -> Result<Vec<u64>, QueryCompleteCandidateErrorV1> {
  let capacity = match operation {
    SetOperationV1::Union => left.len().checked_add(right.len()),
    SetOperationV1::Intersection => Some(left.len().min(right.len())),
  }
  .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_set_overflow", "candidate set capacity overflowed"))?;
  if capacity as u64 > maximum && matches!(operation, SetOperationV1::Intersection) {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_document_limit",
      "candidate set intersection exceeds its admitted count",
    ));
  }
  let mut merged = Vec::new();
  merged.try_reserve_exact(capacity.min(maximum as usize)).map_err(|error| {
    QueryCompleteCandidateErrorV1::resource("query_candidate_set_allocation", format!("cannot reserve candidate set: {error}"))
  })?;
  let (mut left_index, mut right_index) = (0usize, 0usize);
  while left_index < left.len() && right_index < right.len() {
    match left[left_index].cmp(&right[right_index]) {
      Ordering::Less => {
        if matches!(operation, SetOperationV1::Union) {
          push_merged(&mut merged, left[left_index], maximum)?;
        }
        left_index += 1;
      }
      Ordering::Greater => {
        if matches!(operation, SetOperationV1::Union) {
          push_merged(&mut merged, right[right_index], maximum)?;
        }
        right_index += 1;
      }
      Ordering::Equal => {
        push_merged(&mut merged, left[left_index], maximum)?;
        left_index += 1;
        right_index += 1;
      }
    }
  }
  if matches!(operation, SetOperationV1::Union) {
    for value in &left[left_index..] {
      push_merged(&mut merged, *value, maximum)?;
    }
    for value in &right[right_index..] {
      push_merged(&mut merged, *value, maximum)?;
    }
  }
  Ok(merged)
}

fn push_merged(output: &mut Vec<u64>, value: u64, maximum: u64) -> Result<(), QueryCompleteCandidateErrorV1> {
  if output.last() == Some(&value) {
    return Ok(());
  }
  if output.len() as u64 >= maximum {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_document_limit",
      "candidate set exceeds its admitted document count",
    ));
  }
  output.push(value);
  Ok(())
}

fn validate_ordinal_count(ordinals: &[u64], maximum: u64) -> Result<(), QueryCompleteCandidateErrorV1> {
  if ordinals.len() as u64 > maximum {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_document_limit",
      "candidate Posting set exceeds its admitted document count",
    ));
  }
  Ok(())
}

fn validate_posting_workspace(limits: QueryCompleteCandidateLimitsV1) -> Result<(), QueryCompleteCandidateErrorV1> {
  let range_bytes = limits
    .maximum_posting_records
    .checked_mul(size_of::<u64>() as u64)
    .and_then(|bytes| bytes.checked_add(RESULT_FIXED_BYTES_V1))
    .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_workspace_overflow", "Posting workspace overflowed"))?;
  let set_bytes = limits
    .maximum_candidate_documents
    .checked_mul(size_of::<u64>() as u64)
    .and_then(|bytes| bytes.checked_mul(3))
    .and_then(|bytes| bytes.checked_add(RESULT_FIXED_BYTES_V1))
    .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_workspace_overflow", "candidate set workspace overflowed"))?;
  if range_bytes.max(set_bytes) > limits.maximum_retained_bytes {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_workspace_limit",
      "candidate Posting/set workspace cannot fit its retained-byte reservation",
    ));
  }
  Ok(())
}

fn validate_identity_workspace(limits: QueryCompleteCandidateLimitsV1) -> Result<(), QueryCompleteCandidateErrorV1> {
  let bytes = limits
    .maximum_candidate_documents
    .checked_mul(size_of::<QueryCompleteCandidateIdentityV1>() as u64)
    .and_then(|value| value.checked_add(limits.maximum_identity_bytes))
    .and_then(|value| value.checked_add(RESULT_FIXED_BYTES_V1))
    .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_workspace_overflow", "identity workspace overflowed"))?;
  if bytes > limits.maximum_retained_bytes {
    return Err(QueryCompleteCandidateErrorV1::resource(
      "query_candidate_workspace_limit",
      "candidate identity workspace cannot fit its retained-byte reservation",
    ));
  }
  Ok(())
}

fn posting_result_bytes(capacity: usize) -> Result<u64, QueryCompleteCandidateErrorV1> {
  (capacity as u64)
    .checked_mul(size_of::<u64>() as u64)
    .and_then(|bytes| bytes.checked_add(RESULT_FIXED_BYTES_V1))
    .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_retained_overflow", "Posting result bytes overflowed"))
}

fn identity_result_bytes(capacity: usize, identities: &[QueryCompleteCandidateIdentityV1]) -> Result<u64, QueryCompleteCandidateErrorV1> {
  let mut bytes = RESULT_FIXED_BYTES_V1
    .checked_add((capacity as u64).saturating_mul(size_of::<QueryCompleteCandidateIdentityV1>() as u64))
    .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_retained_overflow", "identity result bytes overflowed"))?;
  for identity in identities {
    bytes = bytes
      .checked_add(identity.file_key.capacity() as u64)
      .and_then(|value| value.checked_add(identity.record_revision.capacity() as u64))
      .and_then(|value| value.checked_add(identity.path.capacity() as u64))
      .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_retained_overflow", "identity result bytes overflowed"))?;
  }
  Ok(bytes)
}

fn identity_payload_bytes(identities: &[QueryCompleteCandidateIdentityV1]) -> Result<u64, QueryCompleteCandidateErrorV1> {
  identities.iter().try_fold(0u64, |bytes, identity| {
    bytes
      .checked_add(identity.file_key.len() as u64)
      .and_then(|value| value.checked_add(identity.record_revision.len() as u64))
      .and_then(|value| value.checked_add(identity.path.len() as u64))
      .ok_or_else(|| QueryCompleteCandidateErrorV1::resource("query_candidate_identity_overflow", "identity payload bytes overflowed"))
  })
}

fn reserve_query_memory(memory: &MemoryCoordinator, bytes: u64) -> Result<MemoryReservation, QueryCompleteCandidateErrorV1> {
  memory.reserve(MemoryOwner::Query, bytes, AdmissionClass::Workload).map_err(|error| match error {
    MemoryCoordinatorError::HardLimitExceeded { .. }
    | MemoryCoordinatorError::EmergencyReserveExceeded { .. }
    | MemoryCoordinatorError::SoftPressureDeferred { .. } => {
      QueryCompleteCandidateErrorV1::resource("query_candidate_memory_pressure", error.to_string())
    }
    _ => QueryCompleteCandidateErrorV1::internal("query_candidate_memory_authority", error.to_string()),
  })
}

fn shrink_reservation(reservation: &mut MemoryReservation, retained_bytes: u64) -> Result<(), QueryCompleteCandidateErrorV1> {
  let release = reservation.bytes().checked_sub(retained_bytes).ok_or_else(|| {
    QueryCompleteCandidateErrorV1::internal("query_candidate_memory_accounting", "retained bytes exceed their reservation")
  })?;
  reservation.shrink(release).map_err(|error| {
    QueryCompleteCandidateErrorV1::internal("query_candidate_memory_accounting", format!("cannot shrink query reservation: {error}"))
  })
}

fn require_not_cancelled(cancellation: &CancellationToken) -> Result<(), QueryCompleteCandidateErrorV1> {
  if cancellation.is_cancelled() {
    Err(QueryCompleteCandidateErrorV1::cancelled())
  } else {
    Ok(())
  }
}

fn map_cursor_error(error: ArtifactPageCursorErrorV1) -> QueryCompleteCandidateErrorV1 {
  let code = error.code();
  let context = error.to_string();
  match error {
    ArtifactPageCursorErrorV1::Cancelled => QueryCompleteCandidateErrorV1::cancelled(),
    ArtifactPageCursorErrorV1::SourcePressure(_) | ArtifactPageCursorErrorV1::Allocation(_) => {
      QueryCompleteCandidateErrorV1::resource(code, context)
    }
    ArtifactPageCursorErrorV1::SourceOperational(_) => QueryCompleteCandidateErrorV1::unavailable(code, context),
    ArtifactPageCursorErrorV1::InvalidLimits(_) => QueryCompleteCandidateErrorV1::invalid(code, context),
    ArtifactPageCursorErrorV1::MissingArtifact { .. }
    | ArtifactPageCursorErrorV1::SourceCorrupt(_)
    | ArtifactPageCursorErrorV1::Malformed(_) => QueryCompleteCandidateErrorV1::corrupt(code, context),
  }
}
