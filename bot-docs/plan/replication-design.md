# AeorDB Replication — Comprehensive Design

**Date:** 2026-04-15
**Status:** WIP — questions flagged at bottom

---

## Overview

Multi-node replication for AeorDB using openraft for consensus. Each node maintains a full copy of the database in its own `.aeordb` file. Writes go through Raft consensus to guarantee all nodes apply the same operations in the same order. Reads can be served from any node (eventual consistency) or routed through the leader (linearizable).

## Existing Scaffold

The `replication/` module already has:
- `TypeConfig` — openraft type wiring (NodeId=u64, Node=RaftNode, D=RaftRequest, R=RaftResponse)
- `InMemoryLogStore` — full RaftLogStorage implementation (in-memory, needs disk-backed replacement)
- `StubNetworkFactory` — placeholder for inter-node communication
- `ChunkStateMachine` — applies RaftRequest to ChunkStorage (legacy, needs updating for new engine)
- `RaftNodeManager` — high-level wrapper with initialize/write/is_leader
- `RaftRequest` enum — StoreChunk, StoreHashMap, DeleteChunk (needs expansion)
- `RaftResponse` struct — success + optional message
- `RaftNode` struct — just an address string

## What Needs to Change

The scaffold was built against the old `ChunkStorage` trait (legacy redb-based storage). The current engine is the custom StorageEngine with KV store, directory ops, versioning, etc. The replication layer needs to be rewired to work with the new engine.

---

## Architecture

### Replication Granularity: KV-Level Operations

Every state change in AeorDB ultimately flows through `engine.store_entry(entry_type, key, value)` or `engine.mark_entry_deleted(key)`. These are the atomic units of mutation.

Replicating at the KV level means:
- The leader performs all high-level logic (chunking, directory building, compression, indexing)
- Only the resulting KV operations are replicated
- Followers apply raw KV operations, arriving at identical state
- No ambiguity about what changed — it's a deterministic replay

**RaftRequest enum (expanded):**

```rust
pub enum RaftRequest {
    /// Store a single entry in the KV store.
    StoreEntry {
        entry_type: u8,
        key: Vec<u8>,
        value: Vec<u8>,
        compression_algo: u8,  // 0 = none, 1 = zstd
    },
    /// Mark an entry as deleted in the KV store.
    MarkDeleted {
        key: Vec<u8>,
    },
    /// Update the HEAD root hash.
    UpdateHead {
        hash: Vec<u8>,
    },
    /// Batch of operations (single user-visible action = one Raft log entry).
    Batch {
        operations: Vec<RaftRequest>,
    },
}
```

A single file upload generates one `Batch` containing: N StoreEntry ops (chunks) + 1 StoreEntry (FileRecord at content key) + 1 StoreEntry (FileRecord at path key) + directory updates + UpdateHead. One Raft consensus round.

### The Large File Problem

A 1GB file = ~4000 chunks × 256KB = ~1GB of data in one Raft log entry. This is a problem for:
1. Log replication bandwidth (sending 1GB to each follower)
2. Log storage (keeping 1GB entries in the log until compacted)
3. Snapshot size

**Solution: Separate data plane from consensus plane.**

The Raft log entry contains only **metadata and hashes** — not the chunk data itself:

```rust
pub enum RaftRequest {
    /// Store an entry. For chunks, `value` contains the actual data.
    /// For small entries (< 64KB), data is inline in the log entry.
    StoreEntry {
        entry_type: u8,
        key: Vec<u8>,
        value: Vec<u8>,
        compression_algo: u8,
    },
    /// Store a large chunk by reference. The actual data is transferred
    /// separately via the chunk transfer protocol.
    StoreChunkRef {
        key: Vec<u8>,
        size: u64,
        compression_algo: u8,
    },
    /// Mark an entry as deleted.
    MarkDeleted {
        key: Vec<u8>,
    },
    /// Update HEAD root hash.
    UpdateHead {
        hash: Vec<u8>,
    },
    /// Batch of operations.
    Batch {
        operations: Vec<RaftRequest>,
    },
}
```

For chunks larger than a threshold (e.g. 64KB), the leader uses `StoreChunkRef` instead of `StoreEntry`. The chunk data is transferred to followers via a separate **chunk transfer protocol** (HTTP endpoint). Followers that receive a `StoreChunkRef` fetch the chunk from the leader (or any peer that has it) before applying.

This keeps the Raft log small while still guaranteeing ordering and consistency.

### Chunk Transfer Protocol

Separate from Raft consensus. Followers pull chunks they need:

```
GET /raft/chunks/{hex_hash}
```

This is a simple content-addressed fetch — the leader (or any peer) serves chunk data by hash. Since chunks are immutable and content-addressed, any node that has the chunk can serve it. Dedup is automatic.

The flow:
1. Leader commits a Batch with StoreChunkRef entries
2. Followers receive the committed log entry
3. For each StoreChunkRef, followers check: "do I have this chunk locally?"
4. If not, fetch from leader via GET /raft/chunks/{hash}
5. Once all chunks are available, apply the full batch

This is eventually consistent for chunk availability — there's a brief window where the operation is committed (in the Raft log) but the chunk data isn't yet available on all followers. The follower's apply() blocks until all chunks are fetched.

---

## Cluster Configuration

### Storage

Cluster config stored in system tables under `::aeordb:cluster:` prefix:

```
::aeordb:cluster:node_id     -> u64 (this node's ID, generated once)
::aeordb:cluster:mode        -> "standalone" | "cluster"
::aeordb:cluster:peers       -> JSON array of { node_id, address, label }
```

### Admin API

```
GET  /admin/cluster           -> cluster status (node_id, mode, peers, leader)
POST /admin/cluster/join      -> join an existing cluster
POST /admin/cluster/leave     -> leave the cluster gracefully
POST /admin/cluster/peers     -> add a peer (leader only)
DELETE /admin/cluster/peers/{node_id} -> remove a peer (leader only)
```

### CLI

```bash
# Start as standalone (default, current behavior)
aeordb start -D data.aeordb

# Start and join a cluster
aeordb start -D data.aeordb --join node1.example.com:3000

# Start with known peers (static config)
aeordb start -D data.aeordb --peers "node1:3000,node2:3000,node3:3000"
```

### Bootstrap Flow

1. First node starts with `--peers "self:3000"` or no peers flag
2. Initializes as a single-node Raft cluster (auto-elected leader)
3. Operates normally as standalone
4. Second node starts with `--join node1:3000`
5. Second node contacts node1's `/admin/cluster/join` endpoint
6. Leader (node1) proposes a membership change via Raft
7. Once committed, node2 is part of the cluster
8. Leader starts replicating log entries to node2
9. Node2 catches up (may receive a snapshot if far behind)
10. Repeat for node3, etc.

---

## Inter-Node Communication

### Raft RPC Endpoints (Internal)

Exposed on the same HTTP server but restricted to cluster peers:

```
POST /raft/append-entries    -> AppendEntries RPC
POST /raft/vote              -> RequestVote RPC
POST /raft/snapshot          -> InstallSnapshot RPC
GET  /raft/chunks/{hash}     -> Chunk data transfer
```

These endpoints:
- Do NOT require JWT auth (they use a separate cluster secret/TLS mutual auth)
- Are NOT exposed to clients
- Should be restricted by source IP or mTLS certificate

### RaftNetwork Implementation

Replace `StubNetworkFactory` with `HttpNetworkFactory`:

```rust
pub struct HttpNetworkFactory {
    client: reqwest::Client,  // or hyper client
}

impl RaftNetworkFactory<TypeConfig> for HttpNetworkFactory {
    type Network = HttpNetworkConnection;

    async fn new_client(&mut self, target: u64, node: &RaftNode) -> Self::Network {
        HttpNetworkConnection {
            target_address: node.address.clone(),
            client: self.client.clone(),
        }
    }
}
```

Each RPC method serializes the request as JSON (or bincode for performance), POSTs to the target node's /raft/ endpoint, and deserializes the response.

### Security

Inter-node communication must be authenticated. Options:
- **Shared cluster secret** — a pre-shared key that nodes include in a header. Simple.
- **Mutual TLS (mTLS)** — each node has a certificate signed by a cluster CA. Strongest.
- **Both** — mTLS for transport encryption + shared secret as a belt-and-suspenders.

For v1: shared cluster secret passed via `--cluster-secret` flag or stored in the database.

---

## Storage Changes

### Disk-Backed Log Store

Replace `InMemoryLogStore` with a persistent implementation. The Raft log needs:
- Durable append (fsync!)
- Range reads (for replication)
- Truncation (leader conflict resolution)
- Purge (log compaction after snapshot)

Options:
a) Store Raft log entries in AeorDB's own WAL/KV store (under a `::aeordb:raft:` prefix)
b) Separate file for the Raft log (e.g. `data.aeordb.raft`)
c) Use the existing append-only WAL as the Raft log (the WAL IS the log)

**Recommendation: (a)** — store in the existing engine under a system prefix. This keeps the single-file model. The KV store already handles persistence and crash recovery. We just need to ensure fsync semantics for vote/term storage (critical for safety).

### Vote/Term Persistence

**CRITICAL SAFETY REQUIREMENT:** The current term and voted_for MUST be persisted to stable storage before responding to any RPC. If a node crashes and forgets its vote, it could vote twice in the same term, breaking Raft's safety guarantee.

Store under:
```
::aeordb:raft:vote   -> serialized Vote struct
::aeordb:raft:term   -> current term (u64)
```

The InMemoryLogStore's `save_vote` must be replaced with a disk-backed implementation that calls fsync.

---

## Write Path Changes

### Current Flow
```
Client → HTTP handler → DirectoryOps → StorageEngine → Response
```

### New Flow (Leader)
```
Client → HTTP handler → Build KV operations → Raft::client_write(Batch) →
  Consensus (replicate to majority) → Apply to StorageEngine → Response
```

### New Flow (Follower)
```
Client → HTTP handler → Detect not leader →
  Option A: Redirect client to leader (307 Temporary Redirect)
  Option B: Proxy the request to the leader
  Option C: Reject with error + leader address in response
```

**Recommendation: Option C for v1** — return a JSON error with the leader's address. The client (aeordb-client) can handle retry logic. Simple, no proxy complexity.

```json
{
  "error": "Not the leader",
  "leader": "node1.example.com:3000",
  "leader_id": 1
}
```

### What Goes Through Raft

Every state-changing operation:
- File store/delete
- Symlink create/delete
- Snapshot create/restore/delete
- Fork create/promote/abandon
- Directory create
- User create/update/deactivate
- Group create/update/delete
- Permission changes
- API key create/revoke
- Config changes

What does NOT go through Raft:
- Reads (GET, HEAD, directory listing, queries)
- GC (runs locally on each node)
- Metrics, health checks
- Event stream (SSE)

### Intercepting Writes

The cleanest approach: a `RaftWriteProxy` that wraps `StorageEngine` and intercepts mutations:

```rust
pub struct RaftWriteProxy {
    engine: Arc<StorageEngine>,
    raft: Arc<Raft<TypeConfig>>,
    mode: ClusterMode,  // Standalone | Leader | Follower
}

impl RaftWriteProxy {
    pub async fn store_entry(&self, entry_type: EntryType, key: &[u8], value: &[u8]) -> EngineResult<()> {
        match self.mode {
            ClusterMode::Standalone => {
                // Direct write, no consensus
                self.engine.store_entry(entry_type, key, value)
            }
            ClusterMode::Leader => {
                // Write through Raft
                let request = RaftRequest::StoreEntry { ... };
                self.raft.client_write(request).await?;
                Ok(())
            }
            ClusterMode::Follower => {
                Err(EngineError::NotLeader)
            }
        }
    }
}
```

Actually, this is tricky because DirectoryOps performs multiple KV operations per user action. We need to batch them. The better approach:

```rust
// Collect all KV operations during a write
let mut batch = WriteBatch::new();
// ... DirectoryOps does its work, but writes to the batch instead of the engine ...
// Submit the batch through Raft
raft.client_write(RaftRequest::Batch { operations: batch.into_operations() }).await?;
```

AeorDB already has a `WriteBatch` concept. We need to ensure that DirectoryOps can operate in a "batch mode" where mutations are collected instead of applied immediately.

---

## Read Path

### Consistency Levels

```
GET /engine/file.txt                        -> stale read (any node)
GET /engine/file.txt?consistency=leader      -> linearizable (leader only)
GET /engine/file.txt?consistency=local       -> stale read (explicit)
```

Default: local/stale reads. This is fine for most use cases. A file uploaded to the leader is typically available on followers within milliseconds.

For the aeordb-client sync use case: stale reads are perfect. The client compares hashes and syncs deltas — minor staleness just means the next sync picks up the rest.

---

## Raft Snapshots

When the Raft log gets too long, it needs compaction. A Raft snapshot captures the full state at a specific log index, allowing all earlier log entries to be discarded.

AeorDB's existing snapshot system (VersionManager) is conceptually similar but not identical:
- AeorDB snapshot = named version (user-facing, metadata)
- Raft snapshot = complete state transfer (internal, for log compaction)

For the Raft snapshot, we export the entire database state using the existing `export_version` mechanism. The snapshot data is the serialized `.aeordb` export format. This is already a self-contained bundle of all entries.

When a follower needs a full snapshot (too far behind for log replay):
1. Leader calls `get_snapshot_builder()` → exports current HEAD state
2. Serialized snapshot sent to follower via `install_snapshot` RPC
3. Follower replaces its entire database with the snapshot
4. Follower continues applying log entries from the snapshot's last index

---

## Membership Changes

Raft supports dynamic membership changes via joint consensus:

1. Leader proposes new membership (e.g. add node3)
2. Raft transitions to "joint" membership (old config + new config)
3. Once joint config is committed, transition to new config
4. Once new config is committed, membership change is complete

openraft handles this internally. We just need to call:
```rust
raft.change_membership(new_members, true).await?;
```

The admin API (`POST /admin/cluster/peers`) triggers this.

---

## Phases

### Phase 0: Cluster Configuration
- Node ID generation and persistence in system tables
- Peer list storage
- Admin API endpoints (/admin/cluster/*)
- CLI flags (--join, --peers, --cluster-secret)
- Cluster mode detection (standalone vs cluster)

### Phase 1: Disk-Backed Log Store
- Replace InMemoryLogStore with engine-backed storage
- Persist vote/term with fsync semantics
- Log entry serialization/deserialization
- Raft log stored under ::aeordb:raft: prefix

### Phase 2: RaftRequest Expansion
- Expand RaftRequest enum for all state-changing operations
- StoreEntry, StoreChunkRef, MarkDeleted, UpdateHead, Batch
- RaftResponse updates
- State machine apply() rewrite for new engine

### Phase 3: Network Layer
- HTTP-based RaftNetwork implementation
- /raft/append-entries, /raft/vote, /raft/snapshot endpoints
- Cluster secret authentication
- Chunk transfer endpoint (/raft/chunks/{hash})

### Phase 4: Write Path Integration
- WriteBatch collection mode for DirectoryOps
- Route all writes through Raft on leader
- Follower write rejection with leader redirect
- Batch operations for atomic multi-KV-op writes

### Phase 5: State Machine Rewrite
- Replace ChunkStateMachine with EngineStateMachine
- Apply committed batches to StorageEngine
- Snapshot build via export_version
- Snapshot install via import_backup

### Phase 6: Read Path
- Stale reads from any node (default)
- Optional linearizable reads via leader
- Consistency query parameter

### Phase 7: Bootstrap & Join
- Single-node cluster initialization
- Join flow (new node contacts leader)
- Snapshot transfer for new nodes
- Graceful leave

### Phase 8: Chunk Transfer Protocol
- GET /raft/chunks/{hash} endpoint
- Follower chunk fetching on StoreChunkRef apply
- Background chunk pre-fetching
- Chunk availability tracking

### Phase 9: Testing & Hardening
- Multi-node integration tests (spin up 3 nodes in-process)
- Leader election tests
- Write replication tests
- Node failure/recovery tests
- Network partition simulation
- Split-brain prevention verification
- Snapshot transfer tests
- E2E: real-world multi-node cluster with curl

---

## Open Questions

1. **WriteBatch interception** — DirectoryOps currently calls `engine.store_entry()` directly. How do we collect these into a batch for Raft? Options: (a) add a batch mode to StorageEngine, (b) wrap DirectoryOps to collect operations, (c) capture at a higher level (HTTP handler builds the batch). This is the most architecturally significant decision.

2. **System table replication** — user/group/permission changes are stored in system tables, which use `engine.store_entry()` internally. These MUST be replicated. Do we intercept at the system_tables level too, or at the engine level?

3. **Auth in cluster mode** — with `--auth self`, each node generates its own JWT signing key on first boot. In a cluster, all nodes must share the same signing key (otherwise a JWT from node1 won't verify on node2). Solution: the signing key is replicated through Raft as a system table entry. But there's a bootstrap problem — the first JWT must be valid before replication is set up.

4. **GC coordination** — GC runs locally and reclaims unreachable entries. In a cluster, a chunk might be "unreachable" on one node but still needed by another node that's catching up. Do we disable GC in cluster mode? Or coordinate GC across nodes?

5. **Event bus in cluster** — events (file created, deleted, etc.) are currently local. Should events from other nodes be forwarded to the local event bus? For SSE subscribers watching all changes, they'd want cluster-wide events.

6. **Task/cron coordination** — background tasks (reindex, GC) should probably only run on the leader to avoid duplicate work. Or each node runs its own but they're idempotent.

7. **Hot file recovery** — the hot file mechanism replays uncommitted WAL entries on crash. In cluster mode, the Raft log is the source of truth — do we still need the hot file, or does Raft recovery replace it?

8. **Chunk transfer reliability** — if the leader crashes after committing a StoreChunkRef but before the followers fetch the chunk data, where do followers get the chunk? Answer: from any other node that has it (peers, not just leader). But if no node has it, the operation is stuck. This needs careful handling.

9. **Large snapshot transfer** — for a multi-GB database, the Raft snapshot is the entire database export. Transferring this to a new node could take minutes. During this time, the cluster must continue operating. openraft handles this via streaming snapshot transfer, but we need to ensure our export format supports streaming.

10. **Standalone → cluster migration** — a user starts with a standalone AeorDB, stores data, then decides to add replication. The existing data needs to be treated as the initial state. The first node initializes as a single-node Raft cluster with all existing data as the "initial snapshot."
