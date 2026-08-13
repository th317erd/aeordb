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

When the stream is available, the server is ready to serve the full API. For unfiltered streams, and for streams that explicitly include `server_ready`, the first event is a synthetic `server_ready` event with the current ready state. This event is generated per connection; it is not a one-time broadcast that clients can miss.

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

Path-bearing events are projected for each subscriber before serialization.
A direct root JWT receives complete global events. A scoped root API key still
applies its key rules to path-bearing events. For normal users, every path must
be readable under current user/group permissions; user-owned API-key rules are
an additional bound, while share keys use their active rules as their sole path
authority. Non-root subscribers never receive paths concealed by the
SystemFamily registry, including credential and other engine authority records.
These checks and `path_prefix` are applied to every member of
`payload.entries`; a mixed batch keeps its visible entries and omits denied
siblings. A single-path event is omitted when its path is not visible, and a
batch is omitted when no entries remain.

The stream rechecks mutable user/group and API-key authority as events arrive.
Revoked, expired, removed, or identity-mismatched keys stop receiving events
without requiring a reconnect. Recipient-addressed events are never published
on this global channel; they are available only through `/events/me` to the
matching recipient. For non-root subscribers, only `server_ready`, `heartbeat`,
and path-required relationship events are eligible. Task, GC, version, import,
sync, metrics, unknown, and other administrative events are root-only. A
malformed path-required event with no path fails closed for non-root streams.

### Response Format

The response is an SSE stream. Each event has the standard SSE fields:

```
id: evt-uuid-here
event: entries_created
data: {"event_id":"evt-uuid-here","event_type":"entries_created","timestamp":1775968398000,"payload":{"entries":[{"path":"/data/report.pdf"}],"operation_id":"mutation-uuid-here","publication_sequence":42,"mutation_kind":"file_write"}}

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
| `server_ready` | Synthetic first event on ready SSE connections | `{"status": "ready", "version": "...", "startup_time": 1781233139578, "uptime_ms": 6500}` |
| `stream_gap` | This connection fell behind the bounded event buffer and must refresh authoritative state | `{"missed_events": 3, "action": "refresh"}` |
| `entries_created` | Files were created or updated | `{"entries": [{"path": "..."}], "operation_id": "...", "publication_sequence": 42, "mutation_kind": "file_write"}` |
| `entries_deleted` | Files were deleted | `{"entries": [{"path": "..."}], "operation_id": "...", "publication_sequence": 43, "mutation_kind": "file_delete"}` |
| `versions_created` | A new version (snapshot/fork) was created | Version metadata plus namespace acknowledgement |
| `versions_deleted` | A snapshot/fork was deleted or abandoned | Version metadata plus namespace acknowledgement |
| `versions_restored` | HEAD moved to a retained snapshot root | Version metadata plus namespace acknowledgement |
| `versions_promoted` | A fork root was promoted to HEAD | Version metadata plus namespace acknowledgement |
| `imports_completed` | A backup/patch import completed | Import counts/root metadata; root-changing promoted imports also include namespace acknowledgement fields |
| `permissions_changed` | Permissions were updated for a path | `{"path": "..."}` |
| `indexes_changed` | Index configuration was updated | `{"path": "..."}` |
| `tasks_started` | A background task began execution | `{"task_id": "...", "task_type": "...", "args": {...}}` |
| `tasks_deferred` | A claimed maintenance task was safely returned to `Pending` for retry | `{"task_id": "...", "task_type": "...", "reason": "...", "retryable": true, "retry_at": 1786084780168, "retry_after_ms": 40000, "deferral_count": 4}` |
| `tasks_completed` | A background task completed | `{"task_id": "...", "task_type": "...", "summary": "..."}` |
| `tasks_failed` | A background task failed terminally | `{"task_id": "...", "task_type": "...", "error": "..."}` |
| `tasks_cancelled` | A running task cancellation was observed | `{"task_id": "...", "task_type": "..."}` |
| `gc_status` | Root-only immediate GC phase/terminal status | Bounded GC status object, with optional `task_id` |
| `heartbeat` | Clock synchronization pulse (every 15s) | `{"intent_time", "construct_time", "node_id"}` |
| `metrics` | Root-only administrative runtime snapshot (every 15s) | `{"counts", "sizes", "throughput", "health", "memory", "durability", "configuration"}` |

Coordinator-owned namespace mutations add these acknowledgement fields to
entry and version relationship payloads only after the exact hard publication
is durable:

| Field | Type | Description |
|-------|------|-------------|
| `operation_id` | string | Unique UUID for the logical namespace mutation |
| `publication_sequence` | integer | Exact durability sequence acknowledged by the engine |
| `mutation_kind` | string | Closed mutation family such as `file_write`, `file_delete`, `directory_create`, `directory_delete`, `symlink_write`, `symlink_delete`, `batch_write`, `merge`, `copy`, `rename`, `restore`, `promote`, `import`, `sync_apply`, or `system_write` |

AeorDB is migrating producers to this shared acknowledgement path in stages.
File, directory, symlink, blob/buffered batch, JSON merge, copy, rename,
snapshot/fork, promoted import, explicit HEAD-promotion, and sync-apply
producers now use it. Other system/plugin and maintenance producers may omit these three
fields until their producer wave is converted. An import that does not change
HEAD, including `promote=false` and same-root no-ops, has no root-mutation
acknowledgement to attach. Clients must therefore treat the fields as optional
during the transition, but must not interpret an absent field as a durability
failure.

A logical mutation can emit more than one relationship event. File and symlink
rename preserve separate `entries_deleted` and `entries_created` events, but
both events carry the same `operation_id`, `publication_sequence`, and
`mutation_kind: "rename"`. Batch, merge, and copy operations emit one aggregate
`entries_created` event after their shared hard acknowledgement.

A sync receipt can emit aggregate `entries_created` and `entries_deleted`
events. Both carry the same `operation_id`, `publication_sequence`, and
`mutation_kind: "sync_apply"`, and are emitted only after the complete bounded
merge and any required local conflict evidence are durable. A no-op retry emits
no namespace acknowledgement event.

Fork promotion preserves separate `versions_promoted` and `versions_deleted`
events for compatibility. Both describe one atomic HEAD transition/fork
retirement and therefore carry the same operation ID and publication sequence.
A restore whose snapshot already equals HEAD is a true no-op and emits no
acknowledgement event.

---

### Server Ready Event

The `server_ready` event is emitted as the first SSE event for each eligible `/system/events` connection. Because `/system/events` is only served by the full application router, receiving this event means the server has finished startup and is ready for normal API traffic.

It is sent when:

- no `events` filter is supplied
- `server_ready` is included in the `events` filter

It is not sent when an `events` filter excludes it. This preserves strict event filtering for clients that only want a specific event type.

```json
{
  "status": "ready",
  "version": "0.9.5",
  "startup_time": 1781233139578,
  "uptime_ms": 6500
}
```

| Field | Type | Description |
|-------|------|-------------|
| `status` | string | Always `"ready"` |
| `version` | string | AeorDB server version |
| `startup_time` | integer | Server startup timestamp in Unix milliseconds |
| `uptime_ms` | integer | Current uptime at the moment this SSE connection was accepted |

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

### GC Status Event

`gc_status` is emitted immediately whenever the shared GC executor accepts a
new phase or terminal projection. It is root-only, even when a non-root client
explicitly requests `events=gc_status`. The payload is the exact object shown
at `GET /system/stats` under `health.gc`; task-originated runs include
`task_id`.

```bash
curl -N "http://localhost:6830/system/events?events=gc_status" \
  -H "Authorization: Bearer $TOKEN"
```

The stream is transition delivery, not durable history. Reconnect and refresh
`GET /system/stats` after a connection error or `stream_gap`; the engine
retains only one current/latest status until another run or process restart.

---

### Metrics Event

The `metrics` event delivers a bounded administrative snapshot every 15 seconds, independent of the heartbeat interval. Only the root user receives this event; non-root streams filter it even when `events=metrics` is requested. Non-root dashboards use authenticated `/system/stats` polling, where root-only paths are explicitly redacted.

The producer reads counters, fixed memory-owner/configuration registries, in-memory durability/recovery state, and bounded operating-system probes. It never scans the WAL, KV store, file bodies, or index files, and metrics collection does not trigger cache eviction.

**Subscribe to metrics:**

```bash
curl -N "http://localhost:6830/system/events?events=metrics,gc_status" \
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
    "write_buffer_depth": 42,
    "gc": {"run_id": "...", "state": "running", "phase": "mark", "overall_progress": 0.5}
  },
  "memory": {
    "process": {
      "rss_bytes": 2147483648,
      "peak_rss_bytes": 3221225472,
      "virtual_bytes": 8589934592,
      "data_bytes": 1610612736,
      "swap_bytes": 0,
      "thread_count": 32,
      "fd_count": 128,
      "private_bytes": 1900000000,
      "shared_bytes": 247483648,
      "mapped_bytes": 805306368,
      "allocator_bytes": null
    },
    "coordinator": {
      "pressure": "normal",
      "maintenance_paused": false,
      "accounted_bytes": 1505755136,
      "unaccounted_rss_bytes": 641728512,
      "rejected_reservations": 0,
      "deferred_reservations": 0,
      "owners": []
    },
    "index_cache": {
      "cached_indexes": 16,
      "dirty_indexes": 4,
      "deleted_indexes": 0,
      "pending_mutations": 512,
      "total_mutations": 102400,
      "flushes": 7,
      "flushed_indexes": 92,
      "evictions": 3,
      "evicted_indexes": 12,
      "evicted_bytes": 268435456,
      "entries": 2500000,
      "values": 350000,
      "estimated_bytes": 734003200,
      "estimated_clean_bytes": 503316480,
      "estimated_dirty_bytes": 230686720,
      "clean_reserved_bytes": 503316480,
      "dirty_reserved_bytes": 234881024,
      "flush_reserved_bytes": 16777216,
      "flushing_indexes": 1,
      "max_bytes": 2147483648,
      "mutation_max_bytes": 1073741824,
      "publication_batch_max_bytes": 268435456,
      "clean_ttl_ms": 300000,
      "reservation_owned": true,
      "top_cached_indexes": [
        {
          "parent": "/",
          "field_name": "@path",
          "strategy": "trigram",
          "entries": 1200000,
          "values": 80000,
          "estimated_bytes": 234881024,
          "dirty": false,
          "last_access_age_ms": 42000
        }
      ]
    },
    "directory_cache": {
      "entries": 12000,
      "estimated_bytes": 16777216
    },
    "caches": {
      "permissions_entries": 128,
      "index_config_entries": 16,
      "grants_index_entries": 4,
      "group_entries": 64,
      "api_key_entries": 32
    },
    "estimated_engine_owned_bytes": 767557632
  },
  "durability": {
    "frontier": { "hard_frontier": 4521, "next_sequence": 4522, "waiter_depth": 0, "last_barrier": null },
    "group_policy": { "enabled": true, "max_bytes": 67108864, "max_delay_ms": 100, "disabled_reason": null },
    "latch": { "read_only": false, "runtime_failure": null, "persistent_recovery": null },
    "spill": { "count": 0, "total_bytes": 0, "locations": [], "latest": null },
    "repair": { "required": false, "state": "not_required", "command": null, "progress": null }
  },
  "configuration": {
    "runtime": { "config": {}, "status": { "valid": true, "degraded": false, "sources": {} } },
    "lifecycle": { "config": {}, "status": { "valid": true, "degraded": false, "sources": {} } }
  }
}
```

The metrics event's `memory` snapshot is for the primary data engine. A
separate identity engine selected by `--auth file://...` owns its own bounded
API-key cache and is not yet included in this object.

`memory.process` is sampled from the operating system. Optional fields are `null` when unsupported. The index `estimated_*` fields and owner observations remain bounded estimates, while coordinator reservations are exact. Dirty reservations exclude flush scratch, so the `index_dirty_buffers` owner reconciles to `dirty_reserved_bytes + flush_reserved_bytes`. Clean index cache entries are evicted after the resolved idle TTL or cache cap; dirty and flushing generations remain reserved and non-evictable until publication succeeds or their exact state is restored after failure.

**Payload sections:**

| Section | Description |
|---------|-------------|
| `counts` | Current totals for files, directories, symlinks, chunks, snapshots, and forks |
| `sizes` | Byte-level storage breakdown: disk total, KV file, logical data, chunk data, void space, dedup savings |
| `throughput` | Rolling rates (1m, 5m, 15m averages and peak) for read/write operations and bytes |
| `health` | Operational health signals plus the root-only current/latest GC projection when present |
| `memory` | Process, policy, pressure, fixed owner, reservation, and cache diagnostics |
| `durability` | Hard frontier, waiters, last barrier, grouping, read-only latch, spill, and repair state |
| `configuration` | Complete runtime/lifecycle values, sources, validity, degradation, and pending activation |

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
- If a client falls behind the bounded broadcast buffer, the server emits a
  `stream_gap` event on that connection with the exact number of events missed.
  The client must refresh the authoritative listing/query/state it derives from
  SSE before it resumes applying incremental updates.
- `stream_gap` is delivered regardless of the `events` filter because the
  server cannot prove that every skipped event was irrelevant to that filter.
- Event IDs are unique correlation identifiers, not a replay cursor. AeorDB
  does not retain an SSE replay log and does not honor `Last-Event-ID` for
  recovery.

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

evtSource.addEventListener('stream_gap', async (event) => {
  const data = JSON.parse(event.data);
  console.warn(`Missed ${data.payload.missed_events} events; refreshing`);
  await refreshCurrentView();
});

evtSource.onerror = (err) => {
  console.error('SSE error:', err);
};
```

---

## GET /events/me

A per-user SSE channel that delivers ONLY events addressed to the authenticated user. The server filters the event bus and forwards an event only when its `recipient_user_id` matches the JWT's `sub` claim. Generic events with no recipient (heartbeats, system metrics, file uploads, etc.) are NOT delivered here — those go through `/system/events`.

This channel is the security boundary for personal notifications: each user can only see events sent specifically to them, even if multiple users are subscribed simultaneously.

The per-user stream emits the same `stream_gap` control event if its receiver
falls behind. Clients must refresh their notification/share state before
continuing with incremental events.

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
