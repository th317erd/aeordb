use std::collections::BTreeSet;
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
use crate::engine::{CompressionAlgorithm, EntryType};

use super::database_header::SelectedDatabaseHeaderV4;
use super::entity::EntryTypeV4;
use super::first_authority::{
  FirstAuthorityPublicationErrorV1, LoadedImmutableEntityV1, RootLifecyclePointReadErrorV1, V4FirstAuthorityPublisher,
};
use super::hash::digest_parts;
use super::namespace::{NamespaceTreeEdgeV0, SemanticAvailabilityV1};
use super::read_view::{
  LoadedReadAuthorityV1, ReadViewAuthoritySourceV1, ReadViewAuthorizationFailureV1, ReadViewLifecycleErrorV1, ReadViewSourceErrorV1,
  ResolvedReadViewV1, RootLifecycleObservationV1,
};
use super::read_view_authorization::{
  PathAuthorizationDecisionV1, ResolvedPathAuthorizationV1, SelectedRootPermissionRequestV1, SelectedRootPermissionSourceV1,
};
use super::root_authority::ImmutableNamespaceAuthorityV1;

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
  file_record: FileRecord,
}

impl NativeSelectedNamespaceFileRowV1 {
  pub fn file_key(&self) -> &[u8] {
    &self.file_key
  }

  pub fn record_revision(&self) -> &[u8] {
    &self.record_revision
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

impl NativeSelectedNamespaceReaderV1<'_> {
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
      file_record: loaded.record,
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

struct SelectedNamespaceIdentityStateV1<'request> {
  file_key: &'request [u8],
  record_revision: &'request [u8],
  visited_documents: u64,
  work_steps: u64,
  found: Option<NativeSelectedNamespaceFileRowV1>,
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
    let record = FileRecord::deserialize(&entity.stored_value, header.header.hash_algorithm.hash_length(), entity.entity_version)
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
