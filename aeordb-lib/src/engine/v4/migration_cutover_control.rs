//! Typed codec and identity recipes for the frozen ACUT v1 control body.
//!
//! The common `SystemControlV1` envelope remains owned by `system_control`.
//! This module gives the already-frozen `140 + 5H` body one offset-free,
//! machine-checked interpretation shared by the database control and external
//! cutover journal.

use crate::engine::HashAlgorithm;
use crate::engine::native_durability::PlatformFileIdentityDescriptorV1;

use super::hash::digest_parts;
use super::migration_control::MigrationPhaseV1;
use super::reader::{BoundedReader, FormatError, FormatResult, MalformedInputClass};
use super::system_control::{SystemControlKindV1, decode_system_control, encode_system_control};

const CUTOVER_BODY_FIXED_LENGTH: usize = 140;
const CUTOVER_BODY_HASH_COUNT: usize = 5;
const PATH_IDENTITY_DOMAIN: &[u8] = b"aeordb.side-by-side-cutover.path-identity.v1\0";
const STABLE_FILE_IDENTITY_DOMAIN: &[u8] = b"aeordb.side-by-side-cutover.stable-file-identity.v1\0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CutoverArtifactRoleV1 {
  Source = 1,
  Destination = 2,
}

impl CutoverArtifactRoleV1 {
  const fn required_format(self) -> u16 {
    match self {
      Self::Source => 3,
      Self::Destination => 4,
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutoverStableFileIdentityEvidenceV1 {
  pub role: CutoverArtifactRoleV1,
  pub database_id: [u8; 16],
  pub physical_instance_id: [u8; 16],
  pub platform_file_identity: PlatformFileIdentityDescriptorV1,
  pub format: u16,
  pub selected_header_sequence: u64,
  pub selected_header_blake3: [u8; 32],
  pub file_size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBySideCutoverBodyV1 {
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub holder_boot_id: [u8; 16],
  pub fencing_token: u64,
  pub phase: MigrationPhaseV1,
  pub journal_sequence: u64,
  pub destination_header_sequence: u64,
  pub source_file_size: u64,
  pub destination_file_size: u64,
  pub updated_at_ms: i64,
  pub source_path_identity_hash: Vec<u8>,
  pub destination_path_identity_hash: Vec<u8>,
  pub source_stable_file_identity_hash: Vec<u8>,
  pub destination_stable_file_identity_hash: Vec<u8>,
  pub last_error_evidence: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideBySideCutoverControlV1 {
  pub sequence: u64,
  pub body: SideBySideCutoverBodyV1,
}

pub fn cutover_path_identity_hash_v1(
  algorithm: HashAlgorithm,
  role: CutoverArtifactRoleV1,
  admitted_path_digest: [u8; 32],
) -> FormatResult<Vec<u8>> {
  if all_zero(&admitted_path_digest) {
    return Err(identity_error("cutover_path_identity", "admitted path digest must be nonzero"));
  }
  Ok(digest_parts(algorithm, &[PATH_IDENTITY_DOMAIN, &[role as u8], &admitted_path_digest]))
}

pub fn cutover_stable_file_identity_hash_v1(
  algorithm: HashAlgorithm,
  evidence: &CutoverStableFileIdentityEvidenceV1,
) -> FormatResult<Vec<u8>> {
  if evidence.format != evidence.role.required_format() {
    return Err(kind_error(
      "cutover_file_identity_format",
      "stable source evidence requires format 3 and destination evidence requires format 4",
    ));
  }
  if all_zero(&evidence.database_id)
    || all_zero(&evidence.physical_instance_id)
    || !valid_platform_identity(evidence.platform_file_identity)
    || all_zero(&evidence.selected_header_blake3)
  {
    return Err(identity_error("cutover_file_identity", "stable file identity evidence is incomplete"));
  }
  if evidence.selected_header_sequence == 0 || evidence.file_size == 0 {
    return Err(identity_error("cutover_file_identity_scalar", "stable file identity requires nonzero header sequence and file size"));
  }
  let descriptor = evidence.platform_file_identity.to_bytes();
  Ok(digest_parts(
    algorithm,
    &[
      STABLE_FILE_IDENTITY_DOMAIN,
      &[evidence.role as u8],
      &evidence.database_id,
      &evidence.physical_instance_id,
      &descriptor,
      &evidence.format.to_le_bytes(),
      &evidence.selected_header_sequence.to_le_bytes(),
      &evidence.selected_header_blake3,
      &evidence.file_size.to_le_bytes(),
    ],
  ))
}

pub fn encode_side_by_side_cutover_control_v1(
  sequence: u64,
  request: &SideBySideCutoverBodyV1,
  algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  validate_cutover_body(request, algorithm)?;
  let body_length = cutover_body_length(algorithm)?;
  let mut body = Vec::new();
  body
    .try_reserve_exact(body_length)
    .map_err(|error| FormatError::new(MalformedInputClass::AllocationAmplification, "cutover_control_allocation", error.to_string()))?;
  body.extend_from_slice(&request.database_id);
  body.extend_from_slice(&request.migration_id);
  body.extend_from_slice(&request.source_physical_instance_id);
  body.extend_from_slice(&request.destination_physical_instance_id);
  body.extend_from_slice(&request.holder_boot_id);
  body.extend_from_slice(&request.fencing_token.to_le_bytes());
  body.extend_from_slice(&(request.phase as u16).to_le_bytes());
  body.extend_from_slice(&0u16.to_le_bytes());
  body.extend_from_slice(&3u16.to_le_bytes());
  body.extend_from_slice(&4u16.to_le_bytes());
  body.extend_from_slice(&0u32.to_le_bytes());
  body.extend_from_slice(&request.journal_sequence.to_le_bytes());
  body.extend_from_slice(&request.destination_header_sequence.to_le_bytes());
  body.extend_from_slice(&request.source_file_size.to_le_bytes());
  body.extend_from_slice(&request.destination_file_size.to_le_bytes());
  body.extend_from_slice(&request.updated_at_ms.to_le_bytes());
  body.extend_from_slice(&request.source_path_identity_hash);
  body.extend_from_slice(&request.destination_path_identity_hash);
  body.extend_from_slice(&request.source_stable_file_identity_hash);
  body.extend_from_slice(&request.destination_stable_file_identity_hash);
  body.extend_from_slice(&request.last_error_evidence);
  if body.len() != body_length {
    return Err(length_error("typed cutover writer produced an unexpected body length"));
  }
  let encoded = encode_system_control(SystemControlKindV1::SideBySideCutover, sequence, &body, algorithm)?;
  let decoded = decode_side_by_side_cutover_control_v1(&encoded, algorithm)?;
  if decoded.sequence != sequence || decoded.body != *request {
    return Err(identity_error("cutover_control_encode_roundtrip", "encoded cutover control did not round-trip exactly"));
  }
  Ok(encoded)
}

pub fn decode_side_by_side_cutover_control_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<SideBySideCutoverControlV1> {
  let control = decode_system_control(bytes, algorithm)?;
  if control.kind != SystemControlKindV1::SideBySideCutover {
    return Err(kind_error("cutover_control_kind", "control is not a side-by-side cutover control"));
  }
  let hash_width = algorithm.hash_length();
  let expected = cutover_body_length(algorithm)?;
  let mut reader = BoundedReader::new(control.body, expected)?;
  let database_id = read_id(&mut reader)?;
  let migration_id = read_id(&mut reader)?;
  let source_physical_instance_id = read_id(&mut reader)?;
  let destination_physical_instance_id = read_id(&mut reader)?;
  let holder_boot_id = read_id(&mut reader)?;
  let fencing_token = reader.read_u64()?;
  let phase = MigrationPhaseV1::from_u16(reader.read_u16()?)?;
  let reserved_u16 = reader.read_u16()?;
  let source_format = reader.read_u16()?;
  let destination_format = reader.read_u16()?;
  let reserved_u32 = reader.read_u32()?;
  let journal_sequence = reader.read_u64()?;
  let destination_header_sequence = reader.read_u64()?;
  let source_file_size = reader.read_u64()?;
  let destination_file_size = reader.read_u64()?;
  let updated_at_ms = reader.read_i64()?;
  let source_path_identity_hash = reader.read_exact(hash_width)?.to_vec();
  let destination_path_identity_hash = reader.read_exact(hash_width)?.to_vec();
  let source_stable_file_identity_hash = reader.read_exact(hash_width)?.to_vec();
  let destination_stable_file_identity_hash = reader.read_exact(hash_width)?.to_vec();
  let last_error_evidence = reader.read_exact(hash_width)?.to_vec();
  reader.finish()?;
  if source_format != 3 || destination_format != 4 {
    return Err(kind_error("cutover_control_formats", "cutover source or destination format is invalid"));
  }
  if reserved_u16 != 0 || reserved_u32 != 0 {
    return Err(reserved_error("cutover_control_reserved", "cutover reserve must be zero"));
  }
  let body = SideBySideCutoverBodyV1 {
    database_id,
    migration_id,
    source_physical_instance_id,
    destination_physical_instance_id,
    holder_boot_id,
    fencing_token,
    phase,
    journal_sequence,
    destination_header_sequence,
    source_file_size,
    destination_file_size,
    updated_at_ms,
    source_path_identity_hash,
    destination_path_identity_hash,
    source_stable_file_identity_hash,
    destination_stable_file_identity_hash,
    last_error_evidence,
  };
  validate_cutover_body(&body, algorithm)?;
  Ok(SideBySideCutoverControlV1 { sequence: control.sequence, body })
}

fn validate_cutover_body(request: &SideBySideCutoverBodyV1, algorithm: HashAlgorithm) -> FormatResult<()> {
  if [
    &request.database_id,
    &request.migration_id,
    &request.source_physical_instance_id,
    &request.destination_physical_instance_id,
    &request.holder_boot_id,
  ]
  .into_iter()
  .any(|value| all_zero(value))
    || request.source_physical_instance_id == request.destination_physical_instance_id
  {
    return Err(identity_error("cutover_control_identity", "cutover identities must be nonzero and physical identities must differ"));
  }
  if request.fencing_token == 0
    || request.journal_sequence == 0
    || request.destination_header_sequence == 0
    || request.source_file_size == 0
    || request.destination_file_size == 0
  {
    return Err(identity_error("cutover_control_scalars", "cutover fencing, sequences, and file sizes must be nonzero"));
  }
  if request.updated_at_ms < 0 {
    return Err(closure_error("cutover_control_time", "cutover update time must be nonnegative"));
  }
  let hash_width = algorithm.hash_length();
  for hash in [
    &request.source_path_identity_hash,
    &request.destination_path_identity_hash,
    &request.source_stable_file_identity_hash,
    &request.destination_stable_file_identity_hash,
    &request.last_error_evidence,
  ] {
    if hash.len() != hash_width {
      return Err(length_error_code(
        "cutover_control_hash_length",
        format!("cutover hash has {} bytes instead of {hash_width}", hash.len()),
      ));
    }
  }
  if [
    &request.source_path_identity_hash,
    &request.destination_path_identity_hash,
    &request.source_stable_file_identity_hash,
    &request.destination_stable_file_identity_hash,
  ]
  .into_iter()
  .any(|hash| all_zero(hash))
    || request.source_path_identity_hash == request.destination_path_identity_hash
    || request.source_stable_file_identity_hash == request.destination_stable_file_identity_hash
  {
    return Err(identity_error(
      "cutover_control_hash",
      "required cutover identity hashes must be nonzero and source/destination pairs must differ",
    ));
  }
  Ok(())
}

fn cutover_body_length(algorithm: HashAlgorithm) -> FormatResult<usize> {
  CUTOVER_BODY_HASH_COUNT
    .checked_mul(algorithm.hash_length())
    .and_then(|hashes| CUTOVER_BODY_FIXED_LENGTH.checked_add(hashes))
    .ok_or_else(|| length_error("cutover body length overflow"))
}

fn read_id(reader: &mut BoundedReader<'_>) -> FormatResult<[u8; 16]> {
  reader.read_exact(16)?.try_into().map_err(|_| length_error("cutover identity has the wrong length"))
}

fn valid_platform_identity(identity: PlatformFileIdentityDescriptorV1) -> bool {
  identity.platform != 0 && identity.schema != 0 && !all_zero(&identity.volume_identity) && !all_zero(&identity.file_identity)
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn kind_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::UnknownTypeKindOrEnum, code, context)
}

fn identity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, context)
}

fn reserved_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::NonzeroReservedOrPadding, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}

fn length_error(context: impl Into<String>) -> FormatError {
  length_error_code("cutover_control_length", context)
}

fn length_error_code(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, code, context)
}
