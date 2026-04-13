# Files & Directories

AeorDB exposes a content-addressable filesystem through its engine routes. Every path under `/engine/` represents either a file or a directory.

## Endpoint Summary

| Method | Path | Description | Auth | Status Codes |
|--------|------|-------------|------|-------------|
| PUT | `/engine/{path}` | Store a file | Yes | 201, 400, 404, 409, 500 |
| GET | `/engine/{path}` | Read a file or list a directory | Yes | 200, 404, 500 |
| DELETE | `/engine/{path}` | Delete a file | Yes | 200, 404, 500 |
| HEAD | `/engine/{path}` | Check existence and get metadata | Yes | 200, 404, 500 |

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

```json
[
  {
    "name": "report.pdf",
    "entry_type": "file",
    "total_size": 245678,
    "created_at": 1775968398000,
    "updated_at": 1775968398000,
    "content_type": "application/pdf"
  },
  {
    "name": "images",
    "entry_type": "directory",
    "total_size": 0,
    "created_at": 1775968000000,
    "updated_at": 1775968000000,
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
| `X-Entry-Type` | `file` or `directory` |
| `X-Path` | Canonical path |
| `X-Total-Size` | File size in bytes (files only) |
| `X-Created-At` | Unix timestamp in milliseconds (files only) |
| `X-Updated-At` | Unix timestamp in milliseconds (files only) |
| `Content-Type` | MIME type (files only, if known) |

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
