# AeorDB v4 Definition-of-Done Evidence

## Current boundary

Candidate: `a804732755b187b3e2bcdd109da37a2895dc9a80`, development branch.
Integration/review owner: Codex, direct execution under the owner-authorized
release-qualification plan. Current status: **qualification in progress**.
[Ledger 10](progress/10-release-qualification.md) is the sole active checklist.

Ordinary/native/static, exact-release, 100-complete-suite crash and short-soak
gates pass. The first 12-hour stage is running; all three duration gates, final
packet audit and requested cleanup remain open. Stop before step 4.

Ordinary service authority remains v3-compatible. Public `migrate-v4` is
offline shadow creation/verification, not activation. Public v4 service
activation/cutover/acceptance is unavailable, not merely waiting for a deployment
permission. No production or retained-database action is implied by this packet.

The earlier 535004f1 packet remains in Git at
`6a4006846cc2aa5fc2e4f460dedf726b3193e53f`; historical receipts are in
[ledger 08](progress/08-evidence.md). The
[retirement handoff](handoff-2026-09-09-gpt-6-astra.md) and
[ledger 09](progress/09-repair-cache-followup.md) describe subsequent development.
Do not turn their older test counts or duration passes into current evidence.

## Parent obligation map

| Obligation | Current evidence/boundary |
| --- | --- |
| Frozen formats, both hash widths, native capability/durability behavior | Child 01 fixtures/native history; current full Linux contracts and affected native regressions |
| Single producer/consumer and route ownership | Persisted/route inventories, Child 03/06 ledgers, current contract/architecture gates |
| Readers/capabilities precede v4 writers | Child 01 reader/writer fixtures; ordinary service remains v3 |
| Shared acknowledged durability/authority path | Child 02 evidence and current full regression suite; no operational v4-write claim |
| Shared namespace/root/event ownership | Child 03/06 architecture and behavioral targets |
| V4 formats, roots, controls, lifecycle, GC, indexes and modeled crash states | Implemented substrate with independent fixtures/state-machine tests; not public service activation |
| Bounded v1 indexes; NVT never authoritative | Child 05/06 reference, fallback and bounded-resource targets included in full Linux suite |
| Historical authorization, root selectors and concealment | Child 06 route/reference targets and current full suite |
| User acknowledgement excludes synchronous derived work | Child 06 producer/worker evidence and current regressions |
| Bounded resident memory and scratch | 48baeefe 8 GiB/no-swap overlap carried with explicit source-equivalence proof; unchanged disk floors |
| Current crash/soak proof | 100 complete suites and all short soaks pass; current three-duration gates still open |
| Production-derived migration and dirty restart | Not proven. Large damaged-file repair retired; disposable clean-media migration is separate evidence |
| Canary before cutover and explicit acceptance | Not performed; requires discussion/authorization and available implementation surfaces |
| V3 backup/rollback boundary | Defined in migration contract; operational first-write boundary never crossed |
| Documentation/API/SDK/bot agreement | Current contracts/mdBook/live docs pass; this canonical packet is being reconciled |
| Error handling/debt | 1,503 reviewed inventory entries; 29 architecture tests; eight debt entries/164 retained matches |
| Command-level final packet and requested cleanup | In progress; final seal and cleanup receipt still required |

This map does not mark the entire frozen parent complete. Its production,
activation and current qualification obligations remain visible.

## Child and regression sources

| Child | Proven repository territory | Durable source |
| --- | --- | --- |
| 01 | Formats, capabilities, bounded readers/writers and fixtures | [format ledger](progress/01-format.md) |
| 02 | Durability, strict configuration, bounded ownership and observability | [runtime ledger](progress/02-runtime.md), [post-repair follow-up](progress/09-repair-cache-followup.md) |
| 03 | Namespace, semantic-root and SystemFamily contracts | [namespace ledger](progress/03-namespace.md) |
| 04 | Lifecycle, physical inventory, GC/Void models and internal execution | [GC ledger](progress/04-gc.md); destructive operational activation remains gated |
| 05 | Page-addressable index and sparse NVT/reference behavior | [index ledger](progress/05-index.md) |
| 06 | Coverage/query/APOS/locator/API behavior | [query ledger](progress/06-query.md) |
| 07 | Offline shadow migration and internal/rehearsal cutover machinery | [migration ledger](progress/07-migration.md); no public v4 activation |
| 08 | Continuous evidence/debt/native/resource qualification | [historical ledger](progress/08-evidence.md), [current ledger](progress/10-release-qualification.md) |

The original recent-fix ledger is historical input, not a claim that only seven
fixes exist. Ledgers 09/10 add the cache, checked-reader, read-only verification,
retired-KV chronology and checkpoint-restart red/green regressions, including
native failures and their corrected reruns.

## Exact current ordinary proof

Full ordinary tests ran on base commit
`48baeefe0144e2a84458c6589aa0345afc223ff8` plus the final seven reviewed
source/test/script inputs; those identical inputs then landed as a8047327.
Final overlay manifest SHA-256:
`b488f0a21705e06952cbe9583fb1c2010de4589ddce5b00fa2dcf913e8cb7326`.
Both native manifests match. The release builds subsequently used clean
detached a8047327 Git worktrees and the frozen lockfile.

Desktop ordinary-proof root:
`/media/Data/AeorDB/Tests/p9-s3-checkpoint-followup-20260911/`.

The two space-heavy cases use the executable pinned from the interrupted
full-workspace command's actual Cargo output:

```text
env TMPDIR=<campaign>/large-kv-tests/temporary <pinned-test> test_create_at_stage_clamps_to_max --exact --test-threads=1
env TMPDIR=<campaign>/large-kv-tests/temporary <pinned-test> test_resize_at_max_stage_returns_error --exact --test-threads=1
cargo test --locked -j2 --workspace --all-targets --no-fail-fast -- --skip test_create_at_stage_clamps_to_max --skip test_resize_at_max_stage_returns_error
cargo clippy --locked -j2 --workspace --all-targets -- -D warnings
bash scripts/plan/check-v4-contracts.sh
mdbook build docs
bash scripts/spec/check-v4-debt-spec.sh
bash scripts/spec/soak-cycle-spec.sh
```

These are receipt descriptions, not copy-and-paste commands with resolved
placeholders. The exact paths, commands, environment and deadlines are in
`evidence/*.guard.log` and `run-s3-capacity-linux-sequence.sh`.

Results: two separate passes, then 7,528 remaining top-level passes across
347 Cargo targets; seven existing ignores and three separately counted nested
checks. Combined unique total: **7,530**, no waived case. Full suite passes
17:01:11 UTC; strict Clippy 17:04:12; contracts 17:04:42; docs 17:05:13;
debt self-tests 17:05:43; shell helper/16 scenarios 17:06:13.
`evidence/capacity-linux-sequence.exit` records exit 0 at 17:06:14.

Current native affected commands use one job/test thread: complete CLI
`--all-targets`, plus `error_squelch_architecture_spec` and
`gc_v4_qualification_harness_spec`. macOS passes 230 CLI +38 architecture/harness
tests; Windows passes 227 +38; each retains seven existing CLI ignores.
These are affected matrices, **not fresh full native workspace runs**.
Earlier engine-correction matrices (macOS 1,192/Windows 1,185) stay separately
attributed to their final 48baeefe production inputs; overlapping reruns are not
added to invent a larger unique count.

The closed ordinary-proof manifest has 209 files and SHA-256
`6e25641e4d70228a8e622fed6e748742f7b44e8c96f766204d4d63641bc908a2`.
It includes failed attempts, frozen S3 evidence, final Linux/native receipts and
the lossless capacity archive. It does not seal the later active release run.

## Exact release and real-workload proof

Desktop release root:
`/media/Data/AeorDB/Tests/p9-release-a8047327-20260911/`.
Native roots use the same basename under `~/.cache/codex/aeordb-tests/`.
Artifact hashes are in [the completion report](completion-report.md) and the
[machine record](evidence/p9-final-qualification.json).

| Gate / exact receipt | Result |
| --- | --- |
| Linux `release-build.exit` | Pass 17:16:59 UTC, normal release, two jobs |
| macOS `release-build.exit` | Pass 17:15:40 UTC, normal release, one job |
| Windows `release-build.result.json` | Pass 17:25:44 UTC, native MSVC, one job |
| `copied-s1-release.exit` | Pass 17:17:41; strict new-CLI verify, unchanged copied failure bytes/stat |
| `pinned-soak-fixtures.exit` / `pinned-admission-fixtures.exit` | 16 scenarios/four admission cases pass |
| `live-release.exit` | Pass 17:19:13; real HTTP/docs/payload, clean restart, delete/missing, offline verify |
| `unchanged-runtime-media-proof.exit` | Pass 17:28:15; explicit predecessor attribution and new byte-preserving media-source verify |
| `release-cli-tests.exit` | Pass 17:33:15; 230 tests/21 targets, seven existing ignores |
| `pin-crash-test.exit` | Pass 17:33:46; actual Cargo JSON artifact pinned beside normal worker |
| `crash-100-pinned.exit` | Pass 18:37:48 UTC; 100 complete seven-function suites/2,300 interruption windows; 100 forced-unmount self-skips |
| `s1/s2/s3-short-pinned.exit` | All pass by 18:42:49 UTC; S3 completes 12 verified checkpoint cycles |
| `s1/s2/s3-12h-pinned.exit` | S1 starts 18:42:49 UTC; require three complete 12-hour stages and terminal integrity/resource proof |

The pinned crash manifest SHA is
`aac9bf5743e1ab4f1bf302748815748034d2b40e4293e8d381624ee3f2008460`.
Forced-unmount testing is explicitly self-skipped, not claimed covered.
Seeds control shell cycle scheduling; worker randomness is wall-clock-based.

Full media migration and the 120-second 8 GiB/no-swap overlap passed on 48baeefe.
The current gate proves all non-qualification tracked inputs unchanged and
rechecks predecessor binaries, complete receipts and media hashes. It does not
claim a new full migration or rebind an earlier binary-pinned resume manifest.
The new CLI separately verifies the retained test source with unchanged SHA/stat.
Production-scale repair/recovery and controlled performance parity remain
unproven; the corrupt multi-terabyte file is not an active qualification target.

## Final audit and cleanup — still open

After current gates finish, collect their terminal receipts, native identity,
runner/binary manifests, database hash/size summaries and resource results.
Seal only closed evidence. Retain truthful failed attempts and do not append
output to a file after including it in a seal.

Then remove only individually classified, inactive, unneeded test databases
under desktop `/media/Data/AeorDB/`. Preserve useful corruption specimens,
source inputs and logs; record exact removed paths/bytes/recoverability.
Any old seal whose disposable payload is deliberately retired must retain an
explicit cleanup record, not be silently reported as fully reverified afterward.

The retained FS-Server1 database, service, v4 activation, installation/downloads,
canary/cutover/acceptance and destructive operational GC remain outside this
authority. Stop for the owner's step-4 discussion after the authorized work.
