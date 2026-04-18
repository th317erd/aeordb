# AeorDB Consistency Audit — Design Spec

**Date:** 2026-04-18
**Status:** Approved
**Scope:** Public API surface, internal naming, config structure, error handling

---

## 1. Route Restructuring

Replace the current 10+ route prefixes with 6 top-level namespaces.

### New namespaces

| Namespace | Purpose |
|-----------|---------|
| `/files/` | File database — CRUD, rename, directories, query |
| `/links/` | Symlinks — create, read metadata, delete (without auto-resolving) |
| `/blobs/` | Content-addressed — chunks, hash lookups, upload protocol |
| `/versions/` | Snapshots, forks, history, restore, promote, diff/patch |
| `/sync/` | Replication — diffs, chunks, cluster peers |
| `/auth/` | Tokens, magic links, refresh, API keys |
| `/plugins/` | Plugin invocation (storage uses `/files/plugins/`) |
| `/system/` | Users, groups, tasks, cron, backup, health, metrics, events, GC |

### Route migration table

#### `/files/`

| Old route | New route | Method | Notes |
|-----------|-----------|--------|-------|
| `PUT /engine/{*path}` | `PUT /files/{*path}` | PUT | Store file |
| `GET /engine/{*path}` | `GET /files/{*path}` | GET | Read file, list dir, resolve symlink |
| `DELETE /engine/{*path}` | `DELETE /files/{*path}` | DELETE | Delete file or symlink |
| `HEAD /engine/{*path}` | `HEAD /files/{*path}` | HEAD | Metadata headers |
| `POST /engine-rename/{*path}` | `PATCH /files/{*path}` | PATCH | Rename/move (body: `{"to": "..."}`) — PATCH = metadata update |
| `POST /query` | `POST /files/query` | POST | Query engine |

**Symlink routes** — symlinks need their own prefix because `PUT /files/{*path}` is already "store file content" and axum wildcards can't have suffixes:

| Old route | New route | Method | Notes |
|-----------|-----------|--------|-------|
| `POST /engine-symlink/{*path}` | `PUT /links/{*path}` | PUT | Create symlink (body: `{"target": "..."}`) |
| (part of engine_get) | `GET /links/{*path}` | GET | Get symlink metadata (nofollow) |
| (part of engine_delete) | `DELETE /links/{*path}` | DELETE | Delete symlink |

Note: `GET /files/{*path}` still auto-resolves symlinks (follows them). `GET /links/{*path}` returns the symlink record itself without following. This replaces the `?nofollow` query parameter.

**Plugin routes** — plugins are files stored under `/files/plugins/`, but invocation needs a separate route because `POST /files/{*path}` can't have an `/invoke` suffix with wildcards:

| Old route | New route | Method | Notes |
|-----------|-----------|--------|-------|
| `PUT /{db}/{schema}/{table}/_deploy` | `PUT /files/plugins/{name}` | PUT | Deploy plugin (just a file) |
| `GET /{db}/_plugins` | `GET /files/plugins/` | GET | List plugins (just a directory listing) |
| `DELETE /{db}/{schema}/{table}/{fn}/_remove` | `DELETE /files/plugins/{name}` | DELETE | Remove plugin (just a file) |
| `POST /{db}/{schema}/{table}/{fn}/_invoke` | `POST /plugins/{name}/invoke` | POST | Invoke plugin — separate namespace |

#### `/blobs/`

| Old route | New route | Method |
|-----------|-----------|--------|
| `GET /engine/_hash/{hex}` | `GET /blobs/{hash}` | GET |
| `PUT /upload/chunks/{hash}` | `PUT /blobs/chunks/{hash}` | PUT |
| `POST /upload/check` | `POST /blobs/check` | POST |
| `POST /upload/commit` | `POST /blobs/commit` | POST |
| `GET /upload/config` | `GET /blobs/config` | GET |

#### `/versions/`

| Old route | New route | Method |
|-----------|-----------|--------|
| `POST /version/snapshot` | `POST /versions/snapshots` | POST |
| `GET /version/snapshots` | `GET /versions/snapshots` | GET |
| `POST /version/restore` | `POST /versions/restore` | POST |
| `DELETE /version/snapshot/{name}` | `DELETE /versions/snapshots/{name}` | DELETE |
| `POST /version/fork` | `POST /versions/forks` | POST |
| `GET /version/forks` | `GET /versions/forks` | GET |
| `POST /version/fork/{name}/promote` | `POST /versions/forks/{name}/promote` | POST |
| `DELETE /version/fork/{name}` | `DELETE /versions/forks/{name}` | DELETE |
| `GET /version/file-history/{*path}` | `GET /versions/history/{*path}` | GET |
| `POST /version/file-restore/{*path}` | `POST /versions/restore/{*path}` | POST |
| `POST /admin/promote` | `POST /versions/promote` | POST |
| `POST /admin/export` | `POST /versions/export` | POST |
| `POST /admin/diff` | `POST /versions/diff` | POST |
| `POST /admin/import` | `POST /versions/import` | POST |

#### `/sync/`

| Old route | New route | Method |
|-----------|-----------|--------|
| `POST /sync/diff` | `POST /sync/diff` | POST |
| `POST /sync/chunks` | `POST /sync/chunks` | POST |
| `GET /admin/cluster` | `GET /sync/status` | GET |
| `POST /admin/cluster/peers` | `POST /sync/peers` | POST |
| `GET /admin/cluster/peers` | `GET /sync/peers` | GET |
| `DELETE /admin/cluster/peers/{node_id}` | `DELETE /sync/peers/{node_id}` | DELETE |
| `POST /admin/cluster/sync` | `POST /sync/trigger` | POST |
| `GET /admin/conflicts` | `GET /sync/conflicts` | GET |
| `GET /admin/conflicts/{*path}` | `GET /sync/conflicts/{*path}` | GET |
| `POST /admin/conflict-resolve/{*path}` | `POST /sync/conflicts/{*path}/resolve` | POST |
| `POST /admin/conflict-dismiss/{*path}` | `POST /sync/conflicts/{*path}/dismiss` | POST |

#### `/auth/`

| Old route | New route | Method |
|-----------|-----------|--------|
| `POST /auth/token` | `POST /auth/token` | POST |
| `POST /auth/magic-link` | `POST /auth/magic-link` | POST |
| `GET /auth/magic-link/verify` | `GET /auth/magic-link/verify` | GET |
| `POST /auth/refresh` | `POST /auth/refresh` | POST |
| `POST /api-keys` | `POST /auth/keys` | POST |
| `GET /api-keys` | `GET /auth/keys` | GET |
| `DELETE /api-keys/{key_id}` | `DELETE /auth/keys/{key_id}` | DELETE |
| `POST /admin/api-keys` | `POST /auth/keys/admin` | POST |
| `GET /admin/api-keys` | `GET /auth/keys/admin` | GET |
| `DELETE /admin/api-keys/{key_id}` | `DELETE /auth/keys/admin/{key_id}` | DELETE |

#### `/system/`

| Old route | New route | Method |
|-----------|-----------|--------|
| `GET /admin/health` | `GET /system/health` | GET |
| `GET /admin/metrics` | `GET /system/metrics` | GET |
| `GET /api/stats` | `GET /system/stats` | GET |
| `POST /admin/users` | `POST /system/users` | POST |
| `GET /admin/users` | `GET /system/users` | GET |
| `GET /admin/users/{id}` | `GET /system/users/{id}` | GET |
| `PATCH /admin/users/{id}` | `PATCH /system/users/{id}` | PATCH |
| `DELETE /admin/users/{id}` | `DELETE /system/users/{id}` | DELETE |
| `POST /admin/groups` | `POST /system/groups` | POST |
| `GET /admin/groups` | `GET /system/groups` | GET |
| `GET /admin/groups/{name}` | `GET /system/groups/{name}` | GET |
| `PATCH /admin/groups/{name}` | `PATCH /system/groups/{name}` | PATCH |
| `DELETE /admin/groups/{name}` | `DELETE /system/groups/{name}` | DELETE |
| `POST /admin/gc` | `POST /system/gc` | POST |
| `GET /admin/tasks` | `GET /system/tasks` | GET |
| `POST /admin/tasks/reindex` | `POST /system/tasks/reindex` | POST |
| `POST /admin/tasks/gc` | `POST /system/tasks/gc` | POST |
| `POST /admin/tasks/cleanup` | `POST /system/tasks/cleanup` | POST |
| `GET /admin/tasks/{id}` | `GET /system/tasks/{id}` | GET |
| `DELETE /admin/tasks/{id}` | `DELETE /system/tasks/{id}` | DELETE |
| `GET /admin/cron` | `GET /system/cron` | GET |
| `POST /admin/cron` | `POST /system/cron` | POST |
| `DELETE /admin/cron/{id}` | `DELETE /system/cron/{id}` | DELETE |
| `PATCH /admin/cron/{id}` | `PATCH /system/cron/{id}` | PATCH |
| `GET /events/stream` | `GET /system/events` | GET |
| `GET /portal` | `GET /system/portal` | GET |
| `GET /portal/{filename}` | `GET /system/portal/{filename}` | GET |

---

## 2. HTTP Response Headers

Standardize all custom headers with `X-AeorDB-` vendor prefix.

| Header | Purpose |
|--------|---------|
| `X-AeorDB-Path` | Normalized file path |
| `X-AeorDB-Size` | Total size in bytes |
| `X-AeorDB-Created` | Creation timestamp (milliseconds) |
| `X-AeorDB-Updated` | Update timestamp (milliseconds) |
| `X-AeorDB-Type` | Entry type: `file`, `directory`, `symlink` |
| `X-AeorDB-Hash` | Content hash (opaque string) |
| `X-AeorDB-Link-Target` | Symlink target path |

Replaces current inconsistent mix of `X-Path`, `x-hash`, `X-Total-Size`, etc.

---

## 3. Config / CLI Unification

Every config key has a CLI flag equivalent. CLI always overrides config.

### Config file format (`aeordb.toml`)

```toml
[server]
port = 3000
host = "0.0.0.0"
log_format = "pretty"          # "pretty", "json", "compact"

[server.tls]
cert = "/path/to/cert.pem"
key = "/path/to/key.pem"

[server.cors]
origins = ["https://app.example.com"]

[auth]
mode = "self"                  # "disabled", "self", "file:///path"
jwt_expiry_seconds = 3600

[storage]
database = "data.aeordb"
chunk_size = 262144
hot_dir = "./hot"
```

### 1:1 mapping

| Config key | CLI flag | Type | Default |
|------------|----------|------|---------|
| `server.port` | `--port, -p` | u16 | 3000 |
| `server.host` | `--host` | String | "0.0.0.0" |
| `server.log_format` | `--log-format` | String | "pretty" |
| `server.tls.cert` | `--tls-cert` | Path | (none) |
| `server.tls.key` | `--tls-key` | Path | (none) |
| `server.cors.origins` | `--cors-origins` | String* | (none) |
| `auth.mode` | `--auth` | String | "self" |
| `auth.jwt_expiry_seconds` | `--jwt-expiry` | i64 | 3600 |
| `storage.database` | `--database, -D` | Path | "data.aeordb" |
| `storage.chunk_size` | `--chunk-size` | usize | 262144 |
| `storage.hot_dir` | `--hot-dir` | Path | (none) |

*CLI `--cors-origins` accepts comma-separated string; config uses TOML array. Loader handles conversion.

### Changes from current

- `auth.enabled` (bool) replaced by `auth.mode` (string) to match CLI's richer model
- `--cors` renamed to `--cors-origins`
- `--host` added as new CLI flag (was config-only)
- `--jwt-expiry` and `--chunk-size` added as new CLI flags (were config-only or hardcoded)
- Config durations stay in human-friendly units (seconds); API responses always use milliseconds

---

## 4. Internal Storage Paths (`/.system/`)

Convention: plural nouns, kebab-case for compound words.

| Current | Proposed | Change? |
|---------|----------|---------|
| `/.system/config/{key}` | `/.system/config/{key}` | no |
| `/.system/apikeys/{key_id}` | `/.system/api-keys/{key_id}` | **yes** |
| `/.system/users/{user_id}` | `/.system/users/{user_id}` | no |
| `/.system/groups/{name}` | `/.system/groups/{name}` | no |
| `/.system/permissions/{hash}` | `/.system/permissions/{hash}` | no |
| `/.system/magic-links/{hash}` | `/.system/magic-links/{hash}` | no |
| `/.system/refresh-tokens/{hash}` | `/.system/refresh-tokens/{hash}` | no |
| `/.system/plugins/{key}` | `/.system/plugins/{key}` | no |
| `/.system/cluster/sync/{id}` | `/.system/sync-peers/{id}` | **yes** |

Only 2 renames required.

**Migration:** On startup, check if old paths exist and move them to new paths. Log the migration.

---

## 5. Event Name Consistency

Convention: `plural_noun_past_verb` (e.g. `entries_created`).

Exceptions: `errors`, `heartbeat`, `gc_started`, `gc_completed` — these are naturally singular/bare and forcing them into the pattern makes them awkward.

### Changes

| Current | Proposed | Reason |
|---------|----------|--------|
| `task_created` | `tasks_created` | pluralize |
| `task_started` | `tasks_started` | pluralize |
| `task_completed` | `tasks_completed` | pluralize |
| `task_failed` | `tasks_failed` | pluralize |
| `task_cancelled` | `tasks_cancelled` | pluralize |
| `sync_succeeded` | `syncs_completed` | pluralize + consistent verb |
| `sync_failed` | `syncs_failed` | pluralize |
| (missing) | `gc_started` | new — matches `gc_completed` |
| `errors` | `errors` | keep (bare noun exception) |
| `heartbeat` | `heartbeat` | keep (bare noun exception) |

All other events already follow the convention.

---

## 6. JSON Response Conventions

### Field naming rules

1. **`size`** not `total_size` — no "partial size" concept exists
2. **`entry_type`** not `type` — `type` is a reserved word in most client languages
3. **All timestamps in API responses are milliseconds** (i64). Config files use human-friendly units (seconds for durations).
4. **Hashes are opaque strings** — the engine does not dictate encoding. The user configured the hash algorithm, they know the format.
5. **All list endpoints return wrapped objects**: `{"items": [...]}` not bare arrays. This allows adding pagination metadata later (`total`, `offset`, `limit`).

### Changes required

| Location | Current field | New field |
|----------|--------------|-----------|
| `EngineFileResponse` | `total_size` | `size` |
| `SyncFileEntry` | `size` | `size` (already correct) |
| Rename response | `type` | `entry_type` |
| Any bare array response | `[...]` | `{"items": [...]}` |
| Any timestamp in seconds | seconds | milliseconds |

### Hashes

Hashes returned in API responses are opaque strings. The hash algorithm is configurable, so the engine treats hashes as unique identifiers without assuming a specific encoding. Clients that configured the hash algorithm know what format to expect.

---

## 7. Plugin Routes

Plugins are files. The current `/{database}/{schema}/{table}/` hierarchy is eliminated.

| Operation | Route | How it works |
|-----------|-------|-------------|
| Deploy | `PUT /files/plugins/{name}` | Store file (just a regular PUT) |
| List | `GET /files/plugins/` | List directory (just a regular listing) |
| Delete | `DELETE /files/plugins/{name}` | Delete file (just a regular DELETE) |
| Invoke | `POST /plugins/{name}/invoke` | Execute plugin — separate namespace due to wildcard routing constraint |

Storage, listing, and deletion use standard `/files/` operations. Invocation uses `/plugins/` because axum wildcards can't have suffixes (`/files/{*path}/invoke` won't route correctly). Query params and request body are available for plugin arguments.

---

## 8. Error Codes

12 machine-readable error codes, each mapping to a distinct client-handling scenario.

| Code | HTTP Status | When |
|------|-------------|------|
| `NOT_FOUND` | 404 | Resource doesn't exist |
| `ALREADY_EXISTS` | 409 | Path/resource already taken |
| `CONFLICT` | 409 | Sync/merge conflict |
| `INVALID_INPUT` | 400 | Bad params (message must be specific and actionable) |
| `INVALID_PATH` | 400 | Cyclic symlink, depth exceeded |
| `AUTH_REQUIRED` | 401 | Missing/invalid/expired credentials |
| `FORBIDDEN` | 403 | Insufficient permissions, /.system/ boundary crossing |
| `RATE_LIMITED` | 429 | Rate limit exceeded |
| `PAYLOAD_TOO_LARGE` | 413 | Upload exceeds size limit |
| `METHOD_NOT_ALLOWED` | 405 | Wrong HTTP method |
| `SERVICE_UNAVAILABLE` | 503 | Shutting down, degraded health |
| `INTERNAL_ERROR` | 500 | Everything else |

`SYSTEM_BOUNDARY` dropped — folded into `FORBIDDEN`.

### Error message requirements

Every error response MUST include a specific, actionable `error` message. Prohibited patterns:

- "invalid input" — say what's invalid and why
- "bad request" — say what's wrong with the request
- "operation failed" — say which operation and why
- "internal error" — log the details server-side, give the client a request ID or timestamp to reference

Good examples:
- `"Path '/docs/readme.md' already exists. Use DELETE first or rename with PATCH /files/{path}"`
- `"Snapshot 'v2.1' not found. Use GET /versions/snapshots to list available snapshots"`
- `"Request body must be valid JSON. Received 'text/plain' content type"`
- `"File exceeds 100 MB inline upload limit. Use the chunked upload protocol: PUT /blobs/chunks/{hash}"`

### Error message audit

As part of implementation, audit every `ErrorResponse::new(...)` call across the codebase. Flag and rewrite any message that:
1. Doesn't tell the user what went wrong
2. Doesn't tell the user how to fix it (where applicable)
3. Leaks internal details (stack traces, file paths, raw error objects)
4. Is duplicated verbatim across different error scenarios

---

## 9. Summary of All Changes

| Area | Changes needed |
|------|---------------|
| Routes | ~70 route paths change across 8 namespaces (`/links/` and `/plugins/` added for routing constraints) |
| Headers | 7 custom headers get `X-AeorDB-` prefix |
| Config | `auth.enabled` → `auth.mode`, add missing CLI flags, rename `--cors` |
| `/.system/` | 2 path renames + startup migration |
| Events | 8 event renames + 1 new event |
| JSON | `total_size` → `size`, `type` → `entry_type`, wrap arrays, ms timestamps |
| Plugins | 4 special routes eliminated, use `/files/` + `/invoke` |
| Errors | 2 new codes, 1 dropped, message audit |
| Client report | Full migration guide for client team |
| Documentation | All 29 docs/src/ files updated to new routes/headers/fields |
| Marketing site | aeordb-www examples updated to new routes |
