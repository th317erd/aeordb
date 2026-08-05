use crate::engine::HashAlgorithm;

use super::database_header::validate_capabilities;
use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};

const DIRECTORY_HEADER_LENGTH: usize = 32;
const DIRECTORY_KIND_NAMESPACE_ROOT: u16 = 0x0003;
const SEMANTIC_HEADER_LENGTH: usize = 32;
const SEMANTIC_HARD_CAP: usize = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceRootV1 {
  pub root_hash: Vec<u8>,
  pub required_capabilities: [u8; 32],
  pub namespace_schema_version: u16,
  pub semantic_schema_version: u16,
  pub namespace_tree_root: Vec<u8>,
  pub semantic_state_root: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticObjectKind {
  State { content_only_reason: Option<u16> },
  CatalogLeaf { record_count: u32 },
  CatalogInternal { child_count: u16 },
  Definition { class: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticObjectV1 {
  pub object_id: Vec<u8>,
  pub kind_id: u16,
  pub kind: SemanticObjectKind,
  pub graph_edges: Vec<Vec<u8>>,
}

pub fn decode_namespace_root(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<NamespaceRootV1> {
  let hash_width = hash_algorithm.hash_length();
  let body_length = checked_add(72, checked_mul(2, hash_width, "namespace root hashes")?, "namespace root body")?;
  let expected_length = checked_add(DIRECTORY_HEADER_LENGTH, body_length, "directory envelope")?
    .checked_add(4)
    .ok_or_else(|| length_error("directory total length overflow"))?;
  if value.len() != expected_length {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "directory_length",
      format!("expected {expected_length}, got {}", value.len()),
    ));
  }
  verify_trailing_crc(value)?;
  if &value[..4] != b"ADIR" || u16_at(value, 4) != 1 || u16_at(value, 8) as usize != DIRECTORY_HEADER_LENGTH {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "directory_envelope", "expected ADIR v1 with 32-byte envelope"));
  }
  let kind = u16_at(value, 6);
  if kind != DIRECTORY_KIND_NAMESPACE_ROOT {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "directory_kind",
      format!("kind {kind:#06x} is unknown or writer-disabled"),
    ));
  }
  if u32_at(value, 12) as usize != value.len() || u32_at(value, 16) as usize != body_length {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "directory_lengths",
      format!("total {}, body {}", u32_at(value, 12), u32_at(value, 16)),
    ));
  }
  if u16_at(value, 10) != 0 || u32_at(value, 20) != 0 || value[24..32].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "directory_reserved",
      "directory envelope flags or reserve are nonzero",
    ));
  }

  let body = &value[DIRECTORY_HEADER_LENGTH..value.len() - 4];
  if u32_at(body, 0) != 0 {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "namespace_root_flags", "namespace root flags must be zero"));
  }
  let required_capabilities: [u8; 32] = body[4..36].try_into().expect("fixed namespace capability field");
  validate_capabilities(&required_capabilities, "namespace root")?;
  let namespace_schema_version = u16_at(body, 36);
  let semantic_schema_version = u16_at(body, 38);
  if namespace_schema_version != 1 || semantic_schema_version != 1 {
    return Err(error(
      MalformedInputClass::UnknownMagicOrVersion,
      "namespace_root_schema",
      format!("namespace {namespace_schema_version}, semantic {semantic_schema_version}"),
    ));
  }

  let namespace_tree_root = body[40..40 + hash_width].to_vec();
  let semantic_state_root = body[40 + hash_width..40 + 2 * hash_width].to_vec();
  if all_zero(&namespace_tree_root) || all_zero(&semantic_state_root) {
    return Err(error(
      MalformedInputClass::InvalidGraphEdgeOrCycle,
      "namespace_root_zero_edge",
      "namespace and semantic roots must be nonzero",
    ));
  }
  if body[40 + 2 * hash_width..].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "namespace_root_reserved", "namespace root reserve is nonzero"));
  }

  let root_hash = immutable_id(hash_algorithm, b"aeordb.directory-index.immutable.v1\0", kind, value);
  Ok(NamespaceRootV1 {
    root_hash,
    required_capabilities,
    namespace_schema_version,
    semantic_schema_version,
    namespace_tree_root,
    semantic_state_root,
  })
}

pub fn decode_semantic_object(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<SemanticObjectV1> {
  if value.len() > SEMANTIC_HARD_CAP {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "semantic_object_exceeds_cap",
      format!("{} bytes exceeds {SEMANTIC_HARD_CAP}", value.len()),
    ));
  }
  if value.len() < SEMANTIC_HEADER_LENGTH + 4 {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "semantic_truncated",
      "semantic object is shorter than its envelope",
    ));
  }
  let declared_total = u32_at(value, 12) as usize;
  if declared_total != value.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "semantic_total_length",
      format!("declared {declared_total}, got {}", value.len()),
    ));
  }
  verify_trailing_crc(value)?;
  if &value[..4] != b"ASEM" || u16_at(value, 4) != 1 || u16_at(value, 8) as usize != SEMANTIC_HEADER_LENGTH {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "semantic_envelope", "expected ASEM v1 with 32-byte envelope"));
  }
  let body_length = u32_at(value, 16) as usize;
  if body_length != value.len() - SEMANTIC_HEADER_LENGTH - 4 {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "semantic_body_length",
      format!("declared {body_length}, actual {}", value.len() - SEMANTIC_HEADER_LENGTH - 4),
    ));
  }
  if u16_at(value, 10) != 0 || value[28..32].iter().any(|byte| *byte != 0) {
    return Err(error(
      MalformedInputClass::NonzeroReservedOrPadding,
      "semantic_reserved",
      "semantic envelope flags or reserve are nonzero",
    ));
  }

  let kind_id = u16_at(value, 6);
  let item_count = u64_at(value, 20);
  let body = &value[SEMANTIC_HEADER_LENGTH..value.len() - 4];
  let (kind, graph_edges) = match kind_id {
    0x0001 => decode_semantic_state(body, item_count, hash_algorithm)?,
    0x0002 => decode_catalog_leaf(body, item_count, hash_algorithm)?,
    0x0003 => decode_catalog_internal(body, item_count, hash_algorithm)?,
    0x0004 => decode_definition(body, item_count, hash_algorithm)?,
    _ => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "semantic_kind", format!("unknown semantic kind {kind_id:#06x}")));
    }
  };
  let object_id = immutable_id(hash_algorithm, b"aeordb.semantic-object.immutable.v1\0", kind_id, value);
  Ok(SemanticObjectV1 { object_id, kind_id, kind, graph_edges })
}

fn decode_semantic_state(body: &[u8], item_count: u64, hash_algorithm: HashAlgorithm) -> FormatResult<(SemanticObjectKind, Vec<Vec<u8>>)> {
  let hash_width = hash_algorithm.hash_length();
  let expected_length = checked_add(112, checked_mul(3, hash_width, "semantic state hashes")?, "semantic state body")?;
  if body.len() != expected_length {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "semantic_state_length",
      format!("expected {expected_length}, got {}", body.len()),
    ));
  }
  let flags = u32_at(body, 0);
  if flags & !1 != 0 {
    return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "semantic_state_flags", format!("flags {flags:#010x}")));
  }
  let capabilities: [u8; 32] = body[4..36].try_into().expect("fixed semantic capability field");
  validate_capabilities(&capabilities, "semantic state")?;
  if u16_at(body, 36) != 1 || u16_at(body, 38) != 1 || u16_at(body, 40) != 1 {
    return Err(error(MalformedInputClass::UnknownMagicOrVersion, "semantic_state_schema", "semantic state schema versions must be one"));
  }
  let reason = u16_at(body, 42);
  let catalog_present = match body[44] {
    0 => false,
    1 => true,
    value => {
      return Err(error(
        MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
        "semantic_catalog_presence",
        format!("presence {value}"),
      ));
    }
  };
  if body[45..48].iter().any(|byte| *byte != 0) || body[80 + 3 * hash_width..].iter().any(|byte| *byte != 0) {
    return Err(error(MalformedInputClass::NonzeroReservedOrPadding, "semantic_state_reserved", "semantic state reserve is nonzero"));
  }

  let hashes = &body[48..48 + 3 * hash_width];
  let counts = [
    u64_at(body, 48 + 3 * hash_width),
    u64_at(body, 56 + 3 * hash_width),
    u64_at(body, 64 + 3 * hash_width),
    u64_at(body, 72 + 3 * hash_width),
  ];
  if flags == 0 {
    if reason != 0
      || !catalog_present
      || hashes.chunks(hash_width).any(all_zero)
      || counts[0] != item_count
      || counts[0] == 0
      || counts[1] == 0
    {
      return Err(error(
        MalformedInputClass::CrossRecordClosureMismatch,
        "semantic_state_complete_invariant",
        "complete semantic state has incomplete identities or counts",
      ));
    }
    let catalog_root = hashes[2 * hash_width..3 * hash_width].to_vec();
    Ok((SemanticObjectKind::State { content_only_reason: None }, vec![catalog_root]))
  } else {
    if !(1..=3).contains(&reason)
      || catalog_present
      || hashes.iter().any(|byte| *byte != 0)
      || counts.iter().any(|count| *count != 0)
      || item_count != 0
    {
      return Err(error(
        MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
        "semantic_state_content_only_invariant",
        "content-only semantic state has forbidden authority",
      ));
    }
    Ok((SemanticObjectKind::State { content_only_reason: Some(reason) }, Vec::new()))
  }
}

fn decode_definition(body: &[u8], item_count: u64, hash_algorithm: HashAlgorithm) -> FormatResult<(SemanticObjectKind, Vec<Vec<u8>>)> {
  let hash_width = hash_algorithm.hash_length();
  let minimum = checked_add(16, hash_width, "semantic definition minimum")?;
  if body.len() < minimum || item_count != 1 {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "semantic_definition_length",
      format!("minimum {minimum}, body {}, items {item_count}", body.len()),
    ));
  }
  let class = u16_at(body, 0);
  if !(1..=7).contains(&class) || u16_at(body, 2) != 1 || u32_at(body, 4) != 0 {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "semantic_definition_metadata",
      format!("class {class}, schema {}, flags {}", u16_at(body, 2), u32_at(body, 4)),
    ));
  }
  if all_zero(&body[8..8 + hash_width]) {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "semantic_definition_zero_id",
      "semantic definition ID is zero",
    ));
  }
  let definition_length = u32_at(body, 8 + hash_width) as usize;
  let expected = minimum.checked_add(definition_length).ok_or_else(|| length_error("semantic definition length overflow"))?;
  if body[12 + hash_width..16 + hash_width].iter().any(|byte| *byte != 0) || expected != body.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "semantic_definition_body",
      format!("expected {expected}, got {}", body.len()),
    ));
  }
  Ok((SemanticObjectKind::Definition { class }, Vec::new()))
}

fn decode_catalog_leaf(body: &[u8], item_count: u64, hash_algorithm: HashAlgorithm) -> FormatResult<(SemanticObjectKind, Vec<Vec<u8>>)> {
  let hash_width = hash_algorithm.hash_length();
  let prefix_length = checked_add(16, hash_width, "catalog leaf prefix")?;
  if body.len() < prefix_length || u32_at(body, 0) != 0 {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "catalog_leaf_length_or_flags",
      "catalog leaf prefix is truncated or flags are nonzero",
    ));
  }
  let record_count = u32_at(body, 4);
  if record_count == 0 || record_count > 4_096 {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "catalog_leaf_count",
      format!("records {record_count}, items {item_count}"),
    ));
  }
  if item_count != u64::from(record_count) {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "catalog_leaf_item_count",
      format!("records {record_count}, items {item_count}"),
    ));
  }
  let lookup_digest = &body[8..8 + hash_width];
  let records_length = u32_at(body, 8 + hash_width) as usize;
  if body[12 + hash_width..16 + hash_width].iter().any(|byte| *byte != 0) || prefix_length.checked_add(records_length) != Some(body.len()) {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "catalog_leaf_records_length",
      format!("prefix {prefix_length}, records {records_length}, body {}", body.len()),
    ));
  }

  let record_hash_bytes = checked_mul(2, hash_width, "catalog leaf record hashes")?;
  let record_prefix = checked_add(8, record_hash_bytes, "catalog leaf record prefix")?;
  let mut cursor = prefix_length;
  let mut previous: Option<(u16, &[u8])> = None;
  let mut graph_edges = Vec::with_capacity(record_count as usize);
  for _ in 0..record_count {
    if body.len().saturating_sub(cursor) < record_prefix {
      return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "catalog_leaf_record_truncated", "record prefix is truncated"));
    }
    let kind = u16_at(body, cursor);
    let flags = u16_at(body, cursor + 2);
    let key_length = u32_at(body, cursor + 4) as usize;
    let record_length = record_prefix.checked_add(key_length).ok_or_else(|| length_error("catalog leaf record overflow"))?;
    let record_end = cursor.checked_add(record_length).ok_or_else(|| length_error("catalog leaf record end overflow"))?;
    if !(1..=7).contains(&kind) || flags != 0 || record_end > body.len() {
      return Err(error(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "catalog_leaf_record_metadata",
        format!("kind {kind}, flags {flags}, end {record_end}"),
      ));
    }
    let semantic_id = &body[cursor + 8..cursor + 8 + hash_width];
    let definition_id = &body[cursor + 8 + hash_width..cursor + record_prefix];
    if all_zero(semantic_id) || all_zero(definition_id) {
      return Err(error(MalformedInputClass::InvalidGraphEdgeOrCycle, "catalog_leaf_zero_hash", "record identity or edge is zero"));
    }
    let owner_key = &body[cursor + record_prefix..record_end];
    let kind_bytes = kind.to_le_bytes();
    let expected_lookup = digest_parts(hash_algorithm, &[b"aeordb.semantic-catalog-key.v1\0", &kind_bytes, owner_key]);
    if expected_lookup != lookup_digest {
      return Err(error(
        MalformedInputClass::IdentityKeyOrGenerationMismatch,
        "catalog_leaf_lookup_digest",
        "record does not belong to the leaf lookup digest",
      ));
    }
    if previous.is_some_and(|prior| prior.0 > kind || (prior.0 == kind && prior.1 >= owner_key)) {
      return Err(error(MalformedInputClass::NoncanonicalOrderOrDuplicate, "catalog_leaf_order", "records are not strictly ordered"));
    }
    previous = Some((kind, owner_key));
    graph_edges.push(definition_id.to_vec());
    cursor = record_end;
  }
  if cursor != body.len() {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "catalog_leaf_trailing", "unconsumed catalog leaf bytes"));
  }
  Ok((SemanticObjectKind::CatalogLeaf { record_count }, graph_edges))
}

fn decode_catalog_internal(
  body: &[u8],
  item_count: u64,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<(SemanticObjectKind, Vec<Vec<u8>>)> {
  let hash_width = hash_algorithm.hash_length();
  if body.len() < 20 || u32_at(body, 0) != 0 {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "catalog_internal_length_or_flags",
      "catalog internal prefix is truncated or flags are nonzero",
    ));
  }
  let depth = usize::from(u16_at(body, 4));
  let prefix_length = usize::from(u16_at(body, 6));
  let child_count = u16_at(body, 8);
  if !(2..=256).contains(&child_count)
    || item_count != u64::from(child_count)
    || u16_at(body, 10) != 0
    || depth.checked_add(prefix_length).is_none_or(|next| next >= hash_width)
  {
    return Err(error(
      MalformedInputClass::InvalidGraphEdgeOrCycle,
      "catalog_internal_metadata",
      format!("depth {depth}, prefix {prefix_length}, children {child_count}, items {item_count}"),
    ));
  }
  let child_length = checked_add(12, hash_width, "catalog internal child")?;
  let children_bytes = checked_mul(usize::from(child_count), child_length, "catalog internal children")?;
  let expected_length = checked_add(20, prefix_length, "catalog internal prefix")?
    .checked_add(children_bytes)
    .ok_or_else(|| length_error("catalog internal body overflow"))?;
  if expected_length != body.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "catalog_internal_body_length",
      format!("expected {expected_length}, got {}", body.len()),
    ));
  }

  let mut sum = 0u64;
  let mut previous_edge = None;
  let mut graph_edges = Vec::with_capacity(usize::from(child_count));
  for index in 0..usize::from(child_count) {
    let offset = 20 + prefix_length + index * child_length;
    let edge = body[offset];
    let child_id = &body[offset + 12..offset + child_length];
    if previous_edge.is_some_and(|previous| previous >= edge) {
      return Err(error(
        MalformedInputClass::NoncanonicalOrderOrDuplicate,
        "catalog_internal_child",
        format!("child {index} edge is unordered"),
      ));
    }
    if body[offset + 1] != 0 || u16_at(body, offset + 2) != 0 {
      return Err(error(
        MalformedInputClass::NonzeroReservedOrPadding,
        "catalog_internal_child_reserved",
        format!("child {index} reserve is nonzero"),
      ));
    }
    if all_zero(child_id) {
      return Err(error(MalformedInputClass::InvalidGraphEdgeOrCycle, "catalog_internal_zero_child", format!("child {index}")));
    }
    let count = u64_at(body, offset + 4);
    if count == 0 {
      return Err(error(MalformedInputClass::InvalidGraphEdgeOrCycle, "catalog_internal_zero_count", format!("child {index}")));
    }
    sum = sum.checked_add(count).ok_or_else(|| length_error("catalog internal subtree count overflow"))?;
    previous_edge = Some(edge);
    graph_edges.push(child_id.to_vec());
  }
  if sum != u64_at(body, 12) {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "catalog_internal_subtree_count",
      format!("children sum {sum}, stored {}", u64_at(body, 12)),
    ));
  }
  Ok((SemanticObjectKind::CatalogInternal { child_count }, graph_edges))
}

fn immutable_id(hash_algorithm: HashAlgorithm, domain: &[u8], kind: u16, value: &[u8]) -> Vec<u8> {
  let kind = kind.to_le_bytes();
  digest_parts(hash_algorithm, &[domain, &kind, value])
}

fn verify_trailing_crc(value: &[u8]) -> FormatResult<()> {
  let crc_offset = value.len() - 4;
  let stored = u32_at(value, crc_offset);
  let computed = crc32fast::hash(&value[..crc_offset]);
  if stored != computed {
    return Err(error(
      MalformedInputClass::ChecksumOrIntegrityMismatch,
      "crc_mismatch",
      format!("stored {stored:#010x}, computed {computed:#010x}"),
    ));
  }
  Ok(())
}

fn checked_mul(count: usize, width: usize, context: &'static str) -> FormatResult<usize> {
  count.checked_mul(width).ok_or_else(|| length_error(format!("{context} multiplication overflow")))
}

fn checked_add(left: usize, right: usize, context: &'static str) -> FormatResult<usize> {
  left.checked_add(right).ok_or_else(|| length_error(format!("{context} addition overflow")))
}

fn all_zero(bytes: &[u8]) -> bool {
  bytes.iter().all(|byte| *byte == 0)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
  u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("validated namespace bounds"))
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("validated namespace bounds"))
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
  u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("validated namespace bounds"))
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "length_overflow", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
