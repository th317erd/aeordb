# Content-Addressed B-Tree Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace flat directory child lists with content-addressed B-tree nodes so directory mutations touch O(log N) entries instead of O(N) bytes.

**Architecture:** Each B-tree node (leaf or internal) is stored as its own content-addressed entry in the storage engine. Leaf nodes hold sorted ChildEntry data (~40 per node). Internal nodes hold sorted keys + child node hashes (~77 per node). Mutations only rewrite the path from leaf to root. Unmodified subtrees are shared across versions via structural sharing.

**Tech Stack:** Rust, BLAKE3 (content hashing), existing StorageEngine entry API

**Spec:** `bot-docs/plan/large-directory-btree.md`

---

### Task 1: B-tree node types + serialization

**Files:**
- Create: `aeordb-lib/src/engine/btree.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Create: `aeordb-lib/spec/engine/btree_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Create btree.rs with node types**

```rust
use crate::engine::directory_entry::{ChildEntry, serialize_child_entries, deserialize_child_entries};
use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;

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
```

- [ ] **Step 2: Register module and export**

In `aeordb-lib/src/engine/mod.rs`, add:
```rust
pub mod btree;
pub use btree::{
    BTreeNode, LeafNode, InternalNode,
    BTREE_MAX_LEAF_ENTRIES, BTREE_MIN_LEAF_ENTRIES,
    BTREE_MAX_INTERNAL_KEYS, BTREE_MIN_INTERNAL_KEYS,
    BTREE_CONVERSION_THRESHOLD, is_btree_format,
};
```

- [ ] **Step 3: Write tests**

Create `aeordb-lib/spec/engine/btree_spec.rs` with tests:

```rust
// Serialization tests:
// 1. test_leaf_serialize_deserialize — empty leaf round-trip
// 2. test_leaf_with_entries — leaf with 5 entries round-trip
// 3. test_internal_serialize_deserialize — internal node round-trip
// 4. test_content_hash_deterministic — same node → same hash
// 5. test_content_hash_differs — different entries → different hash

// LeafNode operation tests:
// 6. test_leaf_find — find by name
// 7. test_leaf_find_missing — name not present → None
// 8. test_leaf_upsert_insert — insert new entry
// 9. test_leaf_upsert_update — update existing entry
// 10. test_leaf_upsert_maintains_sort — entries stay sorted after insert
// 11. test_leaf_remove — remove by name
// 12. test_leaf_remove_missing — remove non-existent → false
// 13. test_leaf_is_full — at BTREE_MAX_LEAF_ENTRIES
// 14. test_leaf_split — split produces two halves with correct split key
// 15. test_leaf_split_entries_sorted — both halves remain sorted

// InternalNode operation tests:
// 16. test_internal_find_child_index — correct child for various keys
// 17. test_internal_insert_key — insert maintains sorted order
// 18. test_internal_split — split produces correct halves
// 19. test_internal_is_full — at BTREE_MAX_INTERNAL_KEYS

// Format detection tests:
// 20. test_is_btree_format_leaf — 0x00 prefix detected
// 21. test_is_btree_format_internal — 0x01 prefix detected
// 22. test_is_btree_format_flat — 0x02+ not detected (flat list)
// 23. test_is_btree_format_empty — empty data → false
```

Register in Cargo.toml.

- [ ] **Step 4: Build and test**

Run: `cargo build && cargo test --test btree_spec`

- [ ] **Step 5: Commit**

```bash
git commit -m "B-tree node types, serialization, leaf/internal operations"
```

---

### Task 2: B-tree insert + store operations

**Files:**
- Modify: `aeordb-lib/src/engine/btree.rs`
- Modify: `aeordb-lib/spec/engine/btree_spec.rs`

- [ ] **Step 1: Add B-tree insert function**

Add to `btree.rs`:

```rust
use crate::engine::entry_type::EntryType;
use crate::engine::storage_engine::StorageEngine;

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
        .ok_or_else(|| EngineError::NotFound(format!("B-tree node not found: {}", hex::encode(node_hash))))?;
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
            let child_hash = &internal.children[child_idx];

            let child_result = btree_insert_recursive(engine, child_hash, entry, hash_length, algo)?;

            match child_result {
                InsertResult::Done(new_child_hash) => {
                    internal.children[child_idx] = new_child_hash;
                    let new_hash = store_btree_node(engine, &BTreeNode::Internal(internal), hash_length, algo)?;
                    Ok(InsertResult::Done(new_hash))
                }
                InsertResult::Split(left_hash, split_key, right_hash) => {
                    internal.children[child_idx] = left_hash;
                    internal.insert_key(split_key.clone(), right_hash);

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
fn store_btree_node(
    engine: &StorageEngine,
    node: &BTreeNode,
    hash_length: usize,
    algo: &HashAlgorithm,
) -> EngineResult<Vec<u8>> {
    let serialized = node.serialize(hash_length);
    let content_hash = node.content_hash(hash_length, algo)?;
    engine.store_entry(EntryType::DirectoryIndex, &content_hash, &serialized)?;
    Ok(content_hash)
}

/// Create a new B-tree from a list of ChildEntry values.
/// Used for flat → B-tree conversion.
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
```

- [ ] **Step 2: Add lookup and list functions**

```rust
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
```

- [ ] **Step 3: Add delete function**

```rust
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
                    // For simplicity in v1, just remove the child and corresponding key
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
```

- [ ] **Step 4: Write integration tests**

Add to `btree_spec.rs`:

```rust
// Using real StorageEngine:
// 24. test_btree_insert_single — insert one entry, lookup succeeds
// 25. test_btree_insert_multiple — insert 10 entries, all findable
// 26. test_btree_insert_sorted_order — list returns sorted
// 27. test_btree_insert_causes_split — insert > MAX_LEAF entries, tree has 2 levels
// 28. test_btree_insert_update — insert same name twice, second value wins
// 29. test_btree_lookup_missing — lookup non-existent → None
// 30. test_btree_list_empty — empty tree → empty list
// 31. test_btree_delete — insert then delete, lookup → None
// 32. test_btree_delete_missing — delete non-existent, no error
// 33. test_btree_from_entries — bulk build from 100 entries
// 34. test_btree_from_entries_sorted — bulk build, list returns sorted
// 35. test_btree_large_directory — insert 1000 entries, all findable, list correct
// 36. test_btree_structural_sharing — insert into tree, old root still valid
// 37. test_btree_content_hash_changes — insert changes root hash
// 38. test_btree_delete_to_empty — delete all entries → None root
```

- [ ] **Step 5: Build and test**

Run: `cargo test --test btree_spec`

- [ ] **Step 6: Commit**

```bash
git commit -m "B-tree insert, lookup, list, delete with structural sharing"
```

---

### Task 3: Integrate B-tree into DirectoryOps

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`
- Create: `aeordb-lib/spec/engine/btree_directory_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Update list_directory for B-tree format**

In `directory_ops.rs`, modify `list_directory`:

```rust
pub fn list_directory(&self, path: &str) -> EngineResult<Vec<ChildEntry>> {
    let normalized = normalize_path(path);
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let dir_key = directory_path_hash(&normalized, &algo)?;
    match self.engine.get_entry(&dir_key)? {
        Some((_header, _key, value)) => {
            if value.is_empty() {
                return Ok(Vec::new());
            }
            if crate::engine::btree::is_btree_format(&value) {
                // B-tree format: the value is a root node, but we need the root hash
                // to walk the tree. The path-based entry stores the root node directly.
                // Walk from this node.
                crate::engine::btree::btree_list_from_node(&value, self.engine, hash_length)
            } else {
                // Flat format
                deserialize_child_entries(&value, hash_length)
            }
        }
        None => Err(EngineError::NotFound(normalized)),
    }
}
```

Wait — there's a subtlety. The path-based directory entry currently stores the serialized children directly. With B-tree, it should store the B-tree root node's data. But to walk the tree, we need the children's hashes, which point to OTHER entries.

Actually, the simpler approach: the path-based entry stores the root node's content hash (just the hash, not the data). Then listing reads the hash and walks the tree via `btree_list`. This aligns with how content-addressed entries work.

OR: the path-based entry stores the root node's DATA (same bytes as the content-addressed entry). This way listing doesn't need an extra lookup.

Let me go with storing the root node data at the path key (same as today — the path key's value IS the directory data). For flat lists, it's the child list. For B-tree, it's the root node. The root node's children reference other content-addressed nodes.

Add to btree.rs:
```rust
/// List all children starting from a serialized root node.
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
```

- [ ] **Step 2: Update update_parent_directories for B-tree**

This is the critical change. When the parent directory has > BTREE_CONVERSION_THRESHOLD children, use B-tree operations:

```rust
fn update_parent_directories(&self, child_path: &str, child_entry: ChildEntry) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let parent = match parent_path(child_path) {
        Some(parent) => parent,
        None => return Ok(()),
    };

    let dir_key = directory_path_hash(&parent, &algo)?;

    // Read existing directory
    let existing = self.engine.get_entry(&dir_key)?;

    let (dir_value, content_key) = match existing {
        Some((_header, _key, value)) if !value.is_empty() && is_btree_format(&value) => {
            // B-tree format: insert into the tree
            let root_node = BTreeNode::deserialize(&value, hash_length)?;
            let root_hash = root_node.content_hash(hash_length, &algo)?;
            let new_root_hash = btree_insert(self.engine, &root_hash, child_entry, hash_length, &algo)?;

            // Load the new root node data for the path-based entry
            let new_root_data = self.engine.get_entry(&new_root_hash)?
                .ok_or_else(|| EngineError::NotFound("New B-tree root not found".to_string()))?;

            (new_root_data.2, new_root_hash)
        }
        Some((_header, _key, value)) => {
            // Flat format
            let mut children = if value.is_empty() {
                Vec::new()
            } else {
                deserialize_child_entries(&value, hash_length)?
            };

            // Add or update
            let child_name = &child_entry.name;
            if let Some(existing) = children.iter_mut().find(|c| c.name == *child_name) {
                *existing = child_entry;
            } else {
                children.push(child_entry);
            }

            // Check if we should convert to B-tree
            if children.len() >= BTREE_CONVERSION_THRESHOLD {
                let root_hash = btree_from_entries(self.engine, children, hash_length, &algo)?;
                let root_data = self.engine.get_entry(&root_hash)?
                    .ok_or_else(|| EngineError::NotFound("B-tree root not found".to_string()))?;
                (root_data.2, root_hash)
            } else {
                let dir_value = serialize_child_entries(&children, hash_length);
                let content_key = directory_content_hash(&dir_value, &algo)?;
                self.engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
                (dir_value, content_key)
            }
        }
        None => {
            // New directory
            let mut children = vec![child_entry];
            let dir_value = serialize_child_entries(&children, hash_length);
            let content_key = directory_content_hash(&dir_value, &algo)?;
            self.engine.store_entry(EntryType::DirectoryIndex, &content_key, &dir_value)?;
            (dir_value, content_key)
        }
    };

    // Store at path-based key
    self.engine.store_entry(EntryType::DirectoryIndex, &dir_key, &dir_value)?;

    // HEAD update for root
    if parent == "/" {
        self.engine.update_head(&content_key)?;
        return Ok(());
    }

    // Recurse to grandparent
    let parent_child = ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key,
        total_size: dir_value.len() as u64,
        created_at: chrono::Utc::now().timestamp_millis(),
        updated_at: chrono::Utc::now().timestamp_millis(),
        name: file_name(&parent).unwrap_or("").to_string(),
        content_type: None,
    };

    self.update_parent_directories(&parent, parent_child)
}
```

- [ ] **Step 3: Update remove_from_parent_directory for B-tree**

Similar pattern — detect format, use btree_delete for B-tree, flat removal for flat lists.

- [ ] **Step 4: Write integration tests**

Create `aeordb-lib/spec/engine/btree_directory_spec.rs`:

```rust
// 1. test_small_directory_stays_flat — < 256 files, directory is flat list
// 2. test_large_directory_converts_to_btree — at 256 files, format changes
// 3. test_btree_directory_list — list_directory works after conversion
// 4. test_btree_directory_add_file — add file to B-tree directory
// 5. test_btree_directory_delete_file — delete from B-tree directory
// 6. test_btree_directory_overwrite_file — overwrite in B-tree directory
// 7. test_btree_directory_snapshot — snapshot after B-tree conversion, walk returns correct files
// 8. test_btree_directory_two_snapshots — two snapshots with different B-tree states
// 9. test_mixed_format_coexistence — small dir flat, large dir B-tree, both work
// 10. test_btree_directory_performance — 1000 files, verify insert is fast
```

- [ ] **Step 5: Build and test all**

Run: `cargo test`

- [ ] **Step 6: Commit**

```bash
git commit -m "Integrate B-tree into DirectoryOps: auto-convert at 256 entries"
```

---

### Task 4: Update tree walker + backup for B-tree

**Files:**
- Modify: `aeordb-lib/src/engine/tree_walker.rs`
- Modify: `aeordb-lib/src/engine/backup.rs`
- Modify: `aeordb-lib/spec/engine/btree_directory_spec.rs`

- [ ] **Step 1: Update tree walker to handle B-tree nodes**

The tree walker follows `ChildEntry.hash` for directories. With B-tree directories, the hash points to a B-tree root node (which could be a leaf or internal node). The walker needs to detect this and walk the B-tree to enumerate children.

In `tree_walker.rs`, update `walk_directory`:

```rust
fn walk_directory(
    engine: &StorageEngine,
    dir_hash: &[u8],
    current_path: &str,
    hash_length: usize,
    tree: &mut VersionTree,
) -> EngineResult<()> {
    let dir_data = match engine.get_entry(dir_hash)? {
        Some((_header, _key, value)) => value,
        None => return Ok(()),
    };

    tree.directories.insert(current_path.to_string(), (dir_hash.to_vec(), dir_data.clone()));

    // Get children — handle both flat and B-tree formats
    let children = if crate::engine::btree::is_btree_format(&dir_data) {
        crate::engine::btree::btree_list_from_node(&dir_data, engine, hash_length)?
    } else {
        deserialize_child_entries(&dir_data, hash_length)?
    };

    // ... rest of walk_directory (iterate children, recurse) unchanged
}
```

- [ ] **Step 2: Update backup export to include B-tree nodes**

The export operation walks the tree and copies entries. B-tree internal nodes are stored as DirectoryIndex entries — they'll be included automatically if the walker follows all hashes. But we need to make sure ALL B-tree nodes (not just the root) are copied.

Add a helper that collects all B-tree node hashes recursively:

```rust
fn collect_btree_node_hashes(
    engine: &StorageEngine,
    node_hash: &[u8],
    hash_length: usize,
    hashes: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
    hashes.insert(node_hash.to_vec());
    if let Some((_header, _key, value)) = engine.get_entry(node_hash)? {
        if let Ok(BTreeNode::Internal(internal)) = BTreeNode::deserialize(&value, hash_length) {
            for child_hash in &internal.children {
                collect_btree_node_hashes(engine, child_hash, hash_length, hashes)?;
            }
        }
    }
    Ok(())
}
```

Wire this into the tree walker so `tree.directories` includes ALL B-tree nodes for each directory.

- [ ] **Step 3: Write tests**

Add to `btree_directory_spec.rs`:

```rust
// 11. test_tree_walker_btree_directory — walk version with B-tree dir, all files found
// 12. test_backup_export_btree_directory — export DB with B-tree dir, import, all files present
// 13. test_backup_diff_btree_directory — diff between versions with B-tree dirs
```

- [ ] **Step 4: Build and test all**

Run: `cargo test`

- [ ] **Step 5: Commit**

```bash
git commit -m "Tree walker + backup handle B-tree directory nodes"
```

---

### Task 5: Performance benchmark

**Files:**
- Modify: `tools/directory-stress-test.sh`

- [ ] **Step 1: Run the stress test and compare**

```bash
# Rebuild release
cargo build --release

# Run stress test
./tools/directory-stress-test.sh --db-path /tmp/btree-stress.aeordb --port 3050 --count 20000
```

- [ ] **Step 2: Record results**

Expected improvements:
- Write rate should stay ~500-600/s regardless of directory size (was dropping to 110/s at 20K)
- List latency may increase slightly for small directories (B-tree walk vs flat read)
- DB size should be smaller (no 2MB+ blob rewrites creating voids)

- [ ] **Step 3: Commit benchmark results**

```bash
git commit -m "Benchmark: B-tree directory performance at 20K files"
```
