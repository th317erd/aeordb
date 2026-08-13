use std::cmp::Ordering;
use std::mem::size_of;

use crate::engine::HashAlgorithm;

use super::index_artifact::{
  EncodedImmutableIndexArtifactV1, checked_immutable_index_artifact_encoded_length, checked_immutable_index_artifact_representable_length,
};
use super::index_page::{
  OrderedIndexRoleV1, OrderedPageV1, OrderedPageWriteV1, compare_order_keys, decode_ordered_page, decode_ordered_record,
  encode_ordered_page, ordered_record_order_key, validate_posting_page_link,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

pub const INDEX_PAGE_TARGET_BYTES_V1: usize = 64 * 1_024;
pub const INDEX_PAGE_SPLIT_ABOVE_BYTES_V1: usize = 96 * 1_024;
pub const INDEX_PAGE_MERGE_BELOW_BYTES_V1: usize = 16 * 1_024;
pub const INDEX_ARTIFACT_HARD_CAP_BYTES_V1: usize = 4 * 1_024 * 1_024;
pub const INDEX_COPY_ON_WRITE_WORKSPACE_BYTES_V1: usize = 32 * 1_024 * 1_024;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedPageReplacementV1 {
  pub source_key: Vec<u8>,
  pub source_page_id: u64,
  pub artifacts: Vec<EncodedImmutableIndexArtifactV1>,
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

#[derive(Debug)]
struct OwnedOrderedRecordV1 {
  encoded: Vec<u8>,
  order_key: Vec<u8>,
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
  let replacement = OwnedOrderedRecordV1 { encoded: request.mutation.encoded_record().to_vec(), order_key: mutation_order_key };
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

  let mut replacements = vec![OrderedPageReplacementV1 { source_key: source.key.clone(), source_page_id: source.page_id, artifacts }];
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
    replacements.push(OrderedPageReplacementV1 {
      source_key: next.key.clone(),
      source_page_id: next.page_id,
      artifacts: vec![rewritten_next],
    });
  }

  Ok(OrderedPageMutationPlanV1 {
    replacements,
    allocated_page_ids: page_id_allocator.allocated_page_ids,
    retired_page_ids: Vec::new(),
    next_page_id: page_id_allocator.next_page_id,
  })
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
    records.push(OwnedOrderedRecordV1 { encoded: record.encoded.to_vec(), order_key });
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
