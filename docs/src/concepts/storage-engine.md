# Storage Engine

The storage engine is an append-only WAL (write-ahead log) where the log IS the database. Every write appends a new entry. The file only grows (until garbage collection reclaims unreachable entries). This design gives you crash recovery, versioning, and integrity verification as structural properties rather than bolted-on features.

## Entry Format

Every entry on disk shares the same header format:

```
[Entry Header - 31 bytes fixed + hash_length variable]
  magic:            u32    (0x0AE012DB - marks the start of a valid entry)
  entry_version:    u8     (format version, starting at 1)
  entry_type:       u8     (Chunk, FileRecord, DirectoryIndex, etc.)
  flags:            u8     (operational flags)
  hash_algo:        u16    (BLAKE3_256 = 0x0001, SHA256 = 0x0002, etc.)
  compression_algo: u8     (None = 0x00, Zstd = 0x01)
  encryption_algo:  u8     (None = 0x00, reserved for future use)
  key_length:       u32    (length of the key field)
  value_length:     u32    (length of the value field)
  timestamp:        i64    (UTC milliseconds since epoch)
  total_length:     u32    (total bytes including header, for jump-scanning)
  hash:             [u8; N] (integrity hash, N determined by hash_algo)

[Key - key_length bytes]
[Value - value_length bytes]
```

Key properties:
- **`magic`** (0x0AE012DB) enables recovery scanning -- find entry boundaries even in a corrupted file by scanning for magic bytes
- **`total_length`** enables jump-scanning -- skip to the next entry without reading the full key/value
- **`hash`** covers `entry_type + key + value` -- re-hash and compare to detect corruption
- **`entry_version`** enables format evolution -- the engine selects the correct parser based on this byte

For BLAKE3-256 (the default), the hash is 32 bytes, making the full header 63 bytes.

## Content-Addressed Hashing

Every piece of data is identified by its BLAKE3 hash. Hash inputs are prefixed by type (domain separation) to prevent collisions between different entry types:

| Entry Type | Hash Input | Example |
|------------|-----------|---------|
| Chunk | `chunk:` + raw bytes | `BLAKE3("chunk:" + file_bytes)` |
| FileRecord (path key) | `file:` + path | `BLAKE3("file:/users/alice.json")` |
| FileRecord (content key) | `filec:` + serialized record | `BLAKE3("filec:" + record_bytes)` |
| DirectoryIndex (path key) | `dir:` + path | `BLAKE3("dir:/users/")` |
| DirectoryIndex (content key) | `dirc:` + serialized data | `BLAKE3("dirc:" + dir_bytes)` |

The domain prefix ensures that a chunk's raw data can never produce the same hash as a file path, even if the bytes are identical.

## Chunking

Files are split into 256KB chunks for storage. Each chunk is content-addressed independently:

```
Original file (700KB):
  [Chunk 1: 256KB] -> hash_a
  [Chunk 2: 256KB] -> hash_b
  [Chunk 3: 188KB] -> hash_c

FileRecord:
  path: "/docs/report.pdf"
  chunk_hashes: [hash_a, hash_b, hash_c]
  total_size: 700KB
```

Chunking provides:
- **Deduplication**: Two files sharing identical 256KB blocks store those blocks only once
- **Efficient updates**: Modifying 3 bytes of a 10GB file creates one new chunk, not a new copy of the entire file
- **Streaming reads**: Read a file by iterating its chunk hashes and fetching each chunk

## Dual-Key FileRecords

FileRecords are stored at two keys to support both current reads and historical versioning:

1. **Path key** (`file:/path`) -- mutable, always points to the latest version. Used for reads, metadata, indexing, and deletion. O(1) lookup.

2. **Content key** (`filec:` + serialized record) -- immutable, content-addressed. The directory tree's `ChildEntry.hash` points to this key.

When the version manager walks a snapshot's directory tree, it follows `ChildEntry.hash` to the content key, which resolves to the FileRecord as it existed at snapshot time -- not the current version. This is what makes historical reads correct.

Directories use the same pattern: `dir:/path` (mutable) and `dirc:` + data (immutable content key).

## FileRecord Format

```
[FileRecord Value]
  path_length:      u16
  path:             [u8; path_length]     (full file path)
  content_type_len: u16
  content_type:     [u8; content_type_len] (MIME type)
  total_size:       u64                    (file size in bytes)
  created_at:       i64                    (UTC milliseconds)
  updated_at:       i64                    (UTC milliseconds)
  metadata_length:  u32
  metadata:         [u8; metadata_length]  (arbitrary JSON metadata)
  chunk_count:      u32
  chunk_hashes:     [u8; chunk_count * 32] (ordered BLAKE3 hashes)
```

Metadata fields come first so you can read file metadata without skipping past the chunk list. Chunk hashes are the tail of the record for streaming reads.

## Directory Propagation

When a file is stored or deleted, the change propagates up the directory tree:

```
Store /users/alice.json:

1. Store chunks -> [hash_a, hash_b]
2. Store FileRecord at path key + content key
3. Update /users/ DirectoryIndex (new ChildEntry for alice.json)
4. Update / root DirectoryIndex (new ChildEntry for users/)
5. Update HEAD in file header (new root hash)
```

Each directory gets a new content hash because one of its children changed. This chain of updates from leaf to root is what maintains the Merkle tree and makes versioning work.

## Void Management

When garbage collection reclaims an entry, the bytes the entry occupied become a **void** -- a region of the WAL marked as reclaimable. Voids are tracked entirely in memory by the `VoidManager`, which keeps two parallel indexes:

```
by_offset:  BTreeMap<u64, u32>            // offset → size, ordered iteration
by_size:    BTreeMap<u32, BTreeSet<u64>>  // size → set of offsets, best-fit lookup
```

Voids are **never** written into the WAL as their own records. They live in memory while the process runs and are persisted by riding along inside the [hot tail](#hot-tail) on every periodic flush, so the next clean startup restores the void set without scanning the file. On a dirty startup (hot tail unreadable), voids are re-derived via a **gap scan** of the rebuilt KV: any byte range not covered by a live KV entry between `kv_block_end` and the WAL tail is registered as a void.

When a new entry needs to be written, the engine calls `VoidManager::find_void(needed)` before appending to the tail. If a void of sufficient size exists, the entry is written **in-place** at that void's offset; otherwise the entry appends. If the chosen void is larger than the entry, the remainder is re-registered as a smaller void (when it is at least 63 bytes -- the minimum useful void size for BLAKE3-256: 31-byte fixed header + 32-byte hash + 0-byte key + 0-byte value).

Two size floors govern void tracking:

| Constant | Default | Meaning |
|---|---|---|
| `MINIMUM_VOID_SIZE` | 1 byte | Below this, voids are discarded entirely -- treated as alignment noise |
| `MINIMUM_USEFUL_VOID_SIZE` | 63 bytes | Below this, voids are tracked (for metrics and fragmentation visibility) but never returned by `find_void` -- no real entry would fit |

## Compression

Compression is a post-hash transform:

```
Write: raw data -> hash -> compress -> store
Read:  load -> decompress -> verify hash -> return
```

The hash is always computed on the raw uncompressed data. This preserves deduplication (same content = same hash regardless of compression) and integrity verification.

Each entry carries its own `compression_algo` byte, so compressed and uncompressed entries coexist in the same file. Currently, zstd is the only supported compression algorithm.

## On-Disk Layout: Single File

A complete AeorDB lives in one `.aeordb` file:

```
┌──────────────────────────────────────────────────────────────┐
│  File Header Slot A (256 bytes) — magic + version + CRC      │
├──────────────────────────────────────────────────────────────┤
│  File Header Slot B (256 bytes) — magic + version + CRC      │
├──────────────────────────────────────────────────────────────┤
│  KV Block — bucket pages with per-page magic + CRC           │
│  (stages: 64 KB → 512 KB → 4 MB → 32 MB → 128 MB → …)        │
├──────────────────────────────────────────────────────────────┤
│  WAL — entries appended forward (chunks, file records,       │
│  directory indexes, snapshots, …)                            │
├──────────────────────────────────────────────────────────────┤
│  Hot Tail — magic + count + CRC + recent KV entries          │
└──────────────────────────────────────────────────────────────┘
```

No sidecar files. No journal directory. The single file is the entire database.

The **file header** lives in two slots: slot A at byte 0, slot B at byte 256. Each carries a u64 `sequence`, the header fields, a `format_magic = "AEORDB\0\0"` byte string, a `format_version: u8`, and a u32 CRC over the slot. Writers update whichever slot has the lower sequence (increment then write, then fsync). Readers parse both and pick the highest sequence with a valid CRC. A torn write on one slot leaves the other intact — the database always opens.

On open, if `format_magic` is wrong the engine refuses with "not an AeorDB file". If `format_version` doesn't match what this build understands, the engine refuses with a clear "DB format vN, this build expects vM" message. Format-breaking changes must bump the version.

The **KV block** at the head is a flat bucket array — `hash → entry offset` mappings indexed by a normalized vector table (NVT). Each bucket page carries `[magic: u32][crc32: u32][entry_count: u16][entries…]`. Lookups validate magic + CRC before scanning. If a page fails validation, the engine performs a **per-bucket rebuild**: using the NVT to identify which hashes belong to that bucket, it scans the WAL for those entries and reconstructs the page. The WAL is the source of truth; bucket pages are a recoverable index.

The **WAL** grows forward from the end of the KV block. New entries are appended.

The **hot tail** at EOF is a short journal of KV entries that haven't been flushed into the bucket pages yet. On startup we read the hot tail first, populating the in-memory write buffer with the most recent unflushed entries.

## Online KV Expansion

When the KV bucket pages fill, AeorDB grows the KV block to the next stage **while the database is running** — no restart, no downtime, no rebuild.

The expansion holds both writer and KV locks for the duration of the relocation, then releases them. Reads and writes that complete before or after the expansion proceed normally; writes during the expansion queue on the lock.

Stage progression (BLAKE3, page size 1,314 bytes):

| Stage | Block size | Buckets | Capacity (~) |
|-------|-----------|---------|-------------|
| 0 | 64 KB | 49 | 1,500 entries |
| 1 | 512 KB | 399 | 12,000 |
| 2 | 4 MB | 3,192 | 96,000 |
| 3 | 32 MB | 25,532 | 768,000 |
| 4 | 128 MB | 102,130 | 3,000,000 |

Growth is geometric early (8× per stage) then linear (4×, 2×) to keep relocation costs bounded.

### How it works (atomically)

1. Set `resize_in_progress = true` and `resize_target_stage` in the file header.
2. Scan forward from the new KV-block boundary to find the first WAL entry that starts AFTER it. This is the actual end of the growth zone — entries straddling the boundary must be included so the relocated copy is complete.
3. Bulk-copy the growth zone `[old_kv_end .. actual_copy_end]` to the end of the WAL (right before the hot tail). This temporarily produces two copies — the original (still readable) and the relocated copy. Crash-safe window.
4. fsync.
5. Tell the KV store to finalize: zero the new bucket pages, rehash all entries (entries that lived in the growth zone now point to their relocated offsets), update the in-memory state, write the file header with the new stage.
6. Write a Void entry over the dead tail at the old growth-zone boundary so the entry scanner stays clean on restart.

If the process crashes between steps 2 and 5, the original WAL data is intact and the next startup detects `resize_in_progress = true` in the header. The expansion resumes from the relocated copy.

## Hot Tail

The hot tail is a small, versioned journal at the end of the file. It carries two kinds of transient state that would otherwise be lost on crash:

1. **Pending KV writes** — entries that exist in the in-memory write buffer but haven't been flushed to bucket pages yet.
2. **Void snapshot** — the current `VoidManager` state, so the next clean startup restores reclaimable-space tracking without rescanning the WAL.

```
[Header — 21 bytes]
  magic:           [u8; 5] = AE 01 7D B1 0D
  format_version:  u8                 (top-level layout version; bumped on section changes)
  write_count:     u32                (number of write records below)
  void_count:      u32                (number of void records below)
  header_crc32:    u32                (CRC32 of the preceding 14 bytes)

[Write records — 1 + hash_length + 13 bytes each (42 for BLAKE3-256)]
  version:      u8        (per-record layout version; bumped without a full format bump)
  hash:         [u8; hash_length]
  type_flags:   u8
  offset:       u64                   (WAL position of the actual entry)
  total_length: u32                   (on-disk length of the entry)

[Void records — 13 bytes each]
  version:      u8
  offset:       u64                   (start of the reclaimable region)
  size:         u32                   (length of the region)
```

The hot tail is the durability boundary for both the KV and the void set. Periodic flushes (every 100 ms or whenever the write buffer hits its threshold) rewrite the hot tail in-place at `header.hot_tail_offset`. On the next clean startup the header is parsed, the write records reload into the write buffer, and the void records repopulate the `VoidManager` -- all before any read serves traffic.

If the hot tail's header CRC fails, the magic doesn't match, or the recorded offset points past the file boundary, the engine logs a warning and triggers a **dirty startup**: a full WAL scan (via `scan_entries_dirty_recovery`) rebuilds the KV from scratch and `recover_voids_via_gap_scan` re-derives the void set from gaps in the rebuilt KV. No data is lost -- the WAL is the source of truth; the hot tail is a fast-path index plus a void snapshot.

A magic-byte version bump is enough on its own to invalidate older hot tails: the next open with newer code sees a magic mismatch, falls into dirty startup, and rebuilds correctly. That makes the format safely evolvable without a migration tool.

## fsync Strategy

Not all entries are equally important for durability:

| Data | fsync | Rationale |
|------|-------|-----------|
| Chunks, FileRecords, DeletionRecords | Immediate | The truth -- not rebuildable from other data |
| KV store, NVT, DirectoryIndex, Snapshots | Deferred | Derived data -- can be rebuilt from a full entry scan |

This gives durability where it matters and performance where it doesn't.

## Crash Recovery

The recovery hierarchy, from least to most damage:

| What's Lost | Recovery Method |
|-------------|-----------------|
| Nothing | Read HEAD from KV store, load directory index, ready |
| One file-header slot torn | Read the other slot (highest valid sequence + CRC wins) |
| One KV bucket page corrupted | Per-bucket rebuild from WAL, bounded work |
| Hot tail torn | Dirty startup — full WAL scan rebuilds the KV from scratch, then `recover_voids_via_gap_scan` re-derives the void set from gaps in the rebuilt KV |
| KV store only | Entry-by-entry scan, rebuild KV store, load latest directory index |
| Directory index only | Scan FileRecords + DeletionRecords, reconstruct from paths + timestamps |
| KV store + directory | Full entry scan, rebuild KV, reconstruct directory |
| Only chunks + FileRecords survive | Full data recovery, version history reconstructed via DeletionRecords |

The magic bytes at the start of every entry enable boundary detection even in partially corrupted files. The `total_length` field in each header enables efficient forward scanning.

## Next Steps

- [Architecture](./architecture.md) -- high-level system overview
- [Versioning](./versioning.md) -- snapshots, forks, and the Merkle tree
