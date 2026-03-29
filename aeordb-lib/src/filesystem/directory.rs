use std::sync::Arc;

use crate::storage::{Chunk, ChunkConfig, ChunkHash, ChunkStorage, ChunkStoreError};

use super::btree_node::{BTreeNode, BranchNode, LeafNode, BTREE_FORMAT_VERSION};
use super::index_entry::IndexEntry;

/// Default minimum degree for B-tree nodes.
/// Each node can hold at most (2*t - 1) keys and at least (t - 1) keys (except root).
const DEFAULT_MINIMUM_DEGREE: usize = 32;

pub struct Directory {
  storage: Arc<dyn ChunkStorage>,
  #[allow(dead_code)]
  chunk_config: ChunkConfig,
  minimum_degree: usize,
}

impl Directory {
  pub fn new(storage: Arc<dyn ChunkStorage>, chunk_config: ChunkConfig) -> Self {
    Self {
      storage,
      chunk_config,
      minimum_degree: DEFAULT_MINIMUM_DEGREE,
    }
  }

  pub fn with_minimum_degree(
    storage: Arc<dyn ChunkStorage>,
    chunk_config: ChunkConfig,
    minimum_degree: usize,
  ) -> Self {
    Self {
      storage,
      chunk_config,
      minimum_degree,
    }
  }

  fn max_keys(&self) -> usize {
    2 * self.minimum_degree - 1
  }

  fn min_keys(&self) -> usize {
    self.minimum_degree - 1
  }

  /// Write a BTreeNode to storage as a chunk, returning its hash.
  fn write_node(&self, node: &BTreeNode) -> Result<ChunkHash, ChunkStoreError> {
    let data = node.serialize()?;
    let chunk = Chunk::new(data);
    let hash = chunk.hash;
    self.storage.store_chunk(&chunk)?;
    Ok(hash)
  }

  /// Read a BTreeNode from storage by its chunk hash.
  fn read_node(&self, hash: &ChunkHash) -> Result<BTreeNode, ChunkStoreError> {
    let chunk = self.storage.get_chunk(hash)?
      .ok_or(ChunkStoreError::ChunkNotFound(*hash))?;
    BTreeNode::deserialize(&chunk.data)
  }

  /// Create an empty directory (a single empty leaf node). Returns the root chunk hash.
  pub fn create_empty(&self) -> Result<ChunkHash, ChunkStoreError> {
    let leaf = BTreeNode::Leaf(LeafNode::new());
    self.write_node(&leaf)
  }

  /// Get an entry by name from the B-tree rooted at `root`.
  pub fn get(&self, root: &ChunkHash, name: &str) -> Result<Option<IndexEntry>, ChunkStoreError> {
    let node = self.read_node(root)?;
    self.search_node(&node, name)
  }

  fn search_node(
    &self,
    node: &BTreeNode,
    name: &str,
  ) -> Result<Option<IndexEntry>, ChunkStoreError> {
    match node {
      BTreeNode::Leaf(leaf) => {
        for entry in &leaf.entries {
          if entry.name == name {
            return Ok(Some(entry.clone()));
          }
        }
        Ok(None)
      }
      BTreeNode::Branch(branch) => {
        let child_index = self.find_child_index(&branch.keys, name);
        let child_node = self.read_node(&branch.children[child_index])?;
        self.search_node(&child_node, name)
      }
    }
  }

  /// Find which child to descend into for a given key.
  /// Returns the index i such that keys[i-1] <= name < keys[i].
  fn find_child_index(&self, keys: &[String], name: &str) -> usize {
    keys.iter().position(|key| name < key.as_str()).unwrap_or(keys.len())
  }

  /// Insert an entry into the B-tree. Returns a new root hash (COW).
  /// If an entry with the same name already exists, it is replaced.
  pub fn insert(
    &self,
    root: &ChunkHash,
    entry: IndexEntry,
  ) -> Result<ChunkHash, ChunkStoreError> {
    let root_node = self.read_node(root)?;

    // Check if root is full; if so, split it first.
    if self.node_is_full(&root_node) {
      // Create a new root branch with the old root as its only child.
      let mut new_root_branch = BranchNode::new();
      new_root_branch.children.push(*root);

      // Split the child (old root) at index 0.
      let new_root_branch = self.split_child(new_root_branch, 0)?;

      // Now insert into the non-full new root.
      let result_branch = self.insert_non_full_branch(new_root_branch, entry)?;
      let new_root = BTreeNode::Branch(result_branch);
      self.write_node(&new_root)
    } else {
      let new_root = self.insert_non_full(root_node, entry)?;
      self.write_node(&new_root)
    }
  }

  fn node_is_full(&self, node: &BTreeNode) -> bool {
    match node {
      BTreeNode::Leaf(leaf) => leaf.entries.len() >= self.max_keys(),
      BTreeNode::Branch(branch) => branch.keys.len() >= self.max_keys(),
    }
  }

  /// Insert into a node that is guaranteed not full.
  fn insert_non_full(
    &self,
    node: BTreeNode,
    entry: IndexEntry,
  ) -> Result<BTreeNode, ChunkStoreError> {
    match node {
      BTreeNode::Leaf(mut leaf) => {
        // Check for existing entry with same name (overwrite).
        if let Some(position) = leaf.entries.iter().position(|existing| existing.name == entry.name) {
          leaf.entries[position] = entry;
        } else {
          // Insert in sorted order.
          let position = leaf.entries.iter()
            .position(|existing| entry.name < existing.name)
            .unwrap_or(leaf.entries.len());
          leaf.entries.insert(position, entry);
        }
        Ok(BTreeNode::Leaf(leaf))
      }
      BTreeNode::Branch(branch) => {
        let result = self.insert_non_full_branch(branch, entry)?;
        Ok(BTreeNode::Branch(result))
      }
    }
  }

  /// Insert into a branch node that is guaranteed not full.
  fn insert_non_full_branch(
    &self,
    mut branch: BranchNode,
    entry: IndexEntry,
  ) -> Result<BranchNode, ChunkStoreError> {
    let child_index = self.find_child_index(&branch.keys, &entry.name);

    // Check if the key already exists at this branch level (exact match with a separator key).
    // If so, we still descend into the appropriate child.

    let child_node = self.read_node(&branch.children[child_index])?;

    if self.node_is_full(&child_node) {
      // Split the full child first.
      branch = self.split_child(branch, child_index)?;

      // After splitting, determine which of the two children to descend into.
      let new_child_index = if entry.name > branch.keys[child_index] {
        child_index + 1
      } else {
        child_index
      };

      let child_node = self.read_node(&branch.children[new_child_index])?;
      let new_child = self.insert_non_full(child_node, entry)?;
      let new_child_hash = self.write_node(&new_child)?;
      branch.children[new_child_index] = new_child_hash;
    } else {
      let new_child = self.insert_non_full(child_node, entry)?;
      let new_child_hash = self.write_node(&new_child)?;
      branch.children[child_index] = new_child_hash;
    }

    Ok(branch)
  }

  /// Split the child at `child_index` in the given branch. Returns the modified branch.
  /// The child must be full. After splitting, the branch gets one more key and one more child.
  fn split_child(
    &self,
    mut branch: BranchNode,
    child_index: usize,
  ) -> Result<BranchNode, ChunkStoreError> {
    let child_node = self.read_node(&branch.children[child_index])?;

    match child_node {
      BTreeNode::Leaf(leaf) => {
        let mid = leaf.entries.len() / 2;
        let median_key = leaf.entries[mid].name.clone();

        let left_entries = leaf.entries[..mid].to_vec();
        let right_entries = leaf.entries[mid..].to_vec();

        let left_leaf = BTreeNode::Leaf(LeafNode {
          format_version: BTREE_FORMAT_VERSION,
          entries: left_entries,
        });
        let right_leaf = BTreeNode::Leaf(LeafNode {
          format_version: BTREE_FORMAT_VERSION,
          entries: right_entries,
        });

        let left_hash = self.write_node(&left_leaf)?;
        let right_hash = self.write_node(&right_leaf)?;

        branch.keys.insert(child_index, median_key);
        branch.children[child_index] = left_hash;
        branch.children.insert(child_index + 1, right_hash);
      }
      BTreeNode::Branch(child_branch) => {
        let mid = child_branch.keys.len() / 2;
        let median_key = child_branch.keys[mid].clone();

        let left_keys = child_branch.keys[..mid].to_vec();
        let right_keys = child_branch.keys[mid + 1..].to_vec();
        let left_children = child_branch.children[..=mid].to_vec();
        let right_children = child_branch.children[mid + 1..].to_vec();

        let left_branch = BTreeNode::Branch(BranchNode {
          format_version: BTREE_FORMAT_VERSION,
          keys: left_keys,
          children: left_children,
        });
        let right_branch = BTreeNode::Branch(BranchNode {
          format_version: BTREE_FORMAT_VERSION,
          keys: right_keys,
          children: right_children,
        });

        let left_hash = self.write_node(&left_branch)?;
        let right_hash = self.write_node(&right_branch)?;

        branch.keys.insert(child_index, median_key);
        branch.children[child_index] = left_hash;
        branch.children.insert(child_index + 1, right_hash);
      }
    }

    Ok(branch)
  }

  /// Remove an entry by name. Returns (new_root_hash, Option<removed_entry>). COW.
  pub fn remove(
    &self,
    root: &ChunkHash,
    name: &str,
  ) -> Result<(ChunkHash, Option<IndexEntry>), ChunkStoreError> {
    let root_node = self.read_node(root)?;
    let (new_node, removed) = self.remove_from_node(root_node, name)?;

    // If root is a branch with no keys, collapse to its only child.
    let final_node = match &new_node {
      BTreeNode::Branch(branch) if branch.keys.is_empty() && branch.children.len() == 1 => {
        self.read_node(&branch.children[0])?
      }
      _ => new_node,
    };

    let new_root_hash = self.write_node(&final_node)?;
    Ok((new_root_hash, removed))
  }

  fn remove_from_node(
    &self,
    node: BTreeNode,
    name: &str,
  ) -> Result<(BTreeNode, Option<IndexEntry>), ChunkStoreError> {
    match node {
      BTreeNode::Leaf(mut leaf) => {
        if let Some(position) = leaf.entries.iter().position(|entry| entry.name == name) {
          let removed = leaf.entries.remove(position);
          Ok((BTreeNode::Leaf(leaf), Some(removed)))
        } else {
          Ok((BTreeNode::Leaf(leaf), None))
        }
      }
      BTreeNode::Branch(branch) => self.remove_from_branch(branch, name),
    }
  }

  fn remove_from_branch(
    &self,
    mut branch: BranchNode,
    name: &str,
  ) -> Result<(BTreeNode, Option<IndexEntry>), ChunkStoreError> {
    let child_index = self.find_child_index(&branch.keys, name);

    let child_node = self.read_node(&branch.children[child_index])?;

    // Check if the child has enough keys to descend into it.
    let child_key_count = self.node_key_count(&child_node);

    if child_key_count <= self.min_keys() {
      // Need to ensure the child has enough keys before descending.
      branch = self.ensure_child_has_enough_keys(branch, child_index)?;
      // After rebalancing, the child_index might have changed. Re-find it.
      let new_child_index = self.find_child_index(&branch.keys, name);
      let child_node = self.read_node(&branch.children[new_child_index])?;
      let (new_child, removed) = self.remove_from_node(child_node, name)?;
      let new_child_hash = self.write_node(&new_child)?;
      branch.children[new_child_index] = new_child_hash;
      Ok((BTreeNode::Branch(branch), removed))
    } else {
      let (new_child, removed) = self.remove_from_node(child_node, name)?;
      let new_child_hash = self.write_node(&new_child)?;
      branch.children[child_index] = new_child_hash;
      Ok((BTreeNode::Branch(branch), removed))
    }
  }

  fn node_key_count(&self, node: &BTreeNode) -> usize {
    match node {
      BTreeNode::Leaf(leaf) => leaf.entries.len(),
      BTreeNode::Branch(branch) => branch.keys.len(),
    }
  }

  /// Ensure that branch.children[child_index] has at least `min_keys + 1` keys
  /// by borrowing from a sibling or merging.
  fn ensure_child_has_enough_keys(
    &self,
    mut branch: BranchNode,
    child_index: usize,
  ) -> Result<BranchNode, ChunkStoreError> {
    // Try to borrow from left sibling.
    if child_index > 0 {
      let left_sibling = self.read_node(&branch.children[child_index - 1])?;
      if self.node_key_count(&left_sibling) > self.min_keys() {
        return self.borrow_from_left_sibling(&mut branch, child_index);
      }
    }

    // Try to borrow from right sibling.
    if child_index < branch.children.len() - 1 {
      let right_sibling = self.read_node(&branch.children[child_index + 1])?;
      if self.node_key_count(&right_sibling) > self.min_keys() {
        return self.borrow_from_right_sibling(&mut branch, child_index);
      }
    }

    // Merge with a sibling.
    if child_index > 0 {
      // Merge with left sibling (merge child_index - 1 and child_index).
      self.merge_children(&mut branch, child_index - 1)
    } else {
      // Merge with right sibling.
      self.merge_children(&mut branch, child_index)
    }
  }

  fn borrow_from_left_sibling(
    &self,
    branch: &mut BranchNode,
    child_index: usize,
  ) -> Result<BranchNode, ChunkStoreError> {
    let left_node = self.read_node(&branch.children[child_index - 1])?;
    let child_node = self.read_node(&branch.children[child_index])?;
    let separator_key = branch.keys[child_index - 1].clone();

    match (left_node, child_node) {
      (BTreeNode::Leaf(mut left_leaf), BTreeNode::Leaf(mut child_leaf)) => {
        let borrowed_entry = left_leaf.entries.pop().unwrap();
        let new_separator = borrowed_entry.name.clone();
        child_leaf.entries.insert(0, borrowed_entry);
        branch.keys[child_index - 1] = new_separator;

        branch.children[child_index - 1] = self.write_node(&BTreeNode::Leaf(left_leaf))?;
        branch.children[child_index] = self.write_node(&BTreeNode::Leaf(child_leaf))?;
      }
      (BTreeNode::Branch(mut left_branch), BTreeNode::Branch(mut child_branch)) => {
        let borrowed_key = left_branch.keys.pop().unwrap();
        let borrowed_child = left_branch.children.pop().unwrap();

        child_branch.keys.insert(0, separator_key);
        child_branch.children.insert(0, borrowed_child);
        branch.keys[child_index - 1] = borrowed_key;

        branch.children[child_index - 1] = self.write_node(&BTreeNode::Branch(left_branch))?;
        branch.children[child_index] = self.write_node(&BTreeNode::Branch(child_branch))?;
      }
      _ => {
        return Err(ChunkStoreError::SerializationError(
          "mismatched node types during left sibling borrow".to_string(),
        ));
      }
    }

    Ok(branch.clone())
  }

  fn borrow_from_right_sibling(
    &self,
    branch: &mut BranchNode,
    child_index: usize,
  ) -> Result<BranchNode, ChunkStoreError> {
    let child_node = self.read_node(&branch.children[child_index])?;
    let right_node = self.read_node(&branch.children[child_index + 1])?;
    let separator_key = branch.keys[child_index].clone();

    match (child_node, right_node) {
      (BTreeNode::Leaf(mut child_leaf), BTreeNode::Leaf(mut right_leaf)) => {
        let borrowed_entry = right_leaf.entries.remove(0);
        let new_separator = if right_leaf.entries.is_empty() {
          borrowed_entry.name.clone()
        } else {
          right_leaf.entries[0].name.clone()
        };
        child_leaf.entries.push(borrowed_entry);
        branch.keys[child_index] = new_separator;

        branch.children[child_index] = self.write_node(&BTreeNode::Leaf(child_leaf))?;
        branch.children[child_index + 1] = self.write_node(&BTreeNode::Leaf(right_leaf))?;
      }
      (BTreeNode::Branch(mut child_branch), BTreeNode::Branch(mut right_branch)) => {
        let borrowed_key = right_branch.keys.remove(0);
        let borrowed_child = right_branch.children.remove(0);

        child_branch.keys.push(separator_key);
        child_branch.children.push(borrowed_child);
        branch.keys[child_index] = borrowed_key;

        branch.children[child_index] = self.write_node(&BTreeNode::Branch(child_branch))?;
        branch.children[child_index + 1] = self.write_node(&BTreeNode::Branch(right_branch))?;
      }
      _ => {
        return Err(ChunkStoreError::SerializationError(
          "mismatched node types during right sibling borrow".to_string(),
        ));
      }
    }

    Ok(branch.clone())
  }

  /// Merge children[merge_index] and children[merge_index + 1] into one node.
  /// Removes the separator key from the branch.
  fn merge_children(
    &self,
    branch: &mut BranchNode,
    merge_index: usize,
  ) -> Result<BranchNode, ChunkStoreError> {
    let left_node = self.read_node(&branch.children[merge_index])?;
    let right_node = self.read_node(&branch.children[merge_index + 1])?;
    let separator_key = branch.keys.remove(merge_index);

    let merged_node = match (left_node, right_node) {
      (BTreeNode::Leaf(mut left_leaf), BTreeNode::Leaf(right_leaf)) => {
        left_leaf.entries.extend(right_leaf.entries);
        BTreeNode::Leaf(left_leaf)
      }
      (BTreeNode::Branch(mut left_branch), BTreeNode::Branch(right_branch)) => {
        left_branch.keys.push(separator_key);
        left_branch.keys.extend(right_branch.keys);
        left_branch.children.extend(right_branch.children);
        BTreeNode::Branch(left_branch)
      }
      _ => {
        return Err(ChunkStoreError::SerializationError(
          "mismatched node types during merge".to_string(),
        ));
      }
    };

    let merged_hash = self.write_node(&merged_node)?;
    branch.children[merge_index] = merged_hash;
    branch.children.remove(merge_index + 1);

    Ok(branch.clone())
  }

  /// List all entries in the B-tree (in-order traversal of leaf nodes).
  pub fn list(&self, root: &ChunkHash) -> Result<Vec<IndexEntry>, ChunkStoreError> {
    let mut results = Vec::new();
    let node = self.read_node(root)?;
    self.collect_entries(&node, &mut results)?;
    Ok(results)
  }

  fn collect_entries(
    &self,
    node: &BTreeNode,
    results: &mut Vec<IndexEntry>,
  ) -> Result<(), ChunkStoreError> {
    match node {
      BTreeNode::Leaf(leaf) => {
        results.extend(leaf.entries.iter().cloned());
      }
      BTreeNode::Branch(branch) => {
        for child_hash in &branch.children {
          let child_node = self.read_node(child_hash)?;
          self.collect_entries(&child_node, results)?;
        }
      }
    }
    Ok(())
  }

  /// List entries whose names fall in the range [start, end) (lexicographic).
  pub fn list_range(
    &self,
    root: &ChunkHash,
    start: &str,
    end: &str,
  ) -> Result<Vec<IndexEntry>, ChunkStoreError> {
    let mut results = Vec::new();
    let node = self.read_node(root)?;
    self.collect_entries_in_range(&node, start, end, &mut results)?;
    Ok(results)
  }

  fn collect_entries_in_range(
    &self,
    node: &BTreeNode,
    start: &str,
    end: &str,
    results: &mut Vec<IndexEntry>,
  ) -> Result<(), ChunkStoreError> {
    match node {
      BTreeNode::Leaf(leaf) => {
        for entry in &leaf.entries {
          if entry.name.as_str() >= start && entry.name.as_str() < end {
            results.push(entry.clone());
          }
        }
      }
      BTreeNode::Branch(branch) => {
        for (index, child_hash) in branch.children.iter().enumerate() {
          // Determine if this child's range could overlap [start, end).
          let child_min_could_overlap = if index < branch.keys.len() {
            branch.keys[index].as_str() > start
          } else {
            true
          };
          let child_max_could_overlap = if index > 0 {
            branch.keys[index - 1].as_str() < end
          } else {
            true
          };

          if child_min_could_overlap || child_max_could_overlap {
            let child_node = self.read_node(child_hash)?;
            self.collect_entries_in_range(&child_node, start, end, results)?;
          }
        }
      }
    }
    Ok(())
  }

  /// Count all entries in the B-tree.
  pub fn count(&self, root: &ChunkHash) -> Result<u64, ChunkStoreError> {
    let node = self.read_node(root)?;
    self.count_entries(&node)
  }

  fn count_entries(&self, node: &BTreeNode) -> Result<u64, ChunkStoreError> {
    match node {
      BTreeNode::Leaf(leaf) => Ok(leaf.entries.len() as u64),
      BTreeNode::Branch(branch) => {
        let mut total = 0u64;
        for child_hash in &branch.children {
          let child_node = self.read_node(child_hash)?;
          total += self.count_entries(&child_node)?;
        }
        Ok(total)
      }
    }
  }
}
