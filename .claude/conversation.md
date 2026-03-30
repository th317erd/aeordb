# AeorDB — Sprint 4: Custom Storage Engine (Revised)

Incorporating all of Wyatt's feedback. Previous version's flaws corrected.

---

## Core Design: Append-Only WAL-Filesystem

The data file IS the WAL. Every write appends. The index is a file stored in the same data file. Recovery is always possible by scanning the raw data.

### Terminology (cleaned up)
- **Entry**: A record appended to the data file (has a header + payload)
- **Chunk**: An entry containing raw file data, keyed by BLAKE3 hash
- **File Record**: An entry containing a file's metadata + ordered list of chunk hashes
- **Directory Record**: An entry containing a directory's metadata + list of child entries
- **Snapshot**: An entry containing a serialized index (for fast startup)
- **NO "pages"**. No allocation units. Just packed entries.

---

## File Layout (Revised)

```
aeordb.dat:

[File Header — 64 bytes]
  magic: "AEOR" (4 bytes)
  format_version: u8
  created_at: i64 (ms)
  updated_at: i64 (ms)
  reserved: [u8; remaining to 64]

[Entry 0]
  entry_header:
    entry_type: u8
    flags: u8 (reserved)
    key_length: u32
    value_length: u32
    group_id: [u8; 32]          ← links chunk back to its parent file (BLAKE3 of file path or file record hash)
    timestamp: i64 (ms)
    hash: [u8; 32]              ← BLAKE3 of (entry_type + key + value) — replaces CRC32
  key: [u8; key_length]
  value: [u8; value_length]

[Entry 1] ...
[Entry N] ...
```

### Entry Types

| Type | Key | Value | group_id |
|---|---|---|---|
| Chunk (1) | BLAKE3 hash of value (32 bytes) | raw chunk data | hash of parent file's path |
| FileRecord (2) | file path string | serialized file metadata + ordered chunk hash list | hash of parent directory path |
| DirectoryRecord (3) | directory path string | serialized directory metadata + child list | hash of parent directory path |
| Snapshot (4) | snapshot name string | serialized index | zeros (no parent) |

### No CRC32

BLAKE3 hash in the entry header covers `(entry_type + key + value)`. This serves as both the integrity check AND the content address for chunks. On read, re-hash and compare. If mismatch → corrupt entry.

For chunks specifically, the key IS the hash — so verification is: `BLAKE3(value) == key`. If not, the chunk is corrupt.

### group_id for Recovery

Every entry carries a `group_id` — a 32-byte hash linking it to its parent. For chunks, this is the hash of the file path they belong to. For file records, it's the hash of the parent directory path.

If the index is completely lost, scanning the file and grouping entries by `group_id` reconstructs which chunks belong to which files, and which files belong to which directories. Combined with the ordered chunk list in each FileRecord, full recovery is possible from raw data alone.

---

## Index Strategy (Revised — NOT Fully In-Memory)

### The Problem
Billions of chunks × 44 bytes per index entry = too much RAM.

### The Solution: Two-Tier Index

**Tier 1 — In-Memory (hot):**
```rust
struct FileIndex {
  // Maps file path → location of its FileRecord on disk
  files: HashMap<String, FileLocation>,
}

struct FileLocation {
  file_offset: u64,        // where the FileRecord entry is in the data file
  total_size: u64,         // total file size
  chunk_count: u32,        // how many chunks
}
```

This is the file-level index. With 10 million files × ~150 bytes = ~1.5 GB. Manageable. With LRU eviction for very large deployments, even less.

**Tier 2 — On-Disk (cold):**
Chunk-level index lives ON DISK inside the FileRecord entries. When you need to read a file:
1. Look up the file path in Tier 1 → get FileRecord offset
2. Seek to that offset, read the FileRecord → get the ordered chunk hash list
3. For each chunk hash, look up in... what?

Here's the remaining question: **how do we find a chunk by hash without an in-memory hash map?**

Options:
a) **Sorted chunk index file** — periodically write a sorted list of `(chunk_hash, file_offset)` to the data file. Binary search on disk. O(log n) per lookup.
b) **Bloom filter + scan** — keep a bloom filter in memory (very compact), scan on disk if bloom says "maybe."
c) **Partial in-memory index** — keep recently used chunk locations in an LRU cache. Cache miss = disk scan or sorted index lookup.
d) **B-tree index stored as entries** — our B-tree from backup/ could serve as an on-disk chunk index, stored as entries in the data file.

**Recommended: (a) sorted chunk index + LRU cache.** The sorted index is written periodically as a Snapshot-like entry. It's a single sorted array of `(hash, offset)` pairs — binary search gives O(log n) lookups. Hot chunks are cached in memory via LRU.

---

## Write Path (Revised)

```
1. Compute BLAKE3 hash of the data
2. Construct entry (header + key + value)
3. Append entry to data file
4. fsync (or batch — TBD)
5. Update in-memory file index
6. Return success
```

This IS a WAL. Each append is a log entry. The data file is the log. The index is the "checkpoint" state derived from the log.

### fsync Strategy

Per-op fsync is correct for single-writer. It's what a WAL does. The OS kernel handles write coalescing for sequential appends efficiently. We'll benchmark and optimize if needed.

### Future: Coordinator Writer Pattern

Single "coordinator" thread reserves layout (writes entry headers with reserved space). Multiple writer threads backfill chunk data into reserved slots in parallel. Coordinator syncs. This is a future optimization — the single-writer path is correct first.

---

## Read Path

```
1. Look up file path in Tier 1 index → FileRecord offset
2. Seek to FileRecord, read it → get chunk hash list
3. For each chunk hash:
   a. Check LRU cache for chunk offset
   b. If miss: look up in sorted chunk index (binary search on disk)
   c. Seek to chunk offset, read chunk data
   d. Verify: BLAKE3(data) == chunk_hash
   e. Yield chunk data to caller (streaming)
```

Always streaming. Never load entire files into memory.

---

## Versioning (Revised)

A version/snapshot is a Snapshot entry appended to the data file. Its value contains:
- The serialized Tier 1 file index (file paths → FileRecord offsets)
- The offset of the latest sorted chunk index
- Metadata (name, timestamp, author, message)

Restoring a version:
1. Find the Snapshot entry
2. Deserialize the Tier 1 index from it
3. Load the referenced sorted chunk index
4. The database is now at that point in time

All the data entries from that version are still in the file. Nothing was overwritten. Nothing was deleted. Versioning is just "which index do we use?"

No compaction needed. Ever. Unless you explicitly purge old versions — and even then, only chunks not referenced by ANY remaining version become reclaimable.

---

## Recovery Scenarios

### Scenario 1: Normal startup (index snapshot exists)
1. Read file header
2. Find the latest Snapshot entry (scan backward from end of file, or keep a pointer in the header)
3. Load the serialized index from the Snapshot
4. Replay any entries appended AFTER the snapshot to bring the index up to date
5. Ready

### Scenario 2: Index snapshot is corrupt or missing
1. Scan the entire file from beginning
2. For each entry, rebuild the file index (Tier 1) and chunk index
3. Write a new Snapshot entry at the end
4. Ready (slower startup, but fully recovered)

### Scenario 3: Catastrophic — file is partially corrupt
1. Scan the file, skip entries with bad BLAKE3 hashes
2. For surviving entries, use `group_id` to reconstruct which chunks belong to which files
3. FileRecord entries contain ordered chunk lists — reconstruct file composition
4. DirectoryRecord entries contain child lists — reconstruct directory structure
5. Report what was recovered and what was lost
6. Ready (degraded but functional)

---

## Concurrency

Single writer, multiple readers (for now):
- Writer appends to the file (sequential, no seeking)
- Readers use a snapshot of the Tier 1 index (Arc<RwLock<FileIndex>>)
- Reader snapshots are consistent — they see the state at the time they started reading

Future: coordinator + parallel writers (append-only design supports this naturally).

---

## Single File

Everything in one `.aeor` file:
- File header
- Data entries (chunks, file records, directory records)
- Snapshot entries (serialized indexes)
- Sorted chunk index entries (for efficient hash lookups)

The index is stored IN the database, as just another entry. It's recoverable because the raw data entries contain enough information (`group_id`, chunk hashes, file records) to rebuild it.

---

## Implementation Plan (Revised)

### Task 1: Entry Format + Append Writer
- Entry header struct with BLAKE3 hash (no CRC)
- group_id field for recovery linkage
- Append-only file writer with fsync
- Entry reader (sequential scan + random access by offset)

### Task 2: File Index (Tier 1 — In-Memory)
- FileIndex: path → FileLocation
- Build from file scan
- Update on write
- Snapshot for readers (MVCC via Arc)
- LRU eviction for very large deployments

### Task 3: Chunk Index (Tier 2 — On-Disk)
- Sorted chunk hash → offset array (binary search)
- Periodic write as a SortedIndex entry
- LRU cache for hot chunks in memory
- Fallback to file scan if sorted index is stale

### Task 4: ChunkStorage + Directory Traits
- Implement existing ChunkStorage trait on new engine
- Implement directory operations
- Drop-in replacement for redb-backed code

### Task 5: Versioning
- Snapshot entries (serialized index + chunk index reference)
- Create, restore, list versions

### Task 6: Wire Everything Up
- Replace redb in PathResolver, server, auth, plugins
- Update tests
- Stress test

---

## Open Questions

1. **Sorted chunk index format**: how often do we write it? Every N writes? Every N seconds? On explicit flush?
2. **LRU cache size**: configurable? Default?
3. **File growth**: the file only grows. Should we ever rewrite it? Only on explicit admin action?
4. **Entry ordering in recovery**: if we scan and find chunks but their FileRecord is later in the file, do we buffer chunks until we find their parent? Or two-pass scan?

---

*Ready for Wyatt's review.*
