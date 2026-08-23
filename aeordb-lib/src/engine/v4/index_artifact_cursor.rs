//! Bounded correctness-bearing traversal of immutable ArtifactDirectory trees.

use std::sync::Arc;

use thiserror::Error;

use crate::engine::HashAlgorithm;

use super::index_artifact::EncodedImmutableIndexArtifactV1;
use super::index_page::{
  ArtifactDirectoryEntryV1, ArtifactDirectoryNodeV1, OrderedIndexRoleV1, OrderedPageV1, compare_order_keys, decode_artifact_directory,
  decode_ordered_page, validate_posting_page_link,
};
use super::reader::{FormatError, MalformedInputClass};

pub const ARTIFACT_PAGE_CURSOR_MAXIMUM_DEPTH_V1: usize = 16;
pub const ARTIFACT_PAGE_CURSOR_MAXIMUM_INPUT_BYTES_V1: usize = 64 * 1_024 * 1_024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ArtifactCursorReadErrorV1 {
  #[error("immutable index artifact is missing")]
  Missing,
  #[error("immutable index artifact read was cancelled")]
  Cancelled,
  #[error("immutable index artifact read exceeded resource limits: {0}")]
  ResourcePressure(String),
  #[error("immutable index artifact source returned corrupt authority: {0}")]
  Corrupt(String),
  #[error("immutable index artifact read failed: {0}")]
  Operational(String),
}

#[derive(Clone, Debug)]
pub enum RetainedArtifactBytesV1 {
  Encoded(Arc<EncodedImmutableIndexArtifactV1>),
  Bytes(Arc<Vec<u8>>),
}

impl RetainedArtifactBytesV1 {
  pub fn from_encoded(artifact: Arc<EncodedImmutableIndexArtifactV1>) -> Self {
    Self::Encoded(artifact)
  }

  pub fn from_bytes(bytes: Vec<u8>) -> Self {
    Self::Bytes(Arc::new(bytes))
  }

  pub fn bytes(&self) -> &[u8] {
    match self {
      Self::Encoded(artifact) => &artifact.value,
      Self::Bytes(bytes) => bytes,
    }
  }
}

pub trait ArtifactCursorSourceV1 {
  fn read_immutable_artifact(&mut self, key: &[u8], maximum_bytes: usize) -> Result<RetainedArtifactBytesV1, ArtifactCursorReadErrorV1>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArtifactPageCursorErrorV1 {
  #[error("artifact cursor operation was cancelled")]
  Cancelled,
  #[error("immutable index artifact {key} is missing")]
  MissingArtifact { key: String },
  #[error("immutable index artifact source exceeded resource limits: {0}")]
  SourcePressure(String),
  #[error("immutable index artifact source returned corrupt authority: {0}")]
  SourceCorrupt(String),
  #[error("immutable index artifact source failed: {0}")]
  SourceOperational(String),
  #[error("malformed immutable artifact cursor state: {0}")]
  Malformed(FormatError),
  #[error("invalid artifact cursor limits: {0}")]
  InvalidLimits(String),
  #[error("artifact cursor allocation failed: {0}")]
  Allocation(String),
}

impl ArtifactPageCursorErrorV1 {
  pub fn code(&self) -> &'static str {
    match self {
      Self::Cancelled => "artifact_cursor_cancelled",
      Self::MissingArtifact { .. } => "artifact_cursor_missing",
      Self::SourcePressure(_) => "artifact_cursor_source_pressure",
      Self::SourceCorrupt(_) => "artifact_cursor_source_corrupt",
      Self::SourceOperational(_) => "artifact_cursor_source_operational",
      Self::Malformed(error) => error.code(),
      Self::InvalidLimits(_) => "artifact_cursor_invalid_limits",
      Self::Allocation(_) => "artifact_cursor_allocation",
    }
  }
}

impl From<FormatError> for ArtifactPageCursorErrorV1 {
  fn from(source: FormatError) -> Self {
    Self::Malformed(source)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactPageCursorLimitsV1 {
  maximum_directory_depth: usize,
  maximum_input_bytes: usize,
}

impl ArtifactPageCursorLimitsV1 {
  pub fn new(maximum_directory_depth: usize, maximum_input_bytes: usize) -> Result<Self, ArtifactPageCursorErrorV1> {
    if maximum_directory_depth == 0 || maximum_directory_depth > ARTIFACT_PAGE_CURSOR_MAXIMUM_DEPTH_V1 {
      return Err(ArtifactPageCursorErrorV1::InvalidLimits(format!(
        "directory depth {maximum_directory_depth} is outside 1..={ARTIFACT_PAGE_CURSOR_MAXIMUM_DEPTH_V1}"
      )));
    }
    if maximum_input_bytes == 0 || maximum_input_bytes > ARTIFACT_PAGE_CURSOR_MAXIMUM_INPUT_BYTES_V1 {
      return Err(ArtifactPageCursorErrorV1::InvalidLimits(format!(
        "input bytes {maximum_input_bytes} are outside 1..={ARTIFACT_PAGE_CURSOR_MAXIMUM_INPUT_BYTES_V1}"
      )));
    }
    Ok(Self { maximum_directory_depth, maximum_input_bytes })
  }

  pub const fn maximum_directory_depth(self) -> usize {
    self.maximum_directory_depth
  }

  pub const fn maximum_input_bytes(self) -> usize {
    self.maximum_input_bytes
  }
}

impl Default for ArtifactPageCursorLimitsV1 {
  fn default() -> Self {
    Self { maximum_directory_depth: ARTIFACT_PAGE_CURSOR_MAXIMUM_DEPTH_V1, maximum_input_bytes: 32 * 1_024 * 1_024 }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactDirectoryRootSummaryV1 {
  pub live_count: u64,
  pub tombstone_count: u64,
  pub page_count: u64,
  pub logical_bytes: u64,
  pub minimum_page_id: u64,
  pub maximum_page_id: u64,
}

impl ArtifactDirectoryRootSummaryV1 {
  pub const fn from_directory(directory: &ArtifactDirectoryNodeV1<'_>) -> Self {
    Self {
      live_count: directory.live_count,
      tombstone_count: directory.tombstone_count,
      page_count: directory.page_count,
      logical_bytes: directory.logical_bytes,
      minimum_page_id: directory.minimum_page_id,
      maximum_page_id: directory.maximum_page_id,
    }
  }
}

#[derive(Clone, Copy, Debug)]
pub struct ArtifactPageCursorRootV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub root_key: &'a [u8],
  pub owner_id: &'a [u8],
  pub role: OrderedIndexRoleV1,
  pub maximum_generation: u64,
  pub expected_summary: Option<ArtifactDirectoryRootSummaryV1>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactPageSeekV1<'a> {
  PageOrdinal(u64),
  LiveRecordRank(u64),
  OrderLowerBound(&'a [u8]),
  OrderPredecessor(&'a [u8]),
  PageId(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactPageNeighborModeV1 {
  None,
  Next,
  Both,
}

#[derive(Clone, Copy, Debug)]
pub struct ArtifactPageCursorRequestV1<'a> {
  pub root: ArtifactPageCursorRootV1<'a>,
  pub seek: ArtifactPageSeekV1<'a>,
  pub neighbors: ArtifactPageNeighborModeV1,
  pub limits: ArtifactPageCursorLimitsV1,
}

#[derive(Debug)]
struct TraversedPathV1 {
  directories: Vec<RetainedArtifactBytesV1>,
  selected_entries: Vec<usize>,
  page: RetainedArtifactBytesV1,
  page_ordinal: u64,
  live_rank_before_page: u64,
  live_rank_within_page: Option<u64>,
  record_index_within_page: Option<usize>,
}

#[derive(Debug)]
struct LoadedNeighborV1 {
  directories: Vec<RetainedArtifactBytesV1>,
  page: RetainedArtifactBytesV1,
}

#[derive(Debug)]
pub struct LoadedArtifactPageCursorV1 {
  directories: Vec<RetainedArtifactBytesV1>,
  page: RetainedArtifactBytesV1,
  previous: Option<LoadedNeighborV1>,
  next: Option<LoadedNeighborV1>,
  page_ordinal: u64,
  live_rank_before_page: u64,
  live_rank_within_page: Option<u64>,
  record_index_within_page: Option<usize>,
  root_page_count: u64,
  root_live_count: u64,
  retained_input_bytes: usize,
}

impl LoadedArtifactPageCursorV1 {
  pub fn directory_count(&self) -> usize {
    self.directories.len()
  }

  pub fn directory(&self, index: usize) -> Option<&[u8]> {
    self.directories.get(index).map(RetainedArtifactBytesV1::bytes)
  }

  pub fn page(&self) -> &[u8] {
    self.page.bytes()
  }

  pub fn previous_page(&self) -> Option<&[u8]> {
    self.previous.as_ref().map(|neighbor| neighbor.page.bytes())
  }

  pub fn previous_directory_count(&self) -> usize {
    self.previous.as_ref().map_or(0, |neighbor| neighbor.directories.len())
  }

  pub fn previous_directory(&self, index: usize) -> Option<&[u8]> {
    self.previous.as_ref().and_then(|neighbor| neighbor.directories.get(index)).map(RetainedArtifactBytesV1::bytes)
  }

  pub fn next_page(&self) -> Option<&[u8]> {
    self.next.as_ref().map(|neighbor| neighbor.page.bytes())
  }

  pub fn next_directory_count(&self) -> usize {
    self.next.as_ref().map_or(0, |neighbor| neighbor.directories.len())
  }

  pub fn next_directory(&self, index: usize) -> Option<&[u8]> {
    self.next.as_ref().and_then(|neighbor| neighbor.directories.get(index)).map(RetainedArtifactBytesV1::bytes)
  }

  pub const fn page_ordinal(&self) -> u64 {
    self.page_ordinal
  }

  pub const fn live_rank_before_page(&self) -> u64 {
    self.live_rank_before_page
  }

  pub const fn live_rank_within_page(&self) -> Option<u64> {
    self.live_rank_within_page
  }

  pub const fn record_index_within_page(&self) -> Option<usize> {
    self.record_index_within_page
  }

  pub const fn root_page_count(&self) -> u64 {
    self.root_page_count
  }

  pub const fn root_live_count(&self) -> u64 {
    self.root_live_count
  }

  pub const fn retained_input_bytes(&self) -> usize {
    self.retained_input_bytes
  }
}

struct InputBudgetV1 {
  maximum_bytes: usize,
  retained_bytes: usize,
  artifacts: Vec<(Vec<u8>, RetainedArtifactBytesV1)>,
}

impl InputBudgetV1 {
  fn new(maximum_bytes: usize) -> Self {
    Self { maximum_bytes, retained_bytes: 0, artifacts: Vec::new() }
  }

  fn retained(&self, key: &[u8]) -> Option<RetainedArtifactBytesV1> {
    self.artifacts.iter().find_map(|(retained_key, value)| (retained_key == key).then(|| value.clone()))
  }

  fn remaining(&self, key: &[u8]) -> usize {
    self.maximum_bytes.saturating_sub(self.retained_bytes).saturating_sub(key.len())
  }

  fn retain(&mut self, key: &[u8], value: &RetainedArtifactBytesV1) -> Result<(), ArtifactPageCursorErrorV1> {
    let next = self
      .retained_bytes
      .checked_add(key.len())
      .and_then(|bytes| bytes.checked_add(value.bytes().len()))
      .ok_or_else(|| invalid_limits("artifact cursor retained-byte count overflowed"))?;
    if next > self.maximum_bytes {
      return Err(amplification_error("artifact cursor retained input exceeds its admitted byte bound"));
    }
    self
      .artifacts
      .try_reserve(1)
      .map_err(|error| ArtifactPageCursorErrorV1::Allocation(format!("artifact cursor cache reservation failed: {error}")))?;
    let mut retained_key = Vec::new();
    retained_key
      .try_reserve_exact(key.len())
      .map_err(|error| ArtifactPageCursorErrorV1::Allocation(format!("artifact cursor key allocation failed: {error}")))?;
    retained_key.extend_from_slice(key);
    self.artifacts.push((retained_key, value.clone()));
    self.retained_bytes = next;
    Ok(())
  }
}

#[derive(Clone, Debug)]
struct ChildExpectationV1 {
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

impl ChildExpectationV1 {
  fn from_entry(entry: &ArtifactDirectoryEntryV1<'_>) -> Result<Self, ArtifactPageCursorErrorV1> {
    Ok(Self {
      lower_fence: copy_bytes(entry.lower_fence, "lower fence")?,
      upper_fence: copy_bytes(entry.upper_fence, "upper fence")?,
      child_hash: copy_bytes(entry.child_hash, "child hash")?,
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

struct SelectionStateV1<'a> {
  seek: ArtifactPageSeekV1<'a>,
  remaining: u64,
  page_ordinal: u64,
  live_rank_before_page: u64,
}

impl<'a> SelectionStateV1<'a> {
  fn new(seek: ArtifactPageSeekV1<'a>) -> Self {
    let remaining = match seek {
      ArtifactPageSeekV1::PageOrdinal(value) | ArtifactPageSeekV1::LiveRecordRank(value) => value,
      ArtifactPageSeekV1::OrderLowerBound(_) | ArtifactPageSeekV1::OrderPredecessor(_) | ArtifactPageSeekV1::PageId(_) => 0,
    };
    Self { seek, remaining, page_ordinal: 0, live_rank_before_page: 0 }
  }
}

pub fn load_artifact_page_cursor_v1(
  request: &ArtifactPageCursorRequestV1<'_>,
  source: &mut dyn ArtifactCursorSourceV1,
  is_cancelled: &dyn Fn() -> bool,
) -> Result<Option<LoadedArtifactPageCursorV1>, ArtifactPageCursorErrorV1> {
  validate_request(request)?;
  check_cancelled(is_cancelled)?;
  let mut budget = InputBudgetV1::new(request.limits.maximum_input_bytes());
  let Some(path) = descend(request, source, is_cancelled, &mut budget)? else {
    return Ok(None);
  };
  let current_page = decode_ordered_page(path.page.bytes(), request.root.hash_algorithm)?;
  let previous = if request.neighbors == ArtifactPageNeighborModeV1::Both {
    locate_neighbor(request, &path, NeighborDirectionV1::Previous, source, is_cancelled, &mut budget)?
  } else {
    None
  };
  let next = if request.neighbors != ArtifactPageNeighborModeV1::None {
    locate_neighbor(request, &path, NeighborDirectionV1::Next, source, is_cancelled, &mut budget)?
  } else {
    None
  };
  validate_neighbor_continuity(
    request.root.hash_algorithm,
    request.root.role,
    request.neighbors,
    &current_page,
    previous.as_ref(),
    next.as_ref(),
  )?;
  let (root_page_count, root_live_count) = {
    let root = decode_artifact_directory(path.directories[0].bytes(), request.root.hash_algorithm)?;
    (root.page_count, root.live_count)
  };
  Ok(Some(LoadedArtifactPageCursorV1 {
    directories: path.directories,
    page: path.page,
    previous,
    next,
    page_ordinal: path.page_ordinal,
    live_rank_before_page: path.live_rank_before_page,
    live_rank_within_page: path.live_rank_within_page,
    record_index_within_page: path.record_index_within_page,
    root_page_count,
    root_live_count,
    retained_input_bytes: budget.retained_bytes,
  }))
}

fn descend(
  request: &ArtifactPageCursorRequestV1<'_>,
  source: &mut dyn ArtifactCursorSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut InputBudgetV1,
) -> Result<Option<TraversedPathV1>, ArtifactPageCursorErrorV1> {
  if let ArtifactPageSeekV1::PageId(page_id) = request.seek {
    return descend_to_page_id(request, page_id, source, is_cancelled, budget);
  }
  let mut directories = Vec::new();
  let mut selected_entries = Vec::new();
  let mut current_key = copy_bytes(request.root.root_key, "root key")?;
  let mut expected_level = None;
  let mut expected_child = None;
  let mut selection = SelectionStateV1::new(request.seek);
  loop {
    check_cancelled(is_cancelled)?;
    if directories.len() >= request.limits.maximum_directory_depth() {
      return Err(amplification_error("artifact cursor path exceeds its directory-depth limit"));
    }
    let retained = load_artifact(&current_key, source, is_cancelled, budget)?;
    let directory = decode_artifact_directory(retained.bytes(), request.root.hash_algorithm)?;
    validate_directory_identity(&directory, request.root, expected_level)?;
    if let Some(expected) = expected_child.as_ref() {
      validate_directory_child(&directory, expected)?;
    } else {
      if directory.key != request.root.root_key {
        return Err(closure_error("artifact directory root key disagrees with the selected root"));
      }
      validate_root_summary(&directory, request.root.expected_summary)?;
      validate_seek_against_root(&directory, request.root.hash_algorithm, request.seek)?;
      if usize::from(directory.level).checked_add(1).is_none_or(|depth| depth > request.limits.maximum_directory_depth()) {
        return Err(amplification_error("artifact cursor root level exceeds its directory-depth limit"));
      }
    }
    let Some(selected) = select_entry(&directory, request.root.hash_algorithm, &mut selection)? else {
      return Ok(None);
    };
    let entry = &directory.entries[selected];
    directories
      .try_reserve(1)
      .map_err(|error| ArtifactPageCursorErrorV1::Allocation(format!("artifact cursor path reservation failed: {error}")))?;
    selected_entries
      .try_reserve(1)
      .map_err(|error| ArtifactPageCursorErrorV1::Allocation(format!("artifact cursor index reservation failed: {error}")))?;
    directories.push(retained.clone());
    selected_entries.push(selected);
    if directory.level == 0 {
      let page = load_artifact(entry.child_hash, source, is_cancelled, budget)?;
      let decoded_page = decode_ordered_page(page.bytes(), request.root.hash_algorithm)?;
      validate_leaf_page(&decoded_page, entry, request.root)?;
      let (live_rank_within_page, record_index_within_page) = match request.seek {
        ArtifactPageSeekV1::LiveRecordRank(_) => {
          (Some(selection.remaining), Some(live_record_index_within_page(&decoded_page, selection.remaining)?))
        }
        ArtifactPageSeekV1::PageOrdinal(_)
        | ArtifactPageSeekV1::OrderLowerBound(_)
        | ArtifactPageSeekV1::OrderPredecessor(_)
        | ArtifactPageSeekV1::PageId(_) => (None, None),
      };
      return Ok(Some(TraversedPathV1 {
        directories,
        selected_entries,
        page,
        page_ordinal: selection.page_ordinal,
        live_rank_before_page: selection.live_rank_before_page,
        live_rank_within_page,
        record_index_within_page,
      }));
    }
    current_key = copy_bytes(entry.child_hash, "directory child key")?;
    expected_level = Some(directory.level - 1);
    expected_child = Some(ChildExpectationV1::from_entry(entry)?);
  }
}

fn descend_to_page_id(
  request: &ArtifactPageCursorRequestV1<'_>,
  page_id: u64,
  source: &mut dyn ArtifactCursorSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut InputBudgetV1,
) -> Result<Option<TraversedPathV1>, ArtifactPageCursorErrorV1> {
  let root = load_artifact(request.root.root_key, source, is_cancelled, budget)?;
  let mut search = PageIdSearchV1 { request, page_id, source, is_cancelled, budget, found: None };
  search.search(PageIdSearchNodeV1 {
    retained: root,
    expected_level: None,
    expected_child: None,
    directories: Vec::new(),
    selected_entries: Vec::new(),
    page_ordinal: 0,
    live_rank_before_page: 0,
  })?;
  Ok(search.found)
}

struct PageIdSearchNodeV1 {
  retained: RetainedArtifactBytesV1,
  expected_level: Option<u16>,
  expected_child: Option<ChildExpectationV1>,
  directories: Vec<RetainedArtifactBytesV1>,
  selected_entries: Vec<usize>,
  page_ordinal: u64,
  live_rank_before_page: u64,
}

struct PageIdSearchV1<'request, 'source> {
  request: &'request ArtifactPageCursorRequestV1<'request>,
  page_id: u64,
  source: &'source mut dyn ArtifactCursorSourceV1,
  is_cancelled: &'source dyn Fn() -> bool,
  budget: &'source mut InputBudgetV1,
  found: Option<TraversedPathV1>,
}

impl PageIdSearchV1<'_, '_> {
  fn search(&mut self, node: PageIdSearchNodeV1) -> Result<(), ArtifactPageCursorErrorV1> {
    check_cancelled(self.is_cancelled)?;
    if node.directories.len() >= self.request.limits.maximum_directory_depth() {
      return Err(amplification_error("PageId search exceeds its directory-depth limit"));
    }
    let directory = decode_artifact_directory(node.retained.bytes(), self.request.root.hash_algorithm)?;
    validate_directory_identity(&directory, self.request.root, node.expected_level)?;
    if let Some(expected_child) = node.expected_child.as_ref() {
      validate_directory_child(&directory, expected_child)?;
    } else {
      if directory.key != self.request.root.root_key {
        return Err(closure_error("PageId search root key disagrees with the selected root"));
      }
      validate_root_summary(&directory, self.request.root.expected_summary)?;
      validate_seek_against_root(&directory, self.request.root.hash_algorithm, self.request.seek)?;
      if usize::from(directory.level).checked_add(1).is_none_or(|depth| depth > self.request.limits.maximum_directory_depth()) {
        return Err(amplification_error("PageId search root level exceeds its directory-depth limit"));
      }
    }

    let mut preceding_pages = 0u64;
    let mut preceding_live = 0u64;
    for (index, entry) in directory.entries.iter().enumerate() {
      if self.page_id >= entry.minimum_page_id && self.page_id <= entry.maximum_page_id {
        let child_page_ordinal = node.page_ordinal.checked_add(preceding_pages).ok_or_else(|| rank_error("PageId page rank overflowed"))?;
        let child_live_rank =
          node.live_rank_before_page.checked_add(preceding_live).ok_or_else(|| rank_error("PageId live rank overflowed"))?;
        let mut child_directories = clone_path_prefix(&node.directories, node.directories.len())?;
        child_directories
          .try_reserve(1)
          .map_err(|error| ArtifactPageCursorErrorV1::Allocation(format!("PageId directory path reservation failed: {error}")))?;
        child_directories.push(node.retained.clone());
        let mut child_selected = clone_selected_entries(&node.selected_entries)?;
        child_selected
          .try_reserve(1)
          .map_err(|error| ArtifactPageCursorErrorV1::Allocation(format!("PageId directory index reservation failed: {error}")))?;
        child_selected.push(index);
        if directory.level == 0 {
          let page = load_artifact(entry.child_hash, self.source, self.is_cancelled, self.budget)?;
          let decoded = decode_ordered_page(page.bytes(), self.request.root.hash_algorithm)?;
          validate_leaf_page(&decoded, entry, self.request.root)?;
          if decoded.page_id != self.page_id {
            return Err(closure_error("PageId leaf descriptor range does not resolve to its requested page"));
          }
          if self.found.is_some() {
            return Err(order_error("artifact directory resolves one PageId through multiple leaves"));
          }
          self.found = Some(TraversedPathV1 {
            directories: child_directories,
            selected_entries: child_selected,
            page,
            page_ordinal: child_page_ordinal,
            live_rank_before_page: child_live_rank,
            live_rank_within_page: None,
            record_index_within_page: None,
          });
        } else {
          let child = load_artifact(entry.child_hash, self.source, self.is_cancelled, self.budget)?;
          self.search(PageIdSearchNodeV1 {
            retained: child,
            expected_level: Some(directory.level - 1),
            expected_child: Some(ChildExpectationV1::from_entry(entry)?),
            directories: child_directories,
            selected_entries: child_selected,
            page_ordinal: child_page_ordinal,
            live_rank_before_page: child_live_rank,
          })?;
        }
      }
      preceding_pages = preceding_pages.checked_add(entry.page_count).ok_or_else(|| rank_error("PageId page rank overflowed"))?;
      preceding_live = preceding_live.checked_add(entry.live_count).ok_or_else(|| rank_error("PageId live rank overflowed"))?;
    }
    Ok(())
  }
}

fn validate_seek_against_root(
  root: &ArtifactDirectoryNodeV1<'_>,
  hash_algorithm: HashAlgorithm,
  seek: ArtifactPageSeekV1<'_>,
) -> Result<(), ArtifactPageCursorErrorV1> {
  match seek {
    ArtifactPageSeekV1::PageOrdinal(rank) if rank >= root.page_count => {
      Err(rank_error("requested page ordinal is outside the selected directory root"))
    }
    ArtifactPageSeekV1::LiveRecordRank(rank) if rank >= root.live_count => {
      Err(rank_error("requested live-record rank is outside the selected directory root"))
    }
    ArtifactPageSeekV1::PageId(0) => Err(identity_error("requested PageId is zero")),
    ArtifactPageSeekV1::PageId(_) if !root.role.uses_page_id() => Err(identity_error("requested PageId for a role without PageIds")),
    ArtifactPageSeekV1::OrderLowerBound(key) | ArtifactPageSeekV1::OrderPredecessor(key) => {
      compare_order_keys(hash_algorithm, root.role, key, key).map(|_| ()).map_err(Into::into)
    }
    ArtifactPageSeekV1::PageOrdinal(_) | ArtifactPageSeekV1::LiveRecordRank(_) | ArtifactPageSeekV1::PageId(_) => Ok(()),
  }
}

fn select_entry(
  directory: &ArtifactDirectoryNodeV1<'_>,
  hash_algorithm: HashAlgorithm,
  state: &mut SelectionStateV1<'_>,
) -> Result<Option<usize>, ArtifactPageCursorErrorV1> {
  match state.seek {
    ArtifactPageSeekV1::PageOrdinal(_) => select_ranked_entry(directory, state, false).map(Some),
    ArtifactPageSeekV1::LiveRecordRank(_) => select_ranked_entry(directory, state, true).map(Some),
    ArtifactPageSeekV1::OrderLowerBound(key) => {
      compare_order_keys(hash_algorithm, directory.role, key, key)?;
      let mut selected = directory.entries.len() - 1;
      for (index, entry) in directory.entries.iter().enumerate() {
        if compare_order_keys(hash_algorithm, directory.role, key, entry.upper_fence)? != std::cmp::Ordering::Greater {
          selected = index;
          break;
        }
      }
      add_preceding_ranks(directory, selected, state)?;
      Ok(Some(selected))
    }
    ArtifactPageSeekV1::OrderPredecessor(key) => {
      compare_order_keys(hash_algorithm, directory.role, key, key)?;
      let mut selected = 0usize;
      for (index, entry) in directory.entries.iter().enumerate() {
        if compare_order_keys(hash_algorithm, directory.role, entry.lower_fence, key)? == std::cmp::Ordering::Greater {
          break;
        }
        selected = index;
      }
      add_preceding_ranks(directory, selected, state)?;
      Ok(Some(selected))
    }
    ArtifactPageSeekV1::PageId(page_id) => {
      let mut selected = None;
      for (index, entry) in directory.entries.iter().enumerate() {
        if page_id >= entry.minimum_page_id && page_id <= entry.maximum_page_id {
          if selected.is_some() {
            return Err(order_error("artifact directory repeats one PageId range"));
          }
          selected = Some(index);
        }
      }
      if let Some(index) = selected {
        add_preceding_ranks(directory, index, state)?;
      }
      Ok(selected)
    }
  }
}

fn select_ranked_entry(
  directory: &ArtifactDirectoryNodeV1<'_>,
  state: &mut SelectionStateV1<'_>,
  by_live_rank: bool,
) -> Result<usize, ArtifactPageCursorErrorV1> {
  for (index, entry) in directory.entries.iter().enumerate() {
    let count = if by_live_rank { entry.live_count } else { entry.page_count };
    if state.remaining < count {
      add_preceding_ranks(directory, index, state)?;
      return Ok(index);
    }
    state.remaining = state.remaining.checked_sub(count).ok_or_else(|| rank_error("artifact cursor rank subtraction underflowed"))?;
  }
  Err(rank_error("artifact directory aggregate rank does not select a child"))
}

fn add_preceding_ranks(
  directory: &ArtifactDirectoryNodeV1<'_>,
  selected: usize,
  state: &mut SelectionStateV1<'_>,
) -> Result<(), ArtifactPageCursorErrorV1> {
  for entry in &directory.entries[..selected] {
    state.page_ordinal = state.page_ordinal.checked_add(entry.page_count).ok_or_else(|| rank_error("page ordinal overflowed"))?;
    state.live_rank_before_page =
      state.live_rank_before_page.checked_add(entry.live_count).ok_or_else(|| rank_error("live-record rank overflowed"))?;
  }
  Ok(())
}

fn live_record_index_within_page(page: &OrderedPageV1<'_>, live_rank: u64) -> Result<usize, ArtifactPageCursorErrorV1> {
  if live_rank >= u64::from(page.live_count) {
    return Err(rank_error("selected page does not contain the requested live-record rank"));
  }
  let mut observed = 0u64;
  for (index, record) in page.records.iter().enumerate() {
    let record = record?;
    if !record.tombstone {
      if observed == live_rank {
        return Ok(index);
      }
      observed = observed.checked_add(1).ok_or_else(|| rank_error("page live-record rank overflowed"))?;
    }
  }
  Err(closure_error("ordered page live count does not cover the selected live-record rank"))
}

#[derive(Clone, Copy)]
enum NeighborDirectionV1 {
  Previous,
  Next,
}

fn locate_neighbor(
  request: &ArtifactPageCursorRequestV1<'_>,
  path: &TraversedPathV1,
  direction: NeighborDirectionV1,
  source: &mut dyn ArtifactCursorSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut InputBudgetV1,
) -> Result<Option<LoadedNeighborV1>, ArtifactPageCursorErrorV1> {
  for directory_index in (0..path.directories.len()).rev() {
    let directory = decode_artifact_directory(path.directories[directory_index].bytes(), request.root.hash_algorithm)?;
    let selected = path.selected_entries[directory_index];
    let sibling = match direction {
      NeighborDirectionV1::Previous => selected.checked_sub(1),
      NeighborDirectionV1::Next => selected.checked_add(1).filter(|index| *index < directory.entries.len()),
    };
    let Some(sibling) = sibling else {
      continue;
    };
    let mut directories = clone_path_prefix(&path.directories, directory_index + 1)?;
    let entry = &directory.entries[sibling];
    if directory.level == 0 {
      let page = load_artifact(entry.child_hash, source, is_cancelled, budget)?;
      let decoded = decode_ordered_page(page.bytes(), request.root.hash_algorithm)?;
      validate_leaf_page(&decoded, entry, request.root)?;
      return Ok(Some(LoadedNeighborV1 { directories, page }));
    }
    let mut key = copy_bytes(entry.child_hash, "neighbor directory key")?;
    let mut expected_level = directory.level - 1;
    let mut expected = ChildExpectationV1::from_entry(entry)?;
    loop {
      check_cancelled(is_cancelled)?;
      if directories.len() >= request.limits.maximum_directory_depth() {
        return Err(amplification_error("artifact cursor neighbor path exceeds its directory-depth limit"));
      }
      let retained = load_artifact(&key, source, is_cancelled, budget)?;
      let child = decode_artifact_directory(retained.bytes(), request.root.hash_algorithm)?;
      validate_directory_identity(&child, request.root, Some(expected_level))?;
      validate_directory_child(&child, &expected)?;
      directories
        .try_reserve(1)
        .map_err(|error| ArtifactPageCursorErrorV1::Allocation(format!("neighbor path reservation failed: {error}")))?;
      directories.push(retained.clone());
      let child_entry = match direction {
        NeighborDirectionV1::Previous => child.entries.last(),
        NeighborDirectionV1::Next => child.entries.first(),
      }
      .ok_or_else(|| closure_error("artifact cursor neighbor directory is empty"))?;
      if child.level == 0 {
        let page = load_artifact(child_entry.child_hash, source, is_cancelled, budget)?;
        let decoded = decode_ordered_page(page.bytes(), request.root.hash_algorithm)?;
        validate_leaf_page(&decoded, child_entry, request.root)?;
        return Ok(Some(LoadedNeighborV1 { directories, page }));
      }
      key = copy_bytes(child_entry.child_hash, "neighbor child key")?;
      expected_level = child.level - 1;
      expected = ChildExpectationV1::from_entry(child_entry)?;
    }
  }
  Ok(None)
}

fn validate_neighbor_continuity(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  mode: ArtifactPageNeighborModeV1,
  current: &OrderedPageV1<'_>,
  previous: Option<&LoadedNeighborV1>,
  next: Option<&LoadedNeighborV1>,
) -> Result<(), ArtifactPageCursorErrorV1> {
  if role != OrderedIndexRoleV1::Posting {
    return Ok(());
  }
  if mode == ArtifactPageNeighborModeV1::Both {
    match previous {
      Some(previous) => {
        let previous = decode_ordered_page(previous.page.bytes(), hash_algorithm)?;
        validate_posting_page_link(&previous, current, hash_algorithm)?;
      }
      None if current.previous_page_id != 0 => return Err(closure_error("posting page names a predecessor absent from its directory")),
      None => {}
    }
  }
  if mode != ArtifactPageNeighborModeV1::None {
    match next {
      Some(next) => {
        let next = decode_ordered_page(next.page.bytes(), hash_algorithm)?;
        validate_posting_page_link(current, &next, hash_algorithm)?;
      }
      None if current.next_page_id != 0 => return Err(closure_error("posting page names a successor absent from its directory")),
      None => {}
    }
  }
  Ok(())
}

fn validate_request(request: &ArtifactPageCursorRequestV1<'_>) -> Result<(), ArtifactPageCursorErrorV1> {
  ArtifactPageCursorLimitsV1::new(request.limits.maximum_directory_depth(), request.limits.maximum_input_bytes())?;
  let hash_width = request.root.hash_algorithm.hash_length();
  if request.root.root_key.len() != hash_width
    || request.root.root_key.iter().all(|byte| *byte == 0)
    || request.root.owner_id.len() != hash_width
    || request.root.owner_id.iter().all(|byte| *byte == 0)
    || request.root.maximum_generation == 0
    || request.root.role == OrderedIndexRoleV1::NvtTile
  {
    return Err(identity_error("artifact cursor root key, owner, generation, or role is invalid"));
  }
  if let ArtifactPageSeekV1::OrderLowerBound(key) | ArtifactPageSeekV1::OrderPredecessor(key) = request.seek {
    compare_order_keys(request.root.hash_algorithm, request.root.role, key, key)?;
  }
  if let ArtifactPageSeekV1::PageId(page_id) = request.seek {
    if page_id == 0 || !request.root.role.uses_page_id() {
      return Err(identity_error("artifact cursor PageId seek is zero or uses a role without PageIds"));
    }
  }
  Ok(())
}

fn validate_directory_identity(
  directory: &ArtifactDirectoryNodeV1<'_>,
  root: ArtifactPageCursorRootV1<'_>,
  expected_level: Option<u16>,
) -> Result<(), ArtifactPageCursorErrorV1> {
  if directory.owner_id != root.owner_id
    || directory.role != root.role
    || directory.generation > root.maximum_generation
    || expected_level.is_some_and(|level| directory.level != level)
  {
    return Err(closure_error("artifact directory owner, role, generation, or level disagrees with its selected root"));
  }
  Ok(())
}

fn validate_root_summary(
  directory: &ArtifactDirectoryNodeV1<'_>,
  expected: Option<ArtifactDirectoryRootSummaryV1>,
) -> Result<(), ArtifactPageCursorErrorV1> {
  if directory.key.is_empty() {
    return Err(closure_error("artifact directory root has no canonical key"));
  }
  if let Some(expected) = expected {
    if directory.live_count != expected.live_count
      || directory.tombstone_count != expected.tombstone_count
      || directory.page_count != expected.page_count
      || directory.logical_bytes != expected.logical_bytes
      || directory.minimum_page_id != expected.minimum_page_id
      || directory.maximum_page_id != expected.maximum_page_id
    {
      return Err(closure_error("artifact directory root disagrees with its exact manifest summary"));
    }
  }
  Ok(())
}

fn validate_directory_child(
  directory: &ArtifactDirectoryNodeV1<'_>,
  expected: &ChildExpectationV1,
) -> Result<(), ArtifactPageCursorErrorV1> {
  if directory.key != expected.child_hash
    || directory.generation != expected.child_generation
    || directory.lower_fence != expected.lower_fence
    || directory.upper_fence != expected.upper_fence
    || directory.live_count != expected.live_count
    || directory.tombstone_count != expected.tombstone_count
    || directory.page_count != expected.page_count
    || directory.logical_bytes != expected.logical_bytes
    || directory.minimum_page_id != expected.minimum_page_id
    || directory.maximum_page_id != expected.maximum_page_id
  {
    return Err(closure_error("artifact directory disagrees with its exact parent descriptor"));
  }
  Ok(())
}

fn validate_leaf_page(
  page: &OrderedPageV1<'_>,
  descriptor: &ArtifactDirectoryEntryV1<'_>,
  root: ArtifactPageCursorRootV1<'_>,
) -> Result<(), ArtifactPageCursorErrorV1> {
  if page.key != descriptor.child_hash
    || page.owner_id != root.owner_id
    || page.role != root.role
    || page.generation != descriptor.child_generation
    || page.generation > root.maximum_generation
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

fn load_artifact(
  key: &[u8],
  source: &mut dyn ArtifactCursorSourceV1,
  is_cancelled: &dyn Fn() -> bool,
  budget: &mut InputBudgetV1,
) -> Result<RetainedArtifactBytesV1, ArtifactPageCursorErrorV1> {
  check_cancelled(is_cancelled)?;
  if let Some(value) = budget.retained(key) {
    return Ok(value);
  }
  let maximum = budget.remaining(key);
  if maximum == 0 {
    return Err(amplification_error("artifact cursor has no remaining source-read budget"));
  }
  let value = source.read_immutable_artifact(key, maximum).map_err(|error| map_source_error(key, error))?;
  check_cancelled(is_cancelled)?;
  if value.bytes().len() > maximum {
    return Err(ArtifactPageCursorErrorV1::SourceCorrupt("artifact source returned more bytes than its supplied ceiling".to_owned()));
  }
  budget.retain(key, &value)?;
  Ok(value)
}

fn clone_path_prefix(path: &[RetainedArtifactBytesV1], length: usize) -> Result<Vec<RetainedArtifactBytesV1>, ArtifactPageCursorErrorV1> {
  let mut cloned = Vec::new();
  cloned
    .try_reserve_exact(length)
    .map_err(|error| ArtifactPageCursorErrorV1::Allocation(format!("artifact cursor path clone failed: {error}")))?;
  cloned.extend(path[..length].iter().cloned());
  Ok(cloned)
}

fn clone_selected_entries(entries: &[usize]) -> Result<Vec<usize>, ArtifactPageCursorErrorV1> {
  let mut cloned = Vec::new();
  cloned
    .try_reserve_exact(entries.len())
    .map_err(|error| ArtifactPageCursorErrorV1::Allocation(format!("artifact cursor index clone failed: {error}")))?;
  cloned.extend_from_slice(entries);
  Ok(cloned)
}

fn copy_bytes(value: &[u8], context: &'static str) -> Result<Vec<u8>, ArtifactPageCursorErrorV1> {
  let mut copied = Vec::new();
  copied
    .try_reserve_exact(value.len())
    .map_err(|error| ArtifactPageCursorErrorV1::Allocation(format!("artifact cursor {context} allocation failed: {error}")))?;
  copied.extend_from_slice(value);
  Ok(copied)
}

fn map_source_error(key: &[u8], error: ArtifactCursorReadErrorV1) -> ArtifactPageCursorErrorV1 {
  match error {
    ArtifactCursorReadErrorV1::Missing => ArtifactPageCursorErrorV1::MissingArtifact { key: hex::encode(key) },
    ArtifactCursorReadErrorV1::Cancelled => ArtifactPageCursorErrorV1::Cancelled,
    ArtifactCursorReadErrorV1::ResourcePressure(context) => ArtifactPageCursorErrorV1::SourcePressure(context),
    ArtifactCursorReadErrorV1::Corrupt(context) => ArtifactPageCursorErrorV1::SourceCorrupt(context),
    ArtifactCursorReadErrorV1::Operational(context) => ArtifactPageCursorErrorV1::SourceOperational(context),
  }
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<(), ArtifactPageCursorErrorV1> {
  if is_cancelled() {
    Err(ArtifactPageCursorErrorV1::Cancelled)
  } else {
    Ok(())
  }
}

fn malformed(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> ArtifactPageCursorErrorV1 {
  ArtifactPageCursorErrorV1::Malformed(FormatError::new(class, code, context))
}

fn closure_error(context: impl Into<String>) -> ArtifactPageCursorErrorV1 {
  malformed(MalformedInputClass::CrossRecordClosureMismatch, "artifact_cursor_closure", context)
}

fn order_error(context: impl Into<String>) -> ArtifactPageCursorErrorV1 {
  malformed(MalformedInputClass::NoncanonicalOrderOrDuplicate, "artifact_cursor_order", context)
}

fn identity_error(context: impl Into<String>) -> ArtifactPageCursorErrorV1 {
  malformed(MalformedInputClass::IdentityKeyOrGenerationMismatch, "artifact_cursor_identity", context)
}

fn rank_error(context: impl Into<String>) -> ArtifactPageCursorErrorV1 {
  malformed(MalformedInputClass::CrossRecordClosureMismatch, "artifact_cursor_rank", context)
}

fn amplification_error(context: impl Into<String>) -> ArtifactPageCursorErrorV1 {
  malformed(MalformedInputClass::AllocationAmplification, "artifact_cursor_resource", context)
}

fn invalid_limits(context: impl Into<String>) -> ArtifactPageCursorErrorV1 {
  ArtifactPageCursorErrorV1::InvalidLimits(context.into())
}
