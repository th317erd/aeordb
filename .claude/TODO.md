# AeorDB — TODO

## Current: Sprint 3 — Metrics & Observability

### Task 1: Metrics System
- [ ] Add metrics + metrics-exporter-prometheus dependencies
- [ ] Metrics module with initialize_metrics()
- [ ] Storage metrics (chunk read/write counters, durations, dedup ratio)
- [ ] Filesystem metrics (path resolve, file store/read/delete durations)
- [ ] HTTP metrics (requests/sec, latency histograms, status codes, bytes)
- [ ] Auth metrics (validations, token exchanges, rate limit hits)
- [ ] Plugin metrics (invocations, duration, fuel, memory, errors)
- [ ] Version metrics (snapshots, restores, durations)
- [ ] GET /admin/metrics endpoint (Prometheus format, auth required, rate limited)
- [ ] Instrument all existing code with recording calls
- [ ] Tests

### Task 2: Stress Testing Tool
- [ ] aeordb-cli stress subcommand
- [ ] Configurable: concurrency, duration, operation type, file size
- [ ] Write stress (store files at random paths)
- [ ] Read stress (read files at random paths)
- [ ] Mixed stress (read + write)
- [ ] Report: throughput, latency p50/p95/p99, errors
- [ ] Tests

## Previous Sprints (Complete)
- [x] Sprint 1: Phases 1-4 (storage, HTTP, auth, plugins, indexing, replication)
- [x] Sprint 2: redb-native filesystem (directories, path resolver, versioning, streaming HTTP)
- Test count: 486 (all passing)
