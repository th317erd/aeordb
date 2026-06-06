# Bug Report: `/blobs/commit` Stores Generic MIME Types As Authoritative Metadata

Date: 2026-06-01

## Summary

Files uploaded through the chunked blob API can be persisted with `content_type: "application/octet-stream"` even when the file is a recognizable media type such as MP4.

This appears to be caused by `/blobs/commit` treating any supplied `content_type` as authoritative, including generic fallback values. That behavior differs from the normal `DirectoryOps::store_file_*` path, which treats `application/octet-stream` as unknown and runs MIME detection.

The visible client symptom was an MP4 file in AeorDB displaying as `application/octet-stream` instead of previewing as video.

## Observed Failure

Client browse API for the Taraani sync returned this entry for `skoal.mp4`:

```json
{
  "name": "skoal.mp4",
  "entry_type": 2,
  "size": 9957,
  "content_type": "application/octet-stream",
  "created_at": 1780336302150,
  "updated_at": 1780336302150,
  "sync_status": "synced",
  "has_local": true,
  "effective_permissions": "crudlify"
}
```

The local file is an MP4:

```text
/home/wyatt/Pictures/skoal.mp4: video/mp4
```

Its first bytes are normal MP4 `ftyp` bytes:

```text
00000000: 0000 0020 6674 7970 6973 6f6d 0000 0200  ... ftypisom....
00000010: 6973 6f6d 6973 6f32 6176 6331 6d70 3431  isomiso2avc1mp41
```

## Relevant Code Paths

Normal file stores do MIME refinement:

- `aeordb-lib/src/engine/directory_ops.rs`
- `DirectoryOps::finalize_file(...)`
- `DirectoryOps::store_file_internal_inner(...)`
- both call:

```rust
crate::engine::content_type::detect_content_type(first_bytes_or_data, content_type)
```

`detect_content_type` intentionally treats generic values as unknown:

- `aeordb-lib/src/engine/content_type.rs`

```rust
// If caller provided a specific content type, trust it
if let Some(ct) = provided {
    if ct != "application/octet-stream" && !ct.is_empty() {
        return ct.to_string();
    }
}
```

Chunked blob commits do not follow that same contract:

- `aeordb-lib/src/engine/batch_commit.rs`
- `commit_files(...)`

Current behavior:

```rust
let detected_content_type = if let Some(ref ct) = file.content_type {
    ct.clone()
} else if !chunk_hashes.is_empty() {
    match engine.get_entry(&chunk_hashes[0])? {
        Some((_h, _k, v)) => detect_content_type(&v, None),
        None => "application/octet-stream".to_string(),
    }
} else {
    "application/octet-stream".to_string()
};
```

That means a caller-supplied `application/octet-stream` bypasses detection and is stored directly in:

- `FileRecord.content_type`
- `ChildEntry.content_type`
- directory listing responses
- sync diff responses
- file browser metadata
- native parser/indexing metadata

## Why This Matters

`application/octet-stream` is normally not a meaningful assertion that "this file must be generic binary"; it is often the fallback value from clients and HTTP stacks when they do not know better.

The engine already acknowledges this in `detect_content_type`, and existing tests cover that behavior for normal stores:

- `aeordb-lib/spec/engine/content_type_spec.rs`
- `test_octet_stream_overridden_in_storage`

But chunk commits bypass that behavior, so chunked upload and regular upload can store different metadata for the same bytes.

This is especially visible for media files:

- videos may not preview as videos
- audio may not preview as audio
- parsers/indexers may choose the wrong handling path
- `@content_type` queries can return stale/generic data
- clients that skip unchanged files by content hash may never naturally repair the metadata

## Expected Behavior

`/blobs/commit` should use the same content-type contract as normal file storage:

1. Trust a specific caller-provided MIME type, such as `video/mp4`, `image/png`, or `application/pdf`.
2. Treat `None`, empty string, and `application/octet-stream` as unknown.
3. For unknown/generic values, inspect the first chunk bytes with `detect_content_type`.
4. Store the refined type in `FileRecord`, `ChildEntry`, events, listings, sync diffs, and query-visible metadata.

## Suggested Fix

In `aeordb-lib/src/engine/batch_commit.rs`, route the provided value through `detect_content_type` instead of blindly cloning it.

Conceptually:

```rust
let first_chunk_bytes = if !chunk_hashes.is_empty() {
    engine.get_entry(&chunk_hashes[0])?.map(|(_h, _k, v)| v)
} else {
    None
};

let detected_content_type = match first_chunk_bytes {
    Some(bytes) => detect_content_type(&bytes, file.content_type.as_deref()),
    None => detect_content_type(&[], file.content_type.as_deref()),
};
```

This preserves explicit useful MIME types while allowing generic fallback values to be refined.

## Acceptance Tests

Add tests against the chunk commit path, not only `DirectoryOps`:

1. `/blobs/commit` with `content_type: "application/octet-stream"` and MP4 `ftyp` bytes stores `video/mp4`.
2. `/blobs/commit` with `content_type: null` and MP4 `ftyp` bytes stores `video/mp4`.
3. `/blobs/commit` with `content_type: "video/custom"` preserves `video/custom`.
4. `/blobs/commit` with empty file and no content type stores `application/octet-stream`.
5. Directory listing after commit exposes the refined type.
6. `GET /files/...` response uses the refined `content-type` header.

Useful existing spec files:

- `aeordb-lib/spec/http/upload_commit_spec.rs`
- `aeordb-lib/spec/http/upload_e2e_spec.rs`
- `aeordb-lib/spec/engine/content_type_spec.rs`

## Notes From Client-Side Investigation

The aeordb-client had also been missing media extensions in its extension-based MIME guesser, so older client builds could send `application/octet-stream` for MP4 files. That client-side map is being fixed separately.

However, the engine should still not persist generic MIME fallback values as authoritative when it has enough bytes to refine them. The normal storage path already behaves that way; the chunk commit path should match it.

There is also a stale metadata issue after the bad value has already landed: if a client tracks only content hash and mtime, an unchanged file may be skipped and the stored generic `content_type` may remain until something forces a recommit or metadata repair. Fixing `/blobs/commit` prevents new bad writes, but existing records with generic types may need a repair/migration strategy or explicit recommit.
