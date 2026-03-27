# Replication & Distribution — openraft + redb

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design

---

## Core Concept

AeorDB uses openraft (Rust, MIT/Apache-2.0) for distributed consensus and replication. The same binary runs in single-node mode (zero replication overhead) or multi-node mode (full Raft consensus). No architectural changes between the two — just add nodes.

---

## Architecture

```
Client write request
       ↓
  openraft (leader)
       ↓ appends to Raft log (custom append-only file, NOT redb)
       ↓ replicates log entry to quorum of followers
  openraft (followers)
       ↓ each node applies committed entry to local state machine
  redb (state machine — actual database tables/indexes)
       ↓
  response to client
```

### Component Responsibilities

| Component | Implementation | Role |
|---|---|---|
| **Raft log** | Custom append-only file (pure Rust) | Sequential log of all write operations. Append, fsync, truncate. |
| **State machine** | redb | The actual database. Committed operations are applied here. |
| **Consensus** | openraft | Leader election, log replication, membership changes, snapshot orchestration. |
| **Network** | HTTP (reuse existing query interface transport) | Node-to-node communication for Raft RPCs. |
| **Snapshots** | Copy the redb file | Since redb is single-file COW, snapshots are a file copy. |

---

## openraft Integration

Three traits to implement:

### 1. RaftLogStorage (Raft log persistence)

Custom append-only log file. NOT redb — redb's COW B-trees are 12x slower than append-optimized stores for this workload.

Requirements:
- `append(entries)` — write entries sequentially, fsync, signal completion via callback
- `read(range)` — read entries back by log index
- `truncate_after(log_id)` — remove entries after a given point (conflict resolution)
- `purge(log_id)` — delete entries covered by a snapshot
- `save_vote / read_vote` — persist Raft vote state (single atomic write)

Implementation: a few hundred lines of pure Rust. Structured append to a single file with an index for fast lookups by log ID.

### 2. RaftStateMachine (application state)

This IS redb. When openraft commits an entry, we apply it to redb:
- `apply(entries)` — open a redb write transaction, apply operations, commit
- `build_snapshot()` — copy the redb file (COW makes this safe even during reads)
- `install_snapshot(data)` — replace local redb file with received snapshot

### 3. RaftNetwork (node-to-node communication)

HTTP-based, reusing our existing transport layer:
- `append_entries(rpc)` — POST to peer's Raft endpoint
- `vote(rpc)` — POST to peer's Raft endpoint
- `full_snapshot(snapshot)` — stream redb file to peer

---

## Single-Node Mode

- Initialize openraft with a single-member cluster
- Node is immediately and permanently the leader
- Writes go through the full Raft path (log → commit → apply) but with zero network I/O
- Functionally equivalent to a local database with WAL
- No performance penalty beyond the append-only log write (which we'd want anyway for crash recovery)

## Scaling to Multi-Node

1. Start additional aeordb nodes
2. Leader calls `add_learner()` — new node begins receiving log entries
3. Leader calls `change_membership()` — new node becomes a voting member
4. openraft handles the transition via joint consensus
5. Snapshots (redb file copies) are shipped to lagging followers automatically

---

## Performance Characteristics

| Metric | Value |
|---|---|
| openraft throughput (in-memory) | 3.5M ops/sec @ 4096 clients |
| openraft throughput (disk, no fsync) | ~15K-24K writes/sec |
| openraft throughput (disk, fsync) | ~780 writes/sec |
| Single-client latency (in-memory) | ~30 microseconds |
| Single-client latency (disk + fsync) | 1-5 milliseconds |

Note: fsync-per-write is the fully crash-safe mode. Batching writes significantly improves throughput.

---

## Why NOT RocksDB for the Log Store

RocksDB is faster than redb for append-heavy workloads, but:
- C++ dependency — FFI bindings, complex build chain
- Massive dependency footprint
- Overkill for a simple append-only log
- Violates "keep the engine small" constraint

A custom append-only log in pure Rust is simpler, smaller, faster for this specific workload, and has zero external dependencies.

---

## Open Questions

- [ ] Exact Raft log file format (entry framing, index structure)
- [ ] Snapshot strategy — full file copy vs incremental?
- [ ] Read consistency model — leader reads only, or follower reads with lease?
- [ ] Batching strategy for Raft log appends (throughput vs latency trade-off)
- [ ] Network transport details — reuse query HTTP server or separate Raft port?
- [ ] Conflict resolution for concurrent writes to the same key across partitions
- [ ] Sharding strategy (if any) — single Raft group or multi-Raft?

---

## Problems Addressed

From [Why Databases Suck](../docs/why-databases-suck.md):
- **#3 Scaling is bolted on, not built in** — Same binary, single-node to cluster. Built in from day one.
- **#5 Replication is fragile and dishonest** — Raft provides clear, provable consistency guarantees. No "eventual consistency" hand-waving.
