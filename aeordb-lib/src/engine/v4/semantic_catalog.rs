//! Storage-neutral traversal and definition closure for immutable v4 semantic catalogs.

use std::error::Error;
use std::fmt;

use crate::engine::HashAlgorithm;

use super::hash::try_digest_parts;
use super::namespace::{
  SemanticCatalogNodeV1, SemanticCatalogRecordV1, decode_semantic_catalog_node, decode_semantic_definition_record,
  validate_catalog_owner_key,
};
use super::reader::FormatError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticCatalogReadErrorClassV1 {
  Cancelled,
  Unavailable,
  ResourceLimit,
  Corrupt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticCatalogReadErrorV1 {
  class: SemanticCatalogReadErrorClassV1,
  code: &'static str,
  context: String,
}

impl SemanticCatalogReadErrorV1 {
  pub fn cancelled(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: SemanticCatalogReadErrorClassV1::Cancelled, code, context: context.into() }
  }

  pub fn unavailable(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: SemanticCatalogReadErrorClassV1::Unavailable, code, context: context.into() }
  }

  pub fn resource(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: SemanticCatalogReadErrorClassV1::ResourceLimit, code, context: context.into() }
  }

  pub fn corrupt(code: &'static str, context: impl Into<String>) -> Self {
    Self { class: SemanticCatalogReadErrorClassV1::Corrupt, code, context: context.into() }
  }

  pub const fn class(&self) -> SemanticCatalogReadErrorClassV1 {
    self.class
  }

  pub const fn code(&self) -> &'static str {
    self.code
  }

  pub fn context(&self) -> &str {
    &self.context
  }
}

impl fmt::Display for SemanticCatalogReadErrorV1 {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(formatter, "{}: {}", self.code, self.context)
  }
}

impl Error for SemanticCatalogReadErrorV1 {}

pub trait SemanticCatalogObjectSourceV1 {
  fn load_semantic_object(&self, kind_id: u16, object_id: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1>;
}

pub fn validate_semantic_definition_identity_v1(
  record: SemanticCatalogRecordV1<'_>,
  actual: &[u8],
) -> Result<(), SemanticCatalogReadErrorV1> {
  if actual != record.semantic_id || actual != record.owner_key {
    return Err(SemanticCatalogReadErrorV1::corrupt(
      "semantic_definition_identity",
      "decoded semantic definition identity disagrees with its semantic ID or owner key",
    ));
  }
  Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SemanticCatalogWalkStatsV1 {
  pub records: u64,
  pub nodes: u64,
  pub class_counts: [u64; 8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticCatalogTraversalBoundsV1 {
  expected_records: u64,
  expected_nodes: u64,
}

impl SemanticCatalogTraversalBoundsV1 {
  pub fn new(expected_records: u64, expected_nodes: u64) -> Result<Self, SemanticCatalogReadErrorV1> {
    if expected_records == 0 || expected_nodes == 0 {
      return Err(SemanticCatalogReadErrorV1::corrupt(
        "semantic_catalog_counts",
        "semantic catalog traversal requires nonzero exact record and node counts",
      ));
    }
    Ok(Self { expected_records, expected_nodes })
  }
}

#[derive(Clone, Debug)]
struct OwnedCatalogChildV1 {
  edge: u8,
  record_count: u64,
  object_id: Vec<u8>,
}

enum CatalogWalkFrameV1 {
  Visit { object_id: Vec<u8>, expected_prefix: Vec<u8>, expected_records: u64 },
  Children { prefix: Vec<u8>, children: std::vec::IntoIter<OwnedCatalogChildV1> },
}

pub struct SemanticCatalogReaderV1<'source> {
  hash_algorithm: HashAlgorithm,
  objects: &'source dyn SemanticCatalogObjectSourceV1,
}

impl<'source> SemanticCatalogReaderV1<'source> {
  pub const fn new(hash_algorithm: HashAlgorithm, objects: &'source dyn SemanticCatalogObjectSourceV1) -> Self {
    Self { hash_algorithm, objects }
  }

  pub fn walk_catalog(
    &self,
    catalog_root: &[u8],
    bounds: SemanticCatalogTraversalBoundsV1,
    is_cancelled: &dyn Fn() -> bool,
    mut visit_record: impl FnMut(SemanticCatalogRecordV1<'_>) -> Result<(), SemanticCatalogReadErrorV1>,
  ) -> Result<SemanticCatalogWalkStatsV1, SemanticCatalogReadErrorV1> {
    let mut source = BorrowedCatalogSource(self.objects);
    walk_semantic_catalog_with_mutable_source_v1(self.hash_algorithm, &mut source, catalog_root, bounds, is_cancelled, |record, _| {
      visit_record(record)
    })
  }
}

struct BorrowedCatalogSource<'a>(&'a dyn SemanticCatalogObjectSourceV1);

impl SemanticCatalogObjectSourceV1 for BorrowedCatalogSource<'_> {
  fn load_semantic_object(&self, kind: u16, object_id: &[u8]) -> Result<Option<Vec<u8>>, SemanticCatalogReadErrorV1> {
    self.0.load_semantic_object(kind, object_id)
  }
}

/// Walk one captured immutable tree while allowing its visitor to stage other
/// immutable objects. The visitor must not overwrite the captured closure.
/// The caller owns bounded traversal scratch admission and GC/snapshot pins;
/// this reader neither selects a root nor grants publication authority.
pub fn walk_semantic_catalog_with_mutable_source_v1<S: SemanticCatalogObjectSourceV1 + ?Sized>(
  hash_algorithm: HashAlgorithm,
  objects: &mut S,
  catalog_root: &[u8],
  bounds: SemanticCatalogTraversalBoundsV1,
  is_cancelled: &dyn Fn() -> bool,
  mut visit_record: impl FnMut(SemanticCatalogRecordV1<'_>, &mut S) -> Result<(), SemanticCatalogReadErrorV1>,
) -> Result<SemanticCatalogWalkStatsV1, SemanticCatalogReadErrorV1> {
  check_cancelled(is_cancelled)?;
  let hash_width = hash_algorithm.hash_length();
  if catalog_root.len() != hash_width || catalog_root.iter().all(|byte| *byte == 0) {
    return Err(SemanticCatalogReadErrorV1::corrupt(
      "semantic_catalog_root",
      "semantic catalog traversal requires one nonzero database-width root",
    ));
  }
  let mut root = Vec::new();
  root.try_reserve_exact(catalog_root.len()).map_err(|error| {
    SemanticCatalogReadErrorV1::resource("semantic_catalog_allocation", format!("catalog root allocation failed: {error}"))
  })?;
  root.extend_from_slice(catalog_root);
  let mut stack = Vec::new();
  stack.try_reserve_exact(hash_width.saturating_mul(2).saturating_add(1)).map_err(|error| {
    SemanticCatalogReadErrorV1::resource("semantic_catalog_allocation", format!("catalog stack allocation failed: {error}"))
  })?;
  stack.push(CatalogWalkFrameV1::Visit { object_id: root, expected_prefix: Vec::new(), expected_records: bounds.expected_records });
  let mut stats = SemanticCatalogWalkStatsV1::default();
  while let Some(frame) = stack.pop() {
    if is_cancelled() {
      return Err(SemanticCatalogReadErrorV1::cancelled("semantic_cancelled", "semantic catalog traversal was cancelled"));
    }
    if stack.len() > hash_width.saturating_mul(2) {
      return Err(SemanticCatalogReadErrorV1::corrupt(
        "semantic_catalog_depth",
        "semantic catalog traversal exceeded the database hash width",
      ));
    }
    match frame {
      CatalogWalkFrameV1::Visit { object_id, expected_prefix, expected_records } => {
        if stats.nodes >= bounds.expected_nodes {
          return Err(SemanticCatalogReadErrorV1::corrupt(
            "semantic_catalog_counts",
            "semantic catalog traversal exceeded its selected root's exact node count",
          ));
        }
        let bytes = load_catalog_node(objects, &object_id, is_cancelled)?;
        let node = decode_catalog_node(&bytes, hash_algorithm, &object_id)?;
        if stats.nodes == 0 {
          let records = match &node {
            SemanticCatalogNodeV1::Leaf(leaf) => u64::from(leaf.record_count()),
            SemanticCatalogNodeV1::Internal(internal) => internal.subtree_record_count(),
          };
          if records != bounds.expected_records {
            return Err(SemanticCatalogReadErrorV1::corrupt(
              "semantic_catalog_counts",
              "catalog root disagrees with its selected state's exact record count",
            ));
          }
        }
        stats.nodes = stats
          .nodes
          .checked_add(1)
          .ok_or_else(|| SemanticCatalogReadErrorV1::corrupt("semantic_catalog_count_overflow", "catalog node count overflow"))?;
        match node {
          SemanticCatalogNodeV1::Leaf(leaf) => {
            if !leaf.lookup_digest().starts_with(&expected_prefix)
              || (expected_records != 0 && u64::from(leaf.record_count()) != expected_records)
            {
              return Err(SemanticCatalogReadErrorV1::corrupt(
                "semantic_catalog_leaf_closure",
                "semantic catalog leaf disagrees with its parent prefix or record count",
              ));
            }
            for record in leaf.records() {
              if stats.records >= bounds.expected_records {
                return Err(SemanticCatalogReadErrorV1::corrupt(
                  "semantic_catalog_counts",
                  "semantic catalog traversal exceeded its selected root's exact record count",
                ));
              }
              let record = record.map_err(|error| SemanticCatalogReadErrorV1::corrupt(error.code(), error.context()))?;
              stats.records = stats
                .records
                .checked_add(1)
                .ok_or_else(|| SemanticCatalogReadErrorV1::corrupt("semantic_catalog_count_overflow", "catalog record count overflow"))?;
              let class = usize::from(record.record_kind);
              let class_count = stats.class_counts.get_mut(class).ok_or_else(|| {
                SemanticCatalogReadErrorV1::corrupt("semantic_catalog_record_kind", "catalog record kind exceeds the frozen registry")
              })?;
              *class_count = class_count
                .checked_add(1)
                .ok_or_else(|| SemanticCatalogReadErrorV1::corrupt("semantic_catalog_count_overflow", "catalog class count overflow"))?;
              visit_record(record, objects)?;
              check_cancelled(is_cancelled)?;
            }
          }
          SemanticCatalogNodeV1::Internal(internal) => {
            if usize::from(internal.depth()) != expected_prefix.len()
              || (expected_records != 0 && internal.subtree_record_count() != expected_records)
            {
              return Err(SemanticCatalogReadErrorV1::corrupt(
                "semantic_catalog_internal_closure",
                "semantic catalog internal node disagrees with its parent depth or record count",
              ));
            }
            let mut prefix = expected_prefix;
            prefix.try_reserve_exact(internal.prefix().len()).map_err(|error| {
              SemanticCatalogReadErrorV1::resource("semantic_catalog_allocation", format!("catalog prefix allocation failed: {error}"))
            })?;
            prefix.extend_from_slice(internal.prefix());
            let mut children = Vec::new();
            children.try_reserve_exact(usize::from(internal.child_count())).map_err(|error| {
              SemanticCatalogReadErrorV1::resource("semantic_catalog_allocation", format!("catalog child allocation failed: {error}"))
            })?;
            for child in internal.children() {
              let child = child.map_err(|error| SemanticCatalogReadErrorV1::corrupt(error.code(), error.context()))?;
              let mut object_id = Vec::new();
              object_id.try_reserve_exact(child.object_id.len()).map_err(|error| {
                SemanticCatalogReadErrorV1::resource(
                  "semantic_catalog_allocation",
                  format!("catalog child identity allocation failed: {error}"),
                )
              })?;
              object_id.extend_from_slice(child.object_id);
              children.push(OwnedCatalogChildV1 { edge: child.edge, record_count: child.record_count, object_id });
            }
            stack.push(CatalogWalkFrameV1::Children { prefix, children: children.into_iter() });
          }
        }
      }
      CatalogWalkFrameV1::Children { prefix, mut children } => {
        let Some(child) = children.next() else {
          continue;
        };
        let mut child_prefix = Vec::new();
        child_prefix.try_reserve_exact(prefix.len().saturating_add(1)).map_err(|error| {
          SemanticCatalogReadErrorV1::resource("semantic_catalog_allocation", format!("catalog child prefix allocation failed: {error}"))
        })?;
        child_prefix.extend_from_slice(&prefix);
        child_prefix.push(child.edge);
        if child_prefix.len() > hash_width {
          return Err(SemanticCatalogReadErrorV1::corrupt(
            "semantic_catalog_depth",
            "semantic catalog child prefix exceeds the database hash width",
          ));
        }
        stack.push(CatalogWalkFrameV1::Children { prefix, children });
        stack.push(CatalogWalkFrameV1::Visit {
          object_id: child.object_id,
          expected_prefix: child_prefix,
          expected_records: child.record_count,
        });
      }
    }
  }
  check_cancelled(is_cancelled)?;
  if stats.records != bounds.expected_records || stats.nodes != bounds.expected_nodes {
    return Err(SemanticCatalogReadErrorV1::corrupt(
      "semantic_catalog_counts",
      format!(
        "catalog walk observed {} records and {} nodes; expected {} and {}",
        stats.records, stats.nodes, bounds.expected_records, bounds.expected_nodes
      ),
    ));
  }
  Ok(stats)
}

impl SemanticCatalogReaderV1<'_> {
  /// Inspect an exact full key through at most H+1 catalog nodes. The supplied
  /// root/counts and untouched subtrees must already be admitted. A missing key
  /// does not certify the rest of the catalog. Only one node body and H-sized
  /// path metadata are retained; the caller admits this bounded read scratch
  /// and any data its callback retains, as for `with_definition`.
  pub fn with_record<T>(
    &self,
    catalog_root: &[u8],
    bounds: SemanticCatalogTraversalBoundsV1,
    record_kind: u16,
    owner_key: &[u8],
    is_cancelled: &dyn Fn() -> bool,
    inspect: impl FnOnce(SemanticCatalogRecordV1<'_>) -> Result<T, SemanticCatalogReadErrorV1>,
  ) -> Result<Option<T>, SemanticCatalogReadErrorV1> {
    self.with_record_ordinal(catalog_root, bounds, record_kind, owner_key, is_cancelled, |_, record| inspect(record))
  }

  /// Like `with_record`, also returning its zero-based canonical traversal
  /// ordinal. Counts/untouched subtrees require the same prior validation.
  /// This position belongs to this exact tree, never a persistent identity.
  pub fn with_record_ordinal<T>(
    &self,
    catalog_root: &[u8],
    bounds: SemanticCatalogTraversalBoundsV1,
    record_kind: u16,
    owner_key: &[u8],
    is_cancelled: &dyn Fn() -> bool,
    inspect: impl FnOnce(u64, SemanticCatalogRecordV1<'_>) -> Result<T, SemanticCatalogReadErrorV1>,
  ) -> Result<Option<T>, SemanticCatalogReadErrorV1> {
    check_cancelled(is_cancelled)?;
    let width = self.hash_algorithm.hash_length();
    if catalog_root.len() != width || catalog_root.iter().all(|byte| *byte == 0) {
      return Err(SemanticCatalogReadErrorV1::corrupt("semantic_catalog_root", "lookup requires a nonzero database-width root"));
    }
    validate_catalog_owner_key(record_kind, owner_key, width).map_err(format_error)?;
    if matches!(record_kind, 3..=7) && owner_key.iter().all(|byte| *byte == 0) {
      return Err(SemanticCatalogReadErrorV1::corrupt("semantic_catalog_owner", "lookup definition owner must be nonzero"));
    }
    let lookup = try_digest_parts(self.hash_algorithm, &[b"aeordb.semantic-catalog-key.v1\0", &record_kind.to_le_bytes(), owner_key])
      .map_err(|error| SemanticCatalogReadErrorV1::resource("semantic_catalog_allocation", error.to_string()))?;
    let mut identity = copy_lookup_bytes(catalog_root)?;
    let mut prefix = Vec::new();
    prefix
      .try_reserve_exact(width)
      .map_err(|error| SemanticCatalogReadErrorV1::resource("semantic_catalog_allocation", error.to_string()))?;
    let mut expected_records = bounds.expected_records;
    let mut visited = 0u64;
    let mut ordinal = 0u64;
    loop {
      check_cancelled(is_cancelled)?;
      if visited >= bounds.expected_nodes || visited > width as u64 {
        return Err(SemanticCatalogReadErrorV1::corrupt("semantic_catalog_depth", "lookup exceeds admitted node count or hash depth"));
      }
      visited += 1;
      let bytes = load_catalog_node(self.objects, &identity, is_cancelled)?;
      let node = decode_catalog_node(&bytes, self.hash_algorithm, &identity)?;
      match node {
        SemanticCatalogNodeV1::Leaf(leaf) => {
          if !leaf.lookup_digest().starts_with(&prefix) || u64::from(leaf.record_count()) != expected_records {
            return Err(SemanticCatalogReadErrorV1::corrupt("semantic_catalog_leaf_closure", "lookup leaf disagrees with parent closure"));
          }
          if leaf.lookup_digest() == lookup {
            for record in leaf.records() {
              check_cancelled(is_cancelled)?;
              let record = record.map_err(format_error)?;
              if record.record_kind == record_kind && record.owner_key == owner_key {
                let result = inspect(ordinal, record)?;
                check_cancelled(is_cancelled)?;
                return Ok(Some(result));
              }
              ordinal = checked_catalog_ordinal(ordinal, 1, bounds.expected_records)?;
            }
          }
          check_cancelled(is_cancelled)?;
          return Ok(None);
        }
        SemanticCatalogNodeV1::Internal(internal) => {
          let depth = usize::from(internal.depth());
          if depth != prefix.len() || internal.subtree_record_count() != expected_records {
            return Err(SemanticCatalogReadErrorV1::corrupt(
              "semantic_catalog_internal_closure",
              "lookup internal disagrees with parent closure",
            ));
          }
          let end = depth + internal.prefix().len();
          if lookup[depth..end] != *internal.prefix() {
            check_cancelled(is_cancelled)?;
            return Ok(None);
          }
          let mut selected = None;
          for child in internal.children() {
            let child = child.map_err(format_error)?;
            if child.edge == lookup[end] {
              selected = Some(child);
              break;
            }
            ordinal = checked_catalog_ordinal(ordinal, child.record_count, bounds.expected_records)?;
          }
          let Some(child) = selected else {
            check_cancelled(is_cancelled)?;
            return Ok(None);
          };
          identity = copy_lookup_bytes(child.object_id)?;
          expected_records = child.record_count;
          prefix.extend_from_slice(internal.prefix());
          prefix.push(child.edge);
        }
      }
    }
  }

  pub fn with_definition<T>(
    &self,
    record: SemanticCatalogRecordV1<'_>,
    is_cancelled: &dyn Fn() -> bool,
    inspect: impl FnOnce(&[u8]) -> Result<T, SemanticCatalogReadErrorV1>,
  ) -> Result<T, SemanticCatalogReadErrorV1> {
    if is_cancelled() {
      return Err(SemanticCatalogReadErrorV1::cancelled("semantic_cancelled", "semantic definition read was cancelled"));
    }
    let bytes = self.objects.load_semantic_object(0x0004, record.definition_object_id)?.ok_or_else(|| {
      SemanticCatalogReadErrorV1::corrupt(
        "semantic_definition_missing",
        format!("semantic definition {} is absent", hex::encode(record.definition_object_id)),
      )
    })?;
    if is_cancelled() {
      return Err(SemanticCatalogReadErrorV1::cancelled("semantic_cancelled", "semantic definition load was cancelled"));
    }
    let definition = decode_semantic_definition_record(&bytes, self.hash_algorithm)
      .map_err(|error| SemanticCatalogReadErrorV1::corrupt(error.code(), error.context()))?;
    if definition.object_id != record.definition_object_id
      || definition.class != record.record_kind
      || definition.semantic_id != record.semantic_id
    {
      return Err(SemanticCatalogReadErrorV1::corrupt(
        "semantic_definition_closure",
        "semantic definition identity, class, or semantic ID disagrees with its catalog binding",
      ));
    }
    if matches!(record.record_kind, 6 | 7) {
      // The decoder recomputes the complete, class-domain dependency ID using
      // the selected database algorithm. Public records may bypass leaf decode.
      validate_semantic_definition_identity_v1(record, definition.semantic_id)?;
    }
    if is_cancelled() {
      return Err(SemanticCatalogReadErrorV1::cancelled("semantic_cancelled", "semantic definition inspection was cancelled"));
    }
    let result = inspect(definition.definition)?;
    check_cancelled(is_cancelled)?;
    Ok(result)
  }
}

fn checked_catalog_ordinal(current: u64, count: u64, total: u64) -> Result<u64, SemanticCatalogReadErrorV1> {
  current
    .checked_add(count)
    .filter(|ordinal| *ordinal <= total)
    .ok_or_else(|| SemanticCatalogReadErrorV1::corrupt("semantic_catalog_ordinal", "record ordinal exceeds the exact catalog count"))
}

fn decode_catalog_node<'a>(
  bytes: &'a [u8],
  algorithm: HashAlgorithm,
  identity: &[u8],
) -> Result<SemanticCatalogNodeV1<'a>, SemanticCatalogReadErrorV1> {
  let node = decode_semantic_catalog_node(bytes, algorithm).map_err(format_error)?;
  if node.object_id() != identity {
    return Err(SemanticCatalogReadErrorV1::corrupt(
      "semantic_catalog_identity",
      "semantic catalog node bytes do not match the requested object identity",
    ));
  }
  Ok(node)
}

fn load_catalog_node<S: SemanticCatalogObjectSourceV1 + ?Sized>(
  objects: &S,
  object_id: &[u8],
  is_cancelled: &dyn Fn() -> bool,
) -> Result<Vec<u8>, SemanticCatalogReadErrorV1> {
  check_cancelled(is_cancelled)?;
  let leaf = objects.load_semantic_object(0x0002, object_id)?;
  check_cancelled(is_cancelled)?;
  let internal = objects.load_semantic_object(0x0003, object_id)?;
  check_cancelled(is_cancelled)?;
  let (kind, bytes) = match (leaf, internal) {
    (Some(bytes), None) => (2u16, bytes),
    (None, Some(bytes)) => (3u16, bytes),
    (None, None) => Err(SemanticCatalogReadErrorV1::corrupt(
      "semantic_catalog_missing",
      format!("semantic catalog node {} is absent", hex::encode(object_id)),
    ))?,
    (Some(_), Some(_)) => Err(SemanticCatalogReadErrorV1::corrupt(
      "semantic_catalog_ambiguous",
      format!("semantic catalog node {} exists under both registered kinds", hex::encode(object_id)),
    ))?,
  };
  let cap = if kind == 2 { 1_048_576 } else { 65_536 };
  if bytes.len() > cap || bytes.get(6..8) != Some(kind.to_le_bytes().as_slice()) {
    return Err(SemanticCatalogReadErrorV1::corrupt("semantic_catalog_kind", "catalog object violates its stored kind or size limit"));
  }
  Ok(bytes)
}

fn copy_lookup_bytes(bytes: &[u8]) -> Result<Vec<u8>, SemanticCatalogReadErrorV1> {
  let mut result = Vec::new();
  result
    .try_reserve_exact(bytes.len())
    .map_err(|error| SemanticCatalogReadErrorV1::resource("semantic_catalog_allocation", error.to_string()))?;
  result.extend_from_slice(bytes);
  Ok(result)
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<(), SemanticCatalogReadErrorV1> {
  if is_cancelled() {
    return Err(SemanticCatalogReadErrorV1::cancelled("semantic_cancelled", "semantic catalog operation was cancelled"));
  }
  Ok(())
}

fn format_error(error: FormatError) -> SemanticCatalogReadErrorV1 {
  if error.is_allocation_failure() {
    SemanticCatalogReadErrorV1::resource(error.code(), error.context())
  } else {
    SemanticCatalogReadErrorV1::corrupt(error.code(), error.context())
  }
}
