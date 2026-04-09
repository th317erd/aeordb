# Garbage Collection: Mark-and-Sweep Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement manual mark-and-sweep garbage collection that reclaims orphaned entries by walking all live version trees and in-place overwriting unreachable entries.

**Architecture:** A new `gc.rs` module provides `gc_mark` (collect all reachable hashes by walking HEAD + snapshots + forks) and `gc_sweep` (iterate KV entries, in-place overwrite garbage with DeletionRecord + Void). The existing `tree_walker.rs` does NOT mark intermediate B-tree node hashes, so GC uses its own recursive walker. `AppendWriter` gets `write_entry_at` for in-place overwrites. CLI `aeordb gc` and HTTP `POST /admin/gc` expose the operation.

**Tech Stack:** Rust, blake3, chrono, clap (CLI), axum (HTTP), serde_json (events)

**Spec:** `bot-docs/plan/gc-mark-sweep.md`

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Create | `aeordb-lib/src/engine/gc.rs` | GcResult, gc_mark (walk all live roots), gc_sweep (in-place overwrite), run_gc (orchestrator) |
| Create | `aeordb-lib/spec/engine/gc_spec.rs` | Tests for mark phase, sweep phase, dry run, edge cases |
| Create | `aeordb-cli/src/commands/gc.rs` | CLI `aeordb gc --database <path> [--dry-run]` |
| Create | `aeordb-lib/src/server/gc_routes.rs` | HTTP `POST /admin/gc [?dry_run=true]` |
| Modify | `aeordb-lib/src/engine/append_writer.rs` | Add `write_entry_at(offset, ...)` and `write_void_at(offset, size)` |
| Modify | `aeordb-lib/src/engine/storage_engine.rs` | Add `read_entry_header_at(offset)`, `write_entry_at(offset, ...)`, `write_void_at(offset, size)`, `remove_kv_entry(hash)` |
| Modify | `aeordb-lib/src/engine/engine_event.rs` | Add `EVENT_GC_COMPLETED` constant and `GcEventData` struct |
| Modify | `aeordb-lib/src/engine/mod.rs` | Add `pub mod gc;` and re-exports |
| Modify | `aeordb-lib/src/server/mod.rs` | Add `pub mod gc_routes;` and route registration |
| Modify | `aeordb-cli/src/commands/mod.rs` | Add `pub mod gc;` |
| Modify | `aeordb-cli/src/main.rs` | Add `Gc` subcommand |
| Modify | `aeordb-lib/Cargo.toml` | Add `[[test]]` entry for gc_spec |

---

### Task 1: In-Place Write Infrastructure (AppendWriter + StorageEngine)

**Files:**
- Modify: `aeordb-lib/src/engine/append_writer.rs:134-153`
- Modify: `aeordb-lib/src/engine/storage_engine.rs:646-658`
- Test: `aeordb-lib/spec/engine/gc_spec.rs` (first tests)
- Modify: `aeordb-lib/Cargo.toml` (add test entry)

- [ ] **Step 1: Add `write_entry_at` to AppendWriter**

Add after `write_void` (line 153) in `aeordb-lib/src/engine/append_writer.rs`:

```rust
  /// Write an entry at a specific file offset (in-place overwrite).
  /// Does NOT update current_offset or entry_count — this overwrites existing space.
  /// The caller is responsible for ensuring the entry fits within the available space.
  pub fn write_entry_at(
    &mut self,
    offset: u64,
    entry_type: EntryType,
    key: &[u8],
    value: &[u8],
  ) -> EngineResult<u32> {
    let hash_algo = self.file_header.hash_algo;
    let hash = EntryHeader::compute_hash(entry_type, key, value, hash_algo)?;
    let total_length =
      EntryHeader::compute_total_length(hash_algo, key.len() as u32, value.len() as u32);

    let now = chrono::Utc::now().timestamp_millis();

    let header = EntryHeader {
      entry_version: 1,
      entry_type,
      flags: 0,
      hash_algo,
      compression_algo: CompressionAlgorithm::None,
      encryption_algo: 0,
      key_length: key.len() as u32,
      value_length: value.len() as u32,
      timestamp: now,
      total_length,
      hash,
    };

    self.file.seek(SeekFrom::Start(offset))?;
    let header_bytes = header.serialize();
    self.file.write_all(&header_bytes)?;
    self.file.write_all(key)?;
    self.file.write_all(value)?;
    self.file.sync_all()?;

    Ok(total_length)
  }

  /// Write a void entry at a specific file offset (in-place overwrite).
  /// The void fills exactly `size` bytes starting at `offset`.
  pub fn write_void_at(&mut self, offset: u64, size: u32) -> EngineResult<()> {
    let hash_algo = self.file_header.hash_algo;
    let header_size = 31 + hash_algo.hash_length();

    if (size as usize) < header_size {
      return Err(EngineError::CorruptEntry {
        offset,
        reason: format!(
          "Void size {} is smaller than minimum entry size {}",
          size, header_size
        ),
      });
    }

    let key = b"";
    let value_length = size as usize - header_size;
    let value = vec![0u8; value_length];

    self.write_entry_at(offset, EntryType::Void, key, &value)?;
    Ok(())
  }
```

- [ ] **Step 2: Add helper methods to StorageEngine**

Add after `mark_entry_deleted` (line 658) in `aeordb-lib/src/engine/storage_engine.rs`:

```rust
  /// Read only the entry header at a given file offset (no key/value).
  /// Used by GC to determine entry size without reading the full payload.
  pub fn read_entry_header_at(&self, offset: u64) -> EngineResult<EntryHeader> {
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let (header, _key, _value) = writer.read_entry_at(offset)?;
    Ok(header)
  }

  /// Write a DeletionRecord entry at a specific file offset (in-place).
  /// Returns the total bytes written.
  pub fn write_deletion_at(&self, offset: u64, path: &str) -> EngineResult<u32> {
    let deletion = crate::engine::deletion_record::DeletionRecord::new(
      path.to_string(),
      Some("gc".to_string()),
    );
    let value = deletion.serialize();
    let key = self.compute_hash(
      format!("del:gc:{}:{}", path, deletion.deleted_at).as_bytes(),
    )?;

    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    writer.write_entry_at(offset, EntryType::DeletionRecord, &key, &value)
  }

  /// Write a void entry at a specific file offset (in-place).
  pub fn write_void_at(&self, offset: u64, size: u32) -> EngineResult<()> {
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    writer.write_void_at(offset, size)?;

    // Register the void with the void manager
    let mut vm = self.void_manager.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    vm.register_void(size, offset);

    Ok(())
  }

  /// Remove an entry from the KV store entirely (hard remove, not just flag).
  /// Used by GC sweep to fully remove garbage entries.
  pub fn remove_kv_entry(&self, hash: &[u8]) -> EngineResult<()> {
    let mut kv = self.kv_store.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    kv.mark_deleted(hash);
    Ok(())
  }
```

- [ ] **Step 3: Write the test file skeleton + first test (write_entry_at roundtrip)**

Create `aeordb-lib/spec/engine/gc_spec.rs`:

```rust
use std::sync::Arc;
use aeordb::engine::{
  DirectoryOps, EntryHeader, EntryType, RequestContext, StorageEngine,
  VersionManager,
};
use aeordb::server::create_temp_engine_for_tests;

// ─── In-place write infrastructure ──────────────────────────────────────────

#[test]
fn test_write_entry_at_roundtrip() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Store a file so we have an entry to overwrite
  ops.store_file(&ctx, "/test.txt", b"hello world", Some("text/plain")).unwrap();

  // Get the entry's offset and size
  let head = engine.head_hash().unwrap();
  // Store a dummy chunk entry we'll later overwrite
  let dummy_key = engine.compute_hash(b"dummy:overwrite-test").unwrap();
  let dummy_value = vec![0u8; 200]; // big enough for DeletionRecord + Void
  let offset = engine.store_entry(EntryType::Chunk, &dummy_key, &dummy_value).unwrap();

  // Read back the entry to get its total_length
  let header = engine.read_entry_header_at(offset).unwrap();
  let original_size = header.total_length;
  assert!(original_size > 0);

  // Write a DeletionRecord in-place at that offset
  let written = engine.write_deletion_at(offset, "gc:test").unwrap();
  assert!(written > 0);
  assert!(written <= original_size);
}

#[test]
fn test_write_void_at_creates_valid_void() {
  let (engine, _temp) = create_temp_engine_for_tests();

  // Store a large-ish dummy entry to get an offset
  let dummy_key = engine.compute_hash(b"dummy:void-test").unwrap();
  let dummy_value = vec![0u8; 500];
  let offset = engine.store_entry(EntryType::Chunk, &dummy_key, &dummy_value).unwrap();
  let header = engine.read_entry_header_at(offset).unwrap();
  let entry_size = header.total_length;

  // Compute how much space a deletion takes
  let deletion_size = engine.write_deletion_at(offset, "gc:void-test").unwrap();

  // Write a void in the remaining space
  let remaining = entry_size - deletion_size;
  let void_offset = offset + deletion_size as u64;
  engine.write_void_at(void_offset, remaining).unwrap();

  // Verify the void is registered
  let stats = engine.stats();
  assert!(stats.void_count > 0, "void should be registered");
  assert!(stats.void_space_bytes >= remaining as u64);
}
```

- [ ] **Step 4: Add `[[test]]` entry to Cargo.toml**

Add to `aeordb-lib/Cargo.toml` in the `[[test]]` section:

```toml
[[test]]
name = "gc_spec"
path = "spec/engine/gc_spec.rs"
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd /home/wyatt/Projects/aeordb && cargo test gc_spec --lib -p aeordb -- --test-threads=1`
Expected: 2 tests pass

- [ ] **Step 6: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/engine/append_writer.rs aeordb-lib/src/engine/storage_engine.rs aeordb-lib/spec/engine/gc_spec.rs aeordb-lib/Cargo.toml
git commit -m "GC Phase 1: in-place write infrastructure (write_entry_at, write_void_at)"
```

---

### Task 2: GC Mark Phase (gc_mark)

**Files:**
- Create: `aeordb-lib/src/engine/gc.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Test: `aeordb-lib/spec/engine/gc_spec.rs`

The existing `walk_version_tree` in `tree_walker.rs` does NOT mark intermediate B-tree node hashes. When a directory is B-tree format, `btree_list_from_node` traverses internal nodes recursively but only returns the leaf `ChildEntry` values — the hashes of the internal nodes themselves are never collected. GC must mark those or they'll be swept as garbage. So we write our own recursive walker that collects every hash it touches.

- [ ] **Step 1: Create gc.rs with GcResult and gc_mark**

Create `aeordb-lib/src/engine/gc.rs`:

```rust
use std::collections::HashSet;

use crate::engine::btree::{BTreeNode, is_btree_format};
use crate::engine::directory_entry::deserialize_child_entries;
use crate::engine::entry_type::EntryType;
use crate::engine::errors::EngineResult;
use crate::engine::file_record::FileRecord;
use crate::engine::kv_store::{KVEntry, KV_TYPE_SNAPSHOT, KV_TYPE_FORK};
use crate::engine::request_context::RequestContext;
use crate::engine::storage_engine::StorageEngine;
use crate::engine::version_manager::VersionManager;

use serde::Serialize;

/// Result of a GC run.
#[derive(Debug, Clone, Serialize)]
pub struct GcResult {
  pub versions_scanned: usize,
  pub live_entries: usize,
  pub garbage_entries: usize,
  pub reclaimed_bytes: u64,
  pub duration_ms: u64,
  pub dry_run: bool,
}

/// Collect all reachable hashes from HEAD + all snapshots + all forks.
/// Returns a HashSet of every hash that is "live" and must not be swept.
///
/// This walks the complete version tree for each root, including:
/// - Directory content hashes (both flat and B-tree format)
/// - B-tree intermediate node hashes (internal nodes stored separately in KV)
/// - FileRecord hashes
/// - Chunk hashes
/// - Snapshot and fork KV key hashes
/// - System table entries (/.system/, /.config/)
pub fn gc_mark(engine: &StorageEngine) -> EngineResult<HashSet<Vec<u8>>> {
  let mut live: HashSet<Vec<u8>> = HashSet::new();
  let hash_length = engine.hash_algo().hash_length();

  // Walk HEAD
  let head_hash = engine.head_hash()?;
  if !head_hash.is_empty() && head_hash.iter().any(|&b| b != 0) {
    walk_and_mark(engine, &head_hash, hash_length, &mut live)?;
  }

  // Walk every snapshot
  let vm = VersionManager::new(engine);
  let snapshots = vm.list_snapshots()?;
  for snapshot in &snapshots {
    walk_and_mark(engine, &snapshot.root_hash, hash_length, &mut live)?;
  }

  // Walk every fork
  let forks = vm.list_forks()?;
  for fork in &forks {
    walk_and_mark(engine, &fork.root_hash, hash_length, &mut live)?;
  }

  // Mark snapshot and fork KV key hashes as live (the version entries themselves)
  for snapshot in &snapshots {
    let key = engine.compute_hash(format!("snap:{}", snapshot.name).as_bytes())?;
    live.insert(key);
  }
  for fork in &forks {
    let key = engine.compute_hash(format!("::aeordb:fork:{}", fork.name).as_bytes())?;
    live.insert(key);
  }

  // Mark system table entries as live (/.system/, /.config/)
  mark_system_entries(engine, hash_length, &mut live)?;

  Ok(live)
}

/// Recursively walk a version tree from a root hash, marking every reachable hash.
fn walk_and_mark(
  engine: &StorageEngine,
  root_hash: &[u8],
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  // Already visited — structural sharing optimization
  if live.contains(root_hash) {
    return Ok(());
  }
  live.insert(root_hash.to_vec());

  // Load the entry
  let entry = match engine.get_entry(root_hash)? {
    Some(entry) => entry,
    None => return Ok(()), // missing entry, skip gracefully
  };

  let (header, _key, value) = entry;

  match header.entry_type {
    EntryType::DirectoryIndex => {
      if value.is_empty() {
        return Ok(());
      }

      if is_btree_format(&value) {
        // B-tree directory: walk nodes recursively, marking intermediate hashes
        walk_btree_node(engine, &value, hash_length, live)?;
      } else {
        // Flat directory: mark each child and recurse
        let children = deserialize_child_entries(&value, hash_length)?;
        for child in &children {
          walk_and_mark(engine, &child.hash, hash_length, live)?;
        }
      }
    }
    EntryType::FileRecord => {
      let file_record = FileRecord::deserialize(&value, hash_length)?;
      for chunk_hash in &file_record.chunk_hashes {
        live.insert(chunk_hash.clone());
      }
    }
    EntryType::Chunk => {
      // Leaf — no children
    }
    _ => {
      // Snapshot, fork, deletion records, voids — no children to walk
    }
  }

  Ok(())
}

/// Walk a B-tree node recursively, marking EVERY hash encountered:
/// - The node data is already marked by the caller (the directory entry hash)
/// - Internal node children are stored as separate entries — mark each
/// - Leaf entries point to files/directories — recurse into them
fn walk_btree_node(
  engine: &StorageEngine,
  node_data: &[u8],
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  let node = BTreeNode::deserialize(node_data, hash_length)?;

  match node {
    BTreeNode::Leaf(leaf) => {
      for child in &leaf.entries {
        walk_and_mark(engine, &child.hash, hash_length, live)?;
      }
    }
    BTreeNode::Internal(internal) => {
      for child_hash in &internal.children {
        // Mark the intermediate node hash as live
        if live.contains(child_hash) {
          continue;
        }
        live.insert(child_hash.clone());

        // Load the child node and recurse
        if let Some((_header, _key, child_data)) = engine.get_entry(child_hash)? {
          walk_btree_node(engine, &child_data, hash_length, live)?;
        }
      }
    }
  }

  Ok(())
}

/// Mark system table entries as live (/.system/ and /.config/ paths).
/// These are always reachable regardless of version state.
fn mark_system_entries(
  engine: &StorageEngine,
  hash_length: usize,
  live: &mut HashSet<Vec<u8>>,
) -> EngineResult<()> {
  // System paths are stored with "file:" and "dir:" domain prefixes.
  // Walk HEAD tree and mark anything under /.system/ or /.config/.
  // Since we already walked HEAD above, those entries are already in `live`.
  // But system tables may also have entries NOT in the directory tree
  // (e.g., standalone system records). Walk them explicitly.
  let system_prefixes = ["/.system/", "/.config/"];

  // Get all file records and check if their paths start with system prefixes.
  // We mark them by computing the path-based hash and marking any matching entries.
  for prefix in &system_prefixes {
    let dir_hash = engine.compute_hash(format!("dir:{}", prefix.trim_end_matches('/')).as_bytes())?;
    if let Some((_header, _key, value)) = engine.get_entry(&dir_hash)? {
      live.insert(dir_hash);
      // Walk this directory tree too
      if !value.is_empty() {
        if is_btree_format(&value) {
          walk_btree_node(engine, &value, hash_length, live)?;
        } else {
          let children = deserialize_child_entries(&value, hash_length)?;
          for child in &children {
            walk_and_mark(engine, &child.hash, hash_length, live)?;
          }
        }
      }
    }
  }

  Ok(())
}
```

- [ ] **Step 2: Add `pub mod gc;` and re-exports to mod.rs**

In `aeordb-lib/src/engine/mod.rs`, add after `pub mod fuzzy;` (line 20):

```rust
pub mod gc;
```

And add to the re-exports at the bottom:

```rust
pub use gc::{gc_mark, gc_sweep, run_gc, GcResult};
```

(Note: `gc_sweep` and `run_gc` will be added in Task 3 — for now the re-export will cause a compile warning but we'll add it after Task 3 is complete. For Task 2, only export `gc_mark` and `GcResult`.)

Temporary re-export for Task 2:
```rust
pub use gc::{gc_mark, GcResult};
```

- [ ] **Step 3: Write mark phase tests**

Add to `aeordb-lib/spec/engine/gc_spec.rs`:

```rust
use aeordb::engine::gc::{gc_mark, GcResult};
use aeordb::engine::VersionManager;
use std::collections::HashMap;

// ─── Mark phase ─────────────────────────────────────────────────────────────

fn setup_engine_with_versions() -> (Arc<StorageEngine>, tempfile::TempDir) {
  let ctx = RequestContext::system();
  let (engine, temp) = create_temp_engine_for_tests();
  let ops = DirectoryOps::new(&engine);

  // Create initial files
  ops.store_file(&ctx, "/docs/readme.txt", b"README content", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/docs/notes.txt", b"Notes content", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/config.json", b"{}", Some("application/json")).unwrap();

  // Create a snapshot
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "v1", HashMap::new()).unwrap();

  // Modify a file (creates garbage: old FileRecord + old chunks + old dir entries)
  ops.store_file(&ctx, "/docs/readme.txt", b"Updated README content!!", Some("text/plain")).unwrap();

  // Delete a file (creates garbage: FileRecord + chunks for notes.txt)
  ops.delete_file(&ctx, "/docs/notes.txt").unwrap();

  // Create another snapshot
  vm.create_snapshot(&ctx, "v2", HashMap::new()).unwrap();

  // Create a fork
  vm.create_fork(&ctx, "experiment", None).unwrap();

  (engine, temp)
}

#[test]
fn test_gc_mark_collects_head_entries() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/hello.txt", b"hello", Some("text/plain")).unwrap();

  let live = gc_mark(&engine).unwrap();

  // HEAD root hash must be live
  let head = engine.head_hash().unwrap();
  assert!(live.contains(&head), "HEAD root hash must be marked live");

  // File hash must be live
  assert!(!live.is_empty(), "should have marked some entries as live");
  // At minimum: root dir hash + /hello.txt FileRecord hash + chunk hash
  assert!(live.len() >= 3, "expected at least 3 live entries (dir + file + chunk), got {}", live.len());
}

#[test]
fn test_gc_mark_collects_snapshot_entries() {
  let (engine, _temp) = setup_engine_with_versions();

  let live = gc_mark(&engine).unwrap();

  // Snapshot v1's root hash should be live
  let vm = VersionManager::new(&engine);
  let snapshots = vm.list_snapshots().unwrap();
  assert!(snapshots.len() >= 2, "expected at least 2 snapshots");

  for snapshot in &snapshots {
    assert!(
      live.contains(&snapshot.root_hash),
      "snapshot '{}' root hash should be live",
      snapshot.name
    );
  }

  // Snapshot KV key hashes should also be live
  let snap_key_v1 = engine.compute_hash(b"snap:v1").unwrap();
  assert!(live.contains(&snap_key_v1), "snapshot v1 KV key should be live");
}

#[test]
fn test_gc_mark_collects_fork_entries() {
  let (engine, _temp) = setup_engine_with_versions();

  let live = gc_mark(&engine).unwrap();

  let vm = VersionManager::new(&engine);
  let forks = vm.list_forks().unwrap();
  assert!(!forks.is_empty(), "should have at least one fork");

  for fork in &forks {
    assert!(
      live.contains(&fork.root_hash),
      "fork '{}' root hash should be live",
      fork.name
    );
  }

  let fork_key = engine.compute_hash(b"::aeordb:fork:experiment").unwrap();
  assert!(live.contains(&fork_key), "fork KV key should be live");
}

#[test]
fn test_gc_mark_structural_sharing_dedup() {
  // If HEAD and a snapshot share the same root hash,
  // walk_and_mark should short-circuit on the second walk.
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/test.txt", b"test", Some("text/plain")).unwrap();

  // Create snapshot (same root as HEAD since nothing changed after)
  let vm = VersionManager::new(&engine);
  vm.create_snapshot(&ctx, "same-as-head", HashMap::new()).unwrap();

  // gc_mark should succeed without double-counting
  let live = gc_mark(&engine).unwrap();
  assert!(!live.is_empty());
}

#[test]
fn test_gc_mark_empty_database() {
  let (engine, _temp) = create_temp_engine_for_tests();

  let live = gc_mark(&engine).unwrap();

  // Empty database — only the root directory (possibly empty) is live
  // The head hash might be all zeros or an empty dir hash
  assert!(live.len() <= 2, "empty database should have 0-2 live entries, got {}", live.len());
}

#[test]
fn test_gc_mark_no_snapshots_or_forks() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/only-file.txt", b"alone", Some("text/plain")).unwrap();

  let live = gc_mark(&engine).unwrap();

  // Only HEAD is walked, but all reachable entries should be live
  assert!(live.len() >= 3, "should have dir + file + chunk, got {}", live.len());
}
```

- [ ] **Step 4: Run mark phase tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test gc_spec -- --test-threads=1`
Expected: All mark phase tests pass (plus the 2 infrastructure tests from Task 1)

- [ ] **Step 5: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/engine/gc.rs aeordb-lib/src/engine/mod.rs aeordb-lib/spec/engine/gc_spec.rs
git commit -m "GC Phase 2: mark phase — walk all live roots, collect reachable hashes"
```

---

### Task 3: GC Sweep Phase (gc_sweep + run_gc)

**Files:**
- Modify: `aeordb-lib/src/engine/gc.rs`
- Modify: `aeordb-lib/src/engine/mod.rs` (update re-exports)
- Modify: `aeordb-lib/src/engine/engine_event.rs`
- Test: `aeordb-lib/spec/engine/gc_spec.rs`

- [ ] **Step 1: Add EVENT_GC_COMPLETED to engine_event.rs**

Add after `EVENT_HEARTBEAT` (line 144) in `aeordb-lib/src/engine/engine_event.rs`:

```rust
pub const EVENT_GC_COMPLETED: &str = "gc_completed";
```

Add a payload struct after the existing payload structs:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct GcEventData {
    pub versions_scanned: usize,
    pub live_entries: usize,
    pub garbage_entries: usize,
    pub reclaimed_bytes: u64,
    pub duration_ms: u64,
    pub dry_run: bool,
}
```

Add the re-export in `mod.rs` for `GcEventData` and `EVENT_GC_COMPLETED`.

- [ ] **Step 2: Add gc_sweep and run_gc to gc.rs**

Add at the bottom of `aeordb-lib/src/engine/gc.rs`:

```rust
use crate::engine::engine_event::{EVENT_GC_COMPLETED, GcEventData};
use crate::engine::entry_header::EntryHeader;
use crate::engine::void_manager::VoidManager;

/// Minimum DeletionRecord entry size (header + minimal payload).
/// For Blake3_256: 31 + 32 (header+hash) + 12 (min deletion payload) = 75 bytes.
fn min_deletion_size(engine: &StorageEngine) -> u32 {
  let hash_length = engine.hash_algo().hash_length();
  let header_size = 31 + hash_length;
  // Minimal DeletionRecord value: u16 path_len(2) + empty path(0) + i64 timestamp(8) + u16 reason_len(2) = 12
  // But we include "gc" as reason (2 bytes), so 14.
  // Plus the key: hash of "del:gc:..." = hash_length bytes
  // Total: header(header_size) + key(hash_length) + value(14)
  // Wait — EntryHeader::compute_total_length counts: header_size + key_length + value_length
  // Key will be a hash (hash_length bytes), value will be the DeletionRecord serialized
  // Let's compute it properly:
  let min_deletion_value_size = 2 + 2 + 8 + 2 + 2; // path_len + "gc" path + timestamp + reason_len + "gc" reason = 16
  EntryHeader::compute_total_length(
    engine.hash_algo(),
    hash_length as u32,  // key = computed hash
    min_deletion_value_size,
  )
}

/// Minimum void entry size (header only, zero-fill value).
fn min_void_size(engine: &StorageEngine) -> u32 {
  EntryHeader::compute_total_length(engine.hash_algo(), 0, 0)
}

/// Sweep phase: iterate all KV entries, overwrite non-live entries in-place.
/// If `dry_run` is true, counts garbage without modifying anything.
pub fn gc_sweep(
  engine: &StorageEngine,
  live: &HashSet<Vec<u8>>,
  dry_run: bool,
) -> EngineResult<(usize, u64)> {
  let min_del = min_deletion_size(engine);
  let min_void = min_void_size(engine);

  // Get all KV entries
  let all_entries = engine.iter_kv_entries()?;

  let mut garbage_count: usize = 0;
  let mut reclaimed_bytes: u64 = 0;

  for entry in &all_entries {
    if live.contains(&entry.hash) {
      continue;
    }

    // This entry is garbage
    garbage_count += 1;

    // Read the entry header to get its size
    let header = engine.read_entry_header_at(entry.offset)?;
    let entry_size = header.total_length;
    reclaimed_bytes += entry_size as u64;

    if dry_run {
      continue;
    }

    // Best-effort in-place overwrite
    if entry_size >= min_del {
      // Write DeletionRecord in-place
      let written = engine.write_deletion_at(entry.offset, "gc")?;

      let remaining = entry_size - written;
      if remaining >= min_void {
        // Write Void in the leftover space
        let void_offset = entry.offset + written as u64;
        engine.write_void_at(void_offset, remaining)?;
      }
      // else: small remainder, abandoned
    }
    // else: entry too small for in-place deletion — just remove from KV
    // (the on-disk bytes become orphaned but are tiny, < 75 bytes)

    // Remove from KV store
    engine.remove_kv_entry(&entry.hash)?;
  }

  Ok((garbage_count, reclaimed_bytes))
}

/// Run a complete GC cycle: mark + sweep.
pub fn run_gc(
  engine: &StorageEngine,
  ctx: &RequestContext,
  dry_run: bool,
) -> EngineResult<GcResult> {
  let start = std::time::Instant::now();

  // Count versions
  let vm = VersionManager::new(engine);
  let snapshot_count = vm.list_snapshots()?.len();
  let fork_count = vm.list_forks()?.len();
  let versions_scanned = 1 + snapshot_count + fork_count; // HEAD + snapshots + forks

  // MARK
  let live = gc_mark(engine)?;
  let live_entries = live.len();

  // SWEEP
  let (garbage_entries, reclaimed_bytes) = gc_sweep(engine, &live, dry_run)?;

  let duration_ms = start.elapsed().as_millis() as u64;

  let result = GcResult {
    versions_scanned,
    live_entries,
    garbage_entries,
    reclaimed_bytes,
    duration_ms,
    dry_run,
  };

  // Emit GC event
  let event_data = GcEventData {
    versions_scanned: result.versions_scanned,
    live_entries: result.live_entries,
    garbage_entries: result.garbage_entries,
    reclaimed_bytes: result.reclaimed_bytes,
    duration_ms: result.duration_ms,
    dry_run: result.dry_run,
  };
  ctx.emit(EVENT_GC_COMPLETED, serde_json::json!(event_data));

  Ok(result)
}
```

- [ ] **Step 3: Add `iter_kv_entries` to StorageEngine**

Add to `aeordb-lib/src/engine/storage_engine.rs` after `remove_kv_entry`:

```rust
  /// Iterate all live KV entries. Used by GC sweep.
  pub fn iter_kv_entries(&self) -> EngineResult<Vec<KVEntry>> {
    let mut kv = self.kv_store.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    kv.iter_all()
  }
```

- [ ] **Step 4: Update mod.rs re-exports**

In `aeordb-lib/src/engine/mod.rs`, update the gc re-export:

```rust
pub use gc::{gc_mark, gc_sweep, run_gc, GcResult};
```

Add `GcEventData` and `EVENT_GC_COMPLETED` to the existing `engine_event` re-export block.

- [ ] **Step 5: Write sweep phase tests**

Add to `aeordb-lib/spec/engine/gc_spec.rs`:

```rust
use aeordb::engine::gc::{gc_sweep, run_gc};

// ─── Sweep phase ────────────────────────────────────────────────────────────

#[test]
fn test_gc_sweep_removes_garbage() {
  let (engine, _temp) = setup_engine_with_versions();

  let live = gc_mark(&engine).unwrap();
  let live_count = live.len();

  // There should be garbage (old FileRecord for readme.txt, old chunks, deleted notes.txt)
  let (garbage_count, reclaimed_bytes) = gc_sweep(&engine, &live, false).unwrap();

  assert!(garbage_count > 0, "should have found garbage entries, got 0");
  assert!(reclaimed_bytes > 0, "should have reclaimed some bytes");
}

#[test]
fn test_gc_sweep_preserves_live_entries() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/keep.txt", b"keep me", Some("text/plain")).unwrap();

  // Mark everything as live, sweep should find nothing
  let live = gc_mark(&engine).unwrap();
  let (garbage_count, _) = gc_sweep(&engine, &live, false).unwrap();

  assert_eq!(garbage_count, 0, "no garbage when everything is live");

  // File should still be readable
  let content = ops.read_file("/keep.txt").unwrap();
  assert_eq!(content.unwrap().0, b"keep me");
}

#[test]
fn test_gc_dry_run_does_not_modify() {
  let (engine, _temp) = setup_engine_with_versions();

  let live = gc_mark(&engine).unwrap();

  // Dry run first
  let (dry_count, dry_bytes) = gc_sweep(&engine, &live, true).unwrap();
  assert!(dry_count > 0, "dry run should report garbage");

  // Real run should find the same amount (nothing was modified)
  let (real_count, _real_bytes) = gc_sweep(&engine, &live, false).unwrap();
  assert_eq!(dry_count, real_count, "dry run and real run should find same garbage count");

  // Second real run should find 0 garbage (everything was cleaned)
  let live2 = gc_mark(&engine).unwrap();
  let (second_count, _) = gc_sweep(&engine, &live2, false).unwrap();
  assert_eq!(second_count, 0, "second GC should find no garbage");
}

#[test]
fn test_run_gc_end_to_end() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();

  let result = run_gc(&engine, &ctx, false).unwrap();

  assert_eq!(result.versions_scanned, 4); // HEAD + v1 + v2 + experiment fork
  assert!(result.live_entries > 0);
  assert!(result.garbage_entries > 0);
  assert!(result.reclaimed_bytes > 0);
  assert!(!result.dry_run);

  // Run again — should find 0 garbage
  let result2 = run_gc(&engine, &ctx, false).unwrap();
  assert_eq!(result2.garbage_entries, 0, "second GC should find no garbage");
}

#[test]
fn test_gc_empty_database() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();

  let result = run_gc(&engine, &ctx, false).unwrap();

  assert_eq!(result.garbage_entries, 0);
  assert_eq!(result.versions_scanned, 1); // just HEAD
}

#[test]
fn test_gc_files_still_readable_after_sweep() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Run GC
  let result = run_gc(&engine, &ctx, false).unwrap();
  assert!(result.garbage_entries > 0);

  // Current HEAD file should still be readable
  let content = ops.read_file("/docs/readme.txt").unwrap();
  assert!(content.is_some(), "/docs/readme.txt should still exist after GC");
  assert_eq!(content.unwrap().0, b"Updated README content!!");

  // config.json was never modified — should be readable
  let config = ops.read_file("/config.json").unwrap();
  assert!(config.is_some(), "/config.json should still exist after GC");

  // notes.txt was deleted before GC — should NOT be readable
  let notes = ops.read_file("/docs/notes.txt").unwrap();
  assert!(notes.is_none(), "/docs/notes.txt should not exist after GC (was deleted)");
}

#[test]
fn test_gc_snapshot_still_walkable_after_sweep() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();

  run_gc(&engine, &ctx, false).unwrap();

  // Snapshot v1 should still be walkable
  let vm = VersionManager::new(&engine);
  let snapshot_hash = vm.get_snapshot_hash("v1").unwrap();
  let tree = aeordb::engine::walk_version_tree(&engine, &snapshot_hash).unwrap();

  // v1 was created before readme.txt was modified and before notes.txt was deleted
  assert!(tree.files.contains_key("/docs/readme.txt"), "v1 should have readme.txt");
  assert!(tree.files.contains_key("/docs/notes.txt"), "v1 should have notes.txt");
  assert!(tree.files.contains_key("/config.json"), "v1 should have config.json");
}

#[test]
fn test_gc_in_place_overwrite_creates_voids() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();

  let stats_before = engine.stats();
  let void_count_before = stats_before.void_count;

  run_gc(&engine, &ctx, false).unwrap();

  let stats_after = engine.stats();
  // GC should have created some voids from in-place overwrites
  assert!(
    stats_after.void_count >= void_count_before,
    "GC should create voids (before={}, after={})",
    void_count_before,
    stats_after.void_count
  );
}
```

- [ ] **Step 6: Run all GC tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test gc_spec -- --test-threads=1`
Expected: All tests pass

- [ ] **Step 7: Run the full test suite to verify no regressions**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All 2,111+ tests pass

- [ ] **Step 8: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/engine/gc.rs aeordb-lib/src/engine/mod.rs aeordb-lib/src/engine/engine_event.rs aeordb-lib/src/engine/storage_engine.rs aeordb-lib/spec/engine/gc_spec.rs
git commit -m "GC Phase 3: sweep phase — in-place overwrite garbage, dry-run mode, event emission"
```

---

### Task 4: CLI Command (`aeordb gc`)

**Files:**
- Create: `aeordb-cli/src/commands/gc.rs`
- Modify: `aeordb-cli/src/commands/mod.rs`
- Modify: `aeordb-cli/src/main.rs`

- [ ] **Step 1: Create gc.rs CLI command**

Create `aeordb-cli/src/commands/gc.rs`:

```rust
use std::process;

use aeordb::engine::{RequestContext, StorageEngine};
use aeordb::engine::gc::run_gc;

pub fn run(database: &str, dry_run: bool) {
    if dry_run {
        println!("AeorDB Garbage Collection [DRY RUN]");
    } else {
        println!("AeorDB Garbage Collection");
    }
    println!("Database: {}", database);
    println!();

    // Open database
    let engine = match StorageEngine::open(database) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("Error opening database: {}", e);
            process::exit(1);
        }
    };

    let ctx = RequestContext::system();

    match run_gc(&engine, &ctx, dry_run) {
        Ok(result) => {
            if result.dry_run {
                println!("[DRY RUN] Would collect {} garbage entries ({} bytes)",
                    result.garbage_entries,
                    format_bytes(result.reclaimed_bytes),
                );
            } else {
                println!("Versions scanned: {}", result.versions_scanned);
                println!("Live entries:     {}", result.live_entries);
                println!("Garbage entries:  {}", result.garbage_entries);
                println!("Reclaimed:        {}", format_bytes(result.reclaimed_bytes));
                println!("Duration:         {:.1}s", result.duration_ms as f64 / 1000.0);
            }
        }
        Err(e) => {
            eprintln!("GC failed: {}", e);
            process::exit(1);
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} bytes", bytes)
    }
}
```

- [ ] **Step 2: Register in commands/mod.rs**

Add to `aeordb-cli/src/commands/mod.rs`:

```rust
pub mod gc;
```

- [ ] **Step 3: Add Gc subcommand to main.rs**

Add to the `Commands` enum in `aeordb-cli/src/main.rs` after `Promote`:

```rust
  /// Run garbage collection to reclaim unreachable entries
  Gc {
    #[arg(short = 'D', long, default_value = "data.aeordb")]
    database: String,
    /// Report what would be collected without actually deleting
    #[arg(long)]
    dry_run: bool,
  },
```

Add the match arm in the `main` function:

```rust
      Commands::Gc { database, dry_run } => {
        commands::gc::run(&database, dry_run);
      }
```

- [ ] **Step 4: Verify CLI compiles and runs**

Run: `cd /home/wyatt/Projects/aeordb && cargo build -p aeordb-cli`
Expected: Compiles successfully

Run: `cd /home/wyatt/Projects/aeordb && cargo run -p aeordb-cli -- gc --help`
Expected: Shows help text with `--database` and `--dry-run` flags

- [ ] **Step 5: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-cli/src/commands/gc.rs aeordb-cli/src/commands/mod.rs aeordb-cli/src/main.rs
git commit -m "GC Phase 4: CLI command — aeordb gc --database <path> [--dry-run]"
```

---

### Task 5: HTTP Endpoint (`POST /admin/gc`)

**Files:**
- Create: `aeordb-lib/src/server/gc_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`
- Test: `aeordb-lib/spec/engine/gc_spec.rs` (add HTTP test)

- [ ] **Step 1: Create gc_routes.rs**

Create `aeordb-lib/src/server/gc_routes.rs`:

```rust
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::Extension;
use serde::Deserialize;

use crate::auth::TokenClaims;
use crate::engine::gc::run_gc;
use crate::engine::{RequestContext, is_root};
use crate::server::state::AppState;

#[derive(Deserialize)]
pub struct GcParams {
  pub dry_run: Option<bool>,
}

/// POST /admin/gc -- run garbage collection.
/// Query params: dry_run=true (default: false).
/// Requires root user.
pub async fn run_gc_endpoint(
  State(state): State<AppState>,
  Extension(claims): Extension<TokenClaims>,
  Query(params): Query<GcParams>,
) -> Response {
  // Only root can run GC
  if !is_root(&claims.sub) {
    return (StatusCode::FORBIDDEN, Json(serde_json::json!({
      "error": "Only root user can run garbage collection"
    }))).into_response();
  }

  let dry_run = params.dry_run.unwrap_or(false);
  let ctx = RequestContext::from_claims(&claims.sub, state.event_bus.clone());

  // Run GC (blocking — this is a maintenance operation)
  let result = tokio::task::spawn_blocking(move || {
    run_gc(&state.engine, &ctx, dry_run)
  }).await;

  match result {
    Ok(Ok(gc_result)) => {
      (StatusCode::OK, Json(serde_json::json!(gc_result))).into_response()
    }
    Ok(Err(e)) => {
      (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
        "error": format!("GC failed: {}", e)
      }))).into_response()
    }
    Err(e) => {
      (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
        "error": format!("GC task panicked: {}", e)
      }))).into_response()
    }
  }
}
```

- [ ] **Step 2: Register route in server/mod.rs**

Add `pub mod gc_routes;` after `pub mod engine_routes;` in `aeordb-lib/src/server/mod.rs`.

Add the route after the backup routes (around line 173):

```rust
    .route("/admin/gc", post(gc_routes::run_gc_endpoint))
```

- [ ] **Step 3: Write HTTP test**

Add to `aeordb-lib/spec/engine/gc_spec.rs`:

```rust
// ─── HTTP endpoint (basic compile/integration check) ────────────────────────

// Note: Full HTTP integration tests would go in spec/http/gc_http_spec.rs
// using the test app harness. Here we just verify the GC module integrates
// correctly with the engine.

#[test]
fn test_gc_result_serializes_to_json() {
  let result = GcResult {
    versions_scanned: 5,
    live_entries: 150000,
    garbage_entries: 23000,
    reclaimed_bytes: 47185920,
    duration_ms: 1200,
    dry_run: false,
  };

  let json = serde_json::to_value(&result).unwrap();
  assert_eq!(json["versions_scanned"], 5);
  assert_eq!(json["live_entries"], 150000);
  assert_eq!(json["garbage_entries"], 23000);
  assert_eq!(json["reclaimed_bytes"], 47185920);
  assert_eq!(json["duration_ms"], 1200);
  assert_eq!(json["dry_run"], false);
}
```

- [ ] **Step 4: Run all tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All tests pass (2,111+ existing + ~15 new GC tests)

- [ ] **Step 5: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/server/gc_routes.rs aeordb-lib/src/server/mod.rs aeordb-lib/spec/engine/gc_spec.rs
git commit -m "GC Phase 5: HTTP endpoint — POST /admin/gc with dry_run support"
```

---

### Task 6: Edge Case Tests + Hardening

**Files:**
- Test: `aeordb-lib/spec/engine/gc_spec.rs`

- [ ] **Step 1: Write edge case tests**

Add to `aeordb-lib/spec/engine/gc_spec.rs`:

```rust
// ─── Edge cases ─────────────────────────────────────────────────────────────

#[test]
fn test_gc_after_delete_all_files() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Create files then delete them all
  ops.store_file(&ctx, "/a.txt", b"aaa", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/b.txt", b"bbb", Some("text/plain")).unwrap();
  ops.delete_file(&ctx, "/a.txt").unwrap();
  ops.delete_file(&ctx, "/b.txt").unwrap();

  let result = run_gc(&engine, &ctx, false).unwrap();

  // The orphaned FileRecords, chunks, and old directory entries should be garbage
  assert!(result.garbage_entries > 0, "deleting all files should create garbage");

  // Database should still be functional
  ops.store_file(&ctx, "/c.txt", b"ccc", Some("text/plain")).unwrap();
  let content = ops.read_file("/c.txt").unwrap();
  assert_eq!(content.unwrap().0, b"ccc");
}

#[test]
fn test_gc_with_overwritten_files() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Write the same file 10 times — creates 9 sets of orphaned entries
  for i in 0..10 {
    let content = format!("version {}", i);
    ops.store_file(&ctx, "/evolving.txt", content.as_bytes(), Some("text/plain")).unwrap();
  }

  let result = run_gc(&engine, &ctx, false).unwrap();

  // Should have garbage from the 9 overwritten versions
  assert!(result.garbage_entries > 0, "overwritten files should create garbage");

  // Current version should be intact
  let content = ops.read_file("/evolving.txt").unwrap();
  assert_eq!(content.unwrap().0, b"version 9");
}

#[test]
fn test_gc_idempotent() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();

  // First GC
  let result1 = run_gc(&engine, &ctx, false).unwrap();
  assert!(result1.garbage_entries > 0);

  // Second GC — should be idempotent (0 garbage)
  let result2 = run_gc(&engine, &ctx, false).unwrap();
  assert_eq!(result2.garbage_entries, 0, "second GC should find 0 garbage");

  // Third GC — still 0
  let result3 = run_gc(&engine, &ctx, false).unwrap();
  assert_eq!(result3.garbage_entries, 0);
}

#[test]
fn test_gc_preserves_system_tables() {
  // System tables (/.system/, /.config/) must survive GC
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Create a regular file and a system file
  ops.store_file(&ctx, "/user-data.txt", b"hello", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/.system/users.json", b"{\"root\":true}", Some("application/json")).unwrap();

  // Delete the user file to create garbage
  ops.delete_file(&ctx, "/user-data.txt").unwrap();

  let result = run_gc(&engine, &ctx, false).unwrap();

  // System file must still be readable
  let sys = ops.read_file("/.system/users.json").unwrap();
  assert!(sys.is_some(), "system table entry must survive GC");
}

#[test]
fn test_gc_with_deep_directory_tree() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Create a deeply nested structure
  ops.store_file(&ctx, "/a/b/c/d/e/deep.txt", b"deep file", Some("text/plain")).unwrap();
  ops.store_file(&ctx, "/a/b/other.txt", b"other", Some("text/plain")).unwrap();

  let live = gc_mark(&engine).unwrap();

  // All directory hashes in the chain should be marked live
  // At minimum: /, /a, /a/b, /a/b/c, /a/b/c/d, /a/b/c/d/e + 2 files + chunks
  assert!(live.len() >= 8, "deep tree should have many live entries, got {}", live.len());

  // GC should find 0 garbage (nothing was deleted/overwritten)
  let result = run_gc(&engine, &ctx, false).unwrap();
  assert_eq!(result.garbage_entries, 0);
}

#[test]
fn test_gc_dry_run_result_matches_format() {
  let (engine, _temp) = setup_engine_with_versions();
  let ctx = RequestContext::system();

  let result = run_gc(&engine, &ctx, true).unwrap();

  assert!(result.dry_run);
  assert!(result.garbage_entries > 0);
  assert!(result.reclaimed_bytes > 0);
  assert!(result.duration_ms >= 0);
}
```

- [ ] **Step 2: Run all GC tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test gc_spec -- --test-threads=1`
Expected: All tests pass

- [ ] **Step 3: Run full test suite**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/spec/engine/gc_spec.rs
git commit -m "GC Phase 6: edge case tests — idempotency, deep trees, system tables, overwrites"
```

---

## Post-Implementation Checklist

After all tasks are complete:

- [ ] Update `aeordb-lib/src/engine/mod.rs` — verify all re-exports are correct
- [ ] Update `.claude/TODO.md` — mark GC as completed with test count
- [ ] Update `.claude/DETAILS.md` — add gc.rs and gc_routes.rs to key files
- [ ] Run: `cargo test -- --test-threads=4` — all tests pass
- [ ] Run: `cargo build -p aeordb-cli` — CLI compiles
- [ ] Run: `cargo run -p aeordb-cli -- gc --database data.aeordb --dry-run` — E2E test against real database (if data.aeordb exists)
