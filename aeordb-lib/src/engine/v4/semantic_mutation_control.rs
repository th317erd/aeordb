//! Round 17 borrowed readers and byte encoders, without publication or GC authority.
#[path = "semantic_mutation_writer.rs"]
mod writer;
pub use writer::{encode_semantic_mutation_checkpoint, encode_semantic_mutation_generation, encode_semantic_mutation_task};

#[path = "semantic_mutation_sources.rs"]
mod sources;
pub use sources::{
  SemanticMutationSourceFingerprintRequestV1, SemanticMutationSourceIdentityV1, SemanticMutationSourceFingerprintV1,
  fingerprint_semantic_mutation_sources_v1,
};

use crate::engine::HashAlgorithm;

use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::scope::validate_canonical_absolute_path;
use super::system_control::{SystemControlKindV1, decode_system_control};

const CHECKPOINT_FIXED_BYTES: usize = 168;
const CHECKPOINT_HASH_COUNT: usize = 9;
const CURSOR_CAP: usize = 65_535;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum SemanticMutationTaskStateV1 {
  Queued = 1,
  Capturing = 2,
  Compiling = 3,
  ReadyToActivate = 4,
  Activating = 5,
  Completed = 6,
  Failed = 7,
  Cancelled = 8,
  Superseded = 9,
}

impl SemanticMutationTaskStateV1 {
  fn from_id(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::Queued),
      2 => Ok(Self::Capturing),
      3 => Ok(Self::Compiling),
      4 => Ok(Self::ReadyToActivate),
      5 => Ok(Self::Activating),
      6 => Ok(Self::Completed),
      7 => Ok(Self::Failed),
      8 => Ok(Self::Cancelled),
      9 => Ok(Self::Superseded),
      _ => Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "semantic_task_state", "unknown semantic task state")),
    }
  }

  pub const fn is_terminal(self) -> bool {
    matches!(self, Self::Completed | Self::Failed | Self::Cancelled | Self::Superseded)
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum SemanticMutationPhaseV1 {
  Captured = 1,
  Compiling = 2,
  Pruning = 3,
  Ready = 4,
  Activated = 5,
}

impl SemanticMutationPhaseV1 {
  fn from_id(value: u16) -> FormatResult<Self> {
    match value {
      1 => Ok(Self::Captured),
      2 => Ok(Self::Compiling),
      3 => Ok(Self::Pruning),
      4 => Ok(Self::Ready),
      5 => Ok(Self::Activated),
      _ => Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "semantic_task_phase", "unknown semantic checkpoint phase")),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticMutationCursorV1<'a> {
  None,
  ConfigurationOwner(&'a str),
  DependencyID(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticMutationTaskV1<'a> {
  pub control_sequence: u64,
  pub database_id: &'a [u8],
  pub task_id: &'a [u8],
  pub physical_instance_id: &'a [u8],
  pub holder_boot_id: &'a [u8],
  pub fencing_token: u64,
  pub writer_fence_epoch: u64,
  pub created_at_ms: i64,
  pub updated_at_ms: i64,
  pub state: SemanticMutationTaskStateV1,
  pub pins_released: bool,
  pub checkpoint_sequence: u64,
  pub checkpoint_payload_hash: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticMutationCheckpointV1<'a> {
  pub database_id: &'a [u8],
  pub task_id: &'a [u8],
  pub checkpoint_sequence: u64,
  pub physical_instance_id: &'a [u8],
  pub writer_fence_epoch: u64,
  pub semantic_generation: u64,
  pub header_sequence: u64,
  pub captured_at_ms: i64,
  pub phase: SemanticMutationPhaseV1,
  pub cursor: SemanticMutationCursorV1<'a>,
  pub expected_configuration_count: u64,
  pub configuration_count: u64,
  pub record_count: u64,
  pub node_count: u64,
  pub dependency_count: u64,
  pub mutation_count: u64,
  pub activation_generation: u64,
  pub pruning_record_count: u64,
  pub pruning_node_count: u64,
  pub base_namespace_root: &'a [u8],
  pub staged_directory_root: &'a [u8],
  pub catalog_root: Option<&'a [u8]>,
  pub pruning_catalog_root: Option<&'a [u8]>,
  pub semantic_state: Option<&'a [u8]>,
  pub candidate_namespace_root: Option<&'a [u8]>,
  pub compiler_fingerprint: &'a [u8],
  pub semantic_registry_fingerprint: &'a [u8],
  pub source_identity_fingerprint: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticMutationReferenceRoleV1 {
  AdmittedBaseNamespaceRoot,
  StagedDirectoryTree,
  SemanticCatalog,
  PruningCandidateCatalog,
  CompiledSemanticState,
  StagedCandidateNamespaceRoot,
}

impl<'a> SemanticMutationCheckpointV1<'a> {
  /// Declared typed edges only. The retention owner must validate each actual
  /// closure; the candidate role never manufactures namespace admission.
  pub fn references(&self) -> impl Iterator<Item = (SemanticMutationReferenceRoleV1, &'a [u8])> {
    use SemanticMutationReferenceRoleV1 as Role;
    [
      Some((Role::AdmittedBaseNamespaceRoot, self.base_namespace_root)),
      Some((Role::StagedDirectoryTree, self.staged_directory_root)),
      self.catalog_root.map(|hash| (Role::SemanticCatalog, hash)),
      self.pruning_catalog_root.map(|hash| (Role::PruningCandidateCatalog, hash)),
      self.semantic_state.map(|hash| (Role::CompiledSemanticState, hash)),
      self.candidate_namespace_root.map(|hash| (Role::StagedCandidateNamespaceRoot, hash)),
    ]
    .into_iter()
    .flatten()
  }
}

/// Check the selected payload binding, not current ownership or resume rights.
/// Native resume must additionally check current physical/writer/task fences,
/// captured sources, exact semantic generation and complete retained closure.
pub fn decode_semantic_mutation_selection<'a>(
  task_bytes: &'a [u8],
  checkpoint_bytes: &'a [u8],
  algorithm: HashAlgorithm,
) -> FormatResult<(SemanticMutationTaskV1<'a>, SemanticMutationCheckpointV1<'a>)> {
  let task = decode_semantic_mutation_task(task_bytes, algorithm)?;
  let checkpoint = decode_semantic_mutation_checkpoint(checkpoint_bytes, algorithm)?;
  let digest = super::hash::try_digest_parts(algorithm, &[checkpoint_bytes]).map_err(|source| {
    FormatError::allocation_failure("semantic_task_digest_allocation", format!("cannot bind checkpoint digest: {source}"))
  })?;
  if task.checkpoint_payload_hash != digest {
    return Err(error(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "semantic_task_checkpoint_digest",
      "selected checkpoint payload digest differs",
    ));
  }
  if task.database_id != checkpoint.database_id
    || task.task_id != checkpoint.task_id
    || task.checkpoint_sequence != checkpoint.checkpoint_sequence
    || task.physical_instance_id != checkpoint.physical_instance_id
    || task.writer_fence_epoch < checkpoint.writer_fence_epoch
  {
    return Err(closure_error("semantic_task_checkpoint_identity", "selected checkpoint identity or captured fence differs"));
  }
  if checkpoint.captured_at_ms < task.created_at_ms || checkpoint.captured_at_ms > task.updated_at_ms {
    return Err(closure_error("semantic_task_checkpoint_time", "checkpoint capture time is outside the task lifetime"));
  }
  use SemanticMutationPhaseV1 as Phase;
  use SemanticMutationTaskStateV1 as State;
  let phase_matches = match task.state {
    State::Queued | State::Capturing => checkpoint.phase == Phase::Captured,
    State::Compiling => matches!(checkpoint.phase, Phase::Compiling | Phase::Pruning),
    State::ReadyToActivate | State::Activating => checkpoint.phase == Phase::Ready,
    State::Completed => checkpoint.phase == Phase::Activated,
    State::Failed | State::Cancelled | State::Superseded => checkpoint.phase != Phase::Activated,
  };
  if !phase_matches {
    return Err(closure_error("semantic_task_checkpoint_phase", "selected checkpoint phase differs from the task state"));
  }
  Ok((task, checkpoint))
}

pub fn decode_semantic_mutation_task(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<SemanticMutationTaskV1<'_>> {
  let control = decode_system_control(bytes, algorithm)?;
  require_kind(control.kind, SystemControlKindV1::SemanticMutationTask)?;
  let mut task = decode_task_body(control.body, algorithm)?;
  task.control_sequence = control.sequence;
  Ok(task)
}

pub fn decode_semantic_mutation_checkpoint(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<SemanticMutationCheckpointV1<'_>> {
  let control = decode_system_control(bytes, algorithm)?;
  require_kind(control.kind, SystemControlKindV1::SemanticMutationCheckpoint)?;
  decode_checkpoint_body(control.body, algorithm)
}

pub(crate) fn validate_body(kind: SystemControlKindV1, body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  let identity = match kind {
    SystemControlKindV1::SemanticMutationTask => {
      decode_task_body(body, algorithm)?;
      &body[16..32]
    }
    SystemControlKindV1::SemanticMutationCheckpoint => {
      decode_checkpoint_body(body, algorithm)?;
      &body[16..40]
    }
    SystemControlKindV1::SemanticMutationGeneration => {
      if body.len() != 16 {
        return Err(length_error());
      }
      nonzero(body)?;
      &[]
    }
    _ => return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "semantic_task_kind", "not a semantic mutation control")),
  };
  let mut owned = Vec::new();
  owned.try_reserve_exact(identity.len()).map_err(|source| {
    FormatError::allocation_failure("semantic_task_identity_allocation", format!("cannot retain bounded control identity: {source}"))
  })?;
  owned.extend_from_slice(identity);
  Ok(owned)
}

fn decode_task_body(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<SemanticMutationTaskV1<'_>> {
  if body.len() != 112 + algorithm.hash_length() {
    return Err(length_error());
  }
  for identity in body[..64].chunks_exact(16) {
    nonzero(identity)?;
  }
  nonzero(&body[112..])?;
  let state = SemanticMutationTaskStateV1::from_id(u16_at(body, 96)?)?;
  let flags = u16_at(body, 98)?;
  if flags & !1 != 0 || u32_at(body, 108)? != 0 {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "semantic_task_reserved", "unknown flags or nonzero reserve"));
  }
  let fencing_token = u64_at(body, 64)?;
  let writer_fence_epoch = u64_at(body, 72)?;
  let checkpoint_sequence = u64_at(body, 100)?;
  if fencing_token == 0 || writer_fence_epoch == 0 || checkpoint_sequence == 0 {
    return Err(identity_error());
  }
  let created_at_ms = u64_at(body, 80)?;
  let updated_at_ms = u64_at(body, 88)?;
  if created_at_ms > updated_at_ms || updated_at_ms > i64::MAX as u64 || (flags != 0 && !state.is_terminal()) {
    return Err(closure_error("semantic_task_state_closure", "invalid timestamps or active task released its pins"));
  }
  Ok(SemanticMutationTaskV1 {
    control_sequence: 0,
    database_id: &body[..16],
    task_id: &body[16..32],
    physical_instance_id: &body[32..48],
    holder_boot_id: &body[48..64],
    fencing_token,
    writer_fence_epoch,
    created_at_ms: created_at_ms as i64,
    updated_at_ms: updated_at_ms as i64,
    state,
    pins_released: flags != 0,
    checkpoint_sequence,
    checkpoint_payload_hash: &body[112..],
  })
}

fn decode_checkpoint_body(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<SemanticMutationCheckpointV1<'_>> {
  let width = algorithm.hash_length();
  let fixed = CHECKPOINT_FIXED_BYTES + CHECKPOINT_HASH_COUNT * width;
  if body.len() < fixed {
    return Err(length_error());
  }
  let cursor_length = u32_at(body, 92)? as usize;
  if cursor_length > CURSOR_CAP {
    return Err(error(MalformedInputClass::AllocationAmplification, "semantic_task_cursor_cap", "checkpoint cursor exceeds 65,535 bytes"));
  }
  if body.len() != fixed + cursor_length {
    return Err(length_error());
  }
  for identity in [&body[..16], &body[16..32], &body[40..56]] {
    nonzero(identity)?;
  }
  for offset in [32, 56, 64, 72, 136] {
    if u64_at(body, offset)? == 0 {
      return Err(identity_error());
    }
  }
  let captured_at_ms = u64_at(body, 80)?;
  if captured_at_ms > i64::MAX as u64 {
    return Err(closure_error("semantic_task_timestamp", "checkpoint timestamp is negative"));
  }
  let phase = SemanticMutationPhaseV1::from_id(u16_at(body, 88)?)?;
  let hash = |slot| &body[CHECKPOINT_FIXED_BYTES + slot * width..CHECKPOINT_FIXED_BYTES + (slot + 1) * width];
  for slot in [0, 1, 6, 7, 8] {
    nonzero(hash(slot))?;
  }
  let catalog_root = optional_hash(hash(2));
  let pruning_catalog_root = optional_hash(hash(3));
  let semantic_state = optional_hash(hash(4));
  let candidate_namespace_root = optional_hash(hash(5));
  let configuration_count = u64_at(body, 104)?;
  let record_count = u64_at(body, 112)?;
  let node_count = u64_at(body, 120)?;
  let dependency_count = u64_at(body, 128)?;
  let pruning_record_count = u64_at(body, 152)?;
  let pruning_node_count = u64_at(body, 160)?;
  validate_tree_counts(catalog_root.is_some(), record_count, node_count)?;
  validate_tree_counts(pruning_catalog_root.is_some(), pruning_record_count, pruning_node_count)?;
  if configuration_count > record_count || dependency_count > record_count {
    return Err(closure_error("semantic_task_counts", "catalog member counts exceed total records"));
  }
  let activation_generation = u64_at(body, 144)?;
  let semantic_generation = u64_at(body, 64)?;
  if match phase {
    SemanticMutationPhaseV1::Activated => semantic_generation.checked_add(1) != Some(activation_generation),
    _ => activation_generation != 0,
  } {
    return Err(closure_error("semantic_task_activation_generation", "activation generation does not close against capture"));
  }
  let ready = matches!(phase, SemanticMutationPhaseV1::Ready | SemanticMutationPhaseV1::Activated);
  if ready {
    if catalog_root.is_none()
      || pruning_catalog_root.is_some()
      || semantic_state.is_none()
      || candidate_namespace_root.is_none()
      || configuration_count != u64_at(body, 96)?
      || cursor_length != 0
    {
      return Err(closure_error("semantic_task_ready_closure", "ready checkpoint lacks complete output or retains unfinished work"));
    }
  } else if semantic_state.is_some() || candidate_namespace_root.is_some() {
    return Err(closure_error("semantic_task_partial_output", "unfinished checkpoint cannot claim a completed state or root"));
  }
  if phase == SemanticMutationPhaseV1::Captured && (catalog_root.is_some() || pruning_catalog_root.is_some() || cursor_length != 0) {
    return Err(closure_error("semantic_task_capture_closure", "captured checkpoint cannot contain compiler progress"));
  }
  let cursor_bytes = &body[fixed..];
  let cursor = match u16_at(body, 90)? {
    0 if cursor_bytes.is_empty() => SemanticMutationCursorV1::None,
    1 if phase == SemanticMutationPhaseV1::Compiling && !cursor_bytes.is_empty() => {
      let path = std::str::from_utf8(cursor_bytes).map_err(|source| {
        FormatError::new(
          MalformedInputClass::InvalidUtf8PathGlobOrNativePath,
          "semantic_task_cursor_path",
          format!("cursor owner is not UTF-8: {source}"),
        )
      })?;
      validate_canonical_absolute_path(path)?;
      SemanticMutationCursorV1::ConfigurationOwner(path)
    }
    2 if phase == SemanticMutationPhaseV1::Pruning && cursor_bytes.len() == width && optional_hash(cursor_bytes).is_some() => {
      SemanticMutationCursorV1::DependencyID(cursor_bytes)
    }
    _ => return Err(closure_error("semantic_task_cursor", "cursor kind, phase, length or presence disagrees")),
  };
  Ok(SemanticMutationCheckpointV1 {
    database_id: &body[..16],
    task_id: &body[16..32],
    checkpoint_sequence: u64_at(body, 32)?,
    physical_instance_id: &body[40..56],
    writer_fence_epoch: u64_at(body, 56)?,
    semantic_generation,
    header_sequence: u64_at(body, 72)?,
    captured_at_ms: captured_at_ms as i64,
    phase,
    cursor,
    expected_configuration_count: u64_at(body, 96)?,
    configuration_count,
    record_count,
    node_count,
    dependency_count,
    mutation_count: u64_at(body, 136)?,
    activation_generation,
    pruning_record_count,
    pruning_node_count,
    base_namespace_root: hash(0),
    staged_directory_root: hash(1),
    catalog_root,
    pruning_catalog_root,
    semantic_state,
    candidate_namespace_root,
    compiler_fingerprint: hash(6),
    semantic_registry_fingerprint: hash(7),
    source_identity_fingerprint: hash(8),
  })
}

fn validate_tree_counts(present: bool, records: u64, nodes: u64) -> FormatResult<()> {
  let valid = if present {
    records.checked_mul(2).and_then(|value| value.checked_sub(1)).is_some_and(|maximum| nodes != 0 && nodes <= maximum)
  } else {
    records == 0 && nodes == 0
  };
  if !valid {
    return Err(closure_error("semantic_task_counts", "catalog root presence and checked counts disagree"));
  }
  Ok(())
}

fn require_kind(actual: SystemControlKindV1, expected: SystemControlKindV1) -> FormatResult<()> {
  if actual != expected {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "semantic_task_kind", "wrong semantic mutation control kind"));
  }
  Ok(())
}

fn optional_hash(bytes: &[u8]) -> Option<&[u8]> {
  bytes.iter().any(|byte| *byte != 0).then_some(bytes)
}

fn nonzero(bytes: &[u8]) -> FormatResult<()> {
  if optional_hash(bytes).is_none() {
    return Err(identity_error());
  }
  Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> FormatResult<u16> {
  super::reader::fixed_array_at(bytes, offset).map(u16::from_le_bytes).ok_or_else(length_error)
}

fn u32_at(bytes: &[u8], offset: usize) -> FormatResult<u32> {
  super::reader::fixed_array_at(bytes, offset).map(u32::from_le_bytes).ok_or_else(length_error)
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  super::reader::fixed_array_at(bytes, offset).map(u64::from_le_bytes).ok_or_else(length_error)
}

fn error(class: MalformedInputClass, code: &'static str, message: &'static str) -> FormatError {
  FormatError::new(class, code, message)
}

fn length_error() -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, "semantic_task_length", "semantic mutation body length does not close")
}

fn identity_error() -> FormatError {
  error(MalformedInputClass::IdentityKeyOrGenerationMismatch, "semantic_task_identity", "required semantic mutation identity is zero")
}

fn closure_error(code: &'static str, message: &'static str) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, code, message)
}
