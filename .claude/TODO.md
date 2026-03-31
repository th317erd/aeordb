# AeorDB — TODO

## Custom Storage Engine — COMPLETE

All 11 tasks implemented and tested:

- [x] Task 1: Entry format + append writer + reader
- [x] Task 2: NVT (Normalized Vector Table)
- [x] Task 3: KV Block (sorted hash→offset array)
- [x] Task 4: KV Resize Mode (buffer KVS during resize)
- [x] Task 5: Void Management (deterministic hashes by size)
- [x] Task 6: ChunkStorage Trait (drop-in replacement)
- [x] Task 7: FileRecord + DeletionRecord + DirectoryIndex
- [x] Task 8: StorageEngine + DirectoryOps + path resolver
- [x] Task 9: Versioning (forks + snapshots)
- [x] Task 10: HTTP wiring (/engine/*, /version/*)
- [x] Task 11: Stress test (102% ratio vs redb's 224%)

## Test Count: 785 (all passing)

## Stress Test Results
- Custom engine: 102% storage ratio (~2% overhead)
- redb baseline: 224% storage ratio (124% waste)
- Read: 8ms/file | Write: 12.8 files/sec | Snapshot/fork endpoints working
