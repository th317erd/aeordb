# Version API

AeorDB provides Git-like version control through snapshots (named points in time) and forks (divergent branches of the data).

## Endpoint Summary

| Method | Path | Description | Auth | Root Required |
|--------|------|-------------|------|---------------|
| POST | `/version/snapshot` | Create a snapshot | Yes | No |
| GET | `/version/snapshots` | List all snapshots | Yes | No |
| POST | `/version/restore` | Restore a snapshot | Yes | Yes |
| DELETE | `/version/snapshot/{name}` | Delete a snapshot | Yes | Yes |
| POST | `/version/fork` | Create a fork | Yes | No |
| GET | `/version/forks` | List all forks | Yes | No |
| POST | `/version/fork/{name}/promote` | Promote fork to HEAD | Yes | Yes |
| DELETE | `/version/fork/{name}` | Abandon a fork | Yes | Yes |

---

## Snapshots

### POST /version/snapshot

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
curl -X POST http://localhost:3000/version/snapshot \
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

### GET /version/snapshots

List all snapshots.

**Response:** `200 OK`

```json
[
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
```

**Example:**

```bash
curl http://localhost:3000/version/snapshots \
  -H "Authorization: Bearer $TOKEN"
```

---

### POST /version/restore

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
curl -X POST http://localhost:3000/version/restore \
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

### DELETE /version/snapshot/{name}

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
curl -X DELETE http://localhost:3000/version/snapshot/v1.0 \
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

### POST /version/fork

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
curl -X POST http://localhost:3000/version/fork \
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

### GET /version/forks

List all active forks.

**Response:** `200 OK`

```json
[
  {
    "name": "experiment",
    "root_hash": "a1b2c3d4e5f6...",
    "created_at": 1775968398000
  }
]
```

**Example:**

```bash
curl http://localhost:3000/version/forks \
  -H "Authorization: Bearer $TOKEN"
```

---

### POST /version/fork/{name}/promote

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
curl -X POST http://localhost:3000/version/fork/experiment/promote \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 403 | Non-root user |
| 404 | Fork not found |
| 500 | Internal failure |

---

### DELETE /version/fork/{name}

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
curl -X DELETE http://localhost:3000/version/fork/experiment \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 403 | Non-root user |
| 404 | Fork not found |
| 500 | Internal failure |
