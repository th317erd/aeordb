# AeorDB — Sprint 3: Metrics & Observability

---

## Sprint Goal

Bake a high-performance metrics system directly into the database engine. Record fast, analyze later. Near-zero overhead on the hot path. Prometheus-compatible export for external analysis.

---

## Design Principles

1. **Record fast, analyze later.** Atomic increments and pre-allocated histograms on the hot path. No allocations, no locks, no I/O during recording.
2. **Always on.** Metrics are not optional. They're part of the engine, not a plugin.
3. **Prometheus-compatible.** Industry standard. Works with Grafana, AlertManager, and every monitoring stack.
4. **Structured.** Every metric has labels (path, operation, status) for slicing and dicing.
5. **Comprehensive.** Every layer instrumented: storage, filesystem, HTTP, auth, plugins, versioning.

---

## Implementation

### Dependencies

```toml
metrics = "0.24"
metrics-exporter-prometheus = "0.16"
```

The `metrics` crate provides macros (`counter!`, `gauge!`, `histogram!`) that record to a global recorder. The prometheus exporter renders them as text at an HTTP endpoint. Recording is lock-free atomic operations — near-zero overhead.

<!-- WYATT: These are the standard Rust metrics crates. The `metrics` crate is maintained by the same team as `tracing`. Alternatives exist but this is the ecosystem winner. Any concerns? -->

### Metrics Module

New module: `aeordb-lib/src/metrics/`

#### metrics/mod.rs
- `initialize_metrics()` — installs the Prometheus recorder, returns the endpoint handler
- Re-exports common metric helpers

#### metrics/storage_metrics.rs
Instrumentation for the chunk store and redb operations:

```
aeordb_chunks_stored_total          (counter)   — total chunks written
aeordb_chunks_read_total            (counter)   — total chunks read
aeordb_chunks_deduplicated_total    (counter)   — chunks skipped due to dedup
aeordb_chunk_store_bytes_total      (gauge)     — total bytes in chunk store
aeordb_chunk_store_count            (gauge)     — total chunk count
aeordb_chunk_write_duration_seconds (histogram)  — time to write a chunk
aeordb_chunk_read_duration_seconds  (histogram)  — time to read a chunk
aeordb_redb_transaction_duration_seconds (histogram) — redb transaction latency
aeordb_redb_table_count             (gauge)     — number of redb tables
```

#### metrics/filesystem_metrics.rs
Instrumentation for path resolution and file operations:

```
aeordb_path_resolve_duration_seconds  (histogram) — path traversal time, label: depth
aeordb_file_store_duration_seconds    (histogram) — total file store time
aeordb_file_read_duration_seconds     (histogram) — total file read time (first chunk to last)
aeordb_file_delete_duration_seconds   (histogram) — file deletion time
aeordb_directory_list_duration_seconds (histogram) — directory listing time
aeordb_directories_created_total      (counter)   — directories auto-created (mkdir-p)
aeordb_files_stored_total             (counter)   — total files stored
aeordb_files_read_total               (counter)   — total files read
aeordb_files_deleted_total            (counter)   — total files deleted
aeordb_file_bytes_stored_total        (counter)   — total bytes written to files
aeordb_file_bytes_read_total          (counter)   — total bytes read from files
```

#### metrics/http_metrics.rs
Instrumentation for HTTP request handling:

```
aeordb_http_requests_total            (counter)   — total requests, labels: method, path_pattern, status
aeordb_http_request_duration_seconds  (histogram) — request latency, labels: method, path_pattern
aeordb_http_request_bytes_total       (counter)   — total request body bytes received
aeordb_http_response_bytes_total      (counter)   — total response body bytes sent
aeordb_http_active_connections        (gauge)     — current active connections
```

Implement as a tower middleware layer so it instruments ALL routes automatically.

#### metrics/auth_metrics.rs
```
aeordb_auth_validations_total         (counter)   — JWT validations, label: result (success/failure)
aeordb_auth_token_exchanges_total     (counter)   — API key → JWT exchanges, label: result
aeordb_auth_rate_limit_hits_total     (counter)   — rate limit rejections
```

#### metrics/plugin_metrics.rs
```
aeordb_plugin_invocations_total       (counter)   — plugin executions, labels: path, type (wasm/native)
aeordb_plugin_duration_seconds        (histogram) — plugin execution time
aeordb_plugin_fuel_consumed           (histogram) — WASM fuel consumed per invocation
aeordb_plugin_memory_bytes            (histogram) — WASM memory used per invocation
aeordb_plugin_errors_total            (counter)   — plugin execution errors, label: error_type
```

#### metrics/version_metrics.rs
```
aeordb_version_snapshots_total        (counter)   — versions created
aeordb_version_restores_total         (counter)   — version restores
aeordb_version_snapshot_duration_seconds (histogram) — snapshot creation time
aeordb_version_restore_duration_seconds (histogram) — restore time
aeordb_version_count                  (gauge)     — current number of saved versions
```

### HTTP Endpoint

`GET /admin/metrics` — returns Prometheus text format. Exempt from auth (or behind a separate metrics auth token — debatable).

<!-- WYATT: Should /admin/metrics require auth? Pro: security. Con: Prometheus scraping is simpler without auth. Most databases expose metrics without auth on a separate port. Options: 1) No auth on metrics, 2) Auth required, 3) Separate port for metrics. What's your preference? -->

### Instrumentation Points

The recording calls go directly into the existing code at key points. Examples:

```rust
// In chunk_store.rs store():
let start = std::time::Instant::now();
// ... store the chunk ...
histogram!("aeordb_chunk_write_duration_seconds").record(start.elapsed().as_secs_f64());
counter!("aeordb_chunks_stored_total").increment(1);
```

```rust
// In path_resolver.rs store_file():
let start = std::time::Instant::now();
// ... resolve path, store chunks, create entry ...
histogram!("aeordb_file_store_duration_seconds").record(start.elapsed().as_secs_f64());
counter!("aeordb_files_stored_total").increment(1);
counter!("aeordb_file_bytes_stored_total").increment(data.len() as u64);
```

```rust
// As a tower middleware for HTTP:
// Wrap every request, record method + path_pattern + status + duration
```

### Stress Testing Tool

In addition to the metrics system, create a simple stress testing tool in aeordb-cli:

```
aeordb stress --target http://localhost:3000 \
  --concurrency 50 \
  --duration 30s \
  --operation write \
  --file-size 1kb
```

This generates load and reports throughput/latency stats. Combined with the /admin/metrics endpoint, gives full visibility into performance under pressure.

<!-- WYATT: The stress tool is a separate concern from the metrics system itself. Should I include it in this sprint, or defer it? My instinct is to build the metrics system first, then the stress tool as a follow-up. But they're both small. -->

---

## Tests

```
spec/metrics/
  metrics_spec.rs
    - test_metrics_endpoint_returns_prometheus_format
    - test_chunk_write_increments_counter
    - test_chunk_read_increments_counter
    - test_file_store_records_duration
    - test_http_request_records_metrics
    - test_auth_validation_records_metrics
    - test_plugin_invocation_records_metrics
    - test_metrics_labels_correct
    - test_gauge_reflects_current_state
    - test_histogram_records_timing
```

---

## Questions for Wyatt

1. **Metrics auth** — require auth on /admin/metrics, or leave open for easy Prometheus scraping?
2. **Stress tool** — include in this sprint or defer?
3. **Anything missing** from the metric set?

---

*Waiting for feedback...*
