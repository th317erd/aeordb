# Custom Storage Engine — The AeorDB WAL-Filesystem

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design — Final Architecture

Replaces redb entirely. Append-only WAL that IS the filesystem.

---

## Design Philosophy

- **The data file IS the WAL.** Every write appends. The log IS the database.
- **Zero wasted space.** No allocator, no pages, no power-of-two rounding. Just packed entries.
- **Recovery from raw data.** Even with total index loss, the entity-by-entity scan rebuilds everything.
- **Versioning is free.** The entity log IS the version history. Deletion markers make it complete.
- **Everything is an entry.** Chunks, files, directories, indexes, voids — all the same primitive.

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
  timestamp:     i64          // millisecond precision, when written
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
- **For chunks**: the key IS `BLAKE3("chunk:" + value)`, so verification is inherent.
- **`timestamp`** enables version reconstruction by temporal ordering.
- **`entry_type`** identifies what kind of entity this is, enabling recovery even without the KV store.

---

## File Layout

```
aeordb.dat:

[File Header — 128 bytes]
  magic:            "AEOR" (4 bytes)
  format_version:   u8
  created_at:       i64 (ms)
  updated_at:       i64 (ms)
  kv_block_offset:  u64          ← current NVT + KV block location
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
  Appended sequentially. Never modified in place (except void reuse).
```

### NVT + KV Block at Front of File

The NVT and KV block live at the front of the file (after the header). When they need to grow:

1. Relocate chunks that are in the way:
   a. Read the chunk data
   b. Find a void of sufficient size (or append to end of file)
   c. Write the chunk to its new location
   d. Update the KV store entry for that chunk hash → new offset
   e. Mark the old location as a void
2. Expand the NVT/KV block into the freed space
3. Update the header pointers

**The data stays tight.** Relocated chunks fill voids or go to the end. The NVT/KV block grows in place. No dead space accumulates.

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
- **Tiny memory footprint**: 1024 buckets × 16 bytes = 16 KB. 1M buckets × 16 bytes = 16 MB. Even at very high resolution, fits in memory.

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

At 1 billion chunks with 16M buckets, average scan is ~60 entries. Each entry is 40 bytes (32-byte hash + 8-byte offset), so scanning 60 entries = reading 2.4 KB. Sub-millisecond on any storage.

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

Sorted by hash. The NVT points into sections of this array for fast lookups.

### Size

| Chunks | KV Block Size |
|---|---|
| 1,000,000 | 40 MB |
| 100,000,000 | 4 GB |
| 1,000,000,000 | 40 GB |

At 1 billion chunks, the KV block is 40 GB. This is stored on disk, not in memory. The NVT (in memory) tells you where to seek in the KV block.

### Updates

New chunks aren't immediately added to the KV block (that would require rewriting it). Instead:

1. New entries go to an **overflow area** (appended after the KV block)
2. The overflow is unsorted — lookups scan it linearly
3. When the overflow exceeds a threshold, **merge** it into the KV block (rewrite the KV block with the overflow entries merged in)
4. Update the NVT to reflect the new KV block layout

This is a simplified two-level LSM: KV block (sorted, indexed by NVT) + overflow (unsorted, scanned).

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
  // Iterate known sizes from smallest-sufficient upward
  for size in SIZES.iter().filter(|&&s| s >= needed_size) {
    let key = blake3(format!("::aeordb:void:{size}"));
    if let Some(void_entry) = kv_store.get(&key) {
      // Pop an offset from this void's list
      let offset = void_entry.pop_offset();
      return Some((offset, size));
    }
  }
  None // No void available, append to end of file
}
```

### Void Creation

Voids are created at known moments — never discovered by scanning:
- **Chunk relocation**: old location becomes a void of known size
- **KV block rewrite**: old KV block area becomes a void
- **NVT rewrite**: old NVT area becomes a void

### Best-Fit with Splitting

If a void is larger than needed:
1. Write the entry at the void's offset
2. Leftover space ≥ minimum useful size (e.g., 89 bytes for the smallest possible entry): create a new void of the remainder size
3. Leftover < minimum: waste it (tiny, not worth tracking)

### Size Pool

Void sizes naturally cluster around chunk sizes (power-of-two configurable). Odd-sized voids from FileRecords and metadata are small and infrequent. The void lookup iterates at most a handful of sizes.

---

## Versioning

### The Entity Log IS the Version History

Every FileRecord, DeletionRecord, and Snapshot has a timestamp. Replaying the log in order from any starting point gives the exact filesystem state at any time.

### Creating a Version

```
1. Serialize the current directory index (Tier 1 file index)
2. Append as a Snapshot entity with a name and timestamp
3. Done — the snapshot is now in the log
```

### Restoring a Version

```
1. Find the Snapshot entity by name in the KV store
2. Deserialize the directory index from it
3. Load the current NVT + KV block (these are version-independent)
4. The database now shows the state at that snapshot
```

### Reconstructing Version History (after index loss)

```
1. Scan all FileRecords and DeletionRecords
2. Sort by timestamp
3. Replay forward:
   - FileRecord → add/update file at path
   - DeletionRecord → remove file at path
4. At each Snapshot timestamp, the accumulated state matches that version
```

DeletionRecords are what make this complete. Without them, you'd see deleted files reappear when scanning backward.

---

## Concurrency

### Current: Single Writer + Multiple Readers

- Writer holds a mutex, appends entries, updates index
- Readers snapshot the in-memory file index (Arc + RwLock)
- Readers see a consistent point-in-time view

### Future: Coordinator + Parallel Writers

- Coordinator thread reserves space by writing entry headers
- Worker threads fill in chunk data in parallel
- Coordinator fsyncs and updates indexes
- The append-only design naturally supports this — reserved space is pre-allocated, workers backfill

---

## FileRecord Format

A FileRecord's value contains:

```
[FileRecord Value]
  path_length:     u32
  path:            [u8; path_length]     // the full file path
  content_type_len: u32
  content_type:    [u8; content_type_len] // MIME type (optional, 0 if none)
  total_size:      u64                    // total file size in bytes
  chunk_count:     u32                    // number of chunks
  chunk_hashes:    [u8; chunk_count * 32] // ordered BLAKE3 hashes
  created_at:      i64                    // file creation timestamp
  updated_at:      i64                    // file modification timestamp
  metadata_length: u32                    // additional metadata
  metadata:        [u8; metadata_length]  // arbitrary key-value metadata (JSON)
```

The chunk_hashes list is the ordered sequence of chunks composing the file. To read the file: for each hash, look it up in the KV store → get offset → read chunk data → yield to caller.

---

## DeletionRecord Format

```
[DeletionRecord Value]
  path_length:     u32
  path:            [u8; path_length]     // the deleted file path
  deleted_at:      i64                   // when it was deleted
  reason_length:   u32
  reason:          [u8; reason_length]   // optional deletion reason
```

---

## Comparison to redb

| Aspect | redb | Custom Engine |
|---|---|---|
| Allocation | Buddy (power-of-two rounding) | None (append-only) |
| Storage overhead | 2-8x | ~0.02% (entry headers only) |
| Crash recovery | COW + dual commit slots | Entity scan + checksum verification |
| Versioning | Persistent savepoints | Entity log replay + DeletionRecords |
| Index | On-disk B-tree | NVT + sorted KV block + overflow |
| Concurrency | Single writer + MVCC readers | Single writer + snapshot readers (future: parallel) |
| Code size | ~21K lines | ~2-3K lines estimated |
| Free space reuse | Buddy allocator | Void entities in KV store |

---

## Implementation Order

```
Task 1: Entry format + append writer + reader
Task 2: In-memory file index (Tier 1) + entity scanner
Task 3: NVT + KV block (on-disk hash lookup)
Task 4: Void management
Task 5: ChunkStorage trait implementation (drop-in replacement)
Task 6: Directory operations
Task 7: FileRecord + DeletionRecord handling
Task 8: Versioning (snapshots + restore)
Task 9: Wire to HTTP layer + update tests
Task 10: Stress test + benchmarks
```

Tasks 1-3 are the foundation. Task 5 is the integration point. Tasks 6-8 build the filesystem. Task 9 connects everything.

---

## Open Questions

- [ ] Overflow merge threshold (how big before merging into KV block?)
- [ ] NVT resolution growth strategy (double? based on avg scan length?)
- [ ] fsync batching strategy (per-write now, optimize later?)
- [ ] Maximum single-file database size (limited by u64 offsets = 16 exabytes, effectively unlimited)
- [ ] Minimum void size worth tracking (89 bytes? smaller?)
