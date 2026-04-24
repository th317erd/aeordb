# AeorDB — TODO

## Total: 3,594 tests, all passing

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

## Consistency Audit — COMPLETE

### Phase 1: Route Restructuring
- [x] /files/, /links/, /blobs/, /versions/, /sync/, /auth/, /plugins/, /system/
- [x] 43 files modified, 3,282 tests passing

### Phase 2: HTTP Response Headers
- [x] X-AeorDB- prefix on all 7 custom headers

### Phase 3: Config / CLI Unification
- [x] auth.mode, --cors-origins, --host, --jwt-expiry, --chunk-size, TLS in config

### Phase 4: JSON Response Conventions
- [x] total_size → size, type → entry_type, 14 endpoints wrapped in {items: [...]}

### Phase 5: Internal Storage Paths
- [x] /.system/apikeys → api-keys, cluster/sync → sync-peers, startup migration

### Phase 6: Event Names
- [x] tasks_*, syncs_*, gc_started added

### Phase 7: Error Codes
- [x] 12 codes: added PAYLOAD_TOO_LARGE, METHOD_NOT_ALLOWED, SERVICE_UNAVAILABLE

### Phase 8: Error Message Audit
- [x] 235 messages audited, 99 rewritten to be specific and actionable

### Phases 9-11: Docs & Communication
- [x] Client migration report at .claude/client-migration-report.md
- [x] 20 docs/src/ files updated
- [x] Marketing site updated

---

## Enhanced Metrics — COMPLETE

- [x] Phase 1: EngineCounters (16 AtomicU64 fields) + RateTracker (rolling 1m/5m/15m averages)
- [x] Phase 2: All engine operations instrumented (directory_ops, version_manager, gc reconciliation)
- [x] Phase 3: Heartbeat stripped to clock sync, metrics pulse created (15s SSE event)
- [x] Phase 4: Stats API rewritten to O(1), latency histograms instrumented
- [x] Phase 5: Dashboard updated (identity bar, counts/sizes cards, throughput, health indicators)
- [x] Phase 6: Docs updated (events.md, admin.md, client migration report)
- [x] Phase 7: Marketing site updated (Real-Time Monitoring feature card)

---

## Completed: Final Polish

- [x] Migrate plugin routes to /plugins/{name}/invoke (handler sigs, middleware, tests)
- [x] Wire shared web components into portal (crudlify, shared assets via include_str!)
- [x] Fix stress test read mode (discovers existing files before starting workers)

## Completed: Media Parser Metadata Gaps — 16 tests

- [x] Shared EXIF module (exif.rs) with 5 new textual tags + 9 tests
- [x] Image parser rewired to shared EXIF, TIFF EXIF extraction + 2 tests
- [x] MP4/MOV iTunes metadata (title, artist, description, copyright, etc.) + 3 tests
- [x] WAV RIFF INFO chunks (title, artist, comment, copyright, etc.) + 2 tests

## Completed: File Browser Refactor — 6 tests

- [x] Base class (aeor-file-browser-base.js, 872 lines) — tabs, nav, preview, pagination, abstract data access
- [x] Client subclass (aeor-file-browser.js, 206 lines) — sync relationships, drag-out, open-locally
- [x] Portal subclass (aeor-file-browser-portal.js, 158 lines) — direct /files/ API, auto-open, last-tab guard
- [x] Portal files.mjs simplified (14 lines, was 160) — no fetch shim, no monkey-patches
- [x] ZIP download endpoint (POST /files/download) — 6 tests, recursive folders, .system/ filtering

## Completed: Database Corruption Hardening — 12 tests

- [x] Lost+found quarantine module (quarantine_bytes, quarantine_metadata)
- [x] Scanner magic byte search — scans past corrupt headers instead of stopping
- [x] Hash verification on direct reads — detects bit-flipped data
- [x] KV flush resilience — zeros corrupt pages, flags for rebuild
- [x] Storage engine hardening — IO error tolerance, rebuild_kv(), entries_by_type skip
- [x] Graceful directory listing — returns empty on corrupt index
- [x] Admin repair endpoint (POST /system/repair)
- [x] 12 comprehensive corruption tests

## Completed: Database Resilience Features — 9 tests

- [x] Auto-snapshot before GC (`_aeordb_pre_gc_{timestamp}`, keep last 3)
- [x] Verify module (full integrity scan with VerifyReport)
- [x] `aeordb verify` CLI command with `--repair` flag
- [x] Background integrity scanner (1% sample per hour, quarantine on failure)
- [x] Cluster auto-healing framework (stub transport, verification logic complete)
- [x] 9 resilience tests (GC snapshot, verify, metrics, voids, repair)

## Completed: Hot File Transactions — 16 tests

- [x] KV transaction_depth + TransactionGuard RAII
- [x] store_file and delete_file wrapped in transactions
- [x] Orphan recovery on restart (hot file replay + directory re-propagation)
- [x] 16 tests: guard safety, panic/error paths, deadlock prevention, recovery, edge cases

## Future Plans (Not Started)

- [ ] Void reuse (wire find_void into store_entry — fill gaps before appending)
- [ ] Fork merging (true merge with conflict detection)
- [ ] Encryption, vaults, zero-knowledge multi-user storage
- [ ] Multi-database sharding
