# Admin Operations

Administrative endpoints for garbage collection, background tasks, cron scheduling, metrics, health checks, backup/restore, and user/group management. Most admin endpoints require **root** access.

## Endpoint Summary

### Garbage Collection

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/admin/gc` | Run synchronous garbage collection | Yes |

### Background Tasks

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/admin/tasks/reindex` | Trigger a reindex task | Yes |
| POST | `/admin/tasks/gc` | Trigger a background GC task | Yes |
| GET | `/admin/tasks` | List all tasks with progress | Yes |
| GET | `/admin/tasks/{id}` | Get a single task | Yes |
| DELETE | `/admin/tasks/{id}` | Cancel a task | Yes |

### Cron Scheduling

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| GET | `/admin/cron` | List cron schedules | Yes |
| POST | `/admin/cron` | Create a cron schedule | Yes |
| PATCH | `/admin/cron/{id}` | Update a cron schedule | Yes |
| DELETE | `/admin/cron/{id}` | Delete a cron schedule | Yes |

### Backup & Restore

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/admin/export` | Export database as `.aeordb` | Yes |
| POST | `/admin/diff` | Create patch between versions | Yes |
| POST | `/admin/import` | Import a backup or patch | Yes |
| POST | `/admin/promote` | Promote a version hash to HEAD | Yes |

### Monitoring

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| GET | `/admin/metrics` | Prometheus metrics | Yes (auth required) |
| GET | `/admin/health` | Health check | No (public) |

### API Key Management

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/admin/api-keys` | Create an API key | Yes |
| GET | `/admin/api-keys` | List all API keys | Yes |
| DELETE | `/admin/api-keys/{key_id}` | Revoke an API key | Yes |

### User Management

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/admin/users` | Create a user | Yes |
| GET | `/admin/users` | List all users | Yes |
| GET | `/admin/users/{user_id}` | Get a user | Yes |
| PATCH | `/admin/users/{user_id}` | Update a user | Yes |
| DELETE | `/admin/users/{user_id}` | Deactivate a user (soft delete) | Yes |

### Group Management

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/admin/groups` | Create a group | Yes |
| GET | `/admin/groups` | List all groups | Yes |
| GET | `/admin/groups/{name}` | Get a group | Yes |
| PATCH | `/admin/groups/{name}` | Update a group | Yes |
| DELETE | `/admin/groups/{name}` | Delete a group | Yes |

---

## Garbage Collection

### POST /admin/gc

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
curl -X POST "http://localhost:3000/admin/gc?dry_run=true" \
  -H "Authorization: Bearer $TOKEN"

# Actual GC
curl -X POST http://localhost:3000/admin/gc \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 403 | Non-root user |
| 500 | GC failure |

---

## Background Tasks

### POST /admin/tasks/reindex

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
curl -X POST http://localhost:3000/admin/tasks/reindex \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path": "/data/"}'
```

---

### POST /admin/tasks/gc

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
curl -X POST http://localhost:3000/admin/tasks/gc \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"dry_run": false}'
```

---

### GET /admin/tasks

List all tasks with their current progress.

**Response:** `200 OK`

```json
[
  {
    "id": "task-uuid-here",
    "task_type": "reindex",
    "status": "running",
    "args": {"path": "/data/"},
    "progress": 0.45,
    "eta_ms": 1775968500000
  }
]
```

Each task includes `progress` (0.0-1.0) and `eta_ms` (estimated completion timestamp) if available.

**Example:**

```bash
curl http://localhost:3000/admin/tasks \
  -H "Authorization: Bearer $TOKEN"
```

---

### GET /admin/tasks/{id}

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

### DELETE /admin/tasks/{id}

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
curl -X DELETE http://localhost:3000/admin/tasks/task-uuid-here \
  -H "Authorization: Bearer $TOKEN"
```

---

## Cron Scheduling

### GET /admin/cron

List all cron schedules.

**Response:** `200 OK`

```json
[
  {
    "id": "nightly-gc",
    "schedule": "0 2 * * *",
    "task_type": "gc",
    "args": {"dry_run": false},
    "enabled": true
  }
]
```

---

### POST /admin/cron

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
curl -X POST http://localhost:3000/admin/cron \
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

### PATCH /admin/cron/{id}

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

### DELETE /admin/cron/{id}

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

### POST /admin/export

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
curl -X POST http://localhost:3000/admin/export \
  -H "Authorization: Bearer $TOKEN" \
  -o backup.aeordb

# Export a specific snapshot
curl -X POST "http://localhost:3000/admin/export?snapshot=v1.0" \
  -H "Authorization: Bearer $TOKEN" \
  -o backup-v1.aeordb

# Export by hash
curl -X POST "http://localhost:3000/admin/export?hash=a1b2c3d4..." \
  -H "Authorization: Bearer $TOKEN" \
  -o backup.aeordb
```

---

### POST /admin/diff

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
curl -X POST "http://localhost:3000/admin/diff?from=v1.0&to=v2.0" \
  -H "Authorization: Bearer $TOKEN" \
  -o patch-v1-v2.aeordb
```

---

### POST /admin/import

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
curl -X POST "http://localhost:3000/admin/import?promote=true" \
  -H "Authorization: Bearer $TOKEN" \
  --data-binary @backup.aeordb
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid or corrupt backup file |
| 403 | Non-root user |

---

### POST /admin/promote

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
curl -X POST "http://localhost:3000/admin/promote?hash=a1b2c3d4e5f6..." \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid hash format |
| 404 | Version hash not found in storage |

---

## Monitoring

### GET /admin/health

Public health check endpoint. No authentication required.

**Response:** `200 OK`

```json
{
  "status": "ok"
}
```

**Example:**

```bash
curl http://localhost:3000/admin/health
```

---

### GET /admin/metrics

Prometheus-format metrics endpoint. Requires authentication.

**Response:** `200 OK`

- **Content-Type:** `text/plain; version=0.0.4; charset=utf-8`
- **Body:** Prometheus text exposition format

**Example:**

```bash
curl http://localhost:3000/admin/metrics \
  -H "Authorization: Bearer $TOKEN"
```

---

## API Key Management

### POST /admin/api-keys

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
curl -X POST http://localhost:3000/admin/api-keys \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"user_id": "550e8400-e29b-41d4-a716-446655440000"}'
```

---

### GET /admin/api-keys

List all API keys (metadata only -- no secrets). Requires root.

**Response:** `200 OK`

```json
[
  {
    "key_id": "660e8400-e29b-41d4-a716-446655440001",
    "user_id": "550e8400-e29b-41d4-a716-446655440000",
    "created_at": "2026-04-13T10:00:00Z",
    "is_revoked": false
  }
]
```

---

### DELETE /admin/api-keys/{key_id}

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

### POST /admin/users

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

### GET /admin/users

List all users. Requires root.

**Response:** `200 OK`

```json
[
  {
    "user_id": "550e8400-e29b-41d4-a716-446655440000",
    "username": "alice",
    "email": "alice@example.com",
    "is_active": true,
    "created_at": 1775968398000,
    "updated_at": 1775968398000
  }
]
```

---

### GET /admin/users/{user_id}

Get a single user. Requires root.

**Response:** `200 OK` (same shape as the user object above)

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid UUID |
| 404 | User not found |

---

### PATCH /admin/users/{user_id}

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

### DELETE /admin/users/{user_id}

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

### POST /admin/groups

Create a new group. Requires root.

**Request Body:**

```json
{
  "name": "editors",
  "default_allow": "/content/*",
  "default_deny": "/admin/*",
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
  "default_deny": "/admin/*",
  "query_field": "role",
  "query_operator": "eq",
  "query_value": "editor",
  "created_at": 1775968398000,
  "updated_at": 1775968398000
}
```

---

### GET /admin/groups

List all groups. Requires root.

**Response:** `200 OK` (array of group objects)

---

### GET /admin/groups/{name}

Get a single group. Requires root.

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 404 | Group not found |

---

### PATCH /admin/groups/{name}

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

### DELETE /admin/groups/{name}

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
