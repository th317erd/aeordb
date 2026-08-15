//! Typed codecs for the frozen AMLE and AMPR v1 migration controls.
//!
//! Common framing, CRC validation, body caps, and A/B selection remain owned
//! by `system_control`. This module provides one offset-free interpretation of
//! migration lease and progress bodies for the migration state owner.

use crate::engine::HashAlgorithm;

use super::reader::{BoundedReader, FormatError, FormatResult, MalformedInputClass};
use super::system_control::{SystemControlKindV1, decode_system_control, encode_system_control};

const MIGRATION_LEASE_BODY_LENGTH: usize = 132;
const MIGRATION_PROGRESS_FIXED_LENGTH: usize = 156;
const MIGRATION_PROGRESS_HASH_COUNT: usize = 6;
const MIGRATION_PROGRESS_FLAGS: u32 = MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED
  | MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD
  | MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED
  | MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE;

pub const MIGRATION_PROGRESS_FLAG_SOURCE_GC_SUSPENDED: u32 = 1 << 0;
pub const MIGRATION_PROGRESS_FLAG_SOURCE_WRITE_FREEZE_HELD: u32 = 1 << 1;
pub const MIGRATION_PROGRESS_FLAG_DESTINATION_FULL_VERIFIED: u32 = 1 << 2;
pub const MIGRATION_PROGRESS_FLAG_NEEDS_FULL_RECONCILE: u32 = 1 << 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum MigrationLeaseStateV1 {
  Held = 1,
  Releasing = 2,
  Released = 3,
  Expired = 4,
}

impl MigrationLeaseStateV1 {
  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::Held),
      2 => Ok(Self::Releasing),
      3 => Ok(Self::Released),
      4 => Ok(Self::Expired),
      _ => Err(kind_error("migration_lease_state", "migration lease state is outside the frozen enum")),
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationLeaseBodyV1 {
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub holder_boot_id: [u8; 16],
  pub fencing_token: u64,
  pub acquired_at_ms: i64,
  pub renewed_at_ms: i64,
  pub expires_at_ms: i64,
  pub source_header_sequence: u64,
  pub state: MigrationLeaseStateV1,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationLeaseControlV1 {
  pub sequence: u64,
  pub body: MigrationLeaseBodyV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum MigrationPhaseV1 {
  Preflight = 1,
  Copy = 2,
  Reconcile = 3,
  FinalFreeze = 4,
  DestinationVerify = 5,
  Cutover = 6,
  ReadOnlyValidation = 7,
  OperatorAcceptance = 8,
}

impl MigrationPhaseV1 {
  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::Preflight),
      2 => Ok(Self::Copy),
      3 => Ok(Self::Reconcile),
      4 => Ok(Self::FinalFreeze),
      5 => Ok(Self::DestinationVerify),
      6 => Ok(Self::Cutover),
      7 => Ok(Self::ReadOnlyValidation),
      8 => Ok(Self::OperatorAcceptance),
      _ => Err(kind_error("migration_progress_phase", "migration phase is outside the frozen enum")),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum MigrationProgressStateV1 {
  Pending = 1,
  Running = 2,
  Paused = 3,
  Complete = 4,
  Failed = 5,
  Canceled = 6,
}

impl MigrationProgressStateV1 {
  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::Pending),
      2 => Ok(Self::Running),
      3 => Ok(Self::Paused),
      4 => Ok(Self::Complete),
      5 => Ok(Self::Failed),
      6 => Ok(Self::Canceled),
      _ => Err(kind_error("migration_progress_state", "migration progress state is outside the frozen enum")),
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationProgressBodyV1 {
  pub database_id: [u8; 16],
  pub migration_id: [u8; 16],
  pub source_physical_instance_id: [u8; 16],
  pub destination_physical_instance_id: [u8; 16],
  pub fencing_token: u64,
  pub phase: MigrationPhaseV1,
  pub state: MigrationProgressStateV1,
  pub flags: u32,
  pub source_header_sequence: u64,
  pub destination_header_sequence: u64,
  pub copied_through_write_sequence: u64,
  pub captured_through_publication_sequence: u64,
  pub reconciled_through_publication_sequence: u64,
  pub namespace_count: u64,
  pub entity_count: u64,
  pub copied_bytes: u64,
  pub updated_at_ms: i64,
  pub source_capture_head: Vec<u8>,
  pub checkpoint_artifact: Vec<u8>,
  pub legacy_root_map_control_payload_hash: Vec<u8>,
  pub effective_config_fingerprint: Vec<u8>,
  pub system_family_registry_fingerprint: Vec<u8>,
  pub last_error_evidence: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationProgressControlV1 {
  pub sequence: u64,
  pub body: MigrationProgressBodyV1,
}

pub fn encode_migration_lease_control(sequence: u64, request: &MigrationLeaseBodyV1, algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  validate_lease(request)?;
  let mut body = allocate_body(MIGRATION_LEASE_BODY_LENGTH, "migration_lease_allocation")?;
  body.extend_from_slice(&request.database_id);
  body.extend_from_slice(&request.migration_id);
  body.extend_from_slice(&request.source_physical_instance_id);
  body.extend_from_slice(&request.destination_physical_instance_id);
  body.extend_from_slice(&request.holder_boot_id);
  body.extend_from_slice(&request.fencing_token.to_le_bytes());
  body.extend_from_slice(&request.acquired_at_ms.to_le_bytes());
  body.extend_from_slice(&request.renewed_at_ms.to_le_bytes());
  body.extend_from_slice(&request.expires_at_ms.to_le_bytes());
  body.extend_from_slice(&request.source_header_sequence.to_le_bytes());
  body.extend_from_slice(&(request.state as u16).to_le_bytes());
  body.extend_from_slice(&3u16.to_le_bytes());
  body.extend_from_slice(&4u16.to_le_bytes());
  body.extend_from_slice(&0u16.to_le_bytes());
  body.extend_from_slice(&0u32.to_le_bytes());
  if body.len() != MIGRATION_LEASE_BODY_LENGTH {
    return Err(length_error("migration lease writer produced an unexpected body length"));
  }
  let encoded = encode_system_control(SystemControlKindV1::MigrationLease, sequence, &body, algorithm)?;
  let decoded = decode_migration_lease_control(&encoded, algorithm)?;
  if decoded.sequence != sequence || decoded.body != *request {
    return Err(identity_error("migration_lease_encode_roundtrip", "encoded migration lease did not round-trip exactly"));
  }
  Ok(encoded)
}

pub fn decode_migration_lease_control(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<MigrationLeaseControlV1> {
  let control = decode_system_control(bytes, algorithm)?;
  if control.kind != SystemControlKindV1::MigrationLease {
    return Err(kind_error("migration_lease_control_kind", "control is not a migration lease"));
  }
  let mut reader = BoundedReader::new(control.body, MIGRATION_LEASE_BODY_LENGTH)?;
  let body = MigrationLeaseBodyV1 {
    database_id: read_id(&mut reader)?,
    migration_id: read_id(&mut reader)?,
    source_physical_instance_id: read_id(&mut reader)?,
    destination_physical_instance_id: read_id(&mut reader)?,
    holder_boot_id: read_id(&mut reader)?,
    fencing_token: reader.read_u64()?,
    acquired_at_ms: reader.read_i64()?,
    renewed_at_ms: reader.read_i64()?,
    expires_at_ms: reader.read_i64()?,
    source_header_sequence: reader.read_u64()?,
    state: MigrationLeaseStateV1::from_u16(reader.read_u16()?)?,
  };
  let source_format = reader.read_u16()?;
  let destination_format = reader.read_u16()?;
  let flags = reader.read_u16()?;
  let reserved = reader.read_u32()?;
  reader.finish()?;
  if source_format != 3 || destination_format != 4 {
    return Err(kind_error("migration_lease_formats", "migration source or destination format is invalid"));
  }
  if flags != 0 || reserved != 0 {
    return Err(reserved_error("migration_lease_reserved", "migration lease reserve must be zero"));
  }
  validate_lease(&body)?;
  Ok(MigrationLeaseControlV1 { sequence: control.sequence, body })
}

pub fn encode_migration_progress_control(
  sequence: u64,
  request: &MigrationProgressBodyV1,
  algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  validate_progress(request, algorithm)?;
  let body_length = migration_progress_body_length(algorithm)?;
  let mut body = allocate_body(body_length, "migration_progress_allocation")?;
  body.extend_from_slice(&request.database_id);
  body.extend_from_slice(&request.migration_id);
  body.extend_from_slice(&request.source_physical_instance_id);
  body.extend_from_slice(&request.destination_physical_instance_id);
  body.extend_from_slice(&request.fencing_token.to_le_bytes());
  body.extend_from_slice(&3u16.to_le_bytes());
  body.extend_from_slice(&4u16.to_le_bytes());
  body.extend_from_slice(&(request.phase as u16).to_le_bytes());
  body.extend_from_slice(&(request.state as u16).to_le_bytes());
  body.extend_from_slice(&request.flags.to_le_bytes());
  body.extend_from_slice(&request.source_header_sequence.to_le_bytes());
  body.extend_from_slice(&request.destination_header_sequence.to_le_bytes());
  body.extend_from_slice(&request.copied_through_write_sequence.to_le_bytes());
  body.extend_from_slice(&request.captured_through_publication_sequence.to_le_bytes());
  body.extend_from_slice(&request.reconciled_through_publication_sequence.to_le_bytes());
  body.extend_from_slice(&request.namespace_count.to_le_bytes());
  body.extend_from_slice(&request.entity_count.to_le_bytes());
  body.extend_from_slice(&request.copied_bytes.to_le_bytes());
  body.extend_from_slice(&request.updated_at_ms.to_le_bytes());
  body.extend_from_slice(&request.source_capture_head);
  body.extend_from_slice(&request.checkpoint_artifact);
  body.extend_from_slice(&request.legacy_root_map_control_payload_hash);
  body.extend_from_slice(&request.effective_config_fingerprint);
  body.extend_from_slice(&request.system_family_registry_fingerprint);
  body.extend_from_slice(&request.last_error_evidence);
  if body.len() != body_length {
    return Err(length_error("migration progress writer produced an unexpected body length"));
  }
  let encoded = encode_system_control(SystemControlKindV1::MigrationProgress, sequence, &body, algorithm)?;
  let decoded = decode_migration_progress_control(&encoded, algorithm)?;
  if decoded.sequence != sequence || decoded.body != *request {
    return Err(identity_error("migration_progress_encode_roundtrip", "encoded migration progress did not round-trip exactly"));
  }
  Ok(encoded)
}

pub fn decode_migration_progress_control(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<MigrationProgressControlV1> {
  let control = decode_system_control(bytes, algorithm)?;
  if control.kind != SystemControlKindV1::MigrationProgress {
    return Err(kind_error("migration_progress_control_kind", "control is not migration progress"));
  }
  let expected = migration_progress_body_length(algorithm)?;
  let hash_width = algorithm.hash_length();
  let mut reader = BoundedReader::new(control.body, expected)?;
  let database_id = read_id(&mut reader)?;
  let migration_id = read_id(&mut reader)?;
  let source_physical_instance_id = read_id(&mut reader)?;
  let destination_physical_instance_id = read_id(&mut reader)?;
  let fencing_token = reader.read_u64()?;
  let source_format = reader.read_u16()?;
  let destination_format = reader.read_u16()?;
  let phase = MigrationPhaseV1::from_u16(reader.read_u16()?)?;
  let state = MigrationProgressStateV1::from_u16(reader.read_u16()?)?;
  let flags = reader.read_u32()?;
  let source_header_sequence = reader.read_u64()?;
  let destination_header_sequence = reader.read_u64()?;
  let copied_through_write_sequence = reader.read_u64()?;
  let captured_through_publication_sequence = reader.read_u64()?;
  let reconciled_through_publication_sequence = reader.read_u64()?;
  let namespace_count = reader.read_u64()?;
  let entity_count = reader.read_u64()?;
  let copied_bytes = reader.read_u64()?;
  let updated_at_ms = reader.read_i64()?;
  let source_capture_head = reader.read_exact(hash_width)?.to_vec();
  let checkpoint_artifact = reader.read_exact(hash_width)?.to_vec();
  let legacy_root_map_control_payload_hash = reader.read_exact(hash_width)?.to_vec();
  let effective_config_fingerprint = reader.read_exact(hash_width)?.to_vec();
  let system_family_registry_fingerprint = reader.read_exact(hash_width)?.to_vec();
  let last_error_evidence = reader.read_exact(hash_width)?.to_vec();
  reader.finish()?;
  if source_format != 3 || destination_format != 4 {
    return Err(kind_error("migration_progress_formats", "migration source or destination format is invalid"));
  }
  let body = MigrationProgressBodyV1 {
    database_id,
    migration_id,
    source_physical_instance_id,
    destination_physical_instance_id,
    fencing_token,
    phase,
    state,
    flags,
    source_header_sequence,
    destination_header_sequence,
    copied_through_write_sequence,
    captured_through_publication_sequence,
    reconciled_through_publication_sequence,
    namespace_count,
    entity_count,
    copied_bytes,
    updated_at_ms,
    source_capture_head,
    checkpoint_artifact,
    legacy_root_map_control_payload_hash,
    effective_config_fingerprint,
    system_family_registry_fingerprint,
    last_error_evidence,
  };
  validate_progress(&body, algorithm)?;
  Ok(MigrationProgressControlV1 { sequence: control.sequence, body })
}

fn validate_lease(request: &MigrationLeaseBodyV1) -> FormatResult<()> {
  if [
    &request.database_id,
    &request.migration_id,
    &request.source_physical_instance_id,
    &request.destination_physical_instance_id,
    &request.holder_boot_id,
  ]
  .into_iter()
  .any(|value| all_zero(value))
  {
    return Err(identity_error("migration_lease_identity", "migration lease IDs must be nonzero"));
  }
  if request.fencing_token == 0 || request.source_header_sequence == 0 {
    return Err(identity_error("migration_lease_fencing", "migration fencing token and source header sequence must be nonzero"));
  }
  if request.acquired_at_ms < 0 || request.renewed_at_ms < request.acquired_at_ms || request.expires_at_ms <= request.renewed_at_ms {
    return Err(closure_error("migration_lease_times", "migration lease times are invalid"));
  }
  Ok(())
}

fn validate_progress(request: &MigrationProgressBodyV1, algorithm: HashAlgorithm) -> FormatResult<()> {
  if [&request.database_id, &request.migration_id, &request.source_physical_instance_id, &request.destination_physical_instance_id]
    .into_iter()
    .any(|value| all_zero(value))
  {
    return Err(identity_error("migration_progress_identity", "migration progress IDs must be nonzero"));
  }
  if request.fencing_token == 0 {
    return Err(identity_error("migration_progress_fencing", "migration fencing token must be nonzero"));
  }
  if request.flags & !MIGRATION_PROGRESS_FLAGS != 0 {
    return Err(reserved_error("migration_progress_flags", "migration progress contains unknown flag bits"));
  }
  if request.updated_at_ms < 0 {
    return Err(closure_error("migration_progress_time", "migration progress update time must be nonnegative"));
  }
  let hash_width = algorithm.hash_length();
  require_hash(&request.source_capture_head, hash_width, false, "source capture head")?;
  require_hash(&request.checkpoint_artifact, hash_width, false, "checkpoint artifact")?;
  require_hash(&request.legacy_root_map_control_payload_hash, hash_width, false, "legacy root-map control payload hash")?;
  require_hash(&request.effective_config_fingerprint, hash_width, true, "effective configuration fingerprint")?;
  require_hash(&request.system_family_registry_fingerprint, hash_width, true, "SystemFamily registry fingerprint")?;
  require_hash(&request.last_error_evidence, hash_width, false, "last error evidence")?;
  Ok(())
}

fn migration_progress_body_length(algorithm: HashAlgorithm) -> FormatResult<usize> {
  MIGRATION_PROGRESS_HASH_COUNT
    .checked_mul(algorithm.hash_length())
    .and_then(|hashes| MIGRATION_PROGRESS_FIXED_LENGTH.checked_add(hashes))
    .ok_or_else(|| length_error("migration progress body length overflow"))
}

fn allocate_body(length: usize, code: &'static str) -> FormatResult<Vec<u8>> {
  let mut body = Vec::new();
  body
    .try_reserve_exact(length)
    .map_err(|error| FormatError::new(MalformedInputClass::AllocationAmplification, code, error.to_string()))?;
  Ok(body)
}

fn read_id(reader: &mut BoundedReader<'_>) -> FormatResult<[u8; 16]> {
  let bytes = reader.read_exact(16)?;
  let mut value = [0u8; 16];
  value.copy_from_slice(bytes);
  Ok(value)
}

fn require_hash(bytes: &[u8], expected: usize, nonzero: bool, name: &'static str) -> FormatResult<()> {
  if bytes.len() != expected {
    return Err(length_error(format!("{name} has width {}, expected {expected}", bytes.len())));
  }
  if nonzero && all_zero(bytes) {
    return Err(identity_error("migration_progress_required_hash", format!("{name} must be nonzero")));
  }
  Ok(())
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn length_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, "migration_control_length", context)
}

fn identity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, context)
}

fn kind_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::UnknownTypeKindOrEnum, code, context)
}

fn reserved_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::NonzeroReservedOrPadding, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}
