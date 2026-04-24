# Hot File Transactions — Design Spec

**Date:** 2026-04-24
**Status:** Approved

## Problem

A `store_file` operation involves 4-7 separate `store_entry` calls (chunks, file record at 3 keys, directory propagation up to root). Each entry is individually fsync'd, but the sequence is not atomic. If a crash occurs between storing the file record and updating the parent directory, the file exists but is invisible in listings.

The hot file (write-ahead log for KV entries) currently truncates on every `flush()`, which can happen mid-sequence. This means the hot file doesn't protect the full operation — only individual entry-to-KV consistency.

The same problem exists for `delete_file`: crash between the deletion record and directory update leaves a stale child entry pointing to nothing.

## Solution

Delay hot file truncation until the full multi-entry operation completes. The hot file becomes an implicit intent log — if a crash happens mid-operation, all KV entries from the incomplete operation survive in the hot file and can be used during restart recovery to detect and fix directory inconsistencies.

## Design

### 1. Transaction Depth on KV Store

Add `transaction_depth: u32` to `DiskKVStore`.

**New methods on `StorageEngine`:**
- `begin_transaction()` — acquires KV lock, increments `transaction_depth`
- `end_transaction()` — acquires KV lock, decrements `transaction_depth`, truncates hot file when it reaches 0

**Change in `DiskKVStore::flush()`:**
Skip `truncate_hot_file()` when `transaction_depth > 0`. Everything else (writing KV pages, publishing snapshots, flushing hot buffer) still happens normally.

No deadlock risk: `store_file_internal` doesn't hold the KV lock between `store_entry` calls — each call acquires and releases independently. `begin_transaction` and `end_transaction` acquire the KV lock briefly.

### 2. RAII Transaction Guard

A `TransactionGuard` ensures `end_transaction` is always called, even on error or panic:

```rust
struct TransactionGuard<'a>(&'a StorageEngine);

impl<'a> TransactionGuard<'a> {
    fn new(engine: &'a StorageEngine) -> Self {
        engine.begin_transaction();
        TransactionGuard(engine)
    }
}

impl<'a> Drop for TransactionGuard<'a> {
    fn drop(&mut self) {
        self.0.end_transaction();
    }
}
```

### 3. Wrapping Operations in Transactions

**`store_file_internal_inner`** in `directory_ops.rs`:
```rust
fn store_file_internal_inner(...) -> EngineResult<FileRecord> {
    let _guard = TransactionGuard::new(self.engine);
    // ... all store_entry calls + propagate_to_parents ...
    // guard drops here → end_transaction → truncate hot file
}
```

**`delete_file`** in `directory_ops.rs`:
```rust
pub fn delete_file(...) -> EngineResult<()> {
    let _guard = TransactionGuard::new(self.engine);
    // ... deletion record + directory update ...
}
```

### 4. Recovery on Restart

During `open_internal`, after hot file replay and engine construction:

1. Filter replayed entries for FileRecord types
2. For each FileRecord, read the value and extract the file path
3. Check if the parent directory lists this file
4. If not listed → re-run `propagate_to_parents` for that path (file was stored but directory wasn't updated before crash)

Similarly for DeletionRecords:
1. Filter replayed entries for DeletionRecord types
2. For each, check if the parent directory still lists the deleted file
3. If still listed → remove from directory listing

This is a one-time recovery step during startup. Only processes entries from the hot file replay (typically 0-100 entries). Negligible startup cost.

## Testing Strategy

**Transaction depth:**
- `store_file` wraps in transaction → hot file not truncated mid-sequence
- `end_transaction` triggers truncation → hot file empty after
- Failed `store_file` → guard calls `end_transaction`, depth returns to 0

**RAII guard safety:**
- Panic inside transaction (via `catch_unwind`) → guard fires, `transaction_depth` is 0
- Deeply nested error chain → depth returns to 0, hot file truncated on next success

**Deadlock prevention:**
- Transaction counter always returns to zero after any exit path (success, error, panic)
- Multiple sequential transactions → counter stays at 0 between operations

**Recovery:**
- Store file + simulate crash (leave hot file intact) + reopen → file listed in parent directory
- Delete file + simulate crash + reopen → file removed from parent listing
- Multiple incomplete operations in hot file → all recovered on restart

## Out of Scope

- Wrapping `rename_file` in transactions (lower risk, extend later)
- Cross-engine transactions (multi-database atomicity)
- Rollback semantics (this is replay/recovery, not undo)
