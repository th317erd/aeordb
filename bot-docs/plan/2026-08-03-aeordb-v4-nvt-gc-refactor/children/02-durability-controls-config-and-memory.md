# Child 02: Durability, Controls, Configuration, and Memory

**Parent:** [AeorDB V4, NVT, GC, and Migration Campaign](../../2026-08-03-aeordb-v4-nvt-gc-refactor.md)
**Landing units:** P2a and P2b
**Status:** Starts after Child 01 P1 readers and platform probes are green
**Primary owners:** durability owner and runtime-resource owner
**Deployment shape:** deployable v3 safety phase; no v4 authority activation

## 1. Outcome

Consolidate current v3 writes behind one truthful durability path, make serious
failures latch the database read-only with recoverable spill handling, resolve
configuration property-by-property through one strict owner, and bound all
material memory consumers under process-wide admission and eviction.

This child deliberately improves production safety before v4 migration. It
must preserve v3 persistent bytes except for the specifically approved
system-flagged transition controls.

## 2. Owned Territory

- append, hot-tail, transaction, header, and shutdown write plumbing;
- platform durability adapters supplied by Child 01;
- new `DurabilityCoordinator` and hard-frontier waiter ownership;
- protected v3 transition ControlStore payloads for durability latch, emergency
  spill, repair ticket, path latch, LKG, and diagnostics;
- lifecycle/runtime configuration parsing and source resolution;
- memory coordinator, reservations, owner accounting, pressure policy;
- KV snapshot/page residency, directory/index/cache admission and eviction;
- health, metrics, administrative SSE, CLI status, and Dashboard resource
  diagnostics; and
- related focused specs and real `/tmp` workload harnesses.

Forbidden without handoff:

- changing frozen v4 bytes or registry IDs;
- changing namespace authority semantics owned by Child 03;
- implementing GC/index/query algorithms;
- writing a v4 header or capability; and
- hiding a durability failure to retain availability.

## 3. P2a: One Durability Coordinator

### Contract

Every acknowledged write declares its commit class and joins one coordinator.
The coordinator owns dependency append, data barrier, authority write,
authority barrier, read-back, A/B publication, parent-directory sync, waiter
wake, retry classification, latch transition, spill, and shutdown drain.

Success means the caller's exact authority is at or below the proven hard
frontier. Group commit may coalesce barriers but may not wake a waiter early.

### Work

1. Inventory every call to write, flush, sync, rename/replace, header update,
   transaction end, shutdown flush, and ignored durability result.
2. Add `DurabilityCoordinator` and platform adapter interfaces without changing
   writers; characterize current operation ledgers first.
3. Route dependency and authority publication through the coordinator in
   narrow waves. Preserve dependency-first/selector-last ordering.
4. Add bounded retries only for errors with a meaningful recovery path:
   interrupted-no-progress, selected transient I/O, and explicitly retryable
   handle states. Persistent ENOSPC/quota/read-only/permission/media/device/
   unsupported/checksum failures do not loop indefinitely.
5. On a serious failure, atomically enter database-wide read-only latch,
   preserve the first and latest evidence, stop new writes, keep reads/status/
   repair available where safe, and prevent restart from clearing the latch.
6. Attempt to spill unpersisted engine-owned dirty bytes first to the platform
   user-state location, then configured fallback, then OS temporary storage as
   last resort. Every spill has length, complete digest, order, identity, and
   no-follow path validation.
7. Startup scans every approved spill location. Any artifact aborts normal
   writable startup with the exact repair command. Repair orders artifacts
   oldest first, prompts unless `--yes`, replays idempotently, then runs normal
   verification/repair before clearing state.
8. Implement graceful shutdown admission: stop new writes, drain admitted
   writes and hard waiters, checkpoint/flush bounded dirty work, then close.
   Reads follow the separately bounded shutdown policy and never block status.
9. Make deploy/install inspect transition controls and refuse an older binary
   while latch/spill/repair state is active.

### Serious Failure Classification

The latch is reserved for failures that make subsequent write durability
untrustworthy: persistent no-space/quota, read-only filesystem, permission loss,
media/device I/O, lost device/handle authority, unsupported required durability,
short writes, read-back/checksum mismatch, ambiguous timeout after write, or a
failed required directory/data barrier. Retryable transient errors remain
typed and bounded; ordinary parser/index failures do not latch user storage.

## 4. P2b: Strict Configuration Resolver

### Precedence

Resolve each property independently in this order:

1. CLI override;
2. environment override;
3. valid stored value;
4. valid last-known-good stored value where the property's policy permits; then
5. compiled default.

An invalid higher-present source is an error and never silently falls through.
A valid CLI/environment value may supply a property even when the lower stored
document is missing or malformed; the lower-source degradation remains visible.

### Work

1. Freeze a complete property registry containing type, range, default,
   environment name, CLI name, activation class, redaction, LKG eligibility,
   and owner.
2. Replace module-local environment/default parsing with one resolver.
3. Parse stored JSON strictly: duplicate, unknown, invalid, overflow, malformed,
   and trailing content fail the complete proposed write.
4. Implement root-only `GET`, replacement `PUT`, and RFC 7396 `PATCH` for
   `/system/runtime` and `/system/lifecycle` through coordinated publication.
5. Return complete effective values and their sources, stored validity, LKG
   identity, pending restart/convergence, degradation, and disabled capability.
6. Keep CLI/environment overrides ephemeral; never write them into JSON.
7. Preserve documented transitional names only through one reviewed adapter and
   remove them at the parent plan's deletion gate.

### Frozen Defaults Owned Here

- global memory auto defaults, with the 16 GiB reference profile at 6 GiB soft
  and 8 GiB hard;
- pending-delete grace 86,400 seconds (24 hours), with explicit zero valid and
  exactly two required complete marks as a non-writable engine invariant;
- root expiry optional retention 30 days and optional budget 256 MiB;
- root lifecycle hard maximum 1 GiB;
- migration capture 64 GiB, checkpoint 300 seconds, and capacity-clamped free
  reserve; and
- GC scratch/checkpoint controls defined by Child 04.

## 5. Process-Wide Memory Coordinator

### Owners

Account at minimum:

- resident KV pages and generations;
- write/transaction buffers and hard waiters;
- directory/B-tree caches;
- index pages, NVT tiles, values, postings, memtables, and compaction;
- query candidates, sort/aggregate state, and request pins;
- GC bitmap/frontier/runs/checkpoints;
- parser/plugin input/output amplification;
- task/migration/backup/repair buffers;
- allocator/RSS remainder; and
- emergency spill/shutdown/status headroom.

### Policy

1. Every material growth reserves before allocation.
2. Soft pressure evicts clean/rebuildable caches, shrinks or spills bounded
   buffers, and defers new maintenance.
3. Hard pressure rejects new memory-amplifying work while retaining health,
   metrics, streaming reads, small durable writes, spill, and shutdown capacity.
4. Dirty state flushes or spills before eviction. Clean derived state may be
   dropped without correctness change.
5. Reservation hierarchy and lock order are explicit and tested. A subsystem
   may not recursively reserve while holding a lock that its eviction callback
   needs.
6. Metrics reconcile coordinator-owned bytes with RSS, private dirty/clean,
   mapped pages, allocator stats, and unaccounted remainder.
7. Inactive indexes add negligible resident data. Startup loads registries and
   selected roots, not complete postings/NVT/value stores.

## 6. KV and Cache Corrections

- Remove full fixed-page snapshots and per-write rebuilding of complete type
  indexes from hot paths.
- Keep flushed immutable page metadata shareable; publish write-buffer deltas
  without rescanning the corpus.
- Admit KV pages lazily and evict clean pages under the shared budget.
- Make directory and index caches byte-bounded with LRU/TTL behavior and request
  pins; no configured-but-unused index remains permanently resident.
- Expose per-owner hit/miss/eviction/pin/dirty/spill bytes and age.
- Prove dropping every clean cache reduces RSS near reopened baseline while
  results and authoritative state remain identical.

## 7. Observability

Health, metrics, status, administrative SSE, and Dashboard expose:

- RSS, virtual, shared, private, allocator, mapped, and unaccounted bytes;
- soft/hard limits, reservations, rejected/deferred work, and emergency reserve;
- bytes/items/hits/misses/evictions/pins/dirty/spill per owner;
- durability frontier, waiter depth/age, last barrier latency and error;
- latch state, spill locations/count/bytes, repair command and progress;
- complete config values, source, validity, LKG, restart/convergence state; and
- health endpoint latency without scanning storage on demand.

Metric collection starts after authenticated web login so Dashboard navigation
does not wait for a cold first sample.

## 8. Landing Sequence

1. P2a-1: operation-ledger characterization and coordinator shell.
2. P2a-2: hard-frontier group commit and platform failure matrix.
3. P2a-3: latch, spill, startup discovery, repair, shutdown, downgrade gate.
4. P2b-1: property registry and strict resolver in diagnostic shadow mode.
5. P2b-2: memory coordinator and owner accounting with current behavior adapter.
6. P2b-3: KV/cache owners migrate to real reservations and eviction.
7. P2b-4: runtime/lifecycle APIs, CLI, metrics, SSE, Dashboard.
8. P2 exit: real v3 server, stress, dirty restart, verify, and deployment-ready
   evidence.

Each numbered unit is a separate green commit/revert boundary.

## 9. Verification

Required campaign target:

```bash
timeout 10m cargo test -j 6 -p aeordb --test v3_contract_facade_spec
```

At minimum retain and expand:

```bash
timeout 10m cargo test -j 6 -p aeordb --test append_writer_spec
timeout 10m cargo test -j 6 -p aeordb --test hot_file_transaction_spec
timeout 10m cargo test -j 6 -p aeordb --test shutdown_spec
timeout 10m cargo test -j 6 -p aeordb --test lifecycle_config_spec
timeout 10m cargo test -j 6 -p aeordb --test kv_snapshot_spec
timeout 10m cargo test -j 6 -p aeordb --test cache_and_hardlinks_spec
timeout 10m cargo test -j 6 -p aeordb --test metrics_spec
timeout 10m cargo test -j 6 -p aeordb --test health_spec
timeout 10m cargo test -j 6 -p aeordb-cli --test crash_inject_spec
```

Fault tests interrupt every barrier, authority write, A/B publication,
read-back, waiter wake, latch, spill record, replay, and clear transition.

## 10. Real-World and Resource Proof

1. Start a real authenticated v3 server below `/tmp/codex`.
2. Exercise HTTP and embedded PUT, blob commit, merge, raw batch, sync, plugin,
   backup/restore, task, repair, and shutdown paths.
3. Inject retryable and serious platform failures and verify response, latch,
   spill, restart refusal, ordered repair, and final verification.
4. Run upload + query + index + maintenance under an 8 GiB cgroup with swap
   disabled. Health/status must remain schedulable.
5. Force clean-cache eviction and compare outputs before/after.
6. Dirty-kill/restart repeatedly and run `verify` on a copy after each seeded
   cycle.
7. Record latency/RSS/I/O and confirm unaffected p50/p95 remain within the
   parent's regression envelope.

## 11. Rollback

P2 remains v3-compatible, but persistent transition controls carry safety
authority. Rollback uses a P2-compatible binary while any latch, spill, repair,
or path-latch state exists. An older writer that ignores those controls is not
supported. Clearing state requires the approved repair/probe evidence, never
manual deletion or restart.

## 12. Definition of Done

- [ ] Every acknowledged v3 write is behind one hard-frontier coordinator.
- [ ] No required sync/rename/read-back error is warning-success.
- [ ] Retry classification is bounded and evidence-backed.
- [ ] Serious failures latch read-only and preserve dirty state best-effort.
- [ ] Startup discovers all spill locations and requires ordered repair.
- [ ] Config resolution is per-property, strict, source-visible, and complete.
- [ ] Valid CLI/environment overrides work despite malformed lower storage.
- [ ] Every material memory owner reserves before growth.
- [ ] Clean caches evict and materially lower RSS without result change.
- [ ] The reference workload stays below 8 GiB with swap disabled.
- [ ] Health/status remain responsive under stress and failure.
- [ ] Real `/tmp` reopen and verify pass with a preserved operation ledger.
- [ ] No v4 authority or capability is emitted by this child.
