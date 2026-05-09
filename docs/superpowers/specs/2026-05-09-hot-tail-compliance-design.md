# Hot Tail Compliance Fix

**Date:** 2026-05-09
**Status:** Draft
**Reference:** `docs/superpowers/specs/2026-04-28-single-file-database-design.md` Section 3 (Write Flow)

## Problem

The original single-file database spec defines a precise write flow (Section 3, steps 1-7). Three critical steps were never implemented:

1. **Step 6: Re-write hot tail at new hot_tail_offset after every WAL append** — Not implemented. The hot tail is only rewritten when a threshold is reached (every 32 inserts) or during batch operations. Between rewrites, the hot tail on disk is overwritten by WAL entries and is INVALID.

2. **Step 7: Update hot_tail_offset in file header** — Not implemented during normal operation. The header's hot_tail_offset is only updated during shutdown. On crash, the header points to a position that has been overwritten by WAL entries.

3. **250ms timer flush (Section 4)** — Never implemented. The timer was designed to keep the write buffer small (making per-entry hot tail rewrites cheap) and provide periodic fsync for durability.

These gaps cause data loss on any non-clean shutdown: the hot tail is invalid, the header points to the wrong position, and KV entries in the write buffer are lost.

## Root Cause Analysis

The original implementation used the hot tail as a lazy journal (write only at threshold) instead of a live journal (write after every entry). This was done for performance, but it breaks the crash recovery model:

- **Without per-entry hot tail rewrite:** Between rewrites, the on-disk hot tail is garbage (overwritten by WAL data). Process crash = entries lost.
- **Without header updates:** Even when the hot tail IS rewritten at the correct position, the header doesn't point to it. Restart can't find the hot tail.
- **Without the timer:** The write buffer grows large, making per-entry hot tail rewrites expensive. The timer was designed to keep the buffer small via periodic flushing to KV pages.

## Fix

### Change 1: Implement 250ms timer flush (CRITICAL)

This is the primary missing piece. The timer was designed in the original spec but never implemented. It is the main durability mechanism.

Spawn a tokio interval task in StorageEngine startup (or server startup):

```
Every 250ms:
  try_lock writer (non-blocking)
  IF NOT acquired: skip (writer is busy, data is safe — active write maintains hot tail)

  try_lock kv (non-blocking)
  IF NOT acquired: skip

  IF write_buffer is empty: release locks, skip

  1. kv.flush_hot_buffer()
     → write ALL write_buffer entries to hot tail at hot_tail_offset
     → sync_data()

  2. kv.flush()
     → move write_buffer entries to KV pages
     → sync_data()
     → write empty hot tail (entries are now in pages)

  3. Update header: hot_tail_offset, entry_count
     → writer.update_header()
     → sync_data()

  Release locks
```

This provides:
- **Periodic durability** — entries reach disk (hot tail + KV pages) every 250ms
- **Small write buffer** — flushed to KV pages regularly, never grows unbounded
- **Header stays current** — hot_tail_offset updated every 250ms
- **Maximum data loss window: 250ms** — recoverable via WAL scan

### Change 2: Update header hot_tail_offset during every flush

Currently `flush_batch_and_update_head` updates the header for `head_hash` but NOT `hot_tail_offset`. Every place that writes the header must include the current `hot_tail_offset`:

- `flush_batch_and_update_head` — add `header.hot_tail_offset = writer.current_offset()`
- Timer flush — full header update including `hot_tail_offset`
- Shutdown — already updates `hot_tail_offset` (correct)
- `flush_hot_buffer` within `flush_batch` — header not written here (timer handles it)

### Change 3: HOT_BUFFER_THRESHOLD = 512

The threshold is a safety net for burst writes between timer ticks. Per-entry hot tail rewrite was tried and was too slow — the batched approach is deliberate.

- Previous: 1000 (original), then 32 (my overcorrection)
- Correct: 512 — balances burst handling with durability
- On reaching threshold: `flush_hot_buffer()` writes hot tail + sync_data

### Change 4: Auto-recover from corrupt hot tail on startup

The spec says (Section 6, line 198): "If invalid → dirty startup."

Currently, corrupt hot tail on startup results in an empty write buffer — entries silently lost. The fix:

```
On startup:
  1. Read hot tail from header.hot_tail_offset
  2. IF valid → load entries into write buffer, continue
  3. IF invalid → DIRTY STARTUP:
     a. Log warning: "Corrupt hot tail detected — performing full WAL scan to rebuild KV"
     b. Continue opening the engine (KV pages have most entries)
     c. After engine is constructed: rebuild_kv() — full WAL scan
     d. Write fresh hot tail + update header
```

This replaces the current behavior (silently start with empty buffer, missing entries) with automatic recovery. No need for manual `verify --repair`.

### Change 5: Remove explicit flush_hot_buffer from flush_batch/flush_batch_and_update_head

The explicit `kv.flush_hot_buffer()` calls I added today in `flush_batch` and `flush_batch_and_update_head` should be removed. The timer and threshold handle durability. Adding explicit flushes per-batch was a band-aid for the missing timer — with the timer in place, the band-aid is unnecessary and adds extra fsync overhead per batch.

The batch operations still need to update the header's `hot_tail_offset` (Change 2) so the header pointer stays current.

## Write Flow (Post-Fix)

```
store_entry_internal:
  Lock: writer(WRITE) + kv(LOCK)

  1. writer.append_entry(type, key, value, flags)
     → write at current_offset (NO fsync)
     → advance current_offset

  2. kv.set_hot_tail_offset(writer.current_offset())

  3. kv.insert(KVEntry { hash, type_flags, offset })
     → write_buffer.insert(hash, entry)
     → hot_buffer.push(entry)
     → IF hot_buffer.len() >= 512:
         flush_hot_buffer() → write hot tail + sync_data
     → ELSE:
         publish_buffer_only() → update in-memory snapshot

  Unlock


flush_batch_and_update_head:
  Lock: writer(WRITE) + kv(LOCK)

  1. Append all batch entries to WAL (NO fsync per entry)
  2. kv.insert() for each entry
  3. Update header: head_hash AND hot_tail_offset
     → writer.update_file_header() → sync_data

  Unlock


Timer (250ms):
  try_lock writer + kv (non-blocking)
  IF acquired AND write_buffer non-empty:
    1. kv.flush_hot_buffer() — write hot tail + sync_data
    2. kv.flush() — write_buffer → KV pages + sync_data
    3. writer.update_header(hot_tail_offset, entry_count) — sync_data
  Release
```

## Crash Recovery (Post-Fix)

| Crash Point | State | Recovery |
|---|---|---|
| After WAL append, before timer flush | WAL has entry, hot tail stale | Dirty startup: WAL scan recovers entry |
| After threshold hot tail flush | Hot tail valid, header may be stale | Header points to old position, dirty startup if hot tail at header offset is corrupt |
| After timer flush | Hot tail valid OR empty (entries in pages), header current | Clean restart |
| Between timer flushes (< 250ms) | Up to 250ms of entries in memory only | Dirty startup: WAL scan recovers |

**Maximum data loss window: 250ms of KV mappings.** WAL entries are ALWAYS recoverable via full scan.

## Files Changed

- `engine/storage_engine.rs` — spawn timer task, dirty startup auto-recovery, update hot_tail_offset in flush_batch_and_update_head header write, remove explicit flush_hot_buffer from batch ops
- `engine/disk_kv_store.rs` — HOT_BUFFER_THRESHOLD = 512
- `server/mod.rs` or `cli/commands/start.rs` — spawn timer task on server start
