//! Structural capture records only; these views are not resume or retention permits.
#[path = "semantic_source_catalog_build.rs"]
mod build;
pub use build::{
  build_semantic_source_catalog_pair_v1, SemanticSourceCatalogBuildRequestV1, SemanticSourceCatalogPairRowV1, SemanticSourceCatalogPairV1,
};
#[path = "semantic_source_writer.rs"]
mod writer;
pub use writer::{encode_semantic_source_capture_v1, encode_semantic_source_internal_v1, encode_semantic_source_leaf_v1};

use super::hash::try_digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass, fixed_array_at};
use super::scope::validate_canonical_absolute_path;
use super::semantic_mutation_control::{SemanticMutationCheckpointV1, decode_semantic_mutation_checkpoint};
use super::system_control::{SystemControlKindV1, decode_system_control};
use crate::engine::HashAlgorithm;

const NODE_DOMAIN: &[u8] = b"aeordb.semantic-source-node.v1\0";
const NODE_BODY_CAP: usize = 1 << 20;
const PATH_CAP: usize = 65_535;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticSourceCaptureV1<'a> {
  pub database_id: &'a [u8],
  pub task_id: &'a [u8],
  pub checkpoint_sequence: u64,
  pub physical_instance_id: &'a [u8],
  pub writer_fence_epoch: u64,
  pub semantic_generation: u64,
  pub header_sequence: u64,
  pub captured_at_ms: i64,
  pub protected_path_count: u64,
  pub base_catalog_node_count: u64,
  pub requested_catalog_node_count: u64,
  pub base_namespace_root: &'a [u8],
  pub staged_directory_root: &'a [u8],
  pub base_source_catalog: &'a [u8],
  pub requested_source_catalog: &'a [u8],
  pub source_identity_fingerprint: &'a [u8],
  pub checkpoint_payload_hash: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticSourceLeafEntryV1<'a> {
  pub path: &'a str,
  pub file_record_id: Option<&'a [u8]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticSourceChildV1<'a> {
  pub separator: Option<&'a str>,
  pub node_id: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
pub struct SemanticSourceNodeV1<'a> {
  database_id: &'a [u8],
  kind: u16,
  count: usize,
  width: usize,
  payload: &'a [u8],
}

impl<'a> SemanticSourceNodeV1<'a> {
  pub const fn database_id(&self) -> &'a [u8] {
    self.database_id
  }

  pub fn leaf_entries(&self) -> Option<SemanticSourceLeafEntriesV1<'a>> {
    (self.kind == 1).then_some(SemanticSourceLeafEntriesV1 { bytes: self.payload, remaining: self.count, width: self.width })
  }

  pub fn children(&self) -> Option<SemanticSourceChildrenV1<'a>> {
    (self.kind == 2).then_some(SemanticSourceChildrenV1 { bytes: self.payload, remaining: self.count + 1, width: self.width, first: true })
  }
}

pub struct SemanticSourceLeafEntriesV1<'a> {
  bytes: &'a [u8],
  remaining: usize,
  width: usize,
}

impl<'a> Iterator for SemanticSourceLeafEntriesV1<'a> {
  type Item = FormatResult<SemanticSourceLeafEntryV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.remaining == 0 {
      return None;
    }
    self.remaining -= 1;
    Some((|| {
      let path = take_path(&mut self.bytes)?;
      let identity = take(&mut self.bytes, self.width)?;
      Ok(SemanticSourceLeafEntryV1 { path, file_record_id: (!all_zero(identity)).then_some(identity) })
    })())
  }
}

pub struct SemanticSourceChildrenV1<'a> {
  bytes: &'a [u8],
  remaining: usize,
  width: usize,
  first: bool,
}

impl<'a> Iterator for SemanticSourceChildrenV1<'a> {
  type Item = FormatResult<SemanticSourceChildV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.remaining == 0 {
      return None;
    }
    self.remaining -= 1;
    Some((|| {
      let separator = if self.first {
        self.first = false;
        None
      } else {
        Some(take_path(&mut self.bytes)?)
      };
      Ok(SemanticSourceChildV1 { separator, node_id: take(&mut self.bytes, self.width)? })
    })())
  }
}

pub fn decode_semantic_source_capture_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<SemanticSourceCaptureV1<'_>> {
  let control = decode_system_control(bytes, algorithm)?;
  if control.kind != SystemControlKindV1::SemanticSourceCapture {
    return Err(kind_error());
  }
  decode_capture_body(control.body, algorithm)
}

pub fn decode_semantic_source_node_v1(bytes: &[u8], algorithm: HashAlgorithm) -> FormatResult<SemanticSourceNodeV1<'_>> {
  let control = decode_system_control(bytes, algorithm)?;
  if control.kind != SystemControlKindV1::SemanticSourceNode {
    return Err(kind_error());
  }
  decode_node_body(control.body, algorithm)
}

/// Bind exact immutable bytes, not current ownership, complete closure or executor availability.
pub fn decode_semantic_source_capture_binding_v1<'a>(
  capture_bytes: &'a [u8],
  checkpoint_bytes: &'a [u8],
  algorithm: HashAlgorithm,
) -> FormatResult<(SemanticSourceCaptureV1<'a>, SemanticMutationCheckpointV1<'a>)> {
  let capture = decode_semantic_source_capture_v1(capture_bytes, algorithm)?;
  let checkpoint = decode_semantic_mutation_checkpoint(checkpoint_bytes, algorithm)?;
  let digest = try_digest_parts(algorithm, &[checkpoint_bytes]).map_err(|source| {
    FormatError::allocation_failure("semantic_capture_digest_allocation", format!("cannot bind capture checkpoint: {source}"))
  })?;
  if capture.checkpoint_payload_hash != digest
    || capture.database_id != checkpoint.database_id
    || capture.task_id != checkpoint.task_id
    || capture.checkpoint_sequence != checkpoint.checkpoint_sequence
    || capture.physical_instance_id != checkpoint.physical_instance_id
    || capture.writer_fence_epoch != checkpoint.writer_fence_epoch
    || capture.semantic_generation != checkpoint.semantic_generation
    || capture.header_sequence != checkpoint.header_sequence
    || capture.captured_at_ms != checkpoint.captured_at_ms
    || capture.base_namespace_root != checkpoint.base_namespace_root
    || capture.staged_directory_root != checkpoint.staged_directory_root
    || capture.source_identity_fingerprint != checkpoint.source_identity_fingerprint
  {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "semantic_capture_checkpoint_binding",
      "capture and checkpoint disagree",
    ));
  }
  Ok((capture, checkpoint))
}

pub(crate) fn validate_body(kind: SystemControlKindV1, body: &[u8], algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  match kind {
    SystemControlKindV1::SemanticSourceCapture => {
      decode_capture_body(body, algorithm)?;
      let mut identity = Vec::new();
      identity.try_reserve_exact(24).map_err(|source| {
        FormatError::allocation_failure("semantic_capture_identity_allocation", format!("cannot retain capture identity: {source}"))
      })?;
      identity.extend_from_slice(&body[16..40]);
      Ok(identity)
    }
    SystemControlKindV1::SemanticSourceNode => {
      decode_node_body(body, algorithm)?;
      try_digest_parts(algorithm, &[NODE_DOMAIN, body]).map_err(|source| {
        FormatError::allocation_failure("semantic_capture_identity_allocation", format!("cannot retain source node identity: {source}"))
      })
    }
    _ => Err(kind_error()),
  }
}

fn decode_capture_body(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<SemanticSourceCaptureV1<'_>> {
  let width = algorithm.hash_length();
  if body.len() != 112 + 6 * width {
    return Err(length_error());
  }
  for field in [&body[..16], &body[16..32], &body[40..56]] {
    require_nonzero(field)?;
  }
  for offset in [32, 56, 64, 72, 88, 96, 104] {
    if u64_at(body, offset)? == 0 {
      return Err(identity_error());
    }
  }
  let captured_at_ms = i64::from_le_bytes(fixed_array_at(body, 80).ok_or_else(length_error)?);
  if captured_at_ms < 0 {
    return Err(identity_error());
  }
  let protected_path_count = u64_at(body, 88)?;
  let maximum_nodes = protected_path_count.checked_mul(2).and_then(|value| value.checked_sub(1)).ok_or_else(|| {
    error(MalformedInputClass::LengthCountOrArithmeticOverflow, "semantic_capture_count_overflow", "source node count bound overflowed")
  })?;
  if u64_at(body, 96)? > maximum_nodes || u64_at(body, 104)? > maximum_nodes {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "semantic_capture_node_count",
      "source node count exceeds path bound",
    ));
  }
  for hash in body[112..].chunks_exact(width) {
    require_nonzero(hash)?;
  }
  let hash = |index| &body[112 + index * width..112 + (index + 1) * width];
  Ok(SemanticSourceCaptureV1 {
    database_id: &body[..16],
    task_id: &body[16..32],
    checkpoint_sequence: u64_at(body, 32)?,
    physical_instance_id: &body[40..56],
    writer_fence_epoch: u64_at(body, 56)?,
    semantic_generation: u64_at(body, 64)?,
    header_sequence: u64_at(body, 72)?,
    captured_at_ms,
    protected_path_count,
    base_catalog_node_count: u64_at(body, 96)?,
    requested_catalog_node_count: u64_at(body, 104)?,
    base_namespace_root: hash(0),
    staged_directory_root: hash(1),
    base_source_catalog: hash(2),
    requested_source_catalog: hash(3),
    source_identity_fingerprint: hash(4),
    checkpoint_payload_hash: hash(5),
  })
}

fn decode_node_body(body: &[u8], algorithm: HashAlgorithm) -> FormatResult<SemanticSourceNodeV1<'_>> {
  if body.len() > NODE_BODY_CAP {
    return Err(error(MalformedInputClass::AllocationAmplification, "semantic_source_node_cap", "source node exceeds its body cap"));
  }
  if body.len() < 32 {
    return Err(length_error());
  }
  require_nonzero(&body[..16])?;
  let kind = u16::from_le_bytes(fixed_array_at(body, 16).ok_or_else(length_error)?);
  if !matches!(kind, 1 | 2) {
    return Err(kind_error());
  }
  if body[18..20] != [0; 2] || body[28..32] != [0; 4] {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "semantic_source_node_reserved", "source node reserve is nonzero"));
  }
  let count = u32::from_le_bytes(fixed_array_at(body, 20).ok_or_else(length_error)?) as usize;
  if count == 0 || count > if kind == 1 { 256 } else { 128 } {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "semantic_source_node_count",
      "source node count exceeds its kind bound",
    ));
  }
  let payload_length = u32::from_le_bytes(fixed_array_at(body, 24).ok_or_else(length_error)?) as usize;
  if payload_length != body.len() - 32 {
    return Err(length_error());
  }
  let width = algorithm.hash_length();
  let mut bytes = &body[32..];
  // Fixed maximum fanout: no allocation or repeated prefix parse per child.
  let mut children: [&[u8]; 129] = [&[]; 129];
  if kind == 2 {
    children[0] = take(&mut bytes, width)?;
    require_nonzero(children[0])?;
  }
  let mut previous: Option<&str> = None;
  for index in 0..count {
    let path = take_path(&mut bytes)?;
    if previous.is_some_and(|prior| prior.as_bytes() >= path.as_bytes()) {
      return Err(error(
        MalformedInputClass::NoncanonicalOrderOrDuplicate,
        "semantic_source_node_order",
        "source paths are not strictly ordered",
      ));
    }
    previous = Some(path);
    let identity = take(&mut bytes, width)?;
    if kind == 2 {
      require_nonzero(identity)?;
      if children[..=index].contains(&identity) {
        return Err(error(
          MalformedInputClass::InvalidGraphEdgeOrCycle,
          "semantic_source_node_child",
          "source node repeats a child identity",
        ));
      }
      children[index + 1] = identity;
    }
  }
  if !bytes.is_empty() {
    return Err(length_error());
  }
  Ok(SemanticSourceNodeV1 { database_id: &body[..16], kind, count, width, payload: &body[32..] })
}

fn take<'a>(bytes: &mut &'a [u8], length: usize) -> FormatResult<&'a [u8]> {
  let value = bytes.get(..length).ok_or_else(length_error)?;
  *bytes = &bytes[length..];
  Ok(value)
}

fn take_path<'a>(bytes: &mut &'a [u8]) -> FormatResult<&'a str> {
  let length = u32::from_le_bytes(fixed_array_at(take(bytes, 4)?, 0).ok_or_else(length_error)?) as usize;
  if length == 0 || length > PATH_CAP {
    return Err(error(MalformedInputClass::AllocationAmplification, "semantic_source_path_cap", "source path must contain1..65535 bytes"));
  }
  let path = std::str::from_utf8(take(bytes, length)?)
    .map_err(|source| error(MalformedInputClass::InvalidUtf8PathGlobOrNativePath, "semantic_source_path_utf8", source.to_string()))?;
  validate_canonical_absolute_path(path)?;
  Ok(path)
}

fn u64_at(bytes: &[u8], offset: usize) -> FormatResult<u64> {
  Ok(u64::from_le_bytes(fixed_array_at(bytes, offset).ok_or_else(length_error)?))
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn require_nonzero(bytes: &[u8]) -> FormatResult<()> {
  if all_zero(bytes) {
    Err(identity_error())
  } else {
    Ok(())
  }
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}

fn length_error() -> FormatError {
  error(MalformedInputClass::TruncationOrTrailingBytes, "semantic_capture_length", "source capture bytes do not close their declared shape")
}

fn kind_error() -> FormatError {
  error(MalformedInputClass::UnknownTypeKindOrEnum, "semantic_capture_kind", "unknown or mismatched source capture kind")
}

fn identity_error() -> FormatError {
  error(
    MalformedInputClass::IdentityKeyOrGenerationMismatch,
    "semantic_capture_identity",
    "source capture identity, sequence or timestamp is invalid",
  )
}
