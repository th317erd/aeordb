//! Frozen external-workspace records for recoverable v4 index runtime state.
//!
//! These bytes are node-local recovery evidence. They never select an active
//! index generation and never become namespace or query authority.

use crate::engine::HashAlgorithm;

use super::reader::{FormatError, FormatResult, MalformedInputClass};

pub use super::index_runtime_workspace_payload::*;

const MANIFEST_MAGIC: &[u8; 4] = b"AIWM";
const MANIFEST_SCHEMA_VERSION: u16 = 1;
const MANIFEST_BODY_LENGTH: usize = 204;
pub const INDEX_WORKSPACE_MANIFEST_LENGTH_V1: usize = 208;
const OBJECT_MAGIC: &[u8; 4] = b"AIWO";
const OBJECT_SCHEMA_VERSION: u16 = 1;
pub const INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1: usize = 184;
pub const INDEX_WORKSPACE_OBJECT_MAX_LENGTH_V1: usize = 512 * 1_024 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum IndexWorkspaceObjectKindV1 {
  RuntimeBatch = 1,
  ProducerTask = 2,
}

impl IndexWorkspaceObjectKindV1 {
  pub const fn id(self) -> u16 {
    self as u16
  }

  pub const fn from_id(id: u16) -> Option<Self> {
    match id {
      1 => Some(Self::RuntimeBatch),
      2 => Some(Self::ProducerTask),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWorkspaceManifestWriteV1 {
  pub database_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub workspace_id: [u8; 16],
  pub runtime_id: [u8; 16],
  pub manifest_sequence: u64,
  pub previous_manifest_digest: [u8; 32],
  pub object_kind: IndexWorkspaceObjectKindV1,
  pub object_id: [u8; 16],
  pub object_digest: [u8; 32],
  pub object_stored_bytes: u64,
  pub cumulative_object_count: u64,
  pub cumulative_stored_bytes: u64,
  pub created_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWorkspaceManifestV1 {
  pub database_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub workspace_id: [u8; 16],
  pub runtime_id: [u8; 16],
  pub manifest_sequence: u64,
  pub previous_manifest_digest: [u8; 32],
  pub object_kind: IndexWorkspaceObjectKindV1,
  pub object_id: [u8; 16],
  pub object_digest: [u8; 32],
  pub object_stored_bytes: u64,
  pub cumulative_object_count: u64,
  pub cumulative_stored_bytes: u64,
  pub created_at_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct IndexWorkspaceObjectWriteV1<'a> {
  pub kind: IndexWorkspaceObjectKindV1,
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub workspace_id: [u8; 16],
  pub runtime_id: [u8; 16],
  pub object_id: [u8; 16],
  pub object_sequence: u64,
  pub created_at_ms: u64,
  pub logical_record_count: u64,
  pub minimum_publication_sequence: u64,
  pub maximum_publication_sequence: u64,
  pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWorkspaceObjectV1<'a> {
  pub kind: IndexWorkspaceObjectKindV1,
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub workspace_id: [u8; 16],
  pub runtime_id: [u8; 16],
  pub object_id: [u8; 16],
  pub object_sequence: u64,
  pub created_at_ms: u64,
  pub logical_record_count: u64,
  pub minimum_publication_sequence: u64,
  pub maximum_publication_sequence: u64,
  pub payload_digest: [u8; 32],
  pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
pub(super) struct IndexWorkspaceObjectHeaderWriteV1 {
  pub kind: IndexWorkspaceObjectKindV1,
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub workspace_id: [u8; 16],
  pub runtime_id: [u8; 16],
  pub object_id: [u8; 16],
  pub object_sequence: u64,
  pub created_at_ms: u64,
  pub payload_length: usize,
  pub logical_record_count: u64,
  pub minimum_publication_sequence: u64,
  pub maximum_publication_sequence: u64,
  pub payload_digest: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct IndexWorkspaceObjectHeaderV1 {
  pub kind: IndexWorkspaceObjectKindV1,
  pub hash_algorithm: HashAlgorithm,
  pub database_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub workspace_id: [u8; 16],
  pub runtime_id: [u8; 16],
  pub object_id: [u8; 16],
  pub object_sequence: u64,
  pub created_at_ms: u64,
  pub payload_length: usize,
  pub logical_record_count: u64,
  pub minimum_publication_sequence: u64,
  pub maximum_publication_sequence: u64,
  pub payload_digest: [u8; 32],
  pub total_length: usize,
}

pub fn encode_index_workspace_manifest_v1(
  request: &IndexWorkspaceManifestWriteV1,
) -> FormatResult<[u8; INDEX_WORKSPACE_MANIFEST_LENGTH_V1]> {
  validate_manifest_fields(
    request.database_id,
    request.destination_physical_instance_id,
    request.workspace_id,
    request.runtime_id,
    request.manifest_sequence,
    request.previous_manifest_digest,
    request.object_id,
    request.object_digest,
    request.object_stored_bytes,
    request.cumulative_object_count,
    request.cumulative_stored_bytes,
    request.created_at_ms,
  )?;
  let mut encoded = [0u8; INDEX_WORKSPACE_MANIFEST_LENGTH_V1];
  encoded[..4].copy_from_slice(MANIFEST_MAGIC);
  encoded[4..6].copy_from_slice(&MANIFEST_SCHEMA_VERSION.to_le_bytes());
  encoded[6..8].copy_from_slice(&(MANIFEST_BODY_LENGTH as u16).to_le_bytes());
  encoded[8..12].copy_from_slice(&(INDEX_WORKSPACE_MANIFEST_LENGTH_V1 as u32).to_le_bytes());
  encoded[16..32].copy_from_slice(&request.database_id);
  encoded[32..48].copy_from_slice(&request.destination_physical_instance_id);
  encoded[48..64].copy_from_slice(&request.workspace_id);
  encoded[64..80].copy_from_slice(&request.runtime_id);
  encoded[80..88].copy_from_slice(&request.manifest_sequence.to_le_bytes());
  encoded[88..120].copy_from_slice(&request.previous_manifest_digest);
  encoded[120..122].copy_from_slice(&request.object_kind.id().to_le_bytes());
  encoded[124..140].copy_from_slice(&request.object_id);
  encoded[140..172].copy_from_slice(&request.object_digest);
  encoded[172..180].copy_from_slice(&request.object_stored_bytes.to_le_bytes());
  encoded[180..188].copy_from_slice(&request.cumulative_object_count.to_le_bytes());
  encoded[188..196].copy_from_slice(&request.cumulative_stored_bytes.to_le_bytes());
  encoded[196..204].copy_from_slice(&request.created_at_ms.to_le_bytes());
  let checksum = crc32fast::hash(&encoded[..MANIFEST_BODY_LENGTH]);
  encoded[MANIFEST_BODY_LENGTH..].copy_from_slice(&checksum.to_le_bytes());
  Ok(encoded)
}

pub fn decode_index_workspace_manifest_v1(bytes: &[u8]) -> FormatResult<IndexWorkspaceManifestV1> {
  if bytes.len() != INDEX_WORKSPACE_MANIFEST_LENGTH_V1 {
    return Err(format_error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "index_workspace_manifest_length",
      format!("manifest has {} bytes, expected {INDEX_WORKSPACE_MANIFEST_LENGTH_V1}", bytes.len()),
    ));
  }
  if &bytes[..4] != MANIFEST_MAGIC || u16_at(bytes, 4) != MANIFEST_SCHEMA_VERSION {
    return Err(format_error(
      MalformedInputClass::UnknownMagicOrVersion,
      "index_workspace_manifest_magic",
      "manifest magic or schema version is unknown",
    ));
  }
  if usize::from(u16_at(bytes, 6)) != MANIFEST_BODY_LENGTH || u32_at(bytes, 8) as usize != INDEX_WORKSPACE_MANIFEST_LENGTH_V1 {
    return Err(format_error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "index_workspace_manifest_framing",
      "manifest header or total length is not canonical",
    ));
  }
  if bytes[12..16].iter().any(|byte| *byte != 0) || bytes[122..124].iter().any(|byte| *byte != 0) {
    return Err(format_error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "index_workspace_manifest_reserved",
      "manifest flags or reserved bytes are nonzero",
    ));
  }
  let expected_checksum = crc32fast::hash(&bytes[..MANIFEST_BODY_LENGTH]);
  if u32_at(bytes, MANIFEST_BODY_LENGTH) != expected_checksum {
    return Err(format_error(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "index_workspace_manifest_checksum",
      "manifest CRC32 does not match",
    ));
  }
  let object_kind = IndexWorkspaceObjectKindV1::from_id(u16_at(bytes, 120)).ok_or_else(|| {
    format_error(MalformedInputClass::UnknownTypeKindOrEnum, "index_workspace_manifest_object_kind", "manifest object kind is unknown")
  })?;
  let decoded = IndexWorkspaceManifestV1 {
    database_id: array_at(bytes, 16),
    destination_physical_instance_id: array_at(bytes, 32),
    workspace_id: array_at(bytes, 48),
    runtime_id: array_at(bytes, 64),
    manifest_sequence: u64_at(bytes, 80),
    previous_manifest_digest: array_at(bytes, 88),
    object_kind,
    object_id: array_at(bytes, 124),
    object_digest: array_at(bytes, 140),
    object_stored_bytes: u64_at(bytes, 172),
    cumulative_object_count: u64_at(bytes, 180),
    cumulative_stored_bytes: u64_at(bytes, 188),
    created_at_ms: u64_at(bytes, 196),
  };
  validate_manifest_fields(
    decoded.database_id,
    decoded.destination_physical_instance_id,
    decoded.workspace_id,
    decoded.runtime_id,
    decoded.manifest_sequence,
    decoded.previous_manifest_digest,
    decoded.object_id,
    decoded.object_digest,
    decoded.object_stored_bytes,
    decoded.cumulative_object_count,
    decoded.cumulative_stored_bytes,
    decoded.created_at_ms,
  )?;
  Ok(decoded)
}

pub fn index_workspace_manifest_digest_v1(bytes: &[u8]) -> FormatResult<[u8; 32]> {
  decode_index_workspace_manifest_v1(bytes)?;
  Ok(*blake3::hash(bytes).as_bytes())
}

pub fn encode_index_workspace_object_v1(request: &IndexWorkspaceObjectWriteV1<'_>) -> FormatResult<Vec<u8>> {
  validate_index_workspace_object_payload_v1(
    request.kind,
    request.payload,
    request.hash_algorithm,
    request.logical_record_count,
    request.minimum_publication_sequence,
    request.maximum_publication_sequence,
  )?;
  let header = encode_index_workspace_object_header_v1(&IndexWorkspaceObjectHeaderWriteV1 {
    kind: request.kind,
    hash_algorithm: request.hash_algorithm,
    database_id: request.database_id,
    destination_physical_instance_id: request.destination_physical_instance_id,
    workspace_id: request.workspace_id,
    runtime_id: request.runtime_id,
    object_id: request.object_id,
    object_sequence: request.object_sequence,
    created_at_ms: request.created_at_ms,
    payload_length: request.payload.len(),
    logical_record_count: request.logical_record_count,
    minimum_publication_sequence: request.minimum_publication_sequence,
    maximum_publication_sequence: request.maximum_publication_sequence,
    payload_digest: *blake3::hash(request.payload).as_bytes(),
  })?;
  let total_length = checked_object_length(request.payload.len())?;
  let mut encoded = Vec::new();
  encoded.try_reserve_exact(total_length).map_err(|error| {
    format_error(
      MalformedInputClass::AllocationAmplification,
      "index_workspace_object_allocation",
      format!("object allocation failed: {error}"),
    )
  })?;
  encoded.resize(total_length, 0);
  encoded[..INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1].copy_from_slice(&header);
  let payload_end = INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1 + request.payload.len();
  encoded[INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1..payload_end].copy_from_slice(request.payload);
  let checksum = crc32fast::hash(&encoded[..payload_end]);
  encoded[payload_end..].copy_from_slice(&checksum.to_le_bytes());
  Ok(encoded)
}

pub(super) fn encode_index_workspace_object_header_v1(
  request: &IndexWorkspaceObjectHeaderWriteV1,
) -> FormatResult<[u8; INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1]> {
  let total_length = checked_object_length(request.payload_length)?;
  validate_object_fields(
    request.hash_algorithm,
    request.database_id,
    request.destination_physical_instance_id,
    request.workspace_id,
    request.runtime_id,
    request.object_id,
    request.object_sequence,
    request.created_at_ms,
    request.logical_record_count,
    request.minimum_publication_sequence,
    request.maximum_publication_sequence,
    request.payload_length,
  )?;
  if request.payload_digest.iter().all(|byte| *byte == 0) {
    return Err(format_error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "index_workspace_payload_digest",
      "workspace object payload digest is all zeroes",
    ));
  }
  let total_length_u64 = u64::try_from(total_length).map_err(|error| {
    format_error(
      MalformedInputClass::LengthCountOrArithmeticOverflow,
      "index_workspace_object_length",
      format!("object length exceeds u64: {error}"),
    )
  })?;
  let payload_length = u64::try_from(request.payload_length).map_err(|error| {
    format_error(
      MalformedInputClass::LengthCountOrArithmeticOverflow,
      "index_workspace_payload_length",
      format!("payload length exceeds u64: {error}"),
    )
  })?;
  let mut encoded = [0u8; INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1];
  encoded[..4].copy_from_slice(OBJECT_MAGIC);
  encoded[4..6].copy_from_slice(&OBJECT_SCHEMA_VERSION.to_le_bytes());
  encoded[6..8].copy_from_slice(&request.kind.id().to_le_bytes());
  encoded[8..10].copy_from_slice(&(INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1 as u16).to_le_bytes());
  encoded[10..12].copy_from_slice(&request.hash_algorithm.to_u16().to_le_bytes());
  encoded[12..20].copy_from_slice(&total_length_u64.to_le_bytes());
  encoded[24..40].copy_from_slice(&request.database_id);
  encoded[40..56].copy_from_slice(&request.destination_physical_instance_id);
  encoded[56..72].copy_from_slice(&request.workspace_id);
  encoded[72..88].copy_from_slice(&request.runtime_id);
  encoded[88..104].copy_from_slice(&request.object_id);
  encoded[104..112].copy_from_slice(&request.object_sequence.to_le_bytes());
  encoded[112..120].copy_from_slice(&request.created_at_ms.to_le_bytes());
  encoded[120..128].copy_from_slice(&payload_length.to_le_bytes());
  encoded[128..136].copy_from_slice(&request.logical_record_count.to_le_bytes());
  encoded[136..144].copy_from_slice(&request.minimum_publication_sequence.to_le_bytes());
  encoded[144..152].copy_from_slice(&request.maximum_publication_sequence.to_le_bytes());
  encoded[152..184].copy_from_slice(&request.payload_digest);
  Ok(encoded)
}

pub fn decode_index_workspace_object_v1(bytes: &[u8]) -> FormatResult<IndexWorkspaceObjectV1<'_>> {
  if bytes.len() < INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1 + 4 || bytes.len() > INDEX_WORKSPACE_OBJECT_MAX_LENGTH_V1 {
    return Err(format_error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "index_workspace_object_length",
      "workspace object length is outside the frozen bounds",
    ));
  }
  let header = decode_index_workspace_object_header_v1(&bytes[..INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1], bytes.len())?;
  let payload_end = INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1 + header.payload_length;
  if u32_at(bytes, payload_end) != crc32fast::hash(&bytes[..payload_end]) {
    return Err(format_error(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "index_workspace_object_checksum",
      "workspace object CRC32 does not match",
    ));
  }
  let payload = &bytes[INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1..payload_end];
  if blake3::hash(payload).as_bytes() != &header.payload_digest {
    return Err(format_error(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "index_workspace_payload_digest",
      "workspace object payload digest does not match",
    ));
  }
  let decoded = IndexWorkspaceObjectV1 {
    kind: header.kind,
    hash_algorithm: header.hash_algorithm,
    database_id: header.database_id,
    destination_physical_instance_id: header.destination_physical_instance_id,
    workspace_id: header.workspace_id,
    runtime_id: header.runtime_id,
    object_id: header.object_id,
    object_sequence: header.object_sequence,
    created_at_ms: header.created_at_ms,
    logical_record_count: header.logical_record_count,
    minimum_publication_sequence: header.minimum_publication_sequence,
    maximum_publication_sequence: header.maximum_publication_sequence,
    payload_digest: header.payload_digest,
    payload,
  };
  validate_index_workspace_object_payload_v1(
    decoded.kind,
    decoded.payload,
    decoded.hash_algorithm,
    decoded.logical_record_count,
    decoded.minimum_publication_sequence,
    decoded.maximum_publication_sequence,
  )?;
  Ok(decoded)
}

pub(super) fn decode_index_workspace_object_header_v1(
  bytes: &[u8],
  actual_total_length: usize,
) -> FormatResult<IndexWorkspaceObjectHeaderV1> {
  if bytes.len() != INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1
    || !(INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1 + 4..=INDEX_WORKSPACE_OBJECT_MAX_LENGTH_V1).contains(&actual_total_length)
  {
    return Err(format_error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "index_workspace_object_length",
      "workspace object header or actual length is outside the frozen bounds",
    ));
  }
  if &bytes[..4] != OBJECT_MAGIC || u16_at(bytes, 4) != OBJECT_SCHEMA_VERSION {
    return Err(format_error(
      MalformedInputClass::UnknownMagicOrVersion,
      "index_workspace_object_magic",
      "workspace object magic or schema version is unknown",
    ));
  }
  let kind = IndexWorkspaceObjectKindV1::from_id(u16_at(bytes, 6)).ok_or_else(|| {
    format_error(MalformedInputClass::UnknownTypeKindOrEnum, "index_workspace_object_kind", "workspace object kind is unknown")
  })?;
  let hash_algorithm = HashAlgorithm::from_u16(u16_at(bytes, 10)).ok_or_else(|| {
    format_error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "index_workspace_object_hash_algorithm",
      "workspace object hash algorithm is unknown",
    )
  })?;
  let total_length = usize::try_from(u64_at(bytes, 12)).map_err(|error| {
    format_error(
      MalformedInputClass::LengthCountOrArithmeticOverflow,
      "index_workspace_object_length",
      format!("declared object length exceeds this platform: {error}"),
    )
  })?;
  let payload_length = usize::try_from(u64_at(bytes, 120)).map_err(|error| {
    format_error(
      MalformedInputClass::LengthCountOrArithmeticOverflow,
      "index_workspace_payload_length",
      format!("declared payload length exceeds this platform: {error}"),
    )
  })?;
  if usize::from(u16_at(bytes, 8)) != INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1
    || total_length != actual_total_length
    || checked_object_length(payload_length)? != actual_total_length
  {
    return Err(format_error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "index_workspace_object_framing",
      "workspace object header, payload, or total length is not canonical",
    ));
  }
  if bytes[20..24].iter().any(|byte| *byte != 0) {
    return Err(format_error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "index_workspace_object_flags",
      "workspace object flags are nonzero",
    ));
  }
  let payload_digest = array_at(bytes, 152);
  if payload_digest.iter().all(|byte| *byte == 0) {
    return Err(format_error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "index_workspace_payload_digest",
      "workspace object payload digest is all zeroes",
    ));
  }
  let decoded = IndexWorkspaceObjectHeaderV1 {
    kind,
    hash_algorithm,
    database_id: array_at(bytes, 24),
    destination_physical_instance_id: array_at(bytes, 40),
    workspace_id: array_at(bytes, 56),
    runtime_id: array_at(bytes, 72),
    object_id: array_at(bytes, 88),
    object_sequence: u64_at(bytes, 104),
    created_at_ms: u64_at(bytes, 112),
    logical_record_count: u64_at(bytes, 128),
    minimum_publication_sequence: u64_at(bytes, 136),
    maximum_publication_sequence: u64_at(bytes, 144),
    payload_digest,
    payload_length,
    total_length,
  };
  validate_object_fields(
    decoded.hash_algorithm,
    decoded.database_id,
    decoded.destination_physical_instance_id,
    decoded.workspace_id,
    decoded.runtime_id,
    decoded.object_id,
    decoded.object_sequence,
    decoded.created_at_ms,
    decoded.logical_record_count,
    decoded.minimum_publication_sequence,
    decoded.maximum_publication_sequence,
    decoded.payload_length,
  )?;
  Ok(decoded)
}

pub fn index_workspace_object_digest_v1(bytes: &[u8]) -> FormatResult<[u8; 32]> {
  decode_index_workspace_object_v1(bytes)?;
  Ok(*blake3::hash(bytes).as_bytes())
}

fn checked_object_length(payload_length: usize) -> FormatResult<usize> {
  let total =
    INDEX_WORKSPACE_OBJECT_HEADER_LENGTH_V1.checked_add(payload_length).and_then(|length| length.checked_add(4)).ok_or_else(|| {
      format_error(
        MalformedInputClass::LengthCountOrArithmeticOverflow,
        "index_workspace_object_length",
        "workspace object length overflowed",
      )
    })?;
  if total > INDEX_WORKSPACE_OBJECT_MAX_LENGTH_V1 {
    return Err(format_error(
      MalformedInputClass::AllocationAmplification,
      "index_workspace_object_cap",
      format!("workspace object length {total} exceeds {INDEX_WORKSPACE_OBJECT_MAX_LENGTH_V1}"),
    ));
  }
  Ok(total)
}

#[allow(clippy::too_many_arguments)]
fn validate_object_fields(
  _hash_algorithm: HashAlgorithm,
  database_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  workspace_id: [u8; 16],
  runtime_id: [u8; 16],
  object_id: [u8; 16],
  object_sequence: u64,
  created_at_ms: u64,
  logical_record_count: u64,
  minimum_publication_sequence: u64,
  maximum_publication_sequence: u64,
  payload_length: usize,
) -> FormatResult<()> {
  if [database_id, destination_physical_instance_id, workspace_id, runtime_id, object_id]
    .iter()
    .any(|identity| identity.iter().all(|byte| *byte == 0))
  {
    return Err(format_error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "index_workspace_object_identity",
      "workspace object contains a zero identity",
    ));
  }
  if object_sequence == 0
    || created_at_ms == 0
    || logical_record_count == 0
    || minimum_publication_sequence == 0
    || maximum_publication_sequence < minimum_publication_sequence
    || payload_length == 0
  {
    return Err(format_error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "index_workspace_object_counters",
      "workspace object sequence, time, record count, publication range, or payload is invalid",
    ));
  }
  Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_manifest_fields(
  database_id: [u8; 16],
  destination_physical_instance_id: [u8; 16],
  workspace_id: [u8; 16],
  runtime_id: [u8; 16],
  manifest_sequence: u64,
  previous_manifest_digest: [u8; 32],
  object_id: [u8; 16],
  object_digest: [u8; 32],
  object_stored_bytes: u64,
  cumulative_object_count: u64,
  cumulative_stored_bytes: u64,
  created_at_ms: u64,
) -> FormatResult<()> {
  if [database_id, destination_physical_instance_id, workspace_id, runtime_id, object_id]
    .iter()
    .any(|identity| identity.iter().all(|byte| *byte == 0))
    || object_digest.iter().all(|byte| *byte == 0)
  {
    return Err(format_error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "index_workspace_manifest_identity",
      "manifest contains a zero identity or object digest",
    ));
  }
  if manifest_sequence == 0
    || created_at_ms == 0
    || object_stored_bytes == 0
    || cumulative_object_count == 0
    || cumulative_stored_bytes < object_stored_bytes
  {
    return Err(format_error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "index_workspace_manifest_counters",
      "manifest sequence, time, object count, or byte totals are invalid",
    ));
  }
  let previous_is_zero = previous_manifest_digest.iter().all(|byte| *byte == 0);
  if previous_is_zero != (manifest_sequence == 1) {
    return Err(format_error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "index_workspace_manifest_predecessor",
      "only the first manifest may have a zero predecessor digest",
    ));
  }
  Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
  let mut encoded = [0; 2];
  encoded.copy_from_slice(&bytes[offset..offset + 2]);
  u16::from_le_bytes(encoded)
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
  let mut encoded = [0; 4];
  encoded.copy_from_slice(&bytes[offset..offset + 4]);
  u32::from_le_bytes(encoded)
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
  let mut encoded = [0; 8];
  encoded.copy_from_slice(&bytes[offset..offset + 8]);
  u64::from_le_bytes(encoded)
}

fn array_at<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
  let mut encoded = [0; N];
  encoded.copy_from_slice(&bytes[offset..offset + N]);
  encoded
}

fn format_error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
