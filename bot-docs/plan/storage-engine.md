# Storage Engine — Content-Addressed Chunk Store

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design

---

## Core Concept: Everything Is Chunks

AeorDB's storage engine is a **content-addressed chunk store**. All data — table rows, indexes, blobs, metadata, maps — is stored as immutable chunks keyed by their hash. There is no separate blob store, no separate index store, no separate metadata store. It's chunks all the way down.

A chunk is a fixed-size (configurable, power-of-two) block of bytes, identified by its cryptographic hash. Chunks are immutable — once written, they never change. New data creates new chunks.

```
Level 0: Chunks (raw data, keyed by hash)
   hash_a → [256KB of bytes]
   hash_b → [256KB of bytes]
   hash_c → [128KB of bytes]

Level 1: Hash Map (ordered sequence of chunk hashes)
   file_v3 → [hash_a, hash_b, hash_c]

Level 2: Map of Maps (table, schema, database snapshot)
   table_users_v7 → [file_map_columns, file_map_index, ...]

Level N: Root Map (complete database state at a point in time)
   snapshot_2026_03_26 → [table_users_v7, table_orders_v4, ...]
```

Every level is just a map of hashes pointing to the level below. Maps are themselves data, so they are also stored as chunks. The entire database state is a tree of hash maps resolving to chunks.

---

## Why Content-Addressed Chunks

### Deduplication Is Free
Two tables with identical columns share the same chunks. A 10 GB file that changes 3 bytes creates one new chunk. The other chunks are already stored and don't move.

### Replication Is Only the Difference
Node B already has chunks `[a, b, c, d]`. File updates to `[a, b, e, d]`. Send only chunk `e` and the new map. That's one chunk + a few bytes, not the whole file.

### Versioning Is Trivial
A version IS a map. Keep old maps around, and you can reconstruct any previous state. Old maps point to old chunks. Old chunks are immutable. Every committed state is already a snapshot by definition. Restore any previous version by resolving its root hash map.

### Branching Is Cheap
Fork the database for testing? Copy the root map. Both branches share all existing chunks. They diverge only as new writes create new chunks.

### Integrity Verification Is Built-In
Every chunk is keyed by its hash. Read a chunk, hash it, compare to the key. Mismatch = corruption. Merkle integrity for free from the content-addressing.

### Garbage Collection Is Reference Counting
A chunk is alive if any map references it. Delete old version maps, and chunks unique to those versions become collectible.

### Unified Storage
Small data (index entries, rows): small chunks, fast random access.
Big data (blobs, files, video): many chunks, streaming sequential access.
Same mechanism. Same storage. Same replication. Same versioning.

---

## Chunk Configuration

### Chunk Size: Configurable, Power-of-Two

Chunk size is configurable and MUST be a power of two. This enables:
- Seek directly to any chunk via offset: `chunk_index = offset / chunk_size`
- Detect hash boundaries via modulus: `offset % chunk_size == 0`
- Efficient alignment with OS page sizes and disk sectors

### Dynamic Runtime Adjustment

Chunk size is adjustable at runtime. When chunk size changes:
1. New writes use the new chunk size
2. Existing data is lazily re-chunked on access (read triggers re-hash at new size)
3. Old chunks remain valid until garbage collected

This enables:
- **Performance testing built into the engine** — the database can experiment with chunk sizes and measure throughput
- **Automatic optimization** — the engine can decide the best chunk size for a given workload
- **No migration downtime** — chunk size changes are gradual and non-blocking

### Chunk Size Trade-offs

| Chunk Size | Dedup Granularity | Map Overhead | I/O Efficiency | Seek Speed |
|---|---|---|---|---|
| 64 KB | Very fine | Many hashes per file | More seeks | Fastest seek resolution |
| 256 KB | Good | Moderate | Good balance | Good |
| 1 MB | Coarse | Few hashes per file | Excellent sequential | Slower seek resolution |

Default TBD — likely in the 256KB–1MB range. Benchmarking will determine optimal defaults for different workload profiles.

---

## Seeking and Offset Resolution

Given a power-of-two chunk size:

```
chunk_index = byte_offset / chunk_size
offset_within_chunk = byte_offset % chunk_size

// At any given byte offset, we know:
// 1. Which chunk we're in (chunk_index)
// 2. Where in the chunk we are (offset_within_chunk)
// 3. Whether we're at a chunk boundary (offset_within_chunk == 0)
```

For seek operations, resolve the hash map to find the chunk hash at `chunk_index`, then read from `offset_within_chunk` within that chunk.

---

## Indexing

### No Default Indexes (Except Mandatory Fields)

Nothing is indexed by default. The user explicitly requests indexes on whatever they want. This is a superpower, not an annoyance — the user controls what gets indexed, how, and with what algorithm.

### Mandatory Default Fields (All Documents)

Every document/row automatically includes:

| Field | Type | Description |
|---|---|---|
| `document_id` | UUID/unique | Unique document identifier |
| `created_at` | Timestamp | When the document was created |
| `updated_at` | Timestamp | When the document was last modified |

All of these can be set by the user, but they are mandatory parts of the default schema and will exist on every document.

### Pluggable Indexing Algorithms

The user chooses the indexing algorithm per index. Available algorithms include:

- **Scalar ratio indexing** — the self-correcting [0.0, 1.0] mapping (see [Indexing Engine](./indexing-engine.md))
- **Hash index** — O(1) exact match
- **B-tree** — range queries, ordered access
- **Fuzzy/phonetic** — approximate string matching
- **Full-text / inverted index** — text search
- **Geospatial** — location-based queries
- **Custom plugins** — users can write their own indexer via the WASM/native plugin interface

A single column can have MULTIPLE indexes of different types. Index `"56"` as both a string AND a number. Index a `content` column as both "fuzzy" and "full-text".

The chunk store's own internal lookup (hash → chunk location) uses whatever indexing algorithm is fastest for hash lookups — likely scalar ratio or a purpose-built hash table.

---

## Replication via openraft

Distribution is handled above the chunk store. See [Replication & Distribution](./replication.md).

The Raft log entries are tiny:
- "Store chunk `hash_x` with these bytes" (≤ chunk_size)
- "Update map `key` to new hash list `[a, b, e, d]`" (tiny)

Replication of a 10 GB file change that modifies 3 bytes:
- One new chunk (≤ chunk_size)
- One updated hash map
- Total replication payload: ~chunk_size + map delta

The custom append-only Raft log handles these small entries efficiently. No multi-gigabyte log entries. No OOM. No gRPC payload limits.

---

## redb's Role (Revised)

redb may still serve as an internal index for the chunk store — mapping chunk hashes to physical storage locations (byte offsets in storage files). This is a small, bounded, fast-lookup workload that redb handles well. No large values, no blobs — just hash → offset mappings.

Alternatively, the scalar ratio indexing engine or a purpose-built hash table may serve this role. Decision deferred to implementation phase.

---

## Storage Backend (Physical Layer)

The chunk store needs a physical layer to persist chunks to disk. This remains pluggable:

- **Single file** — all chunks packed into one file with an offset index. The SQLite experience.
- **Directory of files** — one file per chunk or chunks packed into segment files.
- **Ceph via librados** — chunks stored as RADOS objects. Distributed, self-healing.
- **S3-compatible** — chunks as S3 objects. Cloud-native.
- **Community plugins** — any backend that can store and retrieve bytes by key.

The physical backend is independent of the chunk store's logical organization.

---

## Problems Addressed

From [Why Databases Suck](../docs/why-databases-suck.md):
- **#1 Storage engines stuck in the past** — Content-addressed chunks are a fundamentally different approach
- **#6 Buffer management is wheel reinvention** — Chunks are simple fixed-size blocks, cache-friendly
- **#11 Compression/efficiency afterthought** — Chunks can be individually compressed; dedup is structural
- **Blob storage** — The #1 complaint: databases can't store large data. Solved. Everything is chunks.

---

## Open Questions

- [ ] Hash algorithm selection (XXH3 for speed? BLAKE3 for crypto-strength? Configurable?)
- [ ] Optimal default chunk size (benchmark-driven decision)
- [ ] Chunk store file format for single-file backend
- [ ] Garbage collection strategy and scheduling
- [ ] Compression per-chunk (algorithm selection, configurable?)
- [ ] Encryption per-chunk (key management, configurable?)
- [ ] Cache strategy for hot chunks (LRU? LFU? Adaptive?)
- [ ] Does redb serve as the hash→offset index, or do we build our own?
- [ ] Content-defined chunking (rolling hash) as an option alongside fixed-size?
