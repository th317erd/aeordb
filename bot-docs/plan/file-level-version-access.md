# File-Level Version Access — Design Spec

**Date:** 2026-04-13

---

## Overview

Add the ability to read, restore, and view history for individual files at specific historical versions (snapshots). This is the core value proposition for non-developer teams: "I can always get back to any version of any file."

## Scope

Three capabilities:
1. **Read file at version** — retrieve a specific file as it existed at a named snapshot or version hash
2. **Restore file from version** — cherry-pick a single file from a historical version back to HEAD, with an automatic safety snapshot
3. **File history** — list all snapshots where a file existed, with change tracking (added/modified/unchanged/deleted)

---

## API Surface

### Read File at Version

Query parameter on the existing engine route:

```
GET /engine/{*path}?snapshot={name}
GET /engine/{*path}?version={hex_hash}
```

- Returns the file content exactly as it was at that snapshot/version
- Same response shape as a normal `GET /engine/{*path}` (body = file bytes, `Content-Type` header, etc.)
- `snapshot` parameter resolves to a root hash via `version_manager.resolve_root_hash()`
- `version` parameter uses the hash directly
- If both are provided, `snapshot` takes precedence
- 404 if the file did not exist at that version
- 404 if the snapshot/version does not exist
- Requires read permission on the path

### Restore File from Version

New dedicated route:

```
POST /version/file/{*path}/restore
Content-Type: application/json

{ "snapshot": "before-client-review" }
  or
{ "version": "a3f8c1..." }
```

**Flow:**
1. Resolve the historical file via `resolve_file_at_version`
2. Create auto-snapshot named `pre-restore-{ISO8601}` (e.g. `pre-restore-2026-04-13T17-30-00Z`)
   - If name collision: append counter (`pre-restore-2026-04-13T17-30-00Z-2`)
3. Reassemble file content from historical chunk hashes
4. Write to the same path at HEAD via the normal write pipeline (chunking, dedup, directory updates, event emission)

**Auth:** Requires BOTH write permission on the path AND snapshot permission. Missing either results in 403. The auto-snapshot is a mandatory safety mechanism — if we can't create it, the restore does not proceed.

**Response:**
```json
{
  "restored": true,
  "path": "/assets/logo.psd",
  "from_snapshot": "before-client-review",
  "auto_snapshot": "pre-restore-2026-04-13T17-30-00Z",
  "size": 3210240
}
```

**Errors:**
- File not found at version: 404
- Snapshot/version does not exist: 404
- Missing write or snapshot permission: 403
- Engine write failure: 500

### File History

New dedicated route:

```
GET /version/file/{*path}/history
```

**Algorithm:**
1. List all snapshots via `version_manager.list_snapshots()`
2. Sort by `created_at` descending (newest first)
3. For each snapshot, call `resolve_file_at_version` with that snapshot's root hash
4. Compare content hash to prior snapshot to determine `change_type`
5. Omit entries where the file doesn't exist and wasn't in the prior snapshot either

**Change types:**
- `added` — file exists in this snapshot but not the prior one
- `modified` — file exists in both but content hash differs
- `unchanged` — file exists in both with same content hash
- `deleted` — file existed in prior snapshot but not this one

**Response:**
```json
{
  "path": "/assets/logo.psd",
  "history": [
    {
      "snapshot": "final-delivery",
      "timestamp": "2026-04-13T15:00:00Z",
      "change_type": "modified",
      "size": 4821504,
      "content_type": "image/vnd.adobe.photoshop",
      "content_hash": "a3f8c1..."
    },
    {
      "snapshot": "before-client-review",
      "timestamp": "2026-04-10T09:30:00Z",
      "change_type": "added",
      "size": 3210240,
      "content_type": "image/vnd.adobe.photoshop",
      "content_hash": "7b2e44..."
    }
  ]
}
```

**Edge case:** If the file has never existed in any snapshot, returns `200` with an empty `history` array. This is not a 404 — the path is valid, it just has no version history.

**Auth:** Requires read permission on the path.

---

## Engine Layer

### New Module: `version_access.rs`

Separate from `tree_walker.rs` to keep concerns clean. Tree walker does full-tree operations (backup, GC). Version access does targeted path resolution.

### Core Function: `resolve_file_at_version`

```rust
pub fn resolve_file_at_version(
    engine: &StorageEngine,
    root_hash: &[u8],
    path: &str,
) -> EngineResult<(Vec<u8>, FileRecord)>
```

**Algorithm:**
1. Split path into segments (e.g. `/assets/video/hero.mp4` -> `["assets", "video", "hero.mp4"]`)
2. Starting from `root_hash`, load the directory entry
3. For each segment except the last:
   - Parse children (handling both flat and B-tree formats via existing `deserialize_child_entries` / `btree_list_from_node`)
   - Find child matching segment name with `EntryType::DirectoryIndex`
   - If not found: `Err(EngineError::NotFound(...))`
   - Continue with child's hash
4. For the final segment:
   - Find child matching segment name with `EntryType::FileRecord`
   - If not found: `Err(EngineError::NotFound(...))`
   - Load the FileRecord entry, deserialize, return `(file_hash, FileRecord)`

**Performance:** `O(depth)` — reads only the directories on the path to the target file. For a file 4 levels deep, that's 4 directory reads + 1 file record read.

### Content Reassembly

Once we have a historical `FileRecord` with `chunk_hashes`, reassemble the file by reading each chunk from the engine. This is the same operation the existing `read_file_streaming` does, just driven by a historical FileRecord rather than a current KV lookup. Implement as:

```rust
pub fn read_file_at_version(
    engine: &StorageEngine,
    root_hash: &[u8],
    path: &str,
) -> EngineResult<Vec<u8>>
```

Which calls `resolve_file_at_version`, then reads and concatenates chunks.

A streaming variant may be added later if needed for large files, but the non-streaming version is sufficient for the initial implementation.

---

## Server Layer

### Modified Route: `engine_routes.rs`

Modify the existing `GET /engine/{*path}` handler to check for `snapshot` or `version` query parameters. If present, resolve via `version_access::read_file_at_version` instead of the normal `directory_ops::read_file`.

### New Route File: `version_file_routes.rs`

Two new handlers:
- `POST /version/file/{*path}/restore` -> `file_restore`
- `GET /version/file/{*path}/history` -> `file_history`

Registered in the router alongside existing version routes.

---

## Testing

### Unit Tests (`version_access_spec.rs`)
- Resolve file at root level (`/readme.txt`)
- Resolve file nested deeply (`/a/b/c/d/file.txt`)
- Resolve file in B-tree directory (>256 entries)
- File not found at version -> `EngineError::NotFound`
- Directory segment not found -> `EngineError::NotFound`
- Empty path handling
- Invalid root hash -> appropriate error

### Integration Tests (HTTP)
- `GET /engine/file.txt?snapshot=snap1` returns historical content
- `GET /engine/file.txt?version={hash}` returns historical content
- `GET /engine/file.txt?snapshot=nonexistent` returns 404
- `GET /engine/missing.txt?snapshot=snap1` returns 404 (file didn't exist)
- File modified between snapshots returns correct content for each
- `POST /version/file/path/restore` creates auto-snapshot and restores
- Restore with insufficient write permission -> 403
- Restore with insufficient snapshot permission -> 403
- Auto-snapshot name collision handling (multiple restores in same second)
- `GET /version/file/path/history` returns correct change types
- History with file added, modified, unchanged, deleted across snapshots
- History for file that never existed -> empty history array
- History ordering (newest first)

---

## Out of Scope

- Per-write history (tracking every individual write, not just snapshots) — future feature
- Streaming reads for historical files — can be added later
- Directory-level version comparison (diff two snapshots for a whole directory) — future feature
- UI/portal integration — future feature
