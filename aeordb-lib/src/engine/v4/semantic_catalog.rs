//! Storage-neutral traversal and definition closure for immutable v4 semantic catalogs.

use std::error::Error;
use std::fmt;

use crate::engine::HashAlgorithm;

use super::namespace::{SemanticCatalogNodeV1, SemanticCatalogRecordV1, decode_semantic_catalog_node, decode_semantic_definition_record};

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
    let hash_width = self.hash_algorithm.hash_length();
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
    stack.push(CatalogWalkFrameV1::Visit { object_id: root, expected_prefix: Vec::new(), expected_records: 0 });
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
          let bytes = self.load_catalog_node(&object_id)?;
          let node = decode_semantic_catalog_node(&bytes, self.hash_algorithm)
            .map_err(|error| SemanticCatalogReadErrorV1::corrupt(error.code(), error.context()))?;
          if node.object_id() != object_id {
            return Err(SemanticCatalogReadErrorV1::corrupt(
              "semantic_catalog_identity",
              "semantic catalog node bytes do not match the requested object identity",
            ));
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
                visit_record(record)?;
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
    inspect(definition.definition)
  }

  fn load_catalog_node(&self, object_id: &[u8]) -> Result<Vec<u8>, SemanticCatalogReadErrorV1> {
    let leaf = self.objects.load_semantic_object(0x0002, object_id)?;
    let internal = self.objects.load_semantic_object(0x0003, object_id)?;
    match (leaf, internal) {
      (Some(bytes), None) | (None, Some(bytes)) => Ok(bytes),
      (None, None) => Err(SemanticCatalogReadErrorV1::corrupt(
        "semantic_catalog_missing",
        format!("semantic catalog node {} is absent", hex::encode(object_id)),
      )),
      (Some(_), Some(_)) => Err(SemanticCatalogReadErrorV1::corrupt(
        "semantic_catalog_ambiguous",
        format!("semantic catalog node {} exists under both registered kinds", hex::encode(object_id)),
      )),
    }
  }
}
