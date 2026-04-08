use crate::engine::directory_entry::{ChildEntry, serialize_child_entries, deserialize_child_entries};
use crate::engine::entry_type::EntryType;
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::storage_engine::StorageEngine;

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

/// B-tree node marker bytes for format detection.
pub const BTREE_LEAF_MARKER: u8 = 0x00;
pub const BTREE_INTERNAL_MARKER: u8 = 0x01;

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
    pub fn serialize(&self, hash_length: usize) -> Vec<u8> {
        match self {
            BTreeNode::Leaf(leaf) => {
                let child_data = serialize_child_entries(&leaf.entries, hash_length);
                let mut buffer = Vec::with_capacity(1 + 2 + child_data.len());
                buffer.push(BTREE_LEAF_MARKER);
                buffer.extend_from_slice(&(leaf.entries.len() as u16).to_le_bytes());
                buffer.extend_from_slice(&child_data);
                buffer
            }
            BTreeNode::Internal(internal) => {
                let mut buffer = Vec::new();
                buffer.push(BTREE_INTERNAL_MARKER);
                buffer.extend_from_slice(&(internal.keys.len() as u16).to_le_bytes());

                // Serialize keys
                for key in &internal.keys {
                    let key_bytes = key.as_bytes();
                    buffer.extend_from_slice(&(key_bytes.len() as u16).to_le_bytes());
                    buffer.extend_from_slice(key_bytes);
                }

                // Serialize children (keys.len() + 1 hashes)
                for child_hash in &internal.children {
                    buffer.extend_from_slice(child_hash);
                }

                buffer
            }
        }
    }

    /// Deserialize a B-tree node from bytes.
    pub fn deserialize(data: &[u8], hash_length: usize) -> EngineResult<Self> {
        if data.is_empty() {
            return Err(EngineError::CorruptEntry {
                offset: 0,
                reason: "Empty B-tree node data".to_string(),
            });
        }

        match data[0] {
            BTREE_LEAF_MARKER => {
                if data.len() < 3 {
                    return Err(EngineError::CorruptEntry {
                        offset: 0,
                        reason: "Leaf node data too short".to_string(),
                    });
                }
                let entry_count = u16::from_le_bytes([data[1], data[2]]) as usize;
                let entries = if entry_count == 0 {
                    Vec::new()
                } else {
                    deserialize_child_entries(&data[3..], hash_length)?
                };
                Ok(BTreeNode::Leaf(LeafNode { entries }))
            }
            BTREE_INTERNAL_MARKER => {
                if data.len() < 3 {
                    return Err(EngineError::CorruptEntry {
                        offset: 0,
                        reason: "Internal node data too short".to_string(),
                    });
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
                        return Err(EngineError::CorruptEntry {
                            offset: offset as u64,
                            reason: "Internal node data too short for key".to_string(),
                        });
                    }
                    let key = String::from_utf8(data[offset..offset + key_len].to_vec())
                        .map_err(|e| EngineError::CorruptEntry {
                            offset: offset as u64,
                            reason: format!("Invalid UTF-8 key: {}", e),
                        })?;
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
            other => {
                Err(EngineError::CorruptEntry {
                    offset: 0,
                    reason: format!("Unknown B-tree node type: 0x{:02x}", other),
                })
            }
        }
    }

    /// Compute the content hash for this node.
    pub fn content_hash(&self, hash_length: usize, algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
        let serialized = self.serialize(hash_length);
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
        self.entries.binary_search_by(|e| e.name.as_str().cmp(name))
            .ok()
            .map(|idx| &self.entries[idx])
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
            Err(idx) => idx,     // insertion point: go to that child
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

        let left = InternalNode {
            keys: self.keys.clone(),
            children: self.children.clone(),
        };
        let right = InternalNode {
            keys: right_keys,
            children: right_children,
        };
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
    /// Inserted without split. Returns new root hash.
    Done(Vec<u8>),
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
    let result = btree_insert_recursive(engine, root_hash, entry, hash_length, algo)?;

    match result {
        InsertResult::Done(new_root_hash) => Ok(new_root_hash),
        InsertResult::Split(left_hash, split_key, right_hash) => {
            // Root was split — create a new root
            let new_root = BTreeNode::Internal(InternalNode {
                keys: vec![split_key],
                children: vec![left_hash, right_hash],
            });
            let new_root_hash = store_btree_node(engine, &new_root, hash_length, algo)?;
            Ok(new_root_hash)
        }
    }
}

fn btree_insert_recursive(
    engine: &StorageEngine,
    node_hash: &[u8],
    entry: ChildEntry,
    hash_length: usize,
    algo: &HashAlgorithm,
) -> EngineResult<InsertResult> {
    // Load the node
    let node_data = engine.get_entry(node_hash)?
        .ok_or_else(|| EngineError::NotFound(format!(
            "B-tree node not found: {}", hex::encode(node_hash)
        )))?;
    let node = BTreeNode::deserialize(&node_data.2, hash_length)?;

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
                let new_hash = store_btree_node(engine, &BTreeNode::Leaf(leaf), hash_length, algo)?;
                Ok(InsertResult::Done(new_hash))
            }
        }
        BTreeNode::Internal(mut internal) => {
            let child_idx = internal.find_child_index(&entry.name);
            let child_hash = internal.children[child_idx].clone();

            let child_result = btree_insert_recursive(engine, &child_hash, entry, hash_length, algo)?;

            match child_result {
                InsertResult::Done(new_child_hash) => {
                    internal.children[child_idx] = new_child_hash;
                    let new_hash = store_btree_node(engine, &BTreeNode::Internal(internal), hash_length, algo)?;
                    Ok(InsertResult::Done(new_hash))
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
                        let new_hash = store_btree_node(engine, &BTreeNode::Internal(internal), hash_length, algo)?;
                        Ok(InsertResult::Done(new_hash))
                    }
                }
            }
        }
    }
}

/// Store a B-tree node in the engine and return its content hash.
pub fn store_btree_node(
    engine: &StorageEngine,
    node: &BTreeNode,
    hash_length: usize,
    algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
    let serialized = node.serialize(hash_length);
    let content_hash = node.content_hash(hash_length, algo)?;
    // Only store if not already present (content-addressed dedup)
    if !engine.has_entry(&content_hash)? {
        engine.store_entry(EntryType::DirectoryIndex, &content_hash, &serialized)?;
    }
    Ok(content_hash)
}

/// Create a new B-tree from a list of ChildEntry values.
/// Used for flat -> B-tree conversion.
pub fn btree_from_entries(
    engine: &StorageEngine,
    mut entries: Vec<ChildEntry>,
    hash_length: usize,
    algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
    // Sort entries by name
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    if entries.is_empty() {
        // Store empty leaf
        let leaf = BTreeNode::Leaf(LeafNode::new());
        return store_btree_node(engine, &leaf, hash_length, algo);
    }

    // Build leaf nodes
    let mut leaf_hashes = Vec::new();
    let mut split_keys = Vec::new();

    for chunk in entries.chunks(BTREE_MAX_LEAF_ENTRIES) {
        let leaf = BTreeNode::Leaf(LeafNode { entries: chunk.to_vec() });
        let hash = store_btree_node(engine, &leaf, hash_length, algo)?;
        if !leaf_hashes.is_empty() {
            split_keys.push(chunk[0].name.clone());
        }
        leaf_hashes.push(hash);
    }

    if leaf_hashes.len() == 1 {
        return Ok(leaf_hashes.into_iter().next().unwrap());
    }

    // Build internal nodes bottom-up
    build_internal_level(engine, leaf_hashes, split_keys, hash_length, algo)
}

fn build_internal_level(
    engine: &StorageEngine,
    children: Vec<Vec<u8>>,
    keys: Vec<String>,
    hash_length: usize,
    algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
    if children.len() == 1 {
        return Ok(children.into_iter().next().unwrap());
    }

    // Group into internal nodes
    let mut new_children = Vec::new();
    let mut new_keys = Vec::new();

    let max_children = BTREE_MAX_INTERNAL_KEYS + 1;
    let mut i = 0;
    while i < children.len() {
        let end = (i + max_children).min(children.len());
        let node_children = children[i..end].to_vec();
        let node_keys: Vec<String> = if i == 0 {
            keys[..end - 1].to_vec()
        } else {
            keys[i - 1..end - 1].to_vec()
        };

        let internal = BTreeNode::Internal(InternalNode {
            keys: node_keys,
            children: node_children,
        });
        let hash = store_btree_node(engine, &internal, hash_length, algo)?;

        if !new_children.is_empty() && i > 0 && i - 1 < keys.len() {
            new_keys.push(keys[i - 1].clone());
        }
        new_children.push(hash);

        i = end;
    }

    if new_children.len() == 1 {
        Ok(new_children.into_iter().next().unwrap())
    } else {
        build_internal_level(engine, new_children, new_keys, hash_length, algo)
    }
}

/// Look up a single child by name in a B-tree directory.
pub fn btree_lookup(
    engine: &StorageEngine,
    root_hash: &[u8],
    name: &str,
    hash_length: usize,
) -> EngineResult<Option<ChildEntry>> {
    let node_data = engine.get_entry(root_hash)?
        .ok_or_else(|| EngineError::NotFound("B-tree root not found".to_string()))?;
    let node = BTreeNode::deserialize(&node_data.2, hash_length)?;

    match node {
        BTreeNode::Leaf(leaf) => Ok(leaf.find(name).cloned()),
        BTreeNode::Internal(internal) => {
            let child_idx = internal.find_child_index(name);
            btree_lookup(engine, &internal.children[child_idx], name, hash_length)
        }
    }
}

/// List all children in a B-tree directory (in sorted order).
pub fn btree_list(
    engine: &StorageEngine,
    root_hash: &[u8],
    hash_length: usize,
) -> EngineResult<Vec<ChildEntry>> {
    let node_data = engine.get_entry(root_hash)?
        .ok_or_else(|| EngineError::NotFound("B-tree root not found".to_string()))?;
    let node = BTreeNode::deserialize(&node_data.2, hash_length)?;

    match node {
        BTreeNode::Leaf(leaf) => Ok(leaf.entries),
        BTreeNode::Internal(internal) => {
            let mut all_entries = Vec::new();
            for child_hash in &internal.children {
                let child_entries = btree_list(engine, child_hash, hash_length)?;
                all_entries.extend(child_entries);
            }
            Ok(all_entries)
        }
    }
}

/// List all children starting from a serialized root node.
/// Used when the caller already has the root node data (e.g., from a path-keyed entry).
pub fn btree_list_from_node(
    root_data: &[u8],
    engine: &StorageEngine,
    hash_length: usize,
) -> EngineResult<Vec<ChildEntry>> {
    let node = BTreeNode::deserialize(root_data, hash_length)?;
    match node {
        BTreeNode::Leaf(leaf) => Ok(leaf.entries),
        BTreeNode::Internal(internal) => {
            let mut all = Vec::new();
            for child_hash in &internal.children {
                let entries = btree_list(engine, child_hash, hash_length)?;
                all.extend(entries);
            }
            Ok(all)
        }
    }
}

/// Delete a child from a B-tree directory.
/// Returns the new root hash, or None if the tree is now empty.
pub fn btree_delete(
    engine: &StorageEngine,
    root_hash: &[u8],
    name: &str,
    hash_length: usize,
    algo: &HashAlgorithm,
) -> EngineResult<Option<Vec<u8>>> {
    let node_data = engine.get_entry(root_hash)?
        .ok_or_else(|| EngineError::NotFound("B-tree root not found".to_string()))?;
    let mut node = BTreeNode::deserialize(&node_data.2, hash_length)?;

    match &mut node {
        BTreeNode::Leaf(ref mut leaf) => {
            leaf.remove(name);
            if leaf.entries.is_empty() {
                Ok(None) // tree is empty
            } else {
                let new_hash = store_btree_node(engine, &node, hash_length, algo)?;
                Ok(Some(new_hash))
            }
        }
        BTreeNode::Internal(ref mut internal) => {
            let child_idx = internal.find_child_index(name);
            let child_hash = internal.children[child_idx].clone();

            match btree_delete(engine, &child_hash, name, hash_length, algo)? {
                Some(new_child_hash) => {
                    internal.children[child_idx] = new_child_hash;
                    let new_hash = store_btree_node(engine, &node, hash_length, algo)?;
                    Ok(Some(new_hash))
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
                        Ok(Some(internal.children[0].clone()))
                    } else {
                        let new_hash = store_btree_node(engine, &node, hash_length, algo)?;
                        Ok(Some(new_hash))
                    }
                }
            }
        }
    }
}
