# Replication

AeorDB supports multi-node replication using content-addressed sync. Every node is a full peer — any node can accept writes. Nodes sync by comparing directory tree hashes and exchanging missing chunks. Conflicts are detected, preserved, and resolved as first-class database entities.

## How It Works

AeorDB's replication leverages its content-addressed storage architecture:

1. **Every file is identified by its content hash** — identical content always produces the same hash, regardless of when or where it was stored
2. **Directory trees are Merkle trees** — changing one file changes the tree hash all the way to the root
3. **Sync is a tree comparison** — two nodes compare their root hashes. If they differ, they exchange the entries that are different
4. **Chunks are immutable** — once stored, a chunk never changes. This makes transferring data between nodes safe and idempotent

### Sync Protocol

When two nodes sync:

1. Node A asks Node B: "What changed since the last time we synced?" (tree diff)
2. Node B computes the differences and responds with a list of added, modified, and deleted files
3. Node A fetches any missing chunks from Node B
4. Node A computes a three-way merge from the last acknowledged local and remote roots
5. Node A publishes the complete bounded merge receipt, including local conflict evidence
6. Node A records both new roots as the next merge checkpoint

The receiver strictly validates the complete diff envelope before mutation. Paths must be
canonical and unique; identity hashes, whole-file hashes when present, timestamps, symlink
targets, and the exact required-chunk closure must agree. Requested chunks are transferred in
bounded batches and checked for exact hash, decoded size, duplicate, omission, and unexpected
content. Existing chunk keys must resolve to valid chunks rather than merely occupying a KV
key.

Immutable chunks may be staged before namespace publication. If a later check fails, those
unreferenced chunks are recoverable garbage; no path points to them. Files, symlinks,
deletions, parent closure, local conflict evidence, counters, metadata-index wakeups, and SSE
relationship events are then planned under one memory-admitted namespace operation and one
hard durability acknowledgement. Missing delete targets are idempotent, but a wrong-type or
corrupt target is an error. The peer checkpoint is written only after that receipt succeeds.
Retrying after a lost checkpoint repeats the merge idempotently rather than exposing a partial
apply.

Each peer checkpoint stores two roots: the remote root acknowledged from that peer and the
local post-merge root. These are the two bases needed for a later three-way merge. Legacy
single-root checkpoints remain readable; when their meaning cannot be proven, AeorDB performs
a conservative one-time remote comparison instead of inventing deletions.

### Sync is bidirectional. After Node A pulls from Node B, Node B can pull from Node A to get any changes that originated on Node A.

## Conflict Resolution

When two nodes modify the same file independently, AeorDB detects the conflict and resolves it automatically:

- **Last-Write-Wins (LWW)** — the version with the higher virtual timestamp becomes the "current" version
- **Modify beats delete** — if one node modifies a file while another deletes it, the modification wins. Work is never silently lost.
- **Loser preserved** — the "losing" version is stored in `/.aeordb-conflicts/` so it can be recovered

Conflict records are local conflict authority and do not replicate as ordinary peer data. The winning namespace content can converge normally; operators inspect and resolve each node's retained conflict evidence through the typed conflict API.

An unresolved record also acknowledges that exact winner/loser pair locally.
If a detached portable family causes the same pair to be discovered again,
AeorDB validates and reuses the existing metadata and immutable versions rather
than rewriting evidence on every sync cycle. A changed pair remains a new
conflict, and malformed retained evidence stops the receive operation. The
reuse check and publication share namespace authority, including when different
peers present the same pair concurrently.

Conflict evidence is created in the same acknowledged namespace receipt as the selected
winner. Both file and symlink versions are supported, including modify-versus-delete
conflicts. Evidence retains exact immutable loser dependencies, and garbage collection treats
those references as live until the conflict is resolved or dismissed. Malformed evidence
blocks that GC mark instead of authorizing deletion.

Resolving a conflict verifies the selected immutable record, canonical identity, metadata,
and every referenced chunk before changing the visible path. Selecting a tombstone performs
the matching typed deletion. Resolution and evidence cleanup share one receipt, so AeorDB does
not report success with stale conflict metadata left behind. Dismissal likewise acknowledges
the evidence deletion rather than squelching cleanup errors.

### Viewing Conflicts

```bash
# List all unresolved conflicts
curl http://localhost:6830/sync/conflicts \
  -H "Authorization: Bearer $TOKEN"

# Resolve a conflict (pick the winner)
curl -X POST http://localhost:6830/sync/resolve/assets/logo.psd \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pick": "winner"}'

# Keep the current auto-winner and discard the retained evidence
curl -X POST http://localhost:6830/sync/dismiss/assets/logo.psd \
  -H "Authorization: Bearer $TOKEN"
```

## Virtual Clock

Nodes synchronize their clocks using the heartbeat mechanism. Each heartbeat carries three fields — `intent_time`, `construct_time`, and `node_id` — which allow nodes to compute clock offsets and network latency. This ensures that timestamps used for conflict resolution ordering are consistent across nodes to near-millisecond precision. The heartbeat is a dedicated clock-sync pulse and does not carry any stats or metrics data.

When a new node connects, it enters a **honeymoon phase** where only heartbeats are exchanged. The node settles its clock before any data sync begins, ensuring accurate timestamp ordering from the first sync.

## Selective Sync

Nodes can sync specific path subtrees only:

```json
{
  "sync_paths": ["/assets/**", "/docs/**"]
}
```

This is useful for:
- **Desktop clients** that only need their working directory
- **Regional offices** that only need their projects
- **Edge nodes** that serve specific content

## Client = Node

Peers and desktop clients use the same transport shape -- compare roots, exchange a filtered diff, then fetch chunks -- but they do not have the same visibility policy. The SystemFamily registry selects peer-replication policy for root sync JWTs and client-sync policy for ordinary authenticated clients.

## Client Sync

Desktop clients and other non-peer applications can sync using the same protocol as replication peers, with appropriate access restrictions:

- Clients authenticate with their JWT token
- Ordinary authorized files are included
- Required namespace metadata such as descendant `/.aeordb-permissions` and `/.aeordb-config/` records is included under the caller's authorized paths
- Central protected families, node-local state, credentials, conflict records, logs, and derived indexes are omitted
- API key scoping rules apply — a scoped key with restricted path access only sees changes for allowed paths
- Clients can use the `paths` filter for selective sync (e.g., only sync `/assets/**`)

This means a client with a read-only key scoped to `/assets/` sees file changes and required namespace metadata under `/assets/`, but cannot access central protected state, other users' files, or paths outside its scope. Chunk hashes are recomputed after all policy and authorization filters, so omitted files do not leak through the chunk manifest.

## Comparison with Strong Consistency

AeorDB uses **eventual consistency**, not strong consistency (Raft/Paxos). This means:

| Feature | AeorDB (Eventual) | Raft (Strong) |
|---------|-------------------|---------------|
| Write availability | Any node, anytime | Leader only |
| Network partition | Both sides keep writing | Minority is read-only |
| Large files | Stream at your own pace | Consensus on every chunk |
| Complexity | Low | High |
| Consistency guarantee | Eventually identical | Immediately identical |

For creative teams working with large assets across multiple locations, eventual consistency is the right tradeoff — availability and simplicity matter more than instant consistency.

For hands-on instructions for setting up and managing a cluster, see [Cluster Operations](../operations/cluster.md).
