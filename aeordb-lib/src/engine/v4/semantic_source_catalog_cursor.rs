//! Bounded source-catalog projection and one-pass ordered cursor.
use super::*;
use crate::engine::btree::{BTreeNode, InternalNode, LeafNode};
use crate::engine::v4::namespace_seek::{child_bounds, LoadedNamespaceSeekNodeV1};
use crate::engine::v4::semantic_source_capture::decode_semantic_source_node_v1;

pub(super) fn load_catalog_node(
  operation: &CatalogReadOperationV1<'_, '_>,
  hash: &[u8],
  lower: Option<&str>,
  upper: Option<&str>,
) -> Result<LoadedNamespaceSeekNodeV1, SemanticMutationObservationErrorV1> {
  operation.check()?;
  operation.lookup.charge_work().map_err(catalog_read_error)?;
  let header = &operation.capture.header.selected.header;
  let scratch = 4 * SystemControlKindV1::SemanticSourceNode.encoded_cap() as u64 + 4 * FIRST_AUTHORITY_CONTROL_ENTITY_CAP as u64 + 65_536;
  let mut memory = operation.capture.memory.reserve(MemoryOwner::Task, scratch, AdmissionClass::Maintenance)?;
  let loaded = load_immutable_system_control_file(
    &operation.capture._protection.publisher().file,
    &operation.lookup,
    header,
    SystemControlKindV1::SemanticSourceNode,
    hash,
  )
  .map_err(catalog_read_error)?
  .ok_or_else(|| invalid("semantic_source_catalog_node_missing", "source catalog references a missing node"))?;
  let view = decode_semantic_source_node_v1(&loaded.bytes, operation.algorithm())?;
  let node = if let Some(rows) = view.leaf_entries() {
    let mut entries = Vec::new();
    entries
      .try_reserve_exact(256)
      .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_catalog_allocation", source })?;
    for row in rows {
      let row = row?;
      validate_range(row.path, lower, upper, false)?;
      protected_sources::validate_source_path(row.path, operation.algorithm())?;
      let hash = match row.file_record_id {
        Some(hash) => copy_bytes(hash)?,
        None => copy_bytes(&[0; 64][..operation.algorithm().hash_length()])?,
      };
      entries.push(ChildEntry {
        entry_type: crate::engine::EntryType::FileRecord.to_u8(),
        hash,
        total_size: 0,
        created_at: 0,
        updated_at: 0,
        name: copy_path(row.path)?,
        content_type: None,
        virtual_time: 0,
        node_id: 0,
      });
    }
    BTreeNode::Leaf(LeafNode { entries })
  } else {
    let mut keys = Vec::new();
    let mut children = Vec::new();
    keys
      .try_reserve_exact(128)
      .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_catalog_allocation", source })?;
    children
      .try_reserve_exact(129)
      .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_catalog_allocation", source })?;
    for child in view.children().ok_or_else(|| invalid("semantic_source_catalog_shape", "source catalog has no node projection"))? {
      let child = child?;
      if let Some(separator) = child.separator {
        validate_range(separator, lower, upper, true)?;
        keys.push(copy_path(separator)?);
      }
      children.push(copy_bytes(child.node_id)?);
    }
    BTreeNode::Internal(InternalNode { keys, children })
  };
  // The raw control and its decode scratch die here; retain actual projection
  // capacity only, not a maximum-size body reservation at every ancestor.
  let retained = match &node {
    BTreeNode::Leaf(leaf) => {
      leaf.entries.capacity() * std::mem::size_of::<ChildEntry>()
        + leaf.entries.iter().map(|entry| entry.name.capacity() + entry.hash.capacity()).sum::<usize>()
    }
    BTreeNode::Internal(internal) => {
      internal.keys.capacity() * std::mem::size_of::<String>()
        + internal.children.capacity() * std::mem::size_of::<Vec<u8>>()
        + internal.keys.iter().map(String::capacity).sum::<usize>()
        + internal.children.iter().map(Vec::capacity).sum::<usize>()
    }
  } as u64
    + 256;
  drop(loaded);
  memory.shrink(
    scratch
      .checked_sub(retained)
      .ok_or_else(|| invalid("semantic_source_catalog_memory", "source projection exceeded admitted scratch"))?,
  )?;
  operation.check()?;
  memory.check_admission()?;
  Ok(LoadedNamespaceSeekNodeV1 { node, _memory: memory })
}

fn validate_range(path: &str, lower: Option<&str>, upper: Option<&str>, separator: bool) -> Result<(), SemanticMutationObservationErrorV1> {
  if lower.is_some_and(|lower| path < lower || (separator && path == lower)) || upper.is_some_and(|upper| path >= upper) {
    return Err(invalid("semantic_source_catalog_range", "source catalog key violates its inherited half-open range"));
  }
  Ok(())
}

struct SourceCatalogFrameV1 {
  hash: Vec<u8>,
  node: InternalNode,
  next_child: usize,
  lower: Option<String>,
  upper: Option<String>,
  _memory: MemoryReservation,
}

pub(super) struct SourceCatalogCursorV1 {
  stack: Vec<SourceCatalogFrameV1>,
  leaf: Option<(std::vec::IntoIter<ChildEntry>, MemoryReservation)>,
  next_node: Option<(Vec<u8>, Option<String>, Option<String>)>,
  previous: Option<String>,
  pub(super) nodes: u64,
  maximum_depth: usize,
}

impl SourceCatalogCursorV1 {
  pub(super) fn new(root: &[u8], maximum_depth: usize) -> Result<Self, SemanticMutationObservationErrorV1> {
    let mut stack = Vec::new();
    stack
      .try_reserve_exact(maximum_depth)
      .map_err(|source| SemanticMutationObservationErrorV1::Allocation { code: "semantic_source_catalog_allocation", source })?;
    Ok(Self { stack, leaf: None, next_node: Some((copy_bytes(root)?, None, None)), previous: None, nodes: 0, maximum_depth })
  }

  pub(super) fn next_row(
    &mut self,
    operation: &CatalogReadOperationV1<'_, '_>,
  ) -> Result<Option<ChildEntry>, SemanticMutationObservationErrorV1> {
    self.advance(
      |row| {
        operation.check()?;
        if row {
          operation.lookup.charge_work().map_err(catalog_read_error)?;
        }
        Ok(())
      },
      |hash, lower, upper| load_catalog_node(operation, hash, lower, upper),
    )
  }

  // The cursor state machine is independent of physical loading. Model tests
  // can exercise cycles without pretending that invalid content hashes form
  // a valid native content-addressed cycle.
  fn advance(
    &mut self,
    mut admit: impl FnMut(bool) -> Result<(), SemanticMutationObservationErrorV1>,
    mut load: impl FnMut(&[u8], Option<&str>, Option<&str>) -> Result<LoadedNamespaceSeekNodeV1, SemanticMutationObservationErrorV1>,
  ) -> Result<Option<ChildEntry>, SemanticMutationObservationErrorV1> {
    loop {
      admit(false)?;
      if let Some((leaf, memory)) = &mut self.leaf {
        memory.check_admission()?;
        if let Some(row) = leaf.next() {
          admit(true)?;
          if self.previous.as_ref().is_some_and(|previous| previous.as_str() >= row.name.as_str()) {
            return Err(invalid("semantic_source_catalog_order", "source catalog traversal is not strictly ordered"));
          }
          self.previous = Some(copy_path(&row.name)?);
          return Ok(Some(row));
        }
        self.leaf = None;
      }
      if self.next_node.is_none() {
        while let Some(parent) = self.stack.last_mut() {
          if parent.next_child == parent.node.children.len() {
            self.stack.pop();
            continue;
          }
          let index = parent.next_child;
          let (lower, upper) = child_bounds(&parent.node, index, parent.lower.clone(), parent.upper.clone())?;
          let hash = copy_bytes(&parent.node.children[index])?;
          parent.next_child += 1;
          self.next_node = Some((hash, lower, upper));
          break;
        }
      }
      let Some((hash, lower, upper)) = self.next_node.take() else {
        return Ok(None);
      };
      if self.stack.len() >= self.maximum_depth {
        return Err(NamespaceSeekFailureV1::Depth.into());
      }
      if self.stack.iter().any(|frame| frame.hash == hash) {
        return Err(NamespaceSeekFailureV1::Cycle.into());
      }
      let loaded = load(&hash, lower.as_deref(), upper.as_deref())?;
      self.nodes = self.nodes.checked_add(1).ok_or_else(|| invalid("semantic_source_catalog_counts", "source node count overflowed"))?;
      match loaded.node {
        BTreeNode::Leaf(leaf) => self.leaf = Some((leaf.entries.into_iter(), loaded._memory)),
        BTreeNode::Internal(node) => {
          self.stack.push(SourceCatalogFrameV1 { hash, node, next_child: 0, lower, upper, _memory: loaded._memory })
        }
      }
    }
  }
}

#[cfg(test)]
#[path = "../../../spec/engine/native_source_catalog_cursor_spec.rs"]
mod tests;
