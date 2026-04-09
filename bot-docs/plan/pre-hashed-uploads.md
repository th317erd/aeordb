# Pre-Hashed Client Uploads — Spec

**Date:** 2026-04-09
**Status:** Approved
**Priority:** High — eliminates redundant hashing, bandwidth waste, and enables atomic multi-file commits

---

## 1. Overview

A four-phase upload protocol where the client does all hashing locally, negotiates dedup with the server, uploads only missing chunks in parallel, then atomically commits a changeset of files by hash list. The server never re-hashes data it already has, and the client never uploads bytes the server doesn't need.

---

## 2. Protocol

### Phase 1: Negotiate

```
GET /upload/config
```

Response:
```json
{
  "hash_algorithm": "blake3_256",
  "chunk_size": 262144
}
```

Returns the database's hash algorithm and chunk size. These are database-wide settings. The client caches this — it doesn't change unless the database is recreated. No auth required (public endpoint).

### Phase 2: Client hashes locally

No server interaction. The client:
1. Reads each file it wants to upload
2. Splits into `chunk_size`-byte slices (last slice may be smaller)
3. Hashes each slice with the specified algorithm
4. Produces a manifest: list of `(path, [chunk_hashes], content_type)` tuples

### Phase 3: Dedup check

```
POST /upload/check
```

Body:
```json
{
  "hashes": ["abc123...", "def456...", "ghi789..."]
}
```

Response:
```json
{
  "needed": ["def456..."],
  "have": ["abc123...", "ghi789..."]
}
```

The client sends the union of all chunk hashes across all files in the changeset. The server checks its KV index for each hash. The response tells the client exactly which bytes to upload.

Requires auth. Does not check path-level permissions (chunks are not associated with paths yet).

### Phase 4a: Upload missing chunks (parallel)

```
PUT /upload/chunks/{hash}
Content-Type: application/octet-stream

<raw bytes>
```

One request per missing chunk. The client fires these concurrently (N parallel requests). The server:
1. Reads the raw bytes
2. Hashes with the database's algorithm (blake3)
3. Compares computed hash to `{hash}` from URL — 400 on mismatch
4. Stores via `store_entry(EntryType::Chunk, ...)` with dedup check
5. Applies `should_compress` for storage — hash is always on uncompressed data
6. Returns 201 (created) or 200 (already existed)

**Size limit:** Rejects any chunk larger than `chunk_size` bytes. Prevents abuse.

**Chunk lifecycle:** Uploaded chunks not committed become garbage. The existing GC mark-and-sweep collects them — uncommitted chunks are unreachable from any version tree.

Requires auth. No path-level permission check (chunks are content-addressed blobs, not files).

### Phase 4b: Commit changeset (atomic)

```
POST /upload/commit
```

Body:
```json
{
  "files": [
    {
      "path": "/data/report.pdf",
      "chunks": ["abc123...", "def456..."],
      "content_type": "application/pdf"
    },
    {
      "path": "/data/summary.txt",
      "chunks": ["ghi789..."],
      "content_type": "text/plain"
    }
  ]
}
```

Response:
```json
{
  "committed": 2,
  "files": [
    { "path": "/data/report.pdf", "size": 524288 },
    { "path": "/data/summary.txt", "size": 12000 }
  ]
}
```

Requires auth. Path-level permissions checked for each file path (user must have create/update permission). If any permission check fails, the whole commit fails with 403 and a list of denied paths.

---

## 3. Commit Internals

### Validation

1. Parse the request body. Validate all fields present.
2. For each file: check path-level permissions (CRUD flags).
3. For each file: verify all referenced chunk hashes exist in the KV store. If any missing → 400 with list of missing hashes.

### Single-pass directory propagation

Today, `store_file` updates directories bottom-up for each file independently. Storing 10 files in `/data/` rewrites `/data/` ten times. The commit does it once.

1. For each file: create the `FileRecord` entry from chunk hash list + metadata. Compute file size by summing chunk sizes.
2. Collect all affected directories by walking paths. Group files by parent directory.
3. Update directories bottom-up, deduplicating: each directory is updated exactly once with all its new/changed children.
4. Propagate content hashes up to root.
5. Update HEAD once.

### WriteBatch atomicity

Steps 1-5 use a single `WriteBatch` — all FileRecords + all directory updates + HEAD update flush with one writer lock acquisition and one KV lock acquisition. Either everything lands or nothing does.

If validation (step above) fails, nothing is written. If the WriteBatch flush fails (disk error), the hot file WAL provides recovery.

### Event emission

Emit a single `entries_created` event with all committed file paths in the payload, rather than one event per file.

---

## 4. Hash Format

All hashes in the protocol are **hex-encoded strings** (lowercase).

**The problem:** The server internally stores chunks with a domain-prefixed hash: `blake3("chunk:" + data)`. But the client doesn't have the data on the server during the dedup check — it only has hashes. The server can't reverse-map a raw hash to a domain-prefixed hash without the data.

**Solution:** Tell the client the domain prefix. The negotiate endpoint returns the prefix:

```json
{
  "hash_algorithm": "blake3_256",
  "chunk_size": 262144,
  "chunk_hash_prefix": "chunk:"
}
```

The client computes: `blake3("chunk:" + raw_bytes)` — the exact same hash the server uses. No mapping needed. The dedup check, chunk upload, and commit all use this hash directly.

This is a one-time protocol detail the client learns during negotiation. It's not an internal leak — it's part of the content-addressing contract. The client needs to produce hashes the server can look up, and this is how.

The `PUT /upload/chunks/{hash}` URL uses this domain-prefixed hash. The server verifies: `blake3("chunk:" + received_bytes) == {hash}`. The stored entry uses `{hash}` as its key — identical to what `store_file` produces for the same data. Full dedup compatibility with the existing upload path.

---

## 5. Endpoints Summary

| Endpoint | Method | Auth | Body | Purpose |
|----------|--------|------|------|---------|
| `/upload/config` | GET | No | — | Negotiate hash algo + chunk size |
| `/upload/check` | POST | Yes | JSON hash list | Dedup check |
| `/upload/chunks/{hash}` | PUT | Yes | Raw bytes | Upload a single chunk |
| `/upload/commit` | POST | Yes | JSON file manifest | Atomic multi-file commit |

All endpoints under `/upload/` prefix.

---

## 6. Clarifications (from critical analysis)

**Chunk ordering:** The `chunks` array in the commit request is the **assembly order**. The first hash is the first 256KB of the file, the second hash is the next 256KB, etc. The server preserves this order in the FileRecord's `chunk_hashes` field. Reordering = data corruption.

**Content-type precedence:** Client-provided `content_type` in the commit wins. If the client omits it (null or absent), the server detects from the first chunk's magic bytes using `detect_content_type`. Same precedence as the existing PUT endpoint.

**GC race with uploads:** A chunk could theoretically be garbage-collected between the dedup check (phase 3) and the commit (phase 4b) if a GC run happens in between. This is accepted — GC is manual (user-triggered via `POST /admin/gc` or CLI `aeordb gc`). The user should not run GC during active upload sessions. If this race occurs, the commit returns the missing hashes in the error response, and the client can re-upload them and retry the commit.

**Dedup check size:** No hard limit on the number of hashes in a `POST /upload/check` request. Even 40,000 hashes (a ~3MB JSON payload) result in sub-second KV lookups with snapshot-based reads. Clients should batch reasonably but the server accepts whatever the HTTP body limit allows (currently 10GB).

**Concurrent commits:** Two clients committing simultaneously with overlapping paths serialize at the engine write lock. The second commit's version of a file overwrites the first. This is the same behavior as two concurrent PUT requests today — last writer wins. No change.

---

## 7. Error Cases

| Scenario | Response |
|----------|----------|
| Chunk hash mismatch (computed ≠ URL hash) | 400 `{"error": "Hash mismatch", "expected": "...", "got": "..."}` |
| Chunk too large (> chunk_size) | 400 `{"error": "Chunk exceeds maximum size", "max": 262144, "got": N}` |
| Commit references missing chunks | 400 `{"error": "Missing chunks", "missing": ["hash1", "hash2"]}` |
| Commit path permission denied | 403 `{"error": "Permission denied", "denied_paths": ["/secret/file.txt"]}` |
| Empty commit (no files) | 400 `{"error": "No files in commit"}` |
| Empty file (zero chunks) | Allowed — creates a zero-size FileRecord |
| Duplicate paths in commit | Last one wins (same as overwriting a file) |

---

## 8. Implementation Phases

### Phase 1 — Upload config endpoint + dedup check
- `GET /upload/config` returns hash algo + chunk size
- `POST /upload/check` accepts hash list, returns needed/have split
- Tests: config returns correct values, check identifies existing vs missing chunks

### Phase 2 — Chunk upload endpoint
- `PUT /upload/chunks/{hash}` stores a verified chunk
- Hash verification, size limit, compression, dedup
- Tests: upload + verify, hash mismatch rejection, size limit, dedup skip, concurrent uploads

### Phase 3 — Atomic commit
- `POST /upload/commit` creates FileRecords + single-pass directory propagation
- WriteBatch atomicity, permission checks, missing chunk validation
- Tests: single file commit, multi-file commit, missing chunk rejection, permission denied, directory dedup

### Phase 4 — Integration + E2E
- Full round-trip: config → check → upload → commit
- Concurrent chunk upload stress test
- GC collects uncommitted chunks

---

## 9. Non-goals (deferred)

- Resumable uploads (track upload session state server-side)
- Client SDK library (the protocol is HTTP — any client can implement it)
- Upload progress tracking / SSE notifications during upload
- Deletions in changesets (stay as individual DELETE requests)
- Multipart form uploads (chunks are individual PUT requests)
- Server-side re-chunking (server stores chunks exactly as the client split them)
