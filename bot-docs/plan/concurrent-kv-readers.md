# Concurrent KV Readers During Write — Spec

**Date:** 2026-04-08
**Status:** Approved
**Priority:** High — silent cliff under concurrent load

---

## 1. Problem

`DiskKVStore` wraps all state in a single struct behind `RwLock<DiskKVStore>`. Every method — including `get()` — takes `&mut self` because `get()` populates the hot cache on disk miss. As a result, every caller acquires a **write lock**, making the `RwLock` semantically equivalent to a `Mutex`. Reads serialize behind writes. Under concurrent HTTP traffic or long-running internal operations (GC mark, query scans), this becomes a cliff — not a graceful degradation.

---

## 2. Solution: Snapshot-Based Double Buffering

Split `DiskKVStore` into a writer and a reader snapshot. The writer owns mutable state and publishes immutable snapshots after each mutation. Readers grab snapshots lock-free via `Arc` pointer swap. No locks on the read path.

---

## 3. Components

### ReadSnapshot (immutable, shared)

```rust
struct ReadSnapshot {
    buffer: HashMap<Vec<u8>, KVEntry>,   // frozen write buffer
    nvt: Arc<NormalizedVectorTable>,      // shared, cloned on flush
    bucket_count: usize,
    hash_algo: HashAlgorithm,
}
```

Published by the writer after every mutation. Readers grab it via `Arc::clone` (one atomic op). `ReadSnapshot::get()` is `&self` — pure, no mutation, no locks.

**Read path:**
1. Check `self.buffer.get(hash)` — return if found (check deleted flag)
2. Compute bucket via `HashConverter::convert(hash_bytes)` → scalar → `self.nvt.bucket_for_value(scalar)`
3. `File::try_clone()` the KV file handle
4. Seek to `bucket_index * page_size`, read page, deserialize, scan for hash
5. Return entry or None

**`iter_all()` on ReadSnapshot:**
Read all disk pages via cloned file handle, merge frozen buffer on top (buffer wins on conflict), exclude deleted entries. Same logic as today, operating on immutable data.

### KVWriter (mutable, exclusive)

Renamed from `DiskKVStore`. Holds the mutable working state:
- `write_buffer: HashMap<Vec<u8>, KVEntry>`
- `nvt: NormalizedVectorTable` (working copy)
- `kv_file: File`
- `hot_file: Option<File>`
- `hot_buffer: Vec<KVEntry>`
- Stage, bucket_count, entry_count, etc.

Behind a `Mutex<KVWriter>` — only writes acquire it.

**After every `insert()`:**
1. Insert into write_buffer (same as today)
2. Write to hot file journal (same as today)
3. If `write_buffer.len() >= 512` → `flush()`
4. Clone write_buffer into new `ReadSnapshot`, reuse current `Arc<NVT>`, atomically swap the shared pointer

**On `flush()`:**
1. Drain write buffer to disk pages (same as today)
2. `nvt_increment` for each entry (same as today)
3. Clone NVT into new `Arc<NVT>`
4. Publish snapshot: empty buffer + new `Arc<NVT>`, atomic swap

**On `mark_deleted()` / `update_flags()`:**
Writer uses its own write_buffer + direct disk read (via its own file handle) for lookup. Then publishes updated snapshot.

### Atomic Swap Mechanism

`arc_swap::ArcSwap<ReadSnapshot>` — lock-free load/store of `Arc` pointers.
- Readers: `.load()` — one atomic op, returns `Guard<Arc<ReadSnapshot>>`
- Writer: `.store(new_snapshot)` — one atomic op

New dependency: `arc_swap` crate.

---

## 4. StorageEngine Changes

**Fields change from:**
```rust
kv_store: RwLock<DiskKVStore>,
```

**To:**
```rust
kv_writer: Mutex<KVWriter>,
kv_snapshot: Arc<ArcSwap<ReadSnapshot>>,
kv_file: File,  // base handle for try_clone in ReadSnapshot::get()
```

**Read methods become lock-free:**

| Method | Before | After |
|--------|--------|-------|
| `get_entry()` | `kv_store.write()` → `kv.get()` | `kv_snapshot.load()` → `snapshot.get()` |
| `has_entry()` | `kv_store.write()` → `kv.get()` | `kv_snapshot.load()` → `snapshot.get()` |
| `is_entry_deleted()` | `kv_store.write()` → `kv.get()` | `kv_snapshot.load()` → `snapshot.get()` |
| `entries_by_type()` | `kv_store.write()` → `kv.iter_all()` | `kv_snapshot.load()` → `snapshot.iter_all()` |
| `iter_kv_entries()` | `kv_store.write()` → `kv.iter_all()` | `kv_snapshot.load()` → `snapshot.iter_all()` |
| `stats()` | `kv_store.write()` | Snapshot for counts, writer mutex for flush-sensitive stats |

**Write methods use Mutex (same exclusivity, different lock type):**

| Method | Before | After |
|--------|--------|-------|
| `store_entry()` | `kv_store.write()` → `kv.insert()` | `kv_writer.lock()` → `writer.insert()` |
| `store_entry_typed()` | `kv_store.write()` → `kv.insert()` | `kv_writer.lock()` → `writer.insert()` |
| `store_entry_compressed()` | `kv_store.write()` → `kv.insert()` | `kv_writer.lock()` → `writer.insert()` |
| `mark_entry_deleted()` | `kv_store.write()` → `kv.update_flags()` | `kv_writer.lock()` → `writer.update_flags()` |
| `remove_kv_entry()` | `kv_store.write()` → `kv.mark_deleted()` | `kv_writer.lock()` → `writer.mark_deleted()` |
| `flush_batch()` | `kv_store.write()` | `kv_writer.lock()` |

---

## 5. Hot Cache Removal

The hot cache (`hot_cache: HashMap`, `cache_order: Vec`, `HOT_CACHE_MAX`) is deleted entirely. It served two purposes:

1. **Cache repeated disk reads** — the OS page cache handles this
2. **Avoid re-reading recently written entries** — the write buffer snapshot handles this

Two layers (write buffer + OS cache) replace three (write buffer + hot cache + OS cache). Less code, fewer invariants, no cache coherency logic.

---

## 6. Write Buffer Threshold Change

`WRITE_BUFFER_THRESHOLD` changes from 1000 to 512. This controls:
- How often the write buffer flushes to disk
- How often a new `Arc<NVT>` is created (expensive clone, only on flush)
- Maximum size of the HashMap cloned on each insert (~25KB at 512 entries)

512 balances flush frequency against snapshot clone cost.

---

## 7. Consistency Model

**Read-committed isolation.** A reader's snapshot reflects all writes completed before the snapshot was created. Writes that happen after the reader grabs the snapshot are invisible to that reader. The next reader gets a fresh snapshot.

**Stale window:** Between inserts, the snapshot is at most one insert behind. Between flushes, the NVT is at most 512 inserts behind — but entries written since the last flush are in the buffer, not on disk, so the stale NVT doesn't matter for those.

**Resize safety:** If the writer resizes (new kv_file, new page layout), readers holding old snapshots still work — their `kv_file` clone points to the old file (OS keeps it alive until last handle closes), their NVT matches the old layout. Next snapshot picks up the new file.

---

## 8. No Public API Changes

`StorageEngine`'s public method signatures are unchanged. All callers (HTTP routes, DirectoryOps, GC, query engine, backup, etc.) continue calling the same methods with the same arguments. The internal locking strategy changes but the interface is stable.

---

## 9. Testing

### Unit tests (ReadSnapshot)
- `get()` finds entry in frozen buffer
- `get()` falls through to disk when not in buffer
- `get()` returns None for deleted entries (buffer tombstone)
- `get()` returns None for entries not in buffer or disk
- `iter_all()` merges buffer + disk, buffer wins on conflict
- `iter_all()` excludes deleted entries

### Unit tests (KVWriter)
- `insert()` publishes new snapshot after each call
- Snapshot buffer contains the inserted entry
- Flush at 512 entries triggers NVT refresh
- Snapshot after flush has empty buffer + new NVT
- `mark_deleted()` publishes updated snapshot with tombstone

### Concurrency tests
- N reader threads + 1 writer thread, all operating simultaneously — readers never panic, never see corrupt data
- Long-running reader (simulating GC walk via iter_all) doesn't block writes
- Writes don't block concurrent readers
- Reader grabs snapshot, writer flushes + resizes underneath, reader's snapshot still returns correct data

### Regression tests
- All existing `disk_kv_store_spec.rs` tests continue passing
- All existing engine/HTTP/GC tests continue passing (no public API changes)

---

## 10. Implementation Phases

### Phase 1 — ReadSnapshot struct + get()
- Define `ReadSnapshot` with buffer, `Arc<NVT>`, bucket_count, hash_algo
- Implement `ReadSnapshot::get(&self, hash, kv_file)` — buffer check → disk read
- Implement `ReadSnapshot::iter_all(&self, kv_file)` — merge buffer + disk
- Unit tests for ReadSnapshot

### Phase 2 — KVWriter refactor
- Rename/refactor `DiskKVStore` into `KVWriter`
- Remove hot cache (hot_cache, cache_order, cache_put, HOT_CACHE_MAX)
- Change WRITE_BUFFER_THRESHOLD to 512
- Add snapshot publishing after insert/flush/mark_deleted
- Add `arc_swap` dependency
- Unit tests for KVWriter snapshot publishing

### Phase 3 — StorageEngine wiring
- Replace `RwLock<DiskKVStore>` with `Mutex<KVWriter>` + `Arc<ArcSwap<ReadSnapshot>>`
- Rewire all read methods to use snapshot
- Rewire all write methods to use writer mutex
- Run full test suite — all existing tests must pass

### Phase 4 — Concurrency tests
- Multi-threaded reader/writer contention tests
- Starvation tests (long reader + concurrent writes)
- Snapshot-during-resize safety test

### Phase 5 — Cleanup
- Remove dead code (hot cache remnants, old get(&mut self))
- Update DiskKVStore references in comments/docs
- Benchmark: concurrent read throughput before vs after

---

## 11. Non-goals (deferred)

- File handle pooling (optimize try_clone if profiling shows it matters)
- Lock-free write path (writer is already single-threaded, Mutex is fine)
- Snapshot compression (buffer is <25KB, not worth it)
- MMAP for page reads (separate future plan)
- Write-ahead of snapshot to disk (snapshots are in-memory only, rebuilt on restart)

---

## 12. Dependencies

- `arc_swap` crate — lock-free atomic `Arc` pointer swap
