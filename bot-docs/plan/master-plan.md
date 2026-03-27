# AeorDB — Master Plan

The database that says "NO" to every problem current databases have.

---

## Vision

A Rust-native database engine that solves the 12 fundamental problems of existing databases by rethinking storage, indexing, and query execution from first principles. Designed for pluggability, adaptivity, and zero-ceremony developer experience.

---

## Core Architecture Groups

### 1. [Storage Engine (Content-Addressed Chunk Store)](./storage-engine.md)
**Status:** In Design

ALL data is content-addressed chunks (configurable power-of-two size). Files, indexes, blobs, metadata — everything is chunks keyed by hash. Versioning, dedup, and diff-only replication are structural properties. Physical storage backend is pluggable.

### 2. [Indexing Engine](./indexing-engine.md)
**Status:** Not Started

Adaptive, automatic indexing that observes query patterns and builds/adjusts indexes without human intervention.

### 3. [Query Engine](./query-engine.md)
**Status:** In Design

No query language. Compiled functions deployed to the database hierarchy and invoked over HTTP(S) with arguments. Compute happens at the data, only results return to the caller.

### 4. [Schema System](./schema-system.md)
**Status:** Not Started

A schema system that finds the sweet spot between relational rigidity and document chaos.

### 5. [Concurrency & Transactions](./concurrency.md)
**Status:** Not Started

Concurrency control that doesn't generate garbage, create deadlocks, or waste work.

### 6. [Replication & Distribution](./replication.md)
**Status:** In Design

openraft for Raft consensus. redb as state machine. Custom append-only log for Raft entries. Same binary scales from single-node to distributed cluster. No "eventual consistency" — provable Raft guarantees.

### 7. [Type System](./type-system.md)
**Status:** Not Started

Rich, extensible data types — not just primitives and JSON. Graphs, time-series, geospatial, all native.

### 8. [Observability](./observability.md)
**Status:** Not Started

When something is slow, the database tells you WHY and HOW TO FIX IT. Not scripture — answers.

### 9. [Developer Experience](./developer-experience.md)
**Status:** Not Started

Zero-ceremony local development. Point at a file and go (SQLite-style). No DBA required.

### 10. [Compression & Efficiency](./compression.md)
**Status:** Not Started

Largely delegated to the storage plugin, but the database layer also has a role in data layout and encoding.

---

## Design Principles

1. **Pluggable over monolithic** — Core systems are interfaces, not implementations
2. **Adaptive over static** — The database observes and adjusts, humans shouldn't have to babysit
3. **Explicit over implicit** — No magic. Clear trade-offs. User chooses.
4. **Zero-ceremony DX** — Point at a file and go. Scale up when you need to.
5. **Honest guarantees** — Don't promise what you can't deliver. Be clear about trade-offs.
6. **Fail fast, fail loud** — No silent corruption. No swallowed errors.
7. **Embed the filesystem** — Storage engines ARE filesystems. Leverage decades of existing work.

---

### 11. [HTTP Server & Authentication](./http-server-and-auth.md)
**Status:** In Design

axum-based HTTP(S) server. Token-based auth: API keys, magic links, JWT (Ed25519). Per-cell permissions via rule functions (conceptual).

---

## Resolved Decisions

- **Storage:** Content-addressed chunk store. Everything is chunks. No separate blob store.
- **WAL:** Raft log (custom append-only file) serves as WAL. Chunk store immutability provides crash safety.
- **Wire protocol:** HTTP(S). No custom protocol. `curl` is a valid client.
- **Embedded + client-server:** Same binary. Single-node embedded mode or multi-node distributed.
- **Auth:** API-first. JWT tokens. Ed25519 signatures. Stateless validation.
- **Indexing:** User-requested only. No default indexes. Multiple algorithms per column. Pluggable via WASM/native.
- **Mandatory fields:** `document_id`, `created_at`, `updated_at`, `is_deleted` on every document.
- **Versioning:** Free via content-addressed hash maps. Every state is a snapshot. Restore any version.

## Open Questions

- [ ] Licensing model (open source, which license?)
- [ ] Per-cell permission rule mechanism (conceptual, needs design)
- [ ] WASM runtime selection for untrusted plugins (wasmi vs alternatives)

---

## Project Structure

```
aeordb/
  aeordb-lib/         # Core database library
  aeordb-cli/         # Command-line interface
  bot-docs/
    docs/             # Research and context
    plan/             # Architecture and design (this directory)
    test/             # Plan validation tests
```
