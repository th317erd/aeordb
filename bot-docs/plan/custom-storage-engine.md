# Custom Storage Engine — The AeorDB WAL-Filesystem

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design — Final Architecture (Revision 2)

Replaces redb entirely. Append-only WAL that IS the filesystem.

---

## Design Philosophy

- **The data file IS the WAL.** Every write appends. The log IS the database.
- **Zero wasted space.** No allocator, no pages, no power-of-two rounding. Just packed entries.
- **Recovery from raw data.** Even with total index loss, the entity-by-entity scan rebuilds everything.
- **Versioning is free.** The entity log IS the version history. Deletion markers make it complete.
- **Everything is an entry.** Chunks, files, directories, indexes, voids — all the same primitive.
- **Timestamps are always UTC.** Millisecond precision, Unix epoch, no timezone ambiguity.

---

## Three Layers

```
Layer A: NVT + KV Store (the "master index")
  Maps hash → offset for ALL entities in the database
  Fast sub-ms lookups via Normalized Vector Table
  Also stores HEAD pointer, version history pointers
  Can be fully rebuilt from Layers B and C

Layer B: Directory Index / Filesystem Structure
  The "current state" — which files exist at which paths
  THIS is the versioning mechanism — each version is a snapshot of this layer
  Points to FileRecords via their hashes
  Can be rebuilt from FileRecords + DeletionRecords + timestamps

Layer C: FileRecords + Chunks (the raw data)
  FileRecords: file metadata + ordered list of chunk hashes
  Chunks: raw data blocks, named by content hash
  The foundation — if this survives, EVERYTHING can be recovered
```

### Recovery Hierarchy

| What's lost | Recovery |
|---|---|
| Nothing | Read HEAD from KV store → load directory index → ready |
| KV store only | Entity-by-entity scan → rebuild KV store → load latest directory index |
| Directory index only | Scan FileRecords + DeletionRecords → reconstruct from paths + timestamps |
| KV store + directory | Full entity scan → rebuild KV from chunks/FileRecords → reconstruct directory |
| Only chunks + FileRecords survive | Full data recovery, version history reconstructed via DeletionRecords |

### fsync Strategy — Tiered

- **Immediately fsync:** Chunks, FileRecords, DeletionRecords, VoidRecords (the truth — not rebuildable)
- **Deferred fsync:** KVS, NVT, DirectoryIndex, Snapshots (derived data — rebuildable if lost)

This gives durability where it matters and performance where it doesn't.

---

## Entity Types

Six entity types. Every entity has the same header format.

| Type ID | Name | Key | Value | Purpose |
|---|---|---|---|---|
| 0x01 | Chunk | BLAKE3("chunk:" + data) | raw data bytes | File content storage |
| 0x02 | FileRecord | BLAKE3("file:" + path) | metadata + ordered chunk hash list | File composition |
| 0x03 | DeletionRecord | BLAKE3("del:" + path + ":" + timestamp) | deleted path + metadata | Version history completeness |
| 0x04 | Snapshot | BLAKE3("snap:" + name) | serialized directory index | Fast startup (optimization) |
| 0x05 | Void | BLAKE3("::aeordb:void:" + size) | list of offsets with available space | Free space reuse |
| 0x06 | KVEntry | varies | varies | KV store internal entries |

### Domain-Prefixed Hashing

Hash inputs are prefixed by type to guarantee no collisions:

```
Chunk:      BLAKE3("chunk:" + raw_data)
File:       BLAKE3("file:" + path_string)
Directory:  BLAKE3("dir:" + path_string)
Deletion:   BLAKE3("del:" + path + ":" + timestamp_ms)
System:     BLAKE3("::aeordb:..." + system_key)
```

A chunk's raw data can never collide with a file path hash because the prefixes differ.

---

## Entry Format (On-Disk)

Every entity on disk has the same header structure:

```
[Entry Header — 89 bytes]
  entry_type:    u8           // 0x01-0x06
  flags:         u8           // reserved
  key_length:    u32          // length of key field
  value_length:  u32          // length of value field
  timestamp:     i64          // millisecond precision UTC, when written
  hash:          [u8; 32]     // BLAKE3 of (entry_type + key + value) — integrity check
  total_length:  u32          // total bytes of this entry including header (for jump-scanning)
  reserved:      [u8; 6]      // future use

[Key]
  [u8; key_length]

[Value]
  [u8; value_length]
```

### Key Properties

- **`total_length`** enables entity-by-entity scanning: read header → jump `total_length` bytes → next header. Not byte-by-byte.
- **`hash`** covers `(entry_type + key + value)`. On read, re-hash and compare. Mismatch = corrupt entry.
- **For chunks**: the key IS `BLAKE3("chunk:" + value)`, so verification is inherent. No separate CRC needed.
- **`timestamp`** always UTC milliseconds. Enables version reconstruction by temporal ordering.
- **`entry_type`** identifies what kind of entity this is, enabling recovery even without the KV store.

---

## File Layout

```
aeordb.dat:

[File Header — 128 bytes]
  magic:            "AEOR" (4 bytes)
  format_version:   u8
  created_at:       i64 (ms, UTC)
  updated_at:       i64 (ms, UTC)
  kv_block_offset:  u64          ← current KV block location
  kv_block_length:  u64
  nvt_offset:       u64          ← current NVT location
  nvt_length:       u64
  head_hash:        [u8; 32]     ← hash of the current directory index (HEAD)
  entry_count:      u64          ← total entities written (for stats)
  reserved:         [u8; remaining to 128]

[NVT — Normalized Vector Table]
  Immediately after header. Grows by relocating chunks.

[KV Block — Sorted array of (hash, offset) pairs]
  After NVT. Grows by relocating chunks.

[Entries...]
  Chunks, FileRecords, DeletionRecords, Snapshots, Voids...
  Appended sequentially. Voids may be reused by new writes.
```

### NVT + KV Block at Front of File

The NVT and KV block live at the front of the file (after the header). When they need to grow:

1. Relocate chunks that are in the way:
   a. Read the chunk data
   b. Find a void of sufficient size (or append to end of file)
   c. Write the chunk to its new location
   d. Update the KV store entry for that chunk hash → new offset
2. Expand the NVT/KV block into the freed space (no void created — the space is consumed by growth)
3. Update the header pointers

**The data stays tight.** Relocated chunks fill voids or go to the end. The NVT/KV block grows in place.

### KV Resize Mode — Temporary Buffer KVS

When the KV block needs to grow, the database enters a brief "resize mode":

```
1. Enter resize mode
2. Spin up a temporary buffer KVS+NVT (small, fast, located at end of file)
3. New writes go to buffer KVS only (no writes to primary during resize)
4. Reads check buffer first, fall back to primary
5. Meanwhile: grow the primary KVS (relocate chunks, expand space)
6. When primary is grown: merge buffer contents into primary
7. Discard buffer, mark its space as a Void
8. Exit resize mode
```

Advantages:
- Writes never block (buffer always available)
- Resize is a background operation
- Dual lookups only exist during the brief resize window
- Buffer is temporary and small — discarded after merge
- No risk of data loss — buffer contents merged into primary before discard

---

## NVT — Normalized Vector Table

The NVT provides fast hash → bucket lookups with self-correcting resolution.

### How It Works

```
1. Normalize the hash to a scalar: f(hash) = first_8_bytes_as_u64 / u64::MAX → [0.0, 1.0]
2. Map the scalar to a bucket: bucket_index = floor(scalar * num_buckets)
3. The bucket contains: (kv_block_offset, entry_count) for that range of hashes
4. Look up in the KV block at that offset, scan within the bucket
```

### Properties

- **Uniform distribution guaranteed**: BLAKE3 hashes are uniformly distributed. No hot spots.
- **Configurable resolution**: more buckets = smaller scans per lookup. Start at 1024, grow as data grows.
- **Self-correcting**: when a scan finds the exact entry, update the bucket boundary to be more precise. Over time, hot buckets converge to near-exact offsets.
- **Tiny memory footprint**: 1024 buckets × 16 bytes = 16 KB. 1M buckets × 16 bytes = 16 MB.
- **Natural sorting**: the NVT organizes entries in scalar order, which provides sorted access for free.
- **Growth strategy**: double bucket count when average scan length exceeds threshold. Exact threshold determined by stress testing.

### NVT Entry Format

```
struct NVTBucket {
  kv_block_offset: u64,   // where this bucket's entries start in the KV block
  entry_count: u32,       // how many KV entries in this bucket
  reserved: u32,          // future use
}
```

### Scaling

| Chunks in DB | NVT Buckets | NVT Size | Avg Scan per Lookup |
|---|---|---|---|
| 10,000 | 1,024 | 16 KB | ~10 entries |
| 1,000,000 | 65,536 | 1 MB | ~15 entries |
| 100,000,000 | 1,048,576 | 16 MB | ~95 entries |
| 1,000,000,000 | 16,777,216 | 256 MB | ~60 entries |

---

## KV Block — Sorted Hash → Offset Array

The KV block is a flat sorted array of entries:

```
struct KVEntry {
  hash: [u8; 32],    // the entity's hash (its key in the KV store)
  offset: u64,       // byte offset in the data file where the entity lives
}
// 40 bytes per entry
```

Sorted by hash (the NVT's scalar ordering provides the sort naturally).

### Size

| Chunks | KV Block Size |
|---|---|
| 1,000,000 | 40 MB |
| 100,000,000 | 4 GB |
| 1,000,000,000 | 40 GB |

Stored on disk, not in memory. The NVT (in memory) tells you where to seek.

---

## Void Management

Voids are free space created by chunk relocations or KV block rewrites. They are tracked as Void entities in the KV store.

### Void Files by Size

Each distinct void size has its own Void entity:

```
Key:   BLAKE3("::aeordb:void:262144")
Value: list of file offsets where 262144-byte voids exist
```

### Finding a Void for a New Write

```rust
fn find_void(needed_size: u32) -> Option<(u64, u32)> {
  for size in SIZES.iter().filter(|&&s| s >= needed_size) {
    let key = blake3(format!("::aeordb:void:{size}"));
    if let Some(void_entry) = kv_store.get(&key) {
      return Some((offset, size));
    }
  }
  None // No void available, append to end of file
}
```

### Void Creation

Voids are created at known moments — never discovered by scanning:
- **Chunk relocation**: old location becomes a void of known size
- **KV block rewrite**: old KV block area becomes a void (if any space remains after growth)
- **File update**: old FileRecord space becomes a void

### Best-Fit with Splitting

If a void is larger than needed:
1. Write the entry at the void's offset
2. Leftover ≥ minimum useful size (89 bytes — smallest entry header): create a new void of the remainder
3. Leftover < minimum: leave it (tiny, not worth tracking)

---

## FileRecord Format (Revised)

Metadata fields first, chunk list last — so you can read metadata without skipping past chunks.

```
[FileRecord Value]
  path_length:      u16                    // max 65,535 bytes (more than enough)
  path:             [u8; path_length]      // the full file path
  content_type_len: u16                    // MIME type length
  content_type:     [u8; content_type_len] // MIME type (optional, 0 if none)
  total_size:       u64                    // total file size in bytes
  created_at:       i64                    // file creation timestamp (UTC ms)
  updated_at:       i64                    // file modification timestamp (UTC ms)
  metadata_length:  u32                    // additional metadata length
  metadata:         [u8; metadata_length]  // arbitrary key-value metadata (JSON)
  chunk_count:      u32                    // number of chunks
  chunk_hashes:     [u8; chunk_count * 32] // ordered BLAKE3 hashes (THE BODY — last field)
```

Chunk hashes are the tail of the record. To stream a file: read the FileRecord, skip to `chunk_hashes`, iterate the hashes, look up each in the KV store.

---

## DeletionRecord Format

```
[DeletionRecord Value]
  path_length:     u16                    // max 65,535 bytes
  path:            [u8; path_length]      // the deleted file path
  deleted_at:      i64                    // when it was deleted (UTC ms)
  reason_length:   u16                    // optional deletion reason length
  reason:          [u8; reason_length]    // optional deletion reason
```

---

## Versioning

### The Entity Log IS the Version History

Every FileRecord and DeletionRecord has a timestamp. Replaying the log in order from any starting point gives the exact filesystem state at any time.

### Creating a Version

```
1. Serialize the current directory index
2. Append as a Snapshot entity with a name and timestamp
3. Done — the snapshot is now in the log
```

### Restoring a Version

```
1. Find the Snapshot entity by name in the KV store
2. Deserialize the directory index from it
3. The database now shows the state at that snapshot
```

### Reconstructing Version History (after index loss)

```
1. Scan all FileRecords and DeletionRecords (entity-by-entity, not byte-by-byte)
2. Sort by timestamp
3. Replay forward:
   - FileRecord → add/update file at path
   - DeletionRecord → remove file at path
4. At any timestamp, the accumulated state is the directory index at that moment
```

DeletionRecords are what make this complete — without them, deleted files would reappear when scanning backward.

### No Compaction

With versioning, nothing is ever truly "dead." Old versions reference old chunks. Only explicit admin actions (purging old versions) could make chunks unreferenced. For now, compaction is not needed. The file only grows. This is acceptable and intentional.

---

## Concurrency

### Current: Single Writer + Multiple Readers

- Writer holds a mutex, appends entries, updates index
- Readers snapshot the in-memory file index (Arc + RwLock)
- Readers see a consistent point-in-time view

### Future: Coordinator + Parallel Writers

- Coordinator thread reserves space by writing entry headers with pre-allocated sizes
- Worker threads fill in chunk data in parallel into reserved slots
- Coordinator fsyncs and updates indexes
- Append-only design naturally supports this — reserved space is pre-allocated, workers backfill

---

## Comparison to redb

| Aspect | redb | Custom Engine |
|---|---|---|
| Allocation | Buddy (power-of-two rounding) | None (append-only + void reuse) |
| Storage overhead | 2-8x | ~0.02% (entry headers only) |
| Crash recovery | COW + dual commit slots | Entity scan + BLAKE3 verification |
| Versioning | Persistent savepoints | Entity log replay + DeletionRecords |
| Index | On-disk B-tree | NVT + sorted KV block |
| Concurrency | Single writer + MVCC readers | Single writer + snapshot readers (future: parallel) |
| Code size | ~21K lines | ~2-3K lines estimated |
| Free space reuse | Buddy allocator | Void entities via deterministic hashes |

---

## Implementation Order

```
Task 1: Entry format + append writer + reader (header, serialize, deserialize, scan)
Task 2: NVT (normalized vector table — in-memory, self-correcting)
Task 3: KV block (sorted on-disk hash→offset array, indexed by NVT)
Task 4: KV resize mode (temporary buffer KVS during growth)
Task 5: Void management (deterministic void hashes, best-fit reuse)
Task 6: ChunkStorage trait implementation (drop-in replacement for redb)
Task 7: FileRecord + DeletionRecord handling
Task 8: Directory operations + path resolver (on new engine)
Task 9: Versioning (snapshots + restore + log replay)
Task 10: Wire to HTTP layer + auth + plugins (replace redb everywhere)
Task 11: Stress test + benchmarks (compare to redb baseline)
```

Tasks 1-3 are the foundation. Task 6 is the integration point. Tasks 7-9 build the filesystem. Task 10 connects everything. Task 11 proves it.

---

## Resolved Decisions

- **Timestamps**: Always UTC milliseconds. No timezone ambiguity.
- **Integrity**: BLAKE3 for everything. No CRC32. Hash IS the integrity check.
- **Path lengths**: u16 (65,535 bytes max — more than sufficient).
- **FileRecord layout**: Metadata first, chunk hashes last (streaming-friendly).
- **fsync**: Immediately for truth entities (chunks, file records, deletions). Deferred for derived data (KVS, NVT, snapshots).
- **Single file**: Everything in one .aeor file. Index stored as entries in the same file.
- **No compaction**: File only grows. Old versions keep old chunks alive. This is intentional.
- **KV resize**: Temporary buffer KVS during resize operations. Discarded after merge.
- **NVT growth**: Double buckets when scan length exceeds threshold. Exact threshold via stress testing.
- **Minimum void**: 89 bytes (smallest possible entry header). Smaller gaps are abandoned.
