# Reindexing

When you change a table's index configuration (`indexes.json`), existing files need to be re-processed through the indexing pipeline. AeorDB handles this through background reindex tasks.

## Why Reindex

- You added a new index field (e.g., adding a `fulltext` index on a field that was previously unindexed)
- You changed the index type for a field (e.g., switching from `exact` to `fulltext`)
- You added or changed a parser plugin, and existing files need to be re-parsed
- You modified index settings (e.g., changing similarity thresholds)

## Automatic Reindexing

Changing `indexes.json` via the API automatically triggers a background reindex task for the affected directory. You do not need to manually trigger reindexing in most cases.

If every configured field is a virtual metadata field (`@filename`, `@hash`, `@size`, and so on), the automatic task uses metadata-only reindexing. That path reads FileRecord metadata only and does not read or parse file bodies. Mixed configs that include content fields still use the full parser/content indexing path.

## Manual Reindexing

### HTTP API

```bash
curl -X POST http://localhost:6830/system/tasks/reindex \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"path": "/data/", "metadata_only": true}'
```

The `path` argument specifies which directory to reindex. Manual API reindexing defaults to `force: false`, which means index-only reprocessing. Pass `"force": true` only when you deliberately want to migrate older FileRecord payloads while reindexing.

Set `"metadata_only": true` when you only need to rebuild virtual `@` metadata indexes. This skips full file reads, JSON parsing, and parser plugins. Content fields in the config are ignored in metadata-only mode.

Optional flush controls:

| Field | Default | Description |
|-------|---------|-------------|
| `index_flush_writes` | `262144` | Flush buffered index mutations after this many field/strategy updates |
| `index_flush_ms` | `30000` | Flush buffered index mutations after this many milliseconds |

The task worker will:

1. Read the `indexes.json` configuration for that path
2. List all file entries in the directory, or when `force` is true, scan current live FileRecord path keys in the requested subtree
3. If `force` is true, rewrite any older FileRecord payloads to the current version before indexing
4. Rebuild indexes through either the metadata-only path or the full parser/content pipeline
5. Buffer index writes in memory and flush them by write count, elapsed time, or final completion
6. Track progress and update checkpoints

Automatic reindexing triggered by `indexes.json` changes is index-only. Forced reindexing is the deliberate migration path: if a FileRecord is older than the current payload version, AeorDB rewrites the path, identity, and current content-addressed FileRecord entries using the current writer while preserving the file's timestamps, metadata, chunks, and parent directory entry. For FileRecord v0, this backfills the stored whole-file `content_hash` used by `@hash`.

Forced migration includes live path-key records under internal system/config paths such as `/.aeordb-system` and `/.aeordb-config`. Those records can be migrated, but internal/system files are still skipped by the indexing pipeline.

### Glob-Aware Reindexing

When the index config includes a `glob` field, the reindex task uses recursive directory listing instead of direct children only. Files are filtered by the glob pattern before processing.

For example, a config at `/sessions/` with `"glob": "*/session.json"` will recursively list all files under `/sessions/`, filter to those matching `*/session.json`, and reindex each one.

## Progress Tracking

During an active reindex, query responses include a `meta.reindexing` field indicating that results may be incomplete:

```json
{
  "results": [...],
  "meta": {
    "reindexing": true
  }
}
```

You can also check progress through the task system:

```bash
curl http://localhost:6830/system/tasks \
  -H "Authorization: Bearer $API_KEY"
```

The response includes progress details:

```json
{
  "tasks": [
    {
      "id": "abc123",
      "task_type": "reindex",
      "status": "running",
      "progress": {
        "task_id": "abc123",
        "task_type": "reindex",
        "progress": 0.45,
        "eta_ms": 12000,
        "indexed_count": 450,
        "total_count": 1000,
        "message": "indexed 450/1000 files"
      }
    }
  ]
}
```

## Circuit Breaker

If 10 consecutive files fail to index, the reindex task trips a circuit breaker and fails with an error:

```
circuit breaker: 10 consecutive indexing failures
```

This prevents runaway error loops when the index configuration or parser is fundamentally broken. Fix the underlying issue and trigger a new reindex.

## Checkpoint and Resume

Reindex tasks save checkpoints as processed work becomes durable. Because index writes are buffered in memory during reindexing, AeorDB only advances the checkpoint past buffered index mutations after those mutations have been flushed to storage. If the server crashes before a buffer flush, the resumed task may repeat some already-scanned files, but it will not skip unflushed index updates.

The checkpoint is the name of the last successfully processed file (files are processed in alphabetical order for deterministic ordering).

## Cancellation

Cancel a running reindex task:

```bash
curl -X POST http://localhost:6830/system/tasks/{task_id}/cancel \
  -H "Authorization: Bearer $API_KEY"
```

The task checks for cancellation after each batch, so it will stop within one batch cycle.

## Batch Processing

Files are processed in batches of 50. Index file updates are cached in memory and flushed after 262,144 index mutations or 30 seconds by default, plus one final flush at completion. This avoids rewriting the full on-disk index file after every file/field update during large reindexes.

After each batch, the task:

1. Advances the checkpoint when all prior index mutations are durable
2. Computes progress percentage and ETA (using a rolling average of the last 10 batch times)
3. Checks for cancellation

## See Also

- [Task System & Cron](tasks.md) -- task lifecycle, listing, and scheduling
- [Garbage Collection](gc.md) -- reclaiming space from orphaned entries
