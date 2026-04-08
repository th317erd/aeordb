# Large Directory Optimization: Content-Addressed B-Tree — Spec

**Date:** 2026-04-07
**Status:** Draft
**Priority:** High — write throughput degrades 6x at 20K files per directory

---

## 1. The Problem

Directories are stored as a single serialized blob of ALL child entries. Adding one file to a directory with N children requires:
1. Read the entire blob (O(N) bytes)
2. Deserialize all N children
3. Add/update one child
4. Serialize all N+1 children (O(N) bytes)
5. Write the entire blob back
6. Compute new content hash
7. Cascade up to root (repeat for each ancestor)

At 20K files: each child is ~100+ bytes → 2MB+ rewritten per file store. Write throughput drops from 643/s to 110/s.

### Benchmark baseline (SSD /tmp/)

| Files | Write Rate | List Latency |
|-------|-----------|-------------|
| 500 | 643/s | 2.0ms |
| 1,000 | 619/s | 2.0ms |
| 5,000 | 519/s | 6.6ms |
| 10,000 | 288/s | 9.5ms |
| 20,000 | 110/s | 17.4ms |

---

## 2. The Solution: Content-Addressed B-Tree

Replace the flat child list with a B-tree where each node is a separate content-addressed entry in the storage engine. Mutations touch O(log N) nodes instead of O(N) bytes.

### Node structure

**Internal node:**
```
node_type: u8 = 1
key_count: u16
keys: [String; key_count]           // child names (sorted)
children: [Vec<u8>; key_count + 1]  // hashes of child nodes
```

**Leaf node:**
```
node_type: u8 = 0
entry_count: u16
entries: [ChildEntry; entry_count]  // sorted by name
```

Each node is stored as a regular engine entry:
- Key: content hash of the serialized node (content-addressed, immutable)
- Value: serialized node data
- EntryType: DirectoryIndex (reusing existing type)

### Branching factor

Target node size: ~4KB. With BLAKE3 (32-byte hashes) and typical filenames (~20 bytes):
- Internal node: each slot = ~52 bytes (20 name + 32 hash). ~77 slots per 4KB node.
- Leaf node: each slot = ~100 bytes (ChildEntry). ~40 entries per 4KB node.

| Files | Tree Depth | Nodes Written per Insert |
|-------|-----------|------------------------|
| 100 | 1 (just a leaf) | 1 |
| 1,000 | 2 | 2 |
| 10,000 | 2-3 | 2-3 |
| 100,000 | 3 | 3 |
| 1,000,000 | 3-4 | 3-4 |

At 20K files: 3 node writes (~12KB) instead of rewriting a 2MB+ blob.

---

## 3. Operations

### Insert (add/update child)

```
1. Start at root node
2. Binary search keys to find the child subtree
3. Recurse into the correct child node
4. At leaf: insert/update the ChildEntry (maintain sorted order)
5. If leaf overflows (> max_entries): split into two leaves
6. Propagate split key + new child hash up to parent
7. If parent overflows: split parent (recurse)
8. Each modified node → new content hash → new entry in engine
9. Return new root hash
```

Modified nodes get new hashes. Unmodified nodes keep their existing hashes — structural sharing.

### Delete (remove child)

```
1. Walk from root to leaf containing the child
2. Remove the ChildEntry from the leaf
3. If leaf underflows (< min_entries): merge or redistribute with sibling
4. Propagate changes up
5. Return new root hash
```

### Lookup (find one child by name)

```
1. Start at root node
2. Binary search keys
3. Follow the correct child hash
4. At leaf: binary search entries by name
5. Return ChildEntry or None
```

O(log N) node reads. Each read is one `engine.get_entry(hash)`.

### List all children

```
1. Walk to leftmost leaf
2. Read all entries from each leaf, left to right
3. Follow "next leaf" pointers or walk back up the tree
```

O(N/B) node reads where B = entries per leaf. For 20K files with 40 per leaf: ~500 reads.

Alternative: store sibling leaf pointers for O(1) traversal between leaves (B+ tree style).

---

## 4. Versioning & Snapshots

Content-addressed nodes give us versioning for free:

```
Before insert:
  Root_v1 (hash_A) → [Internal (hash_B) → [Leaf0 (hash_C), Leaf1 (hash_D)]]

After inserting into Leaf1:
  Root_v2 (hash_E) → [Internal (hash_F) → [Leaf0 (hash_C), Leaf1' (hash_G)]]
                                            ^^^^^ shared      ^^^^^ new
```

- Snapshot stores root hash (hash_A or hash_E)
- Walking from hash_A always returns the v1 tree
- Walking from hash_E returns the v2 tree
- Leaf0 (hash_C) is shared — stored once, referenced by both versions
- No data duplication for unchanged subtrees

### Diff between versions

Compare two root hashes. Recurse only into subtrees with different hashes. Unchanged subtrees (same hash) = skip entirely. This makes backup diffs O(changes), not O(total files).

---

## 5. Node Serialization

### Internal node binary format

```
[u8 node_type = 1]
[u16 key_count]
For each key:
    [u16 key_length]
    [u8; key_length]  // key bytes (UTF-8 name)
For each child (key_count + 1):
    [u8; hash_length]  // child node hash
```

### Leaf node binary format

```
[u8 node_type = 0]
[u16 entry_count]
For each entry:
    [serialized ChildEntry]  // existing format
```

### Content hash

Each node's content hash = `hash("btree:" + serialized_node_bytes)`. The `btree:` domain prefix avoids collisions with other entry types.

---

## 6. Migration from flat directories

### Threshold-based

Small directories (< threshold entries) stay as flat lists — the overhead of a B-tree isn't worth it for 10 files. Large directories get upgraded to B-trees.

Threshold: **256 entries**. Below 256: flat list (existing behavior). At 256: convert to B-tree on next mutation.

### Detection

The directory entry starts with a `node_type` byte:
- If first byte is `0` or `1`: it's a B-tree node (leaf or internal)
- Otherwise: it's the legacy flat format (starts with child entry data, first byte is entry_type which is >= 2)

This allows seamless detection without a migration step.

### Conversion

When a flat directory reaches 256 entries and a new child is added:
1. Sort all children by name
2. Split into leaf nodes (~40 entries each)
3. Build internal nodes pointing to leaves
4. Store all nodes as content-addressed entries
5. Store root node hash as the directory's content hash
6. Update parent with new root hash

One-time cost, amortized over future inserts.

---

## 7. Directory Operations Changes

### `list_directory`

```
Current: read one blob, deserialize all children
New:     detect format (flat or B-tree), if B-tree walk leaves
```

### `update_parent_directories` (add/update child)

```
Current: read blob → deserialize all → add child → serialize all → write blob
New:     detect format, if B-tree: walk to leaf, insert, split if needed,
         write only modified nodes, return new root hash
```

### `remove_from_parent_directory` (delete child)

```
Current: read blob → deserialize all → remove child → serialize all → write blob
New:     detect format, if B-tree: walk to leaf, remove, merge if needed,
         write only modified nodes, return new root hash
```

### Content-addressed root hash

The directory's content hash IS the B-tree root node's hash. HEAD and snapshots reference root hashes — no change to the versioning model.

---

## 8. B-Tree Configuration

```rust
const BTREE_MAX_LEAF_ENTRIES: usize = 40;      // ~4KB per leaf
const BTREE_MIN_LEAF_ENTRIES: usize = 20;       // merge threshold
const BTREE_MAX_INTERNAL_KEYS: usize = 77;      // ~4KB per internal node
const BTREE_MIN_INTERNAL_KEYS: usize = 38;      // merge threshold
const BTREE_CONVERSION_THRESHOLD: usize = 256;  // convert flat → B-tree at this size
```

---

## 9. Impact on Existing Systems

### Tree walker (backup export/diff)

The tree walker follows `ChildEntry.hash` values. For B-tree directories, the root hash points to a B-tree node, not a flat list. The walker needs to understand B-tree nodes and walk leaves to enumerate children.

### Indexing pipeline

No change — the indexing pipeline receives ChildEntry data from `list_directory`, which handles the format internally.

### Query engine

No change — queries work on indexed fields, not directory structure.

### Events

No change — events fire from DirectoryOps methods, which call the B-tree operations internally.

### Backup export/import

Export walks the tree — needs to understand B-tree nodes and include all nodes (not just root). Import stores nodes as regular entries.

---

## 10. Edge Cases

### Empty directory
Stored as a single empty leaf node. Hash of empty leaf = content hash.

### Single file
Stored as a leaf node with one entry. No internal nodes needed.

### 256 files (conversion point)
On the 257th insert, convert flat → B-tree. All subsequent operations use B-tree path.

### File with same name (overwrite)
Walk to leaf, find entry by name, replace ChildEntry with new values. Modified leaf → new hash → cascade up.

### Concurrent directory mutations
The engine is single-writer, so no concurrent mutations within one process. The B-tree mutation is a sequence of reads + writes, all under the write lock.

### Very deep nesting
Each directory level has its own B-tree. Depth of nesting (path segments) is separate from B-tree depth (within one directory). A path like `/a/b/c/d/file.txt` has 4 directory B-trees (one per level), each independently sized.

---

## 11. Implementation Phases

### Phase 1 — B-tree node types + serialization
- `BTreeNode` enum (Internal/Leaf)
- Node serialization/deserialization
- Content hash computation with `btree:` domain prefix
- Tests: serialize/deserialize roundtrip, content hash determinism

### Phase 2 — B-tree operations (insert, lookup, split)
- `btree_insert(engine, root_hash, child_entry) -> new_root_hash`
- `btree_lookup(engine, root_hash, name) -> Option<ChildEntry>`
- Leaf splitting when full
- Internal node splitting
- Tests: insert into empty, insert causing split, multi-level split, lookup

### Phase 3 — B-tree operations (delete, list, merge)
- `btree_delete(engine, root_hash, name) -> new_root_hash`
- `btree_list(engine, root_hash) -> Vec<ChildEntry>`
- Leaf merging/redistribution when underflow
- Tests: delete, delete causing merge, list all entries, list empty

### Phase 4 — Integrate into DirectoryOps
- Format detection (flat vs B-tree by first byte)
- `list_directory`: handle both formats
- `update_parent_directories`: use B-tree for large directories
- `remove_from_parent_directory`: use B-tree
- Flat → B-tree conversion at 256 entries
- Tests: integration tests with mixed formats, conversion trigger

### Phase 5 — Update tree walker + backup
- Tree walker understands B-tree nodes (walk leaves for children)
- Export includes all B-tree nodes
- Import stores B-tree nodes
- Diff walks B-tree nodes, skips matching subtrees
- Tests: export/import with B-tree directories, diff efficiency

### Phase 6 — Performance benchmarks
- Re-run directory stress test
- Compare: flat vs B-tree at 500, 1K, 5K, 10K, 20K files
- Verify write throughput stays flat (not degrading with N)
- Verify list latency is acceptable
- Verify snapshot/versioning still works correctly

---

## 12. Expected Performance After Optimization

| Files | Nodes per Insert | Bytes Written per Insert | Expected Write Rate |
|-------|-----------------|------------------------|-------------------|
| 500 | 1 (leaf only) | ~4KB | ~600/s (same) |
| 1,000 | 2 | ~8KB | ~600/s |
| 5,000 | 2-3 | ~12KB | ~550/s |
| 10,000 | 3 | ~12KB | ~550/s |
| 20,000 | 3 | ~12KB | ~550/s |
| 100,000 | 3-4 | ~16KB | ~500/s |

Write throughput should remain nearly flat regardless of directory size — O(log N) nodes written instead of O(N) bytes.

---

## 13. Non-Goals (Deferred)

- B+ tree leaf sibling pointers (for faster sequential scan) — add if listing is a bottleneck
- Node caching/LRU for hot B-tree nodes — optimize later
- Concurrent readers during B-tree mutation — single-writer model is sufficient
- Auto-compaction of underutilized B-tree pages — not needed initially
- Variable branching factor based on key length — fixed factor is simpler
