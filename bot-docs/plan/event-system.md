# Event System — Implementation Spec

**Date:** 2026-04-06
**Status:** Approved

---

## 1. Overview

Real-time event system for AeorDB. Every meaningful database mutation produces a structured event delivered through three mechanisms:

- **In-process** — `tokio::broadcast` channel for Rust library consumers
- **SSE** — `GET /events/stream` for HTTP clients (dashboards, monitoring, tools)
- **Webhooks** — HTTP POST callbacks to registered URLs for external integrations

Fire-and-forget delivery. The database never blocks on event subscribers. Slow consumers skip missed events.

---

## 2. EventBus

Shared `Arc<EventBus>` passed to all layers that emit events.

```rust
pub struct EventBus {
    sender: broadcast::Sender<EngineEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self;
    pub fn emit(&self, event: EngineEvent);
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent>;
    pub fn begin_scope(&self, user_id: String) -> EventScope;
}
```

### EventScope (batching)

Collects events during a logical operation. On close, emits them as a single batched event. For single operations, the scope contains one item.

```rust
pub struct EventScope {
    bus: Arc<EventBus>,
    user_id: String,
    entries_created: Vec<EntryEventData>,
    entries_updated: Vec<EntryEventData>,
    entries_deleted: Vec<EntryEventData>,
    // ... other collected events
}

impl EventScope {
    pub fn add_entry_created(&mut self, data: EntryEventData);
    pub fn add_entry_deleted(&mut self, data: EntryEventData);
    pub fn close(self); // emits all collected events as batched events
}

impl Drop for EventScope {
    fn drop(&mut self) { /* flush on drop as safety net */ }
}
```

Multi-file upload opens a scope, does N stores, closes the scope → one `EntriesCreated` event with N items. Single `store_file` opens and closes immediately → `EntriesCreated` with 1 item.

### Capacity and backpressure

Default channel capacity: 1024 events. If a subscriber falls behind, `tokio::broadcast` returns `Lagged(n)` — the subscriber skips missed events. The emitter is never blocked.

---

## 3. Event Envelope

Every event shares a common envelope:

```rust
pub struct EngineEvent {
    pub event_id: uuid::Uuid,
    pub event_type: String,
    pub timestamp: i64,        // ms since epoch — when the event was emitted
    pub user_id: String,       // UUID of acting user, or "system"
    pub payload: EventPayload, // type-specific data
}
```

JSON serialization:
```json
{
  "event_id": "550e8400-e29b-41d4-a716-446655440000",
  "event_type": "entries_created",
  "timestamp": 1775517005796,
  "user_id": "00000000-0000-0000-0000-000000000000",
  "payload": { ... }
}
```

---

## 4. Event Types (19)

### Entry Events (3)

Emitted by: `DirectoryOps`

**EntriesCreated**
```json
{
  "entries": [
    {
      "path": "/people/smith.json",
      "entry_type": "file",
      "content_type": "application/json",
      "size": 1234,
      "hash": "a1b2c3...",
      "created_at": 1775517005000,
      "updated_at": 1775517005000
    }
  ]
}
```

**EntriesUpdated**
```json
{
  "entries": [
    {
      "path": "/people/smith.json",
      "entry_type": "file",
      "content_type": "application/json",
      "size": 1500,
      "hash": "d4e5f6...",
      "previous_hash": "a1b2c3...",
      "created_at": 1775517005000,
      "updated_at": 1775517010000
    }
  ]
}
```

**EntriesDeleted**
```json
{
  "entries": [
    {
      "path": "/people/smith.json",
      "entry_type": "file",
      "content_type": "application/json",
      "size": 1234,
      "hash": "a1b2c3...",
      "created_at": 1775517005000,
      "updated_at": 1775517005000
    }
  ]
}
```

### Version Events (4)

Emitted by: `VersionManager`

**VersionsCreated**
```json
{
  "versions": [
    {
      "name": "v1.0.0",
      "version_type": "snapshot",
      "root_hash": "f6e5d4...",
      "created_at": 1775517005000
    }
  ]
}
```

**VersionsDeleted**
```json
{
  "versions": [
    {
      "name": "v1.0.0",
      "version_type": "snapshot",
      "root_hash": "f6e5d4..."
    }
  ]
}
```

**VersionsPromoted**
```json
{
  "versions": [
    {
      "name": "feature-branch",
      "root_hash": "a1b2c3..."
    }
  ]
}
```

**VersionsRestored**
```json
{
  "versions": [
    {
      "name": "v1.0.0",
      "root_hash": "f6e5d4..."
    }
  ]
}
```

### User Events (3)

Emitted by: `SystemTables` / admin routes

**UsersCreated**
```json
{
  "users": [
    {
      "target_user_id": "550e8400-...",
      "username": "alice",
      "email": "alice@example.com"
    }
  ]
}
```

**UsersActivated**
```json
{
  "users": [
    {
      "target_user_id": "550e8400-...",
      "username": "alice"
    }
  ]
}
```

**UsersDeactivated**
```json
{
  "users": [
    {
      "target_user_id": "550e8400-...",
      "username": "alice"
    }
  ]
}
```

### Admin Events (4)

**PermissionsChanged** — Emitted by: permission middleware / admin routes
```json
{
  "changes": [
    {
      "path": "/people/",
      "group_name": "engineers",
      "action": "updated"
    }
  ]
}
```

**ImportsCompleted** — Emitted by: `backup::import_backup`
```json
{
  "imports": [
    {
      "backup_type": "patch",
      "version_hash": "a1b2c3...",
      "entries_imported": 47,
      "head_promoted": false
    }
  ]
}
```

**IndexesUpdated** — Emitted by: `IndexingPipeline`
```json
{
  "indexes": [
    {
      "path": "/people/",
      "field_name": "name",
      "strategy": "trigram",
      "entry_count": 150
    }
  ]
}
```

**Errors** — Emitted by: any layer
```json
{
  "errors": [
    {
      "path": "/people/upload.pdf",
      "error_type": "parser_failed",
      "message": "parser 'pdf-extractor' returned invalid JSON"
    }
  ]
}
```

### Auth Events (3)

Emitted by: auth routes

**TokensExchanged**
```json
{
  "tokens": [
    {
      "target_user_id": "550e8400-...",
      "method": "api_key"
    }
  ]
}
```

**ApiKeysCreated**
```json
{
  "keys": [
    {
      "target_user_id": "550e8400-...",
      "key_id": "ak_123..."
    }
  ]
}
```

**ApiKeysRevoked**
```json
{
  "keys": [
    {
      "target_user_id": "550e8400-...",
      "key_id": "ak_123..."
    }
  ]
}
```

### Plugin Events (2)

Emitted by: plugin routes

**PluginsDeployed**
```json
{
  "plugins": [
    {
      "name": "pdf-extractor",
      "path": "pdf-extractor",
      "plugin_type": "wasm"
    }
  ]
}
```

**PluginsRemoved**
```json
{
  "plugins": [
    {
      "name": "pdf-extractor",
      "path": "pdf-extractor"
    }
  ]
}
```

### System Events (1)

**Heartbeat** — Emitted by: dedicated tokio task, every 15 seconds, aligned to wall clock

```json
{
  "stats": {
    "entry_count": 42000,
    "kv_entries": 38000,
    "chunk_count": 20000,
    "file_count": 15000,
    "directory_count": 500,
    "snapshot_count": 3,
    "fork_count": 1,
    "void_count": 200,
    "void_space_bytes": 1048576,
    "db_file_size_bytes": 104857600,
    "kv_size_bytes": 2097152,
    "nvt_buckets": 1024
  }
}
```

The heartbeat targets the 0th, 15th, 30th, and 45th second of each minute with millisecond-level precision. Uses `tokio::time::interval` with initial delay calculated to align to the next 15-second boundary.

---

## 5. SSE Endpoint

```
GET /events/stream?events=entries_created,entries_deleted&path_prefix=/people/
```

### Protocol

Standard Server-Sent Events (`text/event-stream`):

```
id: 550e8400-e29b-41d4-a716-446655440000
event: entries_created
data: {"event_id":"550e8400-...","event_type":"entries_created","timestamp":1775517005796,"user_id":"...","payload":{"entries":[...]}}

id: 660f9500-f39c-51e5-b827-557766550000
event: heartbeat
data: {"event_id":"660f9500-...","event_type":"heartbeat","timestamp":1775517015000,"user_id":"system","payload":{"stats":{...}}}

```

### Filtering

Query parameters (all optional):
- `events` — comma-separated list of event types to receive (default: all)
- `path_prefix` — only receive entry/index/permission events matching this path prefix (default: all paths)

### Authentication

Protected route — requires valid Bearer token. Goes through the auth middleware like all other protected endpoints. In `--auth=false` mode, all connections are allowed (NoAuthProvider injects root claims).

### Reconnection

The SSE `id` field is set to the event's `event_id`. On reconnect, the browser sends `Last-Event-ID` header. The server can use this to detect reconnection, but since we don't buffer past events, the client simply starts receiving from the current point. No replay of missed events.

### Implementation

The SSE handler subscribes to the EventBus, filters events by the client's query params, and streams them as SSE frames using axum's `Sse` response type with `tokio_stream`.

---

## 6. Webhooks

### Configuration

Stored in the database at `/.config/webhooks.json`:

```json
{
  "webhooks": [
    {
      "id": "wh_001",
      "url": "https://hooks.slack.com/services/...",
      "events": ["entries_created", "entries_deleted"],
      "path_prefix": "/people/",
      "secret": "hmac-shared-secret",
      "active": true
    },
    {
      "id": "wh_002",
      "url": "https://ci.example.com/aeordb-hook",
      "events": ["versions_created"],
      "secret": "another-secret",
      "active": true
    }
  ]
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | string | Yes | Unique webhook identifier |
| `url` | string | Yes | HTTPS URL to POST to |
| `events` | array | Yes | Event types to deliver |
| `path_prefix` | string | No | Filter by path prefix (entry/index events only) |
| `secret` | string | Yes | HMAC-SHA256 shared secret for signature verification |
| `active` | bool | No | Default true. Set false to disable without deleting. |

### Delivery

For each matching event:

1. Serialize the event as JSON
2. Compute HMAC-SHA256 signature: `HMAC(secret, json_body)`
3. POST to the URL with headers:
   ```
   Content-Type: application/json
   X-AeorDB-Signature: sha256=<hex_encoded_hmac>
   X-AeorDB-Event: entries_created
   X-AeorDB-Delivery: <event_id>
   ```
4. Fire-and-forget — do not wait for response, do not retry

### Execution

Webhook delivery runs on a dedicated `tokio::spawn` task per delivery. Does not block the event bus or the emitting operation. Failed deliveries (connection error, timeout, non-2xx response) are logged via `tracing::warn` but not retried.

### Webhook management

Webhooks are managed by editing `/.config/webhooks.json` via the normal file API:
```
PUT /engine/.config/webhooks.json
GET /engine/.config/webhooks.json
```

Protected by crudlify permissions on `/.config/`. Only users with Configure permission on root can manage webhooks.

### Webhook listener

A `WebhookDispatcher` subscribes to the EventBus and dispatches matching events to registered webhook URLs. It loads the webhook config on startup and reloads it when `/.config/webhooks.json` changes (detected via an `EntriesUpdated` event on that path).

---

## 7. Emission Points

Each layer emits events relevant to its domain:

| Layer | Events | user_id source |
|-------|--------|---------------|
| `DirectoryOps` | EntriesCreated, EntriesUpdated, EntriesDeleted | Passed from caller |
| `VersionManager` | VersionsCreated, VersionsDeleted, VersionsPromoted, VersionsRestored | Passed from caller |
| `SystemTables` / admin routes | UsersCreated, UsersActivated, UsersDeactivated | From JWT claims |
| `IndexingPipeline` | IndexesUpdated | "system" |
| `backup.rs` | ImportsCompleted | From CLI or JWT claims |
| Auth routes | TokensExchanged, ApiKeysCreated, ApiKeysRevoked | From JWT claims or "system" |
| Plugin routes | PluginsDeployed, PluginsRemoved | From JWT claims |
| Permission routes | PermissionsChanged | From JWT claims |
| Error handlers | Errors | "system" |
| Heartbeat task | Heartbeat | "system" |

### Threading user_id via RequestContext

HTTP handlers create `RequestContext::from_claims(&claims.sub, event_bus)` and pass it to every engine call. For `--auth=false` mode, the user_id is the root nil UUID. For engine-internal operations (indexing, heartbeat, startup), `RequestContext::system()` is used.

The `ctx` parameter is the first parameter after `&self` in all engine methods. This is a codebase-wide change but done once — every future feature that needs request context just reads from `ctx`.

---

## 8. RequestContext — Threading Context Through the Engine

### The Problem

Events need `user_id`. Tracing needs `trace_id`. Metrics need request labels. All of these require context to flow from the HTTP handler (or CLI command) down through every engine operation. Rather than add these one at a time, we build a single `RequestContext` that carries everything.

### RequestContext

```rust
pub struct RequestContext {
    pub user_id: String,                        // UUID or "system"
    pub event_bus: Option<Arc<EventBus>>,        // None = no events (tests, CLI)
    pub event_scope: RefCell<EventScope>,         // collects events for batching
    // Future: trace_id, request_id, metric tags
}

impl RequestContext {
    /// Default context for engine-internal operations and tests.
    pub fn system() -> Self;

    /// Context with event bus but system user (CLI tools, background tasks).
    pub fn with_bus(bus: Arc<EventBus>) -> Self;

    /// Full context from HTTP request claims.
    pub fn from_claims(user_id: &str, bus: Arc<EventBus>) -> Self;

    /// Emit a collected event (goes through scope for batching).
    pub fn emit(&self, event_type: &str, payload: EventPayload);

    /// Flush the scope — emits all collected events as batched events.
    pub fn flush(&self);
}
```

### Explicit parameter passing

Every engine method that might emit events takes `ctx: &RequestContext`:

```rust
// DirectoryOps
pub fn store_file(&self, ctx: &RequestContext, path: &str, data: &[u8], ...) -> EngineResult<FileRecord>;
pub fn delete_file(&self, ctx: &RequestContext, path: &str) -> EngineResult<()>;

// VersionManager
pub fn create_snapshot(&self, ctx: &RequestContext, name: &str, ...) -> EngineResult<SnapshotInfo>;

// IndexingPipeline
pub fn run(&self, ctx: &RequestContext, path: &str, data: &[u8], ...) -> EngineResult<()>;
```

### Migration strategy

1. Add `ctx: &RequestContext` parameter to all engine methods that emit events
2. Tests pass `RequestContext::system()` — no events, no bus, no overhead
3. HTTP handlers create context from JWT claims: `RequestContext::from_claims(&claims.sub, event_bus.clone())`
4. CLI commands create context with bus: `RequestContext::with_bus(bus)` or `RequestContext::system()`
5. `RequestContext::flush()` called at the end of each HTTP request (or on Drop)

### AppState

Add `event_bus: Arc<EventBus>` to `AppState`. HTTP handlers create `RequestContext` from the bus + JWT claims, pass it to engine operations.

```rust
pub struct AppState {
    // ... existing fields ...
    pub event_bus: Arc<EventBus>,  // NEW
}
```

---

## 9. Heartbeat Scheduling

The heartbeat targets wall-clock-aligned 15-second intervals:

```rust
async fn heartbeat_task(bus: Arc<EventBus>, engine: Arc<StorageEngine>) {
    // Calculate delay to next 15-second boundary
    let now = chrono::Utc::now();
    let seconds = now.second();
    let next_boundary = ((seconds / 15) + 1) * 15;
    let delay_seconds = next_boundary - seconds;
    let delay_ms = (delay_seconds as u32 * 1000) - now.timestamp_subsec_millis();

    tokio::time::sleep(Duration::from_millis(delay_ms as u64)).await;

    let mut interval = tokio::time::interval(Duration::from_secs(15));
    loop {
        interval.tick().await;
        let stats = engine.stats();
        bus.emit(EngineEvent::heartbeat(stats));
    }
}
```

Started as a `tokio::spawn` task during server startup.

---

## 10. WASM Plugin Event Access

Plugins can emit custom events via a host function:

```
emit_event(event_type_ptr, event_type_len, payload_ptr, payload_len)
```

The host function wraps the payload as an `EngineEvent` and sends it to the EventBus. The `event_type` is prefixed with `plugin:` to distinguish from system events (e.g., `plugin:my_custom_event`).

Plugins cannot subscribe to events — they are emitters only. Subscription is a server-side concern handled by SSE and webhooks.

---

## 11. Edge Cases

### Bus capacity overflow
When the broadcast channel is full, new events are still sent (broadcast replaces oldest). Slow subscribers get `Lagged(n)` on next recv. This is expected — events are best-effort, not guaranteed delivery.

### Webhook target down
Fire-and-forget. Log the failure, move on. No retry queue in v1. Future: configurable retry with exponential backoff.

### Webhook config changes
The `WebhookDispatcher` watches for `EntriesUpdated` events on `/.config/webhooks.json` and reloads the config. This creates a self-referential event (config change → event → dispatcher reload), which is fine — the dispatcher filters it out after reloading.

### SSE client disconnect
When the client closes the connection, the SSE handler's stream ends naturally. The broadcast receiver is dropped, freeing the subscription. No cleanup needed.

### No subscribers
If nobody is subscribed (no SSE clients, no webhooks, no in-process consumers), events are emitted to the broadcast channel and immediately dropped. Zero overhead beyond the event construction.

### Events during import
A bulk import opens an EventScope. All entries created during the import are batched into one `EntriesCreated` event on scope close. The `ImportsCompleted` event is emitted separately after the import finishes.

### Heartbeat and no engine access
The heartbeat task holds an `Arc<StorageEngine>` reference. If the engine is being shut down, the heartbeat task should be cancelled via a `tokio::CancellationToken` or by dropping the task handle.

---

## 12. Implementation Phases

### Phase 1 — RequestContext + EventBus + EngineEvent
- `RequestContext` struct with `user_id`, `event_bus`, `event_scope`
- `RequestContext::system()`, `::with_bus()`, `::from_claims()`
- `EngineEvent` enum with all 19 event types
- `EventBus` with `emit`, `subscribe`
- `EventScope` for batching
- `EntryEventData` and other payload structs
- Tests: emit/receive, batching, lagging subscriber, event serialization, RequestContext construction

### Phase 2 — Context Threading + Emission Points
- Add `ctx: &RequestContext` parameter to all engine methods:
  DirectoryOps (store_file, delete_file, store_file_with_indexing, store_file_with_full_pipeline)
  VersionManager (create_snapshot, delete_snapshot, create_fork, abandon_fork, restore_snapshot, promote)
  IndexingPipeline (run)
  SystemTables (create_user, deactivate_user, activate_user)
- Update ALL callers (HTTP handlers, CLI commands, tests)
- Tests pass `RequestContext::system()` — no behavior change, just signature
- HTTP handlers create context from JWT claims
- Emit entry/version events from engine methods
- Tests: store file → receive event, delete → receive event, snapshot → receive event

### Phase 3 — Heartbeat
- Clock-aligned 15-second interval task
- Emits DatabaseStats as Heartbeat event
- Tests: heartbeat received, stats populated, timing alignment

### Phase 4 — SSE Endpoint
- `GET /events/stream` with `text/event-stream` response
- Query param filtering (events, path_prefix)
- Auth required
- Tests: SSE connection receives events, filtering works, auth required

### Phase 5 — Webhooks
- Webhook config parsing from `/.config/webhooks.json`
- `WebhookDispatcher`: subscribes to bus, POSTs matching events
- HMAC-SHA256 signature
- Config reload on change
- Tests: webhook delivery, signature verification, filtering, inactive webhook skipped

### Phase 6 — Remaining Emission Points + Portal Update
- SystemTables: user events
- Auth routes: token/key events
- Plugin routes: deploy/remove events
- IndexingPipeline: index events
- Backup: import events
- Error events
- Update portal dashboard to use SSE instead of polling
- Tests: all remaining event types, portal SSE integration

---

## 13. Non-Goals (Deferred)

- Event persistence / replay (events are ephemeral — subscribe or miss them)
- Webhook retry queue with exponential backoff
- Event-triggered WASM plugins (database triggers)
- Cross-node event forwarding (Raft-based replication events)
- Event filtering by content (e.g., "only files > 1MB")
- WebSocket protocol (SSE covers the use case more simply)
- Guaranteed delivery / at-least-once semantics
