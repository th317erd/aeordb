# AeorDB V4, NVT, GC, and Migration Campaign

**Campaign ID:** `aeordb-v4-nvt-gc-2026-08-03`
**Decision baseline:** `5d3e284652f9fec7a5c843f1946132574af4d469`
**Remote baseline:** `origin/development` matched the decision baseline on 2026-08-03
**Status:** Owner-ratified; formal implementation plan; implementation not started
**Owner ratification:** 2026-08-03, all Round 15 policy areas 1 through 10
**Normative decision source:** `.codex/conversation.md`, Rounds 10 through 15
**Supersedes:** `2026-07-16-nvt-field-index-refactor-and-migration.md` in full
**Formalization review:** [PASS](2026-08-03-aeordb-v4-nvt-gc-refactor/formalization-review.md)

> **Execution status (2026-09-03):** The header above preserves the plan's
> ratification-time state. Current mutable execution status lives in the
> [Child 08 progress ledger](2026-08-03-aeordb-v4-nvt-gc-refactor/progress/08-evidence.md).
> The safe repository implementation, exact-candidate native qualification,
> and canonical P9 evidence packet are complete. Copied-production, canary,
> installation/deployment, operational cutover/acceptance, first v4 write,
> monitoring, and destructive GC remain separately gated and unexecuted.

The ratified decision source is a required tracked campaign artifact. Before P0
starts, it must be committed alongside this plan and treated as append-only
decision history. Formal plans summarize execution; they do not replace exact
field layouts and rulings preserved there.

## 1. Target

Build AeorDB v4 as a side-by-side, crash-safe migration that preserves the
current content-addressed namespace while replacing unbounded field indexes
with immutable page-addressable indexes and a sparse, non-authoritative NVT;
separately harden root lifecycle, physical GC, Void reuse, durability,
configuration, memory ownership, historical reads, and bot-oriented range
fetching so correctness never depends on derived state.

The campaign is judged by observable behavior and mechanically reproducible
proof, not by module count. It succeeds only when:

- acknowledged authority survives every modeled crash;
- v3 remains untouched and recoverable until the explicit v4 write boundary;
- query results remain exact with every disposable index and NVT hint removed;
- GC uncertainty retains data or leaks space rather than reclaiming early;
- resident memory remains below the ratified hard budget under overlapping
  upload, query, indexing, and GC work;
- all supported platforms read the same persistent bytes and enforce the same
  capability rules; and
- a copied production database migrates, verifies, serves real clients, and
  survives dirty restart before any production cutover is permitted.

## 2. Authority and Precedence

When plan text conflicts, use this order:

1. Round 15 normative corrections and owner ratification;
2. Rounds 10 through 14 exact contracts;
3. Rounds 1 through 9 where not superseded;
4. this parent and its eight executable child plans;
5. incorporated historical plans for behavior explicitly preserved here; then
6. current implementation as characterization evidence, never as authority for
   a behavior already ratified as defective.

Exact persistent schemas remain frozen by the decision log until P0 copies
them into the machine contract registry and hand-authored fixtures. A worker
may not alter a field, width, tag, ordering rule, capability, error, or crash
direction merely because implementation is inconvenient. An impossible
contract stops work and reopens a numbered decision round.

## 3. Non-Negotiable Invariants

1. Migration never upgrades, repairs, tests, or shifts the production v3 file
   in place. It creates a distinct v4 physical database.
2. The v4 DatabaseHeader consists of two 1,024-byte A/B slots. Data begins at
   byte 2,048. Each v4 physical database has a nonzero
   `physical_instance_id`; logical database identity remains separate.
3. Authority publishes dependencies first and its selector last through one
   durability coordinator. False success is forbidden.
4. Namespace/content authority is separate from rebuildable indexes, NVT,
   caches, checkpoints, and soft mutation journals.
5. One admitted `NamespaceRootV1` binds one immutable namespace and one exact
   complete or content-only semantic world.
6. `root_hash` is an explicit read selector. It is not a base64 cursor, lease,
   capability, or stored server object. Every successful namespace read returns
   root state and advisory expiry metadata.
7. APOS carries logical pagination position only. Request `limit` remains a
   separate parameter. Old APOS tokens receive no dual decoder.
8. Current authorization is applied before historical existence, state,
   timing, count, snippet, position, aggregate, or EXPLAIN data is observable.
   Historical permissions may restrict current grants but never expand them.
9. Share credentials select current HEAD only unless a future explicit
   historical-share policy is separately designed and ratified.
10. Converter definitions own canonical equality, typed comparison, and fixed
    coordinates. Digest or coordinate collisions can add work but cannot create
    a false match.
11. Posting pages own ordered index data. NVT supplies sparse approximate page
    hints and conservative query ranges; missing, stale, sparse, resized, or
    corrupt NVT state cannot change results.
12. User writes never wait for parser execution, posting/NVT mutation, index
    publication, or soft mutation-journal sync.
13. Index coverage names an exact NamespaceRoot and exact definition/dependency
    identities. Partial coverage is usable only with a provably complete
    authoritative fallback.
14. Search-hit locations are computed on request by bounded source scans and
    preserve root/content identity through snippet and range fetch. Positional
    indexes are outside this campaign.
15. Directory listings place directories first in both directions and sort
    alphabetically within each category without page gaps or duplication.
16. Logical root retirement is separate from physical reclamation. Pending
    roots remain readable and remain physical mark roots. Retired roots return
    deterministic `410 Gone` before any locator or content byte is removed.
17. Physical reclaim requires two complete marks, frozen candidate grace, a
    final authority/pin/incarnation check, a durable sweep receipt, and
    receipt-backed Void publication.
18. Mandatory logical-retirement evidence remains until root-object physical
    reclaim is proven. Capacity pressure blocks new retirement; it never evicts
    mandatory evidence.
19. Void allocation authority is the selected catalog minus durable immutable
    claims. Raw gaps, hot-tail tuples, missing locators, and pending candidates
    never authorize reuse.
20. Every memory-amplifying owner reserves through one coordinator. The 16 GiB
    reference host uses a 6 GiB soft and 8 GiB hard AeorDB envelope with
    emergency headroom.
21. One strict per-property resolver applies defaults, stored configuration,
    environment, CLI, and last-known-good policy. A valid higher-precedence
    override may bypass a malformed lower source, but the degradation remains
    visible.
22. Serious durability failure latches the database read-only and attempts
    emergency spill. Startup discovers spill artifacts and requires explicit,
    ordered repair rather than silently discarding them.
23. HTTP, CLI, scheduled, plugin, sync, repair, migration, and embedded SDK
    paths are adapters over shared services and state machines.
24. The KV remains unordered and out of redesign scope. This campaign may add
    bounded physical-incarnation continuation and safety metadata, but does not
    redesign KV page placement, key ordering, or lookup semantics.

## 4. Explicit Non-Goals

- Redesigning the disk KV, its fixed bucket pages, or its current lookup rules.
- Making NVT or an index authoritative.
- Persisting query cursors, query sessions, or positional indexes.
- GPU query execution; fixed-coordinate and bitmap-compatible contracts merely
  preserve that future direction.
- In-place v3-to-v4 migration or automatic startup migration.
- Reverse-journaling acknowledged v4 writes back into v3.
- Peer replication of derived index artifacts by default.
- Reclaiming v0/v3 compatibility readers during the initial production
  migration window.
- Trading durability barriers or conservative GC behavior for benchmark wins.

## 5. Evidence Baseline and Drift Rule

The ratified review inspected 93 Axum route registrations, 161 Rust spec files,
and the primary engine/server hotspots. At formalization, the ten named hotspots
contained 18,846 lines, including 3,645 in `storage_engine.rs`, 3,497 in
`directory_ops.rs`, 2,844 in `query_engine.rs`, and 2,761 in
`server/engine_routes.rs`.

P0 does not trust those counts indefinitely. Before each landing unit it must:

1. fetch and compare `origin/development`;
2. rerun route, SystemFamily, protected-literal, mutation-producer,
   stable-key-replacement, persistent-caller, error-suppression, and test-target
   inventories;
3. record new recent fixes and attach a named guarding test;
4. classify drift as absorbed work or a linked roadmap item; and
5. reassign hotspot ownership before editing shared files.

No worker may dismiss a new route, family, writer, task payload, client schema,
or hardening fix as baseline drift.

## 6. Contract Owners

| Contract | Single implementation owner | Required consumers |
| --- | --- | --- |
| Persistent format registry and fixtures | Child 01 format owner | all readers, writers, verify, repair, migration |
| DatabaseHeader/capability admission | Child 01 format owner | open, clone, backup, peer, cutover |
| Durability and control publication | Child 02 durability owner | every authority/control writer |
| Config and memory admission | Child 02 runtime owner | server, CLI, SDK, caches, tasks |
| SystemFamily and semantic root authority | Child 03 namespace owner | mutation, reads, GC, transfer, indexing |
| Root lifecycle and physical reclamation | Child 04 GC owner | reads, pins, migration, allocator |
| Converter/page/NVT semantics | Child 05 index owner | indexing, query, migration, verify |
| Coverage/query/APOS/locator behavior | Child 06 query owner | HTTP, SDK, UI, bots, SSE |
| Clone/capture/cutover state machine | Child 07 migration owner | CLI, deploy, backup, operations |
| Evidence, documentation, debt gates | Child 08 integration owner | every child and release candidate |

`engine/mod.rs`, shared format registries, common errors, Cargo manifests,
fixture manifests, route assembly, and documentation navigation have one
integration owner. Other workers request those edits and do not race them.

## 7. Executable Child Plans

| Child | Scope | Main landing units | Start gate | Completion gate |
| --- | --- | --- | --- | --- |
| [01](2026-08-03-aeordb-v4-nvt-gc-refactor/children/01-format-capabilities-and-fixtures.md) | formats, capabilities, fixtures | P0b, P0c, P1a-c, P3a format slice | Child 08 P0a inventory frozen | readers precede writers; native fixture parity |
| [02](2026-08-03-aeordb-v4-nvt-gc-refactor/children/02-durability-controls-config-and-memory.md) | durability, controls, config, memory | P2a, P2b | Child 01 reader/capability foundations | v3 deployable safety snapshot, bounded real run |
| [03](2026-08-03-aeordb-v4-nvt-gc-refactor/children/03-namespace-semantic-roots-and-system-families.md) | namespace authority and semantics | P2c-e, P3b | Children 01 and 02 green | all producers use one mutation/root path |
| [04](2026-08-03-aeordb-v4-nvt-gc-refactor/children/04-physical-inventory-gc-and-void.md) | logical lifecycle, physical GC, Void | P4 | Child 03 selected-root/lifecycle readers | crash model retains or leaks; never early reclaim |
| [05](2026-08-03-aeordb-v4-nvt-gc-refactor/children/05-index-definitions-pages-and-nvt.md) | definitions, pages, NVT | P5 | Child 01 registries and Child 03 immutable roots | reference equality with absent/corrupt NVT |
| [06](2026-08-03-aeordb-v4-nvt-gc-refactor/children/06-async-coverage-query-pagination-and-locators.md) | async indexing, query, API | P6, P7 | Children 03 and 05; Child 04 root-state API | exact API model; commit excludes derived work |
| [07](2026-08-03-aeordb-v4-nvt-gc-refactor/children/07-side-by-side-migration-cutover-and-rollout.md) | shadow clone through production cutover | P3c, P8 | substrate after Child 03; cutover after 04 and 06 | copied production/canary/operator evidence |
| [08](2026-08-03-aeordb-v4-nvt-gc-refactor/children/08-verification-operations-docs-and-debt.md) | campaign evidence and closure | P0a, P2f, P9 | begins first; closes last | complete DoD evidence and zero stale authority |

## 8. Dependency and Landing Graph

```text
08:P0a inventory/baseline
  -> 01:P0b/P0c contract registry and fixtures
      -> 01:P1 bounded readers/capability/platform probes
          -> 02:P2a/P2b durability/config/memory
              -> 03:P2c-P2e producer convergence
                  -> 01:P3a format-writer slice + 03:P3b v4 roots/semantics
                      -> 07:P3c shadow clone/capture substrate
                      -> 04:P4 lifecycle/physical GC/Void
                      -> 05:P5 definitions/pages/NVT
                          -> 06:P6 async coverage/runtime
                              -> 06:P7 query/API/APOS/locators
                                  -> 07:P8 rehearsal/canary/cutover
                                      -> 08:P9 docs/debt/evidence

04 and 05 may overlap only after their distinct start gates are green.
08 evidence and architecture checks run across every landing unit.
```

Every arrow is a green, pushed, independently revertable snapshot. P2 producer
waves and P3/P6/P7 work are serialized around `storage_engine.rs`,
`directory_ops.rs`, `query_engine.rs`, `index_store.rs`, route assembly, common
errors, Cargo targets, and fixture registries. A hotspot is handed off at a
recorded commit; two workers never edit it concurrently.

## 9. Landing Units

### P0: Freeze Evidence and Contracts

- **P0a:** capture baseline behavior/performance, old-vs-old noise floor,
  producer/consumer inventories, recent-fix ledger, and intended divergences.
- **P0b:** hand-author independent 32-byte and 64-byte hash fixtures for every
  v4 format/control, canonical value, APOS, malformed case, and header slot.
- **P0c:** produce a machine contract registry proving ID uniqueness, exact
  formulas, hash-edge roles, ownership, caps, malformed behavior, and all route
  and SystemFamily classifications.

P0 is documentation/reference/test work only. It creates no production writer.

### P1: Reader-First Foundations

- **P1a:** bounded decoders and malformed-input fixtures.
- **P1b:** capability and selected SystemFamily admission before mutation.
- **P1c:** native platform durability probes and read-only ControlStore A/B
  selection.

Production writers still emit current v3/v0 bytes at P1 exit.

### P2: Deployable V3 Safety Convergence

- **P2a:** one durability coordinator, read-only latch, spill discovery/replay,
  and truthful error propagation.
- **P2b:** one config resolver, memory coordinator, owner metrics, and bounded
  cache/KV/directory ownership.
- **P2c:** strict SystemFamily traversal and corruption boundaries.
- **P2d:** namespace mutation, locator replacement, event, and metric facades.
- **P2e:** producer waves: core DirectoryOps; blob/batch; version/backup/sync;
  system/plugin; maintenance/repair.
- **P2f:** repository-wide error-squelch classification and residual gates.

P2 may write only approved v3-compatible transition controls as system-flagged
v0 FileRecords in the protected control subtree. Downgrade is refused while an
active latch, spill, or repair state exists.

### P3: Shadow V4 Authority and Migration Substrate

- **P3a:** exact v4 header/entity/control writers after fixture readers are green.
- **P3b:** NamespaceRoot, semantic state, root admission/lifecycle readers, and
  one `ResolvedReadView` path.
- **P3c:** shadow clone, bounded capture, root map, migration lease/progress,
  reconciliation, and cutover journal tooling without service activation.

### P4: Root Lifecycle, Physical GC, and Void

Land in this order: physical inventory and stable-key retirement journal;
logical root lifecycle; bounded mark/checkpoint; quarantine; sweep receipts;
Void catalog and immutable claims; repair/status/dashboard adapters. No
destructive activation occurs before the full state-machine model is green.

### P5: Definitions, Pages, and Sparse NVT

Land in this order: converter/definition fixtures; immutable directories and
ordered pages; document/value/state catalogs; sparse fixed-point NVT tiles;
mutation/checkpoint codecs; compaction and corruption fallback. All work remains
shadow-derived until P6 activation.

### P6: Async Coverage and Bounded Runtime

Land soft mutation/gap recovery first, then bounded workers and cache eviction,
then page publication and exact coverage/fallback activation. All producers
acknowledge durable user authority before derived work.

### P7: Query, APOS, Locators, and API Cutover

Land the route matrix and APOS decoder first, then root-aware planners and
locators, then coordinated HTTP/SDK/UI/SSE/docs schemas. No legacy APOS decoder
or implicit selector precedence remains.

### P8: Migration and Production Cutover

Preflight, copied-production rehearsal, native release candidate, canary, and
operator cutover are separate stop-capable units. Production v4 writes require
explicit operator acceptance after a read-only validation window.

### P9: Documentation, Retirement, and Evidence

Update all docs and bot skill material, remove approved transitional paths,
retain required v0/v3 readers, run every release/soak/copy gate, and publish the
durable DoD evidence packet.

## 10. Frozen Operational Policy

| Policy | Initial value/behavior |
| --- | --- |
| AeorDB memory on 16 GiB host | 6 GiB soft, 8 GiB hard |
| Index flush | 262,144 mutations, 30 seconds, or memory pressure |
| Pending-delete grace | 86,400 seconds (24 hours); explicit zero is valid |
| Required complete marks | exactly two; fixed engine invariant, not writable config |
| Existing candidate eligibility | `pending_since + max(grace_at_pending, current configured grace)` |
| Root expiry optional retention | 30 days |
| Root expiry optional budget | 256 MiB |
| Root lifecycle hard cap | 1 GiB; block new retirement rather than evict mandatory evidence |
| Migration capture maximum | 64 GiB by default; bounded disk state |
| Migration free reserve | capacity-clamped ratified expression |
| Migration checkpoint | 300 seconds by default |
| Source GC during migration | durably suspended under migration lease |
| Capture exhaustion | mark `needs_full_reconcile`; never fail source writes |
| V3 rollback | lossless only before first acknowledged v4 write |
| Normal share link | current HEAD only |
| Retired historical root | deterministic `410 Gone` |
| Cargo parallelism | `-j 6` or less for all ordinary commands |

Configuration routes are root-only `GET`, full-replacement `PUT`, and RFC 7396
`PATCH` for both `/system/runtime` and `/system/lifecycle`. The complete
resulting document is validated before coordinated publication. Environment
and CLI values override stored values without being written back.

## 11. Global Verification Spine

Each landing unit creates its named narrow target failing first, turns it green,
then runs affected existing regressions. Phase completion additionally runs:

```bash
cargo fmt --all -- --check
timeout 30m cargo test -j 6 -p aeordb --all-targets
timeout 30m cargo test -j 6 -p aeordb-cli --all-targets
timeout 45m cargo test -j 6 --workspace --all-targets
timeout 30m cargo clippy -j 6 --workspace --all-targets -- -D warnings
cargo build -j 6 --release --bin aeordb
```

Required narrow campaign gates:

| Unit | Command |
| --- | --- |
| P0 | `timeout 2m ./scripts/plan/check-v4-contracts.sh` |
| P1 | `timeout 5m cargo test -j 6 -p aeordb --test v4_format_fixture_spec` |
| P2 | `timeout 10m cargo test -j 6 -p aeordb --test v3_contract_facade_spec` |
| P3 | `timeout 15m cargo test -j 6 -p aeordb --test v4_root_migration_spec` |
| P4 | `timeout 20m cargo test -j 6 -p aeordb --test gc_v4_model_spec` |
| P5 | `timeout 15m cargo test -j 6 -p aeordb --test index_v1_reference_spec` |
| P6 | `timeout 15m cargo test -j 6 -p aeordb --test coverage_runtime_spec` |
| P7 | `timeout 15m cargo test -j 6 -p aeordb --test root_api_reference_spec` |
| P8 | `timeout 30m cargo test -j 6 -p aeordb-cli --test cutover_fault_spec` |
| P9 | `timeout 2m ./scripts/plan/check-v4-debt.sh` |

These targets are implementation obligations; they do not exist yet and are
not current evidence. The first commit in each unit creates the red target.
Long crash, soak, migration, and copied-production harnesses remain explicit
with watchdogs, seeds, progress files, and preserved artifacts.

Native macOS on `wyatt-mac` and Windows on `win11vm` must execute the same
fixture manifest and durability/cutover tests. Cross-compilation cannot clear a
persistent-format or platform-durability gate.

Every significant API/SDK/storage unit also runs a real database below
`/tmp/codex`, exercises real JSON/text/binary data over HTTP and embedded APIs,
reopens, verifies, and records the command/result ledger.

## 12. Behavioral and Resource Gates

- **Authority:** every crash point yields old, new, or typed recoverable state;
  never false success or mixed authority.
- **Root lifecycle:** pending remains readable; retired never reopens; physical
  uncertainty delays reclaim.
- **Index:** deleting/corrupting NVT, cache, or derived journal preserves exact
  reference results or returns typed incomplete when authoritative fallback is
  impossible.
- **Blob commit:** existing-chunk commit reads no file content and performs no
  synchronous parser/index mutation.
- **Memory:** concurrent production-shaped work stays alive and responsive
  under an 8 GiB cgroup/job-object ceiling without swap-dependent success.
- **Health:** remains schedulable under load; calibrated target is p99 below one
  second and no sample above five seconds absent proven OS-level I/O stall.
- **Performance:** unaffected p50/p95 operations regress no more than 10 percent
  without owner-ratified evidence. Safety is never relaxed to pass timing.
- **Migration:** source checksum, size, and header remain unchanged before
  cutover; destination full verify and behavior ledger are clean.
- **Security:** concealed root/path/hash probes reveal no existence, timing,
  count, state, snippet, APOS, aggregate, or EXPLAIN information.

## 13. Recent-Fix Regression Floor

P0 expands this ledger and may not remove an obligation because implementation
changes shape:

| Behavior | Existing guarding inputs | Campaign proof |
| --- | --- | --- |
| Blob backpressure/shutdown | `upload_commit_spec`, `upload_e2e_spec`, `shutdown_spec` | bounded queue, duplicate, disconnect, shutdown model |
| Existing-chunk commit latency | upload and multi-index specs | zero content reads; no derived waiter work |
| Cache/GC residency | `gc_spec`, `cache_and_hardlinks_spec`, metrics | clean eviction lowers RSS without result change |
| B-tree/GC safety | `tree_walker_spec`, `corruption_hardening_spec`, `header_repair_spec` | incomplete walk publishes no mark and creates repair evidence |
| HEAD counters | `engine_counters_spec`, `portal_spec`, `health_spec` | current entities remain distinct from revisions |
| Startup/readiness/shutdown | health, shutdown, resilience specs | progress/ETA/ready remain responsive and admitted work drains |
| Directory-first pagination | directory listing HTTP/engine specs | both directions cross category once without omissions |
| Content hash/reindex | content hash, reindex, query specs | whole-file hash survives migration and exact lookup |
| Media range/coalescing | streaming/range/download specs | byte-exact bounded seek/read-ahead including CRLF text rules |

## 14. Evidence Artifacts

The integration owner creates and maintains:

```text
baseline-environment.json
baseline-behavior-and-performance.json
intended-divergences.yaml
persisted-producer-consumer-inventory.json
route-root-contract-manifest.json
format-contract-registry.json
format-fixture-manifest.json
system-family-registry-v1.manifest.json
capability-matrix.json
error-squelch-inventory.yaml
memory-owner-budget-report.json
durability-platform-report.json
root-publication-model-report.json
gc-model-and-fault-report.json
query-reference-report.json
migration-operation-ledger.json
cutover-crash-report.json
production-canary-report.json
dod-evidence.md
completion-report.md
```

Every report records source commit, binary digest, platform/filesystem,
database-copy checksum, effective config sources, seed, command, timeout,
timestamps, result, and preserved logs. Evidence databases and secrets are
never committed or pushed.

## 15. Landing and Rollback Contract

1. Start from a clean understanding of user changes; never reset or overwrite
   unrelated work.
2. Rebase/merge current `development` at each phase entry and rerun drift
   inventories.
3. Add target-failing tests before behavior writers or activation.
4. Commit reader, writer, publication, activation, and deletion boundaries as
   coherent small snapshots.
5. Run narrow tests after each snapshot and broad gates at phase completion.
6. A failed broad gate blocks push/deploy. A suspected flake must pass an
   isolated rerun and have its variance source recorded.
7. Push every completed green phase. Deployment uses only a committed artifact
   through checked scripts, installs locally by default, and records binary
   digest and restart health.
8. Revert unit is one landing unit. Persisted capability activation may make
   binary rollback unsafe; use the child plan's state rollback, not an older
   incompatible writer.
9. Production/evidence databases are never development targets. All inspection
   begins from a separately checksummed copy.
10. FS-Server1 production mutation requires the explicit P8 operator gate. Plan
    generation, P0, local tests, and copied-production rehearsal never touch it.

## 16. Supersession and Preservation

The July 16 plan is fully superseded because it contains pre-ratification cursor,
header, GC, coverage, and migration contracts. Other historical plans remain
authoritative outside the exact areas incorporated here. This campaign does not
erase their history or silently claim unrelated completion.

The child plans and their banners distinguish:

- **superseded:** this campaign replaces the complete old plan;
- **incorporated:** this campaign owns only named contracts while the old plan
  remains authority elsewhere; and
- **preserved dependency:** this campaign relies on and regression-tests the old
  implementation contract without redesigning it.

## 17. Campaign Definition of Done

- [ ] P0 contract registry and both hash-width fixtures are independently green
      on Linux, native macOS, and native Windows.
- [ ] Every persistent producer/consumer and route has one contract owner.
- [ ] No production writer can emit v4 before capability and reader gates pass.
- [ ] All acknowledged writes use one durability/authority path.
- [ ] All namespace producers use one mutation/root path and one event/metric
      acknowledgement.
- [ ] V4 roots, semantic states, controls, lifecycle, GC, Void, index artifacts,
      and APOS satisfy their exact fixtures and crash models.
- [ ] No index operation loads, clones, sorts, or serializes an entire index.
- [ ] NVT deletion/corruption does not change query results.
- [ ] Historical auth, route selectors, hash fetch, and share behavior pass the
      concealment matrix.
- [ ] User commit latency excludes derived parser/index/NVT work.
- [ ] RSS and scratch/disk growth remain within ratified budgets under overlap.
- [ ] Copied production migration and dirty restart verify cleanly.
- [ ] Canary passes before production cutover; operator explicitly accepts the
      v4 write boundary.
- [ ] V3 backup is retained according to policy; no reverse rollback is implied
      after acknowledged v4 writes.
- [ ] Documentation, CLI, API, SDK, Dashboard, SSE, and bot skill agree.
- [ ] Error-squelch and duplicate-path debt gates are zero or use a shrinking,
      reviewed allowlist with reasons.
- [ ] `dod-evidence.md` and `completion-report.md` contain command-level proof.

## 18. Progress Ledgers

The child documents are the worker briefs. Their mutable execution state lives
in these dedicated ledgers rather than in chat or edits to the frozen plan:

| Child | Progress ledger |
| --- | --- |
| 01 | `2026-08-03-aeordb-v4-nvt-gc-refactor/progress/01-format.md` |
| 02 | `2026-08-03-aeordb-v4-nvt-gc-refactor/progress/02-runtime.md` |
| 03 | `2026-08-03-aeordb-v4-nvt-gc-refactor/progress/03-namespace.md` |
| 04 | `2026-08-03-aeordb-v4-nvt-gc-refactor/progress/04-gc.md` |
| 05 | `2026-08-03-aeordb-v4-nvt-gc-refactor/progress/05-index.md` |
| 06 | `2026-08-03-aeordb-v4-nvt-gc-refactor/progress/06-query.md` |
| 07 | `2026-08-03-aeordb-v4-nvt-gc-refactor/progress/07-migration.md` |
| 08 | `2026-08-03-aeordb-v4-nvt-gc-refactor/progress/08-evidence.md` |

Each records the current landing unit, owner, entry and last-green commits,
owned/forbidden files, hotspot handoff, red/green and broad gate results, drift,
risks, evidence paths, and next exact action.

This plan authorizes implementation only when the user separately requests
execution. Owner ratification of architecture is not automatic authorization
to deploy or mutate production.
