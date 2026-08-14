use crate::engine::HashAlgorithm;

use super::index_artifact::{
  EncodedImmutableIndexArtifactV1, ImmutableIndexArtifactKindV1, ImmutableIndexArtifactWriteV1,
  checked_immutable_index_artifact_encoded_length, decode_immutable_index_artifact, encode_immutable_index_artifact, u16_at, u32_at,
  u64_at,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const NVT_TILE_KIND: u16 = 0x0032;
const MAX_TILE_LENGTH: usize = 4 * 1_024 * 1_024;
const TILE_HEADER_LENGTH: usize = 64;
const ENTRY_LENGTH: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvtEntryWriteV1 {
  pub relative_cell: u32,
  pub predecessor_page_id: Option<u64>,
  pub successor_page_id: Option<u64>,
  pub approximate_live_postings: u64,
  pub sample_coordinate: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvtTileWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub owner_id: &'a [u8],
  pub generation: u64,
  pub resolution: u64,
  pub tile_start_cell: u64,
  pub tile_cell_count: u32,
  pub basis_posting_generation: u64,
  pub entries: &'a [NvtEntryWriteV1],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvtPostingPageSampleV1 {
  pub page_id: u64,
  pub minimum_coordinate: u64,
  pub maximum_coordinate: u64,
  pub live_postings: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseNvtBuildLimitsV1 {
  pub maximum_page_samples: usize,
  pub maximum_tiles: usize,
  pub maximum_output_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseNvtBuildRequestV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub owner_id: &'a [u8],
  pub generation: u64,
  pub resolution: u64,
  pub tile_cell_count: u32,
  pub basis_posting_generation: u64,
  pub pages: &'a [NvtPostingPageSampleV1],
  pub limits: SparseNvtBuildLimitsV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseNvtBuildPlanV1 {
  pub tiles: Vec<EncodedImmutableIndexArtifactV1>,
  pub populated_cell_count: u64,
  pub approximate_live_posting_count: u64,
  pub retained_encoded_bytes: usize,
}

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

pub fn encode_nvt_tile(request: &NvtTileWriteV1<'_>) -> FormatResult<EncodedImmutableIndexArtifactV1> {
  validate_tile_header(request)?;
  let entries_length = request.entries.len().checked_mul(ENTRY_LENGTH).ok_or_else(|| length_error("NVT entry byte length overflow"))?;
  let body_length = TILE_HEADER_LENGTH.checked_add(entries_length).ok_or_else(|| length_error("NVT tile body length overflow"))?;
  let mut body = allocate_zeroed(body_length, "NVT tile body")?;
  write_u16(&mut body, 4, 1)?;
  write_u16(&mut body, 6, 1)?;
  write_u64(&mut body, 8, request.resolution)?;
  write_u64(&mut body, 16, request.tile_start_cell)?;
  write_u32(&mut body, 24, checked_u32(request.tile_cell_count as usize, "NVT tile cell count")?)?;
  write_u32(&mut body, 28, checked_u32(request.entries.len(), "NVT populated entry count")?)?;
  write_u64(&mut body, 32, request.basis_posting_generation)?;
  write_u64(&mut body, 40, checked_u64(entries_length, "NVT entry byte length")?)?;

  let mut approximate_postings = 0u64;
  let mut previous_cell = None;
  for (index, entry) in request.entries.iter().enumerate() {
    if previous_cell.is_some_and(|previous| previous >= entry.relative_cell) {
      return Err(order_error("NVT sparse entries are not strictly ordered by relative cell"));
    }
    if entry.relative_cell >= request.tile_cell_count
      || coordinate_cell(entry.sample_coordinate, request.resolution) != Some(request.tile_start_cell + u64::from(entry.relative_cell))
    {
      return Err(closure_error("NVT sparse entry lies outside its tile or sample cell"));
    }
    validate_optional_page_id(entry.predecessor_page_id, "NVT predecessor PageId")?;
    validate_optional_page_id(entry.successor_page_id, "NVT successor PageId")?;
    approximate_postings = approximate_postings
      .checked_add(entry.approximate_live_postings)
      .ok_or_else(|| length_error("NVT approximate-posting count overflow"))?;
    let offset = TILE_HEADER_LENGTH
      .checked_add(index.checked_mul(ENTRY_LENGTH).ok_or_else(|| length_error("NVT entry offset overflow"))?)
      .ok_or_else(|| length_error("NVT entry offset overflow"))?;
    write_u32(&mut body, offset, entry.relative_cell)?;
    let flags = u32::from(entry.predecessor_page_id.is_some()) | (u32::from(entry.successor_page_id.is_some()) << 1);
    write_u32(&mut body, offset + 4, flags)?;
    match entry.predecessor_page_id {
      Some(page_id) => write_u64(&mut body, offset + 8, page_id)?,
      None => write_u64(&mut body, offset + 8, 0)?,
    }
    match entry.successor_page_id {
      Some(page_id) => write_u64(&mut body, offset + 16, page_id)?,
      None => write_u64(&mut body, offset + 16, 0)?,
    }
    write_u64(&mut body, offset + 24, entry.approximate_live_postings)?;
    write_u64(&mut body, offset + 32, entry.sample_coordinate)?;
    previous_cell = Some(entry.relative_cell);
  }
  write_u64(&mut body, 48, approximate_postings)?;

  let mut identity = allocate_zeroed(
    request.owner_id.len().checked_add(8).ok_or_else(|| length_error("NVT tile identity length overflow"))?,
    "NVT tile identity",
  )?;
  identity[..request.owner_id.len()].copy_from_slice(request.owner_id);
  write_u64(&mut identity, request.owner_id.len(), request.tile_start_cell)?;
  let encoded = encode_immutable_index_artifact(&ImmutableIndexArtifactWriteV1 {
    kind: ImmutableIndexArtifactKindV1::NvtTile,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  decode_nvt_tile(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

pub fn build_sparse_nvt_tiles_v1(request: &SparseNvtBuildRequestV1<'_>) -> FormatResult<SparseNvtBuildPlanV1> {
  validate_build_request(request)?;
  if request.pages.is_empty() {
    return Ok(SparseNvtBuildPlanV1 {
      tiles: Vec::new(),
      populated_cell_count: 0,
      approximate_live_posting_count: 0,
      retained_encoded_bytes: 0,
    });
  }

  let mut populated_cells = Vec::<BuildCellV1>::new();
  for (index, page) in request.pages.iter().enumerate() {
    validate_page_sample(page, request.resolution)?;
    if let Some(previous) = index.checked_sub(1).and_then(|previous| request.pages.get(previous)) {
      if previous.maximum_coordinate > page.minimum_coordinate {
        return Err(order_error("NVT posting-page samples overlap or are not strictly coordinate-ordered"));
      }
    }
    let cell = coordinate_cell(page.minimum_coordinate, request.resolution).ok_or_else(|| closure_error("NVT resolution is zero"))?;
    if let Some(existing) = populated_cells.last_mut().filter(|existing| existing.absolute_cell == cell) {
      existing.approximate_live_postings = existing
        .approximate_live_postings
        .checked_add(page.live_postings)
        .ok_or_else(|| length_error("NVT approximate-posting count overflow"))?;
      continue;
    }
    populated_cells.try_reserve(1).map_err(|error| allocation_error(format!("NVT cell planning reservation failed: {error}")))?;
    populated_cells.push(BuildCellV1 {
      absolute_cell: cell,
      predecessor_page_id: page.page_id,
      approximate_live_postings: page.live_postings,
      sample_coordinate: page.minimum_coordinate,
    });
  }

  let tile_cell_count = u64::from(request.tile_cell_count);
  let mut tiles = Vec::new();
  let mut retained_encoded_bytes = 0usize;
  let mut approximate_live_posting_count = 0u64;
  let mut cursor = 0usize;
  while cursor < populated_cells.len() {
    if tiles.len() >= request.limits.maximum_tiles {
      return Err(amplification_error("NVT build exceeds the tile-count limit"));
    }
    let tile_start_cell = populated_cells[cursor].absolute_cell / tile_cell_count * tile_cell_count;
    let mut end = cursor + 1;
    while end < populated_cells.len() && populated_cells[end].absolute_cell < tile_start_cell + tile_cell_count {
      end += 1;
    }
    let entries_length = (end - cursor).checked_mul(ENTRY_LENGTH).ok_or_else(|| length_error("NVT entry byte length overflow"))?;
    let body_length = TILE_HEADER_LENGTH.checked_add(entries_length).ok_or_else(|| length_error("NVT tile body length overflow"))?;
    let identity_length = request.owner_id.len().checked_add(8).ok_or_else(|| length_error("NVT tile identity length overflow"))?;
    let encoded_length =
      checked_immutable_index_artifact_encoded_length(ImmutableIndexArtifactKindV1::NvtTile, identity_length, body_length)?;
    let next_retained_bytes =
      retained_encoded_bytes.checked_add(encoded_length).ok_or_else(|| length_error("NVT retained output byte count overflow"))?;
    if next_retained_bytes > request.limits.maximum_output_bytes {
      return Err(amplification_error("NVT build exceeds the encoded-output byte limit"));
    }
    let mut entries = Vec::new();
    entries.try_reserve(end - cursor).map_err(|error| allocation_error(format!("NVT entry planning reservation failed: {error}")))?;
    for (relative_index, cell) in populated_cells[cursor..end].iter().enumerate() {
      approximate_live_posting_count = approximate_live_posting_count
        .checked_add(cell.approximate_live_postings)
        .ok_or_else(|| length_error("NVT approximate-posting count overflow"))?;
      let successor_page_id = populated_cells.get(cursor + relative_index + 1).map(|next| next.predecessor_page_id);
      entries.push(NvtEntryWriteV1 {
        relative_cell: u32::try_from(cell.absolute_cell - tile_start_cell)
          .map_err(|error| length_error(format!("NVT relative cell does not fit u32: {error}")))?,
        predecessor_page_id: Some(cell.predecessor_page_id),
        successor_page_id,
        approximate_live_postings: cell.approximate_live_postings,
        sample_coordinate: cell.sample_coordinate,
      });
    }
    let tile = encode_nvt_tile(&NvtTileWriteV1 {
      hash_algorithm: request.hash_algorithm,
      owner_id: request.owner_id,
      generation: request.generation,
      resolution: request.resolution,
      tile_start_cell,
      tile_cell_count: request.tile_cell_count,
      basis_posting_generation: request.basis_posting_generation,
      entries: &entries,
    })?;
    if tile.value.len() != encoded_length {
      return Err(closure_error("NVT tile writer disagrees with its encoded-length preflight"));
    }
    retained_encoded_bytes = next_retained_bytes;
    tiles.try_reserve(1).map_err(|error| allocation_error(format!("NVT tile output reservation failed: {error}")))?;
    tiles.push(tile);
    cursor = end;
  }

  Ok(SparseNvtBuildPlanV1 {
    tiles,
    populated_cell_count: checked_u64(populated_cells.len(), "NVT populated cell count")?,
    approximate_live_posting_count,
    retained_encoded_bytes,
  })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BuildCellV1 {
  absolute_cell: u64,
  predecessor_page_id: u64,
  approximate_live_postings: u64,
  sample_coordinate: u64,
}

fn validate_tile_header(request: &NvtTileWriteV1<'_>) -> FormatResult<()> {
  if request.owner_id.len() != request.hash_algorithm.hash_length()
    || request.owner_id.iter().all(|byte| *byte == 0)
    || request.generation == 0
  {
    return Err(identity_error("NVT tile owner or generation is invalid"));
  }
  if request.resolution == 0
    || request.tile_cell_count == 0
    || !request.tile_cell_count.is_power_of_two()
    || u64::from(request.tile_cell_count) > request.resolution
    || !request.resolution.is_multiple_of(u64::from(request.tile_cell_count))
    || request.tile_start_cell >= request.resolution
    || !request.tile_start_cell.is_multiple_of(u64::from(request.tile_cell_count))
    || request.tile_start_cell.checked_add(u64::from(request.tile_cell_count)).is_none_or(|end| end > request.resolution)
  {
    return Err(closure_error("NVT resolution, tile range, or identity start is invalid"));
  }
  if request.basis_posting_generation == 0 {
    return Err(identity_error("NVT basis posting generation is zero"));
  }
  if request.entries.is_empty() || request.entries.len() > request.tile_cell_count as usize {
    return Err(amplification_error("NVT populated-entry count is outside the tile-cell range"));
  }
  Ok(())
}

fn validate_build_request(request: &SparseNvtBuildRequestV1<'_>) -> FormatResult<()> {
  if request.owner_id.len() != request.hash_algorithm.hash_length()
    || request.owner_id.iter().all(|byte| *byte == 0)
    || request.generation == 0
    || request.basis_posting_generation == 0
  {
    return Err(identity_error("NVT build owner or generation is invalid"));
  }
  if request.resolution == 0
    || request.tile_cell_count == 0
    || !request.tile_cell_count.is_power_of_two()
    || u64::from(request.tile_cell_count) > request.resolution
    || !request.resolution.is_multiple_of(u64::from(request.tile_cell_count))
  {
    return Err(closure_error("NVT build resolution or tile-cell count is invalid"));
  }
  if request.limits.maximum_page_samples == 0 || request.limits.maximum_tiles == 0 || request.limits.maximum_output_bytes == 0 {
    return Err(amplification_error("NVT build limits must all be nonzero"));
  }
  if request.pages.len() > request.limits.maximum_page_samples {
    return Err(amplification_error("NVT build exceeds the page-sample limit"));
  }
  Ok(())
}

fn validate_page_sample(page: &NvtPostingPageSampleV1, resolution: u64) -> FormatResult<()> {
  if page.page_id == 0 {
    return Err(identity_error("NVT posting-page sample PageId is zero"));
  }
  if page.minimum_coordinate > page.maximum_coordinate || coordinate_cell(page.minimum_coordinate, resolution).is_none() {
    return Err(closure_error("NVT posting-page sample coordinate range is invalid"));
  }
  Ok(())
}

fn validate_optional_page_id(page_id: Option<u64>, label: &str) -> FormatResult<()> {
  if page_id == Some(0) {
    return Err(identity_error(format!("{label} is zero")));
  }
  Ok(())
}

fn checked_u32(value: usize, label: &str) -> FormatResult<u32> {
  u32::try_from(value).map_err(|error| length_error(format!("{label} does not fit u32: {error}")))
}

fn checked_u64(value: usize, label: &str) -> FormatResult<u64> {
  u64::try_from(value).map_err(|error| length_error(format!("{label} does not fit u64: {error}")))
}

fn allocate_zeroed(length: usize, label: &str) -> FormatResult<Vec<u8>> {
  let mut value = Vec::new();
  value.try_reserve_exact(length).map_err(|error| allocation_error(format!("{label} allocation failed: {error}")))?;
  value.resize(length, 0);
  Ok(value)
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) -> FormatResult<()> {
  write_bytes(bytes, offset, &value.to_le_bytes())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> FormatResult<()> {
  write_bytes(bytes, offset, &value.to_le_bytes())
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> FormatResult<()> {
  write_bytes(bytes, offset, &value.to_le_bytes())
}

fn write_bytes(bytes: &mut [u8], offset: usize, value: &[u8]) -> FormatResult<()> {
  let end = offset.checked_add(value.len()).ok_or_else(|| length_error("NVT write offset overflow"))?;
  let target = bytes.get_mut(offset..end).ok_or_else(|| truncated_error("NVT write exceeds its allocated buffer"))?;
  target.copy_from_slice(value);
  Ok(())
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

fn allocation_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, "nvt_tile_allocation", context)
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
