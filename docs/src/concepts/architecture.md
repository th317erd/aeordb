# Architecture

AeorDB is a single-file database built on an append-oriented write-ahead log
(WAL). The database file contains user data, namespace state, the in-file KV
lookup block, recovery state, and stored index artifacts. Understanding the
architecture helps you reason about performance, recovery, versioning, and the
v3-to-v4 migration boundary.

The currently selected service authority is the v3 compatibility runtime. The
binary also contains the independently tested v4 format/root/GC/native-index
and migration substrate. `aeordb migrate-v4` can build a separate verified
shadow, but ordinary startup does not select it and no public cutover command
exists. See
[V3-to-V4 Migration and Cutover](../operations/migration.md).

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
        | Append   |  | In-file  |  | Active v0  |
        | Writer   |  | KV/NVT   |  | indexes    |
        +-----+----+  +-----+----+  +------+------+
              |              |              |
              +--------------+--------------+
                             |
                     [ mydb.aeordb ]
                 A/B header + KV + WAL + hot tail
```

## Native Parsers

AeorDB ships with 8 built-in format parsers (text, HTML/XML, PDF, images, audio, video, MS Office, ODF) that run as compiled Rust code during indexing. Native parsers are tried first for recognized content types; unrecognized formats fall through to the WASM plugin system. This means common file types are indexable out of the box with zero deployment overhead. See [Plugin Endpoints](../api/plugins.md#native-parsers) for the full format list.

## Metrics Counters

System metrics combine O(1) atomic counters with one shared bounded runtime-observability producer. That producer samples fixed memory-owner/configuration registries, in-memory durability/recovery state, cache summaries, and bounded operating-system process data. On startup, live file counts and logical bytes are initialized by walking the current directory tree, while stored chunk payload bytes are initialized from KV entry metadata without reading chunk bodies. `GET /system/stats`, root-only Prometheus collection, the root-only `metrics` SSE event, `aeordb status`, and the Dashboard consume the same contract. Collection never scans the WAL, KV store, file bodies, or index files and never evicts caches as a monitoring side effect. Rolling rate computation (1-minute, 5-minute, 15-minute averages) is maintained continuously.

## The Database File (`.aeordb`)

The `.aeordb` file is an append-only WAL. Every write appends a new entry to the end of the file. Entries are never modified in place (except during garbage collection).

### Current V3 Service Layout

```
[Header slot A - 256 bytes]
[Header slot B - 256 bytes]
[Reserved growth zone and in-file KV bucket pages]
[Append-oriented WAL entries]
[Hot tail: pending KV writes, Void snapshot, durability metadata]
```

The two v3 header slots carry sequence and CRC evidence; startup selects the
highest valid slot. The selected header locates the KV block, WAL frontier, hot
tail, and current HEAD. Every WAL entry carries its own header with magic
bytes, type tag, hash algorithm, compression flag, key, and value.

### Entry Types

| Type | Purpose |
|------|---------|
| Chunk | Raw file data (256KB blocks) |
| FileRecord | File metadata + ordered list of chunk hashes |
| DirectoryIndex | Directory contents (child entries with hashes) |
| Snapshot | Named point-in-time version reference |
| DeletionRecord | Marks a file as deleted (for version history completeness) |
| Void | Free space marker (reclaimable by future writes) |

## In-File KV Lookup Block

The active v3 KV store is inside the `.aeordb` file. Bucket pages map hashes to
WAL offsets and carry validation metadata. The WAL remains primary evidence;
corrupt or stale lookup state is rebuilt from verified entries rather than
being treated as user authority.

There is no current `.aeordb.kv` sidecar to delete during recovery. Preserve
the complete database file, lock, configured emergency-spill artifacts, and
logs before attempting repair.

### KV Resize

When the KV block needs to grow, the engine uses a staged in-file transition:
1. A temporary buffer KV store is created
2. Admitted writes are retained in the bounded buffer
3. The growth zone and affected WAL span are relocated safely
4. Buffered entries are merged into the expanded primary block
5. A new durable header/hot-tail boundary is published

Admission, memory pressure, durability failure, or an incomplete resize can
delay or refuse writes; callers must not assume resize is an unbounded
always-successful background operation.

## NVT (Normalized Vector Table)

The active v3 normalized-vector lookup narrows a hash to a small KV bucket
range. It accelerates exact lookup but does not replace full hash verification.

### How It Works

1. Normalize the relevant hash prefix to the configured lookup coordinate.
2. Resolve the coordinate to the selected bucket/page range.
3. Read the candidate entries and require an exact full-hash match.

Lookup metadata is recoverable. It cannot make malformed WAL data valid or
change which namespace root is authoritative.

## Staged V4 Architecture

The v4 target separates persistent authority from acceleration:

- `DatabaseHeaderV4` uses validated A/B publication and capability admission.
- Immutable namespace roots and semantic state select content-addressed
  objects; typed SystemFamily policy defines protected/detached authority.
- Index definitions, pages, directories, manifests, coverage, and sparse NVT
  hints are immutable native artifacts. The NVT is optional acceleration, not
  membership or ordering authority.
- Exact query evaluation falls back to validated pages or authoritative source
  state when coverage or NVT hints are absent, stale, corrupt, or unsupported.
- Root lifecycle, GC candidates, quarantine, sweep evidence, Void claims, and
  settlement use explicit controls and crash-recoverable state machines.
- Side-by-side migration creates and verifies a separate destination before any
  service-path change; read-only validation precedes operator acceptance and
  the first v4 write.

These structures are present for qualification and migration orchestration;
they do not make ordinary v3 service data v4.

## In-File Hot Tail (Crash Recovery)

The selected header points to an in-file hot tail containing pending KV write
records, the current Void snapshot, and durability metadata. During normal
publication:

1. WAL bytes and buffered lookup state are written under the shared durability
   coordinator.
2. The database and hot-tail state are synchronized and read back as required.
3. A header slot is advanced only to a durable frontier.

If the hot tail or selected frontier is invalid after a crash, startup uses the
dirty-rebuild path and reconstructs lookup/Void state from verified WAL
evidence. Configured external emergency-spill directories are incident
evidence for otherwise unpublishable state; they are not the ordinary hot
tail. See [Storage Engine](./storage-engine.md#emergency-spill-recovery).

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
