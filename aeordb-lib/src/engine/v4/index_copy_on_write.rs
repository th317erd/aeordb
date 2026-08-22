use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::ops::Range;

use crate::engine::HashAlgorithm;

use super::index_artifact::{
  EncodedImmutableIndexArtifactV1, checked_immutable_index_artifact_encoded_length, checked_immutable_index_artifact_representable_length,
};
use super::hash::IncrementalDigestV1;
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
  RemoveExisting(&'a [u8]),
}

impl<'a> OrderedPageMutationKindV1<'a> {
  fn encoded_record(self) -> &'a [u8] {
    match self {
      Self::UpsertLive(record) | Self::TombstoneExisting(record) | Self::RemoveExisting(record) => record,
    }
  }

  fn expects_tombstone(self) -> bool {
    matches!(self, Self::TombstoneExisting(_))
  }

  fn removes_existing(self) -> bool {
    matches!(self, Self::RemoveExisting(_))
  }

  fn commitment_tag(self) -> u8 {
    match self {
      Self::UpsertLive(_) => 1,
      Self::TombstoneExisting(_) => 2,
      Self::RemoveExisting(_) => 3,
    }
  }
}

pub(crate) struct IndexMutationCommitmentV1 {
  digest: IncrementalDigestV1,
  count: u64,
}

impl IndexMutationCommitmentV1 {
  pub(crate) fn new(hash_algorithm: HashAlgorithm) -> Self {
    let mut digest = IncrementalDigestV1::new(hash_algorithm);
    digest.update(b"aeordb.index-cow-mutations.v1\0");
    Self { digest, count: 0 }
  }

  pub(crate) fn push(&mut self, mutation: OrderedPageMutationKindV1<'_>) -> FormatResult<()> {
    let encoded = mutation.encoded_record();
    let length = u64::try_from(encoded.len())
      .map_err(|source| amplification_error("index_cow_mutation_commitment", format!("mutation length is not representable: {source}")))?;
    self.digest.update(&[mutation.commitment_tag()]);
    self.digest.update(&length.to_le_bytes());
    self.digest.update(encoded);
    self.count =
      self.count.checked_add(1).ok_or_else(|| arithmetic_error("index_cow_mutation_commitment", "mutation commitment count overflowed"))?;
    Ok(())
  }

  pub(crate) fn finish(mut self) -> Vec<u8> {
    self.digest.update(&self.count.to_le_bytes());
    self.digest.finalize()
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
pub struct OrderedPageBatchMutationRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub source_page: &'a [u8],
  pub next_posting_page: Option<&'a [u8]>,
  pub generation: u64,
  pub next_page_id: u64,
  pub mutations: &'a [OrderedPageMutationKindV1<'a>],
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

#[derive(Debug, Clone, Copy)]
pub struct IndexCopyOnWriteClosureRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub generation: u64,
  pub initial_next_page_id: u64,
  pub applied_mutations: Option<&'a [OrderedPageMutationKindV1<'a>]>,
  pub source_pages: &'a [&'a [u8]],
  pub paths: &'a [ArtifactDirectoryPathV1<'a>],
  pub page_plan: &'a OrderedPageMutationPlanV1,
  pub directory_plan: &'a ArtifactDirectoryMutationPlanV1,
  pub page_layout: IndexPageLayoutV1,
  pub directory_layout: IndexDirectoryLayoutV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCopyOnWriteClosureSummaryV1 {
  pub owner_id: Vec<u8>,
  pub role: OrderedIndexRoleV1,
  pub generation: u64,
  pub source_root_key: Vec<u8>,
  pub root_key: Option<Vec<u8>>,
  pub live_count: u64,
  pub tombstone_count: u64,
  pub page_count: u64,
  pub logical_bytes: u64,
  pub minimum_page_id: u64,
  pub maximum_page_id: u64,
  pub initial_next_page_id: u64,
  pub next_page_id: u64,
  pub mutation_commitment: Option<Vec<u8>>,
  pub page_artifact_count: usize,
  pub directory_artifact_count: usize,
  pub source_page_bytes: usize,
  pub directory_path_bytes: usize,
  pub page_artifact_bytes: usize,
  pub directory_artifact_bytes: usize,
  pub retained_encoded_bytes: usize,
  _validated: IndexCopyOnWriteClosureSealV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndexCopyOnWriteClosureSealV1;

#[derive(Debug)]
struct OwnedOrderedRecordV1 {
  encoded: Vec<u8>,
  order_key: Vec<u8>,
  tombstone: bool,
}

#[derive(Debug)]
struct DecodedOrderedMutationV1<'a> {
  encoded: &'a [u8],
  order_key: Vec<u8>,
  tombstone: bool,
  remove_existing: bool,
}

pub fn mutate_ordered_page_v1(request: &OrderedPageMutationRequestV1<'_>) -> FormatResult<OrderedPageMutationPlanV1> {
  let mutations = [request.mutation];
  mutate_ordered_page_batch_v1(&OrderedPageBatchMutationRequestV1 {
    hash_algorithm: request.hash_algorithm,
    source_page: request.source_page,
    next_posting_page: request.next_posting_page,
    generation: request.generation,
    next_page_id: request.next_page_id,
    mutations: &mutations,
    layout: request.layout,
  })
}

pub fn mutate_ordered_page_batch_v1(request: &OrderedPageBatchMutationRequestV1<'_>) -> FormatResult<OrderedPageMutationPlanV1> {
  validate_layout(request.layout)?;
  if request.mutations.is_empty() {
    return Err(closure_error("index_cow_batch_empty", "ordered page mutation batch is empty"));
  }
  let source = decode_ordered_page(request.source_page, request.hash_algorithm)?;
  validate_mutation_identity(request, &source)?;

  let decoded_mutations = decode_ordered_mutation_batch(request, source.role)?;
  let source_records = collect_owned_records(&source, request.layout.maximum_workspace_bytes)?;
  let (records, changed) = merge_ordered_mutation_batch(request, source.role, source_records, decoded_mutations)?;
  if !changed {
    return Ok(OrderedPageMutationPlanV1 {
      replacements: Vec::new(),
      allocated_page_ids: Vec::new(),
      retired_page_ids: Vec::new(),
      next_page_id: request.next_page_id,
    });
  }
  if records.is_empty() {
    return Ok(OrderedPageMutationPlanV1 {
      replacements: vec![ordered_page_replacement_with_retirement(&source, Vec::new(), true)?],
      allocated_page_ids: Vec::new(),
      retired_page_ids: Vec::new(),
      next_page_id: request.next_page_id,
    });
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
  validate_directory_path_input_workspace(request.paths, request.layout.maximum_workspace_bytes)?;

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
        request.layout.target_bytes,
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
    let target_groups = partition_directory_entries(request.hash_algorithm, root_level, &root_entries, request.layout.target_bytes)?;
    let root_target_bytes = if target_groups.len() < root_entries.len() {
      request.layout.target_bytes
    } else {
      let hard_groups = partition_directory_entries(request.hash_algorithm, root_level, &root_entries, request.layout.hard_artifact_bytes)?;
      if hard_groups.len() >= root_entries.len() {
        return Err(amplification_error(
          "index_cow_directory_nonreducing_root",
          "directory descriptors cannot form a smaller hard-cap-bounded parent level",
        ));
      }
      request.layout.hard_artifact_bytes
    };
    root_entries = encode_directory_entry_groups(
      request,
      root_level,
      &graph.owner_id,
      graph.role,
      &root_entries,
      root_target_bytes,
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

pub fn validate_index_copy_on_write_closure_v1(
  request: &IndexCopyOnWriteClosureRequestV1<'_>,
) -> FormatResult<IndexCopyOnWriteClosureSummaryV1> {
  validate_layout(request.page_layout)?;
  validate_directory_layout(request.directory_layout)?;
  if request.generation == 0 {
    return Err(identity_error("index_cow_closure_generation", "copy-on-write closure generation is zero"));
  }
  if request.source_pages.is_empty() || request.source_pages.len() > INDEX_DIRECTORY_MAXIMUM_AFFECTED_PATHS_V1 {
    return Err(amplification_error(
      "index_cow_closure_source_count",
      format!(
        "{} source pages are outside the 1..={} closure bound",
        request.source_pages.len(),
        INDEX_DIRECTORY_MAXIMUM_AFFECTED_PATHS_V1
      ),
    ));
  }
  let directory_path_bytes = validate_directory_path_input_workspace(request.paths, request.directory_layout.maximum_workspace_bytes)?;
  let page_closure = validate_page_plan_closure(request)?;
  let directory_artifact_bytes = validate_directory_plan_closure(request, &page_closure)?;
  let retained_encoded_bytes = page_closure
    .source_page_bytes
    .checked_add(directory_path_bytes)
    .and_then(|bytes| bytes.checked_add(page_closure.page_artifact_bytes))
    .and_then(|bytes| bytes.checked_add(directory_artifact_bytes))
    .ok_or_else(|| arithmetic_error("index_cow_closure_retained_bytes", "retained encoded byte count overflowed"))?;
  if retained_encoded_bytes > request.directory_layout.maximum_workspace_bytes {
    return Err(amplification_error(
      "index_cow_closure_retained_workspace",
      format!(
        "{retained_encoded_bytes} retained encoded bytes exceed the {}-byte complete-operation cap",
        request.directory_layout.maximum_workspace_bytes
      ),
    ));
  }
  let mutation_commitment = validate_mutation_binding(request, &page_closure, retained_encoded_bytes)?;
  let root_key = request.directory_plan.root_key.as_deref().map(|key| copy_fallible_bytes(key, "closure root key")).transpose()?;

  Ok(IndexCopyOnWriteClosureSummaryV1 {
    owner_id: page_closure.owner_id,
    role: page_closure.role,
    generation: request.generation,
    source_root_key: copy_fallible_bytes(&request.directory_plan.source_root_key, "closure source root key")?,
    root_key,
    live_count: request.directory_plan.live_count,
    tombstone_count: request.directory_plan.tombstone_count,
    page_count: request.directory_plan.page_count,
    logical_bytes: request.directory_plan.logical_bytes,
    minimum_page_id: request.directory_plan.minimum_page_id,
    maximum_page_id: request.directory_plan.maximum_page_id,
    initial_next_page_id: request.initial_next_page_id,
    next_page_id: request.page_plan.next_page_id,
    mutation_commitment,
    page_artifact_count: page_closure.output_pages.len(),
    directory_artifact_count: request.directory_plan.artifacts.len(),
    source_page_bytes: page_closure.source_page_bytes,
    directory_path_bytes,
    page_artifact_bytes: page_closure.page_artifact_bytes,
    directory_artifact_bytes,
    retained_encoded_bytes,
    _validated: IndexCopyOnWriteClosureSealV1,
  })
}

#[derive(Debug)]
struct ValidatedPageClosureV1<'a> {
  role: OrderedIndexRoleV1,
  owner_id: Vec<u8>,
  source_keys: BTreeSet<Vec<u8>>,
  source_pages: Vec<OrderedPageV1<'a>>,
  output_pages: Vec<OrderedPageV1<'a>>,
  source_page_bytes: usize,
  page_artifact_bytes: usize,
}

#[derive(Debug)]
struct BoundMutationRecordV1<'a> {
  order_key: Vec<u8>,
  encoded: &'a [u8],
  tombstone: bool,
  mutation: Option<OrderedPageMutationKindV1<'a>>,
}

fn validate_mutation_binding(
  request: &IndexCopyOnWriteClosureRequestV1<'_>,
  page_closure: &ValidatedPageClosureV1<'_>,
  retained_encoded_bytes: usize,
) -> FormatResult<Option<Vec<u8>>> {
  let Some(mutations) = request.applied_mutations else {
    return Ok(None);
  };
  if mutations.is_empty() {
    return Err(closure_error("index_cow_mutation_binding_empty", "a mutation-bound closure has no mutations"));
  }
  let mut workspace_bytes = retained_encoded_bytes;
  let source = collect_binding_records(
    request.hash_algorithm,
    page_closure.role,
    &page_closure.source_pages,
    request.directory_layout.maximum_workspace_bytes,
    &mut workspace_bytes,
  )?;
  let output = collect_binding_records(
    request.hash_algorithm,
    page_closure.role,
    &page_closure.output_pages,
    request.directory_layout.maximum_workspace_bytes,
    &mut workspace_bytes,
  )?;
  let (mutations, commitment) = collect_binding_mutations(
    request.hash_algorithm,
    page_closure.role,
    mutations,
    request.directory_layout.maximum_workspace_bytes,
    &mut workspace_bytes,
  )?;
  validate_bound_record_stream(request.hash_algorithm, page_closure.role, &source, &mutations, &output)?;
  Ok(Some(commitment))
}

fn collect_binding_records<'a>(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  pages: &'a [OrderedPageV1<'a>],
  maximum_workspace_bytes: usize,
  workspace_bytes: &mut usize,
) -> FormatResult<Vec<BoundMutationRecordV1<'a>>> {
  let count = pages.iter().try_fold(0usize, |count, page| {
    count.checked_add(page.records.len()).ok_or_else(|| arithmetic_error("index_cow_mutation_binding_count", "record count overflowed"))
  })?;
  charge_binding_workspace(
    workspace_bytes,
    count
      .checked_mul(size_of::<BoundMutationRecordV1<'_>>())
      .ok_or_else(|| arithmetic_error("index_cow_mutation_binding_count", "record metadata bytes overflowed"))?,
    maximum_workspace_bytes,
  )?;
  let mut records: Vec<BoundMutationRecordV1<'a>> = Vec::new();
  records
    .try_reserve_exact(count)
    .map_err(|source| amplification_error("index_cow_mutation_binding_records", format!("record reservation failed: {source}")))?;
  let mut previous_page_upper: Option<&[u8]> = None;
  for page in pages {
    if page.role != role {
      return Err(closure_error("index_cow_mutation_binding_role", "mutation-binding pages disagree on ordered role"));
    }
    if let Some(upper) = previous_page_upper {
      if compare_order_keys(hash_algorithm, role, upper, page.lower_fence)? != Ordering::Less {
        return Err(order_error("index_cow_mutation_binding_page_order", "mutation-binding pages are not in strict logical order"));
      }
    }
    for record in page.records.iter() {
      let record = record?;
      let order_key = ordered_record_order_key(&record)?;
      charge_binding_workspace(workspace_bytes, order_key.len(), maximum_workspace_bytes)?;
      if let Some(previous) = records.last() {
        if compare_order_keys(hash_algorithm, role, &previous.order_key, &order_key)? != Ordering::Less {
          return Err(order_error("index_cow_mutation_binding_record_order", "mutation-binding records are not strictly ordered"));
        }
      }
      records.push(BoundMutationRecordV1 { order_key, encoded: record.encoded, tombstone: record.tombstone, mutation: None });
    }
    previous_page_upper = Some(page.upper_fence);
  }
  Ok(records)
}

fn collect_binding_mutations<'a>(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  mutations: &'a [OrderedPageMutationKindV1<'a>],
  maximum_workspace_bytes: usize,
  workspace_bytes: &mut usize,
) -> FormatResult<(Vec<BoundMutationRecordV1<'a>>, Vec<u8>)> {
  charge_binding_workspace(
    workspace_bytes,
    mutations
      .len()
      .checked_mul(size_of::<BoundMutationRecordV1<'_>>())
      .ok_or_else(|| arithmetic_error("index_cow_mutation_binding_count", "mutation metadata bytes overflowed"))?,
    maximum_workspace_bytes,
  )?;
  let mut bound: Vec<BoundMutationRecordV1<'a>> = Vec::new();
  bound
    .try_reserve_exact(mutations.len())
    .map_err(|source| amplification_error("index_cow_mutation_binding_records", format!("mutation reservation failed: {source}")))?;
  let mut commitment = IndexMutationCommitmentV1::new(hash_algorithm);
  for mutation in mutations {
    let decoded = decode_ordered_record(mutation.encoded_record(), hash_algorithm, role)?;
    if mutation.expects_tombstone() != decoded.tombstone
      || (mutation.removes_existing() && (role != OrderedIndexRoleV1::ScopeReverse || decoded.tombstone))
    {
      return Err(closure_error("index_cow_mutation_binding_kind", "mutation kind disagrees with its exact ordered record or role"));
    }
    let order_key = ordered_record_order_key(&decoded)?;
    charge_binding_workspace(workspace_bytes, order_key.len(), maximum_workspace_bytes)?;
    if bound.last().is_some_and(|previous| previous.order_key.as_slice() >= order_key.as_slice()) {
      return Err(order_error(
        "index_cow_mutation_binding_frozen_order",
        "bound mutations are not in strict canonical frozen-batch byte order",
      ));
    }
    commitment.push(*mutation)?;
    bound.push(BoundMutationRecordV1 { order_key, encoded: decoded.encoded, tombstone: decoded.tombstone, mutation: Some(*mutation) });
  }
  sort_bound_mutations_semantically(hash_algorithm, role, &mut bound)?;
  for pair in bound.windows(2) {
    if compare_order_keys(hash_algorithm, role, &pair[0].order_key, &pair[1].order_key)? != Ordering::Less {
      return Err(order_error("index_cow_mutation_binding_order", "bound mutations are not semantically unique"));
    }
  }
  Ok((bound, commitment.finish()))
}

fn sort_bound_mutations_semantically(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  records: &mut [BoundMutationRecordV1<'_>],
) -> FormatResult<()> {
  let len = records.len();
  for root in (0..len / 2).rev() {
    sift_bound_mutation_heap(hash_algorithm, role, records, root, len)?;
  }
  for end in (1..len).rev() {
    records.swap(0, end);
    sift_bound_mutation_heap(hash_algorithm, role, records, 0, end)?;
  }
  Ok(())
}

fn sift_bound_mutation_heap(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  records: &mut [BoundMutationRecordV1<'_>],
  mut root: usize,
  end: usize,
) -> FormatResult<()> {
  loop {
    let Some(left) = root.checked_mul(2).and_then(|value| value.checked_add(1)) else {
      return Err(arithmetic_error("index_cow_mutation_binding_sort", "mutation heap index overflowed"));
    };
    if left >= end {
      return Ok(());
    }
    let mut greatest = root;
    if compare_order_keys(hash_algorithm, role, &records[greatest].order_key, &records[left].order_key)? == Ordering::Less {
      greatest = left;
    }
    let right = left + 1;
    if right < end && compare_order_keys(hash_algorithm, role, &records[greatest].order_key, &records[right].order_key)? == Ordering::Less {
      greatest = right;
    }
    if greatest == root {
      return Ok(());
    }
    records.swap(root, greatest);
    root = greatest;
  }
}

fn validate_bound_record_stream(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  source: &[BoundMutationRecordV1<'_>],
  mutations: &[BoundMutationRecordV1<'_>],
  output: &[BoundMutationRecordV1<'_>],
) -> FormatResult<()> {
  let mut source_index = 0usize;
  let mut mutation_index = 0usize;
  let mut output_index = 0usize;
  while source_index < source.len() || mutation_index < mutations.len() {
    let source_record = source.get(source_index);
    let mutation_record = mutations.get(mutation_index);
    let order = match (source_record, mutation_record) {
      (Some(source), Some(mutation)) => compare_order_keys(hash_algorithm, role, &source.order_key, &mutation.order_key)?,
      (Some(_), None) => Ordering::Less,
      (None, Some(_)) => Ordering::Greater,
      (None, None) => break,
    };
    let expected = match order {
      Ordering::Less => {
        source_index += 1;
        source_record
      }
      Ordering::Greater => {
        mutation_index += 1;
        let mutation = mutation_record.ok_or_else(|| closure_error("index_cow_mutation_binding_stream", "missing mutation record"))?;
        if !matches!(mutation.mutation, Some(OrderedPageMutationKindV1::UpsertLive(_))) {
          return Err(closure_error("index_cow_mutation_binding_missing_source", "tombstone or remove mutation has no live source record"));
        }
        Some(mutation)
      }
      Ordering::Equal => {
        source_index += 1;
        mutation_index += 1;
        let source = source_record.ok_or_else(|| closure_error("index_cow_mutation_binding_stream", "missing source record"))?;
        let mutation = mutation_record.ok_or_else(|| closure_error("index_cow_mutation_binding_stream", "missing mutation record"))?;
        match mutation.mutation {
          Some(OrderedPageMutationKindV1::UpsertLive(_)) => Some(mutation),
          Some(OrderedPageMutationKindV1::TombstoneExisting(_)) if !source.tombstone => Some(mutation),
          Some(OrderedPageMutationKindV1::RemoveExisting(_)) if !source.tombstone => None,
          _ => {
            return Err(closure_error(
              "index_cow_mutation_binding_source_state",
              "tombstone or remove mutation does not replace a live source record",
            ));
          }
        }
      }
    };
    if let Some(expected) = expected {
      let observed = output
        .get(output_index)
        .ok_or_else(|| closure_error("index_cow_mutation_binding_output", "mutation-bound output is missing an expected record"))?;
      if expected.order_key != observed.order_key || expected.encoded != observed.encoded {
        return Err(closure_error(
          "index_cow_mutation_binding_output",
          "mutation-bound output record does not match the exact replay result",
        ));
      }
      output_index += 1;
    }
  }
  if output_index != output.len() {
    return Err(closure_error(
      "index_cow_mutation_binding_output",
      "mutation-bound output contains records absent from the exact replay result",
    ));
  }
  Ok(())
}

fn charge_binding_workspace(workspace_bytes: &mut usize, additional: usize, maximum_workspace_bytes: usize) -> FormatResult<()> {
  *workspace_bytes = workspace_bytes
    .checked_add(additional)
    .ok_or_else(|| arithmetic_error("index_cow_mutation_binding_workspace", "mutation-binding workspace bytes overflowed"))?;
  if *workspace_bytes > maximum_workspace_bytes {
    return Err(amplification_error(
      "index_cow_mutation_binding_workspace",
      format!("mutation binding exceeds the {maximum_workspace_bytes}-byte complete-operation cap"),
    ));
  }
  Ok(())
}

fn validate_page_plan_closure<'a>(request: &'a IndexCopyOnWriteClosureRequestV1<'_>) -> FormatResult<ValidatedPageClosureV1<'a>> {
  if request.page_plan.replacements.len() != request.source_pages.len() {
    return Err(closure_error("index_cow_closure_source_count", "source-page count does not match the page replacement count"));
  }
  let mut sources = Vec::new();
  if let Err(error) = sources.try_reserve_exact(request.source_pages.len()) {
    return Err(amplification_error("index_cow_closure_sources", format!("source-page reservation failed: {error}")));
  }
  let mut source_bytes = 0usize;
  for bytes in request.source_pages {
    source_bytes = source_bytes
      .checked_add(bytes.len())
      .ok_or_else(|| arithmetic_error("index_cow_closure_source_bytes", "source-page bytes overflowed"))?;
    if source_bytes > request.page_layout.maximum_workspace_bytes {
      return Err(amplification_error(
        "index_cow_closure_source_workspace",
        format!("source-page bytes exceed the {}-byte operation cap", request.page_layout.maximum_workspace_bytes),
      ));
    }
    sources.push(decode_ordered_page(bytes, request.hash_algorithm)?);
  }
  let first = sources.first().ok_or_else(|| closure_error("index_cow_closure_source_count", "copy-on-write closure has no source page"))?;
  let role = first.role;
  let owner_id = copy_fallible_bytes(first.owner_id, "closure owner")?;
  let mut sources_by_key = BTreeMap::new();
  let mut source_keys = BTreeSet::new();
  let mut source_page_ids = BTreeSet::new();
  let mut observed_source_page_id_high_water = 0u64;
  for (source_index, source) in sources.iter().enumerate() {
    if source.role != role || source.owner_id != owner_id || source.generation >= request.generation {
      return Err(identity_error(
        "index_cow_closure_source_identity",
        "source pages disagree on owner or role, or are not older than the closure generation",
      ));
    }
    if !source_keys.insert(source.key.clone()) || sources_by_key.insert(source.key.clone(), source_index).is_some() {
      return Err(closure_error("index_cow_closure_duplicate_source", "copy-on-write closure repeats one immutable source page"));
    }
    if role.uses_page_id() && !source_page_ids.insert(source.page_id) {
      return Err(closure_error("index_cow_closure_duplicate_page_id", "source pages reuse one stable PageId"));
    }
    if role.uses_page_id() {
      observed_source_page_id_high_water =
        observed_source_page_id_high_water.max(source.page_id).max(source.previous_page_id).max(source.next_page_id);
    }
  }

  validate_page_id_deltas(request, role, &source_page_ids, observed_source_page_id_high_water)?;
  let output_count = request.page_plan.replacements.iter().try_fold(0usize, |count, replacement| {
    count
      .checked_add(replacement.artifacts.len())
      .ok_or_else(|| arithmetic_error("index_cow_closure_output_count", "page artifact count overflowed"))
  })?;
  let output_metadata_bytes = output_count
    .checked_mul(size_of::<OrderedPageV1<'_>>())
    .ok_or_else(|| arithmetic_error("index_cow_closure_output_count", "page output metadata bytes overflowed"))?;
  if output_metadata_bytes > request.page_layout.maximum_workspace_bytes {
    return Err(amplification_error(
      "index_cow_closure_output_count",
      format!("page output metadata exceeds the {}-byte operation cap", request.page_layout.maximum_workspace_bytes),
    ));
  }
  let mut output_pages = Vec::new();
  if let Err(error) = output_pages.try_reserve_exact(output_count) {
    return Err(amplification_error("index_cow_closure_outputs", format!("page-output reservation failed: {error}")));
  }
  let mut output_bytes = 0usize;
  let mut output_keys = BTreeSet::new();
  let mut output_page_ids = BTreeSet::new();
  let mut allocated_page_ids = BTreeSet::new();
  for page_id in &request.page_plan.allocated_page_ids {
    allocated_page_ids.insert(*page_id);
  }
  for (replacement_index, replacement) in request.page_plan.replacements.iter().enumerate() {
    if sources.get(replacement_index).is_none_or(|source| source.key != replacement.source_key) {
      return Err(closure_error("index_cow_closure_source_order", "replacement order does not match the exact supplied source-page order"));
    }
    let source_index = sources_by_key
      .get(replacement.source_key.as_slice())
      .ok_or_else(|| closure_error("index_cow_closure_unknown_source", "page replacement names a source outside the supplied closure"))?;
    let source = &sources[*source_index];
    if !replacement_matches_source(replacement, source) {
      return Err(closure_error(
        "index_cow_closure_source_summary",
        "page replacement does not exactly summarize its immutable source page",
      ));
    }
    if replacement.source_retired != replacement.artifacts.is_empty() {
      return Err(closure_error(
        "index_cow_closure_retirement_output",
        "retired page replacement presence disagrees with its output artifacts",
      ));
    }
    let mut previous_output: Option<&OrderedPageV1<'_>> = None;
    let mut retains_source_page_id = false;
    for artifact in &replacement.artifacts {
      output_bytes = output_bytes
        .checked_add(artifact.value.len())
        .ok_or_else(|| arithmetic_error("index_cow_closure_output_bytes", "page artifact bytes overflowed"))?;
      if artifact.value.len() > request.page_layout.hard_artifact_bytes || output_bytes > request.page_layout.maximum_workspace_bytes {
        return Err(amplification_error(
          "index_cow_closure_page_workspace",
          format!("page artifacts exceed the {}-byte operation cap", request.page_layout.maximum_workspace_bytes),
        ));
      }
      let page = decode_ordered_page(&artifact.value, request.hash_algorithm)?;
      if page.key != artifact.key
        || page.role != role
        || page.owner_id != owner_id
        || page.generation != request.generation
        || !output_keys.insert(page.key.clone())
      {
        return Err(closure_error(
          "index_cow_closure_page_output",
          "page output has a duplicate or inconsistent key, owner, role, or generation",
        ));
      }
      if let Some(previous) = previous_output {
        if compare_order_keys(request.hash_algorithm, role, previous.upper_fence, page.lower_fence)? != Ordering::Less {
          return Err(closure_error("index_cow_closure_page_order", "one replacement emits overlapping or unordered pages"));
        }
      }
      if role.uses_page_id() {
        retains_source_page_id |= page.page_id == source.page_id;
        if !output_page_ids.insert(page.page_id) {
          return Err(closure_error("index_cow_closure_duplicate_output_page_id", "page outputs reuse one stable PageId"));
        }
        if !source_page_ids.contains(&page.page_id) && !allocated_page_ids.contains(&page.page_id) {
          return Err(identity_error("index_cow_closure_unallocated_page_id", "page output uses neither a source nor an allocated PageId"));
        }
        if page.page_id >= request.page_plan.next_page_id
          || page.previous_page_id >= request.page_plan.next_page_id
          || page.next_page_id >= request.page_plan.next_page_id
        {
          return Err(identity_error(
            "index_cow_closure_page_id_high_water",
            "page output reaches or exceeds the next PageId high-water mark",
          ));
        }
      }
      output_pages.push(page);
      previous_output = output_pages.last();
    }
    if role.uses_page_id() {
      let retirement_set_contains_source = request.page_plan.retired_page_ids.contains(&source.page_id);
      if replacement.source_retired != retirement_set_contains_source || replacement.source_retired == retains_source_page_id {
        return Err(closure_error(
          "index_cow_closure_source_retirement",
          "source retirement flag, retirement set, and stable PageId survival disagree",
        ));
      }
    }
  }
  if sources_by_key.len() != request.page_plan.replacements.len() {
    return Err(closure_error("index_cow_closure_missing_source", "not every supplied source page has one replacement"));
  }
  validate_page_id_output_closure(request, role, &source_page_ids, &output_page_ids)?;
  validate_output_posting_links(request, &source_page_ids, &output_pages)?;
  Ok(ValidatedPageClosureV1 {
    role,
    owner_id,
    source_keys,
    source_pages: sources,
    output_pages,
    source_page_bytes: source_bytes,
    page_artifact_bytes: output_bytes,
  })
}

fn validate_page_id_deltas(
  request: &IndexCopyOnWriteClosureRequestV1<'_>,
  role: OrderedIndexRoleV1,
  source_page_ids: &BTreeSet<u64>,
  observed_source_page_id_high_water: u64,
) -> FormatResult<()> {
  if !role.uses_page_id() {
    if request.initial_next_page_id != 0
      || request.page_plan.next_page_id != 0
      || !request.page_plan.allocated_page_ids.is_empty()
      || !request.page_plan.retired_page_ids.is_empty()
    {
      return Err(identity_error("index_cow_closure_scope_page_id", "scope-page closure carries PageId state"));
    }
    return Ok(());
  }
  if source_page_ids.is_empty() {
    return Err(closure_error("index_cow_closure_source_page_id", "PageId-bearing closure has no source PageIds"));
  }
  if request.initial_next_page_id == 0 || request.initial_next_page_id <= observed_source_page_id_high_water {
    return Err(identity_error("index_cow_closure_initial_page_id", "initial next PageId does not exceed every supplied source PageId"));
  }
  let allocation_count = request
    .page_plan
    .next_page_id
    .checked_sub(request.initial_next_page_id)
    .ok_or_else(|| identity_error("index_cow_closure_page_id_regression", "next PageId regressed below the initial high-water mark"))?;
  let allocation_count = usize::try_from(allocation_count).map_err(|source| {
    amplification_error("index_cow_closure_allocation_count", format!("allocated PageId count is not representable: {source}"))
  })?;
  if request.page_plan.allocated_page_ids.len() != allocation_count {
    return Err(closure_error("index_cow_closure_allocation_range", "allocated PageId list does not exactly cover the high-water advance"));
  }
  for (offset, page_id) in request.page_plan.allocated_page_ids.iter().enumerate() {
    let expected = request
      .initial_next_page_id
      .checked_add(u64::try_from(offset).map_err(|source| {
        amplification_error("index_cow_closure_allocation_count", format!("allocated PageId offset is not representable: {source}"))
      })?)
      .ok_or_else(|| arithmetic_error("index_cow_closure_allocation_range", "allocated PageId range overflowed"))?;
    if *page_id != expected {
      return Err(closure_error("index_cow_closure_allocation_range", "allocated PageIds are not one exact contiguous range"));
    }
  }
  for pair in request.page_plan.retired_page_ids.windows(2) {
    if pair[0] >= pair[1] {
      return Err(closure_error("index_cow_closure_retirement_order", "retired PageIds are duplicate or not strictly ordered"));
    }
  }
  if request.page_plan.retired_page_ids.iter().any(|page_id| {
    *page_id == 0
      || (*page_id >= request.initial_next_page_id && *page_id < request.page_plan.next_page_id)
      || !source_page_ids.contains(page_id)
  }) {
    return Err(closure_error(
      "index_cow_closure_retirement_set",
      "retired PageIds include zero, an allocated identity, or a PageId outside the source closure",
    ));
  }
  Ok(())
}

fn validate_page_id_output_closure(
  request: &IndexCopyOnWriteClosureRequestV1<'_>,
  role: OrderedIndexRoleV1,
  source_page_ids: &BTreeSet<u64>,
  output_page_ids: &BTreeSet<u64>,
) -> FormatResult<()> {
  if !role.uses_page_id() {
    return Ok(());
  }
  let mut expected_retired = Vec::new();
  if let Err(error) = expected_retired.try_reserve_exact(source_page_ids.len()) {
    return Err(amplification_error("index_cow_closure_retirement_set", format!("retired PageId reservation failed: {error}")));
  }
  for page_id in source_page_ids.difference(output_page_ids) {
    expected_retired.push(*page_id);
  }
  if expected_retired != request.page_plan.retired_page_ids {
    return Err(closure_error(
      "index_cow_closure_retirement_set",
      "retired PageIds do not exactly equal source PageIds absent from output",
    ));
  }
  for allocated in &request.page_plan.allocated_page_ids {
    if !output_page_ids.contains(allocated) {
      return Err(closure_error("index_cow_closure_allocated_page_missing", "an allocated PageId has no output page"));
    }
  }
  Ok(())
}

fn validate_output_posting_links(
  request: &IndexCopyOnWriteClosureRequestV1<'_>,
  source_page_ids: &BTreeSet<u64>,
  output_pages: &[OrderedPageV1<'_>],
) -> FormatResult<()> {
  if output_pages.first().is_none_or(|page| page.role != OrderedIndexRoleV1::Posting) {
    return Ok(());
  }
  let mut output_by_page_id = BTreeMap::new();
  let mut retired = BTreeSet::new();
  let mut allocated = BTreeSet::new();
  for (index, page) in output_pages.iter().enumerate() {
    output_by_page_id.insert(page.page_id, index);
  }
  for page_id in &request.page_plan.retired_page_ids {
    retired.insert(*page_id);
  }
  for page_id in &request.page_plan.allocated_page_ids {
    allocated.insert(*page_id);
  }
  for page in output_pages {
    for linked_page_id in [page.previous_page_id, page.next_page_id] {
      if linked_page_id == 0 {
        continue;
      }
      if retired.contains(&linked_page_id)
        || (source_page_ids.contains(&linked_page_id) && !output_by_page_id.contains_key(&linked_page_id))
        || (allocated.contains(&linked_page_id) && !output_by_page_id.contains_key(&linked_page_id))
      {
        return Err(closure_error("index_cow_closure_detached_link", "page output links to a retired or absent affected PageId"));
      }
    }
    if let Some(previous_index) = output_by_page_id.get(&page.previous_page_id) {
      validate_posting_page_link(&output_pages[*previous_index], page, request.hash_algorithm)?;
    }
    if let Some(next_index) = output_by_page_id.get(&page.next_page_id) {
      validate_posting_page_link(page, &output_pages[*next_index], request.hash_algorithm)?;
    }
  }
  Ok(())
}

fn validate_directory_plan_closure(
  request: &IndexCopyOnWriteClosureRequestV1<'_>,
  page_closure: &ValidatedPageClosureV1<'_>,
) -> FormatResult<usize> {
  let directory_request = ArtifactDirectoryMutationRequestV1 {
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    page_plan: request.page_plan,
    paths: request.paths,
    layout: request.directory_layout,
  };
  let replacement_entries = validate_page_replacement_outputs(&directory_request)?;
  let graph = validate_directory_path_graph(&directory_request, &replacement_entries)?;
  if request.directory_plan.source_root_key != graph.source_root_key {
    return Err(closure_error("index_cow_closure_source_root", "directory plan source root disagrees with the supplied paths"));
  }

  let mut source_child_summaries = BTreeMap::new();
  let mut affected_directory_keys = BTreeSet::new();
  for path in request.paths {
    for bytes in path.directories {
      let directory = decode_artifact_directory(bytes, request.hash_algorithm)?;
      affected_directory_keys.insert(directory.key.clone());
      for entry in &directory.entries {
        let summary = OwnedDirectoryEntryV1::from_existing(entry)?;
        if let Some(previous) = source_child_summaries.insert(entry.child_hash.to_vec(), summary.clone()) {
          if previous != summary {
            return Err(closure_error(
              "index_cow_closure_source_child_collision",
              "one source child hash has inconsistent directory summaries",
            ));
          }
        }
      }
    }
  }

  let mut new_page_summaries = BTreeMap::new();
  let mut page_reference_counts: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
  for page in &page_closure.output_pages {
    if new_page_summaries.insert(page.key.clone(), OwnedDirectoryEntryV1::from_page(page)?).is_some() {
      return Err(closure_error("index_cow_closure_duplicate_page_key", "one output page key appears more than once"));
    }
    page_reference_counts.insert(page.key.clone(), 0u64);
  }

  let mut directory_bytes = 0usize;
  let mut new_directory_summaries = BTreeMap::new();
  let mut directory_reference_counts: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
  for artifact in &request.directory_plan.artifacts {
    directory_bytes = directory_bytes
      .checked_add(artifact.value.len())
      .ok_or_else(|| arithmetic_error("index_cow_closure_directory_bytes", "directory artifact bytes overflowed"))?;
    if artifact.value.len() > request.directory_layout.hard_artifact_bytes
      || directory_bytes > request.directory_layout.maximum_workspace_bytes
    {
      return Err(amplification_error(
        "index_cow_closure_directory_workspace",
        format!("directory artifacts exceed the {}-byte operation cap", request.directory_layout.maximum_workspace_bytes),
      ));
    }
    let directory = decode_artifact_directory(&artifact.value, request.hash_algorithm)?;
    if directory.key != artifact.key
      || directory.owner_id != page_closure.owner_id
      || directory.role != page_closure.role
      || directory.generation != request.generation
      || new_directory_summaries.contains_key(directory.key.as_slice())
    {
      return Err(closure_error(
        "index_cow_closure_directory_output",
        "directory output has a duplicate or inconsistent key, owner, role, or generation",
      ));
    }
    for entry in &directory.entries {
      if entry.physical_hint != (PhysicalHintV1 { wal_offset: 0, total_length: 0, write_sequence: 0 }) {
        return Err(closure_error("index_cow_closure_physical_hint", "rewritten directory descriptors retain a physical hint"));
      }
      let expected = if let Some(summary) = new_page_summaries.get(entry.child_hash) {
        let count = page_reference_counts
          .get_mut(entry.child_hash)
          .ok_or_else(|| closure_error("index_cow_closure_page_reference", "new page reference counter is missing"))?;
        *count = count
          .checked_add(1)
          .ok_or_else(|| arithmetic_error("index_cow_closure_page_reference", "new page reference count overflowed"))?;
        summary
      } else if let Some(summary) = new_directory_summaries.get(entry.child_hash) {
        let count = directory_reference_counts
          .get_mut(entry.child_hash)
          .ok_or_else(|| closure_error("index_cow_closure_directory_reference", "new directory reference counter is missing"))?;
        *count = count
          .checked_add(1)
          .ok_or_else(|| arithmetic_error("index_cow_closure_directory_reference", "new directory reference count overflowed"))?;
        summary
      } else {
        if page_closure.source_keys.contains(entry.child_hash) || affected_directory_keys.contains(entry.child_hash) {
          return Err(closure_error("index_cow_closure_stale_child", "rewritten directory still references an affected source artifact"));
        }
        source_child_summaries.get(entry.child_hash).ok_or_else(|| {
          closure_error("index_cow_closure_unknown_child", "rewritten directory references an unknown or out-of-order child")
        })?
      };
      if !directory_entry_matches_owned(entry, expected) {
        return Err(closure_error("index_cow_closure_child_summary", "rewritten child descriptor disagrees with its exact child summary"));
      }
    }
    let key = directory.key.clone();
    new_directory_summaries.insert(key.clone(), OwnedDirectoryEntryV1::from_directory(&directory)?);
    directory_reference_counts.insert(key, 0u64);
  }

  validate_directory_root_closure(
    request.hash_algorithm,
    request.directory_plan,
    &new_page_summaries,
    &page_reference_counts,
    &new_directory_summaries,
    &directory_reference_counts,
  )?;
  Ok(directory_bytes)
}

fn validate_directory_root_closure(
  hash_algorithm: HashAlgorithm,
  plan: &ArtifactDirectoryMutationPlanV1,
  new_page_summaries: &BTreeMap<Vec<u8>, OwnedDirectoryEntryV1>,
  page_reference_counts: &BTreeMap<Vec<u8>, u64>,
  new_directory_summaries: &BTreeMap<Vec<u8>, OwnedDirectoryEntryV1>,
  directory_reference_counts: &BTreeMap<Vec<u8>, u64>,
) -> FormatResult<()> {
  let Some(root_key) = &plan.root_key else {
    if plan.root_level != 0
      || plan.live_count != 0
      || plan.tombstone_count != 0
      || plan.page_count != 0
      || plan.logical_bytes != 0
      || plan.minimum_page_id != 0
      || plan.maximum_page_id != 0
      || !plan.artifacts.is_empty()
      || !new_page_summaries.is_empty()
      || !new_directory_summaries.is_empty()
    {
      return Err(closure_error("index_cow_closure_absent_root", "absent directory root retains artifacts or nonzero aggregate state"));
    }
    return Ok(());
  };
  let root = new_directory_summaries
    .get(root_key.as_slice())
    .ok_or_else(|| closure_error("index_cow_closure_root_missing", "directory root key is absent from dependency-ordered output"))?;
  let root_artifact = plan
    .artifacts
    .last()
    .filter(|artifact| artifact.key == *root_key)
    .ok_or_else(|| closure_error("index_cow_closure_root_order", "directory root is not the final dependency-ordered artifact"))?;
  let root_node = decode_artifact_directory(&root_artifact.value, hash_algorithm)?;
  if plan.root_level != root_node.level
    || plan.live_count != root.live_count
    || plan.tombstone_count != root.tombstone_count
    || plan.page_count != root.page_count
    || plan.logical_bytes != root.logical_bytes
    || plan.minimum_page_id != root.minimum_page_id
    || plan.maximum_page_id != root.maximum_page_id
  {
    return Err(closure_error("index_cow_closure_root_summary", "directory plan root or aggregate summary is inconsistent"));
  }
  if page_reference_counts.values().any(|count| *count != 1) {
    return Err(closure_error("index_cow_closure_page_reference", "an output page is detached or referenced more than once"));
  }
  for (key, count) in directory_reference_counts {
    let expected = u64::from(key.as_slice() != root_key.as_slice());
    if *count != expected {
      return Err(closure_error(
        "index_cow_closure_directory_reference",
        "a directory output is detached, multiply referenced, or treats a non-root artifact as root",
      ));
    }
  }
  Ok(())
}

fn validate_directory_path_input_workspace(paths: &[ArtifactDirectoryPathV1<'_>], maximum_workspace_bytes: usize) -> FormatResult<usize> {
  let mut path_bytes = 0usize;
  for path in paths {
    path_bytes = path.directories.iter().try_fold(path_bytes, |bytes, directory| {
      bytes
        .checked_add(directory.len())
        .ok_or_else(|| arithmetic_error("index_cow_directory_path_bytes", "directory path input bytes overflowed"))
    })?;
    if path_bytes > maximum_workspace_bytes {
      return Err(amplification_error(
        "index_cow_directory_path_workspace",
        format!("directory path inputs exceed the {maximum_workspace_bytes}-byte operation cap"),
      ));
    }
  }
  Ok(path_bytes)
}

fn replacement_matches_source(replacement: &OrderedPageReplacementV1, source: &OrderedPageV1<'_>) -> bool {
  replacement.source_key == source.key
    && replacement.source_page_id == source.page_id
    && replacement.source_role == source.role
    && replacement.source_owner_id == source.owner_id
    && replacement.source_generation == source.generation
    && replacement.source_lower_fence == source.lower_fence
    && replacement.source_upper_fence == source.upper_fence
    && replacement.source_live_count == u64::from(source.live_count)
    && replacement.source_tombstone_count == u64::from(source.tombstone_count)
    && replacement.source_logical_bytes == source.logical_live_bytes
}

fn directory_entry_matches_owned(entry: &ArtifactDirectoryEntryV1<'_>, expected: &OwnedDirectoryEntryV1) -> bool {
  entry.lower_fence == expected.lower_fence
    && entry.upper_fence == expected.upper_fence
    && entry.child_hash == expected.child_hash
    && entry.child_generation == expected.child_generation
    && entry.live_count == expected.live_count
    && entry.tombstone_count == expected.tombstone_count
    && entry.page_count == expected.page_count
    && entry.logical_bytes == expected.logical_bytes
    && entry.minimum_page_id == expected.minimum_page_id
    && entry.maximum_page_id == expected.maximum_page_id
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
  target_bytes: usize,
  artifacts: &mut Vec<EncodedImmutableIndexArtifactV1>,
  retained_artifact_bytes: &mut usize,
) -> FormatResult<Vec<OwnedDirectoryEntryV1>> {
  if entries.is_empty() {
    return Ok(Vec::new());
  }
  let groups = partition_directory_entries(request.hash_algorithm, level, entries, target_bytes)?;
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
    return Ok(std::iter::once(0..entries.len()).collect());
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

fn validate_mutation_identity(request: &OrderedPageBatchMutationRequestV1<'_>, source: &OrderedPageV1<'_>) -> FormatResult<()> {
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

fn decode_ordered_mutation_batch<'a>(
  request: &'a OrderedPageBatchMutationRequestV1<'a>,
  role: OrderedIndexRoleV1,
) -> FormatResult<Vec<DecodedOrderedMutationV1<'a>>> {
  let metadata_bytes = request
    .mutations
    .len()
    .checked_mul(size_of::<DecodedOrderedMutationV1<'_>>())
    .ok_or_else(|| arithmetic_error("index_cow_batch_workspace", "mutation metadata workspace overflowed"))?;
  if metadata_bytes > request.layout.maximum_workspace_bytes {
    return Err(amplification_error(
      "index_cow_batch_workspace",
      format!("mutation metadata exceeds the {}-byte operation cap", request.layout.maximum_workspace_bytes),
    ));
  }
  let mut decoded: Vec<DecodedOrderedMutationV1<'a>> = Vec::new();
  decoded
    .try_reserve_exact(request.mutations.len())
    .map_err(|error| amplification_error("index_cow_batch_capacity", format!("mutation batch reservation failed: {error}")))?;
  let mut retained_bytes = decoded
    .capacity()
    .checked_mul(size_of::<DecodedOrderedMutationV1<'_>>())
    .ok_or_else(|| arithmetic_error("index_cow_batch_workspace", "mutation metadata workspace overflowed"))?;
  for mutation in request.mutations {
    let record = decode_ordered_record(mutation.encoded_record(), request.hash_algorithm, role)?;
    if record.tombstone != mutation.expects_tombstone() {
      return Err(closure_error("index_cow_mutation_tombstone", "mutation kind disagrees with the encoded record tombstone flag"));
    }
    if mutation.removes_existing() && role != OrderedIndexRoleV1::ScopeReverse {
      return Err(closure_error("index_cow_remove_role", "physical ordered-record removal is permitted only for scope-reverse rows"));
    }
    let order_key = ordered_record_order_key(&record)?;
    if let Some(previous) = decoded.last() {
      if compare_order_keys(request.hash_algorithm, role, &previous.order_key, &order_key)? != Ordering::Less {
        return Err(closure_error("index_cow_batch_order", "ordered page mutation batch keys are not strictly increasing and unique"));
      }
    }
    retained_bytes = retained_bytes
      .checked_add(order_key.len())
      .ok_or_else(|| arithmetic_error("index_cow_batch_workspace", "mutation order-key workspace overflowed"))?;
    if retained_bytes > request.layout.maximum_workspace_bytes {
      return Err(amplification_error(
        "index_cow_batch_workspace",
        format!("mutation keys exceed the {}-byte operation cap", request.layout.maximum_workspace_bytes),
      ));
    }
    decoded.push(DecodedOrderedMutationV1 {
      encoded: mutation.encoded_record(),
      order_key,
      tombstone: record.tombstone,
      remove_existing: mutation.removes_existing(),
    });
  }
  Ok(decoded)
}

fn merge_ordered_mutation_batch(
  request: &OrderedPageBatchMutationRequestV1<'_>,
  role: OrderedIndexRoleV1,
  source_records: Vec<OwnedOrderedRecordV1>,
  decoded_mutations: Vec<DecodedOrderedMutationV1<'_>>,
) -> FormatResult<(Vec<OwnedOrderedRecordV1>, bool)> {
  let output_capacity = source_records
    .len()
    .checked_add(decoded_mutations.len())
    .ok_or_else(|| arithmetic_error("index_cow_batch_capacity", "merged record capacity overflowed"))?;
  let peak_metadata_bytes = source_records
    .capacity()
    .checked_mul(size_of::<OwnedOrderedRecordV1>())
    .and_then(|bytes| {
      decoded_mutations.capacity().checked_mul(size_of::<DecodedOrderedMutationV1<'_>>()).and_then(|extra| bytes.checked_add(extra))
    })
    .and_then(|bytes| output_capacity.checked_mul(size_of::<OwnedOrderedRecordV1>()).and_then(|extra| bytes.checked_add(extra)))
    .ok_or_else(|| arithmetic_error("index_cow_batch_workspace", "merged record metadata workspace overflowed"))?;
  let retained_payload_bytes = source_records
    .iter()
    .try_fold(0usize, |bytes, record| bytes.checked_add(record.encoded.len()).and_then(|value| value.checked_add(record.order_key.len())))
    .ok_or_else(|| arithmetic_error("index_cow_batch_workspace", "source record workspace overflowed"))?;
  let mutation_key_bytes = decoded_mutations
    .iter()
    .try_fold(0usize, |bytes, mutation| bytes.checked_add(mutation.order_key.len()))
    .ok_or_else(|| arithmetic_error("index_cow_batch_workspace", "mutation order-key workspace overflowed"))?;
  let mutation_encoded_bytes = decoded_mutations
    .iter()
    .try_fold(0usize, |bytes, mutation| bytes.checked_add(mutation.encoded.len()))
    .ok_or_else(|| arithmetic_error("index_cow_batch_workspace", "mutation record workspace overflowed"))?;
  let peak_bytes = peak_metadata_bytes
    .checked_add(retained_payload_bytes)
    .and_then(|bytes| bytes.checked_add(mutation_key_bytes))
    .and_then(|bytes| bytes.checked_add(mutation_encoded_bytes))
    .ok_or_else(|| arithmetic_error("index_cow_batch_workspace", "merged record workspace overflowed"))?;
  if peak_bytes > request.layout.maximum_workspace_bytes {
    return Err(amplification_error(
      "index_cow_batch_workspace",
      format!("{peak_bytes} batch workspace bytes exceed the {}-byte operation cap", request.layout.maximum_workspace_bytes),
    ));
  }

  let mut merged = Vec::new();
  merged
    .try_reserve_exact(output_capacity)
    .map_err(|error| amplification_error("index_cow_batch_capacity", format!("merged record reservation failed: {error}")))?;
  let mut sources = source_records.into_iter().peekable();
  let mut mutations = decoded_mutations.into_iter().peekable();
  let mut changed = false;
  while let Some(mutation) = mutations.peek() {
    let ordering = match sources.peek() {
      Some(source) => compare_order_keys(request.hash_algorithm, role, &source.order_key, &mutation.order_key)?,
      None => Ordering::Greater,
    };
    match ordering {
      Ordering::Less => {
        merged.push(sources.next().ok_or_else(|| closure_error("index_cow_batch_source", "source iterator ended unexpectedly"))?);
      }
      Ordering::Equal => {
        let source = sources.next().ok_or_else(|| closure_error("index_cow_batch_source", "source iterator ended unexpectedly"))?;
        let mutation = mutations.next().ok_or_else(|| closure_error("index_cow_batch_mutation", "mutation iterator ended unexpectedly"))?;
        if mutation.remove_existing {
          changed = true;
        } else if source.encoded == mutation.encoded {
          merged.push(source);
        } else {
          changed = true;
          merged.push(OwnedOrderedRecordV1 {
            encoded: copy_batch_bytes(mutation.encoded, "mutation record")?,
            order_key: mutation.order_key,
            tombstone: mutation.tombstone,
          });
        }
      }
      Ordering::Greater => {
        let mutation = mutations.next().ok_or_else(|| closure_error("index_cow_batch_mutation", "mutation iterator ended unexpectedly"))?;
        if mutation.remove_existing {
          return Err(closure_error("index_cow_remove_missing", "cannot physically remove an ordered record that is not present"));
        }
        if mutation.tombstone {
          return Err(closure_error("index_cow_tombstone_missing", "cannot tombstone an ordered record that is not present"));
        }
        changed = true;
        merged.push(OwnedOrderedRecordV1 {
          encoded: copy_batch_bytes(mutation.encoded, "mutation record")?,
          order_key: mutation.order_key,
          tombstone: false,
        });
      }
    }
  }
  merged.extend(sources);
  Ok((merged, changed))
}

fn copy_batch_bytes(value: &[u8], context: &'static str) -> FormatResult<Vec<u8>> {
  let mut copied = Vec::new();
  copied
    .try_reserve_exact(value.len())
    .map_err(|error| amplification_error("index_cow_batch_allocation", format!("{context} allocation failed: {error}")))?;
  copied.extend_from_slice(value);
  Ok(copied)
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
  request: &OrderedPageBatchMutationRequestV1<'_>,
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
  request: &OrderedPageBatchMutationRequestV1<'_>,
  source: &OrderedPageV1<'_>,
  records: &[OwnedOrderedRecordV1],
) -> FormatResult<Vec<std::ops::Range<usize>>> {
  let unsplit_length = checked_page_range_representable_length(request.hash_algorithm, source.role, records)?;
  if unsplit_length <= request.layout.split_above_bytes || records.len() == 1 {
    return Ok(std::iter::once(0..records.len()).collect());
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
  request: &OrderedPageBatchMutationRequestV1<'_>,
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

fn order_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::NoncanonicalOrderOrDuplicate, code, context)
}

fn arithmetic_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, code, context)
}

fn amplification_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::AllocationAmplification, code, context)
}
