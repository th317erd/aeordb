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

## Test Count: 785 (all passing, zero clippy warnings)

## Key Files
- `bot-docs/plan/custom-storage-engine.md` — the full engine design
- `bot-docs/plan/future-plans.md` — deferred features
- `.claude/conversation.md` — design conversation rounds 1-7
