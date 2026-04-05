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
- **KV Store**: sorted hash→offset array at front of file
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

## Test Count: 1,310 (all passing)

## Recently Completed Features
- **Users, Groups, Permissions (crudlify)** — 1,008 tests. Root = nil UUID, query-based groups, per-directory `.permissions`, path walk resolution, group/permissions caching, admin API, emergency reset CLI
- **Selective zstd compression** — 35 tests. Auto-detect by content-type/size, transparent compress/decompress, entry header compression_algo field
- **Auth Provider URI (`--auth` flag)** — 41 tests. `--auth=false` (no auth), `--auth=self` (per-db), `--auth=file://path` (shared identity). E2E verified with two databases sharing identity file
- **NVT bitmap compositing query engine** — 78 tests
- **Custom storage engine** — 273 tests
- **Unified indexing (ScalarConverter + NVT)** — 136 tests

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
