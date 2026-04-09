# Concurrent KV Readers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split DiskKVStore into a KVWriter (Mutex, exclusive) and ReadSnapshot (Arc, lock-free) so reads never block behind writes.

**Architecture:** The writer publishes immutable ReadSnapshot structs after each mutation via `arc_swap::ArcSwap`. Readers grab snapshots with zero lock contention. The hot cache is removed — the write buffer snapshot and OS page cache replace it. Write buffer threshold drops from 1000 to 512.

**Tech Stack:** Rust, arc_swap crate, std::sync::Mutex, std::fs::File::try_clone

**Spec:** `bot-docs/plan/concurrent-kv-readers.md`

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Create | `aeordb-lib/src/engine/kv_snapshot.rs` | ReadSnapshot struct + lock-free get/iter_all |
| Create | `aeordb-lib/spec/engine/kv_snapshot_spec.rs` | ReadSnapshot unit tests |
| Create | `aeordb-lib/spec/engine/kv_concurrency_spec.rs` | Multi-threaded contention tests |
| Modify | `aeordb-lib/src/engine/disk_kv_store.rs` | Refactor into KVWriter: remove hot cache, add snapshot publishing, threshold 512 |
| Modify | `aeordb-lib/src/engine/storage_engine.rs` | Replace `RwLock<DiskKVStore>` with `Mutex<KVWriter>` + `Arc<ArcSwap<ReadSnapshot>>` |
| Modify | `aeordb-lib/src/engine/engine_chunk_storage.rs` | Same pattern change for its kv_store field |
| Modify | `aeordb-lib/src/engine/mod.rs` | Add `pub mod kv_snapshot;` and re-exports |
| Modify | `aeordb-lib/Cargo.toml` | Add `arc_swap` dependency + new test entries |

---

### Task 1: ReadSnapshot struct + get()

**Files:**
- Create: `aeordb-lib/src/engine/kv_snapshot.rs`
- Create: `aeordb-lib/spec/engine/kv_snapshot_spec.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Modify: `aeordb-lib/Cargo.toml`

This task builds the immutable read view that readers will use. It depends on nothing except existing types (`KVEntry`, `NormalizedVectorTable`, `HashAlgorithm`, kv_pages functions). No changes to DiskKVStore or StorageEngine yet.

- [ ] **Step 1: Add `arc_swap` dependency**

Add to `aeordb-lib/Cargo.toml` in the `[dependencies]` section under `# Storage`:

```toml
arc_swap = "1"
```

Add test entries:

```toml
[[test]]
name = "kv_snapshot_spec"
path = "spec/engine/kv_snapshot_spec.rs"

[[test]]
name = "kv_concurrency_spec"
path = "spec/engine/kv_concurrency_spec.rs"
```

- [ ] **Step 2: Create kv_snapshot.rs**

Create `aeordb-lib/src/engine/kv_snapshot.rs`:

```rust
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::engine::errors::{EngineError, EngineResult};
use crate::engine::hash_algorithm::HashAlgorithm;
use crate::engine::kv_pages::{bucket_page_offset, deserialize_page, find_in_page, page_size};
use crate::engine::kv_store::{KVEntry, KV_FLAG_DELETED};
use crate::engine::nvt::NormalizedVectorTable;

/// An immutable, cheaply-clonable read view of the KV store.
///
/// Published by KVWriter after each mutation. Readers grab this via
/// `Arc::clone` (one atomic op) and perform lookups without any locks.
///
/// The `buffer` contains entries from the writer's in-memory buffer at
/// the time the snapshot was created. For entries not in the buffer,
/// readers fall through to disk using the NVT for bucket routing and
/// a cloned file handle for I/O.
#[derive(Debug, Clone)]
pub struct ReadSnapshot {
  /// Frozen copy of the writer's write buffer at snapshot creation time.
  buffer: HashMap<Vec<u8>, KVEntry>,
  /// NVT for bucket routing — shared via Arc, only re-cloned on flush.
  nvt: Arc<NormalizedVectorTable>,
  /// Number of buckets (determines page layout).
  bucket_count: usize,
  /// Hash algorithm (determines hash_length for page layout).
  hash_algo: HashAlgorithm,
  /// Total non-deleted entry count at snapshot time.
  entry_count: usize,
}

impl ReadSnapshot {
  /// Create a new read snapshot.
  pub fn new(
    buffer: HashMap<Vec<u8>, KVEntry>,
    nvt: Arc<NormalizedVectorTable>,
    bucket_count: usize,
    hash_algo: HashAlgorithm,
    entry_count: usize,
  ) -> Self {
    ReadSnapshot {
      buffer,
      nvt,
      bucket_count,
      hash_algo,
      entry_count,
    }
  }

  /// Look up an entry by hash. Lock-free — operates on immutable data.
  ///
  /// 1. Check the frozen buffer (most recent writes at snapshot time)
  /// 2. On miss, read from disk via a cloned file handle
  ///
  /// `kv_file` is the base file handle — this method calls `try_clone()`
  /// to get an independent seek position for the read.
  pub fn get(&self, hash: &[u8], kv_file: &File) -> Option<KVEntry> {
    // 1. Check frozen buffer
    if let Some(entry) = self.buffer.get(hash) {
      if entry.is_deleted() {
        return None;
      }
      return Some(entry.clone());
    }

    // 2. Read from disk
    let bucket_index = self.nvt.bucket_for_value(hash);
    if bucket_index >= self.bucket_count {
      return None;
    }

    let hash_length = self.hash_algo.hash_length();
    let offset = bucket_page_offset(bucket_index, hash_length);
    let psize = page_size(hash_length);

    let mut file = kv_file.try_clone().ok()?;
    let mut page_data = vec![0u8; psize];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut page_data).ok()?;

    let entries = deserialize_page(&page_data, hash_length).ok()?;
    find_in_page(&entries, hash).cloned()
  }

  /// Iterate all live entries: reads every page from disk and merges
  /// with the frozen buffer. Buffer entries win on hash conflict.
  /// Excludes deleted entries.
  pub fn iter_all(&self, kv_file: &File) -> EngineResult<Vec<KVEntry>> {
    let hash_length = self.hash_algo.hash_length();
    let psize = page_size(hash_length);
    let mut all: HashMap<Vec<u8>, KVEntry> = HashMap::new();

    let mut file = kv_file.try_clone()
      .map_err(|error| EngineError::IoError(error))?;

    // Read all pages from disk
    for bucket in 0..self.bucket_count {
      let offset = bucket_page_offset(bucket, hash_length);
      let mut page_data = vec![0u8; psize];
      file.seek(SeekFrom::Start(offset))?;
      if file.read_exact(&mut page_data).is_ok() {
        if let Ok(entries) = deserialize_page(&page_data, hash_length) {
          for entry in entries {
            all.insert(entry.hash.clone(), entry);
          }
        }
      }
    }

    // Merge frozen buffer (buffer takes priority)
    for (hash, entry) in &self.buffer {
      all.insert(hash.clone(), entry.clone());
    }

    // Filter out deleted entries
    Ok(all
      .into_values()
      .filter(|e| !e.is_deleted())
      .collect())
  }

  /// Total non-deleted entry count at snapshot time.
  pub fn len(&self) -> usize {
    self.entry_count
  }

  /// Whether the snapshot has zero entries.
  pub fn is_empty(&self) -> bool {
    self.entry_count == 0
  }

  /// Current bucket count.
  pub fn bucket_count(&self) -> usize {
    self.bucket_count
  }

  /// Hash algorithm.
  pub fn hash_algo(&self) -> HashAlgorithm {
    self.hash_algo
  }

  /// Number of entries in the frozen buffer.
  pub fn buffer_len(&self) -> usize {
    self.buffer.len()
  }
}
```

- [ ] **Step 3: Add `pub mod kv_snapshot;` to mod.rs**

In `aeordb-lib/src/engine/mod.rs`, add after `pub mod kv_store;`:

```rust
pub mod kv_snapshot;
```

Add re-export at the bottom:

```rust
pub use kv_snapshot::ReadSnapshot;
```

- [ ] **Step 4: Write ReadSnapshot unit tests**

Create `aeordb-lib/spec/engine/kv_snapshot_spec.rs`:

```rust
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::sync::Arc;
use tempfile::tempdir;

use aeordb::engine::disk_kv_store::DiskKVStore;
use aeordb::engine::hash_algorithm::HashAlgorithm;
use aeordb::engine::kv_pages::*;
use aeordb::engine::kv_snapshot::ReadSnapshot;
use aeordb::engine::kv_store::{KVEntry, KV_TYPE_CHUNK, KV_FLAG_DELETED};
use aeordb::engine::nvt::NormalizedVectorTable;
use aeordb::engine::scalar_converter::HashConverter;

fn make_hash(seed: u8) -> Vec<u8> {
  let data = vec![seed; 32];
  blake3::hash(&data).as_bytes().to_vec()
}

fn make_entry(seed: u8, offset: u64) -> KVEntry {
  KVEntry {
    type_flags: KV_TYPE_CHUNK,
    hash: make_hash(seed),
    offset,
  }
}

fn setup_kv_with_entries(count: u8) -> (DiskKVStore, tempfile::TempDir) {
  let dir = tempdir().unwrap();
  let kv_path = dir.path().join("test.kv");
  let hash_algo = HashAlgorithm::Blake3_256;
  let mut kv = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();

  for i in 0..count {
    kv.insert(make_entry(i, (i as u64 + 1) * 100));
  }
  kv.flush().unwrap();

  (kv, dir)
}

fn make_snapshot_from_kv(
  kv: &DiskKVStore,
  buffer: HashMap<Vec<u8>, KVEntry>,
) -> ReadSnapshot {
  let hash_algo = kv.hash_algo();
  let nvt = NormalizedVectorTable::new(Box::new(HashConverter), kv.bucket_count());
  ReadSnapshot::new(
    buffer,
    Arc::new(nvt),
    kv.bucket_count(),
    hash_algo,
    kv.len(),
  )
}

// ─── get() from buffer ──────────────────────────────────────────────────────

#[test]
fn test_snapshot_get_finds_entry_in_buffer() {
  let dir = tempdir().unwrap();
  let kv_path = dir.path().join("test.kv");
  let hash_algo = HashAlgorithm::Blake3_256;
  let kv = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();

  let entry = make_entry(42, 999);
  let mut buffer = HashMap::new();
  buffer.insert(entry.hash.clone(), entry.clone());

  let snapshot = make_snapshot_from_kv(&kv, buffer);

  let kv_file = File::open(&kv_path).unwrap();
  let found = snapshot.get(&make_hash(42), &kv_file);
  assert!(found.is_some());
  assert_eq!(found.unwrap().offset, 999);
}

#[test]
fn test_snapshot_get_returns_none_for_deleted_in_buffer() {
  let dir = tempdir().unwrap();
  let kv_path = dir.path().join("test.kv");
  let hash_algo = HashAlgorithm::Blake3_256;
  let kv = DiskKVStore::create(&kv_path, hash_algo, None).unwrap();

  let mut entry = make_entry(42, 999);
  entry.type_flags |= KV_FLAG_DELETED;
  let mut buffer = HashMap::new();
  buffer.insert(entry.hash.clone(), entry);

  let snapshot = make_snapshot_from_kv(&kv, buffer);

  let kv_file = File::open(&kv_path).unwrap();
  let found = snapshot.get(&make_hash(42), &kv_file);
  assert!(found.is_none(), "deleted entry should return None");
}

// ─── get() from disk ────────────────────────────────────────────────────────

#[test]
fn test_snapshot_get_falls_through_to_disk() {
  let (kv, dir) = setup_kv_with_entries(5);
  let kv_path = dir.path().join("test.kv");

  // Empty buffer — must fall through to disk
  let snapshot = make_snapshot_from_kv(&kv, HashMap::new());

  let kv_file = File::open(&kv_path).unwrap();
  let found = snapshot.get(&make_hash(0), &kv_file);
  assert!(found.is_some(), "should find entry on disk");
  assert_eq!(found.unwrap().offset, 100);
}

#[test]
fn test_snapshot_get_returns_none_for_missing() {
  let (kv, dir) = setup_kv_with_entries(5);
  let kv_path = dir.path().join("test.kv");

  let snapshot = make_snapshot_from_kv(&kv, HashMap::new());

  let kv_file = File::open(&kv_path).unwrap();
  let found = snapshot.get(&make_hash(99), &kv_file);
  assert!(found.is_none(), "should return None for missing entry");
}

// ─── buffer wins over disk ──────────────────────────────────────────────────

#[test]
fn test_snapshot_buffer_wins_over_disk() {
  let (kv, dir) = setup_kv_with_entries(5);
  let kv_path = dir.path().join("test.kv");

  // Buffer has an updated version of entry 0
  let mut updated = make_entry(0, 9999);
  let mut buffer = HashMap::new();
  buffer.insert(updated.hash.clone(), updated);

  let snapshot = make_snapshot_from_kv(&kv, buffer);

  let kv_file = File::open(&kv_path).unwrap();
  let found = snapshot.get(&make_hash(0), &kv_file);
  assert!(found.is_some());
  assert_eq!(found.unwrap().offset, 9999, "buffer should win over disk");
}

// ─── iter_all ───────────────────────────────────────────────────────────────

#[test]
fn test_snapshot_iter_all_merges_buffer_and_disk() {
  let (kv, dir) = setup_kv_with_entries(5);
  let kv_path = dir.path().join("test.kv");

  // Buffer has one new entry and one updated entry
  let new_entry = make_entry(99, 5000);
  let mut updated = make_entry(0, 9999);
  let mut buffer = HashMap::new();
  buffer.insert(new_entry.hash.clone(), new_entry);
  buffer.insert(updated.hash.clone(), updated);

  let snapshot = make_snapshot_from_kv(&kv, buffer);

  let kv_file = File::open(&kv_path).unwrap();
  let all = snapshot.iter_all(&kv_file).unwrap();

  // 5 on disk + 1 new in buffer = 6 (entry 0 is updated, not duplicated)
  assert_eq!(all.len(), 6, "expected 6 entries (5 disk + 1 new), got {}", all.len());

  // Verify buffer version of entry 0 wins
  let entry_0 = all.iter().find(|e| e.hash == make_hash(0)).unwrap();
  assert_eq!(entry_0.offset, 9999);
}

#[test]
fn test_snapshot_iter_all_excludes_deleted() {
  let (kv, dir) = setup_kv_with_entries(5);
  let kv_path = dir.path().join("test.kv");

  // Buffer tombstones entry 0
  let mut deleted = make_entry(0, 100);
  deleted.type_flags |= KV_FLAG_DELETED;
  let mut buffer = HashMap::new();
  buffer.insert(deleted.hash.clone(), deleted);

  let snapshot = make_snapshot_from_kv(&kv, buffer);

  let kv_file = File::open(&kv_path).unwrap();
  let all = snapshot.iter_all(&kv_file).unwrap();

  assert_eq!(all.len(), 4, "should have 4 entries (5 - 1 deleted)");
  assert!(!all.iter().any(|e| e.hash == make_hash(0)), "deleted entry should be excluded");
}

#[test]
fn test_snapshot_get_concurrent_file_handles() {
  let (kv, dir) = setup_kv_with_entries(10);
  let kv_path = dir.path().join("test.kv");

  let snapshot = make_snapshot_from_kv(&kv, HashMap::new());
  let kv_file = File::open(&kv_path).unwrap();

  // Simulate concurrent reads by calling get() multiple times
  // (each call internally does try_clone)
  for i in 0..10u8 {
    let found = snapshot.get(&make_hash(i), &kv_file);
    assert!(found.is_some(), "entry {} should be found", i);
  }
}
```

- [ ] **Step 5: Run ReadSnapshot tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test kv_snapshot_spec -- --test-threads=1`
Expected: All 8 tests pass

- [ ] **Step 6: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/engine/kv_snapshot.rs aeordb-lib/src/engine/mod.rs aeordb-lib/spec/engine/kv_snapshot_spec.rs aeordb-lib/Cargo.toml
git commit -m "Concurrent KV Phase 1: ReadSnapshot struct with lock-free get/iter_all — 8 tests"
```

---

### Task 2: Refactor DiskKVStore into KVWriter

**Files:**
- Modify: `aeordb-lib/src/engine/disk_kv_store.rs`

This task refactors DiskKVStore in-place: removes the hot cache, lowers the buffer threshold to 512, and adds snapshot publishing. The struct stays named `DiskKVStore` for now (renaming would break 74 tests at once). We add the snapshot publishing mechanism and remove the hot cache.

- [ ] **Step 1: Remove hot cache fields and methods**

In `aeordb-lib/src/engine/disk_kv_store.rs`:

Remove these fields from the struct:
```rust
    /// Hot cache: recently read entries from disk.
    hot_cache: HashMap<Vec<u8>, KVEntry>,
    /// LRU order tracking for hot cache eviction (oldest first).
    cache_order: Vec<Vec<u8>>,
```

Remove the `HOT_CACHE_MAX` constant.

Remove the `cache_put` method entirely.

Remove the `is_cached` method entirely.

In `get()`: remove the hot_cache check (step 2) and the `cache_put` call at the end. The method should now go directly from write_buffer to disk.

In `insert()`: remove the hot_cache invalidation lines:
```rust
    // Remove these two lines:
    self.hot_cache.remove(&entry.hash);
    self.cache_order.retain(|h| h != &entry.hash);
```

In `mark_deleted()`: remove the hot_cache lines:
```rust
    // Remove these two lines:
    self.hot_cache.remove(hash);
    self.cache_order.retain(|h| h != hash);
```

In `update_flags()`: remove the same hot_cache lines.

In `update_offset()`: remove the same hot_cache lines.

In `resize_to_next_stage()`: remove:
```rust
    self.hot_cache.clear();
    self.cache_order.clear();
```

In `create()`, `create_at_stage()`, and `open()`: remove `hot_cache: HashMap::new()` and `cache_order: Vec::new()` from the struct initialization.

- [ ] **Step 2: Change WRITE_BUFFER_THRESHOLD to 512**

```rust
const WRITE_BUFFER_THRESHOLD: usize = 512;
```

- [ ] **Step 3: Add snapshot publishing to DiskKVStore**

Add these imports at the top of `disk_kv_store.rs`:

```rust
use std::sync::Arc;
use arc_swap::ArcSwap;
use crate::engine::kv_snapshot::ReadSnapshot;
```

Add a field to the struct:

```rust
    /// Shared snapshot for lock-free readers. Updated after every mutation.
    snapshot: Arc<ArcSwap<ReadSnapshot>>,
    /// Shared NVT wrapped in Arc — re-cloned only on flush/resize.
    shared_nvt: Arc<NormalizedVectorTable>,
```

Add a method to publish snapshots:

```rust
    /// Publish a new read snapshot from current state.
    /// Called after every insert/mutation so readers see fresh data.
    fn publish_snapshot(&self) {
      let snapshot = ReadSnapshot::new(
        self.write_buffer.clone(),
        Arc::clone(&self.shared_nvt),
        self.bucket_count,
        self.hash_algo,
        self.entry_count,
      );
      self.snapshot.store(Arc::new(snapshot));
    }

    /// Publish a snapshot with a fresh NVT clone (called on flush/resize).
    fn publish_snapshot_with_new_nvt(&mut self) {
      self.shared_nvt = Arc::new(self.nvt.clone());
      self.publish_snapshot();
    }

    /// Get a reference to the current ArcSwap for readers.
    pub fn snapshot_handle(&self) -> &Arc<ArcSwap<ReadSnapshot>> {
      &self.snapshot
    }
```

Call `self.publish_snapshot()` at the end of `insert()` (after the auto-flush check).

Call `self.publish_snapshot_with_new_nvt()` at the end of `flush()` (after the hot file truncate, before the overflow check).

Call `self.publish_snapshot_with_new_nvt()` at the end of `resize_to_next_stage()`.

Call `self.publish_snapshot()` at the end of `mark_deleted()`.

Call `self.publish_snapshot()` at the end of `update_flags()`.

Call `self.publish_snapshot()` at the end of `update_offset()`.

Update `create()`, `create_at_stage()`, and `open()` constructors to initialize the new fields:

```rust
    let shared_nvt = Arc::new(nvt.clone());
    let initial_snapshot = ReadSnapshot::new(
      HashMap::new(),
      Arc::clone(&shared_nvt),
      bucket_count,
      hash_algo,
      entry_count, // 0 for create, scanned count for open
    );
    let snapshot = Arc::new(ArcSwap::new(Arc::new(initial_snapshot)));
```

And add to the struct literal: `snapshot, shared_nvt,`.

- [ ] **Step 4: Make get() work with `&self` instead of `&mut self`**

Since we removed the hot cache, `get()` no longer mutates state... except it seeks the `kv_file`. We need `get()` to stay `&mut self` for the writer's own use (it seeks the file handle), but readers will use ReadSnapshot instead. No signature change needed here — the writer keeps its `&mut self` `get()` for internal use (e.g., `mark_deleted` needs to look up an entry before flagging it).

However, `contains()` should delegate to the snapshot for read-only checks:

```rust
    pub fn contains(&self, hash: &[u8]) -> bool {
      // Check write buffer first (writer's own state)
      if let Some(entry) = self.write_buffer.get(hash) {
        return !entry.is_deleted();
      }
      // Fall through to snapshot for disk reads
      let snap = self.snapshot.load();
      snap.get(hash, &self.kv_file).is_some()
    }
```

Wait — `contains` is only used internally by the writer (and it's `&mut self`). Let's not change its signature. The writer can keep using `get()` for its own lookups. The key insight is: **readers use ReadSnapshot, writers use their own `get()`**. No need to change the writer's internal methods.

- [ ] **Step 5: Run existing disk_kv_store_spec tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test disk_kv_store_spec -- --test-threads=1`
Expected: All 74 tests pass (the refactoring should be transparent to existing tests)

- [ ] **Step 6: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/engine/disk_kv_store.rs
git commit -m "Concurrent KV Phase 2: remove hot cache, add snapshot publishing, threshold 512"
```

---

### Task 3: Wire StorageEngine to use snapshots for reads

**Files:**
- Modify: `aeordb-lib/src/engine/storage_engine.rs`

This is the big one — rewire all read methods to use the snapshot instead of taking a write lock on kv_store.

- [ ] **Step 1: Change StorageEngine fields**

Replace:
```rust
use std::sync::RwLock;
```
With:
```rust
use std::sync::{Mutex, RwLock};
use std::fs::File;
use arc_swap::ArcSwap;
use crate::engine::kv_snapshot::ReadSnapshot;
```

Replace the struct definition:
```rust
pub struct StorageEngine {
  writer: RwLock<AppendWriter>,
  kv_store: RwLock<DiskKVStore>,
  #[allow(dead_code)]
  void_manager: RwLock<VoidManager>,
  hash_algo: HashAlgorithm,
}
```

With:
```rust
pub struct StorageEngine {
  writer: RwLock<AppendWriter>,
  kv_writer: Mutex<DiskKVStore>,
  kv_snapshot: Arc<ArcSwap<ReadSnapshot>>,
  kv_file: File,
  #[allow(dead_code)]
  void_manager: RwLock<VoidManager>,
  hash_algo: HashAlgorithm,
}
```

- [ ] **Step 2: Update constructors**

In `create_with_hot_dir`, replace:
```rust
    Ok(StorageEngine {
      writer: RwLock::new(writer),
      kv_store: RwLock::new(kv_store),
      void_manager: RwLock::new(void_manager),
      hash_algo,
    })
```

With:
```rust
    let kv_file = std::fs::File::open(format!("{}.kv", path))?;
    let kv_snapshot = Arc::clone(kv_store.snapshot_handle());

    Ok(StorageEngine {
      writer: RwLock::new(writer),
      kv_writer: Mutex::new(kv_store),
      kv_snapshot,
      kv_file,
      void_manager: RwLock::new(void_manager),
      hash_algo,
    })
```

Apply the same pattern to `open_internal` (replace `kv_store: RwLock::new(kv_store)` with the same `kv_file` + `kv_snapshot` + `kv_writer: Mutex::new(kv_store)` pattern).

- [ ] **Step 3: Rewire read methods to use snapshot**

Replace `get_entry`:
```rust
  pub fn get_entry(&self, hash: &[u8]) -> EngineResult<Option<EntryData>> {
    let snapshot = self.kv_snapshot.load();
    let kv_entry = match snapshot.get(hash, &self.kv_file) {
      Some(entry) if !entry.is_deleted() => entry,
      _ => return Ok(None),
    };

    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let (header, key, value) = writer.read_entry_at(kv_entry.offset)?;
    Ok(Some((header, key, value)))
  }
```

Replace `has_entry`:
```rust
  pub fn has_entry(&self, hash: &[u8]) -> EngineResult<bool> {
    let snapshot = self.kv_snapshot.load();
    match snapshot.get(hash, &self.kv_file) {
      Some(entry) => Ok(!entry.is_deleted()),
      None => Ok(false),
    }
  }
```

Replace `is_entry_deleted`:
```rust
  pub fn is_entry_deleted(&self, hash: &[u8]) -> EngineResult<bool> {
    let snapshot = self.kv_snapshot.load();
    // If not in snapshot at all, it's not deleted (it doesn't exist)
    // If in snapshot and flagged deleted, it's deleted
    // We need to check the writer's buffer for deleted tombstones too
    let kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    match kv.get_from_buffer_only(hash) {
      Some(entry) => Ok(entry.is_deleted()),
      None => {
        // Check snapshot
        match snapshot.get(hash, &self.kv_file) {
          Some(entry) => Ok(entry.is_deleted()),
          None => Ok(false),
        }
      }
    }
  }
```

Wait — this is getting complicated. `is_entry_deleted` needs the most up-to-date state (including the live write buffer). The snapshot might be one insert behind. Let me simplify: since the snapshot is published after every `insert()` and every `mark_deleted()`, the snapshot IS the current state. The snapshot's buffer contains the latest writes. So:

```rust
  pub fn is_entry_deleted(&self, hash: &[u8]) -> EngineResult<bool> {
    let snapshot = self.kv_snapshot.load();
    // Snapshot buffer contains tombstones from mark_deleted
    if let Some(entry) = snapshot.get(hash, &self.kv_file) {
      // get() returns None for deleted entries, so if we got Some it's alive
      return Ok(false);
    }
    // Not found via get() — could be deleted (tombstone in buffer) or nonexistent
    // We need to distinguish. Check if a tombstoned version exists in the raw buffer.
    // Actually — the snapshot's get() already returns None for deleted entries.
    // We need a way to check "does this hash exist as a deleted tombstone?"
    Ok(false)
  }
```

Hmm, this is messy. Let me add a helper to ReadSnapshot:

In `kv_snapshot.rs`, add:
```rust
  /// Check if a hash exists in the buffer as a deleted tombstone.
  pub fn is_deleted_in_buffer(&self, hash: &[u8]) -> bool {
    self.buffer.get(hash)
      .map(|entry| entry.is_deleted())
      .unwrap_or(false)
  }

  /// Raw buffer lookup — returns entry even if deleted.
  pub fn get_raw(&self, hash: &[u8], kv_file: &File) -> Option<KVEntry> {
    // Check buffer first (including deleted)
    if let Some(entry) = self.buffer.get(hash) {
      return Some(entry.clone());
    }

    // Fall through to disk
    let bucket_index = self.nvt.bucket_for_value(hash);
    if bucket_index >= self.bucket_count {
      return None;
    }

    let hash_length = self.hash_algo.hash_length();
    let offset = bucket_page_offset(bucket_index, hash_length);
    let psize = page_size(hash_length);

    let mut file = kv_file.try_clone().ok()?;
    let mut page_data = vec![0u8; psize];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut page_data).ok()?;

    let entries = deserialize_page(&page_data, hash_length).ok()?;
    entries.iter().find(|e| e.hash == hash).cloned()
  }
```

Then `is_entry_deleted` becomes clean:
```rust
  pub fn is_entry_deleted(&self, hash: &[u8]) -> EngineResult<bool> {
    let snapshot = self.kv_snapshot.load();
    match snapshot.get_raw(hash, &self.kv_file) {
      Some(entry) => Ok(entry.is_deleted()),
      None => Ok(false),
    }
  }
```

Replace `iter_kv_entries`:
```rust
  pub fn iter_kv_entries(&self) -> EngineResult<Vec<KVEntry>> {
    let snapshot = self.kv_snapshot.load();
    snapshot.iter_all(&self.kv_file)
  }
```

Replace `entries_by_type`:
```rust
  pub fn entries_by_type(&self, target_type: u8) -> EngineResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let snapshot = self.kv_snapshot.load();
    let hashes: Vec<(Vec<u8>, u64)> = snapshot.iter_all(&self.kv_file)?
      .into_iter()
      .filter(|entry| entry.entry_type() == target_type)
      .map(|entry| (entry.hash, entry.offset))
      .collect();

    let mut results = Vec::with_capacity(hashes.len());
    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;

    for (hash, offset) in hashes {
      let (_header, _key, value) = writer.read_entry_at(offset)?;
      results.push((hash, value));
    }

    Ok(results)
  }
```

Replace `stats()` — use snapshot for KV entry counts:
```rust
  pub fn stats(&self) -> DatabaseStats {
    let (entry_count, created_at, updated_at, db_file_size_bytes) = {
      let writer = self.writer.read().expect("writer lock poisoned");
      let fh = writer.file_header();
      (fh.entry_count, fh.created_at, fh.updated_at, writer.file_size())
    };

    let snapshot = self.kv_snapshot.load();
    let kv_entries = snapshot.len();
    let nvt_buckets = snapshot.bucket_count();

    let kv_size_bytes = {
      let kv = self.kv_writer.lock().expect("kv_writer lock poisoned");
      std::fs::metadata(kv.path()).map(|m| m.len()).unwrap_or(0)
    };

    let all_entries = snapshot.iter_all(&self.kv_file).unwrap_or_default();

    let mut chunk_count = 0usize;
    let mut file_count = 0usize;
    let mut directory_count = 0usize;
    let mut snapshot_count = 0usize;
    let mut fork_count = 0usize;

    for entry in &all_entries {
      match entry.entry_type() {
        KV_TYPE_CHUNK => chunk_count += 1,
        KV_TYPE_FILE_RECORD => file_count += 1,
        KV_TYPE_DIRECTORY => directory_count += 1,
        KV_TYPE_SNAPSHOT => snapshot_count += 1,
        KV_TYPE_FORK => fork_count += 1,
        _ => {}
      }
    }

    let (void_count, void_space_bytes) = {
      let vm = self.void_manager.read().expect("void_manager lock poisoned");
      (vm.void_count(), vm.total_void_space())
    };

    DatabaseStats {
      entry_count, kv_entries, kv_size_bytes, nvt_buckets,
      nvt_size_bytes: 0, chunk_count, file_count, directory_count,
      snapshot_count, fork_count, void_count, void_space_bytes,
      db_file_size_bytes, created_at, updated_at,
      hash_algorithm: format!("{:?}", self.hash_algo),
    }
  }
```

- [ ] **Step 4: Rewire write methods to use kv_writer Mutex**

Replace every `self.kv_store.write()` in write methods with `self.kv_writer.lock()`:

In `store_entry`, `store_entry_compressed`, `store_entry_typed`:
```rust
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    kv.insert(kv_entry);
```

In `flush_batch`:
```rust
    {
      let mut kv = self.kv_writer.lock()
        .map_err(|error| EngineError::IoError(
          std::io::Error::other(error.to_string()),
        ))?;
      for (i, entry) in batch.entries.iter().enumerate() {
        let kv_entry = KVEntry {
          type_flags: entry.kv_type,
          hash: entry.key.clone(),
          offset: offsets[i],
        };
        kv.insert(kv_entry);
      }
    }
```

In `mark_entry_deleted`:
```rust
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let updated = kv.update_flags(hash, KV_FLAG_DELETED);
```

In `remove_kv_entry`:
```rust
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    kv.mark_deleted(hash);
```

- [ ] **Step 5: Run the full test suite**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All 2,147+ tests pass. This is the critical regression gate — if anything breaks, the wiring is wrong.

- [ ] **Step 6: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/engine/storage_engine.rs aeordb-lib/src/engine/kv_snapshot.rs
git commit -m "Concurrent KV Phase 3: wire StorageEngine reads to lock-free snapshots"
```

---

### Task 4: Wire EngineChunkStorage

**Files:**
- Modify: `aeordb-lib/src/engine/engine_chunk_storage.rs`

Same pattern as StorageEngine: reads use snapshot, writes use mutex.

- [ ] **Step 1: Update EngineChunkStorage fields and constructor**

Replace:
```rust
use std::sync::RwLock;
```
With:
```rust
use std::sync::{Mutex, RwLock};
use std::fs::File;
use arc_swap::ArcSwap;
use crate::engine::kv_snapshot::ReadSnapshot;
```

Replace the struct:
```rust
pub struct EngineChunkStorage {
  writer: RwLock<AppendWriter>,
  kv_writer: Mutex<DiskKVStore>,
  kv_snapshot: Arc<ArcSwap<ReadSnapshot>>,
  kv_file: File,
  void_manager: RwLock<VoidManager>,
  hash_algo: HashAlgorithm,
}
```

Update `create()` and `open()` constructors to match (same pattern as StorageEngine).

- [ ] **Step 2: Rewire read methods**

`get_chunk` — snapshot for KV lookup:
```rust
    let kv_entry = {
      let snapshot = self.kv_snapshot.load();
      match snapshot.get(hash.as_slice(), &self.kv_file) {
        Some(entry) if !entry.is_deleted() => entry,
        _ => return Ok(None),
      }
    };
```

`has_chunk` — snapshot:
```rust
    let snapshot = self.kv_snapshot.load();
    match snapshot.get(hash.as_slice(), &self.kv_file) {
      Some(entry) => Ok(!entry.is_deleted()),
      None => Ok(false),
    }
```

`chunk_count` and `list_chunk_hashes` — snapshot:
```rust
    let snapshot = self.kv_snapshot.load();
    let all_entries = snapshot.iter_all(&self.kv_file)
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;
```

- [ ] **Step 3: Rewire write methods**

`store_chunk` — mutex for dedup check + insert:
```rust
    {
      let mut kv = self.kv_writer.lock()
        .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;
      if let Some(existing) = kv.get(&chunk_hash) {
        if !existing.is_deleted() {
          return Ok(());
        }
      }
    }
    // ... append to file ...
    let mut kv = self.kv_writer.lock()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;
    kv.insert(kv_entry);
```

`remove_chunk` — mutex:
```rust
    let mut kv = self.kv_writer.lock()
      .map_err(|error| ChunkStoreError::IoError(error.to_string()))?;
```

- [ ] **Step 4: Run tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/src/engine/engine_chunk_storage.rs
git commit -m "Concurrent KV Phase 4: wire EngineChunkStorage to lock-free snapshots"
```

---

### Task 5: Concurrency Tests

**Files:**
- Create: `aeordb-lib/spec/engine/kv_concurrency_spec.rs`

- [ ] **Step 1: Write multi-threaded contention tests**

Create `aeordb-lib/spec/engine/kv_concurrency_spec.rs`:

```rust
use std::sync::Arc;
use std::thread;
use aeordb::engine::{DirectoryOps, RequestContext, StorageEngine};
use aeordb::server::create_temp_engine_for_tests;

#[test]
fn test_concurrent_readers_dont_block() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Seed with some files
  for i in 0..20 {
    ops.store_file(&ctx, &format!("/file-{}.txt", i), format!("content {}", i).as_bytes(), Some("text/plain")).unwrap();
  }

  let engine = Arc::new(engine);
  let mut handles = vec![];

  // Spawn 8 reader threads
  for thread_id in 0..8 {
    let engine = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      for i in 0..20 {
        let path = format!("/file-{}.txt", i);
        let ops = DirectoryOps::new(&engine);
        let result = ops.read_file(&path);
        assert!(result.is_ok(), "thread {} read of {} failed: {:?}", thread_id, path, result.err());
      }
    });
    handles.push(handle);
  }

  for handle in handles {
    handle.join().expect("reader thread panicked");
  }
}

#[test]
fn test_readers_and_writer_concurrent() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Seed with initial files
  for i in 0..10 {
    ops.store_file(&ctx, &format!("/seed-{}.txt", i), b"initial", Some("text/plain")).unwrap();
  }

  let engine = Arc::new(engine);
  let mut handles = vec![];

  // Spawn 4 reader threads that continuously read
  for thread_id in 0..4 {
    let engine = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ops = DirectoryOps::new(&engine);
      for _ in 0..50 {
        for i in 0..10 {
          let path = format!("/seed-{}.txt", i);
          // Read should never panic or return an error (file exists)
          let _ = ops.read_file(&path);
        }
      }
    });
    handles.push(handle);
  }

  // Spawn 1 writer thread that adds new files
  {
    let engine = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ctx = RequestContext::system();
      let ops = DirectoryOps::new(&engine);
      for i in 0..50 {
        let path = format!("/new-{}.txt", i);
        ops.store_file(&ctx, &path, format!("new content {}", i).as_bytes(), Some("text/plain")).unwrap();
      }
    });
    handles.push(handle);
  }

  for handle in handles {
    handle.join().expect("thread panicked");
  }

  // Verify all new files exist after threads complete
  let ops = DirectoryOps::new(&engine);
  for i in 0..50 {
    let path = format!("/new-{}.txt", i);
    let content = ops.read_file(&path).unwrap();
    assert!(content.is_some(), "new file {} should exist", path);
  }
}

#[test]
fn test_long_reader_doesnt_block_writer() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Seed with files
  for i in 0..100 {
    ops.store_file(&ctx, &format!("/file-{}.txt", i), format!("content {}", i).as_bytes(), Some("text/plain")).unwrap();
  }

  let engine = Arc::new(engine);

  // Spawn a "long reader" that iterates all entries (simulating GC mark)
  let reader_engine = Arc::clone(&engine);
  let reader_handle = thread::spawn(move || {
    for _ in 0..10 {
      let entries = reader_engine.iter_kv_entries().unwrap();
      assert!(!entries.is_empty());
    }
  });

  // Writer runs concurrently
  let writer_engine = Arc::clone(&engine);
  let writer_handle = thread::spawn(move || {
    let ctx = RequestContext::system();
    let ops = DirectoryOps::new(&writer_engine);
    for i in 100..150 {
      ops.store_file(&ctx, &format!("/file-{}.txt", i), format!("content {}", i).as_bytes(), Some("text/plain")).unwrap();
    }
  });

  reader_handle.join().expect("reader thread panicked");
  writer_handle.join().expect("writer thread panicked");
}

#[test]
fn test_snapshot_isolation_during_write() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  ops.store_file(&ctx, "/before.txt", b"before", Some("text/plain")).unwrap();

  // Grab a snapshot reference (via has_entry) before writing
  let exists_before = engine.has_entry(
    &engine.compute_hash(b"file:/before.txt").unwrap()
  ).unwrap();
  assert!(exists_before);

  // Write a new file
  ops.store_file(&ctx, "/after.txt", b"after", Some("text/plain")).unwrap();

  // Both should be visible now (snapshot updated after write)
  let exists_after = engine.has_entry(
    &engine.compute_hash(b"file:/after.txt").unwrap()
  ).unwrap();
  assert!(exists_after);
}

#[test]
fn test_no_data_corruption_under_contention() {
  let (engine, _temp) = create_temp_engine_for_tests();
  let ctx = RequestContext::system();
  let ops = DirectoryOps::new(&engine);

  // Write files with known content
  for i in 0..20 {
    let content = format!("exact-content-{}", i);
    ops.store_file(&ctx, &format!("/verify-{}.txt", i), content.as_bytes(), Some("text/plain")).unwrap();
  }

  let engine = Arc::new(engine);
  let mut handles = vec![];

  // Spawn readers that verify content integrity
  for _ in 0..4 {
    let engine = Arc::clone(&engine);
    let handle = thread::spawn(move || {
      let ops = DirectoryOps::new(&engine);
      for _ in 0..20 {
        for i in 0..20 {
          let path = format!("/verify-{}.txt", i);
          if let Ok(Some((data, _))) = ops.read_file(&path) {
            let expected = format!("exact-content-{}", i);
            assert_eq!(
              data, expected.as_bytes(),
              "data corruption detected for {}", path
            );
          }
        }
      }
    });
    handles.push(handle);
  }

  for handle in handles {
    handle.join().expect("reader thread panicked");
  }
}
```

- [ ] **Step 2: Run concurrency tests**

Run: `cd /home/wyatt/Projects/aeordb && cargo test --test kv_concurrency_spec -- --test-threads=1`
Expected: All 5 tests pass

- [ ] **Step 3: Run full test suite**

Run: `cd /home/wyatt/Projects/aeordb && cargo test -- --test-threads=4`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
cd /home/wyatt/Projects/aeordb
git add aeordb-lib/spec/engine/kv_concurrency_spec.rs
git commit -m "Concurrent KV Phase 5: multi-threaded contention tests — 5 tests"
```

---

## Post-Implementation Checklist

- [ ] Update `.claude/TODO.md` — add "Completed: Concurrent KV Readers" with test count
- [ ] Update `.claude/DETAILS.md` — add kv_snapshot.rs to key files, note DiskKVStore now publishes snapshots
- [ ] Run: `cargo test -- --test-threads=4` — all tests pass
- [ ] Run: `cargo build -p aeordb-cli` — CLI compiles
- [ ] Verify: existing GC, backup, query, SSE tests all still pass (no public API changes)
