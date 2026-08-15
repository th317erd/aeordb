//! Typed codec for the frozen AIOP v1 index-operation control body.
//!
//! Common control framing, CRC, A/B selection, and body identity validation
//! remain owned by system_control. This module gives runtime and recovery code
//! one offset-free interpretation of the index-operation body.

use crate::engine::HashAlgorithm;

pub use super::control_enums::{RetryClassV1, StableReasonV1};
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::system_control::{SystemControlKindV1, decode_system_control, encode_system_control};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum IndexOperationKindV1 {
  Build = 1,
  Rebuild = 2,
  Reconcile = 3,
  Compact = 4,
}

impl IndexOperationKindV1 {
  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::Build),
      2 => Ok(Self::Rebuild),
      3 => Ok(Self::Reconcile),
      4 => Ok(Self::Compact),
      _ => Err(kind_error("index_operation_kind", "index operation kind is outside the frozen enum")),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum IndexOperationStateV1 {
  Queued = 1,
  Running = 2,
  Checkpointed = 3,
  Publishing = 4,
  Complete = 5,
  Canceled = 6,
  Failed = 7,
}

impl IndexOperationStateV1 {
  fn from_u16(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::Queued),
      2 => Ok(Self::Running),
      3 => Ok(Self::Checkpointed),
      4 => Ok(Self::Publishing),
      5 => Ok(Self::Complete),
      6 => Ok(Self::Canceled),
      7 => Ok(Self::Failed),
      _ => Err(kind_error("index_operation_state", "index operation state is outside the frozen enum")),
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexOperationControlWriteV1<'a> {
  pub database_id: [u8; 16],
  pub index_id: &'a [u8],
  pub operation_id: [u8; 16],
  pub operation_kind: IndexOperationKindV1,
  pub state: IndexOperationStateV1,
  pub created_at_ms: i64,
  pub updated_at_ms: i64,
  pub requested_namespace_root: &'a [u8],
  pub definition_id: &'a [u8],
  pub base_manifest: Option<&'a [u8]>,
  pub target_manifest: Option<&'a [u8]>,
  pub checkpoint_artifact: Option<&'a [u8]>,
  pub captured_runtime_sequence: u64,
  pub reconciled_through_sequence: u64,
  pub completed_work: u64,
  pub total_work_hint: u64,
  pub stable_reason: StableReasonV1,
  pub retry_class: RetryClassV1,
  pub error_evidence_hash: Option<&'a [u8]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexOperationControlV1<'a> {
  pub control_sequence: u64,
  pub database_id: [u8; 16],
  pub index_id: &'a [u8],
  pub operation_id: [u8; 16],
  pub operation_kind: IndexOperationKindV1,
  pub state: IndexOperationStateV1,
  pub created_at_ms: i64,
  pub updated_at_ms: i64,
  pub requested_namespace_root: &'a [u8],
  pub definition_id: &'a [u8],
  pub base_manifest: Option<&'a [u8]>,
  pub target_manifest: Option<&'a [u8]>,
  pub checkpoint_artifact: Option<&'a [u8]>,
  pub captured_runtime_sequence: u64,
  pub reconciled_through_sequence: u64,
  pub completed_work: u64,
  pub total_work_hint: u64,
  pub stable_reason: StableReasonV1,
  pub retry_class: RetryClassV1,
  pub error_evidence_hash: Option<&'a [u8]>,
}

impl<'a> IndexOperationControlV1<'a> {
  pub fn from_write(control_sequence: u64, request: &IndexOperationControlWriteV1<'a>) -> Self {
    Self {
      control_sequence,
      database_id: request.database_id,
      index_id: request.index_id,
      operation_id: request.operation_id,
      operation_kind: request.operation_kind,
      state: request.state,
      created_at_ms: request.created_at_ms,
      updated_at_ms: request.updated_at_ms,
      requested_namespace_root: request.requested_namespace_root,
      definition_id: request.definition_id,
      base_manifest: request.base_manifest,
      target_manifest: request.target_manifest,
      checkpoint_artifact: request.checkpoint_artifact,
      captured_runtime_sequence: request.captured_runtime_sequence,
      reconciled_through_sequence: request.reconciled_through_sequence,
      completed_work: request.completed_work,
      total_work_hint: request.total_work_hint,
      stable_reason: request.stable_reason,
      retry_class: request.retry_class,
      error_evidence_hash: request.error_evidence_hash,
    }
  }

  pub fn as_write(&self) -> IndexOperationControlWriteV1<'_> {
    IndexOperationControlWriteV1 {
      database_id: self.database_id,
      index_id: self.index_id,
      operation_id: self.operation_id,
      operation_kind: self.operation_kind,
      state: self.state,
      created_at_ms: self.created_at_ms,
      updated_at_ms: self.updated_at_ms,
      requested_namespace_root: self.requested_namespace_root,
      definition_id: self.definition_id,
      base_manifest: self.base_manifest,
      target_manifest: self.target_manifest,
      checkpoint_artifact: self.checkpoint_artifact,
      captured_runtime_sequence: self.captured_runtime_sequence,
      reconciled_through_sequence: self.reconciled_through_sequence,
      completed_work: self.completed_work,
      total_work_hint: self.total_work_hint,
      stable_reason: self.stable_reason,
      retry_class: self.retry_class,
      error_evidence_hash: self.error_evidence_hash,
    }
  }
}

pub fn encode_index_operation_control(
  control_sequence: u64,
  request: &IndexOperationControlWriteV1<'_>,
  algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  validate_write(request, algorithm)?;
  let hash_width = algorithm.hash_length();
  let body_length = 88usize
    .checked_add(7usize.checked_mul(hash_width).ok_or_else(|| length_error("index operation hash bytes overflow"))?)
    .ok_or_else(|| length_error("index operation body length overflow"))?;
  let mut body = Vec::new();
  body
    .try_reserve_exact(body_length)
    .map_err(|error| FormatError::new(MalformedInputClass::AllocationAmplification, "index_operation_allocation", error.to_string()))?;
  body.extend_from_slice(&request.database_id);
  body.extend_from_slice(request.index_id);
  body.extend_from_slice(&request.operation_id);
  body.extend_from_slice(&(request.operation_kind as u16).to_le_bytes());
  body.extend_from_slice(&(request.state as u16).to_le_bytes());
  body.extend_from_slice(&request.created_at_ms.to_le_bytes());
  body.extend_from_slice(&request.updated_at_ms.to_le_bytes());
  body.extend_from_slice(request.requested_namespace_root);
  body.extend_from_slice(request.definition_id);
  append_optional_hash(&mut body, request.base_manifest, hash_width);
  append_optional_hash(&mut body, request.target_manifest, hash_width);
  append_optional_hash(&mut body, request.checkpoint_artifact, hash_width);
  body.extend_from_slice(&request.captured_runtime_sequence.to_le_bytes());
  body.extend_from_slice(&request.reconciled_through_sequence.to_le_bytes());
  body.extend_from_slice(&request.completed_work.to_le_bytes());
  body.extend_from_slice(&request.total_work_hint.to_le_bytes());
  body.extend_from_slice(&(request.stable_reason as u16).to_le_bytes());
  body.extend_from_slice(&(request.retry_class as u16).to_le_bytes());
  append_optional_hash(&mut body, request.error_evidence_hash, hash_width);
  if body.len() != body_length {
    return Err(length_error("index operation writer produced an unexpected body length"));
  }
  let encoded = encode_system_control(SystemControlKindV1::IndexOperation, control_sequence, &body, algorithm)?;
  let decoded = decode_index_operation_control(&encoded, algorithm)?;
  if decoded != IndexOperationControlV1::from_write(control_sequence, request) {
    return Err(identity_error("index_operation_encode_roundtrip", "encoded index operation did not round-trip exactly"));
  }
  Ok(encoded)
}

pub fn decode_index_operation_control(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<IndexOperationControlV1<'_>> {
  let control = decode_system_control(bytes, algorithm)?;
  if control.kind != SystemControlKindV1::IndexOperation {
    return Err(kind_error("index_operation_control_kind", "control is not an index operation"));
  }
  let body = control.body;
  let hash_width = algorithm.hash_length();
  let expected = 88usize
    .checked_add(7usize.checked_mul(hash_width).ok_or_else(|| length_error("index operation hash bytes overflow"))?)
    .ok_or_else(|| length_error("index operation body length overflow"))?;
  if body.len() != expected {
    return Err(length_error("index operation body has the wrong fixed length"));
  }
  let database_id = array_16(&body[..16], "index operation database ID")?;
  let index_start = 16;
  let operation_start = index_start + hash_width;
  let operation_id = array_16(&body[operation_start..operation_start + 16], "index operation ID")?;
  let enum_start = operation_start + 16;
  let created_at_ms = read_i64(body, enum_start + 4)?;
  let updated_at_ms = read_i64(body, enum_start + 12)?;
  let hashes_start = enum_start + 20;
  let requested_namespace_root = &body[hashes_start..hashes_start + hash_width];
  let definition_id = &body[hashes_start + hash_width..hashes_start + 2 * hash_width];
  let base_manifest = optional_hash(&body[hashes_start + 2 * hash_width..hashes_start + 3 * hash_width]);
  let target_manifest = optional_hash(&body[hashes_start + 3 * hash_width..hashes_start + 4 * hash_width]);
  let checkpoint_artifact = optional_hash(&body[hashes_start + 4 * hash_width..hashes_start + 5 * hash_width]);
  let counters_start = hashes_start + 5 * hash_width;
  let error_evidence_hash = optional_hash(&body[counters_start + 36..counters_start + 36 + hash_width]);
  Ok(IndexOperationControlV1 {
    control_sequence: control.sequence,
    database_id,
    index_id: &body[index_start..index_start + hash_width],
    operation_id,
    operation_kind: IndexOperationKindV1::from_u16(read_u16(body, enum_start)?)?,
    state: IndexOperationStateV1::from_u16(read_u16(body, enum_start + 2)?)?,
    created_at_ms,
    updated_at_ms,
    requested_namespace_root,
    definition_id,
    base_manifest,
    target_manifest,
    checkpoint_artifact,
    captured_runtime_sequence: read_u64(body, counters_start)?,
    reconciled_through_sequence: read_u64(body, counters_start + 8)?,
    completed_work: read_u64(body, counters_start + 16)?,
    total_work_hint: read_u64(body, counters_start + 24)?,
    stable_reason: StableReasonV1::from_u16(read_u16(body, counters_start + 32)?)
      .ok_or_else(|| kind_error("index_operation_reason", "stable reason is outside the frozen enum"))?,
    retry_class: RetryClassV1::from_u16(read_u16(body, counters_start + 34)?)
      .ok_or_else(|| kind_error("index_operation_retry_class", "retry class is outside the frozen enum"))?,
    error_evidence_hash,
  })
}

fn validate_write(request: &IndexOperationControlWriteV1<'_>, algorithm: HashAlgorithm) -> FormatResult<()> {
  let hash_width = algorithm.hash_length();
  require_hash(request.index_id, hash_width, true, "index ID")?;
  require_hash(request.requested_namespace_root, hash_width, true, "requested namespace root")?;
  require_hash(request.definition_id, hash_width, true, "definition ID")?;
  require_optional_hash(request.base_manifest, hash_width, "base manifest")?;
  require_optional_hash(request.target_manifest, hash_width, "target manifest")?;
  require_optional_hash(request.checkpoint_artifact, hash_width, "checkpoint artifact")?;
  require_optional_hash(request.error_evidence_hash, hash_width, "error evidence hash")?;
  if request.database_id.iter().all(|byte| *byte == 0) || request.operation_id.iter().all(|byte| *byte == 0) {
    return Err(identity_error("index_operation_identity", "database and operation IDs must be nonzero"));
  }
  if request.created_at_ms < 0 || request.updated_at_ms < request.created_at_ms {
    return Err(closure_error("index_operation_times", "index operation times are invalid"));
  }
  if request.captured_runtime_sequence > request.reconciled_through_sequence || request.completed_work > request.total_work_hint {
    return Err(closure_error("index_operation_counters", "operation watermarks or counters are inverted"));
  }
  Ok(())
}

fn require_hash(bytes: &[u8], width: usize, nonzero: bool, name: &'static str) -> FormatResult<()> {
  if bytes.len() != width {
    return Err(length_error(format!("{name} has width {}, expected {width}", bytes.len())));
  }
  if nonzero && bytes.iter().all(|byte| *byte == 0) {
    return Err(identity_error("index_operation_required_hash", format!("{name} must be nonzero")));
  }
  Ok(())
}

fn require_optional_hash(value: Option<&[u8]>, width: usize, name: &'static str) -> FormatResult<()> {
  if let Some(bytes) = value {
    require_hash(bytes, width, true, name)?;
  }
  Ok(())
}

fn append_optional_hash(body: &mut Vec<u8>, value: Option<&[u8]>, width: usize) {
  match value {
    Some(bytes) => body.extend_from_slice(bytes),
    None => body.resize(body.len() + width, 0),
  }
}

fn optional_hash(bytes: &[u8]) -> Option<&[u8]> {
  (!bytes.iter().all(|byte| *byte == 0)).then_some(bytes)
}

fn array_16(bytes: &[u8], name: &'static str) -> FormatResult<[u8; 16]> {
  bytes.try_into().map_err(|_| length_error(format!("{name} does not contain 16 bytes")))
}

fn read_u16(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  let end = offset.checked_add(2).ok_or_else(|| length_error("u16 offset overflow"))?;
  let slice = bytes.get(offset..end).ok_or_else(|| length_error("truncated u16"))?;
  Ok(u16::from_le_bytes(slice.try_into().map_err(|_| length_error("invalid u16 width"))?))
}

fn read_u64(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  let end = offset.checked_add(8).ok_or_else(|| length_error("u64 offset overflow"))?;
  let slice = bytes.get(offset..end).ok_or_else(|| length_error("truncated u64"))?;
  Ok(u64::from_le_bytes(slice.try_into().map_err(|_| length_error("invalid u64 width"))?))
}

fn read_i64(bytes: &[u8], offset: usize) -> FormatResult<i64> {
  let end = offset.checked_add(8).ok_or_else(|| length_error("i64 offset overflow"))?;
  let slice = bytes.get(offset..end).ok_or_else(|| length_error("truncated i64"))?;
  Ok(i64::from_le_bytes(slice.try_into().map_err(|_| length_error("invalid i64 width"))?))
}

fn length_error(context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::LengthCountOrArithmeticOverflow, "index_operation_length", context)
}

fn identity_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::IdentityKeyOrGenerationMismatch, code, context)
}

fn kind_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::UnknownTypeKindOrEnum, code, context)
}

fn closure_error(code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(MalformedInputClass::CrossRecordClosureMismatch, code, context)
}
