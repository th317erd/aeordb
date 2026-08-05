use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::engine::HashAlgorithm;

use super::config_value::{CanonicalValueBounds, validate_canonical_value};
use super::hash::digest_parts;
use super::index_artifact::{decode_immutable_index_artifact, u16_at, u32_at, u64_at};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::scope::validate_canonical_absolute_path;

const DIRECTORY_KIND: u16 = 0x0020;
const POSTING_PAGE_KIND: u16 = 0x0030;
const VALUE_PAGE_KIND: u16 = 0x0031;
const SCOPE_PAGE_KIND: u16 = 0x0033;
const STATE_PAGE_KIND: u16 = 0x0034;
const MAX_ARTIFACT_LENGTH: usize = 4 * 1_024 * 1_024;
const MAX_KEY_LENGTH: usize = 1_024 * 1_024;
const MAX_EVIDENCE_LENGTH: usize = 4 * 1_024;
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

  fn owner_class(self) -> u8 {
    match self {
      Self::ScopeOrdinal | Self::ScopeReverse => 1,
      Self::Value | Self::ValueDocumentState => 2,
      Self::Posting | Self::IndexDocumentState | Self::NvtTile => 3,
    }
  }

  fn key_codec(self) -> u16 {
    match self {
      Self::ScopeOrdinal | Self::ValueDocumentState | Self::IndexDocumentState => 1,
      Self::ScopeReverse => 2,
      Self::Value => 3,
      Self::Posting => 4,
      Self::NvtTile => 5,
    }
  }

  fn uses_page_id(self) -> bool {
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
  let lower_length = usize::try_from(u32_at(body, 16)?).map_err(|_| length_error("directory lower-fence length conversion"))?;
  let upper_length = usize::try_from(u32_at(body, 20)?).map_err(|_| length_error("directory upper-fence length conversion"))?;
  let entries_length = usize::try_from(u32_at(body, 72)?).map_err(|_| length_error("directory entry length conversion"))?;
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
  let lower_length = usize::try_from(u32_at(body, start)?).map_err(|_| length_error("leaf lower-fence length conversion"))?;
  let upper_length = usize::try_from(u32_at(body, start + 4)?).map_err(|_| length_error("leaf upper-fence length conversion"))?;
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
    || logical_bytes == 0
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
  let lower_length = usize::try_from(u32_at(body, start)?).map_err(|_| length_error("internal lower-fence length conversion"))?;
  let upper_length = usize::try_from(u32_at(body, start + 4)?).map_err(|_| length_error("internal upper-fence length conversion"))?;
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
    || logical_bytes == 0
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
  let lower_length = usize::try_from(u32_at(body, 24)?).map_err(|_| length_error("page lower-fence length conversion"))?;
  let upper_length = usize::try_from(u32_at(body, 28)?).map_err(|_| length_error("page upper-fence length conversion"))?;
  let record_count = u32_at(body, 32)?;
  let live_count = u32_at(body, 36)?;
  let tombstone_count = u32_at(body, 40)?;
  let records_length = usize::try_from(u64_at(body, 48)?).map_err(|_| length_error("page record length conversion"))?;
  if u32_at(body, 0)? != 0 || body[80..96].iter().any(|byte| *byte != 0) {
    return Err(reserve_error("ordered-page flags or reserves are nonzero"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "ordered_page_version", "ordered-page body version is not 1"));
  }
  if u16_at(body, 6)? != role.key_codec() {
    return Err(closure_error("ordered-page key codec disagrees with its role"));
  }
  if role != OrderedIndexRoleV1::Posting && (u64_at(body, 8)? != 0 || u64_at(body, 16)? != 0) {
    return Err(reserve_error("non-posting page coordinate fields are nonzero"));
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
    OrderedIndexRoleV1::Posting => decode_posting_record(bytes, cursor),
    OrderedIndexRoleV1::Value => decode_value_record(hash_algorithm, bytes, cursor),
    OrderedIndexRoleV1::ScopeOrdinal => decode_scope_ordinal_record(hash_algorithm, bytes, cursor),
    OrderedIndexRoleV1::ScopeReverse => decode_scope_reverse_record(hash_algorithm, bytes, cursor),
    OrderedIndexRoleV1::ValueDocumentState | OrderedIndexRoleV1::IndexDocumentState => {
      decode_state_record(hash_algorithm, role, bytes, cursor)
    }
    OrderedIndexRoleV1::NvtTile => Err(closure_error("NVT tiles are not ordered-page records")),
  }
}

fn decode_posting_record<'a>(bytes: &'a [u8], cursor: &mut usize) -> FormatResult<OrderedRecordV1<'a>> {
  let start = *cursor;
  let tombstone = decode_record_flags(bytes, start)?;
  let key_length = usize::try_from(u32_at(bytes, start + 4)?).map_err(|_| length_error("posting key length conversion"))?;
  validate_key_length(key_length)?;
  let end =
    start.checked_add(32).and_then(|value| value.checked_add(key_length)).ok_or_else(|| length_error("posting record length overflow"))?;
  if end > bytes.len() {
    return Err(truncated_error("posting record is truncated"));
  }
  let coordinate = u64_at(bytes, start + 8)?;
  let document_ordinal = u64_at(bytes, start + 16)?;
  let suffix = &bytes[start + 24..start + 32];
  let key = &bytes[start + 32..end];
  let encoded = &bytes[start..end];
  *cursor = end;
  Ok(OrderedRecordV1 {
    encoded,
    tombstone,
    coordinate,
    document_ordinal,
    file_key: None,
    sort_key: RecordSortKeyV1::Posting { coordinate, key, document_ordinal, suffix },
  })
}

fn decode_value_record<'a>(hash_algorithm: HashAlgorithm, bytes: &'a [u8], cursor: &mut usize) -> FormatResult<OrderedRecordV1<'a>> {
  let hash_width = hash_algorithm.hash_length();
  let start = *cursor;
  let tombstone = decode_record_flags(bytes, start)?;
  let value_length = usize::try_from(u32_at(bytes, start + 4)?).map_err(|_| length_error("value length conversion"))?;
  let end = start
    .checked_add(24 + hash_width)
    .and_then(|value| value.checked_add(value_length))
    .ok_or_else(|| length_error("value record length overflow"))?;
  if end > bytes.len() {
    return Err(truncated_error("value record is truncated"));
  }
  if bytes[start + 20..start + 24].iter().any(|byte| *byte != 0) {
    return Err(reserve_error("value record reserve is nonzero"));
  }
  let revision = &bytes[start + 24..start + 24 + hash_width];
  if revision.iter().all(|byte| *byte == 0) || tombstone != (value_length == 0) {
    return Err(closure_error("value record revision or tombstone/value presence is invalid"));
  }
  if !tombstone {
    validate_canonical_value(&bytes[start + 24 + hash_width..end], CanonicalValueBounds::SOURCE_VALUE)?;
  }
  let document_ordinal = u64_at(bytes, start + 8)?;
  let encoded = &bytes[start..end];
  let sort_key = RecordSortKeyV1::Contiguous(&bytes[start + 8..start + 20]);
  *cursor = end;
  Ok(OrderedRecordV1 { encoded, tombstone, coordinate: 0, document_ordinal, file_key: None, sort_key })
}

fn decode_scope_ordinal_record<'a>(
  hash_algorithm: HashAlgorithm,
  bytes: &'a [u8],
  cursor: &mut usize,
) -> FormatResult<OrderedRecordV1<'a>> {
  let hash_width = hash_algorithm.hash_length();
  let start = *cursor;
  let tombstone = decode_record_flags(bytes, start)?;
  let path_length = usize::try_from(u32_at(bytes, start + 4)?).map_err(|_| length_error("scope path length conversion"))?;
  validate_key_length(path_length)?;
  let end = start
    .checked_add(16 + 2 * hash_width)
    .and_then(|value| value.checked_add(path_length))
    .ok_or_else(|| length_error("scope ordinal record length overflow"))?;
  if end > bytes.len() {
    return Err(truncated_error("scope ordinal record is truncated"));
  }
  let file_key = &bytes[start + 16..start + 16 + hash_width];
  let revision = &bytes[start + 16 + hash_width..start + 16 + 2 * hash_width];
  let path_bytes = &bytes[start + 16 + 2 * hash_width..end];
  let path = std::str::from_utf8(path_bytes)
    .map_err(|source| error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "scope_ordinal_path_utf8", source.to_string()))?;
  validate_canonical_absolute_path(path)?;
  let expected_file_key = digest_parts(hash_algorithm, &[b"file:", path.as_bytes()]);
  if expected_file_key != file_key || revision.iter().all(|byte| *byte == 0) {
    return Err(identity_error("scope ordinal FileKey or revision identity is invalid"));
  }
  let document_ordinal = u64_at(bytes, start + 8)?;
  let encoded = &bytes[start..end];
  let sort_key = RecordSortKeyV1::Contiguous(&bytes[start + 8..start + 16]);
  *cursor = end;
  Ok(OrderedRecordV1 { encoded, tombstone, coordinate: 0, document_ordinal, file_key: Some(file_key), sort_key })
}

fn decode_scope_reverse_record<'a>(
  hash_algorithm: HashAlgorithm,
  bytes: &'a [u8],
  cursor: &mut usize,
) -> FormatResult<OrderedRecordV1<'a>> {
  let hash_width = hash_algorithm.hash_length();
  let start = *cursor;
  let tombstone = decode_record_flags(bytes, start)?;
  let end = start.checked_add(12 + hash_width).ok_or_else(|| length_error("scope reverse record length overflow"))?;
  if end > bytes.len() {
    return Err(truncated_error("scope reverse record is truncated"));
  }
  let file_key = &bytes[start + 12..end];
  if tombstone || file_key.iter().all(|byte| *byte == 0) {
    return Err(closure_error("scope reverse records cannot be tombstones or use a zero FileKey"));
  }
  let document_ordinal = u64_at(bytes, start + 4)?;
  let encoded = &bytes[start..end];
  let sort_key = RecordSortKeyV1::Contiguous(file_key);
  *cursor = end;
  Ok(OrderedRecordV1 { encoded, tombstone: false, coordinate: 0, document_ordinal, file_key: Some(file_key), sort_key })
}

fn decode_state_record<'a>(
  hash_algorithm: HashAlgorithm,
  role: OrderedIndexRoleV1,
  bytes: &'a [u8],
  cursor: &mut usize,
) -> FormatResult<OrderedRecordV1<'a>> {
  let hash_width = hash_algorithm.hash_length();
  let start = *cursor;
  let flags = *bytes.get(start).ok_or_else(|| truncated_error("state record flags are truncated"))?;
  if flags & !1 != 0 {
    return Err(reserve_error("state record flags contain unknown bits"));
  }
  let tombstone = flags & 1 != 0;
  let stage = *bytes.get(start + 1).ok_or_else(|| truncated_error("state record stage is truncated"))?;
  let reason = u16_at(bytes, start + 2)?;
  let evidence_length = usize::try_from(u32_at(bytes, start + 4)?).map_err(|_| length_error("state evidence length conversion"))?;
  if evidence_length > MAX_EVIDENCE_LENGTH {
    return Err(amplification_error("state evidence exceeds 4 KiB"));
  }
  let end = start
    .checked_add(48 + hash_width)
    .and_then(|value| value.checked_add(evidence_length))
    .ok_or_else(|| length_error("state record length overflow"))?;
  if end > bytes.len() {
    return Err(truncated_error("state record is truncated"));
  }
  if bytes[start + 16..start + 16 + hash_width].iter().all(|byte| *byte == 0) {
    return Err(identity_error("state record revision identity is all zero"));
  }
  if u32_at(bytes, start + 44 + hash_width)? != 0 {
    return Err(reserve_error("state record reserve is nonzero"));
  }
  if !valid_state_reason(role, stage, reason) {
    return Err(closure_error("state stage and reason are not a legal pair for this role"));
  }
  if evidence_length == 0 {
    return Err(closure_error("state evidence is empty"));
  }
  validate_canonical_value(&bytes[start + 48 + hash_width..end], CanonicalValueBounds::CONFIG)?;
  let document_ordinal = u64_at(bytes, start + 8)?;
  let encoded = &bytes[start..end];
  let sort_key = RecordSortKeyV1::Contiguous(&bytes[start + 8..start + 16]);
  *cursor = end;
  Ok(OrderedRecordV1 { encoded, tombstone, coordinate: 0, document_ordinal, file_key: None, sort_key })
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
    let file_key = record.file_key.expect("validated ordinal record has a FileKey");
    if ordinal_live.insert(file_key, record.document_ordinal).is_some() {
      return Err(order_error("scope ordinal page repeats a live FileKey"));
    }
  }
  let mut reverse_live = BTreeMap::new();
  for record in reverse.records.iter() {
    let record = record?;
    let file_key = record.file_key.expect("validated reverse record has a FileKey");
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
    OrderedIndexRoleV1::Value => u64_at(left, 0)?.cmp(&u64_at(right, 0)?).then_with(|| {
      u32::from_le_bytes(left[8..12].try_into().expect("validated value key"))
        .cmp(&u32::from_le_bytes(right[8..12].try_into().expect("validated value key")))
    }),
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
      .then_with(|| {
        u64_at(left, left_key_end).expect("validated posting key").cmp(&u64_at(right, right_key_end).expect("validated posting key"))
      })
      .then_with(|| {
        u32::from_le_bytes(left[left_key_end + 8..left_key_end + 12].try_into().expect("validated posting key"))
          .cmp(&u32::from_le_bytes(right[right_key_end + 8..right_key_end + 12].try_into().expect("validated posting key")))
      })
      .then_with(|| {
        u32::from_le_bytes(left[left_key_end + 12..].try_into().expect("validated posting key"))
          .cmp(&u32::from_le_bytes(right[right_key_end + 12..].try_into().expect("validated posting key")))
      }),
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

fn valid_state_reason(role: OrderedIndexRoleV1, stage: u8, reason: u16) -> bool {
  match role {
    OrderedIndexRoleV1::ValueDocumentState => {
      matches!((stage, reason), (1, 0x0001..=0x0003) | (2, 0x0005..=0x0008) | (3, 0x0002 | 0x0004 | 0x0007 | 0x0008) | (4, 0x0007..=0x000b))
    }
    OrderedIndexRoleV1::IndexDocumentState => {
      matches!((stage, reason), (5, 0x0009..=0x000c | 0x000e | 0x000f) | (6, 0x0002 | 0x000d..=0x000f))
    }
    _ => false,
  }
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
