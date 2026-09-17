//! Shared ordered child selection over format-validated, accounted directories.
//! Physical reads, validation, cancellation and traversal-work charging belong
//! to the supplied loader; this module cannot acquire namespace authority.
use std::collections::TryReserveError;

use crate::engine::btree::{BTreeNode, InternalNode};
use crate::engine::directory_entry::ChildEntry;
use crate::engine::memory_coordinator::MemoryReservation;
use crate::engine::EntryType;

pub(super) struct LoadedNamespaceSeekNodeV1 {
  /// A validated flat directory is represented as a leaf only at its root.
  pub node: BTreeNode,
  pub _memory: MemoryReservation,
}

#[derive(Debug)]
pub(super) enum NamespaceSeekFailureV1 {
  Allocation(TryReserveError),
  Depth,
  Cycle,
  ParentShape,
  ChildIndex,
  InvalidChild(crate::engine::errors::EngineError),
  PathOverflow,
}

pub(super) struct NamespaceSeekFrameV1 {
  node_hash: Vec<u8>,
  child_index: usize,
  lower_bound: Option<String>,
  upper_bound: Option<String>,
}

/// Charge traversal-owned path/child copies and both inherited separator
/// strings per B-tree frame. Encoded names and content types are u16-sized;
/// a short requested path does not bound an unrelated separator's length.
/// Decoded directory nodes retain separate loader-owned reservations.
pub(super) fn namespace_seek_workspace_bytes_v1(
  maximum_path_bytes: u64,
  maximum_path_depth: u64,
  maximum_btree_depth: u64,
  hash_width: u64,
) -> Option<u64> {
  let name_bytes = u64::from(u16::MAX);
  let path_copies = maximum_path_depth.checked_add(8)?.checked_mul(4)?;
  let path_bytes = maximum_path_bytes.checked_mul(path_copies)?;
  let child_bytes = name_bytes
    .checked_mul(2)?
    .checked_add(hash_width)?
    .checked_add(std::mem::size_of::<ChildEntry>() as u64)?
    .checked_mul(maximum_path_depth.checked_add(8)?)?;
  let frame_bytes = (std::mem::size_of::<NamespaceSeekFrameV1>() as u64).checked_add(hash_width)?.checked_mul(maximum_btree_depth)?;
  let bound_bytes = maximum_btree_depth.checked_mul(2)?.checked_add(8)?.checked_mul(name_bytes)?;
  path_bytes.checked_add(child_bytes)?.checked_add(frame_bytes)?.checked_add(bound_bytes)?.checked_add(4096)
}

/// The loader validates inherited [lower, upper) ranges and rejects a flat
/// representation when `btree_child` is true. It checks cancellation/admission
/// and charges bounded work on every call. Keep its lease alive through decode
/// use; only the selected child and bounded ancestor frames escape each node.
pub(super) fn seek_namespace_child_v1<E: From<NamespaceSeekFailureV1>>(
  root_hash: &[u8],
  lower: &str,
  inclusive: bool,
  maximum_depth: usize,
  mut load: impl FnMut(&[u8], Option<&str>, Option<&str>, bool) -> Result<LoadedNamespaceSeekNodeV1, E>,
) -> Result<Option<ChildEntry>, E> {
  let mut stack: Vec<NamespaceSeekFrameV1> = Vec::new();
  stack.try_reserve_exact(maximum_depth).map_err(NamespaceSeekFailureV1::Allocation)?;
  let mut node_hash = root_hash.to_vec();
  let mut lower_bound = None;
  let mut upper_bound = None;
  loop {
    check_ancestry(&stack, &node_hash, maximum_depth)?;
    let loaded = load(&node_hash, lower_bound.as_deref(), upper_bound.as_deref(), !stack.is_empty())?;
    match loaded.node {
      BTreeNode::Leaf(leaf) => {
        let index = leaf.entries.partition_point(|entry| entry.name.as_str() < lower || (!inclusive && entry.name == lower));
        if let Some(entry) = leaf.entries.get(index) {
          return Ok(Some(entry.clone()));
        }
        break;
      }
      BTreeNode::Internal(internal) => {
        let child_index = internal.find_child_index(lower);
        let (child_lower, child_upper) = child_bounds(&internal, child_index, lower_bound.clone(), upper_bound.clone())?;
        stack.push(NamespaceSeekFrameV1 { node_hash, child_index, lower_bound, upper_bound });
        lower_bound = child_lower;
        upper_bound = child_upper;
        node_hash = internal.children[child_index].clone();
      }
    }
  }

  'ascend: while let Some(frame) = stack.pop() {
    let loaded = load(&frame.node_hash, frame.lower_bound.as_deref(), frame.upper_bound.as_deref(), !stack.is_empty())?;
    let BTreeNode::Internal(parent) = loaded.node else {
      return Err(NamespaceSeekFailureV1::ParentShape.into());
    };
    let next_child_index = frame.child_index.checked_add(1).ok_or(NamespaceSeekFailureV1::ChildIndex)?;
    if next_child_index >= parent.children.len() {
      continue;
    }
    let (child_lower, child_upper) = child_bounds(&parent, next_child_index, frame.lower_bound.clone(), frame.upper_bound.clone())?;
    node_hash = parent.children[next_child_index].clone();
    lower_bound = child_lower;
    upper_bound = child_upper;
    stack.push(NamespaceSeekFrameV1 {
      node_hash: frame.node_hash,
      child_index: next_child_index,
      lower_bound: frame.lower_bound,
      upper_bound: frame.upper_bound,
    });
    // Drop the decoded parent and its reservation before loading successors.
    drop(parent);
    drop(loaded._memory);
    loop {
      check_ancestry(&stack, &node_hash, maximum_depth)?;
      let loaded = load(&node_hash, lower_bound.as_deref(), upper_bound.as_deref(), true)?;
      match loaded.node {
        BTreeNode::Leaf(leaf) => match leaf.entries.first() {
          Some(entry) => return Ok(Some(entry.clone())),
          None => continue 'ascend,
        },
        BTreeNode::Internal(internal) => {
          let (child_lower, child_upper) = child_bounds(&internal, 0, lower_bound.clone(), upper_bound.clone())?;
          stack.push(NamespaceSeekFrameV1 { node_hash, child_index: 0, lower_bound, upper_bound });
          lower_bound = child_lower;
          upper_bound = child_upper;
          node_hash = internal.children[0].clone();
        }
      }
    }
  }
  Ok(None)
}

fn check_ancestry(stack: &[NamespaceSeekFrameV1], hash: &[u8], maximum_depth: usize) -> Result<(), NamespaceSeekFailureV1> {
  if stack.len() >= maximum_depth {
    return Err(NamespaceSeekFailureV1::Depth);
  }
  if stack.iter().any(|frame| frame.node_hash == hash) {
    return Err(NamespaceSeekFailureV1::Cycle);
  }
  Ok(())
}

fn child_bounds(
  node: &InternalNode,
  index: usize,
  inherited_lower: Option<String>,
  inherited_upper: Option<String>,
) -> Result<(Option<String>, Option<String>), NamespaceSeekFailureV1> {
  if node.children.len() != node.keys.len() + 1 || index >= node.children.len() {
    return Err(NamespaceSeekFailureV1::ChildIndex);
  }
  let lower = if index == 0 { inherited_lower } else { Some(node.keys[index - 1].clone()) };
  let upper = if index == node.keys.len() { inherited_upper } else { Some(node.keys[index].clone()) };
  Ok((lower, upper))
}

/// Directory components sort by `name + '/'`, files by `name`. Raw B-tree
/// keys remain unchanged. Prefix seeks recover directories whose raw name lies
/// before the lower key while their separator-suffixed path lies after it.
pub(super) fn next_namespace_child_by_path_v1<E: From<NamespaceSeekFailureV1>>(
  lower: Option<&str>,
  mut seek: impl FnMut(&str, bool) -> Result<Option<ChildEntry>, E>,
) -> Result<Option<(String, ChildEntry)>, E> {
  let mut best: Option<(String, ChildEntry)> = None;
  if let Some(lower) = lower {
    let component = lower.split('/').next().map_or("", |component| component);
    for (end, _) in component.char_indices().skip(1).chain(std::iter::once((component.len(), '\0'))) {
      let prefix = &component[..end];
      if prefix.is_empty() {
        continue;
      }
      if let Some(entry) = seek(prefix, true)?.filter(|entry| entry.name == prefix && entry.entry_type == EntryType::DirectoryIndex.to_u8())
      {
        let key = child_scan_key(&entry)?;
        if key.as_str() > lower && best.as_ref().is_none_or(|(best_key, _)| key < *best_key) {
          best = Some((key, entry));
        }
      }
    }
  }
  let mut raw_lower = match lower {
    Some(lower) => lower.to_string(),
    None => String::new(),
  };
  let mut inclusive = lower.is_none();
  loop {
    let Some(child) = seek(&raw_lower, inclusive)? else { break };
    if best.as_ref().is_some_and(|(best_key, _)| child.name.as_str() >= best_key.as_str()) {
      break;
    }
    let key = child_scan_key(&child)?;
    if lower.is_none_or(|lower| key.as_str() > lower) && best.as_ref().is_none_or(|(best_key, _)| key < *best_key) {
      best = Some((key, child.clone()));
    }
    if child.entry_type != EntryType::DirectoryIndex.to_u8() {
      break;
    }
    raw_lower = child.name;
    inclusive = false;
  }
  Ok(best)
}

fn child_scan_key(entry: &ChildEntry) -> Result<String, NamespaceSeekFailureV1> {
  let kind = EntryType::from_u8(entry.entry_type).map_err(NamespaceSeekFailureV1::InvalidChild)?;
  let suffix = usize::from(kind == EntryType::DirectoryIndex);
  let capacity = entry.name.len().checked_add(suffix).ok_or(NamespaceSeekFailureV1::PathOverflow)?;
  let mut key = String::new();
  key.try_reserve_exact(capacity).map_err(NamespaceSeekFailureV1::Allocation)?;
  key.push_str(&entry.name);
  if suffix != 0 {
    key.push('/');
  }
  Ok(key)
}

#[cfg(test)]
#[path = "../../../spec/engine/namespace_seek_internal_spec.rs"]
mod tests;
