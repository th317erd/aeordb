# AeorDB — TODO

## Unified Indexing — COMPLETE

- [x] Task 1: ScalarConverter trait + 9 built-in converters (41 tests)
- [x] Task 2: NVT refactored to use ScalarConverter (3 new tests)
- [x] Task 3: Old indexing module removed
- [x] Task 4: Index file storage at .indexes/ (21 tests)
- [x] Task 5: Write pipeline — store → parse JSON → update indexes (20 tests)
- [x] Task 6: Query engine — chainable builder, intersection, limit (17 tests)
- [x] Task 7: HTTP POST /query endpoint (16 tests)
- [x] Task 8: WASM converter stub + batch API interface (18 tests)

## Test Count: 621 (all passing)

## What's Built

### Custom Storage Engine (self-hosting, no redb)
- Append-only WAL-filesystem with NVT + KV store
- 6 entity types, domain-prefixed hashing, void management
- Forks + snapshots versioning, HEAD management
- ~2% storage overhead (vs redb's 124%)

### Unified Indexing
- ScalarConverter trait: any value → [0.0, 1.0]
- 10 converters: Hash, U8-U64, I64, F64, String, Timestamp, WASM stub
- NVT with pluggable converters
- Write pipeline: store → parse → index
- Query engine with chainable builder
- HTTP POST /query endpoint

### Infrastructure
- axum HTTP server with JWT auth (Ed25519)
- API keys, magic links, refresh tokens, rate limiting
- WASM plugin runtime (wasmi) + native plugin loading
- Prometheus metrics + structured logging
- Stress testing tool (CLI)

## What's Next (see bot-docs/plan/future-plans.md)
- Auth Provider URI system (--auth flag)
- Server-side compilation + in-database SDK
- Schema-as-code (proc macros)
- Functions as endpoints with arguments
- HTTP-to-DB user mapping + crudlify permissions
- Encryption, vaults, zero-knowledge storage
- Garbage collection + cron tasks
