# AeorDB v4 Campaign Handoff for GPT-6 Astra

- **Prepared:** 2026-09-09
- **Repository:** `/home/wyatt/Projects/aeordb-workspace/aeordb`
- **Branch:** `development`
- **Entry HEAD:** `b1ce6ba8` (`record repair candidate replacement`)
- **Primary campaign:** [2026-08-03 AeorDB v4 NVT/GC refactor](./children/07-side-by-side-migration-cutover-and-rollout.md)
- **Active progress ledger:** [Child 07 migration progress](./progress/07-migration.md)

## Executive summary

The multi-week v4 refactor is substantially implemented and repository-qualified. The active work moved into real-data migration development: first an 11.6 GB disposable media rehearsal on `wyatt-desktop`, then an explicitly authorized repair of the offline 4.77 TB production-derived v3 database on `FS-Server1`.

The large repair exposed valuable product defects, most recently a pathological KV page-cache eviction algorithm. The repair has run for nearly five days and is unlikely to finish reliably within its 168-hour supervisor window. Data recovery is not the primary objective: the user retains the original contents independently and explicitly wants this exercise treated as AeorDB development.

The user accepted the recommendation to retire the current repair, preserve its evidence, improve AeorDB using reproducible fixtures, and continue v3-to-v4 migration development without depending on recovery of this database. The corrupt 4.77 TB database must be retained indefinitely unless disk capacity later requires deletion. **Do not delete it.**

Attempt 4 was gracefully retired after the user clarified the pause and explicitly authorized cleanup. The driver published terminal state at `2026-09-09T11:40:21-07:00`; all matching processes and source/lock openers were absent afterward. The retained database is mode `0444`, immutable, and remains in its original location. No database or rebuild workspace was deleted.

## Session continuity context

The original long-running `aeordb` Codex session became corrupt and stopped appearing in the session list after a Codex update. A second session that attempted to repair/recover it also became corrupt. Work was reconstructed from Git history, the campaign plan, progress ledgers, retained test artifacts, and independently refreshed host state; the campaign did not rely on treating a damaged transcript as authoritative.

The continued campaign also survived several app interruptions and a platform outage. This report, the progress ledger, `.codex/DETAILS.md`, and the remote evidence directory are the durable handoff surface for the model transition.

## Mandatory immediate actions

1. Reload all startup/project instructions, including `~/.codex/startup.md`, `~/.codex-personal/startup.md`, `AGENTS.md`, quirks, rules, `.codex/DETAILS.md`, and the applicable `implement` skill.
2. Verify the current Git worktree and confirm the retained database is still closed, mode `0444`, immutable, and the service remains stopped.
3. Read the retirement evidence paths and hashes below before changing repair/cache code.
4. Reproduce and correct the page-cache eviction defect with TDD. Add repair progress telemetry. Run heavy builds/tests on `wyatt-desktop`.
5. Continue v3-to-v4 migration development with disposable rehearsal fixtures. Do not reuse the retained corrupt database without new user authorization.

## Final operational state

Attempt 4 ran from `2026-09-04T12:03:10-07:00` through `2026-09-09T11:40:21-07:00`. The validated driver received `TERM`, propagated graceful retirement to its bounded child, and atomically published:

```text
exit_status=143
termination_reason=driver_signal
```

Post-retirement checks proved:

- No matching driver, timeout wrapper, timing wrapper, or AeorDB worker remained.
- The source and lock file had no openers.
- `aeordb.service` remained failed/offline with `MainPID=0`.
- No `${database}.repaired` copy existed.
- No `.aeordb-rebuild-*` workspace existed.
- Source stat before freezing: `40:387:4771941773716:644:995:986:1788601656:9138000441`.
- Source stat after freezing: `40:387:4771941773716:444:995:986:1788601656:9138000441`.
- Retained source header SHA-256: `21e5f84227342ded504a94ac3310e70158048269044ff4ad8ef870e4f19526c8`.
- Source attributes after freezing: `----i-----------------`.
- Write-open as the `aeordb` service account was refused.
- Available bytes remained `11,163,694,137,344` at the freeze boundary.

There is no longer a live repair to monitor. The driver/state/log artifacts remain as evidence.

## Exact remote paths and identities

- Production-derived v3 database: `FS-Server1:/mnt/storage/aeordb/files.taraani.org.aeordb`
- Attempt root: `FS-Server1:/mnt/storage/aeordb-migration-development-20260904`
- Driver: `/mnt/storage/aeordb-migration-development-20260904/repair-attempt4-driver.sh`
- Driver SHA-256: `edd150d8c2ca8b54701e6626398a3f7e4af8fb6cce258930dcb4a8e4bd6d07f4`
- Candidate: `/mnt/storage/aeordb-migration-development-20260904/bin/aeordb-3ab02246`
- Candidate SHA-256: `d8a810e7c193b5711a07afdb44b17463924077ecb476816f37b4000f1be2f65f`
- Candidate source commit: `3ab0224699c3e132fa18893b708a811657fceddd`
- Logs: `/mnt/storage/aeordb-migration-development-20260904/logs/repair-attempt4.{stdout,stderr,monitor}.log`
- State: `/mnt/storage/aeordb-migration-development-20260904/state/repair-attempt4-*`
- Preflight evidence: `/mnt/storage/aeordb-migration-development-20260904/evidence/attempt4-preflight.txt`
- Preflight SHA-256: `dbf5ad33522964f9e5d73fdcd74338c287969ae123d3d771aecd571022028217`

Retirement evidence:

- Pre-stop state and low-rate profile: `/mnt/storage/aeordb-migration-development-20260904/evidence/attempt4-retirement-pre-stop-20260909T183834Z.txt`
  - SHA-256: `acb7a2b48cc8dd4760c1ef31ccf5725a5defc442fc0b5d5040b92805003cacb8`
- Validated driver-signal receipt: `/mnt/storage/aeordb-migration-development-20260904/evidence/attempt4-retirement-signal-20260909T183942Z.txt`
  - SHA-256: `daa8f51c427846c50ae772860654822a8c4c441ac5673e37996c096d12f66b18`
- Closed-handle/freeze proof: `/mnt/storage/aeordb-migration-development-20260904/evidence/attempt4-retirement-freeze-20260909T184132Z.txt`
  - SHA-256: `d91e8ef6bfacaec36743b3428823cf4f04ee7f22899f88ef6b92c4024cae0ad6`
- Atomic driver exit record: `/mnt/storage/aeordb-migration-development-20260904/state/repair-attempt4-exit.txt`

An earlier draft retirement command failed in the local JavaScript wrapper before reaching the remote host. The three evidence files above were created by the corrected, completed sequence.

## User intent and authorization boundary

The database was previously served from `FS-Server1`, but `aeordb.service` was deliberately stopped weeks before this campaign because it thrashed the disks and would not shut down after hours of waiting. Its final shutdown was forced, so dirty/corrupt state was expected.

The user has an independent copy of every uploaded file and accepts losing this particular database representation. The objective is to make AeorDB reliable, repairable, scalable, and migratable—not heroic recovery of one damaged database.

Authorized:

- Capture read-only evidence and operational telemetry.
- Gracefully retire attempt 4.
- Retain and freeze the corrupt database read-only/immutable.
- Implement systemic, tested AeorDB fixes.
- Use disposable migration fixtures and the sealed media rehearsal.
- Continue the repository v3-to-v4 migration campaign.

Not authorized:

- Delete the retained 4.77 TB database.
- Restart `aeordb.service`.
- Create a full repaired copy beside a v4 shadow.
- Create or activate a production v4 shadow from this retained database.
- Cut over, accept first v4 writes, deploy, or activate destructive GC.

## Why no full database copy was made

`/mnt/storage` had roughly 11.16 TB free. A nearly 5 TB repair copy plus a nearly 5 TB v4 shadow would leave dangerously little working capacity. The approved development plan therefore allowed one in-place repair followed, only after strict verification and freezing, by one same-pool v4 shadow. That path is now retired for this database; the file is being retained as evidence instead.

The capacity safety floor is 5,000,000,000,000 bytes. The attempt driver also enforces an admission memory floor, an emergency host-memory floor of 512 MiB, a 5 GiB soft process limit, a 7 GiB hard limit, and a 168-hour outer bound with a 60-second graceful window.

## Attempt history and landed corrections

### Production admission and dirty-recovery correction

- `4a978eeb` authorized the in-place production-scale development migration boundary.
- `4c253b4a` recorded production repair admission evidence.
- An earlier repair attempt exposed unsafe/incorrect dirty-startup recovery handling.
- `f8a656e6` (`harden dirty recovery after failed startup`) corrected that class of failure and was tested before replacement attempts.

### Duplicate full-scan correction

The CLI performed a full `verify_checked` and then called the public self-verifying repair API, causing another full scan before repair. This was unacceptable on 4.77 TB.

Commit `3ab02246` (`avoid duplicate full scan during repair`) introduced an opaque `PreverifiedRepair<'engine>` token bound to the originating engine, database path, size, modification time, header sequence, and hot-tail frontier. The token consumes the exact stable report once. The public API retains its self-verifying behavior for ordinary callers.

The correction passed focused token/CLI tests, CLI and library suites, resilience tests, production/test Clippy gates, and debug/release end-to-end repair and strict-verification cases. Exact evidence and hashes are recorded in [Child 07 progress](./progress/07-migration.md).

Commit `b1ce6ba8` (`record repair candidate replacement`) was the aligned `HEAD` and `origin/development` entry point before this handoff unit.

### Attempt 3 retirement

Attempt 3 used the older `f8a656e6` candidate. It was gracefully stopped during its read-only scan at 10.58% so the duplicate-scan correction could replace it. Source identity/header remained unchanged. Its unique closed rebuild workspace was validated and removed. This is the procedural precedent for retiring attempt 4, except attempt 4 has crossed the mutation boundary and its retained source must be treated as corrupt evidence.

## Attempt 4 findings

Attempt 4 started at `2026-09-04T12:03:10-07:00` using the exact `3ab02246` candidate.

The initial dirty-recovery scan completed with:

- Scanned bytes: `4,767,747,802,903`.
- Entries collected: `38,895,846`.
- Deletion records: `45`.
- Corrupt entries: `87,084`.
- Rollback-authority entries: `164`.
- Skipped payload bytes: `4,748,630,453,156`.
- Scan duration: `26,412,781 ms` (about 7.34 hours).

Dirty startup then:

- Discarded non-authoritative tail residue beginning at offset `4,771,941,773,698` after invalid magic.
- Rolled back 164 namespace-authority entries beyond the selected header frontier.
- Resolved 38,895,846 scanned records into 29,554,211 entries.
- Rebuilt the KV index in `53,051.59 s`.
- Recovered 7,769,762 voids totaling 2,064,902,953 bytes.
- Initialized configuration authority complete, non-degraded, with zero blocking issues.

The command then entered its pre-repair verification pass. It emitted 641 stale directory path-key divergence warnings; all 641 parsed paths were unique, with zero duplicate groups. The last stdout timestamp was `2026-09-05T15:30:59.089565362Z`, after which the worker continued accumulating CPU and I/O without logical progress telemetry.

## Critical performance defect discovered

Live low-rate profiling on 2026-09-09 found 94.51% of active task-clock samples in:

`aeordb::engine::kv_page_provider::KvPageProvider::read_page_at_with_preparation_access`

The dominant inlined work was `evict_oldest_page`, which currently executes:

```rust
state.pages.iter().min_by_key(|(_, page)| page.last_access)
```

This scans every resident cached page for every eviction.

Relevant geometry for the 32-byte hash format:

- KV page size: 1,450 bytes.
- KV block: approximately 4 GiB, or about 2.96 million bucket pages.
- Default bounded resident cache: up to 2 GiB, or about 1.48 million pages.
- Uniform hash access can plausibly produce a miss rate near 50% once the working set spans the KV block.
- Every full-cache miss can therefore inspect roughly 1.48 million hash-map entries merely to choose one 1,450-byte victim page.

Across a measured 1,479-second interval:

- CPU ticks increased by 52,707, about 527 CPU seconds or 35.6% of one core.
- Read syscalls increased by 227,566, about 154 per second.
- Physical reads increased by 23,259,869,184 bytes, about 15.7 MB/s.
- The existing binary does not export live `KvPageProviderStats`, so read-call rate is not an exact cache-miss rate.

Conclusion: this is not supported as a recursive-loop diagnosis. Directory recursion is capped at 100, logged divergent paths did not repeat, kernel stacks showed real ZFS positioned reads, and cumulative work counters advanced. The algorithm is finite but has pathological near-quadratic behavior at this scale. Without the outer timeout, days to one or two weeks were plausible; years were not. The behavior is still categorically unacceptable.

## Required code improvements

### 1. Bounded-complexity page eviction

Start with a deterministic failing regression in `aeordb-lib/spec/engine/kv_page_provider_spec.rs` or an appropriate internal spec. Do not use wall-clock timing as the only oracle. Instrument candidate examinations or another deterministic work counter so the old linear scan fails mechanically.

Replace full-map minimum selection with exact or appropriately specified bounded-complexity bookkeeping. Candidate designs include an indexed/intrusive LRU with O(1) updates and eviction, or a lazy min-heap with O(log n) operations and strictly bounded compaction. Preserve:

- Exact byte-cap enforcement and memory reservations.
- Hit/miss/read/eviction/failure/deferral accounting.
- Coalesced concurrent misses for one bucket.
- Snapshot generations and historical-page retention.
- Pending-update visibility and preparation barriers.
- Poison/failure behavior.
- Correct eviction after repeated hits, explicit removals, updates, and access-clock boundaries.
- Bounded auxiliary-memory growth under a hot repeatedly accessed page.

Map all callers through `DiskKVStore`, `ReadSnapshot`, generation/update paths, repair verification, GC, and migration readers before changing the shared cache.

### 2. Repair progress telemetry

The verifier must emit bounded, periodic logical progress for all expensive phases, not only WAL rebuild:

- Expected-run scan and merge.
- Actual KV scan and comparison.
- Directory traversal with directories/children checked.
- Path-key FileRecord verification with entries checked.
- Snapshot verification.
- Repair actions and final verification.

Expose cache hits, misses, disk reads, evictions, resident pages/bytes, candidate-examination work, and phase rates in logs or the repair supervisor evidence. Progress instrumentation must have negligible per-record overhead and remain bounded under corrupt input.

### 3. Product proof

- Demonstrate the old eviction complexity with a failing deterministic test.
- Prove LRU/cache semantics and every concurrency/generation/failure path.
- Add a bounded large-page-count benchmark or operation-count regression.
- Run narrow tests first, then affected cache/storage/verify/repair/migration matrices.
- Run heavy compilation and resource tests on `wyatt-desktop` at `/media/Data/AeorDB/Tests/`.
- Exercise the corrected binary against disposable/reproducible fixtures, not the retained corrupt database.
- Do not restart the production repair simply to prove the fix unless the user later reauthorizes using the retained file.

## Disposable rehearsal assets

Heavy rehearsal root:

`wyatt-desktop:/media/Data/AeorDB/Tests/p8-media-rehearsal-20260903/`

Important artifacts:

- Sealed disposable v3 source: `source-v3.aeordb`.
- Sealed migration working copy: `migration-copy-v3.aeordb`.
- Each is about 11.65 GB with SHA-256 `fe54e1ae6582b192434f3aff606bd0041a713128fdedaf8a249524a63a36ecfe` as recorded in durable project context.
- Corpus: 5,406 files and 16,037,628,271 logical bytes.
- Evidence directory: `/media/Data/AeorDB/Tests/p8-media-rehearsal-20260903/evidence/`.
- Exact old v0.9.5 writer SHA-256: `74dae9958c0e22e713b7d04f7ecd0d3d37c488b261186fcb24968785b1df1c8f`.

The sealed source must remain read-only by process discipline. Work on the migration copy or fresh disposable derivatives.

## Host and storage preferences

- Use `/tmp/` only for small, non-durable work that may disappear on reboot.
- Use `~/.cache/` for durable or large local artifacts.
- Do not use `/var` for task artifacts; it is a system partition.
- Heavy CPU/disk work belongs on `wyatt-desktop:/media/Data/AeorDB/Tests/`.
- A clone exists at `wyatt-desktop:~/Projects/aeordb-workspace/` with matching project paths.
- Never transfer Cargo `target` directories. Exclude every `target` from `rsync`; rebuilding on `wyatt-desktop` is faster.
- Git commit/push plus fetch/sync is a valid alternative to `rsync`.
- Preserve at least 250,000,000,000 bytes free on `wyatt-desktop:/media/Data`.
- Preserve at least 5,000,000,000,000 bytes free on `FS-Server1:/mnt/storage`.
- The laptop has a locally overridden `kill`; use explicit, validated process control and never broadly terminate shells.

## Git and worktree state

Before staging this handoff unit, the pre-existing untracked state to preserve was:

```text
## development...origin/development
?? .codex/DETAILS.md
?? .codex/wip.md
?? bot-docs/plan/2026-08-03-aeordb-v4-nvt-gc-refactor/completion-report.user-draft-20260903T143050Z.md
?? bot-docs/plan/2026-08-03-aeordb-v4-nvt-gc-refactor/dod-evidence.user-draft-20260903T143050Z.md
?? bot-docs/plan/2026-08-03-aeordb-v4-nvt-gc-refactor/evidence/p9-final-qualification.user-draft-20260903T143050Z.json
?? downloads/
?? tools/v4-reference/target/
```

These pre-existing untracked files belong to the user or prior workflow. Preserve them. Stage only deliberate handoff/implementation files. Run Git commands from the repository root; do not use `git -C`.

`.codex/DETAILS.md` now includes the attempt-4 retirement and page-cache findings, but remains intentionally untracked with the established local context.

The untracked `Cargo.lock` used for exact candidates was intentionally not committed. Verify its current state before building.

## Completed safe retirement sequence

The prior model completed the planned sequence: fresh evidence and profiling, exact driver/worker validation, graceful driver signaling, atomic exit publication, closed-process/open-handle proof, workspace/repaired-copy absence checks, and source freezing. The database remains at its original path, mode `0444`, immutable, and explicitly retained. No database content or workspace was deleted.

## Definition of the next successful landing unit

The next unit is complete only when:

- A deterministic test proves the old linear eviction defect.
- The replacement eviction design has bounded complexity and bounded memory.
- Cache concurrency, generation, update, pressure, corruption, and cancellation tests pass.
- Repair progress telemetry is independently tested.
- Focused and affected suites pass on `wyatt-desktop` with retained logs and resource evidence.
- The correction is committed and pushed as one coherent green unit.
- Migration development resumes using disposable fixtures; no production service, cutover, deletion, or deployment boundary is crossed.

## Model-transition note

The user referred to the new model as “Astro.” [Official OpenAI documentation](https://developers.openai.com/api/docs/guides/latest-model) current on 2026-09-09 names it **GPT-6 Astra** (`gpt-6-astra`). The model handoff is deliberate. This report is the authoritative starting orientation, but live process, filesystem, Git, and plan state must still be refreshed before action.
