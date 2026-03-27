# Important Details — AeorDB

## Project Location
- Working directory: `/home/wyatt/Projects/aeordb`
- redb reference clone: `/tmp/claude/aeordb-research/redb`

## Architecture Decisions (Settled)
- **Storage:** Content-addressed chunk store (power-of-two configurable chunks, keyed by hash)
- **Initial storage backend:** redb (single-file, to be replaced/augmented by chunk store in Phase 4)
- **Replication:** openraft (Raft consensus) with custom append-only log (NOT redb for Raft log)
- **Query interface:** Compiled function plugins (WASM sandboxed + native dlopen trusted)
- **HTTP server:** axum (on hyper + tokio + tower)
- **Auth:** JWT (Ed25519 signing), API keys (argon2id hashed), magic links
- **Indexing:** User-requested only. Scalar ratio indexing as primary algorithm. Pluggable via WASM/native.
- **Mandatory fields:** `document_id`, `created_at`, `updated_at`, `is_deleted` on every document
- **Versioning:** Free via content-addressed hash maps — every state is a snapshot
- **Permissions:** Rules are WASM plugins returning allow/deny/redact, same interface as query functions

## Key Research Findings
- No existing open-source FS can run both embedded and distributed (gap in ecosystem)
- redb has 3 GiB max value size, no streaming API, COW write amplification on large values
- redb already has a `StorageBackend` trait (6 methods: len, read, write, set_len, sync_data, close)
- openraft: use redb for state machine, NOT for Raft log (12x slower than append-optimized stores)
- WASM runtimes: wasmi is the leading candidate (pure Rust, small, ~5x interpreter overhead)
- IBM Storage Scale eliminated (not fully open source)
- ZFS eliminated for default backend (requires kernel module + root)

## Rust Toolchain
- rustc 1.94.0
- cargo 1.94.0
- clippy 0.1.94

## Bot-Docs Structure
- `bot-docs/docs/` — research artifacts
- `bot-docs/plan/` — architecture and design documents
- `bot-docs/test/` — plan validation (not yet used)

## Conversation Continuation
- Async collaboration via `.claude/conversation.md`
- Wyatt reviews and adds inline comments
- Claude updates plans based on feedback
