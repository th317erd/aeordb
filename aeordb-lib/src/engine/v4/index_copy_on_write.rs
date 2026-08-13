use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::mem::size_of;
use std::ops::Range;

use crate::engine::HashAlgorithm;

use super::index_artifact::{
  EncodedImmutableIndexArtifactV1, checked_immutable_index_artifact_encoded_length, checked_immutable_index_artifact_representable_length,
};
use super::index_page::{
  ArtifactDirectoryEntryV1, ArtifactDirectoryEntryWriteV1, ArtifactDirectoryNodeV1, ArtifactDirectoryWriteV1, OrderedIndexRoleV1,
  OrderedPageV1, OrderedPageWriteV1, PhysicalHintV1, compare_order_keys, decode_artifact_directory, decode_ordered_page,
  decode_ordered_record, encode_artifact_directory, encode_ordered_page, ordered_record_order_key, validate_posting_page_link,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

pub const INDEX_PAGE_TARGET_BYTES_V1: usize = 64 * 1_024;
pub const INDEX_PAGE_SPLIT_ABOVE_BYTES_V1: usize = 96 * 1_024;
pub const INDEX_PAGE_MERGE_BELOW_BYTES_V1: usize = 16 * 1_024;
pub const INDEX_ARTIFACT_HARD_CAP_BYTES_V1: usize = 4 * 1_024 * 1_024;
pub const INDEX_COPY_ON_WRITE_WORKSPACE_BYTES_V1: usize = 32 * 1_024 * 1_024;
pub const INDEX_DIRECTORY_TARGET_BYTES_V1: usize = 64 * 1_024;
pub const INDEX_DIRECTORY_COPY_ON_WRITE_WORKSPACE_BYTES_V1: usize = 64 * 1_024 * 1_024;
pub const INDEX_DIRECTORY_MAXIMUM_AFFECTED_PATHS_V1: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexPageLayoutV1 {
  pub target_bytes: usize,
  pub split_above_bytes: usize,
  pub merge_below_bytes: usize,
  pub hard_artifact_bytes: usize,
  pub maximum_workspace_bytes: usize,
}

pub const fn default_index_page_layout_v1() -> IndexPageLayoutV1 {
  IndexPageLayoutV1 {
    target_bytes: INDEX_PAGE_TARGET_BYTES_V1,
    split_above_bytes: INDEX_PAGE_SPLIT_ABOVE_BYTES_V1,
    merge_below_bytes: INDEX_PAGE_MERGE_BELOW_BYTES_V1,
    hard_artifact_bytes: INDEX_ARTIFACT_HARD_CAP_BYTES_V1,
    maximum_workspace_bytes: INDEX_COPY_ON_WRITE_WORKSPACE_BYTES_V1,
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexDirectoryLayoutV1 {
  pub target_bytes: usize,
  pub hard_artifact_bytes: usize,
  pub maximum_workspace_bytes: usize,
}

pub const fn default_index_directory_layout_v1() -> IndexDirectoryLayoutV1 {
  IndexDirectoryLayoutV1 {
    target_bytes: INDEX_DIRECTORY_TARGET_BYTES_V1,
    hard_artifact_bytes: INDEX_ARTIFACT_HARD_CAP_BYTES_V1,
    maximum_workspace_bytes: INDEX_DIRECTORY_COPY_ON_WRITE_WORKSPACE_BYTES_V1,
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedPageMutationKindV1<'a> {
  UpsertLive(&'a [u8]),
  TombstoneExisting(&'a [u8]),
}

impl<'a> OrderedPageMutationKindV1<'a> {
  fn encoded_record(self) -> &'a [u8] {
    match self {
      Self::UpsertLive(record) | Self::TombstoneExisting(record) => record,
    }
  }

  fn expects_tombstone(self) -> bool {
    matches!(self, Self::TombstoneExisting(_))
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderedPageMutationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub source_page: &'a [u8],
  pub next_posting_page: Option<&'a [u8]>,
  pub generation: u64,
  pub next_page_id: u64,
  pub mutation: OrderedPageMutationKindV1<'a>,
  pub layout: IndexPageLayoutV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TombstoneDropProofV1<'a> {
  pub owner_id: &'a [u8],
  pub source_page_keys: &'a [&'a [u8]],
  pub coverage_epoch_id: u64,
  pub covered_through_sequence: u64,
  pub journal_contiguous_through_sequence: u64,
  pub pin_safe_through_generation: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct OrderedPageCompactionWindowRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub source_pages: &'a [&'a [u8]],
  pub previous_posting_page: Option<&'a [u8]>,
  pub next_posting_page: Option<&'a [u8]>,
  pub generation: u64,
  pub next_page_id: u64,
  pub tombstone_drop_proof: Option<&'a TombstoneDropProofV1<'a>>,
  pub layout: IndexPageLayoutV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedPageReplacementV1 {
  pub source_key: Vec<u8>,
  pub source_page_id: u64,
  pub artifacts: Vec<EncodedImmutableIndexArtifactV1>,
  source_role: OrderedIndexRoleV1,
  source_owner_id: Vec<u8>,
  source_generation: u64,
  source_lower_fence: Vec<u8>,
  source_upper_fence: Vec<u8>,
  source_live_count: u64,
  source_tombstone_count: u64,
  source_logical_bytes: u64,
  source_retired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedPageMutationPlanV1 {
  pub replacements: Vec<OrderedPageReplacementV1>,
  pub allocated_page_ids: Vec<u64>,
  pub retired_page_ids: Vec<u64>,
  pub next_page_id: u64,
}

impl OrderedPageMutationPlanV1 {
  pub fn is_unchanged(&self) -> bool {
    self.replacements.is_empty()
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactDirectoryPathV1<'a> {
  pub source_page_key: &'a [u8],
  pub directories: &'a [&'a [u8]],
}

#[derive(Debug, Clone, Copy)]
pub struct ArtifactDirectoryMutationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub generation: u64,
  pub page_plan: &'a OrderedPageMutationPlanV1,
  pub paths: &'a [ArtifactDirectoryPathV1<'a>],
  pub layout: IndexDirectoryLayoutV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDirectoryMutationPlanV1 {
  pub source_root_key: Vec<u8>,
  pub root_key: Option<Vec<u8>>,
  pub root_level: u16,
  pub live_count: u64,
  pub tombstone_count: u64,
  pub page_count: u64,
  pub logical_bytes: u64,
  pub minimum_page_id: u64,
  pub maximum_page_id: u64,
  pub artifacts: Vec<EncodedImmutableIndexArtifactV1>,
}

#[derive(Debug)]
struct OwnedOrderedRecordV1 {
  encoded: Vec<u8>,
  order_key: Vec<u8>,
  tombstone: bool,
}

pub fn mutate_ordered_page_v1(request: &OrderedPageMutationRequestV1<'_>) -> FormatResult<OrderedPageMutationPlanV1> {
  validate_layout(request.layout)?;
  let source = decode_ordered_page(request.source_page, request.hash_algorithm)?;
  validate_mutation_identity(request, &source)?;
  let mutation_record = decode_ordered_record(request.mutation.encoded_record(), request.hash_algorithm, source.role)?;
  if mutation_record.tombstone != request.mutation.expects_tombstone() {
    return Err(closure_error("index_cow_mutation_tombstone", "mutation kind disagrees with the encoded record tombstone flag"));
  }

  let mut records = collect_owned_records(&source, request.layout.maximum_workspace_bytes)?;
  let mutation_order_key = ordered_record_order_key(&mutation_record)?;
  let mut matching_index = None;
  let mut insertion_index = records.len();
  for (index, existing) in records.iter().enumerate() {
    match compare_order_keys(request.hash_algorithm, source.role, &existing.order_key, &mutation_order_key)? {
      Ordering::Less => {}
      Ordering::Equal => {
        matching_index = Some(index);
        insertion_index = index;
        break;
      }
      Ordering::Greater => {
        insertion_index = index;
        break;
      }
    }
  }

  if request.mutation.expects_tombstone() && matching_index.is_none() {
    return Err(closure_error("index_cow_tombstone_missing", "cannot tombstone an ordered record that is not present"));
  }
  if matching_index.is_some_and(|index| records[index].encoded == request.mutation.encoded_record()) {
    return Ok(OrderedPageMutationPlanV1 {
      replacements: Vec::new(),
      allocated_page_ids: Vec::new(),
      retired_page_ids: Vec::new(),
      next_page_id: request.next_page_id,
    });
  }

  preflight_record_mutation_workspace(
    &records,
    matching_index,
    request.mutation.encoded_record().len(),
    mutation_order_key.len(),
    request.layout.maximum_workspace_bytes,
  )?;
  let replacement = OwnedOrderedRecordV1 {
    encoded: request.mutation.encoded_record().to_vec(),
    order_key: mutation_order_key,
    tombstone: mutation_record.tombstone,
  };
  if let Some(index) = matching_index {
    records[index] = replacement;
  } else {
    records.insert(insertion_index, replacement);
  }
  let record_workspace_bytes = validate_workspace(&records, request.layout.maximum_workspace_bytes)?;

  let groups = partition_records(request, &source, &records)?;
  let relinked_next = if source.role == OrderedIndexRoleV1::Posting && groups.len() > 1 && source.next_page_id != 0 {
    let next_bytes = request.next_posting_page.ok_or_else(|| {
      closure_error("index_cow_next_page_missing", "posting split requires the current next page for bidirectional relinking")
    })?;
    let next = decode_ordered_page(next_bytes, request.hash_algorithm)?;
    validate_relinked_next(request, &source, &next)?;
    Some(next)
  } else {
    None
  };
  preflight_output_workspace(request, &source, &records, &groups, record_workspace_bytes, relinked_next.as_ref())?;
  let mut page_id_allocator = PageIdentityAllocatorV1::new(request.next_page_id);
  let mut page_ids = Vec::with_capacity(groups.len());
  page_ids.push(source.page_id);
  if source.role.uses_page_id() {
    for _ in 1..groups.len() {
      page_ids.push(page_id_allocator.allocate()?);
    }
  } else {
    page_ids.resize(groups.len(), 0);
  }

  let mut artifacts = Vec::with_capacity(groups.len());
  for (index, range) in groups.iter().enumerate() {
    let records = record_slices(&records[range.clone()]);
    let previous_page_id = if source.role == OrderedIndexRoleV1::Posting {
      if index == 0 {
        source.previous_page_id
      } else {
        page_ids[index - 1]
      }
    } else {
      0
    };
    let next_page_id = if source.role == OrderedIndexRoleV1::Posting {
      if index + 1 < groups.len() {
        page_ids[index + 1]
      } else {
        source.next_page_id
      }
    } else {
      0
    };
    artifacts.push(encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm: request.hash_algorithm,
      role: source.role,
      owner_id: source.owner_id,
      generation: request.generation,
      page_id: page_ids[index],
      previous_page_id,
      next_page_id,
      records: &records,
    })?);
  }

  let mut replacements = vec![ordered_page_replacement(&source, artifacts)?];
  if let Some(next) = relinked_next {
    let next_records = next.records.iter().collect::<FormatResult<Vec<_>>>()?;
    let next_record_slices = next_records.iter().map(|record| record.encoded).collect::<Vec<_>>();
    let rewritten_next = encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm: request.hash_algorithm,
      role: next.role,
      owner_id: next.owner_id,
      generation: request.generation,
      page_id: next.page_id,
      previous_page_id: *page_ids
        .last()
        .ok_or_else(|| closure_error("index_cow_empty_partition", "mutation produced no page partitions"))?,
      next_page_id: next.next_page_id,
      records: &next_record_slices,
    })?;
    replacements.push(ordered_page_replacement(&next, vec![rewritten_next])?);
  }

  Ok(OrderedPageMutationPlanV1 {
    replacements,
    allocated_page_ids: page_id_allocator.allocated_page_ids,
    retired_page_ids: Vec::new(),
    next_page_id: page_id_allocator.next_page_id,
  })
}

pub fn compact_ordered_page_window_v1(request: &OrderedPageCompactionWindowRequestV1<'_>) -> FormatResult<OrderedPageMutationPlanV1> {
  validate_layout(request.layout)?;
  if request.source_pages.is_empty() || request.source_pages.len() > 2 {
    return Err(amplification_error(
      "index_cow_compaction_window",
      format!("compaction source-page count {} is outside 1..=2", request.source_pages.len()),
    ));
  }

  let mut sources = Vec::new();
  if let Err(error) = sources.try_reserve_exact(request.source_pages.len()) {
    return Err(amplification_error("index_cow_compaction_sources", format!("source-page reservation failed: {error}")));
  }
  for source_page in request.source_pages {
    sources.push(decode_ordered_page(source_page, request.hash_algorithm)?);
  }
  let previous = request.previous_posting_page.map(|page| decode_ordered_page(page, request.hash_algorithm)).transpose()?;
  let next = request.next_posting_page.map(|page| decode_ordered_page(page, request.hash_algorithm)).transpose()?;
  validate_compaction_window(request, &sources, previous.as_ref(), next.as_ref())?;

  let drop_tombstones = if let Some(proof) = request.tombstone_drop_proof {
    validate_tombstone_drop_proof(request, &sources, proof)?;
    true
  } else {
    false
  };
  let mut records = collect_compaction_records(&sources, request.layout.maximum_workspace_bytes)?;
  let original_record_count = records.len();
  if drop_tombstones {
    records.retain(|record| !record.tombstone);
  }
  let dropped_tombstones = records.len() != original_record_count;

  if sources.len() == 1 && !dropped_tombstones {
    return Ok(unchanged_page_plan(request.next_page_id));
  }
  if sources.len() == 2 && !dropped_tombstones && request.source_pages.iter().all(|page| page.len() >= request.layout.merge_below_bytes) {
    return Ok(unchanged_page_plan(request.next_page_id));
  }

  let role = sources[0].role;
  let retained_source_index = if role.uses_page_id() && sources.len() == 2 && sources[1].page_id < sources[0].page_id { 1 } else { 0 };
  let retained_page_id = sources[retained_source_index].page_id;
  let previous_page_id = if role == OrderedIndexRoleV1::Posting { sources[0].previous_page_id } else { 0 };
  let next_page_id = if role == OrderedIndexRoleV1::Posting { sources[sources.len() - 1].next_page_id } else { 0 };

  if sources.len() == 2 && !records.is_empty() {
    let merged_length = checked_page_range_representable_length(request.hash_algorithm, role, &records)?;
    if merged_length > request.layout.target_bytes {
      return Ok(unchanged_page_plan(request.next_page_id));
    }
  }
  let rewrite_previous =
    role == OrderedIndexRoleV1::Posting && previous_page_id != 0 && (records.is_empty() || retained_page_id != sources[0].page_id);
  let rewrite_next = role == OrderedIndexRoleV1::Posting
    && next_page_id != 0
    && (records.is_empty() || retained_page_id != sources[sources.len() - 1].page_id);
  if rewrite_previous && previous.is_none() {
    return Err(closure_error(
      "index_cow_compaction_previous_missing",
      "compaction requires the previous posting page before constructing output",
    ));
  }
  if rewrite_next && next.is_none() {
    return Err(closure_error("index_cow_compaction_next_missing", "compaction requires the next posting page before constructing output"));
  }
  let record_workspace_bytes = validate_workspace(&records, request.layout.maximum_workspace_bytes)?;
  preflight_compaction_workspace(
    request,
    &sources,
    &records,
    record_workspace_bytes,
    rewrite_previous.then_some(previous.as_ref()).flatten(),
    rewrite_next.then_some(next.as_ref()).flatten(),
  )?;

  let output = if records.is_empty() {
    None
  } else {
    let record_slices = record_slices(&records);
    Some(encode_ordered_page(&OrderedPageWriteV1 {
      hash_algorithm: request.hash_algorithm,
      role,
      owner_id: sources[0].owner_id,
      generation: request.generation,
      page_id: retained_page_id,
      previous_page_id,
      next_page_id,
      records: &record_slices,
    })?)
  };

  let mut replacements = Vec::new();
  if let Err(error) = replacements.try_reserve_exact(sources.len() + usize::from(rewrite_previous) + usize::from(rewrite_next)) {
    return Err(amplification_error("index_cow_compaction_replacements", format!("replacement reservation failed: {error}")));
  }
  let mut output = output;
  let mut retired_page_ids = Vec::new();
  for (source_index, source) in sources.iter().enumerate() {
    let retains_output = output.is_some() && source_index == retained_source_index;
    let artifacts = if retains_output {
      vec![output.take().ok_or_else(|| closure_error("index_cow_compaction_output", "retained compaction output disappeared"))?]
    } else {
      Vec::new()
    };
    let source_retired = !retains_output;
    if source_retired && role.uses_page_id() {
      retired_page_ids.push(source.page_id);
    }
    replacements.push(ordered_page_replacement_with_retirement(source, artifacts, source_retired)?);
  }

  if rewrite_previous {
    let previous = previous.as_ref().ok_or_else(|| {
      closure_error("index_cow_compaction_previous_missing", "compaction requires the previous posting page for relinking")
    })?;
    let rewritten = rewrite_posting_page_links(
      request,
      previous,
      previous.previous_page_id,
      next_page_id_for_previous(&records, retained_page_id, next_page_id),
    )?;
    replacements.push(ordered_page_replacement(previous, vec![rewritten])?);
  }
  if rewrite_next {
    let next = next
      .as_ref()
      .ok_or_else(|| closure_error("index_cow_compaction_next_missing", "compaction requires the next posting page for relinking"))?;
    let previous_page_id = if records.is_empty() { previous_page_id } else { retained_page_id };
    let rewritten = rewrite_posting_page_links(request, next, previous_page_id, next.next_page_id)?;
    replacements.push(ordered_page_replacement(next, vec![rewritten])?);
  }
  retired_page_ids.sort_unstable();

  Ok(OrderedPageMutationPlanV1 { replacements, allocated_page_ids: Vec::new(), retired_page_ids, next_page_id: request.next_page_id })
}

fn unchanged_page_plan(next_page_id: u64) -> OrderedPageMutationPlanV1 {
  OrderedPageMutationPlanV1 { replacements: Vec::new(), allocated_page_ids: Vec::new(), retired_page_ids: Vec::new(), next_page_id }
}

fn next_page_id_for_previous(records: &[OwnedOrderedRecordV1], retained_page_id: u64, outward_next_page_id: u64) -> u64 {
  if records.is_empty() {
    outward_next_page_id
  } else {
    retained_page_id
  }
}

fn rewrite_posting_page_links(
  request: &OrderedPageCompactionWindowRequestV1<'_>,
  source: &OrderedPageV1<'_>,
  previous_page_id: u64,
  next_page_id: u64,
) -> FormatResult<EncodedImmutableIndexArtifactV1> {
  let records = source.records.iter().collect::<FormatResult<Vec<_>>>()?;
  let record_slices = records.iter().map(|record| record.encoded).collect::<Vec<_>>();
  encode_ordered_page(&OrderedPageWriteV1 {
    hash_algorithm: request.hash_algorithm,
    role: source.role,
    owner_id: source.owner_id,
    generation: request.generation,
    page_id: source.page_id,
    previous_page_id,
    next_page_id,
    records: &record_slices,
  })
}

fn ordered_page_replacement(
  source: &OrderedPageV1<'_>,
  artifacts: Vec<EncodedImmutableIndexArtifactV1>,
) -> FormatResult<OrderedPageReplacementV1> {
  ordered_page_replacement_with_retirement(source, artifacts, false)
}

fn ordered_page_replacement_with_retirement(
  source: &OrderedPageV1<'_>,
  artifacts: Vec<EncodedImmutableIndexArtifactV1>,
  source_retired: bool,
) -> FormatResult<OrderedPageReplacementV1> {
  Ok(OrderedPageReplacementV1 {
    source_key: source.key.clone(),
    source_page_id: source.page_id,
    artifacts,
    source_role: source.role,
    source_owner_id: copy_fallible_bytes(source.owner_id, "source page owner")?,
    source_generation: source.generation,
    source_lower_fence: copy_fallible_bytes(source.lower_fence, "source page lower fence")?,
    source_upper_fence: copy_fallible_bytes(source.upper_fence, "source page upper fence")?,
    source_live_count: u64::from(source.live_count),
    source_tombstone_count: u64::from(source.tombstone_count),
    source_logical_bytes: source.logical_live_bytes,
    source_retired,
  })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedDirectoryEntryV1 {
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

impl OwnedDirectoryEntryV1 {
  fn from_page(page: &OrderedPageV1<'_>) -> FormatResult<Self> {
    Ok(Self {
      lower_fence: copy_fallible_bytes(page.lower_fence, "page summary lower fence")?,
      upper_fence: copy_fallible_bytes(page.upper_fence, "page summary upper fence")?,
      child_hash: page.key.clone(),
      child_generation: page.generation,
      live_count: u64::from(page.live_count),
      tombstone_count: u64::from(page.tombstone_count),
      page_count: 1,
      logical_bytes: page.logical_live_bytes,
      minimum_page_id: page.page_id,
      maximum_page_id: page.page_id,
    })
  }

  fn from_directory(directory: &ArtifactDirectoryNodeV1<'_>) -> FormatResult<Self> {
    Ok(Self {
      lower_fence: copy_fallible_bytes(directory.lower_fence, "directory summary lower fence")?,
      upper_fence: copy_fallible_bytes(directory.upper_fence, "directory summary upper fence")?,
      child_hash: directory.key.clone(),
      child_generation: directory.generation,
      live_count: directory.live_count,
      tombstone_count: directory.tombstone_count,
      page_count: directory.page_count,
      logical_bytes: directory.logical_bytes,
      minimum_page_id: directory.minimum_page_id,
      maximum_page_id: directory.maximum_page_id,
    })
  }

  fn from_existing(entry: &ArtifactDirectoryEntryV1<'_>) -> FormatResult<Self> {
    Ok(Self {
      lower_fence: copy_fallible_bytes(entry.lower_fence, "existing directory-entry lower fence")?,
      upper_fence: copy_fallible_bytes(entry.upper_fence, "existing directory-entry upper fence")?,
      child_hash: copy_fallible_bytes(entry.child_hash, "existing directory-entry child hash")?,
      child_generation: entry.child_generation,
      live_count: entry.live_count,
      tombstone_count: entry.tombstone_count,
      page_count: entry.page_count,
      logical_bytes: entry.logical_bytes,
      minimum_page_id: entry.minimum_page_id,
      maximum_page_id: entry.maximum_page_id,
    })
  }

  fn as_write(&self) -> ArtifactDirectoryEntryWriteV1<'_> {
    ArtifactDirectoryEntryWriteV1 {
      lower_fence: &self.lower_fence,
      upper_fence: &self.upper_fence,
      child_hash: &self.child_hash,
      child_generation: self.child_generation,
      live_count: self.live_count,
      tombstone_count: self.tombstone_count,
      page_count: self.page_count,
      logical_bytes: self.logical_bytes,
      minimum_page_id: self.minimum_page_id,
      maximum_page_id: self.maximum_page_id,
      physical_hint: PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 },
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct DirectoryNodeLocationV1 {
  path_index: usize,
  directory_index: usize,
}

#[derive(Debug)]
struct DirectoryPathGraphV1 {
  source_root_key: Vec<u8>,
  root_level: u16,
  role: OrderedIndexRoleV1,
  owner_id: Vec<u8>,
  nodes: BTreeMap<Vec<u8>, DirectoryNodeLocationV1>,
  parent_by_child: BTreeMap<Vec<u8>, Vec<u8>>,
  leaf_by_page: BTreeMap<Vec<u8>, Vec<u8>>,
}

type ChildReplacementMapV1 = BTreeMap<Vec<u8>, Vec<OwnedDirectoryEntryV1>>;
type PendingDirectoryMapV1 = BTreeMap<Vec<u8>, ChildReplacementMapV1>;

pub fn rewrite_artifact_directory_paths_v1(
  request: &ArtifactDirectoryMutationRequestV1<'_>,
) -> FormatResult<ArtifactDirectoryMutationPlanV1> {
  validate_directory_layout(request.layout)?;
  if request.generation == 0 {
    return Err(identity_error("index_cow_directory_generation", "directory copy-on-write generation is zero"));
  }
  if request.page_plan.replacements.is_empty() {
    return Err(closure_error("index_cow_directory_no_replacements", "directory copy-on-write requires at least one page replacement"));
  }
  if request.paths.len() != request.page_plan.replacements.len()
    || request.paths.is_empty()
    || request.paths.len() > INDEX_DIRECTORY_MAXIMUM_AFFECTED_PATHS_V1
  {
    return Err(amplification_error(
      "index_cow_directory_path_count",
      format!(
        "{} paths do not match {} replacements or exceed the {}-path operation cap",
        request.paths.len(),
        request.page_plan.replacements.len(),
        INDEX_DIRECTORY_MAXIMUM_AFFECTED_PATHS_V1
      ),
    ));
  }

  let replacement_entries = validate_page_replacement_outputs(request)?;
  let graph = validate_directory_path_graph(request, &replacement_entries)?;
  let mut pending = PendingDirectoryMapV1::new();
  for (source_page_key, entries) in replacement_entries {
    let leaf_key = graph
      .leaf_by_page
      .get(source_page_key.as_slice())
      .ok_or_else(|| closure_error("index_cow_directory_path_missing", "validated replacement has no leaf path"))?
      .clone();
    let replaced_children = pending.entry(leaf_key).or_default();
    if replaced_children.insert(source_page_key, entries).is_some() {
      return Err(closure_error("index_cow_directory_duplicate_replacement", "one source page was replaced more than once"));
    }
  }

  let mut artifacts = Vec::new();
  let mut retained_artifact_bytes = 0usize;
  let mut root_entries = None;
  while !pending.is_empty() {
    let current = std::mem::take(&mut pending);
    for (source_node_key, child_replacements) in current {
      let location = graph
        .nodes
        .get(source_node_key.as_slice())
        .ok_or_else(|| closure_error("index_cow_directory_node_missing", "affected directory node is absent from the validated paths"))?;
      let source_bytes = request.paths[location.path_index].directories[location.directory_index];
      let source = decode_artifact_directory(source_bytes, request.hash_algorithm)?;
      let rewritten_entries = rewrite_directory_entries(&source, child_replacements, request.layout.maximum_workspace_bytes)?;
      let summaries = encode_directory_entry_groups(
        request,
        source.level,
        &graph.owner_id,
        graph.role,
        &rewritten_entries,
        &mut artifacts,
        &mut retained_artifact_bytes,
      )?;
      if let Some(parent_key) = graph.parent_by_child.get(source_node_key.as_slice()) {
        let parent_replacements = pending.entry(parent_key.clone()).or_default();
        if parent_replacements.insert(source_node_key, summaries).is_some() {
          return Err(closure_error("index_cow_directory_duplicate_child", "one directory child was rewritten more than once"));
        }
      } else if root_entries.replace(summaries).is_some() {
        return Err(closure_error("index_cow_directory_multiple_roots", "affected paths produced more than one source root"));
      }
    }
  }

  let mut root_entries =
    root_entries.ok_or_else(|| closure_error("index_cow_directory_root_missing", "affected paths produced no root"))?;
  let mut root_level = graph.root_level;
  while root_entries.len() > 1 {
    root_level = root_level
      .checked_add(1)
      .filter(|level| *level <= 15)
      .ok_or_else(|| amplification_error("index_cow_directory_depth", "recursive directory split would exceed level 15"))?;
    root_entries = encode_directory_entry_groups(
      request,
      root_level,
      &graph.owner_id,
      graph.role,
      &root_entries,
      &mut artifacts,
      &mut retained_artifact_bytes,
    )?;
  }
  if root_entries.is_empty() {
    return Ok(ArtifactDirectoryMutationPlanV1 {
      source_root_key: graph.source_root_key,
      root_key: None,
      root_level: 0,
      live_count: 0,
      tombstone_count: 0,
      page_count: 0,
      logical_bytes: 0,
      minimum_page_id: 0,
      maximum_page_id: 0,
      artifacts,
    });
  }
  let root = root_entries
    .pop()
    .ok_or_else(|| closure_error("index_cow_directory_root_empty", "directory copy-on-write produced no root summary"))?;
  Ok(ArtifactDirectoryMutationPlanV1 {
    source_root_key: graph.source_root_key,
    root_key: Some(root.child_hash),
    root_level,
    live_count: root.live_count,
    tombstone_count: root.tombstone_count,
    page_count: root.page_count,
    logical_bytes: root.logical_bytes,
    minimum_page_id: root.minimum_page_id,
    maximum_page_id: root.maximum_page_id,
    artifacts,
  })
}

fn validate_page_replacement_outputs(
  request: &ArtifactDirectoryMutationRequestV1<'_>,
) -> FormatResult<BTreeMap<Vec<u8>, Vec<OwnedDirectoryEntryV1>>> {
  let mut replacements = BTreeMap::new();
  for replacement in &request.page_plan.replacements {
    if replacement.source_key.len() != request.hash_algorithm.hash_length() || replacement.source_key.iter().all(|byte| *byte == 0) {
      return Err(identity_error("index_cow_directory_source_key", "page replacement source key has the wrong width or is all zero"));
    }
    if replacement.source_generation >= request.generation {
      return Err(identity_error(
        "index_cow_directory_source_generation",
        "page replacement source generation is not older than the directory generation",
      ));
    }
    if replacement.source_retired != replacement.artifacts.is_empty() {
      return Err(closure_error(
        "index_cow_directory_retirement_output",
        "a retired source page must have no output and a retained source page must have output",
      ));
    }
    if replacement.source_role.uses_page_id()
      && replacement.source_retired
      && !request.page_plan.retired_page_ids.contains(&replacement.source_page_id)
    {
      return Err(closure_error(
        "index_cow_directory_retirement_id",
        "a retired source page ID is absent from the page plan retirement set",
      ));
    }
    let mut entries: Vec<OwnedDirectoryEntryV1> = Vec::new();
    if let Err(error) = entries.try_reserve_exact(replacement.artifacts.len()) {
      return Err(amplification_error("index_cow_directory_page_summaries", format!("page-summary reservation failed: {error}")));
    }
    for artifact in &replacement.artifacts {
      let page = decode_ordered_page(&artifact.value, request.hash_algorithm)?;
      if page.key != artifact.key
        || page.role != replacement.source_role
        || page.owner_id != replacement.source_owner_id
        || page.generation != request.generation
      {
        return Err(closure_error(
          "index_cow_directory_page_output",
          "replacement page key, owner, role, or generation disagrees with its source",
        ));
      }
      if let Some(previous) = entries.last() {
        if compare_order_keys(request.hash_algorithm, page.role, &previous.upper_fence, page.lower_fence)? != Ordering::Less {
          return Err(closure_error("index_cow_directory_page_order", "replacement pages overlap or are not strictly ordered"));
        }
      }
      entries.push(OwnedDirectoryEntryV1::from_page(&page)?);
    }
    if replacements.insert(replacement.source_key.clone(), entries).is_some() {
      return Err(closure_error("index_cow_directory_duplicate_source", "page plan contains a duplicate source key"));
    }
  }
  Ok(replacements)
}

fn validate_directory_path_graph(
  request: &ArtifactDirectoryMutationRequestV1<'_>,
  replacement_entries: &BTreeMap<Vec<u8>, Vec<OwnedDirectoryEntryV1>>,
) -> FormatResult<DirectoryPathGraphV1> {
  let first_replacement = request
    .page_plan
    .replacements
    .first()
    .ok_or_else(|| closure_error("index_cow_directory_no_replacements", "page plan has no replacement"))?;
  let mut graph = DirectoryPathGraphV1 {
    source_root_key: Vec::new(),
    root_level: 0,
    role: first_replacement.source_role,
    owner_id: first_replacement.source_owner_id.clone(),
    nodes: BTreeMap::new(),
    parent_by_child: BTreeMap::new(),
    leaf_by_page: BTreeMap::new(),
  };
  let replacements_by_key =
    request.page_plan.replacements.iter().map(|replacement| (replacement.source_key.as_slice(), replacement)).collect::<BTreeMap<_, _>>();
  let mut seen_paths = BTreeMap::new();

  for (path_index, path) in request.paths.iter().enumerate() {
    let replacement = replacements_by_key
      .get(path.source_page_key)
      .ok_or_else(|| closure_error("index_cow_directory_unknown_path", "directory path names a page outside the replacement plan"))?;
    if seen_paths.insert(path.source_page_key.to_vec(), ()).is_some() {
      return Err(closure_error("index_cow_directory_duplicate_path", "one source page has more than one directory path"));
    }
    if path.directories.is_empty() || path.directories.len() > 16 {
      return Err(amplification_error(
        "index_cow_directory_path_depth",
        format!("directory path depth {} is outside 1..=16", path.directories.len()),
      ));
    }

    for (directory_index, bytes) in path.directories.iter().enumerate() {
      let directory = decode_artifact_directory(bytes, request.hash_algorithm)?;
      let expected_level = u16::try_from(path.directories.len() - directory_index - 1)
        .map_err(|source| amplification_error("index_cow_directory_path_depth", format!("path depth does not fit u16: {source}")))?;
      if directory.level != expected_level
        || directory.role != replacement.source_role
        || directory.owner_id != replacement.source_owner_id
        || directory.generation >= request.generation
      {
        return Err(closure_error("index_cow_directory_path_identity", "directory path level, owner, role, or generation is inconsistent"));
      }
      if directory_index == 0 {
        if graph.source_root_key.is_empty() {
          graph.source_root_key = directory.key.clone();
          graph.root_level = directory.level;
        } else if graph.source_root_key != directory.key || graph.root_level != directory.level {
          return Err(closure_error("index_cow_directory_root_mismatch", "affected paths do not share one exact source root"));
        }
      }
      if let Some(existing) = graph.nodes.get(directory.key.as_slice()) {
        let existing_bytes = request.paths[existing.path_index].directories[existing.directory_index];
        if existing_bytes != *bytes {
          return Err(closure_error("index_cow_directory_hash_collision", "one directory key names different encoded bytes"));
        }
      } else {
        graph.nodes.insert(directory.key.clone(), DirectoryNodeLocationV1 { path_index, directory_index });
      }
    }

    for directory_index in 0..path.directories.len() - 1 {
      let parent = decode_artifact_directory(path.directories[directory_index], request.hash_algorithm)?;
      let child = decode_artifact_directory(path.directories[directory_index + 1], request.hash_algorithm)?;
      let matching = find_unique_directory_entry(&parent, &child.key)?;
      if !directory_entry_matches_directory(matching, &child) {
        return Err(closure_error(
          "index_cow_directory_parent_child",
          "parent descriptor does not exactly summarize the named child directory",
        ));
      }
      if let Some(existing_parent) = graph.parent_by_child.insert(child.key.clone(), parent.key.clone()) {
        if existing_parent != parent.key {
          return Err(closure_error("index_cow_directory_multiple_parents", "one affected directory has multiple parents"));
        }
      }
    }

    let leaf = decode_artifact_directory(path.directories[path.directories.len() - 1], request.hash_algorithm)?;
    let matching = find_unique_directory_entry(&leaf, &replacement.source_key)?;
    if !directory_entry_matches_page_source(matching, replacement) {
      return Err(closure_error("index_cow_directory_leaf_page", "leaf descriptor does not exactly summarize the replaced source page"));
    }
    if !replacement_entries.contains_key(path.source_page_key) {
      return Err(closure_error("index_cow_directory_output_missing", "validated path has no replacement output"));
    }
    graph.leaf_by_page.insert(replacement.source_key.clone(), leaf.key.clone());
  }

  if seen_paths.len() != request.page_plan.replacements.len() {
    return Err(closure_error("index_cow_directory_path_missing", "not every page replacement has one directory path"));
  }
  Ok(graph)
}

fn find_unique_directory_entry<'node, 'data>(
  directory: &'node ArtifactDirectoryNodeV1<'data>,
  child_hash: &[u8],
) -> FormatResult<&'node ArtifactDirectoryEntryV1<'data>> {
  let mut matching = None;
  for entry in &directory.entries {
    if entry.child_hash != child_hash {
      continue;
    }
    if matching.replace(entry).is_some() {
      return Err(closure_error(
        "index_cow_directory_duplicate_child_hash",
        "directory contains more than one descriptor for an affected child hash",
      ));
    }
  }
  matching.ok_or_else(|| closure_error("index_cow_directory_child_hash_missing", "directory does not contain the affected child hash"))
}

fn directory_entry_matches_directory(entry: &ArtifactDirectoryEntryV1<'_>, child: &ArtifactDirectoryNodeV1<'_>) -> bool {
  entry.lower_fence == child.lower_fence
    && entry.upper_fence == child.upper_fence
    && entry.child_generation == child.generation
    && entry.live_count == child.live_count
    && entry.tombstone_count == child.tombstone_count
    && entry.page_count == child.page_count
    && entry.logical_bytes == child.logical_bytes
    && entry.minimum_page_id == child.minimum_page_id
    && entry.maximum_page_id == child.maximum_page_id
}

fn directory_entry_matches_page_source(entry: &ArtifactDirectoryEntryV1<'_>, source: &OrderedPageReplacementV1) -> bool {
  entry.lower_fence == source.source_lower_fence
    && entry.upper_fence == source.source_upper_fence
    && entry.child_generation == source.source_generation
    && entry.live_count == source.source_live_count
    && entry.tombstone_count == source.source_tombstone_count
    && entry.page_count == 1
    && entry.logical_bytes == source.source_logical_bytes
    && entry.minimum_page_id == source.source_page_id
    && entry.maximum_page_id == source.source_page_id
}

fn rewrite_directory_entries(
  source: &ArtifactDirectoryNodeV1<'_>,
  mut replacements: ChildReplacementMapV1,
  maximum_workspace_bytes: usize,
) -> FormatResult<Vec<OwnedDirectoryEntryV1>> {
  let additional_entries = replacements.values().try_fold(0usize, |total, entries| {
    total
      .checked_add(entries.len().saturating_sub(1))
      .ok_or_else(|| arithmetic_error("index_cow_directory_entry_count", "replacement entry count overflowed"))
  })?;
  let capacity = source
    .entries
    .len()
    .checked_add(additional_entries)
    .ok_or_else(|| arithmetic_error("index_cow_directory_entry_count", "rewritten entry count overflowed"))?;
  let mut rewritten = Vec::new();
  if let Err(error) = rewritten.try_reserve_exact(capacity) {
    return Err(amplification_error(
      "index_cow_directory_entry_workspace",
      format!("rewritten directory-entry reservation failed: {error}"),
    ));
  }
  let mut workspace_bytes = rewritten
    .capacity()
    .checked_mul(size_of::<OwnedDirectoryEntryV1>())
    .ok_or_else(|| arithmetic_error("index_cow_directory_workspace", "directory-entry metadata bytes overflowed"))?;
  for entry in &source.entries {
    if let Some(replacement_entries) = replacements.remove(entry.child_hash) {
      for replacement in replacement_entries {
        workspace_bytes = checked_add_owned_entry_workspace(workspace_bytes, &replacement)?;
        rewritten.push(replacement);
      }
    } else {
      let retained = OwnedDirectoryEntryV1::from_existing(entry)?;
      workspace_bytes = checked_add_owned_entry_workspace(workspace_bytes, &retained)?;
      rewritten.push(retained);
    }
    if workspace_bytes > maximum_workspace_bytes {
      return Err(amplification_error(
        "index_cow_directory_workspace_exceeded",
        format!("directory workspace exceeds the {maximum_workspace_bytes}-byte operation cap"),
      ));
    }
  }
  if !replacements.is_empty() {
    return Err(closure_error("index_cow_directory_child_missing", "directory does not contain every child selected for replacement"));
  }
  Ok(rewritten)
}

fn checked_add_owned_entry_workspace(total: usize, entry: &OwnedDirectoryEntryV1) -> FormatResult<usize> {
  total
    .checked_add(entry.lower_fence.len())
    .and_then(|bytes| bytes.checked_add(entry.upper_fence.len()))
    .and_then(|bytes| bytes.checked_add(entry.child_hash.len()))
    .ok_or_else(|| arithmetic_error("index_cow_directory_workspace", "directory-entry workspace bytes overflowed"))
}

#[allow(clippy::too_many_arguments)]
fn encode_directory_entry_groups(
  request: &ArtifactDirectoryMutationRequestV1<'_>,
  level: u16,
  owner_id: &[u8],
  role: OrderedIndexRoleV1,
  entries: &[OwnedDirectoryEntryV1],
  artifacts: &mut Vec<EncodedImmutableIndexArtifactV1>,
  retained_artifact_bytes: &mut usize,
) -> FormatResult<Vec<OwnedDirectoryEntryV1>> {
  if entries.is_empty() {
    return Ok(Vec::new());
  }
  let groups = partition_directory_entries(request.hash_algorithm, level, entries, request.layout.target_bytes)?;
  let mut summaries = Vec::new();
  if let Err(error) = summaries.try_reserve_exact(groups.len()) {
    return Err(amplification_error("index_cow_directory_summary_workspace", format!("directory-summary reservation failed: {error}")));
  }
  for group in groups {
    let group_entries = &entries[group];
    let mut write_entries = Vec::new();
    if let Err(error) = write_entries.try_reserve_exact(group_entries.len()) {
      return Err(amplification_error("index_cow_directory_write_workspace", format!("directory write-entry reservation failed: {error}")));
    }
    write_entries.extend(group_entries.iter().map(OwnedDirectoryEntryV1::as_write));
    let artifact = encode_artifact_directory(&ArtifactDirectoryWriteV1 {
      hash_algorithm: request.hash_algorithm,
      role,
      owner_id,
      generation: request.generation,
      level,
      entries: &write_entries,
    })?;
    let directory = decode_artifact_directory(&artifact.value, request.hash_algorithm)?;
    let summary = OwnedDirectoryEntryV1::from_directory(&directory)?;
    append_directory_artifact(artifact, artifacts, retained_artifact_bytes, request.layout.maximum_workspace_bytes)?;
    summaries.push(summary);
  }
  Ok(summaries)
}

fn partition_directory_entries(
  hash_algorithm: HashAlgorithm,
  level: u16,
  entries: &[OwnedDirectoryEntryV1],
  target_bytes: usize,
) -> FormatResult<Vec<Range<usize>>> {
  if entries.is_empty() {
    return Err(closure_error("index_cow_directory_empty", "rewritten directory has no entries"));
  }
  let unsplit_length = checked_directory_range_representable_length(hash_algorithm, level, entries)?;
  if unsplit_length <= target_bytes || entries.len() == 1 {
    return Ok(vec![0..entries.len()]);
  }
  let mut ranges = Vec::new();
  let mut start = 0usize;
  while start < entries.len() {
    let mut end = start + 1;
    while end < entries.len() {
      let candidate_end = end + 1;
      let candidate_length = checked_directory_range_representable_length(hash_algorithm, level, &entries[start..candidate_end])?;
      if candidate_length > target_bytes {
        break;
      }
      end = candidate_end;
    }
    ranges.push(start..end);
    start = end;
  }
  Ok(ranges)
}

fn checked_directory_range_representable_length(
  hash_algorithm: HashAlgorithm,
  level: u16,
  entries: &[OwnedDirectoryEntryV1],
) -> FormatResult<usize> {
  let first = entries.first().ok_or_else(|| closure_error("index_cow_directory_empty", "directory partition is empty"))?;
  let last = entries.last().ok_or_else(|| closure_error("index_cow_directory_empty", "directory partition is empty"))?;
  let fixed = (if level == 0 { 72usize } else { 88usize })
    .checked_add(hash_algorithm.hash_length())
    .ok_or_else(|| arithmetic_error("index_cow_directory_length", "directory descriptor fixed length overflowed"))?;
  let entries_length = entries.iter().try_fold(0usize, |total, entry| {
    total
      .checked_add(fixed)
      .and_then(|length| length.checked_add(entry.lower_fence.len()))
      .and_then(|length| length.checked_add(entry.upper_fence.len()))
      .ok_or_else(|| arithmetic_error("index_cow_directory_length", "directory descriptor bytes overflowed"))
  })?;
  let body_length = 80usize
    .checked_add(first.lower_fence.len())
    .and_then(|length| length.checked_add(last.upper_fence.len()))
    .and_then(|length| length.checked_add(entries_length))
    .ok_or_else(|| arithmetic_error("index_cow_directory_length", "directory body length overflowed"))?;
  let identity_length = hash_algorithm
    .hash_length()
    .checked_add(2)
    .ok_or_else(|| arithmetic_error("index_cow_directory_length", "directory identity length overflowed"))?;
  checked_immutable_index_artifact_representable_length(identity_length, body_length)
}

fn append_directory_artifact(
  artifact: EncodedImmutableIndexArtifactV1,
  artifacts: &mut Vec<EncodedImmutableIndexArtifactV1>,
  retained_artifact_bytes: &mut usize,
  maximum_workspace_bytes: usize,
) -> FormatResult<()> {
  let artifact_bytes = artifact
    .key
    .len()
    .checked_add(artifact.value.len())
    .and_then(|bytes| bytes.checked_add(size_of::<EncodedImmutableIndexArtifactV1>()))
    .ok_or_else(|| arithmetic_error("index_cow_directory_output", "retained directory artifact bytes overflowed"))?;
  let next = retained_artifact_bytes
    .checked_add(artifact_bytes)
    .ok_or_else(|| arithmetic_error("index_cow_directory_output", "retained directory artifact bytes overflowed"))?;
  let maximum_retained_output_bytes = maximum_workspace_bytes / 2;
  if next > maximum_retained_output_bytes {
    return Err(amplification_error(
      "index_cow_directory_output_exceeded",
      format!("{next} retained directory bytes exceed the {maximum_retained_output_bytes}-byte output half of the operation cap"),
    ));
  }
  if artifacts.len() == artifacts.capacity() {
    if let Err(error) = artifacts.try_reserve(1) {
      return Err(amplification_error(
        "index_cow_directory_artifact_workspace",
        format!("directory artifact-list reservation failed: {error}"),
      ));
    }
  }
  *retained_artifact_bytes = next;
  artifacts.push(artifact);
  Ok(())
}

fn validate_directory_layout(layout: IndexDirectoryLayoutV1) -> FormatResult<()> {
  if layout.target_bytes <= 128
    || layout.target_bytes > layout.hard_artifact_bytes
    || layout.hard_artifact_bytes != INDEX_ARTIFACT_HARD_CAP_BYTES_V1
    || layout.maximum_workspace_bytes < INDEX_DIRECTORY_COPY_ON_WRITE_WORKSPACE_BYTES_V1
  {
    return Err(amplification_error("index_cow_directory_layout", "directory target, hard cap, or workspace bound is invalid"));
  }
  Ok(())
}

#[derive(Debug)]
struct PageIdentityAllocatorV1 {
  next_page_id: u64,
  allocated_page_ids: Vec<u64>,
}

impl PageIdentityAllocatorV1 {
  fn new(next_page_id: u64) -> Self {
    Self { next_page_id, allocated_page_ids: Vec::new() }
  }

  fn allocate(&mut self) -> FormatResult<u64> {
    let allocated = self.next_page_id;
    let next = allocated
      .checked_add(1)
      .ok_or_else(|| arithmetic_error("index_cow_page_id_exhausted", "page ID high-water mark cannot advance without wrapping"))?;
    if allocated == 0 {
      return Err(identity_error("index_cow_page_id_zero", "page ID high-water mark is zero"));
    }
    self.next_page_id = next;
    self.allocated_page_ids.push(allocated);
    Ok(allocated)
  }
}

fn validate_layout(layout: IndexPageLayoutV1) -> FormatResult<()> {
  if layout.merge_below_bytes == 0
    || layout.merge_below_bytes >= layout.target_bytes
    || layout.target_bytes >= layout.split_above_bytes
    || layout.split_above_bytes > layout.hard_artifact_bytes
    || layout.hard_artifact_bytes != INDEX_ARTIFACT_HARD_CAP_BYTES_V1
    || layout.maximum_workspace_bytes < 2 * layout.hard_artifact_bytes
  {
    return Err(amplification_error("index_cow_layout", "page layout thresholds or workspace bound are invalid"));
  }
  Ok(())
}

fn validate_compaction_window(
  request: &OrderedPageCompactionWindowRequestV1<'_>,
  sources: &[OrderedPageV1<'_>],
  previous: Option<&OrderedPageV1<'_>>,
  next: Option<&OrderedPageV1<'_>>,
) -> FormatResult<()> {
  let first = sources.first().ok_or_else(|| closure_error("index_cow_compaction_window", "compaction window is empty"))?;
  for source in sources {
    if source.role != first.role || source.owner_id != first.owner_id {
      return Err(closure_error("index_cow_compaction_identity", "compaction source pages disagree on ordered role or owner identity"));
    }
    if request.generation <= source.generation {
      return Err(identity_error(
        "index_cow_compaction_generation",
        "compaction generation must be strictly newer than every source page birth generation",
      ));
    }
  }
  if sources.len() == 2 {
    if first.role.uses_page_id() && sources[0].page_id == sources[1].page_id {
      return Err(closure_error("index_cow_compaction_duplicate_page_id", "two compaction source pages reuse one stable page ID"));
    }
    if compare_order_keys(request.hash_algorithm, first.role, sources[0].upper_fence, sources[1].lower_fence)? != Ordering::Less {
      return Err(closure_error("index_cow_compaction_order", "compaction source pages overlap or are not strictly ordered"));
    }
    if first.role == OrderedIndexRoleV1::Posting {
      validate_posting_page_link(&sources[0], &sources[1], request.hash_algorithm)?;
    }
  }

  if first.role != OrderedIndexRoleV1::Posting && (previous.is_some() || next.is_some()) {
    return Err(closure_error("index_cow_compaction_nonposting_neighbor", "non-posting compaction cannot carry posting neighbors"));
  }
  if let Some(previous) = previous {
    validate_posting_page_link(previous, first, request.hash_algorithm)?;
    validate_compaction_neighbor_identity(request, first, previous)?;
  }
  if let Some(next) = next {
    let last = sources.last().ok_or_else(|| closure_error("index_cow_compaction_window", "compaction window has no last page"))?;
    validate_posting_page_link(last, next, request.hash_algorithm)?;
    validate_compaction_neighbor_identity(request, first, next)?;
  }

  if first.role.uses_page_id() {
    let mut observed_maximum_page_id = 0u64;
    for page in sources.iter().chain(previous).chain(next) {
      observed_maximum_page_id = observed_maximum_page_id.max(page.page_id).max(page.previous_page_id).max(page.next_page_id);
    }
    if request.next_page_id == 0 || request.next_page_id <= observed_maximum_page_id {
      return Err(identity_error(
        "index_cow_compaction_page_id_high_water",
        "next page ID must exceed every source, neighbor, and outward linked page ID",
      ));
    }
  } else if request.next_page_id != 0 {
    return Err(identity_error("index_cow_compaction_scope_page_id", "scope-page compaction must not carry a page ID high-water mark"));
  }
  Ok(())
}

fn validate_compaction_neighbor_identity(
  request: &OrderedPageCompactionWindowRequestV1<'_>,
  source: &OrderedPageV1<'_>,
  neighbor: &OrderedPageV1<'_>,
) -> FormatResult<()> {
  if neighbor.owner_id != source.owner_id || neighbor.role != source.role || request.generation <= neighbor.generation {
    return Err(identity_error(
      "index_cow_compaction_neighbor_identity",
      "compaction neighbor owner, role, or birth generation is inconsistent",
    ));
  }
  Ok(())
}

fn validate_tombstone_drop_proof(
  request: &OrderedPageCompactionWindowRequestV1<'_>,
  sources: &[OrderedPageV1<'_>],
  proof: &TombstoneDropProofV1<'_>,
) -> FormatResult<()> {
  let first = sources.first().ok_or_else(|| closure_error("index_cow_tombstone_proof_pages", "proof has no source page"))?;
  if proof.owner_id != first.owner_id {
    return Err(identity_error("index_cow_tombstone_proof_owner", "tombstone-drop proof owner disagrees with the compaction window"));
  }
  if proof.source_page_keys.len() != sources.len()
    || proof.source_page_keys.iter().zip(sources).any(|(proof_key, source)| *proof_key != source.key)
  {
    return Err(closure_error(
      "index_cow_tombstone_proof_pages",
      "tombstone-drop proof does not bind the exact ordered immutable source-page set",
    ));
  }
  if proof.coverage_epoch_id == 0
    || proof.covered_through_sequence == 0
    || proof.journal_contiguous_through_sequence < proof.covered_through_sequence
  {
    return Err(closure_error(
      "index_cow_tombstone_proof_coverage",
      "tombstone-drop proof lacks a nonzero epoch or contiguous journal coverage",
    ));
  }
  let newest_source_generation = sources
    .iter()
    .map(|source| source.generation)
    .max()
    .ok_or_else(|| closure_error("index_cow_tombstone_proof_pages", "proof has no source-page generation"))?;
  if proof.pin_safe_through_generation < newest_source_generation || proof.pin_safe_through_generation >= request.generation {
    return Err(closure_error(
      "index_cow_tombstone_proof_pins",
      "tombstone-drop proof does not cover the source generations below the new generation",
    ));
  }
  Ok(())
}

fn collect_compaction_records(sources: &[OrderedPageV1<'_>], maximum_workspace_bytes: usize) -> FormatResult<Vec<OwnedOrderedRecordV1>> {
  let record_count = sources.iter().try_fold(0usize, |count, source| {
    count.checked_add(source.records.len()).ok_or_else(|| arithmetic_error("index_cow_compaction_record_count", "record count overflowed"))
  })?;
  let mut records = Vec::new();
  if let Err(error) = records.try_reserve_exact(record_count) {
    return Err(amplification_error("index_cow_compaction_records", format!("record reservation failed: {error}")));
  }
  let mut accounted_bytes = records
    .capacity()
    .checked_mul(size_of::<OwnedOrderedRecordV1>())
    .ok_or_else(|| arithmetic_error("index_cow_workspace_overflow", "compaction record metadata bytes overflowed"))?;
  for source in sources {
    for record in source.records.iter() {
      let record = record?;
      let order_key = ordered_record_order_key(&record)?;
      accounted_bytes = accounted_bytes
        .checked_add(record.encoded.len())
        .and_then(|total| total.checked_add(order_key.len()))
        .ok_or_else(|| arithmetic_error("index_cow_workspace_overflow", "compaction record bytes overflowed"))?;
      if accounted_bytes > maximum_workspace_bytes {
        return Err(amplification_error(
          "index_cow_workspace_exceeded",
          format!("record workspace exceeds the {maximum_workspace_bytes}-byte operation cap"),
        ));
      }
      records.push(OwnedOrderedRecordV1 {
        encoded: copy_fallible_bytes(record.encoded, "compaction record")?,
        order_key,
        tombstone: record.tombstone,
      });
    }
  }
  Ok(records)
}

fn preflight_compaction_workspace(
  request: &OrderedPageCompactionWindowRequestV1<'_>,
  sources: &[OrderedPageV1<'_>],
  records: &[OwnedOrderedRecordV1],
  record_workspace_bytes: usize,
  rewritten_previous: Option<&OrderedPageV1<'_>>,
  rewritten_next: Option<&OrderedPageV1<'_>>,
) -> FormatResult<()> {
  let mut output_bytes = if records.is_empty() { 0 } else { checked_page_range_length(request.hash_algorithm, sources[0].role, records)? };
  for (neighbor, encoded) in [(rewritten_previous, request.previous_posting_page), (rewritten_next, request.next_posting_page)] {
    let Some(neighbor) = neighbor else {
      continue;
    };
    let encoded = encoded.ok_or_else(|| closure_error("index_cow_compaction_neighbor_missing", "validated neighbor bytes disappeared"))?;
    output_bytes = output_bytes
      .checked_add(encoded.len())
      .and_then(|bytes| {
        neighbor
          .records
          .len()
          .checked_mul(size_of::<super::index_page::OrderedRecordV1<'_>>() + size_of::<&[u8]>())
          .and_then(|metadata| bytes.checked_add(metadata))
      })
      .ok_or_else(|| arithmetic_error("index_cow_compaction_output", "compaction neighbor output bytes overflowed"))?;
  }
  let replacement_count = sources.len() + usize::from(rewritten_previous.is_some()) + usize::from(rewritten_next.is_some());
  let artifact_count = usize::from(!records.is_empty()) + usize::from(rewritten_previous.is_some()) + usize::from(rewritten_next.is_some());
  let retained_key_bytes = artifact_count
    .checked_mul(request.hash_algorithm.hash_length())
    .ok_or_else(|| arithmetic_error("index_cow_compaction_output", "compaction retained artifact-key bytes overflowed"))?;
  output_bytes = output_bytes
    .checked_add(retained_key_bytes)
    .and_then(|bytes| bytes.checked_add(artifact_count.checked_mul(size_of::<EncodedImmutableIndexArtifactV1>())?))
    .ok_or_else(|| arithmetic_error("index_cow_compaction_output", "compaction output bytes overflowed"))?;
  let retained_summary_bytes = sources.iter().chain(rewritten_previous).chain(rewritten_next).try_fold(0usize, |bytes, page| {
    bytes
      .checked_add(page.key.len())
      .and_then(|bytes| bytes.checked_add(page.owner_id.len()))
      .and_then(|bytes| bytes.checked_add(page.lower_fence.len()))
      .and_then(|bytes| bytes.checked_add(page.upper_fence.len()))
      .ok_or_else(|| arithmetic_error("index_cow_compaction_output", "compaction retained source-summary bytes overflowed"))
  })?;
  output_bytes = output_bytes
    .checked_add(retained_summary_bytes)
    .ok_or_else(|| arithmetic_error("index_cow_compaction_output", "compaction retained output bytes overflowed"))?;
  let planning_bytes = sources
    .len()
    .checked_mul(size_of::<OrderedPageV1<'_>>())
    .and_then(|bytes| bytes.checked_add(replacement_count.checked_mul(size_of::<OrderedPageReplacementV1>())?))
    .and_then(|bytes| bytes.checked_add(sources.len().checked_mul(size_of::<u64>())?))
    .and_then(|bytes| bytes.checked_add(records.len().checked_mul(size_of::<&[u8]>())?))
    .ok_or_else(|| arithmetic_error("index_cow_compaction_output", "compaction planning bytes overflowed"))?;
  let peak_bytes = record_workspace_bytes
    .checked_add(output_bytes)
    .and_then(|bytes| bytes.checked_add(planning_bytes))
    .ok_or_else(|| arithmetic_error("index_cow_compaction_output", "compaction peak bytes overflowed"))?;
  if output_bytes > request.layout.maximum_workspace_bytes || peak_bytes > request.layout.maximum_workspace_bytes {
    return Err(amplification_error(
      "index_cow_compaction_workspace_exceeded",
      format!("{peak_bytes} peak bytes exceed the {}-byte operation cap", request.layout.maximum_workspace_bytes),
    ));
  }
  Ok(())
}

fn validate_mutation_identity(request: &OrderedPageMutationRequestV1<'_>, source: &OrderedPageV1<'_>) -> FormatResult<()> {
  if request.generation <= source.generation {
    return Err(identity_error(
      "index_cow_generation",
      "copy-on-write generation must be strictly newer than the source page birth generation",
    ));
  }
  if source.role.uses_page_id() {
    let observed_maximum_page_id = source.page_id.max(source.previous_page_id).max(source.next_page_id);
    if request.next_page_id == 0 || request.next_page_id <= observed_maximum_page_id {
      return Err(identity_error("index_cow_page_id_high_water", "next page ID must be above every page ID linked from the source"));
    }
  } else if request.next_page_id != 0 {
    return Err(identity_error("index_cow_scope_page_id", "scope-page mutation must not carry a page ID high-water mark"));
  }
  Ok(())
}

fn collect_owned_records(source: &OrderedPageV1<'_>, maximum_workspace_bytes: usize) -> FormatResult<Vec<OwnedOrderedRecordV1>> {
  let requested_capacity =
    source.records.len().checked_add(1).ok_or_else(|| arithmetic_error("index_cow_record_capacity", "record capacity overflowed"))?;
  let mut records = Vec::new();
  if let Err(error) = records.try_reserve_exact(requested_capacity) {
    return Err(amplification_error("index_cow_record_capacity", format!("record workspace reservation failed: {error}")));
  }
  let mut accounted_bytes = records
    .capacity()
    .checked_mul(size_of::<OwnedOrderedRecordV1>())
    .ok_or_else(|| arithmetic_error("index_cow_workspace_overflow", "record metadata workspace byte count overflowed"))?;
  for record in source.records.iter() {
    let record = record?;
    let order_key = ordered_record_order_key(&record)?;
    accounted_bytes = accounted_bytes
      .checked_add(record.encoded.len())
      .and_then(|total| total.checked_add(order_key.len()))
      .ok_or_else(|| arithmetic_error("index_cow_workspace_overflow", "record workspace byte count overflowed"))?;
    if accounted_bytes > maximum_workspace_bytes {
      return Err(amplification_error(
        "index_cow_workspace_exceeded",
        format!("record workspace exceeds the {maximum_workspace_bytes}-byte operation cap"),
      ));
    }
    records.push(OwnedOrderedRecordV1 { encoded: record.encoded.to_vec(), order_key, tombstone: record.tombstone });
  }
  Ok(records)
}

fn validate_workspace(records: &Vec<OwnedOrderedRecordV1>, maximum_workspace_bytes: usize) -> FormatResult<usize> {
  let accounted_bytes = checked_record_workspace(records)?;
  if accounted_bytes > maximum_workspace_bytes {
    return Err(amplification_error(
      "index_cow_workspace_exceeded",
      format!("record workspace exceeds the {maximum_workspace_bytes}-byte operation cap"),
    ));
  }
  Ok(accounted_bytes)
}

fn preflight_record_mutation_workspace(
  records: &Vec<OwnedOrderedRecordV1>,
  matching_index: Option<usize>,
  encoded_length: usize,
  order_key_length: usize,
  maximum_workspace_bytes: usize,
) -> FormatResult<()> {
  let current = checked_record_workspace(records)?;
  let removed = if let Some(index) = matching_index {
    records[index]
      .encoded
      .len()
      .checked_add(records[index].order_key.len())
      .ok_or_else(|| arithmetic_error("index_cow_workspace_overflow", "replaced record workspace byte count overflowed"))?
  } else {
    0
  };
  let projected = current
    .checked_sub(removed)
    .and_then(|bytes| bytes.checked_add(encoded_length))
    .and_then(|bytes| bytes.checked_add(order_key_length))
    .ok_or_else(|| arithmetic_error("index_cow_workspace_overflow", "projected record workspace byte count overflowed"))?;
  if projected > maximum_workspace_bytes {
    return Err(amplification_error(
      "index_cow_workspace_exceeded",
      format!("record workspace exceeds the {maximum_workspace_bytes}-byte operation cap"),
    ));
  }
  Ok(())
}

fn checked_record_workspace(records: &Vec<OwnedOrderedRecordV1>) -> FormatResult<usize> {
  records.iter().try_fold(
    records
      .capacity()
      .checked_mul(size_of::<OwnedOrderedRecordV1>())
      .ok_or_else(|| arithmetic_error("index_cow_workspace_overflow", "record metadata workspace byte count overflowed"))?,
    |total, record| {
      total
        .checked_add(record.encoded.len())
        .and_then(|value| value.checked_add(record.order_key.len()))
        .ok_or_else(|| arithmetic_error("index_cow_workspace_overflow", "record workspace byte count overflowed"))
    },
  )
}

fn preflight_output_workspace(
  request: &OrderedPageMutationRequestV1<'_>,
  source: &OrderedPageV1<'_>,
  records: &[OwnedOrderedRecordV1],
  groups: &[std::ops::Range<usize>],
  record_workspace_bytes: usize,
  relinked_next: Option<&OrderedPageV1<'_>>,
) -> FormatResult<()> {
  let mut output_bytes = 0usize;
  for range in groups {
    let page_bytes = checked_page_range_length(request.hash_algorithm, source.role, &records[range.clone()])?;
    output_bytes = output_bytes
      .checked_add(page_bytes)
      .ok_or_else(|| arithmetic_error("index_cow_output_overflow", "copy-on-write output byte count overflowed"))?;
  }
  if let Some(next) = relinked_next {
    let next_bytes =
      request.next_posting_page.ok_or_else(|| closure_error("index_cow_next_page_missing", "validated next posting page disappeared"))?;
    output_bytes = output_bytes
      .checked_add(next_bytes.len())
      .ok_or_else(|| arithmetic_error("index_cow_output_overflow", "copy-on-write output byte count overflowed"))?;
    output_bytes = output_bytes
      .checked_add(
        next
          .records
          .len()
          .checked_mul(size_of::<super::index_page::OrderedRecordV1<'_>>() + size_of::<&[u8]>())
          .ok_or_else(|| arithmetic_error("index_cow_output_overflow", "neighbor record metadata byte count overflowed"))?,
      )
      .ok_or_else(|| arithmetic_error("index_cow_output_overflow", "copy-on-write output byte count overflowed"))?;
  }
  let artifact_count = groups.len() + usize::from(relinked_next.is_some());
  let replacement_count = 1usize + usize::from(relinked_next.is_some());
  let retained_key_bytes = artifact_count
    .checked_add(replacement_count)
    .and_then(|count| count.checked_mul(request.hash_algorithm.hash_length()))
    .ok_or_else(|| arithmetic_error("index_cow_output_overflow", "copy-on-write retained key byte count overflowed"))?;
  output_bytes = output_bytes
    .checked_add(retained_key_bytes)
    .ok_or_else(|| arithmetic_error("index_cow_output_overflow", "copy-on-write output byte count overflowed"))?;
  if output_bytes > request.layout.maximum_workspace_bytes {
    return Err(amplification_error(
      "index_cow_output_exceeds_workspace",
      format!("{output_bytes} output bytes exceed the {}-byte operation cap", request.layout.maximum_workspace_bytes),
    ));
  }
  let planning_bytes = groups
    .len()
    .checked_mul(size_of::<std::ops::Range<usize>>() + size_of::<u64>() + size_of::<EncodedImmutableIndexArtifactV1>())
    .and_then(|bytes| records.len().checked_mul(size_of::<&[u8]>()).and_then(|record_bytes| bytes.checked_add(record_bytes)))
    .ok_or_else(|| arithmetic_error("index_cow_output_overflow", "copy-on-write planning metadata byte count overflowed"))?;
  let peak_bytes = record_workspace_bytes
    .checked_add(output_bytes)
    .and_then(|bytes| bytes.checked_add(planning_bytes))
    .ok_or_else(|| arithmetic_error("index_cow_output_overflow", "copy-on-write peak byte count overflowed"))?;
  if peak_bytes > request.layout.maximum_workspace_bytes {
    return Err(amplification_error(
      "index_cow_peak_workspace_exceeded",
      format!("{peak_bytes} peak bytes exceed the {}-byte operation cap", request.layout.maximum_workspace_bytes),
    ));
  }
  Ok(())
}

fn partition_records(
  request: &OrderedPageMutationRequestV1<'_>,
  source: &OrderedPageV1<'_>,
  records: &[OwnedOrderedRecordV1],
) -> FormatResult<Vec<std::ops::Range<usize>>> {
  let unsplit_length = checked_page_range_representable_length(request.hash_algorithm, source.role, records)?;
  if unsplit_length <= request.layout.split_above_bytes || records.len() == 1 {
    return Ok(vec![0..records.len()]);
  }

  let mut ranges = Vec::new();
  let mut start = 0usize;
  while start < records.len() {
    let mut end = start + 1;
    while end < records.len() {
      let candidate_end = end + 1;
      let candidate_length = checked_page_range_representable_length(request.hash_algorithm, source.role, &records[start..candidate_end])?;
      if candidate_length > request.layout.target_bytes {
        break;
      }
      end = candidate_end;
    }
    ranges.push(start..end);
    start = end;
  }
  Ok(ranges)
}

fn checked_page_range_length(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  records: &[OwnedOrderedRecordV1],
) -> FormatResult<usize> {
  let (identity_length, body_length) = checked_page_range_components(hash_algorithm, role, records)?;
  checked_immutable_index_artifact_encoded_length(role.child_kind(), identity_length, body_length)
}

fn checked_page_range_representable_length(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  records: &[OwnedOrderedRecordV1],
) -> FormatResult<usize> {
  let (identity_length, body_length) = checked_page_range_components(hash_algorithm, role, records)?;
  checked_immutable_index_artifact_representable_length(identity_length, body_length)
}

fn checked_page_range_components(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  records: &[OwnedOrderedRecordV1],
) -> FormatResult<(usize, usize)> {
  let first = records.first().ok_or_else(|| closure_error("index_cow_empty_partition", "page partition is empty"))?;
  let last = records.last().ok_or_else(|| closure_error("index_cow_empty_partition", "page partition is empty"))?;
  let record_bytes = records
    .iter()
    .try_fold(0usize, |total, record| total.checked_add(record.encoded.len()))
    .ok_or_else(|| arithmetic_error("index_cow_partition_length", "page partition record bytes overflowed"))?;
  let body_length = 96usize
    .checked_add(first.order_key.len())
    .and_then(|length| length.checked_add(last.order_key.len()))
    .and_then(|length| length.checked_add(record_bytes))
    .ok_or_else(|| arithmetic_error("index_cow_partition_length", "page partition body length overflowed"))?;
  let hash_width = hash_algorithm.hash_length();
  let identity_length = match role {
    OrderedIndexRoleV1::ScopeOrdinal | OrderedIndexRoleV1::ScopeReverse => hash_width
      .checked_add(1)
      .and_then(|length| length.checked_add(first.order_key.len()))
      .ok_or_else(|| arithmetic_error("index_cow_partition_identity", "scope-page identity length overflowed"))?,
    OrderedIndexRoleV1::Value | OrderedIndexRoleV1::Posting => hash_width + 8,
    OrderedIndexRoleV1::ValueDocumentState | OrderedIndexRoleV1::IndexDocumentState => hash_width + 16,
    OrderedIndexRoleV1::NvtTile => {
      return Err(FormatError::new(MalformedInputClass::UnknownTypeKindOrEnum, "index_cow_nvt_role", "NVT tiles are not ordered pages"));
    }
  };
  Ok((identity_length, body_length))
}

fn validate_relinked_next(
  request: &OrderedPageMutationRequestV1<'_>,
  source: &OrderedPageV1<'_>,
  next: &OrderedPageV1<'_>,
) -> FormatResult<()> {
  validate_posting_page_link(source, next, request.hash_algorithm)?;
  if request.generation <= next.generation {
    return Err(identity_error(
      "index_cow_neighbor_generation",
      "copy-on-write generation must be strictly newer than the next posting page birth generation",
    ));
  }
  let observed_maximum_page_id = next.page_id.max(next.previous_page_id).max(next.next_page_id);
  if request.next_page_id <= observed_maximum_page_id {
    return Err(identity_error(
      "index_cow_neighbor_page_id",
      "next page ID high-water mark does not exceed every page ID linked from the neighbor",
    ));
  }
  Ok(())
}

fn record_slices(records: &[OwnedOrderedRecordV1]) -> Vec<&[u8]> {
  records.iter().map(|record| record.encoded.as_slice()).collect()
}

fn copy_fallible_bytes(value: &[u8], label: &'static str) -> FormatResult<Vec<u8>> {
  let mut copied = Vec::new();
  if let Err(error) = copied.try_reserve_exact(value.len()) {
    return Err(amplification_error("index_cow_copy", format!("{label} reservation failed: {error}")));
  }
  copied.extend_from_slice(value);
  Ok(copied)
}

fn identity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}

fn arithmetic_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, code, context)
}

fn amplification_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::AllocationAmplification, code, context)
}
