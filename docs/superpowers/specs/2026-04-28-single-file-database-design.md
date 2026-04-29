# Single-File Database Refactor — Design Spec

**Date:** 2026-04-28
**Status:** Approved
**Priority:** Critical — eliminates sidecar files, completes the original disk-resident KV design

## Overview

Consolidate the three database files (`.aeordb` WAL, `.aeordb-{name}.kv` sidecar, `.aeordb-{name}-hot{N}` hot file) into a single `.aeordb` file. The KV block lives at the head of the file, the WAL append area in the middle, and the hot tail dangles off the end.

**Prior art:** `bot-docs/plan/disk-resident-kvs.md` (the original design that was shortcut with a sidecar file)

---

## 1. File Layout

```
[File Header: 256 bytes]
  magic: "AEOR" (4 bytes)
  header_version: u8
  hash_algo: u16
  created_at: i64
  updated_at: i64
  kv_block_offset: u64 (= 256, right after header)
  kv_block_length: u64 (stage-dependent)
  kv_block_stage: u8 (0..9+)
  nvt_offset: u64
  nvt_length: u64
  hot_tail_offset: u64
  head_hash: [u8; hash_length]
  entry_count: u64
  resize_in_progress: bool
  resize_target_stage: u8
  ... (remaining fields, padded to 256 bytes)

[KV Block: offset 256 → 256 + kv_block_length]
  Bucket pages, each page_size(hash_length) bytes
  Bucket N at kv_block_offset + N * page_size
  Pinned at the head — grows in place via background WAL relocation

[NVT: nvt_offset → nvt_offset + nvt_length]
  Persisted NVT bitmap, bucket count matches KV stage

[WAL Append Area: after NVT → hot_tail_offset]
  Entries: Chunk, FileRecord, DirectoryIndex, DeletionRecord, Snapshot, Void
  Append-only, grows toward hot tail

[Hot Tail: hot_tail_offset → EOF]
  Magic: 0xAE017DB100C (5 bytes)
  entry_count: u32 (4 bytes)
  entry_count_crc32: u32 (4 bytes) — CRC32 of the entry_count bytes
  entries: [hash(N) + type_flags(1) + offset(8)] × entry_count
```

No sidecar files. One `.aeordb` file is the entire database.

---

## 2. KV Stage Table

Aggressive growth when small (cheap to relocate), conservative when large.

| Stage | Size | Growth | Max Entries (~) |
|-------|------|--------|-----------------|
| 0 | 64 KB | — | 1,500 |
| 1 | 512 KB | 8x | 12,000 |
| 2 | 4 MB | 8x | 96,000 |
| 3 | 32 MB | 8x | 768,000 |
| 4 | 128 MB | 4x | 3,000,000 |
| 5 | 512 MB | 4x | 12,000,000 |
| 6 | 1 GB | 2x | 24,000,000 |
| 7 | 2 GB | 2x | 48,000,000 |
| 8 | 4 GB | 2x | 96,000,000 |
| 9 | 8 GB | 2x | 192,000,000 |

Stages 0-6 are defined explicitly. Stage 7+ follows `previous * 2`. No hard cap.

### Bucket page format

```
entry_count: u16 (2 bytes)
entries: [hash(32) + type_flags(1) + offset(8)] × MAX_ENTRIES_PER_PAGE
```

MAX_ENTRIES_PER_PAGE = 32. Page size = 2 + (32 × 41) = 1,314 bytes, rounded to 1,536 bytes.

---

## 3. Write Flow

### WAL append (store_entry)

1. Truncate file at `hot_tail_offset` (remove old hot tail)
2. Write new WAL entry at `hot_tail_offset`
3. Update `hot_tail_offset` to after the new entry
4. Add KV entry to in-memory write buffer
5. Check flush triggers:
   - Buffer count ≥ 1,000 → flush
   - 250ms timer fired since last flush AND buffer non-empty → flush
6. Re-write hot tail at new `hot_tail_offset` (magic + count + CRC + entries)
7. Update `hot_tail_offset` in file header

### Flush (write buffer → KV bucket pages)

1. For each buffered entry:
   - NVT: hash → bucket index
   - Seek to `kv_block_offset + bucket_index * page_size`
   - Read page, add entry, write page back
   - If page full → trigger resize (see Section 5)
2. Clear write buffer
3. Write empty hot tail (magic + count=0 + CRC)
4. Persist NVT to `nvt_offset`
5. Update header (`hot_tail_offset`, `entry_count`)

### Lookup (get_entry)

1. Check write buffer (HashMap, O(1))
2. If found → return
3. NVT: hash → bucket index
4. Seek to bucket page, read page (~1.5KB)
5. Scan entries in page (linear, ~4-15 entries)
6. Return result

---

## 4. Flush Triggers

### Count-based

Write buffer reaches 1,000 entries → immediate flush during the `store_entry` call.

### Timer-based

Tokio interval task, 250ms tick:

```
tick → try_lock(writer) →
  acquired AND buffer non-empty → flush → release
  NOT acquired (writer busy) → skip, next tick will try
```

If the writer is busy, the hot tail is being maintained by the active write — data is safe on disk. The flush is a performance optimization (faster KV lookups), not a durability concern.

### Explicit

- Before snapshot creation
- On shutdown
- Before GC

---

## 5. KV Resize

The KV block is pinned at the head of the file. When it needs to grow, WAL entries in the growth zone are relocated to make room.

**The KV is not critical for storing — it's recoverable.** If it can't grow, writes still work (hot buffer absorbs). Performance degrades gracefully until resize completes.

### Trigger

A bucket page is full during flush — entry can't fit.

### Resize flow

1. Set `resize_in_progress = true` and `resize_target_stage` in file header
2. Overflowing entry stays in write buffer (flushed after resize)
3. Calculate growth zone: bytes between current KV block end and target KV block end
4. **Background batch relocation** (async, doesn't block reads or writes):
   - Read growth zone in ~64MB chunks
   - Append each chunk to end of WAL (before hot tail)
   - No need to mark voids — the cleared space IS the new KV space
   - One small void at the boundary if sizes don't align
5. When growth zone is clear:
   - Write new KV bucket pages into the expanded region
   - Full rehash from WAL scan into new pages (more buckets = different assignments)
   - Write new NVT (bigger bucket count)
   - Update header: `kv_block_length`, `kv_block_stage`, `nvt_offset`, `nvt_length`
   - Clear `resize_in_progress`
   - Flush any entries waiting in the write buffer

### During resize, everything keeps working

- Writes → write buffer → hot tail (normal path)
- Reads → write buffer check → current KV pages (still valid, just full)
- Lookups may find entries in the buffer that haven't reached KV pages yet

---

## 6. Startup

### Normal startup (including resume-resize)

1. Read file header → `kv_block_offset`, `kv_block_length`, `nvt_offset`, `hot_tail_offset`
2. Read NVT from `nvt_offset` → ready for lookups immediately
3. Read hot tail at `hot_tail_offset`:
   - Verify magic `0xAE017DB100C`
   - Read `entry_count` + CRC32, verify CRC
   - If valid → load entries into write buffer
   - If invalid → dirty startup
4. If `resize_in_progress` → resume background WAL relocation
5. **No full WAL scan**

### Dirty startup (corrupt hot tail or corrupt KV/NVT)

1. Full WAL entry scan from start of append area to hot tail
2. Rebuild KV bucket pages + NVT at current stage
3. Write fresh empty hot tail
4. **Preserve** `resize_in_progress` and `resize_target_stage` (the resize condition still exists)
5. Resume background relocation if resize was in progress

### First-time create

1. Write file header (256 bytes)
2. Write empty KV block at offset 256 (stage 0 = 64KB)
3. Write empty NVT after KV block
4. Set `hot_tail_offset` to after NVT
5. Database ready — initial writes (root dir, system config) flow through normal write path

---

## 7. Eliminated

- `.aeordb-{name}.kv` sidecar file
- `.aeordb-{name}-hot{N}` hot file
- `derive_kv_path()` helper
- `init_hot_file()` method
- Hot file read/write/truncate methods
- All hot file path construction logic

---

## 8. Code Impact

### FileHeader (`file_header.rs`)
- Add `hot_tail_offset: u64`
- Add `resize_target_stage: u8`
- Existing fields already present: `kv_block_offset`, `kv_block_length`, `nvt_offset`, `nvt_length`, `resize_in_progress`, `buffer_kvs_offset`, `buffer_nvt_offset`

### AppendWriter (`append_writer.rs`)
- Append area starts after NVT, not after file header
- Writes at `hot_tail_offset`, then re-writes hot tail after

### DiskKVStore (`disk_kv_store.rs`)
- Reads/writes bucket pages inside the main `.aeordb` file
- Takes file handle + `kv_block_offset` instead of separate path
- Hot tail replaces hot file
- Timer flush task (250ms)
- Write buffer threshold: 1,000 entries (was 10)

### StorageEngine (`storage_engine.rs`)
- `create()`: writes header + KV block + NVT, sets hot_tail_offset
- `open()`: reads header, loads NVT, loads hot tail — no full scan
- No sidecar file management
- Spawns timer flush task

### Public API
Unchanged. `get_entry`, `store_entry`, `has_entry` — same signatures, same behavior. Entirely internal refactor.

---

## 9. Testing Strategy

### Unit tests
- Bucket page serialize/deserialize roundtrip
- Page overflow detection (32 entries = full)
- Hot tail write/read with magic + CRC verification
- CRC corruption detection
- Magic scan in file with WAL entries

### Integration tests
- Create database → verify single file, no sidecars
- Store 100 entries → retrievable via KV lookup
- Crash simulation → hot tail replays correctly
- Corrupt hot tail → falls back to full WAL scan, data intact
- Timer flush: entries move from hot tail to KV pages after 250ms
- Count flush: 1,001 rapid entries → auto-flush triggered

### Resize tests
- Fill stage 0 until overflow → `resize_in_progress` set
- Background relocation completes → KV expanded, entries accessible
- Crash during resize → reopen → resize resumes, data intact
- No sidecar files at any point

### Stress / regression
- Re-run existing stress test against new engine
- Full existing test suite passes (public API unchanged)
- `aeordb verify` clean
