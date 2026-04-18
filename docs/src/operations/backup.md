# Backup & Restore

AeorDB supports exporting database versions as self-contained `.aeordb` files, creating incremental patches between versions, importing backups, and promoting version hashes.

## Concepts

- **Full export**: A clean `.aeordb` file containing only the live entries at a specific version. No voids, no deletion records, no stale overwrites, no history.
- **Patch (diff)**: A `.aeordb` file containing only the changeset between two versions -- new/changed chunks, updated file records, updated directory indexes, and deletion records for removed files.
- **Import**: Applying an export or patch into a target database.
- **Promote**: Setting a version hash as the current HEAD.

## Full Export

Export HEAD, a named snapshot, or a specific version hash as a self-contained backup.

### CLI

```bash
# Export HEAD
aeordb export --database data.aeordb --output backup.aeordb

# Export a named snapshot
aeordb export --database data.aeordb --output backup.aeordb --snapshot v1

# Export a specific version hash
aeordb export --database data.aeordb --output backup.aeordb --hash abc123def456...
```

The output file must not already exist -- the command will refuse to overwrite.

### HTTP API

```bash
curl -X POST http://localhost:3000/versions/export \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"output": "backup.aeordb"}'
```

With a snapshot:
```bash
curl -X POST http://localhost:3000/versions/export \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"output": "backup.aeordb", "snapshot": "v1"}'
```

### Output

```
Export complete.
  Files: 142
  Chunks: 89
  Directories: 23
  Version: abc123def456...
```

## Diff / Patch

Create an incremental patch containing only the changes between two versions. This is significantly smaller than a full export when only a few files have changed.

### CLI

```bash
# Diff between two snapshots
aeordb diff --database data.aeordb --output patch.aeordb --from v1 --to v2

# Diff from a snapshot to HEAD
aeordb diff --database data.aeordb --output patch.aeordb --from v1

# Diff using raw hashes
aeordb diff --database data.aeordb --output patch.aeordb --from abc123... --to def456...
```

The `--from` and `--to` arguments accept either snapshot names or hex-encoded version hashes. If `--to` is omitted, HEAD is used.

### HTTP API

```bash
curl -X POST http://localhost:3000/versions/diff \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"output": "patch.aeordb", "from": "v1", "to": "v2"}'
```

### Output

```
Patch created.
  Files added: 5
  Files modified: 12
  Files deleted: 3
  Chunks: 8
  Directories: 7
  From: abc123...
  To:   def456...
```

### What a Patch Contains

- **New chunks**: Content chunks that exist in the target version but not the base version
- **Added file records**: Files present in the target but not the base
- **Modified file records**: Files that changed between the two versions
- **Deletion records**: Files present in the base but removed in the target
- **Changed directory indexes**: Directory entries that differ between versions

## Import

Apply a full export or incremental patch to a target database.

### CLI

```bash
# Import a full export
aeordb import --database data.aeordb --file backup.aeordb

# Import and immediately promote HEAD
aeordb import --database data.aeordb --file backup.aeordb --promote

# Force import a patch even if base version doesn't match
aeordb import --database data.aeordb --file patch.aeordb --force
```

**Flags:**
- `--promote`: Automatically set HEAD to the imported version
- `--force`: Skip base version verification for patches

### HTTP API

```bash
curl -X POST http://localhost:3000/versions/import \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"file": "backup.aeordb", "promote": true}'
```

### Patch Base Version Check

When importing a patch, AeorDB verifies that the target database's current HEAD matches the patch's base version. If they don't match, the import fails:

```
Target database HEAD (aaa111...) does not match patch base version (bbb222...).
Use --force to apply anyway.
```

Use `--force` to skip this check if you know what you're doing.

### Output

```
Full export imported.
  Entries: 254
  Chunks: 89
  Files: 142
  Directories: 23
  Deletions: 0
  Version: abc123...

  HEAD has been promoted.
```

If `--promote` was not used:
```
  HEAD has NOT been changed.
  To promote: aeordb promote --hash abc123...
```

## Promote

Set a specific version hash as the current HEAD.

### CLI

```bash
aeordb promote --database data.aeordb --hash abc123def456...
```

The command verifies that the hash exists in the database before promoting.

### HTTP API

```bash
curl -X POST http://localhost:3000/versions/promote \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"hash": "abc123def456..."}'
```

## Typical Workflows

### Regular Backups

```bash
# Create a snapshot first
curl -X POST http://localhost:3000/versions/snapshots \
  -H "Authorization: Bearer $API_KEY" \
  -d '{"name": "daily-2024-01-15"}'

# Export it
aeordb export --database data.aeordb \
  --output backups/daily-2024-01-15.aeordb \
  --snapshot daily-2024-01-15
```

### Incremental Backups

```bash
# First backup: full export
aeordb export --database data.aeordb --output backups/full.aeordb --snapshot v1

# Subsequent backups: just the diff
aeordb diff --database data.aeordb --output backups/patch-v1-v2.aeordb --from v1 --to v2
```

### Restore from Backup

```bash
# Import the full backup
aeordb import --database restored.aeordb --file backups/full.aeordb --promote

# Apply incremental patches in order
aeordb import --database restored.aeordb --file backups/patch-v1-v2.aeordb --promote
```

### Migrate Between Servers

```bash
# On source server
aeordb export --database data.aeordb --output transfer.aeordb

# Copy to target server
scp transfer.aeordb target-server:/data/

# On target server
aeordb import --database data.aeordb --file transfer.aeordb --promote
```

## See Also

- [CLI Commands](../cli/commands.md) -- full command reference with all flags
- [Garbage Collection](gc.md) -- clean up orphaned entries after imports
