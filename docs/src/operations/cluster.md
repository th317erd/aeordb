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
aeordb start -D data.aeordb --peers "node2:3000,node3:3000" \
  --cluster-secret-file /etc/aeordb/cluster.key

# At runtime
curl -X POST http://localhost:3000/admin/cluster/peers \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"address": "https://node2:3000", "label": "US West"}'
```

### Cluster Secret

Nodes authenticate with each other using a shared secret:

- `--cluster-secret "mysecret"` — for development (visible in process list)
- `--cluster-secret-file /path/to/secret` — for production (read from file)

The secret is hashed with BLAKE3 and stored in the database. All `/sync/*` endpoints require the `X-Cluster-Secret` header.

### TLS

Inter-node communication uses TLS by default:

- `--cluster-tls=true` (default) — require TLS
- `--cluster-tls=false` — allow plaintext (private networks)
- `--cluster-tls-cert` / `--cluster-tls-key` — custom certificates (self-signed supported)

## Monitoring

### Cluster Status

```bash
curl http://localhost:3000/admin/cluster \
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
curl -X POST http://localhost:3000/admin/cluster/sync \
  -H "Authorization: Bearer $TOKEN"
```

## Managing Conflicts

### Listing Conflicts

```bash
curl http://localhost:3000/admin/conflicts \
  -H "Authorization: Bearer $TOKEN"
```

### Resolving Conflicts

```bash
# Pick the auto-winner (default — higher timestamp)
curl -X POST http://localhost:3000/admin/conflict-dismiss/path/to/file \
  -H "Authorization: Bearer $TOKEN"

# Pick a specific version
curl -X POST http://localhost:3000/admin/conflict-resolve/path/to/file \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pick": "loser"}'
```

## Selective Sync

Configure per-peer path filters:

```bash
curl -X POST http://localhost:3000/admin/cluster/peers \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "address": "https://cdn-edge:3000",
    "label": "CDN Edge",
    "sync_paths": ["/public/**"]
  }'
```

## Troubleshooting

### Node stuck in Honeymoon

The clock hasn't settled. Possible causes:
- High network jitter between nodes
- Large clock offset (> 30 seconds) — check NTP
- Firewall blocking heartbeat messages

### Sync not happening

- Check peer is in Active state: `GET /admin/cluster`
- Check cluster secret matches on both nodes
- Verify network connectivity between nodes
- Trigger manual sync: `POST /admin/cluster/sync`

### Data inconsistency after sync

- Check for unresolved conflicts: `GET /admin/conflicts`
- Conflicts are normal — they mean two nodes wrote the same file
- Resolve conflicts to reconcile the data
