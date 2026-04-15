# Files & Directories

AeorDB exposes a content-addressable filesystem through its engine routes. Every path under `/engine/` represents either a file or a directory.

## Endpoint Summary

| Method | Path | Description | Auth | Status Codes |
|--------|------|-------------|------|-------------|
| PUT | `/engine/{path}` | Store a file | Yes | 201, 400, 404, 409, 500 |
| GET | `/engine/{path}` | Read a file or list a directory | Yes | 200, 404, 500 |
| DELETE | `/engine/{path}` | Delete a file | Yes | 200, 404, 500 |
| HEAD | `/engine/{path}` | Check existence and get metadata | Yes | 200, 404, 500 |
| POST | `/engine-symlink/{path}` | Create or update a symlink | Yes | 201, 400, 500 |

---

## PUT /engine/{path}

Store a file at the given path. Parent directories are created automatically. If a file already exists at the path, it is overwritten (creating a new version).

**Body limit:** 10 GB

### Request

- **Headers:**
  - `Authorization: Bearer <token>` (required)
  - `Content-Type` (optional) -- auto-detected from magic bytes if omitted
- **Body:** raw file bytes

### Response

**Status:** `201 Created`

```json
{
  "path": "/data/report.pdf",
  "content_type": "application/pdf",
  "total_size": 245678,
  "created_at": 1775968398000,
  "updated_at": 1775968398000
}
```

### Side Effects

- If the path matches `/.config/indexes.json` (or a nested variant like `/data/.config/indexes.json`), a reindex task is automatically enqueued for the parent directory. Any existing pending or running reindex for that path is cancelled first.
- Triggers `entries_created` events on the event bus.
- Runs any deployed store-phase plugins.

### Example

```bash
curl -X PUT http://localhost:3000/engine/data/report.pdf \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/pdf" \
  --data-binary @report.pdf
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 400 | Invalid input (e.g., empty path) |
| 404 | Parent path references a non-existent entity |
| 409 | Path conflict (e.g., file exists where directory expected) |
| 500 | Internal storage failure |

---

## GET /engine/{path}

Read a file or list a directory. The server determines the type automatically:
- If the path resolves to a **file**, the file content is streamed with appropriate headers.
- If the path resolves to a **directory**, a JSON array of children is returned.

### Request

- **Headers:**
  - `Authorization: Bearer <token>` (required)

### Query Parameters

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `snapshot` | string | — | Read the file as it was at this named snapshot |
| `version` | string | — | Read the file at this version hash (hex) |
| `nofollow` | boolean | `false` | If the path is a symlink, return metadata instead of following |
| `depth` | integer | `0` | Directory listing depth: `0` = immediate children, `-1` = unlimited recursion |
| `glob` | string | — | Filter directory listing by file name glob pattern (`*`, `?`, `[abc]`) |

### File Response

**Status:** `200 OK`

**Headers:**

| Header | Description |
|--------|-------------|
| `X-Path` | Canonical path of the file |
| `X-Total-Size` | File size in bytes |
| `X-Created-At` | Unix timestamp (milliseconds) |
| `X-Updated-At` | Unix timestamp (milliseconds) |
| `Content-Type` | MIME type (if known) |

**Body:** raw file bytes (streamed)

### Directory Response

**Status:** `200 OK`

Each entry includes `path`, `hash`, and numeric `entry_type` fields. Symlink entries also include a `target` field.

Entry types: `2` = file, `3` = directory, `8` = symlink.

```json
[
  {
    "path": "/data/report.pdf",
    "name": "report.pdf",
    "entry_type": 2,
    "hash": "a3f8c1...",
    "total_size": 245678,
    "created_at": 1775968398000,
    "updated_at": 1775968398000,
    "content_type": "application/pdf"
  },
  {
    "path": "/data/images",
    "name": "images",
    "entry_type": 3,
    "hash": "b2c4d5...",
    "total_size": 0,
    "created_at": 1775968000000,
    "updated_at": 1775968000000,
    "content_type": null
  },
  {
    "path": "/data/latest",
    "name": "latest",
    "entry_type": 8,
    "hash": "c3d5e6...",
    "target": "/data/report.pdf",
    "total_size": 0,
    "created_at": 1775968500000,
    "updated_at": 1775968500000,
    "content_type": null
  }
]
```

### Examples

Read a file:

```bash
curl http://localhost:3000/engine/data/report.pdf \
  -H "Authorization: Bearer $TOKEN" \
  -o report.pdf
```

List a directory:

```bash
curl http://localhost:3000/engine/data/ \
  -H "Authorization: Bearer $TOKEN"
```

### Recursive Directory Listing

Use the `depth` and `glob` query parameters to list files recursively:

```bash
# List all files recursively
curl http://localhost:3000/engine/data/?depth=-1 \
  -H "Authorization: Bearer $TOKEN"

# List only .psd files anywhere under /assets/
curl "http://localhost:3000/engine/assets/?depth=-1&glob=*.psd" \
  -H "Authorization: Bearer $TOKEN"

# List one level deep
curl http://localhost:3000/engine/data/?depth=1 \
  -H "Authorization: Bearer $TOKEN"
```

When `depth > 0` or `depth = -1`, the response contains **files only** in a flat list. Directory entries are traversed but not included in the output.

### Versioned Reads

Read a file as it was at a specific snapshot or version:

```bash
# Read file at a named snapshot
curl "http://localhost:3000/engine/data/report.pdf?snapshot=v1.0" \
  -H "Authorization: Bearer $TOKEN"

# Read file at a specific version hash
curl "http://localhost:3000/engine/data/report.pdf?version=a1b2c3..." \
  -H "Authorization: Bearer $TOKEN"
```

If both `snapshot` and `version` are provided, `snapshot` takes precedence. Returns 404 if the file did not exist at that version.

### Error Responses

| Status | Condition |
|--------|-----------|
| 404 | Path does not exist as file or directory |
| 500 | Internal read failure |

---

## DELETE /engine/{path}

Delete a file at the given path. Creates a `DeletionRecord` and removes the file from its parent directory listing. Directories cannot be deleted directly -- delete all files within first.

### Request

- **Headers:**
  - `Authorization: Bearer <token>` (required)

### Response

**Status:** `200 OK`

```json
{
  "deleted": true,
  "path": "/data/report.pdf"
}
```

### Side Effects

- Triggers `entries_deleted` events on the event bus.
- Updates index entries for the deleted file.

### Example

```bash
curl -X DELETE http://localhost:3000/engine/data/report.pdf \
  -H "Authorization: Bearer $TOKEN"
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 404 | File not found |
| 500 | Internal deletion failure |

---

## Symlinks

AeorDB supports soft symlinks — entries that point to another path. Symlinks are transparent by default: reading a symlink path returns the target's content.

### POST /engine-symlink/{path}

Create or update a symlink.

**Request Body:**

```json
{
  "target": "/assets/logo.psd"
}
```

**Response:** `201 Created`

```json
{
  "path": "/latest-logo",
  "target": "/assets/logo.psd",
  "entry_type": 8,
  "created_at": 1775968398000,
  "updated_at": 1775968398000
}
```

The target path does not need to exist at creation time (dangling symlinks are allowed).

### Reading Symlinks

By default, `GET /engine/{path}` follows symlinks transparently:

```bash
# Returns the content of /assets/logo.psd
curl http://localhost:3000/engine/latest-logo \
  -H "Authorization: Bearer $TOKEN"
```

To inspect the symlink itself without following it, use `?nofollow=true`:

```bash
curl "http://localhost:3000/engine/latest-logo?nofollow=true" \
  -H "Authorization: Bearer $TOKEN"
```

Returns the symlink metadata as JSON instead of the target's content.

### Symlink Resolution

Symlinks can point to other symlinks — chains are followed recursively. AeorDB detects cycles and enforces a maximum resolution depth of 32 hops.

| Scenario | Result |
|----------|--------|
| Symlink → file | Returns file content |
| Symlink → directory | Returns directory listing |
| Symlink → symlink → file | Follows chain, returns file content |
| Symlink → nonexistent | 404 (dangling symlink) |
| Symlink cycle (A → B → A) | 400 with cycle detection message |
| Chain exceeds 32 hops | 400 with depth exceeded message |

### HEAD on Symlinks

`HEAD /engine/{path}` returns symlink metadata as headers:

```
X-Entry-Type: symlink
X-Symlink-Target: /assets/logo.psd
X-Path: /latest-logo
X-Created-At: 1775968398000
X-Updated-At: 1775968398000
```

### Deleting Symlinks

`DELETE /engine/{path}` on a symlink deletes the symlink itself, not the target:

```bash
curl -X DELETE http://localhost:3000/engine/latest-logo \
  -H "Authorization: Bearer $TOKEN"
```

```json
{
  "deleted": true,
  "path": "latest-logo",
  "type": "symlink"
}
```

### Symlinks in Directory Listings

Symlinks appear in directory listings with `entry_type: 8` and a `target` field:

```json
{
  "path": "/data/latest",
  "name": "latest",
  "entry_type": 8,
  "hash": "c3d5e6...",
  "target": "/data/report.pdf",
  "total_size": 0,
  "created_at": 1775968500000,
  "updated_at": 1775968500000,
  "content_type": null
}
```

### Symlink Versioning

Symlinks are versioned like files. Snapshots capture the symlink's target path at that point in time. Restoring a snapshot restores the link, not the resolved content.

---

## HEAD /engine/{path}

Check whether a path exists and retrieve its metadata as response headers, without downloading the body. Works for both files and directories.

### Request

- **Headers:**
  - `Authorization: Bearer <token>` (required)

### Response

**Status:** `200 OK` (empty body)

**Headers:**

| Header | Value |
|--------|-------|
| `X-Entry-Type` | `file`, `directory`, or `symlink` |
| `X-Path` | Canonical path |
| `X-Total-Size` | File size in bytes (files only) |
| `X-Created-At` | Unix timestamp in milliseconds (files only) |
| `X-Updated-At` | Unix timestamp in milliseconds (files only) |
| `Content-Type` | MIME type (files only, if known) |
| `X-Symlink-Target` | Target path (symlinks only) |

### Example

```bash
curl -I http://localhost:3000/engine/data/report.pdf \
  -H "Authorization: Bearer $TOKEN"
```

```
HTTP/1.1 200 OK
X-Entry-Type: file
X-Path: /data/report.pdf
X-Total-Size: 245678
X-Created-At: 1775968398000
X-Updated-At: 1775968398000
Content-Type: application/pdf
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 404 | Path does not exist |
| 500 | Internal metadata lookup failure |
