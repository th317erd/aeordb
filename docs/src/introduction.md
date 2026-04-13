# Introduction

AeorDB is a content-addressed file database that treats your data as a filesystem, not as tables and rows. Store any file at any path, query structured fields with sub-millisecond lookups, and version everything with Git-like snapshots and forks -- all from a single binary with zero external dependencies.

## What Makes AeorDB Different

**Content-addressed storage with BLAKE3.** Every piece of data is identified by its cryptographic hash. This gives you built-in deduplication, integrity verification, and a Merkle tree that makes versioning essentially free.

**A filesystem, not a schema.** Data lives at paths like `/users/alice.json` and `/docs/reports/q1.pdf`, organized into directories. No schemas to define, no migrations to run. Store JSON, images, PDFs, or raw bytes -- the engine handles them all the same way.

**Built-in versioning.** Create named snapshots, fork your database into isolated branches, diff between any two versions, and export/import self-contained `.aeordb` files. The content-addressed Merkle tree means historical reads resolve the exact data at the time of the snapshot, not the latest overwrite.

**WASM plugin system.** Extend the database with WebAssembly plugins for two purposes: *parser plugins* that extract structured fields from non-JSON files (PDFs, images, XML) for indexing, and *query plugins* that run custom logic directly at the data layer. Plugins execute in a sandboxed WASM runtime with configurable memory limits.

**Native HTTP API.** AeorDB exposes its full API over HTTP -- no separate proxy, no client library required. Store files with `PUT`, read them with `GET`, query with `POST /query`, and manage versions with the `/version/*` endpoints. Any HTTP client works.

**Embeddable.** A single `aeordb-cli` binary with no external dependencies. Point it at a `.aeordb` file and you have a running database. Like SQLite, but for files with versioning and a built-in HTTP server.

**Lock-free concurrent reads.** The engine uses snapshot double-buffering via `ArcSwap` so readers never block writers and never see partial state. Queries routinely complete in under a millisecond.

## Key Features

- **Storage:** Append-only WAL file, content-addressed BLAKE3 hashing, automatic zstd compression, 256KB chunking for dedup
- **Indexing:** Scalar bucketing (NVT) with u64, i64, f64, string, timestamp, trigram, phonetic/soundex/dmetaphone index types
- **Querying:** JSON query API with boolean logic (`and`, `or`, `not`), comparison operators, sorting, pagination, projections, and aggregations
- **Versioning:** Snapshots, forks, diff/patch, export/import as self-contained `.aeordb` files
- **Plugins:** WASM parser plugins for any file format, WASM query plugins for custom data-layer logic
- **Operations:** Background task system, cron scheduler, garbage collection, automatic reindexing
- **Auth:** Self-contained JWT auth, API keys, user/group management, path-level permissions, or `--auth false` for local use
- **Observability:** Prometheus metrics at `/admin/metrics`, SSE event stream at `/events/stream`, structured logging

## Next Steps

- [Installation](./getting-started/installation.md) -- build from source
- [Quick Start](./getting-started/quick-start.md) -- store, query, and version data in 5 minutes
- [Architecture](./concepts/architecture.md) -- how the engine works under the hood
