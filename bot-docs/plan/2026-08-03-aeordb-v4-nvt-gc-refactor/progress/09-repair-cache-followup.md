# Repair cache and progress follow-up

Entry: `c8a28c19c3bd5c6e4b62389f6639e3c8bb8e62e9`. Authorized by the retained
[handoff](../handoff-2026-09-09-gpt-6-astra.md) and the user's continuation.
This extends Child 07. The production experiment is retired; the retained file
remains immutable and offline. Source and freeze digest were rechecked at entry.

## Contracts and territory

`KvPageProvider` owns every resident-cache insertion, hit, invalidation and
eviction. `DiskKVStore` creates it for bootstrap/open/layout replacement and
prepares updates; `ReadSnapshot` reads retained generations. StorageEngine,
verification, migration and GC consume these reads through the same provider.
Updates remove pages through `remove_cached_page` before publication. Pending
and historical generation storage remains separate from resident LRU state.
The recent preparation/publication race correction is guarded by
`reader_cannot_publish_pre_update_bytes_into_the_committed_generation_cache`.

Decision: replace full-map minimum selection with intrusive previous/next bucket
links in existing cached entries and oldest/newest endpoints. Hits, removals and
victim selection use a bounded number of hash lookups; no access clock or stale
queue/heap entries remain. Existing page-byte reservations and limits are
preserved. Link memory is fixed per resident entry. No persistent bytes change.

Verification telemetry covers WAL scan/merge, KV scan/compare, directory/child
traversal, path FileRecords/chunks, snapshots, repair actions and final verify.
Periodic events must expose logical units and cache counters with bounded
overhead, preserve error/cancellation semantics, and never imply successful
completion on an incomplete traversal.

## Execution and proof

- [x] Read handoff; refresh Git, remote file identity, service and disk state.
- [x] Run cache baseline and fail the deterministic eviction-work regression.
- [x] Implement intrusive LRU; prove semantics against an independent queue.
- [x] Cover invalidation, update/abort, pressure, corruption, concurrent misses,
      snapshot preservation, hot-page churn and bounded bookkeeping.
- [x] Add and test periodic logical verification/repair telemetry.
- [x] Run cache, snapshot, memory, disk-KV, verify/repair, migration, architecture
      and static gates; exercise disposable real CLI repair/verification.
- [x] Record results and update Child 07's next action.
- [x] Resolve inherited audit landing gate and prepare the verified landing unit.

Heavy work: `wyatt-desktop:/media/Data/AeorDB/Tests/p8-cache-progress-20260909/`.
Use an isolated detached worktree and frozen lockfile, never sync `target`.
Reuse the prior repair test target on the same host. Private test scratch is
`/home/wyatt/.cache/codex/p8-cache-progress-20260909/` because `/media/Data`
does not enforce private directory modes. Keep at least 250 GB free on Data;
use bounded commands and two Cargo jobs while other desktop workloads are active.

Narrow commands: `cargo test --locked -j 2 -p aeordb --test kv_page_provider_spec`
and `cargo test --locked -j 2 -p aeordb --lib kv_page_provider`. Build deadline
20 minutes; individual cached narrow test execution should finish within 120s.
The work-count test requires exactly one candidate per capacity eviction for
16, 256, 4096 and 65,536 resident pages. Timings are supporting evidence only.

## Initial cache/progress evidence

All logs below are under the desktop evidence root above. Baseline cache: 13
tests pass. `red-cache-recompiled.log` fails with 2048 candidate examinations
for 128 evictions at capacity 16 (required 128). The replacement passes all 16
integration tests in `green-cache-integration.log`, including independent LRU
order and failed/deferred-load admission. Internal tests additionally check
10,000 mixed removals/reads and 100,000 hot-page hits with exact list invariants.

`red-progress-compiled.log` demonstrates absent phase events before the change;
`green-progress.log` passes real verification phase capture, bounded cadence,
saturation, cancellation and unwind tests. `internal-all.log` passes all 632
library-internal tests, including real repair phases, failure without false
completion, directory repair, and weak-observer lifetime/lock/poison tests.
Observers use nonblocking constant-size cache snapshots,
not historical-generation traversal, and weak references avoid retaining a
replaced cache. Progress advances at work boundaries, not during blocked I/O.

`affected-core.log` passes the cache, snapshot, memory, resilience and three
offline migration/preflight targets. Total command time including compilation:
4m44.93s; maximum RSS 4,458,040 KiB; Data free afterward 285,349,408,768 bytes.

Nonqualifying attempts are retained: `red-cache.log` reused a stale executable
because preserved rsync timestamps predated the build; transfer now omits
timestamps and test counts are checked. `green-cache.log` filtered out the
integration cases (subsequently run separately). `red-progress.log` was a test
capture borrow-lifetime compilation error, corrected before the behavioral red.

`red-cli-progress.log` reproduces hidden progress under the default CLI warning
filter. `green-cli-progress.log` passes all three real CLI regressions: default
progress plus explicit quiet override, stale-locator repair followed by separate
strict verification and exact payload readback, and malformed logging refusal
before file mutation. Only this tracing target is enabled by default; the final
report and exit status remain authoritative.

`affected-storage.log` passes 236 tests across corruption hardening, disk KV,
GC execution/mark readers, header repair, 65,536-page cache bounds, namespace
mutation and shutdown. Both legacy 5,000-entry disk-KV cases pass separately
with their own deadlines (59.68s and 58.53s test execution). The other 61 cases
also pass on final source, covering the whole 63-test target by composition.
Full workspace/all-target strict Clippy
passes (`clippy-all.log`); compatibility debt passes with 8 reviewed entries and
164 retained matches (`debt-contract.log`).

### Inherited audit blocker at entry (subsequently resolved, not waived)

The untouched entry commit fails the production error-suppression inventory:
1,540 discovered occurrences versus its 1,505-entry inventory. An independent
detached `audit-baseline` worktree reproduces this in `audit-entry-baseline.log`.
The final cache/progress source has the exact same 1,540 semantic occurrences:
both candidate inventories, normalized by removing only source position and
review metadata, hash to
`2432044fddc50f05706356aa927847215f175b018f5c2fd8647365ee70d9b735`.
At this checkpoint no limit or review policy was raised, and no production
inventory was rewritten.
The `audit-*-candidate.json` files are discovery evidence only, not approved
replacement inventories. Owner direction was requested before extending this
pass to the inherited debt.

### Broader CLI qualification finding

`cli-all.log` exposed a GC test fixture lock-reacquisition race before the
intended child-command assertion. The unchanged test also failed during a
20-round two-thread repetition (`gc-cli-repeat.log`) while 20 serial repetitions
passed (`gc-cli-serial.log`). The fixture now returns its existing engine lease;
the open-refusal case retains it and the dry-run case releases it explicitly.
No production locking behavior changed. All 100 concurrent two-thread
confirmation rounds pass (`gc-cli-concurrent-final.log`), as does the full
213-test CLI rerun. Do not count the first complete-CLI invocation as green.

## Pre-authorization verification snapshot (landing was withheld)

At this checkpoint no implementation commit or push had been made. The
`implement` skill's green-landing requirement was blocked by the inherited audit
inventory. The owner subsequently authorized that work; resolution and expanded
qualification are recorded below.

| Gate | Final result | Evidence log |
|---|---|---|
| All library-internal tests plus affected storage/migration matrix | 907 passed, 14 groups | `final-library-matrix.log` |
| Complete disk-KV target by composition | 63 passed | `disk-kv-{large,resize,remainder}-final.log` |
| Complete CLI/all-target suite | 213 passed; 7 existing intentional crash-injection ignores | `cli-final.log` |
| Concurrent GC fixture confirmation | 100 rounds, 200 test executions passed | `gc-cli-concurrent-final.log` |
| Strict workspace/all-target Clippy | passed after final fixture change | `clippy-final.log` |
| Independent contracts and compatibility debt | 454 fixtures, 95 routes, 39 docs; passed | `contract-all.log` |
| Error-suppression architecture | 28 passed, 1 inherited inventory failure | `architecture-final.log` |
| Formatting and whitespace | passed locally | `/tmp/codex/aeordb-cache-progress-20260909/fmt-final.log`, `git diff --check` |

The CLI suite exercises real server startup, HTTP requests, graceful shutdown,
offline verification, fresh/resumed v4 shadow migration, and source-byte
preservation. New CLI repair coverage repairs an intentionally stale locator,
requires durable publication and final verification, runs a separate strict
verification, and reads back the exact original payload. No production service,
cutover, installation, or retained database mutation was used for proof.

Final matrix elapsed time including build/queue time: 10m14.67s, maximum RSS
3,023,764 KiB, zero swaps. CLI suite: 3m18.75s including queue time. Final Data
free space was 275,668,258,816 bytes, above the 250 GB floor. No data was deleted.
The final read-only production check still reports the original stat tuple
`40:387:4771941773716:444:995:986:1788601656:9138000441`, immutable attributes,
and `MainPID=0` with the service failed/offline. No test AeorDB process remained
in the desktop target after qualification.
The first queued large-KV invocation (`disk-kv-large.log`) was terminated before
testing began so build-lock waiting would not consume its test deadline; its
separate final rerun is the qualifying result.

All 17 changed source/spec/manifest files are byte-identical between laptop and
desktop. Their `evidence/source.sha256` manifest hashes to
`dee7778489abd5a81ff8b9c551f635398c5160a05154277862c1f419f6baf417`.
Frozen, untracked root `Cargo.lock` remains
`06e6c7a8eb6dbccf52a0b97a4ee6edeece7297866305b0930314fd47b987faec`.

### Evidence SHA-256

```text
5e5f6da4c52e964fb57e61a478c3abb1763f4c1e7bf0bbe3562b37c63ca4611b  red-cache-recompiled.log
24b269560c7a449f96821579d90b16ea6f26d23659a57be3c6a7553ece9038e1  red-progress-compiled.log
8cd25ff1e52f85a3d3c33e7bc401cadcb2586212173d7ebe161e4bbe8e11ac8e  red-cli-progress.log
a313e13ebb83231458eab289d1e5451ad7a9bbaf4c3afaf7a470151f68b5652f  final-library-matrix.log
ad486686518120e9970683a32cd73ae50b2378bd1d94f828745a472be8f291f5  cli-final.log
0498e7888d9f4e793fa240145cadca4b6c96a597771fb0f88cf62cf2f6f8d6f2  disk-kv-large-final.log
1d92dc5a372b47c18e6e413de90696931c78a541f971c2c00c2ae5db70e7af7d  disk-kv-resize-final.log
b226b765c3cab1462b8d4b0a8c650bc0a497950c6bb672aae4845abea958c938  disk-kv-remainder-final.log
e72fc5100db778a526860d66c7c510fae0eca375078e674c2548f596d88f7980  gc-cli-concurrent-final.log
0ea10efb43f43e71a23bea98a8618937cb213a12c0c9e6e7a348abcb6f09a156  clippy-final.log
055c1b4de8e24e9d35df4a308447f4e702aa7a4bd253e0502017006d53313350  contract-all.log
5fcb2fb94762d7b1176e824bcf25799dd187bbf3e99e341529aa620a89a88ed3  audit-entry-baseline.log
eeb5940f840e69f908e235ba412e02a6ac6db9b341068df232190ebdc96d4484  architecture-final.log
```

That checkpoint was not renewed whole-campaign qualification. The complete library
integration universe, native macOS/Windows, multi-day soaks, and the 11.6 GB
media rehearsal were not repeated in this pass. Prior evidence remains linked
from the handoff but must not be represented as testing this uncommitted patch.
The retired production experiment remains retired.

### Authorized inherited audit remediation

The owner approved continuing through the inherited debt without raising the
1,505-entry ceiling. Child 08 supplies the audit/debt authority; this is not a
production authorization. Optional-value defaults are not being rewritten to
game the scanner. The selected correction consolidates checked fixed-width
format reads: repeated `offset + width` range construction can overflow before
the bounds check, and repeated slice-to-array assertions obscure the shared
invariant. Preserve wire bytes, little-endian decoding and each caller's typed
error code/class/context for ordinary truncation. Test arbitrary offsets,
truncation at every byte, signed/unsigned values and reader cursor behavior.

- [x] Reproduce overflow across the affected fixed-width format readers.
- [x] Introduce one bounded fixed-array read and remove duplicate assertions.
- [x] Run malformed-format/oracle and affected migration/GC regressions.
- [x] Review the remaining inventory without increasing its ceiling.
- [x] Rerun architecture, broad/static gates and review the cache/progress/audit landing.

`red-fixed-width.log`: all 15 affected module regressions fail with arithmetic
overflow before their checked slice access. `green-fixed-width.log`: all 15
pass (36.65s including rebuild). They exercise all 33 numeric helpers using
valid/endian/boundary values, every truncated width and offsets through
`usize::MAX`. The shared `fixed_array_at` checks the tail and then obtains a
statically sized chunk, with no offset addition, heap allocation, unchecked
conversion or fallback value. Four `BoundedReader` scalar methods share it too;
their error/cursor/accounting tests are included in the library-internal matrix.
These are direct helper regressions, not a claim that every public decoder
previously allowed an attacker-controlled overflowing offset through its outer
validation.

The fresh scanner reports 1,503 occurrences: 37 duplicate `expect` calls removed,
no newly introduced suppression identities. `audit-width-reviewed.log` records
review regeneration without `--allow-baseline-growth`; the inventory's ceiling
shrinks from 1,505 to 1,503. Source positions are refreshed, not ignored by CI.
The 36 inherited new identities were inspected separately: 15 absent CLI bound
defaults and the unitless-byte default are contractual options; CLI parse/JSON
conversions keep terminal error envelopes; subcommand selection is optional
precedence; scanner probes respect the selected recovery boundary; historical
rollback skips remain diagnostic with exact rewrite-count validation; readonly
open invariants and partial-initialization poison recovery prevent publishing
state. Preflight/native path assertions follow prior validation; registry and
mapping conversions fail closed; binary-search misses are not I/O failure.
The optional modification timestamp in the existing preverified token remains
reviewed debt, alongside its engine/size/header/hot-tail binding; this patch does
not claim to remove all existing suppression debt or redesign that token.

### Expanded qualification and inherited gate corrections

The former landing blocker is resolved, not waived. `audit-width-core.log`
passes 793 tests: 649 library-internal, 29 audit architecture, 16 cache, 6 offline
preflight, 16 real offline run/restart and 77 independent-format targets.
Elapsed time including compilation: 4m50.25s, maximum RSS 2,826,012 KiB.
`contracts-width-final.log` passes all 454 reference fixtures, 95 routes, 39 docs
and the 8-entry/164-match compatibility debt gate. Formatting and whitespace
checks pass. The original review-policy map is unchanged, SHA-256
`42ace263d99e7c554d2c162bd0fcc8627c4b1a8e010df5baf255427a8c2d9912`.

Native macOS source and evidence live at
`/Users/wyatt/.cache/codex/p8-cache-progress-20260909/`. The original dirty clone
was not reset or updated; a detached c8a28c19 worktree received the exact patch.
`native-width-affected.log` passes 942 tests across 11 groups in 183.09s including
build, maximum RSS 3,211,739,136 bytes. `native-width-cli.log` passes all 213 CLI
tests, with 7 existing intentional crash-injection ignores, in 83.87s including
build. One build job/test thread and home-backed private temporary space were
used. The first Mac contract command failed because its Python lacked PyYAML;
the rerun uses the pre-existing `v4-contract-venv`, not a system modification.
Windows SSH still refuses its forwarded connection; the owner was asked
asynchronously to restore `win11vm`. No Windows success is claimed yet.

Before the gate corrections below, all 37 changed source/spec/fixture/manifest
files matched across laptop, desktop and Mac. That `source-width.sha256` manifest hashes to
`cf5a7924436e0d80c51337066c7a356bfcfa866c29370e2c43c31dcf6214d2fd`.
The full Linux workspace/all-target suite used the separate
`target-qualification` directory, with debug symbols disabled but debug
assertions/overflow checks retained. Its wrapper enforces a 45-minute deadline
and wrote a real free-space check every minute, with termination of its own
timeout process if Data reached the 250 GB floor. Final outcomes are below.
The required echo/extract/jq/plaintext WASM guests were built locally on the
desktop with one job and frozen locks; no target artifacts crossed hosts.
See `workspace-guest-prerequisites.log` and `workspace-width-final{,.guard}.log`.

The first full-workspace attempt (`workspace-width-final.log`) stopped at the
retirement-owner caller inventory. Its unchanged entry-commit test omitted the
already-ratified `migration_offline_run.rs` orchestrator. Baseline/current hashes
match for both files (`lineage-entry-baseline.log`), so this is inherited gate
drift, not a newly added writer. Review traced chain reconstruction/resume and
all publication through shared fenced owners. The exact allowlist now includes
that adapter and adds guards against direct append/flush/control publication;
all 12 lineage-writer tests pass. The full rerun uses `--no-fail-fast` so further
failures can be collected in one pass (`workspace-width-complete.log`).

The macOS contract rerun exposed `mapfile` calls unsupported by native Bash 3.2.
The debt gate now uses line-preserving array reads and empty-array-safe expansion;
its self-test uses either timeout command and portable fixture resets. Native
and Linux self-tests pass, including empty forbidden allowlists and deliberate
refusal cases; an additional spaced-path regression guards argument boundaries.
The native full contract check now passes 454 fixtures, 95 routes, 39 docs and
the unchanged 8-entry/164-match policy (`native-width-contracts-portable.log`).
Neither the allowlist size limit nor the scanner/review policies were loosened.

`workspace-width-complete.log` completed every workspace target and found only
one further inherited inventory failure: the root-codec gate also omitted the
offline adapter's existing import and content-only semantic encoding call. The
updated gate accounts for that exact module and rejects root/control encoding
there; it additionally checks the pure template's explicit
`LegacyGlobalStateNotCaptured` semantics and absence of publication. No product
writer changed. All 14 root-migration tests pass on Linux and macOS. The first
narrow attempt is nonqualifying because its test's function-boundary lookup
omitted a generic parameter; corrected final logs preserve the successful proof.
The full final rerun is `workspace-landing-final.log`, with an unchanged production
source snapshot and all runtime cases retained. Native correction logs are
mirrored under the desktop evidence directory's `macos/` subdirectory.

## Final landing evidence

This document accompanies the coherent cache/progress/audit correction on
`development`, based on c8a28c19. Final source review found no new persistent
format, authority writer, recovery bypass, service activation or deployment
route. The correction is revertible as one source unit; no database conversion
or production operation is needed to revert it.

| Gate | Result | Evidence |
|---|---|---|
| Linux `cargo test --locked -j2 --workspace --all-targets --no-fail-fast` | 7,495 passed across 347 top-level targets; 0 failed, 7 existing intentional ignores | `workspace-landing-final.log` |
| Nested isolated index-store checks within the full suite | 3 additional child-test executions passed; no missing top-level cases | same log |
| Strict workspace/all-target Clippy after the final test correction | passed | `clippy-landing-final.log` |
| macOS affected library, complete CLI, lineage and root-migration targets | 1,181 passed; 7 existing intentional ignores | `macos/native-width-{affected,cli,lineage}.log`, `macos/native-root-codec-owner-final.log` |
| Independent contract gate on Linux and native macOS | 454 fixtures, 95 routes, 39 docs, 8 debt entries / 164 matches passed | `contracts-width-final.log`, `macos/native-width-contracts-portable.log` |
| Debt gate's malformed-policy, refusal, empty-array and spaced-path checks | passed on Linux and native Bash 3.2 macOS | `debt-perimeter-final.log`, `macos/native-debt-perimeter-final.log` |
| Production error-suppression inventory | 1,503 reviewed occurrences; ceiling reduced from 1,505; unchanged review policies | `audit-width-reviewed.log`, full workspace architecture target |
| Formatting and diff hygiene | passed | local `fmt-landing.log`, `git diff --check` |

The raw full-suite log contains 350 result lines and 7,498 passing executions.
Three are nested index-store child invocations that each filter 42 sibling
tests. Taking the last result per Cargo target yields the 347-target / 7,495-test
top-level count above, with zero top-level filtering. Do not mistake the 126
child-filtered occurrences for omitted workspace coverage.

The uninterrupted full run started at 21:01 UTC and finished tests at 21:14 UTC
on 2026-09-09; the wrapper wrote exit status 0 at 21:15 UTC after its guard ended.
Its minimum sampled Data free space was 257,770,430,464 bytes; post-run free space
was 257,770,422,272 bytes, above the 250 GB floor. No data/cache deletion was
needed. No owned Cargo, rustc, AeorDB or timeout process remained after the run.
The final Clippy rerun took 1.95s. Both native final ownership targets also
exited 0; these corrected checks add no production writer.

All **41** changed source/spec/fixture/manifest/script files are byte-identical
on laptop, desktop and macOS. The final `source-landing.sha256` manifest hashes
to `d92f59c6937aff4459c834fec3f8339b9cee0c9a88814a5422189fda0a2e41fe`.
The frozen root `Cargo.lock` hash remains
`06e6c7a8eb6dbccf52a0b97a4ee6edeece7297866305b0930314fd47b987faec`;
the lockfile, native targets, evidence logs and existing user drafts are not
part of the source commit. A fresh fetch found no upstream drift before landing.

### Final evidence SHA-256

```text
3b68cf33deadca91ff5592656a27ac1cf69855e744bb2d1dee8f96f329ddae98  red-fixed-width.log
0cc58c7959f7b6673f8c8bcc8f33fd21058f0cda9d2a366ed5e738d274541ef0  green-fixed-width.log
11d887087adb95c3dbab83cefa42f1cfea6b79373195839f9e5b38b67691d07e  audit-width-core.log
0dcbbcf1b41b92ba07ea7149b54c250db4ae66575881b58d38639aa1a4e37b91  audit-width-reviewed.log
476e67c6082fb095e9465c54d188ac0ea4274494c47ea4ef042cdff54145329e  lineage-entry-baseline.log
c1e47045bb29005a9cbd8de0bbd0c89f5f13b14551f31bdf441395596f875e2f  lineage-owner-green.log
5f3a7554fbbc611b6bf05adf2797c197c9e5ce0c8a2397e3f6effe44243b7295  root-codec-owner-final.log
3a28601f9d5a483b99aa48b6a182eba22ae7a45f4def20ecd2da5d916402cf75  workspace-landing-final.log
7608e561594f148c10010eb98d80fbe8ee3342705a53cdac4d3f4fbd669b09bb  workspace-landing-final.guard.log
fa64ce5d622bc6c73254314876d27f355c35bab87dd17967aefea46aea4804f6  clippy-landing-final.log
f9e41d6f3223f7e5eeff814b0d1384bf81307a3765612e22d5bdb361d55882d0  contracts-width-final.log
46dac2952fa989ee7d55d46aac5e54ffc6623bf0a6ea29d5dc2f64b37638069a  debt-perimeter-final.log
cfbf9d7a4a43220b93d8287781974e1a5f58a1da3a73a53c5798566189c0ba4c  macos/native-width-affected.log
4f1f96a1a13737def80c64d75e2741c79705fcb79e055aa73543efd565d79a32  macos/native-width-cli.log
f3461e9912d896a3a61e757c9ec119b0fa3b334b9582fe1408848eb32af5fe9a  macos/native-width-lineage.log
73756c496bd6f824a4bdfad997487c59a7882ad8aafa00feec38a1a164cf70f0  macos/native-root-codec-owner-final.log
e56c06909236e192446e1c062384b722e93aa11868cde0fd10c939b12c9f24ed  macos/native-width-contracts-portable.log
6e9946094c62fbd6664b2d41531e4b630db5711663ffb740730c982a24d388b9  macos/native-debt-perimeter-final.log
```

### Remaining qualification and authorization boundary

The handoff's bounded cache/progress correction and authorized inherited audit
remediation are qualified by the evidence above. This is **not** a claim that
the whole refactor has renewed all-platform release qualification. Native
Windows could not run because `ssh win11vm` refused its forwarded connection;
the owner has been asked to restore it. The full macOS workspace, multi-day
soaks and 11.6 GB media rehearsal were not repeated for this patch. Current
disposable real CLI repair, verification, payload readback, migration/restart
and source-preservation cases passed; historical large/soak receipts are not
being relabeled as evidence for this revision.

Next action: run the affected native Windows matrix after VM SSH is restored,
then evaluate remaining release qualification against the parent plan. Do not
allocate another large rehearsal copy below the recorded free-space floor.
The retained 4.77 TB production-derived database is still immutable, mode 0444,
with the exact stat tuple recorded above and its service offline (`MainPID=0`).
It was not written, copied, reopened for repair or deleted. Production activation,
retained-database reuse/deletion, deployment, cutover, destructive GC and the
first-v4-write boundary remain separately gated.
