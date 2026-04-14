# Enhanced Directory Listing — Design Spec

**Date:** 2026-04-13

---

## Overview

Enhance the existing `GET /engine/{*path}` directory listing to support recursive traversal with depth control, glob filtering by file name, and content hashes in every response. Driven by the needs of `aeordb-client`, which must diff its local file state against the server efficiently.

## Motivation

The client needs to answer: "Which of my local files are different from (or missing on) the server?" This requires:
1. Content hashes on every file entry (already stored in `ChildEntry.hash`, just not serialized)
2. Recursive listing so the client doesn't have to walk the tree directory-by-directory
3. Glob filtering so the client can scope queries to specific file types

---

## API Surface

Enhanced query parameters on the existing `GET /engine/{*path}` when path resolves to a directory:

```
GET /engine/assets/                         # immediate children (default, depth=0)
GET /engine/assets/?depth=3                 # 3 levels deep
GET /engine/assets/?depth=-1                # unlimited recursion
GET /engine/assets/?depth=-1&glob=*.psd     # all PSDs anywhere under /assets/
GET /engine/assets/?depth=0&glob=*.mp4      # only .mp4 files in immediate children
```

### Query Parameters

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `depth` | integer | `0` | `0` = immediate children. Positive = that many levels. `-1` = unlimited. Other negatives treated as `-1`. |
| `glob` | string | none | Glob pattern matched against the **file name** (not full path). Supports `*`, `?`, `[abc]`, `{a,b}`. |

### Response Shape

Flat JSON array. Each entry is a file with its full path and content hash:

```json
[
  {
    "path": "/assets/logo.psd",
    "name": "logo.psd",
    "entry_type": 2,
    "hash": "a3f8c1...",
    "total_size": 4821504,
    "created_at": 1776142837736,
    "updated_at": 1776142837736,
    "content_type": "image/vnd.adobe.photoshop"
  },
  {
    "path": "/assets/video/hero.mp4",
    "name": "hero.mp4",
    "entry_type": 2,
    "hash": "7b2e44...",
    "total_size": 52428800,
    "created_at": 1776142837800,
    "updated_at": 1776142837800,
    "content_type": "video/mp4"
  }
]
```

### Backwards Compatibility

When `depth=0` (default, no params), the response is the same as today but with two new fields added: `hash` (hex-encoded content hash) and `path` (full path). Existing clients get extra fields — nothing removed or moved.

### Files Only (Recursive Mode)

When `depth > 0` or `depth == -1` (recursive mode), the listing returns **files only**. Directory entries are traversed for recursion but are not included in the output. The client can infer directory structure from file paths. Directory metadata (permissions, groups) is a separate concern handled by the server's permission model, not the file listing.

When `depth=0` (default), the existing behavior is preserved: both files and directories are returned as immediate children, with `entry_type` distinguishing them. This maintains backwards compatibility. The new `hash` and `path` fields are added to all entries; for directory entries, `hash` is the directory's content-addressed hash.

---

## Engine Layer

### New Module: `directory_listing.rs`

Separate from `directory_ops.rs` to keep that file from growing further.

```rust
pub struct ListingEntry {
    pub path: String,
    pub name: String,
    pub entry_type: u8,
    pub hash: Vec<u8>,
    pub total_size: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub content_type: Option<String>,
}

pub fn list_directory_recursive(
    engine: &StorageEngine,
    base_path: &str,
    depth: i32,
    glob: Option<&str>,
) -> EngineResult<Vec<ListingEntry>>
```

**Algorithm:**
1. Normalize `base_path`, resolve via `directory_path_hash` + `engine.get_entry`
2. Parse children (handle both flat and B-tree format via existing helpers)
3. For each child:
   - If `FileRecord`: build full path (`base_path + "/" + name`), check glob match against `name` if glob is present, add to results if it matches (or no glob)
   - If `DirectoryIndex` and depth allows (depth > 0 or depth == -1): recurse with `depth - 1` (or -1 for unlimited)
   - Skip other entry types
4. Return flat vec of all matching file entries

**Glob matching:** Use a small official crate — `glob-match` (zero deps) or similar. Matched against the file name only, not the full path.

**Depth semantics:**
- `0` — immediate children only (no recursion into subdirectories)
- `1` — children + one level of subdirectories
- `-1` — unlimited recursion
- Other negative values treated as `-1`

### Register Module

Add `pub mod directory_listing;` and re-exports in `engine/mod.rs`.

---

## Server Layer

### Modify `engine_routes.rs`

1. **Rename `VersionQuery` to `EngineGetQuery`** and add depth/glob fields:

```rust
#[derive(Deserialize, Default)]
pub struct EngineGetQuery {
    pub snapshot: Option<String>,
    pub version: Option<String>,
    pub depth: Option<i32>,
    pub glob: Option<String>,
}
```

2. **In the directory listing branch of `engine_get`:**
   - If `depth` is present OR `glob` is present: call `list_directory_recursive(engine, path, depth.unwrap_or(0), glob.as_deref())`
   - Otherwise: use existing `list_directory` for the default case
   - In both cases, serialize `hash` (hex-encoded) and `path` in the response

3. **Always include `hash` and `path`** in directory listing responses — even for the default depth=0 case. The `hash` is already on `ChildEntry`, just hex-encode and include it.

---

## Dependencies

Add a lightweight glob matching crate to `aeordb-lib/Cargo.toml`. Candidates:
- `glob-match` — zero deps, ~100 lines, supports `*`, `?`, `[abc]`, `{a,b}`
- Choose during implementation based on what's available and maintained

---

## Testing

### Unit Tests (`directory_listing_spec.rs`)

- `test_list_immediate_children` — depth=0 returns only direct file children
- `test_list_depth_1` — returns files at root + one level of subdirectories
- `test_list_unlimited_depth` — depth=-1 returns all files recursively
- `test_list_glob_filter` — `*.txt` returns only text files
- `test_list_glob_with_depth` — `*.txt` at depth=1
- `test_list_empty_directory` — returns empty vec
- `test_list_nonexistent_directory` — returns NotFound error
- `test_list_no_glob_matches` — glob pattern that matches nothing returns empty vec
- `test_list_depth_boundary` — depth=2 returns files at depths 0, 1, 2 but not 3
- `test_list_files_only` — directory entries excluded from output
- `test_list_includes_content_hash` — every entry has a non-empty hash

### HTTP Integration Tests (`directory_listing_http_spec.rs`)

- `test_default_listing_includes_hash_and_path` — backwards compat: new fields present
- `test_recursive_unlimited` — `?depth=-1` returns flat recursive list
- `test_recursive_depth_1` — correct depth boundary
- `test_glob_filter` — `?glob=*.txt` filters correctly
- `test_glob_with_depth` — `?depth=-1&glob=*.psd` combo
- `test_directories_excluded` — only files in output
- `test_nonexistent_directory_404` — 404 for missing path
- `test_file_get_unaffected` — normal GET on a file still works (regression)
- `test_version_query_still_works` — `?snapshot=` on a file still works (regression)

---

## Out of Scope

- Pagination / streaming for very large listings — future enhancement if needed
- Glob matching against full path (only matches file name)
- Directory entries in recursive output
- Permission filtering (server already handles 403s at the route level)
