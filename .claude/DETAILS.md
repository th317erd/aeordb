# Important Details — AeorDB

## Project Location
- Working directory: `/home/wyatt/Projects/aeordb`
- redb reference clone: `/tmp/claude/aeordb-research/redb`

## Architecture Decisions (Settled — Revision 2)
- **Chunks:** Immutable data blocks. Header (33 bytes: format_version u8, created_at i64, updated_at i64, reserved 16 bytes) + data (BLAKE3 hashed). NO linked list pointers.
- **Files:** Ordered chunk hash lists in B-tree index entries (inline for small, overflow chunk for large)
- **Directories:** Per-directory COW B-trees. Nodes stored as chunks in redb.
- **redb role:** Layer 1 dumb chunk store (hash → bytes). Filesystem B-trees built on top.
- **Versioning:** COW B-tree root hash = entire database state. Bases (I-frames) + diffs (P-frames).
- **Streaming:** ALWAYS. No full-file memory loads. Non-negotiable.
- **No soft-delete:** Delete is real. Recovery via version restore.
- **No linked lists:** Dropped. B-tree owns all structure.
- **Paths:** Everything is a path. Config, indexes, permissions inherit downward via dot-prefix conventions.
- **Parsers:** Plugins at paths. Multiple per directory. Extract fields for indexing.
- **Permissions:** `crudlify` (8 ops), tri-state flags (allow/deny/empty), multi-group links, proximity-ordered resolution.
- **Replication:** openraft. Custom append-only log (NOT redb for Raft log).
- **Query interface:** WASM/native function plugins invoked over HTTP.
- **HTTP server:** axum (on hyper + tokio + tower)
- **Auth:** JWT (Ed25519), API keys (argon2id), magic links, refresh tokens, rate limiting
- **Mandatory fields:** document_id, created_at, updated_at (NO is_deleted)

## Key Plan Documents
- `bot-docs/plan/storage-architecture.md` — Revision 2, the finalized design
- `bot-docs/plan/data-model.md` — paths, parsers, indexes
- `bot-docs/plan/permissions.md` — crudlify system
- `bot-docs/plan/master-plan.md` — top-level overview
- `.claude/conversation.md` — Sprint 2 plan with Wyatt's approvals

## Rust Toolchain
- rustc 1.94.0
- cargo 1.94.0
- clippy 0.1.94

## Test Count: 380 (all passing)
