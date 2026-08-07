# Task System & Cron

AeorDB runs long-running operations (reindexing, garbage collection) as background tasks. Tasks are managed by a task queue, executed by a dedicated worker, and can be triggered manually or on a cron schedule.

## Built-in Task Types

| Task Type | Description |
|-----------|-------------|
| `reindex` | Re-run the indexing pipeline on all files under a directory |
| `gc` | Run garbage collection (mark-and-sweep) |
| `backup` | Export HEAD (or a named snapshot) as a timestamped `.aeordb` file |
| `cleanup` | Remove expired refresh tokens and used or expired magic links |

## Task Lifecycle

```
pending  -->  running  -->  completed
  ^             |      -->  failed
  |             |      -->  cancelled
  +-------------+
    deferred
```

1. **Pending**: Task is enqueued and waiting for the worker to pick it up.
2. **Running**: Worker has dequeued the task and is executing it.
3. **Completed**: Task finished successfully.
4. **Failed**: Task encountered an error (e.g., circuit breaker tripped, GC failed).
5. **Cancelled**: Task was cancelled by the user between batch iterations.
6. **Deferred**: Deferral is an event, not a persisted status. If host-memory
   pressure rises or the worker begins shutting down after claiming a task,
   the task returns to `Pending`, clears its execution timestamps and error,
   and preserves its last durable checkpoint for a later retry. Pressure
   deferrals set `retry_at` and increment `deferral_count`; shutdown requeues
   are immediately eligible.

On server startup, any tasks left in `Running` state (from a previous crash) are durably reset to `Pending` so they can be re-executed. A worker panic after dequeue also returns the task to `Pending` in the same process. Both paths preserve the last durable checkpoint. A malformed task registry, missing or identity-mismatched task record, or failed recovery write aborts startup instead of silently stranding work.

## API

### List Tasks

```bash
curl http://localhost:6830/system/tasks \
  -H "Authorization: Bearer $API_KEY"
```

Response:
```json
{
  "items": [
    {
      "id": "abc123",
      "task_type": "reindex",
      "status": "running",
      "args": {"path": "/data/"},
      "created_at": 1700000000000,
      "retry_at": null,
      "deferral_count": 0,
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
curl -X POST http://localhost:6830/system/tasks/reindex \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"path": "/data/", "metadata_only": true}'
```

Manual API reindexing defaults to `force: false`, which is index-only. Use `"force": true` to also migrate older FileRecord payloads to the current version before indexing.

Use `"metadata_only": true` to rebuild only virtual `@` metadata indexes without reading file bodies or invoking parsers. Reindex tasks buffer index writes in memory and flush them after `index_flush_writes` mutations or `index_flush_ms` milliseconds. Defaults are 262,144 mutations and 30,000 ms.

**Garbage Collection:**
```bash
curl -X POST http://localhost:6830/system/tasks/gc \
  -H "Authorization: Bearer $API_KEY"
```

### Cancel a Task

```bash
curl -X POST http://localhost:6830/system/tasks/{task_id}/cancel \
  -H "Authorization: Bearer $API_KEY"
```

Cancellation is cooperative. Long-running reindex, GC, and backup operations
check cancellation at bounded safe points; they finish any durability-critical
publication already in progress before stopping. A cancelled running task is
not later overwritten as completed or failed by a stale worker transition.

## Memory Pressure And Retry

Maintenance tasks are admitted through the process-wide memory coordinator.
Soft pressure, the configured host-available-memory floor, and ordinary hard
pressure keep unclaimed tasks in `Pending`. If pressure appears after dequeue,
the worker requeues the task and emits `tasks_deferred`. Reindex flushes its
dirty index buffer before saving the last completed path, so a retry may repeat
work but cannot skip an acknowledged index mutation.

Pressure-deferred tasks use a durable exponential retry delay starting at five
seconds and capped at five minutes. While `retry_at` is in the future, FIFO
selection leaves that task unchanged and may claim newer eligible work. This
prevents sustained pressure or a task larger than the configured maintenance
budget from rewriting the same task record and emitting SSE events every worker
poll. Legacy task records without retry fields remain immediately eligible.

FIFO selection preflights persisted task headers, reserves the encoded and
decoded task workspace, and scans one record at a time. Startup recovery and
history pruning use the same bounded reads instead of constructing a second
copy of the complete task queue. If optional history pruning is deferred by
memory pressure, task completion remains authoritative and pruning is retried
after a later task iteration; corruption and I/O failures still surface.

`ResourceExhausted`, worker shutdown, and non-user cancellation are retryable
deferrals. Invalid arguments, malformed persistent state, circuit-breaker
failures, and other operation errors remain terminal `failed` outcomes.

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
curl -X PUT http://localhost:6830/.config/cron.json \
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
curl http://localhost:6830/.config/cron.json \
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

Pruning runs after each task iteration when maintenance memory is available.

## Events

The task system emits events on the event bus:

| Event | Description |
|-------|-------------|
| `tasks_started` | A task has begun execution |
| `tasks_deferred` | A claimed task returned to `Pending`; payload includes `task_id`, `task_type`, `reason`, `retryable`, `retry_at`, `retry_after_ms`, and `deferral_count` |
| `tasks_completed` | A task finished successfully |
| `tasks_failed` | A task encountered an error |
| `tasks_cancelled` | A running task cancellation was observed by the worker |
| `gc_started` | GC has begun execution |
| `gc_completed` | GC-specific completion event with statistics |

## See Also

- [Garbage Collection](gc.md) -- details on the GC mark-and-sweep algorithm
- [Reindexing](reindex.md) -- details on the reindex process, circuit breaker, and checkpoints
