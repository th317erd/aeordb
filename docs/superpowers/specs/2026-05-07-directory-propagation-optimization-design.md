# Directory Propagation Optimization

**Date:** 2026-05-07
**Status:** Approved

## Problem

Every file write or delete triggers `update_parent_directories`, which walks from the file's parent up to root, reading/modifying/writing each directory level. For a file 3 levels deep, this produces 6 `store_entry` calls (2 per level), each acquiring the writer lock + KV mutex independently. Additionally, each level reads the existing directory from the WAL via a disk seek. The result is ~450ms per file operation, dominated by lock contention, redundant disk reads, and duplicate data writes.

## Optimization 1: Batched Directory Writes

### Current behavior

`update_parent_directories` calls `self.engine.store_entry()` twice per level during the upward walk:
```
Level /Pictures/Family: store_entry(content_key, data), store_entry(dir_key, data)
Level /Pictures:        store_entry(content_key, data), store_entry(dir_key, data)
Level /:                store_entry(content_key, data), store_entry(dir_key, data)
update_head(content_key)
```

Each `store_entry` acquires the writer RwLock + KV Mutex independently. For 3 levels = 6 lock acquisitions + 1 for update_head.

### Fix

Collect all entries during the upward walk into a `WriteBatch`. Flush once at the end with a single lock acquisition. Add a `flush_batch_and_update_head` method to `StorageEngine` that atomically flushes the batch and updates HEAD in the same lock hold.

The upward walk only needs read access to existing directories (via `get_entry` on the lock-free KV snapshot). Writes are deferred to the batch.

### New method on StorageEngine

```rust
pub fn flush_batch_and_update_head(
    &self,
    batch: WriteBatch,
    head_hash: &[u8],
) -> EngineResult<Vec<u64>>
```

Acquires writer + KV locks once, appends all batch entries, inserts into KV, updates the file header's head_hash, releases locks.

## Optimization 2: Directory Content Cache

### Current behavior

Each level in `update_parent_directories` reads the directory from the WAL via `get_entry(dir_key)` → `read_entry_at_shared(offset)` — a disk seek + read. When uploading 10 files to the same directory, each file re-reads the same parent directories from disk.

### Fix

Add a directory content cache to `StorageEngine`:

```rust
pub dir_content_cache: RwLock<HashMap<Vec<u8>, Vec<u8>>>
```

Keyed by `content_key` (content-addressed hash). Values are the serialized directory data.

**Write-through:** When `update_parent_directories` produces new directory content, cache it by content_key before adding to the batch.

**Read:** Before hitting the WAL, check the cache. `update_parent_directories` reads directories by `dir_key` (path-addressed). With hard links (Optimization 3), the `dir_key` entry contains a 32-byte content hash. Follow the link, then check the cache by content_key.

**Eviction:**
- On snapshot restore: `evict_all()` (restore can change everything)
- Natural replacement: writing a new version of a directory replaces its cache entry
- No TTL needed — write-through means the cache is always consistent

**No size bound needed.** Directory entries are small (a few KB). Even 10,000 cached directories = ~30MB. The keyspace is bounded by the number of directories in the database.

## Optimization 3: Hard Links (Eliminate Double-Store)

### Current behavior

Each directory level stores the same serialized data twice:
```rust
store_entry(DirectoryIndex, &content_key, &dir_value)  // content-addressed (for Merkle tree)
store_entry(DirectoryIndex, &dir_key, &dir_value)       // path-addressed (for lookups)
```

This doubles WAL write volume for directory updates.

### Fix

Store full data only at `content_key`. At `dir_key`, store a lightweight **hard link** — just the 32-byte content hash.

**Discriminator:** A value of exactly `hash_length` bytes (32 for BLAKE3) at a `dir_key` is a hard link. Real directory data is always longer than 32 bytes (even a single child entry serializes to ~70 bytes). Empty directories have a zero-length value. So the length check is unambiguous.

**Read path:** When reading a directory by path:
1. Read `dir_key` from KV → get value
2. If `value.len() == hash_length` → it's a hard link; read `content_key` (the value) from WAL (or cache)
3. Otherwise → it's inline data (backward-compatible with pre-optimization entries)

**Write path in `update_parent_directories`:**
1. Compute new directory content → `dir_value`
2. Hash it → `content_key`
3. Batch: `add(DirectoryIndex, content_key, dir_value)` (full data)
4. Batch: `add(DirectoryIndex, dir_key, content_key)` (32-byte hard link)
5. Cache: `dir_content_cache.insert(content_key, dir_value)`

**Backward compatibility:** Old entries that stored full data at `dir_key` still work. The read path checks length — anything >32 bytes is interpreted as inline data. No migration needed. Old entries are naturally replaced as directories are modified.

### GC Impact

GC already marks both content hashes and path hashes for every directory (lines 108-115 of `gc.rs`):

```rust
live.insert(root_hash.to_vec());      // content hash
let path_hash = engine.compute_hash(format!("dir:{}", dir_path).as_bytes())?;
live.insert(path_hash);                // path hash
```

Both the hard link entry (`dir_key`) and the actual data entry (`content_key`) are marked as live. No changes to GC needed.

**Update for safety:** Add hard link awareness to `walk_directory_tree` so that if GC ever encounters a hard link value when reading from a `dir_key`, it follows the link to the actual content. Currently GC only traverses by content hash (not path hash), so this is a defensive measure:

In `walk_directory_tree`, after reading the entry, check if the value is a hard link:
```rust
let value = if value.len() == hash_length {
    // Hard link — follow to actual content
    live.insert(value.clone()); // mark the content hash too
    match engine.get_entry_including_deleted(&value)? {
        Some((_h, _k, v)) => v,
        None => return Ok(()),
    }
} else {
    value
};
```

## B-Tree Node Handling

B-tree directories call `btree_insert_batched` which internally stores intermediate B-tree nodes via `store_entry`. These nodes must be stored before the parent node can reference them (parent needs their content hash).

However, the upward walk does NOT re-read just-written directories — each level computes content → hash → passes the hash up as the child entry. The content hash is computed locally from the data, not by reading it back. So batching works without a "pending writes" overlay.

For B-tree operations: `btree_insert_batched` returns the new root hash and root data. The intermediate B-tree nodes it creates are stored directly (not batched), since they're leaf/internal nodes needed for future reads by other operations. Only the directory-level entries (content_key + dir_key hard link) are batched. This is a pragmatic split — B-tree nodes are small and infrequent (only on directories with >64 children), so batching them has minimal benefit.

## Read Path Changes

Every code path that reads a directory by path (`dir_key`) must follow hard links. Add a `read_directory_data` helper to `DirectoryOps`:

```rust
fn read_directory_data(&self, dir_key: &[u8]) -> EngineResult<Option<(EntryHeader, Vec<u8>)>>
```

This helper: reads `dir_key` → if value is `hash_length` bytes, follows the link (checks cache first, then WAL) → returns the actual directory data. All existing callers of `get_entry(dir_key)` for directories switch to this helper.

Callers to update:
- `update_parent_directories`
- `remove_from_parent_directory`
- `list_directory` / `list_directory_children`
- `delete_directory`
- Any other path that reads directory content by path hash

## Replication Consideration

The sync engine exchanges entries between nodes. A hard link entry (32-byte value at `dir_key`) is only valid if the target `content_key` entry also exists on the receiving node. The current sync protocol sends all entries by walking the Merkle tree (content-addressed), so content entries are naturally sent. Hard link entries (path-addressed) are sent separately. As long as content entries arrive before or with hard links, consistency is maintained. The sync engine already sends content entries first (tree walk order), so no changes needed. Added as a note for future awareness.

## Files Touched

- **Modify:** `engine/storage_engine.rs` — add `flush_batch_and_update_head`, add `dir_content_cache` field, initialize in constructors
- **Modify:** `engine/directory_ops.rs` — rewrite `update_parent_directories` to use batch + cache + hard links, update `remove_from_parent_directory` similarly, add `read_directory_data` helper that follows hard links + checks cache, update all directory read call sites to use the helper
- **Modify:** `engine/gc.rs` — add hard link awareness to `walk_directory_tree` (defensive)
- **Modify:** `engine/btree.rs` — no changes needed; B-tree node storage stays synchronous, only directory-level entries are batched

## Expected Impact

For a file 3 levels deep:
- **Before:** 6 lock acquisitions, 3 WAL reads, 6 WAL writes (full data × 2 per level)
- **After:** 1 lock acquisition, 0-3 WAL reads (cached), 6 WAL writes (3 full data + 3 hard links of 32 bytes each)
- **Bulk operations:** 10 files in same directory = 1 propagation's worth of reads (cached after first), still 10 propagations but each much faster

## Testing

- Unit test: write a file, verify directory content cache is populated, write another file in same directory, verify cache hit (no WAL read on second write)
- Unit test: read a directory by path, verify hard link is followed to content
- Unit test: GC after hard link writes, verify no data loss
- Integration test: time a batch of 10 file writes, verify improvement over baseline
- Backward compatibility: verify old inline directory entries still read correctly
