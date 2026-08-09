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

### Startup Authority And Failure Semantics

Each database has one persisted, nonzero cluster node ID. AeorDB creates it
exactly once; concurrent initialization selects one acknowledged value, and a
later startup never replaces it. The persisted peer list is a versioned,
1 MiB-bounded authority document. Peer add, upsert, remove, and startup
registration operations decide against the current document and publish one
atomic replacement, so concurrent accepted changes are not lost.

Runtime peer state is derived from that document. AeorDB updates the in-memory
peer manager only after the corresponding persistent mutation is acknowledged.
An unchanged startup peer or upsert is a true no-op: it does not reorder the
document or consume another durability sequence.

Cluster authority is required for a writable server startup. AeorDB refuses to
construct the application router when the node ID, peer document, or required
bundled-plugin authority is malformed, oversized, unreadable, or internally
inconsistent. A local node ID that collides with a persisted peer ID is also a
startup error. Likewise, an explicit `--peers` registration failure aborts
startup instead of serving without the requested peers. While initialization
is in progress or has failed, the startup health surface reports that state;
ordinary application routes are not exposed as ready.

Bundled-plugin replacement policy is evaluated against the current record
under namespace authority. Canonical no-op records do not require WASM compile
admission; records that may change are validated and then rechecked before one
atomic per-plugin publication. Stored checksums and record paths are verified
before use, and compiled runtimes are identified by path plus checksum rather
than path alone.

### Authentication

All `/sync/*` endpoints require JWT authentication. Nodes mint short-lived root JWTs (nil UUID `sub`) internally when calling each other's `/sync/diff` and `/sync/chunks` endpoints. This works because every node in the cluster shares the same Ed25519 signing key (distributed by `/sync/join`).

The signing seed is exactly 32 bytes. First startup installs it through one
atomic system-namespace decision, so concurrent authentication providers all
receive the same persisted winner and consume one durability sequence. Reads,
readiness checks, and join reject malformed or oversized seed authority rather
than treating it as absent or regenerating it. Legacy system config keys are
limited to 255 bytes, cannot contain path-shaping characters, and their values
are limited to 1 MiB.

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
# Pick the auto-winner (normally the higher virtual timestamp)
curl -X POST http://localhost:6830/sync/resolve/path/to/file \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pick": "winner"}'

# Pick a specific version
curl -X POST http://localhost:6830/sync/resolve/path/to/file \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"pick": "loser"}'

# Accept the current auto-winner and only remove the retained evidence
curl -X POST http://localhost:6830/sync/dismiss/path/to/file \
  -H "Authorization: Bearer $TOKEN"
```

The selected version may be a file, symlink, or deletion. AeorDB verifies its
exact immutable identity and, for files, every referenced chunk and whole-file
hash before publication. The chosen mutation and deletion of the local
`/.aeordb-conflicts/.../.meta` evidence are one acknowledged operation. Missing
or corrupt selected data, malformed evidence, and cleanup failure are surfaced;
they are not converted into successful resolution. Conflict evidence is local
authority and remains omitted from peer replication.

Sync results report `conflicts_detected` as the number of newly retained
winner/loser pairs. Detached portable system families can expose the same pair
again after peer checkpoints advance; when its exact metadata and immutable
versions still validate, AeorDB reuses the existing local evidence and reports
zero new conflicts. That decision and any new evidence publication are
serialized under namespace authority, so concurrent cycles from different
peers cannot both count or rewrite one pair. Malformed or incomplete existing
evidence fails the sync closed instead of being silently replaced or counted as
a fresh conflict.

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

### Transport And Apply Contract

`POST /sync/diff` returns file identity hashes, optional whole-file
`content_hash` values for migrated records, original `created_at` and
`updated_at` timestamps, symlink identities, deletions, and the exact set of
chunks required by the filtered response. The built-in receiver rejects
unknown response fields, noncanonical or duplicate paths, invalid hash widths,
identity mismatches, duplicate or unrelated chunk-manifest entries, more than
100,000 namespace operations, or more than 1,000,000 file-to-chunk references.
Its diff response reader is bounded to 128 MiB.

`POST /sync/chunks` accepts at most 10,000 hashes and caps its serialized
response at 512 MiB. It builds that response in an engine-local temporary file
and streams memory-admitted 64 KiB frames instead of retaining a second full
base64/JSON response in memory. Missing, malformed, and inaccessible requested
hashes are omitted for wire compatibility, but storage corruption or an
operational read failure fails the request. The built-in peer receiver requests
at most 256 hashes at a time, accepts at most 96 MiB of JSON and 64 MiB of
decoded bytes per response, and requires exactly one valid response entry for
every hash it requested. Memory pressure, shutdown, or cancellation returns a
retryable `503`; oversized requests/responses must be split and retried.

Chunk publication is content-addressed and may precede namespace authority.
The namespace merge itself is one memory-admitted hard receipt across files,
symlinks, deletions, parent directories, and local conflict evidence. A failed
receipt publishes none of those changes and does not advance the peer
checkpoint. After success, AeorDB stores both the acknowledged remote root and
the local post-merge root. Those distinct roots prevent local-only changes from
being reclassified as remote changes on the next three-way merge. Checkpoint
save failure is retryable: replaying the same response is idempotent.

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
