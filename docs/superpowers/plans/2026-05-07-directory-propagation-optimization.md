# Directory Propagation Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce per-file write/delete latency from ~450ms to <100ms by batching WAL writes, caching directory content, and eliminating duplicate directory storage.

**Architecture:** Three optimizations to `update_parent_directories` and `remove_from_parent_directory`: (1) collect all directory writes into a `WriteBatch` flushed once at the end, (2) cache directory content by content hash to avoid redundant WAL reads, (3) store a 32-byte hard link at path-based keys instead of duplicating full directory data.

**Tech Stack:** Rust, existing `WriteBatch` API, `RwLock<HashMap>` for cache

**Spec:** `docs/superpowers/specs/2026-05-07-directory-propagation-optimization-design.md`

---

### Task 1: Add `flush_batch_and_update_head` to StorageEngine

**Files:**
- Modify: `aeordb-lib/src/engine/storage_engine.rs`

- [ ] **Step 1: Add the method**

In `aeordb-lib/src/engine/storage_engine.rs`, add this method to `impl StorageEngine` after the existing `flush_batch` method (around line 831):

```rust
  /// Flush a write batch AND update HEAD atomically in a single lock hold.
  /// This avoids separate lock acquisitions for the batch and the head update.
  pub fn flush_batch_and_update_head(&self, batch: WriteBatch, head_hash: &[u8]) -> EngineResult<Vec<u64>> {
    if batch.is_empty() {
      // Still update HEAD even if batch is empty (e.g., system path that skips propagation)
      return self.update_head(head_hash).map(|_| Vec::new());
    }

    let mut writer = self.writer.write()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;
    let mut kv = self.kv_writer.lock()
      .map_err(|error| EngineError::IoError(
        std::io::Error::other(error.to_string()),
      ))?;

    let mut offsets = Vec::with_capacity(batch.entries.len());

    for entry in &batch.entries {
      let offset = writer.append_entry(
        entry.entry_type,
        &entry.key,
        &entry.value,
        0, // flags
      )?;
      kv.set_hot_tail_offset(writer.current_offset());
      offsets.push(offset);
    }

    for (i, entry) in batch.entries.iter().enumerate() {
      let kv_entry = KVEntry {
        type_flags: entry.kv_type,
        hash: entry.key.clone(),
        offset: offsets[i],
      };
      kv.insert(kv_entry)?;
    }

    // Update HEAD in the same lock hold
    let mut header = writer.file_header().clone();
    header.head_hash = head_hash.to_vec();
    writer.update_file_header(&header)?;

    self.counters.load().set_write_buffer_depth(kv.write_buffer_len() as u64);

    Ok(offsets)
  }
```

Note: `batch.entries` is a private field. You'll need to either make it `pub(crate)` or add a method to iterate. Read the existing `flush_batch` method — it accesses `batch.entries` directly, so it's already accessible within the crate. Check if the field visibility allows this. If not, add `pub(crate)` to the `entries` field in `WriteBatch`.

- [ ] **Step 2: Build and verify**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -5`
Expected: No errors.

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/engine/storage_engine.rs
git commit -m "feat: flush_batch_and_update_head for atomic batch + HEAD update"
```

---

### Task 2: Add directory content cache to StorageEngine

**Files:**
- Modify: `aeordb-lib/src/engine/storage_engine.rs`

- [ ] **Step 1: Add the cache field and methods**

In `aeordb-lib/src/engine/storage_engine.rs`:

Add import at the top if not already present:
```rust
use std::collections::HashMap;
```

Add a field to `pub struct StorageEngine` (before `_file_lock`):
```rust
  /// Cache of directory content keyed by content hash. Content-addressed data
  /// is immutable, so this cache can never serve stale data for a given key.
  /// Populated by update_parent_directories, read by directory lookups.
  pub(crate) dir_content_cache: RwLock<HashMap<Vec<u8>, Vec<u8>>>,
```

Initialize in both constructors (`create_with_hot_dir` and `open_with_hot_dir`), add to the `StorageEngine { ... }` struct literal:
```rust
      dir_content_cache: RwLock::new(HashMap::new()),
```

Add convenience methods to `impl StorageEngine`:
```rust
  /// Get directory content from cache by content hash.
  pub(crate) fn get_cached_dir_content(&self, content_key: &[u8]) -> Option<Vec<u8>> {
    self.dir_content_cache.read().ok()?.get(content_key).cloned()
  }

  /// Cache directory content by content hash.
  pub(crate) fn cache_dir_content(&self, content_key: Vec<u8>, value: Vec<u8>) {
    if let Ok(mut cache) = self.dir_content_cache.write() {
      cache.insert(content_key, value);
    }
  }

  /// Clear the directory content cache (called on snapshot restore).
  pub fn clear_dir_content_cache(&self) {
    if let Ok(mut cache) = self.dir_content_cache.write() {
      cache.clear();
    }
  }
```

- [ ] **Step 2: Call clear_dir_content_cache on snapshot restore**

In `aeordb-lib/src/server/engine_routes.rs`, find the `snapshot_restore` handler. After the existing cache evictions (permissions, index_config, group, api_key), add:

```rust
state.engine.clear_dir_content_cache();
```

- [ ] **Step 3: Build and verify**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -5`
Expected: No errors.

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/storage_engine.rs aeordb-lib/src/server/engine_routes.rs
git commit -m "feat: directory content cache on StorageEngine with snapshot restore eviction"
```

---

### Task 3: Add `read_directory_data` helper that follows hard links + checks cache

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`

- [ ] **Step 1: Add the helper method**

In `aeordb-lib/src/engine/directory_ops.rs`, add this method to `impl<'a> DirectoryOps<'a>` (before `update_parent_directories`):

```rust
  /// Read directory data by path key, following hard links and checking the
  /// content cache. Returns the entry header and directory value bytes.
  ///
  /// Hard link detection: if the value at dir_key is exactly hash_length bytes,
  /// it's a hard link (content hash pointer). Follow it to get the actual data.
  /// Backward compatible: values >hash_length are inline data (pre-optimization).
  pub(crate) fn read_directory_data(&self, dir_key: &[u8]) -> EngineResult<Option<(crate::engine::entry_header::EntryHeader, Vec<u8>)>> {
    let hash_length = self.engine.hash_algo().hash_length();

    let entry = match self.engine.get_entry(dir_key)? {
      Some(entry) => entry,
      None => return Ok(None),
    };

    let (header, _key, value) = entry;

    // Check if this is a hard link (value == hash_length bytes)
    if value.len() == hash_length {
      let content_key = &value;

      // Check cache first
      if let Some(cached) = self.engine.get_cached_dir_content(content_key) {
        return Ok(Some((header, cached)));
      }

      // Cache miss — read from WAL
      match self.engine.get_entry(content_key)? {
        Some((_h, _k, content_value)) => {
          // Cache for future reads
          self.engine.cache_dir_content(content_key.to_vec(), content_value.clone());
          Ok(Some((header, content_value)))
        }
        None => {
          tracing::warn!("Hard link target not found for directory entry");
          Ok(None)
        }
      }
    } else {
      // Inline data (backward compatible or empty directory)
      Ok(Some((header, value)))
    }
  }
```

- [ ] **Step 2: Build and verify**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -5`
Expected: No errors (method is defined but not yet called).

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/engine/directory_ops.rs
git commit -m "feat: read_directory_data helper with hard link + cache support"
```

---

### Task 4: Rewrite `update_parent_directories` with batch + cache + hard links

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`

This is the core change. Read the existing `update_parent_directories` method (lines 1581-1690) fully before modifying.

- [ ] **Step 1: Rewrite the method**

Replace the entire `update_parent_directories` method with the optimized version. Key changes:

1. Create a `WriteBatch` at the start
2. Each level reads directories via `read_directory_data` (follows hard links, checks cache)
3. For flat format: compute content, hash it, add content entry to batch, add hard link to batch, cache the content
4. For B-tree format: B-tree node storage stays synchronous (btree_insert_batched stores nodes directly), but the root node content entry and hard link go into the batch
5. At root level: call `flush_batch_and_update_head` instead of `store_entry` + `update_head`
6. At non-root levels that exit early (system path check): flush whatever is in the batch

```rust
  fn update_parent_directories(
    &self,
    child_path: &str,
    child_entry: ChildEntry,
  ) -> EngineResult<()> {
    let algo = self.engine.hash_algo();
    let hash_length = algo.hash_length();

    let mut current_child_path = child_path.to_string();
    let mut current_child_entry = child_entry;
    let mut batch = crate::engine::storage_engine::WriteBatch::new();
    let mut last_content_key: Option<Vec<u8>> = None;

    for _depth in 0..Self::MAX_DIRECTORY_DEPTH {
      let parent = match parent_path(&current_child_path) {
        Some(parent) => parent,
        None => {
          // root has no parent — flush any pending batch
          if !batch.is_empty() {
            self.engine.flush_batch(batch)?;
          }
          return Ok(());
        }
      };

      // Don't propagate system paths to root
      if parent == "/" && is_system_path(&current_child_path) {
        if !batch.is_empty() {
          self.engine.flush_batch(batch)?;
        }
        return Ok(());
      }

      let dir_key = directory_path_hash(&parent, &algo)?;

      // Read existing directory (follows hard links, checks cache)
      let existing = self.read_directory_data(&dir_key)?;

      let (dir_value, content_key) = match existing {
        Some((_header, value)) if !value.is_empty() && crate::engine::btree::is_btree_format(&value) => {
          // === B-TREE FORMAT ===
          // B-tree node storage stays synchronous (btree_insert_batched stores nodes directly)
          let (new_root_hash, new_root_data) = crate::engine::btree::btree_insert_batched(
            self.engine, &value, current_child_entry, hash_length, &algo
          )?;
          (new_root_data, new_root_hash)
        }
        Some((header, value)) => {
          // === FLAT FORMAT ===
          let mut children = if value.is_empty() {
            Vec::new()
          } else {
            deserialize_child_entries(&value, hash_length, header.entry_version)?
          };

          let child_name = &current_child_entry.name;
          if let Some(existing) = children.iter_mut().find(|c| c.name == *child_name) {
            *existing = current_child_entry;
          } else {
            children.push(current_child_entry);
          }

          if children.len() >= crate::engine::btree::BTREE_CONVERSION_THRESHOLD {
            // Convert flat -> B-tree (stores nodes synchronously)
            let root_hash = crate::engine::btree::btree_from_entries(
              self.engine, children, hash_length, &algo
            )?;
            let root_entry = self.engine.get_entry(&root_hash)?
              .ok_or_else(|| EngineError::NotFound("B-tree root not found after conversion".to_string()))?;
            (root_entry.2, root_hash)
          } else {
            let dir_value = serialize_child_entries(&children, hash_length)?;
            let content_key = directory_content_hash(&dir_value, &algo)?;
            // Add content entry to batch (not stored immediately)
            batch.add(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone());
            // Cache the content
            self.engine.cache_dir_content(content_key.clone(), dir_value.clone());
            (dir_value, content_key)
          }
        }
        None => {
          // New directory
          self.engine.counters().increment_directories();
          let children = vec![current_child_entry];
          let dir_value = serialize_child_entries(&children, hash_length)?;
          let content_key = directory_content_hash(&dir_value, &algo)?;
          batch.add(EntryType::DirectoryIndex, content_key.clone(), dir_value.clone());
          self.engine.cache_dir_content(content_key.clone(), dir_value.clone());
          (dir_value, content_key)
        }
      };

      // Add hard link at path-based key (32-byte content hash, not full data)
      batch.add(EntryType::DirectoryIndex, dir_key, content_key.clone());

      // If root, flush batch + update HEAD atomically
      if parent == "/" {
        self.engine.flush_batch_and_update_head(batch, &content_key)?;
        return Ok(());
      }

      // Set up next iteration
      let now_ms = chrono::Utc::now().timestamp_millis();
      last_content_key = Some(content_key.clone());
      current_child_entry = ChildEntry {
        entry_type: EntryType::DirectoryIndex.to_u8(),
        hash: content_key,
        total_size: dir_value.len() as u64,
        created_at: now_ms,
        updated_at: now_ms,
        name: file_name(&parent).unwrap_or("").to_string(),
        content_type: None,
        virtual_time: now_ms as u64,
        node_id: 0,
      };
      current_child_path = parent;
    }

    Err(EngineError::InvalidInput(
      format!("Directory depth exceeds maximum of {} levels", Self::MAX_DIRECTORY_DEPTH),
    ))
  }
```

**IMPORTANT:** For B-tree format, the content entry and hard link are NOT added to the batch via `batch.add()` because `btree_insert_batched` already stored the root node. We still need to store the hard link at `dir_key`. But the B-tree root data is already at `new_root_hash` in the KV. So for B-tree, only add the hard link to the batch:

After the B-tree match arm produces `(new_root_data, new_root_hash)`, the hard link is added outside the match (which already happens — `batch.add(EntryType::DirectoryIndex, dir_key, content_key.clone())`). But we should also cache the B-tree root data:

```rust
// After B-tree match arm:
self.engine.cache_dir_content(new_root_hash.clone(), new_root_data.clone());
```

Add this line inside the B-tree match arm, after `(new_root_data, new_root_hash)` is computed.

- [ ] **Step 2: Build and verify**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -10`
Expected: No errors. If there are errors about private fields, fix visibility.

- [ ] **Step 3: Run tests**

Run: `cargo test -p aeordb --lib 2>&1 | tail -5`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/directory_ops.rs
git commit -m "perf: batch + cache + hard links in update_parent_directories"
```

---

### Task 5: Update `remove_from_parent_directory` with same optimizations

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`

Read the existing `remove_from_parent_directory` method (around line 1695) fully before modifying.

- [ ] **Step 1: Rewrite the method**

Apply the same three optimizations as Task 4:
1. Use `read_directory_data` to read the directory (follows hard links, checks cache)
2. Add content entry + hard link to a `WriteBatch` instead of calling `store_entry` directly
3. At root: call `flush_batch_and_update_head`
4. For the upward propagation (calls `update_parent_directories` at line 1775), pass the already-batched changes

Actually, `remove_from_parent_directory` only modifies ONE level (the immediate parent), then calls `update_parent_directories` to propagate up. So the simplest fix:
1. Use `read_directory_data` for the initial read
2. Store content entry + hard link (can still use `store_entry` for just this one level since it's only 2 writes)
3. The propagation via `update_parent_directories` already uses the batch

Alternatively, batch the immediate parent's writes too. The cleanest approach: make `remove_from_parent_directory` use the same batch pattern.

Replace the method. Key changes from the original:
- `self.engine.get_entry(&dir_key)?` → `self.read_directory_data(&dir_key)?`
- `self.engine.store_entry(...)` for content_key → `batch.add(...)` + `cache_dir_content`
- `self.engine.store_entry(...)` for dir_key → `batch.add(...)` (hard link)
- `self.engine.update_head(...)` → `self.engine.flush_batch_and_update_head(batch, ...)`
- For non-root: flush batch before calling `update_parent_directories`

- [ ] **Step 2: Build and test**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -5`
Run: `cargo test -p aeordb --lib 2>&1 | tail -5`
Expected: No errors, all tests pass.

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/engine/directory_ops.rs
git commit -m "perf: batch + cache + hard links in remove_from_parent_directory"
```

---

### Task 6: Update `list_directory` and other read paths to follow hard links

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`

- [ ] **Step 1: Update `list_directory`**

In `list_directory` (around line 853), replace the `self.engine.get_entry(&dir_key)?` call with `self.read_directory_data(&dir_key)?`.

The existing code:
```rust
    match self.engine.get_entry(&dir_key) {
      Ok(Some((header, _key, value))) => {
```

Change to:
```rust
    match self.read_directory_data(&dir_key) {
      Ok(Some((header, value))) => {
```

Note the tuple change: `read_directory_data` returns `(header, value)` not `(header, key, value)`.

- [ ] **Step 2: Find and update all other directory read paths**

Search for other places in `directory_ops.rs` that read directories by `dir_key`:

```bash
grep -n "get_entry(&dir_key)" aeordb-lib/src/engine/directory_ops.rs
```

Each hit that reads directory content needs to switch to `read_directory_data`. Common locations:
- `delete_directory` method
- `get_directory_child` if it exists
- Any method that reads `dir_key` and interprets the value as directory data

For each one: replace `self.engine.get_entry(&dir_key)?` with `self.read_directory_data(&dir_key)?` and adjust the tuple destructuring from `(header, _key, value)` to `(header, value)`.

**Do NOT change** `get_entry` calls that read files (file_key), chunks, or other non-directory entries. Only change directory reads by path hash.

- [ ] **Step 3: Build and test**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -10`
Run: `cargo test -p aeordb --lib 2>&1 | tail -5`
Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/directory_ops.rs
git commit -m "refactor: directory read paths follow hard links via read_directory_data"
```

---

### Task 7: Add hard link awareness to GC (defensive)

**Files:**
- Modify: `aeordb-lib/src/engine/gc.rs`

- [ ] **Step 1: Update `walk_directory_tree`**

In `aeordb-lib/src/engine/gc.rs`, find `walk_directory_tree` (around line 97). After the entry is read (around line 119-124), add hard link following:

After this block:
```rust
  let (header, _key, value) = entry;
```

Add:
```rust
  // Follow hard links: if value is exactly hash_length bytes, it's a pointer to content
  let value = if value.len() == hash_length {
    live.insert(value.clone()); // Mark the content hash as live too
    match engine.get_entry_including_deleted(&value)? {
      Some((_h, _k, v)) => v,
      None => return Ok(()),
    }
  } else {
    value
  };
```

This ensures that even if GC somehow reads a directory via path hash (currently it doesn't, but defensively), it follows the hard link and marks both entries as live.

- [ ] **Step 2: Build and test**

Run: `cargo build -p aeordb --lib 2>&1 | grep "^error" | head -5`
Run: `cargo test -p aeordb --lib 2>&1 | tail -5`
Expected: All pass.

- [ ] **Step 3: Commit**

```bash
git add aeordb-lib/src/engine/gc.rs
git commit -m "fix: GC follows hard links in directory entries (defensive)"
```

---

### Task 8: End-to-end verification

**Files:** None (verification only)

- [ ] **Step 1: Build release binary**

Run: `cargo build --release 2>&1 | tail -5`
Expected: Compiles cleanly.

- [ ] **Step 2: Start server and time operations**

```bash
./target/release/aeordb start -D "/path/to/test.aeordb" --port 6830
```

Get a token and time file operations:

```bash
TOKEN=$(curl -s -X POST http://localhost:6830/auth/token \
  -H "Content-Type: application/json" \
  -d '{"api_key":"YOUR_KEY"}' | python3 -c "import sys,json; print(json.load(sys.stdin)['token'])")

# Time a file write
START=$(date +%s%N)
curl -s -X PUT -H "Authorization: Bearer $TOKEN" -H "Content-Type: text/plain" \
  --data-binary "test" http://localhost:6830/files/Pictures/Family/perf-test.txt > /dev/null
END=$(date +%s%N)
echo "Write: $(( (END - START) / 1000000 ))ms"

# Time a file delete
START=$(date +%s%N)
curl -s -X DELETE -H "Authorization: Bearer $TOKEN" \
  http://localhost:6830/files/Pictures/Family/perf-test.txt > /dev/null
END=$(date +%s%N)
echo "Delete: $(( (END - START) / 1000000 ))ms"
```

Expected: Significant improvement from baseline ~450ms.

- [ ] **Step 3: Verify directory listing still works**

```bash
curl -s -H "Authorization: Bearer $TOKEN" http://localhost:6830/files/Pictures/ | python3 -c "
import sys,json; d=json.load(sys.stdin); print(f'Entries: {len(d.get(\"entries\",[]))}')"
```

Expected: Correct entry count, no errors.

- [ ] **Step 4: Verify backward compatibility**

Existing directories (stored with inline data before the optimization) should still be readable. The length check in `read_directory_data` handles this — values >32 bytes are interpreted as inline data.

- [ ] **Step 5: Verify GC**

```bash
curl -s -X POST -H "Authorization: Bearer $TOKEN" http://localhost:6830/system/gc?dry_run=true
```

Expected: GC completes without errors, no live data marked as garbage.

- [ ] **Step 6: Final commit**

```bash
git add -A
git commit -m "perf: directory propagation optimization — verified working"
```
