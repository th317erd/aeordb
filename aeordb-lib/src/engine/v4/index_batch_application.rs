//! Bounded sparse reads and successor-artifact retention for frozen index batches.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::Arc;

use thiserror::Error;

use crate::engine::HashAlgorithm;

use super::index_artifact::{EncodedImmutableIndexArtifactV1, ImmutableIndexArtifactKindV1, decode_immutable_index_artifact};
use super::index_generation_publication::{INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1, INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1};
use super::index_page::{
  ArtifactDirectoryEntryV1, ArtifactDirectoryNodeV1, OrderedIndexRoleV1, OrderedPageV1, compare_order_keys, decode_artifact_directory,
  decode_ordered_page, validate_posting_page_link,
};
use super::reader::{FormatError, MalformedInputClass};

pub const INDEX_BATCH_PATH_MAXIMUM_DEPTH_V1: usize = 16;
pub const INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1: usize = 32 * 1_024 * 1_024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IndexBatchArtifactReadErrorV1 {
  #[error("immutable index artifact is missing")]
  Missing,
  #[error("immutable index artifact read was cancelled")]
  Cancelled,
  #[error("immutable index artifact read exceeded resource limits: {0}")]
  ResourcePressure(String),
  #[error("immutable index artifact read failed: {0}")]
  Operational(String),
}

pub trait IndexBatchArtifactSourceV1 {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<Vec<u8>, IndexBatchArtifactReadErrorV1>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexBatchApplicationErrorV1 {
  #[error("index batch operation was cancelled")]
  Cancelled,
  #[error("immutable index artifact {key} is missing")]
  MissingArtifact { key: String },
  #[error("immutable index artifact source exceeded resource limits: {0}")]
  SourcePressure(String),
  #[error("immutable index artifact source failed: {0}")]
  SourceOperational(String),
  #[error("malformed immutable index state: {0}")]
  Malformed(FormatError),
  #[error("invalid index batch limits: {0}")]
  InvalidLimits(String),
  #[error("index batch successor overlay exceeds its artifact-count limit")]
  OverlayCount,
  #[error("index batch successor overlay exceeds its retained-byte limit")]
  OverlayBytes,
  #[error("index batch successor overlay contains a conflicting immutable key")]
  OverlayConflict,
  #[error("index batch allocation failed: {0}")]
  Allocation(String),
}

impl IndexBatchApplicationErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Cancelled => "index_batch_cancelled",
      Self::MissingArtifact { .. } => "index_batch_artifact_missing",
      Self::SourcePressure(_) => "index_batch_source_pressure",
      Self::SourceOperational(_) => "index_batch_source_operational",
      Self::Malformed(error) => error.code(),
      Self::InvalidLimits(_) => "index_batch_invalid_limits",
      Self::OverlayCount => "index_batch_overlay_count",
      Self::OverlayBytes => "index_batch_overlay_bytes",
      Self::OverlayConflict => "index_batch_overlay_conflict",
      Self::Allocation(_) => "index_batch_allocation",
    }
  }
}

impl From<FormatError> for IndexBatchApplicationErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Malformed(source)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexBatchArtifactOverlayLimitsV1 {
  maximum_artifacts: usize,
  maximum_retained_bytes: usize,
}

impl IndexBatchArtifactOverlayLimitsV1 {
  pub fn new(maximum_artifacts: usize, maximum_retained_bytes: usize) -> Result<Self, IndexBatchApplicationErrorV1> {
    if maximum_artifacts == 0 || maximum_artifacts > INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1 {
      return Err(IndexBatchApplicationErrorV1::InvalidLimits(format!(
        "artifact count {maximum_artifacts} is outside 1..={INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1}"
      )));
    }
    if maximum_retained_bytes == 0 || maximum_retained_bytes > INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1 {
      return Err(IndexBatchApplicationErrorV1::InvalidLimits(format!(
        "retained bytes {maximum_retained_bytes} are outside 1..={INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1}"
      )));
    }
    Ok(Self { maximum_artifacts, maximum_retained_bytes })
  }

  pub fn maximum_artifacts(self) -> usize {
    self.maximum_artifacts
  }

  pub fn maximum_retained_bytes(self) -> usize {
    self.maximum_retained_bytes
  }
}

impl Default for IndexBatchArtifactOverlayLimitsV1 {
  fn default() -> Self {
    Self { maximum_artifacts: INDEX_GENERATION_DEPENDENCY_HARD_CAP_V1, maximum_retained_bytes: INDEX_GENERATION_TOTAL_BYTES_HARD_CAP_V1 }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderedPagePathLookupLimitsV1 {
  maximum_directory_depth: usize,
  maximum_input_bytes: usize,
}

impl OrderedPagePathLookupLimitsV1 {
  pub fn new(maximum_directory_depth: usize, maximum_input_bytes: usize) -> Result<Self, IndexBatchApplicationErrorV1> {
    if maximum_directory_depth == 0 || maximum_directory_depth > INDEX_BATCH_PATH_MAXIMUM_DEPTH_V1 {
      return Err(IndexBatchApplicationErrorV1::InvalidLimits(format!(
        "directory depth {maximum_directory_depth} is outside 1..={INDEX_BATCH_PATH_MAXIMUM_DEPTH_V1}"
      )));
    }
    if maximum_input_bytes == 0 || maximum_input_bytes > INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1 {
      return Err(IndexBatchApplicationErrorV1::InvalidLimits(format!(
        "path input bytes {maximum_input_bytes} are outside 1..={INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1}"
      )));
    }
    Ok(Self { maximum_directory_depth, maximum_input_bytes })
  }

  pub fn maximum_directory_depth(self) -> usize {
    self.maximum_directory_depth
  }

  pub fn maximum_input_bytes(self) -> usize {
    self.maximum_input_bytes
  }
}

impl Default for OrderedPagePathLookupLimitsV1 {
  fn default() -> Self {
    Self { maximum_directory_depth: INDEX_BATCH_PATH_MAXIMUM_DEPTH_V1, maximum_input_bytes: INDEX_BATCH_PATH_MAXIMUM_INPUT_BYTES_V1 }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct OrderedPagePathLookupRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub root_key: &'a [u8],
  pub owner_id: &'a [u8],
  pub role: OrderedIndexRoleV1,
  pub order_key: &'a [u8],
  pub load_posting_successor: bool,
  pub limits: OrderedPagePathLookupLimitsV1,
}

#[derive(Clone, Debug)]
enum RetainedArtifactBytesV1 {
  Prepared(Arc<EncodedImmutableIndexArtifactV1>),
  Source(Arc<Vec<u8>>),
}

impl RetainedArtifactBytesV1 {
  fn value(&self) -> &[u8] {
    match self {
      Self::Prepared(artifact) => &artifact.value,
      Self::Source(value) => value,
    }
  }
}

#[derive(Debug)]
pub struct SparseIndexArtifactOverlayV1 {
  hash_algorithm: HashAlgorithm,
  limits: IndexBatchArtifactOverlayLimitsV1,
  artifacts: Vec<Arc<EncodedImmutableIndexArtifactV1>>,
  by_key: HashMap<Vec<u8>, usize>,
  retained_bytes: usize,
}

impl SparseIndexArtifactOverlayV1 {
  pub fn new(hash_algorithm: HashAlgorithm, limits: IndexBatchArtifactOverlayLimitsV1) -> Result<Self, IndexBatchApplicationErrorV1> {
    IndexBatchArtifactOverlayLimitsV1::new(limits.maximum_artifacts(), limits.maximum_retained_bytes())?;
    Ok(Self { hash_algorithm, limits, artifacts: Vec::new(), by_key: HashMap::new(), retained_bytes: 0 })
  }

  pub fn insert(&mut self, artifact: EncodedImmutableIndexArtifactV1) -> Result<bool, IndexBatchApplicationErrorV1> {
    let decoded = decode_immutable_index_artifact(
      &artifact.value,
      self.hash_algorithm,
      ImmutableIndexArtifactKindV1::MutationJournalSegment.maximum_encoded_length(),
    )?;
    let kind = ImmutableIndexArtifactKindV1::from_u16(decoded.kind).ok_or_else(|| {
      IndexBatchApplicationErrorV1::Malformed(FormatError::new(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "index_batch_overlay_kind",
        "prepared immutable artifact kind is unknown",
      ))
    })?;
    if decoded.key != artifact.key || artifact.value.len() > kind.maximum_encoded_length() {
      return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
        MalformedInputClass::IdentityKeyOrGenerationMismatch,
        "index_batch_overlay_identity",
        "prepared immutable artifact key or kind is invalid",
      )));
    }
    if let Some(index) = self.by_key.get(artifact.key.as_slice()) {
      if self.artifacts[*index].value == artifact.value {
        return Ok(false);
      }
      return Err(IndexBatchApplicationErrorV1::OverlayConflict);
    }
    if self.artifacts.len() >= self.limits.maximum_artifacts() {
      return Err(IndexBatchApplicationErrorV1::OverlayCount);
    }
    let retained = checked_overlay_artifact_bytes(&artifact)?;
    let projected = self.retained_bytes.checked_add(retained).ok_or(IndexBatchApplicationErrorV1::OverlayBytes)?;
    if projected > self.limits.maximum_retained_bytes() {
      return Err(IndexBatchApplicationErrorV1::OverlayBytes);
    }
    self
      .artifacts
      .try_reserve(1)
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("successor artifact reservation failed: {error}")))?;
    self
      .by_key
      .try_reserve(1)
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("successor lookup reservation failed: {error}")))?;
    let index = self.artifacts.len();
    let key = clone_bytes(&artifact.key, "successor lookup key")?;
    self.artifacts.push(Arc::new(artifact));
    self.by_key.insert(key, index);
    self.retained_bytes = projected;
    Ok(true)
  }

  pub fn artifact_count(&self) -> usize {
    self.artifacts.len()
  }

  pub fn retained_bytes(&self) -> usize {
    self.retained_bytes
  }

  pub fn prepared_artifacts(&self) -> impl ExactSizeIterator<Item = &EncodedImmutableIndexArtifactV1> {
    self.artifacts.iter().map(Arc::as_ref)
  }

  fn get(&self, key: &[u8]) -> Option<RetainedArtifactBytesV1> {
    self.by_key.get(key).map(|index| RetainedArtifactBytesV1::Prepared(Arc::clone(&self.artifacts[*index])))
  }
}

#[derive(Debug)]
pub struct LoadedOrderedPagePathV1 {
  directories: Vec<RetainedArtifactBytesV1>,
  page: RetainedArtifactBytesV1,
  next_posting: Option<LoadedPostingSuccessorV1>,
  input_bytes: usize,
}

impl LoadedOrderedPagePathV1 {
  pub fn directory_count(&self) -> usize {
    self.directories.len()
  }

  pub fn directory(&self, index: usize) -> Option<&[u8]> {
    self.directories.get(index).map(RetainedArtifactBytesV1::value)
  }

  pub fn page(&self) -> &[u8] {
    self.page.value()
  }

  pub fn next_posting_page(&self) -> Option<&[u8]> {
    self.next_posting.as_ref().map(|next| next.page.value())
  }

  pub fn next_directory_count(&self) -> usize {
    self.next_posting.as_ref().map_or(0, |next| next.directories.len())
  }

  pub fn next_directory(&self, index: usize) -> Option<&[u8]> {
    self.next_posting.as_ref().and_then(|next| next.directories.get(index)).map(RetainedArtifactBytesV1::value)
  }

  pub fn input_bytes(&self) -> usize {
    self.input_bytes
  }
}

#[derive(Debug)]
struct LoadedPostingSuccessorV1 {
  directories: Vec<RetainedArtifactBytesV1>,
  page: RetainedArtifactBytesV1,
}

#[derive(Debug)]
struct TraversedPagePathV1 {
  directories: Vec<RetainedArtifactBytesV1>,
  selected_entries: Vec<usize>,
  page: RetainedArtifactBytesV1,
}

#[derive(Debug)]
struct OwnedDirectoryEntryExpectationV1 {
  lower_fence: Vec<u8>,
  upper_fence: Vec<u8>,
  child_hash: Vec<u8>,
  child_generation: u64,
  live_count: u64,
  tombstone_count: u64,
  page_count: u64,
  logical_bytes: u64,
  minimum_page_id: u64,
  maximum_page_id: u64,
}

impl OwnedDirectoryEntryExpectationV1 {
  fn from_entry(entry: &ArtifactDirectoryEntryV1<'_>) -> Result<Self, IndexBatchApplicationErrorV1> {
    Ok(Self {
      lower_fence: clone_bytes(entry.lower_fence, "directory child lower fence")?,
      upper_fence: clone_bytes(entry.upper_fence, "directory child upper fence")?,
      child_hash: clone_bytes(entry.child_hash, "directory child hash")?,
      child_generation: entry.child_generation,
      live_count: entry.live_count,
      tombstone_count: entry.tombstone_count,
      page_count: entry.page_count,
      logical_bytes: entry.logical_bytes,
      minimum_page_id: entry.minimum_page_id,
      maximum_page_id: entry.maximum_page_id,
    })
  }
}

#[derive(Debug)]
struct PathInputBudgetV1 {
  maximum_bytes: usize,
  retained_bytes: usize,
  observed_keys: Vec<Vec<u8>>,
}

impl PathInputBudgetV1 {
  fn new(maximum_bytes: usize) -> Self {
    Self { maximum_bytes, retained_bytes: 0, observed_keys: Vec::new() }
  }

  fn observe(&mut self, key: &[u8], value_length: usize) -> Result<(), IndexBatchApplicationErrorV1> {
    if self.observed_keys.iter().any(|observed| observed == key) {
      return Ok(());
    }
    let next = self
      .retained_bytes
      .checked_add(key.len())
      .and_then(|bytes| bytes.checked_add(value_length))
      .ok_or_else(|| IndexBatchApplicationErrorV1::InvalidLimits("path retained-byte count overflowed".to_string()))?;
    if next > self.maximum_bytes {
      return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
        MalformedInputClass::AllocationAmplification,
        "index_batch_path_input_bytes",
        format!("{next} path bytes exceed the {}-byte operation cap", self.maximum_bytes),
      )));
    }
    self
      .observed_keys
      .try_reserve(1)
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("path key reservation failed: {error}")))?;
    self.observed_keys.push(clone_bytes(key, "path budget key")?);
    self.retained_bytes = next;
    Ok(())
  }

  fn remaining_bytes(&self, key: &[u8]) -> usize {
    if self.observed_keys.iter().any(|observed| observed == key) {
      return self.maximum_bytes;
    }
    self.maximum_bytes.saturating_sub(self.retained_bytes).saturating_sub(key.len())
  }
}

pub fn load_ordered_page_path_v1(
  request: &OrderedPagePathLookupRequestV1<'_>,
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<LoadedOrderedPagePathV1, IndexBatchApplicationErrorV1> {
  validate_lookup_request(request, overlay)?;
  check_cancelled(is_cancelled)?;
  let mut budget = PathInputBudgetV1::new(request.limits.maximum_input_bytes());
  let traversed = descend_to_order_key(request, overlay, source, is_cancelled, &mut budget)?;
  let next_posting = if request.role == OrderedIndexRoleV1::Posting && request.load_posting_successor {
    load_posting_successor(request, &traversed, overlay, source, is_cancelled, &mut budget)?
  } else {
    None
  };
  Ok(LoadedOrderedPagePathV1 { directories: traversed.directories, page: traversed.page, next_posting, input_bytes: budget.retained_bytes })
}

fn descend_to_order_key(
  request: &OrderedPagePathLookupRequestV1<'_>,
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut PathInputBudgetV1,
) -> Result<TraversedPagePathV1, IndexBatchApplicationErrorV1> {
  let mut directories = Vec::new();
  let mut selected_entries = Vec::new();
  let mut current_key = clone_bytes(request.root_key, "root key")?;
  let mut expected_level = None;
  let mut expected_child = None;
  loop {
    check_cancelled(is_cancelled)?;
    if directories.len() >= request.limits.maximum_directory_depth() {
      return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
        MalformedInputClass::AllocationAmplification,
        "index_batch_path_depth",
        "ordered-page path exceeds its directory-depth limit",
      )));
    }
    let retained = load_artifact(&current_key, overlay, source, is_cancelled, budget)?;
    let directory = decode_artifact_directory(retained.value(), request.hash_algorithm)?;
    validate_directory_identity(&directory, &current_key, request.owner_id, request.role, expected_level)?;
    if let Some(expected) = expected_child.as_ref() {
      validate_directory_child(&directory, expected)?;
    }
    let selected = select_directory_entry(&directory, request.hash_algorithm, request.role, request.order_key)?;
    let entry = &directory.entries[selected];
    directories
      .try_reserve(1)
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("directory path reservation failed: {error}")))?;
    selected_entries
      .try_reserve(1)
      .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("directory index reservation failed: {error}")))?;
    directories.push(retained.clone());
    selected_entries.push(selected);
    if directory.level == 0 {
      let page = load_artifact(entry.child_hash, overlay, source, is_cancelled, budget)?;
      let decoded_page = decode_ordered_page(page.value(), request.hash_algorithm)?;
      validate_leaf_page(&decoded_page, entry, request.owner_id, request.role)?;
      return Ok(TraversedPagePathV1 { directories, selected_entries, page });
    }
    current_key = clone_bytes(entry.child_hash, "directory child key")?;
    expected_level = Some(directory.level - 1);
    expected_child = Some(OwnedDirectoryEntryExpectationV1::from_entry(entry)?);
  }
}

fn load_posting_successor(
  request: &OrderedPagePathLookupRequestV1<'_>,
  traversed: &TraversedPagePathV1,
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut PathInputBudgetV1,
) -> Result<Option<LoadedPostingSuccessorV1>, IndexBatchApplicationErrorV1> {
  let current_page = decode_ordered_page(traversed.page.value(), request.hash_algorithm)?;
  let successor = locate_logical_successor(request, traversed, overlay, source, is_cancelled, budget)?;
  match successor {
    None if current_page.next_page_id == 0 => Ok(None),
    None => Err(closure_error("posting page names a successor absent from its artifact directory")),
    Some(_successor) if current_page.next_page_id == 0 => {
      Err(closure_error("posting artifact directory has a successor for a terminal posting page"))
    }
    Some(successor) => {
      let next = decode_ordered_page(successor.page.value(), request.hash_algorithm)?;
      if next.page_id != current_page.next_page_id {
        return Err(closure_error("posting artifact-directory successor does not match the page next-link"));
      }
      validate_posting_page_link(&current_page, &next, request.hash_algorithm)?;
      Ok(Some(successor))
    }
  }
}

fn locate_logical_successor(
  request: &OrderedPagePathLookupRequestV1<'_>,
  traversed: &TraversedPagePathV1,
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut PathInputBudgetV1,
) -> Result<Option<LoadedPostingSuccessorV1>, IndexBatchApplicationErrorV1> {
  for directory_index in (0..traversed.directories.len()).rev() {
    let directory = decode_artifact_directory(traversed.directories[directory_index].value(), request.hash_algorithm)?;
    let selected = traversed.selected_entries[directory_index];
    let Some(next_index) = selected.checked_add(1).filter(|index| *index < directory.entries.len()) else {
      continue;
    };
    let mut successor_directories = traversed.directories[..=directory_index].to_vec();
    let entry = &directory.entries[next_index];
    if directory.level == 0 {
      let page = load_artifact(entry.child_hash, overlay, source, is_cancelled, budget)?;
      let decoded = decode_ordered_page(page.value(), request.hash_algorithm)?;
      validate_leaf_page(&decoded, entry, request.owner_id, request.role)?;
      return Ok(Some(LoadedPostingSuccessorV1 { directories: successor_directories, page }));
    }

    let mut current_key = clone_bytes(entry.child_hash, "successor directory key")?;
    let mut expected_level = directory.level - 1;
    let mut expected_child = OwnedDirectoryEntryExpectationV1::from_entry(entry)?;
    loop {
      check_cancelled(is_cancelled)?;
      if successor_directories.len() >= request.limits.maximum_directory_depth() {
        return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
          MalformedInputClass::AllocationAmplification,
          "index_batch_path_depth",
          "posting successor path exceeds its directory-depth limit",
        )));
      }
      let retained = load_artifact(&current_key, overlay, source, is_cancelled, budget)?;
      let child = decode_artifact_directory(retained.value(), request.hash_algorithm)?;
      validate_directory_identity(&child, &current_key, request.owner_id, request.role, Some(expected_level))?;
      validate_directory_child(&child, &expected_child)?;
      successor_directories
        .try_reserve(1)
        .map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("successor path reservation failed: {error}")))?;
      successor_directories.push(retained.clone());
      let first = child.entries.first().ok_or_else(|| closure_error("posting successor directory is empty"))?;
      if child.level == 0 {
        let page = load_artifact(first.child_hash, overlay, source, is_cancelled, budget)?;
        let decoded = decode_ordered_page(page.value(), request.hash_algorithm)?;
        validate_leaf_page(&decoded, first, request.owner_id, request.role)?;
        return Ok(Some(LoadedPostingSuccessorV1 { directories: successor_directories, page }));
      }
      current_key = clone_bytes(first.child_hash, "successor child key")?;
      expected_level = child.level - 1;
      expected_child = OwnedDirectoryEntryExpectationV1::from_entry(first)?;
    }
  }
  Ok(None)
}

fn load_artifact(
  key: &[u8],
  overlay: &SparseIndexArtifactOverlayV1,
  source: &mut dyn IndexBatchArtifactSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut PathInputBudgetV1,
) -> Result<RetainedArtifactBytesV1, IndexBatchApplicationErrorV1> {
  check_cancelled(is_cancelled)?;
  let retained = if let Some(retained) = overlay.get(key) {
    retained
  } else {
    let value = source.read_immutable_artifact(key, budget.remaining_bytes(key)).map_err(|error| map_source_error(key, error))?;
    check_cancelled(is_cancelled)?;
    RetainedArtifactBytesV1::Source(Arc::new(value))
  };
  budget.observe(key, retained.value().len())?;
  Ok(retained)
}

fn validate_lookup_request(
  request: &OrderedPagePathLookupRequestV1<'_>,
  overlay: &SparseIndexArtifactOverlayV1,
) -> Result<(), IndexBatchApplicationErrorV1> {
  OrderedPagePathLookupLimitsV1::new(request.limits.maximum_directory_depth(), request.limits.maximum_input_bytes())?;
  let hash_width = request.hash_algorithm.hash_length();
  if overlay.hash_algorithm != request.hash_algorithm
    || request.root_key.len() != hash_width
    || request.root_key.iter().all(|byte| *byte == 0)
    || request.owner_id.len() != hash_width
    || request.owner_id.iter().all(|byte| *byte == 0)
    || request.role == OrderedIndexRoleV1::NvtTile
  {
    return Err(IndexBatchApplicationErrorV1::Malformed(FormatError::new(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "index_batch_lookup_identity",
      "lookup hash profile, root, owner, or ordered role is invalid",
    )));
  }
  compare_order_keys(request.hash_algorithm, request.role, request.order_key, request.order_key)?;
  Ok(())
}

fn validate_directory_identity(
  directory: &ArtifactDirectoryNodeV1<'_>,
  expected_key: &[u8],
  owner_id: &[u8],
  role: OrderedIndexRoleV1,
  expected_level: Option<u16>,
) -> Result<(), IndexBatchApplicationErrorV1> {
  if directory.key != expected_key
    || directory.owner_id != owner_id
    || directory.role != role
    || expected_level.is_some_and(|level| directory.level != level)
  {
    return Err(closure_error("artifact directory key, owner, role, or level disagrees with its selected path"));
  }
  Ok(())
}

fn validate_leaf_page(
  page: &OrderedPageV1<'_>,
  descriptor: &ArtifactDirectoryEntryV1<'_>,
  owner_id: &[u8],
  role: OrderedIndexRoleV1,
) -> Result<(), IndexBatchApplicationErrorV1> {
  if page.key != descriptor.child_hash
    || page.owner_id != owner_id
    || page.role != role
    || page.generation != descriptor.child_generation
    || page.lower_fence != descriptor.lower_fence
    || page.upper_fence != descriptor.upper_fence
    || u64::from(page.live_count) != descriptor.live_count
    || u64::from(page.tombstone_count) != descriptor.tombstone_count
    || page.logical_live_bytes != descriptor.logical_bytes
    || descriptor.page_count != 1
    || page.page_id != descriptor.minimum_page_id
    || page.page_id != descriptor.maximum_page_id
  {
    return Err(closure_error("ordered page disagrees with its exact artifact-directory descriptor"));
  }
  Ok(())
}

fn validate_directory_child(
  directory: &ArtifactDirectoryNodeV1<'_>,
  descriptor: &OwnedDirectoryEntryExpectationV1,
) -> Result<(), IndexBatchApplicationErrorV1> {
  if directory.key != descriptor.child_hash
    || directory.generation != descriptor.child_generation
    || directory.lower_fence != descriptor.lower_fence
    || directory.upper_fence != descriptor.upper_fence
    || directory.live_count != descriptor.live_count
    || directory.tombstone_count != descriptor.tombstone_count
    || directory.page_count != descriptor.page_count
    || directory.logical_bytes != descriptor.logical_bytes
    || directory.minimum_page_id != descriptor.minimum_page_id
    || directory.maximum_page_id != descriptor.maximum_page_id
  {
    return Err(closure_error("artifact directory disagrees with its exact parent descriptor"));
  }
  Ok(())
}

fn select_directory_entry(
  directory: &ArtifactDirectoryNodeV1<'_>,
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  order_key: &[u8],
) -> Result<usize, IndexBatchApplicationErrorV1> {
  for (index, entry) in directory.entries.iter().enumerate() {
    if compare_order_keys(hash_algorithm, role, order_key, entry.upper_fence)? != std::cmp::Ordering::Greater {
      return Ok(index);
    }
  }
  directory.entries.len().checked_sub(1).ok_or_else(|| closure_error("artifact directory is empty"))
}

fn checked_overlay_artifact_bytes(artifact: &EncodedImmutableIndexArtifactV1) -> Result<usize, IndexBatchApplicationErrorV1> {
  size_of::<Arc<EncodedImmutableIndexArtifactV1>>()
    .checked_add(size_of::<(Vec<u8>, usize)>())
    .and_then(|bytes| bytes.checked_add(artifact.key.len().checked_mul(2)?))
    .and_then(|bytes| bytes.checked_add(artifact.value.len()))
    .ok_or(IndexBatchApplicationErrorV1::OverlayBytes)
}

fn clone_bytes(value: &[u8], context: &'static str) -> Result<Vec<u8>, IndexBatchApplicationErrorV1> {
  let mut cloned = Vec::new();
  cloned.try_reserve_exact(value.len()).map_err(|error| IndexBatchApplicationErrorV1::Allocation(format!("{context}: {error}")))?;
  cloned.extend_from_slice(value);
  Ok(cloned)
}

fn map_source_error(key: &[u8], error: IndexBatchArtifactReadErrorV1) -> IndexBatchApplicationErrorV1 {
  match error {
    IndexBatchArtifactReadErrorV1::Missing => IndexBatchApplicationErrorV1::MissingArtifact { key: hex::encode(key) },
    IndexBatchArtifactReadErrorV1::Cancelled => IndexBatchApplicationErrorV1::Cancelled,
    IndexBatchArtifactReadErrorV1::ResourcePressure(context) => IndexBatchApplicationErrorV1::SourcePressure(context),
    IndexBatchArtifactReadErrorV1::Operational(context) => IndexBatchApplicationErrorV1::SourceOperational(context),
  }
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<(), IndexBatchApplicationErrorV1> {
  if is_cancelled() {
    Err(IndexBatchApplicationErrorV1::Cancelled)
  } else {
    Ok(())
  }
}

fn closure_error(context: impl Into<String>) -> IndexBatchApplicationErrorV1 {
  IndexBatchApplicationErrorV1::Malformed(FormatError::new(
    MalformedInputClass::CrossRecordClosureMismatch,
    "index_batch_path_closure",
    context,
  ))
}
