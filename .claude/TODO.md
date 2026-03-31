# AeorDB — TODO

## Current: Custom Storage Engine Implementation

### Task 1: Entry Format + Append Writer + Reader
- [ ] Entry header struct (magic 0x0AE012DB, version, type, flags, hash_algo, dynamic hash)
- [ ] HashAlgorithm enum (BLAKE3_256, SHA256, SHA512, SHA3_256, SHA3_512)
- [ ] Append-only file writer with fsync
- [ ] Sequential reader (entity-by-entity scan via total_length jumps)
- [ ] Random-access reader (seek to offset, read entry)
- [ ] File header struct (256 bytes, hash_algo, resize flags, etc.)
- [ ] Tests

### Task 2: NVT (Normalized Vector Table)
- [ ] Hash-to-scalar conversion
- [ ] NVT bucket structure (offset + count)
- [ ] Bucket lookup
- [ ] Self-correcting scan updates
- [ ] Resize (double buckets)
- [ ] Serialize/deserialize (versioned)
- [ ] Tests

### Task 3: KV Block (Sorted Hash→Offset Array)
- [ ] KVEntry struct (type_flags + hash + offset, dynamic hash length)
- [ ] Sorted array on disk
- [ ] Insert, lookup, bulk operations
- [ ] Indexed by NVT
- [ ] Tests

### Task 4: KV Resize Mode
- [ ] Buffer KVS+NVT during resize
- [ ] Bulk entity relocation
- [ ] Merge buffer into primary
- [ ] Crash recovery (resize_in_progress flag)
- [ ] Tests

### Task 5: Void Management
- [ ] Deterministic void hashes by size
- [ ] Find void, create void, split void
- [ ] Truncate-not-void at EOF
- [ ] Every gap gets a void entry
- [ ] Tests

### Task 6: ChunkStorage Trait Implementation
- [ ] Implement existing ChunkStorage trait on new engine
- [ ] Drop-in replacement for RedbChunkStorage
- [ ] Tests

### Task 7: FileRecord + DeletionRecord
- [ ] FileRecord format (metadata first, chunks last)
- [ ] DeletionRecord format
- [ ] DirectoryIndex as FileRecord with type 0x03
- [ ] ChildEntry format (fixed fields first, variable last)
- [ ] Tests

### Task 8: Directory Operations + Path Resolver
- [ ] Path normalization (Unix-style)
- [ ] Directory listing via DirectoryIndex
- [ ] Propagate-up to root on write
- [ ] Store/read/delete files
- [ ] Streaming reads
- [ ] Tests

### Task 9: Versioning (Forks + Snapshots)
- [ ] Create/restore snapshots
- [ ] Create/promote/abandon forks
- [ ] HEAD management
- [ ] Auto-snapshot naming (auto-{timestamp})
- [ ] Tests

### Task 10: Wire to HTTP Layer
- [ ] Replace redb in path resolver and HTTP handlers
- [ ] Fork/snapshot HTTP endpoints
- [ ] System tables migration
- [ ] Update existing tests
- [ ] Tests

### Task 11: Stress Test + Benchmarks
- [ ] Compare storage overhead to redb baseline
- [ ] Throughput benchmarks
- [ ] Large file tests
- [ ] Recovery tests
