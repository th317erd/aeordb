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

### All 38 Audit Items — COMPLETE
- [x] Batches A+B+C all fixed: TOCTOU, cycle detection, WASM safety, fsync, cleanup
- [x] 20 functional code fixes + 18 documentation/deferred items
---

## Consistency Audit — In Progress

### Phase 1: Route Restructuring
- [ ] /files/ routes (PUT/GET/DELETE/HEAD /engine/ → /files/, PATCH rename, /files/query)
- [ ] /links/ routes (PUT/GET/DELETE /links/, replaces /engine-symlink/)
- [ ] /blobs/ routes (/upload/ → /blobs/, /engine/_hash/ → /blobs/)
- [ ] /versions/ routes (/version/ → /versions/, backup routes moved here)
- [ ] /sync/ routes (/admin/cluster/ → /sync/, conflicts moved here)
- [ ] /auth/ routes (/api-keys → /auth/keys, admin keys → /auth/keys/admin)
- [ ] /plugins/ routes (/{db}/{schema}/{table}/ → /files/plugins/ + /plugins/)
- [ ] /system/ routes (/admin/ → /system/, portal, events, stats)
- [ ] Deprecated route 404 tests

### Phase 2: HTTP Response Headers
- [ ] X-AeorDB- prefix on all custom headers

### Phase 3: Config / CLI Unification
- [ ] auth.mode, --cors-origins, new CLI flags, config 1:1 mapping

### Phase 4: JSON Response Conventions
- [ ] total_size → size, type → entry_type
- [ ] Wrap collections in {items: [...]}

### Phase 5: Internal Storage Paths
- [ ] /.system/apikeys → api-keys, cluster/sync → sync-peers, migration

### Phase 6: Event Names
- [ ] Pluralize task_*/sync_*, add gc_started

### Phase 7: Error Codes
- [ ] Add PAYLOAD_TOO_LARGE, METHOD_NOT_ALLOWED, SERVICE_UNAVAILABLE
- [ ] Remove SYSTEM_BOUNDARY → FORBIDDEN

### Phase 8: Error Message Audit
- [ ] Audit and rewrite all vague ErrorResponse messages

### Phase 9-11: Docs & Communication
- [ ] Client migration report
- [ ] Update docs/src/ (29 files)
- [ ] Update aeordb-www marketing site

---

## Future Plans (Not Started)

- [ ] Fork merging (true merge with conflict detection)
- [ ] File defragmentation (rewrite file to eliminate voids)
- [ ] Encryption, vaults, zero-knowledge multi-user storage
- [ ] Multi-database sharding
