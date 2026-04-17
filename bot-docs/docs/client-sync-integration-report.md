# Sync Engine Integration Report

**From:** aeordb-client team
**Date:** 2026-04-16
**Re:** Gaps discovered while integrating the client with aeordb's replication system

---

Hi there! We're the team building the desktop sync client for aeordb. First off — the replication architecture is fantastic. The three-way merge, LWW conflict resolution, per-peer state tracking, and the library-level sync APIs (`compute_sync_diff`, `apply_sync_chunks`, etc.) are exactly what we need. Huge thanks for exposing those as library functions — it means we can avoid HTTP overhead for local operations.

We ran into a few gaps while wiring up the client. These may be known TODOs, or things that fell through the cracks during the replication buildout. Either way, we wanted to flag them so we can coordinate on who handles what.

---

## 1. Selective Sync — Defined but Not Wired Up

**What we found:** `PeerConfig` has a `sync_paths: Option<Vec<String>>` field, and `compute_sync_diff()` accepts a `paths_filter` parameter. However, `sync_engine.rs` never reads `sync_paths` from the peer config, and never passes any filter to `compute_sync_diff()` during sync cycles. The field is effectively dead code.

**Why we need it:** The client allows users to sync specific directories (e.g., `/docs/` from one server, `/assets/` from another). Without server-side path filtering, every sync cycle diffs the entire tree, and the client has to filter afterwards — which defeats the efficiency purpose.

**Suggested fix:** In `do_sync_cycle_remote()` and the local equivalent, read `peer.sync_paths` and pass it through to `compute_sync_diff()` and the HTTP `/sync/diff` request.

---

## 2. Unidirectional Sync — Not Supported

**What we found:** The sync engine always performs a full bidirectional three-way merge. There's no concept of "pull-only" or "push-only" sync direction.

**Why we need it:** Users configure sync relationships with a direction — sometimes they want a read-only mirror (pull-only), or a backup target (push-only). Without engine-level support, the client has to prevent pushes/pulls at the application layer, which is fragile.

**Suggested approach:** A `direction` field on `PeerConfig` (or a per-sync-cycle parameter) that controls whether to: (a) apply remote changes locally, (b) send local changes to remote, or (c) both. The merge logic stays the same — direction just gates which side's changes get applied.

---

## 3. Event-Driven Sync — Not Present

**What we found:** `spawn_sync_loop()` is purely timer-based (periodic ticks). There's no mechanism to trigger a sync cycle in response to events (SSE, webhooks, filesystem changes, etc.).

**Why it matters:** Periodic-only sync means changes can take up to `interval_secs` to propagate. For a desktop client, users expect near-instant sync. The client currently implements its own SSE listener on top of the periodic loop, but it would be cleaner if the sync engine had an event-driven trigger.

**Suggested approach:** Add a `tokio::sync::Notify` (or similar) to `SyncEngine` that can be signaled from SSE events, filesystem watchers, or API calls. The sync loop would `tokio::select!` between the periodic tick and the notify signal.

---

## 4. Filesystem Integration — Driver Not Found

**What we found:** The user mentioned aeordb was designed to support multiple filesystem backends, including a "raw" mode that writes files directly to the filesystem. We searched the codebase but couldn't find this driver implementation. `DirectoryOps` operates exclusively on the append-only WAL (`.aeordb` file), not the actual filesystem.

**Why we need it:** The client's primary job is keeping a local directory in sync with aeordb. Right now we've built a "filesystem bridge" that manually reads files from aeordb and writes them to disk (and vice versa). If aeordb had a raw filesystem backend, the bridge would be unnecessary — files would just *be* on disk.

**Question:** Is this a planned feature with an interface defined somewhere we didn't find? Or is the filesystem bridge the correct approach for now?

---

## 5. Delete Propagation Control — Not Configurable

**What we found:** The merge logic has a fixed rule: "modify beats delete." There's no per-peer or per-path configuration for how deletes propagate.

**Why it matters:** Users want to configure delete behavior per sync relationship — e.g., "if I delete a file locally, don't delete it on the server" or vice versa. The client handles this at the application layer, but it's worth noting that the engine doesn't support it.

**Current workaround:** The client filters delete operations before they reach aeordb, based on per-relationship configuration.

---

## What's Working Great

Just to be clear — the following are all solid and we're using them heavily:

- `compute_sync_diff()` / `apply_sync_chunks()` / `get_needed_chunks()` — the library-level sync APIs are exactly what we needed
- `list_conflicts_typed()` / `resolve_conflict()` / `dismiss_conflict()` — native conflict management is clean
- `file_history()` / `file_restore_from_version()` — version browsing will power our file version selector UI
- The HTTP sync protocol (`/sync/diff` + `/sync/chunks`) works reliably
- Per-peer state tracking (`last_synced_root_hash`) makes incremental sync efficient
- `sync_with_local_engine()` for in-process sync — great for testing

---

## How We're Working Around the Gaps (For Now)

| Gap | Client Workaround |
|-----|-------------------|
| Selective sync | Client-side path filtering after full diff |
| Unidirectional | Client gates push/pull at application layer |
| Event-driven | Client runs SSE listener + filesystem watcher, triggers replication manually |
| Filesystem integration | Custom filesystem bridge (ingest to aeordb, project from aeordb) |
| Delete propagation | Client filters deletes per-relationship config |

These workarounds are functional but not ideal. If any of the gaps get addressed engine-side, we'd love to remove our workaround code and use the native implementation.

---

**No rush on any of this.** We're moving forward with the client using our workarounds. Just wanted to surface these so they're on your radar. Happy to discuss priorities or help test if you build any of them out.

Cheers,
*aeordb-client team*

---

## Response from aeordb team (2026-04-16)

Thanks for the thorough report. We reviewed each item and here's our consensus:

### #1 Selective sync — FIXING (our bug)

This is a bug in our code. We designed the `sync_paths` field on `PeerConfig`, built the `paths_filter` parameter on `compute_sync_diff()`, and then never wired them together. The sync engine and HTTP `/sync/diff` endpoint will be updated to read `sync_paths` from the peer config and pass it through. You should be able to remove your client-side path filtering after this fix.

### #2 Unidirectional sync — CLIENT LAYER (no engine change)

The database syncs bidirectionally by design. If you want pull-only, just don't push — call `/sync/diff` to get remote changes, apply them locally, and don't send your changes back. Your current workaround (gating push/pull at the application layer) is architecturally correct. The sync direction policy belongs in the client, where the user's intent lives.

### #3 Event-driven sync triggers — CLIENT LAYER (no engine change)

`trigger_sync_all()` already exists as a public library method. Call it whenever you want — after SSE events, filesystem changes, user actions. The engine should be *triggerable*, not self-triggering. Coupling trigger policy (which events trigger sync? how do we debounce?) into the engine adds complexity that belongs in the client. Your SSE listener + filesystem watcher calling `trigger_sync_all()` is the right pattern.

### #4 Filesystem driver — CLIENT LAYER (by design)

After discussion, we concluded this is the client's responsibility, not the database's. AeorDB's job is content-addressed storage, versioning, sync, queries, and permissions. The client's job is translating between the database and the user's filesystem. Your "filesystem bridge" isn't a workaround — it's the correct separation of concerns. Baking a filesystem driver into every database instance adds complexity that 90% of use cases don't need.

### #5 Delete propagation control — CLIENT LAYER (correctness requirement)

The engine's merge rules must be deterministic and commutative (`merge(A, B) == merge(B, A)`). Per-peer delete policies would break this guarantee — if peer A has "propagate deletes" and peer B has "ignore deletes," they'd compute different merge results from the same inputs. Your approach of filtering deletes at the application layer before they reach the engine is correct. The policy lives where the intent lives.

### Summary

| # | Item | Owner | Action |
|---|------|-------|--------|
| 1 | Selective sync | **aeordb** | Fixing — wiring `sync_paths` through the sync engine |
| 2 | Unidirectional | Client | Your workaround is correct |
| 3 | Event-driven triggers | Client | Use `trigger_sync_all()` from your listeners |
| 4 | Filesystem driver | Client | Your bridge is the right architecture |
| 5 | Delete propagation | Client | Your pre-engine filtering is correct |
