# Child 08: Verification, Operations, Documentation, and Debt

**Parent:** [AeorDB V4, NVT, GC, and Migration Campaign](../../2026-08-03-aeordb-v4-nvt-gc-refactor.md)
**Landing units:** P0a, P2f, continuous phase gates, and P9
**Status:** Starts first and closes last
**Primary owner:** campaign integration/evidence owner
**Authority:** may block any landing unit whose proof is incomplete

## 1. Outcome

Maintain independent evidence that every campaign phase preserves or
intentionally changes behavior, classify the repository's error-suppression and
duplicate-path debt, keep plans/docs/clients synchronized, and produce a durable
completion packet that lets an operator migrate, monitor, diagnose, repair, and
recover without implementation lore.

This child does not rubber-stamp subsystem tests. It owns the cross-boundary
oracles, drift detection, broad gates, real-world exercises, native matrix,
production-copy evidence, supersession map, and deletion proof.

## 2. Owned Territory

- campaign plan/status/evidence files and progress ledger;
- baseline/reference/operation-ledger harnesses;
- architecture/grep/contract/route/SystemFamily checks;
- repository-wide error-squelch inventory and allowlist;
- broad test, lint, build, soak, crash, resource, and native qualification;
- real `/tmp` and copied-production test orchestration;
- API/SDK/CLI/Dashboard/SSE/docs/SKILL consistency review;
- plan supersession banners and roadmap links;
- transitional-path/debt deletion gates; and
- final `dod-evidence.md` and `completion-report.md`.

Subsystem behavior remains owned by its child. This owner may add independent
tests and block activation but must not silently rewrite frozen contracts.

## 3. P0a: Evidence Before Implementation

### Baseline

Record:

- source commit/branch/remotes, Cargo/Rust/tool versions, platform/filesystem;
- route registrations, embedded/public surfaces, test targets, and docs pages;
- source line/fanout hotspots and shared ownership;
- persistent EntryTypes/KV tags/header versions/control paths;
- every namespace/root/stable-key mutation producer and persistent caller;
- every protected path/SystemFamily literal and transfer consumer;
- every memory/cache/index/KV/GC/query owner;
- every raw write/sync/rename/barrier and durability suppression;
- recent hardening commits and guarding specs; and
- current performance/RSS/I/O distributions.

Formalization baseline is `5d3e284652f9fec7a5c843f1946132574af4d469`,
which matched `origin/development` on 2026-08-03. Rerun before implementation;
do not assume it remains current.

### Noise Floor and Divergences

Run old code against itself twice on deterministic fixtures. Classify timestamp/
ID/order/randomness noise before comparing old/new. Create a machine-readable
allowlist for ratified intended changes only, including v4 bytes, root selectors,
APOS, GC lifecycle, async indexing, strict errors, memory/config diagnostics,
and migration behavior.

Every residual difference is one of:

- regression: fix before landing;
- ratified intended change: exact allowlist entry and test; or
- newly discovered beneficial/necessary divergence: stop for owner ruling.

### Outputs

Create the parent plan's baseline, inventory, divergence, route, recent-fix, and
resource artifacts before Child 01 P0b starts.

## 4. Independent Oracles

Maintain oracles that do not call the production implementation being tested:

- binary format/CRC/hash/identity reference;
- converter canonicalization/comparison/coordinate model;
- ordered posting/page/directory model;
- query/boolean/sort/aggregate/pagination reference;
- namespace/root admission and lifecycle model;
- GC eligibility/sweep/Void state machine;
- durability operation ledger and crash frontier;
- migration producer/root/family reconciliation ledger; and
- authorization/concealment route matrix.

An oracle may share frozen constants only through checked fixture input, not
production serializer/planner/state-machine code.

## 5. P2f: Error-Squelch Audit

Inventory every production occurrence of ignored/converted errors, including
`let _ =`, `.ok()`, default-on-error, broad `Err(_)`, conditional success-only
branches, panic/expect, logging-without-return, and cleanup failures.

Classify each occurrence:

| Class | Required behavior |
| --- | --- |
| Durability/authority | propagate failure, latch where serious, never success |
| Correctness-bearing read/traversal | typed incomplete/corrupt; never empty-complete |
| Retryable operational | bounded retry with evidence and terminal result |
| Rebuildable derived | degrade/reconcile/rebuild with exact fallback |
| Optional telemetry/temp cleanup | warn/metric/debt; may preserve primary success |
| Deliberately ignored | reviewed local rationale and architecture allowlist |

Add a shrinking checked allowlist. New suppression fails CI. Entries require
file, line/pattern, class, rationale, owner, test, and removal condition. The
allowlist may shrink but cannot grow without owner/integration review.

Specifically audit fsync/sync/rename/read-back, shutdown, spill, ControlStore,
GC workspace, Void settlement, migration/cutover, plugin host, backup/import,
repair, and async worker errors.

## 6. Architecture and Grep Gates

Machine checks fail on:

- raw HEAD/root/control mutation outside shared coordinators;
- stable-key replacement outside locator/retirement coordinator;
- duplicate protected-path/SystemFamily lists outside v0 migration evaluator;
- namespace route reads bypassing `ResolvedReadView`;
- raw hash retrieval without route classification;
- user commit calling parser/posting/NVT publication;
- whole-index load/clone/sort/serialize in v1 paths;
- NVT used as membership or order authority;
- unbounded GC `HashSet`/queue/full candidate vectors;
- direct Void reuse without selected catalog/claim;
- production serializer generating its own goldens;
- config parsing/defaults outside the central resolver;
- material memory allocation outside the coordinator; and
- legacy APOS decoder or superseded encoded-root-token concepts in active
  code/docs.

Checks operate on syntax/registries where possible. Text grep is a backstop and
uses a reviewed path/comment fixture allowlist to avoid false confidence.

## 7. Verification Protocol

For every landing unit:

1. Record entry commit and baseline drift.
2. Create the named target failing for the intended behavior.
3. Run the child's narrow command.
4. Run affected current regressions, including recent-fix guards.
5. Run independent oracle/equivalence diff and classify residuals.
6. Run fault/resource/performance cases appropriate to the unit.
7. Run a real database under `/tmp/codex` for significant API/SDK/storage work.
8. Reopen and verify the test database; preserve command/result ledger.
9. Run the parent broad gates before phase push.
10. Record commit, commands, duration, result, RSS/I/O, failures, reruns, and
    artifact locations in progress/evidence.

A timeout is failure and preserves state/logs. A suspected CPU-contention flake
gets one isolated rerun plus variance diagnosis; it is not declared a pass from
belief.

## 8. Broad and Long Gates

Ordinary gates use at most six Cargo jobs:

```bash
cargo fmt --all -- --check
timeout 30m cargo test -j 6 -p aeordb --all-targets
timeout 30m cargo test -j 6 -p aeordb-cli --all-targets
timeout 45m cargo test -j 6 --workspace --all-targets
timeout 30m cargo clippy -j 6 --workspace --all-targets -- -D warnings
cargo build -j 6 --release --bin aeordb
```

Long suites remain explicit, seeded, watched, and progress-reporting:

```bash
AEORDB_SOAK_DB=/tmp/codex/aeordb-v4-s1/soak.aeordb \
  AEORDB_SOAK_HOURS=12 ./scripts/soak.sh s1
AEORDB_SOAK_DB=/tmp/codex/aeordb-v4-s2/soak.aeordb \
  AEORDB_SOAK_DURATION_SECS=43200 ./scripts/soak.sh s2
AEORDB_SOAK_DB=/tmp/codex/aeordb-v4-s3/soak.aeordb \
  AEORDB_SOAK_DURATION_SECS=43200 ./scripts/soak.sh s3
./scripts/crash_inject_soak.sh 100
```

These scripts must honor `CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-6}` and never
mutate an evidence/production original.

## 9. Native and Real-World Matrix

- Linux local/CI runs full ordinary gates, resource limits, crash/soak, and
  `/tmp` HTTP/SDK scenarios.
- `wyatt-mac` runs native fixtures, format, durability, cutover, relevant full
  suite, and release build.
- `win11vm` runs the same native Windows set and release build.
- Cross-compilation is packaging assistance, not native persisted-format proof.
- Copied-production migration uses a separately checksummed copy and never
  mutates production/evidence originals.
- FS-Server1 production work occurs only through Child 07's explicit P8 gate.

## 10. Documentation Deliverables

Update and cross-link at minimum:

- `docs/src/concepts/storage-engine.md`: v4 header, authority, artifacts,
  durability, memory;
- `docs/src/concepts/indexing.md`: definitions, pages, coverage, sparse NVT;
- query/search/list/fetch API docs: `root_hash`, root metadata, APOS, errors,
  locators, ranges, CRLF behavior;
- admin/config docs: runtime/lifecycle precedence, GC, Void, memory, latches,
  spill, repair, migration, rollback boundary;
- reindex versus format/index migration operations docs;
- backup/restore/peer/sync policy for authoritative and derived state;
- CLI and embedded SDK reference;
- Dashboard/metrics/SSE schemas;
- root-served mdBook navigation and bot-oriented `SKILL.md`; and
- release notes and old-binary capability boundary.

Docs must be served by a real `/tmp` server, links checked, examples executed,
and API schemas compared mechanically with route/SDK types.

## 11. Debt Deletion Phase

P9 deletes rather than merely deprecates approved transitional code:

- duplicate durability/config/memory/mutation/root/locator paths;
- duplicate protected-family/path lists;
- v1 whole-index blobs and unbounded resident maps;
- direct stable-key replacement and unsafe Void reuse;
- legacy APOS decoder/position payload;
- temporary feature gates/adapters whose support window ended; and
- stale plan authority and contradictory documentation.

Retain required v0/v3 readers, fixtures, v3 backup policy, and permanent public
facades. Label every remaining indirection as permanent projection or timed
compatibility shim with owner/removal gate.

`scripts/plan/check-v4-debt.sh` enforces zero references or a shrinking reviewed
allowlist:

```bash
timeout 2m ./scripts/plan/check-v4-debt.sh
```

## 12. Supersession and Roadmap Discipline

Add banners to incorporated plans rather than deleting history. Each banner
states whether the old document is fully superseded or only incorporated for
named contracts and links this parent.

Every gap found during execution is either:

- absorbed because it is adjacent, required, and within current ownership; or
- added to `future-plans.md` with dependency, rationale, owner, and evidence.

Do not silently expand scope or drop a discovered issue.

## 13. Landing and Progress Files

Each child maintains a progress file with:

- current landing unit and owner;
- entry/last-green commit;
- files owned/forbidden/hotspot handoff;
- red/green commands and results;
- open failures/risks/drift;
- evidence artifact paths; and
- next exact action.

At phase completion: merge by hand with union semantics, run broad gates, commit
the coherent unit, push, and record the pushed commit. Never chain push after a
failing command. Docs-only commits that include upstream changes run the same
broad gate appropriate to those changes.

## 14. Landing Sequence and Rollback

1. P0a freezes baseline, noise floor, inventories, and intended divergences.
2. Every landing unit receives continuous oracle, drift, narrow, broad, real-
   world, and evidence review.
3. P2f classifies the complete error-squelch surface and activates residual
   architecture gates before v4 production writers.
4. P9 updates docs/clients/operations, deletes approved debt, runs final native/
   soak/copy/release gates, and publishes completion evidence.

Evidence/check/doc changes revert as their own landing units. Never rewrite or
delete a truthful failing report merely to make a subsystem appear green. If an
oracle or gate is itself wrong, preserve the original result, correct the gate
in a reviewed commit, rerun it, and record both outcomes. This child owns no
production persistence and cannot waive another child's state rollback rule.

## 15. Final Evidence Packet

`dod-evidence.md` maps every parent/child DoD item to command output, report,
commit, and reviewer. `completion-report.md` records:

- why the campaign existed;
- architecture and behavior delivered;
- paths/state machines deleted;
- migration and rollback boundary;
- native/test/soak/resource/performance results;
- production canary/cutover evidence;
- notable defects found and fixed;
- retained compatibility/debt with explicit owner; and
- exact operator recovery instructions.

No claim may say “all tests passed” without command, commit, timing, and result.

## 16. Definition of Done

- [ ] Baseline, noise floor, inventories, and divergence ledger exist before P0b.
- [ ] Every phase has an independent oracle and one-command red/green target.
- [ ] Every recent fix has a named guarding spec.
- [ ] Error-squelch inventory is complete and residual allowlist only shrinks.
- [ ] Architecture gates prevent every duplicate/bypass class named above.
- [ ] Significant phases have real `/tmp` reopen/verify evidence.
- [ ] Linux, native macOS, and native Windows qualification passes.
- [ ] S1/S2/S3/crash/resource/copy gates pass with preserved logs.
- [ ] Docs/API/SDK/UI/SSE/SKILL schemas agree and examples execute.
- [ ] Superseded plans cannot be mistaken for current authority.
- [ ] Transitional debt is deleted or retained under explicit support policy.
- [ ] `dod-evidence.md` and `completion-report.md` are complete and reviewable.
- [ ] No evidence database, secret, or transient build artifact is committed.
