# AeorDB v4 Repository-Only Completion Report

## Result

The authorized repository implementation and qualification boundary is complete. The exact implementation candidate is commit `535004f166ad4cfaf8e7ab740458f2ef4733d2bc` on `development`; later commits through this report contain documentation and evidence reconciliation only. Linux, native macOS arm64, native Windows x86_64 MSVC, live reopen, constrained-resource, three 12-hour stages, and the final 100-case restart-resilience qualification are green.

This is deliberately not a production-cutover claim. Copied-production rehearsal, canary operation, installation/deployment, operator acceptance, first acknowledged v4 write, production monitoring, and destructive GC remain closed behind their separately authorized gates.

The machine-readable qualification record is [p9-final-qualification.json](evidence/p9-final-qualification.json), and the obligation-by-obligation proof is [dod-evidence.md](dod-evidence.md).

## Why The Campaign Existed

The prior architecture mixed authoritative namespace state with derived index, cache, and recovery behavior; retained unbounded index paths; had multiple durability and mutation routes; and lacked one complete native, resource, and restart proof for the proposed v4 migration. The campaign replaced those ambiguities with explicit persistent contracts, one-way state transitions, bounded ownership, and independent proof. Its governing rule is conservative failure: uncertain authority retains data or leaks space, never acknowledges an unproven write or reclaims a possibly live extent.

## Delivered Architecture And Behavior

- A frozen v4 format and capability registry covers database headers, entity/namespace/semantic records, controls, migration records, index artifacts, GC artifacts, APOS, both supported hash widths, malformed inputs, and native persistence behavior. Bounded readers preceded writers.
- A shared durability coordinator owns acknowledged authority publication, native barriers, read-back, grouping, failure latching, spill evidence, repair admission, shutdown drain, and checked downgrade safety.
- One strict configuration resolver and one process-wide memory coordinator cover KV pages and retained generations, directory/index/query/parser/plugin/task/GC/repair growth, resource-pressure refusal, and observability.
- Namespace mutation, stable locators, semantic roots, read views, events, and acknowledgement paths converge on typed shared authority. Historical reads apply current authorization before revealing existence, timing, count, position, snippet, aggregate, or explanation data.
- Root lifecycle and physical reclamation are separate. Pending roots remain readable; retired roots fail deterministically; physical reclaim requires conservative mark, grace, recheck, receipt, and claim evidence. Unjournaled v3 direct reuse remains disabled.
- Page-addressable indexes and sparse NVT hints are bounded and derived. Missing, stale, resized, or invalid NVT state cannot change authoritative query results.
- Parser/index coverage is asynchronous to user acknowledgement. Query, sort, aggregate, pagination, locator, range, API, SDK, Dashboard, SSE, and bot-facing contracts share the same root and authorization semantics.
- Side-by-side migration preflight, destination authority, lease/progress state, capture/reconciliation, synthetic cutover journal, rollback rules, and native release-candidate tooling are implemented without activating a production destination.
- Transitional duplicate paths are either removed or retained under explicit compatibility ownership. The reviewed error-handling inventory shrank to 1,505 entries across 35 policies, and all 29 architecture checks pass.

## Exact Candidate Identity

| Item | Value |
| --- | --- |
| Implementation commit | `535004f166ad4cfaf8e7ab740458f2ef4733d2bc` |
| Source archive SHA-256 | `1f534f95840121689b6ccb36dd13eeca7dc0c0aef7f8332ef036078b73b50f05` |
| Root `Cargo.lock` SHA-256 | `06e6c7a8eb6dbccf52a0b97a4ee6edeece7297866305b0930314fd47b987faec` |
| Linux release SHA-256 | `775f56f1bba3d73f97d0339b77e2bc3a3bc3726465b57abe8fa0e5d91a32fdbe` |
| macOS arm64 release SHA-256 | `d43cf494f797f57e038c8c67de379cef40df55c96fad8b950ece104ad80a857c` |
| Windows x86_64 release SHA-256 | `167a64b97e830e9aea8231df9734036f349c2ca57810310322c503e042e75cf3` |

The exact Git-object comparison proves that the engine tree and both long-run scripts are byte-identical between the long-duration candidate `9a71d4ce` and `535004f1`. The final successor nevertheless reran all 100 restart-resilience cases because its rebuilt worker executable was not byte-identical.

## Qualification Summary

| Gate | Result | Primary evidence |
| --- | --- | --- |
| Linux ordinary/static/release | 7,403 workspace tests passed; zero failed; seven intentionally ignored; strict workspace Clippy passed | Result `87d80d19ba3cabd178f9f747f8c1a20cfbd271f3bf2dceaf061c1914ac418984`; manifest `c7dbb076bcb03a9c0de4ca7f89686db58ab40b6790a07e41a0ef6fda66b43fc1` |
| Native macOS arm64 | 994 tests passed; zero failed; seven intentionally ignored; release build passed | Result `f3fe3b5177291c5abd1bf82993c4d4937f14a5ae9f66bdcf37f8dd55909b0809`; manifest `09f9144c6a2d193f4e9db4575bc5807d949bc25c7faf3f48b5ff3e7bff3e61dd` |
| Native Windows x86_64 MSVC | 990 tests passed; zero failed; seven intentionally ignored; release build passed | Result `88e387a3c392e4a9afd8748d93dfa024eb5e762abd73fdfc91968ee8bc6c81cc`; manifest `708d34aebbd70d7578298a5e2fa7f87b33263197b7648d7dca2b4e0497aa5928` |
| Live release/reopen | Real HTTP/docs/payload/delete/missing behavior, graceful shutdown, reopen, and offline verification passed | Result `d5be2aa00b2bed936dd2a9f5dd933a6b8fb082e43d632f70b47065cce8552d85` |
| 8 GiB/no-swap overlap | Peak 1,611,886,592 bytes; zero swap; health p99 81.018 ms; maximum 771.856 ms; no functional failures | Result `54bc98f0061273a1d37faa3b20655b2d5993d6bce3631c7a659d12ba00350899` |
| S1 | 12 hours; 25,632 writes, 12,808 reads, 4,125 deletes; exit zero | Manifest `fbb043f84000308db76155ebc1eaa94bae48736f280e4cf6b9ef79baa2f53309` |
| S2 | 12 hours; 61 planned worker cycles; zero issue cycles; exit zero | Manifest `fb364ad795a8e335b52ab21ad9c61d801cd67486a36e9ff0636bb3f67e47bf99` |
| S3 | 12 hours; 69,194 durable checkpoint records; exit zero | Manifest `c96290dfd87064495349fcb2cfbd203dd53ab25a5a312e2f75d3dcb93dff0008` |
| Restart-resilience | 100 of 100 final exact-candidate cases passed | Result `c4230b0c2f854167575cbefe3383e1d5de25858a46b3a7fcd04c3acf9f08ce08`; manifest `fa64f09dd2d7a0d39f8bdbd67368cf25d41e80272e248801de3617a9a75b8fb2` |
| Final evidence audit | 29 referenced manifests and their contents verified; one known invalid first seal independently confirmed as rejected | Result `ed87c06ca35b6fdbc1839d2c2560805b54a6091cadb797edd5e047755eb002b4`; manifest `3a0ca376d724ecbb2c88abe3c5210ab232cfa25cbb81b54e97cd960a2212fdab` |

The centralized 31-row evidence index is `/media/Data/AeorDB/Tests/p9-exact-535004f1/evidence/final-evidence-index.tsv`, SHA-256 `f4f6048be6286492bf106bddc96bf417068c7ed133e008e52f9b57b1c2c3a72b`. It includes passing evidence, truthful failed-attempt evidence, the rejected first source-equivalence seal, and the final audit chain.

## Commands That Cleared The Boundary

The exact Linux runner executed formatting and shell gates; AeorDB, CLI, and workspace all-target tests; strict workspace Clippy; the 1,505-entry inventory and 29 architecture tests; contract, debt, and documentation gates; and release builds. The platform runners executed the same frozen source archive and lock with one-job native storage/CLI matrices and native release builds. The macOS command was:

```text
python3 deadline.py 21600 bash qualify-macos.sh
```

Each internal macOS stage had a separate deadline: formatting 300 seconds, storage 10,800 seconds, CLI 5,400 seconds, and release build 5,400 seconds. The final integrity command ran the complete manifest/content verifier with a 1,800-second hard deadline on `wyatt-desktop` and returned `verified_manifests=29 known_rejected_manifests=1`.

The exact per-target commands, timestamps, environment, and results remain in the sealed runner logs referenced by [dod-evidence.md](dod-evidence.md) and [p9-final-qualification.json](evidence/p9-final-qualification.json).

## Problems Found And Corrected During Qualification

- Dirty startup could retain stable namespace authorities beyond the selected header frontier. Recovery now excludes those rows from rebuilt KV authority and durably retires only complete, classified stale records before publication.
- Both online and interrupted KV expansion could leave reusable-space snapshots out of canonical offset order. Both producers now sort their transformed snapshots.
- An old page load could re-enter the committed-generation cache after replacement publication. Update preparation and publication now fence ordinary cache misses and wake every terminal path.
- Snapshot contention could turn a committed request into a long stall. Expansion now defers before mutation and retries after the retained reader releases.
- A coalesced stream could retain planned offsets across online layout expansion. Streaming replans live chunks under a bounded retry contract without retaining the maintenance guard across a slow client.
- Repeated interruption exposed unsafe unjournaled v3 direct reuse. Consumption remains disabled until receipt-backed v4 catalog/claim authority can prove it safe.
- The restart harness reused paths without retiring the prior expected checkpoint value, and one reader accepted an incomplete final checkpoint fragment. The ordering protocol and shared newline-complete reader now fail closed, cover early worker exit, and pass the final 100-case rerun.
- Final source-location inventory drift was reproduced and corrected by shrinking the reviewed inventory from 1,506 to 1,505. No suppression allowance was added.
- The first source-equivalence seal was invalidated by output appended after manifest creation. It remains preserved and rejected; an immutable rerun and the final independent evidence audit passed.

Every failed or incomplete attempt remains preserved and is not counted as a successful gate.

## Retained Compatibility And Debt

Required v0/v3 readers, fixtures, backup policy, and permanent public facades remain. V3 direct reuse remains deliberately disabled. Copied-production, canary, and production transition code remains disconnected from operational authority until its explicit gate. The checked compatibility inventory, `v4-debt-policy.json`, and `p9-debt-gate-report.json` own every retained path and its removal condition; the debt gate cannot grow silently.

## Migration, Rollback, And Recovery Boundary

The repository contains the side-by-side migration and cutover substrate, but this campaign did not access or mutate a production-derived database and did not deploy or install a candidate. Before any operational transition:

1. Preserve and checksum the original v3 database and surrounding transition/spill evidence. Work only from a separately checksummed copy.
2. Follow `docs/src/operations/migration.md`, `backup.md`, `deployment-safety.md`, `observability.md`, and `gc.md`; run the checked preflight and copied-production rehearsal under separate authorization.
3. Require clean destination verification, native candidate parity, a read-only validation window, canary evidence, and explicit operator acceptance before the first v4 write.
4. Treat v3 rollback as lossless only before the first acknowledged v4 write. After that boundary, do not imply reverse journaling or binary rollback.
5. If durability or authority evidence is unresolved, keep the database read-only, preserve the original artifacts, and use the explicit repair workflow. Never reset, overwrite, compact, reclaim, or repeatedly reopen the original evidence before preservation.

## Separately Gated Work

The following remain intentionally unexecuted and are not repository defects:

- copied-production migration and dirty-restart rehearsal;
- canary operation under real clients/load and operator acceptance;
- local installation or production deployment;
- operational cutover, first acknowledged v4 write, and post-cutover monitoring; and
- destructive GC activation and real reclaim/reuse on operational data.

Those actions require new explicit authorization and their own evidence. Repository completion grants no implied permission to perform them.
