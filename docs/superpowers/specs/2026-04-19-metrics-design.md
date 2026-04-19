# AeorDB Enhanced Metrics — Design Spec

**Date:** 2026-04-19
**Status:** Approved
**Scope:** Atomic counters, rolling rate computation, stats API, dashboard, Prometheus

---

## Overview

Replace the O(n) stats computation with O(1) atomic counters. Add rolling throughput rates, latency histograms, and health signals for operators and future auto-tuning. Expose via stats API, Prometheus, heartbeat SSE, and dashboard.

---

## 1. Atomic Counters

Replace the current `iter_all()` + type-counting approach with `AtomicU64` counters maintained in the `StorageEngine`. Counters are incremented/decremented in real-time during operations. GC periodically reconciles counters against the authoritative KV snapshot to correct any drift.

### Counter struct

```rust
pub struct EngineCounters {
    // Counts
    pub files: AtomicU64,
    pub directories: AtomicU64,
    pub symlinks: AtomicU64,
    pub chunks: AtomicU64,
    pub snapshots: AtomicU64,
    pub forks: AtomicU64,

    // Sizes (bytes)
    pub logical_data_size: AtomicU64,    // sum of all file sizes
    pub chunk_data_size: AtomicU64,      // actual bytes in chunk entries
    pub void_space: AtomicU64,           // reclaimable

    // Throughput tracking
    pub writes_total: AtomicU64,         // monotonic counter
    pub reads_total: AtomicU64,          // monotonic counter
    pub bytes_written_total: AtomicU64,  // monotonic counter
    pub bytes_read_total: AtomicU64,     // monotonic counter

    // Dedup tracking
    pub chunks_deduped_total: AtomicU64, // chunks that already existed

    // Write buffer
    pub write_buffer_depth: AtomicU64,   // current buffered writes pending flush
}
```

### Increment/decrement points

| Counter | Increment | Decrement |
|---------|-----------|-----------|
| files | `store_file` | `delete_file` |
| directories | `update_parent_directories` (new dir) | `remove_from_parent_directory` (empty dir removed) |
| symlinks | `store_symlink` | `delete_symlink` |
| chunks | `store_entry(Chunk)` (when new) | GC sweep |
| snapshots | `snapshot_create` | `snapshot_delete` |
| forks | `fork_create` | `fork_abandon` |
| logical_data_size | `store_file` (+file_size) | `delete_file` (-file_size) |
| chunk_data_size | `store_entry(Chunk)` (+chunk_bytes) | GC sweep (-freed_bytes) |
| void_space | `delete_entry` (+entry_size) | GC sweep / void reuse (-reclaimed) |
| writes_total | `store_file`, `store_symlink` | (monotonic, never decremented) |
| reads_total | `read_file`, `read_file_streaming` | (monotonic, never decremented) |
| bytes_written_total | `store_file` (+file_size) | (monotonic) |
| bytes_read_total | `read_file` (+file_size) | (monotonic) |
| chunks_deduped_total | `store_file` (chunk already existed) | (monotonic) |
| write_buffer_depth | `insert()` (+1) | `flush()` (reset to 0) |

### GC reconciliation

During GC sweep, after the authoritative count is known:
```rust
counters.files.store(authoritative_file_count, Ordering::Relaxed);
counters.chunks.store(authoritative_chunk_count, Ordering::Relaxed);
// etc.
```

### Initialization

On startup, compute initial counts from the KV snapshot (one-time O(n) scan) and set the atomics. This is the same cost as the current stats() call but only happens once.

---

## 2. Rolling Rate Computation

Server-side rate computation using a sliding window approach.

### Rate tracker

```rust
pub struct RateTracker {
    // Ring buffer of (timestamp_ms, count) samples taken every second
    samples: Mutex<VecDeque<(u64, u64)>>,
    max_samples: usize, // 900 = 15 minutes of 1-second samples
}

impl RateTracker {
    fn record(&self, count: u64);       // called every second by background task
    fn rate_1m(&self) -> f64;           // ops/sec over last 60 samples
    fn rate_5m(&self) -> f64;           // ops/sec over last 300 samples
    fn rate_15m(&self) -> f64;          // ops/sec over last 900 samples
    fn peak_1m(&self) -> f64;           // max 1-second rate seen in last 60s
}
```

### Tracked rates

| Rate | Source counter |
|------|---------------|
| writes/sec | `writes_total` delta |
| reads/sec | `reads_total` delta |
| bytes_written/sec | `bytes_written_total` delta |
| bytes_read/sec | `bytes_read_total` delta |

### Background sampler

A tokio task runs every 1 second, reads the monotonic counters, computes deltas, and pushes samples into the rate trackers. This is cheap — just 4 atomic loads and 4 VecDeque pushes per second.

---

## 3. Latency Tracking

Use the existing `metrics` crate histograms, but ensure they're actually instrumented.

### Histograms to instrument

| Metric | Where to instrument |
|--------|-------------------|
| Write latency | `store_file` in `directory_ops.rs` |
| Read latency | `read_file` / `read_file_streaming` in `directory_ops.rs` |
| Query latency | `execute` in `query_engine.rs` |
| Flush latency | `flush()` in `disk_kv_store.rs` |

### Percentiles

Computed from Prometheus histograms: p50, p95, p99. Exposed in stats API as pre-computed values using the `metrics` crate's summary capabilities, or computed from histogram buckets.

---

## 4. Stats API Response

`GET /system/stats` returns:

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
  "sync": {
    "active_peers": 2,
    "failing_peers": 0,
    "last_sync_ms": 1776563922032,
    "sync_lag_entries": { "peer_2": 0, "peer_3": 15 }
  }
}
```

---

## 5. Heartbeat Enhancement

The heartbeat event (every 15 seconds) should include a subset of the stats for real-time dashboard updates without requiring a full stats poll:

```json
{
  "counts": { "files": 150000, "chunks": 420000, ... },
  "sizes": { "disk_total": 2147483648, ... },
  "throughput": { "writes_per_sec": { "1m": 42.3 }, "reads_per_sec": { "1m": 156.2 } },
  "write_buffer_depth": 42
}
```

---

## 6. Prometheus Integration

All atomic counters and rates should also be exposed as Prometheus metrics at `GET /system/metrics`:

```
# Counts
aeordb_files_total 150000
aeordb_directories_total 23000
aeordb_symlinks_total 500
aeordb_chunks_total 420000
aeordb_snapshots_total 12
aeordb_forks_total 2

# Sizes
aeordb_disk_bytes 2147483648
aeordb_logical_data_bytes 1800000000
aeordb_chunk_data_bytes 1200000000
aeordb_void_space_bytes 5242880
aeordb_dedup_savings_bytes 600000000

# Throughput (monotonic counters — Prometheus computes rates)
aeordb_writes_total 892345
aeordb_reads_total 3456789
aeordb_bytes_written_total 91827364532
aeordb_bytes_read_total 345678901234

# Dedup
aeordb_chunks_deduped_total 234567

# Health signals
aeordb_write_buffer_depth 42
aeordb_kv_fill_ratio 0.72
aeordb_disk_usage_percent 48.5
```

---

## 7. Dashboard Updates

The portal dashboard should show:

### Top bar
- Version, database path, uptime, hash algorithm

### Stat cards
- Files, Directories, Symlinks, Chunks, Snapshots, Forks

### Size cards
- Disk total, Logical data, Chunk data, Dedup savings, Void space

### Throughput charts (live, rolling)
- Writes/sec line chart (1m rate from heartbeat)
- Reads/sec line chart (1m rate from heartbeat)
- Bytes written/sec
- Bytes read/sec

### Latency display
- Write/Read/Query p50/p95/p99

### Health indicators
- Disk usage bar
- KV fill ratio bar
- Dedup hit rate
- Write buffer depth
- Sync peer status (if replication active)

---

## 8. Implementation Notes

### What NOT to change
- The `HealthReport` from `GET /system/health` stays as-is — it's a status check, not metrics
- Prometheus histogram mechanics stay as-is — we just ensure they're instrumented
- The heartbeat interval stays at 15 seconds

### Performance budget
- `GET /system/stats` must be O(1) — no iteration, just atomic loads + rate tracker reads
- Background sampler: 1 second interval, <100 microseconds per tick
- Counter operations: single `fetch_add(1, Relaxed)` per operation — nanoseconds

### Migration
- On startup, do one-time O(n) scan to initialize counters from existing data
- After initialization, counters are maintained in real-time
- GC reconciliation happens naturally during GC runs
