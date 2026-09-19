//! Captured namespace configuration inputs, separate from protected staging.
use super::*;
#[path = "semantic_source_union_native.rs"]
mod source_union;
pub use source_union::{NativeSemanticSourceUnionValidationBoundsV1, SemanticSourceUnionValidationSummaryV1};
pub use source_union::{
  NativeSemanticSourceReplacementV1, NativeSemanticSourceUnionBoundsV1, NativeSemanticSourceUnionErrorV1,
  NativeSemanticSourceUnionRequestV1, NativeSemanticSourceUnionV1,
};
use crate::engine::btree::BTreeNode;
use crate::engine::v4::namespace_seek::{
  LoadedNamespaceSeekNodeV1, NamespaceSeekFailureV1, namespace_seek_workspace_bytes_v1, next_namespace_child_by_path_v1,
  seek_namespace_child_v1,
};
use crate::engine::v4::read_view_native::{
  MAX_DIRECTORY_ENTITY_BYTES, NativeSelectedNamespaceReadErrorV1, decode_validated_selected_directory_node, join_selected_path,
  validate_selected_directory_entity, validate_selected_file_record_metadata,
};

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticNamespaceSourceBoundsV1 {
  pub maximum_path_bytes: usize,
  pub maximum_path_depth: usize,
  pub maximum_btree_depth: usize,
  pub maximum_directory_entity_bytes: usize,
  /// Charge physical reads, decoded directory elements and selected children.
  pub maximum_work: u64,
  /// Body/chunk limits are per file; read bytes span all directories and files.
  pub sources: NativeSemanticSourceReadBoundsV1,
}

#[derive(Clone, Copy, Debug)]
pub struct NativeSemanticNamespaceSourceRequestV1<'a> {
  /// Exact DirectoryIndex content identity, not root admission or permission.
  pub tree_root: &'a [u8],
  pub bounds: NativeSemanticNamespaceSourceBoundsV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSemanticNamespaceSourceSummaryV1 {
  pub configurations: u64,
  pub complete: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeSemanticNamespaceSourceErrorV1 {
  #[error(transparent)]
  Source(#[from] SemanticMutationObservationErrorV1),
  #[error(transparent)]
  Directory(#[from] NativeSelectedNamespaceReadErrorV1),
}

impl NativeSemanticNamespaceSourceErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Source(error) => error.code(),
      Self::Directory(error) => error.code(),
    }
  }
}

impl From<NamespaceSeekFailureV1> for NativeSemanticNamespaceSourceErrorV1 {
  fn from(error: NamespaceSeekFailureV1) -> Self {
    Self::Directory(error.into())
  }
}

/// Exact namespace bytes never expose protected-source staging operations.
pub struct NativeSemanticNamespaceSourceV1<'a> {
  _capture: &'a NativeSemanticMutationInventoryV1<'a>,
  value: DecodedSemanticSourceV1,
}

impl NativeSemanticNamespaceSourceV1<'_> {
  pub fn record(&self) -> &FileRecord {
    &self.value.record
  }
  pub fn encoded_record(&self) -> &[u8] {
    &self.value.encoded_record
  }
  pub fn body(&self) -> &[u8] {
    &self.value.body
  }
  pub fn revision(&self) -> &[u8] {
    &self.value.revision
  }
  pub fn entity_version(&self) -> u8 {
    self.value.entity_version
  }
  pub fn flags(&self) -> u8 {
    self.value.flags
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  pub fn read_namespace_configuration_source(
    &self,
    path: &str,
    revision: &[u8],
    bounds: NativeSemanticSourceReadBoundsV1,
  ) -> Result<NativeSemanticNamespaceSourceV1<'_>, NativeSemanticNamespaceSourceErrorV1> {
    self.read_namespace_source_from_lookup(path, revision, bounds, &self.source_lookup(bounds))
  }

  fn read_namespace_source_from_lookup(
    &self,
    path: &str,
    revision: &[u8],
    bounds: NativeSemanticSourceReadBoundsV1,
    lookup: &impl FirstAuthorityEntityLookupV1,
  ) -> Result<NativeSemanticNamespaceSourceV1<'_>, NativeSemanticNamespaceSourceErrorV1> {
    let value = self
      .read_decoded_source_from_lookup(path, Some(revision), bounds, lookup, SemanticSourceKindV1::Namespace, || {})
      .map_err(|error| match error {
        SemanticMutationObservationErrorV1::Authority(source) => map_namespace_read_error(source),
        other => other,
      })?
      .ok_or_else(|| invalid("semantic_namespace_source_missing", "captured namespace source is missing"))?;
    Ok(NativeSemanticNamespaceSourceV1 { _capture: self, value })
  }

  /// Callbacks are provisional until complete=true. This visits configurations,
  /// not every ordinary file's dependency closure or user read permissions.
  pub fn visit_namespace_configuration_sources(
    &self,
    request: NativeSemanticNamespaceSourceRequestV1<'_>,
    visitor: impl FnMut(&NativeSemanticNamespaceSourceV1<'_>) -> Result<bool, NativeSemanticNamespaceSourceErrorV1>,
  ) -> Result<NativeSemanticNamespaceSourceSummaryV1, NativeSemanticNamespaceSourceErrorV1> {
    self.visit_namespace_configuration_sources_observed(request, visitor, || {})
  }

  pub(crate) fn visit_namespace_configuration_sources_observed(
    &self,
    request: NativeSemanticNamespaceSourceRequestV1<'_>,
    mut visitor: impl FnMut(&NativeSemanticNamespaceSourceV1<'_>) -> Result<bool, NativeSemanticNamespaceSourceErrorV1>,
    before_complete: impl FnOnce(),
  ) -> Result<NativeSemanticNamespaceSourceSummaryV1, NativeSemanticNamespaceSourceErrorV1> {
    let mut cursor = self.open_namespace_configuration_cursor(request)?;
    let mut configurations = 0u64;
    while let Some(source) = cursor.next_source()? {
      configurations = configurations
        .checked_add(1)
        .ok_or_else(|| invalid("semantic_namespace_source_count", "namespace configuration count overflowed"))?;
      let keep_going = visitor(&source)?;
      let operation = &cursor.operation;
      operation.check()?;
      if !keep_going {
        before_complete();
        operation.check()?;
        return Ok(NativeSemanticNamespaceSourceSummaryV1 { configurations, complete: false });
      }
    }
    let operation = &cursor.operation;
    before_complete();
    operation.check()?;
    Ok(NativeSemanticNamespaceSourceSummaryV1 { configurations, complete: true })
  }
}

struct NamespaceDirectoryFrameV1 {
  path: String,
  hash: Vec<u8>,
  lower: Option<String>,
}

/// Pausable traversal over one captured tree, not admission or complete capture.
/// Rows retain their source capture independently of this cursor. Errors are
/// terminal; an incomplete prefix never proves absence or completed traversal.
pub struct NativeSemanticNamespaceSourceCursorV1<'a> {
  operation: NamespaceSourceOperationV1<'a>,
  state: NamespaceSourceStateV1,
}

struct NamespaceSourceStateV1 {
  stack: Vec<NamespaceDirectoryFrameV1>,
  failed: bool,
}

impl<'a> NativeSemanticNamespaceSourceCursorV1<'a> {
  pub fn next_source(&mut self) -> Result<Option<NativeSemanticNamespaceSourceV1<'a>>, NativeSemanticNamespaceSourceErrorV1> {
    self.state.next_source(&self.operation)
  }
}

impl NamespaceSourceStateV1 {
  fn new<A: NamespaceReadAdmissionV1>(
    operation: &NamespaceSourceOperationV1<'_, A>,
    tree_root: &[u8],
  ) -> Result<Self, NativeSemanticNamespaceSourceErrorV1> {
    operation.check()?;
    if tree_root.len() != operation.algorithm().hash_length() || tree_root.iter().all(|byte| *byte == 0) {
      return Err(
        invalid("semantic_namespace_source_root", "namespace traversal requires a nonzero selected-width directory identity").into(),
      );
    }
    let mut stack = Vec::new();
    stack.try_reserve_exact(operation.bounds.maximum_path_depth + 1).map_err(namespace_allocation)?;
    stack.push(NamespaceDirectoryFrameV1 { path: String::from("/"), hash: copy_namespace_bytes(tree_root)?, lower: None });
    operation.check()?;
    Ok(Self { stack, failed: false })
  }

  /// Seed only ancestors of the exclusive full-file bound. A missing/non-
  /// directory component is a valid suffix boundary, not source-prefix proof.
  fn seek_after<A: NamespaceReadAdmissionV1>(
    &mut self,
    operation: &NamespaceSourceOperationV1<'_, A>,
    after_path: &str,
  ) -> Result<(), NativeSemanticNamespaceSourceErrorV1> {
    let mut relative =
      after_path.strip_prefix('/').ok_or_else(|| invalid("semantic_namespace_source_path", "seek bound is not absolute"))?;
    while let Some(frame) = self.stack.last_mut() {
      operation.check()?;
      frame.lower = Some(copy_namespace_path(relative)?);
      let Some((component, remaining)) = relative.split_once('/') else {
        break;
      };
      let child =
        seek_namespace_child_v1(&frame.hash, component, true, operation.bounds.maximum_btree_depth, |hash, lower, upper, btree_child| {
          operation.load_node(hash, &frame.path, lower, upper, btree_child)
        })?;
      let Some(child) = child.filter(|child| child.name == component && child.entry_type == EntryTypeV4::DirectoryIndex.to_u8()) else {
        break;
      };
      operation.lookup.charge_work(1).map_err(map_namespace_read_error)?;
      let path = join_selected_path(&frame.path, &child.name, operation.bounds.maximum_path_bytes)?;
      if self.stack.len() >= operation.bounds.maximum_path_depth {
        return Err(resource("semantic_namespace_source_depth", "namespace seek exceeds its path-depth bound").into());
      }
      if self.stack.iter().any(|ancestor| ancestor.hash == child.hash) {
        return Err(invalid("semantic_namespace_source_cycle", "namespace directory repeats an ancestor").into());
      }
      self.stack.push(NamespaceDirectoryFrameV1 { path, hash: child.hash, lower: None });
      relative = remaining;
    }
    operation.check()?;
    Ok(())
  }

  fn next_source<'a, A: NamespaceReadAdmissionV1>(
    &mut self,
    operation: &NamespaceSourceOperationV1<'a, A>,
  ) -> Result<Option<NativeSemanticNamespaceSourceV1<'a>>, NativeSemanticNamespaceSourceErrorV1> {
    if self.failed {
      return Err(invalid("semantic_namespace_cursor_failed", "namespace source cursor cannot continue after failure").into());
    }
    let result = self.next_source_inner(operation);
    match result {
      Ok(value) => Ok(value),
      Err(error) => {
        self.failed = true;
        Err(error)
      }
    }
  }

  fn next_source_inner<'a, A: NamespaceReadAdmissionV1>(
    &mut self,
    operation: &NamespaceSourceOperationV1<'a, A>,
  ) -> Result<Option<NativeSemanticNamespaceSourceV1<'a>>, NativeSemanticNamespaceSourceErrorV1> {
    operation.check()?;
    while let Some(frame) = self.stack.last_mut() {
      operation.check()?;
      let next = next_namespace_child_by_path_v1(frame.lower.as_deref(), |name, inclusive| {
        seek_namespace_child_v1(&frame.hash, name, inclusive, operation.bounds.maximum_btree_depth, |hash, lower, upper, btree_child| {
          operation.load_node(hash, &frame.path, lower, upper, btree_child)
        })
      })?;
      let Some((key, child)) = next else {
        self.stack.pop();
        continue;
      };
      operation.lookup.charge_work(1).map_err(map_namespace_read_error)?;
      let path = join_selected_path(&frame.path, &child.name, operation.bounds.maximum_path_bytes)?;
      frame.lower = Some(key);
      if self.stack.len() > operation.bounds.maximum_path_depth {
        return Err(resource("semantic_namespace_source_depth", "namespace source traversal exceeds its path-depth bound").into());
      }
      if is_namespace_source_path(&path, operation.algorithm())? {
        if child.entry_type != EntryTypeV4::FileRecord.to_u8() {
          return Err(invalid("semantic_namespace_source_role", "namespace configuration path is not a FileRecord").into());
        }
        let capture = operation.capture;
        let source = capture.read_namespace_source_from_lookup(&path, &child.hash, operation.bounds.sources, &operation.lookup)?;
        validate_selected_file_record_metadata(source.record(), &child, &path).map_err(NativeSelectedNamespaceReadErrorV1::from)?;
        operation.check()?;
        return Ok(Some(source));
      }
      if child.entry_type == EntryTypeV4::DirectoryIndex.to_u8() {
        if self.stack.iter().any(|ancestor| ancestor.hash == child.hash) {
          return Err(invalid("semantic_namespace_source_cycle", "namespace directory repeats an ancestor").into());
        }
        self.stack.push(NamespaceDirectoryFrameV1 { path, hash: child.hash, lower: None });
      }
    }
    operation.check()?;
    Ok(None)
  }
}

impl NativeSemanticMutationInventoryV1<'_> {
  pub fn open_namespace_configuration_cursor(
    &self,
    request: NativeSemanticNamespaceSourceRequestV1<'_>,
  ) -> Result<NativeSemanticNamespaceSourceCursorV1<'_>, NativeSemanticNamespaceSourceErrorV1> {
    let operation = NamespaceSourceOperationV1::new(self, request)?;
    let state = NamespaceSourceStateV1::new(&operation, request.tree_root)?;
    Ok(NativeSemanticNamespaceSourceCursorV1 { operation, state })
  }

  /// Start strictly after a canonical namespace configuration FILE path,
  /// even if that file or one of its ancestors is absent from this capture.
  /// The bound is not a configuration-owner path or proof of processed work.
  /// Seeks and later reads share one budget and never consult live locators.
  pub fn open_namespace_configuration_cursor_after(
    &self,
    request: NativeSemanticNamespaceSourceRequestV1<'_>,
    after_path: &str,
  ) -> Result<NativeSemanticNamespaceSourceCursorV1<'_>, NativeSemanticNamespaceSourceErrorV1> {
    let operation = NamespaceSourceOperationV1::new(self, request)?;
    validate_namespace_source_path(after_path, operation.algorithm())?;
    if after_path.len() > operation.bounds.maximum_path_bytes {
      return Err(resource("semantic_namespace_source_path_bound", "namespace seek exceeds its path-byte bound").into());
    }
    if after_path.split('/').skip(1).count() > operation.bounds.maximum_path_depth {
      return Err(resource("semantic_namespace_source_depth", "namespace seek exceeds its path-depth bound").into());
    }
    let mut state = NamespaceSourceStateV1::new(&operation, request.tree_root)?;
    state.seek_after(&operation, after_path)?;
    Ok(NativeSemanticNamespaceSourceCursorV1 { operation, state })
  }
}

// Generic only over an additional enclosing read allowance. Standalone cursors
// retain their original owned budget and auto traits; they do not acquire a
// shared Cell/dynamic callback merely because another caller composes budgets.
trait NamespaceReadAdmissionV1 {
  fn admit(&self, locator: &KVEntry) -> Result<(), FirstAuthorityPublicationErrorV1>;
}

impl NamespaceReadAdmissionV1 for () {
  fn admit(&self, _locator: &KVEntry) -> Result<(), FirstAuthorityPublicationErrorV1> {
    Ok(())
  }
}

impl<T: FirstAuthorityEntityLookupV1> NamespaceReadAdmissionV1 for &T {
  fn admit(&self, locator: &KVEntry) -> Result<(), FirstAuthorityPublicationErrorV1> {
    FirstAuthorityEntityLookupV1::admit_read(*self, locator)
  }
}

struct NamespaceSourceLookupV1<'a, A> {
  captured: CapturedEntityLookupV1<'a>,
  remaining_work: Cell<u64>,
  additional_read_admission: A,
}

impl<A> NamespaceSourceLookupV1<'_, A> {
  fn charge_work(&self, amount: u64) -> Result<(), FirstAuthorityPublicationErrorV1> {
    let remaining = self.remaining_work.get().checked_sub(amount).ok_or_else(|| {
      FirstAuthorityPublicationErrorV1::invalid(
        "semantic_namespace_source_work_bound",
        "namespace source traversal exhausted its work bound",
      )
    })?;
    self.remaining_work.set(remaining);
    Ok(())
  }
}

impl<A: NamespaceReadAdmissionV1> FirstAuthorityEntityLookupV1 for NamespaceSourceLookupV1<'_, A> {
  fn get(&self, key: &[u8]) -> Result<Option<KVEntry>, EngineError> {
    self.captured.get(key)
  }
  fn hash_algo(&self) -> HashAlgorithm {
    self.captured.hash_algo()
  }
  fn admit_read(&self, locator: &KVEntry) -> Result<(), FirstAuthorityPublicationErrorV1> {
    self.charge_work(1)?;
    self.captured.admit_read(locator)?;
    self.additional_read_admission.admit(locator)
  }
}

struct NamespaceSourceOperationV1<'a, A = ()> {
  capture: &'a NativeSemanticMutationInventoryV1<'a>,
  bounds: NativeSemanticNamespaceSourceBoundsV1,
  lookup: NamespaceSourceLookupV1<'a, A>,
  _memory: MemoryReservation,
}

impl<'a> NamespaceSourceOperationV1<'a> {
  fn new(
    capture: &'a NativeSemanticMutationInventoryV1<'a>,
    request: NativeSemanticNamespaceSourceRequestV1<'_>,
  ) -> Result<Self, SemanticMutationObservationErrorV1> {
    Self::with_read_admission(capture, request, ())
  }
}

impl<'a, A: NamespaceReadAdmissionV1> NamespaceSourceOperationV1<'a, A> {
  fn with_read_admission(
    capture: &'a NativeSemanticMutationInventoryV1<'a>,
    request: NativeSemanticNamespaceSourceRequestV1<'_>,
    additional_read_admission: A,
  ) -> Result<Self, SemanticMutationObservationErrorV1> {
    check_cancelled(&capture.cancellation)?;
    capture._memory.check_admission()?;
    let bounds = request.bounds;
    validate_source_read_bounds(bounds.sources)?;
    if bounds.maximum_path_bytes == 0
      || bounds.maximum_path_bytes > u16::MAX as usize
      || bounds.maximum_path_depth == 0
      || bounds.maximum_path_depth > 256
      || bounds.maximum_btree_depth == 0
      || bounds.maximum_btree_depth > 256
      || bounds.maximum_directory_entity_bytes == 0
      || bounds.maximum_directory_entity_bytes > MAX_DIRECTORY_ENTITY_BYTES
      || bounds.maximum_work == 0
    {
      return Err(invalid("semantic_namespace_source_bounds", "namespace source traversal requires valid bounded paths, nodes and work"));
    }
    let header = &capture.header.selected.header;
    if request.tree_root.len() != header.hash_algorithm.hash_length() || request.tree_root.iter().all(|byte| *byte == 0) {
      return Err(invalid("semantic_namespace_source_root", "namespace traversal requires a nonzero selected-width directory identity"));
    }
    let scratch = namespace_seek_workspace_bytes_v1(
      bounds.maximum_path_bytes as u64,
      bounds.maximum_path_depth as u64,
      bounds.maximum_btree_depth as u64,
      header.hash_algorithm.hash_length() as u64,
    )
    .ok_or_else(|| resource("semantic_namespace_source_memory", "namespace source workspace accounting overflowed"))?;
    let memory = capture.memory.reserve(MemoryOwner::Task, scratch, AdmissionClass::Maintenance)?;
    let lookup = NamespaceSourceLookupV1 {
      captured: CapturedEntityLookupV1 {
        snapshot: &capture.snapshot,
        header,
        bounds: NativeSemanticMutationInventoryBoundsV1 {
          maximum_work: bounds.maximum_work,
          maximum_entity_bytes: MAXIMUM_FILE_RECORD_BYTES
            .max(bounds.maximum_directory_entity_bytes)
            .max(bounds.sources.maximum_chunk_entity_bytes),
          maximum_read_bytes: bounds.sources.maximum_read_bytes,
        },
        cancellation: &capture.cancellation,
        remaining_read_bytes: Cell::new(bounds.sources.maximum_read_bytes),
      },
      remaining_work: Cell::new(bounds.maximum_work),
      additional_read_admission,
    };
    Ok(Self { capture, bounds, lookup, _memory: memory })
  }

  fn algorithm(&self) -> HashAlgorithm {
    self.capture.header.selected.header.hash_algorithm
  }

  fn check(&self) -> Result<(), SemanticMutationObservationErrorV1> {
    check_cancelled(&self.capture.cancellation)?;
    self.capture._memory.check_admission()?;
    self._memory.check_admission()?;
    Ok(())
  }

  fn load_node(
    &self,
    hash: &[u8],
    path: &str,
    lower: Option<&str>,
    upper: Option<&str>,
    btree_child: bool,
  ) -> Result<LoadedNamespaceSeekNodeV1, NativeSemanticNamespaceSourceErrorV1> {
    self.check()?;
    let locator = self
      .lookup
      .get(hash)
      .map_err(FirstAuthorityPublicationErrorV1::from)
      .map_err(SemanticMutationObservationErrorV1::from)?
      .ok_or_else(|| invalid("semantic_namespace_source_directory_missing", "captured namespace directory is missing"))?;
    if locator.type_flags != KV_TYPE_DIRECTORY {
      return Err(invalid("semantic_namespace_source_directory_role", "namespace directory key resolves to another KV role").into());
    }
    if locator.total_length as usize > self.bounds.maximum_directory_entity_bytes {
      return Err(resource("semantic_namespace_source_directory_bound", "namespace directory exceeds its entity byte bound").into());
    }
    let memory = self
      .capture
      .memory
      .reserve(MemoryOwner::Task, u64::from(locator.total_length) * 4 + SOURCE_SCRATCH_BYTES, AdmissionClass::Maintenance)
      .map_err(SemanticMutationObservationErrorV1::from)?;
    let header = &self.capture.header.selected.header;
    let bytes = read_entity_bounded(
      &self.capture._protection.publisher().file,
      &self.lookup,
      hash,
      self.bounds.maximum_directory_entity_bytes,
      header.write_sequence_high_water,
    )
    .map_err(map_namespace_read_error)?
    .ok_or_else(|| invalid("semantic_namespace_source_directory_missing", "captured namespace directory disappeared"))?;
    let value =
      decode_whole_entity(&bytes, self.algorithm(), header.write_sequence_high_water).map_err(SemanticMutationObservationErrorV1::from)?;
    let entity = LoadedImmutableEntityV1 {
      entity_version: value.entity_version,
      entry_type: value.entry_type,
      flags: value.flags,
      compression_algorithm: value.compression_algorithm,
      timestamp_ms: value.timestamp_ms,
      write_sequence: value.write_sequence,
      key: copy_namespace_bytes(value.key)?,
      stored_value: copy_namespace_bytes(value.stored_value)?,
    };
    validate_selected_directory_entity(&entity, self.algorithm(), hash).map_err(NativeSelectedNamespaceReadErrorV1::from)?;
    let node = decode_validated_selected_directory_node(&entity, self.algorithm().hash_length(), path, lower, upper, btree_child)?;
    let decoded_work = match &node {
      BTreeNode::Leaf(leaf) => leaf.entries.len(),
      BTreeNode::Internal(internal) => internal.keys.len(),
    };
    self.lookup.charge_work(decoded_work as u64).map_err(map_namespace_read_error)?;
    self.check()?;
    memory.check_admission().map_err(SemanticMutationObservationErrorV1::from)?;
    Ok(LoadedNamespaceSeekNodeV1 { node, _memory: memory })
  }
}

pub(super) fn validate_namespace_source_path(path: &str, algorithm: HashAlgorithm) -> Result<(), SemanticMutationObservationErrorV1> {
  validate_canonical_absolute_path(path)?;
  if path.len() > u16::MAX as usize {
    return Err(invalid("semantic_namespace_source_path", "namespace source path exceeds the FileRecord path width"));
  }
  if !is_namespace_source_path(path, algorithm)? {
    return Err(invalid("semantic_namespace_source_family", "path is not a descendant namespace configuration input"));
  }
  Ok(())
}

fn is_namespace_source_path(path: &str, algorithm: HashAlgorithm) -> Result<bool, SemanticMutationObservationErrorV1> {
  Ok(matches!(
    SystemFamilyPolicyResolverV1::embedded(algorithm)?.policy(SystemFamilySubjectV1::Path(path), "captured namespace source")?,
    SystemFamilyPolicyDecisionV1::Known { family_id: 0x0002, .. }
  ))
}

fn copy_namespace_bytes(bytes: &[u8]) -> Result<Vec<u8>, SemanticMutationObservationErrorV1> {
  let mut copy = Vec::new();
  copy.try_reserve_exact(bytes.len()).map_err(namespace_allocation)?;
  copy.extend_from_slice(bytes);
  Ok(copy)
}

fn copy_namespace_path(path: &str) -> Result<String, SemanticMutationObservationErrorV1> {
  let mut copy = String::new();
  copy.try_reserve_exact(path.len()).map_err(namespace_allocation)?;
  copy.push_str(path);
  Ok(copy)
}

fn namespace_allocation(source: std::collections::TryReserveError) -> SemanticMutationObservationErrorV1 {
  SemanticMutationObservationErrorV1::Allocation { code: "semantic_namespace_source_allocation", source }
}

fn map_namespace_read_error(source: FirstAuthorityPublicationErrorV1) -> SemanticMutationObservationErrorV1 {
  if source.code() == "semantic_namespace_source_work_bound" {
    SemanticMutationObservationErrorV1::ResourceRead { code: "semantic_namespace_source_work_bound", source }
  } else {
    map_source_read_error(source)
  }
}
