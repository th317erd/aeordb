use crate::engine::btree::{BTreeNode, is_btree_format};
use crate::engine::directory_entry::{ChildEntry, deserialize_child_entries};
use crate::engine::entry_type::EntryType;
use crate::engine::{CompressionAlgorithm, HashAlgorithm};

use super::database_header::validate_capabilities;
use super::entity::{EntryTypeV4, WHOLE_ENTITY_V1_FLAG_SYSTEM, decode_whole_entity};
use super::hash::digest_parts;
use super::reader::{FormatError, FormatResult, MalformedInputClass};
use super::scope::validate_canonical_absolute_path;

const DIRECTORY_HEADER_LENGTH: usize = 32;
const DIRECTORY_KIND_NAMESPACE_ROOT: u16 = 0x0003;
const SEMANTIC_HEADER_LENGTH: usize = 32;
const SEMANTIC_HARD_CAP: usize = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceRootV1 {
  pub root_hash: Vec<u8>,
  pub required_capabilities: [u8; 32],
  pub namespace_tree_codec: u16,
  pub semantic_state_codec: u16,
  pub namespace_tree_root: Vec<u8>,
  pub semantic_state_root: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceRootWriteV1 {
  pub required_capabilities: [u8; 32],
  pub namespace_tree_root: Vec<u8>,
  pub semantic_state_root: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedNamespaceRootV1 {
  pub root_hash: Vec<u8>,
  pub value: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceTreeLayoutV0 {
  Empty,
  Flat { child_count: usize },
  BTreeLeaf { child_count: usize },
  BTreeInternal { separator_count: usize, child_count: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceTreeRootV0 {
  pub root_hash: Vec<u8>,
  pub layout: NamespaceTreeLayoutV0,
  pub edges: Vec<NamespaceTreeEdgeV0>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceTreeEdgeV0 {
  Entry { name: String, entry_type: EntryType, identity: Vec<u8> },
  BTreeNode { identity: Vec<u8> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum SemanticUnavailableReasonV1 {
  LegacyGlobalStateNotCaptured = 1,
  LegacyDependencyCannotBeProven = 2,
  LegacySemanticControlCorruptOrIncomplete = 3,
}

impl SemanticUnavailableReasonV1 {
  fn from_u16(value: u16) -> Option<Self> {
    match value {
      1 => Some(Self::LegacyGlobalStateNotCaptured),
      2 => Some(Self::LegacyDependencyCannotBeProven),
      3 => Some(Self::LegacySemanticControlCorruptOrIncomplete),
      _ => None,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticAvailabilityV1 {
  Complete {
    compiler_fingerprint: Vec<u8>,
    semantic_registry_fingerprint: Vec<u8>,
    catalog_root: Vec<u8>,
    catalog_record_count: u64,
    catalog_node_count: u64,
    definition_count: u64,
    dependency_count: u64,
  },
  ContentOnly {
    reason: SemanticUnavailableReasonV1,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticStateV1 {
  pub object_id: Vec<u8>,
  pub required_capabilities: [u8; 32],
  pub semantic_catalog_codec: u16,
  pub semantic_definition_codec: u16,
  pub compiler_profile_version: u16,
  pub availability: SemanticAvailabilityV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticStateWriteV1 {
  pub required_capabilities: [u8; 32],
  pub availability: SemanticAvailabilityV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedSemanticObjectV1 {
  pub object_id: Vec<u8>,
  pub value: Vec<u8>,
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
  pub semantic_state: Option<SemanticStateV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDefinitionRecordV1<'a> {
  pub object_id: Vec<u8>,
  pub class: u16,
  pub semantic_id: &'a [u8],
  pub definition: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticCatalogRecordV1<'a> {
  pub record_kind: u16,
  pub semantic_id: &'a [u8],
  pub definition_object_id: &'a [u8],
  pub owner_key: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticCatalogChildV1<'a> {
  pub edge: u8,
  pub record_count: u64,
  pub object_id: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticCatalogLeafV1<'a> {
  object_id: Vec<u8>,
  lookup_digest: &'a [u8],
  records: &'a [u8],
  record_count: u32,
  hash_width: usize,
}

impl<'a> SemanticCatalogLeafV1<'a> {
  pub fn object_id(&self) -> &[u8] {
    &self.object_id
  }

  pub fn lookup_digest(&self) -> &[u8] {
    self.lookup_digest
  }

  pub const fn record_count(&self) -> u32 {
    self.record_count
  }

  pub fn records(&self) -> SemanticCatalogRecordIteratorV1<'a> {
    SemanticCatalogRecordIteratorV1 {
      bytes: self.records,
      hash_width: self.hash_width,
      remaining: self.record_count,
      cursor: 0,
      failed: false,
    }
  }
}

pub struct SemanticCatalogRecordIteratorV1<'a> {
  bytes: &'a [u8],
  hash_width: usize,
  remaining: u32,
  cursor: usize,
  failed: bool,
}

impl<'a> Iterator for SemanticCatalogRecordIteratorV1<'a> {
  type Item = FormatResult<SemanticCatalogRecordV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed || self.remaining == 0 {
      return None;
    }
    match decode_catalog_record_view(self.bytes, self.cursor, self.hash_width) {
      Ok((record, next_cursor)) => {
        self.cursor = next_cursor;
        self.remaining -= 1;
        Some(Ok(record))
      }
      Err(error) => {
        self.failed = true;
        Some(Err(error))
      }
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticCatalogInternalV1<'a> {
  object_id: Vec<u8>,
  depth: u16,
  prefix: &'a [u8],
  children: &'a [u8],
  child_count: u16,
  subtree_record_count: u64,
  hash_width: usize,
}

impl<'a> SemanticCatalogInternalV1<'a> {
  pub fn object_id(&self) -> &[u8] {
    &self.object_id
  }

  pub const fn depth(&self) -> u16 {
    self.depth
  }

  pub const fn prefix(&self) -> &[u8] {
    self.prefix
  }

  pub const fn child_count(&self) -> u16 {
    self.child_count
  }

  pub const fn subtree_record_count(&self) -> u64 {
    self.subtree_record_count
  }

  pub fn children(&self) -> SemanticCatalogChildIteratorV1<'a> {
    SemanticCatalogChildIteratorV1 {
      bytes: self.children,
      hash_width: self.hash_width,
      remaining: self.child_count,
      cursor: 0,
      failed: false,
    }
  }
}

pub struct SemanticCatalogChildIteratorV1<'a> {
  bytes: &'a [u8],
  hash_width: usize,
  remaining: u16,
  cursor: usize,
  failed: bool,
}

impl<'a> Iterator for SemanticCatalogChildIteratorV1<'a> {
  type Item = FormatResult<SemanticCatalogChildV1<'a>>;

  fn next(&mut self) -> Option<Self::Item> {
    if self.failed || self.remaining == 0 {
      return None;
    }
    match decode_catalog_child_view(self.bytes, self.cursor, self.hash_width) {
      Ok((child, next_cursor)) => {
        self.cursor = next_cursor;
        self.remaining -= 1;
        Some(Ok(child))
      }
      Err(error) => {
        self.failed = true;
        Some(Err(error))
      }
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticCatalogNodeV1<'a> {
  Leaf(SemanticCatalogLeafV1<'a>),
  Internal(SemanticCatalogInternalV1<'a>),
}

impl SemanticCatalogNodeV1<'_> {
  pub fn object_id(&self) -> &[u8] {
    match self {
      Self::Leaf(node) => node.object_id(),
      Self::Internal(node) => node.object_id(),
    }
  }
}

pub fn encode_namespace_root(request: &NamespaceRootWriteV1, hash_algorithm: HashAlgorithm) -> FormatResult<EncodedNamespaceRootV1> {
  validate_capabilities(&request.required_capabilities, "namespace root")?;
  let hash_width = hash_algorithm.hash_length();
  require_nonzero_hash(&request.namespace_tree_root, hash_width, "namespace root tree edge", "namespace_root_zero_edge")?;
  require_nonzero_hash(&request.semantic_state_root, hash_width, "namespace root semantic edge", "namespace_root_zero_edge")?;

  let body_length = checked_add(72, checked_mul(2, hash_width, "namespace root hashes")?, "namespace root body")?;
  let total_length = checked_add(DIRECTORY_HEADER_LENGTH, body_length, "directory envelope")?
    .checked_add(4)
    .ok_or_else(|| length_error("directory total length overflow"))?;
  let mut value = vec![0u8; total_length];
  value[..4].copy_from_slice(b"ADIR");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, DIRECTORY_KIND_NAMESPACE_ROOT);
  put_u16(&mut value, 8, DIRECTORY_HEADER_LENGTH as u16);
  put_u32(&mut value, 12, checked_u32(total_length, "directory total length")?);
  put_u32(&mut value, 16, checked_u32(body_length, "directory body length")?);

  let body = &mut value[DIRECTORY_HEADER_LENGTH..DIRECTORY_HEADER_LENGTH + body_length];
  body[4..36].copy_from_slice(&request.required_capabilities);
  put_u16(body, 36, 1);
  put_u16(body, 38, 1);
  body[40..40 + hash_width].copy_from_slice(&request.namespace_tree_root);
  body[40 + hash_width..40 + 2 * hash_width].copy_from_slice(&request.semantic_state_root);
  write_trailing_crc(&mut value);

  let root_hash = immutable_id(hash_algorithm, b"aeordb.directory-index.immutable.v1\0", DIRECTORY_KIND_NAMESPACE_ROOT, &value);
  let decoded = decode_namespace_root(&value, hash_algorithm)?;
  if decoded.root_hash != root_hash {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "namespace_root_encode_roundtrip",
      "encoded namespace root did not round-trip its identity",
    ));
  }
  Ok(EncodedNamespaceRootV1 { root_hash, value })
}

pub fn encode_semantic_state_object(
  request: &SemanticStateWriteV1,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<EncodedSemanticObjectV1> {
  validate_capabilities(&request.required_capabilities, "semantic state")?;
  let hash_width = hash_algorithm.hash_length();
  let body_length = checked_add(112, checked_mul(3, hash_width, "semantic state hashes")?, "semantic state body")?;
  let total_length = checked_add(SEMANTIC_HEADER_LENGTH, body_length, "semantic state envelope")?
    .checked_add(4)
    .ok_or_else(|| length_error("semantic state total length overflow"))?;
  if total_length > 4_096 {
    return Err(error(
      MalformedInputClass::AllocationAmplification,
      "semantic_state_exceeds_cap",
      format!("{total_length} bytes exceeds 4096"),
    ));
  }

  let mut item_count = 0u64;
  let mut body = vec![0u8; body_length];
  body[4..36].copy_from_slice(&request.required_capabilities);
  put_u16(&mut body, 36, 1);
  put_u16(&mut body, 38, 1);
  put_u16(&mut body, 40, 1);
  match &request.availability {
    SemanticAvailabilityV1::Complete {
      compiler_fingerprint,
      semantic_registry_fingerprint,
      catalog_root,
      catalog_record_count,
      catalog_node_count,
      definition_count,
      dependency_count,
    } => {
      require_nonzero_hash(compiler_fingerprint, hash_width, "semantic compiler fingerprint", "semantic_state_hash_width")?;
      require_nonzero_hash(semantic_registry_fingerprint, hash_width, "semantic registry fingerprint", "semantic_state_hash_width")?;
      require_nonzero_hash(catalog_root, hash_width, "semantic catalog root", "semantic_state_hash_width")?;
      if *catalog_record_count == 0 || *catalog_node_count == 0 {
        return Err(error(
          MalformedInputClass::CrossRecordClosureMismatch,
          "semantic_state_complete_invariant",
          "complete semantic state requires nonzero catalog record and node counts",
        ));
      }
      body[44] = 1;
      let hashes_offset = 48;
      body[hashes_offset..hashes_offset + hash_width].copy_from_slice(compiler_fingerprint);
      body[hashes_offset + hash_width..hashes_offset + 2 * hash_width].copy_from_slice(semantic_registry_fingerprint);
      body[hashes_offset + 2 * hash_width..hashes_offset + 3 * hash_width].copy_from_slice(catalog_root);
      let counts_offset = hashes_offset + 3 * hash_width;
      put_u64(&mut body, counts_offset, *catalog_record_count);
      put_u64(&mut body, counts_offset + 8, *catalog_node_count);
      put_u64(&mut body, counts_offset + 16, *definition_count);
      put_u64(&mut body, counts_offset + 24, *dependency_count);
      item_count = *catalog_record_count;
    }
    SemanticAvailabilityV1::ContentOnly { reason } => {
      put_u32(&mut body, 0, 1);
      put_u16(&mut body, 42, *reason as u16);
    }
  }

  let mut value = vec![0u8; total_length];
  value[..4].copy_from_slice(b"ASEM");
  put_u16(&mut value, 4, 1);
  put_u16(&mut value, 6, 1);
  put_u16(&mut value, 8, SEMANTIC_HEADER_LENGTH as u16);
  put_u32(&mut value, 12, checked_u32(total_length, "semantic total length")?);
  put_u32(&mut value, 16, checked_u32(body_length, "semantic body length")?);
  put_u64(&mut value, 20, item_count);
  value[SEMANTIC_HEADER_LENGTH..SEMANTIC_HEADER_LENGTH + body_length].copy_from_slice(&body);
  write_trailing_crc(&mut value);

  let object_id = immutable_id(hash_algorithm, b"aeordb.semantic-object.immutable.v1\0", 1, &value);
  let decoded = decode_semantic_object(&value, hash_algorithm)?;
  if decoded.object_id != object_id || decoded.semantic_state.is_none() {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "semantic_state_encode_roundtrip",
      "encoded semantic state did not round-trip its identity",
    ));
  }
  Ok(EncodedSemanticObjectV1 { object_id, value })
}

pub fn decode_namespace_root_entity(
  entity_bytes: &[u8],
  hash_algorithm: HashAlgorithm,
  write_sequence_high_water: u64,
) -> FormatResult<NamespaceRootV1> {
  let entity = decode_whole_entity(entity_bytes, hash_algorithm, write_sequence_high_water)?;
  if entity.entry_type != EntryTypeV4::DirectoryIndex || entity.entity_version != 1 {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "namespace_root_entity_type",
      format!("expected DirectoryIndex entity version 1, got {:?} version {}", entity.entry_type, entity.entity_version),
    ));
  }
  if entity.flags != WHOLE_ENTITY_V1_FLAG_SYSTEM || entity.compression_algorithm != CompressionAlgorithm::None {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "namespace_root_entity_representation",
      format!("flags {:#04x}, compression {:?}", entity.flags, entity.compression_algorithm),
    ));
  }
  let root = decode_namespace_root(entity.stored_value, hash_algorithm)?;
  if entity.key != root.root_hash {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "namespace_root_entity_key",
      "outer entity key does not match the immutable NamespaceRoot identity",
    ));
  }
  Ok(root)
}

pub fn decode_namespace_tree_root_v0(
  entity_bytes: &[u8],
  expected_root_hash: &[u8],
  hash_algorithm: HashAlgorithm,
  write_sequence_high_water: u64,
) -> FormatResult<NamespaceTreeRootV0> {
  if expected_root_hash.len() != hash_algorithm.hash_length() {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "namespace_tree_hash_width",
      format!("expected {}, got {}", hash_algorithm.hash_length(), expected_root_hash.len()),
    ));
  }
  let entity = decode_whole_entity(entity_bytes, hash_algorithm, write_sequence_high_water)?;
  if entity.entry_type != EntryTypeV4::DirectoryIndex || entity.entity_version != 0 {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "namespace_tree_entity_type",
      format!("expected DirectoryIndex entity version 0, got {:?} version {}", entity.entry_type, entity.entity_version),
    ));
  }
  if entity.flags != 0 || entity.compression_algorithm != CompressionAlgorithm::None {
    return Err(error(
      MalformedInputClass::CrossRecordClosureMismatch,
      "namespace_tree_entity_representation",
      format!("flags {:#04x}, compression {:?}", entity.flags, entity.compression_algorithm),
    ));
  }
  if entity.key != expected_root_hash {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "namespace_tree_entity_key",
      "outer entity key does not match the NamespaceRoot tree edge",
    ));
  }

  let (identity_domain, layout, edges) = if entity.stored_value.is_empty() {
    (b"dirc:".as_slice(), NamespaceTreeLayoutV0::Empty, Vec::new())
  } else if is_btree_format(entity.stored_value) {
    let node = BTreeNode::deserialize(entity.stored_value, hash_algorithm.hash_length(), 0)
      .map_err(|source| tree_payload_error(source.to_string()))?;
    let canonical = node.serialize(hash_algorithm.hash_length()).map_err(|source| tree_payload_error(source.to_string()))?;
    if canonical != entity.stored_value {
      return Err(tree_payload_error("B-tree root is not canonically encoded"));
    }
    let (layout, graph_edges) = match node {
      BTreeNode::Leaf(leaf) => {
        let edges = validated_child_edges(leaf.entries.iter(), hash_algorithm.hash_length())?;
        (NamespaceTreeLayoutV0::BTreeLeaf { child_count: leaf.entries.len() }, edges)
      }
      BTreeNode::Internal(internal) => {
        validate_strict_names(&internal.keys)?;
        if internal.children.iter().any(|child| child.len() != hash_algorithm.hash_length() || all_zero(child)) {
          return Err(tree_payload_error("B-tree root contains an invalid child edge"));
        }
        let edges: Vec<_> = internal.children.into_iter().map(|identity| NamespaceTreeEdgeV0::BTreeNode { identity }).collect();
        (NamespaceTreeLayoutV0::BTreeInternal { separator_count: internal.keys.len(), child_count: edges.len() }, edges)
      }
    };
    (b"btree:".as_slice(), layout, graph_edges)
  } else {
    let children = deserialize_child_entries(entity.stored_value, hash_algorithm.hash_length(), 0)
      .map_err(|source| tree_payload_error(source.to_string()))?;
    let edges = validated_child_edges(children.iter(), hash_algorithm.hash_length())?;
    (b"dirc:".as_slice(), NamespaceTreeLayoutV0::Flat { child_count: children.len() }, edges)
  };
  let computed = digest_parts(hash_algorithm, &[identity_domain, entity.stored_value]);
  if computed != expected_root_hash {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      "namespace_tree_content_identity",
      "namespace tree bytes do not match their typed content identity",
    ));
  }
  Ok(NamespaceTreeRootV0 { root_hash: expected_root_hash.to_vec(), layout, edges })
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
  let namespace_tree_codec = u16_at(body, 36);
  let semantic_state_codec = u16_at(body, 38);
  if namespace_tree_codec != 1 || semantic_state_codec != 1 {
    return Err(error(
      MalformedInputClass::UnknownMagicOrVersion,
      "namespace_root_schema",
      format!("namespace {namespace_tree_codec}, semantic {semantic_state_codec}"),
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
    namespace_tree_codec,
    semantic_state_codec,
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
  let (kind, graph_edges, state_fields) = match kind_id {
    0x0001 => {
      let (availability, required_capabilities, graph_edges) = decode_semantic_state(body, item_count, hash_algorithm)?;
      let summary = match &availability {
        SemanticAvailabilityV1::Complete { .. } => SemanticObjectKind::State { content_only_reason: None },
        SemanticAvailabilityV1::ContentOnly { reason } => SemanticObjectKind::State { content_only_reason: Some(*reason as u16) },
      };
      (summary, graph_edges, Some((required_capabilities, availability)))
    }
    0x0002 => {
      let (kind, graph_edges) = decode_catalog_leaf(body, item_count, hash_algorithm)?;
      (kind, graph_edges, None)
    }
    0x0003 => {
      let (kind, graph_edges) = decode_catalog_internal(body, item_count, hash_algorithm)?;
      (kind, graph_edges, None)
    }
    0x0004 => {
      let (kind, graph_edges) = decode_definition(body, item_count, hash_algorithm)?;
      (kind, graph_edges, None)
    }
    _ => {
      return Err(error(MalformedInputClass::UnknownTypeKindOrEnum, "semantic_kind", format!("unknown semantic kind {kind_id:#06x}")));
    }
  };
  let object_id = immutable_id(hash_algorithm, b"aeordb.semantic-object.immutable.v1\0", kind_id, value);
  let semantic_state = state_fields.map(|(required_capabilities, availability)| SemanticStateV1 {
    object_id: object_id.clone(),
    required_capabilities,
    semantic_catalog_codec: 1,
    semantic_definition_codec: 1,
    compiler_profile_version: 1,
    availability,
  });
  Ok(SemanticObjectV1 { object_id, kind_id, kind, graph_edges, semantic_state })
}

pub fn decode_semantic_definition_record(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<SemanticDefinitionRecordV1<'_>> {
  let object = decode_semantic_object(value, hash_algorithm)?;
  let SemanticObjectKind::Definition { class } = object.kind else {
    return Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "semantic_definition_expected",
      "semantic object is not a definition record",
    ));
  };
  let body = semantic_object_body(value)?;
  let hash_width = hash_algorithm.hash_length();
  let definition_start = checked_add(16, hash_width, "semantic definition payload start")?;
  Ok(SemanticDefinitionRecordV1 {
    object_id: object.object_id,
    class,
    semantic_id: &body[8..8 + hash_width],
    definition: &body[definition_start..],
  })
}

pub fn decode_semantic_catalog_node(value: &[u8], hash_algorithm: HashAlgorithm) -> FormatResult<SemanticCatalogNodeV1<'_>> {
  let object = decode_semantic_object(value, hash_algorithm)?;
  let body = semantic_object_body(value)?;
  let hash_width = hash_algorithm.hash_length();
  match object.kind {
    SemanticObjectKind::CatalogLeaf { record_count } => {
      let prefix_length = checked_add(16, hash_width, "catalog leaf view prefix")?;
      Ok(SemanticCatalogNodeV1::Leaf(SemanticCatalogLeafV1 {
        object_id: object.object_id,
        lookup_digest: &body[8..8 + hash_width],
        records: &body[prefix_length..],
        record_count,
        hash_width,
      }))
    }
    SemanticObjectKind::CatalogInternal { child_count } => {
      let depth = u16_at(body, 4);
      let prefix_length = usize::from(u16_at(body, 6));
      let children_start = checked_add(20, prefix_length, "catalog internal view children")?;
      Ok(SemanticCatalogNodeV1::Internal(SemanticCatalogInternalV1 {
        object_id: object.object_id,
        depth,
        prefix: &body[20..children_start],
        children: &body[children_start..],
        child_count,
        subtree_record_count: u64_at(body, 12),
        hash_width,
      }))
    }
    _ => Err(error(
      MalformedInputClass::UnknownTypeKindOrEnum,
      "semantic_catalog_expected",
      "semantic object is not a catalog leaf or internal node",
    )),
  }
}

fn semantic_object_body(value: &[u8]) -> FormatResult<&[u8]> {
  let body_end = value.len().checked_sub(4).ok_or_else(|| length_error("semantic object body end underflow"))?;
  if body_end < SEMANTIC_HEADER_LENGTH {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "semantic_object_body_truncated",
      "semantic object has no complete body",
    ));
  }
  Ok(&value[SEMANTIC_HEADER_LENGTH..body_end])
}

fn decode_catalog_record_view(bytes: &[u8], cursor: usize, hash_width: usize) -> FormatResult<(SemanticCatalogRecordV1<'_>, usize)> {
  let hash_bytes = checked_mul(2, hash_width, "catalog record view hashes")?;
  let prefix_length = checked_add(8, hash_bytes, "catalog record view prefix")?;
  let available = bytes.len().checked_sub(cursor).ok_or_else(|| length_error("catalog record view cursor"))?;
  if available < prefix_length {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "catalog_record_view_truncated", "catalog record view is truncated"));
  }
  let owner_length = u32_at(bytes, cursor + 4) as usize;
  let record_length = prefix_length.checked_add(owner_length).ok_or_else(|| length_error("catalog record view length"))?;
  let end = cursor.checked_add(record_length).ok_or_else(|| length_error("catalog record view end"))?;
  if end > bytes.len() {
    return Err(error(
      MalformedInputClass::TruncationOrTrailingBytes,
      "catalog_record_view_truncated",
      "catalog record owner key is truncated",
    ));
  }
  Ok((
    SemanticCatalogRecordV1 {
      record_kind: u16_at(bytes, cursor),
      semantic_id: &bytes[cursor + 8..cursor + 8 + hash_width],
      definition_object_id: &bytes[cursor + 8 + hash_width..cursor + prefix_length],
      owner_key: &bytes[cursor + prefix_length..end],
    },
    end,
  ))
}

fn decode_catalog_child_view(bytes: &[u8], cursor: usize, hash_width: usize) -> FormatResult<(SemanticCatalogChildV1<'_>, usize)> {
  let child_length = checked_add(12, hash_width, "catalog child view length")?;
  let end = cursor.checked_add(child_length).ok_or_else(|| length_error("catalog child view end"))?;
  if end > bytes.len() {
    return Err(error(MalformedInputClass::TruncationOrTrailingBytes, "catalog_child_view_truncated", "catalog child view is truncated"));
  }
  Ok((SemanticCatalogChildV1 { edge: bytes[cursor], record_count: u64_at(bytes, cursor + 4), object_id: &bytes[cursor + 12..end] }, end))
}

fn decode_semantic_state(
  body: &[u8],
  item_count: u64,
  hash_algorithm: HashAlgorithm,
) -> FormatResult<(SemanticAvailabilityV1, [u8; 32], Vec<Vec<u8>>)> {
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
    let compiler_fingerprint = hashes[..hash_width].to_vec();
    let semantic_registry_fingerprint = hashes[hash_width..2 * hash_width].to_vec();
    let catalog_root = hashes[2 * hash_width..3 * hash_width].to_vec();
    Ok((
      SemanticAvailabilityV1::Complete {
        compiler_fingerprint,
        semantic_registry_fingerprint,
        catalog_root: catalog_root.clone(),
        catalog_record_count: counts[0],
        catalog_node_count: counts[1],
        definition_count: counts[2],
        dependency_count: counts[3],
      },
      capabilities,
      vec![catalog_root],
    ))
  } else {
    let reason = SemanticUnavailableReasonV1::from_u16(reason)
      .ok_or_else(|| error(MalformedInputClass::UnknownTypeKindOrEnum, "semantic_state_unavailable_reason", format!("reason {reason}")))?;
    if catalog_present || hashes.iter().any(|byte| *byte != 0) || counts.iter().any(|count| *count != 0) || item_count != 0 {
      return Err(error(
        MalformedInputClass::NoncanonicalBooleanOrOptionalPresence,
        "semantic_state_content_only_invariant",
        "content-only semantic state has forbidden authority",
      ));
    }
    Ok((SemanticAvailabilityV1::ContentOnly { reason }, capabilities, Vec::new()))
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
    validate_catalog_owner_key(kind, owner_key, hash_width)?;
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

fn validate_catalog_owner_key(kind: u16, owner_key: &[u8], hash_width: usize) -> FormatResult<()> {
  match kind {
    1 | 2 => {
      if owner_key.len() < 3 || owner_key.len() > 65_537 {
        return Err(error(
          MalformedInputClass::IdentityKeyOrGenerationMismatch,
          "catalog_leaf_owner_key",
          "control projection owner key has an invalid length",
        ));
      }
      let control_kind = u16::from_le_bytes([owner_key[0], owner_key[1]]);
      let path = std::str::from_utf8(&owner_key[2..]).map_err(|error| {
        FormatError::new(
          MalformedInputClass::IdentityKeyOrGenerationMismatch,
          "catalog_leaf_owner_key",
          format!("control projection owner path is not UTF-8: {error}"),
        )
      })?;
      if control_kind == 0 {
        return Err(error(
          MalformedInputClass::IdentityKeyOrGenerationMismatch,
          "catalog_leaf_owner_key",
          "control projection owner requires a nonzero kind",
        ));
      }
      validate_canonical_absolute_path(path).map_err(|source| {
        FormatError::new(
          MalformedInputClass::IdentityKeyOrGenerationMismatch,
          "catalog_leaf_owner_key",
          format!("control projection owner path is not canonical: {source}"),
        )
      })?;
    }
    3..=7 if owner_key.len() == hash_width => {}
    3..=7 => {
      return Err(error(
        MalformedInputClass::IdentityKeyOrGenerationMismatch,
        "catalog_leaf_owner_key",
        format!("semantic definition owner has {} bytes, expected {hash_width}", owner_key.len()),
      ));
    }
    _ => {
      return Err(error(
        MalformedInputClass::UnknownTypeKindOrEnum,
        "catalog_leaf_owner_key",
        format!("semantic catalog class {kind} is not registered"),
      ));
    }
  }
  Ok(())
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

fn require_nonzero_hash(bytes: &[u8], expected: usize, context: &'static str, code: &'static str) -> FormatResult<()> {
  if bytes.len() != expected {
    return Err(error(
      MalformedInputClass::IdentityKeyOrGenerationMismatch,
      code,
      format!("{context} has width {}, expected {expected}", bytes.len()),
    ));
  }
  if all_zero(bytes) {
    return Err(error(MalformedInputClass::InvalidGraphEdgeOrCycle, code, format!("{context} is zero")));
  }
  Ok(())
}

fn write_trailing_crc(value: &mut [u8]) {
  let checksum_offset = value.len() - 4;
  let checksum = crc32fast::hash(&value[..checksum_offset]);
  value[checksum_offset..].copy_from_slice(&checksum.to_le_bytes());
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

fn validated_child_edges<'a>(children: impl Iterator<Item = &'a ChildEntry>, hash_width: usize) -> FormatResult<Vec<NamespaceTreeEdgeV0>> {
  let mut previous = None;
  let mut edges = Vec::new();
  for child in children {
    if child.name.is_empty() || previous.is_some_and(|value| value >= child.name.as_str()) {
      return Err(tree_payload_error("directory children are empty, duplicate, or out of order"));
    }
    if child.hash.len() != hash_width || all_zero(&child.hash) {
      return Err(tree_payload_error("directory child contains an invalid content edge"));
    }
    let entry_type = EntryType::from_u8(child.entry_type).map_err(|source| tree_payload_error(source.to_string()))?;
    edges.push(NamespaceTreeEdgeV0::Entry { name: child.name.clone(), entry_type, identity: child.hash.clone() });
    previous = Some(child.name.as_str());
  }
  Ok(edges)
}

fn validate_strict_names(names: &[String]) -> FormatResult<()> {
  let mut previous = None;
  for name in names {
    if name.is_empty() || previous.is_some_and(|value: &String| value >= name) {
      return Err(tree_payload_error("B-tree separators are empty, duplicate, or out of order"));
    }
    previous = Some(name);
  }
  Ok(())
}

fn tree_payload_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::CrossRecordClosureMismatch, "namespace_tree_payload", context)
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

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
  bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
  bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
  bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn checked_u32(value: usize, context: &'static str) -> FormatResult<u32> {
  if value > u32::MAX as usize {
    return Err(error(
      MalformedInputClass::LengthCountOrArithmeticOverflow,
      "namespace_writer_length_overflow",
      format!("{context} exceeds u32"),
    ));
  }
  Ok(value as u32)
}

fn length_error(context: impl Into<String>) -> FormatError {
  error(MalformedInputClass::LengthCountOrArithmeticOverflow, "length_overflow", context)
}

fn error(class: MalformedInputClass, code: &'static str, context: impl Into<String>) -> FormatError {
  FormatError::new(class, code, context)
}
