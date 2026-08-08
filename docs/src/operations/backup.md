# Backup & Restore

AeorDB supports exporting database versions as self-contained `.aeordb` files, creating incremental patches between versions, importing backups, and promoting version hashes.

## Concepts

- **Full export**: A clean `.aeordb` file containing only the live entries at a specific version. No voids, no deletion records, no stale overwrites, no history.
- **Patch (diff)**: A `.aeordb` file containing only the changeset between two versions -- new/changed chunks, updated file records, updated directory indexes, and deletion records for removed files.
- **Import**: Applying an export or patch into a target database.
- **Promote**: Setting a version hash as the current HEAD.

## Privileged Backup (`--root-key`)

By default, `aeordb export` applies the data-export policy: ordinary user data
and namespace permission files are included, while protected operational
families, conflicts, logs, and derived indexes are omitted. The export stops at
the specified version (HEAD or one named snapshot).

Modern databases keep protected system trees detached from the user-data
root. When exporting an older snapshot whose root still names
`/.aeordb-system` or `/.aeordb-config`, a user-only export removes those root
relationships and writes a normalized root. In that compatibility case, the
reported `Version` differs from the requested source hash. The reported hash
is authoritative: it is also stored as the export's base hash, target hash,
and `HEAD`, so importing and promoting the artifact never creates a dangling
relationship. An export preserves its source root hash only when policy leaves
the reachable tree unchanged; filtering any child rebuilds parent closure and
produces a new authoritative root.

When you supply the source database's **root API key**, the CLI unlocks
registry-governed logical backup mode:

- **Portable required state** is included, including users, groups, central and
  namespace permissions, conflicts, portable configuration, plugin state, and
  other families selected by the embedded SystemFamily registry.
- **All named snapshots** are walked, not just HEAD. The exported `.aeordb` carries the full snapshot history.
- **Credentials and secrets are NEVER exported**, regardless of the key. This
  includes API keys, refresh tokens, magic links, system signing material, and
  email credentials. Node-local controls, logs, GC state, and derived indexes
  are also omitted or rebuilt according to registry policy.

The registry is the authority for each family. Backup traversal classifies a
path before reading its body, rejects unknown protected families, and rebuilds
directory indexes when omitted children would otherwise remain reachable.

Supply the key via flag or environment variable:

```bash
# Flag
aeordb export -D source.aeordb -o backup.aeordb \
  --root-key aeor_k_…

# Environment variable
AEORDB_ROOT_KEY=aeor_k_… aeordb export -D source.aeordb -o backup.aeordb
```

When importing a backup that contains protected portable state, the target
database's root key must be provided the same way. This proves ownership of the
destination before those records are merged.

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
curl -X POST http://localhost:6830/versions/export \
  -H "Authorization: Bearer $API_KEY" \
  --output backup.aeordb
```

With a snapshot:
```bash
curl -X POST "http://localhost:6830/versions/export?snapshot=v1" \
  -H "Authorization: Bearer $API_KEY" \
  --output backup-v1.aeordb
```

> **Note:** HTTP exports never include system data or other snapshots — that's CLI-only with `--root-key`. The HTTP endpoint is for sharing a single version's user data.

HTTP export bodies are streamed from a temporary file beside the source
database. The server does not materialize the complete `.aeordb` artifact in
memory or stage it in the operating system's temporary directory. Dropping the
response releases the stream reservation and temporary file. Keep enough free
space on the database filesystem for the generated archive while it is being
downloaded.

### Output

```
Export complete.
  Files: 142
  Chunks: 89
  Directories: 23
  Version: abc123def456...
```

Use the returned `Version` for later promotion or identity checks. It is the
root contained in the artifact and may be a normalized replacement for a
legacy source root as described above.

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
Both roots must exist and pass strict traversal. A made-up or collected hash is
reported as corruption/missing state; it is never interpreted as an empty
version.

### HTTP API

```bash
curl -X POST "http://localhost:6830/versions/diff?from=v1&to=v2" \
  -H "Authorization: Bearer $API_KEY" \
  --output patch.aeordb
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
- **Selected directory closures**: Complete base and target directory routing metadata used to prove both logical roots

Patch production applies the `LogicalBackup` SystemFamily policy before it
compares leaves or writes either directory closure. Credentials, secrets,
node-local controls, logs, GC state, and derived indexes are not copied into
the patch, and unknown protected paths are rejected before an artifact is
created. A change confined to omitted families returns `No changes visible
under logical-backup policy`.

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

# Import a privileged backup that contains system data (users, groups,
# snapshots). Required when the backup was made with --root-key.
aeordb import --database data.aeordb --file backup.aeordb \
  --root-key aeor_k_…
```

**Flags:**
- `--promote`: Automatically set HEAD to the imported version
- `--force`: Skip base version verification for patches
- `--root-key <key>` (or `AEORDB_ROOT_KEY` env var): Required when the
  backup contains system data. The key must be the **target** database's
  root key — proving you own where the data is going.

When the backup contains system data and no root key is provided, the
import succeeds but skips the system entries (a warning is printed).

The CLI determines whether privileged import is required from decoded record
paths and the embedded SystemFamily policy, not from the legacy `FLAG_SYSTEM`
header bit. A missing or forged flag therefore cannot move portable protected
state through the ordinary data-import path. Even with a valid root key,
credentials, secrets, node-local controls, logs, GC state, and derived indexes
remain omitted according to import policy.

### HTTP API

```bash
curl -X POST http://localhost:6830/versions/import \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/octet-stream" \
  --data-binary @backup.aeordb
```

Use `?promote=true`, `?force=true`, and `?mode=restore` as needed. Uploads are
streamed to a temporary file beside the target database with a 10 GiB transfer
limit. AeorDB rejects an oversized declared `Content-Length` before creating an
artifact and independently counts streamed bytes, so chunked uploads cannot
bypass the limit. Before the first target write, AeorDB validates the artifact
type and every inventoried entry body and checksum, then admits its inventory
and transfer workspace under the process memory coordinator. Full exports also
walk HEAD and every imported snapshot through the selected data-import policy
before mutation. Full import rebuilds directory closure when policy omits a
child and remaps snapshot roots to that rebuilt closure. Sparse patch import
validates the advertised target as a patch-first, target-fallback overlay,
rejects contradictory or malformed deletion records, applies only
policy-selected deletions, and publishes the rebuilt selected root. Both forms
reject unknown protected, structurally invalid, or embedded-path-mismatched
leaves before the first target write. The temporary file is removed together
with its lock sidecar on success, refusal, malformed input, cancellation, or
handler failure.

Immutable chunks and entity bodies are staged independently from namespace
authority. Current path locators are replaced through hard-acknowledged batches
bounded to 256 locators or 8 MiB of retained state; an individually larger
record is handled alone. Snapshot trees are copied as historical content and
cannot overwrite current HEAD locators. Because ordinary reads resolve through
HEAD, importing without `--promote` does not change the visible namespace.
An already-occupied immutable hash is accepted as deduplicated content only
when it resolves through an exact verified entry of the required type. A chunk,
FileRecord, symlink, directory, B-tree node, or snapshot collision with another
entity type is corruption and aborts import before HEAD promotion.
When filtering requires a B-tree rebuild, AeorDB preflights every planned node
before writing the batch. Existing nodes deduplicate only when their type,
version, key, and bytes match exactly; one conflict leaves every other new node
unwritten.

Unless `--force` is present, a promoted import also compares HEAD at final
publication with the root captured when import began. A concurrent acknowledged
write therefore causes a conflict instead of being silently overwritten. A
root-changing `imports_completed` event is emitted only after the durability
frontier and carries the namespace operation ID and publication sequence.
Live namespace counters are reconciled from the newly selected HEAD after that
same acknowledgement. An unpromoted import does not change live file,
directory, symlink, or logical-byte counts.

For full exports, `entries_imported` counts selected logical objects across
HEAD and each imported snapshot. `chunks_imported` includes only chunk payloads
that were not already present; file and directory counts include each selected
root in which the object was processed. Import write metrics count these
logical mutations, with newly copied chunk payload bytes counted once.

### Patch Base Version Check

When importing a patch, AeorDB verifies that the target database's current HEAD
is either the exact patch base or semantically equivalent to its selected base
closure. Otherwise, import fails:

```
Target database HEAD (aaa111...) does not match patch base version (bbb222...).
Use --force to apply anyway.
```

Use `--force` to skip this check if you know what you're doing.
`--force` does not make a patch self-contained: unchanged entries referenced by
its target root must still be available from the target database. Missing or
corrupt fallback entries remain hard failures.

The `version_hash` returned by current patch import is the selected target root
rebuilt in the target. Legacy patches whose headers contain an unfiltered root
can produce a different selected result after current policy omission. Sparse
import counts only newly copied chunks, changed path objects, rebuilt
directories, and applied deletions; unchanged overlay entries are not rewritten
or counted. Each applied deletion writes durable replay evidence before its
path alias is tombstoned, so restart and KV rebuild preserve the deletion. Its
acknowledgement records the entity identity selected by the starting root, not
the mutable path-key hash. If HEAD already omits the path but a stale live
locator remains, import validates and retires that derived locator using its
decoded entity identity.

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

The command verifies that the hash exists and resolves to a `DirectoryIndex`
root before promoting. The root must be structurally valid, physically
verified, and stored under its canonical content hash. The HEAD change is one
hard-acknowledged namespace transition; arbitrary path locators, malformed
directories, files, chunks, symlinks, or metadata hashes are rejected.

### HTTP API

```bash
curl -X POST http://localhost:6830/versions/promote \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"hash": "abc123def456..."}'
```

## Typical Workflows

### Regular Backups

```bash
# Create a snapshot first
curl -X POST http://localhost:6830/versions/snapshots \
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

## Automated Backup Scheduling

Use the `"backup"` task type with the cron scheduler to run automated backups on a schedule. The backup task exports HEAD (or a named snapshot) as a timestamped `.aeordb` file and optionally enforces a retention policy.

### Cron Configuration

```json
{
  "schedules": [
    {
      "id": "nightly-backup",
      "task_type": "backup",
      "schedule": "0 1 * * *",
      "args": {
        "backup_dir": "/var/backups/aeordb/",
        "retention_count": 7
      },
      "enabled": true
    }
  ]
}
```

### Backup Task Arguments

| Argument | Type | Default | Description |
|----------|------|---------|-------------|
| `backup_dir` | string | `"./backups/"` | Destination directory for backup files |
| `retention_count` | integer | `0` | Keep at most this many `.aeordb` files in the backup directory. `0` means unlimited. |
| `snapshot` | string | -- | Export a named snapshot instead of HEAD |

The task creates a timestamped filename (e.g., `backup-head-20260419T030000.000Z.aeordb`) to avoid collisions. When `retention_count` is set, the oldest `.aeordb` files in `backup_dir` are deleted after each backup to stay within the limit.

### Example: Weekly Backups with 4-Week Retention

```bash
curl -X POST http://localhost:6830/system/cron \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "weekly-backup",
    "schedule": "0 2 * * 0",
    "task_type": "backup",
    "args": {
      "backup_dir": "/var/backups/aeordb/",
      "retention_count": 4
    },
    "enabled": true
  }'
```

## See Also

- [CLI Commands](../cli/commands.md) -- full command reference with all flags
- [Garbage Collection](gc.md) -- clean up orphaned entries after imports
