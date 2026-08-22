use crate::engine::HashAlgorithm;

use super::field_definition::decode_field_index_definition;
use super::index_artifact::{IndexManifestKindV1, u16_at, u32_at, u64_at};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::scope::decode_scope_definition;
use super::value_store::decode_value_store_definition;

const CAPABILITY_WIDTH: usize = 32;
const SCOPE_DEFINITION_CAP: usize = 65_536;
const VALUE_STORE_DEFINITION_CAP: usize = 512 * 1_024;
const FIELD_INDEX_DEFINITION_CAP: usize = 256 * 1_024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageVersionV1<'a> {
  pub source_namespace_root: &'a [u8],
  pub coverage_epoch_id: &'a [u8],
  pub coverage_publication_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeCatalogManifestBodyV1<'a> {
  pub required_reader_capabilities: [u8; CAPABILITY_WIDTH],
  pub coverage: CoverageVersionV1<'a>,
  pub next_document_ordinal: u64,
  pub ordinal_directory_root: Option<&'a [u8]>,
  pub reverse_directory_root: Option<&'a [u8]>,
  pub live_document_count: u64,
  pub retained_tombstone_count: u64,
  pub ordinal_page_count: u64,
  pub reverse_page_count: u64,
  pub scope_definition: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueStoreManifestBodyV1<'a> {
  pub required_reader_capabilities: [u8; CAPABILITY_WIDTH],
  pub coverage: CoverageVersionV1<'a>,
  pub scope_catalog_manifest: &'a [u8],
  pub value_directory_root: Option<&'a [u8]>,
  pub document_state_directory_root: Option<&'a [u8]>,
  pub next_page_id: u64,
  pub value_page_count: u64,
  pub state_page_count: u64,
  pub value_document_count: u64,
  pub unindexable_document_count: u64,
  pub live_value_count: u64,
  pub value_tombstone_count: u64,
  pub state_tombstone_count: u64,
  pub live_canonical_value_bytes: u64,
  pub value_store_definition: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldIndexManifestBodyV1<'a> {
  pub required_reader_capabilities: [u8; CAPABILITY_WIDTH],
  pub coverage: CoverageVersionV1<'a>,
  pub value_store_manifest: &'a [u8],
  pub posting_directory_root: Option<&'a [u8]>,
  pub document_state_directory_root: Option<&'a [u8]>,
  pub first_page_id: u64,
  pub last_page_id: u64,
  pub next_page_id: u64,
  pub posting_page_count: u64,
  pub state_page_count: u64,
  pub live_posting_count: u64,
  pub posting_tombstone_count: u64,
  pub posting_document_count: u64,
  pub unindexable_document_count: u64,
  pub state_tombstone_count: u64,
  pub live_canonical_posting_bytes: u64,
  pub field_index_definition: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldNvtManifestBodyV1<'a> {
  pub required_reader_capabilities: [u8; CAPABILITY_WIDTH],
  pub tile_cells: u32,
  pub resolution: u64,
  pub basis_posting_generation: u64,
  pub basis_source_head_hash: &'a [u8],
  pub tile_directory_root: Option<&'a [u8]>,
  pub tile_count: u64,
  pub populated_cell_count: u64,
  pub approximate_live_posting_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexManifestBodyV1<'a> {
  ScopeCatalog(ScopeCatalogManifestBodyV1<'a>),
  ValueStore(ValueStoreManifestBodyV1<'a>),
  FieldIndex(FieldIndexManifestBodyV1<'a>),
  FieldNvt(FieldNvtManifestBodyV1<'a>),
}

impl<'a> IndexManifestBodyV1<'a> {
  pub fn kind(&self) -> IndexManifestKindV1 {
    match self {
      Self::ScopeCatalog(_) => IndexManifestKindV1::ScopeCatalog,
      Self::ValueStore(_) => IndexManifestKindV1::ValueStore,
      Self::FieldIndex(_) => IndexManifestKindV1::FieldIndex,
      Self::FieldNvt(_) => IndexManifestKindV1::FieldNvt,
    }
  }

  pub fn populated(&self) -> bool {
    match self {
      Self::ScopeCatalog(body) => body.ordinal_directory_root.is_some() || body.reverse_directory_root.is_some(),
      Self::ValueStore(body) => body.value_directory_root.is_some() || body.document_state_directory_root.is_some(),
      Self::FieldIndex(body) => body.posting_directory_root.is_some() || body.document_state_directory_root.is_some(),
      Self::FieldNvt(body) => body.tile_directory_root.is_some(),
    }
  }

  pub fn definition(&self) -> Option<&'a [u8]> {
    match self {
      Self::ScopeCatalog(body) => Some(body.scope_definition),
      Self::ValueStore(body) => Some(body.value_store_definition),
      Self::FieldIndex(body) => Some(body.field_index_definition),
      Self::FieldNvt(_) => None,
    }
  }

  pub fn coverage(&self) -> Option<&CoverageVersionV1<'_>> {
    match self {
      Self::ScopeCatalog(body) => Some(&body.coverage),
      Self::ValueStore(body) => Some(&body.coverage),
      Self::FieldIndex(body) => Some(&body.coverage),
      Self::FieldNvt(_) => None,
    }
  }
}

pub(crate) fn decode_index_manifest_body<'a>(
  kind: IndexManifestKindV1,
  body: &'a [u8],
  owner_id: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<IndexManifestBodyV1<'a>> {
  match kind {
    IndexManifestKindV1::ScopeCatalog => decode_scope_manifest_body(body, owner_id, hash_algorithm).map(IndexManifestBodyV1::ScopeCatalog),
    IndexManifestKindV1::ValueStore => decode_value_manifest_body(body, owner_id, hash_algorithm).map(IndexManifestBodyV1::ValueStore),
    IndexManifestKindV1::FieldIndex => decode_field_manifest_body(body, owner_id, hash_algorithm).map(IndexManifestBodyV1::FieldIndex),
    IndexManifestKindV1::FieldNvt => decode_nvt_manifest_body(body, hash_algorithm).map(IndexManifestBodyV1::FieldNvt),
  }
}

pub(crate) fn encode_index_manifest_body(
  body: &IndexManifestBodyV1<'_>,
  owner_id: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  validate_hash(owner_id, hash_algorithm, "manifest owner")?;
  let encoded = match body {
    IndexManifestBodyV1::ScopeCatalog(body) => encode_scope_manifest_body(body, hash_algorithm)?,
    IndexManifestBodyV1::ValueStore(body) => encode_value_manifest_body(body, hash_algorithm)?,
    IndexManifestBodyV1::FieldIndex(body) => encode_field_manifest_body(body, hash_algorithm)?,
    IndexManifestBodyV1::FieldNvt(body) => encode_nvt_manifest_body(body, hash_algorithm)?,
  };
  decode_index_manifest_body(body.kind(), &encoded, owner_id, hash_algorithm)?;
  Ok(encoded)
}

fn decode_scope_manifest_body<'a>(
  body: &'a [u8],
  owner_id: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<ScopeCatalogManifestBodyV1<'a>> {
  let hash_width = hash_algorithm.hash_length();
  let definition_start = 112 + 3 * hash_width;
  let (required_reader_capabilities, coverage, definition) =
    decode_correctness_prefix(body, hash_width, definition_start, SCOPE_DEFINITION_CAP)?;
  require_v1_codecs(body, &[64 + hash_width, 66 + hash_width], "scope manifest")?;
  if body[69 + hash_width..72 + hash_width].iter().any(|byte| *byte != 0) {
    return Err(reserve_error("scope manifest reserve is nonzero"));
  }
  let presence = body[68 + hash_width];
  require_known_presence(presence, 0x03, "scope manifest")?;
  let next_document_ordinal = u64_at(body, 72 + hash_width)?;
  if next_document_ordinal == 0 {
    return Err(identity_error("scope manifest next document ordinal is zero"));
  }
  let ordinal_directory_root = decode_root(presence, 1, &body[80 + hash_width..80 + 2 * hash_width])?;
  let reverse_directory_root = decode_root(presence, 2, &body[80 + 2 * hash_width..80 + 3 * hash_width])?;
  let live_document_count = u64_at(body, 80 + 3 * hash_width)?;
  let retained_tombstone_count = u64_at(body, 88 + 3 * hash_width)?;
  let ordinal_page_count = u64_at(body, 96 + 3 * hash_width)?;
  let reverse_page_count = u64_at(body, 104 + 3 * hash_width)?;
  if (ordinal_directory_root.is_none() && (live_document_count != 0 || retained_tombstone_count != 0 || ordinal_page_count != 0))
    || (ordinal_directory_root.is_some() && ordinal_page_count == 0)
    || (reverse_directory_root.is_none() && (live_document_count != 0 || reverse_page_count != 0))
    || (reverse_directory_root.is_some() && (live_document_count == 0 || reverse_page_count == 0))
  {
    return Err(closure_error("scope manifest roots and counts disagree"));
  }
  let scope = decode_scope_definition(definition, hash_algorithm).map_err(|source| nested_definition_error("scope", source))?;
  if scope.scope_id != owner_id {
    return Err(closure_error("embedded ScopeDefinition does not derive the manifest owner"));
  }
  Ok(ScopeCatalogManifestBodyV1 {
    required_reader_capabilities,
    coverage,
    next_document_ordinal,
    ordinal_directory_root,
    reverse_directory_root,
    live_document_count,
    retained_tombstone_count,
    ordinal_page_count,
    reverse_page_count,
    scope_definition: definition,
  })
}

fn decode_value_manifest_body<'a>(
  body: &'a [u8],
  owner_id: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<ValueStoreManifestBodyV1<'a>> {
  let hash_width = hash_algorithm.hash_length();
  let definition_start = 144 + 4 * hash_width;
  let (required_reader_capabilities, coverage, definition) =
    decode_correctness_prefix(body, hash_width, definition_start, VALUE_STORE_DEFINITION_CAP)?;
  require_v1_codecs(body, &[64 + hash_width, 66 + hash_width, 68 + hash_width], "value manifest")?;
  if body[71 + hash_width] != 0 {
    return Err(reserve_error("value manifest reserve is nonzero"));
  }
  let presence = body[70 + hash_width];
  require_known_presence(presence, 0x03, "value manifest")?;
  let scope_catalog_manifest = &body[72 + hash_width..72 + 2 * hash_width];
  validate_hash(scope_catalog_manifest, hash_algorithm, "value manifest ScopeCatalog reference")?;
  let value_directory_root = decode_root(presence, 1, &body[72 + 2 * hash_width..72 + 3 * hash_width])?;
  let document_state_directory_root = decode_root(presence, 2, &body[72 + 3 * hash_width..72 + 4 * hash_width])?;
  let next_page_id = u64_at(body, 72 + 4 * hash_width)?;
  if next_page_id == 0 {
    return Err(identity_error("value manifest next page ID is zero"));
  }
  let value_page_count = u64_at(body, 80 + 4 * hash_width)?;
  let state_page_count = u64_at(body, 88 + 4 * hash_width)?;
  let value_document_count = u64_at(body, 96 + 4 * hash_width)?;
  let unindexable_document_count = u64_at(body, 104 + 4 * hash_width)?;
  let live_value_count = u64_at(body, 112 + 4 * hash_width)?;
  let value_tombstone_count = u64_at(body, 120 + 4 * hash_width)?;
  let state_tombstone_count = u64_at(body, 128 + 4 * hash_width)?;
  let live_canonical_value_bytes = u64_at(body, 136 + 4 * hash_width)?;
  let value_live_counts_disagree = (value_document_count == 0) != (live_value_count == 0);
  if (value_directory_root.is_none()
    && [value_page_count, value_document_count, live_value_count, value_tombstone_count, live_canonical_value_bytes]
      .iter()
      .any(|count| *count != 0))
    || (value_directory_root.is_some()
      && (value_page_count == 0
        || (live_value_count == 0 && value_tombstone_count == 0)
        || value_live_counts_disagree
        || value_document_count > live_value_count))
    || (document_state_directory_root.is_none()
      && [state_page_count, unindexable_document_count, state_tombstone_count].iter().any(|count| *count != 0))
    || (document_state_directory_root.is_some()
      && (state_page_count == 0 || (unindexable_document_count == 0 && state_tombstone_count == 0)))
  {
    return Err(closure_error("value manifest roots and counts disagree"));
  }
  let value_store =
    decode_value_store_definition(definition, hash_algorithm).map_err(|source| nested_definition_error("value-store", source))?;
  if value_store.value_store_id != owner_id {
    return Err(closure_error("embedded ValueStoreDefinition does not derive the manifest owner"));
  }
  Ok(ValueStoreManifestBodyV1 {
    required_reader_capabilities,
    coverage,
    scope_catalog_manifest,
    value_directory_root,
    document_state_directory_root,
    next_page_id,
    value_page_count,
    state_page_count,
    value_document_count,
    unindexable_document_count,
    live_value_count,
    value_tombstone_count,
    state_tombstone_count,
    live_canonical_value_bytes,
    value_store_definition: definition,
  })
}

fn decode_field_manifest_body<'a>(
  body: &'a [u8],
  owner_id: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<FieldIndexManifestBodyV1<'a>> {
  let hash_width = hash_algorithm.hash_length();
  let definition_start = 160 + 4 * hash_width;
  let (required_reader_capabilities, coverage, definition) =
    decode_correctness_prefix(body, hash_width, definition_start, FIELD_INDEX_DEFINITION_CAP)?;
  require_v1_codecs(body, &[64 + hash_width, 66 + hash_width, 68 + hash_width], "field manifest")?;
  if body[71 + hash_width] != 0 {
    return Err(reserve_error("field manifest reserve is nonzero"));
  }
  let presence = body[70 + hash_width];
  require_known_presence(presence, 0x03, "field manifest")?;
  let value_store_manifest = &body[72 + hash_width..72 + 2 * hash_width];
  validate_hash(value_store_manifest, hash_algorithm, "field manifest ValueStore reference")?;
  let posting_directory_root = decode_root(presence, 1, &body[72 + 2 * hash_width..72 + 3 * hash_width])?;
  let document_state_directory_root = decode_root(presence, 2, &body[72 + 3 * hash_width..72 + 4 * hash_width])?;
  let first_page_id = u64_at(body, 72 + 4 * hash_width)?;
  let last_page_id = u64_at(body, 80 + 4 * hash_width)?;
  let next_page_id = u64_at(body, 88 + 4 * hash_width)?;
  if next_page_id == 0 {
    return Err(identity_error("field manifest next page ID is zero"));
  }
  let posting_page_count = u64_at(body, 96 + 4 * hash_width)?;
  let state_page_count = u64_at(body, 104 + 4 * hash_width)?;
  let live_posting_count = u64_at(body, 112 + 4 * hash_width)?;
  let posting_tombstone_count = u64_at(body, 120 + 4 * hash_width)?;
  let posting_document_count = u64_at(body, 128 + 4 * hash_width)?;
  let unindexable_document_count = u64_at(body, 136 + 4 * hash_width)?;
  let state_tombstone_count = u64_at(body, 144 + 4 * hash_width)?;
  let live_canonical_posting_bytes = u64_at(body, 152 + 4 * hash_width)?;
  let posting_live_counts_disagree = (posting_document_count == 0) != (live_posting_count == 0);
  if (posting_directory_root.is_none()
    && (first_page_id != 0
      || last_page_id != 0
      || [posting_page_count, live_posting_count, posting_tombstone_count, posting_document_count, live_canonical_posting_bytes]
        .iter()
        .any(|count| *count != 0)))
    || (posting_directory_root.is_some()
      && (first_page_id == 0
        || last_page_id == 0
        || first_page_id > last_page_id
        || next_page_id <= last_page_id
        || posting_page_count == 0
        || (live_posting_count == 0 && posting_tombstone_count == 0)
        || posting_live_counts_disagree
        || posting_document_count > live_posting_count))
    || (document_state_directory_root.is_none()
      && [state_page_count, unindexable_document_count, state_tombstone_count].iter().any(|count| *count != 0))
    || (document_state_directory_root.is_some()
      && (state_page_count == 0 || (unindexable_document_count == 0 && state_tombstone_count == 0)))
  {
    return Err(closure_error("field manifest roots, page IDs, and counts disagree"));
  }
  let field_index =
    decode_field_index_definition(definition, hash_algorithm).map_err(|source| nested_definition_error("field-index", source))?;
  if field_index.index_id != owner_id {
    return Err(closure_error("embedded FieldIndexDefinition does not derive the manifest owner"));
  }
  Ok(FieldIndexManifestBodyV1 {
    required_reader_capabilities,
    coverage,
    value_store_manifest,
    posting_directory_root,
    document_state_directory_root,
    first_page_id,
    last_page_id,
    next_page_id,
    posting_page_count,
    state_page_count,
    live_posting_count,
    posting_tombstone_count,
    posting_document_count,
    unindexable_document_count,
    state_tombstone_count,
    live_canonical_posting_bytes,
    field_index_definition: definition,
  })
}

fn decode_nvt_manifest_body(body: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<FieldNvtManifestBodyV1<'_>> {
  let hash_width = hash_algorithm.hash_length();
  let expected_length = 88usize.checked_add(2 * hash_width).ok_or_else(|| length_error("NVT manifest length overflow"))?;
  if body.len() != expected_length {
    return Err(truncated_error(format!("expected {expected_length} NVT body bytes, got {}", body.len())));
  }
  if u32_at(body, 0)? != 0 {
    return Err(reserve_error("NVT manifest flags are nonzero"));
  }
  let required_reader_capabilities = decode_capabilities(&body[4..36])?;
  require_v1_codecs(body, &[36, 38], "NVT manifest")?;
  let tile_cells = u32_at(body, 40)?;
  let resolution = u64_at(body, 48)?;
  let basis_posting_generation = u64_at(body, 56)?;
  let basis_source_head_hash = &body[64..64 + hash_width];
  if tile_cells == 0
    || !tile_cells.is_power_of_two()
    || resolution == 0
    || u64::from(tile_cells) > resolution
    || !resolution.is_multiple_of(u64::from(tile_cells))
    || basis_posting_generation == 0
  {
    return Err(closure_error("NVT manifest resolution or basis generation is invalid"));
  }
  validate_hash(basis_source_head_hash, hash_algorithm, "NVT basis source root")?;
  if body[45..48].iter().any(|byte| *byte != 0) {
    return Err(reserve_error("NVT manifest reserve is nonzero"));
  }
  let presence = body[44];
  require_known_presence(presence, 1, "NVT manifest")?;
  let tile_directory_root = decode_root(presence, 1, &body[64 + hash_width..64 + 2 * hash_width])?;
  let tile_count = u64_at(body, 64 + 2 * hash_width)?;
  let populated_cell_count = u64_at(body, 72 + 2 * hash_width)?;
  let approximate_live_posting_count = u64_at(body, 80 + 2 * hash_width)?;
  if tile_count > resolution / u64::from(tile_cells)
    || populated_cell_count > resolution
    || (tile_directory_root.is_none() && (tile_count != 0 || populated_cell_count != 0))
    || (tile_directory_root.is_some() && (tile_count == 0 || populated_cell_count == 0))
  {
    return Err(closure_error("NVT manifest root and counts disagree"));
  }
  Ok(FieldNvtManifestBodyV1 {
    required_reader_capabilities,
    tile_cells,
    resolution,
    basis_posting_generation,
    basis_source_head_hash,
    tile_directory_root,
    tile_count,
    populated_cell_count,
    approximate_live_posting_count,
  })
}

fn encode_scope_manifest_body(body: &ScopeCatalogManifestBodyV1<'_>, hash_algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_definition_length(body.scope_definition.len(), SCOPE_DEFINITION_CAP, "scope definition")?;
  let hash_width = hash_algorithm.hash_length();
  let length = 112usize
    .checked_add(3 * hash_width)
    .and_then(|value| value.checked_add(body.scope_definition.len()))
    .ok_or_else(|| length_error("scope manifest body length overflow"))?;
  let mut encoded = allocate_zeroed(length)?;
  encode_correctness_prefix(&mut encoded, &body.required_reader_capabilities, &body.coverage, body.scope_definition, hash_algorithm)?;
  write_u16(&mut encoded, 64 + hash_width, 1)?;
  write_u16(&mut encoded, 66 + hash_width, 1)?;
  let mut presence = 0;
  encode_root(&mut encoded[80 + hash_width..80 + 2 * hash_width], body.ordinal_directory_root, hash_algorithm, 1, &mut presence)?;
  encode_root(&mut encoded[80 + 2 * hash_width..80 + 3 * hash_width], body.reverse_directory_root, hash_algorithm, 2, &mut presence)?;
  encoded[68 + hash_width] = presence;
  write_u64(&mut encoded, 72 + hash_width, body.next_document_ordinal)?;
  for (offset, value) in [
    (80 + 3 * hash_width, body.live_document_count),
    (88 + 3 * hash_width, body.retained_tombstone_count),
    (96 + 3 * hash_width, body.ordinal_page_count),
    (104 + 3 * hash_width, body.reverse_page_count),
  ] {
    write_u64(&mut encoded, offset, value)?;
  }
  encoded[112 + 3 * hash_width..].copy_from_slice(body.scope_definition);
  Ok(encoded)
}

fn encode_value_manifest_body(body: &ValueStoreManifestBodyV1<'_>, hash_algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_definition_length(body.value_store_definition.len(), VALUE_STORE_DEFINITION_CAP, "value-store definition")?;
  let hash_width = hash_algorithm.hash_length();
  validate_hash(body.scope_catalog_manifest, hash_algorithm, "value manifest ScopeCatalog reference")?;
  let length = 144usize
    .checked_add(4 * hash_width)
    .and_then(|value| value.checked_add(body.value_store_definition.len()))
    .ok_or_else(|| length_error("value manifest body length overflow"))?;
  let mut encoded = allocate_zeroed(length)?;
  encode_correctness_prefix(&mut encoded, &body.required_reader_capabilities, &body.coverage, body.value_store_definition, hash_algorithm)?;
  for offset in [64 + hash_width, 66 + hash_width, 68 + hash_width] {
    write_u16(&mut encoded, offset, 1)?;
  }
  encoded[72 + hash_width..72 + 2 * hash_width].copy_from_slice(body.scope_catalog_manifest);
  let mut presence = 0;
  encode_root(&mut encoded[72 + 2 * hash_width..72 + 3 * hash_width], body.value_directory_root, hash_algorithm, 1, &mut presence)?;
  encode_root(
    &mut encoded[72 + 3 * hash_width..72 + 4 * hash_width],
    body.document_state_directory_root,
    hash_algorithm,
    2,
    &mut presence,
  )?;
  encoded[70 + hash_width] = presence;
  for (offset, value) in [
    (72 + 4 * hash_width, body.next_page_id),
    (80 + 4 * hash_width, body.value_page_count),
    (88 + 4 * hash_width, body.state_page_count),
    (96 + 4 * hash_width, body.value_document_count),
    (104 + 4 * hash_width, body.unindexable_document_count),
    (112 + 4 * hash_width, body.live_value_count),
    (120 + 4 * hash_width, body.value_tombstone_count),
    (128 + 4 * hash_width, body.state_tombstone_count),
    (136 + 4 * hash_width, body.live_canonical_value_bytes),
  ] {
    write_u64(&mut encoded, offset, value)?;
  }
  encoded[144 + 4 * hash_width..].copy_from_slice(body.value_store_definition);
  Ok(encoded)
}

fn encode_field_manifest_body(body: &FieldIndexManifestBodyV1<'_>, hash_algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_definition_length(body.field_index_definition.len(), FIELD_INDEX_DEFINITION_CAP, "field-index definition")?;
  let hash_width = hash_algorithm.hash_length();
  validate_hash(body.value_store_manifest, hash_algorithm, "field manifest ValueStore reference")?;
  let length = 160usize
    .checked_add(4 * hash_width)
    .and_then(|value| value.checked_add(body.field_index_definition.len()))
    .ok_or_else(|| length_error("field manifest body length overflow"))?;
  let mut encoded = allocate_zeroed(length)?;
  encode_correctness_prefix(&mut encoded, &body.required_reader_capabilities, &body.coverage, body.field_index_definition, hash_algorithm)?;
  for offset in [64 + hash_width, 66 + hash_width, 68 + hash_width] {
    write_u16(&mut encoded, offset, 1)?;
  }
  encoded[72 + hash_width..72 + 2 * hash_width].copy_from_slice(body.value_store_manifest);
  let mut presence = 0;
  encode_root(&mut encoded[72 + 2 * hash_width..72 + 3 * hash_width], body.posting_directory_root, hash_algorithm, 1, &mut presence)?;
  encode_root(
    &mut encoded[72 + 3 * hash_width..72 + 4 * hash_width],
    body.document_state_directory_root,
    hash_algorithm,
    2,
    &mut presence,
  )?;
  encoded[70 + hash_width] = presence;
  for (offset, value) in [
    (72 + 4 * hash_width, body.first_page_id),
    (80 + 4 * hash_width, body.last_page_id),
    (88 + 4 * hash_width, body.next_page_id),
    (96 + 4 * hash_width, body.posting_page_count),
    (104 + 4 * hash_width, body.state_page_count),
    (112 + 4 * hash_width, body.live_posting_count),
    (120 + 4 * hash_width, body.posting_tombstone_count),
    (128 + 4 * hash_width, body.posting_document_count),
    (136 + 4 * hash_width, body.unindexable_document_count),
    (144 + 4 * hash_width, body.state_tombstone_count),
    (152 + 4 * hash_width, body.live_canonical_posting_bytes),
  ] {
    write_u64(&mut encoded, offset, value)?;
  }
  encoded[160 + 4 * hash_width..].copy_from_slice(body.field_index_definition);
  Ok(encoded)
}

fn encode_nvt_manifest_body(body: &FieldNvtManifestBodyV1<'_>, hash_algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let hash_width = hash_algorithm.hash_length();
  validate_capabilities(&body.required_reader_capabilities)?;
  validate_hash(body.basis_source_head_hash, hash_algorithm, "NVT basis source root")?;
  let length = 88usize.checked_add(2 * hash_width).ok_or_else(|| length_error("NVT manifest body length overflow"))?;
  let mut encoded = allocate_zeroed(length)?;
  encoded[4..36].copy_from_slice(&body.required_reader_capabilities);
  write_u16(&mut encoded, 36, 1)?;
  write_u16(&mut encoded, 38, 1)?;
  write_u32(&mut encoded, 40, body.tile_cells)?;
  let mut presence = 0;
  encode_root(&mut encoded[64 + hash_width..64 + 2 * hash_width], body.tile_directory_root, hash_algorithm, 1, &mut presence)?;
  encoded[44] = presence;
  write_u64(&mut encoded, 48, body.resolution)?;
  write_u64(&mut encoded, 56, body.basis_posting_generation)?;
  encoded[64..64 + hash_width].copy_from_slice(body.basis_source_head_hash);
  write_u64(&mut encoded, 64 + 2 * hash_width, body.tile_count)?;
  write_u64(&mut encoded, 72 + 2 * hash_width, body.populated_cell_count)?;
  write_u64(&mut encoded, 80 + 2 * hash_width, body.approximate_live_posting_count)?;
  Ok(encoded)
}

fn decode_correctness_prefix<'a>(
  body: &'a [u8],
  hash_width: usize,
  definition_start: usize,
  definition_cap: usize,
) -> FormatResult<([u8; CAPABILITY_WIDTH], CoverageVersionV1<'a>, &'a [u8])> {
  if body.len() < definition_start {
    return Err(truncated_error("manifest body is truncated"));
  }
  if u32_at(body, 0)? != 0 {
    return Err(reserve_error("manifest flags are nonzero"));
  }
  let required_reader_capabilities = decode_capabilities(&body[4..36])?;
  let definition_length = usize::try_from(u32_at(body, 36)?).map_err(|_| length_error("definition length conversion"))?;
  validate_definition_length(definition_length, definition_cap, "manifest definition")?;
  let definition_end = definition_start.checked_add(definition_length).ok_or_else(|| length_error("definition end overflow"))?;
  if definition_end != body.len() {
    return Err(truncated_error("embedded definition does not consume the manifest body"));
  }
  let source_namespace_root = &body[40..40 + hash_width];
  let coverage_epoch_id = &body[40 + hash_width..56 + hash_width];
  if source_namespace_root.iter().all(|byte| *byte == 0) || coverage_epoch_id.iter().all(|byte| *byte == 0) {
    return Err(identity_error("manifest coverage namespace root or epoch ID is zero"));
  }
  Ok((
    required_reader_capabilities,
    CoverageVersionV1 { source_namespace_root, coverage_epoch_id, coverage_publication_sequence: u64_at(body, 56 + hash_width)? },
    &body[definition_start..definition_end],
  ))
}

fn encode_correctness_prefix(
  encoded: &mut [u8],
  capabilities: &[u8; CAPABILITY_WIDTH],
  coverage: &CoverageVersionV1<'_>,
  definition: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<()> {
  validate_capabilities(capabilities)?;
  validate_hash(coverage.source_namespace_root, hash_algorithm, "manifest source namespace root")?;
  if coverage.coverage_epoch_id.len() != 16 || coverage.coverage_epoch_id.iter().all(|byte| *byte == 0) {
    return Err(identity_error("manifest coverage epoch ID has the wrong width or is zero"));
  }
  encoded[4..36].copy_from_slice(capabilities);
  write_u32(encoded, 36, checked_u32(definition.len(), "manifest definition length")?)?;
  let hash_width = hash_algorithm.hash_length();
  encoded[40..40 + hash_width].copy_from_slice(coverage.source_namespace_root);
  encoded[40 + hash_width..56 + hash_width].copy_from_slice(coverage.coverage_epoch_id);
  write_u64(encoded, 56 + hash_width, coverage.coverage_publication_sequence)
}

fn decode_capabilities(value: &[u8]) -> FormatResult<[u8; CAPABILITY_WIDTH]> {
  validate_capabilities(value)?;
  value.try_into().map_err(|source| truncated_error(format!("validated capability width conversion failed: {source}")))
}

fn validate_capabilities(value: &[u8]) -> FormatResult<()> {
  if value.len() != CAPABILITY_WIDTH {
    return Err(truncated_error("manifest capability bitset is not 32 bytes"));
  }
  if value[3..].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::UnknownRequiredCapability,
      "index_manifest_unknown_capability",
      "capability bit 24 or later is not recognized",
    ));
  }
  Ok(())
}

fn require_v1_codecs(body: &[u8], offsets: &[usize], context: &'static str) -> FormatResult<()> {
  for offset in offsets {
    if u16_at(body, *offset)? != 1 {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "index_manifest_codec", format!("{context} codec is not v1")));
    }
  }
  Ok(())
}

fn require_known_presence(presence: u8, known: u8, context: &'static str) -> FormatResult<()> {
  if presence & !known != 0 {
    return Err(error(
      MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
      "index_manifest_presence",
      format!("{context} contains unknown presence bits"),
    ));
  }
  Ok(())
}

fn decode_root(presence: u8, bit: u8, root: &[u8]) -> FormatResult<Option<&[u8]>> {
  let present = presence & bit != 0;
  let zero = root.iter().all(|byte| *byte == 0);
  if present == zero {
    return Err(closure_error("manifest root presence bit and hash disagree"));
  }
  Ok(present.then_some(root))
}

fn encode_root(destination: &mut [u8], root: Option<&[u8]>, hash_algorithm: HashAlgorithm, bit: u8, presence: &mut u8) -> FormatResult<()> {
  if destination.len() != hash_algorithm.hash_length() {
    return Err(length_error("manifest root destination has the wrong width"));
  }
  if let Some(root) = root {
    validate_hash(root, hash_algorithm, "manifest root")?;
    destination.copy_from_slice(root);
    *presence |= bit;
  }
  Ok(())
}

fn validate_definition_length(length: usize, cap: usize, context: &'static str) -> FormatResult<()> {
  if length > cap {
    return Err(amplification_error(format!("{context} length {length} exceeds {cap}")));
  }
  Ok(())
}

fn validate_hash(value: &[u8], hash_algorithm: HashAlgorithm, context: &'static str) -> FormatResult<()> {
  if value.len() != hash_algorithm.hash_length() || value.iter().all(|byte| *byte == 0) {
    return Err(identity_error(format!("{context} has the wrong width or is all zero")));
  }
  Ok(())
}

fn allocate_zeroed(length: usize) -> FormatResult<Vec<u8>> {
  let mut value = Vec::new();
  value
    .try_reserve_exact(length)
    .map_err(|source| amplification_error(format!("manifest allocation of {length} bytes failed: {source}")))?;
  value.resize(length, 0);
  Ok(value)
}

fn checked_u32(value: usize, context: &'static str) -> FormatResult<u32> {
  u32::try_from(value).map_err(|source| length_error(format!("{context} does not fit u32: {source}")))
}

fn write_u16(destination: &mut [u8], offset: usize, value: u16) -> FormatResult<()> {
  let target = destination.get_mut(offset..offset + 2).ok_or_else(|| truncated_error("u16 write exceeds manifest body"))?;
  target.copy_from_slice(&value.to_le_bytes());
  Ok(())
}

fn write_u32(destination: &mut [u8], offset: usize, value: u32) -> FormatResult<()> {
  let target = destination.get_mut(offset..offset + 4).ok_or_else(|| truncated_error("u32 write exceeds manifest body"))?;
  target.copy_from_slice(&value.to_le_bytes());
  Ok(())
}

fn write_u64(destination: &mut [u8], offset: usize, value: u64) -> FormatResult<()> {
  let target = destination.get_mut(offset..offset + 8).ok_or_else(|| truncated_error("u64 write exceeds manifest body"))?;
  target.copy_from_slice(&value.to_le_bytes());
  Ok(())
}

fn nested_definition_error(label: &'static str, source: FormatError) -> FormatError {
  closure_error(format!("embedded {label} definition rejected: {} ({})", source.code(), source.context()))
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}

fn truncated_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, "index_manifest_length", context)
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "index_manifest_arithmetic", context)
}

fn amplification_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::AllocationAmplification, "index_manifest_bound", context)
}

fn reserve_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::NonzeroReservedOrPadding, "index_manifest_reserved", context)
}

fn identity_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "index_manifest_identity", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, "index_manifest_closure", context)
}
