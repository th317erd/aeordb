# Production Readiness — Error Handling, Observability, Resilience

**Date:** 2026-04-17
**Status:** Design spec
**Goal:** Button down all hatches before real-world use

---

## Overview

AeorDB needs comprehensive error handling, failure visibility, and graceful degradation before production use. The core engine is solid (2,920 tests, audit fixes applied), but the operational layer — how failures are surfaced, retried, and recovered from — needs work.

---

## 1. Sync Failure Observability

### Current State
Sync failures are logged as `warn!` and silently retried on the next tick. No user-facing signal. No metrics. No events.

### What's Needed

**Per-peer sync status in admin API:**
```
GET /admin/cluster
```
Response includes per-peer sync health:
```json
{
  "peers": [
    {
      "node_id": 2,
      "address": "https://node2:3000",
      "state": "active",
      "sync_status": {
        "last_success_at": 1776300000000,
        "last_attempt_at": 1776300030000,
        "last_error": null,
        "consecutive_failures": 0,
        "total_syncs": 147,
        "total_failures": 3
      }
    },
    {
      "node_id": 3,
      "address": "https://node3:3000",
      "state": "active",
      "sync_status": {
        "last_success_at": 1776299900000,
        "last_attempt_at": 1776300030000,
        "last_error": "Failed to store file /assets/big.psd: No space left on device",
        "consecutive_failures": 4,
        "total_syncs": 130,
        "total_failures": 7
      }
    }
  ]
}
```

**Event bus emissions on sync failures:**
```rust
// New event type
pub const EVENT_SYNC_FAILED: &str = "sync_failed";
pub const EVENT_SYNC_SUCCEEDED: &str = "sync_succeeded";

// Payload
{
  "peer_node_id": 3,
  "peer_address": "https://node3:3000",
  "error": "Failed to store file: No space left on device",
  "consecutive_failures": 4,
  "will_retry_in_secs": 120
}
```

SSE subscribers and webhooks receive these — enabling external alerting (PagerDuty, Slack, email).

**Prometheus metrics:**
```
aeordb_sync_cycles_total{peer="node3", result="success"} 130
aeordb_sync_cycles_total{peer="node3", result="failure"} 7
aeordb_sync_duration_seconds{peer="node3"} 0.45
aeordb_sync_consecutive_failures{peer="node3"} 4
```

**Dashboard Nodes section:**
Each peer card shows:
- Last sync time + status (green checkmark or red X)
- Consecutive failure count
- Last error message (if any)
- Retry countdown

---

## 2. Retry Strategy

### Current State
Fixed interval retry — every `interval_secs` regardless of failure count.

### What's Needed

**Exponential backoff with jitter:**

```
attempt 1: retry in 30s
attempt 2: retry in 60s
attempt 3: retry in 120s
attempt 4: retry in 240s
attempt 5+: retry in 300s (cap)
```

Plus random jitter (±10%) to prevent thundering herd when multiple peers reconnect.

**Reset on success:** One successful sync resets the backoff to base interval.

**Implementation:**
```rust
pub struct SyncRetryState {
    pub consecutive_failures: u32,
    pub last_attempt_at: u64,
    pub last_error: Option<String>,
    pub base_interval_secs: u64,
    pub max_interval_secs: u64,
}

impl SyncRetryState {
    pub fn next_retry_interval(&self) -> u64 {
        if self.consecutive_failures == 0 {
            return self.base_interval_secs;
        }
        let backoff = self.base_interval_secs * 2u64.pow(
            (self.consecutive_failures - 1).min(5)
        );
        let capped = backoff.min(self.max_interval_secs);
        // Add ±10% jitter
        let jitter = (capped as f64 * 0.1 * (rand::random::<f64>() * 2.0 - 1.0)) as i64;
        (capped as i64 + jitter).max(1) as u64
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.last_error = None;
    }

    pub fn record_failure(&mut self, error: String) {
        self.consecutive_failures += 1;
        self.last_error = Some(error);
        self.last_attempt_at = chrono::Utc::now().timestamp_millis() as u64;
    }
}
```

---

## 3. Comprehensive Health Endpoint

### Current State
`GET /admin/health` returns `{"status": "ok"}` — always, regardless of actual health.

### What's Needed

**Deep health check:**
```
GET /admin/health
```

```json
{
  "status": "healthy",
  "checks": {
    "engine": {
      "status": "ok",
      "entry_count": 15432,
      "db_file_size_bytes": 52428800
    },
    "disk": {
      "status": "ok",
      "available_bytes": 107374182400,
      "total_bytes": 214748364800,
      "usage_percent": 50.0
    },
    "sync": {
      "status": "degraded",
      "active_peers": 2,
      "failing_peers": 1,
      "details": "Peer node3 has 4 consecutive sync failures"
    },
    "auth": {
      "status": "ok",
      "mode": "self-contained",
      "signing_key_present": true
    }
  },
  "uptime_seconds": 86400,
  "version": "0.1.0"
}
```

**Status values:**
- `healthy` — everything working
- `degraded` — working but with issues (sync failures, high disk usage)
- `unhealthy` — critical problems (no signing key, disk full, engine error)

**Thresholds:**
- Disk > 90% → degraded
- Disk > 98% → unhealthy
- Any peer with > 10 consecutive sync failures → degraded
- No signing key in cluster mode → unhealthy
- Engine can't read/write → unhealthy

---

## 4. Graceful Degradation

### Disk Full
**Current behavior:** `store_entry` returns `IoError`, caller gets 500.
**Desired behavior:**
- Detect disk space on startup and periodically
- When disk > 95%, emit a warning event
- When disk > 98%, reject new writes with `507 Insufficient Storage` instead of generic 500
- Continue serving reads
- Continue sync (pull-only — don't try to write)
- Health endpoint reports `unhealthy`

### Network Failures (Sync)
**Current behavior:** Sync fails, retries next tick.
**Desired behavior:**
- Exponential backoff (section 2)
- After N failures, transition peer to `Degraded` state (new ConnectionState variant)
- Continue heartbeat exchange during Degraded (so we detect recovery)
- On successful heartbeat response, transition back to Active and attempt sync

### Corrupt Data
**Current behavior:** Hash verification on reads returns CorruptEntry error. Caller gets 500.
**Desired behavior:**
- Log the corruption at ERROR level with entry offset and hash
- Emit a `data_corruption_detected` event
- Return 500 with a clear message: "Data corruption detected. Run GC to scan and report."
- Don't crash — continue serving other requests
- Consider: mark the entry as "suspect" in the KV store so it's flagged in future reads

<!--
If the node is part of a replication group, then the chunk could be marked for "refetch"... maybe your idea of marking it as "suspect" somehow is a good idea... suspect chunks can be checked against other nodes? 
 -->

### Out of Memory
**Current behavior:** Allocation panics (vec![0u8; huge_value]).
**Fixed behavior (from audit):** Length validation prevents this.
**Additional:** Consider setting a process-level memory limit via cgroups/ulimit, and document it.

---

## 5. Startup Validation

Before accepting ANY traffic (client or peer), the server should validate:

1. **Database file opens cleanly** — file header valid, KV index loads
2. **Signing key present** (if auth enabled) — can create/verify JWTs
3. **System data intact** — `/.system/` directory exists and is readable
4. **Disk space adequate** — at least 100MB free (configurable)
5. **Hot file replayed** — if hot file exists, replay before accepting writes

If any check fails, the server should:
- Log the specific failure
- Return `unhealthy` from the health endpoint
- Reject all non-health-check requests with 503

---

## 6. Shutdown Behavior

### Current State
No graceful shutdown. Process kill drops everything.

### What's Needed

**Graceful shutdown sequence:**
1. Stop accepting new connections
2. Complete in-flight requests (with a timeout, e.g. 30s)
3. Abort the sync loop (wait for current cycle to finish, don't start new ones)
4. Flush KV store (write buffer → disk)
5. Flush hot file buffer
6. Sync all data to disk (fsync)
7. Close file handles
8. Exit

**Signal handling:** Catch SIGTERM and SIGINT, trigger graceful shutdown.

**Implementation:** Use `tokio::signal` + a `CancellationToken` that propagates to all background tasks.

---

## 7. Operational Logging

### Current State
Logging is ad-hoc. Some operations log, many don't. Log levels are inconsistent.

### What's Needed

**Structured logging on key operations:**

| Operation | Level | Fields |
|-----------|-------|--------|
| Server startup | INFO | port, auth_mode, db_path, version |
| Server shutdown | INFO | reason, uptime |
| File stored | DEBUG | path, size, content_type |
| File deleted | DEBUG | path |
| Sync started | DEBUG | peer, direction |
| Sync completed | INFO | peer, operations, conflicts, duration_ms |
| Sync failed | WARN | peer, error, consecutive_failures |
| Conflict detected | INFO | path, conflict_type, winner |
| Auth failure | WARN | reason, ip (if available) |
| Disk space warning | WARN | available_bytes, usage_percent |
| Data corruption | ERROR | offset, hash, entry_type |
| GC completed | INFO | reclaimed_bytes, entries_swept, duration_ms |
| Peer connected | INFO | peer, node_id |
| Peer disconnected | WARN | peer, reason |
| Honeymoon settled | INFO | peer, heartbeats, offset_ms |

---

## Implementation Phases

### Phase 1: Sync Status Tracking
- Add `SyncRetryState` to PeerConnection
- Record success/failure after each sync cycle
- Exponential backoff in sync loop
- Include sync status in `GET /admin/cluster` response
- Emit sync_failed/sync_succeeded events

### Phase 2: Health Endpoint Enhancement
- Deep health check (engine, disk, sync, auth)
- Status values (healthy/degraded/unhealthy)
- Disk space monitoring
- Startup validation checks

### Phase 3: Graceful Shutdown
- Signal handling (SIGTERM, SIGINT)
- CancellationToken propagation to background tasks
- Ordered shutdown sequence (flush, sync, close)
- Shutdown timeout

### Phase 4: Operational Improvements
- Structured logging consistency
- Dashboard sync status display
- Prometheus metrics for sync
- Disk space warnings

---

## Out of Scope (For Now)

- Automatic remediation (auto-GC on low disk)
- External alerting integrations (Slack, PagerDuty)
- Distributed tracing (OpenTelemetry)
- Log aggregation (structured JSON output exists, but no shipping)
- Rate limiting on sync endpoints (noted in audit, deferred)
