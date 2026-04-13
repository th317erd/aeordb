# AeorDB — TODO

## Total: ~2,200+ tests, all passing

---

## Completed Features

- [x] Custom storage engine (append-only WAL-filesystem) — 273 tests
- [x] Users, Groups, Permissions (crudlify) — 1,008 tests
- [x] Selective zstd compression — 35 tests
- [x] Auth Provider URI (--auth flag) — 41 tests
- [x] NVT bitmap compositing query engine — 78 tests
- [x] Unified indexing (ScalarConverter + NVT) — 136 tests
- [x] HTTP Portal Dashboard (stats API + embedded UI) — 17 tests
- [x] Sorting + Pagination (cursors, HTTP envelope, QueryBuilder) — 56 tests
- [x] Aggregations — 47 tests
- [x] Fuzzy Search, Trigram Indexing & Phonetic Matching — 203 tests
- [x] Document Parsers (WASM plugin SDK, content-type, source resolution) — 88 tests
- [x] Version Export, Patch & Import (CLI + HTTP) — 135 tests
- [x] Event System (EventBus, SSE, Webhooks, Heartbeat) — 144 tests
- [x] Content-Addressed B-Tree Directories — 93 tests
- [x] Disk-Resident KV Store (bucket pages, NVT index, hot file WAL) — 58 tests
- [x] Garbage Collection (mark-and-sweep, in-place overwrite, CLI + HTTP) — 36 tests
- [x] Concurrent KV Readers (snapshot double-buffering, lock-free reads) — 20 tests
- [x] Pre-Hashed Client Uploads (4-phase protocol, dedup, atomic commit) — 28 tests
- [x] Content-Addressed FileRecords (dual-key storage, correct snapshot versioning) — 10 tests
- [x] WASM Query Plugins (host functions, SDK, QueryBuilder, echo-plugin E2E) — ~130 tests
- [x] Parser hardening (Contains/Similar on parser fields, content-type auto-routing) — 6 tests

---

## Performance Optimizations (Completed)

- [x] Snapshot buffer-only publish on insert (Arc pages, no disk I/O per write)
- [x] Incremental page updates on flush (only re-read modified buckets)
- [x] bulk_insert for KV resize (skip snapshot publishing + dedup checks)
- [x] GC batch nosync writes (one sync at end, not per-entry)
- [x] GC mark_deleted_batch (fixed O(n²) buffer cloning — 13.5hrs → 3.8min)
- [x] WASM converter stubs removed (dead code returning 0.5)

---

## Benchmark Results (Release Mode, 381K files)

| Metric | Value |
|--------|-------|
| Write throughput | 1,477 files/sec |
| Read throughput | 131,048 reads/sec |
| Eq query | 0.2ms |
| Between query | 0.5ms |
| Contains query | 0.6ms |
| Concurrent ops | 9,406 ops/sec |
| GC (2.36M garbage) | 228 seconds |
| Delete rate | 449/sec |

---

## Completed: Task System, Cron & Reindex — 65 tests
- [x] Task 1: TaskQueue core (persistence + in-memory progress) — 21 tests
- [x] Task 2: Task worker + reindex/GC executors — 15 tests
- [x] Task 3: Auto-trigger + query meta
- [x] Task 4: Cron scheduler — 14 tests
- [x] Task 5: HTTP API (tasks + cron) — 15 tests

## Completed: Stress Tests — 13 tests
- [x] 1. Deep nesting: 100 levels in 10ms
- [x] 2. Large files: 50MB roundtrip in 54ms, dedup instant
- [x] 3. Concurrent HTTP: 40 threads, 12.8K ops/sec, zero panics
- [x] 4. Snapshots at scale: 1000 files, tree walks ~10ms
- [x] 5. Many-result queries: 2000 files, 1800-match query correct
- [x] 6. Index cardinality: high card 1.2ms, low card 1.7ms
- [x] 7. WASM under load: 150 invocations, 100% success
- [x] 8. Mixed workload: 13 threads, 438 ops, zero panics

## Future Plans (Not Started)

- [ ] Cron/background task system
- [ ] Fork merging (true merge with conflict detection, not just fast-forward)
- [ ] File defragmentation (rewrite file to eliminate voids)
- [ ] Encryption, vaults, zero-knowledge multi-user storage
- [ ] Multi-database sharding
