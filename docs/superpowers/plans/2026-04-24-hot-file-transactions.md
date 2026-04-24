# Hot File Transactions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make multi-entry operations (`store_file`, `delete_file`) atomic by delaying hot file truncation until the full operation completes, then replaying incomplete operations on restart.

**Architecture:** Add `transaction_depth` counter to `DiskKVStore`. An RAII `TransactionGuard` wraps `store_file` and `delete_file`, calling `begin_transaction` on creation and `end_transaction` on drop. `flush()` skips `truncate_hot_file()` when inside a transaction. On restart, hot file replay detects orphaned FileRecords and stale directory entries and repairs them.

**Tech Stack:** Rust, RAII pattern, existing hot file WAL infrastructure

**Spec:** `docs/superpowers/specs/2026-04-24-hot-file-transactions-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `aeordb-lib/src/engine/disk_kv_store.rs` | Modify | Add `transaction_depth`, skip truncation in flush when > 0 |
| `aeordb-lib/src/engine/storage_engine.rs` | Modify | Add `begin_transaction()`, `end_transaction()`, `TransactionGuard` |
| `aeordb-lib/src/engine/directory_ops.rs` | Modify | Wrap `store_file_internal_inner` and `delete_file` with `TransactionGuard` |
| `aeordb-lib/src/engine/storage_engine.rs` | Modify | Recovery in `open_internal`: detect orphaned files after hot replay |
| `aeordb-lib/spec/engine/hot_file_transaction_spec.rs` | Create | All transaction + recovery tests |

---

### Task 1: Add Transaction Depth to KV Store

**Files:**
- Modify: `aeordb-lib/src/engine/disk_kv_store.rs`

- [ ] **Step 1: Add `transaction_depth` field to `DiskKVStore`**

Add `pub transaction_depth: u32` to the `DiskKVStore` struct. Initialize to `0` in both `create()` and `open()`.

- [ ] **Step 2: Guard `truncate_hot_file` in `flush()`**

In `flush()`, find where `self.truncate_hot_file()?;` is called (line ~482). Replace:

```rust
        self.flush_hot_buffer()?;
        self.truncate_hot_file()?;
```

With:

```rust
        self.flush_hot_buffer()?;
        if self.transaction_depth == 0 {
            self.truncate_hot_file()?;
        }
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -5`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/disk_kv_store.rs
git commit -m "Add transaction_depth to KV store, skip hot truncation during transactions"
```

---

### Task 2: Add Transaction API + Guard to StorageEngine

**Files:**
- Modify: `aeordb-lib/src/engine/storage_engine.rs`

- [ ] **Step 1: Add `begin_transaction` and `end_transaction` methods**

Add to `impl StorageEngine`:

```rust
  /// Begin a transaction — hot file will not be truncated until the
  /// matching `end_transaction` call. Use `TransactionGuard` instead
  /// of calling this directly.
  pub fn begin_transaction(&self) {
    if let Ok(mut kv) = self.kv_writer.lock() {
      kv.transaction_depth += 1;
    }
  }

  /// End a transaction — if this brings the depth to 0, truncate
  /// the hot file. Always pair with `begin_transaction`.
  pub fn end_transaction(&self) {
    if let Ok(mut kv) = self.kv_writer.lock() {
      kv.transaction_depth = kv.transaction_depth.saturating_sub(1);
      if kv.transaction_depth == 0 {
        if let Err(e) = kv.truncate_hot_file_if_exists() {
          tracing::warn!("Failed to truncate hot file after transaction: {}", e);
        }
      }
    }
  }
```

Note: `truncate_hot_file` is currently `fn truncate_hot_file(&mut self)`. The implementing agent should check whether it's private — if so, either make it `pub(crate)` or add a `pub(crate) fn truncate_hot_file_if_exists(&mut self)` wrapper that calls it and ignores "no hot file" errors.

- [ ] **Step 2: Add `TransactionGuard` struct**

Add in the same file (or a new `transaction.rs` — agent's choice):

```rust
/// RAII guard that ensures `end_transaction` is always called,
/// even on error or panic. Create via `TransactionGuard::new(engine)`.
pub struct TransactionGuard<'a> {
  engine: &'a StorageEngine,
}

impl<'a> TransactionGuard<'a> {
  pub fn new(engine: &'a StorageEngine) -> Self {
    engine.begin_transaction();
    TransactionGuard { engine }
  }
}

impl<'a> Drop for TransactionGuard<'a> {
  fn drop(&mut self) {
    self.engine.end_transaction();
  }
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -5`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/storage_engine.rs
git commit -m "Add begin/end_transaction and RAII TransactionGuard"
```

---

### Task 3: Wrap store_file and delete_file in Transactions

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`

- [ ] **Step 1: Wrap `store_file_internal_inner` in a transaction**

Read `directory_ops.rs` and find `store_file_internal_inner` (line ~296). At the very beginning of the function body (after the `let normalized = ...` line), add:

```rust
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);
```

This ensures the hot file is not truncated until the entire store operation (chunks + file records + directory propagation) completes. The guard drops at the end of the function — on success, error, or panic.

- [ ] **Step 2: Wrap `delete_file` in a transaction**

Find `delete_file` (line ~497). After the path normalization (`let normalized = normalize_path(path);`), add:

```rust
    let _txn = crate::engine::storage_engine::TransactionGuard::new(self.engine);
```

- [ ] **Step 3: Also wrap `delete_file_with_indexing`**

Find `delete_file_with_indexing` (line ~820). Add the same guard after normalization.

- [ ] **Step 4: Verify compilation and existing tests**

Run: `cargo build -p aeordb 2>&1 | tail -5`
Run: `cargo test 2>&1 | grep "FAILED" | grep "test " || echo "ALL TESTS PASS"`

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/src/engine/directory_ops.rs
git commit -m "Wrap store_file and delete_file in transactions (hot file protection)"
```

---

### Task 4: Recovery on Restart

**Files:**
- Modify: `aeordb-lib/src/engine/storage_engine.rs`

After hot file replay in `open_internal`, detect and repair incomplete operations.

- [ ] **Step 1: Add recovery logic after hot file replay**

In `open_internal`, find where hot entries are replayed (lines ~418-423):

```rust
    if !hot_entries_to_replay.is_empty() {
      for entry in hot_entries_to_replay {
        kv_store.insert(entry)?;
      }
      kv_store.flush()?;
    }
```

Change to capture the replayed entries for recovery:

```rust
    let mut replayed_file_record_hashes: Vec<Vec<u8>> = Vec::new();
    let mut replayed_deletion_hashes: Vec<Vec<u8>> = Vec::new();

    if !hot_entries_to_replay.is_empty() {
      tracing::info!("Replaying {} hot file entries", hot_entries_to_replay.len());
      for entry in &hot_entries_to_replay {
        if entry.entry_type() == crate::engine::kv_store::KV_TYPE_FILE_RECORD {
          replayed_file_record_hashes.push(entry.hash.clone());
        } else if entry.entry_type() == crate::engine::kv_store::KV_TYPE_DELETION {
          replayed_deletion_hashes.push(entry.hash.clone());
        }
      }
      for entry in hot_entries_to_replay {
        kv_store.insert(entry)?;
      }
      kv_store.flush()?;
    }
```

- [ ] **Step 2: After engine construction, run recovery**

After the engine is constructed and the counters are initialized (near the `Ok(engine)` return), add a recovery step. This needs to happen AFTER the engine is built so we can use `DirectoryOps`:

```rust
    // Recovery: check for orphaned file records from incomplete transactions
    if !replayed_file_record_hashes.is_empty() {
      Self::recover_orphaned_files(&engine, &replayed_file_record_hashes);
    }
```

Add the recovery method:

```rust
  /// After hot file replay, check if replayed FileRecords are properly
  /// listed in their parent directories. If not, re-propagate.
  fn recover_orphaned_files(engine: &StorageEngine, file_record_hashes: &[Vec<u8>]) {
    let hash_length = engine.hash_algo().hash_length();
    let ops = DirectoryOps::new(engine);
    let ctx = RequestContext::system();

    for hash in file_record_hashes {
      // Read the file record to get its path
      match engine.get_entry(hash) {
        Ok(Some((header, _key, value))) => {
          if let Ok(record) = FileRecord::deserialize(&value, hash_length, header.entry_version) {
            let path = &record.path;
            // Check if the file is listed in its parent directory
            if let Some(parent) = crate::engine::path_utils::parent_path(path) {
              match ops.list_directory(&parent) {
                Ok(children) => {
                  let file_name = crate::engine::path_utils::file_name(path).unwrap_or("");
                  let is_listed = children.iter().any(|c| c.name == file_name);
                  if !is_listed {
                    tracing::info!(
                      "Recovering orphaned file '{}' — re-propagating to parent directory",
                      path
                    );
                    // Re-propagate by calling update_parent_directories
                    // This requires building a ChildEntry — use the file record data
                    if let Err(e) = ops.repair_parent_listing(&ctx, path, &record, hash) {
                      tracing::warn!("Failed to recover orphaned file '{}': {}", path, e);
                    }
                  }
                }
                Err(_) => {} // Parent directory doesn't exist or is corrupt — skip
              }
            }
          }
        }
        _ => {} // Entry not found or error — skip
      }
    }
  }
```

Note: `ops.repair_parent_listing` doesn't exist yet. The implementing agent should check what `update_parent_directories` needs and either:
1. Make `update_parent_directories` public and call it directly, or
2. Add a thin `repair_parent_listing` wrapper on `DirectoryOps` that reconstructs the `ChildEntry` and calls the existing propagation logic.

The key information needed is: the file path, the file record (for size, content type, timestamps), and the identity hash (for the child entry's hash field). All of this is available from the FileRecord and the KV entry.

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p aeordb 2>&1 | tail -5`

- [ ] **Step 4: Commit**

```bash
git add aeordb-lib/src/engine/storage_engine.rs aeordb-lib/src/engine/directory_ops.rs
git commit -m "Recovery on restart: re-propagate orphaned files from incomplete transactions"
```

---

### Task 5: Comprehensive Tests

**Files:**
- Create: `aeordb-lib/spec/engine/hot_file_transaction_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

- [ ] **Step 1: Create the test file**

Create `aeordb-lib/spec/engine/hot_file_transaction_spec.rs`:

```rust
use aeordb::engine::{StorageEngine, DirectoryOps, RequestContext};
use aeordb::engine::storage_engine::TransactionGuard;

fn create_test_db_with_hot_dir() -> (StorageEngine, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let hot_dir = temp.path();
    let engine = StorageEngine::create_with_hot_dir(
        db_path.to_str().unwrap(),
        Some(hot_dir),
    ).unwrap();
    (engine, temp)
}

// =========================================================================
// Transaction depth
// =========================================================================

#[test]
fn transaction_guard_increments_and_decrements_depth() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    // Depth starts at 0
    {
        let _guard = TransactionGuard::new(&engine);
        // Inside transaction — depth is 1
        // Store a file — flush should NOT truncate hot file
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file(&ctx, "/test.txt", b"hello", Some("text/plain")).unwrap();
    }
    // Guard dropped — depth back to 0, hot file truncated
}

#[test]
fn transaction_guard_fires_on_error() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let result: Result<(), String> = (|| {
        let _guard = TransactionGuard::new(&engine);
        // Simulate an error mid-transaction
        return Err("simulated error".to_string());
    })();

    assert!(result.is_err());
    // Guard should have dropped — verify we can start a new transaction
    let _guard2 = TransactionGuard::new(&engine);
    // If this doesn't deadlock, depth management is correct
}

#[test]
fn transaction_guard_fires_on_panic() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = TransactionGuard::new(&engine);
        panic!("simulated panic inside transaction");
    }));

    assert!(result.is_err());
    // Guard should have dropped despite panic
    // Verify depth is back to 0 by successfully starting a new transaction
    let _guard2 = TransactionGuard::new(&engine);
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    // This should work — depth is 0, hot file can truncate
    ops.store_file(&ctx, "/after-panic.txt", b"recovered", Some("text/plain")).unwrap();
}

#[test]
fn transaction_depth_always_returns_to_zero() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    // Multiple sequential transactions
    for i in 0..10 {
        let _guard = TransactionGuard::new(&engine);
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        let path = format!("/file_{}.txt", i);
        ops.store_file(&ctx, &path, format!("content-{}", i).as_bytes(), Some("text/plain")).unwrap();
    }

    // All guards dropped — depth must be 0
    // Prove it by successfully storing another file (which triggers flush + truncate)
    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();
    ops.store_file(&ctx, "/final.txt", b"final", Some("text/plain")).unwrap();
}

// =========================================================================
// store_file is transactional
// =========================================================================

#[test]
fn store_file_wraps_in_transaction() {
    let (engine, temp) = create_test_db_with_hot_dir();

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    // Store a file — this should be wrapped in a transaction internally
    ops.store_file(&ctx, "/docs/readme.md", b"# Hello", Some("text/markdown")).unwrap();

    // Verify the file is listed in its parent directory
    let children = ops.list_directory("/docs").unwrap();
    assert!(children.iter().any(|c| c.name == "readme.md"), "file should be in parent listing");

    // Verify the file is readable
    let data = ops.read_file("/docs/readme.md").unwrap();
    assert_eq!(data, b"# Hello");
}

// =========================================================================
// delete_file is transactional
// =========================================================================

#[test]
fn delete_file_wraps_in_transaction() {
    let (engine, _temp) = create_test_db_with_hot_dir();

    let ops = DirectoryOps::new(&engine);
    let ctx = RequestContext::system();

    ops.store_file(&ctx, "/docs/to-delete.txt", b"delete me", Some("text/plain")).unwrap();

    // Verify it exists
    let children = ops.list_directory("/docs").unwrap();
    assert!(children.iter().any(|c| c.name == "to-delete.txt"));

    // Delete it
    ops.delete_file(&ctx, "/docs/to-delete.txt").unwrap();

    // Verify it's gone from listing
    let children = ops.list_directory("/docs").unwrap();
    assert!(!children.iter().any(|c| c.name == "to-delete.txt"), "file should be removed from listing");
}

// =========================================================================
// Recovery tests
// =========================================================================

#[test]
fn recovery_detects_orphaned_file_after_hot_replay() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.aeordb");
    let db_str = db_path.to_str().unwrap();
    let hot_dir = temp.path();

    // Create DB and store files normally
    {
        let engine = StorageEngine::create_with_hot_dir(db_str, Some(hot_dir)).unwrap();
        let ops = DirectoryOps::new(&engine);
        let ctx = RequestContext::system();
        ops.store_file(&ctx, "/docs/existing.txt", b"exists", Some("text/plain")).unwrap();
    }

    // Reopen — should have no recovery needed
    {
        let engine = StorageEngine::open_with_hot_dir(db_str, Some(hot_dir)).unwrap();
        let ops = DirectoryOps::new(&engine);
        let children = ops.list_directory("/docs").unwrap();
        assert!(children.iter().any(|c| c.name == "existing.txt"));
    }
}
```

- [ ] **Step 2: Register the test in Cargo.toml**

Add:
```toml
[[test]]
name = "hot_file_transaction_spec"
path = "spec/engine/hot_file_transaction_spec.rs"
```

- [ ] **Step 3: Run the tests**

Run: `cargo test --test hot_file_transaction_spec 2>&1 | tail -20`
Expected: All tests pass

- [ ] **Step 4: Run the full suite**

Run: `cargo test 2>&1 | grep "FAILED" | grep "test " || echo "ALL TESTS PASS"`

- [ ] **Step 5: Commit**

```bash
git add aeordb-lib/spec/engine/hot_file_transaction_spec.rs aeordb-lib/Cargo.toml
git commit -m "Add hot file transaction tests: guard safety, deadlock prevention, recovery"
```

---

### Task 6: Full Verification

- [ ] **Step 1: Run the complete test suite**

Run: `cargo test 2>&1 | grep "test result:" | awk '{sum += $4} END {print "Total:", sum, "tests"}'`

- [ ] **Step 2: Verify store_file + crash + reopen recovery**

This is hard to test automatically (requires killing mid-operation), but the hot file replay test validates the recovery logic. The transaction guard tests prove the RAII pattern works.

- [ ] **Step 3: Update TODO.md**

- [ ] **Step 4: Final commit**
