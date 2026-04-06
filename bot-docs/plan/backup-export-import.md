# Version Export, Patch & Import — Implementation Spec

**Date:** 2026-04-06
**Status:** Approved

---

## 1. Overview

Three operations for extracting and restoring versioned database state:

- **Export** — extract a complete, usable .aeordb from a single version hash. No history, no voids, no deleted entries. Clean tree.
- **Diff/Patch** — extract only the changeset between two version hashes. Contains only new/changed chunks, updated FileRecords/DirectoryIndexes, and deletion markers. NOT a usable database.
- **Import** — apply an export or patch to a target database. Does NOT auto-promote HEAD. User explicitly promotes when ready.

All three produce/consume standard .aeordb files. The `backup_type` field in the file header distinguishes them.

Users who want a raw byte-for-byte copy can `cp data.aeordb backup.aeordb`. We don't provide tooling for that — it's a filesystem operation.

---

## 2. File Header Extension

Three new fields in the reserved bytes of the existing 256-byte `FileHeader`:

| Field | Type | Offset | Description |
|-------|------|--------|-------------|
| `backup_type` | u8 | After `buffer_nvt_offset` | 0=normal, 1=full export, 2=patch |
| `base_hash` | [u8; N] | After `backup_type` | Source version hash (N = hash_algo.hash_length()) |
| `target_hash` | [u8; N] | After `base_hash` | Destination version hash |

For `backup_type = 0` (normal database): `base_hash` and `target_hash` are zeroed.

For `backup_type = 1` (full export): `base_hash == target_hash` (both are the exported version).

For `backup_type = 2` (patch): `base_hash` is the "from" version, `target_hash` is the "to" version.

### Engine open behavior:

- `backup_type 0` — normal open, no restrictions
- `backup_type 1` — normal open, no restrictions (it's a complete, usable database)
- `backup_type 2` — **refuse to open** with actionable error:
  ```
  Error: This is a patch export and cannot be used as a standalone database.

    Base version:   a1b2c3d4e5f6...
    Target version: f6e5d4c3b2a1...

    To apply this patch, import it into a database at the base version:
      aeordb import --database <your.aeordb> --file <this_file>
  ```

A separate `StorageEngine::open_for_import()` allows opening all types (including patches) for import/inspection purposes.

---

## 3. Export Operation

Extracts a complete, clean database from a single version hash.

### Algorithm:

```
Input: source .aeordb + version hash (snapshot name, HEAD, or raw hash)
Output: new .aeordb file (backup_type = 1)

1. Resolve version hash:
   - --head → read HEAD hash from file header
   - --snapshot name → look up snapshot, get root_hash
   - --hash raw → use directly
2. Walk the directory tree from root_hash recursively:
   - For each DirectoryIndex: collect it + its chunk hashes
   - For each FileRecord: collect it + its chunk hashes
   - Collect all unique chunk hashes referenced by any entry
3. Create new .aeordb file
4. Write FileHeader:
   - backup_type = 1
   - base_hash = version_hash
   - target_hash = version_hash
   - hash_algo = source hash_algo
5. Write all collected entries:
   - Chunks first (they're referenced by FileRecords/DirectoryIndexes)
   - FileRecords
   - DirectoryIndexes
6. Build KV block + NVT from written entries
7. Finalize file header with KV/NVT offsets
```

The output is a clean, compacted database. No voids, no deletion records, no stale overwrites, no snapshots, no forks. Just the tree at that version. It can be opened with `StorageEngine::open` and used normally.

### CLI:

```bash
# Export current HEAD
aeordb export --database data.aeordb --output backup.aeordb

# Export a specific snapshot
aeordb export --database data.aeordb --snapshot v1.0.0 --output v1.aeordb

# Export a specific hash
aeordb export --database data.aeordb --hash a1b2c3... --output version.aeordb
```

### HTTP:

```
POST /admin/export              → streams HEAD export as .aeordb
POST /admin/export?snapshot=v1  → streams snapshot export
POST /admin/export?hash=a1b2... → streams specific version export
```

Response: `application/octet-stream` with `Content-Disposition: attachment; filename="export-{hash_prefix}.aeordb"`

---

## 4. Diff/Patch Operation

Extracts only the changeset between two versions. The output is NOT a usable database — it's an instruction set for transforming version A into version B.

### Algorithm:

```
Input: source .aeordb + from_hash + to_hash
Output: new .aeordb file (backup_type = 2)

1. Resolve from_hash and to_hash (snapshot name, HEAD, or raw hash)
2. Walk from_hash tree → collect all (path, file_hash, chunk_hashes) → Map A
3. Walk to_hash tree → collect all (path, file_hash, chunk_hashes) → Map B
4. Collect all chunk hashes referenced by Map A → Set chunks_A
5. Compare:
   - path in B but not A → ADDED
     - Include: FileRecord + DirectoryIndex changes
     - Include: only chunks NOT in chunks_A (new chunks only)
   - path in both, different file_hash → MODIFIED
     - Include: B's FileRecord + DirectoryIndex changes
     - Include: only chunks in B's entry NOT in chunks_A (new/changed chunks only)
   - path in A but not B → DELETED
     - Include: DeletionRecord for the path
6. Create new .aeordb file
7. Write FileHeader:
   - backup_type = 2
   - base_hash = from_hash
   - target_hash = to_hash
8. Write all new chunks, new/modified FileRecords, changed DirectoryIndexes, DeletionRecords
9. Build KV block + NVT
```

Key: only chunks that DON'T exist in the base version are included. A modified file that shares 90% of its chunks with the base version only exports the 10% that changed, plus the updated FileRecord with the new chunk list.

### CLI:

```bash
# Diff between two snapshots
aeordb diff --database data.aeordb --from v1.0.0 --to v2.0.0 --output patch.aeordb

# Diff from snapshot to current HEAD
aeordb diff --database data.aeordb --from v1.0.0 --output patch.aeordb

# Diff between two raw hashes
aeordb diff --database data.aeordb --from a1b2... --to f6e5... --output patch.aeordb
```

### HTTP:

```
POST /admin/diff?from=v1.0.0&to=v2.0.0 → streams patch .aeordb
POST /admin/diff?from=v1.0.0            → diff from snapshot to HEAD
POST /admin/diff?from=a1b2...&to=f6e5... → diff between hashes
```

---

## 5. Import Operation

Applies an export or patch .aeordb to a target database. Does NOT automatically promote HEAD.

### Algorithm:

```
Input: target .aeordb + backup .aeordb file + optional --promote + optional --force

1. Open backup file with StorageEngine::open_for_import()
2. Read backup_type, base_hash, target_hash from header

For full export (backup_type = 1):
  3. Scan all entries in the backup
  4. For each entry: store into target database
     - Chunks: store via engine (content-addressed dedup handles duplicates)
     - FileRecords: store via DirectoryOps
     - DirectoryIndexes: store via DirectoryOps
  5. Print summary:
     "Import complete. Entries imported: N. Version hash: target_hash.
      HEAD has NOT been changed.
      To promote: aeordb promote target_hash"
  6. If --promote: update target HEAD to target_hash, create snapshot

For patch (backup_type = 2):
  3. Read target database HEAD hash
  4. If HEAD != base_hash AND --force not set:
     Error: "Target database HEAD (xxx) does not match patch base version (yyy).
             Use --force to apply anyway."
  5. Scan all entries in the patch:
     - Chunks: store into target (dedup handles existing chunks)
     - FileRecords: store into target (these reference both existing and new chunks)
     - DeletionRecords: delete from target via DirectoryOps
     - DirectoryIndexes: store into target
  6. Print summary:
     "Patch applied. Entries: N added, M modified, D deleted. Version hash: target_hash.
      HEAD has NOT been changed.
      To promote: aeordb promote target_hash"
  7. If --promote: update target HEAD to target_hash, create snapshot
```

### CLI:

```bash
# Import a full export (inspect before promoting)
aeordb import --database target.aeordb --file backup.aeordb

# Import and promote in one shot
aeordb import --database target.aeordb --file backup.aeordb --promote

# Import a patch (strict base check)
aeordb import --database target.aeordb --file patch.aeordb

# Import a patch (skip base check)
aeordb import --database target.aeordb --file patch.aeordb --force

# Import patch and promote
aeordb import --database target.aeordb --file patch.aeordb --promote
```

### HTTP:

```
POST /admin/import                → upload .aeordb, apply without promoting
POST /admin/import?promote=true   → upload .aeordb, apply and promote HEAD
POST /admin/import?force=true     → skip base version check for patches
POST /admin/import?promote=true&force=true → both
```

Request: `Content-Type: application/octet-stream`, body is the .aeordb file.

Response:
```json
{
  "status": "success",
  "backup_type": "patch",
  "entries_imported": 47,
  "files_added": 8,
  "files_modified": 3,
  "files_deleted": 1,
  "chunks_imported": 35,
  "version_hash": "f6e5d4c3b2a1...",
  "head_promoted": false,
  "message": "Patch applied. HEAD has NOT been changed. To promote: POST /admin/promote?hash=f6e5d4c3b2a1..."
}
```

---

## 6. Promote Operation

Separate from import. Updates HEAD to a specific version hash.

### CLI:

```bash
aeordb promote --database data.aeordb --hash f6e5d4c3b2a1...
```

### HTTP:

```
POST /admin/promote?hash=f6e5d4c3b2a1...
```

This already exists as part of the version management system (`update_head`). The CLI command is new but the engine method exists.

---

## 7. Tree Walking

Both export and diff need to walk a version's directory tree recursively. This is a core utility:

```rust
/// Walk a version's directory tree and collect all entries.
/// Returns: (file_records, directory_indexes, chunk_hashes)
fn walk_version_tree(
    engine: &StorageEngine,
    root_hash: &[u8],
) -> EngineResult<VersionTree> {
    // 1. Load root DirectoryIndex from root_hash
    // 2. For each child:
    //    - If file: collect FileRecord + its chunk hashes
    //    - If directory: recurse, collect DirectoryIndex + children
    // 3. Return all collected entries
}

struct VersionTree {
    files: HashMap<String, (Vec<u8>, FileRecord)>,  // path → (file_hash, record)
    directories: HashMap<String, (Vec<u8>, Vec<u8>)>,  // path → (dir_hash, dir_data)
    chunks: HashSet<Vec<u8>>,  // all chunk hashes referenced
}
```

The diff operation uses two `VersionTree` instances and compares them.

---

## 8. Edge Cases

### Empty database export
Export of an empty database (only root directory) produces a minimal .aeordb with just the root DirectoryIndex. Valid and usable.

### Patch with no changes
If from_hash == to_hash, the patch is empty (no entries). Valid but pointless. The tool should warn: "No changes between versions."

### Large files
Files split into many chunks. Export collects all chunks. Diff only collects chunks not in the base. Content-addressed dedup means shared chunks across files are only written once.

### Import into non-empty database
Full export import into a database that already has data: files are stored alongside existing data. The imported tree exists as a version that can be promoted. Existing data is not deleted unless the user promotes and then deletes old snapshots.

### Interrupted import
If import fails partway through (crash, disk full), the target database has partially imported entries but HEAD hasn't changed. The database is still at its previous state. The user can retry the import — content-addressed storage handles duplicates gracefully.

### Hash algorithm mismatch
If the backup uses a different hash algorithm than the target database, import should fail with a clear error: "Hash algorithm mismatch: backup uses X, target uses Y."

---

## 9. Implementation Phases

### Phase 1 — File Header Extension
- Add `backup_type`, `base_hash`, `target_hash` to FileHeader
- Update serialize/deserialize
- Add open guard for patch type (backup_type > 1)
- Add `StorageEngine::open_for_import`
- Tests: header round-trip, open guard, actionable error message

### Phase 2 — Tree Walker
- Implement `walk_version_tree` utility
- Collect files, directories, chunk hashes from a root hash
- Tests: empty tree, single file, nested directories, large trees

### Phase 3 — Export
- Implement export operation: walk tree → write clean .aeordb
- CLI command: `aeordb export`
- Tests: export HEAD, export snapshot, export by hash, verify output is openable

### Phase 4 — Diff/Patch
- Implement tree comparison (walk two trees, find differences)
- Implement patch generation: only new chunks, updated records, deletion markers
- CLI command: `aeordb diff`
- Tests: added files, modified files (shared chunks), deleted files, no changes

### Phase 5 — Import
- Implement import for full exports and patches
- Base version verification (strict + --force)
- --promote flag
- CLI command: `aeordb import`
- CLI command: `aeordb promote`
- Tests: import export, import patch (matching base), import patch (wrong base), --force, --promote

### Phase 6 — HTTP API
- `POST /admin/export`
- `POST /admin/diff`
- `POST /admin/import`
- `POST /admin/promote`
- Tests: HTTP round-trip export → import, streaming large exports

### Phase 7 — E2E Verification
- Create database with files → snapshot → modify → snapshot → export → diff → import → verify
- Verify chunk dedup: shared chunks not duplicated in patch
- Verify interrupted import recovery
- Verify hash algorithm mismatch detection

---

## 10. Non-Goals (Deferred)

- Automatic backup scheduling (requires cron/background task system)
- Backup retention policies (auto-prune old exports)
- Streaming export (write as we walk, don't collect everything in memory first) — optimize later
- Text-based export format (JSON Lines) — secondary format, build later if needed
- Encryption of backup files — ties into the encryption/vault feature
- Remote backup targets (S3, SSH) — application-level concern, not database
