# AeorDB API Migration Guide

This guide covers every breaking change in the API consistency audit. Use it to update your client code, SDKs, and integrations.

## 1. Route Migration

### Files & Directories

| Old Route | New Route | Method | Notes |
|-----------|-----------|--------|-------|
| `PUT /engine/{path}` | `PUT /files/{path}` | PUT | Store a file |
| `GET /engine/{path}` | `GET /files/{path}` | GET | Read file or list directory |
| `DELETE /engine/{path}` | `DELETE /files/{path}` | DELETE | Delete a file |
| `HEAD /engine/{path}` | `HEAD /files/{path}` | HEAD | Check existence / metadata |
| `GET /engine/_hash/{hex}` | `GET /files/_hash/{hex}` | GET | Fetch by content hash |

### Symlinks

| Old Route | New Route | Method Change | Notes |
|-----------|-----------|---------------|-------|
| `POST /engine-symlink/{path}` | `PUT /links/{path}` | POST -> PUT | Create/update symlink |

### Rename

| Old Route | New Route | Method Change | Notes |
|-----------|-----------|---------------|-------|
| `POST /engine-rename/` | `PATCH /files/` | POST -> PATCH | Rename a file or directory |

### Query

| Old Route | New Route | Method | Notes |
|-----------|-----------|--------|-------|
| `POST /query` | `POST /files/query` | POST | Execute a query |

### Upload Protocol

| Old Route | New Route | Method | Notes |
|-----------|-----------|--------|-------|
| `GET /upload/config` | `GET /blobs/config` | GET | Negotiate hash algorithm |
| `POST /upload/check` | `POST /blobs/check` | POST | Dedup check |
| `PUT /upload/chunks/{hash}` | `PUT /blobs/chunks/{hash}` | PUT | Upload a chunk |
| `POST /upload/commit` | `POST /blobs/commit` | POST | Atomic commit |

### Versioning

| Old Route | New Route | Method | Notes |
|-----------|-----------|--------|-------|
| `POST /version/snapshot` | `POST /versions/snapshots` | POST | Create snapshot |
| `GET /version/snapshots` | `GET /versions/snapshots` | GET | List snapshots |
| `POST /version/restore` | `POST /versions/restore` | POST | Restore snapshot |
| `DELETE /version/snapshot/{name}` | `DELETE /versions/snapshots/{name}` | DELETE | Delete snapshot |
| `POST /version/fork` | `POST /versions/forks` | POST | Create fork |
| `GET /version/forks` | `GET /versions/forks` | GET | List forks |
| `POST /version/fork/{name}/promote` | `POST /versions/forks/{name}/promote` | POST | Promote fork |
| `DELETE /version/fork/{name}` | `DELETE /versions/forks/{name}` | DELETE | Abandon fork |
| `GET /version/file-history/{path}` | `GET /versions/history/{path}` | GET | File change history |
| `POST /version/file-restore/{path}` | `POST /versions/restore/{path}` | POST | Restore file from version |

### Authentication

| Old Route | New Route | Method | Notes |
|-----------|-----------|--------|-------|
| `POST /api-keys` | `POST /auth/keys` | POST | Create API key (self-service) |
| `GET /api-keys` | `GET /auth/keys` | GET | List your API keys |
| `DELETE /api-keys/{key_id}` | `DELETE /auth/keys/{key_id}` | DELETE | Revoke your API key |
| `POST /admin/api-keys` | `POST /auth/keys/admin` | POST | Create API key (admin) |
| `GET /admin/api-keys` | `GET /auth/keys/admin` | GET | List all API keys (admin) |
| `DELETE /admin/api-keys/{key_id}` | `DELETE /auth/keys/admin/{key_id}` | DELETE | Revoke any API key (admin) |

### Admin -> System

| Old Route | New Route | Method | Notes |
|-----------|-----------|--------|-------|
| `POST /admin/gc` | `POST /system/gc` | POST | Synchronous GC |
| `POST /admin/tasks/reindex` | `POST /system/tasks/reindex` | POST | Trigger reindex |
| `POST /admin/tasks/gc` | `POST /system/tasks/gc` | POST | Background GC |
| `GET /admin/tasks` | `GET /system/tasks` | GET | List tasks |
| `GET /admin/tasks/{id}` | `GET /system/tasks/{id}` | GET | Get task |
| `DELETE /admin/tasks/{id}` | `DELETE /system/tasks/{id}` | DELETE | Cancel task |
| `GET /admin/cron` | `GET /system/cron` | GET | List cron schedules |
| `POST /admin/cron` | `POST /system/cron` | POST | Create cron schedule |
| `PATCH /admin/cron/{id}` | `PATCH /system/cron/{id}` | PATCH | Update cron schedule |
| `DELETE /admin/cron/{id}` | `DELETE /system/cron/{id}` | DELETE | Delete cron schedule |
| `GET /admin/metrics` | `GET /system/metrics` | GET | Prometheus metrics |
| `GET /admin/health` | `GET /system/health` | GET | Health check |
| `POST /admin/users` | `POST /system/users` | POST | Create user |
| `GET /admin/users` | `GET /system/users` | GET | List users |
| `GET /admin/users/{id}` | `GET /system/users/{id}` | GET | Get user |
| `PATCH /admin/users/{id}` | `PATCH /system/users/{id}` | PATCH | Update user |
| `DELETE /admin/users/{id}` | `DELETE /system/users/{id}` | DELETE | Deactivate user |
| `POST /admin/groups` | `POST /system/groups` | POST | Create group |
| `GET /admin/groups` | `GET /system/groups` | GET | List groups |
| `GET /admin/groups/{name}` | `GET /system/groups/{name}` | GET | Get group |
| `PATCH /admin/groups/{name}` | `PATCH /system/groups/{name}` | PATCH | Update group |
| `DELETE /admin/groups/{name}` | `DELETE /system/groups/{name}` | DELETE | Delete group |

### Admin -> Versions (Backup/Restore)

| Old Route | New Route | Method | Notes |
|-----------|-----------|--------|-------|
| `POST /admin/export` | `POST /versions/export` | POST | Export database |
| `POST /admin/diff` | `POST /versions/diff` | POST | Create patch |
| `POST /admin/import` | `POST /versions/import` | POST | Import backup |
| `POST /admin/promote` | `POST /versions/promote` | POST | Promote version hash |

### Cluster -> Sync

| Old Route | New Route | Method | Notes |
|-----------|-----------|--------|-------|
| `GET /admin/cluster` | `GET /sync/` | GET | Cluster status |
| `POST /admin/cluster/peers` | `POST /sync/peers` | POST | Add peer |
| `POST /admin/cluster/sync` | `POST /sync/trigger` | POST | Trigger sync |
| `GET /admin/conflicts` | `GET /sync/conflicts` | GET | List conflicts |
| `POST /admin/conflict-resolve/{path}` | `POST /sync/conflicts/{path}/resolve` | POST | Resolve conflict |
| `POST /admin/conflict-dismiss/{path}` | `POST /sync/conflicts/{path}/dismiss` | POST | Dismiss conflict |

### Events

| Old Route | New Route | Method | Notes |
|-----------|-----------|--------|-------|
| `GET /events/stream` | `GET /system/events` | GET | SSE event stream |

### Plugins

| Old Route | New Route | Method | Notes |
|-----------|-----------|--------|-------|
| `PUT /{db}/{schema}/{table}/_deploy` | `PUT /files/plugins/{db}/{schema}/{table}/_deploy` | PUT | Deploy plugin |
| `POST /{db}/{schema}/{table}/{fn}/_invoke` | `POST /plugins/{db}/{schema}/{table}/{fn}/_invoke` | POST | Invoke plugin |
| `GET /{db}/_plugins` | `GET /plugins/{db}` | GET | List plugins |
| `DELETE /{db}/{schema}/{table}/{fn}/_remove` | `DELETE /plugins/{db}/{schema}/{table}/{fn}/_remove` | DELETE | Remove plugin |

---

## 2. Header Renames

All custom headers now use the `X-AeorDB-` prefix for clarity and to avoid collisions with other systems.

| Old Header | New Header |
|------------|------------|
| `X-Path` | `X-AeorDB-Path` |
| `X-Total-Size` | `X-AeorDB-Size` |
| `X-Created-At` | `X-AeorDB-Created-At` |
| `X-Updated-At` | `X-AeorDB-Updated-At` |
| `X-Entry-Type` | `X-AeorDB-Entry-Type` |
| `X-Symlink-Target` | `X-AeorDB-Symlink-Target` |
| `X-Request-ID` | `X-AeorDB-Request-ID` |

---

## 3. JSON Field Changes

These fields were renamed in JSON response bodies for consistency:

| Old Field | New Field | Where It Appears |
|-----------|-----------|------------------|
| `total_size` | `size` | File metadata, directory listings, query results |
| `type` | `entry_type` | Symlink delete response (`"type": "symlink"` -> `"entry_type": "symlink"`) |
| `results` | `items` | Query responses (`POST /files/query`) — now matches all other collection endpoints |
| `total_count` | `total` | Query pagination — now matches directory listing pagination |

### Before

```json
{
  "path": "/data/report.pdf",
  "total_size": 245678,
  "created_at": 1775968398000
}
```

### After

```json
{
  "path": "/data/report.pdf",
  "size": 245678,
  "created_at": 1775968398000
}
```

---

## 4. Collection Response Wrapping

All endpoints that previously returned bare JSON arrays now return objects with an `items` key. Directory listings also include pagination metadata.

### Before

```json
[
  {"path": "/data/report.pdf", "name": "report.pdf", "entry_type": 2},
  {"path": "/data/images", "name": "images", "entry_type": 3}
]
```

### After (directory listing with pagination)

```json
{
  "items": [
    {"path": "/data/report.pdf", "name": "report.pdf", "entry_type": 2},
    {"path": "/data/images", "name": "images", "entry_type": 3}
  ],
  "total": 50,
  "limit": 10,
  "offset": 0
}
```

### After (query response)

```json
{
  "items": [...],
  "total": 50,
  "has_more": true,
  "next_cursor": "...",
  "prev_cursor": "..."
}
```

### Pagination support

Directory listings now support `?limit=N&offset=M` query parameters:

```
GET /files/my-dir?limit=10&offset=20
```

**Affected endpoints:**
- `GET /files/{path}` (directory listings)
- `GET /versions/snapshots` (snapshot list)
- `GET /versions/forks` (fork list)
- `GET /system/tasks` (task list)
- `GET /system/cron` (cron schedule list)
- `GET /system/users` (user list)
- `GET /system/groups` (group list)
- `GET /auth/keys` (API key list)
- `GET /auth/keys/admin` (admin API key list)
- `GET /sync/conflicts` (conflict list)
- `GET /plugins/{db}` (plugin list)

---

## 5. Event Name Changes

Event names were updated for consistency (plural resource names) and a new event was added.

| Old Event Name | New Event Name |
|----------------|----------------|
| `task.started` | `tasks_started` |
| `task.completed` | `tasks_completed` |
| `task.failed` | `tasks_failed` |
| `gc.completed` | `gc_completed` |
| `sync.started` | `syncs_started` |
| `sync.completed` | `syncs_completed` |
| `sync.failed` | `syncs_failed` |
| *(new)* | `gc_started` |

**Unchanged event names:** `entries_created`, `entries_deleted`, `versions_created`, `permissions_changed`, `indexes_changed`.

### Client-side update

```javascript
// Before
evtSource.addEventListener('task.completed', handler);

// After
evtSource.addEventListener('tasks_completed', handler);
```

---

## 6. Error Code Changes

### New error codes

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `PAYLOAD_TOO_LARGE` | 413 | Request body exceeds the endpoint's size limit |
| `METHOD_NOT_ALLOWED` | 405 | HTTP method not supported for this endpoint |
| `SERVICE_UNAVAILABLE` | 503 | Server is shutting down or overloaded |

### Removed error codes

| Code | Replacement |
|------|-------------|
| `SYSTEM_BOUNDARY` | Replaced by standard `FORBIDDEN` (403) |

---

## 7. Config Changes

### CLI flag rename

| Old Flag | New Flag | Notes |
|----------|----------|-------|
| `--cors` | `--cors-origins` | More descriptive name |

### Auth config

| Old Config | New Config | Notes |
|------------|------------|-------|
| `auth.enabled` | `auth.mode` | Now accepts `disabled`, `self-contained`, `file` |

### Before

```bash
aeordb start --cors "*"
```

### After

```bash
aeordb start --cors-origins "*"
```

---

## 8. Symlink Route Changes

| Old | New |
|-----|-----|
| `POST /engine-symlink/{path}` | `PUT /links/{path}` |

The method changed from `POST` to `PUT` because symlink creation is idempotent (creating a symlink to the same target twice produces the same result).

### Before

```bash
curl -X POST http://localhost:3000/engine-symlink/latest-logo \
  -H "Content-Type: application/json" \
  -d '{"target": "/assets/logo.psd"}'
```

### After

```bash
curl -X PUT http://localhost:3000/links/latest-logo \
  -H "Content-Type: application/json" \
  -d '{"target": "/assets/logo.psd"}'
```

---

## 9. Rename Route Changes

| Old | New |
|-----|-----|
| `POST /engine-rename/` | `PATCH /files/` |

The method changed from `POST` to `PATCH` because renaming modifies an existing resource rather than creating a new one.

### Before

```bash
curl -X POST http://localhost:3000/engine-rename/ \
  -H "Content-Type: application/json" \
  -d '{"from": "/old/path.txt", "to": "/new/path.txt"}'
```

### After

```bash
curl -X PATCH http://localhost:3000/files/ \
  -H "Content-Type: application/json" \
  -d '{"from": "/old/path.txt", "to": "/new/path.txt"}'
```

---

## Quick Migration Checklist

- [ ] Search-and-replace `/engine/` with `/files/` in all API calls
- [ ] Replace `/engine-symlink/` with `/links/` and change POST to PUT
- [ ] Replace `/engine-rename/` with `PATCH /files/` and change POST to PATCH
- [ ] Replace `/upload/` with `/blobs/` in upload protocol calls
- [ ] Replace `/query` endpoint with `/files/query`
- [ ] Replace `/version/snapshot` with `/versions/snapshots`
- [ ] Replace `/version/fork` with `/versions/forks`
- [ ] Replace `/version/file-history/` with `/versions/history/`
- [ ] Replace `/version/file-restore/` with `/versions/restore/`
- [ ] Replace `/api-keys` with `/auth/keys`
- [ ] Replace `/admin/api-keys` with `/auth/keys/admin`
- [ ] Replace `/admin/cluster` with `/sync/`
- [ ] Replace `/admin/conflicts` with `/sync/conflicts`
- [ ] Replace `/admin/export|import|diff|promote` with `/versions/export|import|diff|promote`
- [ ] Replace `/admin/gc|tasks|cron|health|metrics|users|groups` with `/system/...`
- [ ] Replace `/events/stream` with `/system/events`
- [ ] Update all `X-Path` headers to `X-AeorDB-Path` (and all other header renames)
- [ ] Replace `total_size` with `size` in JSON parsing
- [ ] Replace `"type"` with `"entry_type"` in symlink delete response parsing
- [ ] Replace `"results"` with `"items"` in query response parsing
- [ ] Replace `"total_count"` with `"total"` in query pagination parsing
- [ ] Wrap array response parsing to expect `{items: [...]}` objects
- [ ] Directory listings now include `total`, `limit`, `offset` metadata
- [ ] Update event listener names (`task.*` -> `tasks_*`, `sync.*` -> `syncs_*`)
- [ ] Replace `--cors` CLI flag with `--cors-origins`
- [ ] Handle new error codes: `PAYLOAD_TOO_LARGE`, `METHOD_NOT_ALLOWED`, `SERVICE_UNAVAILABLE`
- [ ] Remove handling for `SYSTEM_BOUNDARY` error code
