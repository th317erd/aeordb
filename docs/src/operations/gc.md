# Garbage Collection

AeorDB is an append-only database: writes, overwrites, and deletes all append new entries without modifying existing ones. Over time, this leaves orphaned entries -- old file versions, deleted content, stale directory indexes -- consuming disk space. Garbage collection (GC) reclaims that space.

## What GC Does

GC uses a **mark-and-sweep** algorithm:

1. **Mark phase**: Starting from HEAD, all snapshots, and all forks, GC walks every reachable directory tree. It marks every entry that is still live: directory indexes, file records, content chunks, system tables, task queue records, and deletion records.

2. **Sweep phase**: GC iterates all entries in the key-value store. Any entry whose hash is not in the live set is garbage. Non-live entries are overwritten in-place with DeletionRecord or Void entries, then removed from the KV index.

### What Gets Collected

- Old file versions that have been overwritten
- Content chunks no longer referenced by any file record
- Directory indexes from previous tree states
- Entries orphaned by deletes

### What Does NOT Get Collected

- HEAD and all reachable entries from HEAD
- All snapshot root trees and their descendants
- All fork root trees and their descendants
- System table entries (`/.system`, `/.config`)
- Task queue records (registry + individual task entries)
- DeletionRecord entries (needed for KV rebuild from `.aeordb` scan)

## Running GC

### CLI

```bash
# Run GC
aeordb gc --database data.aeordb

# Dry run -- report what would be collected without deleting
aeordb gc --database data.aeordb --dry-run
```

Example output:
```
AeorDB Garbage Collection
Database: data.aeordb

Versions scanned: 3
Live entries:     1247
Garbage entries:  89
Reclaimed:        1.2 MB
Duration:         0.3s
```

Dry run output:
```
AeorDB Garbage Collection [DRY RUN]
Database: data.aeordb

[DRY RUN] Would collect 89 garbage entries (1.2 MB)
```

### HTTP API

**Synchronous GC (blocks until complete):**

```bash
# Run GC
curl -X POST http://localhost:6830/system/gc \
  -H "Authorization: Bearer $API_KEY"

# Dry run
curl -X POST http://localhost:6830/system/gc?dry_run=true \
  -H "Authorization: Bearer $API_KEY"
```

Response:
```json
{
  "versions_scanned": 3,
  "live_entries": 1247,
  "garbage_entries": 89,
  "reclaimed_bytes": 1258291,
  "duration_ms": 312,
  "dry_run": false
}
```

**Background GC (returns immediately, runs as a task):**

```bash
curl -X POST http://localhost:6830/system/tasks/gc \
  -H "Authorization: Bearer $API_KEY"
```

This enqueues a GC task that the background task worker will pick up. Track its progress via the [task system](tasks.md).

## When to Run GC

- **After bulk deletes**: If you delete a large number of files, their content chunks become garbage.
- **After bulk overwrites**: Updating many files leaves old versions behind.
- **After version cleanup**: If you delete old snapshots, the entries they exclusively referenced become garbage.
- **Periodically**: Set up a [cron schedule](tasks.md) for automatic GC.

Example cron configuration (`/.config/cron.json`):
```json
{
  "schedules": [
    {
      "id": "nightly-gc",
      "task_type": "gc",
      "schedule": "0 3 * * *",
      "args": {},
      "enabled": true
    }
  ]
}
```

## Concurrency and Safety

GC should not be run concurrently with writes. The sweep phase re-verifies each candidate against the current KV state before overwriting to mitigate races, but for full safety, callers should ensure exclusive access during GC.

**Crash safety**: If the process crashes mid-sweep, the `.aeordb` file may contain partially overwritten entries. On restart, the `.kv` index file will be stale and must be deleted to trigger a full rebuild from the `.aeordb` file scan. The rebuild replays deletion records and reconstructs the index, so no committed data is lost. Garbage entries that were not yet swept will persist until the next GC run.

## Performance

At scale, expect approximately 10K entries/sec sweep throughput. The mark phase is faster since it only walks reachable trees. The sweep phase writes are batched with a single sync at the end for performance.

## Async Index Cleanup

When files are deleted, their index entries are cleaned up asynchronously in the background. Deletions are debounced with a 50ms timeout and batched up to 100 paths per batch. This means index cleanup does not block the delete response, and rapid successive deletes are coalesced into efficient batch operations.

## See Also

- [Task System & Cron](tasks.md) -- background task execution and scheduling
- [Reindexing](reindex.md) -- rebuilding indexes after config changes
- [Backup & Restore](backup.md) -- exporting clean versions without garbage
