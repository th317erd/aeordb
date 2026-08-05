use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Read, Seek, SeekFrom};

use crate::engine::errors::EngineError;
use crate::engine::file_header::{FileHeader, HEADER_REGION_SIZE, decode_active_header_region};
use crate::engine::hash_algorithm::HashAlgorithm;

use super::reader::{FormatError, FormatResult, MalformedInputClass};

pub const DATABASE_HEADER_V4_SLOT_LENGTH: usize = 1_024;
pub const DATABASE_HEADER_V4_REGION_LENGTH: usize = DATABASE_HEADER_V4_SLOT_LENGTH * 2;
pub const DATABASE_HEADER_V4_DATA_OFFSET: u64 = DATABASE_HEADER_V4_REGION_LENGTH as u64;
const CRC_OFFSET: usize = 1_020;
const KNOWN_CAPABILITY_BYTES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseHeaderVersion {
  V3,
  V4,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseHeaderV4 {
  pub hash_algorithm: HashAlgorithm,
  pub slot_sequence: u64,
  pub created_at_ms: u64,
  pub updated_at_ms: u64,
  pub database_id: [u8; 16],
  pub write_sequence_high_water: u64,
  pub required_reader_capabilities: [u8; 32],
  pub kv_block_offset: u64,
  pub kv_block_length: u64,
  pub kv_block_version: u8,
  pub kv_block_stage: u8,
  pub resize_in_progress: bool,
  pub resize_target_stage: u8,
  pub nvt_offset: u64,
  pub nvt_length: u64,
  pub nvt_version: u8,
  pub backup_type: u8,
  pub hot_tail_offset: u64,
  pub buffer_kvs_offset: u64,
  pub buffer_nvt_offset: u64,
  pub entry_count: u64,
  pub head_hash: Vec<u8>,
  pub base_hash: Vec<u8>,
  pub target_hash: Vec<u8>,
  pub required_writer_capabilities: [u8; 32],
  pub system_family_registry_version: u16,
  pub system_family_registry_fingerprint: Vec<u8>,
  pub writer_fence_epoch: u64,
  pub physical_instance_id: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedDatabaseHeaderV4 {
  pub header: DatabaseHeaderV4,
  pub selected_slot: usize,
  pub redundancy_degraded: bool,
}

#[derive(Debug, Clone)]
pub enum ReadOnlyDatabaseHeader {
  V3 { header: FileHeader, selected_slot: usize },
  V4(SelectedDatabaseHeaderV4),
}

impl ReadOnlyDatabaseHeader {
  pub fn version(&self) -> DatabaseHeaderVersion {
    match self {
      Self::V3 { .. } => DatabaseHeaderVersion::V3,
      Self::V4(_) => DatabaseHeaderVersion::V4,
    }
  }

  pub fn data_offset(&self) -> u64 {
    match self {
      Self::V3 { .. } => HEADER_REGION_SIZE as u64,
      Self::V4(_) => DATABASE_HEADER_V4_DATA_OFFSET,
    }
  }
}

#[derive(Debug)]
pub enum DatabaseHeaderReadError {
  Io(io::Error),
  Probe(FormatError),
  V3(EngineError),
  V4(FormatError),
}

impl Display for DatabaseHeaderReadError {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
    match self {
      Self::Io(error) => write!(formatter, "database header I/O failed: {error}"),
      Self::Probe(error) => write!(formatter, "database header format probe failed: {error}"),
      Self::V3(error) => write!(formatter, "v3 database header is invalid: {error}"),
      Self::V4(error) => write!(formatter, "v4 database header is invalid: {error}"),
    }
  }
}

impl Error for DatabaseHeaderReadError {
  fn source(&self) -> Option<&(dyn Error + 'static)> {
    match self {
      Self::Io(error) => Some(error),
      Self::Probe(error) => Some(error),
      Self::V3(error) => Some(error),
      Self::V4(error) => Some(error),
    }
  }
}

/// Probe and decode a v3 or v4 database header without writing either format.
///
/// The reader is always sought to byte zero before the probe and again before
/// the selected fixed-size region is read. The returned variant keeps legacy
/// and v4 policy separate for later capability admission.
pub fn read_database_header_read_only(reader: &mut (impl Read + Seek)) -> Result<ReadOnlyDatabaseHeader, DatabaseHeaderReadError> {
  reader.seek(SeekFrom::Start(0)).map_err(DatabaseHeaderReadError::Io)?;
  let mut prefix = [0u8; 5];
  let prefix_length = read_up_to(reader, &mut prefix).map_err(DatabaseHeaderReadError::Io)?;
  let version = probe_header_version(&prefix[..prefix_length]).map_err(DatabaseHeaderReadError::Probe)?;
  reader.seek(SeekFrom::Start(0)).map_err(DatabaseHeaderReadError::Io)?;

  match version {
    DatabaseHeaderVersion::V3 => {
      let mut region = [0u8; HEADER_REGION_SIZE];
      let region_length = read_up_to(reader, &mut region).map_err(DatabaseHeaderReadError::Io)?;
      let (header, selected_slot) = decode_active_header_region(&region[..region_length]).map_err(DatabaseHeaderReadError::V3)?;
      Ok(ReadOnlyDatabaseHeader::V3 { header, selected_slot })
    }
    DatabaseHeaderVersion::V4 => {
      let mut region = [0u8; DATABASE_HEADER_V4_REGION_LENGTH];
      let region_length = read_up_to(reader, &mut region).map_err(DatabaseHeaderReadError::Io)?;
      decode_header_region(&region[..region_length]).map(ReadOnlyDatabaseHeader::V4).map_err(DatabaseHeaderReadError::V4)
    }
  }
}

fn read_up_to(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
  let mut filled = 0;
  while filled < buffer.len() {
    match reader.read(&mut buffer[filled..]) {
      Ok(0) => break,
      Ok(read) => filled += read,
      Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
      Err(error) => return Err(error),
    }
  }
  Ok(filled)
}

pub fn probe_header_version(prefix: &[u8]) -> FormatResult<DatabaseHeaderVersion> {
  if prefix.len() < 5 {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "truncated_header_probe",
      "database header probe requires at least five bytes",
    ));
  }
  if &prefix[..4] != b"AEOR" {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "unknown_header_magic", "expected AEOR"));
  }
  match prefix[4] {
    3 => Ok(DatabaseHeaderVersion::V3),
    4 => Ok(DatabaseHeaderVersion::V4),
    version => Err(error(
      MalformedInputClass::UnknownMagicOrVersion,
      "unknown_header_version",
      format!("unsupported database header version {version}"),
    )),
  }
}

pub fn read_header_region(reader: &mut impl Read) -> FormatResult<SelectedDatabaseHeaderV4> {
  let mut region = [0u8; DATABASE_HEADER_V4_REGION_LENGTH];
  reader.read_exact(&mut region).map_err(|source| {
    error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "truncated_header_region",
      format!("could not read {DATABASE_HEADER_V4_REGION_LENGTH}-byte header region: {source}"),
    )
  })?;
  decode_header_region(&region)
}

pub fn decode_header_region(region: &[u8]) -> FormatResult<SelectedDatabaseHeaderV4> {
  if region.len() != DATABASE_HEADER_V4_REGION_LENGTH {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "header_region_length",
      format!("expected {DATABASE_HEADER_V4_REGION_LENGTH} bytes, got {}", region.len()),
    ));
  }

  let slot_a_bytes = &region[..DATABASE_HEADER_V4_SLOT_LENGTH];
  let slot_b_bytes = &region[DATABASE_HEADER_V4_SLOT_LENGTH..];
  let slot_a = decode_slot(slot_a_bytes);
  let slot_b = decode_slot(slot_b_bytes);

  match (slot_a, slot_b) {
    (Ok(header_a), Ok(header_b)) => {
      if header_a.slot_sequence > header_b.slot_sequence {
        Ok(selected(header_a, 0, false))
      } else if header_b.slot_sequence > header_a.slot_sequence {
        Ok(selected(header_b, 1, false))
      } else if slot_a_bytes == slot_b_bytes {
        Ok(selected(header_a, 0, false))
      } else {
        Err(error(
          MalformedInputClass::AmbiguousEqualSequenceSelector,
          "ambiguous_equal_sequence",
          format!("slots disagree at sequence {}", header_a.slot_sequence),
        ))
      }
    }
    (Ok(header), Err(_)) => Ok(selected(header, 0, true)),
    (Err(_), Ok(header)) => Ok(selected(header, 1, true)),
    (Err(left), Err(right)) if left.code() == right.code() && left.code() != "crc_mismatch" => Err(left),
    (Err(left), Err(right)) => Err(error(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "no_valid_slot",
      format!("slot A: {}; slot B: {}", left.code(), right.code()),
    )),
  }
}

fn selected(header: DatabaseHeaderV4, selected_slot: usize, redundancy_degraded: bool) -> SelectedDatabaseHeaderV4 {
  SelectedDatabaseHeaderV4 { header, selected_slot, redundancy_degraded }
}

fn decode_slot(slot: &[u8]) -> FormatResult<DatabaseHeaderV4> {
  if slot.len() != DATABASE_HEADER_V4_SLOT_LENGTH {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "slot_length", "v4 header slot is not 1024 bytes"));
  }

  let stored_crc = u32_at(slot, CRC_OFFSET);
  let computed_crc = crc32fast::hash(&slot[..CRC_OFFSET]);
  if stored_crc != computed_crc {
    return Err(error(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "crc_mismatch",
      format!("stored {stored_crc:#010x}, computed {computed_crc:#010x}"),
    ));
  }
  if &slot[..4] != b"AEOR" {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "magic", "expected AEOR"));
  }
  if slot[4] != 4 || u16_at(slot, 5) as usize != DATABASE_HEADER_V4_SLOT_LENGTH {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "version_or_slot_length", "expected v4 and a 1024-byte slot"));
  }
  require_zero(&slot[7..8], "reserved_nonzero", "header flags")?;
  require_zero(&slot[386..392], "reserved_nonzero", "registry reserve")?;
  require_zero(&slot[480..CRC_OFFSET], "reserved_nonzero", "future header reserve")?;

  let hash_algorithm = HashAlgorithm::from_u16(u16_at(slot, 8)).ok_or_else(|| {
    error(MalformedInputClass::UnknownTypeKindOrEnum, "hash_algorithm", format!("unknown algorithm {:#06x}", u16_at(slot, 8)))
  })?;
  let hash_width = hash_algorithm.hash_length();

  let required_reader_capabilities: [u8; 32] = slot[58..90].try_into().expect("fixed capability field");
  let required_writer_capabilities: [u8; 32] = slot[352..384].try_into().expect("fixed capability field");
  validate_capabilities(&required_reader_capabilities, "reader")?;
  validate_capabilities(&required_writer_capabilities, "writer")?;

  let resize_in_progress = match slot[108] {
    0 => false,
    1 => true,
    value => {
      return Err(error(
        MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
        "noncanonical_boolean",
        format!("resize_in_progress is {value}"),
      ));
    }
  };
  if slot[106] != 1 || slot[126] != 1 {
    return Err(error(
      MalformedInputClass::UnknownMagicOrVersion,
      "unsupported_region_version",
      format!("KV version {}, NVT version {}", slot[106], slot[126]),
    ));
  }
  if slot[127] > 2 {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "backup_type", format!("unknown backup type {}", slot[127])));
  }

  let database_id: [u8; 16] = slot[34..50].try_into().expect("fixed database ID");
  let physical_instance_id: [u8; 16] = slot[464..480].try_into().expect("fixed physical ID");
  if all_zero(&database_id) || all_zero(&physical_instance_id) {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "zero_identity",
      "database_id and physical_instance_id must be nonzero",
    ));
  }

  let system_family_registry_version = u16_at(slot, 384);
  let writer_fence_epoch = u64_at(slot, 456);
  if system_family_registry_version == 0 || writer_fence_epoch == 0 {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "zero_registry_or_fence",
      "registry version and writer fence must be nonzero",
    ));
  }

  let kv_block_offset = u64_at(slot, 90);
  let kv_block_length = u64_at(slot, 98);
  let nvt_offset = u64_at(slot, 110);
  let nvt_length = u64_at(slot, 118);
  let hot_tail_offset = u64_at(slot, 128);
  let kv_end = checked_end(kv_block_offset, kv_block_length, "KV")?;
  let nvt_end = checked_end(nvt_offset, nvt_length, "NVT")?;
  if kv_block_offset < DATABASE_HEADER_V4_DATA_OFFSET || kv_end > nvt_offset || nvt_end > hot_tail_offset {
    return Err(error(
      MalformedInputClass::LengthCountOrArithmeticOverflow,
      "region_overlap",
      format!("KV {kv_block_offset}..{kv_end}, NVT {nvt_offset}..{nvt_end}, hot tail {hot_tail_offset}"),
    ));
  }

  Ok(DatabaseHeaderV4 {
    hash_algorithm,
    slot_sequence: u64_at(slot, 10),
    created_at_ms: u64_at(slot, 18),
    updated_at_ms: u64_at(slot, 26),
    database_id,
    write_sequence_high_water: u64_at(slot, 50),
    required_reader_capabilities,
    kv_block_offset,
    kv_block_length,
    kv_block_version: slot[106],
    kv_block_stage: slot[107],
    resize_in_progress,
    resize_target_stage: slot[109],
    nvt_offset,
    nvt_length,
    nvt_version: slot[126],
    backup_type: slot[127],
    hot_tail_offset,
    buffer_kvs_offset: u64_at(slot, 136),
    buffer_nvt_offset: u64_at(slot, 144),
    entry_count: u64_at(slot, 152),
    head_hash: hash_at(slot, 160, hash_width)?,
    base_hash: hash_at(slot, 224, hash_width)?,
    target_hash: hash_at(slot, 288, hash_width)?,
    required_writer_capabilities,
    system_family_registry_version,
    system_family_registry_fingerprint: hash_at(slot, 392, hash_width)?,
    writer_fence_epoch,
    physical_instance_id,
  })
}

pub(crate) fn validate_capabilities(capabilities: &[u8; 32], role: &str) -> FormatResult<()> {
  if capabilities[KNOWN_CAPABILITY_BYTES..].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::UnknownRequiredCapability,
      "unsupported_required_capability",
      format!("{role} capability bit 24 or greater is set"),
    ));
  }
  Ok(())
}

fn hash_at(slot: &[u8], offset: usize, width: usize) -> FormatResult<Vec<u8>> {
  if slot[offset + width..offset + 64].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "hash_padding_nonzero",
      format!("hash slot at {offset} has nonzero padding"),
    ));
  }
  Ok(slot[offset..offset + width].to_vec())
}

fn require_zero(bytes: &[u8], code: &'static str, context: &'static str) -> FormatResult<()> {
  if bytes.iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, code, context));
  }
  Ok(())
}

fn checked_end(offset: u64, length: u64, region: &'static str) -> FormatResult<u64> {
  offset.checked_add(length).ok_or_else(|| {
    error(MalformedInputClass::LengthCountOrArithmeticOverflow, "offset_overflow", format!("{region} offset {offset} plus length {length}"))
  })
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
  u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("fixed slot bounds"))
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed slot bounds"))
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
  u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed slot bounds"))
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
