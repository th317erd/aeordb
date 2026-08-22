use crate::engine::HashAlgorithm;

use super::field_definition::decode_field_index_definition;
use super::hash::digest_parts;
pub use super::index_manifest::{
  CoverageVersionV1, FieldIndexManifestBodyV1, FieldNvtManifestBodyV1, IndexManifestBodyV1, ScopeCatalogManifestBodyV1,
  ValueStoreManifestBodyV1,
};
use super::index_manifest::{decode_index_manifest_body, encode_index_manifest_body};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::value_store::decode_value_store_definition;

pub(crate) const INDEX_ENVELOPE_LENGTH: usize = 32;
const MAX_IDENTITY_LENGTH: usize = 4_096;
const MAX_MANIFEST_LENGTH: usize = 1_048_576;
const MAX_ORDERED_ARTIFACT_LENGTH: usize = 4 * 1_024 * 1_024;
const MAX_MUTATION_JOURNAL_LENGTH: usize = 16 * 1_024 * 1_024;
const MAX_INDEX_TASK_CHECKPOINT_LENGTH: usize = 4 * 1_024 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ImmutableIndexArtifactKindV1 {
  FieldIndexManifest = 0x0010,
  FieldNvtManifest = 0x0011,
  ScopeCatalogManifest = 0x0012,
  ValueStoreManifest = 0x0013,
  ArtifactDirectoryNode = 0x0020,
  PostingPage = 0x0030,
  ValuePage = 0x0031,
  NvtTile = 0x0032,
  ScopeCatalogPage = 0x0033,
  DocumentStatePage = 0x0034,
  MutationJournalSegment = 0x0040,
  IndexTaskCheckpoint = 0x0041,
}

impl ImmutableIndexArtifactKindV1 {
  pub const ALL: [Self; 12] = [
    Self::FieldIndexManifest,
    Self::FieldNvtManifest,
    Self::ScopeCatalogManifest,
    Self::ValueStoreManifest,
    Self::ArtifactDirectoryNode,
    Self::PostingPage,
    Self::ValuePage,
    Self::NvtTile,
    Self::ScopeCatalogPage,
    Self::DocumentStatePage,
    Self::MutationJournalSegment,
    Self::IndexTaskCheckpoint,
  ];

  pub fn from_u16(value: u16) -> Option<Self> {
    Self::ALL.into_iter().find(|kind| *kind as u16 == value)
  }

  pub fn id(self) -> u16 {
    self as u16
  }

  pub fn maximum_encoded_length(self) -> usize {
    match self {
      Self::FieldIndexManifest | Self::FieldNvtManifest | Self::ScopeCatalogManifest | Self::ValueStoreManifest => MAX_MANIFEST_LENGTH,
      Self::ArtifactDirectoryNode
      | Self::PostingPage
      | Self::ValuePage
      | Self::NvtTile
      | Self::ScopeCatalogPage
      | Self::DocumentStatePage => MAX_ORDERED_ARTIFACT_LENGTH,
      Self::MutationJournalSegment => MAX_MUTATION_JOURNAL_LENGTH,
      Self::IndexTaskCheckpoint => MAX_INDEX_TASK_CHECKPOINT_LENGTH,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableIndexArtifactWriteV1<'a> {
  pub kind: ImmutableIndexArtifactKindV1,
  pub hash_algorithm: HashAlgorithm,
  pub generation: u64,
  pub identity: &'a [u8],
  pub body: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedImmutableIndexArtifactV1 {
  pub key: Vec<u8>,
  pub value: Vec<u8>,
}

/// Return the exact immutable AIDX envelope length after validating every
/// representational and per-kind bound, without allocating the artifact.
pub fn checked_immutable_index_artifact_encoded_length(
  kind: ImmutableIndexArtifactKindV1,
  identity_length: usize,
  body_length: usize,
) -> FormatResult<usize> {
  let total_length = checked_immutable_index_artifact_representable_length(identity_length, body_length)?;
  let maximum_length = kind.maximum_encoded_length();
  if total_length > maximum_length {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "index_artifact_exceeds_cap",
      format!("{total_length} bytes exceeds {maximum_length}"),
    ));
  }
  Ok(total_length)
}

pub(crate) fn checked_immutable_index_artifact_representable_length(identity_length: usize, body_length: usize) -> FormatResult<usize> {
  if identity_length == 0 {
    return Err(identity_error("immutable artifact identity is empty"));
  }
  if identity_length > MAX_IDENTITY_LENGTH {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "index_artifact_identity_exceeds_cap",
      format!("identity length {identity_length} exceeds {MAX_IDENTITY_LENGTH}"),
    ));
  }
  let _identity_length = checked_index_u16(identity_length, "artifact identity length exceeds u16")?;
  let _body_length = checked_index_u32(body_length, "artifact body length exceeds u32")?;
  let total_length = INDEX_ENVELOPE_LENGTH
    .checked_add(identity_length)
    .and_then(|length| length.checked_add(body_length))
    .and_then(|length| length.checked_add(4))
    .ok_or_else(|| length_error("artifact length formula overflow"))?;
  let _total_length = checked_index_u32(total_length, "artifact total length exceeds u32")?;
  Ok(total_length)
}

/// Encode one complete immutable AIDX envelope without publishing it.
pub fn encode_immutable_index_artifact(request: &ImmutableIndexArtifactWriteV1<'_>) -> FormatResult<EncodedImmutableIndexArtifactV1> {
  if request.generation == 0 {
    return Err(identity_error("immutable artifact generation is zero"));
  }
  let total_length = checked_immutable_index_artifact_encoded_length(request.kind, request.identity.len(), request.body.len())?;
  let identity_length = checked_index_u16(request.identity.len(), "artifact identity length exceeds u16")?;
  let body_length = checked_index_u32(request.body.len(), "artifact body length exceeds u32")?;
  let total_length_u32 = checked_index_u32(total_length, "artifact total length exceeds u32")?;

  let mut value = vec![0u8; total_length];
  value[..4].copy_from_slice(b"AIDX");
  value[4..6].copy_from_slice(&1u16.to_le_bytes());
  value[6..8].copy_from_slice(&request.kind.id().to_le_bytes());
  value[8..10].copy_from_slice(&(INDEX_ENVELOPE_LENGTH as u16).to_le_bytes());
  value[12..16].copy_from_slice(&total_length_u32.to_le_bytes());
  value[16..18].copy_from_slice(&identity_length.to_le_bytes());
  value[20..24].copy_from_slice(&body_length.to_le_bytes());
  value[24..32].copy_from_slice(&request.generation.to_le_bytes());
  let identity_end = INDEX_ENVELOPE_LENGTH + request.identity.len();
  let body_end = identity_end + request.body.len();
  value[INDEX_ENVELOPE_LENGTH..identity_end].copy_from_slice(request.identity);
  value[identity_end..body_end].copy_from_slice(request.body);
  let checksum = crc32fast::hash(&value[..body_end]);
  value[body_end..].copy_from_slice(&checksum.to_le_bytes());

  let decoded = decode_immutable_index_artifact(&value, request.hash_algorithm, request.kind.maximum_encoded_length())?;
  if decoded.kind != request.kind.id() {
    return Err(closure_error("encoded immutable artifact kind disagrees with its request"));
  }
  Ok(EncodedImmutableIndexArtifactV1 { key: decoded.key, value })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePointerKindV1 {
  FieldIndex,
  FieldNvt,
  ScopeCatalog,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivePointerWriteV1<'a> {
  pub kind: ActivePointerKindV1,
  pub hash_algorithm: HashAlgorithm,
  pub generation: u64,
  pub owner_id: &'a [u8],
  pub slot: u8,
  pub sequence: u64,
  pub target_manifest_hash: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedActivePointerV1 {
  pub key: Vec<u8>,
  pub value: Vec<u8>,
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

/// Encode one complete stable A/B active pointer without publishing it.
pub fn encode_active_pointer(request: &ActivePointerWriteV1<'_>) -> FormatResult<EncodedActivePointerV1> {
  let hash_width = request.hash_algorithm.hash_length();
  if request.generation == 0
    || request.sequence == 0
    || request.owner_id.len() != hash_width
    || request.owner_id.iter().all(|byte| *byte == 0)
    || request.target_manifest_hash.len() != hash_width
    || request.target_manifest_hash.iter().all(|byte| *byte == 0)
  {
    return Err(identity_error("active-pointer generation, sequence, owner, or target disagrees with the database hash profile"));
  }
  if request.slot > 1 {
    return Err(error(
      MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
      "index_pointer_slot",
      format!("slot {} is not A/B", request.slot),
    ));
  }

  let identity_length = hash_width.checked_add(1).ok_or_else(|| length_error("active-pointer identity length overflow"))?;
  let body_length = hash_width.checked_add(8).ok_or_else(|| length_error("active-pointer body length overflow"))?;
  let total_length = INDEX_ENVELOPE_LENGTH
    .checked_add(identity_length)
    .and_then(|length| length.checked_add(body_length))
    .and_then(|length| length.checked_add(4))
    .ok_or_else(|| length_error("active-pointer total length overflow"))?;
  let identity_length_u16 = checked_index_u16(identity_length, "active-pointer identity length exceeds u16")?;
  let body_length_u32 = checked_index_u32(body_length, "active-pointer body length exceeds u32")?;
  let total_length_u32 = checked_index_u32(total_length, "active-pointer total length exceeds u32")?;

  let mut value = Vec::new();
  value.try_reserve_exact(total_length).map_err(|source| {
    error(MalformedInputClass::AllocationAmplification, "index_pointer_allocation", format!("active-pointer allocation failed: {source}"))
  })?;
  value.resize(total_length, 0);
  value[..4].copy_from_slice(b"AIDX");
  value[4..6].copy_from_slice(&1u16.to_le_bytes());
  value[6..8].copy_from_slice(&request.kind.id().to_le_bytes());
  value[8..10].copy_from_slice(&(INDEX_ENVELOPE_LENGTH as u16).to_le_bytes());
  value[12..16].copy_from_slice(&total_length_u32.to_le_bytes());
  value[16..18].copy_from_slice(&identity_length_u16.to_le_bytes());
  value[20..24].copy_from_slice(&body_length_u32.to_le_bytes());
  value[24..32].copy_from_slice(&request.generation.to_le_bytes());

  let owner_end = INDEX_ENVELOPE_LENGTH + hash_width;
  let identity_end = owner_end + 1;
  let sequence_end = identity_end + 8;
  let body_end = sequence_end + hash_width;
  value[INDEX_ENVELOPE_LENGTH..owner_end].copy_from_slice(request.owner_id);
  value[owner_end] = request.slot;
  value[identity_end..sequence_end].copy_from_slice(&request.sequence.to_le_bytes());
  value[sequence_end..body_end].copy_from_slice(request.target_manifest_hash);
  let checksum = crc32fast::hash(&value[..body_end]);
  value[body_end..].copy_from_slice(&checksum.to_le_bytes());

  let decoded = decode_active_pointer(&value, request.hash_algorithm)?;
  if decoded.kind != request.kind
    || decoded.generation != request.generation
    || decoded.owner_id != request.owner_id
    || decoded.slot != request.slot
    || decoded.sequence != request.sequence
    || decoded.target_manifest_hash != request.target_manifest_hash
  {
    return Err(closure_error("encoded active pointer disagrees with its request"));
  }
  Ok(EncodedActivePointerV1 { key: decoded.key, value })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePointerSlotObservationV1<'a> {
  Missing,
  StructurallyInvalid,
  Structural { pointer: &'a ActivePointerV1<'a>, closure_valid: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivePointerClosureSelectionV1<'a> {
  pub selected: Option<&'a ActivePointerV1<'a>>,
  pub repair_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivePointerRewritePlanV1<'a> {
  pub selection: ActivePointerClosureSelectionV1<'a>,
  pub write_slot: u8,
  pub next_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexManifestV1<'a> {
  pub kind: IndexManifestKindV1,
  pub generation: u64,
  pub owner_id: &'a [u8],
  pub populated: bool,
  pub body: &'a [u8],
  pub definition: Option<&'a [u8]>,
  pub details: IndexManifestBodyV1<'a>,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexManifestWriteV1<'a> {
  pub hash_algorithm: HashAlgorithm,
  pub generation: u64,
  pub owner_id: &'a [u8],
  pub body: IndexManifestBodyV1<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexManifestSummaryV1<'a> {
  pub kind: IndexManifestKindV1,
  pub generation: u64,
  pub owner_id: &'a [u8],
  pub populated: bool,
  pub key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexControlOrManifestV1<'a> {
  Pointer(ActivePointerV1<'a>),
  Manifest(IndexManifestSummaryV1<'a>),
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
    decode_index_manifest(value, hash_algorithm).map(|manifest| {
      IndexControlOrManifestV1::Manifest(IndexManifestSummaryV1 {
        kind: manifest.kind,
        generation: manifest.generation,
        owner_id: manifest.owner_id,
        populated: manifest.populated,
        key: manifest.key,
      })
    })
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
  select_closure_valid_active_pointer(
    left.kind,
    left.owner_id,
    ActivePointerSlotObservationV1::Structural { pointer: left, closure_valid: true },
    ActivePointerSlotObservationV1::Structural { pointer: right, closure_valid: true },
  )?
  .selected
  .ok_or_else(|| closure_error("structurally valid active-pointer pair has no closure-valid member"))
}

pub fn select_closure_valid_active_pointer<'a>(
  expected_kind: ActivePointerKindV1,
  expected_owner_id: &[u8],
  slot_a: ActivePointerSlotObservationV1<'a>,
  slot_b: ActivePointerSlotObservationV1<'a>,
) -> FormatResult<ActivePointerClosureSelectionV1<'a>> {
  let slot_a = validate_active_pointer_observation(expected_kind, expected_owner_id, 0, slot_a)?;
  let slot_b = validate_active_pointer_observation(expected_kind, expected_owner_id, 1, slot_b)?;
  select_validated_active_pointer_pair(slot_a, slot_b)
}

pub fn plan_active_pointer_rewrite<'a>(
  expected_kind: ActivePointerKindV1,
  expected_owner_id: &[u8],
  slot_a: ActivePointerSlotObservationV1<'a>,
  slot_b: ActivePointerSlotObservationV1<'a>,
) -> FormatResult<ActivePointerRewritePlanV1<'a>> {
  let slot_a = validate_active_pointer_observation(expected_kind, expected_owner_id, 0, slot_a)?;
  let slot_b = validate_active_pointer_observation(expected_kind, expected_owner_id, 1, slot_b)?;
  let selection = select_validated_active_pointer_pair(slot_a, slot_b)?;
  let maximum_sequence = match (slot_a.pointer, slot_b.pointer) {
    (None, None) => None,
    (Some(pointer), None) | (None, Some(pointer)) => Some(pointer.sequence),
    (Some(left), Some(right)) => Some(left.sequence.max(right.sequence)),
  };
  let next_sequence = match maximum_sequence {
    None => 1,
    Some(sequence) => sequence.checked_add(1).ok_or_else(|| length_error("active-pointer publication sequence is exhausted"))?,
  };
  let write_slot = match (slot_a.pointer, slot_b.pointer) {
    (None, _) => 0,
    (Some(_), None) => 1,
    (Some(left), Some(right)) if left.sequence == right.sequence => 1,
    (Some(left), Some(right)) if left.sequence < right.sequence => 0,
    (Some(_), Some(_)) => 1,
  };
  Ok(ActivePointerRewritePlanV1 { selection, write_slot, next_sequence })
}

#[derive(Debug, Clone, Copy)]
struct ValidatedActivePointerSlotV1<'a> {
  pointer: Option<&'a ActivePointerV1<'a>>,
  closure_valid: bool,
}

fn validate_active_pointer_observation<'a>(
  expected_kind: ActivePointerKindV1,
  expected_owner_id: &[u8],
  expected_slot: u8,
  observation: ActivePointerSlotObservationV1<'a>,
) -> FormatResult<ValidatedActivePointerSlotV1<'a>> {
  if expected_owner_id.is_empty() || expected_owner_id.iter().all(|byte| *byte == 0) {
    return Err(identity_error("active-pointer expected owner is empty or all zero"));
  }
  let ActivePointerSlotObservationV1::Structural { pointer, closure_valid } = observation else {
    return Ok(ValidatedActivePointerSlotV1 { pointer: None, closure_valid: false });
  };
  if pointer.kind != expected_kind || pointer.owner_id != expected_owner_id || pointer.slot != expected_slot {
    return Err(closure_error("active-pointer observation has a foreign kind, owner, or slot"));
  }
  if pointer.generation == 0
    || pointer.sequence == 0
    || pointer.target_manifest_hash.len() != expected_owner_id.len()
    || pointer.target_manifest_hash.iter().all(|byte| *byte == 0)
  {
    return Err(identity_error("structural active-pointer observation has an invalid generation, sequence, or target"));
  }
  Ok(ValidatedActivePointerSlotV1 { pointer: Some(pointer), closure_valid })
}

fn select_validated_active_pointer_pair<'a>(
  slot_a: ValidatedActivePointerSlotV1<'a>,
  slot_b: ValidatedActivePointerSlotV1<'a>,
) -> FormatResult<ActivePointerClosureSelectionV1<'a>> {
  let repair_required = match (slot_a.pointer, slot_b.pointer) {
    (Some(left), Some(right)) if left.sequence == right.sequence && left.target_manifest_hash != right.target_manifest_hash => {
      return Err(error(
        MalformedInputClass::AmbiguousEqualSequenceSelector,
        "index_pointer_ambiguous",
        "equal pointer sequences select different manifests",
      ));
    }
    (Some(left), Some(right)) => left.sequence == right.sequence,
    _ => false,
  };
  let selected = match (slot_a.pointer.filter(|_| slot_a.closure_valid), slot_b.pointer.filter(|_| slot_b.closure_valid)) {
    (None, None) => None,
    (Some(pointer), None) | (None, Some(pointer)) => Some(pointer),
    (Some(left), Some(right)) if left.sequence >= right.sequence => Some(left),
    (Some(_), Some(right)) => Some(right),
  };
  Ok(ActivePointerClosureSelectionV1 { selected, repair_required })
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

  let details = decode_index_manifest_body(kind, artifact.body, owner_id, hash_algorithm)?;
  let populated = details.populated();
  let definition = details.definition();
  Ok(IndexManifestV1 {
    kind,
    generation: artifact.generation,
    owner_id,
    populated,
    body: artifact.body,
    definition,
    details,
    key: artifact.key,
  })
}

pub fn encode_index_manifest(request: &IndexManifestWriteV1<'_>) -> FormatResult<EncodedImmutableIndexArtifactV1> {
  let hash_width = request.hash_algorithm.hash_length();
  if request.generation == 0 || request.owner_id.len() != hash_width || request.owner_id.iter().all(|byte| *byte == 0) {
    return Err(identity_error("manifest generation is zero or owner disagrees with the database hash profile"));
  }
  let body = encode_index_manifest_body(&request.body, request.owner_id, request.hash_algorithm)?;
  let identity_length = hash_width.checked_add(8).ok_or_else(|| length_error("manifest identity length overflow"))?;
  let mut identity = Vec::new();
  identity
    .try_reserve_exact(identity_length)
    .map_err(|source| error(MalformedInputClass::AllocationAmplification, "index_manifest_identity_allocation", source.to_string()))?;
  identity.extend_from_slice(request.owner_id);
  identity.extend_from_slice(&request.generation.to_le_bytes());
  let kind = ImmutableIndexArtifactKindV1::from_u16(request.body.kind().id())
    .ok_or_else(|| error(MalformedInputClass::UnknownTypeKindOrEnum, "index_manifest_kind", "manifest body has no immutable kind"))?;
  let encoded = encode_immutable_index_artifact(&ImmutableIndexArtifactWriteV1 {
    kind,
    hash_algorithm: request.hash_algorithm,
    generation: request.generation,
    identity: &identity,
    body: &body,
  })?;
  decode_index_manifest(&encoded.value, request.hash_algorithm)?;
  Ok(encoded)
}

pub fn validate_correctness_manifest_chain(
  scope: &IndexManifestV1<'_>,
  value: &IndexManifestV1<'_>,
  field: &IndexManifestV1<'_>,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<()> {
  let IndexManifestBodyV1::ScopeCatalog(scope_body) = &scope.details else {
    return Err(closure_error("correctness chain scope node is not a ScopeCatalog manifest"));
  };
  let IndexManifestBodyV1::ValueStore(value_body) = &value.details else {
    return Err(closure_error("correctness chain value node is not a ValueStore manifest"));
  };
  let IndexManifestBodyV1::FieldIndex(field_body) = &field.details else {
    return Err(closure_error("correctness chain field node is not a FieldIndex manifest"));
  };
  if value_body.scope_catalog_manifest != scope.key || field_body.value_store_manifest != value.key {
    return Err(closure_error("correctness manifest reference chain does not select the supplied exact manifests"));
  }
  if scope_body.coverage != value_body.coverage || scope_body.coverage != field_body.coverage {
    return Err(closure_error("correctness manifest coverage versions are not byte-identical"));
  }

  let value_definition = decode_value_store_definition(value_body.value_store_definition, hash_algorithm)
    .map_err(|source| closure_error(format!("correctness chain ValueStore definition rejected: {source}")))?;
  if value_definition.scope_id != scope.owner_id {
    return Err(closure_error("ValueStore definition ScopeId does not match the supplied ScopeCatalog owner"));
  }
  let field_definition = decode_field_index_definition(field_body.field_index_definition, hash_algorithm)
    .map_err(|source| closure_error(format!("correctness chain FieldIndex definition rejected: {source}")))?;
  if field_definition.value_store_id != value.owner_id {
    return Err(closure_error("FieldIndex definition ValueStoreId does not match the supplied ValueStore owner"));
  }
  Ok(())
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

fn checked_index_u16(value: usize, context: &'static str) -> FormatResult<u16> {
  if value > usize::from(u16::MAX) {
    return Err(length_error(context));
  }
  Ok(value as u16)
}

fn checked_index_u32(value: usize, context: &'static str) -> FormatResult<u32> {
  if value > u32::MAX as usize {
    return Err(length_error(context));
  }
  Ok(value as u32)
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
