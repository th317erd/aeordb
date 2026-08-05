use crate::engine::HashAlgorithm;

use super::field_definition::decode_field_index_definition;
use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::scope::decode_scope_definition;
use super::value_store::decode_value_store_definition;

pub(crate) const INDEX_ENVELOPE_LENGTH: usize = 32;
const MAX_IDENTITY_LENGTH: usize = 4_096;
const MAX_MANIFEST_LENGTH: usize = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePointerKindV1 {
  FieldIndex,
  FieldNvt,
  ScopeCatalog,
}

impl ActivePointerKindV1 {
  pub fn id(self) -> u16 {
    match self {
      Self::FieldIndex => 0x0001,
      Self::FieldNvt => 0x0002,
      Self::ScopeCatalog => 0x0003,
    }
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::FieldIndex => "field-index",
      Self::FieldNvt => "field-nvt",
      Self::ScopeCatalog => "scope-catalog",
    }
  }

  fn from_id(id: u16) -> Option<Self> {
    match id {
      0x0001 => Some(Self::FieldIndex),
      0x0002 => Some(Self::FieldNvt),
      0x0003 => Some(Self::ScopeCatalog),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexManifestKindV1 {
  FieldIndex,
  FieldNvt,
  ScopeCatalog,
  ValueStore,
}

impl IndexManifestKindV1 {
  pub fn id(self) -> u16 {
    match self {
      Self::FieldIndex => 0x0010,
      Self::FieldNvt => 0x0011,
      Self::ScopeCatalog => 0x0012,
      Self::ValueStore => 0x0013,
    }
  }

  pub fn name(self) -> &'static str {
    match self {
      Self::FieldIndex => "field-index",
      Self::FieldNvt => "field-nvt",
      Self::ScopeCatalog => "scope-catalog",
      Self::ValueStore => "value-store",
    }
  }

  fn from_id(id: u16) -> Option<Self> {
    match id {
      0x0010 => Some(Self::FieldIndex),
      0x0011 => Some(Self::FieldNvt),
      0x0012 => Some(Self::ScopeCatalog),
      0x0013 => Some(Self::ValueStore),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivePointerV1<'a> {
  pub kind: ActivePointerKindV1,
  pub generation: u64,
  pub owner_id: &'a [u8],
  pub slot: u8,
  pub sequence: u64,
  pub target_manifest_hash: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexManifestV1<'a> {
  pub kind: IndexManifestKindV1,
  pub generation: u64,
  pub owner_id: &'a [u8],
  pub populated: bool,
  pub body: &'a [u8],
  pub definition: Option<&'a [u8]>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexControlOrManifestV1<'a> {
  Pointer(ActivePointerV1<'a>),
  Manifest(IndexManifestV1<'a>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableIndexArtifactV1<'a> {
  pub kind: u16,
  pub generation: u64,
  pub identity: &'a [u8],
  pub body: &'a [u8],
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexEnvelopeV1<'a> {
  pub kind: u16,
  pub generation: u64,
  pub identity: &'a [u8],
  pub body: &'a [u8],
}

pub fn decode_index_control_or_manifest(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<IndexControlOrManifestV1<'_>> {
  if value.len() > MAX_MANIFEST_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "index_manifest_exceeds_cap",
      format!("{} bytes exceeds {MAX_MANIFEST_LENGTH}", value.len()),
    ));
  }
  let kind = u16_at(value, 6)?;
  if ActivePointerKindV1::from_id(kind).is_some() {
    decode_active_pointer(value, hash_algorithm).map(IndexControlOrManifestV1::Pointer)
  } else if IndexManifestKindV1::from_id(kind).is_some() {
    decode_index_manifest(value, hash_algorithm).map(IndexControlOrManifestV1::Manifest)
  } else {
    Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "index_artifact_kind", format!("unsupported kind 0x{kind:04x}")))
  }
}

pub fn decode_active_pointer(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<ActivePointerV1<'_>> {
  let hash_width = hash_algorithm.hash_length();
  let expected_length = 45usize.checked_add(2 * hash_width).ok_or_else(|| length_error("pointer length overflow"))?;
  if value.len() != expected_length {
    return Err(error(
      if value.len() > expected_length {
        MalformedInputClass::AllocationAmplification
      } else {
        MalformedInputClass::TruncationOrTrailingBytes
      },
      "index_pointer_length",
      format!("expected {expected_length} bytes, got {}", value.len()),
    ));
  }
  let envelope = decode_index_envelope(value, expected_length)?;
  let kind = ActivePointerKindV1::from_id(envelope.kind).ok_or_else(|| {
    error(MalformedInputClass::UnknownTypeKindOrEnum, "index_pointer_kind", format!("unknown pointer kind 0x{:04x}", envelope.kind))
  })?;
  if envelope.identity.len() != hash_width + 1 || envelope.body.len() != 8 + hash_width {
    return Err(closure_error("active-pointer identity or body width disagrees with the hash profile"));
  }
  let owner_id = &envelope.identity[..hash_width];
  let slot = envelope.identity[hash_width];
  if owner_id.iter().all(|byte| *byte == 0) {
    return Err(identity_error("active-pointer owner ID is all zero"));
  }
  if slot > 1 {
    return Err(error(MalformedInputClass::NoncanonicalBooleanOrOptionalPresence, "index_pointer_slot", format!("slot {slot} is not A/B")));
  }
  let sequence = u64_at(envelope.body, 0)?;
  let target_manifest_hash = &envelope.body[8..];
  if sequence == 0 || target_manifest_hash.iter().all(|byte| *byte == 0) {
    return Err(identity_error("active-pointer sequence or target is zero"));
  }
  let key = digest_parts(hash_algorithm, &[b"aeordb.index-artifact.pointer.v1\0", &kind.id().to_le_bytes(), envelope.identity]);
  Ok(ActivePointerV1 { kind, generation: envelope.generation, owner_id, slot, sequence, target_manifest_hash, key })
}

pub fn select_active_pointer<'a>(left: &'a ActivePointerV1<'a>, right: &'a ActivePointerV1<'a>) -> FormatResult<&'a ActivePointerV1<'a>> {
  if left.kind != right.kind || left.owner_id != right.owner_id || left.slot == right.slot {
    return Err(closure_error("active-pointer pair does not describe opposite slots for one owner and kind"));
  }
  if left.sequence > right.sequence {
    Ok(left)
  } else if right.sequence > left.sequence {
    Ok(right)
  } else if left.target_manifest_hash != right.target_manifest_hash {
    Err(error(
      MalformedInputClass::AmbiguousEqualSequenceSelector,
      "index_pointer_ambiguous",
      "equal pointer sequences select different manifests",
    ))
  } else if left.slot == 0 {
    Ok(left)
  } else {
    Ok(right)
  }
}

pub fn decode_index_manifest(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<IndexManifestV1<'_>> {
  let hash_width = hash_algorithm.hash_length();
  let minimum_length = 44usize.checked_add(hash_width).ok_or_else(|| length_error("manifest minimum length overflow"))?;
  if value.len() < minimum_length {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "index_manifest_truncated",
      format!("{} bytes is shorter than {minimum_length}", value.len()),
    ));
  }
  let artifact = decode_immutable_index_artifact(value, hash_algorithm, MAX_MANIFEST_LENGTH)?;
  let kind = IndexManifestKindV1::from_id(artifact.kind).ok_or_else(|| {
    error(MalformedInputClass::UnknownTypeKindOrEnum, "index_manifest_kind", format!("unknown manifest kind 0x{:04x}", artifact.kind))
  })?;
  if artifact.identity.len() != hash_width + 8 {
    return Err(closure_error("manifest identity does not contain one owner ID and generation"));
  }
  let owner_id = &artifact.identity[..hash_width];
  if owner_id.iter().all(|byte| *byte == 0) || u64_at(artifact.identity, hash_width)? != artifact.generation {
    return Err(identity_error("manifest owner is zero or identity generation disagrees with the envelope"));
  }

  let (populated, definition) = match kind {
    IndexManifestKindV1::ScopeCatalog => decode_scope_manifest_body(artifact.body, owner_id, hash_algorithm)?,
    IndexManifestKindV1::ValueStore => decode_value_manifest_body(artifact.body, owner_id, hash_algorithm)?,
    IndexManifestKindV1::FieldIndex => decode_field_manifest_body(artifact.body, owner_id, hash_algorithm)?,
    IndexManifestKindV1::FieldNvt => (decode_nvt_manifest_body(artifact.body, hash_width)?, None),
  };
  Ok(IndexManifestV1 { kind, generation: artifact.generation, owner_id, populated, body: artifact.body, definition, key: artifact.key })
}

pub(crate) fn decode_immutable_index_artifact(
  value: &[u8],
  hash_algorithm: HashAlgorithm,
  maximum_length: usize,
) -> FormatResult<ImmutableIndexArtifactV1<'_>> {
  let envelope = decode_index_envelope(value, maximum_length)?;
  let key = digest_parts(hash_algorithm, &[b"aeordb.index-artifact.immutable.v1\0", &envelope.kind.to_le_bytes(), value]);
  Ok(ImmutableIndexArtifactV1 {
    kind: envelope.kind,
    generation: envelope.generation,
    identity: envelope.identity,
    body: envelope.body,
    key,
  })
}

pub(crate) fn decode_index_envelope(value: &[u8], maximum_length: usize) -> FormatResult<IndexEnvelopeV1<'_>> {
  if value.len() > maximum_length {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "index_artifact_exceeds_cap",
      format!("{} bytes exceeds {maximum_length}", value.len()),
    ));
  }
  if value.len() < INDEX_ENVELOPE_LENGTH + 1 + 4 {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "index_artifact_truncated",
      format!("{} bytes is shorter than the minimum envelope", value.len()),
    ));
  }
  if &value[..4] != b"AIDX" || u16_at(value, 4)? != 1 || usize::from(u16_at(value, 8)?) != INDEX_ENVELOPE_LENGTH {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "index_artifact_envelope", "expected AIDX v1 with a 32-byte header"));
  }
  let kind = u16_at(value, 6)?;
  if kind == 0 {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "index_artifact_kind", "artifact kind is zero"));
  }
  if u16_at(value, 10)? != 0 || u16_at(value, 18)? != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "index_artifact_reserved", "flags or reserve are nonzero"));
  }
  if usize::try_from(u32_at(value, 12)?).map_err(|_| length_error("artifact total length does not fit usize"))? != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "index_artifact_total_length",
      "declared artifact length differs from input",
    ));
  }
  let identity_length = usize::from(u16_at(value, 16)?);
  let body_length = usize::try_from(u32_at(value, 20)?).map_err(|_| length_error("artifact body length does not fit usize"))?;
  if identity_length == 0 {
    return Err(identity_error("artifact identity is empty"));
  }
  if identity_length > MAX_IDENTITY_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "index_artifact_identity_exceeds_cap",
      format!("identity length {identity_length} exceeds {MAX_IDENTITY_LENGTH}"),
    ));
  }
  let expected_length = INDEX_ENVELOPE_LENGTH
    .checked_add(identity_length)
    .and_then(|length| length.checked_add(body_length))
    .and_then(|length| length.checked_add(4))
    .ok_or_else(|| length_error("artifact length formula overflow"))?;
  if expected_length != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "index_artifact_length_formula",
      format!("identity and body end at {expected_length}, input ends at {}", value.len()),
    ));
  }
  let generation = u64_at(value, 24)?;
  if generation == 0 {
    return Err(identity_error("artifact generation is zero"));
  }
  verify_index_crc(value)?;
  let identity_end = INDEX_ENVELOPE_LENGTH + identity_length;
  Ok(IndexEnvelopeV1 {
    kind,
    generation,
    identity: &value[INDEX_ENVELOPE_LENGTH..identity_end],
    body: &value[identity_end..value.len() - 4],
  })
}

pub(crate) fn verify_index_crc(value: &[u8]) -> FormatResult<()> {
  if value.len() < 4 {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "index_artifact_crc", "artifact omits CRC"));
  }
  let crc_offset = value.len() - 4;
  if u32_at(value, crc_offset)? != crc32fast::hash(&value[..crc_offset]) {
    return Err(error(MalformedInputClass::ChecksumOrIntegrityMismatch, "index_artifact_crc", "artifact CRC-32/ISO-HDLC mismatch"));
  }
  Ok(())
}

fn decode_correctness_prefix(body: &[u8], hash_width: usize, definition_start: usize, definition_cap: usize) -> FormatResult<&[u8]> {
  if body.len() < definition_start {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "index_manifest_body", "manifest body is truncated"));
  }
  if u32_at(body, 0)? != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "index_manifest_flags", "manifest flags are nonzero"));
  }
  validate_capabilities(&body[4..36])?;
  let definition_length = usize::try_from(u32_at(body, 36)?).map_err(|_| length_error("definition length does not fit usize"))?;
  if definition_length > definition_cap {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "index_manifest_definition_exceeds_cap",
      format!("definition length {definition_length} exceeds {definition_cap}"),
    ));
  }
  if body[40..40 + hash_width].iter().all(|byte| *byte == 0) || body[40 + hash_width..56 + hash_width].iter().all(|byte| *byte == 0) {
    return Err(identity_error("manifest coverage root or builder implementation ID is zero"));
  }
  let definition_end = definition_start.checked_add(definition_length).ok_or_else(|| length_error("definition end overflow"))?;
  if definition_end != body.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "index_manifest_definition_length",
      "embedded definition does not consume the body",
    ));
  }
  Ok(&body[definition_start..definition_end])
}

fn decode_scope_manifest_body<'a>(
  body: &'a [u8],
  owner_id: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<(bool, Option<&'a [u8]>)> {
  let hash_width = hash_algorithm.hash_length();
  let definition_start = 112 + 3 * hash_width;
  let definition = decode_correctness_prefix(body, hash_width, definition_start, 65_536)?;
  if u16_at(body, 64 + hash_width)? != 1 || u16_at(body, 66 + hash_width)? != 1 {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "scope_manifest_codec", "scope manifest codec IDs are not v1"));
  }
  if body[69 + hash_width..72 + hash_width].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "scope_manifest_reserved", "scope manifest reserve is nonzero"));
  }
  let presence = body[68 + hash_width];
  if presence & !0x03 != 0 {
    return Err(error(
      MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
      "scope_manifest_presence",
      "scope manifest contains unknown presence bits",
    ));
  }
  if u64_at(body, 72 + hash_width)? == 0 {
    return Err(identity_error("scope manifest next ordinal is zero"));
  }
  let ordinal = validate_root(presence, 1, &body[80 + hash_width..80 + 2 * hash_width])?;
  let reverse = validate_root(presence, 2, &body[80 + 2 * hash_width..80 + 3 * hash_width])?;
  let live = u64_at(body, 80 + 3 * hash_width)?;
  let tombstones = u64_at(body, 88 + 3 * hash_width)?;
  let ordinal_pages = u64_at(body, 96 + 3 * hash_width)?;
  let reverse_pages = u64_at(body, 104 + 3 * hash_width)?;
  if (!ordinal && (live != 0 || tombstones != 0 || ordinal_pages != 0))
    || (ordinal && ordinal_pages == 0)
    || (!reverse && (live != 0 || reverse_pages != 0))
    || (reverse && (live == 0 || reverse_pages == 0))
  {
    return Err(closure_error("scope manifest roots and counts disagree"));
  }
  let scope = decode_scope_definition(definition, hash_algorithm).map_err(|source| nested_definition_error("scope", source))?;
  if scope.scope_id != owner_id {
    return Err(closure_error("embedded ScopeDefinition does not derive the manifest owner"));
  }
  Ok((ordinal || reverse, Some(definition)))
}

fn decode_value_manifest_body<'a>(
  body: &'a [u8],
  owner_id: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<(bool, Option<&'a [u8]>)> {
  let hash_width = hash_algorithm.hash_length();
  let definition_start = 144 + 4 * hash_width;
  let definition = decode_correctness_prefix(body, hash_width, definition_start, 512 * 1_024)?;
  if [64 + hash_width, 66 + hash_width, 68 + hash_width].iter().any(|offset| u16_at(body, *offset).ok() != Some(1)) {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "value_manifest_codec", "value manifest codec IDs are not v1"));
  }
  if body[71 + hash_width] != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "value_manifest_reserved", "value manifest reserve is nonzero"));
  }
  if body[72 + hash_width..72 + 2 * hash_width].iter().all(|byte| *byte == 0) {
    return Err(identity_error("value manifest ScopeCatalogManifest reference is zero"));
  }
  if u64_at(body, 72 + 4 * hash_width)? == 0 {
    return Err(identity_error("value manifest high-water sequence is zero"));
  }
  let presence = body[70 + hash_width];
  if presence & !0x03 != 0 {
    return Err(error(
      MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
      "value_manifest_presence",
      "value manifest contains unknown presence bits",
    ));
  }
  let values = validate_root(presence, 1, &body[72 + 2 * hash_width..72 + 3 * hash_width])?;
  let states = validate_root(presence, 2, &body[72 + 3 * hash_width..72 + 4 * hash_width])?;
  let value_counts = [
    u64_at(body, 80 + 4 * hash_width)?,
    u64_at(body, 96 + 4 * hash_width)?,
    u64_at(body, 112 + 4 * hash_width)?,
    u64_at(body, 120 + 4 * hash_width)?,
    u64_at(body, 136 + 4 * hash_width)?,
  ];
  let state_counts = [u64_at(body, 88 + 4 * hash_width)?, u64_at(body, 104 + 4 * hash_width)?, u64_at(body, 128 + 4 * hash_width)?];
  if (!values && value_counts.iter().any(|count| *count != 0))
    || (values && (value_counts[0] == 0 || value_counts[1] == 0 || value_counts[2] == 0))
    || (!states && state_counts.iter().any(|count| *count != 0))
    || (states && (state_counts[0] == 0 || state_counts[1] == 0))
  {
    return Err(closure_error("value manifest roots and counts disagree"));
  }
  let value_store =
    decode_value_store_definition(definition, hash_algorithm).map_err(|source| nested_definition_error("value-store", source))?;
  if value_store.value_store_id != owner_id {
    return Err(closure_error("embedded ValueStoreDefinition does not derive the manifest owner"));
  }
  Ok((values || states, Some(definition)))
}

fn decode_field_manifest_body<'a>(
  body: &'a [u8],
  owner_id: &[u8],
  hash_algorithm: HashAlgorithm,
) -> FormatResult<(bool, Option<&'a [u8]>)> {
  let hash_width = hash_algorithm.hash_length();
  let definition_start = 160 + 4 * hash_width;
  let definition = decode_correctness_prefix(body, hash_width, definition_start, 256 * 1_024)?;
  if [64 + hash_width, 66 + hash_width, 68 + hash_width].iter().any(|offset| u16_at(body, *offset).ok() != Some(1)) {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "field_manifest_codec", "field manifest codec IDs are not v1"));
  }
  if body[71 + hash_width] != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "field_manifest_reserved", "field manifest reserve is nonzero"));
  }
  if body[72 + hash_width..72 + 2 * hash_width].iter().all(|byte| *byte == 0) {
    return Err(identity_error("field manifest ValueStoreManifest reference is zero"));
  }
  let presence = body[70 + hash_width];
  if presence & !0x03 != 0 {
    return Err(error(
      MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
      "field_manifest_presence",
      "field manifest contains unknown presence bits",
    ));
  }
  let postings = validate_root(presence, 1, &body[72 + 2 * hash_width..72 + 3 * hash_width])?;
  let states = validate_root(presence, 2, &body[72 + 3 * hash_width..72 + 4 * hash_width])?;
  let first_page_id = u64_at(body, 72 + 4 * hash_width)?;
  let last_page_id = u64_at(body, 80 + 4 * hash_width)?;
  let next_page_id = u64_at(body, 88 + 4 * hash_width)?;
  if next_page_id == 0 {
    return Err(identity_error("field manifest next page ID is zero"));
  }
  let posting_counts = [
    u64_at(body, 96 + 4 * hash_width)?,
    u64_at(body, 112 + 4 * hash_width)?,
    u64_at(body, 120 + 4 * hash_width)?,
    u64_at(body, 128 + 4 * hash_width)?,
    u64_at(body, 152 + 4 * hash_width)?,
  ];
  let state_counts = [u64_at(body, 104 + 4 * hash_width)?, u64_at(body, 136 + 4 * hash_width)?, u64_at(body, 144 + 4 * hash_width)?];
  if (!postings && (first_page_id != 0 || last_page_id != 0 || posting_counts.iter().any(|count| *count != 0)))
    || (postings
      && (first_page_id == 0
        || last_page_id == 0
        || first_page_id > last_page_id
        || next_page_id <= last_page_id
        || posting_counts[0] == 0
        || posting_counts[1] == 0
        || posting_counts[3] == 0))
    || (!states && state_counts.iter().any(|count| *count != 0))
    || (states && (state_counts[0] == 0 || state_counts[1] == 0))
  {
    return Err(closure_error("field manifest roots, page IDs, and counts disagree"));
  }
  let field_index =
    decode_field_index_definition(definition, hash_algorithm).map_err(|source| nested_definition_error("field-index", source))?;
  if field_index.index_id != owner_id {
    return Err(closure_error("embedded FieldIndexDefinition does not derive the manifest owner"));
  }
  Ok((postings || states, Some(definition)))
}

fn decode_nvt_manifest_body(body: &[u8], hash_width: usize) -> FormatResult<bool> {
  let expected_length = 88usize.checked_add(2 * hash_width).ok_or_else(|| length_error("NVT manifest length overflow"))?;
  if body.len() != expected_length {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "nvt_manifest_length",
      format!("expected {expected_length} body bytes, got {}", body.len()),
    ));
  }
  if u32_at(body, 0)? != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "nvt_manifest_flags", "NVT manifest flags are nonzero"));
  }
  validate_capabilities(&body[4..36])?;
  let tile_cells = u32_at(body, 40)?;
  let resolution = u64_at(body, 48)?;
  if u16_at(body, 36)? != 1 || u16_at(body, 38)? != 1 {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "nvt_manifest_codec", "NVT manifest codec IDs are not v1"));
  }
  if tile_cells == 0
    || !tile_cells.is_power_of_two()
    || resolution == 0
    || u64::from(tile_cells) > resolution
    || !resolution.is_multiple_of(u64::from(tile_cells))
    || u64_at(body, 56)? == 0
    || body[64..64 + hash_width].iter().all(|byte| *byte == 0)
  {
    return Err(closure_error("NVT manifest resolution, generation, or posting manifest is invalid"));
  }
  if body[45..48].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "nvt_manifest_reserved", "NVT manifest reserve is nonzero"));
  }
  let presence = body[44];
  if presence & !1 != 0 {
    return Err(error(
      MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
      "nvt_manifest_presence",
      "NVT manifest contains unknown presence bits",
    ));
  }
  let tiles = validate_root(presence, 1, &body[64 + hash_width..64 + 2 * hash_width])?;
  let tile_count = u64_at(body, 64 + 2 * hash_width)?;
  let populated_cells = u64_at(body, 72 + 2 * hash_width)?;
  if tile_count > resolution / u64::from(tile_cells)
    || populated_cells > resolution
    || (!tiles && (tile_count != 0 || populated_cells != 0))
    || (tiles && (tile_count == 0 || populated_cells == 0))
  {
    return Err(closure_error("NVT manifest root and counts disagree"));
  }
  Ok(tiles)
}

fn validate_capabilities(capabilities: &[u8]) -> FormatResult<()> {
  if capabilities.len() != 32 {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "index_capability_width", "capability bitset is not 32 bytes"));
  }
  if capabilities[3..].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::UnknownRequiredCapability,
      "index_unknown_capability",
      "capability bit 24 or later is not recognized",
    ));
  }
  Ok(())
}

fn validate_root(presence: u8, bit: u8, root: &[u8]) -> FormatResult<bool> {
  let present = presence & bit != 0;
  let zero = root.iter().all(|byte| *byte == 0);
  if present == zero {
    return Err(closure_error("manifest root presence bit and hash disagree"));
  }
  Ok(present)
}

fn nested_definition_error(label: &'static str, source: FormatError) -> FormatError {
  closure_error(format!("embedded {label} definition rejected: {} ({})", source.code(), source.context()))
}

pub(crate) fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let value = bytes.get(offset..offset + 2).ok_or_else(|| truncated_error(offset, 2))?;
  Ok(u16::from_le_bytes(value.try_into().expect("exact slice length")))
}

pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  let value = bytes.get(offset..offset + 4).ok_or_else(|| truncated_error(offset, 4))?;
  Ok(u32::from_le_bytes(value.try_into().expect("exact slice length")))
}

pub(crate) fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let value = bytes.get(offset..offset + 8).ok_or_else(|| truncated_error(offset, 8))?;
  Ok(u64::from_le_bytes(value.try_into().expect("exact slice length")))
}

fn truncated_error(offset: usize, width: usize) -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, "index_artifact_truncated", format!("need {width} bytes at offset {offset}"))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "index_artifact_length_overflow", context)
}

fn identity_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "index_artifact_identity", context)
}

fn closure_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, "index_artifact_closure", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
