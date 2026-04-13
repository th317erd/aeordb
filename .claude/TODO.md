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

## Audit Fixes — Critical (do first)

- [ ] C1: Backup routes lack root auth — any authenticated user can export/import/promote
- [ ] C2: process::exit(1) in init_hot_file — bypasses Drop, corrupts DB
- [ ] C3: TOCTOU between append-writer write and KV insert — data loss on crash
- [ ] C4: WASM plugin recompiled on every invoke — cache compiled modules
- [ ] C5: get_entry takes WRITE lock for reads — serializes all reads with all writes

## Audit Fixes — High Security

- [ ] H1: Snapshot/fork routes lack root checks — any user can restore/promote
- [ ] H2: Magic link JWT uses email as sub — bypasses all permission checks
- [ ] H3: WASM host functions use RequestContext::system() — should use caller's context
- [ ] H4: WASM host functions perform zero permission checks
- [ ] H5: normalize_path doesn't resolve ".." segments — path traversal
- [ ] H6: Plugin-controlled HTTP response headers — injection risk
- [ ] H7: 10GB body limit on all routes — DoS via memory exhaustion

## Audit Fixes — High Correctness

- [ ] H8: flush_batch same TOCTOU gap, no hot file journaling
- [ ] H9: insert() silently discards flush errors (let _ = self.flush())
- [ ] H10: Drop impl silently discards flush errors — undetected data loss
- [ ] H11: GC mark doesn't mark DeletionRecords — KV rebuild resurrects deleted files
- [ ] H12: GC sweep not atomic with writes — concurrent write can be swept
- [ ] H13: Stale .kv detection insufficient — only checks empty vs non-empty

## Audit Fixes — High Performance

- [ ] H14: fsync per entry append — group commit would yield 10-100x improvement

## Audit Fixes — Medium

- [ ] M1: Lock poisoning panics (10+ locations use .expect()/.unwrap() on locks)
- [ ] M2: Permission gaps on query/plugin/upload/version/SSE routes
- [ ] M3: B-tree deletion doesn't rebalance — degrades to O(n)
- [ ] M4: iter_all() full scan used by stats(), entries_by_type(), GC
- [ ] M5: publish_buffer_only HashMap clone per insert — 54MB/sec churn
- [ ] M6: Unsafe raw pointer in EngineFileStream — should use Arc
- [ ] M7: Indexing pipeline errors silently discarded
- [ ] M8: GC crash mid-sweep leaves partially overwritten entries
- [ ] M9: tree_walker has no cycle detection — infinite recursion possible
- [ ] M10: Unbounded memory in ReadSnapshot at max KV stage (~164MB)
- [ ] M11: entry_exists_on_disk redundant disk read (snapshot has the data)
- [ ] M12: WASM memory: negative len cast wraps to huge usize, OOM
- [ ] M13: WASM memory: unbounded allocation from guest-controlled length
- [ ] M14: HOST_RESPONSE_OFFSET=0 overlaps request data in WASM memory
- [ ] M15: store_file_internal not atomic — partial failure leaves orphans
- [ ] M16: Duplicate compression detection logic in two store methods
- [ ] M17: VoidManager grows without bound (never compacted)
- [ ] M18: FieldIndex.values HashMap grows without eviction
- [ ] M19: execute_paginated fetches ALL results then paginates in memory
- [ ] M20: NOT query collects all hashes as universe — expensive

## Audit Fixes — Low/Info

- [ ] L1: Dead code: void_manager #[allow(dead_code)] never used for reuse
- [ ] L2: Duplicated EntryType→KV_TYPE mapping in 4 places
- [ ] L3: Redundant is_entry_deleted checks in version_manager
- [ ] L4: Fuel exhaustion detection via string matching (brittle)
- [ ] L5: Dead event branch (overwrite vs new file — same value)
- [ ] L6: path_segments doesn't filter "." or ".."
- [ ] L7: update_parent_directories recursion — no depth limit
- [ ] L8: Cron: malformed cron.json silently returns empty
- [ ] L9: Backup: predictable temp file naming (symlink attack)
- [ ] L10: CRLF in X-Path response header from user input
- [ ] L11: Hot file no rotation (always -hot001)
- [ ] L12: TaskQueue stored as FileRecord type — confused with real files
- [ ] L13: execute_tier2 computes bitmap mask but discards it

---

## Future Plans (Not Started)

- [ ] Cron/background task system enhancements
- [ ] Fork merging (true merge with conflict detection)
- [ ] File defragmentation (rewrite file to eliminate voids)
- [ ] Encryption, vaults, zero-knowledge multi-user storage
- [ ] Multi-database sharding
