# Introduction

AeorDB is a content-addressed file database that treats your data as a filesystem, not as tables and rows. Store any file at any path, query structured fields with sub-millisecond lookups, and version everything with Git-like snapshots and forks -- all from a single binary with zero external dependencies.

## What Makes AeorDB Different

**Content-addressed storage with BLAKE3.** Every piece of data is identified by its cryptographic hash. This gives you built-in deduplication, integrity verification, and a Merkle tree that makes versioning essentially free.

**A filesystem, not a schema.** Data lives at paths like `/users/alice.json` and `/docs/reports/q1.pdf`, organized into directories. No schemas to define, no migrations to run. Store JSON, images, PDFs, or raw bytes -- the engine handles them all the same way.

**Built-in versioning.** Create named snapshots, fork your database into isolated branches, diff between any two versions, and export/import self-contained `.aeordb` files. The content-addressed Merkle tree means historical reads resolve the exact data at the time of the snapshot, not the latest overwrite.

**Native parsers for common formats.** AeorDB includes 8 built-in parsers (text, HTML/XML, PDF, images, audio, video, MS Office, ODF) that run automatically during indexing with zero deployment overhead. Common file types are searchable out of the box.

**WASM plugin system.** Extend the database with WebAssembly plugins for two purposes: *parser plugins* that extract structured fields from custom file formats for indexing, and *query plugins* that run custom logic directly at the data layer. Plugins execute in a sandboxed WASM runtime with configurable memory limits. Native parsers handle common formats; WASM plugins extend to anything else.

**Native HTTP API.** AeorDB exposes its full API over HTTP -- no separate proxy, no client library required. Store files with `PUT`, read them with `GET`, query with `POST /files/query`, and manage versions with the `/versions/*` endpoints. Any HTTP client works.

**Embeddable.** A single `aeordb` binary with no external dependencies. Point it at a `.aeordb` file and you have a running database. Like SQLite, but for files with versioning and a built-in HTTP server.

**Lock-free concurrent reads.** The engine uses snapshot double-buffering via `ArcSwap` so readers never block writers and never see partial state. Queries routinely complete in under a millisecond.

## Key Features

- **Storage:** Append-only WAL file, content-addressed BLAKE3 hashing, automatic zstd compression, 256KB chunking for dedup
- **Indexing:** Scalar bucketing (NVT) with u64, i64, f64, string, timestamp, trigram, phonetic/soundex/dmetaphone index types
- **Native parsers:** 8 built-in parsers (text, HTML/XML, PDF, images, audio, video, MS Office, ODF) -- no WASM deployment needed
- **Querying:** JSON query API with boolean logic (`and`, `or`, `not`), comparison operators, sorting, pagination, projections, aggregations, and zero-config virtual fields for searching by filename, extension, size, and content type
- **Versioning:** Snapshots, forks, diff/patch, export/import as self-contained `.aeordb` files, file-level history and restore
- **Plugins:** WASM parser plugins for custom formats, WASM query plugins for server-side logic
- **Operations:** Background task system, cron scheduler (including automated backups), garbage collection, automatic reindexing
- **Auth:** Self-contained JWT auth, API keys, user/group management with tags, path-level permissions, or `--auth false` for local use
- **TLS:** Native HTTPS via rustls with `--tls-cert` and `--tls-key` flags
- **Configuration:** TOML config file support (`--config`) with 1:1 CLI flag mapping
- **Observability:** O(1) stats at `/system/stats`, Prometheus metrics at `/system/metrics`, real-time `metrics` SSE event, structured logging

## Next Steps

- [Installation](./getting-started/installation.md) -- build from source
- [Quick Start](./getting-started/quick-start.md) -- store, query, and version data in 5 minutes
- [Architecture](./concepts/architecture.md) -- how the engine works under the hood
