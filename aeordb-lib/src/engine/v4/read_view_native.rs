use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::mem::size_of;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::engine::btree::{BTREE_MAX_INTERNAL_KEYS, BTREE_MAX_LEAF_ENTRIES, BTreeNode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::file_record::FileRecord;
use crate::engine::memory_coordinator::{AdmissionClass, MemoryCoordinator, MemoryOwner, MemoryReservation};
use crate::engine::permission_resolver::{evaluate_ordered_path_permissions, normalize_permission_path};
use crate::engine::permissions::{PathPermissions, PermissionLink};
use crate::engine::path_utils::normalize_path;
use crate::engine::{CompressionAlgorithm, EntryType, HashAlgorithm};

use super::database_header::SelectedDatabaseHeaderV4;
use super::entity::EntryTypeV4;
use super::first_authority::{
  FirstAuthorityPublicationErrorV1, LoadedImmutableEntityV1, RootLifecyclePointReadErrorV1, V4FirstAuthorityPublisher,
};
use super::hash::digest_parts;
use super::index_artifact_cursor::{ArtifactPageCursorLimitsV1, ArtifactPageNeighborModeV1, ArtifactPageSeekV1};
use super::index_artifact_native::{
  NativeSelectedArtifactCursorErrorClassV1, NativeSelectedArtifactCursorErrorV1, NativeSelectedArtifactLoadRequestV1,
  NativeSelectedArtifactPageCursorV1, NativeSelectedPostingSeekLoadRequestV1, load_native_selected_artifact_page_cursor_v1,
  load_native_selected_posting_seek_v1,
};
pub use super::index_artifact_native::{
  NativeSelectedNvtFallbackReasonV1, NativeSelectedNvtFallbackV1, NativeSelectedPostingPageV1, NativeSelectedPostingSeekSourceV1,
};
use super::index_coverage_planner::IndexCoverageGenerationHealthV1;
use super::index_coverage_registry::{
  IndexCoverageNvtDescriptorV1, IndexCoverageNvtStatusV1, IndexCoverageRegistryOwnerKindV1, IndexCoverageRegistrySelectionV1,
  IndexCoverageRegistryGenerationV1, IndexCoverageRegistrySnapshotV1, field_definition_fingerprint, field_dependency_fingerprint,
};
use super::index_native_parser::{
  NativeIndexParserBodySourceV1, NativeIndexParserBodyV1, NativeIndexParserExecutorV1, native_parser_body_reservation_bytes_v1,
};
use super::index_producer_collector::{IndexParserExecutionErrorV1, IndexParserExecutorV1};
use super::index_source::PluginMapperExecutorV1;
use super::namespace::{NamespaceTreeEdgeV0, SemanticAvailabilityV1};
use super::query_planner::{
  QueryPlanningCoverageGenerationV1, QueryPlanningErrorClassV1, QueryPlanningIndexCandidateV1, QueryPlanningIndexEstimatesV1,
  QueryPlanningScopeV1, RootAwareQueryFieldCatalogV1, canonical_query_field_name_v1,
};
use super::read_view::{
  LoadedReadAuthorityV1, ReadViewAuthoritySourceV1, ReadViewAuthorizationFailureV1, ReadViewLifecycleErrorV1, ReadViewSourceErrorV1,
  ResolvedReadViewV1, RootLifecycleObservationV1,
};
use super::read_view_authorization::{
  PathAuthorizationDecisionV1, ResolvedPathAuthorizationV1, SelectedRootPermissionRequestV1, SelectedRootPermissionSourceV1,
};
use super::root_authority::ImmutableNamespaceAuthorityV1;
use super::scope::{ScopeDefinitionV1, decode_scope_definition, scope_matches_path, scope_owner_overlaps_query_path};
use super::semantic_catalog::{
  SemanticCatalogObjectSourceV1, SemanticCatalogReadErrorClassV1, SemanticCatalogReadErrorV1, SemanticCatalogReaderV1,
  SemanticCatalogTraversalBoundsV1, SemanticCatalogWalkStatsV1, validate_semantic_definition_identity_v1,
};
use super::source_evaluator::{
  AuthoritativeSourceDocumentV1, AuthoritativeSourceEvaluationErrorV1, AuthoritativeSourceEvaluationV1, AuthoritativeSourceEvaluatorV1,
  AuthoritativeSourceMemoryPolicyV1,
};
use super::value_store::decode_value_store_definition;
use super::field_definition::decode_field_index_definition;
use super::index_page::OrderedIndexRoleV1;

// The frozen namespace authority permits a 48 MiB tree entity. Reserve enough
// for that entity, its decoded form, transient validation copies, and the
// smaller admission/control entities before any authority allocation occurs.
const AUTHORITY_PEAK_RESERVATION_BYTES: u64 = 128 * 1024 * 1024;
const AUTHORITY_RETAINED_BASE_BYTES: u64 = 16 * 1024;
const PERMISSION_WORKSPACE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DIRECTORY_ENTITY_BYTES: usize = 48 * 1024 * 1024;
const MAX_FILE_RECORD_ENTITY_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHUNK_ENTITY_BYTES: usize = 2 * 1024 * 1024;
const MAX_PERMISSION_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_PERMISSION_DOCUMENT_CHUNKS: usize = 64;
const MAX_FLAT_DIRECTORY_ENTRIES: usize = 256;
const MAX_BTREE_DEPTH: usize = 128;
const MAX_BTREE_SCAN_NODES: usize = 100_000;
const MAX_DESCENDANT_DEPTH: usize = 10;
const MAX_DESCENDANT_PERMISSION_FILES: usize = 1_000;
const MAX_DESCENDANT_DIRECTORIES: usize = 100_000;
const SELECTED_NAMESPACE_MAXIMUM_PAGE_DOCUMENTS: usize = 4_096;
const SELECTED_NAMESPACE_MAXIMUM_PAGE_BYTES: u64 = 128 * 1024 * 1024;
const SELECTED_NAMESPACE_MAXIMUM_PATH_BYTES: usize = u16::MAX as usize;
const SELECTED_NAMESPACE_MAXIMUM_DEPTH: usize = 128;
const SELECTED_NAMESPACE_MAXIMUM_WORK_STEPS: u64 = 10_000_000;
const SELECTED_NAMESPACE_MAXIMUM_IDENTITY_DOCUMENTS: u64 = 10_000_000;
const SELECTED_NAMESPACE_WORKSPACE_BYTES: u64 = 32 * 1024 * 1024;
const SELECTED_SEMANTIC_MAXIMUM_REQUESTED_FIELDS: usize = 1_024;
const SELECTED_SEMANTIC_MAXIMUM_SCOPES: usize = 4_096;
const SELECTED_SEMANTIC_MAXIMUM_VALUE_STORES: usize = 262_144;
const SELECTED_SEMANTIC_MAXIMUM_FIELD_INDEXES: usize = 262_144;
const SELECTED_SEMANTIC_MAXIMUM_CATALOG_ITEMS: u64 = 1_000_000;
const SELECTED_SEMANTIC_MAXIMUM_WORK_STEPS: u64 = 10_000_000;
const SELECTED_SEMANTIC_MAXIMUM_DEFINITION_BYTES: u64 = 64 * 1024 * 1024;
const SELECTED_SEMANTIC_MAXIMUM_RETAINED_BYTES: u64 = 128 * 1024 * 1024;
const SELECTED_SEMANTIC_WORKSPACE_BYTES: u64 = 32 * 1024 * 1024;
const SELECTED_SOURCE_MAXIMUM_RETAINED_BYTES: u64 = 256 * 1024 * 1024;
const SELECTED_SOURCE_RECEIPT_FIXED_BYTES: u64 = 4 * 1024;
const SELECTED_SOURCE_PREPARED_FIXED_BYTES: u64 = 4 * 1024;

/// One production source for captured v4 authority, lifecycle, and selected
/// permission reads. Callers must use the same process memory coordinator for
/// this source and its `RootReadPinCoordinatorV1`.
#[derive(Clone)]
pub struct NativeReadViewSourceV1 {
  publisher: Arc<V4FirstAuthorityPublisher>,
  memory: Arc<MemoryCoordinator>,
  current_configured_grace_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSelectedNamespaceLimitsV1 {
  maximum_page_documents: usize,
  maximum_page_bytes: u64,
  maximum_path_bytes: usize,
  maximum_depth: usize,
  maximum_work_steps: u64,
  maximum_identity_documents: u64,
}

impl NativeSelectedNamespaceLimitsV1 {
  pub fn new(
    maximum_page_documents: usize,
    maximum_page_bytes: u64,
    maximum_path_bytes: usize,
    maximum_depth: usize,
    maximum_work_steps: u64,
    maximum_identity_documents: u64,
  ) -> Result<Self, NativeSelectedNamespaceReadErrorV1> {
    let minimum_row_slot_bytes = maximum_page_documents.checked_mul(size_of::<NativeSelectedNamespaceFileRowV1>());
    if maximum_page_documents == 0
      || maximum_page_documents > SELECTED_NAMESPACE_MAXIMUM_PAGE_DOCUMENTS
      || maximum_page_bytes == 0
      || maximum_page_bytes > SELECTED_NAMESPACE_MAXIMUM_PAGE_BYTES
      || minimum_row_slot_bytes.is_none_or(|bytes| bytes as u64 > maximum_page_bytes)
      || maximum_path_bytes == 0
      || maximum_path_bytes > SELECTED_NAMESPACE_MAXIMUM_PATH_BYTES
      || maximum_depth == 0
      || maximum_depth > SELECTED_NAMESPACE_MAXIMUM_DEPTH
      || maximum_work_steps == 0
      || maximum_work_steps > SELECTED_NAMESPACE_MAXIMUM_WORK_STEPS
      || maximum_identity_documents == 0
      || maximum_identity_documents > SELECTED_NAMESPACE_MAXIMUM_IDENTITY_DOCUMENTS
    {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_namespace_limits",
        "selected namespace limits must be nonzero, fit their retained row slots, and remain within frozen protocol maxima",
      ));
    }
    Ok(Self {
      maximum_page_documents,
      maximum_page_bytes,
      maximum_path_bytes,
      maximum_depth,
      maximum_work_steps,
      maximum_identity_documents,
    })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSelectedSemanticCountLimitsV1 {
  maximum_requested_fields: usize,
  maximum_scopes: usize,
  maximum_value_stores: usize,
  maximum_field_indexes: usize,
  maximum_catalog_items: u64,
  maximum_work_steps: u64,
}

impl NativeSelectedSemanticCountLimitsV1 {
  pub fn new(
    maximum_requested_fields: usize,
    maximum_scopes: usize,
    maximum_value_stores: usize,
    maximum_field_indexes: usize,
    maximum_catalog_items: u64,
    maximum_work_steps: u64,
  ) -> Result<Self, NativeSelectedNamespaceReadErrorV1> {
    if maximum_requested_fields == 0
      || maximum_requested_fields > SELECTED_SEMANTIC_MAXIMUM_REQUESTED_FIELDS
      || maximum_scopes == 0
      || maximum_scopes > SELECTED_SEMANTIC_MAXIMUM_SCOPES
      || maximum_value_stores == 0
      || maximum_value_stores > SELECTED_SEMANTIC_MAXIMUM_VALUE_STORES
      || maximum_field_indexes == 0
      || maximum_field_indexes > SELECTED_SEMANTIC_MAXIMUM_FIELD_INDEXES
      || maximum_catalog_items == 0
      || maximum_catalog_items > SELECTED_SEMANTIC_MAXIMUM_CATALOG_ITEMS
      || maximum_work_steps == 0
      || maximum_work_steps > SELECTED_SEMANTIC_MAXIMUM_WORK_STEPS
    {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_semantic_count_limits",
        "selected semantic count limits must be nonzero and remain within protocol maxima",
      ));
    }
    Ok(Self {
      maximum_requested_fields,
      maximum_scopes,
      maximum_value_stores,
      maximum_field_indexes,
      maximum_catalog_items,
      maximum_work_steps,
    })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSelectedSemanticByteLimitsV1 {
  maximum_definition_bytes: u64,
  maximum_retained_bytes: u64,
}

impl NativeSelectedSemanticByteLimitsV1 {
  pub fn new(maximum_definition_bytes: u64, maximum_retained_bytes: u64) -> Result<Self, NativeSelectedNamespaceReadErrorV1> {
    if maximum_definition_bytes == 0
      || maximum_definition_bytes > SELECTED_SEMANTIC_MAXIMUM_DEFINITION_BYTES
      || maximum_retained_bytes == 0
      || maximum_retained_bytes > SELECTED_SEMANTIC_MAXIMUM_RETAINED_BYTES
      || maximum_retained_bytes < maximum_definition_bytes
    {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_semantic_byte_limits",
        "selected semantic byte limits must be nonzero, cover definitions, and remain within protocol maxima",
      ));
    }
    Ok(Self { maximum_definition_bytes, maximum_retained_bytes })
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSelectedSemanticLimitsV1 {
  counts: NativeSelectedSemanticCountLimitsV1,
  bytes: NativeSelectedSemanticByteLimitsV1,
}

impl NativeSelectedSemanticLimitsV1 {
  pub const fn new(counts: NativeSelectedSemanticCountLimitsV1, bytes: NativeSelectedSemanticByteLimitsV1) -> Self {
    Self { counts, bytes }
  }
}

pub fn default_native_selected_semantic_limits_v1() -> NativeSelectedSemanticLimitsV1 {
  NativeSelectedSemanticLimitsV1 {
    counts: NativeSelectedSemanticCountLimitsV1 {
      maximum_requested_fields: SELECTED_SEMANTIC_MAXIMUM_REQUESTED_FIELDS,
      maximum_scopes: SELECTED_SEMANTIC_MAXIMUM_SCOPES,
      maximum_value_stores: SELECTED_SEMANTIC_MAXIMUM_VALUE_STORES,
      maximum_field_indexes: SELECTED_SEMANTIC_MAXIMUM_FIELD_INDEXES,
      maximum_catalog_items: SELECTED_SEMANTIC_MAXIMUM_CATALOG_ITEMS,
      maximum_work_steps: SELECTED_SEMANTIC_MAXIMUM_WORK_STEPS,
    },
    bytes: NativeSelectedSemanticByteLimitsV1 {
      maximum_definition_bytes: SELECTED_SEMANTIC_MAXIMUM_DEFINITION_BYTES,
      maximum_retained_bytes: SELECTED_SEMANTIC_MAXIMUM_RETAINED_BYTES,
    },
  }
}

#[derive(Clone, Copy)]
pub struct NativeSelectedArtifactCursorRequestV1<'a> {
  pub catalog: &'a RootAwareQueryFieldCatalogV1,
  pub scope_id: &'a [u8],
  pub selected_generation: &'a QueryPlanningCoverageGenerationV1,
  pub role: OrderedIndexRoleV1,
  pub seek: ArtifactPageSeekV1<'a>,
  pub neighbors: ArtifactPageNeighborModeV1,
  pub limits: ArtifactPageCursorLimitsV1,
}

#[derive(Clone, Copy)]
pub struct NativeSelectedPostingSeekRequestV1<'a> {
  pub catalog: &'a RootAwareQueryFieldCatalogV1,
  pub scope_id: &'a [u8],
  pub selected_generation: &'a QueryPlanningCoverageGenerationV1,
  pub nvt_descriptor: Option<&'a IndexCoverageNvtDescriptorV1>,
  pub target_coordinate: u64,
  pub target_posting_position: &'a [u8],
  pub neighbors: ArtifactPageNeighborModeV1,
  pub limits: ArtifactPageCursorLimitsV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSelectedSourceLimitsV1 {
  maximum_retained_bytes: u64,
}

impl NativeSelectedSourceLimitsV1 {
  pub fn new(maximum_retained_bytes: u64) -> Result<Self, NativeSelectedNamespaceReadErrorV1> {
    if maximum_retained_bytes == 0 || maximum_retained_bytes > SELECTED_SOURCE_MAXIMUM_RETAINED_BYTES {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_source_limits",
        "selected source retained-byte limit must be nonzero and remain within the protocol maximum",
      ));
    }
    Ok(Self { maximum_retained_bytes })
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeSelectedSourceOutcomeV1 {
  OutOfScope,
  Missing,
  Values(Vec<Vec<u8>>),
  ParserUnindexable(super::index_producer_collector::IndexParserDeterministicFailureV1),
  SourceUnindexable { code: &'static str, context: String },
}

#[derive(Clone, Copy)]
pub enum NativeSelectedSourceParserV1<'parser> {
  Native,
  Explicit(&'parser dyn IndexParserExecutorV1),
}

pub struct NativeSelectedSourceEvaluationV1 {
  selected_root: Vec<u8>,
  semantic_state_root: Vec<u8>,
  scope_id: Vec<u8>,
  value_store_id: Vec<u8>,
  file_key: Vec<u8>,
  record_revision: Vec<u8>,
  outcome: NativeSelectedSourceOutcomeV1,
  _source_memory: Option<MemoryReservation>,
  _receipt_memory: MemoryReservation,
}

impl fmt::Debug for NativeSelectedSourceEvaluationV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("NativeSelectedSourceEvaluationV1")
      .field("selected_root", &hex::encode(&self.selected_root))
      .field("semantic_state_root", &hex::encode(&self.semantic_state_root))
      .field("scope_id", &hex::encode(&self.scope_id))
      .field("value_store_id", &hex::encode(&self.value_store_id))
      .field("file_key", &hex::encode(&self.file_key))
      .field("record_revision", &hex::encode(&self.record_revision))
      .field("outcome", &self.outcome)
      .finish_non_exhaustive()
  }
}

impl NativeSelectedSourceEvaluationV1 {
  pub fn selected_root(&self) -> &[u8] {
    &self.selected_root
  }

  pub fn semantic_state_root(&self) -> &[u8] {
    &self.semantic_state_root
  }

  pub fn scope_id(&self) -> &[u8] {
    &self.scope_id
  }

  pub fn value_store_id(&self) -> &[u8] {
    &self.value_store_id
  }

  pub fn file_key(&self) -> &[u8] {
    &self.file_key
  }

  pub fn record_revision(&self) -> &[u8] {
    &self.record_revision
  }

  pub const fn outcome(&self) -> &NativeSelectedSourceOutcomeV1 {
    &self.outcome
  }

  pub(crate) fn into_outcome(self) -> NativeSelectedSourceOutcomeV1 {
    self.outcome
  }
}

pub struct NativeSelectedSourceEvaluatorV1<'reader, 'view, 'definition> {
  reader: &'reader NativeSelectedNamespaceReaderV1<'view>,
  scope: ScopeDefinitionV1<'definition>,
  scope_id: Vec<u8>,
  value_store_id: Vec<u8>,
  evaluator: AuthoritativeSourceEvaluatorV1<'definition>,
  receipt_maximum: u64,
  limits: NativeSelectedSourceLimitsV1,
  _prepared_memory: MemoryReservation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSelectedNamespaceReadErrorClassV1 {
  InvalidRequest,
  ResourceLimit,
  Unavailable,
  Corrupt,
  Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSelectedNamespaceReadErrorV1 {
  class: NativeSelectedNamespaceReadErrorClassV1,
  code: &'static str,
  context: String,
}

impl NativeSelectedNamespaceReadErrorV1 {
  fn invalid(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeSelectedNamespaceReadErrorClassV1::InvalidRequest, code, context: context.into() }
  }

  fn resource(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeSelectedNamespaceReadErrorClassV1::ResourceLimit, code, context: context.into() }
  }

  fn unavailable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeSelectedNamespaceReadErrorClassV1::Unavailable, code, context: context.into() }
  }

  fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: NativeSelectedNamespaceReadErrorClassV1::Corrupt, code, context: context.into() }
  }

  fn cancelled() -> Self {
    Self {
      class: NativeSelectedNamespaceReadErrorClassV1::Cancelled,
      code: "selected_namespace_cancelled",
      context: "selected namespace read was cancelled".to_string(),
    }
  }

  pub const fn class(&self) -> NativeSelectedNamespaceReadErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for NativeSelectedNamespaceReadErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for NativeSelectedNamespaceReadErrorV1 {}

impl From<ReadViewAuthorizationFailureV1> for NativeSelectedNamespaceReadErrorV1 {
  fn from(error: ReadViewAuthorizationFailureV1) -> Self {
    match error {
      ReadViewAuthorizationFailureV1::Denied => Self::corrupt(
        "selected_namespace_authorization_invariant",
        "an already-authorized selected namespace reader encountered a second authorization denial",
      ),
      ReadViewAuthorizationFailureV1::Canceled => Self::cancelled(),
      ReadViewAuthorizationFailureV1::Corrupt(context) => Self::corrupt("selected_namespace_corrupt", context),
      ReadViewAuthorizationFailureV1::Unavailable(context) => Self::unavailable("selected_namespace_unavailable", context),
    }
  }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NativeSelectedNamespaceFileRowV1 {
  file_key: Vec<u8>,
  record_revision: Vec<u8>,
  entity_version: u8,
  file_record: FileRecord,
  authority_binding: [u8; 32],
}

impl NativeSelectedNamespaceFileRowV1 {
  pub fn file_key(&self) -> &[u8] {
    &self.file_key
  }

  pub fn record_revision(&self) -> &[u8] {
    &self.record_revision
  }

  pub const fn entity_version(&self) -> u8 {
    self.entity_version
  }

  pub fn path(&self) -> &str {
    &self.file_record.path
  }

  pub const fn file_record(&self) -> &FileRecord {
    &self.file_record
  }
}

pub struct NativeSelectedNamespacePageV1 {
  database_id: [u8; 16],
  physical_instance_id: [u8; 16],
  selected_root: Vec<u8>,
  namespace_tree_root: Vec<u8>,
  semantic_state_root: Vec<u8>,
  publication_sequence: u64,
  header_slot_sequence: u64,
  rows: Vec<NativeSelectedNamespaceFileRowV1>,
  next_resume_after: Option<String>,
  complete: bool,
  _memory: MemoryReservation,
}

impl NativeSelectedNamespacePageV1 {
  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn physical_instance_id(&self) -> [u8; 16] {
    self.physical_instance_id
  }

  pub fn selected_root(&self) -> &[u8] {
    &self.selected_root
  }

  pub fn namespace_tree_root(&self) -> &[u8] {
    &self.namespace_tree_root
  }

  pub fn semantic_state_root(&self) -> &[u8] {
    &self.semantic_state_root
  }

  pub const fn publication_sequence(&self) -> u64 {
    self.publication_sequence
  }

  pub const fn header_slot_sequence(&self) -> u64 {
    self.header_slot_sequence
  }

  pub fn rows(&self) -> &[NativeSelectedNamespaceFileRowV1] {
    &self.rows
  }

  pub fn next_resume_after(&self) -> Option<&str> {
    self.next_resume_after.as_deref()
  }

  pub const fn complete(&self) -> bool {
    self.complete
  }
}

pub struct NativeSelectedNamespaceIdentityResultV1 {
  database_id: [u8; 16],
  physical_instance_id: [u8; 16],
  selected_root: Vec<u8>,
  namespace_tree_root: Vec<u8>,
  semantic_state_root: Vec<u8>,
  publication_sequence: u64,
  header_slot_sequence: u64,
  found: Option<NativeSelectedNamespaceFileRowV1>,
  _memory: MemoryReservation,
}

impl NativeSelectedNamespaceIdentityResultV1 {
  pub const fn database_id(&self) -> [u8; 16] {
    self.database_id
  }

  pub const fn physical_instance_id(&self) -> [u8; 16] {
    self.physical_instance_id
  }

  pub fn selected_root(&self) -> &[u8] {
    &self.selected_root
  }

  pub fn namespace_tree_root(&self) -> &[u8] {
    &self.namespace_tree_root
  }

  pub fn semantic_state_root(&self) -> &[u8] {
    &self.semantic_state_root
  }

  pub const fn publication_sequence(&self) -> u64 {
    self.publication_sequence
  }

  pub const fn header_slot_sequence(&self) -> u64 {
    self.header_slot_sequence
  }

  pub const fn found(&self) -> Option<&NativeSelectedNamespaceFileRowV1> {
    self.found.as_ref()
  }

  pub fn into_found(self) -> Option<NativeSelectedNamespaceFileRowV1> {
    self.found
  }

  pub const fn is_absent(&self) -> bool {
    self.found.is_none()
  }
}

pub struct NativeSelectedSemanticCatalogV1 {
  selected_root: Vec<u8>,
  semantic_state_root: Vec<u8>,
  catalogs: Vec<RootAwareQueryFieldCatalogV1>,
  scope_definitions: Vec<NativeSelectedScopeDefinitionV1>,
  coverage_bound: bool,
  _coverage_memory: Option<MemoryReservation>,
  _memory: MemoryReservation,
}

struct NativePlannerCoverageBindingV1 {
  catalog_index: usize,
  scope_index: usize,
  candidate_index: usize,
  selected_generation: Option<QueryPlanningCoverageGenerationV1>,
  nvt_hint_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSelectedScopeDefinitionV1 {
  scope_id: Vec<u8>,
  encoded_definition: Vec<u8>,
}

impl NativeSelectedScopeDefinitionV1 {
  pub fn scope_id(&self) -> &[u8] {
    &self.scope_id
  }

  pub fn encoded_definition(&self) -> &[u8] {
    &self.encoded_definition
  }
}

impl NativeSelectedSemanticCatalogV1 {
  pub fn selected_root(&self) -> &[u8] {
    &self.selected_root
  }

  pub fn semantic_state_root(&self) -> &[u8] {
    &self.semantic_state_root
  }

  pub fn catalogs(&self) -> &[RootAwareQueryFieldCatalogV1] {
    &self.catalogs
  }

  /// Every selected semantic scope that can overlap the authorized query
  /// path, including scopes that do not define one of the requested fields.
  /// Effective-scope resolution must use this complete set before selecting a
  /// field-specific ValueStore.
  pub fn scope_definitions(&self) -> &[NativeSelectedScopeDefinitionV1] {
    &self.scope_definitions
  }
}

pub struct NativeSelectedNamespaceReaderV1<'view> {
  source: NativeReadViewSourceV1,
  view: &'view ResolvedReadViewV1<ResolvedPathAuthorizationV1>,
  authorized_scope: &'view str,
  limits: NativeSelectedNamespaceLimitsV1,
}

struct AccountedLoadedImmutableEntityV1 {
  entity: LoadedImmutableEntityV1,
  _memory: MemoryReservation,
}

struct LoadedSelectedFileRecordV1 {
  entity_version: u8,
  record: FileRecord,
}

#[derive(Clone)]
struct SelectedSemanticScopeDefinitionV1 {
  encoded_definition: Vec<u8>,
}

#[derive(Clone)]
struct SelectedSemanticValueStoreDefinitionV1 {
  value_store_id: Vec<u8>,
  encoded_definition: Vec<u8>,
}

#[derive(Clone)]
struct SelectedSemanticFieldIndexDefinitionV1 {
  encoded_definition: Vec<u8>,
}

struct CapturedSelectedSemanticObjectSourceV1<'reader, 'view> {
  reader: &'reader NativeSelectedNamespaceReaderV1<'view>,
}

impl SemanticCatalogObjectSourceV1 for CapturedSelectedSemanticObjectSourceV1<'_, '_> {
  fn load_semantic_object(&self, kind_id: u16, object_id: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    self
      .reader
      .source
      .publisher
      .load_semantic_object_at_captured_header(self.reader.view.captured_header(), kind_id, object_id, self.reader.view.cancellation())
      .map_err(|error| {
        if error.code() == "captured_authority_cancelled" {
          SemanticCatalogReadErrorV1::cancelled(error.code(), error.to_string())
        } else if authority_error_is_unavailable(&error) {
          SemanticCatalogReadErrorV1::unavailable(error.code(), error.to_string())
        } else {
          SemanticCatalogReadErrorV1::corrupt(error.code(), error.to_string())
        }
      })
  }
}

impl std::ops::Deref for AccountedLoadedImmutableEntityV1 {
  type Target = LoadedImmutableEntityV1;

  fn deref(&self) -> &Self::Target {
    &self.entity
  }
}

impl NativeReadViewSourceV1 {
  pub const fn new(publisher: Arc<V4FirstAuthorityPublisher>, memory: Arc<MemoryCoordinator>, current_configured_grace_ms: u64) -> Self {
    Self { publisher, memory, current_configured_grace_ms }
  }

  pub fn publisher(&self) -> &Arc<V4FirstAuthorityPublisher> {
    &self.publisher
  }

  pub fn memory_coordinator(&self) -> &Arc<MemoryCoordinator> {
    &self.memory
  }

  pub fn selected_namespace_reader<'view>(
    &self,
    view: &'view ResolvedReadViewV1<ResolvedPathAuthorizationV1>,
    limits: NativeSelectedNamespaceLimitsV1,
  ) -> Result<NativeSelectedNamespaceReaderV1<'view>, NativeSelectedNamespaceReadErrorV1> {
    let captured = view.captured_header();
    let authority = view.authority();
    if captured.header.database_id != view.database_id()
      || captured.header.physical_instance_id != view.physical_instance_id()
      || captured.header.hash_algorithm != view.hash_algorithm()
      || captured.header.slot_sequence != view.header_slot_sequence()
      || captured.header.write_sequence_high_water != view.write_sequence_high_water()
      || authority.root.root_hash != view.root_metadata().hash
      || authority.namespace_tree.root_hash != authority.root.namespace_tree_root
      || authority.semantic_state.object_id != authority.root.semantic_state_root
      || authority.admission.database_id != view.database_id()
      || authority.admission.namespace_root != view.root_metadata().hash
      || authority.admission.publication_sequence == 0
    {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_namespace_view_closure",
        "resolved read view does not retain one exact captured selected-root closure",
      ));
    }
    if !view.authorization().is_direct()
      || !matches!(
        view.authorization().operation(),
        crate::engine::permission_resolver::CrudlifyOp::Read | crate::engine::permission_resolver::CrudlifyOp::List
      )
    {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_namespace_authorization_scope",
        "selected namespace reader requires direct read or list authority",
      ));
    }
    let authorized_scope = canonical_selected_authorization_scope(view.authorization().path())?;
    Ok(NativeSelectedNamespaceReaderV1 { source: self.clone(), view, authorized_scope, limits })
  }
}

struct SelectedNamespaceScanStateV1<'request> {
  resume_after: Option<&'request str>,
  resume_seen: bool,
  rows: Vec<NativeSelectedNamespaceFileRowV1>,
  has_more: bool,
  work_steps: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectedDirectoryVisitControlV1 {
  Continue,
  Break,
}

enum SelectedDirectoryVisitV1 {
  Node,
  Child(ChildEntry),
}

impl<'view> NativeSelectedNamespaceReaderV1<'view> {
  pub fn scan_files(
    &self,
    scope: &str,
    resume_after: Option<&str>,
  ) -> Result<NativeSelectedNamespacePageV1, NativeSelectedNamespaceReadErrorV1> {
    self.validate_path(scope, true)?;
    self.validate_authorized_scope(scope)?;
    if let Some(resume_after) = resume_after {
      self.validate_path(resume_after, false)?;
      if !selected_path_is_within_scope(scope, resume_after) {
        return Err(NativeSelectedNamespaceReadErrorV1::invalid(
          "selected_namespace_resume_scope",
          "selected namespace resume path is outside the requested scope",
        ));
      }
    }
    self.check_cancelled()?;
    let mut page_memory = self
      .source
      .memory
      .reserve(MemoryOwner::Query, self.limits.maximum_page_bytes, AdmissionClass::Workload)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_page_memory", error.to_string()))?;
    let _workspace = self
      .source
      .memory
      .reserve(MemoryOwner::Query, SELECTED_NAMESPACE_WORKSPACE_BYTES, AdmissionClass::Workload)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_workspace_memory", error.to_string()))?;
    let mut rows = Vec::new();
    rows.try_reserve_exact(self.limits.maximum_page_documents).map_err(|error| {
      NativeSelectedNamespaceReadErrorV1::resource(
        "selected_namespace_page_allocation",
        format!("cannot reserve selected namespace page rows: {error}"),
      )
    })?;
    let mut state =
      SelectedNamespaceScanStateV1 { resume_after, resume_seen: resume_after.is_none(), rows, has_more: false, work_steps: 0 };
    let reference = self
      .source
      .resolve_path(self.view.captured_header(), &self.view.authority().namespace_tree.root_hash, scope, self.view.cancellation())
      .map_err(map_selected_namespace_error)?;
    if let Some(reference) = reference {
      self.scan_reference(scope, reference, selected_path_depth(scope), &mut state)?;
    }
    if !state.resume_seen {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_namespace_resume_missing",
        "selected immutable namespace no longer contains its own resume path",
      ));
    }
    self.check_cancelled()?;
    let complete = !state.has_more;
    let next_resume_after =
      if complete { None } else { state.rows.last().map(|row| try_clone_selected_string(row.path(), "resume path")).transpose()? };
    let selected_root = try_clone_selected_bytes(&self.view.root_metadata().hash, "selected root")?;
    let namespace_tree_root = try_clone_selected_bytes(&self.view.authority().namespace_tree.root_hash, "namespace tree root")?;
    let semantic_state_root = try_clone_selected_bytes(&self.view.authority().semantic_state.object_id, "semantic state root")?;
    let retained = selected_namespace_page_retained_bytes(
      &state.rows,
      state.rows.capacity(),
      None,
      [selected_root.capacity(), namespace_tree_root.capacity(), semantic_state_root.capacity()],
      next_resume_after.as_ref().map_or(0, String::capacity),
    )?;
    if retained > self.limits.maximum_page_bytes {
      return Err(NativeSelectedNamespaceReadErrorV1::resource(
        "selected_namespace_page_bytes",
        "selected namespace page exceeds its retained-byte bound",
      ));
    }
    page_memory
      .shrink(page_memory.bytes().checked_sub(retained).ok_or_else(|| {
        NativeSelectedNamespaceReadErrorV1::corrupt(
          "selected_namespace_page_accounting",
          "selected namespace retained page exceeds its memory reservation",
        )
      })?)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::corrupt("selected_namespace_page_accounting", error.to_string()))?;
    Ok(NativeSelectedNamespacePageV1 {
      database_id: self.view.database_id(),
      physical_instance_id: self.view.physical_instance_id(),
      selected_root,
      namespace_tree_root,
      semantic_state_root,
      publication_sequence: self.view.authority().admission.publication_sequence,
      header_slot_sequence: self.view.header_slot_sequence(),
      rows: state.rows,
      next_resume_after,
      complete,
      _memory: page_memory,
    })
  }

  pub fn resolve_file_identity(
    &self,
    scope: &str,
    file_key: &[u8],
    record_revision: &[u8],
  ) -> Result<NativeSelectedNamespaceIdentityResultV1, NativeSelectedNamespaceReadErrorV1> {
    self.validate_path(scope, true)?;
    self.validate_authorized_scope(scope)?;
    self.validate_identity(file_key, "FileKey")?;
    self.validate_identity(record_revision, "RecordRevision")?;
    self.check_cancelled()?;
    let _workspace = self
      .source
      .memory
      .reserve(MemoryOwner::Query, SELECTED_NAMESPACE_WORKSPACE_BYTES, AdmissionClass::Workload)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_workspace_memory", error.to_string()))?;
    let result_memory = self
      .source
      .memory
      .reserve(MemoryOwner::Query, self.limits.maximum_page_bytes, AdmissionClass::Workload)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_identity_memory", error.to_string()))?;
    let reference = self
      .source
      .resolve_path(self.view.captured_header(), &self.view.authority().namespace_tree.root_hash, scope, self.view.cancellation())
      .map_err(map_selected_namespace_error)?;
    let mut state = SelectedNamespaceIdentityStateV1 { file_key, record_revision, visited_documents: 0, work_steps: 0, found: None };
    if let Some(reference) = reference {
      self.find_identity(scope, reference, selected_path_depth(scope), &mut state)?;
    }
    self.check_cancelled()?;
    self.build_identity_result(state.found, result_memory)
  }

  pub(crate) fn restore_ordered_file_row(
    &self,
    file_key: &[u8],
    record_revision: &[u8],
    entity_version: u8,
    encoded_file_record: &[u8],
  ) -> Result<NativeSelectedNamespaceFileRowV1, NativeSelectedNamespaceReadErrorV1> {
    self.check_cancelled()?;
    self.validate_identity(file_key, "FileKey")?;
    self.validate_identity(record_revision, "RecordRevision")?;
    if !matches!(entity_version, 0 | 1) {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_namespace_ordered_file_version",
        "ordered query workspace contains an unreadable FileRecord version",
      ));
    }
    if encoded_file_record.len() > MAX_FILE_RECORD_ENTITY_BYTES {
      return Err(NativeSelectedNamespaceReadErrorV1::resource(
        "selected_namespace_ordered_file_bytes",
        "ordered query workspace FileRecord exceeds the native entity bound",
      ));
    }
    if digest_parts(self.view.hash_algorithm(), &[b"filec:", encoded_file_record]) != record_revision {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_namespace_ordered_revision",
        "ordered query workspace FileRecord does not match its RecordRevision",
      ));
    }
    let file_record = deserialize_file_record_v0_v1(encoded_file_record, self.view.hash_algorithm(), entity_version)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::corrupt("selected_namespace_ordered_file_record", error))?;
    self.validate_path(&file_record.path, false)?;
    self.validate_authorized_scope(&file_record.path)?;
    if digest_parts(self.view.hash_algorithm(), &[b"file:", file_record.path.as_bytes()]) != file_key {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_namespace_ordered_file_key",
        "ordered query workspace FileRecord path does not match its FileKey",
      ));
    }
    Ok(NativeSelectedNamespaceFileRowV1 {
      file_key: try_clone_selected_bytes(file_key, "ordered FileKey")?,
      record_revision: try_clone_selected_bytes(record_revision, "ordered RecordRevision")?,
      entity_version,
      file_record,
      authority_binding: self.authority_binding(),
    })
  }

  pub fn load_planner_catalogs(
    &self,
    query_path: &str,
    requested_fields: &[&str],
    limits: NativeSelectedSemanticLimitsV1,
  ) -> Result<NativeSelectedSemanticCatalogV1, NativeSelectedNamespaceReadErrorV1> {
    self.validate_path(query_path, true)?;
    self.validate_authorized_scope(query_path)?;
    self.check_cancelled()?;
    if requested_fields.is_empty() || requested_fields.len() > limits.counts.maximum_requested_fields {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_semantic_requested_fields",
        "selected semantic catalog requests must contain a bounded nonempty field set",
      ));
    }
    for field in requested_fields {
      self.check_cancelled()?;
      canonical_query_field_name_v1(field).map_err(map_query_field_error)?;
    }

    let SemanticAvailabilityV1::Complete {
      catalog_root, catalog_record_count, catalog_node_count, definition_count, dependency_count, ..
    } = &self.view.authority().semantic_state.availability
    else {
      return Err(NativeSelectedNamespaceReadErrorV1::unavailable(
        "selected_semantic_content_only",
        "selected root does not retain complete semantic definitions",
      ));
    };
    if *catalog_record_count == 0 {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_semantic_field_missing",
        "selected semantic state has no definitions for the requested fields",
      ));
    }
    validate_selected_semantic_counts(*catalog_record_count, *catalog_node_count, *definition_count, *dependency_count, limits)?;

    let mut result_memory =
      self
        .source
        .memory
        .reserve(MemoryOwner::Query, limits.bytes.maximum_retained_bytes, AdmissionClass::Workload)
        .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_result_memory", error.to_string()))?;
    let _workspace = self
      .source
      .memory
      .reserve(MemoryOwner::Query, SELECTED_SEMANTIC_WORKSPACE_BYTES, AdmissionClass::Workload)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_workspace_memory", error.to_string()))?;
    let mut required_fields = BTreeSet::new();
    for field in requested_fields {
      self.check_cancelled()?;
      let canonical = canonical_query_field_name_v1(field).map_err(map_query_field_error)?;
      required_fields.insert(try_clone_selected_string(canonical, "requested semantic field")?);
    }
    if required_fields.len() > limits.counts.maximum_requested_fields {
      return Err(NativeSelectedNamespaceReadErrorV1::resource(
        "selected_semantic_requested_fields",
        "selected semantic field set exceeds its admitted unique-field count",
      ));
    }
    let object_source = CapturedSelectedSemanticObjectSourceV1 { reader: self };
    let semantic_reader = SemanticCatalogReaderV1::new(self.view.hash_algorithm(), &object_source);
    let expected = SelectedSemanticExpectedCountsV1 {
      records: *catalog_record_count,
      nodes: *catalog_node_count,
      definitions: *definition_count,
      dependencies: *dependency_count,
    };
    let traversal_bounds = SemanticCatalogTraversalBoundsV1::new(expected.records, expected.nodes).map_err(map_semantic_catalog_error)?;
    let mut definition_bytes = 0u64;
    let mut scopes = BTreeMap::new();
    let stats = semantic_reader
      .walk_catalog(catalog_root, traversal_bounds, &|| self.view.cancellation().is_cancelled(), |record| {
        if record.record_kind != 3 {
          return Ok(());
        }
        semantic_reader.with_definition(record, &|| self.view.cancellation().is_cancelled(), |definition| {
          let scope = decode_scope_definition(definition, self.view.hash_algorithm())
            .map_err(|error| SemanticCatalogReadErrorV1::corrupt(error.code(), error.context()))?;
          validate_semantic_definition_identity_v1(record, &scope.scope_id)?;
          let applies = scope_owner_overlaps_query_path(&scope, query_path)
            .map_err(|error| SemanticCatalogReadErrorV1::corrupt(error.code(), error.context()))?;
          if !applies {
            return Ok(());
          }
          if scopes.len() >= limits.counts.maximum_scopes {
            return Err(SemanticCatalogReadErrorV1::resource(
              "selected_semantic_scope_limit",
              "applicable selected semantic scopes exceed their admitted count",
            ));
          }
          definition_bytes = add_selected_definition_bytes(definition_bytes, definition.len(), limits.bytes.maximum_definition_bytes)?;
          let scope_id = try_clone_semantic_bytes(&scope.scope_id, "scope identity")?;
          let retained =
            SelectedSemanticScopeDefinitionV1 { encoded_definition: try_clone_semantic_bytes(definition, "scope definition")? };
          if scopes.insert(scope_id, retained).is_some() {
            return Err(SemanticCatalogReadErrorV1::corrupt(
              "selected_semantic_scope_duplicate",
              "selected semantic catalog repeats one applicable ScopeId",
            ));
          }
          Ok(())
        })
      })
      .map_err(map_semantic_catalog_error)?;
    validate_selected_semantic_walk(stats, expected).map_err(map_semantic_catalog_error)?;

    let mut values = BTreeMap::new();
    let mut value_owners = BTreeSet::new();
    let stats = semantic_reader
      .walk_catalog(catalog_root, traversal_bounds, &|| self.view.cancellation().is_cancelled(), |record| {
        if record.record_kind != 4 {
          return Ok(());
        }
        semantic_reader.with_definition(record, &|| self.view.cancellation().is_cancelled(), |definition| {
          let value = decode_value_store_definition(definition, self.view.hash_algorithm())
            .map_err(|error| SemanticCatalogReadErrorV1::corrupt(error.code(), error.context()))?;
          validate_semantic_definition_identity_v1(record, &value.value_store_id)?;
          if !scopes.contains_key(value.scope_id) {
            return Ok(());
          }
          let canonical = canonical_query_field_name_v1(value.field_name)
            .map_err(|error| SemanticCatalogReadErrorV1::corrupt(error.code(), error.context()))?;
          if canonical != value.field_name {
            return Err(SemanticCatalogReadErrorV1::corrupt(
              "selected_semantic_field_noncanonical",
              "selected ValueStore definition uses a legacy field alias",
            ));
          }
          if !required_fields.contains(canonical) {
            return Ok(());
          }
          if values.len() >= limits.counts.maximum_value_stores {
            return Err(SemanticCatalogReadErrorV1::resource(
              "selected_semantic_value_store_limit",
              "applicable selected ValueStores exceed their admitted count",
            ));
          }
          definition_bytes = add_selected_definition_bytes(definition_bytes, definition.len(), limits.bytes.maximum_definition_bytes)?;
          let field_name = try_clone_semantic_string(canonical, "ValueStore field name")?;
          let scope_id = try_clone_semantic_bytes(value.scope_id, "ValueStore scope identity")?;
          let value_store_id = try_clone_semantic_bytes(&value.value_store_id, "ValueStore identity")?;
          let retained = SelectedSemanticValueStoreDefinitionV1 {
            value_store_id: try_clone_semantic_bytes(&value_store_id, "retained ValueStore identity")?,
            encoded_definition: try_clone_semantic_bytes(definition, "ValueStore definition")?,
          };
          let field_key = try_clone_semantic_string(&field_name, "ValueStore field key")?;
          let scope_key = try_clone_semantic_bytes(&scope_id, "ValueStore scope key")?;
          if values.insert((field_key, scope_key), retained).is_some() || !value_owners.insert(value_store_id) {
            return Err(SemanticCatalogReadErrorV1::corrupt(
              "selected_semantic_value_store_duplicate",
              "selected semantic catalog repeats one field/scope ValueStore relationship",
            ));
          }
          Ok(())
        })
      })
      .map_err(map_semantic_catalog_error)?;
    validate_selected_semantic_walk(stats, expected).map_err(map_semantic_catalog_error)?;

    let mut indexes: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, SelectedSemanticFieldIndexDefinitionV1>> = BTreeMap::new();
    let mut field_index_count = 0usize;
    let stats = semantic_reader
      .walk_catalog(catalog_root, traversal_bounds, &|| self.view.cancellation().is_cancelled(), |record| {
        if record.record_kind != 5 {
          return Ok(());
        }
        semantic_reader.with_definition(record, &|| self.view.cancellation().is_cancelled(), |definition| {
          let field = decode_field_index_definition(definition, self.view.hash_algorithm())
            .map_err(|error| SemanticCatalogReadErrorV1::corrupt(error.code(), error.context()))?;
          validate_semantic_definition_identity_v1(record, &field.index_id)?;
          if !value_owners.contains(field.value_store_id) {
            return Ok(());
          }
          if field_index_count >= limits.counts.maximum_field_indexes {
            return Err(SemanticCatalogReadErrorV1::resource(
              "selected_semantic_field_index_limit",
              "applicable selected FieldIndexes exceed their admitted count",
            ));
          }
          definition_bytes = add_selected_definition_bytes(definition_bytes, definition.len(), limits.bytes.maximum_definition_bytes)?;
          let value_store_id = try_clone_semantic_bytes(field.value_store_id, "FieldIndex ValueStore identity")?;
          let index_id = try_clone_semantic_bytes(&field.index_id, "FieldIndex identity")?;
          let retained =
            SelectedSemanticFieldIndexDefinitionV1 { encoded_definition: try_clone_semantic_bytes(definition, "FieldIndex definition")? };
          if indexes.entry(value_store_id).or_default().insert(index_id, retained).is_some() {
            return Err(SemanticCatalogReadErrorV1::corrupt(
              "selected_semantic_field_index_duplicate",
              "selected semantic catalog repeats one FieldIndex identity",
            ));
          }
          field_index_count += 1;
          Ok(())
        })
      })
      .map_err(map_semantic_catalog_error)?;
    validate_selected_semantic_walk(stats, expected).map_err(map_semantic_catalog_error)?;
    self.check_cancelled()?;

    let mut catalogs = Vec::new();
    catalogs.try_reserve_exact(required_fields.len()).map_err(|error| {
      NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_result_allocation", format!("catalog allocation failed: {error}"))
    })?;
    for field_name in required_fields {
      let mut planner_scopes = Vec::new();
      for ((value_field, scope_id), value) in &values {
        if value_field != &field_name {
          continue;
        }
        let scope = scopes.get(scope_id).ok_or_else(|| {
          NativeSelectedNamespaceReadErrorV1::corrupt(
            "selected_semantic_scope_closure",
            "retained ValueStore lost its selected ScopeDefinition",
          )
        })?;
        let selected_indexes = indexes.get(&value.value_store_id);
        let index_count = selected_indexes.map_or(0, BTreeMap::len);
        let mut planner_indexes = Vec::new();
        planner_indexes.try_reserve_exact(index_count).map_err(|error| {
          NativeSelectedNamespaceReadErrorV1::resource(
            "selected_semantic_result_allocation",
            format!("FieldIndex catalog allocation failed: {error}"),
          )
        })?;
        if let Some(selected_indexes) = selected_indexes {
          for (index_id, selected) in selected_indexes {
            planner_indexes.push(QueryPlanningIndexCandidateV1 {
              index_id: try_clone_selected_bytes(index_id, "planner FieldIndex identity")?,
              encoded_field_definition: try_clone_selected_bytes(&selected.encoded_definition, "planner FieldIndex definition")?,
              selected_generation: None,
              estimates: QueryPlanningIndexEstimatesV1::new(0, 0, 0, 0, u64::MAX).map_err(map_query_catalog_error)?,
              nvt_hint_available: false,
            });
          }
        }
        planner_scopes.push(QueryPlanningScopeV1 {
          scope_id: try_clone_selected_bytes(scope_id, "planner scope identity")?,
          value_store_id: try_clone_selected_bytes(&value.value_store_id, "planner ValueStore identity")?,
          encoded_scope_definition: try_clone_selected_bytes(&scope.encoded_definition, "planner scope definition")?,
          encoded_value_store_definition: try_clone_selected_bytes(&value.encoded_definition, "planner ValueStore definition")?,
          semantic_availability: super::index_coverage_planner::IndexSemanticQueryAvailabilityV1::Complete,
          authoritative_document_count: u64::MAX,
          indexes: planner_indexes,
        });
      }
      if planner_scopes.is_empty() {
        return Err(NativeSelectedNamespaceReadErrorV1::invalid(
          "selected_semantic_field_missing",
          format!("selected semantic root has no applicable ValueStore for {field_name}"),
        ));
      }
      catalogs.push(RootAwareQueryFieldCatalogV1 {
        database_id: self.view.database_id(),
        physical_instance_id: self.view.physical_instance_id(),
        selected_namespace_root: try_clone_selected_bytes(&self.view.root_metadata().hash, "planner selected root")?,
        semantic_state_root: try_clone_selected_bytes(&self.view.authority().semantic_state.object_id, "planner semantic root")?,
        publication_sequence: self.view.authority().admission.publication_sequence,
        field_name,
        complete: true,
        scopes: planner_scopes,
      });
    }
    let mut scope_definitions = Vec::new();
    scope_definitions.try_reserve_exact(scopes.len()).map_err(|error| {
      NativeSelectedNamespaceReadErrorV1::resource(
        "selected_semantic_result_allocation",
        format!("scope-definition catalog allocation failed: {error}"),
      )
    })?;
    scope_definitions.extend(
      scopes
        .into_iter()
        .map(|(scope_id, scope)| NativeSelectedScopeDefinitionV1 { scope_id, encoded_definition: scope.encoded_definition }),
    );
    let selected_root = try_clone_selected_bytes(&self.view.root_metadata().hash, "selected semantic root receipt")?;
    let semantic_state_root = try_clone_selected_bytes(&self.view.authority().semantic_state.object_id, "selected semantic state receipt")?;
    let retained = selected_semantic_catalog_retained_bytes(
      &selected_root,
      &semantic_state_root,
      &catalogs,
      catalogs.capacity(),
      &scope_definitions,
      scope_definitions.capacity(),
    )?;
    if retained > limits.bytes.maximum_retained_bytes {
      return Err(NativeSelectedNamespaceReadErrorV1::resource(
        "selected_semantic_retained_bytes",
        "selected semantic planner catalogs exceed their retained-byte bound",
      ));
    }
    result_memory
      .shrink(result_memory.bytes().checked_sub(retained).ok_or_else(|| {
        NativeSelectedNamespaceReadErrorV1::corrupt(
          "selected_semantic_result_accounting",
          "selected semantic result exceeds its memory reservation",
        )
      })?)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::corrupt("selected_semantic_result_accounting", error.to_string()))?;
    Ok(NativeSelectedSemanticCatalogV1 {
      selected_root,
      semantic_state_root,
      catalogs,
      scope_definitions,
      coverage_bound: false,
      _coverage_memory: None,
      _memory: result_memory,
    })
  }

  pub fn bind_planner_coverage(
    &self,
    selected: &mut NativeSelectedSemanticCatalogV1,
    snapshot: &IndexCoverageRegistrySnapshotV1,
  ) -> Result<(), NativeSelectedNamespaceReadErrorV1> {
    self.check_cancelled()?;
    if selected.selected_root != self.view.root_metadata().hash
      || selected.semantic_state_root != self.view.authority().semantic_state.object_id
      || snapshot.hash_algorithm() != self.view.hash_algorithm()
      || snapshot.database_id() != self.view.database_id()
    {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_coverage_authority",
        "selected planner catalog or coverage snapshot belongs to another read authority",
      ));
    }
    if selected.coverage_bound
      || selected
        .catalogs
        .iter()
        .flat_map(|catalog| &catalog.scopes)
        .flat_map(|scope| &scope.indexes)
        .any(|candidate| candidate.selected_generation.is_some() || candidate.nvt_hint_available)
    {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_coverage_rebind",
        "selected planner coverage may be bound exactly once",
      ));
    }

    let mut selected_generation_count = 0usize;
    for catalog in &selected.catalogs {
      for scope in &catalog.scopes {
        for candidate in &scope.indexes {
          self.check_cancelled()?;
          if selected_planner_coverage(snapshot, candidate)?.is_some() {
            selected_generation_count = selected_generation_count.checked_add(1).ok_or_else(|| {
              NativeSelectedNamespaceReadErrorV1::resource("selected_coverage_count", "selected planner generation count overflowed")
            })?;
          }
        }
      }
    }
    self.check_cancelled()?;
    if selected_generation_count == 0 {
      selected.coverage_bound = true;
      return Ok(());
    }

    let transient_bytes = selected_coverage_binding_bound(selected_generation_count, self.view.hash_algorithm())?;
    let mut coverage_memory = self
      .source
      .memory
      .reserve(MemoryOwner::Query, transient_bytes, AdmissionClass::Workload)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_coverage_memory", error.to_string()))?;
    let mut bindings = Vec::new();
    bindings.try_reserve_exact(selected_generation_count).map_err(|error| {
      NativeSelectedNamespaceReadErrorV1::resource("selected_coverage_allocation", format!("coverage binding allocation failed: {error}"))
    })?;
    let mut retained_bytes = 0u64;
    for (catalog_index, catalog) in selected.catalogs.iter().enumerate() {
      for (scope_index, scope) in catalog.scopes.iter().enumerate() {
        for (candidate_index, candidate) in scope.indexes.iter().enumerate() {
          self.check_cancelled()?;
          let Some((generation, nvt_hint_available)) = selected_planner_coverage(snapshot, candidate)? else {
            continue;
          };
          let generation = try_clone_planning_generation(generation)?;
          retained_bytes = retained_bytes
            .checked_add(planning_generation_retained_bytes(&generation)?)
            .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_coverage_retained", "coverage bytes overflowed"))?;
          bindings.push(NativePlannerCoverageBindingV1 {
            catalog_index,
            scope_index,
            candidate_index,
            selected_generation: Some(generation),
            nvt_hint_available,
          });
        }
      }
    }
    self.check_cancelled()?;
    let coverage_memory = if retained_bytes == 0 {
      drop(coverage_memory);
      None
    } else {
      let release_bytes = coverage_memory.bytes().checked_sub(retained_bytes).ok_or_else(|| {
        NativeSelectedNamespaceReadErrorV1::corrupt(
          "selected_coverage_accounting",
          "retained planner generations exceed their coverage reservation",
        )
      })?;
      coverage_memory
        .shrink(release_bytes)
        .map_err(|error| NativeSelectedNamespaceReadErrorV1::corrupt("selected_coverage_accounting", error.to_string()))?;
      Some(coverage_memory)
    };
    for binding in &mut bindings {
      let candidate = &mut selected.catalogs[binding.catalog_index].scopes[binding.scope_index].indexes[binding.candidate_index];
      candidate.selected_generation = binding.selected_generation.take();
      candidate.nvt_hint_available = binding.nvt_hint_available;
    }
    drop(bindings);
    selected._coverage_memory = coverage_memory;
    selected.coverage_bound = true;
    Ok(())
  }

  pub fn load_index_artifact_page_cursor(
    &self,
    request: &NativeSelectedArtifactCursorRequestV1<'_>,
  ) -> Result<Option<NativeSelectedArtifactPageCursorV1>, NativeSelectedNamespaceReadErrorV1> {
    self.check_cancelled()?;
    self.validate_selected_artifact_catalog(request.catalog, request.scope_id, request.selected_generation, request.role)?;
    load_native_selected_artifact_page_cursor_v1(NativeSelectedArtifactLoadRequestV1 {
      publisher: self.source.publisher.as_ref(),
      memory: self.source.memory.as_ref(),
      captured: self.view.captured_header(),
      supported_reader_capabilities: self.view.supported_reader_capabilities(),
      selected_root: &self.view.root_metadata().hash,
      selected_generation: request.selected_generation,
      role: request.role,
      seek: request.seek,
      neighbors: request.neighbors,
      limits: request.limits,
      cancellation: self.view.cancellation(),
    })
    .map_err(map_selected_artifact_cursor_error)
  }

  pub fn seek_posting_page(
    &self,
    request: &NativeSelectedPostingSeekRequestV1<'_>,
  ) -> Result<Option<NativeSelectedPostingPageV1>, NativeSelectedNamespaceReadErrorV1> {
    self.check_cancelled()?;
    self.validate_selected_artifact_catalog(request.catalog, request.scope_id, request.selected_generation, OrderedIndexRoleV1::Posting)?;
    load_native_selected_posting_seek_v1(NativeSelectedPostingSeekLoadRequestV1 {
      publisher: self.source.publisher.as_ref(),
      memory: self.source.memory.as_ref(),
      captured: self.view.captured_header(),
      supported_reader_capabilities: self.view.supported_reader_capabilities(),
      selected_root: &self.view.root_metadata().hash,
      selected_generation: request.selected_generation,
      nvt_descriptor: request.nvt_descriptor,
      target_coordinate: request.target_coordinate,
      target_posting_position: request.target_posting_position,
      neighbors: request.neighbors,
      limits: request.limits,
      cancellation: self.view.cancellation(),
    })
    .map_err(map_selected_artifact_cursor_error)
  }

  pub fn prepare_authoritative_source<'reader, 'definition>(
    &'reader self,
    catalog: &'definition RootAwareQueryFieldCatalogV1,
    scope_id: &[u8],
    limits: NativeSelectedSourceLimitsV1,
  ) -> Result<NativeSelectedSourceEvaluatorV1<'reader, 'view, 'definition>, NativeSelectedNamespaceReadErrorV1> {
    self.check_cancelled()?;
    if catalog.database_id != self.view.database_id()
      || catalog.physical_instance_id != self.view.physical_instance_id()
      || catalog.selected_namespace_root != self.view.root_metadata().hash
      || catalog.semantic_state_root != self.view.authority().semantic_state.object_id
      || catalog.publication_sequence != self.view.authority().admission.publication_sequence
      || !catalog.complete
    {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_source_catalog_authority",
        "source catalog does not bind the exact complete selected semantic authority",
      ));
    }
    self.validate_identity(scope_id, "ScopeId")?;
    let mut selected_scope = None;
    for scope in &catalog.scopes {
      if scope.scope_id == scope_id && selected_scope.replace(scope).is_some() {
        return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
          "selected_source_scope_duplicate",
          "source catalog repeats the selected ScopeId",
        ));
      }
    }
    let selected_scope = selected_scope.ok_or_else(|| {
      NativeSelectedNamespaceReadErrorV1::invalid("selected_source_scope_missing", "source catalog does not contain the requested ScopeId")
    })?;
    let prepared_memory = self
      .source
      .memory
      .reserve(MemoryOwner::Query, SELECTED_SOURCE_PREPARED_FIXED_BYTES, AdmissionClass::Workload)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_source_memory", error.to_string()))?;
    let scope = decode_scope_definition(&selected_scope.encoded_scope_definition, self.view.hash_algorithm())
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::corrupt(error.code(), error.context()))?;
    if scope.scope_id != scope_id {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_source_scope_identity",
        "source catalog ScopeDefinition does not match its ScopeId",
      ));
    }
    let evaluator = AuthoritativeSourceEvaluatorV1::from_encoded(
      &selected_scope.encoded_value_store_definition,
      self.view.hash_algorithm(),
      scope_id,
      &selected_scope.value_store_id,
      self.source.memory.as_ref().clone(),
      AuthoritativeSourceMemoryPolicyV1::selected_query(),
    )
    .map_err(map_selected_source_evaluator_error)?;
    if evaluator.definition().field_name != catalog.field_name {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_source_field_identity",
        "selected ValueStore field does not match its planner catalog field",
      ));
    }
    let value_store_id = try_clone_selected_bytes(&evaluator.definition().value_store_id, "selected source ValueStore identity")?;
    let scope_id = try_clone_selected_bytes(scope_id, "selected source ScopeId")?;
    let receipt_maximum = selected_source_receipt_maximum_bytes(self.view.hash_algorithm().hash_length())?;
    let maximum_retained = evaluator.maximum_outcome_retained_bytes().checked_add(receipt_maximum).ok_or_else(|| {
      NativeSelectedNamespaceReadErrorV1::resource("selected_source_retained_bytes", "source retained-byte bound overflowed")
    })?;
    if maximum_retained > limits.maximum_retained_bytes {
      return Err(NativeSelectedNamespaceReadErrorV1::resource(
        "selected_source_retained_bytes",
        "selected ValueStore maximum retained result exceeds the caller bound",
      ));
    }
    Ok(NativeSelectedSourceEvaluatorV1 {
      reader: self,
      scope,
      scope_id,
      value_store_id,
      evaluator,
      receipt_maximum,
      limits,
      _prepared_memory: prepared_memory,
    })
  }

  fn validate_selected_artifact_catalog(
    &self,
    catalog: &RootAwareQueryFieldCatalogV1,
    scope_id: &[u8],
    selected_generation: &QueryPlanningCoverageGenerationV1,
    role: OrderedIndexRoleV1,
  ) -> Result<(), NativeSelectedNamespaceReadErrorV1> {
    if catalog.database_id != self.view.database_id()
      || catalog.physical_instance_id != self.view.physical_instance_id()
      || catalog.selected_namespace_root != self.view.root_metadata().hash
      || catalog.semantic_state_root != self.view.authority().semantic_state.object_id
      || catalog.publication_sequence != self.view.authority().admission.publication_sequence
      || !catalog.complete
    {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_artifact_catalog_authority",
        "artifact catalog does not bind the exact complete selected semantic authority",
      ));
    }
    if !matches!(role, OrderedIndexRoleV1::Posting | OrderedIndexRoleV1::IndexDocumentState) {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_artifact_catalog_role",
        "field-index artifact catalogs admit only Posting or IndexDocumentState roles",
      ));
    }
    let coverage_is_partial = selected_generation.source_namespace_root != catalog.selected_namespace_root;
    if selected_generation.coverage_publication_sequence > catalog.publication_sequence
      || (coverage_is_partial && selected_generation.coverage_publication_sequence >= catalog.publication_sequence)
      || (coverage_is_partial && selected_generation.health != IndexCoverageGenerationHealthV1::Healthy)
    {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_artifact_generation_interval",
        "artifact generation is not valid for the selected target publication interval",
      ));
    }
    self.validate_identity(scope_id, "ScopeId")?;
    let mut matching_scope = None;
    for scope in &catalog.scopes {
      if scope.scope_id == scope_id && matching_scope.replace(scope).is_some() {
        return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
          "selected_artifact_scope_duplicate",
          "artifact catalog repeats the selected ScopeId",
        ));
      }
    }
    let scope = matching_scope.ok_or_else(|| {
      NativeSelectedNamespaceReadErrorV1::invalid("selected_artifact_scope_missing", "artifact catalog has no selected ScopeId")
    })?;
    let mut matching_generation = 0usize;
    for candidate in &scope.indexes {
      if candidate.index_id == selected_generation.owner_id && candidate.selected_generation.as_ref() == Some(selected_generation) {
        let definition_fingerprint = field_definition_fingerprint(self.view.hash_algorithm(), &candidate.encoded_field_definition);
        let dependency_fingerprint = field_dependency_fingerprint(self.view.hash_algorithm(), &scope.scope_id, &scope.value_store_id);
        if definition_fingerprint != selected_generation.definition_fingerprint
          || dependency_fingerprint != selected_generation.dependency_fingerprint
        {
          return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
            "selected_artifact_catalog_fingerprint",
            "artifact catalog semantics disagree with the selected generation fingerprints",
          ));
        }
        matching_generation = matching_generation.checked_add(1).ok_or_else(|| {
          NativeSelectedNamespaceReadErrorV1::corrupt(
            "selected_artifact_generation_count",
            "artifact catalog generation match count overflowed",
          )
        })?;
      }
    }
    if matching_generation != 1 {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_artifact_generation_catalog",
        "artifact generation is not the unique selected generation of its authorized planner scope",
      ));
    }
    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  fn finish_source_evaluation(
    &self,
    row: &NativeSelectedNamespaceFileRowV1,
    scope_id: &[u8],
    value_store_id: Vec<u8>,
    outcome: NativeSelectedSourceOutcomeV1,
    source_memory: Option<MemoryReservation>,
    source_retained_bytes: u64,
    mut receipt_memory: MemoryReservation,
    limits: NativeSelectedSourceLimitsV1,
  ) -> Result<NativeSelectedSourceEvaluationV1, NativeSelectedNamespaceReadErrorV1> {
    self.check_cancelled()?;
    let selected_root = try_clone_selected_bytes(&self.view.root_metadata().hash, "selected source root")?;
    let semantic_state_root = try_clone_selected_bytes(&self.view.authority().semantic_state.object_id, "selected source semantic root")?;
    let scope_id = try_clone_selected_bytes(scope_id, "selected source scope identity")?;
    let file_key = try_clone_selected_bytes(row.file_key(), "selected source FileKey")?;
    let record_revision = try_clone_selected_bytes(row.record_revision(), "selected source revision")?;
    let receipt_retained = selected_source_receipt_retained_bytes([
      selected_root.capacity(),
      semantic_state_root.capacity(),
      scope_id.capacity(),
      value_store_id.capacity(),
      file_key.capacity(),
      record_revision.capacity(),
    ])?;
    let total_retained = receipt_retained.checked_add(source_retained_bytes).ok_or_else(|| {
      NativeSelectedNamespaceReadErrorV1::resource("selected_source_retained_bytes", "source result byte count overflowed")
    })?;
    if total_retained > limits.maximum_retained_bytes {
      return Err(NativeSelectedNamespaceReadErrorV1::resource(
        "selected_source_retained_bytes",
        "selected source result exceeds its retained-byte bound",
      ));
    }
    receipt_memory
      .shrink(receipt_memory.bytes().checked_sub(receipt_retained).ok_or_else(|| {
        NativeSelectedNamespaceReadErrorV1::corrupt("selected_source_receipt_accounting", "source receipt exceeds its reservation")
      })?)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::corrupt("selected_source_receipt_accounting", error.to_string()))?;
    Ok(NativeSelectedSourceEvaluationV1 {
      selected_root,
      semantic_state_root,
      scope_id,
      value_store_id,
      file_key,
      record_revision,
      outcome,
      _source_memory: source_memory,
      _receipt_memory: receipt_memory,
    })
  }

  fn scan_reference(
    &self,
    path: &str,
    reference: ChildEntry,
    depth: usize,
    state: &mut SelectedNamespaceScanStateV1<'_>,
  ) -> Result<SelectedDirectoryVisitControlV1, NativeSelectedNamespaceReadErrorV1> {
    self.step(&mut state.work_steps)?;
    let entry_type = EntryType::from_u8(reference.entry_type)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::corrupt("selected_namespace_entry_type", error.to_string()))?;
    match entry_type {
      EntryType::FileRecord => {
        if !state.resume_seen {
          if state.resume_after == Some(path) {
            state.resume_seen = true;
          }
          return Ok(SelectedDirectoryVisitControlV1::Continue);
        }
        if state.rows.len() >= self.limits.maximum_page_documents {
          state.has_more = true;
          return Ok(SelectedDirectoryVisitControlV1::Break);
        }
        let row = self.load_file_row(&reference, path)?;
        let prospective = selected_namespace_page_retained_bytes(
          &state.rows,
          state.rows.capacity(),
          Some(&row),
          [
            self.view.root_metadata().hash.len(),
            self.view.authority().namespace_tree.root_hash.len(),
            self.view.authority().semantic_state.object_id.len(),
          ],
          row.path().len(),
        )?;
        if prospective > self.limits.maximum_page_bytes {
          if state.rows.is_empty() {
            return Err(NativeSelectedNamespaceReadErrorV1::resource(
              "selected_namespace_row_bytes",
              "the first selected namespace row cannot fit in the page byte bound",
            ));
          }
          state.has_more = true;
          return Ok(SelectedDirectoryVisitControlV1::Break);
        }
        state.rows.push(row);
        Ok(SelectedDirectoryVisitControlV1::Continue)
      }
      EntryType::DirectoryIndex => {
        if depth >= self.limits.maximum_depth {
          return Err(NativeSelectedNamespaceReadErrorV1::resource(
            "selected_namespace_depth",
            "selected namespace traversal exceeds its path-depth bound",
          ));
        }
        let source = self.source.clone();
        source.visit_directory_children(self.view.captured_header(), &reference.hash, self.view.cancellation(), |visit| match visit {
          SelectedDirectoryVisitV1::Node => {
            self.step(&mut state.work_steps)?;
            Ok(SelectedDirectoryVisitControlV1::Continue)
          }
          SelectedDirectoryVisitV1::Child(child) => {
            let child_path = join_selected_path(path, &child.name, self.limits.maximum_path_bytes)?;
            self.scan_reference(&child_path, child, depth + 1, state)
          }
        })
      }
      EntryType::Symlink => Ok(SelectedDirectoryVisitControlV1::Continue),
      EntryType::Chunk | EntryType::DeletionRecord | EntryType::Snapshot | EntryType::Void | EntryType::Fork => {
        Err(NativeSelectedNamespaceReadErrorV1::corrupt(
          "selected_namespace_child_role",
          "selected namespace directory contains an entity role that cannot be a namespace child",
        ))
      }
    }
  }

  fn load_file_row(
    &self,
    reference: &ChildEntry,
    path: &str,
  ) -> Result<NativeSelectedNamespaceFileRowV1, NativeSelectedNamespaceReadErrorV1> {
    let loaded = self
      .source
      .load_file_record(self.view.captured_header(), reference, path, self.view.cancellation())
      .map_err(map_selected_namespace_error)?;
    let file_key = digest_parts(self.view.hash_algorithm(), &[b"file:", path.as_bytes()]);
    Ok(NativeSelectedNamespaceFileRowV1 {
      file_key,
      record_revision: try_clone_selected_bytes(&reference.hash, "record revision")?,
      entity_version: loaded.entity_version,
      file_record: loaded.record,
      authority_binding: self.authority_binding(),
    })
  }

  fn find_identity(
    &self,
    path: &str,
    reference: ChildEntry,
    depth: usize,
    state: &mut SelectedNamespaceIdentityStateV1<'_>,
  ) -> Result<SelectedDirectoryVisitControlV1, NativeSelectedNamespaceReadErrorV1> {
    self.step(&mut state.work_steps)?;
    let entry_type = EntryType::from_u8(reference.entry_type)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::corrupt("selected_namespace_entry_type", error.to_string()))?;
    match entry_type {
      EntryType::FileRecord => {
        state.visited_documents = state.visited_documents.checked_add(1).ok_or_else(|| {
          NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_identity_count", "identity document count overflowed")
        })?;
        if state.visited_documents > self.limits.maximum_identity_documents {
          return Err(NativeSelectedNamespaceReadErrorV1::resource(
            "selected_namespace_identity_count",
            "selected namespace identity lookup exceeded its document bound",
          ));
        }
        let derived = digest_parts(self.view.hash_algorithm(), &[b"file:", path.as_bytes()]);
        if derived != state.file_key {
          return Ok(SelectedDirectoryVisitControlV1::Continue);
        }
        if reference.hash != state.record_revision {
          return Ok(SelectedDirectoryVisitControlV1::Break);
        }
        state.found = Some(self.load_file_row(&reference, path)?);
        Ok(SelectedDirectoryVisitControlV1::Break)
      }
      EntryType::DirectoryIndex => {
        if depth >= self.limits.maximum_depth {
          return Err(NativeSelectedNamespaceReadErrorV1::resource(
            "selected_namespace_depth",
            "selected namespace traversal exceeds its path-depth bound",
          ));
        }
        let source = self.source.clone();
        source.visit_directory_children(self.view.captured_header(), &reference.hash, self.view.cancellation(), |visit| match visit {
          SelectedDirectoryVisitV1::Node => {
            self.step(&mut state.work_steps)?;
            Ok(SelectedDirectoryVisitControlV1::Continue)
          }
          SelectedDirectoryVisitV1::Child(child) => {
            let child_path = join_selected_path(path, &child.name, self.limits.maximum_path_bytes)?;
            self.find_identity(&child_path, child, depth + 1, state)
          }
        })
      }
      EntryType::Symlink => Ok(SelectedDirectoryVisitControlV1::Continue),
      EntryType::Chunk | EntryType::DeletionRecord | EntryType::Snapshot | EntryType::Void | EntryType::Fork => {
        Err(NativeSelectedNamespaceReadErrorV1::corrupt(
          "selected_namespace_child_role",
          "selected namespace directory contains an entity role that cannot be a namespace child",
        ))
      }
    }
  }

  fn validate_path(&self, path: &str, allow_root: bool) -> Result<(), NativeSelectedNamespaceReadErrorV1> {
    if path.len() > self.limits.maximum_path_bytes {
      return Err(NativeSelectedNamespaceReadErrorV1::resource(
        "selected_namespace_path_bytes",
        "selected namespace path exceeds its byte bound",
      ));
    }
    if path.is_empty() || (!allow_root && path == "/") || path.as_bytes().contains(&0) || normalize_path(path) != path {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_namespace_path",
        "selected namespace path is not canonical for this operation",
      ));
    }
    Ok(())
  }

  fn build_identity_result(
    &self,
    found: Option<NativeSelectedNamespaceFileRowV1>,
    mut memory: MemoryReservation,
  ) -> Result<NativeSelectedNamespaceIdentityResultV1, NativeSelectedNamespaceReadErrorV1> {
    let selected_root = try_clone_selected_bytes(&self.view.root_metadata().hash, "selected root")?;
    let namespace_tree_root = try_clone_selected_bytes(&self.view.authority().namespace_tree.root_hash, "namespace tree root")?;
    let semantic_state_root = try_clone_selected_bytes(&self.view.authority().semantic_state.object_id, "semantic state root")?;
    let retained = selected_namespace_identity_retained_bytes(
      found.as_ref(),
      [selected_root.capacity(), namespace_tree_root.capacity(), semantic_state_root.capacity()],
    )?;
    if retained > memory.bytes() {
      return Err(NativeSelectedNamespaceReadErrorV1::resource(
        "selected_namespace_identity_bytes",
        "selected namespace identity result exceeds its retained-byte bound",
      ));
    }
    memory
      .shrink(memory.bytes() - retained)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::corrupt("selected_namespace_identity_accounting", error.to_string()))?;
    Ok(NativeSelectedNamespaceIdentityResultV1 {
      database_id: self.view.database_id(),
      physical_instance_id: self.view.physical_instance_id(),
      selected_root,
      namespace_tree_root,
      semantic_state_root,
      publication_sequence: self.view.authority().admission.publication_sequence,
      header_slot_sequence: self.view.header_slot_sequence(),
      found,
      _memory: memory,
    })
  }

  fn validate_identity(&self, identity: &[u8], label: &'static str) -> Result<(), NativeSelectedNamespaceReadErrorV1> {
    if identity.len() != self.view.hash_algorithm().hash_length() || identity.iter().all(|byte| *byte == 0) {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_namespace_identity",
        format!("{label} has the wrong width or is all zero"),
      ));
    }
    Ok(())
  }

  fn validate_authorized_scope(&self, scope: &str) -> Result<(), NativeSelectedNamespaceReadErrorV1> {
    if !selected_path_is_within_scope(self.authorized_scope, scope) {
      return Err(NativeSelectedNamespaceReadErrorV1::invalid(
        "selected_namespace_authorization_scope",
        "selected namespace scope is outside the resolved read-view authorization",
      ));
    }
    Ok(())
  }

  fn check_cancelled(&self) -> Result<(), NativeSelectedNamespaceReadErrorV1> {
    if self.view.cancellation().is_cancelled() {
      return Err(NativeSelectedNamespaceReadErrorV1::cancelled());
    }
    Ok(())
  }

  fn authority_binding(&self) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aeordb.selected-namespace-row.v1\0");
    hasher.update(&self.view.database_id());
    hasher.update(&self.view.physical_instance_id());
    hasher.update(&(self.view.hash_algorithm().hash_length() as u64).to_le_bytes());
    hasher.update(&self.view.root_metadata().hash);
    hasher.update(&self.view.authority().namespace_tree.root_hash);
    hasher.update(&self.view.authority().semantic_state.object_id);
    hasher.update(&self.view.authority().admission.publication_sequence.to_le_bytes());
    hasher.update(&self.view.header_slot_sequence().to_le_bytes());
    hasher.update(&self.view.write_sequence_high_water().to_le_bytes());
    *hasher.finalize().as_bytes()
  }

  fn step(&self, work_steps: &mut u64) -> Result<(), NativeSelectedNamespaceReadErrorV1> {
    self.check_cancelled()?;
    *work_steps = work_steps
      .checked_add(1)
      .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_work", "selected namespace work count overflowed"))?;
    if *work_steps > self.limits.maximum_work_steps {
      return Err(NativeSelectedNamespaceReadErrorV1::resource(
        "selected_namespace_work",
        "selected namespace traversal exceeds its work bound",
      ));
    }
    Ok(())
  }
}

impl NativeSelectedSourceEvaluatorV1<'_, '_, '_> {
  pub fn evaluate(
    &self,
    row: &NativeSelectedNamespaceFileRowV1,
    parser: NativeSelectedSourceParserV1<'_>,
    mapper: Option<&dyn PluginMapperExecutorV1>,
  ) -> Result<NativeSelectedSourceEvaluationV1, NativeSelectedNamespaceReadErrorV1> {
    self.reader.check_cancelled()?;
    if row.authority_binding != self.reader.authority_binding() {
      return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
        "selected_source_row_authority",
        "source row was not produced by this exact selected read view",
      ));
    }
    let receipt_memory = self
      .reader
      .source
      .memory
      .reserve(MemoryOwner::Query, self.receipt_maximum, AdmissionClass::Workload)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_source_receipt_memory", error.to_string()))?;
    let value_store_id = try_clone_selected_bytes(&self.value_store_id, "selected source ValueStore identity")?;
    if !scope_matches_path(&self.scope, row.path())
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::corrupt(error.code(), error.context()))?
    {
      return self.reader.finish_source_evaluation(
        row,
        &self.scope_id,
        value_store_id,
        NativeSelectedSourceOutcomeV1::OutOfScope,
        None,
        0,
        receipt_memory,
        self.limits,
      );
    }
    let body_source = CapturedSelectedParserBodySourceV1 { reader: self.reader, row };
    let native_parser = NativeIndexParserExecutorV1::new(&body_source);
    let parser: &dyn IndexParserExecutorV1 = match parser {
      NativeSelectedSourceParserV1::Native => &native_parser,
      NativeSelectedSourceParserV1::Explicit(parser) => parser,
    };
    let evaluation = self
      .evaluator
      .evaluate(
        AuthoritativeSourceDocumentV1 {
          namespace_root: &self.reader.view.root_metadata().hash,
          record_revision_hash: row.record_revision(),
          file_record: row.file_record(),
        },
        parser,
        mapper,
        &|| self.reader.view.cancellation().is_cancelled(),
      )
      .map_err(map_selected_source_evaluator_error)?;
    let retained_bytes = evaluation.retained_bytes();
    let (outcome, source_memory) = match evaluation {
      AuthoritativeSourceEvaluationV1::Missing => (NativeSelectedSourceOutcomeV1::Missing, None),
      AuthoritativeSourceEvaluationV1::Values { values, reservation, .. } => {
        (NativeSelectedSourceOutcomeV1::Values(values), Some(reservation))
      }
      AuthoritativeSourceEvaluationV1::ParserUnindexable { failure, reservation, .. } => {
        (NativeSelectedSourceOutcomeV1::ParserUnindexable(failure), Some(reservation))
      }
      AuthoritativeSourceEvaluationV1::SourceUnindexable { code, context, reservation, .. } => {
        (NativeSelectedSourceOutcomeV1::SourceUnindexable { code, context }, Some(reservation))
      }
    };
    self.reader.finish_source_evaluation(
      row,
      &self.scope_id,
      value_store_id,
      outcome,
      source_memory,
      retained_bytes,
      receipt_memory,
      self.limits,
    )
  }
}

struct CapturedSelectedParserBodySourceV1<'reader, 'view> {
  reader: &'reader NativeSelectedNamespaceReaderV1<'view>,
  row: &'reader NativeSelectedNamespaceFileRowV1,
}

impl NativeIndexParserBodySourceV1 for CapturedSelectedParserBodySourceV1<'_, '_> {
  fn hash_algorithm(&self) -> crate::engine::HashAlgorithm {
    self.reader.view.hash_algorithm()
  }

  fn load_body(
    &self,
    request: &super::index_producer_collector::IndexParserExecutionRequestV1<'_>,
    workspace_bytes: u64,
  ) -> Result<NativeIndexParserBodyV1, IndexParserExecutionErrorV1> {
    if request.namespace_root() != self.reader.view.root_metadata().hash
      || request.record_revision_hash() != self.row.record_revision()
      || request.file_record() != self.row.file_record()
      || self.row.authority_binding != self.reader.authority_binding()
    {
      return Err(IndexParserExecutionErrorV1::host_failure(
        "selected_source_corrupt_request",
        "selected parser request does not bind the exact captured row authority",
      ));
    }
    if (request.is_cancelled())() {
      return Err(IndexParserExecutionErrorV1::cancelled(
        "selected_source_cancelled_before_body",
        "selected source body read was cancelled",
      ));
    }
    let record = request.file_record();
    let expected_size = usize::try_from(record.total_size).map_err(|error| {
      IndexParserExecutionErrorV1::host_failure(
        "selected_source_corrupt_size",
        format!("selected document size does not fit this platform: {error}"),
      )
    })?;
    let body_bytes = native_parser_body_reservation_bytes_v1(record.total_size)?;
    let body_memory = self
      .reader
      .source
      .memory
      .reserve(MemoryOwner::Query, body_bytes.max(1), AdmissionClass::Workload)
      .map_err(|error| IndexParserExecutionErrorV1::host_failure("selected_source_resource_body", error.to_string()))?;
    let workspace_memory = self
      .reader
      .source
      .memory
      .reserve(MemoryOwner::Query, workspace_bytes.max(1), AdmissionClass::Workload)
      .map_err(|error| IndexParserExecutionErrorV1::host_failure("selected_source_resource_workspace", error.to_string()))?;
    let mut body = Vec::new();
    body.try_reserve_exact(expected_size).map_err(|error| {
      IndexParserExecutionErrorV1::host_failure("selected_source_resource_allocation", format!("cannot reserve selected body: {error}"))
    })?;
    for chunk_hash in &record.chunk_hashes {
      if (request.is_cancelled())() {
        return Err(IndexParserExecutionErrorV1::cancelled(
          "selected_source_cancelled_during_body",
          "selected source body read was cancelled",
        ));
      }
      if chunk_hash.len() != self.reader.view.hash_algorithm().hash_length() || chunk_hash.iter().all(|byte| *byte == 0) {
        return Err(IndexParserExecutionErrorV1::host_failure(
          "selected_source_corrupt_chunk_hash",
          "selected FileRecord contains a foreign-width or zero chunk hash",
        ));
      }
      let remaining = expected_size.checked_sub(body.len()).ok_or_else(|| {
        IndexParserExecutionErrorV1::host_failure("selected_source_corrupt_body_length", "selected body already exceeds its declared size")
      })?;
      let chunk = self
        .reader
        .source
        .load_entity_at_header(self.reader.view.captured_header(), chunk_hash, MAX_CHUNK_ENTITY_BYTES, self.reader.view.cancellation())
        .map_err(map_selected_body_authorization_error)?
        .ok_or_else(|| {
          IndexParserExecutionErrorV1::host_failure(
            "selected_source_corrupt_chunk_missing",
            format!("selected chunk {} is missing", hex::encode(chunk_hash)),
          )
        })?;
      if chunk.entry_type != EntryTypeV4::Chunk || chunk.entity_version != 0 || chunk.flags != 0 || chunk.key != *chunk_hash {
        return Err(IndexParserExecutionErrorV1::host_failure(
          "selected_source_corrupt_chunk_representation",
          "selected chunk representation is noncanonical",
        ));
      }
      let decoded = crate::engine::compression::decompress_bounded(&chunk.stored_value, chunk.compression_algorithm, remaining)
        .map_err(|error| IndexParserExecutionErrorV1::host_failure("selected_source_corrupt_chunk_compression", error.to_string()))?;
      if digest_parts(self.reader.view.hash_algorithm(), &[b"chunk:", &decoded]) != *chunk_hash {
        return Err(IndexParserExecutionErrorV1::host_failure(
          "selected_source_corrupt_chunk_identity",
          "selected chunk content identity is invalid",
        ));
      }
      let next = body.len().checked_add(decoded.len()).ok_or_else(|| {
        IndexParserExecutionErrorV1::host_failure("selected_source_corrupt_body_length", "selected body length overflowed")
      })?;
      if next > expected_size {
        return Err(IndexParserExecutionErrorV1::host_failure(
          "selected_source_corrupt_body_length",
          "selected body exceeds its declared size",
        ));
      }
      body.extend_from_slice(&decoded);
    }
    if body.len() != expected_size {
      return Err(IndexParserExecutionErrorV1::host_failure(
        "selected_source_corrupt_body_length",
        format!("selected body has {} bytes; FileRecord declares {expected_size}", body.len()),
      ));
    }
    if !record.content_hash.is_empty()
      && (record.content_hash.len() != self.reader.view.hash_algorithm().hash_length()
        || digest_parts(self.reader.view.hash_algorithm(), &[&body]) != record.content_hash)
    {
      return Err(IndexParserExecutionErrorV1::host_failure(
        "selected_source_corrupt_content_hash",
        "selected body does not match the FileRecord whole-content hash",
      ));
    }
    if (request.is_cancelled())() {
      return Err(IndexParserExecutionErrorV1::cancelled(
        "selected_source_cancelled_after_body",
        "selected source body read was cancelled",
      ));
    }
    Ok(NativeIndexParserBodyV1::new(body, body_memory, workspace_memory))
  }
}

struct SelectedNamespaceIdentityStateV1<'request> {
  file_key: &'request [u8],
  record_revision: &'request [u8],
  visited_documents: u64,
  work_steps: u64,
  found: Option<NativeSelectedNamespaceFileRowV1>,
}

#[derive(Clone, Copy)]
struct SelectedSemanticExpectedCountsV1 {
  records: u64,
  nodes: u64,
  definitions: u64,
  dependencies: u64,
}

fn validate_selected_semantic_counts(
  records: u64,
  nodes: u64,
  definitions: u64,
  dependencies: u64,
  limits: NativeSelectedSemanticLimitsV1,
) -> Result<(), NativeSelectedNamespaceReadErrorV1> {
  if nodes == 0 || definitions == 0 || definitions > records || dependencies > definitions {
    return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
      "selected_semantic_counts",
      "selected semantic-state counts cannot describe one complete catalog",
    ));
  }
  if records > limits.counts.maximum_catalog_items || nodes > limits.counts.maximum_catalog_items {
    return Err(NativeSelectedNamespaceReadErrorV1::resource(
      "selected_semantic_catalog_items",
      "selected semantic catalog exceeds its admitted record or node count",
    ));
  }
  let work = records
    .checked_add(nodes)
    .and_then(|value| value.checked_mul(3))
    .and_then(|value| value.checked_add(definitions))
    .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_work", "selected semantic work count overflowed"))?;
  if work > limits.counts.maximum_work_steps {
    return Err(NativeSelectedNamespaceReadErrorV1::resource(
      "selected_semantic_work",
      "selected semantic catalog exceeds its admitted traversal work",
    ));
  }
  Ok(())
}

fn validate_selected_semantic_walk(
  stats: SemanticCatalogWalkStatsV1,
  expected: SelectedSemanticExpectedCountsV1,
) -> Result<(), SemanticCatalogReadErrorV1> {
  let dependencies = stats.class_counts[6]
    .checked_add(stats.class_counts[7])
    .ok_or_else(|| SemanticCatalogReadErrorV1::corrupt("semantic_catalog_count_overflow", "catalog dependency count overflow"))?;
  if stats.records != expected.records || stats.nodes != expected.nodes || dependencies != expected.dependencies {
    return Err(SemanticCatalogReadErrorV1::corrupt(
      "semantic_catalog_counts",
      format!(
        "catalog walk observed {} records, {} nodes, and {} dependencies; expected {}, {}, and {}",
        stats.records, stats.nodes, dependencies, expected.records, expected.nodes, expected.dependencies
      ),
    ));
  }
  let required_definitions = stats.class_counts[3]
    .checked_add(stats.class_counts[4])
    .and_then(|value| value.checked_add(stats.class_counts[5]))
    .and_then(|value| value.checked_add(expected.dependencies))
    .ok_or_else(|| SemanticCatalogReadErrorV1::corrupt("semantic_catalog_count_overflow", "required definition count overflow"))?;
  if expected.definitions < required_definitions {
    return Err(SemanticCatalogReadErrorV1::corrupt(
      "semantic_catalog_counts",
      "semantic-state definition count is smaller than its uniquely owned scope/value/field/dependency definitions",
    ));
  }
  Ok(())
}

fn add_selected_definition_bytes(total: u64, bytes: usize, limit: u64) -> Result<u64, SemanticCatalogReadErrorV1> {
  let bytes =
    u64::try_from(bytes).map_err(|error| SemanticCatalogReadErrorV1::resource("selected_semantic_definition_bytes", error.to_string()))?;
  let total = total.checked_add(bytes).ok_or_else(|| {
    SemanticCatalogReadErrorV1::resource("selected_semantic_definition_bytes", "selected definition byte count overflowed")
  })?;
  if total > limit {
    return Err(SemanticCatalogReadErrorV1::resource(
      "selected_semantic_definition_bytes",
      "selected semantic definitions exceed their admitted byte count",
    ));
  }
  Ok(total)
}

fn try_clone_semantic_bytes(bytes: &[u8], label: &'static str) -> Result<Vec<u8>, SemanticCatalogReadErrorV1> {
  let mut cloned = Vec::new();
  cloned
    .try_reserve_exact(bytes.len())
    .map_err(|error| SemanticCatalogReadErrorV1::resource("selected_semantic_allocation", format!("cannot allocate {label}: {error}")))?;
  cloned.extend_from_slice(bytes);
  Ok(cloned)
}

fn try_clone_semantic_string(value: &str, label: &'static str) -> Result<String, SemanticCatalogReadErrorV1> {
  String::from_utf8(try_clone_semantic_bytes(value.as_bytes(), label)?)
    .map_err(|error| SemanticCatalogReadErrorV1::corrupt("selected_semantic_encoding", format!("cannot retain {label}: {error}")))
}

fn map_semantic_catalog_error(error: SemanticCatalogReadErrorV1) -> NativeSelectedNamespaceReadErrorV1 {
  match error.class() {
    SemanticCatalogReadErrorClassV1::Cancelled => NativeSelectedNamespaceReadErrorV1::cancelled(),
    SemanticCatalogReadErrorClassV1::Unavailable => NativeSelectedNamespaceReadErrorV1::unavailable(error.code(), error.context()),
    SemanticCatalogReadErrorClassV1::ResourceLimit => NativeSelectedNamespaceReadErrorV1::resource(error.code(), error.context()),
    SemanticCatalogReadErrorClassV1::Corrupt => NativeSelectedNamespaceReadErrorV1::corrupt(error.code(), error.context()),
  }
}

fn map_selected_artifact_cursor_error(error: NativeSelectedArtifactCursorErrorV1) -> NativeSelectedNamespaceReadErrorV1 {
  match error.class() {
    NativeSelectedArtifactCursorErrorClassV1::InvalidRequest => NativeSelectedNamespaceReadErrorV1::invalid(error.code(), error.context()),
    NativeSelectedArtifactCursorErrorClassV1::ResourceLimit => NativeSelectedNamespaceReadErrorV1::resource(error.code(), error.context()),
    NativeSelectedArtifactCursorErrorClassV1::Unavailable => NativeSelectedNamespaceReadErrorV1::unavailable(error.code(), error.context()),
    NativeSelectedArtifactCursorErrorClassV1::Corrupt => NativeSelectedNamespaceReadErrorV1::corrupt(error.code(), error.context()),
    NativeSelectedArtifactCursorErrorClassV1::Cancelled => NativeSelectedNamespaceReadErrorV1::cancelled(),
  }
}

fn map_query_field_error(error: super::query_planner::QueryPlanningErrorV1) -> NativeSelectedNamespaceReadErrorV1 {
  match error.class() {
    QueryPlanningErrorClassV1::ResourceLimit => NativeSelectedNamespaceReadErrorV1::resource(error.code(), error.context()),
    QueryPlanningErrorClassV1::Cancelled => NativeSelectedNamespaceReadErrorV1::cancelled(),
    QueryPlanningErrorClassV1::HistoricalViewUnavailable => NativeSelectedNamespaceReadErrorV1::unavailable(error.code(), error.context()),
    QueryPlanningErrorClassV1::InvalidRequest => NativeSelectedNamespaceReadErrorV1::invalid(error.code(), error.context()),
    QueryPlanningErrorClassV1::CorruptSource => NativeSelectedNamespaceReadErrorV1::corrupt(error.code(), error.context()),
  }
}

fn map_query_catalog_error(error: super::query_planner::QueryPlanningErrorV1) -> NativeSelectedNamespaceReadErrorV1 {
  NativeSelectedNamespaceReadErrorV1::corrupt(error.code(), error.context())
}

fn selected_semantic_catalog_retained_bytes(
  selected_root: &[u8],
  semantic_state_root: &[u8],
  catalogs: &[RootAwareQueryFieldCatalogV1],
  catalog_capacity: usize,
  scope_definitions: &[NativeSelectedScopeDefinitionV1],
  scope_definition_capacity: usize,
) -> Result<u64, NativeSelectedNamespaceReadErrorV1> {
  let catalog_slots = catalog_capacity.checked_mul(size_of::<RootAwareQueryFieldCatalogV1>()).ok_or_else(|| {
    NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_retained_bytes", "planner catalog slot accounting overflowed")
  })?;
  let scope_slots = scope_definition_capacity.checked_mul(size_of::<NativeSelectedScopeDefinitionV1>()).ok_or_else(|| {
    NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_retained_bytes", "complete scope-definition slot accounting overflowed")
  })?;
  let mut retained = size_of::<NativeSelectedSemanticCatalogV1>()
    .checked_add(selected_root.len())
    .and_then(|value| value.checked_add(semantic_state_root.len()))
    .and_then(|value| value.checked_add(catalog_slots))
    .and_then(|value| value.checked_add(scope_slots))
    .ok_or_else(|| {
      NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_retained_bytes", "planner catalog accounting overflowed")
    })?;
  for catalog in catalogs {
    retained = retained
      .checked_add(catalog.selected_namespace_root.capacity())
      .and_then(|value| value.checked_add(catalog.semantic_state_root.capacity()))
      .and_then(|value| value.checked_add(catalog.field_name.capacity()))
      .and_then(|value| value.checked_add(catalog.scopes.capacity().checked_mul(size_of::<QueryPlanningScopeV1>())?))
      .ok_or_else(|| {
        NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_retained_bytes", "planner field catalog accounting overflowed")
      })?;
    for scope in &catalog.scopes {
      retained = retained
        .checked_add(scope.scope_id.capacity())
        .and_then(|value| value.checked_add(scope.encoded_scope_definition.capacity()))
        .and_then(|value| value.checked_add(scope.encoded_value_store_definition.capacity()))
        .and_then(|value| value.checked_add(scope.indexes.capacity().checked_mul(size_of::<QueryPlanningIndexCandidateV1>())?))
        .ok_or_else(|| {
          NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_retained_bytes", "planner scope accounting overflowed")
        })?;
      for index in &scope.indexes {
        retained = retained
          .checked_add(index.index_id.capacity())
          .and_then(|value| value.checked_add(index.encoded_field_definition.capacity()))
          .ok_or_else(|| {
            NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_retained_bytes", "planner index accounting overflowed")
          })?;
      }
    }
  }
  for scope in scope_definitions {
    retained = retained
      .checked_add(scope.scope_id.capacity())
      .and_then(|value| value.checked_add(scope.encoded_definition.capacity()))
      .ok_or_else(|| {
        NativeSelectedNamespaceReadErrorV1::resource("selected_semantic_retained_bytes", "complete scope-definition accounting overflowed")
      })?;
  }
  u64::try_from(retained).map_err(|error| {
    NativeSelectedNamespaceReadErrorV1::resource(
      "selected_semantic_retained_bytes",
      format!("planner catalog retained bytes exceed u64: {error}"),
    )
  })
}

fn selected_coverage_binding_bound(
  selected_generation_count: usize,
  hash_algorithm: HashAlgorithm,
) -> Result<u64, NativeSelectedNamespaceReadErrorV1> {
  const GENERATION_HEAP_VECTORS: usize = 5;
  const ALLOCATION_ALLOWANCE: usize = 16;
  let generation_heap = hash_algorithm
    .hash_length()
    .checked_add(ALLOCATION_ALLOWANCE)
    .and_then(|value| value.checked_mul(GENERATION_HEAP_VECTORS))
    .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_coverage_bound", "generation heap bound overflowed"))?;
  let per_generation = size_of::<NativePlannerCoverageBindingV1>()
    .checked_add(generation_heap)
    .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_coverage_bound", "per-generation coverage bound overflowed"))?;
  let bytes = selected_generation_count
    .checked_mul(per_generation)
    .and_then(|value| value.checked_add(size_of::<Vec<NativePlannerCoverageBindingV1>>()))
    .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_coverage_bound", "coverage binding bound overflowed"))?;
  u64::try_from(bytes).map_err(|error| {
    NativeSelectedNamespaceReadErrorV1::resource("selected_coverage_bound", format!("coverage bound exceeds u64: {error}"))
  })
}

fn selected_planner_coverage<'snapshot>(
  snapshot: &'snapshot IndexCoverageRegistrySnapshotV1,
  candidate: &QueryPlanningIndexCandidateV1,
) -> Result<Option<(&'snapshot IndexCoverageRegistryGenerationV1, bool)>, NativeSelectedNamespaceReadErrorV1> {
  let Some(entry) = snapshot.entry(IndexCoverageRegistryOwnerKindV1::FieldIndex, &candidate.index_id) else {
    return Ok(None);
  };
  match entry.selection() {
    IndexCoverageRegistrySelectionV1::Unavailable(_) => {
      if matches!(entry.nvt_status(), IndexCoverageNvtStatusV1::Usable(_)) {
        return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
          "selected_coverage_nvt_without_generation",
          "coverage registry exposed a usable NVT without a selected FieldIndex generation",
        ));
      }
      Ok(None)
    }
    IndexCoverageRegistrySelectionV1::Selected(generation) => {
      if generation.owner_id() != candidate.index_id {
        return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
          "selected_coverage_owner",
          "coverage registry generation owner disagrees with its planner candidate",
        ));
      }
      Ok(Some((generation, matches!(entry.nvt_status(), IndexCoverageNvtStatusV1::Usable(_)))))
    }
  }
}

fn try_clone_planning_generation(
  generation: &IndexCoverageRegistryGenerationV1,
) -> Result<QueryPlanningCoverageGenerationV1, NativeSelectedNamespaceReadErrorV1> {
  Ok(QueryPlanningCoverageGenerationV1 {
    generation: generation.generation(),
    owner_id: try_clone_selected_bytes(generation.owner_id(), "coverage owner identity")?,
    manifest_hash: try_clone_selected_bytes(generation.manifest_hash(), "coverage manifest identity")?,
    source_namespace_root: try_clone_selected_bytes(generation.source_namespace_root(), "coverage source root")?,
    coverage_epoch_id: *generation.coverage_epoch_id(),
    coverage_publication_sequence: generation.coverage_publication_sequence(),
    definition_fingerprint: try_clone_selected_bytes(generation.definition_fingerprint(), "coverage definition fingerprint")?,
    dependency_fingerprint: try_clone_selected_bytes(generation.dependency_fingerprint(), "coverage dependency fingerprint")?,
    health: generation.health(),
  })
}

fn planning_generation_retained_bytes(generation: &QueryPlanningCoverageGenerationV1) -> Result<u64, NativeSelectedNamespaceReadErrorV1> {
  let bytes = generation
    .owner_id
    .capacity()
    .checked_add(generation.manifest_hash.capacity())
    .and_then(|value| value.checked_add(generation.source_namespace_root.capacity()))
    .and_then(|value| value.checked_add(generation.definition_fingerprint.capacity()))
    .and_then(|value| value.checked_add(generation.dependency_fingerprint.capacity()))
    .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_coverage_retained", "generation bytes overflowed"))?;
  u64::try_from(bytes).map_err(|error| {
    NativeSelectedNamespaceReadErrorV1::resource("selected_coverage_retained", format!("generation bytes exceed u64: {error}"))
  })
}

impl ReadViewAuthoritySourceV1 for NativeReadViewSourceV1 {
  fn capture_header(&self, cancellation: &CancellationToken) -> Result<SelectedDatabaseHeaderV4, ReadViewSourceErrorV1> {
    if cancellation.is_cancelled() {
      return Err(ReadViewSourceErrorV1::Canceled);
    }
    let selected = self.publisher.observe().map_err(map_header_error)?.selected;
    if cancellation.is_cancelled() {
      return Err(ReadViewSourceErrorV1::Canceled);
    }
    Ok(selected)
  }

  fn load_verified_authority(
    &self,
    header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<LoadedReadAuthorityV1, ReadViewSourceErrorV1> {
    if cancellation.is_cancelled() {
      return Err(ReadViewSourceErrorV1::Canceled);
    }
    let mut memory = self
      .memory
      .reserve(MemoryOwner::Query, AUTHORITY_PEAK_RESERVATION_BYTES, AdmissionClass::Workload)
      .map_err(|error| ReadViewSourceErrorV1::Memory(error.to_string()))?;
    let authority = self
      .publisher
      .load_namespace_authority_at_captured_header(header, root_hash, cancellation)
      .map_err(map_authority_error)?
      .ok_or(ReadViewSourceErrorV1::RootNotAdmitted)?;
    if cancellation.is_cancelled() {
      return Err(ReadViewSourceErrorV1::Canceled);
    }
    let retained = authority_retained_bytes(&authority)?;
    memory.shrink(memory.bytes().saturating_sub(retained)).map_err(|error| ReadViewSourceErrorV1::Memory(error.to_string()))?;
    Ok(LoadedReadAuthorityV1::new_accounted(authority, None, memory))
  }

  fn observe_lifecycle(
    &self,
    header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<RootLifecycleObservationV1, ReadViewLifecycleErrorV1> {
    self
      .publisher
      .observe_root_lifecycle_at_captured_header(header, root_hash, self.current_configured_grace_ms, cancellation, &self.memory)
      .map_err(map_lifecycle_error)
  }
}

impl SelectedRootPermissionSourceV1 for NativeReadViewSourceV1 {
  fn authorize_selected_root(
    &self,
    header: &SelectedDatabaseHeaderV4,
    authority: &LoadedReadAuthorityV1,
    request: SelectedRootPermissionRequestV1<'_>,
    cancellation: &CancellationToken,
  ) -> Result<Option<PathAuthorizationDecisionV1>, ReadViewAuthorizationFailureV1> {
    if cancellation.is_cancelled() {
      return Err(ReadViewAuthorizationFailureV1::Canceled);
    }
    let _workspace = self
      .memory
      .reserve(MemoryOwner::Query, PERMISSION_WORKSPACE_BYTES, AdmissionClass::Workload)
      .map_err(|error| ReadViewAuthorizationFailureV1::Unavailable(format!("selected permission memory admission failed: {error}")))?;
    let tree_root = authority.authority.namespace_tree.root_hash.as_slice();
    let direct = evaluate_ordered_path_permissions(request.current_groups(), request.path(), request.operation(), |level| {
      let path = permission_document_path(level);
      self.load_permission_document(header, tree_root, &path, cancellation)
    })?;
    if direct {
      return Ok(Some(PathAuthorizationDecisionV1::direct()));
    }
    if !matches!(
      request.operation(),
      crate::engine::permission_resolver::CrudlifyOp::Read | crate::engine::permission_resolver::CrudlifyOp::List
    ) {
      return Ok(None);
    }
    let children = self.descendant_grant_children(header, tree_root, request.path(), request.current_groups(), cancellation)?;
    Ok(PathAuthorizationDecisionV1::ancestor_navigation(children))
  }
}

impl NativeReadViewSourceV1 {
  fn load_permission_document(
    &self,
    header: &SelectedDatabaseHeaderV4,
    tree_root: &[u8],
    path: &str,
    cancellation: &CancellationToken,
  ) -> Result<Option<PathPermissions>, ReadViewAuthorizationFailureV1> {
    let Some(entry) = self.resolve_path(header, tree_root, path, cancellation)? else {
      return Ok(None);
    };
    let entry_type = EntryType::from_u8(entry.entry_type).map_err(|error| selected_corrupt(path, error))?;
    if entry_type != EntryType::FileRecord {
      return Err(selected_corrupt(path, "permission path resolves to a non-file entity"));
    }
    let bytes = self.load_file_bytes(header, &entry, path, cancellation)?;
    PathPermissions::deserialize_stored(&bytes, path).map(Some).map_err(|error| selected_corrupt(path, error))
  }

  fn resolve_path(
    &self,
    header: &SelectedDatabaseHeaderV4,
    tree_root: &[u8],
    path: &str,
    cancellation: &CancellationToken,
  ) -> Result<Option<ChildEntry>, ReadViewAuthorizationFailureV1> {
    let normalized = normalize_permission_path(path);
    if normalized != path || !path.starts_with('/') || path.split('/').any(|segment| segment == "." || segment == "..") {
      return Err(selected_corrupt(path, "selected permission path is not canonical"));
    }
    if path == "/" {
      return Ok(Some(directory_child(tree_root.to_vec(), String::new())));
    }
    let mut directory_hash = tree_root.to_vec();
    let segments = path.split('/').filter(|segment| !segment.is_empty()).collect::<Vec<_>>();
    if segments.len() > MAX_BTREE_DEPTH {
      return Err(selected_corrupt(path, "selected permission path exceeds the traversal depth bound"));
    }
    for (index, segment) in segments.iter().enumerate() {
      ensure_selected_not_cancelled(cancellation)?;
      let child = self.lookup_directory_child(header, &directory_hash, segment, cancellation)?;
      let Some(child) = child else {
        return Ok(None);
      };
      if index + 1 == segments.len() {
        return Ok(Some(child));
      }
      if EntryType::from_u8(child.entry_type).map_err(|error| selected_corrupt(path, error))? != EntryType::DirectoryIndex {
        return Ok(None);
      }
      directory_hash = child.hash;
    }
    Ok(None)
  }

  fn lookup_directory_child(
    &self,
    header: &SelectedDatabaseHeaderV4,
    directory_hash: &[u8],
    name: &str,
    cancellation: &CancellationToken,
  ) -> Result<Option<ChildEntry>, ReadViewAuthorizationFailureV1> {
    let mut current_hash = directory_hash.to_vec();
    let mut ancestors = BTreeSet::new();
    let mut btree_child = false;
    for _ in 0..MAX_BTREE_DEPTH {
      ensure_selected_not_cancelled(cancellation)?;
      if !ancestors.insert(current_hash.clone()) {
        return Err(selected_corrupt(name, "selected directory B-tree contains a cycle"));
      }
      let entity = self.load_directory_entity(header, &current_hash, cancellation)?;
      if !is_btree_format(&entity.stored_value) {
        if btree_child {
          return Err(selected_corrupt(name, "selected B-tree child uses the flat-directory format"));
        }
        let children = deserialize_child_entries(&entity.stored_value, header.header.hash_algorithm.hash_length(), entity.entity_version)
          .map_err(|error| selected_corrupt(name, error))?;
        if children.len() > MAX_FLAT_DIRECTORY_ENTRIES {
          return Err(selected_corrupt(name, "selected flat directory exceeds its entry bound"));
        }
        validate_sorted_children(&children, name)?;
        return Ok(children.into_iter().find(|child| child.name == name));
      }
      match decode_canonical_btree_node(&entity, header.header.hash_algorithm.hash_length(), name)? {
        BTreeNode::Leaf(leaf) => {
          return Ok(leaf.entries.into_iter().find(|entry| entry.name == name));
        }
        BTreeNode::Internal(internal) => {
          current_hash = internal.children[internal.find_child_index(name)].clone();
          btree_child = true;
        }
      }
    }
    Err(selected_corrupt(name, "selected directory B-tree exceeds the traversal depth bound"))
  }

  fn load_directory_entity(
    &self,
    header: &SelectedDatabaseHeaderV4,
    hash: &[u8],
    cancellation: &CancellationToken,
  ) -> Result<AccountedLoadedImmutableEntityV1, ReadViewAuthorizationFailureV1> {
    let entity = self
      .load_entity_at_header(header, hash, MAX_DIRECTORY_ENTITY_BYTES, cancellation)?
      .ok_or_else(|| selected_corrupt(&hex::encode(hash), "selected directory entity is missing"))?;
    if entity.entry_type != EntryTypeV4::DirectoryIndex
      || entity.entity_version != 0
      || entity.flags != 0
      || entity.compression_algorithm != CompressionAlgorithm::None
      || entity.key != hash
    {
      return Err(selected_corrupt(&hex::encode(hash), "selected directory entity representation is noncanonical"));
    }
    let domain = if is_btree_format(&entity.stored_value) { b"btree:".as_slice() } else { b"dirc:".as_slice() };
    if digest_parts(header.header.hash_algorithm, &[domain, &entity.stored_value]) != hash {
      return Err(selected_corrupt(&hex::encode(hash), "selected directory content identity is invalid"));
    }
    Ok(entity)
  }

  fn load_file_bytes(
    &self,
    header: &SelectedDatabaseHeaderV4,
    entry: &ChildEntry,
    expected_path: &str,
    cancellation: &CancellationToken,
  ) -> Result<Vec<u8>, ReadViewAuthorizationFailureV1> {
    let loaded = self.load_file_record(header, entry, expected_path, cancellation)?;
    let entity_version = loaded.entity_version;
    let record = loaded.record;
    if record.total_size > MAX_PERMISSION_DOCUMENT_BYTES as u64 {
      return Err(selected_corrupt(expected_path, "selected permission FileRecord exceeds its byte bound"));
    }
    if record.chunk_hashes.len() > MAX_PERMISSION_DOCUMENT_CHUNKS {
      return Err(selected_corrupt(expected_path, "selected permission FileRecord exceeds its chunk-count bound"));
    }
    let output_length = usize::try_from(record.total_size).map_err(|error| selected_corrupt(expected_path, error))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(output_length).map_err(|error| selected_unavailable(expected_path, error))?;
    for chunk_hash in &record.chunk_hashes {
      ensure_selected_not_cancelled(cancellation)?;
      let chunk = self
        .load_entity_at_header(header, chunk_hash, MAX_CHUNK_ENTITY_BYTES, cancellation)?
        .ok_or_else(|| selected_corrupt(expected_path, format!("selected chunk {} is missing", hex::encode(chunk_hash))))?;
      if chunk.entry_type != EntryTypeV4::Chunk || chunk.entity_version != 0 || chunk.flags != 0 || chunk.key != *chunk_hash {
        return Err(selected_corrupt(expected_path, "selected chunk representation is noncanonical"));
      }
      let remaining = output_length.saturating_sub(bytes.len());
      let decoded = crate::engine::compression::decompress_bounded(&chunk.stored_value, chunk.compression_algorithm, remaining)
        .map_err(|error| selected_corrupt(expected_path, error))?;
      if digest_parts(header.header.hash_algorithm, &[b"chunk:", &decoded]) != *chunk_hash {
        return Err(selected_corrupt(expected_path, "selected chunk content identity is invalid"));
      }
      bytes.extend_from_slice(&decoded);
    }
    if bytes.len() != output_length {
      return Err(selected_corrupt(expected_path, "selected file chunks do not match the declared length"));
    }
    if entity_version == 1 && digest_parts(header.header.hash_algorithm, &[&bytes]) != record.content_hash {
      return Err(selected_corrupt(expected_path, "selected file content hash is invalid"));
    }
    Ok(bytes)
  }

  fn load_file_record(
    &self,
    header: &SelectedDatabaseHeaderV4,
    entry: &ChildEntry,
    expected_path: &str,
    cancellation: &CancellationToken,
  ) -> Result<LoadedSelectedFileRecordV1, ReadViewAuthorizationFailureV1> {
    let entity = self
      .load_entity_at_header(header, &entry.hash, MAX_FILE_RECORD_ENTITY_BYTES, cancellation)?
      .ok_or_else(|| selected_corrupt(expected_path, "selected FileRecord is missing"))?;
    if entity.entry_type != EntryTypeV4::FileRecord
      || !matches!(entity.entity_version, 0 | 1)
      || entity.flags != 0
      || entity.compression_algorithm != CompressionAlgorithm::None
      || entity.key != entry.hash
      || digest_parts(header.header.hash_algorithm, &[b"filec:", &entity.stored_value]) != entry.hash
    {
      return Err(selected_corrupt(expected_path, "selected FileRecord representation or identity is invalid"));
    }
    let record = deserialize_file_record_v0_v1(&entity.stored_value, header.header.hash_algorithm, entity.entity_version)
      .map_err(|error| selected_corrupt(expected_path, error))?;
    if record.path != expected_path
      || record.total_size != entry.total_size
      || record.content_type != entry.content_type
      || record.created_at != entry.created_at
      || record.updated_at != entry.updated_at
    {
      return Err(selected_corrupt(expected_path, "selected FileRecord metadata does not match its directory entry"));
    }
    Ok(LoadedSelectedFileRecordV1 { entity_version: entity.entity_version, record })
  }

  fn load_entity_at_header(
    &self,
    header: &SelectedDatabaseHeaderV4,
    key: &[u8],
    maximum_total_length: usize,
    cancellation: &CancellationToken,
  ) -> Result<Option<AccountedLoadedImmutableEntityV1>, ReadViewAuthorizationFailureV1> {
    ensure_selected_not_cancelled(cancellation)?;
    let locator = self.publisher.locator(key).map_err(map_selected_authority_error)?;
    let length = locator.as_ref().map_or(0, |locator| locator.total_length as u64);
    if length > maximum_total_length as u64 {
      return Err(selected_corrupt(&hex::encode(key), "selected entity exceeds its role bound"));
    }
    let charge = length
      .checked_mul(2)
      .and_then(|bytes| bytes.checked_add(4096))
      .ok_or_else(|| selected_unavailable(&hex::encode(key), "selected entity memory charge overflow"))?;
    let memory = self
      .memory
      .reserve(MemoryOwner::Query, charge, AdmissionClass::Workload)
      .map_err(|error| selected_unavailable(&hex::encode(key), error))?;
    self
      .publisher
      .load_immutable_entity_at_captured_header(header, key, maximum_total_length, cancellation)
      .map_err(map_selected_authority_error)
      .map(|entity| entity.map(|entity| AccountedLoadedImmutableEntityV1 { entity, _memory: memory }))
  }

  fn descendant_grant_children(
    &self,
    header: &SelectedDatabaseHeaderV4,
    tree_root: &[u8],
    parent_path: &str,
    current_groups: &[String],
    cancellation: &CancellationToken,
  ) -> Result<BTreeSet<String>, ReadViewAuthorizationFailureV1> {
    let normalized_parent = normalize_navigation_path(parent_path);
    let Some(parent) = self.resolve_path(header, tree_root, &normalized_parent, cancellation)? else {
      return Ok(BTreeSet::new());
    };
    if EntryType::from_u8(parent.entry_type).map_err(|error| selected_corrupt(parent_path, error))? != EntryType::DirectoryIndex {
      return Ok(BTreeSet::new());
    }
    let mut visited_directories = 0usize;
    let mut permission_files = 0usize;
    let mut allowed_children = BTreeSet::new();
    self.scan_descendant_directory(
      header,
      tree_root,
      &normalized_parent,
      &normalized_parent,
      &parent.hash,
      path_depth(&normalized_parent),
      current_groups,
      cancellation,
      &mut visited_directories,
      &mut permission_files,
      &mut allowed_children,
    )?;
    Ok(allowed_children)
  }

  #[allow(clippy::too_many_arguments)]
  fn scan_descendant_directory(
    &self,
    header: &SelectedDatabaseHeaderV4,
    tree_root: &[u8],
    parent_path: &str,
    directory_path: &str,
    directory_hash: &[u8],
    depth: usize,
    current_groups: &[String],
    cancellation: &CancellationToken,
    visited_directories: &mut usize,
    permission_files: &mut usize,
    allowed_children: &mut BTreeSet<String>,
  ) -> Result<(), ReadViewAuthorizationFailureV1> {
    ensure_selected_not_cancelled(cancellation)?;
    *visited_directories = visited_directories.saturating_add(1);
    if *visited_directories > MAX_DESCENDANT_DIRECTORIES {
      return Err(selected_unavailable(parent_path, "selected descendant permission scan exceeded its directory bound"));
    }
    self.visit_directory_children(header, directory_hash, cancellation, |visit| {
      if let SelectedDirectoryVisitV1::Child(child) = visit {
        if child.name == ".aeordb-permissions" {
          *permission_files = permission_files.saturating_add(1);
          if *permission_files > MAX_DESCENDANT_PERMISSION_FILES {
            return Err(selected_unavailable(parent_path, "selected descendant permission scan exceeded its permission-file bound"));
          }
          let permission_path = join_path(directory_path, &child.name);
          let Some(document) = self.load_permission_document(header, tree_root, &permission_path, cancellation)? else {
            return Err(selected_corrupt(&permission_path, "listed permission authority disappeared"));
          };
          collect_descendant_children(&document.links, current_groups, parent_path, directory_path, allowed_children);
        } else if EntryType::from_u8(child.entry_type).map_err(|error| selected_corrupt(directory_path, error))?
          == EntryType::DirectoryIndex
          && depth < MAX_DESCENDANT_DEPTH
        {
          let child_path = join_path(directory_path, &child.name);
          self.scan_descendant_directory(
            header,
            tree_root,
            parent_path,
            &child_path,
            &child.hash,
            depth + 1,
            current_groups,
            cancellation,
            visited_directories,
            permission_files,
            allowed_children,
          )?;
        }
      }
      Ok(SelectedDirectoryVisitControlV1::Continue)
    })?;
    Ok(())
  }

  fn visit_directory_children<E>(
    &self,
    header: &SelectedDatabaseHeaderV4,
    root_hash: &[u8],
    cancellation: &CancellationToken,
    mut visitor: impl FnMut(SelectedDirectoryVisitV1) -> Result<SelectedDirectoryVisitControlV1, E>,
  ) -> Result<SelectedDirectoryVisitControlV1, E>
  where
    E: From<ReadViewAuthorizationFailureV1>,
  {
    let mut stack = vec![(root_hash.to_vec(), 0usize, false)];
    let mut visited_nodes = 0usize;
    let mut previous = None;
    while let Some((hash, depth, btree_child)) = stack.pop() {
      ensure_selected_not_cancelled(cancellation).map_err(E::from)?;
      visited_nodes = visited_nodes.saturating_add(1);
      if depth > MAX_BTREE_DEPTH || visited_nodes > MAX_BTREE_SCAN_NODES {
        return Err(E::from(selected_corrupt(&hex::encode(root_hash), "selected directory B-tree exceeds its depth or node bound")));
      }
      if visitor(SelectedDirectoryVisitV1::Node)? == SelectedDirectoryVisitControlV1::Break {
        return Ok(SelectedDirectoryVisitControlV1::Break);
      }
      let entity = self.load_directory_entity(header, &hash, cancellation).map_err(E::from)?;
      if !is_btree_format(&entity.stored_value) {
        if btree_child {
          return Err(E::from(selected_corrupt(&hex::encode(root_hash), "selected B-tree child uses the flat-directory format")));
        }
        let children = deserialize_child_entries(&entity.stored_value, header.header.hash_algorithm.hash_length(), entity.entity_version)
          .map_err(|error| E::from(selected_corrupt(&hex::encode(root_hash), error)))?;
        if children.len() > MAX_FLAT_DIRECTORY_ENTRIES {
          return Err(E::from(selected_corrupt(&hex::encode(root_hash), "selected flat directory exceeds its entry bound")));
        }
        validate_sorted_children(&children, &hex::encode(root_hash)).map_err(E::from)?;
        for child in children {
          validate_child_order(previous.as_deref(), &child.name).map_err(E::from)?;
          previous = Some(child.name.clone());
          if visitor(SelectedDirectoryVisitV1::Child(child))? == SelectedDirectoryVisitControlV1::Break {
            return Ok(SelectedDirectoryVisitControlV1::Break);
          }
        }
        continue;
      }
      match decode_canonical_btree_node(&entity, header.header.hash_algorithm.hash_length(), &hex::encode(root_hash)).map_err(E::from)? {
        BTreeNode::Leaf(leaf) => {
          for child in leaf.entries {
            validate_child_order(previous.as_deref(), &child.name).map_err(E::from)?;
            previous = Some(child.name.clone());
            if visitor(SelectedDirectoryVisitV1::Child(child))? == SelectedDirectoryVisitControlV1::Break {
              return Ok(SelectedDirectoryVisitControlV1::Break);
            }
          }
        }
        BTreeNode::Internal(internal) => {
          for child in internal.children.into_iter().rev() {
            stack.push((child, depth + 1, true));
          }
        }
      }
    }
    Ok(SelectedDirectoryVisitControlV1::Continue)
  }
}

fn map_selected_namespace_error(error: ReadViewAuthorizationFailureV1) -> NativeSelectedNamespaceReadErrorV1 {
  error.into()
}

fn canonical_selected_authorization_scope(path: &str) -> Result<&str, NativeSelectedNamespaceReadErrorV1> {
  if path == "/" {
    return Ok(path);
  }
  let scope = match path.strip_suffix('/') {
    Some(scope) => scope,
    None => path,
  };
  if scope.is_empty()
    || !scope.starts_with('/')
    || scope.ends_with('/')
    || scope.len() > SELECTED_NAMESPACE_MAXIMUM_PATH_BYTES
    || scope.as_bytes().contains(&0)
    || scope.trim() != scope
    || scope.split('/').skip(1).any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
  {
    return Err(NativeSelectedNamespaceReadErrorV1::invalid(
      "selected_namespace_authorization_scope",
      "resolved read-view authorization path is not a canonical selected namespace scope",
    ));
  }
  Ok(scope)
}

fn join_selected_path(parent: &str, child: &str, maximum_path_bytes: usize) -> Result<String, NativeSelectedNamespaceReadErrorV1> {
  if child.is_empty() || matches!(child, "." | "..") || child.contains('/') || child.as_bytes().contains(&0) {
    return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
      "selected_namespace_child_name",
      "selected namespace contains a noncanonical child name",
    ));
  }
  let separator_bytes = usize::from(parent != "/");
  let joined_length = parent.len().checked_add(separator_bytes).and_then(|length| length.checked_add(child.len())).ok_or_else(|| {
    NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_path_bytes", "selected namespace path length overflowed")
  })?;
  if joined_length > maximum_path_bytes {
    return Err(NativeSelectedNamespaceReadErrorV1::resource(
      "selected_namespace_path_bytes",
      "selected namespace path exceeds its byte bound",
    ));
  }
  let mut path = String::new();
  path
    .try_reserve_exact(joined_length)
    .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_path_allocation", error.to_string()))?;
  if parent == "/" {
    path.push('/');
  } else {
    path.push_str(parent);
    path.push('/');
  }
  path.push_str(child);
  if normalize_path(&path) != path {
    return Err(NativeSelectedNamespaceReadErrorV1::corrupt(
      "selected_namespace_child_path",
      "selected namespace child does not produce a canonical path",
    ));
  }
  Ok(path)
}

fn selected_path_depth(path: &str) -> usize {
  path.split('/').filter(|segment| !segment.is_empty()).count()
}

fn selected_path_is_within_scope(scope: &str, path: &str) -> bool {
  if scope == "/" {
    return path.starts_with('/');
  }
  path == scope || path.strip_prefix(scope).is_some_and(|suffix| suffix.starts_with('/'))
}

fn try_clone_selected_bytes(bytes: &[u8], label: &'static str) -> Result<Vec<u8>, NativeSelectedNamespaceReadErrorV1> {
  let mut cloned = Vec::new();
  cloned.try_reserve_exact(bytes.len()).map_err(|error| {
    NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_result_allocation", format!("cannot allocate {label}: {error}"))
  })?;
  cloned.extend_from_slice(bytes);
  Ok(cloned)
}

fn try_clone_selected_string(value: &str, label: &'static str) -> Result<String, NativeSelectedNamespaceReadErrorV1> {
  String::from_utf8(try_clone_selected_bytes(value.as_bytes(), label)?).map_err(|error| {
    NativeSelectedNamespaceReadErrorV1::corrupt("selected_namespace_result_encoding", format!("cannot retain {label}: {error}"))
  })
}

fn selected_namespace_row_heap_bytes(row: &NativeSelectedNamespaceFileRowV1) -> Result<u64, NativeSelectedNamespaceReadErrorV1> {
  let record = &row.file_record;
  let chunk_vector_bytes = record
    .chunk_hashes
    .capacity()
    .checked_mul(size_of::<Vec<u8>>())
    .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_retained_bytes", "chunk vector size overflowed"))?;
  let mut bytes = 0u64;
  for capacity in [
    row.file_key.capacity(),
    row.record_revision.capacity(),
    record.path.capacity(),
    record.content_type.as_ref().map_or(0, String::capacity),
    record.metadata.capacity(),
    record.content_hash.capacity(),
    chunk_vector_bytes,
  ] {
    bytes = bytes
      .checked_add(capacity as u64)
      .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_retained_bytes", "row size overflowed"))?;
  }
  for chunk_hash in &record.chunk_hashes {
    bytes = bytes
      .checked_add(chunk_hash.capacity() as u64)
      .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_retained_bytes", "row size overflowed"))?;
  }
  Ok(bytes)
}

fn selected_namespace_identity_retained_bytes(
  found: Option<&NativeSelectedNamespaceFileRowV1>,
  root_capacities: [usize; 3],
) -> Result<u64, NativeSelectedNamespaceReadErrorV1> {
  let mut bytes = (size_of::<NativeSelectedNamespaceIdentityResultV1>() as u64)
    .checked_add(root_capacities[0] as u64)
    .and_then(|total| total.checked_add(root_capacities[1] as u64))
    .and_then(|total| total.checked_add(root_capacities[2] as u64))
    .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_retained_bytes", "identity size overflowed"))?;
  if let Some(found) = found {
    bytes = bytes
      .checked_add(selected_namespace_row_heap_bytes(found)?)
      .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_retained_bytes", "identity row size overflowed"))?;
  }
  Ok(bytes)
}

fn selected_namespace_page_retained_bytes(
  rows: &[NativeSelectedNamespaceFileRowV1],
  rows_capacity: usize,
  pending: Option<&NativeSelectedNamespaceFileRowV1>,
  root_capacities: [usize; 3],
  resume_capacity: usize,
) -> Result<u64, NativeSelectedNamespaceReadErrorV1> {
  let row_capacity_bytes = rows_capacity
    .checked_mul(size_of::<NativeSelectedNamespaceFileRowV1>())
    .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_retained_bytes", "page row capacity overflowed"))?;
  let mut bytes = (size_of::<NativeSelectedNamespacePageV1>() as u64)
    .checked_add(row_capacity_bytes as u64)
    .and_then(|total| total.checked_add(root_capacities[0] as u64))
    .and_then(|total| total.checked_add(root_capacities[1] as u64))
    .and_then(|total| total.checked_add(root_capacities[2] as u64))
    .and_then(|total| total.checked_add(resume_capacity as u64))
    .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_retained_bytes", "page size overflowed"))?;
  for row in rows.iter().chain(pending) {
    bytes = bytes
      .checked_add(selected_namespace_row_heap_bytes(row)?)
      .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_namespace_retained_bytes", "page row size overflowed"))?;
  }
  Ok(bytes)
}

fn selected_source_receipt_maximum_bytes(hash_width: usize) -> Result<u64, NativeSelectedNamespaceReadErrorV1> {
  let width = u64::try_from(hash_width)
    .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_source_receipt_bytes", error.to_string()))?;
  width
    .checked_mul(6)
    .and_then(|bytes| bytes.checked_add(SELECTED_SOURCE_RECEIPT_FIXED_BYTES))
    .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_source_receipt_bytes", "source receipt bound overflowed"))
}

fn selected_source_receipt_retained_bytes(capacities: [usize; 6]) -> Result<u64, NativeSelectedNamespaceReadErrorV1> {
  capacities.into_iter().try_fold(SELECTED_SOURCE_RECEIPT_FIXED_BYTES, |total, capacity| {
    let capacity = u64::try_from(capacity)
      .map_err(|error| NativeSelectedNamespaceReadErrorV1::resource("selected_source_receipt_bytes", error.to_string()))?;
    total
      .checked_add(capacity)
      .ok_or_else(|| NativeSelectedNamespaceReadErrorV1::resource("selected_source_receipt_bytes", "source receipt size overflowed"))
  })
}

fn map_selected_source_evaluator_error(error: AuthoritativeSourceEvaluationErrorV1) -> NativeSelectedNamespaceReadErrorV1 {
  match error {
    AuthoritativeSourceEvaluationErrorV1::InvalidConfiguration { code, context } => {
      NativeSelectedNamespaceReadErrorV1::corrupt(code, context)
    }
    AuthoritativeSourceEvaluationErrorV1::Cancelled => NativeSelectedNamespaceReadErrorV1::cancelled(),
    AuthoritativeSourceEvaluationErrorV1::ResourcePressure(context) => {
      NativeSelectedNamespaceReadErrorV1::resource("selected_source_memory", context)
    }
    AuthoritativeSourceEvaluationErrorV1::Parser(error) => match error.class() {
      super::index_producer_collector::IndexParserExecutionErrorClassV1::Cancelled => NativeSelectedNamespaceReadErrorV1::cancelled(),
      super::index_producer_collector::IndexParserExecutionErrorClassV1::DependencyUnavailable => {
        NativeSelectedNamespaceReadErrorV1::unavailable(error.code(), error.context())
      }
      super::index_producer_collector::IndexParserExecutionErrorClassV1::HostFailure
        if error.code().starts_with("selected_source_corrupt_") =>
      {
        NativeSelectedNamespaceReadErrorV1::corrupt(error.code(), error.context())
      }
      super::index_producer_collector::IndexParserExecutionErrorClassV1::HostFailure
        if error.code().starts_with("selected_source_resource_") =>
      {
        NativeSelectedNamespaceReadErrorV1::resource(error.code(), error.context())
      }
      super::index_producer_collector::IndexParserExecutionErrorClassV1::HostFailure => {
        NativeSelectedNamespaceReadErrorV1::unavailable(error.code(), error.context())
      }
    },
    AuthoritativeSourceEvaluationErrorV1::Source(error) => match error.class() {
      super::index_source::SourceOperationalErrorClassV1::Cancelled => NativeSelectedNamespaceReadErrorV1::cancelled(),
      super::index_source::SourceOperationalErrorClassV1::DependencyUnavailable => {
        NativeSelectedNamespaceReadErrorV1::unavailable(error.code(), error.context())
      }
      super::index_source::SourceOperationalErrorClassV1::HostFailure => {
        NativeSelectedNamespaceReadErrorV1::unavailable(error.code(), error.context())
      }
    },
  }
}

fn map_selected_body_authorization_error(error: ReadViewAuthorizationFailureV1) -> IndexParserExecutionErrorV1 {
  match error {
    ReadViewAuthorizationFailureV1::Canceled => {
      IndexParserExecutionErrorV1::cancelled("selected_source_cancelled_during_body", error.to_string())
    }
    ReadViewAuthorizationFailureV1::Denied | ReadViewAuthorizationFailureV1::Corrupt(_) => {
      IndexParserExecutionErrorV1::host_failure("selected_source_corrupt_entity", error.to_string())
    }
    ReadViewAuthorizationFailureV1::Unavailable(_) => {
      IndexParserExecutionErrorV1::host_failure("selected_source_unavailable_entity", error.to_string())
    }
  }
}

fn permission_document_path(level: &str) -> String {
  join_path(level, ".aeordb-permissions")
}

fn normalize_navigation_path(path: &str) -> String {
  let normalized = normalize_permission_path(path);
  if normalized == "/" {
    normalized
  } else {
    normalized.trim_end_matches('/').to_string()
  }
}

fn join_path(parent: &str, child: &str) -> String {
  if parent == "/" {
    format!("/{child}")
  } else {
    format!("{}/{child}", parent.trim_end_matches('/'))
  }
}

fn path_depth(path: &str) -> usize {
  path.split('/').filter(|segment| !segment.is_empty()).count()
}

fn directory_child(hash: Vec<u8>, name: String) -> ChildEntry {
  ChildEntry {
    entry_type: EntryType::DirectoryIndex.to_u8(),
    hash,
    total_size: 0,
    created_at: 0,
    updated_at: 0,
    name,
    content_type: None,
    virtual_time: 0,
    node_id: 0,
  }
}

fn decode_canonical_btree_node(
  entity: &LoadedImmutableEntityV1,
  hash_width: usize,
  path: &str,
) -> Result<BTreeNode, ReadViewAuthorizationFailureV1> {
  let node =
    BTreeNode::deserialize(&entity.stored_value, hash_width, entity.entity_version).map_err(|error| selected_corrupt(path, error))?;
  let canonical = node.serialize(hash_width).map_err(|error| selected_corrupt(path, error))?;
  if canonical != entity.stored_value {
    return Err(selected_corrupt(path, "selected B-tree node is not canonically encoded"));
  }
  match &node {
    BTreeNode::Leaf(leaf) => {
      if leaf.entries.len() > BTREE_MAX_LEAF_ENTRIES {
        return Err(selected_corrupt(path, "selected B-tree leaf exceeds its canonical fanout"));
      }
      validate_sorted_children(&leaf.entries, path)?;
    }
    BTreeNode::Internal(internal) => {
      if internal.keys.is_empty() || internal.keys.len() > BTREE_MAX_INTERNAL_KEYS {
        return Err(selected_corrupt(path, "selected B-tree internal node has noncanonical fanout"));
      }
      for pair in internal.keys.windows(2) {
        if pair[0] >= pair[1] {
          return Err(selected_corrupt(path, "selected B-tree separator keys are not strictly increasing"));
        }
      }
      if internal.children.iter().any(|child| child.iter().all(|byte| *byte == 0)) {
        return Err(selected_corrupt(path, "selected B-tree contains a zero child identity"));
      }
    }
  }
  Ok(node)
}

fn validate_sorted_children(children: &[ChildEntry], path: &str) -> Result<(), ReadViewAuthorizationFailureV1> {
  for pair in children.windows(2) {
    validate_child_order(Some(&pair[0].name), &pair[1].name).map_err(|error| selected_corrupt(path, error))?;
  }
  Ok(())
}

fn validate_child_order(previous: Option<&str>, current: &str) -> Result<(), ReadViewAuthorizationFailureV1> {
  if previous.is_some_and(|previous| previous >= current) {
    Err(selected_corrupt(current, "selected directory child names are not strictly increasing"))
  } else {
    Ok(())
  }
}

fn collect_descendant_children(
  links: &[PermissionLink],
  current_groups: &[String],
  parent_path: &str,
  document_directory: &str,
  output: &mut BTreeSet<String>,
) {
  for link in links {
    if !current_groups.contains(&link.group) {
      continue;
    }
    let target = link.path_pattern.as_ref().map_or_else(|| document_directory.to_string(), |name| join_path(document_directory, name));
    if let Some(child) = next_segment_below(parent_path, &target) {
      output.insert(child.to_string());
    }
  }
}

fn next_segment_below<'a>(parent: &str, target: &'a str) -> Option<&'a str> {
  let parent = if parent == "/" { "" } else { parent.trim_end_matches('/') };
  let suffix = target.strip_prefix(parent)?;
  if !suffix.starts_with('/') {
    return None;
  }
  let remainder = &suffix[1..];
  (!remainder.is_empty()).then(|| remainder.split('/').next()).flatten()
}

fn deserialize_file_record_v0_v1(encoded: &[u8], hash_algorithm: HashAlgorithm, entity_version: u8) -> Result<FileRecord, String> {
  FileRecord::deserialize(encoded, hash_algorithm.hash_length(), entity_version).map_err(|error| error.to_string())
}

fn ensure_selected_not_cancelled(cancellation: &CancellationToken) -> Result<(), ReadViewAuthorizationFailureV1> {
  if cancellation.is_cancelled() {
    Err(ReadViewAuthorizationFailureV1::Canceled)
  } else {
    Ok(())
  }
}

fn map_selected_authority_error(error: FirstAuthorityPublicationErrorV1) -> ReadViewAuthorizationFailureV1 {
  if error.code() == "captured_authority_cancelled" {
    ReadViewAuthorizationFailureV1::Canceled
  } else if authority_error_is_unavailable(&error) {
    ReadViewAuthorizationFailureV1::Unavailable(error.to_string())
  } else {
    ReadViewAuthorizationFailureV1::Corrupt(error.to_string())
  }
}

fn selected_corrupt(path: &str, error: impl std::fmt::Display) -> ReadViewAuthorizationFailureV1 {
  ReadViewAuthorizationFailureV1::Corrupt(format!("selected permission authority at {path}: {error}"))
}

fn selected_unavailable(path: &str, error: impl std::fmt::Display) -> ReadViewAuthorizationFailureV1 {
  ReadViewAuthorizationFailureV1::Unavailable(format!("selected permission authority at {path}: {error}"))
}

fn authority_retained_bytes(authority: &ImmutableNamespaceAuthorityV1) -> Result<u64, ReadViewSourceErrorV1> {
  let mut bytes = AUTHORITY_RETAINED_BASE_BYTES
    .checked_add(size_of::<ImmutableNamespaceAuthorityV1>() as u64)
    .ok_or_else(|| ReadViewSourceErrorV1::Memory("authority retained-size overflow".to_string()))?;
  for value in [
    &authority.root.root_hash,
    &authority.root.namespace_tree_root,
    &authority.root.semantic_state_root,
    &authority.namespace_tree.root_hash,
    &authority.semantic_state.object_id,
    &authority.admission.namespace_root,
    &authority.admission.authority_identity_digest,
    &authority.admission.authority_after,
    &authority.admission.prepare_payload_hash,
  ] {
    bytes = bytes
      .checked_add(value.capacity() as u64)
      .ok_or_else(|| ReadViewSourceErrorV1::Memory("authority retained-size overflow".to_string()))?;
  }
  bytes = bytes
    .checked_add((authority.namespace_tree.edges.capacity() * size_of::<NamespaceTreeEdgeV0>()) as u64)
    .ok_or_else(|| ReadViewSourceErrorV1::Memory("authority retained-size overflow".to_string()))?;
  for edge in &authority.namespace_tree.edges {
    let edge_bytes = match edge {
      NamespaceTreeEdgeV0::Entry { name, identity, .. } => name.capacity().saturating_add(identity.capacity()),
      NamespaceTreeEdgeV0::BTreeNode { identity } => identity.capacity(),
    };
    bytes =
      bytes.checked_add(edge_bytes as u64).ok_or_else(|| ReadViewSourceErrorV1::Memory("authority retained-size overflow".to_string()))?;
  }
  if let SemanticAvailabilityV1::Complete { compiler_fingerprint, semantic_registry_fingerprint, catalog_root, .. } =
    &authority.semantic_state.availability
  {
    for value in [compiler_fingerprint, semantic_registry_fingerprint, catalog_root] {
      bytes = bytes
        .checked_add(value.capacity() as u64)
        .ok_or_else(|| ReadViewSourceErrorV1::Memory("authority retained-size overflow".to_string()))?;
    }
  }
  if bytes > AUTHORITY_PEAK_RESERVATION_BYTES {
    return Err(ReadViewSourceErrorV1::Memory(format!(
      "retained authority requires {bytes} bytes, exceeding its {AUTHORITY_PEAK_RESERVATION_BYTES}-byte admitted peak",
    )));
  }
  Ok(bytes)
}

fn map_header_error(error: FirstAuthorityPublicationErrorV1) -> ReadViewSourceErrorV1 {
  if authority_error_is_unavailable(&error) {
    ReadViewSourceErrorV1::HeaderUnavailable(error.to_string())
  } else {
    ReadViewSourceErrorV1::HeaderCorrupt(error.to_string())
  }
}

fn map_authority_error(error: FirstAuthorityPublicationErrorV1) -> ReadViewSourceErrorV1 {
  if error.code() == "captured_authority_cancelled" {
    ReadViewSourceErrorV1::Canceled
  } else if authority_error_is_unavailable(&error) {
    ReadViewSourceErrorV1::AuthorityUnavailable(error.to_string())
  } else {
    ReadViewSourceErrorV1::AuthorityCorrupt(error.to_string())
  }
}

fn authority_error_is_unavailable(error: &FirstAuthorityPublicationErrorV1) -> bool {
  matches!(error.code(), "engine_failure" | "native_io_failure" | "durability_failure")
}

fn map_lifecycle_error(error: RootLifecyclePointReadErrorV1) -> ReadViewLifecycleErrorV1 {
  match error {
    RootLifecyclePointReadErrorV1::Canceled => ReadViewLifecycleErrorV1::Canceled,
    RootLifecyclePointReadErrorV1::Memory(source) => ReadViewLifecycleErrorV1::Memory(source.to_string()),
    RootLifecyclePointReadErrorV1::Authority(source) if authority_error_is_unavailable(&source) => {
      ReadViewLifecycleErrorV1::Unavailable(source.to_string())
    }
    RootLifecyclePointReadErrorV1::Authority(source) => ReadViewLifecycleErrorV1::Corrupt(source.to_string()),
    RootLifecyclePointReadErrorV1::Invalid { code, message } => ReadViewLifecycleErrorV1::Corrupt(format!("{code}: {message}")),
    RootLifecyclePointReadErrorV1::Format(source) => ReadViewLifecycleErrorV1::Corrupt(source.to_string()),
  }
}
