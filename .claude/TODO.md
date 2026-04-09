# AeorDB — TODO

## Recently Completed
- [x] Users, Groups, Permissions (crudlify) — 1,008 tests
- [x] Selective zstd compression — 35 tests
- [x] Auth Provider URI (--auth flag) — 41 tests
- [x] NVT bitmap compositing query engine — 78 tests
- [x] Custom storage engine — 273 tests
- [x] Unified indexing (ScalarConverter + NVT) — 136 tests

- [x] HTTP Portal Dashboard (stats API + embedded UI) — 17 tests

## In Progress: Version Export, Patch & Import
- [x] Phase 1: File header extension — 21 tests
- [x] Phase 2: Tree walker utility — 21 tests
- [x] Phase 3: Export operation + CLI — 17 tests
- [x] Phase 4: Diff/Patch operation + CLI — 20 tests
- [x] Phase 5: Import operation + CLI + promote — 18 tests
- [x] Phase 6: HTTP API endpoints — 13 tests
- [x] Fix: dual-key directory storage for immutable snapshots — 25 tests
- [ ] Phase 7: E2E verification (manual testing)

## In Progress: Event System
- [x] Phase 1: EventBus + EngineEvent + RequestContext types — 32 tests
- [x] Phase 2: Thread ctx through all engine methods + callers + tests (45 files)
- [x] Phase 3: Emit events from engine methods — 34 tests
- [x] Phase 4: Heartbeat task — 7 tests
- [x] Phase 5: SSE endpoint — 27 tests
- [x] Phase 6: Webhooks — 44 tests

## Completed: Sorting + Pagination
- [x] Tasks 1-3: Types + execute_paginated + sorting — 31 tests
- [x] Tasks 4-5+7: Cursors + HTTP envelope + QueryBuilder — 25 tests

## Completed: Aggregations — 47 tests

## In Progress: Content-Addressed B-Tree Directories
- [ ] Tasks 1-2: Node types + serialization + B-tree operations
- [ ] Task 3: Integrate into DirectoryOps
- [ ] Task 4: Update tree walker + backup
- [ ] Task 5: Performance benchmark

## Completed: Disk-Resident KV Store
- [x] Tasks 1-2: KV page format + DiskKVStore — 41 tests
- [x] Task 3: Wire into StorageEngine
- [x] Tasks 4-5: Startup without scan + resize on overflow — 17 tests
- [x] Task 6: Benchmark (flat ~1000/s to 250K)

## Total: 2,164 tests, all passing

## In Progress: Fuzzy Search, Trigram Indexing & Phonetic Matching
- [x] Phase 1: Multi-Index Foundation (strategy(), expand_value(), scored QueryResult, IndexManager changes) — 30 tests
- [x] Phase 2: Trigram Indexing (fuzzy.rs, TrigramConverter, Dice similarity) — 43 tests
- [x] Phase 3: Phonetic Indexing (phonetic.rs, PhoneticConverter, Soundex + Double Metaphone) — 71 tests
- [x] Phase 4: Fuzzy Scoring + Recheck (DL, JW, auto fuzziness, score-based sorting) — 37 tests
- [x] Phase 5: Composite Match + Polish (match op, HTTP updates, E2E) — 22 tests

## In Progress: Document Parsers
- [x] Tasks 1-2: Config rename (field_name→name, converter_type→type) + test migration
- [x] Task 3: Source path resolution module — 39 tests
- [x] Task 4: Recursive guard for system directories
- [x] Task 5: IndexingPipeline extraction — 18 tests
- [x] Tasks 6-7: .logs/ system + source resolution integration (folded into Task 5)
- [x] Tasks 8-9: Parser plugin invocation + content-type registry — 31 tests
- [x] Tasks 10-11: Plugin mapper + WASM log host function
- [x] Task 12: Wire PluginManager (done in Tasks 8-11)
- [ ] Task 13: E2E test with real WASM parser (deferred — needs compiled WASM binary)
- [x] Tasks 14-15: HTTP verified + docs updated

## Completed: Garbage Collection (Mark-and-Sweep) — 36 tests
- [x] Task 1: In-place write infrastructure (write_entry_at, write_void_at) — 5 tests
- [x] Task 2: GC mark phase (walk all live roots, collect reachable hashes) — 6 tests
- [x] Task 3: GC sweep phase (in-place overwrite garbage, dry-run, events) — 8 tests
- [x] Task 4: CLI command (aeordb gc --database --dry-run)
- [x] Task 5: HTTP endpoint (POST /admin/gc) — 11 tests
- [x] Task 6: Edge case tests (folded into Task 3)

## Completed: Concurrent KV Readers (Snapshot Double-Buffering) — 20 new tests
- [x] Task 1: ReadSnapshot struct + lock-free get/iter_all — 12 tests
- [x] Task 2: Refactor DiskKVStore — remove hot cache, snapshot publishing, threshold 512
- [x] Task 3: Wire StorageEngine reads to snapshots
- [x] Task 4: Wire EngineChunkStorage to snapshots
- [x] Task 5: Concurrency tests (multi-threaded contention) — 8 tests

## Remaining Future Plans
- [ ] Server-side compilation + in-database SDK + schema-as-code
- [ ] Cron/background task system
- [ ] Pre-hashed client uploads
- [ ] Fork merging (true merge, not just fast-forward)
- [ ] Concurrent parallel writers (coordinator pattern)
- [ ] Large directory optimization
- [ ] File defragmentation
- [ ] Encryption, vaults, zero-knowledge multi-user storage
- [ ] Multi-database sharding
- [ ] Query engine enhancements (aggregations, sorting, pagination, etc.)
