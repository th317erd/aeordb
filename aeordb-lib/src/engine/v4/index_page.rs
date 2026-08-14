use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::engine::HashAlgorithm;

use super::index_artifact::{
  EncodedImmutableIndexArtifactV1, ImmutableIndexArtifactKindV1, ImmutableIndexArtifactWriteV1,
  checked_immutable_index_artifact_encoded_length, decode_immutable_index_artifact, encode_immutable_index_artifact, u16_at, u32_at,
  u64_at,
};
use super::index_record::{
  DocumentStateOwnerV1, decode_canonical_value_record_prefix, decode_document_state_record_prefix, decode_scope_document_record_prefix,
  decode_scope_reverse_record_prefix,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const DIRECTORY_KIND: u16 = 0x0020;
const POSTING_PAGE_KIND: u16 = 0x0030;
const VALUE_PAGE_KIND: u16 = 0x0031;
const SCOPE_PAGE_KIND: u16 = 0x0033;
const STATE_PAGE_KIND: u16 = 0x0034;
const MAX_ARTIFACT_LENGTH: usize = 4 * 1_024 * 1_024;
const MAX_KEY_LENGTH: usize = 1_024 * 1_024;
const MAX_DIRECTORY_ENTRIES: u32 = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderedIndexRoleV1 {
  ScopeOrdinal,
  ScopeReverse,
  Value,
  ValueDocumentState,
  Posting,
  IndexDocumentState,
  NvtTile,
}

impl OrderedIndexRoleV1 {
  pub fn id(self) -> u8 {
    match self {
      Self::ScopeOrdinal => 1,
      Self::ScopeReverse => 2,
      Self::Value => 3,
      Self::ValueDocumentState => 4,
      Self::Posting => 5,
      Self::IndexDocumentState => 6,
      Self::NvtTile => 7,
    }
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::ScopeOrdinal => "scope-ordinal",
      Self::ScopeReverse => "scope-reverse",
      Self::Value => "value",
      Self::ValueDocumentState => "value-document-state",
      Self::Posting => "posting",
      Self::IndexDocumentState => "index-document-state",
      Self::NvtTile => "nvt-tile",
    }
  }

  fn from_id(id: u8) -> Option<Self> {
    match id {
      1 => Some(Self::ScopeOrdinal),
      2 => Some(Self::ScopeReverse),
      3 => Some(Self::Value),
      4 => Some(Self::ValueDocumentState),
      5 => Some(Self::Posting),
      6 => Some(Self::IndexDocumentState),
      7 => Some(Self::NvtTile),
      _ => None,
    }
  }

  pub fn owner_class(self) -> u8 {
    match self {
      Self::ScopeOrdinal | Self::ScopeReverse => 1,
      Self::Value | Self::ValueDocumentState => 2,
      Self::Posting | Self::IndexDocumentState | Self::NvtTile => 3,
    }
  }

  pub fn key_codec(self) -> u16 {
    match self {
      Self::ScopeOrdinal | Self::ValueDocumentState | Self::IndexDocumentState => 1,
      Self::ScopeReverse => 2,
      Self::Value => 3,
      Self::Posting => 4,
      Self::NvtTile => 5,
    }
  }

  pub fn child_kind(self) -> ImmutableIndexArtifactKindV1 {
    match self {
      Self::ScopeOrdinal | Self::ScopeReverse => ImmutableIndexArtifactKindV1::ScopeCatalogPage,
      Self::Value => ImmutableIndexArtifactKindV1::ValuePage,
      Self::ValueDocumentState | Self::IndexDocumentState => ImmutableIndexArtifactKindV1::DocumentStatePage,
      Self::Posting => ImmutableIndexArtifactKindV1::PostingPage,
      Self::NvtTile => ImmutableIndexArtifactKindV1::NvtTile,
    }
  }

  pub fn uses_page_id(self) -> bool {
    !matches!(self, Self::ScopeOrdinal | Self::ScopeReverse | Self::NvtTile)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalHintV1 {
  pub wal_offset: u64,
  pub total_length: u32,
  pub write_sequence: u64,
}

impl PhysicalHintV1 {
  pub fn is_complete(self) -> bool {
    self.total_length != 0
  }

  pub fn matches(self, wal_offset: u64, total_length: u32, write_sequence: u64) -> bool {
    self.is_complete() && self.wal_offset == wal_offset && self.total_length == total_length && self.write_sequence == write_sequence
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactDirectoryEntryWriteV1<'a> {
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub child_hash: &'a [u8],
  pub child_generation: u64,
  pub live_count: u64,
  pub tombstone_count: u64,
  pub page_count: u64,
  pub logical_bytes: u64,
  pub minimum_page_id: u64,
  pub maximum_page_id: u64,
  pub physical_hint: PhysicalHintV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactDirectoryWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub role: OrderedIndexRoleV1,
  pub owner_id: &'a [u8],
  pub generation: u64,
  pub level: u16,
  pub entries: &'a [ArtifactDirectoryEntryWriteV1<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderedPageWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub role: OrderedIndexRoleV1,
  pub owner_id: &'a [u8],
  pub generation: u64,
  pub page_id: u64,
  pub previous_page_id: u64,
  pub next_page_id: u64,
  pub records: &'a [&'a [u8]],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingRecordV1<'a> {
  pub tombstone: bool,
  pub coordinate: u64,
  pub document_ordinal: u64,
  pub source_value_ordinal: u32,
  pub expansion_ordinal: u32,
  pub posting_key: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDirectoryEntryV1<'a> {
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub child_hash: &'a [u8],
  pub child_generation: u64,
  pub live_count: u64,
  pub tombstone_count: u64,
  pub page_count: u64,
  pub logical_bytes: u64,
  pub minimum_page_id: u64,
  pub maximum_page_id: u64,
  pub physical_hint: PhysicalHintV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDirectoryNodeV1<'a> {
  pub role: OrderedIndexRoleV1,
  pub owner_id: &'a [u8],
  pub generation: u64,
  pub level: u16,
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub live_count: u64,
  pub tombstone_count: u64,
  pub page_count: u64,
  pub logical_bytes: u64,
  pub minimum_page_id: u64,
  pub maximum_page_id: u64,
  pub entries: Vec<ArtifactDirectoryEntryV1<'a>>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedRecordV1<'a> {
  pub encoded: &'a [u8],
  pub tombstone: bool,
  pub coordinate: u64,
  pub document_ordinal: u64,
  pub file_key: Option<&'a [u8]>,
  sort_key: RecordSortKeyV1<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedRecordsV1<'a> {
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  bytes: &'a [u8],
  count: u32,
}

impl<'a> OrderedRecordsV1<'a> {
  pub fn len(&self) -> usize {
    self.count as usize
  }

  pub fn is_empty(&self) -> bool {
    self.count == 0
  }

  pub fn iter(&self) -> OrderedRecordIteratorV1<'a> {
    OrderedRecordIteratorV1 {
      hash_algorithm: self.hash_algorithm,
      role: self.role,
      bytes: self.bytes,
      cursor: 0,
      remaining: self.count,
      failed: false,
    }
  }
}

pub struct OrderedRecordIteratorV1<'a> {
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  bytes: &'a [u8],
  cursor: usize,
  remaining: u32,
  failed: bool,
}

impl<'a> Iterator for OrderedRecordIteratorV1<'a> {
  type Item = FormatResult<OrderedRecordV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed || self.remaining == 0 {
      return None;
    }
    self.remaining -= 1;
    let decoded = decode_record(self.hash_algorithm, self.role, self.bytes, &mut self.cursor);
    if decoded.is_err() {
      self.failed = true;
    }
    Some(decoded)
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    let remaining = if self.failed { 0 } else { self.remaining as usize };
    (remaining, Some(remaining))
  }
}

impl ExactSizeIterator for OrderedRecordIteratorV1<'_> {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordSortKeyV1<'a> {
  Contiguous(&'a [u8]),
  Posting { coordinate: u64, key: &'a [u8], document_ordinal: u64, suffix: &'a [u8] },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedPageV1<'a> {
  pub role: OrderedIndexRoleV1,
  pub owner_id: &'a [u8],
  pub generation: u64,
  pub page_id: u64,
  pub previous_page_id: u64,
  pub next_page_id: u64,
  pub lower_fence: &'a [u8],
  pub upper_fence: &'a [u8],
  pub live_count: u32,
  pub tombstone_count: u32,
  pub logical_live_bytes: u64,
  pub minimum_coordinate: u64,
  pub maximum_coordinate: u64,
  pub records: OrderedRecordsV1<'a>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderedIndexArtifactV1<'a> {
  Directory(ArtifactDirectoryNodeV1<'a>),
  Page(OrderedPageV1<'a>),
}

pub fn decode_ordered_index_artifact(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<OrderedIndexArtifactV1<'_>> {
  if value.len() > MAX_ARTIFACT_LENGTH {
    return Err(amplification_error(format!("{} bytes exceeds the {MAX_ARTIFACT_LENGTH}-byte artifact cap", value.len())));
  }
  match u16_at(value, 6)? {
    DIRECTORY_KIND => decode_artifact_directory(value, hash_algorithm).map(OrderedIndexArtifactV1::Directory),
    POSTING_PAGE_KIND | VALUE_PAGE_KIND | SCOPE_PAGE_KIND | STATE_PAGE_KIND => {
      decode_ordered_page(value, hash_algorithm).map(OrderedIndexArtifactV1::Page)
    }
    kind => Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "ordered_index_artifact_kind",
      format!("unsupported ordered artifact kind 0x{kind:04x}"),
    )),
  }
}

pub fn decode_artifact_directory(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<ArtifactDirectoryNodeV1<'_>> {
  let artifact = decode_immutable_index_artifact(value, hash_algorithm, MAX_ARTIFACT_LENGTH)?;
  if artifact.kind != DIRECTORY_KIND {
    return Err(kind_error("artifact is not an ArtifactDirectoryNodeV1"));
  }
  let hash_width = hash_algorithm.hash_length();
  if artifact.identity.len() != hash_width + 2 {
    return Err(closure_error("directory identity must contain owner ID, owner class, and role"));
  }
  let owner_id = &artifact.identity[..hash_width];
  if owner_id.iter().all(|byte| *byte == 0) {
    return Err(identity_error("directory owner ID is all zero"));
  }
  let role = OrderedIndexRoleV1::from_id(artifact.identity[hash_width + 1]).ok_or_else(|| kind_error("directory role is unknown"))?;
  if artifact.identity[hash_width] != role.owner_class() {
    return Err(closure_error("directory owner class disagrees with its role"));
  }
  let body = artifact.body;
  if body.len() < 80 {
    return Err(truncated_error("directory body is shorter than its fixed header"));
  }
  let level = u16_at(body, 0)?;
  let key_codec = u16_at(body, 2)?;
  let entry_count = u32_at(body, 4)?;
  let lower_length = checked_usize_from_u32(u32_at(body, 16)?, "directory lower-fence length")?;
  let upper_length = checked_usize_from_u32(u32_at(body, 20)?, "directory upper-fence length")?;
  let entries_length = checked_usize_from_u32(u32_at(body, 72)?, "directory entry length")?;
  if level > 15 {
    return Err(amplification_error("directory level exceeds 15"));
  }
  if key_codec != role.key_codec() {
    return Err(closure_error("directory key codec disagrees with its role"));
  }
  if entry_count == 0 || entry_count > MAX_DIRECTORY_ENTRIES {
    return Err(amplification_error("directory entry count is outside 1..=65536"));
  }
  if u32_at(body, 8)? != 0 || u32_at(body, 12)? != 0 || u32_at(body, 76)? != 0 {
    return Err(reserve_error("directory flags or reserves are nonzero"));
  }
  validate_key_length(lower_length)?;
  validate_key_length(upper_length)?;
  let expected_length = 80usize
    .checked_add(lower_length)
    .and_then(|length| length.checked_add(upper_length))
    .and_then(|length| length.checked_add(entries_length))
    .ok_or_else(|| length_error("directory body length overflow"))?;
  if expected_length != body.len() {
    return Err(truncated_error("directory fences and entries do not consume the body"));
  }
  let lower_end = 80 + lower_length;
  let upper_end = lower_end + upper_length;
  let lower_fence = &body[80..lower_end];
  let upper_fence = &body[lower_end..upper_end];
  validate_order_key(hash_algorithm, role, lower_fence)?;
  validate_order_key(hash_algorithm, role, upper_fence)?;
  if compare_order_keys(hash_algorithm, role, lower_fence, upper_fence)? == Ordering::Greater {
    return Err(order_error("directory lower fence sorts after its upper fence"));
  }

  let entries_end = upper_end + entries_length;
  let mut cursor = upper_end;
  let mut entries: Vec<ArtifactDirectoryEntryV1<'_>> = Vec::new();
  for _ in 0..entry_count {
    let entry = if level == 0 {
      decode_leaf_descriptor(hash_algorithm, role, artifact.generation, body, &mut cursor, entries_end)?
    } else {
      decode_internal_descriptor(hash_algorithm, role, artifact.generation, body, &mut cursor, entries_end)?
    };
    if let Some(previous) = entries.last() {
      if compare_order_keys(hash_algorithm, role, previous.upper_fence, entry.lower_fence)? != Ordering::Less {
        return Err(order_error("directory child ranges overlap or are not strictly ordered"));
      }
    }
    entries.push(entry);
  }
  if cursor != entries_end
    || entries.first().map(|entry| entry.lower_fence) != Some(lower_fence)
    || entries.last().map(|entry| entry.upper_fence) != Some(upper_fence)
  {
    return Err(closure_error("directory descriptors do not consume their area or match outer fences"));
  }

  let live_count = checked_sum(entries.iter().map(|entry| entry.live_count), "directory live-count overflow")?;
  let tombstone_count = checked_sum(entries.iter().map(|entry| entry.tombstone_count), "directory tombstone-count overflow")?;
  let page_count = checked_sum(entries.iter().map(|entry| entry.page_count), "directory page-count overflow")?;
  let logical_bytes = checked_sum(entries.iter().map(|entry| entry.logical_bytes), "directory logical-byte overflow")?;
  let minimum_page_id = entries.iter().map(|entry| entry.minimum_page_id).min().ok_or_else(|| closure_error("directory is empty"))?;
  let maximum_page_id = entries.iter().map(|entry| entry.maximum_page_id).max().ok_or_else(|| closure_error("directory is empty"))?;
  if u64_at(body, 24)? != live_count
    || u64_at(body, 32)? != tombstone_count
    || u64_at(body, 40)? != page_count
    || u64_at(body, 48)? != logical_bytes
    || u64_at(body, 56)? != minimum_page_id
    || u64_at(body, 64)? != maximum_page_id
    || page_count == 0
  {
    return Err(closure_error("directory aggregate fields disagree with its descriptors"));
  }

  Ok(ArtifactDirectoryNodeV1 {
    role,
    owner_id,
    generation: artifact.generation,
    level,
    lower_fence,
    upper_fence,
    live_count,
    tombstone_count,
    page_count,
    logical_bytes,
    minimum_page_id,
    maximum_page_id,
    entries,
    key: artifact.key,
  })
}

fn decode_leaf_descriptor<'a>(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  parent_generation: u64,
  body: &'a [u8],
  cursor: &mut usize,
  end: usize,
) -> FormatResult<ArtifactDirectoryEntryV1<'a>> {
  let hash_width = hash_algorithm.hash_length();
  let fixed = 72usize.checked_add(hash_width).ok_or_else(|| length_error("leaf descriptor fixed length overflow"))?;
  let start = *cursor;
  if start.checked_add(fixed).is_none_or(|next| next > end) {
    return Err(truncated_error("leaf directory descriptor is truncated"));
  }
  let lower_length = checked_usize_from_u32(u32_at(body, start)?, "leaf lower-fence length")?;
  let upper_length = checked_usize_from_u32(u32_at(body, start + 4)?, "leaf upper-fence length")?;
  validate_key_length(lower_length)?;
  validate_key_length(upper_length)?;
  let page_id = u64_at(body, start + 8)?;
  let child_hash = &body[start + 16..start + 16 + hash_width];
  let fields = start + 16 + hash_width;
  let child_generation = u64_at(body, fields)?;
  let live_count = u64_at(body, fields + 8)?;
  let tombstone_count = u64_at(body, fields + 16)?;
  let logical_bytes = u64_at(body, fields + 24)?;
  let physical_hint = decode_physical_hint(body, fields + 32)?;
  let key_start = start + fixed;
  let next = key_start
    .checked_add(lower_length)
    .and_then(|value| value.checked_add(upper_length))
    .ok_or_else(|| length_error("leaf descriptor fence length overflow"))?;
  if next > end {
    return Err(truncated_error("leaf descriptor fences are truncated"));
  }
  if child_hash.iter().all(|byte| *byte == 0)
    || child_generation == 0
    || child_generation > parent_generation
    || live_count.checked_add(tombstone_count).is_none_or(|count| count == 0)
    || (logical_bytes == 0) != (live_count == 0)
  {
    return Err(closure_error("leaf descriptor child identity, generation, counts, or size is invalid"));
  }
  if role.uses_page_id() == (page_id == 0) {
    return Err(closure_error("leaf descriptor page ID presence disagrees with its role"));
  }
  let lower_fence = &body[key_start..key_start + lower_length];
  let upper_fence = &body[key_start + lower_length..next];
  validate_descriptor_fences(hash_algorithm, role, lower_fence, upper_fence)?;
  *cursor = next;
  Ok(ArtifactDirectoryEntryV1 {
    lower_fence,
    upper_fence,
    child_hash,
    child_generation,
    live_count,
    tombstone_count,
    page_count: 1,
    logical_bytes,
    minimum_page_id: page_id,
    maximum_page_id: page_id,
    physical_hint,
  })
}

fn decode_internal_descriptor<'a>(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  parent_generation: u64,
  body: &'a [u8],
  cursor: &mut usize,
  end: usize,
) -> FormatResult<ArtifactDirectoryEntryV1<'a>> {
  let hash_width = hash_algorithm.hash_length();
  let fixed = 88usize.checked_add(hash_width).ok_or_else(|| length_error("internal descriptor fixed length overflow"))?;
  let start = *cursor;
  if start.checked_add(fixed).is_none_or(|next| next > end) {
    return Err(truncated_error("internal directory descriptor is truncated"));
  }
  let lower_length = checked_usize_from_u32(u32_at(body, start)?, "internal lower-fence length")?;
  let upper_length = checked_usize_from_u32(u32_at(body, start + 4)?, "internal upper-fence length")?;
  validate_key_length(lower_length)?;
  validate_key_length(upper_length)?;
  let child_hash = &body[start + 8..start + 8 + hash_width];
  let fields = start + 8 + hash_width;
  let child_generation = u64_at(body, fields)?;
  let live_count = u64_at(body, fields + 8)?;
  let tombstone_count = u64_at(body, fields + 16)?;
  let page_count = u64_at(body, fields + 24)?;
  let logical_bytes = u64_at(body, fields + 32)?;
  let minimum_page_id = u64_at(body, fields + 40)?;
  let maximum_page_id = u64_at(body, fields + 48)?;
  let physical_hint = decode_physical_hint(body, fields + 56)?;
  let key_start = start + fixed;
  let next = key_start
    .checked_add(lower_length)
    .and_then(|value| value.checked_add(upper_length))
    .ok_or_else(|| length_error("internal descriptor fence length overflow"))?;
  if next > end {
    return Err(truncated_error("internal descriptor fences are truncated"));
  }
  if child_hash.iter().all(|byte| *byte == 0)
    || child_generation == 0
    || child_generation > parent_generation
    || live_count.checked_add(tombstone_count).is_none_or(|count| count == 0)
    || page_count == 0
    || (logical_bytes == 0) != (live_count == 0)
    || minimum_page_id > maximum_page_id
    || (role.uses_page_id() && minimum_page_id == 0)
    || (!role.uses_page_id() && (minimum_page_id != 0 || maximum_page_id != 0))
  {
    return Err(closure_error("internal descriptor child identity, generation, counts, or page range is invalid"));
  }
  let lower_fence = &body[key_start..key_start + lower_length];
  let upper_fence = &body[key_start + lower_length..next];
  validate_descriptor_fences(hash_algorithm, role, lower_fence, upper_fence)?;
  *cursor = next;
  Ok(ArtifactDirectoryEntryV1 {
    lower_fence,
    upper_fence,
    child_hash,
    child_generation,
    live_count,
    tombstone_count,
    page_count,
    logical_bytes,
    minimum_page_id,
    maximum_page_id,
    physical_hint,
  })
}

fn decode_physical_hint(body: &[u8], offset: usize) -> FormatResult<PhysicalHintV1> {
  let wal_offset = u64_at(body, offset)?;
  let total_length = u32_at(body, offset + 8)?;
  if u32_at(body, offset + 12)? != 0 {
    return Err(reserve_error("physical-hint reserve is nonzero"));
  }
  let write_sequence = u64_at(body, offset + 16)?;
  Ok(PhysicalHintV1 { wal_offset, total_length, write_sequence })
}

pub fn encode_artifact_directory(request: &ArtifactDirectoryWriteV1<'_>) -> FormatResult<EncodedImmutableIndexArtifactV1> {
  validate_owner(request.owner_id, request.hash_algorithm, "directory owner")?;
  if request.generation == 0 {
    return Err(identity_error("directory generation is zero"));
  }
  if request.level > 15 {
    return Err(amplification_error("directory level exceeds 15"));
  }
  if request.entries.is_empty() || request.entries.len() > MAX_DIRECTORY_ENTRIES as usize {
    return Err(amplification_error("directory entry count is outside 1..=65536"));
  }

  let hash_width = request.hash_algorithm.hash_length();
  let descriptor_fixed_length = if request.level == 0 { 72usize.checked_add(hash_width) } else { 88usize.checked_add(hash_width) }
    .ok_or_else(|| length_error("directory descriptor fixed length overflow"))?;
  let mut entries_length = 0usize;
  let mut live_count = 0u64;
  let mut tombstone_count = 0u64;
  let mut page_count = 0u64;
  let mut logical_bytes = 0u64;
  let mut minimum_page_id = request.entries[0].minimum_page_id;
  let mut maximum_page_id = request.entries[0].maximum_page_id;
  let mut previous_upper_fence = None;
  for entry in request.entries {
    validate_directory_write_entry(request, entry)?;
    if let Some(previous_upper_fence) = previous_upper_fence {
      if compare_order_keys(request.hash_algorithm, request.role, previous_upper_fence, entry.lower_fence)? != Ordering::Less {
        return Err(order_error("directory child ranges overlap or are not strictly ordered"));
      }
    }
    previous_upper_fence = Some(entry.upper_fence);
    let descriptor_length = descriptor_fixed_length
      .checked_add(entry.lower_fence.len())
      .and_then(|length| length.checked_add(entry.upper_fence.len()))
      .ok_or_else(|| length_error("directory descriptor length overflow"))?;
    entries_length = entries_length.checked_add(descriptor_length).ok_or_else(|| length_error("directory entries length overflow"))?;
    live_count = live_count.checked_add(entry.live_count).ok_or_else(|| length_error("directory live-count overflow"))?;
    tombstone_count =
      tombstone_count.checked_add(entry.tombstone_count).ok_or_else(|| length_error("directory tombstone-count overflow"))?;
    page_count = page_count.checked_add(entry.page_count).ok_or_else(|| length_error("directory page-count overflow"))?;
    logical_bytes = logical_bytes.checked_add(entry.logical_bytes).ok_or_else(|| length_error("directory logical-byte overflow"))?;
    minimum_page_id = minimum_page_id.min(entry.minimum_page_id);
    maximum_page_id = maximum_page_id.max(entry.maximum_page_id);
  }

  let lower_fence = request.entries[0].lower_fence;
  let upper_fence = request.entries[request.entries.len() - 1].upper_fence;
  let body_length = 80usize
    .checked_add(lower_fence.len())
    .and_then(|length| length.checked_add(upper_fence.len()))
    .and_then(|length| length.checked_add(entries_length))
    .ok_or_else(|| length_error("directory body length overflow"))?;
  let identity_length = hash_width.checked_add(2).ok_or_else(|| length_error("directory identity length overflow"))?;
  checked_immutable_index_artifact_encoded_length(ImmutableIndexArtifactKindV1::ArtifactDirectoryNode, identity_length, body_length)?;

  let mut identity = allocate_zeroed(identity_length, "directory identity")?;
  identity[..hash_width].copy_from_slice(request.owner_id);
  identity[hash_width] = request.role.owner_class();
  identity[hash_width + 1] = request.role.id();

  let mut body = allocate_zeroed(body_length, "directory body")?;
  write_u16(&mut body, 0, request.level)?;
  write_u16(&mut body, 2, request.role.key_codec())?;
  write_u32(&mut body, 4, checked_u32(request.entries.len(), "directory entry count")?)?;
  write_u32(&mut body, 16, checked_u32(lower_fence.len(), "directory lower-fence length")?)?;
  write_u32(&mut body, 20, checked_u32(upper_fence.len(), "directory upper-fence length")?)?;
  write_u64(&mut body, 24, live_count)?;
  write_u64(&mut body, 32, tombstone_count)?;
  write_u64(&mut body, 40, page_count)?;
  write_u64(&mut body, 48, logical_bytes)?;
  write_u64(&mut body, 56, minimum_page_id)?;
  write_u64(&mut body, 64, maximum_page_id)?;
  write_u32(&mut body, 72, checked_u32(entries_length, "directory entries length")?)?;
  let mut cursor = 80usize;
  write_bytes(&mut body, cursor, lower_fence, "directory lower fence")?;
  cursor += lower_fence.len();
  write_bytes(&mut body, cursor, upper_fence, "directory upper fence")?;
  cursor += upper_fence.len();
  for entry in request.entries {
    cursor = encode_directory_descriptor(&mut body, cursor, hash_width, request.level, entry)?;
  }
  if cursor != body.len() {
    return Err(closure_error("encoded directory descriptors do not consume the body"));
  }

  let encoded = encode_immutable_index_artifact(&ImmutableIndexArtifactWriteV1 {
    kind: ImmutableIndexArtifactKindV1::ArtifactDirectoryNode,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  decode_artifact_directory(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

fn validate_directory_write_entry(request: &ArtifactDirectoryWriteV1<'_>, entry: &ArtifactDirectoryEntryWriteV1<'_>) -> FormatResult<()> {
  validate_key_length(entry.lower_fence.len())?;
  validate_key_length(entry.upper_fence.len())?;
  validate_descriptor_fences(request.hash_algorithm, request.role, entry.lower_fence, entry.upper_fence)?;
  validate_hash(entry.child_hash, request.hash_algorithm, "directory child hash")?;
  if entry.child_generation == 0 || entry.child_generation > request.generation {
    return Err(closure_error("directory child generation is zero or newer than its parent"));
  }
  if entry.live_count.checked_add(entry.tombstone_count).is_none_or(|count| count == 0)
    || (entry.logical_bytes == 0) != (entry.live_count == 0)
  {
    return Err(closure_error("directory child counts or logical size are invalid"));
  }
  if request.level == 0 {
    if entry.page_count != 1 || entry.minimum_page_id != entry.maximum_page_id {
      return Err(closure_error("leaf descriptor must describe exactly one page identity"));
    }
  } else if entry.page_count == 0 || entry.minimum_page_id > entry.maximum_page_id {
    return Err(closure_error("internal descriptor page count or range is invalid"));
  }
  if request.role.uses_page_id() {
    if entry.minimum_page_id == 0 {
      return Err(closure_error("directory role requires nonzero page IDs"));
    }
  } else if entry.minimum_page_id != 0 || entry.maximum_page_id != 0 {
    return Err(closure_error("directory role forbids page IDs"));
  }
  Ok(())
}

fn encode_directory_descriptor(
  body: &mut [u8],
  start: usize,
  hash_width: usize,
  level: u16,
  entry: &ArtifactDirectoryEntryWriteV1<'_>,
) -> FormatResult<usize> {
  write_u32(body, start, checked_u32(entry.lower_fence.len(), "descriptor lower-fence length")?)?;
  write_u32(body, start + 4, checked_u32(entry.upper_fence.len(), "descriptor upper-fence length")?)?;
  let fixed_length = if level == 0 {
    write_u64(body, start + 8, entry.minimum_page_id)?;
    write_bytes(body, start + 16, entry.child_hash, "leaf child hash")?;
    let fields = start + 16 + hash_width;
    write_u64(body, fields, entry.child_generation)?;
    write_u64(body, fields + 8, entry.live_count)?;
    write_u64(body, fields + 16, entry.tombstone_count)?;
    write_u64(body, fields + 24, entry.logical_bytes)?;
    write_physical_hint(body, fields + 32, entry.physical_hint)?;
    72usize.checked_add(hash_width).ok_or_else(|| length_error("leaf descriptor fixed length overflow"))?
  } else {
    write_bytes(body, start + 8, entry.child_hash, "internal child hash")?;
    let fields = start + 8 + hash_width;
    write_u64(body, fields, entry.child_generation)?;
    write_u64(body, fields + 8, entry.live_count)?;
    write_u64(body, fields + 16, entry.tombstone_count)?;
    write_u64(body, fields + 24, entry.page_count)?;
    write_u64(body, fields + 32, entry.logical_bytes)?;
    write_u64(body, fields + 40, entry.minimum_page_id)?;
    write_u64(body, fields + 48, entry.maximum_page_id)?;
    write_physical_hint(body, fields + 56, entry.physical_hint)?;
    88usize.checked_add(hash_width).ok_or_else(|| length_error("internal descriptor fixed length overflow"))?
  };
  let mut cursor = start.checked_add(fixed_length).ok_or_else(|| length_error("directory descriptor cursor overflow"))?;
  write_bytes(body, cursor, entry.lower_fence, "descriptor lower fence")?;
  cursor += entry.lower_fence.len();
  write_bytes(body, cursor, entry.upper_fence, "descriptor upper fence")?;
  cursor += entry.upper_fence.len();
  Ok(cursor)
}

fn write_physical_hint(body: &mut [u8], offset: usize, hint: PhysicalHintV1) -> FormatResult<()> {
  write_u64(body, offset, hint.wal_offset)?;
  write_u32(body, offset + 8, hint.total_length)?;
  write_u64(body, offset + 16, hint.write_sequence)
}

pub fn encode_ordered_page(request: &OrderedPageWriteV1<'_>) -> FormatResult<EncodedImmutableIndexArtifactV1> {
  let prepared = prepare_ordered_page(request)?;
  let scan = &prepared.scan;
  let lower_fence = &prepared.lower_fence;
  let upper_fence = &prepared.upper_fence;
  let kind = prepared.kind;
  let identity_length = prepared.identity_length;
  let body_length = prepared.body_length;

  let identity = encode_page_identity(request, lower_fence, identity_length)?;
  let mut body = allocate_zeroed(body_length, "ordered-page body")?;
  write_u16(&mut body, 4, 1)?;
  write_u16(&mut body, 6, request.role.key_codec())?;
  write_u64(&mut body, 8, request.previous_page_id)?;
  write_u64(&mut body, 16, request.next_page_id)?;
  write_u32(&mut body, 24, checked_u32(lower_fence.len(), "ordered-page lower-fence length")?)?;
  write_u32(&mut body, 28, checked_u32(upper_fence.len(), "ordered-page upper-fence length")?)?;
  write_u32(&mut body, 32, scan.record_count)?;
  write_u32(&mut body, 36, scan.live_count)?;
  write_u32(&mut body, 40, scan.record_count - scan.live_count)?;
  write_u64(
    &mut body,
    48,
    u64::try_from(scan.records_length).map_err(|source| length_error(format!("ordered-page record bytes do not fit u64: {source}")))?,
  )?;
  write_u64(&mut body, 56, scan.logical_live_bytes)?;
  if request.role == OrderedIndexRoleV1::Posting {
    write_u64(&mut body, 64, scan.first.coordinate)?;
    write_u64(&mut body, 72, scan.last.coordinate)?;
  }
  let mut cursor = 96usize;
  write_bytes(&mut body, cursor, lower_fence, "ordered-page lower fence")?;
  cursor += lower_fence.len();
  write_bytes(&mut body, cursor, upper_fence, "ordered-page upper fence")?;
  cursor += upper_fence.len();
  for record in request.records {
    write_bytes(&mut body, cursor, record, "ordered-page record")?;
    cursor += record.len();
  }
  if cursor != body.len() {
    return Err(closure_error("encoded ordered-page records do not consume the body"));
  }

  let encoded = encode_immutable_index_artifact(&ImmutableIndexArtifactWriteV1 {
    kind,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  decode_ordered_page(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

/// Return the exact encoded page length after running the same validation and
/// sizing path as [`encode_ordered_page`], without allocating an artifact.
pub fn checked_ordered_page_encoded_length(request: &OrderedPageWriteV1<'_>) -> FormatResult<usize> {
  Ok(prepare_ordered_page(request)?.encoded_length)
}

fn prepare_ordered_page<'a>(request: &OrderedPageWriteV1<'a>) -> FormatResult<PreparedOrderedPageV1<'a>> {
  validate_owner(request.owner_id, request.hash_algorithm, "ordered-page owner")?;
  if request.generation == 0 {
    return Err(identity_error("ordered-page generation is zero"));
  }
  validate_page_identity_and_links(request)?;
  let scan = scan_record_slices(request.hash_algorithm, request.role, request.records)?;
  let lower_fence = record_order_key(&scan.first)?;
  let upper_fence = record_order_key(&scan.last)?;
  validate_descriptor_fences(request.hash_algorithm, request.role, &lower_fence, &upper_fence)?;

  let kind = page_kind(request.role)?;
  let identity_length = page_identity_length(request.role, request.hash_algorithm.hash_length(), lower_fence.len())?;
  let body_length = 96usize
    .checked_add(lower_fence.len())
    .and_then(|length| length.checked_add(upper_fence.len()))
    .and_then(|length| length.checked_add(scan.records_length))
    .ok_or_else(|| length_error("ordered-page body length overflow"))?;
  let encoded_length = checked_immutable_index_artifact_encoded_length(kind, identity_length, body_length)?;
  Ok(PreparedOrderedPageV1 { scan, lower_fence, upper_fence, kind, identity_length, body_length, encoded_length })
}

fn validate_page_identity_and_links(request: &OrderedPageWriteV1<'_>) -> FormatResult<()> {
  match request.role {
    OrderedIndexRoleV1::ScopeOrdinal | OrderedIndexRoleV1::ScopeReverse => {
      if request.page_id != 0 {
        return Err(identity_error("scope-catalog pages must use page ID zero"));
      }
    }
    OrderedIndexRoleV1::Value
    | OrderedIndexRoleV1::ValueDocumentState
    | OrderedIndexRoleV1::Posting
    | OrderedIndexRoleV1::IndexDocumentState => {
      if request.page_id == 0 {
        return Err(identity_error("ordered-page role requires a nonzero page ID"));
      }
    }
    OrderedIndexRoleV1::NvtTile => return Err(kind_error("NVT tiles are not ordered pages")),
  }
  if request.role != OrderedIndexRoleV1::Posting && (request.previous_page_id != 0 || request.next_page_id != 0) {
    return Err(reserve_error("non-posting page links are nonzero"));
  }
  if request.role == OrderedIndexRoleV1::Posting && (request.previous_page_id == request.page_id || request.next_page_id == request.page_id)
  {
    return Err(closure_error("posting page links form a self-cycle"));
  }
  Ok(())
}

fn page_kind(role: OrderedIndexRoleV1) -> FormatResult<ImmutableIndexArtifactKindV1> {
  if role == OrderedIndexRoleV1::NvtTile {
    return Err(kind_error("NVT tiles are not ordered pages"));
  }
  Ok(role.child_kind())
}

fn page_identity_length(role: OrderedIndexRoleV1, hash_width: usize, lower_fence_length: usize) -> FormatResult<usize> {
  match role {
    OrderedIndexRoleV1::ScopeOrdinal | OrderedIndexRoleV1::ScopeReverse => hash_width
      .checked_add(1)
      .and_then(|length| length.checked_add(lower_fence_length))
      .ok_or_else(|| length_error("scope-page identity length overflow")),
    OrderedIndexRoleV1::Value | OrderedIndexRoleV1::Posting => {
      hash_width.checked_add(8).ok_or_else(|| length_error("ID-page identity length overflow"))
    }
    OrderedIndexRoleV1::ValueDocumentState | OrderedIndexRoleV1::IndexDocumentState => {
      hash_width.checked_add(16).ok_or_else(|| length_error("state-page identity length overflow"))
    }
    OrderedIndexRoleV1::NvtTile => Err(kind_error("NVT tiles are not ordered pages")),
  }
}

fn encode_page_identity(request: &OrderedPageWriteV1<'_>, lower_fence: &[u8], length: usize) -> FormatResult<Vec<u8>> {
  let hash_width = request.hash_algorithm.hash_length();
  let mut identity = allocate_zeroed(length, "ordered-page identity")?;
  identity[..hash_width].copy_from_slice(request.owner_id);
  match request.role {
    OrderedIndexRoleV1::ScopeOrdinal | OrderedIndexRoleV1::ScopeReverse => {
      identity[hash_width] = request.role.id();
      write_bytes(&mut identity, hash_width + 1, lower_fence, "scope-page identity fence")?;
    }
    OrderedIndexRoleV1::Value | OrderedIndexRoleV1::Posting => write_u64(&mut identity, hash_width, request.page_id)?,
    OrderedIndexRoleV1::ValueDocumentState | OrderedIndexRoleV1::IndexDocumentState => {
      identity[hash_width] = request.role.owner_class();
      write_u64(&mut identity, hash_width + 8, request.page_id)?;
    }
    OrderedIndexRoleV1::NvtTile => return Err(kind_error("NVT tiles are not ordered pages")),
  }
  Ok(identity)
}

struct RecordSliceScanV1<'a> {
  first: OrderedRecordV1<'a>,
  last: OrderedRecordV1<'a>,
  record_count: u32,
  live_count: u32,
  records_length: usize,
  logical_live_bytes: u64,
}

struct PreparedOrderedPageV1<'a> {
  scan: RecordSliceScanV1<'a>,
  lower_fence: Vec<u8>,
  upper_fence: Vec<u8>,
  kind: ImmutableIndexArtifactKindV1,
  identity_length: usize,
  body_length: usize,
  encoded_length: usize,
}

fn scan_record_slices<'a>(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  records: &[&'a [u8]],
) -> FormatResult<RecordSliceScanV1<'a>> {
  if records.is_empty() {
    return Err(closure_error("ordered page has no records"));
  }
  let record_count = checked_u32(records.len(), "ordered-page record count")?;
  let mut first = None;
  let mut previous: Option<OrderedRecordV1<'a>> = None;
  let mut live_count = 0u32;
  let mut records_length = 0usize;
  let mut logical_live_bytes = 0u64;
  for encoded in records {
    let mut cursor = 0usize;
    let record = decode_record(hash_algorithm, role, encoded, &mut cursor)?;
    if cursor != encoded.len() {
      return Err(truncated_error("one ordered-page record contains trailing bytes"));
    }
    if let Some(previous) = &previous {
      if compare_records(hash_algorithm, role, previous, &record)? != Ordering::Less {
        return Err(order_error("ordered-page records are not strictly ordered"));
      }
    }
    records_length = records_length.checked_add(encoded.len()).ok_or_else(|| length_error("ordered-page record length overflow"))?;
    if !record.tombstone {
      live_count = live_count.checked_add(1).ok_or_else(|| length_error("ordered-page live count overflow"))?;
      let encoded_length =
        u64::try_from(encoded.len()).map_err(|source| length_error(format!("ordered-page record length does not fit u64: {source}")))?;
      logical_live_bytes =
        logical_live_bytes.checked_add(encoded_length).ok_or_else(|| length_error("ordered-page logical-byte overflow"))?;
    }
    if first.is_none() {
      first = Some(record.clone());
    }
    previous = Some(record);
  }
  Ok(RecordSliceScanV1 {
    first: first.ok_or_else(|| closure_error("ordered page has no first record"))?,
    last: previous.ok_or_else(|| closure_error("ordered page has no last record"))?,
    record_count,
    live_count,
    records_length,
    logical_live_bytes,
  })
}

fn record_order_key(record: &OrderedRecordV1<'_>) -> FormatResult<Vec<u8>> {
  match &record.sort_key {
    RecordSortKeyV1::Contiguous(key) => copy_fallible(key, "ordered record key"),
    RecordSortKeyV1::Posting { coordinate, key, document_ordinal, suffix } => {
      let length = 24usize.checked_add(key.len()).ok_or_else(|| length_error("posting order-key length overflow"))?;
      validate_key_length(length)?;
      let mut encoded = allocate_zeroed(length, "posting order key")?;
      write_u64(&mut encoded, 0, *coordinate)?;
      write_bytes(&mut encoded, 8, key, "posting order key")?;
      write_u64(&mut encoded, 8 + key.len(), *document_ordinal)?;
      write_bytes(&mut encoded, 16 + key.len(), suffix, "posting order suffix")?;
      Ok(encoded)
    }
  }
}

/// Return the canonical order-key length without allocating the key.
pub fn checked_ordered_record_order_key_length(record: &OrderedRecordV1<'_>) -> FormatResult<usize> {
  match &record.sort_key {
    RecordSortKeyV1::Contiguous(key) => Ok(key.len()),
    RecordSortKeyV1::Posting { key, .. } => {
      let length = 24usize.checked_add(key.len()).ok_or_else(|| length_error("posting order-key length overflow"))?;
      validate_key_length(length)?;
      Ok(length)
    }
  }
}

/// Decode exactly one role-specific ordered record.
pub fn decode_ordered_record(value: &[u8], hash_algorithm: HashAlgorithm, role: OrderedIndexRoleV1) -> FormatResult<OrderedRecordV1<'_>> {
  let mut cursor = 0usize;
  let record = decode_record(hash_algorithm, role, value, &mut cursor)?;
  if cursor != value.len() {
    return Err(truncated_error("ordered record contains trailing bytes"));
  }
  Ok(record)
}

/// Materialize the canonical order key owned by a decoded ordered record.
pub fn ordered_record_order_key(record: &OrderedRecordV1<'_>) -> FormatResult<Vec<u8>> {
  record_order_key(record)
}

pub fn validate_posting_page_link(left: &OrderedPageV1<'_>, right: &OrderedPageV1<'_>, hash_algorithm: HashAlgorithm) -> FormatResult<()> {
  if left.role != OrderedIndexRoleV1::Posting || right.role != OrderedIndexRoleV1::Posting {
    return Err(closure_error("adjacent posting-link validation received a non-posting page"));
  }
  validate_owner(left.owner_id, hash_algorithm, "left posting-page owner")?;
  validate_owner(right.owner_id, hash_algorithm, "right posting-page owner")?;
  if left.owner_id != right.owner_id
    || left.page_id == right.page_id
    || left.next_page_id != right.page_id
    || right.previous_page_id != left.page_id
  {
    return Err(closure_error("adjacent posting pages disagree on owner, identity, or bidirectional links"));
  }
  if compare_order_keys(hash_algorithm, OrderedIndexRoleV1::Posting, left.upper_fence, right.lower_fence)? != Ordering::Less {
    return Err(order_error("adjacent posting pages overlap or are not strictly ordered"));
  }
  Ok(())
}

pub fn decode_ordered_page(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<OrderedPageV1<'_>> {
  let artifact = decode_immutable_index_artifact(value, hash_algorithm, MAX_ARTIFACT_LENGTH)?;
  let hash_width = hash_algorithm.hash_length();
  let (role, owner_id, page_id) = match artifact.kind {
    POSTING_PAGE_KIND => decode_id_page_identity(artifact.identity, hash_width, OrderedIndexRoleV1::Posting)?,
    VALUE_PAGE_KIND => decode_id_page_identity(artifact.identity, hash_width, OrderedIndexRoleV1::Value)?,
    STATE_PAGE_KIND => decode_state_page_identity(artifact.identity, hash_width)?,
    SCOPE_PAGE_KIND => decode_scope_page_identity(artifact.identity, hash_width)?,
    _ => return Err(kind_error("artifact is not an ordered page")),
  };
  if owner_id.iter().all(|byte| *byte == 0) {
    return Err(identity_error("ordered-page owner ID is all zero"));
  }
  let body = artifact.body;
  if body.len() < 96 {
    return Err(truncated_error("ordered-page body is shorter than its fixed header"));
  }
  let lower_length = checked_usize_from_u32(u32_at(body, 24)?, "page lower-fence length")?;
  let upper_length = checked_usize_from_u32(u32_at(body, 28)?, "page upper-fence length")?;
  let record_count = u32_at(body, 32)?;
  let live_count = u32_at(body, 36)?;
  let tombstone_count = u32_at(body, 40)?;
  let records_length = checked_usize_from_u64(u64_at(body, 48)?, "page record length")?;
  if u32_at(body, 0)? != 0 || body[80..96].iter().any(|byte| *byte != 0) {
    return Err(reserve_error("ordered-page flags or reserves are nonzero"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "ordered_page_version", "ordered-page body version is not 1"));
  }
  if u16_at(body, 6)? != role.key_codec() {
    return Err(closure_error("ordered-page key codec disagrees with its role"));
  }
  let previous_page_id = u64_at(body, 8)?;
  let next_page_id = u64_at(body, 16)?;
  if role != OrderedIndexRoleV1::Posting && (previous_page_id != 0 || next_page_id != 0) {
    return Err(reserve_error("non-posting page links are nonzero"));
  }
  if role == OrderedIndexRoleV1::Posting && (previous_page_id == page_id || next_page_id == page_id) {
    return Err(closure_error("posting page links form a self-cycle"));
  }
  validate_key_length(lower_length)?;
  validate_key_length(upper_length)?;
  if record_count == 0 || live_count.checked_add(tombstone_count) != Some(record_count) {
    return Err(closure_error("ordered-page record counts are empty, overflowed, or inconsistent"));
  }
  if u32_at(body, 44)? != 0 {
    return Err(reserve_error("ordered-page count reserve is nonzero"));
  }
  let expected_length = 96usize
    .checked_add(lower_length)
    .and_then(|length| length.checked_add(upper_length))
    .and_then(|length| length.checked_add(records_length))
    .ok_or_else(|| length_error("ordered-page body length overflow"))?;
  if expected_length != body.len() {
    return Err(truncated_error("ordered-page fences and records do not consume the body"));
  }
  let lower_end = 96 + lower_length;
  let upper_end = lower_end + upper_length;
  let lower_fence = &body[96..lower_end];
  let upper_fence = &body[lower_end..upper_end];
  validate_descriptor_fences(hash_algorithm, role, lower_fence, upper_fence)?;
  let record_bytes = &body[upper_end..];
  let scanned = scan_records(hash_algorithm, role, record_bytes, record_count)?;
  if !record_matches_fence(hash_algorithm, role, &scanned.first, lower_fence)?
    || !record_matches_fence(hash_algorithm, role, &scanned.last, upper_fence)?
  {
    return Err(closure_error("ordered-page record bounds disagree with its fences"));
  }
  if scanned.live_count != live_count
    || record_count - scanned.live_count != tombstone_count
    || u64_at(body, 56)? != scanned.logical_live_bytes
  {
    return Err(closure_error("ordered-page aggregate counts disagree with decoded records"));
  }
  let minimum_coordinate = u64_at(body, 64)?;
  let maximum_coordinate = u64_at(body, 72)?;
  if role == OrderedIndexRoleV1::Posting {
    if minimum_coordinate != scanned.first.coordinate
      || maximum_coordinate != scanned.last.coordinate
      || minimum_coordinate > maximum_coordinate
    {
      return Err(closure_error("posting-page coordinate bounds disagree with its records"));
    }
  } else if minimum_coordinate != 0 || maximum_coordinate != 0 {
    return Err(reserve_error("non-posting page coordinate bounds are nonzero"));
  }
  if artifact.kind == SCOPE_PAGE_KIND && &artifact.identity[hash_width + 1..] != lower_fence {
    return Err(identity_error("scope-page identity fence disagrees with its first record"));
  }
  Ok(OrderedPageV1 {
    role,
    owner_id,
    generation: artifact.generation,
    page_id,
    previous_page_id,
    next_page_id,
    lower_fence,
    upper_fence,
    live_count,
    tombstone_count,
    logical_live_bytes: scanned.logical_live_bytes,
    minimum_coordinate,
    maximum_coordinate,
    records: OrderedRecordsV1 { hash_algorithm, role, bytes: record_bytes, count: record_count },
    key: artifact.key,
  })
}

fn decode_id_page_identity(identity: &[u8], hash_width: usize, role: OrderedIndexRoleV1) -> FormatResult<(OrderedIndexRoleV1, &[u8], u64)> {
  if identity.len() != hash_width + 8 {
    return Err(closure_error("ID-page identity length disagrees with the hash profile"));
  }
  let page_id = u64_at(identity, hash_width)?;
  if page_id == 0 {
    return Err(identity_error("ordered-page ID is zero"));
  }
  Ok((role, &identity[..hash_width], page_id))
}

fn decode_state_page_identity(identity: &[u8], hash_width: usize) -> FormatResult<(OrderedIndexRoleV1, &[u8], u64)> {
  if identity.len() != hash_width + 16 || identity[hash_width + 1..hash_width + 8].iter().any(|byte| *byte != 0) {
    return Err(closure_error("state-page identity length or reserve is invalid"));
  }
  let role = match identity[hash_width] {
    2 => OrderedIndexRoleV1::ValueDocumentState,
    3 => OrderedIndexRoleV1::IndexDocumentState,
    _ => return Err(closure_error("state-page owner class is invalid")),
  };
  let page_id = u64_at(identity, hash_width + 8)?;
  if page_id == 0 {
    return Err(identity_error("state-page ID is zero"));
  }
  Ok((role, &identity[..hash_width], page_id))
}

fn decode_scope_page_identity(identity: &[u8], hash_width: usize) -> FormatResult<(OrderedIndexRoleV1, &[u8], u64)> {
  if identity.len() < hash_width + 1 {
    return Err(truncated_error("scope-page identity is truncated"));
  }
  let role = match identity[hash_width] {
    1 if identity.len() == hash_width + 9 => OrderedIndexRoleV1::ScopeOrdinal,
    2 if identity.len() == 1 + 2 * hash_width => OrderedIndexRoleV1::ScopeReverse,
    _ => return Err(closure_error("scope-page role and identity length disagree")),
  };
  Ok((role, &identity[..hash_width], 0))
}

struct RecordScanV1<'a> {
  first: OrderedRecordV1<'a>,
  last: OrderedRecordV1<'a>,
  live_count: u32,
  logical_live_bytes: u64,
}

fn scan_records(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  bytes: &[u8],
  record_count: u32,
) -> FormatResult<RecordScanV1<'_>> {
  let mut cursor = 0usize;
  let mut first = None;
  let mut previous: Option<OrderedRecordV1<'_>> = None;
  let mut live_count = 0u32;
  let mut logical_live_bytes = 0u64;
  for _ in 0..record_count {
    let record = decode_record(hash_algorithm, role, bytes, &mut cursor)?;
    if let Some(previous) = &previous {
      if compare_records(hash_algorithm, role, previous, &record)? != Ordering::Less {
        return Err(order_error("ordered-page records are not strictly ordered"));
      }
    }
    if !record.tombstone {
      live_count = live_count.checked_add(1).ok_or_else(|| length_error("ordered-page live count overflow"))?;
      logical_live_bytes =
        logical_live_bytes.checked_add(record.encoded.len() as u64).ok_or_else(|| length_error("ordered-page logical-byte overflow"))?;
    }
    if first.is_none() {
      first = Some(record.clone());
    }
    previous = Some(record);
  }
  if cursor != bytes.len() {
    return Err(truncated_error("ordered-page record count does not consume the declared record area"));
  }
  Ok(RecordScanV1 {
    first: first.ok_or_else(|| closure_error("ordered page has no first record"))?,
    last: previous.ok_or_else(|| closure_error("ordered page has no last record"))?,
    live_count,
    logical_live_bytes,
  })
}

fn decode_record<'a>(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  bytes: &'a [u8],
  cursor: &mut usize,
) -> FormatResult<OrderedRecordV1<'a>> {
  match role {
    OrderedIndexRoleV1::Posting => decode_posting_record_for_page(bytes, cursor),
    OrderedIndexRoleV1::Value => decode_value_record(hash_algorithm, bytes, cursor),
    OrderedIndexRoleV1::ScopeOrdinal => decode_scope_ordinal_record(hash_algorithm, bytes, cursor),
    OrderedIndexRoleV1::ScopeReverse => decode_scope_reverse_record(hash_algorithm, bytes, cursor),
    OrderedIndexRoleV1::ValueDocumentState | OrderedIndexRoleV1::IndexDocumentState => {
      decode_state_record(hash_algorithm, role, bytes, cursor)
    }
    OrderedIndexRoleV1::NvtTile => Err(closure_error("NVT tiles are not ordered-page records")),
  }
}

pub fn decode_posting_record(value: &[u8]) -> FormatResult<PostingRecordV1<'_>> {
  let (record, consumed) = decode_posting_record_prefix(value)?;
  if consumed != value.len() {
    return Err(truncated_error("posting record contains trailing bytes"));
  }
  Ok(record)
}

pub fn encode_posting_record(record: &PostingRecordV1<'_>) -> FormatResult<Vec<u8>> {
  validate_key_length(record.posting_key.len())?;
  if record.document_ordinal == 0 {
    return Err(identity_error("posting document ordinal is zero"));
  }
  let length = 32usize.checked_add(record.posting_key.len()).ok_or_else(|| length_error("posting record length overflow"))?;
  let mut encoded = allocate_zeroed(length, "posting record")?;
  encoded[0] = u8::from(record.tombstone);
  write_u32(&mut encoded, 4, checked_u32(record.posting_key.len(), "posting key length")?)?;
  write_u64(&mut encoded, 8, record.coordinate)?;
  write_u64(&mut encoded, 16, record.document_ordinal)?;
  write_u32(&mut encoded, 24, record.source_value_ordinal)?;
  write_u32(&mut encoded, 28, record.expansion_ordinal)?;
  write_bytes(&mut encoded, 32, record.posting_key, "posting key")?;
  decode_posting_record(&encoded)?;
  Ok(encoded)
}

fn decode_posting_record_prefix(bytes: &[u8]) -> FormatResult<(PostingRecordV1<'_>, usize)> {
  let tombstone = decode_record_flags(bytes, 0)?;
  let key_length = checked_usize_from_u32(u32_at(bytes, 4)?, "posting key length")?;
  validate_key_length(key_length)?;
  let end = 32usize.checked_add(key_length).ok_or_else(|| length_error("posting record length overflow"))?;
  if end > bytes.len() {
    return Err(truncated_error("posting record is truncated"));
  }
  let document_ordinal = u64_at(bytes, 16)?;
  if document_ordinal == 0 {
    return Err(identity_error("posting document ordinal is zero"));
  }
  Ok((
    PostingRecordV1 {
      tombstone,
      coordinate: u64_at(bytes, 8)?,
      document_ordinal,
      source_value_ordinal: u32_at(bytes, 24)?,
      expansion_ordinal: u32_at(bytes, 28)?,
      posting_key: &bytes[32..end],
    },
    end,
  ))
}

fn decode_posting_record_for_page<'a>(bytes: &'a [u8], cursor: &mut usize) -> FormatResult<OrderedRecordV1<'a>> {
  let start = *cursor;
  let (record, consumed) = decode_posting_record_prefix(&bytes[start..])?;
  let end = start.checked_add(consumed).ok_or_else(|| length_error("posting record end overflow"))?;
  let suffix = &bytes[start + 24..start + 32];
  let key = record.posting_key;
  let encoded = &bytes[start..end];
  *cursor = end;
  Ok(OrderedRecordV1 {
    encoded,
    tombstone: record.tombstone,
    coordinate: record.coordinate,
    document_ordinal: record.document_ordinal,
    file_key: None,
    sort_key: RecordSortKeyV1::Posting { coordinate: record.coordinate, key, document_ordinal: record.document_ordinal, suffix },
  })
}

fn decode_value_record<'a>(hash_algorithm: HashAlgorithm, bytes: &'a [u8], cursor: &mut usize) -> FormatResult<OrderedRecordV1<'a>> {
  let start = *cursor;
  let (record, consumed) = decode_canonical_value_record_prefix(&bytes[start..], hash_algorithm)?;
  let end = start.checked_add(consumed).ok_or_else(|| length_error("value record end overflow"))?;
  let encoded = &bytes[start..end];
  let sort_key = RecordSortKeyV1::Contiguous(&bytes[start + 8..start + 20]);
  *cursor = end;
  Ok(OrderedRecordV1 {
    encoded,
    tombstone: record.tombstone,
    coordinate: 0,
    document_ordinal: record.document_ordinal,
    file_key: None,
    sort_key,
  })
}

fn decode_scope_ordinal_record<'a>(
  hash_algorithm: HashAlgorithm,
  bytes: &'a [u8],
  cursor: &mut usize,
) -> FormatResult<OrderedRecordV1<'a>> {
  let start = *cursor;
  let (record, consumed) = decode_scope_document_record_prefix(&bytes[start..], hash_algorithm)?;
  let end = start.checked_add(consumed).ok_or_else(|| length_error("scope ordinal record end overflow"))?;
  let encoded = &bytes[start..end];
  let sort_key = RecordSortKeyV1::Contiguous(&bytes[start + 8..start + 16]);
  *cursor = end;
  Ok(OrderedRecordV1 {
    encoded,
    tombstone: record.tombstone,
    coordinate: 0,
    document_ordinal: record.document_ordinal,
    file_key: Some(record.file_key),
    sort_key,
  })
}

fn decode_scope_reverse_record<'a>(
  hash_algorithm: HashAlgorithm,
  bytes: &'a [u8],
  cursor: &mut usize,
) -> FormatResult<OrderedRecordV1<'a>> {
  let start = *cursor;
  let (record, consumed) = decode_scope_reverse_record_prefix(&bytes[start..], hash_algorithm)?;
  let end = start.checked_add(consumed).ok_or_else(|| length_error("scope reverse record end overflow"))?;
  let encoded = &bytes[start..end];
  let sort_key = RecordSortKeyV1::Contiguous(record.file_key);
  *cursor = end;
  Ok(OrderedRecordV1 {
    encoded,
    tombstone: false,
    coordinate: 0,
    document_ordinal: record.document_ordinal,
    file_key: Some(record.file_key),
    sort_key,
  })
}

fn decode_state_record<'a>(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  bytes: &'a [u8],
  cursor: &mut usize,
) -> FormatResult<OrderedRecordV1<'a>> {
  let start = *cursor;
  let owner = match role {
    OrderedIndexRoleV1::ValueDocumentState => DocumentStateOwnerV1::ValueStore,
    OrderedIndexRoleV1::IndexDocumentState => DocumentStateOwnerV1::FieldIndex,
    _ => return Err(closure_error("non-state role reached document-state decoder")),
  };
  let (record, consumed) = decode_document_state_record_prefix(&bytes[start..], owner, hash_algorithm)?;
  let end = start.checked_add(consumed).ok_or_else(|| length_error("state record end overflow"))?;
  let encoded = &bytes[start..end];
  let sort_key = RecordSortKeyV1::Contiguous(&bytes[start + 8..start + 16]);
  *cursor = end;
  Ok(OrderedRecordV1 {
    encoded,
    tombstone: record.tombstone,
    coordinate: 0,
    document_ordinal: record.document_ordinal,
    file_key: None,
    sort_key,
  })
}

fn decode_record_flags(bytes: &[u8], offset: usize) -> FormatResult<bool> {
  let flags = *bytes.get(offset).ok_or_else(|| truncated_error("ordered record flags are truncated"))?;
  let reserve = bytes.get(offset + 1..offset + 4).ok_or_else(|| truncated_error("ordered record reserve is truncated"))?;
  if flags & !1 != 0 || reserve.iter().any(|byte| *byte != 0) {
    return Err(reserve_error("ordered record flags or reserve are noncanonical"));
  }
  Ok(flags & 1 != 0)
}

pub fn validate_scope_catalog_pair(ordinal: &[u8], reverse: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<()> {
  let ordinal = decode_ordered_page(ordinal, hash_algorithm)?;
  let reverse = decode_ordered_page(reverse, hash_algorithm)?;
  if ordinal.role != OrderedIndexRoleV1::ScopeOrdinal
    || reverse.role != OrderedIndexRoleV1::ScopeReverse
    || ordinal.owner_id != reverse.owner_id
  {
    return Err(closure_error("scope catalog directions do not describe the same scope"));
  }
  let mut ordinal_live = BTreeMap::new();
  for record in ordinal.records.iter() {
    let record = record?;
    if record.tombstone {
      continue;
    }
    let file_key = record.file_key.ok_or_else(|| closure_error("validated ordinal record has no FileKey"))?;
    if ordinal_live.insert(file_key, record.document_ordinal).is_some() {
      return Err(order_error("scope ordinal page repeats a live FileKey"));
    }
  }
  let mut reverse_live = BTreeMap::new();
  for record in reverse.records.iter() {
    let record = record?;
    let file_key = record.file_key.ok_or_else(|| closure_error("validated reverse record has no FileKey"))?;
    if reverse_live.insert(file_key, record.document_ordinal).is_some() {
      return Err(order_error("scope reverse page repeats a FileKey"));
    }
  }
  if ordinal_live != reverse_live {
    return Err(closure_error("scope ordinal and reverse pages are not an exact live bijection"));
  }
  Ok(())
}

pub fn compare_order_keys(hash_algorithm: HashAlgorithm, role: OrderedIndexRoleV1, left: &[u8], right: &[u8]) -> FormatResult<Ordering> {
  validate_order_key(hash_algorithm, role, left)?;
  validate_order_key(hash_algorithm, role, right)?;
  Ok(match role {
    OrderedIndexRoleV1::ScopeOrdinal
    | OrderedIndexRoleV1::ValueDocumentState
    | OrderedIndexRoleV1::IndexDocumentState
    | OrderedIndexRoleV1::NvtTile => u64_at(left, 0)?.cmp(&u64_at(right, 0)?),
    OrderedIndexRoleV1::ScopeReverse => left.cmp(right),
    OrderedIndexRoleV1::Value => u64_at(left, 0)?.cmp(&u64_at(right, 0)?).then(u32_at(left, 8)?.cmp(&u32_at(right, 8)?)),
    OrderedIndexRoleV1::Posting => compare_posting_positions(left, right)?,
  })
}

fn compare_records(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  left: &OrderedRecordV1<'_>,
  right: &OrderedRecordV1<'_>,
) -> FormatResult<Ordering> {
  match (&left.sort_key, &right.sort_key) {
    (RecordSortKeyV1::Contiguous(left), RecordSortKeyV1::Contiguous(right)) => compare_order_keys(hash_algorithm, role, left, right),
    (
      RecordSortKeyV1::Posting { coordinate: lc, key: lk, document_ordinal: ld, suffix: ls },
      RecordSortKeyV1::Posting { coordinate: rc, key: rk, document_ordinal: rd, suffix: rs },
    ) if role == OrderedIndexRoleV1::Posting => Ok(lc.cmp(rc).then_with(|| lk.cmp(rk)).then_with(|| ld.cmp(rd)).then_with(|| ls.cmp(rs))),
    _ => Err(closure_error("record sort-key representation disagrees with page role")),
  }
}

fn record_matches_fence(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  record: &OrderedRecordV1<'_>,
  fence: &[u8],
) -> FormatResult<bool> {
  match &record.sort_key {
    RecordSortKeyV1::Contiguous(key) => Ok(compare_order_keys(hash_algorithm, role, key, fence)? == Ordering::Equal),
    RecordSortKeyV1::Posting { coordinate, key, document_ordinal, suffix } if role == OrderedIndexRoleV1::Posting => {
      validate_order_key(hash_algorithm, role, fence)?;
      let key_end = fence.len() - 16;
      Ok(
        *coordinate == u64_at(fence, 0)?
          && *key == &fence[8..key_end]
          && *document_ordinal == u64_at(fence, key_end)?
          && *suffix == &fence[key_end + 8..],
      )
    }
    _ => Err(closure_error("record sort-key representation disagrees with page role")),
  }
}

fn compare_posting_positions(left: &[u8], right: &[u8]) -> FormatResult<Ordering> {
  let left_key_end = left.len() - 16;
  let right_key_end = right.len() - 16;
  Ok(
    u64_at(left, 0)?
      .cmp(&u64_at(right, 0)?)
      .then_with(|| left[8..left_key_end].cmp(&right[8..right_key_end]))
      .then(u64_at(left, left_key_end)?.cmp(&u64_at(right, right_key_end)?))
      .then(u32_at(left, left_key_end + 8)?.cmp(&u32_at(right, right_key_end + 8)?))
      .then(u32_at(left, left_key_end + 12)?.cmp(&u32_at(right, right_key_end + 12)?)),
  )
}

fn validate_descriptor_fences(hash_algorithm: HashAlgorithm, role: OrderedIndexRoleV1, lower: &[u8], upper: &[u8]) -> FormatResult<()> {
  validate_order_key(hash_algorithm, role, lower)?;
  validate_order_key(hash_algorithm, role, upper)?;
  if compare_order_keys(hash_algorithm, role, lower, upper)? == Ordering::Greater {
    return Err(order_error("descriptor lower fence sorts after upper fence"));
  }
  Ok(())
}

fn validate_order_key(hash_algorithm: HashAlgorithm, role: OrderedIndexRoleV1, key: &[u8]) -> FormatResult<()> {
  let valid = match role {
    OrderedIndexRoleV1::ScopeOrdinal | OrderedIndexRoleV1::ValueDocumentState | OrderedIndexRoleV1::IndexDocumentState => key.len() == 8,
    OrderedIndexRoleV1::ScopeReverse => key.len() == hash_algorithm.hash_length(),
    OrderedIndexRoleV1::Value => key.len() == 12,
    OrderedIndexRoleV1::Posting => key.len() >= 25,
    OrderedIndexRoleV1::NvtTile => key.len() == 8,
  };
  if !valid {
    return Err(closure_error("ordered key length disagrees with its role codec"));
  }
  Ok(())
}

fn validate_key_length(length: usize) -> FormatResult<()> {
  if length == 0 || length > MAX_KEY_LENGTH {
    return Err(amplification_error(format!("key length {length} is outside 1..={MAX_KEY_LENGTH}")));
  }
  Ok(())
}

fn checked_sum(mut values: impl Iterator<Item = u64>, context: &'static str) -> FormatResult<u64> {
  values.try_fold(0u64, |total, value| total.checked_add(value).ok_or_else(|| length_error(context)))
}

fn validate_owner(value: &[u8], hash_algorithm: HashAlgorithm, context: &'static str) -> FormatResult<()> {
  validate_hash(value, hash_algorithm, context)
}

fn validate_hash(value: &[u8], hash_algorithm: HashAlgorithm, context: &'static str) -> FormatResult<()> {
  if value.len() != hash_algorithm.hash_length() || value.iter().all(|byte| *byte == 0) {
    return Err(identity_error(format!("{context} has the wrong width or is all zero")));
  }
  Ok(())
}

fn allocate_zeroed(length: usize, label: &'static str) -> FormatResult<Vec<u8>> {
  let mut value = Vec::new();
  value
    .try_reserve_exact(length)
    .map_err(|source| amplification_error(format!("{label} allocation of {length} bytes failed: {source}")))?;
  value.resize(length, 0);
  Ok(value)
}

fn copy_fallible(source: &[u8], label: &'static str) -> FormatResult<Vec<u8>> {
  let mut value = allocate_zeroed(source.len(), label)?;
  value.copy_from_slice(source);
  Ok(value)
}

fn checked_u32(value: usize, context: &'static str) -> FormatResult<u32> {
  u32::try_from(value).map_err(|source| length_error(format!("{context} does not fit u32: {source}")))
}

fn checked_usize_from_u32(value: u32, context: &'static str) -> FormatResult<usize> {
  match usize::try_from(value) {
    Ok(value) => Ok(value),
    Err(source) => Err(length_error(format!("{context} does not fit usize: {source}"))),
  }
}

fn checked_usize_from_u64(value: u64, context: &'static str) -> FormatResult<usize> {
  match usize::try_from(value) {
    Ok(value) => Ok(value),
    Err(source) => Err(length_error(format!("{context} does not fit usize: {source}"))),
  }
}

fn write_bytes(destination: &mut [u8], offset: usize, value: &[u8], context: &'static str) -> FormatResult<()> {
  let end = offset.checked_add(value.len()).ok_or_else(|| length_error("byte write end overflow"))?;
  let target = destination.get_mut(offset..end).ok_or_else(|| truncated_error(format!("{context} exceeds its destination")))?;
  target.copy_from_slice(value);
  Ok(())
}

fn write_u16(destination: &mut [u8], offset: usize, value: u16) -> FormatResult<()> {
  write_bytes(destination, offset, &value.to_le_bytes(), "u16 write")
}

fn write_u32(destination: &mut [u8], offset: usize, value: u32) -> FormatResult<()> {
  write_bytes(destination, offset, &value.to_le_bytes(), "u32 write")
}

fn write_u64(destination: &mut [u8], offset: usize, value: u64) -> FormatResult<()> {
  write_bytes(destination, offset, &value.to_le_bytes(), "u64 write")
}

fn kind_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::UnknownTypeKindOrEnum, "ordered_index_kind", context)
}

fn truncated_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, "ordered_index_length", context)
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "ordered_index_arithmetic", context)
}

fn amplification_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, "ordered_index_bound", context)
}

fn reserve_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NonzeroReservedOrPadding, "ordered_index_reserved", context)
}

fn identity_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "ordered_index_identity", context)
}

fn order_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "ordered_index_order", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, "ordered_index_closure", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
