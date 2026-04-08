# Disk-Resident KV Store — Spec

**Date:** 2026-04-07
**Status:** Draft
**Priority:** High — KV store Vec::insert is O(N), degrading write throughput at 50K+ entries

---

## 1. The Problem

The KV store is entirely in memory as a sorted `Vec<KVEntry>`. Every insert does `Vec::insert` at a sorted position, shifting all subsequent entries — O(N). At 250K files (~750K KV entries), this memcpy dominates write throughput (72/s, down from 1,000/s at 20K).

Additionally, the entire KV store is rebuilt from a full entry scan on every database open. At millions of entries, startup takes minutes.

The KV store cannot remain fully in-memory at scale.

---

## 2. Architecture

```
hash → NVT (in memory) → bucket index → KV block page (on disk) → KV entry → entity file offset
```

**NVT** — in memory, ~1MB at 64K buckets. Maps hash → scalar → bucket index. IS the index. IS the iterator. Stays in RAM forever.

**KV block** — on disk at `kv_block_offset`. Array of fixed-size bucket pages. Bucket N at `kv_block_offset + N * page_size`. Each page holds unsorted KV entries for that bucket.

**Write buffer** — bounded in-memory HashMap of recent writes not yet flushed to disk. Checked first on every lookup.

**Hot cache** — bounded LRU HashMap of recently read entries from disk. Configurable pool size.

---

## 3. NVT Resolution

The NVT bucket count scales with the KV block stage — not a fixed size.

| Stage | KV Block Size | NVT Buckets | Entries/Bucket (at capacity) |
|-------|-------------|-------------|------------------------------|
| 0 | 64 KB | 1,024 | ~1-2 |
| 1 | 256 KB | 4,096 | ~1-2 |
| 2 | 1 MB | 8,192 | ~3 |
| 3 | 4 MB | 16,384 | ~6 |
| 4 | 16 MB | 32,768 | ~12 |
| 5 | 64 MB | 65,536 | ~23 |
| 6 | 256 MB | 65,536 | ~92 |
| 7 | 1 GB | 131,072 | ~183 |

Small databases start with 1,024 buckets (same as today). The NVT grows alongside the KV block when a resize is triggered. No waste for small databases, high resolution at scale.

---

## 4. KV Block Layout

```
[file header: 256 bytes]
[kv_block_offset →]
  [bucket 0 page: entry, entry, <empty slots>]
  [bucket 1 page: entry, <empty slots>]
  [bucket 2 page: entry, entry, entry, entry, <empty slots>]
  ...
  [bucket 65535 page: entry, <empty slots>]
[← kv_block_offset + kv_block_length]
[NVT data]
[append area: entries...]
```

### Bucket page format

```
[entry_count: u16]
[entry 0: hash(32 bytes) + type_flags(1 byte) + offset(8 bytes)]
[entry 1: ...]
...
[<empty slots>]
```

Each KV entry is 41 bytes (32-byte hash + 1 type_flags + 8 offset). Page size = 2 + (max_entries_per_page × 41).

### Page size calculation

Target: room for ~32 entries per page (handles 1M+ files at 64K buckets with headroom).

Page size: 2 + (32 × 41) = 1,314 bytes. Round to 1,536 bytes (1.5KB) for alignment.

Total KV block at 64K buckets: 64K × 1,536 = 96MB. Acceptable for databases with millions of files.

For smaller databases (< 10K files), the KV block is mostly empty pages. Sparse on disk — the OS handles this efficiently (filesystem holes, page cache).

---

## 5. Stage Table (KV Block Growth)

The KV block starts small and grows in stages. Each stage provides ~4x headroom.

| Stage | KV Block Size | Max Entries (~) | Notes |
|-------|-------------|-----------------|-------|
| 0 | 64 KB | ~1,500 | Fresh database |
| 1 | 256 KB | ~6,000 | Small workloads |
| 2 | 1 MB | ~24,000 | Medium |
| 3 | 4 MB | ~96,000 | |
| 4 | 16 MB | ~384,000 | |
| 5 | 64 MB | ~1.5M | |
| 6 | 256 MB | ~6M | |
| 7 | 1 GB | ~24M | Large-scale |

At each stage, page size and bucket count may adjust. For stages 0-2, use fewer buckets (4K or 16K) with smaller pages. From stage 3+, use 64K buckets with full pages.

Growth trigger: when a bucket page overflows (write attempt to a full page), the KVResizeManager kicks in.

---

## 6. Operations

### Lookup

```
1. Check write buffer (HashMap, O(1))
2. If found → return
3. Check hot cache (LRU HashMap, O(1))
4. If found → return
5. NVT: hash → scalar → bucket index
6. Seek to kv_block_offset + bucket_index * page_size
7. Read page (~1.5KB)
8. Scan entries in page (linear, ~4-15 entries)
9. If found → insert into hot cache → return
10. Not found → return None
```

### Insert

```
1. Add to write buffer (HashMap, O(1))
2. If write buffer exceeds threshold (e.g., 1000 entries):
   a. For each buffered entry:
      - NVT: hash → bucket index
      - Seek to bucket page on disk
      - Read page
      - If page has space → add entry, write page back
      - If page full → trigger KV resize (stage up)
   b. Clear write buffer
```

### Delete (mark deleted)

```
1. Check write buffer → if present, mark deleted there
2. Otherwise, add a deletion marker to write buffer
3. On flush: find entry in bucket page → set KV_FLAG_DELETED on type_flags → write page back
```

### Iteration

```
1. Walk NVT buckets 0..bucket_count
2. For each bucket with entries:
   a. Read bucket page from disk
   b. Emit entries (skipping deleted)
3. Also emit entries from write buffer (may overlap — buffer takes priority)
```

---

## 7. Write Buffer

Bounded in-memory HashMap: `HashMap<Vec<u8>, KVEntry>`.

- Max size: 1,000 entries (configurable)
- On overflow: flush all buffered entries to disk pages
- On explicit flush (e.g., before snapshot): flush all
- On shutdown: flush all
- On crash: lost. Recovery: full entry scan to rebuild (existing fallback)

The write buffer is checked BEFORE the on-disk KV block on every lookup. Entries in the buffer supersede entries on disk (handles updates and deletes).

---

## 8. Hot Cache

Bounded LRU HashMap: `HashMap<Vec<u8>, KVEntry>` with LRU eviction.

- Max size: 10,000 entries (configurable)
- Populated on disk reads (lookup miss in buffer → read from disk → cache)
- Evicts least recently used when full
- Invalidated on write (if an entry is updated, remove from cache)

The hot cache reduces disk reads for frequently accessed entries (the same B-tree internal nodes are read on every insert — caching them avoids repeated seeks).

---

## 9. KV Resize (Existing Infrastructure)

When a bucket page overflows during flush:

1. Set `resize_in_progress = true` in file header
2. Allocate new KV block at `buffer_kvs_offset` (next stage size)
3. Write the overflowed entry to the new block immediately
4. Both old and new blocks are live — reads check both
5. Background: migrate entries from old block → new block (page by page)
6. When complete: update `kv_block_offset` to new block, clear `resize_in_progress`
7. Old block space becomes void (reclaimable)

On startup with `resize_in_progress = true`: resume migration or fall back to full scan.

---

## 10. Startup

### Normal startup (no resize in progress)

```
1. Read file header → kv_block_offset, kv_block_length
2. Read NVT from disk (or rebuild from KV block if not persisted)
3. KV block is already on disk → ready for lookups
4. Write buffer starts empty
5. Hot cache starts empty
6. No entry scan needed
```

### Dirty startup (crash, or resize_in_progress)

```
1. Read file header → detect resize_in_progress
2. Fall back to full entry scan → rebuild NVT + KV block from scratch
3. Clear resize_in_progress flag
```

### First-time startup (new database)

```
1. Create KV block at stage 0 (64KB)
2. Initialize empty NVT
3. Write file header
```

---

## 11. NVT Persistence

The NVT needs to be on disk too — otherwise we'd need to rebuild it from the KV block on every startup.

Store the NVT in the file header area (it already has `nvt_offset` and `nvt_length` fields). The NVT is small (64K × 16 bytes = 1MB) — write it alongside the KV block during flush or resize.

On startup: read NVT from disk → ready immediately.

---

## 12. Impact on Existing Code

### StorageEngine

- `get_entry`: uses KV lookup (buffer → cache → disk) to find offset, then reads entry from append area
- `store_entry`: appends entry to file, adds KV entry to write buffer (not disk yet)
- `flush_batch`: same but multiple entries to write buffer
- New: `flush_kv_buffer()`: writes buffer entries to disk pages
- New: `open` no longer scans all entries (reads KV block + NVT from disk)

### KVStore / KVResizeManager

Major refactor — the KV store becomes a thin wrapper around:
- NVT (in memory)
- Write buffer (HashMap)
- Hot cache (LRU HashMap)
- Disk KV block reader/writer

### AppendWriter

No change — entries are still appended. The KV block is a separate area at the front of the file.

### All callers

No interface change — `get_entry`, `store_entry`, `has_entry` signatures unchanged. The disk-resident KV is an internal optimization.

---

## 13. Expected Performance

| Files | Current (Vec) | Disk KV | Why |
|-------|-------------|---------|-----|
| 20K | 1,000/s | ~1,000/s | Write buffer absorbs, same speed |
| 50K | 617/s | ~900/s | No Vec::insert shift |
| 100K | 265/s | ~800/s | Disk page writes are O(1) |
| 250K | 72/s | ~700/s | Bounded by disk I/O, not memory |
| 1M | N/A (OOM risk) | ~500/s | KV fits on disk, not RAM |

Write throughput should be nearly flat — bounded by disk I/O for page writes and entity appends, not by memory operations.

---

## 14. Implementation Phases

### Phase 1 — KV block page format + read/write
- Page serialization/deserialization
- Read a bucket page from disk
- Write a bucket page to disk
- Stage table for KV block sizing

### Phase 2 — Write buffer + disk flush
- In-memory HashMap write buffer
- Flush buffer → write to bucket pages
- Threshold-based auto-flush

### Phase 3 — Disk-based lookup path
- Lookup: buffer → NVT → disk page → scan entries
- Replace Vec-based KV store with disk-resident version
- Hot cache (LRU)

### Phase 4 — Startup without scan
- Read KV block + NVT from disk on open
- No full entry scan (unless dirty)
- NVT persistence

### Phase 5 — KV resize integration
- Overflow detection → stage up
- Dual-block operation during resize
- Migration + cutover

### Phase 6 — Benchmark
- Re-run stress test to 250K
- Compare: Vec vs disk KV at each checkpoint
- Memory usage comparison

---

## 15. Non-Goals (Deferred)

- Metrics-driven growth prediction (use metrics to anticipate resize)
- Dynamic bucket count adjustment based on entry density
- Compression of KV block pages
- MMAP for KV block access (use explicit seek + read for now)
- Concurrent readers during KV page write (single-writer model)

These have been added to `future-plans.md`.