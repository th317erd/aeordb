# Architecture

AeorDB is a single-file database built on an append-only write-ahead log (WAL). The database file contains all data, indexes, and metadata in one place. Understanding the architecture helps you reason about performance, recovery, and versioning behavior.

## High-Level Overview

```
                         aeordb start
                             |
                     +-------+-------+
                     |  HTTP Server  |
                     |  (axum)       |
                     +-------+-------+
                             |
              +--------------+--------------+
              |              |              |
        +-----+----+  +-----+----+  +------+------+
        | Query    |  | Plugin   |  | Version     |
        | Engine   |  | Manager  |  | Manager     |
        +-----+----+  +-----+----+  +------+------+
              |              |              |
              |         +----+----+         |
              |         | Native  |         |
              |         | Parsers |         |
              |         +---------+         |
              +--------------+--------------+
                             |
                    +--------+--------+
                    | Storage Engine  |
                    | (StorageEngine) |
                    +--------+--------+
                             |
              +--------------+--------------+
              |              |              |
        +-----+----+  +-----+----+  +------+------+
        | Append   |  | KV Store |  | NVT         |
        | Writer   |  | (.kv)    |  | (in-memory) |
        +----------+  +----------+  +-------------+
              |              |
              +--------------+
                    |
            [  mydb.aeordb  ]    <-- single file on disk
            [ mydb.aeordb.kv ]   <-- KV index file
```

## Native Parsers

AeorDB ships with 8 built-in format parsers (text, HTML/XML, PDF, images, audio, video, MS Office, ODF) that run as compiled Rust code during indexing. Native parsers are tried first for recognized content types; unrecognized formats fall through to the WASM plugin system. This means common file types are indexable out of the box with zero deployment overhead. See [Plugin Endpoints](../api/plugins.md#native-parsers) for the full format list.

## Metrics Counters

System metrics combine O(1) atomic counters with one shared bounded runtime-observability producer. That producer samples fixed memory-owner/configuration registries, in-memory durability/recovery state, cache summaries, and bounded operating-system process data. On startup, live file counts and logical bytes are initialized by walking the current directory tree, while stored chunk payload bytes are initialized from KV entry metadata without reading chunk bodies. `GET /system/stats`, root-only Prometheus collection, the root-only `metrics` SSE event, `aeordb status`, and the Dashboard consume the same contract. Collection never scans the WAL, KV store, file bodies, or index files and never evicts caches as a monitoring side effect. Rolling rate computation (1-minute, 5-minute, 15-minute averages) is maintained continuously.

## The Database File (`.aeordb`)

The `.aeordb` file is an append-only WAL. Every write appends a new entry to the end of the file. Entries are never modified in place (except during garbage collection).

### File Layout

```
[File Header - 256 bytes]
  Magic: "AEOR"
  Hash algorithm, timestamps, KV/NVT pointers, HEAD hash, entry count

[Entry 1] [Entry 2] [Entry 3] ... [Entry N]
  Chunks, FileRecords, DirectoryIndexes, Snapshots, DeletionRecords, Voids
```

The 256-byte file header contains pointers to the KV block, NVT, and the current HEAD hash. Every entry carries its own header with magic bytes, type tag, hash algorithm, compression flag, key, and value.

### Entry Types

| Type | Purpose |
|------|---------|
| Chunk | Raw file data (256KB blocks) |
| FileRecord | File metadata + ordered list of chunk hashes |
| DirectoryIndex | Directory contents (child entries with hashes) |
| Snapshot | Named point-in-time version reference |
| DeletionRecord | Marks a file as deleted (for version history completeness) |
| Void | Free space marker (reclaimable by future writes) |

## The KV Index File (`.aeordb.kv`)

The KV store is a sorted array of `(hash, offset)` pairs stored in a separate file. It maps content hashes to byte offsets in the main `.aeordb` file, providing O(1) lookups when combined with the NVT.

Each entry is `hash_length + 8` bytes (40 bytes for BLAKE3-256). The entries are sorted by hash, and the NVT tells you which bucket to look in, so lookups are a single seek + small scan.

### KV Resize

When the KV store needs to grow, the engine enters a brief resize mode:
1. A temporary buffer KV store is created
2. New writes go to the buffer (no blocking)
3. The primary KV store is expanded
4. Buffer contents are merged into the primary
5. Buffer is discarded

Writes never block during resize.

## NVT (Normalized Vector Table)

The NVT is an in-memory structure that provides fast hash-to-bucket lookups for the KV store.

### How It Works

1. Normalize the hash to a scalar: `first_8_bytes_as_u64 / u64::MAX` produces a value in [0.0, 1.0]
2. Map the scalar to a bucket: `bucket_index = floor(scalar * num_buckets)`
3. The bucket points to a range in the KV store -- scan that range for the exact hash

BLAKE3 hashes are uniformly distributed, so buckets stay balanced without manual tuning. The NVT starts at 1,024 buckets and doubles when the average scan length exceeds a threshold.

### Scaling

| Entries | NVT Buckets | NVT Memory | Avg Scan |
|---------|-------------|------------|----------|
| 10,000 | 1,024 | 16 KB | ~10 |
| 1,000,000 | 65,536 | 1 MB | ~15 |
| 100,000,000 | 1,048,576 | 16 MB | ~95 |

## Hot File WAL (Crash Recovery)

The `--hot-dir` flag specifies a directory for write-ahead hot files. During a write:

1. The entry is written to a hot file first (fsync'd)
2. The entry is then written to the main `.aeordb` file
3. On success, the hot file entry is cleared

If the process crashes between steps 1 and 2, the hot file is replayed on the next startup to recover uncommitted writes. If `--hot-dir` is not specified, the hot directory defaults to the same directory as the database file.

## Snapshot Double-Buffering

AeorDB uses `ArcSwap` for lock-free concurrent reads. The in-memory directory state is wrapped in an `Arc` that readers clone cheaply. When a write completes:

1. The writer builds a new directory state
2. The new state is swapped in atomically via `ArcSwap::store`
3. Readers holding the old `Arc` continue using it until they finish
4. The old state is dropped when the last reader releases it

This means:
- Readers never block writers
- Writers never block readers
- Every read sees a consistent point-in-time snapshot
- No read locks, no write locks on the read path

## B-Tree Directories

Small directories (under 256 entries) are stored as flat lists of child entries. When a directory exceeds 256 entries, the engine automatically converts it to a B-tree structure. This keeps directory lookups O(log n) even for directories with millions of files.

B-tree nodes are themselves stored as content-addressed entries, so they participate in versioning and structural sharing just like any other data.

## Directory Propagation

When a file changes, the engine propagates the update up the directory tree:

```
Write /users/alice.json
  -> update /users/ directory (new child hash for alice.json)
    -> update / root directory (new child hash for users/)
      -> update HEAD (new root hash)
```

Each directory gets a new content hash because its contents changed. This is how the Merkle tree works -- a change at any leaf creates new hashes all the way to the root. The root hash (HEAD) uniquely identifies the complete state of the database.

## Unified Authority Caches

Every storage engine owns bounded, memory-coordinator-accounted caches for
frequently accessed authority and metadata:

| Cached Data | Invalidated On |
|-------------|----------------|
| Permissions and grants index | Acknowledged write, delete, or rename of `.aeordb-permissions` files |
| Index configs | Acknowledged write, delete, or rename of `.aeordb-config/indexes.json` |
| Groups | Acknowledged user or group authority change |
| API keys | Acknowledged API-key creation, update, or revocation |

Invalidation is a post-acknowledgement coordinator responsibility, not
route-local or producer-specific fanout cleanup. For ordinary mutations the
coordinator derives targeted invalidations from the acknowledged canonical
source paths; a whole-root publication clears every authority cache. Embedded,
HTTP, plugin, sync, restore, and custom-fanout producers therefore converge on
the same behavior. If invalidation itself fails, AeorDB logs the failure and
subsequent access to a poisoned cache fails closed; the already durable mutation
is never converted into a retryable HTTP failure.

Whole-root replacement is an explicit namespace-batch contract. The shared
coordinator validates the exact `/` root transition and performs complete
authority and directory-content cache invalidation after hard publication,
before any caller-specific counter or event fanout. Incremental HEAD updates
remain crate-private to the directory planner, so import, restore, plugin, and
embedded callers cannot accidentally bypass whole-root invalidation with a
custom fanout.

Clean entries are admitted through the process memory coordinator and may be
evicted under pressure. Cache correctness never depends on retention: a miss
reloads the current stored authority. With `--auth file://...`, the main data
engine owns permission/group authority while the separate identity engine owns
the API-key cache and API-key records.

### KV Type Index

The KV store maintains a type-based index that enables O(k) lookups by entry type (snapshots, files, directories, etc.) instead of O(n) full scans. For example, listing all snapshots requires scanning only snapshot entries rather than the entire KV store.

## Next Steps

- [Storage Engine](./storage-engine.md) -- entry format, hashing, chunking, and dedup details
- [Versioning](./versioning.md) -- how snapshots, forks, and diff/patch work
- [Indexing & Queries](./indexing.md) -- how indexes are built and queried
