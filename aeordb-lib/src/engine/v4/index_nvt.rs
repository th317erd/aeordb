use crate::engine::HashAlgorithm;

use super::index_artifact::{decode_immutable_index_artifact, u16_at, u32_at, u64_at};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const NVT_TILE_KIND: u16 = 0x0032;
const MAX_TILE_LENGTH: usize = 4 * 1_024 * 1_024;
const TILE_HEADER_LENGTH: usize = 64;
const ENTRY_LENGTH: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvtEntryV1 {
  pub relative_cell: u32,
  pub predecessor_page_id: Option<u64>,
  pub successor_page_id: Option<u64>,
  pub approximate_live_postings: u64,
  pub sample_coordinate: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvtEntriesV1<'a> {
  bytes: &'a [u8],
  count: u32,
}

impl<'a> NvtEntriesV1<'a> {
  pub fn len(&self) -> usize {
    self.count as usize
  }

  pub fn is_empty(&self) -> bool {
    self.count == 0
  }

  pub fn entry_at(&self, index: usize) -> FormatResult<NvtEntryV1> {
    if index >= self.len() {
      return Err(error(
        MalformedInputClass::TruncationOrTrailingBytes,
        "nvt_entry_index",
        format!("entry index {index} is outside {} entries", self.len()),
      ));
    }
    decode_entry(self.bytes, index)
  }

  pub fn iter(&self) -> NvtEntryIteratorV1<'a> {
    NvtEntryIteratorV1 { entries: self.clone(), index: 0, failed: false }
  }
}

pub struct NvtEntryIteratorV1<'a> {
  entries: NvtEntriesV1<'a>,
  index: usize,
  failed: bool,
}

impl Iterator for NvtEntryIteratorV1<'_> {
  type Item = FormatResult<NvtEntryV1>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed || self.index == self.entries.len() {
      return None;
    }
    let decoded = self.entries.entry_at(self.index);
    self.index += 1;
    if decoded.is_err() {
      self.failed = true;
    }
    Some(decoded)
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    let remaining = if self.failed { 0 } else { self.entries.len() - self.index };
    (remaining, Some(remaining))
  }
}

impl ExactSizeIterator for NvtEntryIteratorV1<'_> {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvtTileV1<'a> {
  pub owner_id: &'a [u8],
  pub generation: u64,
  pub resolution: u64,
  pub tile_start_cell: u64,
  pub tile_cell_count: u32,
  pub basis_posting_generation: u64,
  pub approximate_postings: u64,
  pub entries: NvtEntriesV1<'a>,
  pub key: Vec<u8>,
}

impl NvtTileV1<'_> {
  pub fn predecessor_entry(&self, relative_cell: u32) -> Option<NvtEntryV1> {
    if relative_cell >= self.tile_cell_count || self.entries.is_empty() {
      return None;
    }
    let mut low = 0usize;
    let mut high = self.entries.len();
    while low < high {
      let middle = low + (high - low) / 2;
      let entry = self.entries.entry_at(middle).ok()?;
      if entry.relative_cell <= relative_cell {
        low = middle + 1;
      } else {
        high = middle;
      }
    }
    if low == 0 {
      None
    } else {
      self.entries.entry_at(low - 1).ok()
    }
  }
}

pub fn decode_nvt_tile(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<NvtTileV1<'_>> {
  let artifact = decode_immutable_index_artifact(value, hash_algorithm, MAX_TILE_LENGTH)?;
  let hash_width = hash_algorithm.hash_length();
  if artifact.kind != NVT_TILE_KIND {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "nvt_tile_kind", "artifact is not an NvtTileV1"));
  }
  if artifact.identity.len() != hash_width + 8 {
    return Err(closure_error("NVT tile identity length disagrees with the hash profile"));
  }
  let owner_id = &artifact.identity[..hash_width];
  if owner_id.iter().all(|byte| *byte == 0) {
    return Err(identity_error("NVT tile owner IndexId is all zero"));
  }
  let tile_start_cell = u64_at(artifact.identity, hash_width)?;
  let body = artifact.body;
  if body.len() < TILE_HEADER_LENGTH {
    return Err(truncated_error("NVT tile body is shorter than its fixed header"));
  }
  let resolution = u64_at(body, 8)?;
  let tile_cell_count = u32_at(body, 24)?;
  let populated_entry_count = u32_at(body, 28)?;
  let basis_posting_generation = u64_at(body, 32)?;
  let entries_length = usize::try_from(u64_at(body, 40)?).map_err(|_| length_error("NVT entry length does not fit usize"))?;
  if u32_at(body, 0)? != 0 || u64_at(body, 56)? != 0 {
    return Err(reserve_error("NVT tile flags or reserve are nonzero"));
  }
  if u16_at(body, 4)? != 1 {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "nvt_tile_version", "NVT tile body version is not 1"));
  }
  if u16_at(body, 6)? != 1 {
    return Err(closure_error("NVT tile coordinate codec is not fixed-point-u64 v1"));
  }
  if resolution == 0
    || u64_at(body, 16)? != tile_start_cell
    || tile_cell_count == 0
    || !tile_cell_count.is_power_of_two()
    || u64::from(tile_cell_count) > resolution
    || resolution % u64::from(tile_cell_count) != 0
    || tile_start_cell >= resolution
    || tile_start_cell % u64::from(tile_cell_count) != 0
    || tile_start_cell.checked_add(u64::from(tile_cell_count)).is_none_or(|end| end > resolution)
  {
    return Err(closure_error("NVT resolution, tile range, or identity start is invalid"));
  }
  if populated_entry_count == 0 || populated_entry_count > tile_cell_count {
    return Err(amplification_error("NVT populated-entry count is outside the tile-cell range"));
  }
  if basis_posting_generation == 0 {
    return Err(identity_error("NVT basis posting generation is zero"));
  }
  let expected_entries_length = usize::try_from(populated_entry_count)
    .ok()
    .and_then(|count| count.checked_mul(ENTRY_LENGTH))
    .ok_or_else(|| length_error("NVT entry-count multiplication overflow"))?;
  if entries_length != expected_entries_length {
    return Err(closure_error("NVT populated-entry count disagrees with entry bytes"));
  }
  if TILE_HEADER_LENGTH.checked_add(entries_length) != Some(body.len()) {
    return Err(truncated_error("NVT entry bytes do not consume the body"));
  }

  let entries = NvtEntriesV1 { bytes: &body[TILE_HEADER_LENGTH..], count: populated_entry_count };
  let mut previous_cell = None;
  let mut approximate_postings = 0u64;
  for entry in entries.iter() {
    let entry = entry?;
    if previous_cell.is_some_and(|previous| previous >= entry.relative_cell) {
      return Err(order_error("NVT sparse entries are not strictly ordered by relative cell"));
    }
    if entry.relative_cell >= tile_cell_count {
      return Err(closure_error("NVT sparse entry lies outside the tile"));
    }
    let sample_cell = coordinate_cell(entry.sample_coordinate, resolution).ok_or_else(|| closure_error("NVT resolution is zero"))?;
    if sample_cell != tile_start_cell + u64::from(entry.relative_cell) {
      return Err(closure_error("NVT sample coordinate maps to a different cell"));
    }
    approximate_postings = approximate_postings
      .checked_add(entry.approximate_live_postings)
      .ok_or_else(|| length_error("NVT approximate-posting count overflow"))?;
    previous_cell = Some(entry.relative_cell);
  }
  if u64_at(body, 48)? != approximate_postings {
    return Err(closure_error("NVT approximate-posting aggregate disagrees with entries"));
  }
  Ok(NvtTileV1 {
    owner_id,
    generation: artifact.generation,
    resolution,
    tile_start_cell,
    tile_cell_count,
    basis_posting_generation,
    approximate_postings,
    entries,
    key: artifact.key,
  })
}

fn decode_entry(bytes: &[u8], index: usize) -> FormatResult<NvtEntryV1> {
  let offset = index.checked_mul(ENTRY_LENGTH).ok_or_else(|| length_error("NVT entry offset overflow"))?;
  if offset.checked_add(ENTRY_LENGTH).is_none_or(|end| end > bytes.len()) {
    return Err(truncated_error("NVT entry is truncated"));
  }
  let relative_cell = u32_at(bytes, offset)?;
  let flags = u32_at(bytes, offset + 4)?;
  if flags & !0x03 != 0 {
    return Err(reserve_error("NVT entry flags contain unknown bits"));
  }
  let predecessor = u64_at(bytes, offset + 8)?;
  let successor = u64_at(bytes, offset + 16)?;
  if (flags & 1 != 0) != (predecessor != 0) || (flags & 2 != 0) != (successor != 0) {
    return Err(error(
      MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
      "nvt_entry_presence",
      "NVT page-ID presence bits disagree with zero/nonzero values",
    ));
  }
  Ok(NvtEntryV1 {
    relative_cell,
    predecessor_page_id: (predecessor != 0).then_some(predecessor),
    successor_page_id: (successor != 0).then_some(successor),
    approximate_live_postings: u64_at(bytes, offset + 24)?,
    sample_coordinate: u64_at(bytes, offset + 32)?,
  })
}

pub fn coordinate_cell(coordinate: u64, resolution: u64) -> Option<u64> {
  if resolution == 0 {
    return None;
  }
  let scaled = (u128::from(coordinate) * u128::from(resolution)) >> 64;
  Some(scaled.min(u128::from(resolution - 1)) as u64)
}

pub fn verified_page_hint(page_id: Option<u64>, known_minimum: u64, known_maximum: u64, present_page_ids: &[u64]) -> Option<u64> {
  if known_minimum > known_maximum || present_page_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
    return None;
  }
  let page_id = page_id?;
  (page_id >= known_minimum && page_id <= known_maximum && present_page_ids.binary_search(&page_id).is_ok()).then_some(page_id)
}

pub fn verified_predecessor_or_fallback(
  tile: Option<&NvtTileV1<'_>>,
  relative_cell: u32,
  known_minimum: u64,
  known_maximum: u64,
  present_page_ids: &[u64],
  directory_fallback: u64,
) -> u64 {
  tile
    .and_then(|tile| tile.predecessor_entry(relative_cell))
    .and_then(|entry| verified_page_hint(entry.predecessor_page_id, known_minimum, known_maximum, present_page_ids))
    .unwrap_or(directory_fallback)
}

fn truncated_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, "nvt_tile_length", context)
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "nvt_tile_arithmetic", context)
}

fn amplification_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, "nvt_tile_bound", context)
}

fn reserve_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NonzeroReservedOrPadding, "nvt_tile_reserved", context)
}

fn identity_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "nvt_tile_identity", context)
}

fn order_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "nvt_tile_order", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, "nvt_tile_closure", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
