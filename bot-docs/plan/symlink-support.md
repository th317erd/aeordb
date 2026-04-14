# Soft Symlink Support — Design Spec

**Date:** 2026-04-14

---

## Overview

Add soft symlink support to AeorDB as a first-class entity type. Symlinks store a target path and resolve transparently on read. They are versioned like files, appear in directory listings, and survive backup/export/import. The primary driver is the `aeordb-client`, which must faithfully represent local filesystems that contain symlinks.

## Design Principles

- **Unix semantics** — symlinks behave like Unix soft links. Transparent resolution on read. Dangling links are allowed (target need not exist at creation time).
- **First-class entity** — symlinks are a distinct entry type, not files in disguise.
- **Versioned** — snapshots capture the symlink's target path at that point in time.
- **Recursive resolution with cycle detection** — symlinks can point to other symlinks. Cycles are detected via a visited set and produce a clear error. A `MAX_SYMLINK_DEPTH` safety valve (32) prevents pathologically long chains.

---

## Data Model

### New EntryType

```rust
Symlink = 0x08
```

Plus `KV_TYPE_SYMLINK` constant in `kv_store.rs`.

### SymlinkRecord

New struct in `symlink_record.rs`:

```rust
pub struct SymlinkRecord {
    pub path: String,       // the symlink's own normalized path
    pub target: String,     // the target path (absolute, normalized)
    pub created_at: i64,    // ms since epoch
    pub updated_at: i64,    // ms since epoch
}
```

**Serialization format** (binary, same pattern as FileRecord):
```
path_length: u16
path: [u8; path_length]
target_length: u16
target: [u8; target_length]
created_at: i64
updated_at: i64
```

### Storage Keys

- **Path-based key:** `symlink:{normalized_path}` domain-prefixed hash (mutable, for reads/deletion)
- **Content-addressed key:** hash of the serialized SymlinkRecord (immutable, for versioning via ChildEntry.hash)

### Directory Entries

Symlinks appear as `ChildEntry` with:
- `entry_type: 0x08` (Symlink)
- `hash`: content-addressed hash of the SymlinkRecord
- `total_size: 0`
- `content_type: None`
- `name`: the symlink's filename
- `created_at` / `updated_at`: from the SymlinkRecord

---

## API Surface

### Create / Update Symlink

```
POST /engine-symlink/{*path}
Content-Type: application/json

{"target": "/real/path/to/file.txt"}
```

- Creates a symlink at `{path}` pointing to `target`
- If the symlink already exists, updates its target (idempotent)
- Does NOT validate that the target exists (Unix semantics — dangling links allowed)
- Response: `201 Created`

```json
{
  "path": "/link-name",
  "target": "/real/path/to/file.txt",
  "entry_type": 8,
  "created_at": 1776142837736,
  "updated_at": 1776142837736
}
```

### Read (Transparent — Default)

```
GET /engine/{*path}
```

If the path is a symlink, silently resolve the target chain:
- Target is a file → return file content (same as normal file GET)
- Target is a directory → return directory listing
- Target is another symlink → follow the chain (recursive resolution with cycle detection)
- Target doesn't exist (dangling) → 404 with message: `"Dangling symlink: target '/path' does not exist"`
- Cycle detected → 400 with message: `"Symlink cycle detected: /a → /b → /a"`
- Max depth exceeded → 400 with message: `"Symlink resolution exceeded maximum depth (32)"`

### Read (No Follow)

```
GET /engine/{*path}?nofollow=true
```

The `nofollow` parameter is added to the existing `EngineGetQuery` struct as `nofollow: Option<bool>`.

Returns symlink metadata as JSON without following:

```json
{
  "path": "/link-name",
  "target": "/real/path/to/file.txt",
  "entry_type": 8,
  "created_at": 1776142837736,
  "updated_at": 1776142837736
}
```

### HEAD

```
HEAD /engine/{*path}
```

If it's a symlink, returns headers:
- `X-Entry-Type: symlink`
- `X-Symlink-Target: /real/path/to/file.txt`

### Delete

```
DELETE /engine/{*path}
```

Existing delete endpoint. If the path is a symlink, deletes the symlink itself, not the target.

### Directory Listings

Symlinks appear in listings with `entry_type: 8` and a `target` field:

```json
{
  "path": "/project/latest",
  "name": "latest",
  "entry_type": 8,
  "hash": "...",
  "target": "/project/v2/logo.psd",
  "total_size": 0,
  "created_at": 1776142837736,
  "updated_at": 1776142837736,
  "content_type": null
}
```

The `target` field is included by reading the SymlinkRecord when building the listing response. For non-symlink entries, this field is absent.

---

## Engine Layer

### New Module: `symlink_record.rs`

`SymlinkRecord` struct with `new`, `serialize`, `deserialize` methods.

Hash functions:
- `symlink_path_hash(path, algo)` — `symlink:{path}` domain prefix
- `symlink_content_hash(data, algo)` — `symlinkc:{serialized_data}` domain prefix

### New Module: `symlink_resolver.rs`

```rust
pub const MAX_SYMLINK_DEPTH: usize = 32;

pub enum ResolvedTarget {
    File(FileRecord),
    Directory(String),  // the resolved directory path (caller lists it)
}

pub fn resolve_symlink(
    engine: &StorageEngine,
    path: &str,
) -> EngineResult<ResolvedTarget>
```

**Algorithm:**
1. Create `HashSet<String>` for visited paths, set depth counter to 0
2. Normalize `path`, add to visited set
3. Look up path in engine:
   - Check if it's a symlink (try `symlink_path_hash` + `get_entry`):
     - If found: deserialize SymlinkRecord, get target path
     - If target is in visited set → `Err(EngineError::CyclicSymlink(...))`
     - If depth >= MAX_SYMLINK_DEPTH → `Err(EngineError::SymlinkDepthExceeded(...))`
     - Add target to visited, increment depth, loop back to step 3 with target path
   - Check if it's a file (`file_path_hash` + `get_entry`):
     - If found → return `ResolvedTarget::File(record)`
   - Check if it's a directory (`directory_path_hash` + `get_entry`):
     - If found → return `ResolvedTarget::Directory(resolved_path)`
   - None found → `Err(EngineError::NotFound(...))`

### New Error Variants

Add to `EngineError`:
- `CyclicSymlink(String)` — cycle detected during resolution
- `SymlinkDepthExceeded(String)` — exceeded MAX_SYMLINK_DEPTH

### Modify: `directory_ops.rs`

Add methods:
- `store_symlink(ctx, path, target)` — creates/updates a symlink
- `delete_symlink(ctx, path)` — deletes a symlink (or reuse existing delete with symlink awareness)
- `get_symlink(path)` — reads a SymlinkRecord at path

The store method:
1. Create SymlinkRecord
2. Serialize, store at content-addressed key and path-based key
3. Build ChildEntry with entry_type Symlink
4. Update parent directory (same propagation as files)

### Modify: `entry_type.rs`

Add `Symlink = 0x08` variant and update `from_u8`, `to_kv_type`.

### Modify: `kv_store.rs`

Add `KV_TYPE_SYMLINK` constant.

### Modify: `engine/mod.rs`

Register new modules, add re-exports.

### Touch Points (Existing Code)

- **`tree_walker.rs`** — add `EntryType::Symlink` case to `walk_directory`. Treat symlinks as leaf entries (like files). Store in a new `symlinks: HashMap<String, (Vec<u8>, SymlinkRecord)>` field on `VersionTree`.
- **`version_access.rs`** — `resolve_file_at_version` should also handle the final segment being a symlink (return the SymlinkRecord data).
- **`gc.rs`** — `gc_mark` must include symlink entries as live roots.
- **`backup.rs`** — `export_version` and `import_backup` must handle symlinks in the tree.
- **`directory_listing.rs`** — include symlinks as `ListingEntry` with entry_type 8. Add `target` field to `ListingEntry`.
- **`engine_routes.rs`** — `engine_get` must check for symlinks before the file/directory checks. If symlink and no `nofollow`, resolve and serve. `engine_head` must return symlink headers. `engine_delete` must handle symlink deletion.
- **`diff_trees`** in `tree_walker.rs` — handle symlinks in diff output.

---

## Server Layer

### New Route File: `symlink_routes.rs`

Handler for `POST /engine-symlink/{*path}`.

### Modify `engine_routes.rs`

- `engine_get`: check symlink first → resolve or return metadata (nofollow)
- `engine_head`: if symlink → return X-Entry-Type and X-Symlink-Target headers
- `engine_delete`: handle symlink deletion
- Directory listing serialization: for ChildEntry with entry_type Symlink, include `target` field by looking up the SymlinkRecord

### Modify `server/mod.rs`

Register `POST /engine-symlink/{*path}` route.

---

## Versioning Behavior

- Snapshots capture symlinks as-is (the SymlinkRecord with its target path)
- Restoring a snapshot restores the symlink (the link, not the resolved content)
- File history (`/version/file-history/{*path}`) treats symlinks like files — tracks when the symlink was added/modified/deleted across snapshots
- `resolve_file_at_version` at a historical version: if the path was a symlink at that version, the caller gets the SymlinkRecord (not transparent resolution of the historical target)

---

## Testing

### Unit Tests

**`symlink_record_spec.rs`:**
- Serialize/deserialize roundtrip
- Field preservation (path, target, timestamps)

**`symlink_resolver_spec.rs`:**
- Resolve symlink → file
- Resolve symlink → directory
- Resolve symlink → symlink → file (chain of 2)
- Resolve symlink → symlink → symlink → file (chain of 3)
- Dangling symlink → NotFound
- Cyclic symlink (A → B → A) → CyclicSymlink error
- Self-referencing symlink (A → A) → CyclicSymlink error
- Long chain at MAX_SYMLINK_DEPTH → SymlinkDepthExceeded error
- Symlink to target that later becomes a symlink → follows chain

**`symlink_ops_spec.rs`:**
- Store symlink, read back SymlinkRecord
- Update symlink target (store again with different target)
- Delete symlink, verify gone
- Symlink appears in parent directory listing with entry_type 8
- Store symlink to nonexistent target (succeeds — no validation)
- Symlink versioning: snapshot captures symlink, modify target, restore brings back old target

### HTTP Integration Tests

**`symlink_http_spec.rs`:**
- POST /engine-symlink/link.txt → 201
- POST again with different target → updates
- GET /engine/link.txt → returns target file content
- GET /engine/link.txt?nofollow=true → returns symlink JSON
- HEAD /engine/link.txt → X-Symlink-Target header
- DELETE /engine/link.txt → deletes symlink, target unaffected
- GET dangling symlink → 404 with dangling message
- Directory symlink → GET returns target directory listing
- Symlink chain resolution (link1 → link2 → file) → returns file content
- Cyclic symlink → error response
- POST with missing target field → 400
- Symlink in directory listing includes target field
- Symlink versioning via snapshot/restore

### E2E Curl Verification (Real World)

Against a running instance:
1. Create a file at /assets/logo.psd
2. Create a symlink /latest-logo → /assets/logo.psd
3. GET /engine/latest-logo → returns logo content
4. GET /engine/latest-logo?nofollow=true → returns symlink metadata
5. HEAD /engine/latest-logo → X-Symlink-Target header
6. Create a second symlink /alias → /latest-logo (chain)
7. GET /engine/alias → returns logo content (resolved through chain)
8. Create cyclic symlink /loop-a → /loop-b, /loop-b → /loop-a
9. GET /engine/loop-a → cycle error
10. Create snapshot, change symlink target, restore snapshot → old target restored
11. Delete symlink, verify target file still exists
12. Directory listing shows symlinks with entry_type 8

---

## Out of Scope

- Hard links (content-addressed storage already gives you dedup — hard links add little value)
- Relative symlink targets (all paths are absolute within the database)
- Permission changes propagated through symlinks (permissions are on the target, not the link)
- Symlink-aware query engine (queries search files, not symlinks)
