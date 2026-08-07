# Observability

AeorDB exposes one bounded runtime snapshot through several operator surfaces:

| Surface | Audience | Format |
|---------|----------|--------|
| `GET /system/health` | Public load balancers and startup probes | Minimal health/startup state |
| `GET /system/stats` | Authenticated clients | Structured JSON; root-only paths are redacted for non-root users |
| `GET /system/metrics` | Root monitoring systems | Prometheus text |
| `GET /system/events?events=metrics` | Root live dashboards | SSE every 15 seconds |
| `aeordb status` | Root operators | Concise text or exact JSON |
| Browser Dashboard | Logged-in operators | Continuously collected visual state |

Stats, Prometheus, administrative SSE, CLI status, and the Dashboard use the same runtime-observability producer. Collection is bounded by fixed owner/property registries and bounded diagnostic arrays. It does not scan WAL entries, KV pages, file bodies, or index files, and it does not evict caches.

## Memory

Use the fields together instead of treating RSS alone as a leak detector:

- `memory.process.rss_bytes` is the operating system's resident-set measurement.
- `private_bytes`, `shared_bytes`, and `mapped_bytes` provide additional ownership evidence where supported. They overlap and must not be summed.
- `memory.coordinator.observed_bytes` is current memory attributed to fixed engine owners.
- `reserved_bytes` and `critical_reserved_bytes` are exact outstanding coordinator reservations.
- `accounted_bytes` combines coordinator observations and reservations according to coordinator accounting rules.
- `unaccounted_rss_bytes` is RSS not explained by current accounting. It can include allocator fragmentation, stacks, shared libraries, runtime overhead, and platform-probe differences; growth over time is a signal to investigate, not proof by itself.
- `pressure` is `unconfigured`, `normal`, `soft`, or `hard`. `maintenance_paused` shows whether background work is held back to protect serving/durability headroom.
- `owners[]` reports resident, clean, dirty, evictable, pinned, spill, item, hit/miss/eviction, and reservation evidence for each frozen memory owner.

Optional process fields are `null` in JSON and `NaN` in Prometheus when the platform cannot supply trustworthy evidence. AeorDB does not invent zeroes for unsupported ownership probes.

## Durability

The durability section separates admission/execution state from repair policy:

- `frontier.hard_frontier` is the highest contiguous hard sequence proven durable.
- `next_sequence`, `waiter_depth`, `pending_hard`, and `oldest_waiter_age_ms` expose queue pressure.
- `last_barrier` retains exactly one bounded success/failure/unwind observation with operation, sequence range, waiter count, attempts, latency, completion time, and bounded error evidence.
- `group_policy` shows whether hard commits are grouped and the active byte/time bounds. A malformed grouping policy disables grouping; it does not disable durability.
- `latch.read_only` is the write-safety decision. Runtime or persistent recovery evidence explains why it is latched.
- `spill` reports bounded hot-tail preservation evidence. Paths and unstructured diagnostic strings are root-only; non-root stats preserve state and numeric evidence while redacting text that could contain host paths.
- `repair` reports whether explicit repair is required and supplies the root-only command when available.

A rising waiter age with a stationary hard frontier indicates durability-path starvation. A read-only latch or required repair is a correctness state, not merely a performance alert.

## Configuration

Both `configuration.runtime` and `configuration.lifecycle` use the same envelope as their dedicated root APIs:

- `config` is the active effective document.
- `status.desired_config` is the validated desired document.
- `status.sources` and `desired_sources` identify the exact source per property.
- `valid`, `degraded`, `issues`, `disabled_capabilities`, and `convergence_errors` describe whether the family is usable and complete.
- `pending_restart` contains startup-bound changes that are durable but not active in this process.
- `pending_convergence` contains dynamic changes awaiting their owning subsystem's acknowledgement.

Registered root-only paths are represented as `{"redacted":true}` for non-root stats callers. Their source names remain visible so clients can diagnose precedence without learning host filesystem layout.

## Dashboard

Metrics collection begins after login, before the Dashboard tab is opened. Root sessions use the administrative metrics SSE stream after an initial stats fetch. Non-root sessions poll `/system/stats` every 15 seconds because metrics SSE is root-only. A malformed or failed root SSE stream is closed, surfaced as an error, and replaced with polling.

The Dashboard shows:

- current memory pressure and accounting;
- durability writability, frontier, waiters, and last barrier;
- recovery/spill/repair state;
- runtime/lifecycle validity and pending activation counts;
- per-owner memory observations and reservations; and
- effective configuration values, exact sources, and activation state.

## Incident Workflow

1. Check public startup/readiness without credentials:

   ```bash
   curl https://files.example.org/system/health
   ```

2. Capture one root JSON snapshot:

   ```bash
   AEORDB_ROOT_KEY="$ROOT_KEY" aeordb status --target https://files.example.org --json > status.json
   ```

3. Compare RSS, accounted/unaccounted memory, owner growth, pressure, waiter age/frontier movement, latch/repair state, and configuration degradation.
4. Use Prometheus for trends and SSE for live transitions. Do not repeatedly restart a latched or corruption-suspected database before preserving the database, spill artifacts, and logs.
5. If `durability.repair.required` is true, review the reported artifacts and run the displayed repair command under the normal evidence-copy safety rules.

See [Admin Operations](../api/admin.md#monitoring), [Events](../api/events.md#metrics-event), [CLI Commands](../cli/commands.md#aeordb-status), and [Deployment Safety](./deployment-safety.md).
