# Version API

AeorDB provides Git-like version control through snapshots (named points in time) and forks (divergent branches of the data).

## Endpoint Summary

| Method | Path | Description | Auth | Root Required |
|--------|------|-------------|------|---------------|
| POST | `/versions/snapshots` | Create a snapshot | Yes | No |
| GET | `/versions/snapshots` | List all snapshots | Yes | No |
| POST | `/versions/restore` | Restore a snapshot | Yes | Yes |
| DELETE | `/versions/snapshots/{name}` | Delete a snapshot | Yes | Yes |
| POST | `/versions/forks` | Create a fork | Yes | No |
| GET | `/versions/forks` | List all forks | Yes | No |
| POST | `/versions/forks/{name}/promote` | Promote fork to HEAD | Yes | Yes |
| DELETE | `/versions/forks/{name}` | Abandon a fork | Yes | Yes |
| GET | `/files/{path}?snapshot={name}` | Read file at a snapshot | Yes | No |
| GET | `/files/{path}?version={hash}` | Read file at a version hash | Yes | No |
| GET | `/versions/history/{path}` | File change history across snapshots | Yes | No |
| POST | `/versions/restore/{path}` | Restore file from a version | Yes | Yes |

---

## Snapshots

### POST /versions/snapshots

Create a named snapshot of the current HEAD.

**Request Body:**

```json
{
  "name": "v1.0",
  "metadata": {
    "description": "First stable release",
    "author": "alice"
  }
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Unique snapshot name |
| `metadata` | object | No | Arbitrary key-value metadata (defaults to empty) |

**Response:** `201 Created`

```json
{
  "name": "v1.0",
  "root_hash": "a1b2c3d4e5f6...",
  "created_at": 1775968398000,
  "metadata": {
    "description": "First stable release",
    "author": "alice"
  }
}
```

**Example:**

```bash
curl -X POST http://localhost:6830/versions/snapshots \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name": "v1.0", "metadata": {"description": "First stable release"}}'
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 409 | Snapshot with this name already exists |
| 500 | Internal failure |

---

### GET /versions/snapshots

List all snapshots.

**Response:** `200 OK`

```json
{
  "items": [
    {
      "name": "v1.0",
      "root_hash": "a1b2c3d4e5f6...",
      "created_at": 1775968398000,
      "metadata": {"description": "First stable release"}
    },
    {
      "name": "v2.0",
      "root_hash": "f6e5d4c3b2a1...",
      "created_at": 1775969000000,
      "metadata": {}
    }
  ]
}
```

**Example:**

```bash
curl http://localhost:6830/versions/snapshots \
  -H "Authorization: Bearer $TOKEN"
```

---

### POST /versions/restore

Restore a named snapshot, making it the current HEAD. **Requires root.**

**Request Body:**

```json
{
  "name": "v1.0"
}
```

**Response:** `200 OK`

```json
{
  "restored": true,
  "name": "v1.0"
}
```

**Example:**

```bash
curl -X POST http://localhost:6830/versions/restore \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name": "v1.0"}'
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 403 | Non-root user |
| 404 | Snapshot not found |
| 500 | Internal failure |

---

### DELETE /versions/snapshots/{name}

Delete a named snapshot. **Requires root.**

**Response:** `200 OK`

```json
{
  "deleted": true,
  "name": "v1.0"
}
```

**Example:**

```bash
curl -X DELETE http://localhost:6830/versions/snapshots/v1.0 \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 403 | Non-root user |
| 404 | Snapshot not found |
| 500 | Internal failure |

---

## Forks

Forks create a divergent branch of the data, optionally based on a named snapshot.

### POST /versions/forks

Create a new fork.

**Request Body:**

```json
{
  "name": "experiment",
  "base": "v1.0"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Unique fork name |
| `base` | string | No | Snapshot name to fork from (defaults to current HEAD) |

**Response:** `201 Created`

```json
{
  "name": "experiment",
  "root_hash": "a1b2c3d4e5f6...",
  "created_at": 1775968398000
}
```

**Example:**

```bash
curl -X POST http://localhost:6830/versions/forks \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name": "experiment", "base": "v1.0"}'
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 409 | Fork with this name already exists |
| 500 | Internal failure |

---

### GET /versions/forks

List all active forks.

**Response:** `200 OK`

```json
{
  "items": [
    {
      "name": "experiment",
      "root_hash": "a1b2c3d4e5f6...",
      "created_at": 1775968398000
    }
  ]
}
```

**Example:**

```bash
curl http://localhost:6830/versions/forks \
  -H "Authorization: Bearer $TOKEN"
```

---

### POST /versions/forks/{name}/promote

Promote a fork's state to HEAD, making it the active version. **Requires root.**

**Response:** `200 OK`

```json
{
  "promoted": true,
  "name": "experiment"
}
```

**Example:**

```bash
curl -X POST http://localhost:6830/versions/forks/experiment/promote \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 403 | Non-root user |
| 404 | Fork not found |
| 500 | Internal failure |

---

### DELETE /versions/forks/{name}

Abandon a fork (soft delete). **Requires root.**

**Response:** `200 OK`

```json
{
  "abandoned": true,
  "name": "experiment"
}
```

**Example:**

```bash
curl -X DELETE http://localhost:6830/versions/forks/experiment \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 403 | Non-root user |
| 404 | Fork not found |
| 500 | Internal failure |

---

## File-Level Version Access

Read, restore, and view history for individual files at specific historical versions.

### Reading Files at a Version

Use query parameters on the standard file read endpoint:

```bash
# Read a file as it was at a named snapshot
curl "http://localhost:6830/files/assets/logo.psd?snapshot=v1.0" \
  -H "Authorization: Bearer $TOKEN"

# Read a file at a specific version hash
curl "http://localhost:6830/files/assets/logo.psd?version=a1b2c3d4..." \
  -H "Authorization: Bearer $TOKEN"
```

Returns the file content exactly as it was at that version, with the same headers as a normal file read. If both `snapshot` and `version` are provided, `snapshot` takes precedence.

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 404 | File did not exist at that version |
| 404 | Snapshot or version not found |
| 400 | Invalid version hash (not valid hex) |

---

### GET /versions/history/{path}

View the change history of a single file across all snapshots. Returns entries ordered newest-first, each with a `change_type` indicating what happened to the file at that snapshot.

**Response:** `200 OK`

```json
{
  "path": "assets/logo.psd",
  "history": [
    {
      "snapshot": "v2.0",
      "timestamp": 1775969000000,
      "change_type": "modified",
      "size": 512000,
      "content_type": "image/vnd.adobe.photoshop",
      "content_hash": "f6e5d4c3..."
    },
    {
      "snapshot": "v1.0",
      "timestamp": 1775968398000,
      "change_type": "added",
      "size": 256000,
      "content_type": "image/vnd.adobe.photoshop",
      "content_hash": "a1b2c3d4..."
    }
  ]
}
```

**Change types:**

| Type | Meaning |
|------|---------|
| `added` | File exists in this snapshot but not the previous one |
| `modified` | File exists in both but content changed |
| `unchanged` | File exists in both with identical content |
| `deleted` | File existed in the previous snapshot but not this one |

If the file has never existed in any snapshot, returns `200` with an empty `history` array.

**Example:**

```bash
curl http://localhost:6830/versions/history/assets/logo.psd \
  -H "Authorization: Bearer $TOKEN"
```

---

### POST /versions/restore/{path}

Restore a single file from a historical version to the current HEAD. **Requires root.**

Before restoring, an automatic safety snapshot is created (named `pre-restore-{timestamp}`) to preserve the current state. If the safety snapshot cannot be created, the restore is rejected.

**Request Body:**

```json
{
  "snapshot": "v1.0"
}
```

Or using a version hash:

```json
{
  "version": "a1b2c3d4..."
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `snapshot` | string | One required | Snapshot name to restore from |
| `version` | string | One required | Version hash (hex) to restore from |

If both are provided, `snapshot` takes precedence.

**Response:** `200 OK`

```json
{
  "restored": true,
  "path": "assets/logo.psd",
  "from_snapshot": "v1.0",
  "auto_snapshot": "pre-restore-2026-04-14T05-01-01Z",
  "size": 256000
}
```

The `auto_snapshot` field contains the name of the safety snapshot created before the restore. You can use this snapshot to recover the pre-restore state if needed.

**Example:**

```bash
curl -X POST http://localhost:6830/versions/restore/assets/logo.psd \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"snapshot": "v1.0"}'
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Neither `snapshot` nor `version` provided |
| 403 | Non-root user (requires both write and snapshot permissions) |
| 404 | File not found at the specified version |
| 404 | Snapshot or version not found |
| 500 | Failed to create safety snapshot or write restored file |
