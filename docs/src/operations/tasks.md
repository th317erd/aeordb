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
4. **Failed**: Task encountered an error (e.g., an exact partial reindex result, circuit breaker, or GC failure).
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
        "message": "processed 650/1000 files, completed 650, failed 0, migrated 0"
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

**Credential Cleanup:**
```bash
curl -X POST http://localhost:6830/system/tasks/cleanup \
  -H "Authorization: Bearer $API_KEY"
```

The root-only cleanup endpoint runs inline and returns exact acknowledged
deletion counts:

```json
{
  "tokens_cleaned": 14,
  "links_cleaned": 3
}
```

Scheduled cleanup tasks use the same engine operation. AeorDB strictly scans
the refresh-token and magic-link authorities without constructing a complete
directory listing, bounds each persisted credential body to 1 MiB, and retains
at most 128 deletion candidates at a time. Each candidate is deleted only when
its immutable FileRecord identity still matches the record observed during the
scan. A concurrently refreshed, replaced, or already-removed credential is a
safe no-op rather than a stale deletion.

Each non-empty batch is one `maintenance_repair` durability acknowledgement,
one `entries_deleted` SSE event containing the exact deleted paths for root
subscribers, and one logical write metric. The SSE adapter projects batch
members per subscriber and never serializes protected credential paths to
non-root callers. Cleanup counters advance only after that acknowledgement.
Empty or entirely changed batches publish nothing. If a later scan or batch
fails after earlier batches were acknowledged, the operation fails with an
exact partial result containing the acknowledged token/link counts and bounded
failure evidence; it does not report the completed prefix as either total
success or total failure. Maintenance-memory pressure refuses before mutation
and the HTTP endpoint returns retryable `503 SERVICE_UNAVAILABLE` rather than
misclassifying pressure as an internal server error.

### Cancel a Task

```bash
curl -X POST http://localhost:6830/system/tasks/{task_id}/cancel \
  -H "Authorization: Bearer $API_KEY"
```

Cancellation is cooperative. Long-running reindex, GC, and backup operations
check cancellation at bounded safe points. Credential cleanup checks before
each batch publication. Tasks finish any durability-critical publication
already in progress before stopping. A cancelled running task is not later
overwritten as completed or failed by a stale worker transition.

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
Reindex failures include exact completed/failed counts with bounded diagnostic
samples. Their checkpoint never advances beyond the first incomplete path,
even when later files finish successfully before the task becomes terminal or
is deferred. A later index-flush or checkpoint-publication failure retains
those earlier partial results instead of replacing them with only the final
authority error.

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

AeorDB includes a built-in cron scheduler that checks the engine-owned
`/.aeordb-config/cron.json` authority every 60 seconds and enqueues matching
tasks. Manage it through the root-only `/system/cron` API; generic file and
blob routes cannot access this protected path.

### Configuration

The stored document has this shape:

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

**Create a schedule:**
```bash
curl -X POST http://localhost:6830/system/cron \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "nightly-gc",
    "task_type": "gc",
    "schedule": "0 3 * * *",
    "args": {},
    "enabled": true
  }'
```

**Read the schedule:**
```bash
curl http://localhost:6830/system/cron \
  -H "Authorization: Bearer $API_KEY"
```

**Disable a schedule:**
```bash
curl -X PATCH http://localhost:6830/system/cron/nightly-gc \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"enabled":false}'
```

Create, update, and delete are atomic read-modify-write operations under the
same namespace authority as the stored file. Concurrent requests cannot discard
one another's schedules. The complete document is limited to 1 MiB and every
cron expression is validated before publication. Malformed, unreadable,
duplicate-ID, or oversized stored authority is reported as an error rather than
treated as an empty schedule.

### Deduplication

The cron scheduler checks whether a task with the same type and arguments is already pending or running before enqueuing. This prevents duplicate tasks from stacking up if a previous run hasn't finished.

Configuration and task-registry reads are strict. A failed scheduler tick is
logged as an error and retried at the next cadence; task-list failures never
become permission to enqueue duplicates, and enqueue failures are not discarded.
Tasks durably enqueued before a later failure remain visible and are deduplicated
on retry.

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

The task system emits events on the event bus. These events are administrative
and are delivered through `/system/events` only to root subscribers; task
arguments, summaries, and failures can contain paths or operational details.

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
