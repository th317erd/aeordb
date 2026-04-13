# AeorDB — TODO

## Total: ~2,300+ tests, all passing

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
- [x] Configurable CORS (CLI + per-path /.config/cors.json) — 23 tests
- [x] Task System, Cron & Reindex — 65 tests
- [x] Query operator fixes (u64 Eq/Between/Gt/Lt precision, Contains word-boundary) — 22 tests

---

## Completed: Performance Optimizations
- [x] Snapshot buffer-only publish on insert (no disk I/O per write)
- [x] Incremental page updates on flush (only modified buckets)
- [x] bulk_insert for KV resize (skip snapshot publishing)
- [x] GC batch nosync writes (one sync at end)
- [x] GC mark_deleted_batch (fixed O(n²) buffer cloning)

## Completed: Stress Tests — 13 tests
- [x] Deep nesting, large files, concurrent HTTP, snapshots, many-result queries, cardinality, WASM load, mixed workload

---


- [ ] Cron/background task system enhancements
- [ ] Fork merging (true merge with conflict detection)
- [ ] File defragmentation (rewrite file to eliminate voids)
- [ ] Encryption, vaults, zero-knowledge multi-user storage
- [ ] Multi-database sharding
## Audit Fixes — DONE (20 of 38 fixed)

### Fixed (Critical + High + Medium + Low)
- [x] C1: Backup routes require root auth
- [x] C2: init_hot_file returns Result (no process::exit)
- [x] C4: WASM plugin cache (compiled modules cached)
- [x] C5: get_entry uses READ lock (concurrent reads unblocked)
- [x] H1: Destructive version ops require root
- [x] H2: Non-UUID JWT sub rejected (permission bypass fixed)
- [x] H3: WASM host functions use caller's RequestContext
- [x] H5: normalize_path resolves ".." segments
- [x] H6: Plugin response headers filtered via allowlist
- [x] H7: Route-specific body limits (not 10GB everywhere)
- [x] H9: insert() logs flush errors (was silent)
- [x] H10: Drop impl logs flush errors
- [x] H11: GC marks DeletionRecords as live
- [x] H12: GC sweep re-verifies entries (concurrent write safety)
- [x] M1: Lock poisoning handled gracefully
- [x] M6: EngineFileStream no unsafe (eagerly loaded)
- [x] M7: Indexing pipeline errors logged
- [x] M11: entry_exists_on_disk uses snapshot
- [x] L2: EntryType::to_kv_type() (deduplicated)
- [x] L5: Dead event branch removed

### Remaining — In Progress
**Batch A: Correctness + Safety**
- [ ] C3: TOCTOU — hold both locks in store_entry (writer + KV)
- [ ] H4: WASM host function permission checks via PermissionResolver
- [ ] H8: flush_batch — hold both locks, journal to hot file
- [ ] H13: Stale .kv detection — compare entry counts, log warning
- [ ] M8: GC crash mid-sweep — document risk, consider two-phase
- [ ] M9: tree_walker cycle detection — add visited set
- [ ] M12: WASM negative len cast — check non-negative before cast
- [ ] M13: WASM unbounded alloc — clamp guest-controlled lengths to 1MB
- [ ] M14: WASM HOST_RESPONSE_OFFSET — document or separate regions
- [ ] M15: store_file_internal not atomic — document orphan recovery via GC

**Batch B: Performance**
- [ ] H14: fsync group commit — batch syncs, skip per-entry fsync with hot file
- [ ] M4: Incremental stats counters — avoid iter_all() in stats()
- [ ] M5: Persistent/COW HashMap for buffer snapshots
- [ ] M19: execute_paginated — push limit into execution, not post-filter
- [ ] M20: NOT query — use NVTMask complement instead of collecting universe

**Batch C: Cleanup**
- [ ] M2: Permission gaps on query/plugin/upload/version/SSE routes
- [ ] M3: B-tree deletion rebalancing
- [ ] M10: ReadSnapshot memory at max stage (~164MB)
- [ ] M16: Duplicate compression detection logic
- [ ] M17: VoidManager grows without bound
- [ ] M18: FieldIndex.values grows without eviction
- [ ] L1: void_manager dead code (#[allow(dead_code)])
- [ ] L3: Redundant is_entry_deleted in version_manager
- [ ] L4: Fuel exhaustion string matching (brittle)
- [ ] L6: path_segments doesn't filter "."/".."
- [ ] L7: update_parent_directories no depth limit
- [ ] L8: Cron malformed json silently returns empty
- [ ] L9: Backup predictable temp file naming
- [ ] L10: CRLF in X-Path response header
- [ ] L11: Hot file no rotation
- [ ] L12: TaskQueue stored as FileRecord type
- [ ] L13: execute_tier2 discards computed bitmap mask

---

## Future Plans (Not Started)

- [ ] Fork merging (true merge with conflict detection)
- [ ] File defragmentation (rewrite file to eliminate voids)
- [ ] Encryption, vaults, zero-knowledge multi-user storage
- [ ] Multi-database sharding
