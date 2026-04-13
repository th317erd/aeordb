# Reindexing

When you change a table's index configuration (`indexes.json`), existing files need to be re-processed through the indexing pipeline. AeorDB handles this through background reindex tasks.

## Why Reindex

- You added a new index field (e.g., adding a `fulltext` index on a field that was previously unindexed)
- You changed the index type for a field (e.g., switching from `exact` to `fulltext`)
- You added or changed a parser plugin, and existing files need to be re-parsed
- You modified index settings (e.g., changing similarity thresholds)

## Automatic Reindexing

Changing `indexes.json` via the API automatically triggers a background reindex task for the affected directory. You do not need to manually trigger reindexing in most cases.

## Manual Reindexing

### HTTP API

```bash
curl -X POST http://localhost:3000/admin/tasks/reindex \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"path": "/data/"}'
```

The `path` argument specifies which directory to reindex. The task worker will:

1. Read the `indexes.json` configuration for that path
2. List all file entries in the directory
3. Re-read each file and run it through the indexing pipeline
4. Track progress and update checkpoints

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
curl http://localhost:3000/admin/tasks \
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

Reindex tasks save a checkpoint after each batch (50 files). If the server crashes or the task is cancelled and restarted, it resumes from the last checkpoint rather than starting over.

The checkpoint is the name of the last successfully processed file (files are processed in alphabetical order for deterministic ordering).

## Cancellation

Cancel a running reindex task:

```bash
curl -X POST http://localhost:3000/admin/tasks/{task_id}/cancel \
  -H "Authorization: Bearer $API_KEY"
```

The task checks for cancellation after each batch, so it will stop within one batch cycle.

## Batch Processing

Files are processed in batches of 50. After each batch, the task:

1. Updates the checkpoint to the last file in the batch
2. Computes progress percentage and ETA (using a rolling average of the last 10 batch times)
3. Checks for cancellation

## See Also

- [Task System & Cron](tasks.md) -- task lifecycle, listing, and scheduling
- [Garbage Collection](gc.md) -- reclaiming space from orphaned entries
