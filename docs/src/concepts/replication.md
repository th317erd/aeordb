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
4. Node A merges the changes into its own tree, detecting conflicts
5. Node A updates its HEAD to the merged state

This process is **atomic** — either all changes are applied, or none are. A network failure mid-sync leaves the database unchanged.

### Sync is bidirectional. After Node A pulls from Node B, Node B can pull from Node A to get any changes that originated on Node A.

## Conflict Resolution

When two nodes modify the same file independently, AeorDB detects the conflict and resolves it automatically:

- **Last-Write-Wins (LWW)** — the version with the higher virtual timestamp becomes the "current" version
- **Modify beats delete** — if one node modifies a file while another deletes it, the modification wins. Work is never silently lost.
- **Loser preserved** — the "losing" version is stored in `/.conflicts/` so it can be recovered

Conflicts are stored as regular database entries, which means they sync to all nodes automatically. A conflict resolved on any node propagates the resolution to all other nodes.

### Viewing Conflicts

```bash
# List all unresolved conflicts
curl http://localhost:3000/admin/conflicts \
  -H "Authorization: Bearer $TOKEN"

# Resolve a conflict (pick the winner)
curl -X POST http://localhost:3000/admin/conflict-resolve/assets/logo.psd \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pick": "winner"}'
```

## Virtual Clock

Nodes synchronize their clocks using the heartbeat mechanism. Each heartbeat includes timing information that allows nodes to compute clock offsets and network latency. This ensures that timestamps — used for conflict resolution ordering — are consistent across nodes to near-millisecond precision.

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

The replication protocol is the same protocol that the AeorDB client uses. A desktop client syncing with a server and a server syncing with another server use the same mechanism — compare hashes, exchange chunks, merge trees.

## Client Sync

Desktop clients and other non-peer applications can sync using the same protocol as replication peers, with appropriate access restrictions:

- Clients authenticate with their JWT token
- The `/.system/` directory is automatically excluded from client sync results
- API key scoping rules apply — a scoped key with restricted path access only sees changes for allowed paths
- Clients can use the `paths` filter for selective sync (e.g., only sync `/assets/**`)

This means a client with a read-only key scoped to `/assets/` will only see file changes under `/assets/` in sync diffs, and cannot access system data, other users' files, or paths outside its scope.

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
