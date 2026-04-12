# Task System, Cron & Reindex Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a background task system with persistent queue, cron scheduler, and automatic reindexing — with progress visible in query responses.

**Architecture:** Tasks persist in `/.system/tasks/` via SystemTables. A tokio worker loop processes tasks one at a time with cooperative yielding. In-memory progress tracked via `Arc<RwLock<HashMap>>`. Cron scheduler reads `/.config/cron.json` and enqueues tasks on matching expressions. Query engine checks reindex progress and includes `meta` in responses. Auto-trigger on `indexes.json` change enqueues reindex tasks.

**Tech Stack:** Rust, tokio, cron crate, serde_json, uuid

**Spec:** `bot-docs/plan/task-system-cron-reindex.md`

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Create | `aeordb-lib/src/engine/task_queue.rs` | TaskRecord, TaskQueue (persist/load/update), TaskProgress (in-memory), TaskStatus enum |
| Create | `aeordb-lib/src/engine/task_worker.rs` | spawn_task_worker, process_next_task, reindex executor, gc executor |
| Create | `aeordb-lib/src/engine/cron_scheduler.rs` | CronSchedule, CronConfig, spawn_cron_scheduler, cron expression matching |
| Create | `aeordb-lib/src/server/task_routes.rs` | HTTP endpoints: /admin/tasks/*, /admin/cron/* |
| Create | `aeordb-lib/spec/engine/task_queue_spec.rs` | Task queue CRUD, persistence, worker tests |
| Create | `aeordb-lib/spec/engine/reindex_spec.rs` | Reindex execution, auto-trigger, query meta |
| Create | `aeordb-lib/spec/engine/cron_spec.rs` | Cron parsing, scheduling |
| Create | `aeordb-lib/spec/http/task_http_spec.rs` | HTTP endpoint tests |
| Modify | `aeordb-lib/src/engine/mod.rs` | Add modules + re-exports |
| Modify | `aeordb-lib/src/engine/query_engine.rs` | Add `meta` field to PaginatedResult, check reindex progress |
| Modify | `aeordb-lib/src/engine/directory_ops.rs` | Auto-trigger reindex on indexes.json store |
| Modify | `aeordb-lib/src/engine/engine_event.rs` | Add task event constants |
| Modify | `aeordb-lib/src/server/mod.rs` | Register task/cron routes |
| Modify | `aeordb-cli/src/commands/start.rs` | Spawn worker + scheduler |
| Modify | `aeordb-lib/Cargo.toml` | Add `cron` dependency + test entries |

---

### Task 1: TaskQueue Core (Persistence + In-Memory Progress)

**Files:**
- Create: `aeordb-lib/src/engine/task_queue.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Modify: `aeordb-lib/src/engine/engine_event.rs`
- Create: `aeordb-lib/spec/engine/task_queue_spec.rs`
- Modify: `aeordb-lib/Cargo.toml`

Build the task queue: persistent task records in `/.system/tasks/` + in-memory progress tracking + task status management.

**TaskRecord struct:**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub task_type: String,           // "reindex", "gc"
    pub args: serde_json::Value,
    pub status: TaskStatus,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub error: Option<String>,
    pub checkpoint: Option<String>,  // for crash resumption
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending, Running, Completed, Failed, Cancelled,
}
```

**ProgressInfo struct (in-memory only):**
```rust
#[derive(Debug, Clone)]
pub struct ProgressInfo {
    pub task_id: String,
    pub task_type: String,
    pub args: serde_json::Value,
    pub progress: f64,
    pub eta_ms: Option<i64>,
    pub indexed_count: usize,
    pub total_count: usize,
    pub stale_since: Option<i64>,
    pub message: Option<String>,
}
```

**TaskQueue struct:**
```rust
pub struct TaskQueue {
    engine: Arc<StorageEngine>,
    progress: Arc<RwLock<HashMap<String, ProgressInfo>>>,
    cancelled: Arc<RwLock<HashSet<String>>>,
}
```

Methods:
- `enqueue(task_type, args) -> TaskRecord` — create pending task in /.system/tasks/
- `dequeue_next() -> Option<TaskRecord>` — oldest pending task
- `update_status(id, status, error)` — update persistent record
- `update_checkpoint(id, checkpoint)` — save resumption point
- `get_task(id) -> Option<TaskRecord>` — load single task
- `list_tasks() -> Vec<TaskRecord>` — all tasks
- `cancel(id)` — set cancelled flag + update status
- `is_cancelled(id) -> bool` — check cancellation flag
- `set_progress(id, info)` — update in-memory progress
- `get_progress(id) -> Option<ProgressInfo>` — read progress
- `get_reindex_progress_for_path(path) -> Option<ProgressInfo>` — find active reindex matching path
- `prune_completed(max_age_days, max_count)` — retention cleanup

Storage uses `SystemTables` pattern: keys are `blake3("::aeordb:task:{id}")`, registry at `"::aeordb:task:_registry"`.

**Event constants:** Add to `engine_event.rs`:
```rust
pub const EVENT_TASK_CREATED: &str = "task_created";
pub const EVENT_TASK_STARTED: &str = "task_started";
pub const EVENT_TASK_COMPLETED: &str = "task_completed";
pub const EVENT_TASK_FAILED: &str = "task_failed";
pub const EVENT_TASK_CANCELLED: &str = "task_cancelled";
```

**Tests (8):**
1. `test_enqueue_creates_pending_task` — enqueue, verify status=pending
2. `test_dequeue_returns_oldest_pending` — enqueue 3, dequeue returns first
3. `test_update_status_persists` — update to completed, reload, verify
4. `test_task_survives_reload` — enqueue, drop queue, create new queue from same engine, task exists
5. `test_cancel_sets_flag` — cancel, verify is_cancelled returns true
6. `test_progress_tracking` — set_progress, get_progress, verify fields
7. `test_prune_completed` — enqueue + complete 5, prune with max_count=2, verify 3 removed
8. `test_get_reindex_progress_for_path` — set reindex progress, query by path prefix

- [ ] **Step 1:** Add `cron = "0.13"` to Cargo.toml dependencies. Add test entries for all 4 spec files.
- [ ] **Step 2:** Create `task_queue.rs` with all types and methods.
- [ ] **Step 3:** Add `pub mod task_queue;` to mod.rs + re-exports.
- [ ] **Step 4:** Add event constants to engine_event.rs.
- [ ] **Step 5:** Write all 8 tests in `task_queue_spec.rs`.
- [ ] **Step 6:** Run tests: `cargo test --test task_queue_spec -- --test-threads=1`
- [ ] **Step 7:** Commit.

---

### Task 2: Task Worker + Reindex Executor

**Files:**
- Create: `aeordb-lib/src/engine/task_worker.rs`
- Modify: `aeordb-lib/src/engine/mod.rs`
- Modify: `aeordb-cli/src/commands/start.rs`
- Create: `aeordb-lib/spec/engine/reindex_spec.rs`

Build the background worker loop and the reindex task executor.

**spawn_task_worker:**
```rust
pub fn spawn_task_worker(
    queue: Arc<TaskQueue>,
    engine: Arc<StorageEngine>,
    plugin_manager: Arc<PluginManager>,
    event_bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()>
```

Follows the heartbeat pattern. Loop: dequeue → execute → update status → prune → sleep 1s.

**Reindex executor:**
```rust
fn execute_reindex(
    queue: &TaskQueue,
    task: &TaskRecord,
    engine: &StorageEngine,
    plugin_manager: &PluginManager,
    ctx: &RequestContext,
) -> Result<(), String>
```

1. Parse `args.path`
2. Read `{path}/.config/indexes.json`
3. List files via `DirectoryOps::list_directory`
4. Filter to FileRecord entries only (skip subdirectories)
5. Sort alphabetically (for deterministic checkpoint resumption)
6. If checkpoint exists, skip files before it
7. Process in batches of 50:
   - Read file, run `IndexingPipeline::run`
   - On failure: increment consecutive_failures, if >= 10 → circuit breaker
   - Update progress + checkpoint after each batch
   - Check cancellation flag
   - `tokio::task::yield_now()` between batches

**GC executor:**
```rust
fn execute_gc(
    queue: &TaskQueue,
    task: &TaskRecord,
    engine: &StorageEngine,
    ctx: &RequestContext,
) -> Result<(), String>
```

Wraps existing `run_gc`. Progress: mark=0.0-0.5, sweep=0.5-1.0 (approximate).

**Wire into start.rs:** After `spawn_webhook_dispatcher`, spawn the task worker. Pass the `Arc<TaskQueue>` (created alongside the app).

**Tests (8):**
1. `test_reindex_indexes_all_files` — store 20 JSON files, add index config, enqueue reindex, process task, query returns correct results
2. `test_reindex_checkpoint_resume` — store 100 files, set checkpoint at file 50, process, verify only files after 50 are processed
3. `test_reindex_cancellation` — enqueue reindex, cancel it, process, verify it stops early
4. `test_reindex_circuit_breaker` — store files, configure parser that doesn't exist (will fail), process, verify task fails after 10 consecutive errors
5. `test_reindex_progress_updates` — enqueue reindex, process, verify progress increments
6. `test_gc_task_executes` — enqueue gc, process, verify GC ran (garbage collected)
7. `test_worker_processes_fifo` — enqueue task A then B, process, verify A runs first
8. `test_reindex_with_parser` — deploy WASM parser, configure with parser, store text files, reindex, query works

- [ ] **Step 1:** Create `task_worker.rs` with worker loop, reindex executor, GC executor.
- [ ] **Step 2:** Wire into start.rs (spawn worker after app creation).
- [ ] **Step 3:** Write all 8 tests in `reindex_spec.rs`.
- [ ] **Step 4:** Run tests: `cargo test --test reindex_spec -- --test-threads=1`
- [ ] **Step 5:** Commit.

---

### Task 3: Auto-Trigger + Query Meta

**Files:**
- Modify: `aeordb-lib/src/engine/directory_ops.rs`
- Modify: `aeordb-lib/src/engine/query_engine.rs`
- Modify: `aeordb-lib/spec/engine/reindex_spec.rs` (add more tests)

**Auto-trigger in directory_ops.rs:**

In `store_file_internal` (or `store_file_with_full_pipeline`), after storing the file, check if the path ends with `/.config/indexes.json`. If so, extract the parent path and enqueue a reindex task. Cancel any existing reindex for the same path first.

This requires `store_file` to have access to the `TaskQueue`. Options:
- Pass `Option<&TaskQueue>` into store_file — cleanest but changes the signature
- Store `Arc<TaskQueue>` in StorageEngine — couples engine to task system
- Use an event: emit `indexes_config_changed` event, let a listener enqueue the task

**Recommended:** Pass `Option<Arc<TaskQueue>>` into the store methods that need it. The HTTP handler has access to it via AppState. The CLI/tests pass None.

Actually simpler: **detect in the HTTP handler**, not in `store_file`. The `engine_store_file` handler in `engine_routes.rs` already has access to AppState. After storing, check if path ends with `.config/indexes.json` and enqueue. This avoids changing the engine API.

**Query meta in query_engine.rs:**

Add `meta` field to `PaginatedResult`:
```rust
pub struct PaginatedResult {
    pub results: Vec<QueryResult>,
    pub total_count: Option<u64>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
    pub default_limit_hit: bool,
    pub meta: Option<QueryMeta>,  // NEW
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryMeta {
    pub reindexing: Option<f64>,
    pub reindexing_eta: Option<i64>,
    pub reindexing_indexed: Option<usize>,
    pub reindexing_total: Option<usize>,
    pub reindexing_stale_since: Option<i64>,
}
```

`execute_paginated` checks the shared progress state (passed in or accessible via engine extension). If a reindex is active for the query path, populate meta.

The `TaskQueue`'s progress state needs to be accessible from the query engine. Options:
- Pass `Arc<RwLock<HashMap<String, ProgressInfo>>>` into QueryEngine
- Store it in StorageEngine as an extension
- Pass it as a parameter to execute_paginated

**Recommended:** Add `Arc<RwLock<HashMap<String, ProgressInfo>>>` as an optional field on QueryEngine (or pass to execute_paginated). The HTTP query handler passes it from AppState.

**Tests (5):**
1. `test_auto_trigger_on_indexes_json_store` — store indexes.json via HTTP, verify reindex task enqueued
2. `test_auto_trigger_cancels_existing_reindex` — start reindex, store new indexes.json, verify old cancelled + new enqueued
3. `test_query_meta_during_reindex` — set reindex progress, execute query, verify meta fields present
4. `test_query_meta_absent_when_no_reindex` — no reindex running, query returns no meta
5. `test_query_meta_path_prefix_matching` — reindex on /docs/, query on /docs/sub/, meta present

- [ ] **Step 1:** Add auto-trigger detection in HTTP handler.
- [ ] **Step 2:** Add `QueryMeta` to PaginatedResult, wire into execute_paginated.
- [ ] **Step 3:** Write tests.
- [ ] **Step 4:** Run tests.
- [ ] **Step 5:** Commit.

---

### Task 4: Cron Scheduler

**Files:**
- Create: `aeordb-lib/src/engine/cron_scheduler.rs`
- Create: `aeordb-lib/spec/engine/cron_spec.rs`
- Modify: `aeordb-cli/src/commands/start.rs`
- Modify: `aeordb-lib/Cargo.toml` (cron dependency already added in Task 1)

**CronSchedule struct:**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronSchedule {
    pub id: String,
    pub task_type: String,
    pub schedule: String,     // "0 3 * * 0"
    pub args: serde_json::Value,
    pub enabled: bool,
}
```

**CronConfig:**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronConfig {
    pub schedules: Vec<CronSchedule>,
}
```

**spawn_cron_scheduler:**
```rust
pub fn spawn_cron_scheduler(
    queue: Arc<TaskQueue>,
    engine: Arc<StorageEngine>,
    event_bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()>
```

Loop: every 60s, load schedules, check each against current time, enqueue matching tasks.

Use the `cron` crate to parse expressions and check if they match the current minute:
```rust
use cron::Schedule;
let schedule = Schedule::from_str("0 3 * * 0 *").unwrap();
// Check if any upcoming occurrence is within the current minute
```

Note: the `cron` crate uses 6-field format (second minute hour day month weekday). Accept 5-field input and prepend "0 " to convert.

**load_cron_config:** Read `/.config/cron.json` via DirectoryOps. Return empty if not found.

**save_cron_config:** Write `/.config/cron.json` via DirectoryOps (for HTTP API persistence).

**Tests (5):**
1. `test_parse_cron_expression` — valid expression parses, invalid returns error
2. `test_cron_matches_current_time` — set up expression that matches now, verify it triggers
3. `test_cron_config_load_from_engine` — store cron.json, load, verify schedules
4. `test_cron_disabled_schedule_skipped` — schedule with enabled:false doesn't trigger
5. `test_cron_save_config` — save via API, reload, verify persisted

- [ ] **Step 1:** Create `cron_scheduler.rs`.
- [ ] **Step 2:** Wire into start.rs.
- [ ] **Step 3:** Write tests.
- [ ] **Step 4:** Run tests.
- [ ] **Step 5:** Commit.

---

### Task 5: HTTP API + Events

**Files:**
- Create: `aeordb-lib/src/server/task_routes.rs`
- Modify: `aeordb-lib/src/server/mod.rs`
- Modify: `aeordb-lib/src/server/state.rs` (add TaskQueue to AppState)
- Create: `aeordb-lib/spec/http/task_http_spec.rs`

**Task endpoints:**

| Endpoint | Method | Handler |
|----------|--------|---------|
| `/admin/tasks` | GET | list_tasks — all tasks with status/progress |
| `/admin/tasks/reindex` | POST | trigger_reindex — body: `{"path":"/docs/"}` |
| `/admin/tasks/gc` | POST | trigger_gc — body: `{"dry_run":false}` |
| `/admin/tasks/{id}` | GET | get_task — single task status |
| `/admin/tasks/{id}` | DELETE | cancel_task — cancel running/pending task |

**Cron endpoints:**

| Endpoint | Method | Handler |
|----------|--------|---------|
| `/admin/cron` | GET | list_schedules |
| `/admin/cron` | POST | create_schedule — body: CronSchedule JSON |
| `/admin/cron/{id}` | DELETE | delete_schedule |
| `/admin/cron/{id}` | PATCH | update_schedule — enable/disable, change expression |

All require admin auth (root user).

**AppState changes:** Add `task_queue: Arc<TaskQueue>` to AppState. Initialize in `create_app_with_all`.

**Event emission:** Task lifecycle events emitted from the worker (task_created on enqueue, task_started/completed/failed/cancelled from worker).

**Tests (6):**
1. `test_trigger_reindex_via_http` — POST /admin/tasks/reindex, verify 200 + task created
2. `test_trigger_gc_via_http` — POST /admin/tasks/gc, verify 200
3. `test_list_tasks` — create tasks, GET /admin/tasks, verify list
4. `test_cancel_task_via_http` — create + DELETE, verify cancelled
5. `test_cron_crud_via_http` — POST /admin/cron, GET, DELETE cycle
6. `test_task_endpoints_require_auth` — no token → 401

- [ ] **Step 1:** Add TaskQueue to AppState.
- [ ] **Step 2:** Create task_routes.rs with all handlers.
- [ ] **Step 3:** Register routes in server/mod.rs.
- [ ] **Step 4:** Write tests.
- [ ] **Step 5:** Run full suite: `cargo test -- --test-threads=4`
- [ ] **Step 6:** Commit.

---

## Post-Implementation Checklist

- [ ] Update `.claude/TODO.md` — add "Task System, Cron & Reindex" with test count
- [ ] Update `.claude/DETAILS.md` — add new files to key files list
- [ ] Run: `cargo test -- --test-threads=4` — all tests pass
- [ ] E2E: start server, store files, add index config, verify reindex auto-triggers, query shows meta
- [ ] E2E: create cron schedule via API, verify it persists
