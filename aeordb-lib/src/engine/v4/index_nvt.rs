use crate::engine::HashAlgorithm;

use super::index_artifact::{
  EncodedImmutableIndexArtifactV1, ImmutableIndexArtifactKindV1, ImmutableIndexArtifactWriteV1,
  checked_immutable_index_artifact_encoded_length, decode_immutable_index_artifact, decode_index_manifest, encode_immutable_index_artifact,
  u16_at, u32_at, u64_at,
};
use super::index_manifest::IndexManifestBodyV1;
use super::index_page::{
  ArtifactDirectoryEntryV1, ArtifactDirectoryNodeV1, OrderedIndexRoleV1, OrderedPageV1, compare_order_keys, decode_artifact_directory,
  decode_ordered_page,
};
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const NVT_TILE_KIND: u16 = 0x0032;
const MAX_TILE_LENGTH: usize = 4 * 1_024 * 1_024;
const TILE_HEADER_LENGTH: usize = 64;
const ENTRY_LENGTH: usize = 40;
const MAX_INDEX_PATH_ARTIFACT_LENGTH: usize = 4 * 1_024 * 1_024;
const MAX_NVT_LOOKUP_TILE_CANDIDATES: usize = 16;
const MAX_NVT_LOOKUP_INPUT_BYTES: usize = 64 * 1_024 * 1_024;
const MAX_NVT_HEALING_PROPOSAL_BYTES: usize = 4 * 1_024;
const NVT_HEALING_PROPOSAL_FIXED_BYTES: usize = 32;

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
pub struct SparseNvtLookupLimitsV1 {
  pub maximum_directory_depth: usize,
  pub maximum_tile_candidates: usize,
  pub maximum_input_bytes: usize,
}

pub const fn default_sparse_nvt_lookup_limits_v1() -> SparseNvtLookupLimitsV1 {
  SparseNvtLookupLimitsV1 { maximum_directory_depth: 16, maximum_tile_candidates: 4, maximum_input_bytes: 32 * 1_024 * 1_024 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImmutableIndexPathV1<'a> {
  pub directories: &'a [&'a [u8]],
  pub leaf: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedFieldIndexV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub manifest_key: Vec<u8>,
  pub owner_id: &'a [u8],
  pub generation: u64,
  pub source_head_hash: &'a [u8],
  pub posting_directory_root: Option<&'a [u8]>,
  pub first_page_id: u64,
  pub last_page_id: u64,
  pub next_page_id: u64,
  pub posting_page_count: u64,
  pub live_posting_count: u64,
  pub posting_tombstone_count: u64,
  pub live_canonical_posting_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedFieldNvtV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub manifest_key: Vec<u8>,
  pub owner_id: &'a [u8],
  pub generation: u64,
  pub resolution: u64,
  pub tile_cells: u32,
  pub basis_posting_generation: u64,
  pub basis_source_head_hash: &'a [u8],
  pub tile_directory_root: Option<&'a [u8]>,
  pub tile_count: u64,
  pub populated_cell_count: u64,
  pub approximate_live_posting_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvtFallbackReasonV1 {
  Absent,
  Corrupt,
  IncompatibleOwner,
  StalePostingGeneration,
  StaleSourceHead,
  StalePageHint,
  MissingPredecessor,
  ResourceLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvtFallbackV1 {
  pub reason: NvtFallbackReasonV1,
  pub diagnostic: Option<FormatError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NvtBasisStatusV1<'a> {
  Usable(PinnedFieldNvtV1<'a>),
  Unavailable(NvtFallbackV1),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvtPageHintV1 {
  pub page_id: u64,
  pub tile_start_cell: u64,
  pub sample_coordinate: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvtHintSelectionV1 {
  pub hint: Option<NvtPageHintV1>,
  pub fallback: Option<NvtFallbackV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostingPageAnchorV1 {
  pub page_id: u64,
  pub generation: u64,
  pub page_artifact_hash: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvtHealingLimitsV1 {
  pub maximum_proposal_bytes: usize,
}

pub const fn default_nvt_healing_limits_v1() -> NvtHealingLimitsV1 {
  NvtHealingLimitsV1 { maximum_proposal_bytes: 512 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvtHealingDiagnosticV1 {
  pub class: MalformedInputClass,
  pub code: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvtHealingProposalV1 {
  pub field_index_manifest_key: Vec<u8>,
  pub observed_nvt_manifest_key: Option<Vec<u8>>,
  pub owner_id: Vec<u8>,
  pub posting_generation: u64,
  pub source_head_hash: Vec<u8>,
  pub target_coordinate: u64,
  pub exact_page_id: u64,
  pub exact_page_generation: u64,
  pub exact_page_artifact_hash: Vec<u8>,
  pub reason: NvtFallbackReasonV1,
  pub diagnostic: Option<NvtHealingDiagnosticV1>,
  pub retained_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NvtHealingDispositionV1 {
  NotNeeded,
  Proposed(NvtHealingProposalV1),
  Skipped(FormatError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvtLookupSourceV1 {
  Hint,
  ExactFallback,
}

#[derive(Debug, Clone, Copy)]
pub enum NvtLookupAttemptV1<'a> {
  Hint { basis: &'a PinnedFieldNvtV1<'a>, hint: NvtPageHintV1, posting_path: Option<&'a ImmutableIndexPathV1<'a>> },
  Fallback { basis: Option<&'a PinnedFieldNvtV1<'a>>, cause: &'a NvtFallbackV1 },
}

#[derive(Debug, Clone, Copy)]
pub struct NvtLookupRequestV1<'a> {
  pub field: &'a PinnedFieldIndexV1<'a>,
  pub target_coordinate: u64,
  pub target_posting_position: &'a [u8],
  pub attempt: NvtLookupAttemptV1<'a>,
  pub exact_posting_path: Option<&'a ImmutableIndexPathV1<'a>>,
  pub lookup_limits: SparseNvtLookupLimitsV1,
  pub healing_limits: NvtHealingLimitsV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvtLookupResolutionV1 {
  pub anchor: Option<PostingPageAnchorV1>,
  pub source: NvtLookupSourceV1,
  pub healing: NvtHealingDispositionV1,
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

pub fn pin_field_index_v1(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<PinnedFieldIndexV1<'_>> {
  let manifest = decode_index_manifest(value, hash_algorithm)?;
  let IndexManifestBodyV1::FieldIndex(body) = manifest.details else {
    return Err(lookup_closure_error("pinned manifest is not a FieldIndex manifest"));
  };
  Ok(PinnedFieldIndexV1 {
    hash_algorithm,
    manifest_key: manifest.key,
    owner_id: manifest.owner_id,
    generation: manifest.generation,
    source_head_hash: body.coverage.source_namespace_root,
    posting_directory_root: body.posting_directory_root,
    first_page_id: body.first_page_id,
    last_page_id: body.last_page_id,
    next_page_id: body.next_page_id,
    posting_page_count: body.posting_page_count,
    live_posting_count: body.live_posting_count,
    posting_tombstone_count: body.posting_tombstone_count,
    live_canonical_posting_bytes: body.live_canonical_posting_bytes,
  })
}

pub fn validate_field_nvt_basis_v1<'a>(field: &PinnedFieldIndexV1<'_>, value: Option<&'a [u8]>) -> NvtBasisStatusV1<'a> {
  let Some(value) = value else {
    return unavailable_nvt_basis(NvtFallbackReasonV1::Absent, None);
  };
  let manifest = match decode_index_manifest(value, field.hash_algorithm) {
    Ok(manifest) => manifest,
    Err(error) => return unavailable_nvt_basis(nvt_error_reason(&error), Some(error)),
  };
  let IndexManifestBodyV1::FieldNvt(body) = manifest.details else {
    return unavailable_nvt_basis(
      NvtFallbackReasonV1::Corrupt,
      Some(lookup_closure_error("NVT basis artifact is not a FieldNvt manifest")),
    );
  };
  if manifest.owner_id != field.owner_id {
    return unavailable_nvt_basis(NvtFallbackReasonV1::IncompatibleOwner, None);
  }
  if body.basis_posting_generation != field.generation {
    return unavailable_nvt_basis(NvtFallbackReasonV1::StalePostingGeneration, None);
  }
  if body.basis_source_head_hash != field.source_head_hash {
    return unavailable_nvt_basis(NvtFallbackReasonV1::StaleSourceHead, None);
  }
  NvtBasisStatusV1::Usable(PinnedFieldNvtV1 {
    hash_algorithm: field.hash_algorithm,
    manifest_key: manifest.key,
    owner_id: manifest.owner_id,
    generation: manifest.generation,
    resolution: body.resolution,
    tile_cells: body.tile_cells,
    basis_posting_generation: body.basis_posting_generation,
    basis_source_head_hash: body.basis_source_head_hash,
    tile_directory_root: body.tile_directory_root,
    tile_count: body.tile_count,
    populated_cell_count: body.populated_cell_count,
    approximate_live_posting_count: body.approximate_live_posting_count,
  })
}

pub fn select_nvt_predecessor_hint_v1(
  basis: &PinnedFieldNvtV1<'_>,
  target_coordinate: u64,
  candidates: &[ImmutableIndexPathV1<'_>],
  limits: SparseNvtLookupLimitsV1,
) -> FormatResult<NvtHintSelectionV1> {
  validate_lookup_limits(limits)?;
  if candidates.len() > limits.maximum_tile_candidates {
    let error = lookup_amplification_error("NVT lookup exceeds the tile-candidate limit");
    return Ok(failed_nvt_hint(NvtFallbackReasonV1::ResourceLimit, error));
  }
  if let Err(error) = validate_paths_input_bytes(candidates, limits) {
    return Ok(failed_nvt_hint(nvt_error_reason(&error), error));
  }
  if basis.tile_directory_root.is_none() || candidates.is_empty() {
    return Ok(missing_nvt_hint());
  }
  let Some(target_cell) = coordinate_cell(target_coordinate, basis.resolution) else {
    return Ok(failed_nvt_hint(NvtFallbackReasonV1::Corrupt, lookup_closure_error("pinned NVT resolution is zero")));
  };
  let target_tile_start = target_cell / u64::from(basis.tile_cells) * u64::from(basis.tile_cells);
  let mut lookup_cell = target_tile_start;
  for candidate in candidates {
    let lookup_key = lookup_cell.to_le_bytes();
    let tile = match validate_nvt_tile_path(basis, &lookup_key, candidate, limits) {
      Ok(tile) => tile,
      Err(error) => {
        return Ok(failed_nvt_hint(nvt_error_reason(&error), error));
      }
    };
    if tile.tile_start_cell > lookup_cell {
      return Ok(missing_nvt_hint());
    }
    let relative_cell = if tile.tile_start_cell == target_tile_start {
      u32::try_from(target_cell - tile.tile_start_cell)
        .map_err(|error| lookup_length_error(format!("target relative cell does not fit u32: {error}")))?
    } else {
      tile.tile_cell_count - 1
    };
    if let Some(entry) = tile.predecessor_entry(relative_cell) {
      if let Some(page_id) = entry.predecessor_page_id {
        return Ok(NvtHintSelectionV1 {
          hint: Some(NvtPageHintV1 { page_id, tile_start_cell: tile.tile_start_cell, sample_coordinate: entry.sample_coordinate }),
          fallback: None,
        });
      }
    }
    if tile.tile_start_cell == 0 {
      break;
    }
    lookup_cell = tile.tile_start_cell - 1;
  }
  Ok(missing_nvt_hint())
}

pub fn validate_nvt_page_hint_v1(
  field: &PinnedFieldIndexV1<'_>,
  target_posting_position: &[u8],
  page_id: u64,
  path: Option<&ImmutableIndexPathV1<'_>>,
  limits: SparseNvtLookupLimitsV1,
) -> FormatResult<Option<PostingPageAnchorV1>> {
  validate_lookup_limits(limits)?;
  validate_posting_position(field.hash_algorithm, target_posting_position)?;
  if page_id == 0 || page_id >= field.next_page_id || field.posting_directory_root.is_none() {
    return Ok(None);
  }
  let Some(path) = path else {
    return Ok(None);
  };
  validate_path_input_bytes(path, limits)?;
  let root = posting_root_expectation(field)?;
  let Some(descriptor) = validate_directory_path(root, PathSelectionV1::PageId(page_id), path, limits)? else {
    return Ok(None);
  };
  let page = decode_ordered_page(path.leaf, field.hash_algorithm)?;
  validate_posting_page_closure(field, &descriptor, &page)?;
  if page.page_id != page_id
    || compare_order_keys(field.hash_algorithm, OrderedIndexRoleV1::Posting, page.lower_fence, target_posting_position)?
      == std::cmp::Ordering::Greater
  {
    return Ok(None);
  }
  Ok(Some(PostingPageAnchorV1 { page_id: page.page_id, generation: page.generation, page_artifact_hash: page.key }))
}

pub fn exact_posting_predecessor_v1(
  field: &PinnedFieldIndexV1<'_>,
  target_posting_position: &[u8],
  path: Option<&ImmutableIndexPathV1<'_>>,
  limits: SparseNvtLookupLimitsV1,
) -> FormatResult<Option<PostingPageAnchorV1>> {
  validate_lookup_limits(limits)?;
  validate_posting_position(field.hash_algorithm, target_posting_position)?;
  let Some(_) = field.posting_directory_root else {
    if path.is_some() {
      return Err(lookup_closure_error("empty FieldIndex was supplied a Posting path"));
    }
    return Ok(None);
  };
  let path = path.ok_or_else(|| lookup_closure_error("populated FieldIndex is missing its exact Posting path"))?;
  validate_path_input_bytes(path, limits)?;
  let root = posting_root_expectation(field)?;
  let descriptor = validate_directory_path(root, PathSelectionV1::OrderKey(target_posting_position), path, limits)?
    .ok_or_else(|| lookup_closure_error("exact Posting predecessor path selected no descriptor"))?;
  let page = decode_ordered_page(path.leaf, field.hash_algorithm)?;
  validate_posting_page_closure(field, &descriptor, &page)?;
  Ok(Some(PostingPageAnchorV1 { page_id: page.page_id, generation: page.generation, page_artifact_hash: page.key }))
}

pub fn resolve_nvt_lookup_v1(request: &NvtLookupRequestV1<'_>) -> FormatResult<NvtLookupResolutionV1> {
  match request.attempt {
    NvtLookupAttemptV1::Hint { basis, hint, posting_path } => {
      if let Some(cause) = incompatible_pinned_basis(request.field, basis) {
        return resolve_exact_nvt_fallback(request, Some(basis), &cause);
      }
      if nvt_hint_matches_target(basis, request.target_coordinate, hint) {
        if let Some(anchor) =
          validate_nvt_page_hint_v1(request.field, request.target_posting_position, hint.page_id, posting_path, request.lookup_limits)?
        {
          return Ok(NvtLookupResolutionV1 {
            anchor: Some(anchor),
            source: NvtLookupSourceV1::Hint,
            healing: NvtHealingDispositionV1::NotNeeded,
          });
        }
      }
      let cause = NvtFallbackV1 { reason: NvtFallbackReasonV1::StalePageHint, diagnostic: None };
      resolve_exact_nvt_fallback(request, Some(basis), &cause)
    }
    NvtLookupAttemptV1::Fallback { basis, cause } => resolve_exact_nvt_fallback(request, basis, cause),
  }
}

fn resolve_exact_nvt_fallback(
  request: &NvtLookupRequestV1<'_>,
  basis: Option<&PinnedFieldNvtV1<'_>>,
  cause: &NvtFallbackV1,
) -> FormatResult<NvtLookupResolutionV1> {
  let anchor =
    exact_posting_predecessor_v1(request.field, request.target_posting_position, request.exact_posting_path, request.lookup_limits)?;
  let compatible_basis = basis.filter(|basis| incompatible_pinned_basis(request.field, basis).is_none());
  let healing = if anchor.is_none() {
    NvtHealingDispositionV1::NotNeeded
  } else if cause.reason == NvtFallbackReasonV1::ResourceLimit {
    NvtHealingDispositionV1::Skipped(lookup_amplification_error("resource-limited NVT evidence is not a repair signal"))
  } else if let Some(anchor) = anchor.as_ref() {
    match build_nvt_healing_proposal(request, compatible_basis, cause, anchor) {
      Ok(proposal) => NvtHealingDispositionV1::Proposed(proposal),
      Err(error) => NvtHealingDispositionV1::Skipped(error),
    }
  } else {
    NvtHealingDispositionV1::NotNeeded
  };
  Ok(NvtLookupResolutionV1 { anchor, source: NvtLookupSourceV1::ExactFallback, healing })
}

fn incompatible_pinned_basis(field: &PinnedFieldIndexV1<'_>, basis: &PinnedFieldNvtV1<'_>) -> Option<NvtFallbackV1> {
  let reason = if basis.hash_algorithm != field.hash_algorithm || basis.owner_id != field.owner_id {
    NvtFallbackReasonV1::IncompatibleOwner
  } else if basis.basis_posting_generation != field.generation {
    NvtFallbackReasonV1::StalePostingGeneration
  } else if basis.basis_source_head_hash != field.source_head_hash {
    NvtFallbackReasonV1::StaleSourceHead
  } else {
    return None;
  };
  Some(NvtFallbackV1 { reason, diagnostic: None })
}

fn nvt_hint_matches_target(basis: &PinnedFieldNvtV1<'_>, target_coordinate: u64, hint: NvtPageHintV1) -> bool {
  let Some(target_cell) = coordinate_cell(target_coordinate, basis.resolution) else {
    return false;
  };
  let Some(sample_cell) = coordinate_cell(hint.sample_coordinate, basis.resolution) else {
    return false;
  };
  let tile_cells = u64::from(basis.tile_cells);
  let Some(tile_end) = hint.tile_start_cell.checked_add(tile_cells) else {
    return false;
  };
  hint.page_id != 0
    && tile_cells != 0
    && hint.tile_start_cell.is_multiple_of(tile_cells)
    && sample_cell >= hint.tile_start_cell
    && sample_cell < tile_end
    && sample_cell <= target_cell
    && hint.tile_start_cell <= target_cell / tile_cells * tile_cells
}

fn build_nvt_healing_proposal(
  request: &NvtLookupRequestV1<'_>,
  basis: Option<&PinnedFieldNvtV1<'_>>,
  cause: &NvtFallbackV1,
  anchor: &PostingPageAnchorV1,
) -> FormatResult<NvtHealingProposalV1> {
  if request.healing_limits.maximum_proposal_bytes == 0 || request.healing_limits.maximum_proposal_bytes > MAX_NVT_HEALING_PROPOSAL_BYTES {
    return Err(lookup_amplification_error("NVT healing proposal budget is outside the admitted range"));
  }
  let observed_nvt_manifest_key = basis.map(|basis| basis.manifest_key.as_slice());
  let retained_bytes = [
    request.field.manifest_key.len(),
    observed_nvt_manifest_key.map_or(0, <[u8]>::len),
    request.field.owner_id.len(),
    request.field.source_head_hash.len(),
    anchor.page_artifact_hash.len(),
    NVT_HEALING_PROPOSAL_FIXED_BYTES,
  ]
  .into_iter()
  .try_fold(0usize, |total, bytes| {
    total.checked_add(bytes).ok_or_else(|| lookup_length_error("NVT healing proposal byte count overflow"))
  })?;
  if retained_bytes > request.healing_limits.maximum_proposal_bytes {
    return Err(lookup_amplification_error("NVT healing proposal exceeds the caller byte budget"));
  }
  Ok(NvtHealingProposalV1 {
    field_index_manifest_key: try_copy_healing_bytes(&request.field.manifest_key)?,
    observed_nvt_manifest_key: match observed_nvt_manifest_key {
      Some(value) => Some(try_copy_healing_bytes(value)?),
      None => None,
    },
    owner_id: try_copy_healing_bytes(request.field.owner_id)?,
    posting_generation: request.field.generation,
    source_head_hash: try_copy_healing_bytes(request.field.source_head_hash)?,
    target_coordinate: request.target_coordinate,
    exact_page_id: anchor.page_id,
    exact_page_generation: anchor.generation,
    exact_page_artifact_hash: try_copy_healing_bytes(&anchor.page_artifact_hash)?,
    reason: cause.reason,
    diagnostic: cause.diagnostic.as_ref().map(|error| NvtHealingDiagnosticV1 { class: error.class(), code: error.code() }),
    retained_bytes,
  })
}

fn try_copy_healing_bytes(value: &[u8]) -> FormatResult<Vec<u8>> {
  let mut copy = Vec::new();
  copy
    .try_reserve_exact(value.len())
    .map_err(|error| lookup_amplification_error(format!("NVT healing proposal allocation failed: {error}")))?;
  copy.extend_from_slice(value);
  Ok(copy)
}

#[derive(Debug, Clone, Copy)]
struct DirectoryRootExpectationV1<'a> {
  hash_algorithm: HashAlgorithm,
  root_key: &'a [u8],
  owner_id: &'a [u8],
  maximum_generation: u64,
  role: OrderedIndexRoleV1,
  live_count: u64,
  tombstone_count: u64,
  page_count: u64,
  logical_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
enum PathSelectionV1<'a> {
  OrderKey(&'a [u8]),
  PageId(u64),
}

fn posting_root_expectation<'a>(field: &PinnedFieldIndexV1<'a>) -> FormatResult<DirectoryRootExpectationV1<'a>> {
  let root_key = field.posting_directory_root.ok_or_else(|| lookup_closure_error("FieldIndex has no Posting directory root"))?;
  Ok(DirectoryRootExpectationV1 {
    hash_algorithm: field.hash_algorithm,
    root_key,
    owner_id: field.owner_id,
    maximum_generation: field.generation,
    role: OrderedIndexRoleV1::Posting,
    live_count: field.live_posting_count,
    tombstone_count: field.posting_tombstone_count,
    page_count: field.posting_page_count,
    logical_bytes: Some(field.live_canonical_posting_bytes),
  })
}

fn nvt_root_expectation<'a>(basis: &PinnedFieldNvtV1<'a>) -> FormatResult<DirectoryRootExpectationV1<'a>> {
  let root_key = basis.tile_directory_root.ok_or_else(|| lookup_closure_error("FieldNvt has no tile directory root"))?;
  Ok(DirectoryRootExpectationV1 {
    hash_algorithm: basis.hash_algorithm,
    root_key,
    owner_id: basis.owner_id,
    maximum_generation: basis.generation,
    role: OrderedIndexRoleV1::NvtTile,
    live_count: basis.populated_cell_count,
    tombstone_count: 0,
    page_count: basis.tile_count,
    logical_bytes: None,
  })
}

fn validate_directory_path<'a>(
  root: DirectoryRootExpectationV1<'_>,
  selection: PathSelectionV1<'_>,
  path: &ImmutableIndexPathV1<'a>,
  limits: SparseNvtLookupLimitsV1,
) -> FormatResult<Option<ArtifactDirectoryEntryV1<'a>>> {
  if path.directories.is_empty() || path.directories.len() > limits.maximum_directory_depth || path.directories.len() > 16 {
    return Err(lookup_amplification_error("immutable index path depth is outside the admitted range"));
  }
  let mut parent_descriptor: Option<ArtifactDirectoryEntryV1<'a>> = None;
  for (index, bytes) in path.directories.iter().enumerate() {
    let directory = decode_artifact_directory(bytes, root.hash_algorithm)?;
    validate_directory_identity(&directory, root.owner_id, root.role, root.maximum_generation)?;
    let expected_level = u16::try_from(path.directories.len() - index - 1)
      .map_err(|error| lookup_length_error(format!("directory path level does not fit u16: {error}")))?;
    if directory.level != expected_level {
      return Err(lookup_closure_error("directory path levels are not a complete root-to-leaf chain"));
    }
    if index == 0 {
      validate_directory_root(&directory, root)?;
    } else {
      let parent = parent_descriptor.as_ref().ok_or_else(|| lookup_closure_error("directory path is missing its parent descriptor"))?;
      validate_directory_descriptor(parent, &directory)?;
    }
    let next_value = if index + 1 < path.directories.len() { path.directories[index + 1] } else { path.leaf };
    let next_key = decode_immutable_index_artifact(next_value, root.hash_algorithm, MAX_INDEX_PATH_ARTIFACT_LENGTH)?.key;
    let selected = match selection {
      PathSelectionV1::OrderKey(key) => {
        let entry = predecessor_directory_entry(&directory, root.hash_algorithm, key)?;
        if entry.child_hash != next_key {
          return Err(lookup_closure_error("directory predecessor path does not name the supplied child"));
        }
        Some(entry.clone())
      }
      PathSelectionV1::PageId(page_id) => matching_page_id_entry(&directory, page_id, &next_key)?,
    };
    let Some(selected) = selected else {
      return Ok(None);
    };
    parent_descriptor = Some(selected);
  }
  Ok(parent_descriptor)
}

fn predecessor_directory_entry<'node, 'data>(
  directory: &'node ArtifactDirectoryNodeV1<'data>,
  hash_algorithm: HashAlgorithm,
  key: &[u8],
) -> FormatResult<&'node ArtifactDirectoryEntryV1<'data>> {
  compare_order_keys(hash_algorithm, directory.role, key, key)?;
  let mut low = 0usize;
  let mut high = directory.entries.len();
  while low < high {
    let middle = low + (high - low) / 2;
    if compare_order_keys(hash_algorithm, directory.role, directory.entries[middle].lower_fence, key)? != std::cmp::Ordering::Greater {
      low = middle + 1;
    } else {
      high = middle;
    }
  }
  let index = if low == 0 { 0 } else { low - 1 };
  directory.entries.get(index).ok_or_else(|| lookup_closure_error("validated directory has no predecessor descriptor"))
}

fn matching_page_id_entry<'data>(
  directory: &ArtifactDirectoryNodeV1<'data>,
  page_id: u64,
  next_key: &[u8],
) -> FormatResult<Option<ArtifactDirectoryEntryV1<'data>>> {
  let mut matching = None;
  for entry in &directory.entries {
    if page_id >= entry.minimum_page_id && page_id <= entry.maximum_page_id && entry.child_hash == next_key {
      if matching.is_some() {
        return Err(lookup_order_error("directory repeats one PageId path child"));
      }
      matching = Some(entry.clone());
    }
  }
  Ok(matching)
}

fn validate_directory_root(directory: &ArtifactDirectoryNodeV1<'_>, root: DirectoryRootExpectationV1<'_>) -> FormatResult<()> {
  if directory.key != root.root_key
    || directory.live_count != root.live_count
    || directory.tombstone_count != root.tombstone_count
    || directory.page_count != root.page_count
    || root.logical_bytes.is_some_and(|logical_bytes| directory.logical_bytes != logical_bytes)
  {
    return Err(lookup_closure_error("directory root disagrees with its pinned manifest"));
  }
  Ok(())
}

fn validate_directory_identity(
  directory: &ArtifactDirectoryNodeV1<'_>,
  owner_id: &[u8],
  role: OrderedIndexRoleV1,
  maximum_generation: u64,
) -> FormatResult<()> {
  if directory.owner_id != owner_id || directory.role != role || directory.generation > maximum_generation {
    return Err(lookup_closure_error("directory owner, role, or birth generation disagrees with its pinned manifest"));
  }
  Ok(())
}

fn validate_directory_descriptor(entry: &ArtifactDirectoryEntryV1<'_>, child: &ArtifactDirectoryNodeV1<'_>) -> FormatResult<()> {
  if entry.child_hash != child.key
    || entry.child_generation != child.generation
    || entry.lower_fence != child.lower_fence
    || entry.upper_fence != child.upper_fence
    || entry.live_count != child.live_count
    || entry.tombstone_count != child.tombstone_count
    || entry.page_count != child.page_count
    || entry.logical_bytes != child.logical_bytes
    || entry.minimum_page_id != child.minimum_page_id
    || entry.maximum_page_id != child.maximum_page_id
  {
    return Err(lookup_closure_error("directory descriptor disagrees with its child directory"));
  }
  Ok(())
}

fn validate_posting_page_closure(
  field: &PinnedFieldIndexV1<'_>,
  descriptor: &ArtifactDirectoryEntryV1<'_>,
  page: &OrderedPageV1<'_>,
) -> FormatResult<()> {
  if page.owner_id != field.owner_id
    || page.role != OrderedIndexRoleV1::Posting
    || page.generation > field.generation
    || descriptor.child_hash != page.key
    || descriptor.child_generation != page.generation
    || descriptor.lower_fence != page.lower_fence
    || descriptor.upper_fence != page.upper_fence
    || descriptor.live_count != u64::from(page.live_count)
    || descriptor.tombstone_count != u64::from(page.tombstone_count)
    || descriptor.page_count != 1
    || descriptor.logical_bytes != page.logical_live_bytes
    || descriptor.minimum_page_id != page.page_id
    || descriptor.maximum_page_id != page.page_id
  {
    return Err(lookup_closure_error("Posting descriptor or page disagrees with the pinned FieldIndex"));
  }
  if (page.previous_page_id == 0) != (page.page_id == field.first_page_id)
    || (page.next_page_id == 0) != (page.page_id == field.last_page_id)
  {
    return Err(lookup_closure_error("Posting page endpoint links disagree with the pinned FieldIndex"));
  }
  Ok(())
}

fn validate_nvt_tile_path<'a>(
  basis: &PinnedFieldNvtV1<'_>,
  lookup_key: &[u8],
  path: &ImmutableIndexPathV1<'a>,
  limits: SparseNvtLookupLimitsV1,
) -> FormatResult<NvtTileV1<'a>> {
  let root = nvt_root_expectation(basis)?;
  let descriptor = validate_directory_path(root, PathSelectionV1::OrderKey(lookup_key), path, limits)?
    .ok_or_else(|| lookup_closure_error("NVT directory predecessor path selected no descriptor"))?;
  let tile = decode_nvt_tile(path.leaf, basis.hash_algorithm)?;
  let tile_fence = tile.tile_start_cell.to_le_bytes();
  if tile.owner_id != basis.owner_id
    || tile.generation > basis.generation
    || tile.resolution != basis.resolution
    || tile.tile_cell_count != basis.tile_cells
    || tile.basis_posting_generation != basis.basis_posting_generation
    || descriptor.child_hash != tile.key
    || descriptor.child_generation != tile.generation
    || descriptor.lower_fence != tile_fence
    || descriptor.upper_fence != tile_fence
    || descriptor.live_count != checked_u64(tile.entries.len(), "NVT tile entry count")?
    || descriptor.tombstone_count != 0
    || descriptor.page_count != 1
    || descriptor.logical_bytes != checked_u64(path.leaf.len(), "NVT tile encoded length")?
    || descriptor.minimum_page_id != 0
    || descriptor.maximum_page_id != 0
  {
    return Err(lookup_closure_error("NVT tile descriptor or basis closure disagrees"));
  }
  Ok(tile)
}

fn validate_posting_position(hash_algorithm: HashAlgorithm, key: &[u8]) -> FormatResult<()> {
  compare_order_keys(hash_algorithm, OrderedIndexRoleV1::Posting, key, key).map(|_| ())
}

fn validate_lookup_limits(limits: SparseNvtLookupLimitsV1) -> FormatResult<()> {
  if limits.maximum_directory_depth == 0
    || limits.maximum_directory_depth > 16
    || limits.maximum_tile_candidates == 0
    || limits.maximum_tile_candidates > MAX_NVT_LOOKUP_TILE_CANDIDATES
    || limits.maximum_input_bytes == 0
    || limits.maximum_input_bytes > MAX_NVT_LOOKUP_INPUT_BYTES
  {
    return Err(lookup_amplification_error("NVT lookup limits are outside the admitted range"));
  }
  Ok(())
}

fn validate_paths_input_bytes(paths: &[ImmutableIndexPathV1<'_>], limits: SparseNvtLookupLimitsV1) -> FormatResult<()> {
  let mut total = 0usize;
  for path in paths {
    total =
      total.checked_add(path_input_bytes(path)?).ok_or_else(|| lookup_length_error("NVT lookup aggregate input-byte count overflow"))?;
    if total > limits.maximum_input_bytes {
      return Err(lookup_amplification_error("NVT lookup aggregate input bytes exceed the admitted limit"));
    }
  }
  Ok(())
}

fn validate_path_input_bytes(path: &ImmutableIndexPathV1<'_>, limits: SparseNvtLookupLimitsV1) -> FormatResult<()> {
  if path.directories.len() > limits.maximum_directory_depth {
    return Err(lookup_amplification_error("immutable index path exceeds the directory-depth limit"));
  }
  if path_input_bytes(path)? > limits.maximum_input_bytes {
    return Err(lookup_amplification_error("immutable index path exceeds the input-byte limit"));
  }
  Ok(())
}

fn path_input_bytes(path: &ImmutableIndexPathV1<'_>) -> FormatResult<usize> {
  path.directories.iter().try_fold(path.leaf.len(), |total, bytes| {
    total.checked_add(bytes.len()).ok_or_else(|| lookup_length_error("index path byte count overflow"))
  })
}

fn missing_nvt_hint() -> NvtHintSelectionV1 {
  NvtHintSelectionV1 { hint: None, fallback: Some(NvtFallbackV1 { reason: NvtFallbackReasonV1::MissingPredecessor, diagnostic: None }) }
}

fn failed_nvt_hint(reason: NvtFallbackReasonV1, diagnostic: FormatError) -> NvtHintSelectionV1 {
  NvtHintSelectionV1 { hint: None, fallback: Some(NvtFallbackV1 { reason, diagnostic: Some(diagnostic) }) }
}

fn nvt_error_reason(error: &FormatError) -> NvtFallbackReasonV1 {
  if error.class() == MalformedInputClass::AllocationAmplification {
    NvtFallbackReasonV1::ResourceLimit
  } else {
    NvtFallbackReasonV1::Corrupt
  }
}

fn unavailable_nvt_basis<'a>(reason: NvtFallbackReasonV1, diagnostic: Option<FormatError>) -> NvtBasisStatusV1<'a> {
  NvtBasisStatusV1::Unavailable(NvtFallbackV1 { reason, diagnostic })
}

fn lookup_closure_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, "nvt_lookup_closure", context)
}

fn lookup_length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "nvt_lookup_arithmetic", context)
}

fn lookup_amplification_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, "nvt_lookup_bound", context)
}

fn lookup_order_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "nvt_lookup_order", context)
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
