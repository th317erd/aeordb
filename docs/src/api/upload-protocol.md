# Pre-Hashed Upload Protocol

AeorDB provides a 4-phase upload protocol for efficient, deduplicated file transfers. Clients split files into chunks, hash them locally, and only upload chunks the server does not already have.

> **When to use this protocol:** Inline uploads via `PUT /files/{path}` are capped at 100 MB. Files larger than 100 MB must use this chunked upload protocol. It is also beneficial for large batches of files because the dedup check (phase 2) skips chunks already on the server.

## Protocol Overview

1. **Negotiate** -- GET `/blobs/config` to learn the hash algorithm and chunk size.
2. **Dedup check** -- POST `/blobs/check` with a list of chunk hashes to find which are already stored.
3. **Upload** -- PUT `/blobs/chunks/{hash}` for each needed chunk.
4. **Commit** -- POST `/blobs/commit` to atomically assemble chunks into files.

## Endpoint Summary

| Method | Path | Description | Auth | Body Limit |
|--------|------|-------------|------|-----------|
| GET | `/blobs/config` | Negotiate hash algorithm and chunk size | No | -- |
| POST | `/blobs/check` | Check which chunks the server already has | Yes | 32 MiB |
| PUT | `/blobs/chunks/{hash}` | Upload a single chunk | Yes | 10 GB |
| POST | `/blobs/commit` | Atomic multi-file commit from chunks | Yes | 32 MiB |

---

## Phase 1: GET /blobs/config

Retrieve the server's hash algorithm, chunk size, and hash prefix. This endpoint is **public** (no authentication required).

### Response

**Status:** `200 OK`

```json
{
  "hash_algorithm": "blake3",
  "chunk_size": 262144,
  "chunk_hash_prefix": "chunk:"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `hash_algorithm` | string | Hash algorithm used by the server (e.g., `"blake3"`) |
| `chunk_size` | integer | Maximum chunk size in bytes (262,144 = 256 KB) |
| `chunk_hash_prefix` | string | Prefix prepended to chunk data before hashing |

### How to Compute Chunk Hashes

The server computes chunk hashes as:

```
hash = blake3("chunk:" + chunk_bytes)
```

Clients must use the same formula. The prefix (`"chunk:"`) is prepended to the raw bytes before hashing, not to the hex-encoded hash.

### Example

```bash
curl http://localhost:6830/blobs/config
```

---

## Phase 2: POST /blobs/check

Send a list of chunk hashes to determine which ones the server already has (deduplication). Only upload the ones in the `needed` list.

The request body is a JSON manifest and is capped at 32 MiB. Clients syncing
very large files or large batches should split `/blobs/check` calls before that
limit.

### Request Body

```json
{
  "hashes": [
    "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2",
    "f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1f6e5"
  ]
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `hashes` | array of strings | Yes | Hex-encoded chunk hashes |

### Response

**Status:** `200 OK`

```json
{
  "have": [
    "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
  ],
  "needed": [
    "f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3b2a1f6e5"
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `have` | array | Hashes the server already has -- skip these |
| `needed` | array | Hashes the server needs -- upload these |

### Example

```bash
curl -X POST http://localhost:6830/blobs/check \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"hashes": ["a1b2c3...", "f6e5d4..."]}'
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 400 | Invalid hex hash in the list |

---

## Phase 3: PUT /blobs/chunks/{hash}

Upload a single chunk. The server verifies the hash matches the content before storing.

### Request

- **URL parameter:** `{hash}` -- hex-encoded blake3 hash of `"chunk:" + chunk_bytes`
- **Headers:**
  - `Authorization: Bearer <token>` (required)
- **Body:** raw chunk bytes

### Hash Verification

The server recomputes the hash from the uploaded bytes:

```
computed = blake3("chunk:" + body_bytes)
```

If the computed hash does not match the URL parameter, the upload is rejected.

### Response

**Status:** `201 Created` (new chunk stored)

```json
{
  "status": "created",
  "hash": "f6e5d4c3b2a1..."
}
```

**Status:** `200 OK` (chunk already exists -- dedup)

```json
{
  "status": "exists",
  "hash": "f6e5d4c3b2a1..."
}
```

### Compression

Blob staging stores chunk bytes exactly as uploaded. AeorDB does not blindly
compress `/blobs/chunks/` payloads because large media files are often already
compressed, and commit can use raw chunk headers for fast metadata validation.

### Example

```bash
curl -X PUT http://localhost:6830/blobs/chunks/f6e5d4c3b2a1... \
  -H "Authorization: Bearer $TOKEN" \
  --data-binary @chunk_001.bin
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 400 | Chunk exceeds maximum size (262,144 bytes) |
| 400 | Invalid hex hash in URL |
| 400 | Hash mismatch between URL and computed hash |
| 500 | Storage failure |

---

## Phase 4: POST /blobs/commit

Atomically commit multiple files from previously uploaded chunks. Each file specifies its path, content type, and the ordered list of chunk hashes that compose it.

The request body is a JSON manifest and is capped at 32 MiB. This is separate
from the 10 GB raw chunk upload limit because `/blobs/commit` carries paths and
hash references, not file bytes.

During commit, AeorDB records the raw whole-file content hash
(`blake3(file bytes)`) in the file metadata. That stored value backs `@hash`
searches; it is not derived from the first chunk.

By default, AeorDB streams the ordered stored chunks and computes that hash on
the server. Trusted sync clients that already computed the raw file hash can
send `content_hash` and `size` with each file. For raw stored chunks, AeorDB
then validates chunk existence and byte length from KV metadata and can publish
the FileRecord without rereading every chunk body.

### Request Body

```json
{
  "files": [
    {
      "path": "/data/report.pdf",
      "content_type": "application/pdf",
      "content_hash": "9b01f3e3d06f...",
      "size": 15234212,
      "chunk_hashes": [
        "a1b2c3d4e5f6...",
        "f6e5d4c3b2a1..."
      ]
    },
    {
      "path": "/data/image.png",
      "content_type": "image/png",
      "chunk_hashes": [
        "1234abcd5678..."
      ]
    }
  ]
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `files` | array | Yes | List of files to commit |
| `files[].path` | string | Yes | Destination path for the file |
| `files[].content_type` | string | No | MIME type |
| `files[].chunk_hashes` | array | Yes | Ordered list of hex-encoded chunk hashes. `chunks` is also accepted for compatibility. |
| `files[].content_hash` | string | No | Raw whole-file hash (`blake3(file bytes)`) as hex. When paired with `size`, this enables the raw-chunk metadata fast path for trusted callers. |
| `files[].size` | integer | No | Total raw file byte length. If supplied, AeorDB validates it against stored chunk metadata or the streamed byte count. |

### Response

**Status:** `200 OK`

The response contains a summary of the commit operation.

### Example

```bash
curl -X POST http://localhost:6830/blobs/commit \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "files": [
      {
        "path": "/data/report.pdf",
        "content_type": "application/pdf",
        "content_hash": "9b01f3e3d06f...",
        "size": 15234212,
        "chunk_hashes": ["a1b2c3d4...", "f6e5d4c3..."]
      }
    ]
  }'
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 400 | Invalid input (missing path, bad hash, size mismatch, etc.) |
| 429 | Blob commit workers are saturated, or an identical commit is already in progress. The response includes `"retryable": true`. |
| 500 | Commit task failure or panic |

---

## Full Upload Workflow

Here is a complete workflow for uploading a file:

```bash
# 1. Get server configuration
CONFIG=$(curl -s http://localhost:6830/blobs/config)
CHUNK_SIZE=$(echo $CONFIG | jq -r '.chunk_size')

# 2. Split file into chunks and hash them
# (pseudo-code: split report.pdf into 256KB chunks, hash each with blake3)
# chunk_hashes=["hash1", "hash2", ...]
# content_hash=blake3(report.pdf raw bytes)
# size=report.pdf raw byte length

# 3. Check which chunks are needed
DEDUP=$(curl -s -X POST http://localhost:6830/blobs/check \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"hashes": ["hash1", "hash2"]}')

# 4. Upload only the needed chunks
for hash in $(echo $DEDUP | jq -r '.needed[]'); do
  curl -X PUT "http://localhost:6830/blobs/chunks/$hash" \
    -H "Authorization: Bearer $TOKEN" \
    --data-binary @"chunk_$hash.bin"
done

# 5. Commit the file
curl -X POST http://localhost:6830/blobs/commit \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "files": [{
      "path": "/data/report.pdf",
      "content_type": "application/pdf",
      "content_hash": "whole-file-hash",
      "size": 15234212,
      "chunk_hashes": ["hash1", "hash2"]
    }]
  }'
```
