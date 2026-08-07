# Cluster Operations

## Setting Up a Cluster

### Starting the First Node

Start a node normally. It operates as a standalone database:

```bash
aeordb start -D nodeA.aeordb --auth self
```

Save the root API key it prints — you'll use it as the join token for new nodes.

### Joining a Cluster

A new node joins by calling `/sync/join` on an existing member. This fetches the cluster's JWT signing key (so tokens validate cluster-wide) and registers both nodes as peers of each other.

```bash
aeordb start -D nodeB.aeordb --auth self \
  --port 6841 \
  --join http://nodeA:6830 \
  --join-token "$NODE_A_ROOT_KEY"
```

After join, node B:

- Adopts node A's JWT signing key (persisted in `/.aeordb-system/config/jwt_signing_key`)
- Adds node A as a peer (persisted)
- Is automatically added as a peer on node A

The flag is one-shot: subsequent restarts of node B do not need `--join`.

### Adding Peers Manually

For nodes already in a cluster (sharing the same signing key), peers can be added without rejoining:

```bash
# At startup — comma-separated peer URLs, idempotent
aeordb start -D data.aeordb --peers "http://nodeC:6830,http://nodeD:6830"

# At runtime
curl -X POST http://localhost:6830/sync/peers \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"address": "https://nodeC:6830", "label": "US West"}'
```

### Authentication

All `/sync/*` endpoints require JWT authentication. Nodes mint short-lived root JWTs (nil UUID `sub`) internally when calling each other's `/sync/diff` and `/sync/chunks` endpoints. This works because every node in the cluster shares the same Ed25519 signing key (distributed by `/sync/join`).

Protected `/.aeordb-*` state is not exposed through generic HTTP file APIs merely because a caller is root. Peer sync also does not copy the entire `/.aeordb-system/` tree. A root sync JWT selects the peer-replication policy in AeorDB's SystemFamily registry: portable state such as users, groups, central permissions, plugin definitions, and namespace configuration is included, while node-local credentials, signing keys, API keys, email secrets, operational controls, logs, derived indexes, and conflict records are omitted. Root and nested `.aeordb-indexes` and `.aeordb-logs` instances use the same declared policies. Cluster join transfers the signing key through the dedicated `/sync/join` protocol rather than ordinary replication.

Both sync producers and receivers enforce this policy. A peer response containing an unknown protected path, an omitted family, or a structural container presented as a file/symlink is rejected before AeorDB requests its chunks or mutates the destination.

### TLS

The server listener can be terminated with TLS using `--tls-cert` and `--tls-key`. There is no separate "cluster TLS" toggle — peer URLs use whichever scheme you configure (`http://` or `https://`). For production, use `https://` peer addresses with valid certificates, or place nodes behind a private network.

## Monitoring

### Cluster Status

```bash
# This node's view of the cluster
curl http://localhost:6830/sync/status \
  -H "Authorization: Bearer $TOKEN"

# Peer list with sync state and last-sync timestamps
curl http://localhost:6830/sync/peers \
  -H "Authorization: Bearer $TOKEN"
```

### Connection States

Each peer connection has a state:

| State | Meaning |
|-------|---------|
| Disconnected | No active connection |
| Honeymoon | Clock settling in progress — no data sync yet |
| Active | Fully synced and exchanging data |

The **honeymoon phase** is mandatory on every connect/reconnect. It ensures clocks are calibrated before any data is exchanged.

### Triggering Sync

Sync happens automatically via SSE events and periodic fallback. You can also trigger it manually:

```bash
# Sync with all peers
curl -X POST http://localhost:6830/sync/trigger \
  -H "Authorization: Bearer $TOKEN"
```

## Managing Conflicts

### Listing Conflicts

```bash
curl http://localhost:6830/sync/conflicts \
  -H "Authorization: Bearer $TOKEN"
```

### Resolving Conflicts

```bash
# Pick the auto-winner (default — higher timestamp)
curl -X POST http://localhost:6830/sync/conflicts/path/to/file \
  -H "Authorization: Bearer $TOKEN"

# Pick a specific version
curl -X POST http://localhost:6830/sync/conflicts/path/to/file \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pick": "loser"}'
```

## Selective Sync

Configure per-peer path filters:

```bash
curl -X POST http://localhost:6830/sync/peers \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "address": "https://cdn-edge:6830",
    "label": "CDN Edge",
    "sync_paths": ["/public/**"]
  }'
```

## Client Sync

In addition to peer-to-peer replication, AeorDB supports client sync using the same endpoints.

### Authentication

All sync endpoints use JWT Bearer token authentication:

| Caller | Access Level |
|--------|-------------|
| Root sync JWT (nil UUID) | Peer-replication policy: ordinary data plus registry-approved portable system families |
| Non-root JWT | Client-sync policy plus current path authorization |

The boolean compatibility selector used inside the sync implementation maps to complete registry policies; it is not an "include every system path" switch. Client sync includes ordinary authorized files and required namespace metadata such as descendant `/.aeordb-permissions` and `/.aeordb-config/` records. It omits central protected state, conflicts, derived indexes, logs, credentials, and node-local data. Non-root tokens are then filtered further:

- API key scoping rules restrict which paths are visible
- `chunk_hashes_needed` is rebuilt from the files left after registry and authorization filtering

### Example: Client Sync

```bash
# Get a JWT token
TOKEN=$(curl -s -X POST http://localhost:6830/auth/token \
  -H "Content-Type: application/json" \
  -d '{"api_key": "aeor_k_..."}' | jq -r .token)

# Sync diff — only see changes for allowed paths
curl -X POST http://localhost:6830/sync/diff \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"paths": ["/assets/**"]}'

# Fetch needed chunks
curl -X POST http://localhost:6830/sync/chunks \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"hashes": ["abc123...", "def456..."]}'
```

### Scoped API Keys for Sync

Create a scoped API key for a client that should only sync specific paths:

```bash
curl -X POST http://localhost:6830/auth/keys \
  -H "Authorization: Bearer $ROOT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "label": "Designer MacBook - assets only",
    "rules": [
      {"/assets/**": "-r--l---"},
      {"**": "--------"}
    ]
  }'
```

The client using this key will only see `/assets/` changes in sync responses, regardless of what `paths` filter it requests.

## Troubleshooting

### Node stuck in Honeymoon

The clock hasn't settled. Possible causes:
- High network jitter between nodes
- Large clock offset (> 30 seconds) — check NTP
- Firewall blocking heartbeat messages

### Sync not happening

- Check peer is in Active state: `GET /sync/peers`
- Verify the signing key is shared. If you started a node without `--join`, its JWTs won't validate on the other nodes — re-join via `/sync/join` or copy `/.aeordb-system/config/jwt_signing_key` manually
- Verify network connectivity between nodes
- Trigger manual sync: `POST /sync/trigger` — the response includes per-peer success/failure with error messages

### Data inconsistency after sync

- Check for unresolved conflicts: `GET /sync/conflicts`
- Conflicts are normal — they mean two nodes wrote the same file
- Resolve conflicts to reconcile the data
