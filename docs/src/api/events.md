# Events & Webhooks

AeorDB publishes real-time events via Server-Sent Events (SSE). Clients can subscribe to a filtered stream of engine events for live updates.

## Endpoint Summary

| Method | Path | Description | Auth |
|--------|------|-------------|------|
| GET | `/events/stream` | SSE event stream | Yes |

---

## GET /events/stream

Open a persistent Server-Sent Events connection. The server pushes events as they occur and sends periodic keepalive pings.

### Query Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `events` | string | Comma-separated list of event types to receive (default: all) |
| `path_prefix` | string | Only receive events whose payload contains a path starting with this prefix |

### Request

```bash
curl -N http://localhost:3000/events/stream \
  -H "Authorization: Bearer $TOKEN"
```

### Filtered Stream

Subscribe to only specific event types:

```bash
curl -N "http://localhost:3000/events/stream?events=entries_created,entries_deleted" \
  -H "Authorization: Bearer $TOKEN"
```

Filter by path prefix:

```bash
curl -N "http://localhost:3000/events/stream?path_prefix=/data/users" \
  -H "Authorization: Bearer $TOKEN"
```

Combine both:

```bash
curl -N "http://localhost:3000/events/stream?events=entries_created&path_prefix=/data/" \
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
  'http://localhost:3000/events/stream?events=entries_created',
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

## Webhook Configuration

Webhooks can be configured in-database by storing webhook configuration files. The event bus internally broadcasts all events, and webhook delivery can be wired to the SSE stream.

Webhook configuration is stored at a well-known path within the engine and follows the same event type filtering as the SSE endpoint.
