# Content-Addressed FileRecord Keys — Spec

**Date:** 2026-04-09
**Status:** Approved
**Priority:** High — snapshots currently return wrong file versions

---

## 1. Problem

FileRecords are stored at a path-based key: `hash("file:/path")`. This key is mutable — it always points to the latest version. Directory `ChildEntry.hash` also points to this path-based key. When the tree walker resolves a snapshot's directory tree, it follows `ChildEntry.hash` to the FileRecord — but since the key is mutable, it gets the *current* version, not the version at snapshot time.

Directories already solved this with dual-key storage: a mutable path key (`dir:/path`) and an immutable content key (`dirc:` + serialized data). `ChildEntry.hash` for directories points to the content key, enabling correct Merkle-tree semantics for versioning.

Files need the same treatment.

---

## 2. Solution

Store FileRecords at two keys:

1. **Path key** (`file:/path`) → mutable, always points to the latest FileRecord. Used for reads, metadata, indexing, deletion. **No change to any read path.**

2. **Content key** (`filec:` + serialized FileRecord bytes) → immutable. Stored alongside the path key on every write. `ChildEntry.hash` points to this key.

The tree walker, GC, backup export, and snapshot versioning automatically get correct historical file resolution because they follow `ChildEntry.hash` which now points to the immutable content key.

---

## 3. What Changes

### store_file_internal (directory_ops.rs)

**Before:**
```rust
let file_key = file_path_hash(&normalized, &algo)?;
engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;
let child = ChildEntry { hash: file_key.clone(), ... };
```

**After:**
```rust
let file_key = file_path_hash(&normalized, &algo)?;
let file_content_key = file_content_hash(&file_value, &algo)?;

// Store at content key (immutable — for versioning)
engine.store_entry(EntryType::FileRecord, &file_content_key, &file_value)?;
// Store at path key (mutable — for reads/indexing/deletion)
engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;

// ChildEntry points to content key, not path key
let child = ChildEntry { hash: file_content_key.clone(), ... };
```

### batch_commit (batch_commit.rs)

Same pattern — compute both keys, store at both, ChildEntry uses content key.

### file_content_hash (new helper)

```rust
pub fn file_content_hash(serialized_data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
    algo.compute_hash(format!("filec:").as_bytes().iter()
        .chain(serialized_data.iter())
        .copied()
        .collect::<Vec<u8>>()
        .as_slice())
}
```

Or simpler:
```rust
pub fn file_content_hash(serialized_data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
    let mut input = Vec::with_capacity(6 + serialized_data.len());
    input.extend_from_slice(b"filec:");
    input.extend_from_slice(serialized_data);
    algo.compute_hash(&input)
}
```

---

## 4. What Does NOT Change

- `read_file` / `read_file_streaming` — still uses path key. O(1). No change.
- `get_metadata` — still uses path key. No change.
- `exists` / `head` — still uses path key. No change.
- `delete_file` — still uses path key for KV deletion + indexing. No change.
- `indexing_pipeline` — still uses path key as document ID. No change.
- `backup.rs` — deletion records still use path key. No change.
- `tree_walker.rs` — follows `ChildEntry.hash` which now points to content key. Works because the FileRecord IS stored at the content key. **Correct historical resolution.**
- `gc.rs` — marks `ChildEntry.hash` as live. Now marks the content key. The path key for the latest version is also reachable from HEAD's tree (it's still written). Old content keys for superseded versions are NOT reachable — they become garbage after GC. **Correct.**

---

## 5. GC Implications

With dual-key FileRecords:
- The **content key** for the current version is reachable from HEAD's directory tree (via ChildEntry.hash). Marked live by GC.
- The **path key** for the current version is NOT in the directory tree — it's a standalone mutable index. GC would sweep it as garbage unless we explicitly mark it live.

**Fix:** In `gc.rs::mark_system_entries` (or a new `mark_path_keys` function), iterate all live files from the tree walk and mark their corresponding path keys as live. Since we already have the file paths from the tree walk, computing `file_path_hash` for each is trivial.

Or simpler: in `gc.rs::walk_and_mark`, when we encounter a FileRecord, also compute and mark its path-based hash:
```rust
EntryType::FileRecord => {
    let file_record = FileRecord::deserialize(&value, hash_length)?;
    // Mark all chunk hashes
    for chunk_hash in &file_record.chunk_hashes {
        live.insert(chunk_hash.clone());
    }
    // Also mark the path-based key as live (mutable index)
    let path_key = file_path_hash(&file_record.path, &algo)?;
    live.insert(path_key);
}
```

---

## 6. Snapshot Versioning — How It Now Works

**Before (broken):**
1. Store `/docs/readme.txt` with content "v1"
2. Create snapshot "snap1"
3. Overwrite `/docs/readme.txt` with content "v2"
4. Walk snapshot "snap1" tree → `ChildEntry.hash` = `hash("file:/docs/readme.txt")` → resolves to "v2" (wrong!)

**After (correct):**
1. Store `/docs/readme.txt` with content "v1" → content key = `hash("filec:" + v1_record)`, path key = `hash("file:/docs/readme.txt")`
2. Create snapshot "snap1" — directory tree has `ChildEntry.hash = content_key_v1`
3. Overwrite `/docs/readme.txt` with content "v2" → new content key = `hash("filec:" + v2_record)`, path key updated to point to v2
4. Walk snapshot "snap1" tree → `ChildEntry.hash` = `content_key_v1` → resolves to "v1" (correct!)

The old content key (`content_key_v1`) is reachable from the snapshot's tree, so GC won't sweep it. The path key always points to the latest version.

---

## 7. Implementation Phases

### Phase 1 — Add file_content_hash + dual-key storage
- Add `file_content_hash` helper to directory_ops.rs
- Modify `store_file_internal` to store at both keys, ChildEntry uses content key
- Modify `batch_commit::commit_files` same pattern
- Tests: file stored at content key, ChildEntry.hash is content-addressed

### Phase 2 — Fix GC to mark path keys
- Modify `gc.rs::walk_and_mark` to also mark path-based file keys as live
- Tests: GC doesn't sweep path keys for live files, GC does sweep old content keys

### Phase 3 — Snapshot versioning tests
- Store file, snapshot, overwrite, walk snapshot tree — verify historical version
- Export snapshot — verify historical file content
- Diff between snapshots — verify modifications detected correctly

---

## 8. Non-goals

- Removing file_path_hash entirely (needed for reads, indexing, deletion)
- Changing read paths (they stay O(1) via path key)
- Changing the indexing pipeline (it keeps using path key as document ID)
- Retroactive fix for existing databases (old snapshots will still have path-based ChildEntry hashes — new writes produce content-addressed hashes, old ones stay as-is)
