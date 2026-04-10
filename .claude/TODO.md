# AeorDB — TODO

## Total: ~2,202 tests, all passing

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

---

## Minor Loose Ends — Cleared

- [x] Version Export Phase 7: E2E manual verification — passed (export, diff, import all work)
- [x] WASM Parser E2E test — 11/11 tests pass (binary compiled, 167KB)
- [x] Snapshot versioning bug — fixed via content-addressed FileRecord keys (10 tests)
- [x] Hot file naming mismatch — fixed (derive db_name from .aeordb stem)

---

## Completed: WASM Query Plugins (Phases 1+2) — ~130 new tests
- [x] Task 1: HostState with engine access + RequestContext
- [x] Task 2: 7 real host functions (CRUD + query + aggregate)
- [x] Task 3: SDK PluginContext + aeordb_query_plugin! macro — 97 SDK tests
- [x] Task 4: SDK QueryBuilder + AggregateBuilder (fluent API)
- [x] Task 5: Fix _invoke HTTP endpoint — 5 tests
- [x] Task 6: Echo-plugin E2E tests — 14 tests

## Future Plans (Not Started)
- [ ] Cron/background task system
- [ ] Fork merging (true merge with conflict detection, not just fast-forward)
- [ ] File defragmentation (rewrite file to eliminate voids)
- [ ] Encryption, vaults, zero-knowledge multi-user storage
- [ ] Multi-database sharding
