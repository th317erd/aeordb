# Events & Webhooks

AeorDB publishes real-time events via Server-Sent Events (SSE). Clients can subscribe to a filtered stream of engine events for live updates.

## Endpoint Summary

| Method | Path | Description | Auth |
|--------|------|-------------|------|
| GET | `/system/events` | Global SSE event stream (all events, filtered by permissions) | Yes |
| GET | `/events/me` | Per-user SSE channel (events addressed to the authenticated user) | Yes |

---

## GET /system/events

Open a persistent Server-Sent Events connection. The server pushes events as they occur and sends periodic keepalive pings.

### Query Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `events` | string | Comma-separated list of event types to receive (default: all) |
| `path_prefix` | string | Only receive events whose payload contains a path starting with this prefix |

### Request

```bash
curl -N http://localhost:6830/system/events \
  -H "Authorization: Bearer $TOKEN"
```

### Filtered Stream

Subscribe to only specific event types:

```bash
curl -N "http://localhost:6830/system/events?events=entries_created,entries_deleted" \
  -H "Authorization: Bearer $TOKEN"
```

Filter by path prefix:

```bash
curl -N "http://localhost:6830/system/events?path_prefix=/data/users" \
  -H "Authorization: Bearer $TOKEN"
```

Combine both:

```bash
curl -N "http://localhost:6830/system/events?events=entries_created&path_prefix=/data/" \
  -H "Authorization: Bearer $TOKEN"
```

### Response Format

The response is an SSE stream. Each event has the standard SSE fields:

```
id: evt-uuid-here
event: entries_created
data: {"event_id":"evt-uuid-here","event_type":"entries_created","timestamp":1775968398000,"payload":{"entries":[{"path":"/data/report.pdf"}]}}

```

### Event Envelope

Each event is a JSON object with:

| Field | Type | Description |
|-------|------|-------------|
| `event_id` | string | Unique event identifier |
| `event_type` | string | Type of event (see below) |
| `timestamp` | integer | Unix timestamp (milliseconds) |
| `payload` | object | Event-specific data |

### Event Types

| Event Type | Description | Payload |
|------------|-------------|---------|
| `entries_created` | Files were created or updated | `{"entries": [{"path": "..."}]}` |
| `entries_deleted` | Files were deleted | `{"entries": [{"path": "..."}]}` |
| `versions_created` | A new version (snapshot/fork) was created | Version metadata |
| `permissions_changed` | Permissions were updated for a path | `{"path": "..."}` |
| `indexes_changed` | Index configuration was updated | `{"path": "..."}` |
| `heartbeat` | Clock synchronization pulse (every 15s) | `{"intent_time", "construct_time", "node_id"}` |
| `metrics` | System metrics snapshot (every 15s) | `{"counts", "sizes", "throughput", "health"}` |

---

### Heartbeat Event

The `heartbeat` event is used exclusively for **clock synchronization** between nodes. It fires every 15 seconds and carries only three fields:

```json
{
  "intent_time": 1776563925000,
  "construct_time": 1776563925003,
  "node_id": 1
}
```

| Field | Type | Description |
|-------|------|-------------|
| `intent_time` | integer | Timestamp (ms) when the heartbeat was scheduled to fire |
| `construct_time` | integer | Timestamp (ms) when the heartbeat payload was actually constructed |
| `node_id` | integer | The node that emitted this heartbeat |

The delta between `intent_time` and `construct_time` is used by peers to measure clock offset and network latency. **The heartbeat does not contain any stats or metrics data** — it is a lightweight clock-sync mechanism only.

> **Breaking change:** Prior versions included stats fields (file counts, disk usage, etc.) in the heartbeat payload. These fields have been removed. Use the `metrics` event for monitoring data.

---

### Metrics Event

The `metrics` event delivers system metrics to connected clients every 15 seconds (configurable, independent of the heartbeat interval). All values are computed in O(1) from atomic counters — subscribing to this event has no performance impact on the server.

**Subscribe to metrics:**

```bash
curl -N "http://localhost:6830/system/events?events=metrics" \
  -H "Authorization: Bearer $TOKEN"
```

**Payload:**

```json
{
  "counts": {
    "files": 150000,
    "directories": 23000,
    "symlinks": 500,
    "chunks": 420000,
    "snapshots": 12,
    "forks": 2
  },
  "sizes": {
    "disk_total": 2147483648,
    "kv_file": 86114304,
    "logical_data": 1800000000,
    "chunk_data": 1200000000,
    "void_space": 5242880,
    "dedup_savings": 600000000
  },
  "throughput": {
    "writes_per_sec": { "1m": 42.3, "5m": 38.1, "15m": 35.7, "peak_1m": 120.0 },
    "reads_per_sec": { "1m": 156.2, "5m": 140.5, "15m": 138.0, "peak_1m": 450.0 },
    "bytes_written_per_sec": { "1m": 435200, "5m": 392000, "15m": 367000 },
    "bytes_read_per_sec": { "1m": 16065536, "5m": 14450000, "15m": 14200000 }
  },
  "health": {
    "disk_usage_percent": 48.5,
    "kv_fill_ratio": 0.72,
    "dedup_hit_rate": 0.33,
    "write_buffer_depth": 42
  }
}
```

**Payload sections:**

| Section | Description |
|---------|-------------|
| `counts` | Current totals for files, directories, symlinks, chunks, snapshots, and forks |
| `sizes` | Byte-level storage breakdown: disk total, KV file, logical data, chunk data, void space, dedup savings |
| `throughput` | Rolling rates (1m, 5m, 15m averages and peak) for read/write operations and bytes |
| `health` | Operational health signals: disk usage, KV fill ratio, dedup efficiency, write buffer depth |

> **Migration note:** If your dashboard previously subscribed to `?events=heartbeat` for monitoring data, switch to `?events=metrics`. If you need both clock data and metrics (uncommon), subscribe to `?events=heartbeat,metrics`.

### Keepalive

The server sends a keepalive ping every **30 seconds** to prevent connection timeouts:

```
: ping

```

### Path Prefix Matching

The path prefix filter checks two locations in the event payload:

1. **Batch events:** `payload.entries[].path` -- matches if any entry's path starts with the prefix
2. **Single-path events:** `payload.path` -- matches if the path starts with the prefix

### Connection Behavior

- The connection stays open indefinitely until the client disconnects.
- If the client falls behind (lagged), missed events are silently dropped.
- Reconnecting clients should use the last received `id` for gap detection (standard SSE `Last-Event-ID` header).

### JavaScript Example

```javascript
const evtSource = new EventSource(
  'http://localhost:6830/system/events?events=entries_created',
  { headers: { 'Authorization': 'Bearer ' + token } }
);

evtSource.addEventListener('entries_created', (event) => {
  const data = JSON.parse(event.data);
  console.log('Files created:', data.payload.entries);
});

evtSource.onerror = (err) => {
  console.error('SSE error:', err);
};
```

---

## GET /events/me

A per-user SSE channel that delivers ONLY events addressed to the authenticated user. The server filters the event bus and forwards an event only when its `recipient_user_id` matches the JWT's `sub` claim. Generic events with no recipient (heartbeats, system metrics, file uploads, etc.) are NOT delivered here — those go through `/system/events`.

This channel is the security boundary for personal notifications: each user can only see events sent specifically to them, even if multiple users are subscribed simultaneously.

### Request

```bash
curl -N http://localhost:6830/events/me \
  -H "Authorization: Bearer $TOKEN"
```

EventSource (browsers can't set Authorization headers on SSE):

```javascript
const evt = new EventSource('/events/me?token=' + encodeURIComponent(token));
evt.addEventListener('files_shared', (e) => {
  const payload = JSON.parse(e.data).payload;
  alert(`${payload.from} shared ${payload.path} with you`);
});
```

### Event Types Currently Routed Here

| Event | Payload | Triggered By |
|-------|---------|--------------|
| `files_shared` | `{ path, permissions, from }` | A `POST /files/share` call where the recipient is in the `users` list. One event per (recipient, path). |

Additional per-user event types (group invitations, mentions, etc.) will be added on this channel — the recipient field is the routing boundary.

### Event Envelope

```json
{
  "event_id": "uuid",
  "event_type": "files_shared",
  "timestamp": 1778391000000,
  "user_id": "00000000-0000-0000-0000-000000000000",
  "recipient_user_id": "6874d1cd-…",
  "payload": {
    "path": "/Pictures/Family/photo.jpg",
    "permissions": ".r..l...",
    "from": "Root"
  }
}
```

`user_id` is the actor (who performed the action). `recipient_user_id` matches the authenticated subscriber.

---

## Webhook Configuration

Webhooks can be configured in-database by storing webhook configuration files. The event bus internally broadcasts all events, and webhook delivery can be wired to the SSE stream.

Webhook configuration is stored at a well-known path within the engine and follows the same event type filtering as the SSE endpoint.
