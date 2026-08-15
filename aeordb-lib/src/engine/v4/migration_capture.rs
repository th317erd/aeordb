//! Fixed, independently verifiable checkpoints for external migration capture.
//!
//! AMCM is a constant-size selected checkpoint. Immutable capture segments are
//! ordinal-named and hash-chained outside this format, so reopening never
//! allocates a descriptor table proportional to capture size.

use crate::engine::HashAlgorithm;

use super::hash::digest_parts;
use super::reader::{BoundedReader, FormatError, FormatResult, MalformedInputClass};

const MAGIC: &[u8; 4] = b"AMCM";
const VERSION: u16 = 1;
const FIXED_PREFIX_LENGTH: usize = 176;
const HASH_COUNT: usize = 7;
const SOURCE_AUTHORITY_DIGEST_LENGTH: usize = 32;
const RESERVED_LENGTH: usize = 64;
const CRC_LENGTH: usize = 4;
const MANIFEST_IDENTITY_DOMAIN: &[u8] = b"aeordb.migration-capture-manifest.v1\0";

pub const MIGRATION_CAPTURE_FLAG_NEEDS_FULL_RECONCILE: u32 = 1 << 0;
pub const MIGRATION_CAPTURE_FLAG_OPTIONAL_CAPTURE_STOPPED: u32 = 1 << 1;
const KNOWN_FLAGS: u32 = MIGRATION_CAPTURE_FLAG_NEEDS_FULL_RECONCILE | MIGRATION_CAPTURE_FLAG_OPTIONAL_CAPTURE_STOPPED;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum MigrationCaptureManifestStateV1 {
  Capturing = 1,
  NeedsFullReconcile = 2,
  Canceled = 3,
  Failed = 4,
}

impl MigrationCaptureManifestStateV1 {
  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::Capturing),
      2 => Ok(Self::NeedsFullReconcile),
      3 => Ok(Self::Canceled),
      4 => Ok(Self::Failed),
      _ => Err(error(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "migration_capture_state",
        "capture manifest state is outside the frozen enum",
      )),
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCaptureManifestWriteV1 {
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub fencing_token: u64,
  pub capture_generation: u64,
  pub checkpoint_sequence: u64,
  pub state: MigrationCaptureManifestStateV1,
  pub flags: u32,
  pub created_at_ms: i64,
  pub updated_at_ms: i64,
  pub captured_through_publication_sequence: u64,
  pub observed_through_publication_sequence: u64,
  pub first_segment_ordinal: u64,
  pub last_segment_ordinal: u64,
  pub segment_count: u64,
  pub segment_stored_bytes: u64,
  pub source_root_before: Vec<u8>,
  pub source_root_after: Vec<u8>,
  pub segment_head: Vec<u8>,
  pub previous_manifest: Vec<u8>,
  pub effective_config_fingerprint: Vec<u8>,
  pub system_family_registry_fingerprint: Vec<u8>,
  pub failure_evidence: Vec<u8>,
  pub source_authority_digest: [u8; 32],
}

pub fn encode_migration_capture_manifest(request: &MigrationCaptureManifestWriteV1, algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_manifest(request, algorithm)?;
  let expected = manifest_length(algorithm)?;
  let expected_u32 = u32::try_from(expected).map_err(|_| length_error("manifest length does not fit u32"))?;
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(expected)
    .map_err(|source| error(MalformedInputClass::AllocationAmplification, "migration_capture_allocation", source.to_string()))?;
  bytes.extend_from_slice(MAGIC);
  bytes.extend_from_slice(&VERSION.to_le_bytes());
  bytes.extend_from_slice(&algorithm.to_u16().to_le_bytes());
  bytes.extend_from_slice(&(request.state as u16).to_le_bytes());
  bytes.extend_from_slice(&0u16.to_le_bytes());
  bytes.extend_from_slice(&request.flags.to_le_bytes());
  bytes.extend_from_slice(&expected_u32.to_le_bytes());
  bytes.extend_from_slice(&0u32.to_le_bytes());
  bytes.extend_from_slice(&request.database_id);
  bytes.extend_from_slice(&request.migration_id);
  bytes.extend_from_slice(&request.source_physical_instance_id);
  bytes.extend_from_slice(&request.destination_physical_instance_id);
  bytes.extend_from_slice(&request.fencing_token.to_le_bytes());
  bytes.extend_from_slice(&request.capture_generation.to_le_bytes());
  bytes.extend_from_slice(&request.checkpoint_sequence.to_le_bytes());
  bytes.extend_from_slice(&request.created_at_ms.to_le_bytes());
  bytes.extend_from_slice(&request.updated_at_ms.to_le_bytes());
  bytes.extend_from_slice(&request.captured_through_publication_sequence.to_le_bytes());
  bytes.extend_from_slice(&request.observed_through_publication_sequence.to_le_bytes());
  bytes.extend_from_slice(&request.first_segment_ordinal.to_le_bytes());
  bytes.extend_from_slice(&request.last_segment_ordinal.to_le_bytes());
  bytes.extend_from_slice(&request.segment_count.to_le_bytes());
  bytes.extend_from_slice(&request.segment_stored_bytes.to_le_bytes());
  bytes.extend_from_slice(&request.source_root_before);
  bytes.extend_from_slice(&request.source_root_after);
  bytes.extend_from_slice(&request.segment_head);
  bytes.extend_from_slice(&request.previous_manifest);
  bytes.extend_from_slice(&request.effective_config_fingerprint);
  bytes.extend_from_slice(&request.system_family_registry_fingerprint);
  bytes.extend_from_slice(&request.failure_evidence);
  bytes.extend_from_slice(&request.source_authority_digest);
  bytes.resize(expected - CRC_LENGTH, 0);
  let crc = crc32fast::hash(&bytes);
  bytes.extend_from_slice(&crc.to_le_bytes());
  if bytes.len() != expected {
    return Err(length_error("manifest writer produced an unexpected length"));
  }
  let decoded = decode_migration_capture_manifest(&bytes, algorithm)?;
  if decoded != *request {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "migration_capture_encode_roundtrip",
      "encoded manifest did not round-trip exactly",
    ));
  }
  Ok(bytes)
}

pub fn decode_migration_capture_manifest(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<MigrationCaptureManifestWriteV1> {
  let expected = manifest_length(algorithm)?;
  if bytes.len() != expected {
    return Err(length_error(format!("manifest has {} bytes, expected {expected}", bytes.len())));
  }
  let (encoded, encoded_crc) = bytes.split_at(expected - CRC_LENGTH);
  let expected_crc = u32::from_le_bytes(encoded_crc.try_into().map_err(|_| length_error("manifest CRC is truncated"))?);
  if crc32fast::hash(encoded) != expected_crc {
    return Err(error(MalformedInputClass::ChecksumOrIntegrityMismatch, "migration_capture_crc", "capture manifest CRC does not match"));
  }

  let mut reader = BoundedReader::new(encoded, expected)?;
  if reader.read_exact(4)? != MAGIC || reader.read_u16()? != VERSION {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "migration_capture_envelope", "expected AMCM schema version 1"));
  }
  let encoded_algorithm = HashAlgorithm::from_u16(reader.read_u16()?).ok_or_else(|| {
    error(MalformedInputClass::UnknownTypeKindOrEnum, "migration_capture_hash_algorithm", "capture hash algorithm is unknown")
  })?;
  if encoded_algorithm != algorithm {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "migration_capture_hash_algorithm",
      "capture hash algorithm does not match the selected database profile",
    ));
  }
  let state = MigrationCaptureManifestStateV1::from_u16(reader.read_u16()?)?;
  if reader.read_u16()? != 0 {
    return Err(reserved_error());
  }
  let flags = reader.read_u32()?;
  if usize::try_from(reader.read_u32()?).map_err(|_| length_error("declared manifest length does not fit usize"))? != expected {
    return Err(length_error("declared manifest length does not match framing"));
  }
  if reader.read_u32()? != 0 {
    return Err(reserved_error());
  }
  let request = MigrationCaptureManifestWriteV1 {
    database_id: read_id(&mut reader)?,
    migration_id: read_id(&mut reader)?,
    source_physical_instance_id: read_id(&mut reader)?,
    destination_physical_instance_id: read_id(&mut reader)?,
    fencing_token: reader.read_u64()?,
    capture_generation: reader.read_u64()?,
    checkpoint_sequence: reader.read_u64()?,
    state,
    flags,
    created_at_ms: reader.read_i64()?,
    updated_at_ms: reader.read_i64()?,
    captured_through_publication_sequence: reader.read_u64()?,
    observed_through_publication_sequence: reader.read_u64()?,
    first_segment_ordinal: reader.read_u64()?,
    last_segment_ordinal: reader.read_u64()?,
    segment_count: reader.read_u64()?,
    segment_stored_bytes: reader.read_u64()?,
    source_root_before: reader.read_exact(algorithm.hash_length())?.to_vec(),
    source_root_after: reader.read_exact(algorithm.hash_length())?.to_vec(),
    segment_head: reader.read_exact(algorithm.hash_length())?.to_vec(),
    previous_manifest: reader.read_exact(algorithm.hash_length())?.to_vec(),
    effective_config_fingerprint: reader.read_exact(algorithm.hash_length())?.to_vec(),
    system_family_registry_fingerprint: reader.read_exact(algorithm.hash_length())?.to_vec(),
    failure_evidence: reader.read_exact(algorithm.hash_length())?.to_vec(),
    source_authority_digest: reader
      .read_exact(SOURCE_AUTHORITY_DIGEST_LENGTH)?
      .try_into()
      .map_err(|_| length_error("source authority digest is truncated"))?,
  };
  if reader.read_exact(RESERVED_LENGTH)?.iter().any(|byte| *byte != 0) {
    return Err(reserved_error());
  }
  reader.finish()?;
  validate_manifest(&request, algorithm)?;
  Ok(request)
}

pub fn migration_capture_manifest_identity(bytes: &[u8], algorithm: HashAlgorithm) -> Vec<u8> {
  digest_parts(algorithm, &[MANIFEST_IDENTITY_DOMAIN, bytes])
}

fn validate_manifest(request: &MigrationCaptureManifestWriteV1, algorithm: HashAlgorithm) -> FormatResult<()> {
  if [&request.database_id, &request.migration_id, &request.source_physical_instance_id, &request.destination_physical_instance_id]
    .into_iter()
    .any(|value| all_zero(value))
    || request.source_physical_instance_id == request.destination_physical_instance_id
    || request.fencing_token == 0
    || request.capture_generation == 0
    || request.checkpoint_sequence == 0
    || all_zero(&request.source_authority_digest)
  {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "migration_capture_identity",
      "capture identities, generations, and fencing token must be nonzero and source/destination must differ",
    ));
  }
  if request.created_at_ms < 0 || request.updated_at_ms < request.created_at_ms {
    return Err(error(MalformedInputClass::CrossRecordClosureMismatch, "migration_capture_time", "capture timestamps are invalid"));
  }
  if request.observed_through_publication_sequence < request.captured_through_publication_sequence {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "migration_capture_sequence",
      "capture publication watermarks are invalid",
    ));
  }
  validate_state_flags(request)?;
  validate_segment_closure(request)?;

  let width = algorithm.hash_length();
  for value in [
    &request.source_root_before,
    &request.source_root_after,
    &request.segment_head,
    &request.previous_manifest,
    &request.effective_config_fingerprint,
    &request.system_family_registry_fingerprint,
    &request.failure_evidence,
  ] {
    if value.len() != width {
      return Err(error(
        MalformedInputClass::LengthCountOrArithmeticOverflow,
        "migration_capture_hash_width",
        format!("capture hash has width {}, expected {width}", value.len()),
      ));
    }
  }
  if all_zero(&request.source_root_before)
    || all_zero(&request.source_root_after)
    || all_zero(&request.effective_config_fingerprint)
    || all_zero(&request.system_family_registry_fingerprint)
  {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "migration_capture_required_hash",
      "capture source and policy closure hashes must be nonzero",
    ));
  }
  if request.checkpoint_sequence == 1 {
    if !all_zero(&request.previous_manifest) {
      return Err(error(
        MalformedInputClass::CrossRecordClosureMismatch,
        "migration_capture_previous_manifest",
        "first capture checkpoint cannot name a previous manifest",
      ));
    }
  } else if all_zero(&request.previous_manifest) {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "migration_capture_previous_manifest",
      "later capture checkpoint must name its predecessor",
    ));
  }
  let needs_evidence = !matches!(request.state, MigrationCaptureManifestStateV1::Capturing);
  if needs_evidence == all_zero(&request.failure_evidence) {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "migration_capture_failure_evidence",
      "terminal or incomplete capture state must retain nonzero evidence and capturing state must not",
    ));
  }
  Ok(())
}

fn validate_state_flags(request: &MigrationCaptureManifestWriteV1) -> FormatResult<()> {
  if request.flags & !KNOWN_FLAGS != 0 {
    return Err(error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "migration_capture_state_flags",
      "capture manifest contains unknown flags",
    ));
  }
  let needs = request.flags & MIGRATION_CAPTURE_FLAG_NEEDS_FULL_RECONCILE != 0;
  let stopped = request.flags & MIGRATION_CAPTURE_FLAG_OPTIONAL_CAPTURE_STOPPED != 0;
  let valid = match request.state {
    MigrationCaptureManifestStateV1::Capturing => {
      !needs && !stopped && request.observed_through_publication_sequence == request.captured_through_publication_sequence
    }
    MigrationCaptureManifestStateV1::NeedsFullReconcile => needs && stopped,
    MigrationCaptureManifestStateV1::Canceled | MigrationCaptureManifestStateV1::Failed => stopped,
  };
  if !valid {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "migration_capture_state_flags",
      "capture state, flags, and watermarks disagree",
    ));
  }
  Ok(())
}

fn validate_segment_closure(request: &MigrationCaptureManifestWriteV1) -> FormatResult<()> {
  if request.segment_count == 0 {
    if request.first_segment_ordinal != 0
      || request.last_segment_ordinal != 0
      || request.segment_stored_bytes != 0
      || !all_zero(&request.segment_head)
      || request.source_root_before != request.source_root_after
    {
      return Err(segment_closure_error());
    }
    return Ok(());
  }
  if request.captured_through_publication_sequence == 0 {
    return Err(segment_closure_error());
  }
  let expected_count = request
    .last_segment_ordinal
    .checked_sub(request.first_segment_ordinal)
    .and_then(|span| span.checked_add(1))
    .ok_or_else(segment_closure_error)?;
  if request.first_segment_ordinal == 0
    || request.segment_count != expected_count
    || request.segment_stored_bytes < request.segment_count
    || all_zero(&request.segment_head)
  {
    return Err(segment_closure_error());
  }
  Ok(())
}

fn manifest_length(algorithm: HashAlgorithm) -> FormatResult<usize> {
  HASH_COUNT
    .checked_mul(algorithm.hash_length())
    .and_then(|hashes| FIXED_PREFIX_LENGTH.checked_add(hashes))
    .and_then(|length| length.checked_add(SOURCE_AUTHORITY_DIGEST_LENGTH))
    .and_then(|length| length.checked_add(RESERVED_LENGTH))
    .and_then(|length| length.checked_add(CRC_LENGTH))
    .ok_or_else(|| length_error("capture manifest length overflow"))
}

fn read_id(reader: &mut BoundedReader<'_>) -> FormatResult<[u8; 16]> {
  reader.read_exact(16)?.try_into().map_err(|_| length_error("capture identity is truncated"))
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn segment_closure_error() -> FormatError {
  error(
    MalformedInputClass::CrossRecordClosureMismatch,
    "migration_capture_segment_closure",
    "capture segment ordinals, count, bytes, head, or roots disagree",
  )
}

fn reserved_error() -> FormatError {
  error(MalformedInputClass::NonzeroReservedOrPadding, "migration_capture_reserved", "capture manifest reserve must be zero")
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "migration_capture_length", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
