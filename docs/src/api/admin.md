# System Operations

Administrative endpoints for garbage collection, background tasks, cron scheduling, metrics, health checks, backup/restore, and user/group management. Most system endpoints require **root** access.

## Endpoint Summary

### Garbage Collection

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/system/gc` | Run synchronous garbage collection | Yes |

### Background Tasks

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/system/tasks/reindex` | Trigger a reindex task | Yes |
| POST | `/system/tasks/gc` | Trigger a background GC task | Yes |
| GET | `/system/tasks` | List all tasks with progress | Yes |
| GET | `/system/tasks/{id}` | Get a single task | Yes |
| DELETE | `/system/tasks/{id}` | Cancel a task | Yes |

### Cron Scheduling

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| GET | `/system/cron` | List cron schedules | Yes |
| POST | `/system/cron` | Create a cron schedule | Yes |
| PATCH | `/system/cron/{id}` | Update a cron schedule | Yes |
| DELETE | `/system/cron/{id}` | Delete a cron schedule | Yes |

### Lifecycle Configuration

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| GET | `/system/lifecycle` | Read lifecycle policy | Yes |
| PUT | `/system/lifecycle` | Replace lifecycle policy | Yes |

### Backup & Restore

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/versions/export` | Export database as `.aeordb` | Yes |
| POST | `/versions/diff` | Create patch between versions | Yes |
| POST | `/versions/import` | Import a backup or patch | Yes |
| POST | `/versions/promote` | Promote a version hash to HEAD | Yes |

### Monitoring

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| GET | `/system/stats` | System stats (JSON) | Yes (auth required) |
| GET | `/system/metrics` | Prometheus metrics | Yes (auth required) |
| GET | `/system/health` | Health check | No (public) |

### API Key Management

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/auth/keys/admin` | Create an API key | Yes |
| GET | `/auth/keys/admin` | List all API keys | Yes |
| DELETE | `/auth/keys/admin/{key_id}` | Revoke an API key | Yes |

### User Management

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/system/users` | Create a user | Yes |
| GET | `/system/users` | List all users | Yes |
| GET | `/system/users/{user_id}` | Get a user | Yes |
| PATCH | `/system/users/{user_id}` | Update a user | Yes |
| DELETE | `/system/users/{user_id}` | Deactivate a user (soft delete) | Yes |

### Group Management

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| POST | `/system/groups` | Create a group | Yes |
| GET | `/system/groups` | List all groups | Yes |
| GET | `/system/groups/{name}` | Get a group | Yes |
| PATCH | `/system/groups/{name}` | Update a group | Yes |
| DELETE | `/system/groups/{name}` | Delete a group | Yes |

### Email Configuration

| Method | Path | Description | Root Required |
|--------|------|-------------|---------------|
| GET | `/system/email-config` | Get email configuration (secrets masked) | Yes |
| PUT | `/system/email-config` | Save email configuration (SMTP or OAuth) | Yes |
| POST | `/system/email-test` | Send a test email | Yes |

---

## Garbage Collection

### POST /system/gc

Run garbage collection synchronously. Identifies and removes orphaned entries not reachable from the current HEAD.

**Query Parameters:**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `dry_run` | boolean | `false` | If true, report what would be collected without deleting |

**Response:** `200 OK`

The response contains GC statistics (entries scanned, reclaimed bytes, etc.).

**Example:**

```bash
# Dry run
curl -X POST "http://localhost:6830/system/gc?dry_run=true" \
  -H "Authorization: Bearer $TOKEN"

# Actual GC
curl -X POST http://localhost:6830/system/gc \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 403 | Non-root user |
| 500 | GC failure |

---

## Background Tasks

### POST /system/tasks/reindex

Enqueue a reindex task for a directory path. Re-scans all files and rebuilds index entries.

**Request Body:**

```json
{
  "path": "/data/",
  "force": false,
  "metadata_only": false,
  "index_flush_writes": 262144,
  "index_flush_ms": 30000
}
```

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `path` | string | Required | Directory path to reindex |
| `force` | boolean | `false` | When true, also migrates older live FileRecord payloads in the requested subtree to the current version before indexing eligible files. Omit or set to `false` for index-only reprocessing. Internal/system FileRecords can be migrated but are not indexed. |
| `metadata_only` | boolean | `false` | When true, rebuild only virtual `@` metadata indexes from FileRecord metadata. This skips file body reads, JSON parsing, and parser plugins. |
| `index_flush_writes` | integer | `262144` | Flush buffered index mutations after this many field/strategy updates. |
| `index_flush_ms` | integer | `30000` | Flush buffered index mutations after this many milliseconds. |

**Response:** `200 OK`

```json
{
  "id": "task-uuid-here",
  "task_type": "reindex",
  "status": "pending"
}
```

**Example:**

```bash
curl -X POST http://localhost:6830/system/tasks/reindex \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path": "/data/", "metadata_only": true}'
```

---

### POST /system/tasks/gc

Enqueue a background GC task (non-blocking).

**Request Body:**

```json
{
  "dry_run": false
}
```

**Response:** `200 OK`

```json
{
  "id": "task-uuid-here",
  "task_type": "gc",
  "status": "pending"
}
```

**Example:**

```bash
curl -X POST http://localhost:6830/system/tasks/gc \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"dry_run": false}'
```

---

### GET /system/tasks

List all tasks with their current progress.

**Response:** `200 OK`

```json
{
  "items": [
    {
      "id": "task-uuid-here",
      "task_type": "reindex",
      "status": "running",
      "args": {"path": "/data/"},
      "created_at": 1775968400000,
      "started_at": 1775968450000,
      "completed_at": null,
      "error": null,
      "checkpoint": "/data/file-0042.json",
      "retry_at": null,
      "deferral_count": 0,
      "progress": 0.45,
      "eta_ms": 1775968500000
    }
  ]
}
```

Each task includes `progress` (0.0-1.0) and `eta_ms` when available. A
pressure-deferred task remains `pending`, preserves its `checkpoint`, records
the earliest next claim time in `retry_at`, and increments `deferral_count`.

**Example:**

```bash
curl http://localhost:6830/system/tasks \
  -H "Authorization: Bearer $TOKEN"
```

---

### GET /system/tasks/{id}

Get a single task by ID.

**Response:** `200 OK`

```json
{
  "id": "task-uuid-here",
  "task_type": "reindex",
  "status": "running",
  "args": {"path": "/data/"},
  "created_at": 1775968400000,
  "started_at": 1775968450000,
  "completed_at": null,
  "error": null,
  "checkpoint": "/data/file-0042.json",
  "retry_at": null,
  "deferral_count": 0,
  "progress": 0.45,
  "eta_ms": 1775968500000
}
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 404 | Task not found |

---

### DELETE /system/tasks/{id}

Cancel a task.

**Response:** `200 OK`

```json
{
  "id": "task-uuid-here",
  "status": "cancelled"
}
```

**Example:**

```bash
curl -X DELETE http://localhost:6830/system/tasks/task-uuid-here \
  -H "Authorization: Bearer $TOKEN"
```

---

## Cron Scheduling

> **Tip:** The portal Settings page provides an intuitive UI for scheduling garbage collection. Navigate to Settings → Garbage Collector to configure.

### GET /system/cron

List all cron schedules.

**Response:** `200 OK`

```json
{
  "items": [
    {
      "id": "nightly-gc",
      "schedule": "0 2 * * *",
      "task_type": "gc",
      "args": {"dry_run": false},
      "enabled": true
    }
  ]
}
```

---

### POST /system/cron

Create a new cron schedule.

**Request Body:**

```json
{
  "id": "nightly-gc",
  "schedule": "0 2 * * *",
  "task_type": "gc",
  "args": {"dry_run": false},
  "enabled": true
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `id` | string | Yes | Unique schedule identifier |
| `schedule` | string | Yes | Cron expression |
| `task_type` | string | Yes | Task type to enqueue (`"gc"`, `"reindex"`, `"backup"`) |
| `args` | object | Yes | Arguments passed to the task |
| `enabled` | boolean | Yes | Whether the schedule is active |

**Response:** `201 Created`

**Example:**

```bash
curl -X POST http://localhost:6830/system/cron \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "id": "nightly-gc",
    "schedule": "0 2 * * *",
    "task_type": "gc",
    "args": {"dry_run": false},
    "enabled": true
  }'
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid cron expression |
| 409 | Schedule with this ID already exists |

---

### PATCH /system/cron/{id}

Update a cron schedule. All fields are optional -- only provided fields are changed.

**Request Body:**

```json
{
  "enabled": false,
  "schedule": "0 3 * * *"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `enabled` | boolean | Enable or disable the schedule |
| `schedule` | string | New cron expression |
| `task_type` | string | New task type |
| `args` | object | New task arguments |

**Response:** `200 OK`

Returns the updated schedule.

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid cron expression |
| 404 | Schedule not found |

---

### DELETE /system/cron/{id}

Delete a cron schedule.

**Response:** `200 OK`

```json
{
  "id": "nightly-gc",
  "deleted": true
}
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 404 | Schedule not found |

---

## Lifecycle Configuration

Lifecycle configuration is stored inside the database at `/.aeordb-config/lifecycle.json`. Missing fields use safe defaults, so older databases that only have `snapshot_retention` continue to allow snapshot writes.

### GET /system/lifecycle

Return the current lifecycle policy.

**Response:** `200 OK`

```json
{
  "snapshot_writes_enabled": true,
  "snapshot_retention": {
    "auto_months": 0,
    "manual_months": 0
  }
}
```

### PUT /system/lifecycle

Replace the lifecycle policy.

**Request Body:**

```json
{
  "snapshot_writes_enabled": false,
  "snapshot_retention": {
    "auto_months": 1,
    "manual_months": 12
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `snapshot_writes_enabled` | boolean | `true` | Allow creation of new snapshot records. When `false`, existing snapshots can still be listed, read, restored, deleted, exported, and pruned. |
| `snapshot_retention.auto_months` | integer | `0` | Months after which auto snapshots are eligible for pruning. `0` disables pruning. |
| `snapshot_retention.manual_months` | integer | `0` | Months after which manual snapshots are eligible for pruning. `0` disables pruning. |

When `snapshot_writes_enabled` is `false`, manual `POST /versions/snapshots` and snapshot rename requests return `403`. Automatic safety snapshots, such as file-restore and pre-GC snapshots, are skipped; the underlying operation continues when it can safely proceed without writing a new snapshot.

---

## Backup & Restore

### POST /versions/export

Export the database (or a specific version) as an `.aeordb` archive file.

**Query Parameters:**

| Parameter | Type | Description |
|-----------|------|-------------|
| `snapshot` | string | Export a named snapshot (default: HEAD) |
| `hash` | string | Export a specific version by hex hash |

**Response:** `200 OK`

- **Content-Type:** `application/octet-stream`
- **Content-Disposition:** `attachment; filename="export-{hash_prefix}.aeordb"`
- **Body:** binary archive data

The archive is generated on a blocking engine worker and streamed from a
temporary file beside the database, on the same data filesystem. AeorDB does
not buffer the complete archive in RAM or stage it in the operating system's
temporary directory. The temporary file and its memory reservation are
released when the transfer finishes or the client disconnects. The database
filesystem therefore needs enough free space for the generated archive while
the response is active.

**Example:**

```bash
# Export HEAD
curl -X POST http://localhost:6830/versions/export \
  -H "Authorization: Bearer $TOKEN" \
  -o backup.aeordb

# Export a specific snapshot
curl -X POST "http://localhost:6830/versions/export?snapshot=v1.0" \
  -H "Authorization: Bearer $TOKEN" \
  -o backup-v1.aeordb

# Export by hash
curl -X POST "http://localhost:6830/versions/export?hash=a1b2c3d4..." \
  -H "Authorization: Bearer $TOKEN" \
  -o backup.aeordb
```

---

### POST /versions/diff

Create a patch file representing the difference between two versions.

**Query Parameters:**

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `from` | string | Yes | Source snapshot name or hex hash |
| `to` | string | No | Target snapshot name or hex hash (default: HEAD) |

**Response:** `200 OK`

- **Content-Type:** `application/octet-stream`
- **Content-Disposition:** `attachment; filename="patch-{hash_prefix}.aeordb"`
- **Body:** binary patch data

Patch files use the same disk-backed response streaming as full exports. Each
reference is resolved as an exact snapshot name first and otherwise must be a
complete hex hash for the database's configured hash algorithm.

**Example:**

```bash
curl -X POST "http://localhost:6830/versions/diff?from=v1.0&to=v2.0" \
  -H "Authorization: Bearer $TOKEN" \
  -o patch-v1-v2.aeordb
```

---

### POST /versions/import

Import a backup or patch file. The request is streamed to a temporary file
beside the target database and is not buffered as one in-memory body or staged
in the operating system's temporary directory. The transfer limit is **10
GiB**, and the target filesystem needs enough free space for the upload while
the operation is active. The limit is enforced against both a declared
`Content-Length` and the actual number of bytes received from the stream.

**Query Parameters:**

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `force` | boolean | `false` | Force import even if conflicts exist |
| `promote` | boolean | `false` | Promote the imported version to HEAD |
| `mode` | string | `merge` | `merge` overlays the backup; `restore` requires an empty target unless `force=true` |

**Request:**

- **Headers:**
  - `Authorization: Bearer <token>` (required)
- **Body:** raw `.aeordb` file bytes

**Response:** `200 OK`

```json
{
  "status": "success",
  "backup_type": "export",
  "entries_imported": 1500,
  "chunks_imported": 3200,
  "files_imported": 450,
  "directories_imported": 30,
  "deletions_applied": 5,
  "version_hash": "a1b2c3d4e5f6...",
  "head_promoted": true
}
```

**Example:**

```bash
curl -X POST "http://localhost:6830/versions/import?promote=true" \
  -H "Authorization: Bearer $TOKEN" \
  --data-binary @backup.aeordb
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Empty, invalid, corrupt, unsupported, or malformed backup file |
| 403 | Non-root user |
| 413 | Upload exceeds 10 GiB |
| 503 | Backup/restore memory admission or cancellation prevents the operation |

---

### POST /versions/promote

Promote an arbitrary version hash to HEAD.

**Query Parameters:**

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `hash` | string | Yes | Hex-encoded version hash to promote |

**Response:** `200 OK`

```json
{
  "status": "success",
  "head": "a1b2c3d4e5f6..."
}
```

**Example:**

```bash
curl -X POST "http://localhost:6830/versions/promote?hash=a1b2c3d4e5f6..." \
  -H "Authorization: Bearer $TOKEN"
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid hash format |
| 404 | Version hash not found in storage |

---

## Monitoring

### GET /system/stats

System statistics endpoint. Returns a structured JSON snapshot of all engine metrics. All values are read from O(1) atomic counters — this endpoint is safe to poll frequently with no performance impact.

**Response:** `200 OK`

```json
{
  "identity": {
    "version": "0.9.0",
    "database_path": "/data/mydb.aeordb",
    "hash_algorithm": "Blake3_256",
    "chunk_size": 262144,
    "node_id": 1,
    "uptime_seconds": 86400
  },
  "counts": {
    "files": 150000,
    "directories": 23000,
    "symlinks": 500,
    "chunks": 420000,
    "snapshots": 12,
    "forks": 2
  },
  "sizes": {
    "disk_total": 2147483648,
    "kv_file": 86114304,
    "logical_data": 1800000000,
    "chunk_data": 1200000000,
    "void_space": 5242880,
    "dedup_savings": 600000000
  },
  "throughput": {
    "writes_per_sec": { "1m": 42.3, "5m": 38.1, "15m": 35.7, "peak_1m": 120.0 },
    "reads_per_sec": { "1m": 156.2, "5m": 140.5, "15m": 138.0, "peak_1m": 450.0 },
    "bytes_written_per_sec": { "1m": 435200, "5m": 392000, "15m": 367000 },
    "bytes_read_per_sec": { "1m": 16065536, "5m": 14450000, "15m": 14200000 }
  },
  "latency": {
    "write": { "p50": 5.6, "p95": 15.4, "p99": 20.5 },
    "read": { "p50": 2.1, "p95": 8.3, "p99": 12.0 },
    "query": { "p50": 4.2, "p95": 22.0, "p99": 45.0 },
    "flush": { "p50": 1.2, "p95": 5.0, "p99": 12.0 }
  },
  "health": {
    "disk_usage_percent": 48.5,
    "kv_fill_ratio": 0.72,
    "dedup_hit_rate": 0.33,
    "gc_last_reclaimed_bytes": 1048576,
    "write_buffer_depth": 42
  },
  "memory": {
    "process": {
      "rss_bytes": 2147483648,
      "peak_rss_bytes": 3221225472,
      "virtual_bytes": 8589934592,
      "data_bytes": 1610612736,
      "swap_bytes": 0,
      "thread_count": 32,
      "fd_count": 128
    },
    "index_cache": {
      "cached_indexes": 16,
      "dirty_indexes": 4,
      "deleted_indexes": 0,
      "pending_mutations": 512,
      "total_mutations": 102400,
      "flushes": 7,
      "flushed_indexes": 92,
      "evictions": 3,
      "evicted_indexes": 12,
      "evicted_bytes": 268435456,
      "entries": 2500000,
      "values": 350000,
      "estimated_bytes": 734003200,
      "estimated_clean_bytes": 503316480,
      "estimated_dirty_bytes": 230686720,
      "clean_reserved_bytes": 503316480,
      "dirty_reserved_bytes": 234881024,
      "flush_reserved_bytes": 16777216,
      "flushing_indexes": 1,
      "max_bytes": 2147483648,
      "mutation_max_bytes": 1073741824,
      "publication_batch_max_bytes": 268435456,
      "clean_ttl_ms": 300000,
      "reservation_owned": true,
      "top_cached_indexes": [
        {
          "parent": "/",
          "field_name": "@path",
          "strategy": "trigram",
          "entries": 1200000,
          "values": 80000,
          "estimated_bytes": 234881024,
          "dirty": false,
          "last_access_age_ms": 42000
        }
      ]
    },
    "directory_cache": {
      "entries": 12000,
      "estimated_bytes": 16777216
    },
    "caches": {
      "permissions_entries": 128,
      "index_config_entries": 16,
      "grants_index_entries": 4
    },
    "estimated_engine_owned_bytes": 767557632
  },
  "sync": {
    "active_peers": 2,
    "failing_peers": 0,
    "last_sync_ms": 1776563922032,
    "sync_lag_entries": { "peer_2": 0, "peer_3": 15 }
  }
}
```

**Response sections:**

| Section | Description |
|---------|-------------|
| `identity` | Server version, database path, hash algorithm, chunk size, node ID, and uptime |
| `counts` | Current totals for files, directories, symlinks, chunks, snapshots, and forks |
| `sizes` | Byte-level storage breakdown: disk total, KV file size, logical data, chunk data, void space, dedup savings |
| `throughput` | Rolling read/write rates (1m, 5m, 15m averages) and peak rates |
| `latency` | Percentile latencies (p50, p95, p99) for write, read, query, and flush operations (in milliseconds) |
| `health` | Operational health signals: disk usage, KV fill ratio, dedup hit rate, last GC reclamation, write buffer depth |
| `memory` | Process memory and AeorDB-owned cache diagnostics, including RSS, swap, thread/fd counts, index cache estimates, directory cache estimates, and cache entry counts |
| `sync` | Replication status: active/failing peers, last sync timestamp, per-peer sync lag (only present when replication is active) |

> **Note:** The previous `GET /system/stats` returned a flat object computed via O(n) iteration. The new response is structured into nested sections and is O(1) — no performance concerns polling at high frequency.

`sizes.logical_data` is the sum of live file sizes reachable from the current HEAD tree. `sizes.chunk_data` is the stored payload size of unique chunk entries in the KV index, initialized from entry metadata without reading chunk bodies. `sizes.void_space` is tracked reusable space inside the append log; it is not filesystem free space.

`memory.process` is sampled from the operating system. On Linux this uses `/proc/self/status` plus `/proc/self/fd`; on macOS it uses Mach task information plus `/dev/fd`. Platform-specific fields that are unavailable are reported as `0`. `memory.index_cache.estimated_*`, `memory.directory_cache.estimated_bytes`, and `memory.estimated_engine_owned_bytes` are allocation estimates intended for diagnosis and trend monitoring. The index `*_reserved_bytes` fields are exact coordinator reservations: clean cache, retained dirty state, and serialized flush scratch are reported separately. While `reservation_owned` is true, the `index_dirty_buffers` coordinator owner reconciles to `dirty_reserved_bytes + flush_reserved_bytes`; flush scratch alone is critical durable-write headroom.

The `durability_waiters` owner reserves bounded emergency headroom for the
operation ledger and each admitted commit record until its exact result is
retired. Hard-authority records additionally enter the bounded frontier queue.
Exhausting that headroom refuses a new pre-mutation commit with a
retryable `503` instead of allowing the queue to grow or falsely latching a
storage failure. Until strict runtime activation, an unavailable memory policy
retains the legacy write path behind a fixed structural waiter ceiling and
reports its bytes as legacy observation; it does not permit an unbounded queue.

The index cache holds full field/strategy index files while they have unflushed mutations or recent read/write activity. Clean indexes are recoverable from disk and may be evicted after `clean_ttl_ms` of idleness or earlier when the cache exceeds `max_bytes`. Dirty and in-flight indexes are never evicted before durable publication succeeds. Publication failure restores the exact dirty generation and releases its flush scratch reservation. The resolved defaults are `min(2 GiB, hard-memory-limit / 4)` for clean indexes, `min(1 GiB, hard-memory-limit / 8)` for dirty mutations, 256 MiB per publication batch, and five minutes idle TTL. Use the registered runtime properties or their official environment forms: `AEORDB_CACHE_INDEX_CLEAN_MAX_BYTES`, `AEORDB_INDEX_MUTATION_BUFFER_MAX_BYTES`, `AEORDB_INDEX_PUBLICATION_BATCH_MAX_BYTES`, and `AEORDB_CACHE_INDEX_CLEAN_TTL_SECONDS`.

**Example:**

```bash
curl http://localhost:6830/system/stats \
  -H "Authorization: Bearer $TOKEN"
```

---

### GET /system/health

Public health check endpoint. No authentication required. Once the database is ready, this returns only a minimal status object -- no detailed internal checks are exposed.

**Response:** `200 OK`

```json
{
  "status": "healthy",
  "version": "0.9.5"
}
```

During startup, clean opens, dirty startup, or WAL/KV recovery, AeorDB binds HTTP before the storage engine is ready. In that state, `/system/health` still returns `200 OK` with startup progress:

```json
{
  "status": "starting",
  "phase": "rebuild_kv_scan",
  "message": "Scanning WAL entries for dirty startup recovery",
  "version": "0.9.5",
  "progress": 0.42,
  "eta": {
    "seconds": 480,
    "at": "2026-06-12T03:05:00Z"
  }
}
```

`progress` is an overall startup fraction from `0.0` to `1.0`. `eta` is `null` when unknown, or an object with estimated seconds remaining and an RFC 3339 timestamp. Non-health routes return `503 Service Unavailable` until the full application router is ready.

For detailed system diagnostics after startup, use `GET /system/stats` instead (requires authentication).

**Example:**

```bash
curl http://localhost:6830/system/health
```

---

### GET /system/metrics

Prometheus-format metrics endpoint. Requires authentication.

**Response:** `200 OK`

- **Content-Type:** `text/plain; version=0.0.4; charset=utf-8`
- **Body:** Prometheus text exposition format

Memory gauges are updated when `/system/metrics` is rendered and by the periodic metrics pulse:

| Metric | Description |
|--------|-------------|
| `aeordb_process_rss_bytes` | Current process resident set size |
| `aeordb_process_peak_rss_bytes` | Peak process resident set size observed by the OS |
| `aeordb_process_virtual_bytes` | Process virtual address size |
| `aeordb_process_data_bytes` | Process data/heap segment size where available |
| `aeordb_process_swap_bytes` | Process swap usage where available |
| `aeordb_process_thread_count` | Process thread count where available |
| `aeordb_process_fd_count` | Open file descriptor count where available |
| `aeordb_engine_memory_estimated_bytes` | Estimated AeorDB-owned cache memory tracked by diagnostics |
| `aeordb_index_cache_estimated_bytes` | Estimated shared index cache memory |
| `aeordb_index_cache_estimated_clean_bytes` | Estimated clean, evictable index memory |
| `aeordb_index_cache_estimated_dirty_bytes` | Estimated retained dirty or flushing index memory |
| `aeordb_index_cache_clean_reserved_bytes` | Exact coordinator reservation for clean index cache state |
| `aeordb_index_cache_dirty_reserved_bytes` | Exact coordinator reservation for dirty index state, excluding flush scratch |
| `aeordb_index_cache_flush_reserved_bytes` | Exact critical reservation for serialized publication scratch |
| `aeordb_index_cache_cached_indexes` | Cached field/strategy index count |
| `aeordb_index_cache_dirty_indexes` | Cached indexes with unflushed mutations |
| `aeordb_index_cache_flushing_indexes` | Index generations currently being durably published |
| `aeordb_index_cache_pending_mutations` | Pending index mutations in the shared buffer |
| `aeordb_index_cache_max_bytes` | Resolved clean index cache limit |
| `aeordb_index_mutation_buffer_max_bytes` | Resolved dirty index mutation limit |
| `aeordb_index_publication_batch_max_bytes` | Resolved serialized publication batch limit |
| `aeordb_index_cache_evictions` | Number of clean index eviction passes that removed at least one index |
| `aeordb_index_cache_evicted_indexes` | Total clean indexes evicted from memory since startup |
| `aeordb_index_cache_evicted_bytes` | Estimated clean index bytes evicted from memory since startup |
| `aeordb_index_cache_entries` | Indexed scalar entry count currently cached |
| `aeordb_index_cache_values` | Raw indexed value count currently cached |
| `aeordb_directory_cache_estimated_bytes` | Estimated directory content cache memory |
| `aeordb_directory_cache_entries` | Directory content cache entry count |

**Example:**

```bash
curl http://localhost:6830/system/metrics \
  -H "Authorization: Bearer $TOKEN"
```

---

## API Key Management

### POST /auth/keys/admin

Create a new API key. The plaintext key is returned **only once** -- store it securely. Requires root.

**Request Body:**

```json
{
  "user_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `user_id` | string (UUID) | No | User to create the key for (defaults to the calling user) |

**Response:** `201 Created`

```json
{
  "key_id": "660e8400-e29b-41d4-a716-446655440001",
  "api_key": "aeor_660e8400_a1b2c3d4e5f6...",
  "user_id": "550e8400-e29b-41d4-a716-446655440000",
  "created_at": "2026-04-13T10:00:00Z"
}
```

**Example:**

```bash
curl -X POST http://localhost:6830/auth/keys/admin \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"user_id": "550e8400-e29b-41d4-a716-446655440000"}'
```

---

### GET /auth/keys/admin

List all API keys (metadata only -- no secrets). Requires root.

**Response:** `200 OK`

```json
{
  "items": [
    {
      "key_id": "660e8400-e29b-41d4-a716-446655440001",
      "user_id": "550e8400-e29b-41d4-a716-446655440000",
      "created_at": "2026-04-13T10:00:00Z",
      "is_revoked": false
    }
  ]
}
```

---

### DELETE /auth/keys/admin/{key_id}

Revoke an API key. Revoked keys cannot be used to obtain tokens. Requires root.

**Response:** `200 OK`

```json
{
  "revoked": true,
  "key_id": "660e8400-e29b-41d4-a716-446655440001"
}
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid key ID format |
| 404 | API key not found |

---

## User Management

### POST /system/users

Create a new user. Requires root.

**Request Body:**

```json
{
  "username": "alice",
  "email": "alice@example.com",
  "tags": ["editor", "us-west"]
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `username` | string | Yes | Unique username |
| `email` | string | No | User email address |
| `tags` | array of strings | No | Admin-assigned tags for group membership queries (default: empty) |

**Response:** `201 Created`

```json
{
  "user_id": "550e8400-e29b-41d4-a716-446655440000",
  "username": "alice",
  "email": "alice@example.com",
  "is_active": true,
  "tags": ["editor", "us-west"],
  "created_at": 1775968398000,
  "updated_at": 1775968398000
}
```

---

### GET /system/users

List all users. Requires root.

**Response:** `200 OK`

```json
{
  "items": [
    {
      "user_id": "550e8400-e29b-41d4-a716-446655440000",
      "username": "alice",
      "email": "alice@example.com",
      "is_active": true,
      "created_at": 1775968398000,
      "updated_at": 1775968398000
    }
  ]
}
```

---

### GET /system/users/{user_id}

Get a single user. Requires root.

**Response:** `200 OK` (same shape as the user object above)

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Invalid UUID |
| 404 | User not found |

---

### PATCH /system/users/{user_id}

Update a user. All fields are optional. Requires root.

**Request Body:**

```json
{
  "username": "alice_updated",
  "email": "newemail@example.com",
  "is_active": true,
  "tags": ["editor", "us-west", "senior"]
}
```

Tags are admin-only -- users cannot modify their own tags, preventing privilege escalation through self-assigned group membership.

**Response:** `200 OK` (returns the updated user)

---

### DELETE /system/users/{user_id}

Deactivate a user (soft delete -- sets `is_active` to false). Requires root.

**Response:** `200 OK`

```json
{
  "deactivated": true,
  "user_id": "550e8400-e29b-41d4-a716-446655440000"
}
```

---

## Group Management

Groups define path-level access control rules using query-based membership. Users are matched into groups by querying safe fields on their user record, including `tags`.

### Tag-Based Group Membership

When `query_field` is set to `"tags"`, three special operators are available:

| Operator | Description | Example |
|----------|-------------|---------|
| `has` | User has this exact tag | `"query_value": "editor"` |
| `has_any` | User has at least one of these tags (comma-separated) | `"query_value": "editor,admin"` |
| `has_all` | User has all of these tags (comma-separated) | `"query_value": "editor,us-west"` |

Standard operators (`eq`, `gt`, etc.) also work with tags -- they match against the comma-joined tag string.

### POST /system/groups

Create a new group. Requires root.

**Request Body:**

```json
{
  "name": "editors",
  "default_allow": "/content/*",
  "default_deny": "/system/*",
  "query_field": "tags",
  "query_operator": "has",
  "query_value": "editor"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Unique group name |
| `default_allow` | string | Yes | Path pattern for allowed access |
| `default_deny` | string | Yes | Path pattern for denied access |
| `query_field` | string | Yes | User field to query for membership (must be a safe field) |
| `query_operator` | string | Yes | Comparison operator |
| `query_value` | string | Yes | Value to match against |

**Response:** `201 Created`

```json
{
  "name": "editors",
  "default_allow": "/content/*",
  "default_deny": "/system/*",
  "query_field": "role",
  "query_operator": "eq",
  "query_value": "editor",
  "created_at": 1775968398000,
  "updated_at": 1775968398000
}
```

---

### GET /system/groups

List all groups. Requires root.

**Response:** `200 OK` (object with `items` array of group objects)

---

### GET /system/groups/{name}

Get a single group. Requires root.

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 404 | Group not found |

---

### PATCH /system/groups/{name}

Update a group. All fields are optional. Requires root.

**Request Body:**

```json
{
  "default_allow": "/content/*",
  "query_value": "senior-editor"
}
```

The `query_field` value is validated against a whitelist of safe fields. Attempting to use an unsafe field returns a `400` error.

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 400 | Unsafe query field |
| 404 | Group not found |

---

### DELETE /system/groups/{name}

Delete a group. Requires root.

**Response:** `200 OK`

```json
{
  "deleted": true,
  "name": "editors"
}
```

**Error Responses:**

| Status | Condition |
|--------|-----------|
| 404 | Group not found |

---

## Email Configuration

AeorDB supports sending email notifications (e.g., when files are shared via `POST /files/share`). Email can be configured using either SMTP or OAuth providers.

### GET /system/email-config

Retrieve the current email configuration. Sensitive fields (passwords, client secrets, refresh tokens) are masked as `"--------"` in the response, and a `"configured": true` field is added.

**Auth:** Root only.

**Response:** `200 OK`

```json
{
  "provider": "smtp",
  "host": "smtp.example.com",
  "port": 587,
  "username": "noreply@example.com",
  "password": "--------",
  "from_address": "noreply@example.com",
  "from_name": "AeorDB",
  "tls": "starttls",
  "configured": true
}
```

---

### PUT /system/email-config

Save email configuration. Supports two provider types: SMTP and OAuth.

**Auth:** Root only.

**SMTP Configuration:**

```json
{
  "provider": "smtp",
  "host": "smtp.example.com",
  "port": 587,
  "username": "noreply@example.com",
  "password": "secret",
  "from_address": "noreply@example.com",
  "from_name": "AeorDB",
  "tls": "starttls"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `provider` | string | Yes | Must be `"smtp"` |
| `host` | string | Yes | SMTP server hostname |
| `port` | integer | Yes | SMTP server port |
| `username` | string | Yes | SMTP username |
| `password` | string | Yes | SMTP password |
| `from_address` | string | Yes | Sender email address |
| `from_name` | string | No | Sender display name (default: `"AeorDB"`) |
| `tls` | string | No | TLS mode: `"starttls"` (port 587, default), `"tls"` (implicit TLS, port 465), or `"none"` (cleartext) |

**OAuth Configuration:**

```json
{
  "provider": "oauth",
  "oauth_provider": "gmail",
  "client_id": "...",
  "client_secret": "...",
  "refresh_token": "...",
  "from_address": "noreply@example.com",
  "from_name": "AeorDB"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `provider` | string | Yes | Must be `"oauth"` |
| `oauth_provider` | string | Yes | OAuth provider: `"gmail"`, `"outlook"`, or `"custom"` |
| `client_id` | string | Yes | OAuth client ID |
| `client_secret` | string | Yes | OAuth client secret |
| `refresh_token` | string | Yes | OAuth refresh token |
| `from_address` | string | Yes | Sender email address |
| `from_name` | string | No | Sender display name |

**Response:** `200 OK`

---

### POST /system/email-test

Send a test email to verify the current configuration.

**Auth:** Root only.

**Request Body:**

```json
{
  "to": "recipient@example.com"
}
```

**Response:** `200 OK`

```json
{
  "sent": true,
  "message": "Test email sent successfully"
}
```

On failure:

```json
{
  "sent": false,
  "error": "Connection refused: smtp.example.com:587"
}
```

### Share Notifications

When files are shared via `POST /files/share`, email notifications are automatically sent to recipients if email is configured. If email is not configured, sharing still works -- notifications are silently skipped.
