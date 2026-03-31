# Future Plans

Ideas discussed but deferred. Not blocking current implementation.

---

## Chunk Ownership & Garbage Collection

**What:** Track how many FileRecords/versions reference each chunk. Clean up unreferenced chunks.

**Ideas discussed:**
- Reference counting in KV entries (u32 per entry)
- Periodic tracing reconciliation (walk all roots, mark reachable, reconcile counts)
- "Dropped" flag on chunks eligible for cleanup

**Why deferred:** Requires careful design to handle concurrent forks/versions. With versioning, chunks are rarely truly orphaned. No data loss risk from deferring — orphaned chunks just occupy space.

---

## Cron/Background Task System

**What:** A scheduled task runner for database maintenance.

**Tasks it would handle:**
- Garbage collection (unreferenced chunk cleanup)
- Integrity verification (walk entities, verify hashes)
- NVT optimization (resize if scan lengths are too high)
- Void consolidation (merge adjacent small voids)
- Stale fork cleanup (delete abandoned forks after configurable period)
- Ref count reconciliation (tracing verification)
- Auto-snapshots (on admin-configured schedule)

**Why deferred:** The database works correctly without it. These are optimization and hygiene operations.

---

## Pre-Hashed & Pre-Chunked Client Uploads

**What:** Allow clients to send data already split into chunks with pre-computed hashes.

**Benefits:**
- Client does the CPU-intensive hashing
- Server just verifies and stores
- Dedup check before upload (client sends hash list, server says "I already have these, only send the new ones")
- Dramatically reduces upload bandwidth for incremental backups

**API concept:**
```
POST /fs/myapp/data/bigfile.bin/_chunked
Content-Type: application/x-aeordb-chunked

{
  "chunk_size": 262144,
  "hash_algo": "blake3_256",
  "chunks": [
    { "hash": "abc123...", "data": "<base64>" },
    { "hash": "def456...", "data": null },  // null = server already has it
    ...
  ]
}
```

**Why deferred:** Requires client SDK support. Current simple PUT works fine for now.

---

## Merge Operations (Fork Merging)

**What:** Combine changes from two forks, handling conflicts.

**Discussed:** Currently fork promotion is fast-forward only (just move HEAD pointer). True merging (combining divergent changes) requires conflict detection and resolution.

**Why deferred:** Fast-forward covers the primary use case (batch writes). Merging is complex and needs careful design (what happens when both forks modify the same file?).

---

## Concurrent Parallel Writers (Coordinator Pattern)

**What:** Single coordinator reserves layout, multiple worker threads fill chunk data in parallel.

**Discussed:** The coordinator writes entry headers with reserved space. Workers backfill chunk data independently. Coordinator fsyncs and updates KVS.

**Why deferred:** Single-writer is correct and sufficient. Parallel writes are an optimization for high-throughput workloads.

---

## Large Directory Optimization

**What:** Directories with millions of entries are currently single FileRecords that get chunked. May need additional optimization for very large directories.

**Ideas:**
- B-tree-structured directory listings (sorted sub-chunks for binary search within directory)
- Indexed child lookups (instead of scanning the full child list)

**Why deferred:** Chunked FileRecords handle large directories already. Optimization for extreme scale.

---

## File Defragmentation

**What:** Over time, void fragmentation accumulates (many small voids). A defrag operation would rewrite the file to eliminate voids and pack entities contiguously.

**Why deferred:** Voids are tracked and reused. Fragmentation is slow-growing. Defrag is an admin-triggered maintenance operation.

---

## Encryption at Rest

**What:** Encrypt entity values on disk.

**Ideas:**
- Per-entity encryption with a database key
- Key management (stored externally, derived from password, HSM)
- The entry header stays unencrypted (needed for scanning), values are encrypted

**Why deferred:** Requires careful key management design. Not blocking core functionality.

---

## Multi-Database Sharding

**What:** Split a single logical database across multiple .aeordb files.

**Ideas:**
- Shard by hash prefix (chunk hashes are uniformly distributed)
- Each shard is a complete .aeordb file with its own NVT+KVS
- A coordinator layer routes operations to the correct shard

**Why deferred:** Single file handles terabytes. Sharding is for petabyte scale.

---

## Custom Rust MIME Detection Crate

**What:** Build our own MIME detection crate from the JavaScript `file-type` library (https://github.com/sindresorhus/file-type).

**Why discussed:** Wanted the best possible MIME detection from content bytes.

**Resolution:** Using `file-format` crate (200+ formats, zero deps) for now. Custom crate only if `file-format` proves insufficient.
