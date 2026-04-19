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
| POST | `/blobs/check` | Check which chunks the server already has | Yes | 1 MB |
| PUT | `/blobs/chunks/{hash}` | Upload a single chunk | Yes | 10 GB |
| POST | `/blobs/commit` | Atomic multi-file commit from chunks | Yes | 1 MB |

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

The server automatically applies Zstd compression to chunks when beneficial (based on size heuristics). This is transparent to the client.

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

### Request Body

```json
{
  "files": [
    {
      "path": "/data/report.pdf",
      "content_type": "application/pdf",
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
| `files[].chunk_hashes` | array | Yes | Ordered list of hex-encoded chunk hashes |

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
        "chunk_hashes": ["a1b2c3d4...", "f6e5d4c3..."]
      }
    ]
  }'
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 400 | Invalid input (missing path, bad hash, etc.) |
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
      "chunk_hashes": ["hash1", "hash2"]
    }]
  }'
```
