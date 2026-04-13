# Task System & Cron

AeorDB runs long-running operations (reindexing, garbage collection) as background tasks. Tasks are managed by a task queue, executed by a dedicated worker, and can be triggered manually or on a cron schedule.

## Built-in Task Types

| Task Type | Description |
|-----------|-------------|
| `reindex` | Re-run the indexing pipeline on all files under a directory |
| `gc` | Run garbage collection (mark-and-sweep) |

## Task Lifecycle

```
pending  -->  running  -->  completed
                       -->  failed
                       -->  cancelled
```

1. **Pending**: Task is enqueued and waiting for the worker to pick it up.
2. **Running**: Worker has dequeued the task and is executing it.
3. **Completed**: Task finished successfully.
4. **Failed**: Task encountered an error (e.g., circuit breaker tripped, GC failed).
5. **Cancelled**: Task was cancelled by the user between batch iterations.

On server startup, any tasks left in `Running` state (from a previous crash) are reset to `Pending` so they can be re-executed.

## API

### List Tasks

```bash
curl http://localhost:3000/admin/tasks \
  -H "Authorization: Bearer $API_KEY"
```

Response:
```json
{
  "tasks": [
    {
      "id": "abc123",
      "task_type": "reindex",
      "status": "running",
      "args": {"path": "/data/"},
      "created_at": 1700000000000,
      "progress": {
        "task_id": "abc123",
        "task_type": "reindex",
        "progress": 0.65,
        "eta_ms": 8000,
        "indexed_count": 650,
        "total_count": 1000,
        "message": "indexed 650/1000 files"
      }
    },
    {
      "id": "def456",
      "task_type": "gc",
      "status": "completed",
      "args": {},
      "created_at": 1699999000000
    }
  ]
}
```

### Trigger a Task

**Reindex:**
```bash
curl -X POST http://localhost:3000/admin/tasks/reindex \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"path": "/data/"}'
```

**Garbage Collection:**
```bash
curl -X POST http://localhost:3000/admin/tasks/gc \
  -H "Authorization: Bearer $API_KEY"
```

### Cancel a Task

```bash
curl -X POST http://localhost:3000/admin/tasks/{task_id}/cancel \
  -H "Authorization: Bearer $API_KEY"
```

Cancellation is cooperative: the task checks for cancellation between batch iterations. It will not interrupt a batch in progress.

## Progress Tracking

Running tasks expose in-memory progress information:

| Field | Type | Description |
|-------|------|-------------|
| `task_id` | `String` | Task identifier |
| `task_type` | `String` | Task type (e.g., `"reindex"`) |
| `progress` | `f64` | Completion fraction (0.0 to 1.0) |
| `eta_ms` | `Option<i64>` | Estimated time remaining in milliseconds |
| `indexed_count` | `usize` | Number of items processed so far |
| `total_count` | `usize` | Total items to process |
| `message` | `Option<String>` | Human-readable progress message |

Progress is computed using a rolling average of the last 10 batch execution times for ETA calculation.

During an active reindex, query responses include `meta.reindexing: true` so clients know results may be incomplete.

## Cron Scheduling

AeorDB includes a built-in cron scheduler that checks `/.config/cron.json` every 60 seconds and enqueues matching tasks.

### Configuration

Store the cron configuration at `/.config/cron.json`:

```json
{
  "schedules": [
    {
      "id": "nightly-gc",
      "task_type": "gc",
      "schedule": "0 3 * * *",
      "args": {},
      "enabled": true
    },
    {
      "id": "hourly-reindex",
      "task_type": "reindex",
      "schedule": "0 * * * *",
      "args": {"path": "/data/"},
      "enabled": true
    }
  ]
}
```

### Cron Expression Format

Standard 5-field Unix cron expressions:

```
minute  hour  day-of-month  month  day-of-week
  *       *        *          *        *
```

Examples:
- `0 3 * * *` -- every day at 3:00 AM
- `*/15 * * * *` -- every 15 minutes
- `0 0 * * 0` -- every Sunday at midnight
- `30 2 1 * *` -- 2:30 AM on the 1st of every month

### Cron API

**Create/update the schedule:**
```bash
curl -X PUT http://localhost:3000/.config/cron.json \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "schedules": [
      {
        "id": "nightly-gc",
        "task_type": "gc",
        "schedule": "0 3 * * *",
        "args": {},
        "enabled": true
      }
    ]
  }'
```

**Read the schedule:**
```bash
curl http://localhost:3000/.config/cron.json \
  -H "Authorization: Bearer $API_KEY"
```

**Disable a schedule** (set `enabled: false` and re-upload):
```bash
# Fetch, modify, re-upload
```

### Deduplication

The cron scheduler checks whether a task with the same type and arguments is already pending or running before enqueuing. This prevents duplicate tasks from stacking up if a previous run hasn't finished.

### CronSchedule Fields

| Field | Type | Description |
|-------|------|-------------|
| `id` | `String` | Unique identifier for this schedule |
| `task_type` | `String` | Task type to enqueue (e.g., `"gc"`, `"reindex"`) |
| `schedule` | `String` | 5-field Unix cron expression |
| `args` | `serde_json::Value` | Arguments passed to the task |
| `enabled` | `bool` | Whether this schedule is active (default `true`) |

## Task Retention

Completed tasks are automatically pruned:
- Tasks older than 24 hours are removed
- At most 100 completed tasks are retained

Pruning runs after each task completes.

## Events

The task system emits events on the event bus:

| Event | Description |
|-------|-------------|
| `task.started` | A task has begun execution |
| `task.completed` | A task finished successfully |
| `task.failed` | A task encountered an error |
| `gc.completed` | GC-specific completion event with statistics |

## See Also

- [Garbage Collection](gc.md) -- details on the GC mark-and-sweep algorithm
- [Reindexing](reindex.md) -- details on the reindex process, circuit breaker, and checkpoints
