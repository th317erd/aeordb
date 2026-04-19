# System Operations

Administrative endpoints for garbage collection, background tasks, cron scheduling, metrics, health checks, backup/restore, and user/group management. Most system endpoints require **root** access.

## Endpoint Summary

### Garbage Collection

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/system/gc` | Run synchronous garbage collection | Yes |

### Background Tasks

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/system/tasks/reindex` | Trigger a reindex task | Yes |
| POST | `/system/tasks/gc` | Trigger a background GC task | Yes |
| GET | `/system/tasks` | List all tasks with progress | Yes |
| GET | `/system/tasks/{id}` | Get a single task | Yes |
| DELETE | `/system/tasks/{id}` | Cancel a task | Yes |

### Cron Scheduling

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| GET | `/system/cron` | List cron schedules | Yes |
| POST | `/system/cron` | Create a cron schedule | Yes |
| PATCH | `/system/cron/{id}` | Update a cron schedule | Yes |
| DELETE | `/system/cron/{id}` | Delete a cron schedule | Yes |

### Backup & Restore

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/versions/export` | Export database as `.aeordb` | Yes |
| POST | `/versions/diff` | Create patch between versions | Yes |
| POST | `/versions/import` | Import a backup or patch | Yes |
| POST | `/versions/promote` | Promote a version hash to HEAD | Yes |

### Monitoring

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| GET | `/system/stats` | System stats (JSON) | Yes (auth required) |
| GET | `/system/metrics` | Prometheus metrics | Yes (auth required) |
| GET | `/system/health` | Health check | No (public) |

### API Key Management

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/auth/keys/admin` | Create an API key | Yes |
| GET | `/auth/keys/admin` | List all API keys | Yes |
| DELETE | `/auth/keys/admin/{key_id}` | Revoke an API key | Yes |

### User Management

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/system/users` | Create a user | Yes |
| GET | `/system/users` | List all users | Yes |
| GET | `/system/users/{user_id}` | Get a user | Yes |
| PATCH | `/system/users/{user_id}` | Update a user | Yes |
| DELETE | `/system/users/{user_id}` | Deactivate a user (soft delete) | Yes |

### Group Management

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/system/groups` | Create a group | Yes |
| GET | `/system/groups` | List all groups | Yes |
| GET | `/system/groups/{name}` | Get a group | Yes |
| PATCH | `/system/groups/{name}` | Update a group | Yes |
| DELETE | `/system/groups/{name}` | Delete a group | Yes |

---

## Garbage Collection

### POST /system/gc

Run garbage collection synchronously. Identifies and removes orphaned entries not reachable from the current HEAD.

**Query Parameters:**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `dry_run` | boolean | `false` | If true, report what would be collected without deleting |

**Response:** `200 OK`

The response contains GC statistics (entries scanned, reclaimed bytes, etc.).

**Example:**

```bash
# Dry run
curl -X POST "http://localhost:3000/system/gc?dry_run=true" \
  -H "Authorization: Bearer $TOKEN"

# Actual GC
curl -X POST http://localhost:3000/system/gc \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 403 | Non-root user |
| 500 | GC failure |

---

## Background Tasks

### POST /system/tasks/reindex

Enqueue a reindex task for a directory path. Re-scans all files and rebuilds index entries.

**Request Body:**

```json
{
  "path": "/data/"
}
```

**Response:** `200 OK`

```json
{
  "id": "task-uuid-here",
  "task_type": "reindex",
  "status": "pending"
}
```

**Example:**

```bash
curl -X POST http://localhost:3000/system/tasks/reindex \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path": "/data/"}'
```

---

### POST /system/tasks/gc

Enqueue a background GC task (non-blocking).

**Request Body:**

```json
{
  "dry_run": false
}
```

**Response:** `200 OK`

```json
{
  "id": "task-uuid-here",
  "task_type": "gc",
  "status": "pending"
}
```

**Example:**

```bash
curl -X POST http://localhost:3000/system/tasks/gc \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"dry_run": false}'
```

---

### GET /system/tasks

List all tasks with their current progress.

**Response:** `200 OK`

```json
{
  "items": [
    {
      "id": "task-uuid-here",
      "task_type": "reindex",
      "status": "running",
      "args": {"path": "/data/"},
      "progress": 0.45,
      "eta_ms": 1775968500000
    }
  ]
}
```

Each task includes `progress` (0.0-1.0) and `eta_ms` (estimated completion timestamp) if available.

**Example:**

```bash
curl http://localhost:3000/system/tasks \
  -H "Authorization: Bearer $TOKEN"
```

---

### GET /system/tasks/{id}

Get a single task by ID.

**Response:** `200 OK`

```json
{
  "id": "task-uuid-here",
  "task_type": "reindex",
  "status": "running",
  "args": {"path": "/data/"},
  "progress": 0.45,
  "eta_ms": 1775968500000
}
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 404 | Task not found |

---

### DELETE /system/tasks/{id}

Cancel a task.

**Response:** `200 OK`

```json
{
  "id": "task-uuid-here",
  "status": "cancelled"
}
```

**Example:**

```bash
curl -X DELETE http://localhost:3000/system/tasks/task-uuid-here \
  -H "Authorization: Bearer $TOKEN"
```

---

## Cron Scheduling

### GET /system/cron

List all cron schedules.

**Response:** `200 OK`

```json
{
  "items": [
    {
      "id": "nightly-gc",
      "schedule": "0 2 * * *",
      "task_type": "gc",
      "args": {"dry_run": false},
      "enabled": true
    }
  ]
}
```

---

### POST /system/cron

Create a new cron schedule.

**Request Body:**

```json
{
  "id": "nightly-gc",
  "schedule": "0 2 * * *",
  "task_type": "gc",
  "args": {"dry_run": false},
  "enabled": true
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | string | Yes | Unique schedule identifier |
| `schedule` | string | Yes | Cron expression |
| `task_type` | string | Yes | Task type to enqueue (`"gc"`, `"reindex"`) |
| `args` | object | Yes | Arguments passed to the task |
| `enabled` | boolean | Yes | Whether the schedule is active |

**Response:** `201 Created`

**Example:**

```bash
curl -X POST http://localhost:3000/system/cron \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "nightly-gc",
    "schedule": "0 2 * * *",
    "task_type": "gc",
    "args": {"dry_run": false},
    "enabled": true
  }'
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid cron expression |
| 409 | Schedule with this ID already exists |

---

### PATCH /system/cron/{id}

Update a cron schedule. All fields are optional -- only provided fields are changed.

**Request Body:**

```json
{
  "enabled": false,
  "schedule": "0 3 * * *"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | boolean | Enable or disable the schedule |
| `schedule` | string | New cron expression |
| `task_type` | string | New task type |
| `args` | object | New task arguments |

**Response:** `200 OK`

Returns the updated schedule.

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid cron expression |
| 404 | Schedule not found |

---

### DELETE /system/cron/{id}

Delete a cron schedule.

**Response:** `200 OK`

```json
{
  "id": "nightly-gc",
  "deleted": true
}
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 404 | Schedule not found |

---

## Backup & Restore

### POST /versions/export

Export the database (or a specific version) as an `.aeordb` archive file.

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `snapshot` | string | Export a named snapshot (default: HEAD) |
| `hash` | string | Export a specific version by hex hash |

**Response:** `200 OK`

- **Content-Type:** `application/octet-stream`
- **Content-Disposition:** `attachment; filename="export-{hash_prefix}.aeordb"`
- **Body:** binary archive data

**Example:**

```bash
# Export HEAD
curl -X POST http://localhost:3000/versions/export \
  -H "Authorization: Bearer $TOKEN" \
  -o backup.aeordb

# Export a specific snapshot
curl -X POST "http://localhost:3000/versions/export?snapshot=v1.0" \
  -H "Authorization: Bearer $TOKEN" \
  -o backup-v1.aeordb

# Export by hash
curl -X POST "http://localhost:3000/versions/export?hash=a1b2c3d4..." \
  -H "Authorization: Bearer $TOKEN" \
  -o backup.aeordb
```

---

### POST /versions/diff

Create a patch file representing the difference between two versions.

**Query Parameters:**

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `from` | string | Yes | Source snapshot name or hex hash |
| `to` | string | No | Target snapshot name or hex hash (default: HEAD) |

**Response:** `200 OK`

- **Content-Type:** `application/octet-stream`
- **Content-Disposition:** `attachment; filename="patch-{hash_prefix}.aeordb"`
- **Body:** binary patch data

**Example:**

```bash
curl -X POST "http://localhost:3000/versions/diff?from=v1.0&to=v2.0" \
  -H "Authorization: Bearer $TOKEN" \
  -o patch-v1-v2.aeordb
```

---

### POST /versions/import

Import a backup or patch file. Body limit: **10 MB**.

**Query Parameters:**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `force` | boolean | `false` | Force import even if conflicts exist |
| `promote` | boolean | `false` | Promote the imported version to HEAD |

**Request:**

- **Headers:**
  - `Authorization: Bearer <token>` (required)
- **Body:** raw `.aeordb` file bytes

**Response:** `200 OK`

```json
{
  "status": "success",
  "backup_type": "export",
  "entries_imported": 1500,
  "chunks_imported": 3200,
  "files_imported": 450,
  "directories_imported": 30,
  "deletions_applied": 5,
  "version_hash": "a1b2c3d4e5f6...",
  "head_promoted": true
}
```

**Example:**

```bash
curl -X POST "http://localhost:3000/versions/import?promote=true" \
  -H "Authorization: Bearer $TOKEN" \
  --data-binary @backup.aeordb
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid or corrupt backup file |
| 403 | Non-root user |

---

### POST /versions/promote

Promote an arbitrary version hash to HEAD.

**Query Parameters:**

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `hash` | string | Yes | Hex-encoded version hash to promote |

**Response:** `200 OK`

```json
{
  "status": "success",
  "head": "a1b2c3d4e5f6..."
}
```

**Example:**

```bash
curl -X POST "http://localhost:3000/versions/promote?hash=a1b2c3d4e5f6..." \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid hash format |
| 404 | Version hash not found in storage |

---

## Monitoring

### GET /system/stats

System statistics endpoint. Returns a structured JSON snapshot of all engine metrics. All values are read from O(1) atomic counters — this endpoint is safe to poll frequently with no performance impact.

**Response:** `200 OK`

```json
{
  "identity": {
    "version": "0.9.0",
    "database_path": "/data/mydb.aeordb",
    "hash_algorithm": "Blake3_256",
    "chunk_size": 262144,
    "node_id": 1,
    "uptime_seconds": 86400
  },
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
  "latency": {
    "write": { "p50": 5.6, "p95": 15.4, "p99": 20.5 },
    "read": { "p50": 2.1, "p95": 8.3, "p99": 12.0 },
    "query": { "p50": 4.2, "p95": 22.0, "p99": 45.0 },
    "flush": { "p50": 1.2, "p95": 5.0, "p99": 12.0 }
  },
  "health": {
    "disk_usage_percent": 48.5,
    "kv_fill_ratio": 0.72,
    "dedup_hit_rate": 0.33,
    "gc_last_reclaimed_bytes": 1048576,
    "write_buffer_depth": 42
  },
  "sync": {
    "active_peers": 2,
    "failing_peers": 0,
    "last_sync_ms": 1776563922032,
    "sync_lag_entries": { "peer_2": 0, "peer_3": 15 }
  }
}
```

**Response sections:**

| Section | Description |
|---------|-------------|
| `identity` | Server version, database path, hash algorithm, chunk size, node ID, and uptime |
| `counts` | Current totals for files, directories, symlinks, chunks, snapshots, and forks |
| `sizes` | Byte-level storage breakdown: disk total, KV file size, logical data, chunk data, void space, dedup savings |
| `throughput` | Rolling read/write rates (1m, 5m, 15m averages) and peak rates |
| `latency` | Percentile latencies (p50, p95, p99) for write, read, query, and flush operations (in milliseconds) |
| `health` | Operational health signals: disk usage, KV fill ratio, dedup hit rate, last GC reclamation, write buffer depth |
| `sync` | Replication status: active/failing peers, last sync timestamp, per-peer sync lag (only present when replication is active) |

> **Note:** The previous `GET /system/stats` returned a flat object computed via O(n) iteration. The new response is structured into nested sections and is O(1) — no performance concerns polling at high frequency.

**Example:**

```bash
curl http://localhost:3000/system/stats \
  -H "Authorization: Bearer $TOKEN"
```

---

### GET /system/health

Public health check endpoint. No authentication required.

**Response:** `200 OK`

```json
{
  "status": "ok"
}
```

**Example:**

```bash
curl http://localhost:3000/system/health
```

---

### GET /system/metrics

Prometheus-format metrics endpoint. Requires authentication.

**Response:** `200 OK`

- **Content-Type:** `text/plain; version=0.0.4; charset=utf-8`
- **Body:** Prometheus text exposition format

**Example:**

```bash
curl http://localhost:3000/system/metrics \
  -H "Authorization: Bearer $TOKEN"
```

---

## API Key Management

### POST /auth/keys/admin

Create a new API key. The plaintext key is returned **only once** -- store it securely. Requires root.

**Request Body:**

```json
{
  "user_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `user_id` | string (UUID) | No | User to create the key for (defaults to the calling user) |

**Response:** `201 Created`

```json
{
  "key_id": "660e8400-e29b-41d4-a716-446655440001",
  "api_key": "aeor_660e8400_a1b2c3d4e5f6...",
  "user_id": "550e8400-e29b-41d4-a716-446655440000",
  "created_at": "2026-04-13T10:00:00Z"
}
```

**Example:**

```bash
curl -X POST http://localhost:3000/auth/keys/admin \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"user_id": "550e8400-e29b-41d4-a716-446655440000"}'
```

---

### GET /auth/keys/admin

List all API keys (metadata only -- no secrets). Requires root.

**Response:** `200 OK`

```json
{
  "items": [
    {
      "key_id": "660e8400-e29b-41d4-a716-446655440001",
      "user_id": "550e8400-e29b-41d4-a716-446655440000",
      "created_at": "2026-04-13T10:00:00Z",
      "is_revoked": false
    }
  ]
}
```

---

### DELETE /auth/keys/admin/{key_id}

Revoke an API key. Revoked keys cannot be used to obtain tokens. Requires root.

**Response:** `200 OK`

```json
{
  "revoked": true,
  "key_id": "660e8400-e29b-41d4-a716-446655440001"
}
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid key ID format |
| 404 | API key not found |

---

## User Management

### POST /system/users

Create a new user. Requires root.

**Request Body:**

```json
{
  "username": "alice",
  "email": "alice@example.com"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `username` | string | Yes | Unique username |
| `email` | string | No | User email address |

**Response:** `201 Created`

```json
{
  "user_id": "550e8400-e29b-41d4-a716-446655440000",
  "username": "alice",
  "email": "alice@example.com",
  "is_active": true,
  "created_at": 1775968398000,
  "updated_at": 1775968398000
}
```

---

### GET /system/users

List all users. Requires root.

**Response:** `200 OK`

```json
{
  "items": [
    {
      "user_id": "550e8400-e29b-41d4-a716-446655440000",
      "username": "alice",
      "email": "alice@example.com",
      "is_active": true,
      "created_at": 1775968398000,
      "updated_at": 1775968398000
    }
  ]
}
```

---

### GET /system/users/{user_id}

Get a single user. Requires root.

**Response:** `200 OK` (same shape as the user object above)

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid UUID |
| 404 | User not found |

---

### PATCH /system/users/{user_id}

Update a user. All fields are optional. Requires root.

**Request Body:**

```json
{
  "username": "alice_updated",
  "email": "newemail@example.com",
  "is_active": true
}
```

**Response:** `200 OK` (returns the updated user)

---

### DELETE /system/users/{user_id}

Deactivate a user (soft delete -- sets `is_active` to false). Requires root.

**Response:** `200 OK`

```json
{
  "deactivated": true,
  "user_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

---

## Group Management

Groups define path-level access control rules using query-based membership.

### POST /system/groups

Create a new group. Requires root.

**Request Body:**

```json
{
  "name": "editors",
  "default_allow": "/content/*",
  "default_deny": "/system/*",
  "query_field": "role",
  "query_operator": "eq",
  "query_value": "editor"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Unique group name |
| `default_allow` | string | Yes | Path pattern for allowed access |
| `default_deny` | string | Yes | Path pattern for denied access |
| `query_field` | string | Yes | User field to query for membership (must be a safe field) |
| `query_operator` | string | Yes | Comparison operator |
| `query_value` | string | Yes | Value to match against |

**Response:** `201 Created`

```json
{
  "name": "editors",
  "default_allow": "/content/*",
  "default_deny": "/system/*",
  "query_field": "role",
  "query_operator": "eq",
  "query_value": "editor",
  "created_at": 1775968398000,
  "updated_at": 1775968398000
}
```

---

### GET /system/groups

List all groups. Requires root.

**Response:** `200 OK` (object with `items` array of group objects)

---

### GET /system/groups/{name}

Get a single group. Requires root.

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 404 | Group not found |

---

### PATCH /system/groups/{name}

Update a group. All fields are optional. Requires root.

**Request Body:**

```json
{
  "default_allow": "/content/*",
  "query_value": "senior-editor"
}
```

The `query_field` value is validated against a whitelist of safe fields. Attempting to use an unsafe field returns a `400` error.

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Unsafe query field |
| 404 | Group not found |

---

### DELETE /system/groups/{name}

Delete a group. Requires root.

**Response:** `200 OK`

```json
{
  "deleted": true,
  "name": "editors"
}
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 404 | Group not found |
