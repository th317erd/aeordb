//! Byte-only capture encoders. No physical publication, closure or resume authority.
use super::{NODE_BODY_CAP, PATH_CAP, SemanticSourceCaptureV1, SemanticSourceChildV1, SemanticSourceLeafEntryV1, error, require_nonzero};
use super::super::reader::{FormatResult, MalformedInputClass};
use super::super::scope::validate_canonical_absolute_path;
use super::super::system_control::{SystemControlKindV1, encode_system_control_with_body};
use crate::engine::HashAlgorithm;

/// Encode one immutable companion. Declared counts are not traversal evidence.
pub fn encode_semantic_source_capture_v1(capture: &SemanticSourceCaptureV1<'_>, algorithm: HashAlgorithm) -> FormatResult<Vec<u8>> {
  for identity in [capture.database_id, capture.task_id, capture.physical_instance_id] {
    require_field(identity, 16)?;
  }
  let width = algorithm.hash_length();
  let hashes = [
    capture.base_namespace_root,
    capture.staged_directory_root,
    capture.base_source_catalog,
    capture.requested_source_catalog,
    capture.source_identity_fingerprint,
    capture.checkpoint_payload_hash,
  ];
  for hash in hashes {
    require_field(hash, width)?;
  }
  // Fixed-size fill, followed by the shared decoder's scalar/count validation.
  encode_system_control_with_body(SystemControlKindV1::SemanticSourceCapture, 1, 112 + 6 * width, algorithm, |body| {
    body[..16].copy_from_slice(capture.database_id);
    body[16..32].copy_from_slice(capture.task_id);
    body[40..56].copy_from_slice(capture.physical_instance_id);
    for (offset, value) in [
      (32, capture.checkpoint_sequence),
      (56, capture.writer_fence_epoch),
      (64, capture.semantic_generation),
      (72, capture.header_sequence),
      (88, capture.protected_path_count),
      (96, capture.base_catalog_node_count),
      (104, capture.requested_catalog_node_count),
    ] {
      body[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    body[80..88].copy_from_slice(&capture.captured_at_ms.to_le_bytes());
    for (index, hash) in hashes.into_iter().enumerate() {
      body[112 + index * width..112 + (index + 1) * width].copy_from_slice(hash);
    }
  })
}

/// Preserve explicit absence without treating Some(zero) as absent.
pub fn encode_semantic_source_leaf_v1(
  database_id: &[u8],
  rows: &[SemanticSourceLeafEntryV1<'_>],
  algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  require_field(database_id, 16)?;
  require_count(rows.len(), 1, 256)?;
  let width = algorithm.hash_length();
  let mut body_length = 32;
  let mut previous = None;
  for row in rows {
    validate_path(row.path, &mut previous)?;
    if let Some(identity) = row.file_record_id {
      require_field(identity, width)?;
    }
    body_length = add_row_length(body_length, row.path, width)?;
  }
  encode_system_control_with_body(SystemControlKindV1::SemanticSourceNode, 1, body_length, algorithm, |body| {
    fill_node_header(body, database_id, 1, rows.len());
    let mut offset = 32;
    for row in rows {
      put_path(body, &mut offset, row.path);
      if let Some(identity) = row.file_record_id {
        body[offset..offset + width].copy_from_slice(identity);
      }
      // The shared framing owner zero-fills absent identities and reserves.
      offset += width;
    }
  })
}

/// The first child has no separator; later children have strictly ordered paths.
pub fn encode_semantic_source_internal_v1(
  database_id: &[u8],
  children: &[SemanticSourceChildV1<'_>],
  algorithm: HashAlgorithm,
) -> FormatResult<Vec<u8>> {
  require_field(database_id, 16)?;
  require_count(children.len(), 2, 129)?;
  let width = algorithm.hash_length();
  let mut body_length = 32 + width;
  let mut previous = None;
  for (index, child) in children.iter().enumerate() {
    require_field(child.node_id, width)?;
    if children[..index].iter().any(|prior| prior.node_id == child.node_id) {
      return Err(error(
        MalformedInputClass::InvalidGraphEdgeOrCycle,
        "semantic_source_node_child",
        "source node repeats a child identity",
      ));
    }
    match (index, child.separator) {
      (0, None) => {}
      (1.., Some(path)) => {
        validate_path(path, &mut previous)?;
        body_length = add_row_length(body_length, path, width)?;
      }
      _ => {
        return Err(error(
          MalformedInputClass::CrossRecordClosureMismatch,
          "semantic_source_child_separator",
          "only the first child must omit its separator",
        ))
      }
    }
  }
  encode_system_control_with_body(SystemControlKindV1::SemanticSourceNode, 1, body_length, algorithm, |body| {
    fill_node_header(body, database_id, 2, children.len() - 1);
    let mut offset = 32;
    for child in children {
      if let Some(path) = child.separator {
        put_path(body, &mut offset, path);
      }
      body[offset..offset + width].copy_from_slice(child.node_id);
      offset += width;
    }
  })
}

fn require_field(bytes: &[u8], width: usize) -> FormatResult<()> {
  if bytes.len() != width {
    return Err(error(
      MalformedInputClass::LengthCountOrArithmeticOverflow,
      "semantic_source_field_width",
      "source capture field has the wrong width",
    ));
  }
  require_nonzero(bytes)
}

fn require_count(count: usize, minimum: usize, maximum: usize) -> FormatResult<()> {
  if !(minimum..=maximum).contains(&count) {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "semantic_source_node_count",
      "source node count exceeds its kind bound",
    ));
  }
  Ok(())
}

fn validate_path<'a>(path: &'a str, previous: &mut Option<&'a str>) -> FormatResult<()> {
  if path.is_empty() || path.len() > PATH_CAP {
    return Err(error(MalformedInputClass::AllocationAmplification, "semantic_source_path_cap", "source path must contain1..65535 bytes"));
  }
  validate_canonical_absolute_path(path)?;
  if previous.is_some_and(|prior| prior.as_bytes() >= path.as_bytes()) {
    return Err(error(
      MalformedInputClass::NoncanonicalOrderOrDuplicate,
      "semantic_source_node_order",
      "source paths are not strictly ordered",
    ));
  }
  *previous = Some(path);
  Ok(())
}

fn add_row_length(length: usize, path: &str, width: usize) -> FormatResult<usize> {
  length
    .checked_add(4 + path.len() + width)
    .filter(|value| *value <= NODE_BODY_CAP)
    .ok_or_else(|| error(MalformedInputClass::AllocationAmplification, "semantic_source_node_cap", "source node exceeds its body cap"))
}

fn fill_node_header(body: &mut [u8], database_id: &[u8], kind: u16, count: usize) {
  let payload_length = body.len() - 32;
  body[..16].copy_from_slice(database_id);
  body[16..18].copy_from_slice(&kind.to_le_bytes());
  body[20..24].copy_from_slice(&(count as u32).to_le_bytes());
  body[24..28].copy_from_slice(&(payload_length as u32).to_le_bytes());
}

fn put_path(body: &mut [u8], offset: &mut usize, path: &str) {
  body[*offset..*offset + 4].copy_from_slice(&(path.len() as u32).to_le_bytes());
  *offset += 4;
  body[*offset..*offset + path.len()].copy_from_slice(path.as_bytes());
  *offset += path.len();
}
