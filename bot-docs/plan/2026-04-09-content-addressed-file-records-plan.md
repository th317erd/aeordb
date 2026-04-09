# Content-Addressed FileRecord Keys Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Store FileRecords at both a path key (mutable, for reads) and a content key (immutable, in ChildEntry.hash) so snapshots correctly resolve historical file versions.

**Architecture:** Add `file_content_hash` helper. Modify `store_file_internal` and `batch_commit` to store at both keys, with ChildEntry.hash pointing to the content key. Fix GC to mark path keys as live. All read paths stay unchanged (O(1) via path key).

**Tech Stack:** Rust, blake3

**Spec:** `bot-docs/plan/content-addressed-file-records.md`

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `aeordb-lib/src/engine/directory_ops.rs` | Add `file_content_hash`, modify `store_file_internal` for dual-key storage |
| Modify | `aeordb-lib/src/engine/batch_commit.rs` | Same dual-key pattern for commit |
| Modify | `aeordb-lib/src/engine/gc.rs` | Mark path-based file keys as live |
| Modify | `aeordb-lib/src/engine/mod.rs` | Export `file_content_hash` |
| Create | `aeordb-lib/spec/engine/content_addressed_file_spec.rs` | Tests for dual-key storage + snapshot versioning |
| Modify | `aeordb-lib/Cargo.toml` | Add test entry |

---

### Task 1: Dual-Key FileRecord Storage

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`
- Modify: `aeordb-lib/src/engine/batch_commit.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Create: `aeordb-lib/spec/engine/content_addressed_file_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Add `file_content_hash` helper**

In `aeordb-lib/src/engine/directory_ops.rs`, add after the `directory_content_hash` function (around line 46):

```rust
/// Compute a content-addressed hash for a serialized FileRecord.
/// Uses the "filec:" domain prefix, distinct from the path-based "file:" prefix.
pub fn file_content_hash(data: &[u8], algo: &HashAlgorithm) -> EngineResult<Vec<u8>> {
  let mut input = Vec::with_capacity(6 + data.len());
  input.extend_from_slice(b"filec:");
  input.extend_from_slice(data);
  algo.compute_hash(&input)
}
```

Add the re-export in `aeordb-lib/src/engine/mod.rs` — add `file_content_hash` to the existing `directory_ops` re-export line.

- [ ] **Step 2: Modify `store_file_internal` for dual-key storage**

In `aeordb-lib/src/engine/directory_ops.rs`, find the `store_file_internal` method (around line 249-261). Replace:

```rust
    let file_value = file_record.serialize(hash_length);
    self.engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;

    // Build child entry for directory update
    let child = ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: file_key.clone(),
```

With:

```rust
    let file_value = file_record.serialize(hash_length);

    // Content-addressed key (immutable — for versioning via ChildEntry.hash)
    let file_content_key = file_content_hash(&file_value, &algo)?;
    self.engine.store_entry(EntryType::FileRecord, &file_content_key, &file_value)?;

    // Path-based key (mutable — for reads, indexing, deletion)
    self.engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;

    // Build child entry with content-addressed hash (not path hash)
    let child = ChildEntry {
      entry_type: EntryType::FileRecord.to_u8(),
      hash: file_content_key.clone(),
```

- [ ] **Step 3: Modify `batch_commit` for dual-key storage**

In `aeordb-lib/src/engine/batch_commit.rs`, find where the FileRecord is stored and the ChildEntry is built. Add the import at the top:

```rust
use crate::engine::directory_ops::file_content_hash;
```

Then find the section that stores the FileRecord (around line 160-170). Replace storing at `file_key` only with storing at both keys:

```rust
        // Store at content-addressed key (immutable — for versioning)
        let file_content_key = file_content_hash(&file_value, &algo)?;
        engine.store_entry(EntryType::FileRecord, &file_content_key, &file_value)?;

        // Store at path-based key (mutable — for reads/indexing/deletion)
        engine.store_entry(EntryType::FileRecord, &file_key, &file_value)?;
```

And update the ChildEntry to use content key:

```rust
        let child = ChildEntry {
            entry_type: EntryType::FileRecord.to_u8(),
            hash: file_content_key.clone(),
```

Note: `batch_commit.rs` may use a `WriteBatch` for the FileRecords. If so, add both entries to the batch. Read the current code carefully — the pattern should mirror what you did in `store_file_internal`.

- [ ] **Step 4: Add test entry to Cargo.toml**

```toml
[[test]]
name = "content_addressed_file_spec"
path = "spec/engine/content_addressed_file_spec.rs"
```

- [ ] **Step 5: Write tests for dual-key storage**

Create `aeordb-lib/spec/engine/content_addressed_file_spec.rs`:

```rust
use std::sync::Arc;
use std::collections::HashMap;
use aeordb::engine::{
  DirectoryOps, EntryType, RequestContext, StorageEngine, VersionManager,
  file_path_hash, file_content_hash,
};
use aeordb::engine::file_record::FileRecord;
use aeordb::engine::tree_walker::walk_version_tree;
use aeordb::server::create_temp_engine_for_tests;

// ─── Dual-key storage ───────────────────────────────────────────────────────

#[test]
fn test_file_stored_at_both_path_and_content_keys() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/test.txt", b"hello world", Some("text/plain")).unwrap();

  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();

  // Path key should work (for reads)
  let path_key = file_path_hash("/test.txt", &algo).unwrap();
  let path_entry = engine.get_entry(&path_key).unwrap();
  assert!(path_entry.is_some(), "FileRecord should be at path key");

  // Content key should also work (for versioning)
  let file_value = path_entry.unwrap().2;
  let content_key = file_content_hash(&file_value, &algo).unwrap();
  let content_entry = engine.get_entry(&content_key).unwrap();
  assert!(content_entry.is_some(), "FileRecord should be at content key");
}

#[test]
fn test_child_entry_uses_content_hash_not_path_hash() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/docs/readme.txt", b"readme content", Some("text/plain")).unwrap();

  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();

  // Walk the tree to get the ChildEntry hash
  let head = engine.head_hash().unwrap();
  let tree = walk_version_tree(&engine, &head).unwrap();
  let (file_hash, _record) = tree.files.get("/docs/readme.txt").unwrap();

  // The hash in the tree should be a content hash, not a path hash
  let path_key = file_path_hash("/docs/readme.txt", &algo).unwrap();
  assert_ne!(file_hash, &path_key, "tree should use content hash, not path hash");

  // The content hash should resolve to the FileRecord
  let entry = engine.get_entry(file_hash).unwrap();
  assert!(entry.is_some(), "content hash should resolve to FileRecord");
}

#[test]
fn test_read_file_still_works_via_path_key() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();

  // read_file uses path key internally — should still work
  let content = ops.read_file("/test.txt").unwrap();
  assert_eq!(content, b"hello");
}

#[test]
fn test_overwrite_changes_content_key_but_path_key_still_works() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/test.txt", b"version 1", Some("text/plain")).unwrap();

  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();
  let path_key = file_path_hash("/test.txt", &algo).unwrap();

  // Get v1 content key
  let v1_value = engine.get_entry(&path_key).unwrap().unwrap().2;
  let v1_content_key = file_content_hash(&v1_value, &algo).unwrap();

  // Overwrite
  ops.store_file(&ctx, "/test.txt", b"version 2", Some("text/plain")).unwrap();

  // Path key now points to v2
  let v2_value = engine.get_entry(&path_key).unwrap().unwrap().2;
  let v2_record = FileRecord::deserialize(&v2_value, hash_length).unwrap();
  assert_ne!(v1_value, v2_value, "content should differ");

  // v1 content key still resolves to v1
  let v1_entry = engine.get_entry(&v1_content_key).unwrap();
  assert!(v1_entry.is_some(), "old content key should still exist");

  // v2 has a different content key
  let v2_content_key = file_content_hash(&v2_value, &algo).unwrap();
  assert_ne!(v1_content_key, v2_content_key);
}

// ─── Snapshot versioning ────────────────────────────────────────────────────

#[test]
fn test_snapshot_preserves_historical_file_version() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let hash_length = engine.hash_algo().hash_length();

  // Store v1
  ops.store_file(&ctx, "/doc.txt", b"version 1 content", Some("text/plain")).unwrap();

  // Create snapshot
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

  // Overwrite with v2
  ops.store_file(&ctx, "/doc.txt", b"version 2 content completely different", Some("text/plain")).unwrap();

  // Walk HEAD tree — should have v2
  let head = engine.head_hash().unwrap();
  let head_tree = walk_version_tree(&engine, &head).unwrap();
  let (_, head_record) = head_tree.files.get("/doc.txt").unwrap();
  // v2 is longer, so total_size differs
  assert_eq!(head_record.total_size, 38);

  // Walk snapshot tree — should have v1
  let snap_hash = vm.get_snapshot_hash("v1").unwrap();
  let snap_tree = walk_version_tree(&engine, &snap_hash).unwrap();
  let (_, snap_record) = snap_tree.files.get("/doc.txt").unwrap();
  assert_eq!(snap_record.total_size, 17, "snapshot should have v1 size (17), not v2 size");
}

#[test]
fn test_snapshot_file_content_readable() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);
  let hash_length = engine.hash_algo().hash_length();

  ops.store_file(&ctx, "/doc.txt", b"original content", Some("text/plain")).unwrap();

  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "snap", HashMap::new()).unwrap();

  ops.store_file(&ctx, "/doc.txt", b"overwritten", Some("text/plain")).unwrap();

  // Current version via read_file should be "overwritten"
  let current = ops.read_file("/doc.txt").unwrap();
  assert_eq!(current, b"overwritten");

  // Snapshot tree should have the original file record with original chunks
  let snap_hash = vm.get_snapshot_hash("snap").unwrap();
  let snap_tree = walk_version_tree(&engine, &snap_hash).unwrap();
  let (file_hash, snap_record) = snap_tree.files.get("/doc.txt").unwrap();

  // Read chunks from the snapshot's file record
  let mut snap_content = Vec::new();
  for chunk_hash in &snap_record.chunk_hashes {
    let chunk_entry = engine.get_entry(chunk_hash).unwrap()
      .expect("snapshot chunk should still exist");
    snap_content.extend_from_slice(&chunk_entry.2);
  }
  assert_eq!(snap_content, b"original content", "snapshot should have original content");
}

#[test]
fn test_deleted_file_snapshot_still_has_it() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/ephemeral.txt", b"here today", Some("text/plain")).unwrap();

  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "before-delete", HashMap::new()).unwrap();

  ops.delete_file(&ctx, "/ephemeral.txt").unwrap();

  // Current: file should not exist
  let head = engine.head_hash().unwrap();
  let head_tree = walk_version_tree(&engine, &head).unwrap();
  assert!(!head_tree.files.contains_key("/ephemeral.txt"));

  // Snapshot: file should still exist
  let snap_hash = vm.get_snapshot_hash("before-delete").unwrap();
  let snap_tree = walk_version_tree(&engine, &snap_hash).unwrap();
  assert!(snap_tree.files.contains_key("/ephemeral.txt"), "snapshot should still have deleted file");
}
```

- [ ] **Step 6: Run tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test content_addressed_file_spec -- --test-threads=1`
Expected: All 7 tests pass

- [ ] **Step 7: Run full test suite**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All tests pass (no regressions)

- [ ] **Step 8: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/engine/directory_ops.rs aeordb-lib/src/engine/batch_commit.rs aeordb-lib/src/engine/mod.rs aeordb-lib/spec/engine/content_addressed_file_spec.rs aeordb-lib/Cargo.toml
git commit -m "Content-addressed FileRecords: dual-key storage + snapshot versioning — 7 tests"
```

---

### Task 2: Fix GC to Mark Path Keys

**Files:**
- Modify: `aeordb-lib/src/engine/gc.rs`
- Modify: `aeordb-lib/spec/engine/content_addressed_file_spec.rs`

With dual-key storage, GC walks the tree via content keys (ChildEntry.hash). But the path key is a standalone mutable index not in the tree. GC must also mark it as live, or it gets swept.

- [ ] **Step 1: Modify `mark_file_entry` in gc.rs**

In `aeordb-lib/src/engine/gc.rs`, find the `mark_file_entry` function (around line 137-156). It currently marks the file hash (now the content key) and its chunks. Add marking the path-based key:

```rust
fn mark_file_entry(
  engine: &StorageEngine,
  file_hash: &[u8],
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  if !live.insert(file_hash.to_vec()) {
    return Ok(());
  }

  if let Some((_header, _key, value)) = engine.get_entry(file_hash)? {
    let file_record = FileRecord::deserialize(&value, hash_length)?;

    // Mark all chunk hashes as live
    for chunk_hash in &file_record.chunk_hashes {
      live.insert(chunk_hash.clone());
    }

    // Also mark the path-based key as live (mutable index for reads/indexing)
    let algo = engine.hash_algo();
    let path_key = crate::engine::directory_ops::file_path_hash(&file_record.path, &algo)?;
    live.insert(path_key);
  }

  Ok(())
}
```

- [ ] **Step 2: Write GC test for path key preservation**

Add to `aeordb-lib/spec/engine/content_addressed_file_spec.rs`:

```rust
use aeordb::engine::gc::run_gc;

// ─── GC with dual keys ─────────────────────────────────────────────────────

#[test]
fn test_gc_preserves_path_keys_for_live_files() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/keep.txt", b"keep me", Some("text/plain")).unwrap();

  // Run GC
  let result = run_gc(&engine, &ctx, false).unwrap();

  // Path key should still work after GC
  let algo = engine.hash_algo();
  let path_key = file_path_hash("/keep.txt", &algo).unwrap();
  assert!(engine.has_entry(&path_key).unwrap(), "path key should survive GC");

  // Read should still work
  let content = ops.read_file("/keep.txt").unwrap();
  assert_eq!(content, b"keep me");
}

#[test]
fn test_gc_sweeps_old_content_keys_after_overwrite() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/evolve.txt", b"version 1", Some("text/plain")).unwrap();

  let algo = engine.hash_algo();
  let hash_length = algo.hash_length();

  // Capture v1 content key
  let path_key = file_path_hash("/evolve.txt", &algo).unwrap();
  let v1_value = engine.get_entry(&path_key).unwrap().unwrap().2;
  let v1_content_key = file_content_hash(&v1_value, &algo).unwrap();

  // Overwrite with v2
  ops.store_file(&ctx, "/evolve.txt", b"version 2", Some("text/plain")).unwrap();

  // Before GC, v1 content key should still exist
  assert!(engine.has_entry(&v1_content_key).unwrap(), "v1 content key should exist before GC");

  // Run GC — v1 content key is unreachable (no snapshot references it)
  let result = run_gc(&engine, &ctx, false).unwrap();
  assert!(result.garbage_entries > 0, "GC should find garbage from overwrite");

  // After GC, v1 content key should be swept
  assert!(!engine.has_entry(&v1_content_key).unwrap(), "v1 content key should be swept by GC");

  // But the file is still readable (path key + v2 content key intact)
  let content = ops.read_file("/evolve.txt").unwrap();
  assert_eq!(content, b"version 2");
}

#[test]
fn test_gc_preserves_snapshot_content_keys() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/doc.txt", b"version 1", Some("text/plain")).unwrap();

  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

  ops.store_file(&ctx, "/doc.txt", b"version 2", Some("text/plain")).unwrap();

  // Run GC — v1 content key should NOT be swept (snapshot references it)
  run_gc(&engine, &ctx, false).unwrap();

  // Snapshot tree should still walk correctly
  let snap_hash = vm.get_snapshot_hash("v1").unwrap();
  let snap_tree = walk_version_tree(&engine, &snap_hash).unwrap();
  assert!(snap_tree.files.contains_key("/doc.txt"), "snapshot should still have the file");
  let (_, record) = snap_tree.files.get("/doc.txt").unwrap();
  assert_eq!(record.total_size, 9, "snapshot should have v1 size (9 bytes)");
}
```

- [ ] **Step 3: Run tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test content_addressed_file_spec -- --test-threads=1`
Expected: All 10 tests pass

- [ ] **Step 4: Run full suite + GC tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/engine/gc.rs aeordb-lib/spec/engine/content_addressed_file_spec.rs
git commit -m "Fix GC to mark path-based file keys as live — 10 tests total"
```

---

## Post-Implementation Checklist

- [ ] Update `.claude/TODO.md` — mark loose end "snapshot versioning" as fixed
- [ ] Update `.claude/DETAILS.md` — note dual-key FileRecord storage
- [ ] Run: `cargo test -- --test-threads=4` — all tests pass
- [ ] Run: E2E export/snapshot verification (store file → snapshot → overwrite → export snapshot → verify historical content)
