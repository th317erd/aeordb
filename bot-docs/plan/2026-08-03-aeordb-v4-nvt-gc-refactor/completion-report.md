# AeorDB v4 Qualification and Completion Report

## Current result — September 13, 2026

**Qualification remains in progress.** Candidate `a804732755b187b3e2bcdd109da37a2895dc9a80`
has passed full Linux coverage, affected native tests, all three normal native
release builds, live HTTP/restart/readback, and byte-preserving media verification.
All 100 complete crash suites, all three short soaks and all three full 12-hour
S1/S2/S3 stages pass. The driver exited successfully at 06:45:28 UTC on September
13. Final evidence sealing and requested test-database cleanup remain active.

The owner authorized release-qualification steps 1–3 and cleanup, then required
a **stop and discussion before step 4**. No installation, publication, deployment,
canary, production service change, operational cutover, first v4 write,
destructive operational GC, or retained production-database reuse is authorized.

Crucially, ordinary service opens/reads/writes/queries/GC still use the **v3
compatibility runtime**. V4 formats and state machines are implemented/tested
substrate; public `migrate-v4` creates and verifies an **offline shadow only**.
There is no public v4 service activation, cutover, acceptance or first-write
command. This is an implementation boundary, not merely a missing deployment
approval. See the [operator contract](../../../docs/src/operations/migration.md)
and [CLI command surface](../../../aeordb-cli/src/main.rs).

Current commands, failures and receipts are in [ledger 10](progress/10-release-qualification.md),
with the obligation map in [DoD evidence](dod-evidence.md) and the
[machine record](evidence/p9-final-qualification.json).

## Historical context and preservation

This multi-week campaign replaced ambiguous durability, namespace, index,
memory, GC and migration behavior with explicit contracts and independent
regressions. The earlier `535004f1` packet is historical, not current release
proof. Its original reports remain in Git at documentation commit
`6a4006846cc2aa5fc2e4f460dedf726b3193e53f`, and its detailed receipts remain in
[ledger 08](progress/08-evidence.md). Historical manifests and rejected attempts
are not rewritten into current passes.

Subsequent development exercised a disposable media database and an explicitly
authorized in-place repair of a badly damaged 4.77 TB production-derived file.
The latter was retired after nearly five days; it did **not** produce a verified
migration. The owner prioritized learning and product corrections over recovery.
The [September 9 handoff](handoff-2026-09-09-gpt-6-astra.md) records that decision.

The retained file is `FS-Server1:/mnt/storage/aeordb/files.taraani.org.aeordb`,
4,771,941,773,716 bytes. Its last recorded state is offline, immutable and mode
0444. Renewed release qualification has not reopened, repaired, copied, deleted,
or restarted service against it. This report does not claim a fresh inspection
or full checksum of that multi-terabyte file.

## Delivered contracts and subsequent corrections

Children 01–06 provide frozen formats/capabilities, native durability and
publication, strict configuration/memory ownership, namespace/read-view
contracts, conservative lifecycle/GC/Void state machines, page-addressable
indexes, sparse non-authoritative NVT, and exact query/pagination/locator
behavior. Child 07 supplies shadow migration and internal/rehearsal cutover
machinery. These contracts do not imply a running v4 service.

The post-handoff corrections are recorded with failing-first evidence in
[ledger 09](progress/09-repair-cache-followup.md) and
[ledger 10](progress/10-release-qualification.md):

- Bounded constant-work KV cache eviction replaces scale-dependent eviction
  scans; repair telemetry makes progress and expensive work observable.
- Fixed-width persisted readers use checked reads; 37 duplicate unchecked
  conversions were removed, with malformed-input and native coverage.
- Plain verification now uses a read-only, non-publishing path. Explicit repair
  remains a distinct mutating operation.
- Rebuild selection preserves retired-key chronology, avoiding resurrection
  of older directory entries covered by newer retirement evidence.
- Soak runners stop on the first failed cycle and retain diagnostic evidence.
  Diagnostic-copy failures and malformed comparison output cannot become success.
- Crash-worker restart durably trims only an incomplete checkpoint tail after
  exclusive database ownership. Completed malformed records remain unchanged
  and fatal. Native Windows testing proved that truncation requires a read/write
  handle; explicit EOF positioning preserves the complete prefix.
- Diagnostic wording distinguishes failed comparison from established data loss.

The reviewed suppression inventory is now 1,503 entries; its allowance did not
grow for these changes. Architecture, contracts and debt checks remain enforced.

## Candidate identity

| Input/artifact | Identity |
| --- | --- |
| Product commit | `a804732755b187b3e2bcdd109da37a2895dc9a80` |
| Git source tree | `11734d533452466b75e207cbf3e7f43e75fd6c13` |
| Frozen Cargo.lock SHA-256 | `06e6c7a8eb6dbccf52a0b97a4ee6edeece7297866305b0930314fd47b987faec` |
| Linux CLI SHA-256 | `aef7b082c3b693838a8b8659ff2280d98589ac5cb917376e5ad43e9d2aff5fec` |
| macOS arm64 CLI SHA-256 | `3837136f5f62a9d637866415d5c4328012829939a7b9da901d3a03cafc350b14` |
| Windows MSVC CLI SHA-256 | `ccadd817214083a04f4cf61f0fa3c7294587002ea03908f09e8a53b932791d5d` |

All binaries retain the normal release profile: optimized, debug information
enabled, no stripping. Linux uses two Cargo jobs; native macOS/Windows use one.
No Cargo target artifacts were transferred between hosts. Each normal binary
was pinned before test-feature builds could replace its build-cache path.

## Current qualification

| Gate | Proven result and attribution |
| --- | --- |
| Full Linux | 7,530 distinct top-level tests across 347 Cargo targets; zero failures; seven existing ignores; three nested checks counted separately |
| Capacity-safe full run | Two unchanged 8 GiB KV cases ran separately on Data; remaining 7,528 ran on private home scratch. No test was waived |
| Static/contracts/docs | Strict all-target Clippy; 454 independent fixtures; 95 routes/39 docs; 1,503-entry inventory; 29 architecture plus nine harness tests; debt eight entries/164 matches; mdBook and shell gates pass |
| macOS affected correction | 230 CLI tests plus 38 architecture/harness tests, seven existing ignores; preceding engine-correction matrix remains separately attributed |
| Windows affected correction | 227 CLI tests plus 38 architecture/harness tests, seven existing ignores; preceding engine-correction matrix remains separately attributed |
| Exact native release | Linux 17:16:59, macOS 17:15:40, Windows 17:25:44 UTC on September 11 |
| Current live release | HTTP/docs/binary payload, clean restart, delete/missing cases and offline verification pass at 17:19:13 UTC |
| Current release CLI | 230 tests/21 targets, seven existing ignores; passes at 17:33:15 UTC |
| Full media migration | Passed on unchanged engine/migration/runtime candidate `48baeefe`; source-preserving interrupted resume and completed retry verify 15,354,506,282 content bytes and 50,151 entities |
| Current media check | Source equivalence and retained hashes/receipts rechecked; a8047327 read-only verify preserves source bytes/stat; gate passes at 17:28:15 UTC |
| Resource overlap | Explicitly carried from `48baeefe`: 120 seconds, 8 GiB/no-swap, peak 2,190,843,904 bytes, three KV expansions, no functional failures, health p99 98.872 ms/maximum 676.777 ms |
| Crash qualification | 100 complete seven-function suites pass at 18:37:48 UTC; 2,300 interruption windows; 100 explicit forced-unmount self-skips |
| Short S1/S2/S3 | All pass by 18:42:49 UTC; S3 verifies and compares checkpoints through 12 cycles |
| 12-hour S1 | Pass September 12 at 06:43:37 UTC; strict read-only verification reports zero issues/632 intact snapshots; database bytes/stat unchanged |
| S1 resource summary | Pass 06:44:08 UTC; 721 samples/43,200 seconds; RSS growth 12.8%, VmData growth 15.8%, maximum 13 file descriptors |
| 12-hour S2 | Pass September 12 at 18:45:01 UTC; all 65 restart cycles pass normal-reopen/copy verification |
| 12-hour S3 | Pass September 13 at 06:45:28 UTC; 1,510/1,510 copied verifications and checkpoint comparisons; configured 43,200-second window, 43,227-second guarded stage |
| Final audit and test-DB cleanup | Pending |

The full-media operation was **not rerun or relabeled** as a8047327 execution.
A mechanical source-delta gate permits only the checkpoint/test/harness/progress
correction, revalidates prior artifact hashes and receipts, and checks source
invariance with the new CLI. This avoids another roughly 23 GB database pair.
The new verification takes 313.42 seconds/58,576 KiB maximum RSS/zero swap;
these observations are not a controlled performance comparison.

The current desktop campaign is
`/media/Data/AeorDB/Tests/p9-release-a8047327-20260911/`.
Native receipts use the same basename under `~/.cache/codex/aeordb-tests/`.
The closed ordinary-correction evidence seal covers 209 files, SHA-256
`6e25641e4d70228a8e622fed6e748742f7b44e8c96f766204d4d63641bc908a2`.
The current crash and duration gates are not covered by that earlier seal.

## Retained failures and resource limits

The failed plain-verification source copy, failed 33420bad S1, and malformed
48baeefe S3 checkpoint remain evidence, not passes. Initial Windows checkpoint
truncation failure and the Linux home-capacity interruption are retained too.
The stopped 8 GiB synthetic fixture is losslessly archived in 269,332 bytes;
only verified redundant raw copies were removed.

Data must retain 250,000,000,000 free bytes; private home scratch must retain
68,719,476,736. Guards sample every 30 seconds and enforce per-stage deadlines.
Model checks use approximately ten-minute intervals for crash/soak workloads.
Cycle scheduling is seeded; workers are wall-clock-seeded, not fully
deterministic. Forced-unmount testing is explicitly disabled/self-skipped.

The owner-requested cleanup covers unneeded, inactive disposable test databases
under desktop `/media/Data/AeorDB/`. Logs, useful failure specimens and source
inputs must be retained. Cleanup must record exact paths, reclaimed bytes and
recoverability; it does not authorize deletion of the FS-Server1 database.

## Remaining boundary

Completion requires the reconciled sealed packet and scoped cleanup. Then stop
for the owner's step-4 discussion.

A future operational proposal must distinguish a v3-compatible deployment from
additional v4 service activation work. It must not invent a cutover command or
treat shadow verification, file renaming or ordinary startup as acceptance.
Before any acknowledged v4 write, the designed rollback boundary preserves v3;
after that boundary, no reverse journal or safe binary rollback is implied.
The production-scale repair/migration remains unproven and retired, and
controlled performance parity at that scale has not been established.
