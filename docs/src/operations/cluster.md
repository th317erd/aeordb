# Cluster Operations

## Setting Up a Cluster

### Starting the First Node

Start a node normally. It operates as a standalone database:

```bash
aeordb start -D data.aeordb --auth self
```

### Adding Peers

Add peers at startup or at runtime:

```bash
# At startup
aeordb start -D data.aeordb --peers "node2:6830,node3:6830"

# At runtime
curl -X POST http://localhost:6830/sync/peers \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"address": "https://node2:6830", "label": "US West"}'
```

### Authentication

All `/sync/*` endpoints require JWT authentication. Nodes use root JWT tokens (nil UUID) for peer-to-peer sync. The `.system/` namespace is never exposed through the HTTP file APIs (`/files/`, `/links/`, `/blobs/`) -- not even to root users. Instead, the sync system transfers `.system/` data internally through the `/sync/diff` and `/sync/chunks` endpoints when the caller is authenticated as a root JWT user (handled automatically via the `SyncCaller::RootUser` mechanism). Operators do not need to manage `.system/` data manually.

### TLS

Inter-node communication uses TLS by default:

- `--cluster-tls=true` (default) — require TLS
- `--cluster-tls=false` — allow plaintext (private networks)
- `--cluster-tls-cert` / `--cluster-tls-key` — custom certificates (self-signed supported)

## Monitoring

### Cluster Status

```bash
curl http://localhost:6830/sync/ \
  -H "Authorization: Bearer $TOKEN"
```

Returns: node ID, peer count, peer list with connection states.

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
| Root JWT (nil UUID) | Full sync access -- `.system/` data is included automatically by the sync internals |
| Non-root JWT | Filtered access (scoped) |

The `.system/` namespace is completely invisible through all HTTP file APIs, regardless of caller privilege. During sync, `.system/` data is transferred internally when the sync system detects a root JWT caller -- this is handled by the `include_system()` mechanism and requires no operator intervention. Non-root tokens get filtered results:

- API key scoping rules restrict which paths are visible
- Chunks with the `FLAG_SYSTEM` flag are never served through file APIs

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

- Check peer is in Active state: `GET /sync/`
- Verify JWT tokens are valid and use the same signing key on both nodes
- Verify network connectivity between nodes
- Trigger manual sync: `POST /sync/trigger`

### Data inconsistency after sync

- Check for unresolved conflicts: `GET /sync/conflicts`
- Conflicts are normal — they mean two nodes wrote the same file
- Resolve conflicts to reconcile the data
