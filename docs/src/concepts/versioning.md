# Versioning

AeorDB's content-addressed Merkle tree makes versioning a structural property of the storage engine rather than an add-on feature. Every write creates new hashes up the directory tree, so every committed state is already a snapshot by definition -- you just need to save a pointer to the root hash.

## How Versioning Works

The database state at any point in time is fully described by its root hash (HEAD). HEAD is the hash of the root DirectoryIndex, which contains hashes of its children, which contain hashes of their children, all the way down to the chunks of individual files.

```
HEAD -> root DirectoryIndex
         |
         +-- /users/ (DirectoryIndex)
         |     +-- alice.json (FileRecord -> [chunk_a, chunk_b])
         |     +-- bob.json   (FileRecord -> [chunk_c])
         |
         +-- /docs/ (DirectoryIndex)
               +-- readme.md  (FileRecord -> [chunk_d])
```

When you write `/users/alice.json`, the engine creates:
1. New chunks (if the content changed)
2. A new FileRecord with new chunk hashes
3. A new `/users/` DirectoryIndex with the updated child entry
4. A new root DirectoryIndex with the updated `/users/` child entry
5. HEAD now points to the new root hash

The old root hash still exists and still points to the old directory tree with the old file content. Nothing was overwritten -- new entries were appended.

## Snapshots

A snapshot is a named reference to a root hash. Creating a snapshot saves the current HEAD so you can return to it later.

### Create a Snapshot

```bash
curl -X POST http://localhost:3000/version/snapshot \
  -H "Content-Type: application/json" \
  -d '{"name": "v1.0"}'
```

### List Snapshots

```bash
curl http://localhost:3000/version/snapshots
```

Response:

```json
{
  "snapshots": [
    {"name": "v1.0", "root_hash": "a1b2c3...", "created_at": 1775968398000},
    {"name": "v2.0", "root_hash": "d4e5f6...", "created_at": 1775968500000}
  ]
}
```

### Restore a Snapshot

Restoring a snapshot sets HEAD back to the snapshot's root hash. The current state is not lost -- you can snapshot it before restoring if you want to preserve it.

```bash
curl -X POST http://localhost:3000/version/restore \
  -H "Content-Type: application/json" \
  -d '{"name": "v1.0"}'
```

### Delete a Snapshot

```bash
curl -X DELETE http://localhost:3000/version/snapshot/v1.0
```

After deleting a snapshot, entries that were only reachable through that snapshot become eligible for garbage collection.

## Forks

Forks are isolated branches of the database. Writes to a fork do not affect HEAD or other forks. This is useful for testing changes, running experiments, or staging updates before promoting them.

### Create a Fork

```bash
curl -X POST http://localhost:3000/version/fork \
  -H "Content-Type: application/json" \
  -d '{"name": "experiment"}'
```

### List Forks

```bash
curl http://localhost:3000/version/forks
```

### Promote a Fork

When you're satisfied with the changes in a fork, promote it to HEAD:

```bash
curl -X POST http://localhost:3000/version/fork/experiment/promote
```

### Abandon a Fork

```bash
curl -X DELETE http://localhost:3000/version/fork/experiment
```

## Tree Walking

The content-addressed Merkle tree enables historical reads. When you walk a snapshot's directory tree:

1. Start from the snapshot's root hash
2. Load the root DirectoryIndex -- each child entry has a hash
3. Follow child hashes to subdirectories or files
4. Each FileRecord's `ChildEntry.hash` points to a content-addressed (immutable) key

Because file content keys are immutable, walking a snapshot's tree always resolves to the data as it existed when the snapshot was taken, even if the files have been overwritten or deleted since then.

```
Snapshot "v1.0" root_hash: aaa111...
  -> /users/ dir_hash: bbb222...
     -> alice.json content_key: ccc333...  (resolves to Alice's v1.0 data)

Current HEAD root_hash: ddd444...
  -> /users/ dir_hash: eee555...
     -> alice.json content_key: fff666...  (resolves to Alice's current data)
```

Both trees can coexist because they share unchanged chunks and directories (structural sharing). Only the parts that differ consume additional storage.

## Export and Import

AeorDB can export a version as a self-contained `.aeordb` file and import it into another database.

### Export

Export creates a clean, compacted database from a single version -- no history, no voids, no deleted entries:

```bash
# Export current HEAD
aeordb export --database data.aeordb --output backup.aeordb

# Export a specific snapshot
aeordb export --database data.aeordb --snapshot v1.0 --output v1.aeordb

# Export via HTTP
curl -X POST http://localhost:3000/admin/export --output backup.aeordb
curl -X POST "http://localhost:3000/admin/export?snapshot=v1.0" --output v1.aeordb
```

The exported file is a fully functional database that can be opened with `aeordb start`.

### Import

Import applies an export or patch file to a target database:

```bash
# Import without promoting HEAD (inspect first)
aeordb import --database target.aeordb --file backup.aeordb

# Import and promote in one step
aeordb import --database target.aeordb --file backup.aeordb --promote

# Import via HTTP
curl -X POST http://localhost:3000/admin/import \
  -H "Content-Type: application/octet-stream" \
  --data-binary @backup.aeordb
```

Import does NOT automatically change HEAD. The imported version exists in the database and can be promoted explicitly when ready.

## Diff and Patch

Diff extracts only the changeset between two versions. The output is a patch file -- not a standalone database.

```bash
# Diff between two snapshots
aeordb diff --database data.aeordb --from v1.0 --to v2.0 --output patch.aeordb

# Diff from a snapshot to current HEAD
aeordb diff --database data.aeordb --from v1.0 --output patch.aeordb

# Diff via HTTP
curl -X POST "http://localhost:3000/admin/diff?from=v1.0&to=v2.0" --output patch.aeordb
```

A patch file contains only new/changed chunks, updated FileRecords, and deletion markers. Chunks shared between the two versions are not included, making patches much smaller than full exports.

### Applying a Patch

```bash
# Apply patch (strict base version check)
aeordb import --database target.aeordb --file patch.aeordb

# Skip base version check
aeordb import --database target.aeordb --file patch.aeordb --force

# Apply and promote
aeordb import --database target.aeordb --file patch.aeordb --promote
```

If the target database's HEAD does not match the patch's base version, the import fails unless `--force` is used.

## Promote

Promote sets HEAD to a specific version hash. This is separate from import so you can inspect the imported data before committing to it:

```bash
# CLI
aeordb promote --database data.aeordb --hash f6e5d4c3...

# HTTP
curl -X POST "http://localhost:3000/admin/promote" \
  -H "Content-Type: application/json" \
  -d '{"hash": "f6e5d4c3..."}'
```

## Next Steps

- [Storage Engine](./storage-engine.md) -- how the Merkle tree and content addressing work at the byte level
- [Architecture](./architecture.md) -- system overview and crash recovery
