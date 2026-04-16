# AeorDB Replication v2 — Content-Addressed Sync Design

**Date:** 2026-04-15
**Status:** Design spec
**Replaces:** replication-design.md (openraft approach — abandoned in favor of this simpler model)

---

## Overview

Multi-node replication for AeorDB using content-addressed sync instead of Raft consensus. Every node is a full peer — any node can accept writes. Nodes sync by comparing directory tree hashes and exchanging missing chunks. Conflicts are detected, preserved, and resolved as first-class database entities.

This approach leverages AeorDB's existing content-addressed architecture: immutable chunks, hash-based dedup, tree diffing, and the existing client sync model. The replication protocol is the same protocol the aeordb-client uses — a node is just a client with a `.aeordb` file instead of a local filesystem.

## Design Principles

- **Any node can write** — no leader, no quorum, no election
- **Eventually consistent** — nodes converge to identical state after sync
- **Content-addressed sync** — exchange chunks by hash, merge directory trees
- **Conflicts are first-class** — detected, preserved in `/.conflicts/`, resolved explicitly
- **Modify beats delete** — safety-first conflict resolution (work is never silently lost)
- **Deterministic merge** — same inputs → same result on every node
- **Selective sync** — nodes can sync specific path subtrees only
- **Client = node** — the same protocol works for server↔server, client↔server, and client↔client

## Why Not Raft

Raft provides strong consistency with a single-leader write model. For AeorDB's target audience (creative teams, legal, marketing), this is the wrong tradeoff:

- **Raft requires a quorum** — 2 of 3 nodes must be alive for writes. Our users need to keep working when disconnected.
- **Raft has a single writer** — all writes go through the leader. Our users write on their local machines, in field offices, on the road.
- **Raft struggles with large files** — consensus on 1GB log entries is painful. Our users work with video, PSDs, and large assets.
- **Raft is complex** — leader election, term numbers, log compaction, membership changes. Content-addressed sync is dramatically simpler.

Our approach sacrifices strong consistency for availability and simplicity. For a file database serving creative professionals, this is the correct tradeoff.

---

## Ordering and Determinism

### Virtual Clock

All nodes sync time via the existing heartbeat mechanism. Each entry stores:

```rust
pub struct EntryMeta {
    pub virtual_time: u64,  // heartbeat-synced clock (ms)
    pub node_id: u64,       // unique node identifier
}
```

The ordering tuple `(virtual_time, node_id)` provides a **total deterministic order** across all entries from all nodes. When two entries compete:

1. Higher `virtual_time` wins
2. If tied, higher `node_id` wins (arbitrary but deterministic)

This ordering doesn't need to reflect true real-world sequence. It needs to be:
- **Deterministic** — every node picks the same winner given the same inputs
- **Stable** — the ordering doesn't change after the fact
- **Close enough** — in the common case (writes separated by >10ms), it reflects real order

### Why This Is Sufficient

For the 99.9% case: writes to different paths have no conflict. Order doesn't matter.

For the 0.1% case: two nodes write the same path within milliseconds. The virtual clock picks a deterministic winner. The "loser" is preserved in `/.conflicts/`. Nobody's work is lost. The user resolves the conflict when convenient.

---

## Sync Protocol

### Overview

Sync is a bidirectional operation between two peers:

1. **Diff** — compare directory trees, identify what changed
2. **Exchange** — transfer missing chunks
3. **Merge** — combine changes into a unified HEAD, detecting conflicts

### Sync Endpoint

```
POST /sync/diff
Content-Type: application/json

{
    "since_root_hash": "abc123...",   // the last root hash we synced to
    "current_root_hash": "def456...", // our current HEAD
    "paths": ["/assets/**"],          // optional: selective sync filter
    "node_id": 2,                     // requesting node's ID
    "virtual_time": 1776208000000     // requesting node's current virtual clock
}
```

**Response:**

```json
{
    "root_hash": "789abc...",
    "changes": {
        "files_added": [
            {"path": "/assets/new.psd", "hash": "...", "size": 1024, "virtual_time": 1776208000100, "node_id": 1}
        ],
        "files_modified": [
            {"path": "/assets/logo.psd", "hash": "...", "size": 2048, "virtual_time": 1776208000200, "node_id": 1}
        ],
        "files_deleted": [
            {"path": "/assets/old.txt", "virtual_time": 1776208000050, "node_id": 1}
        ],
        "symlinks_added": [...],
        "symlinks_modified": [...],
        "symlinks_deleted": [...]
    },
    "chunk_hashes_needed": ["aaa...", "bbb...", "ccc..."]
}
```

The response uses the existing `diff_trees` mechanism, filtered by the optional `paths` parameter.

### Chunk Transfer

Chunks are fetched via the existing endpoint:

```
GET /engine/_hash/{hex_hash}
```

Or a batch endpoint for efficiency:

```
POST /sync/chunks
Content-Type: application/json

{
    "hashes": ["aaa...", "bbb...", "ccc..."]
}
```

Response: multipart binary with each chunk, or a streaming format.

Since chunks are content-addressed and immutable, they can be fetched from ANY peer that has them — not just the node that created them. This enables fan-out: new nodes can fetch popular chunks from multiple peers simultaneously.

### Sync Triggers

1. **SSE events** — each node subscribes to peers' `/events/stream`. When a write event arrives, a sync cycle is triggered.
2. **Periodic fallback** — every N seconds (configurable, e.g. 30s), a full sync cycle runs regardless of events. Catches missed SSE events.
3. **On-demand** — `POST /admin/sync` triggers an immediate sync cycle.

### Sync Flow (Between Two Peers)

```
Node A                           Node B
  |                                |
  |-- POST /sync/diff ----------->|  "here's my HEAD, what's new?"
  |<-- changes + chunk list ------|
  |                                |
  |-- GET /sync/chunks ---------->|  "send me these chunks"
  |<-- chunk data ----------------|
  |                                |
  |   [merge changes locally]      |
  |   [detect conflicts]           |
  |   [update HEAD]                |
  |                                |
  |-- POST /sync/diff ----------->|  "here's my NEW head (with merged changes)"
  |<-- "nothing new" -------------|  (or: changes that A has that B doesn't)
  |                                |
```

Bidirectional sync is two rounds: A pulls from B, then B pulls from A (or A pushes its changes to B in the response).

---

## Directory Merge Algorithm

The merge algorithm takes two diverged directory trees and produces a single merged tree with conflicts flagged.

### Inputs

- **Base tree** — the last common state (identified by the `since_root_hash`)
- **Local tree** — this node's current HEAD
- **Remote tree** — the peer's current HEAD

### Algorithm

This is a three-way merge (like Git), using the base as the common ancestor:

1. Compute `local_diff = diff_trees(base, local_tree)` — what we changed
2. Compute `remote_diff = diff_trees(base, remote_tree)` — what they changed
3. For each path in the union of both diffs:

   **No conflict (most common):**
   - Path only in local_diff → keep our change
   - Path only in remote_diff → apply their change

   **Potential conflict (same path in both diffs):**
   - Both added/modified the same path:
     - If same content hash → no conflict (same change on both sides)
     - If different content hash → **CONFLICT**: LWW by `(virtual_time, node_id)`, loser stored in `/.conflicts/`
   - One modified, one deleted:
     - **Modify wins** (safety-first — work is never silently lost)
     - The deletion is recorded as a conflict for visibility
   - Both deleted → no conflict, it's deleted

4. Apply all non-conflicting changes
5. Store conflict entries in `/.conflicts/`
6. Update HEAD to the merged root hash

### Determinism Guarantee

The merge algorithm MUST be commutative: `merge(A_changes, B_changes) = merge(B_changes, A_changes)`. This is guaranteed because:
- The conflict resolution rule is based on `(virtual_time, node_id)` which is a total order
- The "modify beats delete" rule is symmetric (doesn't depend on which node is "local" vs "remote")
- Content hashing means the same merged content produces the same hashes

After both nodes independently merge, they arrive at the same HEAD. No further sync rounds needed.

---

## Conflict System

### Storage

Conflicts are stored as regular database entries under a hidden `/.conflicts/` path:

```
/.conflicts/
    assets/
        logo.psd/
            node-1           → FileRecord pointing to Node 1's version (chunks exist)
            node-2           → FileRecord pointing to Node 2's version (chunks exist)
            .meta            → JSON: conflict metadata
```

The `.meta` file contains:
```json
{
    "path": "/assets/logo.psd",
    "conflict_type": "concurrent_modify",
    "auto_winner": "node-2",
    "auto_winner_reason": "higher virtual_time (1776208000200 > 1776208000100)",
    "created_at": 1776208000300,
    "versions": [
        {
            "node_id": 1,
            "virtual_time": 1776208000100,
            "hash": "aaa...",
            "size": 1024
        },
        {
            "node_id": 2,
            "virtual_time": 1776208000200,
            "hash": "bbb...",
            "size": 2048
        }
    ]
}
```

### Conflict Lifecycle

1. **Detection** — during merge, conflicting writes to the same path are identified
2. **Auto-resolution** — LWW picks a winner, which becomes the "current" version at the real path
3. **Preservation** — the losing version is stored in `/.conflicts/` with full metadata
4. **Visibility** — API endpoint `GET /admin/conflicts` lists all unresolved conflicts
5. **User resolution** — user reviews and picks a version (or keeps the auto-winner)
6. **Cleanup** — resolved conflicts are deleted from `/.conflicts/`

### Conflicts Sync

Since conflicts are stored as regular database entries (files with chunks), they sync automatically using the same mechanism. Every node sees the same conflicts. A conflict resolved on any node syncs the resolution to all nodes.

### Conflict Resolution API

```
GET  /admin/conflicts                    → list all unresolved conflicts
GET  /admin/conflicts/{path}             → get conflict details for a path
POST /admin/conflicts/{path}/resolve     → resolve: {"pick": "node-1"} or {"pick": "node-2"}
POST /admin/conflicts/{path}/dismiss     → accept auto-winner, remove conflict entry
```

Resolution writes the chosen version to the real path and deletes the conflict entries. Since this is a normal write, it syncs to all nodes.

---

## Conflict Resolution Hierarchy

| Scenario | Resolution | Loser preserved? |
|----------|-----------|------------------|
| Different paths | No conflict — merge both | N/A |
| Same path, both modify, same hash | No conflict — identical change | N/A |
| Same path, both modify, different hash | LWW by `(virtual_time, node_id)` | Yes, in `/.conflicts/` |
| Same path, one modify + one delete | **Modify wins** | Deletion recorded as conflict |
| Same path, both delete | No conflict — deleted on both | N/A |
| Same path, both create (new file) | LWW by `(virtual_time, node_id)` | Yes, in `/.conflicts/` |

---

## Selective Sync

Nodes can sync specific path subtrees only. The sync endpoint accepts a `paths` parameter with glob patterns:

```json
{
    "paths": ["/assets/**", "/docs/**"]
}
```

The diff computation filters to only include entries under the requested paths. Chunks are only fetched for matching entries. The merge only touches the specified subtrees.

### Use Cases

- **Client syncs only its working directory** — designer binds `/assets/` from the server
- **Regional office syncs only its projects** — `/projects/us-west/**`
- **Backup node syncs everything** — no paths filter (full sync)
- **Server-to-server selective** — replicate only specific content to a CDN edge node

### Configuration

Selective sync is configured per peer in the cluster config:

```json
{
    "peers": [
        {
            "node_id": 2,
            "address": "https://node2:3000",
            "label": "us-west office",
            "sync_paths": ["/projects/us-west/**", "/shared/**"]
        },
        {
            "node_id": 3,
            "address": "https://node3:3000",
            "label": "backup",
            "sync_paths": null   // null = full sync
        }
    ]
}
```

---

## Peer Management

### Cluster Configuration

Stored in system tables:

```
::aeordb:cluster:node_id      → u64 (generated once, persisted)
::aeordb:cluster:mode          → "standalone" | "cluster"
::aeordb:cluster:secret_hash   → BLAKE3 hash of the cluster secret
::aeordb:cluster:peers         → JSON array of peer configs
```

### API

```
GET    /admin/cluster              → cluster status (mode, node_id, peers, sync state)
POST   /admin/cluster/peers        → add a peer
DELETE /admin/cluster/peers/{id}   → remove a peer
POST   /admin/cluster/sync         → trigger immediate sync with all peers
POST   /admin/cluster/sync/{id}    → trigger sync with specific peer
```

### CLI

```bash
# Start standalone (default)
aeordb start -D data.aeordb

# Start with peers
aeordb start -D data.aeordb --peers "node2:3000,node3:3000" --cluster-secret-file /etc/aeordb/cluster.key

# Add a peer at runtime
curl -X POST http://localhost:3000/admin/cluster/peers \
  -d '{"address": "https://node2:3000", "label": "us-west"}'
```

### Inter-Node Authentication

Peers authenticate using a shared cluster secret:
- `--cluster-secret "mysecret"` for development
- `--cluster-secret-file /path/to/secret` for production (read from file, zeroed from memory)
- Sent as `X-Cluster-Secret` header on all `/sync/*` endpoints
- Endpoints reject requests without a valid secret

### Node Join Flow

1. New node starts with `--peers "existing:3000"`
2. Contacts existing node's `/admin/cluster/peers` to register itself
3. Triggers a full sync (no `since_root_hash` → receives entire tree)
4. Once synced, begins normal bidirectional sync cycles
5. No special bootstrap — just a sync with no prior state

---

## Auth in Cluster Mode

Each node shares the same JWT signing key. The signing key is stored in system tables and syncs like any other data.

### Bootstrap

1. First node generates signing key on first boot (existing behavior)
2. Second node joins, syncs, receives the signing key as part of the tree
3. Before accepting client traffic, the joining node verifies it has a valid signing key

### Constraint

A joining node MUST NOT accept client HTTP traffic until its first sync completes and a valid JWT signing key is present in its system tables. This prevents:
- Generating a conflicting signing key
- Accepting JWTs signed by a key that will be overwritten
- Serving stale auth data

---

## Implementation Phases

### Prerequisite 0: Legacy Cleanup (FIRST step)

Remove all openraft scaffolding and legacy storage code. This is purely subtractive — no new code, just deletion.

**Remove:**
- `src/replication/` — all 6 files (mod.rs, types.rs, raft_node.rs, network.rs, log_store.rs, state_machine.rs)
- `spec/replication/raft_spec.rs` — openraft test file
- `src/storage/` — all 4 files (mod.rs, chunk.rs, chunk_config.rs, chunk_header.rs, chunk_storage.rs) — legacy chunk storage trait, only consumed by replication
- `src/engine/engine_chunk_storage.rs` — bridge from new engine to legacy ChunkStorage trait, only consumed by replication
- `pub mod replication;` from `lib.rs`
- `pub mod storage;` from `lib.rs`
- `pub mod engine_chunk_storage;` and `pub use engine_chunk_storage::EngineChunkStorage;` from `engine/mod.rs`
- `openraft` from both `[dependencies]` and `[dev-dependencies]` in `aeordb-lib/Cargo.toml`
- `[[test]]` entry for `raft_spec` in Cargo.toml
- Check `futures-util` — if only used by the replication state_machine, remove it too

**Verify:** `cargo check` compiles cleanly. `cargo test` has no regressions (existing raft_spec tests are gone, everything else passes).

### Prerequisite 1: Entry Versioning Foundation

Fix the entry versioning system so it's ready for format changes.

**Changes:**
- Change `entry_version` from starting at `1` to starting at `0` (remove the version 0 rejection in entry_header.rs)
- Add `pub const CURRENT_ENTRY_VERSION: u8 = 0;` constant
- Replace hardcoded `entry_version: 1` in append_writer.rs with `CURRENT_ENTRY_VERSION`
- Add version-based dispatch stubs in all deserializers (FileRecord, SymlinkRecord, ChildEntry, SnapshotInfo, ForkInfo) — currently they assume a single format; add a match on version that routes to `deserialize_v0`
- No format changes yet — just the routing infrastructure

### Prerequisite 2: Hash and Timestamp Refactor (MUST complete before replication)

**Problem:** FileRecord and SymlinkRecord content hashes currently include timestamps (`created_at`, `updated_at`) in the hashed data. This means identical content stored at different times produces different hashes, breaking dedup and causing false conflicts during replication.

**Changes required:**

1. **Introduce identity hashes** — new hash functions (`file_identity_hash`, `symlink_identity_hash`) that hash only content-defining fields (path, content_type, chunk_hashes) and deliberately EXCLUDE timestamps, metadata, and total_size. These identity hashes are used in `ChildEntry.hash` for directory trees and versioning.

2. **Virtual clock for all timestamps** — replace all uses of `chrono::Utc::now()` for entry timestamps with the heartbeat-synced virtual clock. This ensures `created_at` and `updated_at` are consistent across nodes. The virtual clock uses peer heartbeats to compute clock offsets, wire times, and corrected timestamps to near-ms precision.

3. **Heartbeat clock sync protocol** — enhance the existing heartbeat to include `(intent_time, construct_time, sender_node_id)`. Receiving nodes compute clock offset and wire time from each heartbeat, building a per-peer stats table (offset, latency, jitter). The corrected virtual time is used for all entry timestamps and conflict ordering. The heartbeat timing is self-correcting: it monitors its own fire accuracy and compensates for OS scheduling drift, converging to sub-ms alignment with the target interval.

4. **Honeymoon phase** — mandatory settling period on every peer connect/reconnect. During honeymoon, nodes exchange heartbeats only — no data sync, no chunk exchange, no merges. The honeymoon ends when clock offset variance stabilizes below a threshold (e.g. < 5ms stddev) and a minimum heartbeat count is reached. This ensures the virtual clock is calibrated before any ordering decisions are made. The dashboard shows per-connection state (Disconnected / Honeymoon / Active) in a dedicated Nodes section with live clock stats.

5. **Persisted clock state for fast honeymoon** — each peer's last known clock offset, wire time, jitter, and the timestamp of that measurement are persisted in system tables alongside the peer config. On reconnect, the honeymoon uses this as a seed estimate instead of starting from zero, allowing much faster settling when system clocks haven't drifted significantly. Confidence in the seed decays with time since last measurement.

6. **Entry ordering metadata** — each entry gets `(virtual_time, node_id)` as ordering metadata, stored separately from user-visible timestamps. This is the conflict resolution key.

7. **No migration** — we are pre-production. Existing test databases are disposable. Just change the format. No dual-format support needed.

This is a foundational change that affects: `file_record.rs`, `symlink_record.rs`, `directory_ops.rs`, `directory_entry.rs` (ChildEntry.hash semantics), `tree_walker.rs`, `version_access.rs`, `heartbeat.rs`, and all tests that verify content hashes.

### Phase 0: Entry Metadata
- Add `virtual_time: u64` and `node_id: u64` to entry metadata
- Node ID generation and persistence
- Virtual clock sync via heartbeat (enhanced with clock offset / wire time computation)

### Phase 1: Sync Endpoint
- `POST /sync/diff` — compute and return tree diff with path filtering
- `POST /sync/chunks` — batch chunk transfer
- Cluster secret authentication on sync endpoints

### Phase 2: Directory Merge
- Three-way merge algorithm (base, local, remote)
- Deterministic conflict resolution (LWW + modify-beats-delete)
- Merge produces new HEAD

### Phase 3: Conflict System
- `/.conflicts/` storage structure
- Conflict detection during merge
- Conflict resolution API (`GET/POST /admin/conflicts`)
- Conflicts sync as regular entries

### Phase 4: Peer Management
- System table storage for peers
- Admin API for peer CRUD
- CLI flags (--peers, --cluster-secret-file)
- SSE subscription to peers for sync triggers

### Phase 5: Sync Engine
- Background sync loop (SSE-triggered + periodic fallback)
- Bidirectional sync between all peers
- Selective sync with path filtering
- Sync state tracking (last synced hash per peer)

### Phase 6: Auth & Join
- Signing key sync
- Join flow (first sync before accepting traffic)
- Cluster secret validation

### Phase 7: Testing & Hardening
- Multi-node integration tests
- Conflict detection and resolution tests
- Selective sync tests
- Network failure / reconnection tests
- Large file sync tests
- Clock drift simulation
- Deterministic merge verification (both nodes arrive at same HEAD)
- E2E: real multi-node cluster with curl

---

## What We Keep from the Raft Design

- Cluster configuration in system tables
- Inter-node HTTP communication
- Cluster secret authentication
- The general phase structure

## What We Drop

- openraft dependency (for replication — keep it for future strong-consistency mode if needed)
- Leader election, terms, log replication
- Quorum requirements
- Single-writer model
- Raft log storage
- Raft snapshots

## What We Gain

- Any node can write (no leader)
- Works during network partitions (nodes keep writing independently)
- Natural large file handling (stream at your own pace)
- Dramatically simpler implementation
- Client = node (unified sync protocol)
- Selective sync built in
- Conflict detection and resolution as first-class features

---

## Resolved Questions

1. **System table conflicts** — API keys are immutable after creation (no modifications, only revocation). Revocation ALWAYS wins regardless of timestamp — this is a security rule, not a conflict resolution rule. User/group LWW is sufficient for the rare concurrent-create case.

2. **GC in cluster mode** — each node GCs independently, scoped to its own tree. GC is **conflict-aware**: any chunk referenced by an entry in `/.conflicts/` is a live root and must not be collected. Conflicts are GC roots until resolved.

3. **Snapshot/version semantics** — snapshots are node-local. A snapshot taken on Node A captures Node A's state at that moment. Snapshots are just more entries with chunks — they propagate to peers via normal sync. A snapshot represents a state that existed, somewhere, at some time.

4. **Ordering within a sync batch** — additions/creates are processed before deletions. This ensures directories exist before files are added to them. The modify-beats-delete conflict rule only applies when the same path has conflicting operations from different nodes — it does not affect intra-batch ordering.

5. **Diamond merge problem** — not a significant problem with content-addressed sync. Nodes that have never directly synced simply diff and trade missing entries. Content addressing means the same content produces the same hash regardless of provenance. True conflicts (same path, different content) are handled by LWW. Chunks propagate through the network naturally — if a node doesn't have a chunk today, a future sync cycle with any peer that has it fills the gap.

6. **Deletion propagation with selective sync** — deletions only propagate to peers that sync the affected path. If Node B only syncs `/assets/` and Node A deletes something in `/docs/`, Node B never sees the deletion. This is correct behavior — you only see changes to paths you're subscribed to.

7. **Initial full sync** — new nodes sync HEAD first (immediately usable), then backfill historical versions in the background. Desktop clients may only want HEAD and request specific historical versions on demand. Replication nodes pull everything over time.

8. **Auth bootstrap** — JWT signing key is stored in system tables. New node receives it during initial sync (HEAD-first). Node MUST NOT accept client traffic until signing key is present. Honeymoon phase naturally enforces this — no data sync (and thus no signing key) until clocks settle.

## Remaining Open Question

1. **Migration strategy for identity hashes** — existing databases have FileRecord content hashes that include timestamps. On upgrade, do we: (a) rewrite all hashes (expensive, breaks snapshots), (b) support both hash formats indefinitely (complexity), or (c) only use identity hashes for new writes and let old entries age out through GC? Need to decide before the prerequisite phase.
