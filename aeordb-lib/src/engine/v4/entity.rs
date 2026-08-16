use crate::engine::{CompressionAlgorithm, HashAlgorithm};

use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};

pub const WHOLE_ENTITY_V1_MAGIC: u32 = 0x0ae0_12db;
/// Highest per-type entity version supported by the v1 physical framing.
///
/// Byte 4 is the version of the complete typed entity, not a second framing
/// version. The selected v4 database layout and this record's magic/header
/// geometry identify the physical framing.
pub const WHOLE_ENTITY_V1_VERSION: u8 = 1;
pub const WHOLE_ENTITY_V1_MAX_HEADER_LENGTH: usize = 4_096;
pub const WHOLE_ENTITY_V1_KEY_CAP: usize = 1_073_741_824;
pub const WHOLE_ENTITY_V1_VALUE_CAP: usize = 1_073_741_824;
pub const WHOLE_ENTITY_V1_FLAG_SYSTEM: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryTypeV4 {
  Chunk = 0x01,
  FileRecord = 0x02,
  DirectoryIndex = 0x03,
  DeletionRecord = 0x04,
  Snapshot = 0x05,
  Void = 0x06,
  Fork = 0x07,
  Symlink = 0x08,
  IndexArtifact = 0x09,
  GcArtifact = 0x0a,
}

impl EntryTypeV4 {
  pub fn from_u8(value: u8) -> FormatResult<Self> {
    match value {
      0x01 => Ok(Self::Chunk),
      0x02 => Ok(Self::FileRecord),
      0x03 => Ok(Self::DirectoryIndex),
      0x04 => Ok(Self::DeletionRecord),
      0x05 => Ok(Self::Snapshot),
      0x06 => Ok(Self::Void),
      0x07 => Ok(Self::Fork),
      0x08 => Ok(Self::Symlink),
      0x09 => Ok(Self::IndexArtifact),
      0x0a => Ok(Self::GcArtifact),
      _ => Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "unknown_entry_type", format!("entry type {value:#04x}"))),
    }
  }

  pub fn to_u8(self) -> u8 {
    self as u8
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WholeEntityV1<'a> {
  pub entity_version: u8,
  pub entry_type: EntryTypeV4,
  pub flags: u8,
  pub hash_algorithm: HashAlgorithm,
  pub compression_algorithm: CompressionAlgorithm,
  pub timestamp_ms: u64,
  pub write_sequence: u64,
  pub integrity_hash: &'a [u8],
  pub key: &'a [u8],
  pub stored_value: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WholeEntityWriteV1<'a> {
  pub entity_version: u8,
  pub entry_type: EntryTypeV4,
  pub flags: u8,
  pub hash_algorithm: HashAlgorithm,
  pub compression_algorithm: CompressionAlgorithm,
  pub timestamp_ms: u64,
  pub write_sequence: u64,
  pub key: &'a [u8],
  pub stored_value: &'a [u8],
}

/// Return the exact encoded length after applying all component and arithmetic
/// bounds, without allocating the entity.
pub fn checked_whole_entity_encoded_length(
  hash_algorithm: HashAlgorithm,
  key_length: usize,
  stored_value_length: usize,
) -> FormatResult<usize> {
  if key_length > WHOLE_ENTITY_V1_KEY_CAP || stored_value_length > WHOLE_ENTITY_V1_VALUE_CAP {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "entity_component_exceeds_cap",
      format!("key {key_length}, value {stored_value_length}"),
    ));
  }
  let header_length = 77usize.checked_add(hash_algorithm.hash_length()).ok_or_else(|| {
    error(MalformedInputClass::LengthCountOrArithmeticOverflow, "header_length_overflow", "whole entity header length overflow")
  })?;
  if header_length > WHOLE_ENTITY_V1_MAX_HEADER_LENGTH {
    return Err(error(MalformedInputClass::AllocationAmplification, "entity_header_exceeds_cap", format!("header length {header_length}")));
  }
  let total_length = header_length
    .checked_add(key_length)
    .and_then(|length| length.checked_add(stored_value_length))
    .ok_or_else(|| error(MalformedInputClass::LengthCountOrArithmeticOverflow, "entity_length_overflow", "entity length overflow"))?;
  checked_u32(total_length, "total_length_conversion", "entity length exceeds u32")?;
  Ok(total_length)
}

/// Encode one complete v1 entity without publishing it to a database.
pub fn encode_whole_entity(request: &WholeEntityWriteV1<'_>) -> FormatResult<Vec<u8>> {
  validate_entity_version(request.entity_version)?;
  validate_entry_type_entity_version(request.entry_type, request.entity_version)?;
  if request.flags & !WHOLE_ENTITY_V1_FLAG_SYSTEM != 0 {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "unknown_entity_flags", format!("flags {:#04x}", request.flags)));
  }
  if request.write_sequence == 0 {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "unreserved_write_sequence",
      "whole entity write sequence must be nonzero",
    ));
  }

  let key_length = request.key.len();
  let value_length = request.stored_value.len();
  let total_length = checked_whole_entity_encoded_length(request.hash_algorithm, key_length, value_length)?;
  let key_length_u32 = checked_u32(key_length, "key_length_conversion", "key length exceeds u32")?;
  let value_length_u32 = checked_u32(value_length, "value_length_conversion", "value length exceeds u32")?;
  let hash_width = request.hash_algorithm.hash_length();
  let header_length = 77 + hash_width;
  let total_length_u32 = checked_u32(total_length, "total_length_conversion", "entity length exceeds u32")?;

  let mut entity = vec![0u8; total_length];
  put_u32(&mut entity, 0, WHOLE_ENTITY_V1_MAGIC);
  entity[4] = request.entity_version;
  entity[5] = request.entry_type.to_u8();
  put_u16(&mut entity, 6, header_length as u16);
  put_u32(&mut entity, 8, total_length_u32);
  entity[12] = request.flags;
  put_u16(&mut entity, 13, request.hash_algorithm.to_u16());
  entity[15] = request.compression_algorithm.to_u8();
  put_u32(&mut entity, 17, key_length_u32);
  put_u32(&mut entity, 21, value_length_u32);
  put_u64(&mut entity, 25, request.timestamp_ms);
  put_u64(&mut entity, 33, request.write_sequence);

  let integrity = digest_parts(
    request.hash_algorithm,
    &[b"aeordb-entry-v1\0", &entity[4..6], &entity[12..13], &entity[13..17], &entity[17..25], request.key, request.stored_value],
  );
  entity[41..41 + hash_width].copy_from_slice(&integrity);
  let crc_offset = header_length - 4;
  let header_crc = crc32fast::hash(&entity[..crc_offset]);
  put_u32(&mut entity, crc_offset, header_crc);
  let key_end = header_length + key_length;
  entity[header_length..key_end].copy_from_slice(request.key);
  entity[key_end..].copy_from_slice(request.stored_value);

  decode_whole_entity(&entity, request.hash_algorithm, request.write_sequence)?;
  Ok(entity)
}

pub fn decode_whole_entity<'a>(
  entity: &'a [u8],
  expected_hash_algorithm: HashAlgorithm,
  write_sequence_high_water: u64,
) -> FormatResult<WholeEntityV1<'a>> {
  if entity.len() < 12 {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "truncated_entity_prefix", "need 12-byte entity prefix"));
  }
  if u32_at(entity, 0) != WHOLE_ENTITY_V1_MAGIC {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "entity_magic_or_version", "expected whole entity v1 magic"));
  }
  let entity_version = entity[4];
  validate_entity_version(entity_version)?;

  let entry_type = EntryTypeV4::from_u8(entity[5])?;
  validate_entry_type_entity_version(entry_type, entity_version)?;
  let hash_width = expected_hash_algorithm.hash_length();
  let expected_header_length = 77usize.checked_add(hash_width).ok_or_else(|| {
    error(MalformedInputClass::LengthCountOrArithmeticOverflow, "header_length_overflow", "whole entity header length overflow")
  })?;
  let header_length = usize::from(u16_at(entity, 6));
  if header_length != expected_header_length || header_length > WHOLE_ENTITY_V1_MAX_HEADER_LENGTH || entity.len() < header_length {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "header_length",
      format!("expected {expected_header_length}, declared {header_length}, input {}", entity.len()),
    ));
  }

  let crc_offset = header_length - 4;
  let stored_crc = u32_at(entity, crc_offset);
  let computed_crc = crc32fast::hash(&entity[..crc_offset]);
  if stored_crc != computed_crc {
    return Err(error(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "header_crc_mismatch",
      format!("stored {stored_crc:#010x}, computed {computed_crc:#010x}"),
    ));
  }

  let total_length = usize::try_from(u32_at(entity, 8)).map_err(|_| {
    error(MalformedInputClass::LengthCountOrArithmeticOverflow, "total_length_conversion", "total length does not fit usize")
  })?;
  if total_length != entity.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "total_length",
      format!("declared {total_length}, input {}", entity.len()),
    ));
  }

  let flags = entity[12];
  if flags & !WHOLE_ENTITY_V1_FLAG_SYSTEM != 0 {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "unknown_entity_flags", format!("flags {flags:#04x}")));
  }
  let stored_hash_algorithm = HashAlgorithm::from_u16(u16_at(entity, 13)).ok_or_else(|| {
    error(MalformedInputClass::UnknownTypeKindOrEnum, "unknown_hash_algorithm", format!("algorithm {:#06x}", u16_at(entity, 13)))
  })?;
  if stored_hash_algorithm != expected_hash_algorithm {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "hash_algorithm_mismatch",
      format!("stored {stored_hash_algorithm:?}, database {expected_hash_algorithm:?}"),
    ));
  }
  let compression_algorithm = CompressionAlgorithm::from_u8(entity[15]).ok_or_else(|| {
    error(MalformedInputClass::UnknownTypeKindOrEnum, "unknown_compression_algorithm", format!("codec {:#04x}", entity[15]))
  })?;
  if entity[16] != 0 {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "unsupported_encryption_algorithm",
      format!("encryption algorithm {:#04x}", entity[16]),
    ));
  }

  let key_length = usize::try_from(u32_at(entity, 17))
    .map_err(|_| error(MalformedInputClass::LengthCountOrArithmeticOverflow, "key_length_conversion", "key length does not fit usize"))?;
  let value_length = usize::try_from(u32_at(entity, 21)).map_err(|_| {
    error(MalformedInputClass::LengthCountOrArithmeticOverflow, "value_length_conversion", "value length does not fit usize")
  })?;
  if key_length > WHOLE_ENTITY_V1_KEY_CAP || value_length > WHOLE_ENTITY_V1_VALUE_CAP {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "entity_component_exceeds_cap",
      format!("key {key_length}, value {value_length}"),
    ));
  }
  let key_end = header_length
    .checked_add(key_length)
    .ok_or_else(|| error(MalformedInputClass::LengthCountOrArithmeticOverflow, "key_end_overflow", "header plus key length overflow"))?;
  let value_end = key_end.checked_add(value_length).ok_or_else(|| {
    error(MalformedInputClass::LengthCountOrArithmeticOverflow, "value_end_overflow", "key end plus value length overflow")
  })?;
  if value_end != entity.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "entity_length_disagreement",
      format!("calculated {value_end}, input {}", entity.len()),
    ));
  }

  let write_sequence = u64_at(entity, 33);
  if write_sequence == 0 || write_sequence > write_sequence_high_water {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "unreserved_write_sequence",
      format!("sequence {write_sequence}, high water {write_sequence_high_water}"),
    ));
  }
  if entity[41 + hash_width..crc_offset].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "reserved_nonzero", "whole entity v1 reserve"));
  }

  let integrity_hash = &entity[41..41 + hash_width];
  let key = &entity[header_length..key_end];
  let stored_value = &entity[key_end..value_end];
  let computed_integrity = digest_parts(
    expected_hash_algorithm,
    &[b"aeordb-entry-v1\0", &entity[4..6], &entity[12..13], &entity[13..17], &entity[17..25], key, stored_value],
  );
  if integrity_hash != computed_integrity {
    return Err(error(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "integrity_hash_mismatch",
      "whole entity content hash does not match",
    ));
  }

  Ok(WholeEntityV1 {
    entity_version,
    entry_type,
    flags,
    hash_algorithm: stored_hash_algorithm,
    compression_algorithm,
    timestamp_ms: u64_at(entity, 25),
    write_sequence,
    integrity_hash,
    key,
    stored_value,
  })
}

fn validate_entity_version(entity_version: u8) -> FormatResult<()> {
  if entity_version > WHOLE_ENTITY_V1_VERSION {
    return Err(error(
      MalformedInputClass::UnknownMagicOrVersion,
      "unsupported_entity_version",
      format!("whole entity version {entity_version} exceeds supported version {WHOLE_ENTITY_V1_VERSION}"),
    ));
  }
  Ok(())
}

fn validate_entry_type_entity_version(entry_type: EntryTypeV4, entity_version: u8) -> FormatResult<()> {
  let supported = match entry_type {
    EntryTypeV4::Chunk
    | EntryTypeV4::DeletionRecord
    | EntryTypeV4::Snapshot
    | EntryTypeV4::Void
    | EntryTypeV4::Fork
    | EntryTypeV4::Symlink => entity_version == 0,
    EntryTypeV4::FileRecord | EntryTypeV4::DirectoryIndex => matches!(entity_version, 0 | 1),
    EntryTypeV4::IndexArtifact | EntryTypeV4::GcArtifact => entity_version == 1,
  };
  if !supported {
    return Err(error(
      MalformedInputClass::UnknownMagicOrVersion,
      "unsupported_entry_type_entity_version",
      format!("{entry_type:?} entity version {entity_version} is not a registered codec"),
    ));
  }
  Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
  u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("validated entity bounds"))
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("validated entity bounds"))
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
  u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("validated entity bounds"))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn checked_u32(value: usize, code: &'static str, context: &'static str) -> FormatResult<u32> {
  if value > u32::MAX as usize {
    return Err(error(MalformedInputClass::LengthCountOrArithmeticOverflow, code, context));
  }
  Ok(value as u32)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
