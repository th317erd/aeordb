# Important Details — AeorDB

## Project Location
- Working directory: `/home/wyatt/Projects/aeordb`
- redb fork: `/tmp/claude/aeordb-research/redb-fork` (with pluggable allocator PR)
- Test databases: `/media/wyatt/Elements/wyatt-desktop/AEORDB-TEST/`

## Architecture (Current — Custom Storage Engine)

### Custom Engine (src/engine/)
- **Append-only WAL-filesystem** — the data file IS the WAL
- **Entry format**: magic 0x0AE012DB, versioned headers, dynamic hash algorithm
- **Six entity types**: Chunk, FileRecord, DirectoryIndex, DeletionRecord, Snapshot, Void
- **NVT**: hash-to-scalar [0.0,1.0] → bucket-based KV block indexing
- **KV Store**: disk-resident bucket pages, lock-free snapshot reads via ArcSwap, Mutex for writes
- **Void management**: deterministic hashes by size, best-fit with splitting
- **StorageEngine**: top-level combining writer, KV manager, void manager
- **DirectoryOps**: store/read/delete files, list directories, parent propagation
- **VersionManager**: forks + snapshots, HEAD management, fast-forward promotion
- **Domain-prefixed hashing**: chunk:, file:, dir:, del:, snap:, ::aeordb:

### HTTP Endpoints
- `/engine/{*path}` — new engine file CRUD (PUT/GET/DELETE/HEAD)
- `/version/snapshot` — create/list/restore/delete snapshots
- `/version/fork` — create/list/promote/abandon forks
- `/fs/{*path}` — legacy redb-based file CRUD (still functional)

### Legacy (src/storage/, src/filesystem/)
- redb-based storage still exists and works (system tables: auth, API keys, etc.)
- Custom B-tree code in backup/ (may be used for indexing engine)

## Performance Baseline
- Custom engine: 102% storage ratio (~2% overhead) ← NEW
- redb baseline: 224% storage ratio (124% waste) ← OLD
- Read: 8ms/file average
- Write: 12.8 files/sec (sequential curl)

## Dependencies
- `blake3` for hashing
- `file-format` for MIME detection (selected, not yet integrated)
- `wasmi` for WASM plugins
- `openraft` for distributed consensus
- `axum` + `tokio` for HTTP

## Test Count: 881 passing (full suite, 2 pre-existing hot-file failures)

## Recently Completed Features
- **Users, Groups, Permissions (crudlify)** — 1,008 tests. Root = nil UUID, query-based groups, per-directory `.permissions`, path walk resolution, group/permissions caching, admin API, emergency reset CLI
- **Selective zstd compression** — 35 tests. Auto-detect by content-type/size, transparent compress/decompress, entry header compression_algo field
- **Auth Provider URI (`--auth` flag)** — 41 tests. `--auth=false` (no auth), `--auth=self` (per-db), `--auth=file://path` (shared identity). E2E verified with two databases sharing identity file
- **NVT bitmap compositing query engine** — 78 tests
- **Custom storage engine** — 273 tests
- **Unified indexing (ScalarConverter + NVT)** — 136 tests
- **File-level version access** — 43 tests. Read files at historical versions (GET ?snapshot=), file history across snapshots (GET /version/file-history/), restore from version with auto-snapshot safety (POST /version/file-restore/)
- **Enhanced directory listing** — 20 tests. Recursive listing with depth control (?depth=), glob filtering (?glob=), content hashes in every entry
- **Soft symlink support** — 78 tests. EntryType::Symlink (0x08), SymlinkRecord, POST /engine-symlink/ endpoint, transparent resolution with cycle detection (MAX_DEPTH=32), nofollow query param, HEAD headers, GC/backup/tree-walker integration
- **Enhanced API keys** — 92 tests. Self-service POST/GET/DELETE /api-keys, scoped permissions via path-glob rules (first-match-wins), mandatory expiration (2yr default, 10yr max), key_id in JWT, permission middleware enforcement (denied=404 not 403), directory listing/query/symlink filtering, ApiKeyCache (LRU+TTL)

## Key Files
- `bot-docs/plan/custom-storage-engine.md` — the full engine design
- `bot-docs/plan/users-groups-permissions.md` — users, groups, crudlify design
- `bot-docs/plan/future-plans.md` — deferred features (cleaned up, only unbuilt items remain)
- `.claude/conversation.md` — design conversation rounds 1-7
- `aeordb-lib/src/auth/provider.rs` — AuthProvider trait, FileAuthProvider, NoAuthProvider
- `aeordb-lib/src/auth/auth_uri.rs` — AuthMode enum, parse_auth_uri
- `aeordb-lib/src/engine/compression.rs` — CompressionAlgorithm, should_compress, compress/decompress
- `aeordb-lib/src/engine/permission_resolver.rs` — CrudlifyOp, path walk resolution
- `aeordb-lib/src/engine/group_cache.rs` — user_id → groups LRU+TTL cache
- `aeordb-lib/src/engine/permissions_cache.rs` — path → PathPermissions LRU+TTL cache
- `aeordb-lib/src/server/portal_routes.rs` — embedded dashboard UI routes + stats API
- `aeordb-lib/src/portal/` — frontend assets (index.html, app.mjs, dashboard.mjs, users.mjs)
- `aeordb-lib/src/engine/fuzzy.rs` — extract_trigrams, trigram_similarity, damerau_levenshtein, jaro_winkler
- `aeordb-lib/src/engine/phonetic.rs` — soundex, dmetaphone_primary, dmetaphone_alt
- `aeordb-lib/src/engine/indexing_pipeline.rs` — IndexingPipeline, parser invocation, source resolution
- `aeordb-lib/src/engine/source_resolver.rs` — resolve_source, walk_path (array-of-segments JSON traversal)
- `aeordb-lib/src/engine/backup.rs` — export_version, create_patch, import_backup
- `aeordb-lib/src/engine/tree_walker.rs` — walk_version_tree, diff_trees, VersionTree, TreeDiff
- `aeordb-lib/src/server/backup_routes.rs` — HTTP export/diff/import/promote endpoints
- `aeordb-lib/src/engine/event_bus.rs` — EventBus (tokio::broadcast), fire-and-forget
- `aeordb-lib/src/engine/engine_event.rs` — EngineEvent, 19 event types, payload structs
- `aeordb-lib/src/engine/request_context.rs` — RequestContext threaded through all engine methods
- `aeordb-lib/src/engine/heartbeat.rs` — 15-second clock-aligned heartbeat with DatabaseStats
- `aeordb-lib/src/server/sse_routes.rs` — GET /events/stream SSE endpoint
- `aeordb-lib/src/engine/webhook.rs` — webhook dispatcher with HMAC-SHA256 signatures
- `aeordb-lib/src/engine/gc.rs` — gc_mark (walk all live roots), gc_sweep (in-place overwrite), run_gc
- `aeordb-lib/src/server/gc_routes.rs` — POST /admin/gc endpoint (root-only, dry_run support)
- `aeordb-lib/src/engine/kv_snapshot.rs` — ReadSnapshot (lock-free immutable KV read view via ArcSwap)
- `aeordb-lib/src/engine/batch_commit.rs` — commit_files (atomic multi-file commit from pre-uploaded chunks)
- `aeordb-lib/src/server/upload_routes.rs` — /upload/config, /upload/check, /upload/chunks/{hash}, /upload/commit
- `aeordb-lib/src/engine/version_access.rs` — resolve_file_at_version (O(depth) targeted path walk), read_file_at_version
- `aeordb-lib/src/server/version_file_routes.rs` — file_history + file_restore HTTP handlers
- `bot-docs/plan/file-level-version-access.md` — design spec for file-level version access
- `aeordb-lib/src/engine/symlink_record.rs` — SymlinkRecord struct, serialize/deserialize, hash functions
- `aeordb-lib/src/engine/symlink_resolver.rs` — resolve_symlink with cycle detection, ResolvedTarget enum
- `aeordb-lib/src/server/symlink_routes.rs` — POST /engine-symlink/{*path} handler
- `aeordb-lib/src/engine/directory_listing.rs` — list_directory_recursive with depth, glob, target field
- `bot-docs/plan/symlink-support.md` — design spec for symlink support
- `aeordb-lib/src/engine/api_key_rules.rs` — KeyRule, match_rules, flag parsing, operation mapping
- `aeordb-lib/src/engine/api_key_cache.rs` — ApiKeyCache (LRU+TTL for scoped key records)
- `aeordb-lib/src/server/api_key_self_service_routes.rs` — POST/GET/DELETE /api-keys handlers
- `bot-docs/plan/enhanced-api-keys.md` — design spec for enhanced API key system
- `aeordb-lib/src/engine/virtual_clock.rs` — VirtualClock trait, SystemClock, MockClock, PeerClockTracker
- `aeordb-lib/src/engine/peer_connection.rs` — PeerManager, PeerConnection, ConnectionState, PeerConfig
- `aeordb-lib/src/engine/merge.rs` — three_way_merge, deterministic LWW, modify-beats-delete
- `aeordb-lib/src/engine/sync_apply.rs` — atomic merge application with chunk verification
- `aeordb-lib/src/engine/sync_engine.rs` — SyncEngine with in-process bidirectional sync
- `aeordb-lib/src/engine/conflict_store.rs` — /.conflicts/ storage, list, resolve, dismiss
- `aeordb-lib/src/engine/cluster_join.rs` — has_signing_key, is_ready_for_traffic, get_cluster_mode
- `aeordb-lib/src/server/sync_routes.rs` — POST /sync/diff, /sync/chunks with cluster secret auth
- `aeordb-lib/src/server/cluster_routes.rs` — GET/POST/DELETE /admin/cluster endpoints
- `aeordb-lib/src/server/conflict_routes.rs` — GET/POST /admin/conflicts, resolve, dismiss
- `bot-docs/plan/replication-v2-design.md` — content-addressed sync replication design
