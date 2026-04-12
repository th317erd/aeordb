# Task System, Cron Scheduler & Reindex — Spec

**Date:** 2026-04-11
**Status:** Approved
**Priority:** High — no way to reindex existing files after config change

---

## 1. Overview

A background task system that runs jobs without blocking HTTP requests. Tasks are persistently stored (survive restart), with in-memory progress tracking. A cron scheduler triggers recurring tasks on a schedule. The first two built-in task types are **reindex** and **gc**.

Query responses include reindex progress metadata so clients know when results may be incomplete.

---

## 2. Task Queue

### Storage

Tasks are persisted in `/.system/tasks/` as JSON files via `SystemTables`. Each task:

```json
{
  "id": "uuid",
  "task_type": "reindex",
  "args": { "path": "/docs/" },
  "status": "pending",
  "created_at": 1775968398000,
  "started_at": null,
  "completed_at": null,
  "error": null
}
```

Statuses: `pending` → `running` → `completed` or `failed` or `cancelled`.

### In-Memory Progress

A shared `Arc<Mutex<TaskProgress>>` (or `Arc<RwLock<HashMap<String, ProgressInfo>>>`) tracks live progress for running tasks:

```rust
struct ProgressInfo {
    task_id: String,
    task_type: String,
    args: serde_json::Value,
    progress: f64,       // 0.0 to 1.0
    eta_ms: Option<i64>, // estimated completion timestamp (ms)
    message: Option<String>,
}
```

This is purely in-memory — not persisted. On restart, incomplete tasks are re-queued (status reset to `pending`).

The persistent task record also stores a **checkpoint** field for crash resumption:

```json
{
  "checkpoint": "/docs/file-04327.json"
}
```

On restart, the reindex task resumes from the checkpoint path instead of starting over. The checkpoint is updated every batch (persisted to `/.system/tasks/`).

### Worker Loop

A single background worker (`tokio::spawn`) processes tasks one at a time:

```
loop {
    1. Load pending tasks from /.system/tasks/ (sorted by created_at)
    2. Pick the oldest pending task
    3. Set status = "running", started_at = now
    4. Execute the task (spawn_blocking for engine work)
       - Task yields between batches (reindex N files, release, continue)
       - Updates in-memory progress after each batch
    5. On success: status = "completed", completed_at = now
    6. On failure: status = "failed", error = message
    7. Remove in-memory progress entry
    8. Sleep 1 second, repeat
}
```

The worker holds `Arc<StorageEngine>`, `Arc<PluginManager>`, `Arc<EventBus>`, and the shared progress state.

### Cooperative Yielding

The reindex task processes files in batches (e.g., 50 files per batch). Between batches, it releases the writer Mutex so pending HTTP requests can proceed. This prevents starvation:

```
for batch in file_batches(50) {
    for file in batch {
        read file → run indexing pipeline
    }
    update progress
    tokio::task::yield_now().await  // let HTTP handlers run
}
```

### Task Retention

Completed, failed, and cancelled tasks are automatically cleaned up to prevent unbounded growth in `/.system/tasks/`:

- **Max retention:** 7 days after completion/failure/cancellation
- **Max count:** 100 completed/failed/cancelled tasks (oldest removed first)
- The worker checks retention after each task completes and prunes stale records

Pending and running tasks are never pruned.

### ETA Calculation

ETA uses a **rolling average** of recent batch times rather than linear extrapolation from overall progress. This handles variable file sizes and parser costs more accurately:

```
rolling_window = last 10 batch durations
avg_batch_time = mean(rolling_window)
remaining_batches = (total_files - files_done) / batch_size
eta_ms = now + (remaining_batches * avg_batch_time)
```

---

## 3. Built-In Task Types

### reindex

**Trigger:** Automatic (on `indexes.json` change) or manual (`POST /admin/tasks/reindex`).

**Args:** `{ "path": "/docs/" }` — the directory to reindex.

**Execution:**
1. Read `{path}/.config/indexes.json` to get the current index config
2. List all files in the directory (non-recursive — only direct children)
3. If resuming from checkpoint, skip files alphabetically before the checkpoint path
4. For each file (in batches of 50):
   a. Read file content via `DirectoryOps::read_file`
   b. Get metadata for content_type
   c. Run `IndexingPipeline::run` with the file data
   d. If pipeline fails, increment consecutive failure counter
   e. Update progress: `files_done / total_files`
   f. Update checkpoint in persistent task record
5. Compute ETA using rolling average of last 10 batch durations

**Circuit breaker:** If 10 consecutive files fail to index (parser errors, corrupt data, etc.), the task pauses with status `failed` and error "circuit breaker: 10 consecutive indexing failures — possible parser bug." This prevents a broken parser from thrashing the server. The consecutive counter resets on any successful file.

**Auto-trigger:** When `store_file` detects that the path ends with `/.config/indexes.json`, it enqueues a reindex task for the parent directory. If a reindex for the same path is already running, the running task is **cancelled** and a new one is enqueued. This ensures the reindex always uses the latest config. The worker checks for cancellation between batches and stops gracefully.

**Cancellation:** Tasks can be cancelled via `DELETE /admin/tasks/{id}` or automatically when superseded by a new task for the same path. Cancelled status is `cancelled`. The worker checks a cancellation flag between batches — if set, it stops processing, sets status to `cancelled`, and moves on to the next task.

**Recursive:** For v1, reindex is non-recursive (only files directly in the configured directory). Subdirectories with their own `indexes.json` would need separate reindex tasks.

### gc

**Trigger:** Scheduled via cron or manual (`POST /admin/tasks/gc`).

**Args:** `{ "dry_run": false }` (optional).

**Execution:** Calls `run_gc(engine, ctx, dry_run)` — the existing GC implementation. Progress tracking is coarser (mark phase = 0.0-0.5, sweep phase = 0.5-1.0).

---

## 4. Cron Scheduler

### Config File

`/.config/cron.json`:

```json
{
  "schedules": [
    {
      "id": "weekly-gc",
      "task_type": "gc",
      "schedule": "0 3 * * 0",
      "args": {},
      "enabled": true
    },
    {
      "id": "nightly-reindex",
      "task_type": "reindex",
      "schedule": "0 2 * * *",
      "args": { "path": "/data/" },
      "enabled": true
    }
  ]
}
```

Standard 5-field cron: `minute hour day_of_month month day_of_week`.

### Scheduler Loop

A tokio task that runs every 60 seconds:

```
loop {
    sleep 60 seconds
    for each enabled schedule:
        if cron_matches(schedule.schedule, now):
            enqueue task (with dedup check)
}
```

Uses the `cron` crate for parsing and matching.

### HTTP API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/admin/cron` | GET | List all schedules |
| `/admin/cron` | POST | Create a new schedule |
| `/admin/cron/{id}` | DELETE | Remove a schedule |
| `/admin/cron/{id}` | PATCH | Update a schedule (enable/disable, change schedule) |

Schedules created via API are **immediately persisted** to `/.config/cron.json` via `DirectoryOps::store_file` (merged with any existing config). The in-memory schedule list is also updated immediately. Changes take effect on the next scheduler tick (within 60 seconds). This means cron changes survive restart — the API modifies the same config file that the startup loader reads.

All cron endpoints require admin auth (root user).

---

## 5. Task HTTP API

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/admin/tasks` | GET | List all tasks (with status, progress) |
| `/admin/tasks/reindex` | POST | Manually trigger reindex. Body: `{ "path": "/docs/" }` |
| `/admin/tasks/gc` | POST | Manually trigger GC. Body: `{ "dry_run": false }` |
| `/admin/tasks/{id}` | GET | Get single task status |
| `/admin/tasks/{id}` | DELETE | Cancel/remove a task |

All task endpoints require admin auth.

`GET /admin/tasks` response:

```json
{
  "tasks": [
    {
      "id": "abc-123",
      "task_type": "reindex",
      "args": { "path": "/docs/" },
      "status": "running",
      "progress": 0.67,
      "eta_ms": 1775968398803,
      "created_at": 1775968000000,
      "started_at": 1775968100000,
      "completed_at": null,
      "error": null
    }
  ]
}
```

---

## 6. Query Response Meta

When a reindex task is running for a path, queries against that path (or a parent path) include reindex status in the response:

```json
{
  "results": [...],
  "total_count": 10,
  "meta": {
    "reindexing": 0.67,
    "reindexing_eta": 1775968398803,
    "reindexing_indexed": 6700,
    "reindexing_total": 10000,
    "reindexing_stale_since": 1775968000000
  }
}
```

Fields:
- `reindexing` — progress as 0.0 to 1.0
- `reindexing_eta` — estimated completion timestamp (ms since epoch)
- `reindexing_indexed` — number of files indexed so far
- `reindexing_total` — total files to index
- `reindexing_stale_since` — timestamp when the index config changed (when the reindex was triggered)

The query engine checks the shared progress state:
1. Load all running reindex tasks from in-memory progress
2. Check if any task's `args.path` is a prefix of the query path
3. If yes, include the progress fields in `meta`
4. If no reindex is running, `meta` is absent or `{}`

This is a read from shared state (no locks needed if using `Arc<RwLock>` with a read lock, or `ArcSwap` for lock-free reads).

---

## 7. Auto-Trigger on Config Change

When `DirectoryOps::store_file` stores a file whose path matches `*/.config/indexes.json`:

1. Extract the parent directory path (everything before `/.config/indexes.json`)
2. Check if a reindex task for that path is already pending or running
3. If not, enqueue a new reindex task with `{ "path": parent_path }`
4. Emit a `task_created` event

This happens inline during the store operation — just an enqueue, not the reindex itself. The background worker picks it up.

---

## 8. Event Emission

New event types:

```
EVENT_TASK_CREATED = "task_created"
EVENT_TASK_STARTED = "task_started"
EVENT_TASK_COMPLETED = "task_completed"
EVENT_TASK_FAILED = "task_failed"
EVENT_TASK_CANCELLED = "task_cancelled"
```

Payload includes task_id, task_type, args, and for completed/failed: duration_ms, error.

---

## 9. Startup Behavior

On server start:

1. Load `/.config/cron.json` — register schedules
2. Scan `/.system/tasks/` — find any tasks with status `running` (crashed mid-execution)
3. Reset those to `pending` so the worker re-processes them
4. Spawn the worker loop
5. Spawn the scheduler loop

---

## 10. Implementation Phases

### Phase 1 — Task queue core
- `TaskQueue` struct with enqueue/dequeue/update/list
- Persistent storage in `/.system/tasks/`
- In-memory progress tracking (`Arc<RwLock<HashMap>>`)
- Worker loop (tokio::spawn + spawn_blocking)
- Startup: reload incomplete tasks

### Phase 2 — Reindex task type
- Reindex executor: list files, read, run pipeline, update progress
- Auto-trigger on `indexes.json` change
- Dedup check (don't enqueue duplicate reindex)
- Query meta: include reindex progress in query responses

### Phase 3 — GC task type
- GC executor: wraps existing `run_gc`
- Progress tracking (mark=0-0.5, sweep=0.5-1.0)

### Phase 4 — Cron scheduler
- Cron expression parsing (`cron` crate)
- `/.config/cron.json` loading
- Scheduler loop (60s tick, match expressions, enqueue)
- HTTP API for cron management

### Phase 5 — Task HTTP API + events
- `/admin/tasks` endpoints (list, trigger, status, cancel)
- `/admin/cron` endpoints (list, create, delete, update)
- Event emission for task lifecycle

---

## 11. Dependencies

- `cron` crate — cron expression parsing
- `uuid` — already available (task IDs)

---

## 12. Non-goals (deferred)

- Parallel task execution (one task at a time for v1)
- Task priorities
- Task dependencies (task B waits for task A)
- Recursive reindex (each subdirectory is a separate task)
- Task result storage (beyond status + error message)
- Plugin-defined custom task types (future — WASM plugins registering their own task types)

---

## 13. Task Logging

Tasks write structured logs to `/.logs/system/tasks.log` via the existing logging system. Each task logs:

- **Start:** task_id, task_type, args, timestamp
- **Progress:** periodic updates (every batch for reindex, phase transitions for GC)
- **Completion:** task_id, duration, result summary (files indexed, garbage collected, etc.)
- **Failure:** task_id, error message, stack context
- **Cancellation:** task_id, reason (superseded, manual), progress at cancellation

The log is append-only — stored as a file in the database via `DirectoryOps::store_file`. This means task history is queryable, exportable, and survives GC (it's a real file, not metadata).
