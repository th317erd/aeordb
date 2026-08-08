use crate::engine::directory_entry::{ChildEntry, serialize_child_entries, deserialize_child_entries};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::storage_engine::{StorageEngine, WriteBatch};
use crate::engine::traversal::{TraversalIntegrity, VisitorCompletion};

/// Maximum entries in a leaf node before splitting.
pub const BTREE_MAX_LEAF_ENTRIES: usize = 40;
/// Minimum entries in a leaf node before merging.
pub const BTREE_MIN_LEAF_ENTRIES: usize = 20;
/// Maximum keys in an internal node before splitting.
pub const BTREE_MAX_INTERNAL_KEYS: usize = 77;
/// Minimum keys in an internal node before merging.
pub const BTREE_MIN_INTERNAL_KEYS: usize = 38;
/// Directory size threshold for converting flat list to B-tree.
pub const BTREE_CONVERSION_THRESHOLD: usize = 256;
const BTREE_MAX_WALK_DEPTH: usize = 128;
const BTREE_MAX_WALK_NODES: u64 = 16 * 1024 * 1024;
const BTREE_MAX_WALK_WARNINGS: usize = 1024;
const BTREE_WARNING_OMISSION: &str = "additional B-tree traversal warnings omitted";

/// B-tree node marker bytes for format detection.
pub const BTREE_LEAF_MARKER: u8 = 0x00;
pub const BTREE_INTERNAL_MARKER: u8 = 0x01;

/// One immutable B-tree node produced by a mutation plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BTreeNodeWrite {
  pub key: Vec<u8>,
  pub value: Vec<u8>,
}

/// A complete, mutation-free B-tree rewrite plan.
///
/// Callers decide which hard-authority batch publishes these immutable nodes.
/// Constructing a plan never writes the engine or updates a stable locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BTreeMutationPlan {
  root_hash: Vec<u8>,
  root_data: Vec<u8>,
  node_writes: Vec<BTreeNodeWrite>,
}

/// A set of name-level changes to apply to one B-tree root.
///
/// Names must be unique across both collections. Callers must resolve
/// contradictory remove/upsert intent before asking the B-tree to plan it.
#[derive(Debug, Clone, Default)]
pub struct BTreeMutationDelta {
  pub upserts: Vec<ChildEntry>,
  pub removals: Vec<String>,
}

#[derive(Debug, Default)]
struct PlannedNodeOverlay {
  writes: Vec<BTreeNodeWrite>,
  positions: std::collections::HashMap<Vec<u8>, usize>,
}

impl PlannedNodeOverlay {
  fn insert(&mut self, write: BTreeNodeWrite) -> EngineResult<()> {
    if let Some(position) = self.positions.get(&write.key).copied() {
      if self.writes[position].value != write.value {
        return Err(EngineError::CorruptEntry {
          offset: 0,
          reason: format!("planned B-tree hash {} has conflicting bytes", hex::encode(&write.key)),
        });
      }
      return Ok(());
    }
    self.positions.insert(write.key.clone(), self.writes.len());
    self.writes.push(write);
    Ok(())
  }

  fn value(&self, key: &[u8]) -> Option<&[u8]> {
    self.positions.get(key).map(|position| self.writes[*position].value.as_slice())
  }

  fn reachable_writes(&self, root_hash: &[u8], hash_length: usize) -> EngineResult<Vec<BTreeNodeWrite>> {
    let mut visited = std::collections::HashSet::new();
    let mut reachable = Vec::new();
    self.visit_reachable(root_hash, hash_length, &mut visited, &mut reachable)?;
    Ok(reachable)
  }

  fn visit_reachable(
    &self,
    hash: &[u8],
    hash_length: usize,
    visited: &mut std::collections::HashSet<Vec<u8>>,
    reachable: &mut Vec<BTreeNodeWrite>,
  ) -> EngineResult<()> {
    let Some(position) = self.positions.get(hash).copied() else {
      return Ok(());
    };
    if !visited.insert(hash.to_vec()) {
      return Ok(());
    }
    let write = &self.writes[position];
    if let BTreeNode::Internal(internal) = BTreeNode::deserialize(&write.value, hash_length, 0)? {
      for child_hash in internal.children {
        self.visit_reachable(&child_hash, hash_length, visited, reachable)?;
      }
    }
    reachable.push(write.clone());
    Ok(())
  }
}

impl BTreeMutationPlan {
  pub fn root_hash(&self) -> &[u8] {
    &self.root_hash
  }

  pub fn root_data(&self) -> &[u8] {
    &self.root_data
  }

  pub fn node_writes(&self) -> impl Iterator<Item = &BTreeNodeWrite> {
    self.node_writes.iter()
  }

  pub fn append_to_batch(&self, batch: &mut WriteBatch) {
    for write in &self.node_writes {
      batch.add(EntryType::DirectoryIndex, write.key.clone(), write.value.clone());
    }
  }
}

/// B-tree traversal policy for callers with different safety needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BTreeWalkMode {
  /// Any missing/corrupt node aborts the walk.
  Strict,
  /// Missing/corrupt child nodes are reported and skipped so read-only callers
  /// can return partial data without treating one damaged branch as an empty
  /// directory.
  BestEffort,
}

/// A recoverable B-tree walk problem observed during best-effort traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BTreeWalkWarning {
  pub node_hash: Option<Vec<u8>>,
  pub reason: String,
}

impl BTreeWalkWarning {
  pub fn node_hash_hex(&self) -> Option<String> {
    self.node_hash.as_ref().map(hex::encode)
  }
}

/// Result of a B-tree listing that may be partial.
#[derive(Debug, Clone, Default)]
pub struct BTreeListResult {
  pub entries: Vec<ChildEntry>,
  pub warnings: Vec<BTreeWalkWarning>,
  pub integrity: TraversalIntegrity,
}

impl BTreeListResult {
  pub fn is_complete(&self) -> bool {
    self.integrity.is_complete()
  }
}

/// Outcome of a bounded visitor walk over a B-tree.
pub(crate) struct BTreeVisitResult {
  pub warnings: Vec<BTreeWalkWarning>,
  pub integrity: TraversalIntegrity,
  pub visitor_completion: VisitorCompletion,
}

#[derive(Default)]
struct BTreeWalkState {
  ancestors: Vec<Vec<u8>>,
  visited_nodes: u64,
}

impl BTreeWalkState {
  fn enter(&mut self, node_hash: Option<&[u8]>) -> EngineResult<bool> {
    self.visited_nodes = self
      .visited_nodes
      .checked_add(1)
      .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: "B-tree visited-node counter overflow".to_string() })?;
    if self.visited_nodes > BTREE_MAX_WALK_NODES {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("B-tree traversal exceeds {BTREE_MAX_WALK_NODES} nodes") });
    }
    let Some(node_hash) = node_hash else {
      return Ok(false);
    };
    if self.ancestors.len() >= BTREE_MAX_WALK_DEPTH {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("B-tree traversal exceeds depth {BTREE_MAX_WALK_DEPTH}") });
    }
    if self.ancestors.iter().any(|ancestor| ancestor.as_slice() == node_hash) {
      return Err(EngineError::CorruptEntry { offset: 0, reason: format!("B-tree cycle at node {}", hex::encode(node_hash)) });
    }
    self.ancestors.push(node_hash.to_vec());
    Ok(true)
  }

  fn leave(&mut self, entered_hash: bool) {
    if entered_hash {
      self.ancestors.pop();
    }
  }
}

/// A B-tree node — either a leaf containing ChildEntry data,
/// or an internal node containing sorted keys and child node hashes.
#[derive(Debug, Clone)]
pub enum BTreeNode {
  Leaf(LeafNode),
  Internal(InternalNode),
}

/// Leaf node: holds sorted ChildEntry values.
#[derive(Debug, Clone)]
pub struct LeafNode {
  pub entries: Vec<ChildEntry>,
}

/// Internal node: holds sorted keys (child names) and child node hashes.
/// children.len() == keys.len() + 1
#[derive(Debug, Clone)]
pub struct InternalNode {
  pub keys: Vec<String>,
  pub children: Vec<Vec<u8>>, // hashes of child nodes
}

impl BTreeNode {
  /// Serialize a B-tree node to bytes.
  pub fn serialize(&self, hash_length: usize) -> EngineResult<Vec<u8>> {
    match self {
      BTreeNode::Leaf(leaf) => {
        let child_data = serialize_child_entries(&leaf.entries, hash_length)?;
        let mut buffer = Vec::with_capacity(1 + 2 + child_data.len());
        buffer.push(BTREE_LEAF_MARKER);
        buffer.extend_from_slice(&(leaf.entries.len() as u16).to_le_bytes());
        buffer.extend_from_slice(&child_data);
        Ok(buffer)
      }
      BTreeNode::Internal(internal) => {
        let mut buffer = Vec::new();
        buffer.push(BTREE_INTERNAL_MARKER);
        buffer.extend_from_slice(&(internal.keys.len() as u16).to_le_bytes());

        // Serialize keys
        for key in &internal.keys {
          let key_bytes = key.as_bytes();
          if key_bytes.len() > u16::MAX as usize {
            return Err(EngineError::InvalidInput(format!("B-tree key too long: {} bytes exceeds u16 max (65535)", key_bytes.len())));
          }
          buffer.extend_from_slice(&(key_bytes.len() as u16).to_le_bytes());
          buffer.extend_from_slice(key_bytes);
        }

        // Serialize children (keys.len() + 1 hashes)
        for child_hash in &internal.children {
          buffer.extend_from_slice(child_hash);
        }

        Ok(buffer)
      }
    }
  }

  /// Deserialize a B-tree node from bytes. Dispatches on the surrounding
  /// KV `EntryHeader.entry_version` — callers MUST pass it through.
  pub fn deserialize(data: &[u8], hash_length: usize, version: u8) -> EngineResult<Self> {
    match version {
      0 => Self::deserialize_v0(data, hash_length),
      _ => Err(EngineError::InvalidEntryVersion(version)),
    }
  }

  fn deserialize_v0(data: &[u8], hash_length: usize) -> EngineResult<Self> {
    if data.is_empty() {
      return Err(EngineError::CorruptEntry { offset: 0, reason: "Empty B-tree node data".to_string() });
    }

    match data[0] {
      BTREE_LEAF_MARKER => {
        if data.len() < 3 {
          return Err(EngineError::CorruptEntry { offset: 0, reason: "Leaf node data too short".to_string() });
        }
        let entry_count = u16::from_le_bytes([data[1], data[2]]) as usize;
        let entries = if entry_count == 0 { Vec::new() } else { deserialize_child_entries(&data[3..], hash_length, 0)? };
        Ok(BTreeNode::Leaf(LeafNode { entries }))
      }
      BTREE_INTERNAL_MARKER => {
        if data.len() < 3 {
          return Err(EngineError::CorruptEntry { offset: 0, reason: "Internal node data too short".to_string() });
        }
        let key_count = u16::from_le_bytes([data[1], data[2]]) as usize;
        let mut offset = 3;

        // Read keys
        let mut keys = Vec::with_capacity(key_count);
        for _ in 0..key_count {
          if offset + 2 > data.len() {
            return Err(EngineError::CorruptEntry {
              offset: offset as u64,
              reason: "Internal node data too short for key length".to_string(),
            });
          }
          let key_len = u16::from_le_bytes([data[offset], data[offset + 1]]) as usize;
          offset += 2;
          if offset + key_len > data.len() {
            return Err(EngineError::CorruptEntry { offset: offset as u64, reason: "Internal node data too short for key".to_string() });
          }
          let key = String::from_utf8(data[offset..offset + key_len].to_vec())
            .map_err(|e| EngineError::CorruptEntry { offset: offset as u64, reason: format!("Invalid UTF-8 key: {}", e) })?;
          keys.push(key);
          offset += key_len;
        }

        // Read children (key_count + 1 hashes)
        let child_count = key_count + 1;
        let mut children = Vec::with_capacity(child_count);
        for _ in 0..child_count {
          if offset + hash_length > data.len() {
            return Err(EngineError::CorruptEntry {
              offset: offset as u64,
              reason: "Internal node data too short for child hash".to_string(),
            });
          }
          children.push(data[offset..offset + hash_length].to_vec());
          offset += hash_length;
        }

        Ok(BTreeNode::Internal(InternalNode { keys, children }))
      }
      other => Err(EngineError::CorruptEntry { offset: 0, reason: format!("Unknown B-tree node type: 0x{:02x}", other) }),
    }
  }

  /// Compute the content hash for this node.
  pub fn content_hash(&self, hash_length: usize, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
    let serialized = self.serialize(hash_length)?;
    let mut input = Vec::with_capacity(6 + serialized.len());
    input.extend_from_slice(b"btree:");
    input.extend_from_slice(&serialized);
    algo.compute_hash(&input)
  }

  /// Check if this node is a leaf.
  pub fn is_leaf(&self) -> bool {
    matches!(self, BTreeNode::Leaf(_))
  }
}

impl Default for LeafNode {
  fn default() -> Self {
    Self::new()
  }
}

impl LeafNode {
  pub fn new() -> Self {
    LeafNode { entries: Vec::new() }
  }

  pub fn is_full(&self) -> bool {
    self.entries.len() >= BTREE_MAX_LEAF_ENTRIES
  }

  pub fn is_underflow(&self) -> bool {
    self.entries.len() < BTREE_MIN_LEAF_ENTRIES
  }

  /// Find a child entry by name (binary search since entries are sorted).
  pub fn find(&self, name: &str) -> Option<&ChildEntry> {
    self.entries.binary_search_by(|e| e.name.as_str().cmp(name)).ok().map(|idx| &self.entries[idx])
  }

  /// Insert or update a child entry, maintaining sorted order.
  /// Returns true if inserted (new), false if updated (existing).
  pub fn upsert(&mut self, entry: ChildEntry) -> bool {
    match self.entries.binary_search_by(|e| e.name.as_str().cmp(&entry.name)) {
      Ok(idx) => {
        self.entries[idx] = entry;
        false // updated
      }
      Err(idx) => {
        self.entries.insert(idx, entry);
        true // inserted
      }
    }
  }

  /// Remove a child entry by name. Returns true if found and removed.
  pub fn remove(&mut self, name: &str) -> bool {
    if let Ok(idx) = self.entries.binary_search_by(|e| e.name.as_str().cmp(name)) {
      self.entries.remove(idx);
      true
    } else {
      false
    }
  }

  /// Split this leaf into two halves. Returns (left, split_key, right).
  pub fn split(&mut self) -> (LeafNode, String, LeafNode) {
    let mid = self.entries.len() / 2;
    let right_entries = self.entries.split_off(mid);
    let split_key = right_entries[0].name.clone();
    let left = LeafNode { entries: self.entries.clone() };
    let right = LeafNode { entries: right_entries };
    (left, split_key, right)
  }
}

impl InternalNode {
  /// Find which child subtree a key belongs to.
  /// Returns the index into self.children.
  pub fn find_child_index(&self, name: &str) -> usize {
    match self.keys.binary_search_by(|k| k.as_str().cmp(name)) {
      Ok(idx) => idx + 1, // exact match: go right
      Err(idx) => idx,    // insertion point: go to that child
    }
  }

  pub fn is_full(&self) -> bool {
    self.keys.len() >= BTREE_MAX_INTERNAL_KEYS
  }

  pub fn is_underflow(&self) -> bool {
    self.keys.len() < BTREE_MIN_INTERNAL_KEYS
  }

  /// Insert a new key and child hash at the correct position.
  pub fn insert_key(&mut self, key: String, right_child_hash: Vec<u8>) {
    let idx = match self.keys.binary_search_by(|k| k.as_str().cmp(&key)) {
      Ok(idx) => idx,
      Err(idx) => idx,
    };
    self.keys.insert(idx, key);
    self.children.insert(idx + 1, right_child_hash);
  }

  /// Split this internal node. Returns (left, split_key, right).
  pub fn split(&mut self) -> (InternalNode, String, InternalNode) {
    let mid = self.keys.len() / 2;
    let split_key = self.keys[mid].clone();

    let right_keys = self.keys.split_off(mid + 1);
    self.keys.pop(); // remove the split key from left

    let right_children = self.children.split_off(mid + 1);

    let left = InternalNode { keys: self.keys.clone(), children: self.children.clone() };
    let right = InternalNode { keys: right_keys, children: right_children };
    (left, split_key, right)
  }
}

/// Detect whether directory data is a B-tree node or flat list.
/// B-tree nodes start with 0x00 (leaf) or 0x01 (internal).
/// Flat lists start with the first ChildEntry's entry_type (>= 0x02).
pub fn is_btree_format(data: &[u8]) -> bool {
  if data.is_empty() {
    return false;
  }
  data[0] == BTREE_LEAF_MARKER || data[0] == BTREE_INTERNAL_MARKER
}

// ─── B-tree operations (Task 2) ─────────────────────────────────────────────

/// Result of a B-tree insert that may cause a split.
enum InsertResult {
  /// Inserted without split. Returns (new_hash, serialized_data).
  Done(Vec<u8>, Vec<u8>),
  /// Node was split. Returns (new_left_hash, split_key, new_right_hash).
  Split(Vec<u8>, String, Vec<u8>),
}

/// Insert a child entry into a B-tree directory.
/// Returns the new root hash of the directory.
pub fn btree_insert(
  engine: &StorageEngine,
  root_hash: &[u8],
  entry: ChildEntry,
  hash_length: usize,
  algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
  let root_data =
    engine.get_entry(root_hash)?.ok_or_else(|| EngineError::NotFound(format!("B-tree root not found: {}", hex::encode(root_hash))))?;
  let (new_hash, _) = btree_insert_with_data(engine, &root_data.2, entry, hash_length, algo)?;
  Ok(new_hash)
}

/// Insert into a B-tree, starting from an already-loaded root node.
/// Avoids re-reading the root from the engine.
/// Returns (new_root_hash, new_root_data) so the caller doesn't need to read it back.
pub fn btree_insert_with_data(
  engine: &StorageEngine,
  root_data: &[u8],
  entry: ChildEntry,
  hash_length: usize,
  algo: &HashAlgorithm,
) -> EngineResult<(Vec<u8>, Vec<u8>)> {
  let root_node = BTreeNode::deserialize(root_data, hash_length, 0)?;

  let result = btree_insert_node(engine, root_node, entry, hash_length, algo)?;

  match result {
    InsertResult::Done(new_hash, new_data) => Ok((new_hash, new_data)),
    InsertResult::Split(left_hash, split_key, right_hash) => {
      let new_root = BTreeNode::Internal(InternalNode { keys: vec![split_key], children: vec![left_hash, right_hash] });
      let new_data = new_root.serialize(hash_length)?;
      let new_hash = store_btree_node(engine, &new_root, hash_length, algo)?;
      Ok((new_hash, new_data))
    }
  }
}

fn btree_insert_node(
  engine: &StorageEngine,
  node: BTreeNode,
  entry: ChildEntry,
  hash_length: usize,
  algo: &HashAlgorithm,
) -> EngineResult<InsertResult> {
  match node {
    BTreeNode::Leaf(mut leaf) => {
      leaf.upsert(entry);

      if leaf.is_full() {
        // Split
        let (left, split_key, right) = leaf.split();
        let left_hash = store_btree_node(engine, &BTreeNode::Leaf(left), hash_length, algo)?;
        let right_hash = store_btree_node(engine, &BTreeNode::Leaf(right), hash_length, algo)?;
        Ok(InsertResult::Split(left_hash, split_key, right_hash))
      } else {
        let node = BTreeNode::Leaf(leaf);
        let data = node.serialize(hash_length)?;
        let hash = store_btree_node(engine, &node, hash_length, algo)?;
        Ok(InsertResult::Done(hash, data))
      }
    }
    BTreeNode::Internal(mut internal) => {
      let child_idx = internal.find_child_index(&entry.name);
      let child_hash = internal.children[child_idx].clone();

      // Read the child node
      let child_data = engine
        .get_entry(&child_hash)?
        .ok_or_else(|| EngineError::NotFound(format!("B-tree child not found: {}", hex::encode(&child_hash))))?;
      let child_node = BTreeNode::deserialize(&child_data.2, hash_length, child_data.0.entry_version)?;

      // Recurse into child
      let child_result = btree_insert_node(engine, child_node, entry, hash_length, algo)?;

      match child_result {
        InsertResult::Done(new_child_hash, _) => {
          internal.children[child_idx] = new_child_hash;
          let node = BTreeNode::Internal(internal);
          let data = node.serialize(hash_length)?;
          let hash = store_btree_node(engine, &node, hash_length, algo)?;
          Ok(InsertResult::Done(hash, data))
        }
        InsertResult::Split(left_hash, split_key, right_hash) => {
          internal.children[child_idx] = left_hash;
          internal.insert_key(split_key, right_hash);

          if internal.is_full() {
            let (left, parent_split_key, right) = internal.split();
            let left_hash = store_btree_node(engine, &BTreeNode::Internal(left), hash_length, algo)?;
            let right_hash = store_btree_node(engine, &BTreeNode::Internal(right), hash_length, algo)?;
            Ok(InsertResult::Split(left_hash, parent_split_key, right_hash))
          } else {
            let node = BTreeNode::Internal(internal);
            let data = node.serialize(hash_length)?;
            let hash = store_btree_node(engine, &node, hash_length, algo)?;
            Ok(InsertResult::Done(hash, data))
          }
        }
      }
    }
  }
}

/// Store a B-tree node in the engine and return its content hash.
pub fn store_btree_node(engine: &StorageEngine, node: &BTreeNode, hash_length: usize, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let serialized = node.serialize(hash_length)?;
  let content_hash = node.content_hash(hash_length, algo)?;
  engine.store_entry(EntryType::DirectoryIndex, &content_hash, &serialized)?;
  Ok(content_hash)
}

/// Create a new B-tree from a list of ChildEntry values.
/// Used for flat -> B-tree conversion.
pub fn btree_from_entries(
  engine: &StorageEngine,
  entries: Vec<ChildEntry>,
  hash_length: usize,
  algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
  let plan = btree_plan_from_entries(entries, hash_length, algo)?;
  let root_hash = plan.root_hash.clone();
  let mut batch = WriteBatch::new();
  plan.append_to_batch(&mut batch);
  engine.flush_batch(batch)?;
  Ok(root_hash)
}

#[derive(Debug)]
struct PlannedBTreeLevelNode {
  hash: Vec<u8>,
  data: Vec<u8>,
  first_key: Option<String>,
}

fn append_planned_node(
  node: BTreeNode,
  first_key: Option<String>,
  hash_length: usize,
  algo: &HashAlgorithm,
  writes: &mut Vec<BTreeNodeWrite>,
) -> EngineResult<PlannedBTreeLevelNode> {
  let data = node.serialize(hash_length)?;
  let hash = node.content_hash(hash_length, algo)?;
  writes.push(BTreeNodeWrite { key: hash.clone(), value: data.clone() });
  Ok(PlannedBTreeLevelNode { hash, data, first_key })
}

/// Build a complete B-tree without writing any node to storage.
pub fn btree_plan_from_entries(mut entries: Vec<ChildEntry>, hash_length: usize, algo: &HashAlgorithm) -> EngineResult<BTreeMutationPlan> {
  // Sort entries by name
  entries.sort_by(|a, b| a.name.cmp(&b.name));
  let mut node_writes = Vec::new();

  if entries.is_empty() {
    let root = append_planned_node(BTreeNode::Leaf(LeafNode::new()), None, hash_length, algo, &mut node_writes)?;
    return Ok(BTreeMutationPlan { root_hash: root.hash, root_data: root.data, node_writes });
  }

  let mut level = Vec::new();
  for chunk in entries.chunks(BTREE_MAX_LEAF_ENTRIES) {
    level.push(append_planned_node(
      BTreeNode::Leaf(LeafNode { entries: chunk.to_vec() }),
      Some(chunk[0].name.clone()),
      hash_length,
      algo,
      &mut node_writes,
    )?);
  }

  let max_children = BTREE_MAX_INTERNAL_KEYS + 1;
  while level.len() > 1 {
    let mut next_level = Vec::new();
    for children in level.chunks(max_children) {
      let keys = children
        .iter()
        .skip(1)
        .map(|child| {
          child
            .first_key
            .clone()
            .ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: "planned B-tree child has no first key".to_string() })
        })
        .collect::<EngineResult<Vec<_>>>()?;
      let child_hashes = children.iter().map(|child| child.hash.clone()).collect();
      let first_key = children.first().and_then(|child| child.first_key.clone());
      next_level.push(append_planned_node(
        BTreeNode::Internal(InternalNode { keys, children: child_hashes }),
        first_key,
        hash_length,
        algo,
        &mut node_writes,
      )?);
    }
    level = next_level;
  }

  let root = level.pop().ok_or_else(|| EngineError::CorruptEntry { offset: 0, reason: "planned B-tree has no root".to_string() })?;
  Ok(BTreeMutationPlan { root_hash: root.hash, root_data: root.data, node_writes })
}

/// Look up a single child by name in a B-tree directory.
///
/// When `include_deleted` is true, uses `get_entry_including_deleted()` so that
/// B-tree nodes marked as deleted (common when walking historical snapshots) are
/// still reachable.
pub fn btree_lookup(
  engine: &StorageEngine,
  root_hash: &[u8],
  name: &str,
  hash_length: usize,
  include_deleted: bool,
) -> EngineResult<Option<ChildEntry>> {
  let mut state = BTreeWalkState::default();
  let mut current_hash = root_hash.to_vec();
  loop {
    state.enter(Some(&current_hash))?;
    let node_data = if include_deleted { engine.get_entry_including_deleted(&current_hash)? } else { engine.get_entry(&current_hash)? }
      .ok_or_else(|| EngineError::NotFound("B-tree node not found".to_string()))?;
    let node = BTreeNode::deserialize(&node_data.2, hash_length, node_data.0.entry_version)?;

    match node {
      BTreeNode::Leaf(leaf) => return Ok(leaf.find(name).cloned()),
      BTreeNode::Internal(internal) => {
        let child_idx = internal.find_child_index(name);
        current_hash = internal.children[child_idx].clone();
      }
    }
  }
}

/// List all children in a B-tree directory (in sorted order).
///
/// When `include_deleted` is true, uses `get_entry_including_deleted()` so that
/// B-tree nodes marked as deleted (common when walking historical snapshots) are
/// still reachable.
pub fn btree_list(engine: &StorageEngine, root_hash: &[u8], hash_length: usize, include_deleted: bool) -> EngineResult<Vec<ChildEntry>> {
  Ok(btree_list_with_mode(engine, root_hash, hash_length, include_deleted, BTreeWalkMode::Strict)?.entries)
}

/// List all children starting from a serialized root node.
/// Used when the caller already has the root node data (e.g., from a path-keyed entry).
///
/// When `include_deleted` is true, uses `get_entry_including_deleted()` so that
/// B-tree nodes marked as deleted (common when walking historical snapshots) are
/// still reachable.
pub fn btree_list_from_node(
  root_data: &[u8],
  engine: &StorageEngine,
  hash_length: usize,
  include_deleted: bool,
) -> EngineResult<Vec<ChildEntry>> {
  Ok(btree_list_from_node_with_mode(root_data, engine, hash_length, include_deleted, BTreeWalkMode::Strict)?.entries)
}

/// List all children in a B-tree directory with an explicit traversal policy.
pub fn btree_list_with_mode(
  engine: &StorageEngine,
  root_hash: &[u8],
  hash_length: usize,
  include_deleted: bool,
  mode: BTreeWalkMode,
) -> EngineResult<BTreeListResult> {
  let mut state = BTreeWalkState::default();
  btree_list_hash_with_mode(engine, root_hash, hash_length, include_deleted, mode, true, &mut state)
}

fn btree_list_hash_with_mode(
  engine: &StorageEngine,
  root_hash: &[u8],
  hash_length: usize,
  include_deleted: bool,
  mode: BTreeWalkMode,
  is_root: bool,
  state: &mut BTreeWalkState,
) -> EngineResult<BTreeListResult> {
  let entered = match state.enter(Some(root_hash)) {
    Ok(entered) => entered,
    Err(error) => return btree_walk_error(mode, Some(root_hash), error, is_root),
  };
  let result = (|| {
    let node_data = if include_deleted { engine.get_entry_including_deleted(root_hash) } else { engine.get_entry(root_hash) };
    let node_data = match node_data {
      Ok(Some(data)) => data,
      Ok(None) => {
        return btree_walk_error(
          mode,
          Some(root_hash),
          EngineError::NotFound(format!("B-tree node not found: {}", hex::encode(root_hash))),
          is_root,
        )
      }
      Err(error) => return btree_walk_error(mode, Some(root_hash), error, is_root),
    };
    if node_data.0.entry_type != EntryType::DirectoryIndex {
      return btree_walk_error(
        mode,
        Some(root_hash),
        EngineError::CorruptEntry { offset: 0, reason: format!("B-tree node hash resolved to {:?} entry", node_data.0.entry_type) },
        is_root,
      );
    }
    btree_list_loaded_node(
      Some(root_hash),
      &node_data.2,
      node_data.0.entry_version,
      engine,
      hash_length,
      include_deleted,
      mode,
      is_root,
      state,
    )
  })();
  state.leave(entered);
  result
}

/// List all children starting from serialized root node data with an explicit
/// traversal policy.
pub fn btree_list_from_node_with_mode(
  root_data: &[u8],
  engine: &StorageEngine,
  hash_length: usize,
  include_deleted: bool,
  mode: BTreeWalkMode,
) -> EngineResult<BTreeListResult> {
  let mut state = BTreeWalkState::default();
  state.enter(None)?;
  btree_list_loaded_node(None, root_data, 0, engine, hash_length, include_deleted, mode, true, &mut state)
}

/// Visit directory entries in key order without collecting the entire B-tree.
/// Returning `false` from the visitor stops before loading any later branch.
pub(crate) fn btree_visit_from_node_with_mode<F>(
  root_data: &[u8],
  engine: &StorageEngine,
  hash_length: usize,
  include_deleted: bool,
  mode: BTreeWalkMode,
  visitor: &mut F,
) -> EngineResult<BTreeVisitResult>
where
  F: FnMut(&ChildEntry) -> EngineResult<bool>,
{
  let mut state = BTreeWalkState::default();
  state.enter(None)?;
  btree_visit_loaded_node(None, root_data, 0, engine, hash_length, include_deleted, mode, true, visitor, &mut state)
}

fn btree_visit_hash_with_mode<F>(
  node_hash: &[u8],
  engine: &StorageEngine,
  hash_length: usize,
  include_deleted: bool,
  mode: BTreeWalkMode,
  visitor: &mut F,
  state: &mut BTreeWalkState,
) -> EngineResult<BTreeVisitResult>
where
  F: FnMut(&ChildEntry) -> EngineResult<bool>,
{
  let entered = match state.enter(Some(node_hash)) {
    Ok(entered) => entered,
    Err(error) => return btree_visit_error(mode, Some(node_hash), error, false),
  };
  let result = (|| {
    let node_data = if include_deleted { engine.get_entry_including_deleted(node_hash) } else { engine.get_entry(node_hash) };
    let node_data = match node_data {
      Ok(Some(data)) => data,
      Ok(None) => {
        return btree_visit_error(
          mode,
          Some(node_hash),
          EngineError::NotFound(format!("B-tree node not found: {}", hex::encode(node_hash))),
          false,
        )
      }
      Err(error) => return btree_visit_error(mode, Some(node_hash), error, false),
    };
    if node_data.0.entry_type != EntryType::DirectoryIndex {
      return btree_visit_error(
        mode,
        Some(node_hash),
        EngineError::CorruptEntry { offset: 0, reason: format!("B-tree node hash resolved to {:?} entry", node_data.0.entry_type) },
        false,
      );
    }
    btree_visit_loaded_node(
      Some(node_hash),
      &node_data.2,
      node_data.0.entry_version,
      engine,
      hash_length,
      include_deleted,
      mode,
      false,
      visitor,
      state,
    )
  })();
  state.leave(entered);
  result
}

fn btree_visit_loaded_node<F>(
  node_hash: Option<&[u8]>,
  node_data: &[u8],
  entry_version: u8,
  engine: &StorageEngine,
  hash_length: usize,
  include_deleted: bool,
  mode: BTreeWalkMode,
  is_root: bool,
  visitor: &mut F,
  state: &mut BTreeWalkState,
) -> EngineResult<BTreeVisitResult>
where
  F: FnMut(&ChildEntry) -> EngineResult<bool>,
{
  let node = match BTreeNode::deserialize(node_data, hash_length, entry_version) {
    Ok(node) => node,
    Err(error) => return btree_visit_error(mode, node_hash, error, is_root),
  };
  match node {
    BTreeNode::Leaf(leaf) => {
      for entry in &leaf.entries {
        if !visitor(entry)? {
          return Ok(BTreeVisitResult {
            warnings: Vec::new(),
            integrity: TraversalIntegrity::Complete,
            visitor_completion: VisitorCompletion::StoppedByVisitor,
          });
        }
      }
      Ok(BTreeVisitResult {
        warnings: Vec::new(),
        integrity: TraversalIntegrity::Complete,
        visitor_completion: VisitorCompletion::Exhausted,
      })
    }
    BTreeNode::Internal(internal) => {
      let mut warnings = Vec::new();
      let mut integrity = TraversalIntegrity::Complete;
      for child_hash in &internal.children {
        let child = btree_visit_hash_with_mode(child_hash, engine, hash_length, include_deleted, mode, visitor, state)?;
        integrity = integrity.combine(child.integrity);
        append_bounded_warnings(&mut warnings, child.warnings);
        if child.visitor_completion == VisitorCompletion::StoppedByVisitor {
          return Ok(BTreeVisitResult { warnings, integrity, visitor_completion: VisitorCompletion::StoppedByVisitor });
        }
      }
      Ok(BTreeVisitResult { warnings, integrity, visitor_completion: VisitorCompletion::Exhausted })
    }
  }
}

fn btree_visit_error(mode: BTreeWalkMode, node_hash: Option<&[u8]>, error: EngineError, is_root: bool) -> EngineResult<BTreeVisitResult> {
  match mode {
    BTreeWalkMode::Strict => Err(error),
    BTreeWalkMode::BestEffort if is_operational_walk_error(&error) => Err(error),
    BTreeWalkMode::BestEffort => Ok(BTreeVisitResult {
      warnings: vec![BTreeWalkWarning { node_hash: node_hash.map(Vec::from), reason: error.to_string() }],
      integrity: if is_root { TraversalIntegrity::Corrupt } else { TraversalIntegrity::DiagnosticallyPartial },
      visitor_completion: VisitorCompletion::Exhausted,
    }),
  }
}

fn btree_list_loaded_node(
  node_hash: Option<&[u8]>,
  node_data: &[u8],
  entry_version: u8,
  engine: &StorageEngine,
  hash_length: usize,
  include_deleted: bool,
  mode: BTreeWalkMode,
  is_root: bool,
  state: &mut BTreeWalkState,
) -> EngineResult<BTreeListResult> {
  let node = match BTreeNode::deserialize(node_data, hash_length, entry_version) {
    Ok(node) => node,
    Err(error) => return btree_walk_error(mode, node_hash, error, is_root),
  };
  match node {
    BTreeNode::Leaf(leaf) => Ok(BTreeListResult { entries: leaf.entries, warnings: Vec::new(), integrity: TraversalIntegrity::Complete }),
    BTreeNode::Internal(internal) => {
      let mut result = BTreeListResult::default();
      for child_hash in &internal.children {
        let mut child_result = btree_list_hash_with_mode(engine, child_hash, hash_length, include_deleted, mode, false, state)?;
        result.integrity = result.integrity.combine(child_result.integrity);
        result.entries.append(&mut child_result.entries);
        append_bounded_warnings(&mut result.warnings, child_result.warnings);
      }
      Ok(result)
    }
  }
}

fn btree_walk_error(mode: BTreeWalkMode, node_hash: Option<&[u8]>, error: EngineError, is_root: bool) -> EngineResult<BTreeListResult> {
  match mode {
    BTreeWalkMode::Strict => Err(error),
    BTreeWalkMode::BestEffort if is_operational_walk_error(&error) => Err(error),
    BTreeWalkMode::BestEffort => Ok(BTreeListResult {
      entries: Vec::new(),
      warnings: vec![BTreeWalkWarning { node_hash: node_hash.map(Vec::from), reason: error.to_string() }],
      integrity: if is_root { TraversalIntegrity::Corrupt } else { TraversalIntegrity::DiagnosticallyPartial },
    }),
  }
}

fn is_operational_walk_error(error: &EngineError) -> bool {
  matches!(
    error,
    EngineError::IoError(_)
      | EngineError::ResourceExhausted(_)
      | EngineError::DurabilityFailure(_)
      | EngineError::PostMutationDurabilityFailure(_)
      | EngineError::ShuttingDown
      | EngineError::Cancelled(_)
  )
}

fn append_bounded_warnings(target: &mut Vec<BTreeWalkWarning>, mut source: Vec<BTreeWalkWarning>) {
  if source.is_empty() || target.last().is_some_and(|warning| warning.reason == BTREE_WARNING_OMISSION) {
    return;
  }
  if target.len().saturating_add(source.len()) <= BTREE_MAX_WALK_WARNINGS {
    target.append(&mut source);
    return;
  }

  target.truncate(BTREE_MAX_WALK_WARNINGS.saturating_sub(1));
  let available = BTREE_MAX_WALK_WARNINGS.saturating_sub(1).saturating_sub(target.len());
  target.extend(source.drain(..source.len().min(available)));
  target.push(BTreeWalkWarning { node_hash: None, reason: BTREE_WARNING_OMISSION.to_string() });
}

/// Delete a child from a B-tree directory.
/// Returns the new root hash, or None if the tree is now empty.
///
/// NOTE: btree_delete does NOT rebalance after removal. After many deletions,
/// the tree can have near-empty leaf nodes, degrading lookup from O(log N)
/// toward O(N). For now, this is acceptable -- a full reindex rebuilds the tree.
/// Future: implement sibling borrowing and node merging on underflow.
fn load_planned_btree_node(engine: &StorageEngine, overlay: &PlannedNodeOverlay, node_hash: &[u8]) -> EngineResult<(Vec<u8>, u8)> {
  if let Some(value) = overlay.value(node_hash) {
    return Ok((value.to_vec(), 0));
  }
  let (header, _key, value) = engine.get_entry(node_hash)?.ok_or_else(|| EngineError::NotFound("B-tree node not found".to_string()))?;
  Ok((value, header.entry_version))
}

pub fn btree_delete(
  engine: &StorageEngine,
  root_hash: &[u8],
  name: &str,
  hash_length: usize,
  algo: &HashAlgorithm,
) -> EngineResult<Option<Vec<u8>>> {
  let Some(plan) = btree_plan_delete(engine, root_hash, name, hash_length, algo)? else {
    return Ok(None);
  };
  let root_hash = plan.root_hash.clone();
  let mut batch = WriteBatch::new();
  plan.append_to_batch(&mut batch);
  if !batch.is_empty() {
    engine.flush_batch(batch)?;
  }
  Ok(Some(root_hash))
}

/// Plan a B-tree deletion without publishing any replacement node.
pub fn btree_plan_delete(
  engine: &StorageEngine,
  root_hash: &[u8],
  name: &str,
  hash_length: usize,
  algo: &HashAlgorithm,
) -> EngineResult<Option<BTreeMutationPlan>> {
  let mut overlay = PlannedNodeOverlay::default();
  let Some((root_hash, root_data)) = btree_plan_delete_node(engine, root_hash, None, name, hash_length, algo, &mut overlay)? else {
    return Ok(None);
  };
  let node_writes = overlay.reachable_writes(&root_hash, hash_length)?;
  Ok(Some(BTreeMutationPlan { root_hash, root_data, node_writes }))
}

fn btree_plan_delete_node(
  engine: &StorageEngine,
  node_hash: &[u8],
  supplied_node_data: Option<&[u8]>,
  name: &str,
  hash_length: usize,
  algo: &HashAlgorithm,
  overlay: &mut PlannedNodeOverlay,
) -> EngineResult<Option<(Vec<u8>, Vec<u8>)>> {
  let (node_data, entry_version) = match supplied_node_data {
    Some(data) => (data.to_vec(), 0),
    None => load_planned_btree_node(engine, overlay, node_hash)?,
  };
  let mut node = BTreeNode::deserialize(&node_data, hash_length, entry_version)?;

  match &mut node {
    BTreeNode::Leaf(ref mut leaf) => {
      leaf.remove(name);
      if leaf.entries.is_empty() {
        Ok(None) // tree is empty
      } else {
        let data = node.serialize(hash_length)?;
        let hash = node.content_hash(hash_length, algo)?;
        overlay.insert(BTreeNodeWrite { key: hash.clone(), value: data.clone() })?;
        Ok(Some((hash, data)))
      }
    }
    BTreeNode::Internal(ref mut internal) => {
      let child_idx = internal.find_child_index(name);
      let child_hash = internal.children[child_idx].clone();

      match btree_plan_delete_node(engine, &child_hash, None, name, hash_length, algo, overlay)? {
        Some((new_child_hash, _new_child_data)) => {
          internal.children[child_idx] = new_child_hash;
          let data = node.serialize(hash_length)?;
          let hash = node.content_hash(hash_length, algo)?;
          overlay.insert(BTreeNodeWrite { key: hash.clone(), value: data.clone() })?;
          Ok(Some((hash, data)))
        }
        None => {
          // Child is now empty — remove from internal node
          if child_idx < internal.keys.len() {
            internal.keys.remove(child_idx);
          } else if !internal.keys.is_empty() {
            internal.keys.remove(child_idx - 1);
          }
          internal.children.remove(child_idx);

          if internal.children.is_empty() {
            Ok(None)
          } else if internal.children.len() == 1 {
            // Collapse: single child becomes the new root
            let remaining_hash = internal.children[0].clone();
            let (remaining_data, _entry_version) = load_planned_btree_node(engine, overlay, &remaining_hash)?;
            Ok(Some((remaining_hash, remaining_data)))
          } else {
            let data = node.serialize(hash_length)?;
            let hash = node.content_hash(hash_length, algo)?;
            overlay.insert(BTreeNodeWrite { key: hash.clone(), value: data.clone() })?;
            Ok(Some((hash, data)))
          }
        }
      }
    }
  }
}

// ─── Batched B-tree insert (write buffering) ────────────────────────────────

/// Insert into a B-tree with batched writes.
/// All node writes are accumulated in a WriteBatch and flushed at the end,
/// reducing lock acquisitions from O(tree_depth) to O(1).
/// Returns (new_root_hash, new_root_data) so the caller doesn't need to read it back.
pub fn btree_insert_batched(
  engine: &StorageEngine,
  root_data: &[u8],
  entry: ChildEntry,
  hash_length: usize,
  algo: &HashAlgorithm,
) -> EngineResult<(Vec<u8>, Vec<u8>)> {
  let plan = btree_plan_insert(engine, root_data, entry, hash_length, algo)?;
  let result = (plan.root_hash.clone(), plan.root_data.clone());
  let mut batch = WriteBatch::new();
  plan.append_to_batch(&mut batch);
  engine.flush_batch(batch)?;
  Ok(result)
}

/// Plan a B-tree insert without publishing any new node.
pub fn btree_plan_insert(
  engine: &StorageEngine,
  root_data: &[u8],
  entry: ChildEntry,
  hash_length: usize,
  algo: &HashAlgorithm,
) -> EngineResult<BTreeMutationPlan> {
  let mut overlay = PlannedNodeOverlay::default();
  let (new_hash, new_data) = btree_plan_insert_with_overlay(engine, root_data, entry, hash_length, algo, &mut overlay)?;
  let node_writes = overlay.reachable_writes(&new_hash, hash_length)?;
  Ok(BTreeMutationPlan { root_hash: new_hash, root_data: new_data, node_writes })
}

fn btree_plan_insert_with_overlay(
  engine: &StorageEngine,
  root_data: &[u8],
  entry: ChildEntry,
  hash_length: usize,
  algo: &HashAlgorithm,
  overlay: &mut PlannedNodeOverlay,
) -> EngineResult<(Vec<u8>, Vec<u8>)> {
  let root_node = BTreeNode::deserialize(root_data, hash_length, 0)?;

  let result = btree_plan_insert_node(engine, root_node, entry, hash_length, algo, overlay)?;

  let (new_hash, new_data) = match result {
    InsertResult::Done(hash, data) => (hash, data),
    InsertResult::Split(left_hash, split_key, right_hash) => {
      let new_root = BTreeNode::Internal(InternalNode { keys: vec![split_key], children: vec![left_hash, right_hash] });
      let new_data = new_root.serialize(hash_length)?;
      let new_hash = new_root.content_hash(hash_length, algo)?;
      overlay.insert(BTreeNodeWrite { key: new_hash.clone(), value: new_data.clone() })?;
      (new_hash, new_data)
    }
  };
  Ok((new_hash, new_data))
}

/// Apply many name-level changes through one unpublished planned-node overlay.
///
/// The returned plan contains only newly planned nodes reachable from the
/// final root. `None` means the mutations removed every entry.
pub fn btree_plan_apply(
  engine: &StorageEngine,
  root_data: &[u8],
  mut delta: BTreeMutationDelta,
  hash_length: usize,
  algo: &HashAlgorithm,
) -> EngineResult<Option<BTreeMutationPlan>> {
  if delta.upserts.is_empty() && delta.removals.is_empty() {
    return Err(EngineError::InvalidInput("B-tree mutation delta is empty".to_string()));
  }

  let mut names = std::collections::HashSet::with_capacity(delta.upserts.len() + delta.removals.len());
  for name in &delta.removals {
    if !names.insert(name.clone()) {
      return Err(EngineError::InvalidInput(format!("Duplicate B-tree mutation name: {name}")));
    }
  }
  for entry in &delta.upserts {
    if !names.insert(entry.name.clone()) {
      return Err(EngineError::InvalidInput(format!("Conflicting B-tree mutation name: {}", entry.name)));
    }
  }
  delta.removals.sort();
  delta.upserts.sort_by(|left, right| left.name.cmp(&right.name));

  let root_node = BTreeNode::deserialize(root_data, hash_length, 0)?;
  let mut current = Some((root_node.content_hash(hash_length, algo)?, root_data.to_vec()));
  let mut overlay = PlannedNodeOverlay::default();

  for name in delta.removals {
    let Some((root_hash, root_data)) = current.take() else {
      continue;
    };
    current = btree_plan_delete_node(engine, &root_hash, Some(&root_data), &name, hash_length, algo, &mut overlay)?;
  }

  for entry in delta.upserts {
    let root_data = match current.as_ref() {
      Some((_root_hash, root_data)) => root_data.clone(),
      None => BTreeNode::Leaf(LeafNode::new()).serialize(hash_length)?,
    };
    current = Some(btree_plan_insert_with_overlay(engine, &root_data, entry, hash_length, algo, &mut overlay)?);
  }

  let Some((root_hash, root_data)) = current else {
    return Ok(None);
  };
  let node_writes = overlay.reachable_writes(&root_hash, hash_length)?;
  Ok(Some(BTreeMutationPlan { root_hash, root_data, node_writes }))
}

fn btree_plan_insert_node(
  engine: &StorageEngine,
  node: BTreeNode,
  entry: ChildEntry,
  hash_length: usize,
  algo: &HashAlgorithm,
  overlay: &mut PlannedNodeOverlay,
) -> EngineResult<InsertResult> {
  match node {
    BTreeNode::Leaf(mut leaf) => {
      leaf.upsert(entry);

      if leaf.is_full() {
        let (left, split_key, right) = leaf.split();
        let left_node = BTreeNode::Leaf(left);
        let right_node = BTreeNode::Leaf(right);
        let left_hash = left_node.content_hash(hash_length, algo)?;
        let right_hash = right_node.content_hash(hash_length, algo)?;
        overlay.insert(BTreeNodeWrite { key: left_hash.clone(), value: left_node.serialize(hash_length)? })?;
        overlay.insert(BTreeNodeWrite { key: right_hash.clone(), value: right_node.serialize(hash_length)? })?;
        Ok(InsertResult::Split(left_hash, split_key, right_hash))
      } else {
        let node = BTreeNode::Leaf(leaf);
        let data = node.serialize(hash_length)?;
        let hash = node.content_hash(hash_length, algo)?;
        overlay.insert(BTreeNodeWrite { key: hash.clone(), value: data.clone() })?;
        Ok(InsertResult::Done(hash, data))
      }
    }
    BTreeNode::Internal(mut internal) => {
      let child_idx = internal.find_child_index(&entry.name);
      let child_hash = internal.children[child_idx].clone();

      // Read child node (still needs disk read)
      let (child_data, entry_version) = load_planned_btree_node(engine, overlay, &child_hash)?;
      let child_node = BTreeNode::deserialize(&child_data, hash_length, entry_version)?;

      let child_result = btree_plan_insert_node(engine, child_node, entry, hash_length, algo, overlay)?;

      match child_result {
        InsertResult::Done(new_child_hash, _) => {
          internal.children[child_idx] = new_child_hash;
          let node = BTreeNode::Internal(internal);
          let data = node.serialize(hash_length)?;
          let hash = node.content_hash(hash_length, algo)?;
          overlay.insert(BTreeNodeWrite { key: hash.clone(), value: data.clone() })?;
          Ok(InsertResult::Done(hash, data))
        }
        InsertResult::Split(left_hash, split_key, right_hash) => {
          internal.children[child_idx] = left_hash;
          internal.insert_key(split_key.clone(), right_hash);

          if internal.is_full() {
            let (left, parent_split_key, right) = internal.split();
            let left_node = BTreeNode::Internal(left);
            let right_node = BTreeNode::Internal(right);
            let left_hash = left_node.content_hash(hash_length, algo)?;
            let right_hash = right_node.content_hash(hash_length, algo)?;
            overlay.insert(BTreeNodeWrite { key: left_hash.clone(), value: left_node.serialize(hash_length)? })?;
            overlay.insert(BTreeNodeWrite { key: right_hash.clone(), value: right_node.serialize(hash_length)? })?;
            Ok(InsertResult::Split(left_hash, parent_split_key, right_hash))
          } else {
            let node = BTreeNode::Internal(internal);
            let data = node.serialize(hash_length)?;
            let hash = node.content_hash(hash_length, algo)?;
            overlay.insert(BTreeNodeWrite { key: hash.clone(), value: data.clone() })?;
            Ok(InsertResult::Done(hash, data))
          }
        }
      }
    }
  }
}

#[cfg(test)]
mod walk_bound_tests {
  use super::*;

  #[test]
  fn best_effort_warning_collection_is_bounded_and_marks_omissions() {
    let mut target = (0..BTREE_MAX_WALK_WARNINGS - 1)
      .map(|index| BTreeWalkWarning { node_hash: None, reason: format!("warning-{index}") })
      .collect::<Vec<_>>();
    let source = vec![
      BTreeWalkWarning { node_hash: None, reason: "overflow-a".to_string() },
      BTreeWalkWarning { node_hash: None, reason: "overflow-b".to_string() },
    ];

    append_bounded_warnings(&mut target, source);

    assert_eq!(target.len(), BTREE_MAX_WALK_WARNINGS);
    assert_eq!(target.last().unwrap().reason, BTREE_WARNING_OMISSION);
  }
}
