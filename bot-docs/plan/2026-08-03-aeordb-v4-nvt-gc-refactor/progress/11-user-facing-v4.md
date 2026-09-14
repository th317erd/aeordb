# User-facing v4 completion — 2026-09-13

## Active scope and entry

Owner: Codex, direct execution with `planning-cap` then `implement`.
Entry: `9d04c76dc643f2de94fd389aac5c6a87889532fe`, `development`; fetched
origin matches, tracked tree clean before this unit. Preserve unrelated
untracked files and all sealed qualification artifacts.

Active goal: **Fully complete the full user-facing v4 refactor and have it ready
for prime-time for use in production databases.** This is the active execution
ledger; ledger 10 is closed historical qualification, not full v4 completion.
Extend the ratified parent and its eight children; do not replace frozen bytes,
authority rules, migration acceptance, or safety/resource gates.

The dependency-ordered execution bridge and audited ownership map are in
[user-facing-v4-completion.md](../user-facing-v4-completion.md). Later-unit exact
consumer/test inventory remains an entry gate, not presumed complete.

The owner explicitly wants v4 as the default for new databases and still has
existing databases to migrate. Implementation and disposable qualification are
in scope. Deployment, installation/publication, actual production migration,
service changes, or mutation/reuse of the retained FS-Server1 database remain
outside this authorization. The prior step-4 discussion is not permission to
cross those operational boundaries.

## Current facts (not a readiness claim)

Current landing: independent semantic-envelope size admission is qualified;
next are catalog leaf/internal writers and all-class definition wrapping.
See the [envelope proof](../evidence/user-facing-v4-u1-semantic-envelope-proof-20260914.json).
Owner-key validation is committed/pushed as
`7ee309c424a7fad7dc550508a2d7acdb624b7ccd`.
All ten owner-key evidence gates pass, including the separate debt
self-test; see the [owner-key unit proof](../evidence/user-facing-v4-u1-catalog-owner-proof-20260914.json).
The preceding Windows native-path fix is committed/pushed as
`3354ee917266259ab342c087f1a504fbf121d82c`.
All final2 Linux, macOS and Windows path gates pass, including the
18 Windows release regressions. The strict source, receipt, test-count,
binary and resource audit passes; see the
[native-path unit proof](../evidence/user-facing-v4-u1-windows-path-proof-20260914.json).
This unit is complete; full U1 and user-facing v4 readiness are not.
The preceding Round16 identity/dynamic-width correction is committed/pushed as
`9d3d4b83c6a3a39198af435eeac29dbb91ca2a04`, with its
[complete catalog proof](../evidence/user-facing-v4-u1-catalog-proof-20260914.json).
Earlier pending statuses below are historical records, not current blockers.

September 14 owner update: catalog-key clarification is approved, including
dynamic width from the selected database hash registry; normative Round 16 is
appended to `.codex/conversation.md`. Windows SSH is restored and the prior
43-input writer unit was qualified and landed as c222ee29. The current 44-input
dependency catalog correction passes macOS, Windows and Linux full/release
qualification. Preserve the Windows long-scratch
failure and A/B receipts; native path hardening is the required following unit.
Historical blocked/pending entries below are superseded by these updates.

- Normal CLI/server creation calls `StorageEngine::create*` →
  `AppendWriter::create` → legacy `FileHeader::new`, whose version is 3.
- Ordinary service selected-root reads bind `LegacyV3SelectedRootAdapterV1`.
- Public `migrate-v4` builds/verifies a separate offline shadow. Service
  activation, operator acceptance and first-write integration are unfinished.
- Native v4 authority publication, read views, indexes, GC and cutover state
  machines exist. Reuse and connect these owners; do not build a second engine
  or change the legacy version constant to disguise a v3 file as v4.
- Prior a804 release/crash/soak passes remain useful v3-compatible evidence.
  They do not qualify an actual v4 service and cannot satisfy this goal alone.

## TODO / proof obligations

- [x] Reconcile readiness claim with normal CLI/server/SDK source and historical
  qualification. Restore ratified parent and required child contracts.
- [x] Establish executable failing-first live normal-creation target against
  the pinned a804 release; preserve exact binary identity and observations.
- [ ] Complete producer/consumer, startup/lifecycle, persisted caller, recent-fix
  and test inventory; append evidence-backed execution extension and phase
  gates without reopening frozen decisions unnecessarily.
- [ ] Integrate v4 creation/open/admission and one runtime authority path for
  embedded, service and maintenance consumers; preserve sanctioned v3 readers.
- [ ] Complete every semantic publisher family and native selected-root reader,
  index/query/NVT, GC, durability/config/memory and restart/shutdown integration.
- [ ] Make ordinary new databases v4 by default; explicit migration remains
  side-by-side, never an automatic in-place rewrite on open.
- [ ] Finish public offline migration → verified read-only service → operator
  acceptance → first durable write, including retry, rollback and copy adoption.
- [ ] Remove transitional inactive bindings and refresh CLI/API/SDK/UI/docs;
  prove no unaccounted legacy authority/writer bypass remains.
- [ ] Run final-source actual-v4 Linux/native, fault/crash, bounded resources,
  media migration, live clients and all required full soaks; seal truthful DoD
  evidence and clean only exact unneeded disposable test database targets.
- [ ] Full adversarial contract/behavior review, coherent commits/push, report
  readiness without implying production deployment occurred.

## Completed landing unit: U0 entry proof and empty semantic state

Owned: this ledger, execution-status banner, append-only decision context,
`scripts/spec/v4-default-live-spec.mjs`, semantic-state codec, index semantic
consumer, independent reference reader and their dedicated regression specs.
The target uses an ordinary release binary, real loopback HTTP upload/read,
clean shutdown/reopen/delete/missing reads, then independently probes bytes
0–4. A v3 prefix is a target failure, never an accepted expected-pass. This
small gate is not a replacement for full format, crash, auth or service proof.

Linux execution stays on `wyatt-desktop`; private durable runtime under
`/home/wyatt/.cache/codex/`, evidence under `/media/Data/AeorDB/Tests/`.
Use a fresh namespace, never overwrite prior sealed artifacts or failure DBs.
Cargo ≤2 jobs (native 1), serialized heavy workloads, 8 GiB/no-swap memory gate,
Data free floor 250,000,000,000 bytes, home 68,719,476,736 bytes.
All long commands have host-side deadlines; monitor long work sparsely.

Next action: U0 is committed/pushed as `94eb8f32b702f868bba98ad3d42196b5d06615bc`;
U1's first four canonical writers are committed/pushed as `6cd7f3aa`.
Native conformance/availability corrections pass final Linux, native and static
qualification, including the corrected accounting-test binding. Exact-source,
test-count, binary, receipt and resource evidence is independently audited in
[the native unit proof](../evidence/user-facing-v4-u1-native-proof-20260914.json).
The native unit is committed/pushed as `369cb199`. The subsequent definition
writers and shared-evaluator correction now pass candidate-2 Linux, macOS and
Windows qualification. The strict complete evidence check passes and its
[definition unit proof](../evidence/user-facing-v4-u1-definition-proof-20260914.json)
is included in that landing snapshot, committed/pushed as `c222ee29`.
The subsequent Round16 catalog correction is also landed as `9d3d4b83`.
The Windows native-path unit is committed/pushed as `3354ee91`.
Owner-key validation is committed/pushed as `7ee309c4`.
Next: close the independent envelope-cap prerequisite and implement catalog
codecs/COW and compilation under the approved rule.
Do not repeat completed writer tests without a source or evidence reason. The original
normal-creation target remains red until actual runtime/default integration;
neither prerequisite unit completes U1 or the full readiness goal.

### Live baseline result

Executed September 13 on desktop under owned systemd unit
`aeordb-v4-default-baseline-20260913`, 8 GiB/no-swap limit and 240-second
host-side maximum. Test exited 1 in 1.452 seconds: **`3 !== 4`** at the
ordinary-format assertion. All six HTTP status checks and both byte comparisons
passed; both child servers exited normally (0, no signal). Unit has MainPID 0.
Do not interpret systemd's implausible 256 KiB reported peak as measured AeorDB
RSS; this baseline is behavioral proof, not the resource qualification gate.

Runtime evidence: desktop
`/home/wyatt/.cache/codex/v4-service-completion-20260913/v4-default-Bky6YH/`;
database 11,897,670 bytes, observed prefix `41454f5203`. TAP and exit receipt:
`/media/Data/AeorDB/Tests/v4-service-completion-20260913/`.
No production/evidence database was opened or changed.

### First bounded correction: canonical empty semantic state

Round 10 explicitly permits an empty catalog with no root object, zero root
slot and canonical zero counts; a complete state still has nonzero compiler and
semantic-registry fingerprints. `namespace.rs` writer/decoder and the independent
reference `core.rs` currently reject that valid representation. This is a
contract defect, not a new policy. Complete-empty must not become content-only.

Entry scope: codec + reference oracle; catalog/index consumers must not request
the all-zero object or infer unknown semantic state. Native selected-field reads
already reject absent field definitions explicitly. Existing nonempty and
content-only fixture bytes and errors remain unchanged.

- [x] Add independent empty-state bytes and failing reader/writer targets for
  both widths, including every root-presence/count inconsistency.
- [x] Correct codec/reference and audit complete-empty index scope/compaction
  consumers; preserve cancellation, malformed-state and budget admission.
- [x] Focused regression, reference, formatting and architecture proof; record
  exact source/lock/artifact identity before landing this coherent correction.

This is one prerequisite, not the whole integration plan or v4-default gate.

Codec red proof: `empty-semantic-red`, exit 101 at 18:22:15 UTC, no guard stop;
3/3 targeted tests failed on `semantic_state_complete_invariant` / zero catalog
root rejection, after 3m54s release compilation. Reader/writer oracles derive
canonical empty bytes from independent frozen nonempty fixtures; production
encoding does not generate expected bytes. Two widths and a 64-combination
presence/count matrix are represented.

Independent reference red proof: `empty-reference-red`, exit 101 at 18:25:10 UTC,
1 failed test, 149 filtered, expected empty combination rejected. Reference
Cargo.lock SHA-256:
`38b2314cddbad4b60cac64d5e105f0e7e40f4001a61066e06b8dc2bc7dfdf156`.

Consumer red proof: `empty-consumer-red`, exit 101 at 18:29:34 UTC, 1 passed,
3 failed, 30 filtered. Against the corrected codec and original consumer,
empty scope and compaction returned corruption; compaction also lost post-read
cancellation. The preread cancellation/budget regression already passed.

Corrected consumer skips nonexistent catalog/ordinal reads while retaining the
memory reservation until result drop, and checks cancellation after compaction's
state read. `empty-semantic-green` passes at 18:34:53 UTC: **224 tests across six
targets**, no failures/ignores/filters (34 index semantic source, 7 catalog
reader, 4 first authority, 77 format fixtures, 85 native read views, 17 root
migration). No disk guard stop; Data/home free bytes 355,208,835,072 /
73,618,620,416. Release compilation plus execution took 3m30s.

Independent reference `empty-reference-green` passes all **150 tests** at
18:38:50 UTC, no failures/ignores/filters. The initial service invocation failed
before execution because the scratch shell helper is deliberately nonexecutable;
invoking it with `/bin/bash` resolved the launch error without changing source.
These are narrow passes, not a full workspace, native, or runtime qualification.

Build source is a new detached worktree at desktop
`/home/wyatt/.cache/codex/v4-service-completion-20260913/source`, entry 9d04.
Preserved the existing dirty desktop clone unchanged; fetched refs and created
the new worktree only. Source-only rsync, frozen main lock, prior frozen portal
inputs. Reusing same-host build cache
`/media/Data/AeorDB/Tests/p9-release-33420bad-20260910/target`; pinned release
binaries and sealed evidence elsewhere are untouched. Host runner `run-unit.sh`
records input patch, hashes, 30-second disk guards and terminal receipts. No
overlapping heavy Cargo jobs, no cross-host target transfer.

Reference fixture verification also passes all 454 independent cases at
18:40:22 UTC. Local changed-file rustfmt, Node syntax, diff hygiene and debt
8/164 pass. The serialized broad U0 sequence is running under owned desktop
unit `aeordb-v4-u0-gates-20260913` (unified session 48299), with per-stage
deadlines and unchanged free-space guards. Full tests use the existing same-host
debug cache `p8-cache-progress-20260909/target-qualification`, then separately
execute the two unchanged maximum-stage KV cases on Data. Workspace Clippy,
reference Clippy, contracts and debt self-tests follow. Results are pending;
no full-suite/native/readiness claim is made. U1 read-only inventory is appended
to the execution bridge; no U1 production change is included in this candidate.

Broad-run correction: first `u0-workspace` was stopped with scoped SIGTERM at
18:48:42 UTC (exit 143) after discovering that its scratch runner omitted the
existing debug-cache profile settings (`CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`). It had only compiled, and unnecessarily consumed
about 14.7 GB of cache space; it is **not a passing test run**. No source or
assertion changed. Successor `run-u0-final-gates.sh` uses the established profile,
verifies the same eight source/lock hashes, and writes new result prefixes.
Active Linux session 57823, unit `aeordb-v4-u0-final-gates-20260913`.

Fresh isolated native worktrees at the same 9d04 base have the identical eight
source/lock inputs (checksums verified before execution); prior source/evidence
and pinned release artifacts are unchanged. Mac and Windows are running the
same six affected targets, all reference tests, and fixture verification at
one Cargo job each. Parallelism is across separate hosts only; each host runs
one Cargo process at a time. Floors remain 30 GB Mac and 8 GB Windows. Native
results are pending, not full native-workspace qualification.

Native terminal progress: macOS affected targets pass at 18:53:32 UTC, reference
150 tests at 18:54:03 and all 454 fixtures at 18:54:33; all exits 0, source
hashes unchanged, no guard termination. Windows affected targets pass at
18:57:51 UTC, with minimum sampled free bytes 19,547,877,376. Its reference
command stops before compilation at 18:57:52 because `aho-corasick 1.1.5` is
not cached and offline mode forbids download. Preserve that exit 101; a guarded
locked fetch followed by separately named `-cached` reference gates is running.
No lock regeneration, production fix, or affected-suite rerun is required by
this dependency-cache failure.

Windows continuation closes successfully: locked dependency fetch at 18:59:00,
all 150 reference tests at 18:59:56 and 454 fixture verification at 19:00:10.
Source/lock checks remain identical; final Windows free bytes 19,397,976,064.
Both native hosts pass all 224 affected tests plus reference/fixture gates.
No native workers remain. Linux full/static sequence continues; final count
and all required gate receipts remain outstanding before U0 landing.

Linux broad run has two failed targets (discovered at 19:13 UTC; the
`--no-fail-fast` suite continues to expose any further failures). All six
`default_plugins_spec` cases lack their prebuilt WASM inputs in the new
worktree. The architecture gate has one failure solely for five shifted line
numbers in the reviewed `namespace.rs` error-handling inventory. The inventory
line locations are corrected locally; IDs, patterns, review decisions and
ceilings are unchanged. No production behavior is changed by either repair.
Do not sync over the active source; restore prerequisites and rerun failed
targets after the current run closes, then complete remaining broad/static
gates. A failed parent run is retained as failed, never relabeled.

The full diagnostic run closes at 19:26:03 UTC (exit 101, no guard stop): a
third failed target, `wasm_query_e2e_spec`, has 26 missing echo-WASM fixture
failures and four passes. No additional cause beyond missing plugin inputs and
the five stale inventory locations was reported. After shutdown, synchronized
only the reviewed JSON line refresh and linked all three existing same-host
plugin targets (extract, jq, echo) into the new worktree. Original plugin/SDK
sources match entry 9d04 and all three module hashes are recorded; no build
artifact crossed hosts or was overwritten.

Successor `aeordb-v4-u0-corrected-gates-20260913` / unified session 35324 runs
all three affected targets (65 tests), a fresh complete workspace pass with
only the separately scheduled two large KV cases excluded, then both KV cases
and the remaining Clippy/contracts/debt gates. The original eight source/lock
hashes are unchanged; the additional inventory and plugin input manifests are
checked before and after. Prior failed runs remain preserved. No U0 commit or
full-v4 readiness claim yet.

The three prerequisite regression targets now pass all **65 tests** at
19:28:33 UTC, with no failures, filters, ignores or guard stop. The fresh full
workspace stage is executing (27 completed result blocks at 19:32:52, zero
failed-target/error summaries). All later stages remain pending. Evidence
mirroring is restricted to top-level logs/receipts/hashes/patches; the large
temporary KV databases and build targets are never copied to the laptop.

At 19:40 UTC, U1 audit identified a dependency catalog key ambiguity in the
frozen Round 9/10 contract (same module in parser and mapper roles, plus 32-byte
fingerprint versus H-wide reader). The exact counterexample and recommended
clarification are recorded in the execution bridge; an asynchronous owner
question is pending. No dependency key/reader/fixture change is authorized by
that proposal. U0 broad/static verification continues independently.

The corrected full Linux workspace run passes at **19:58:34 UTC**, exit 0,
no guard stop: **347 top-level targets, 7,535 passed, 0 failed, 7 existing
ignores, 2 deliberately split KV cases**. The three nested index-store child
checks also pass and are counted separately. The proven split-summary checker
accepts this exact log; all six checker regressions pass locally. The first
unchanged maximum-stage KV case passes at **20:03:04 UTC** (one exact test,
62 filtered, 242.22 seconds); the second is running on Data. Clippy/reference
Clippy/contracts/debt remain pending. No full-U0 or full-v4 completion claim.

U1 scope-writer tests are drafted separately under the task cache, not included
in the fixed U0 source: six targets cover all six frozen fixtures, five hash
algorithms, canonical paths/globs, mode consistency, Unicode/case identity and
the combined 64 KiB boundary. They are not installed, compiled or claimed green.
The native selector availability audit adds a separate required regression to
the execution bridge. Both preparatory actions preserve the running candidate.

The second unchanged maximum-stage KV case passes at **20:07:35 UTC** (exit 0,
one exact test, 62 filtered, no guard stop). Combined observed Linux proof is
now **7,537 distinct top-level tests** plus three separately counted subprocess
checks, zero failures, seven pre-existing ignores, and no unexecuted split
case. Both temporary large files were removed by their normal test cleanup;
Data free bytes returned to 340,542,517,248. Strict workspace Clippy is running.

Strict workspace Clippy passes at **20:10:05 UTC**. The separate reference
crate's strict Clippy stops at **20:10:35 UTC**, exit 101, on ten pre-existing
`nonminimal_bool` expressions in `gc_audit.rs`, `gc_mark.rs` and
`system_control.rs`; no changed empty-state code is flagged. Preserve the failed
receipt and parent sequence. Each change is only `A != !B` → `A == B` or
`A == !B` → `A != B`; all four boolean input pairs verify both equivalences.
No lint suppression or fixture change is added.

The three reference-only files are synchronized after all old workers exited.
New `u0-reference-lint-inputs.sha256` supplements the unchanged original eight
inputs and audit/plugin manifests. Linux sequence `u0-static-final` runs all
150 reference tests, 454 fixture checks, strict reference Clippy, contracts and
debt self-tests. Native `reference-lint` / `reference-verify-lint` repeat the
reference proof against the same new three-file hashes. The 7,537-test workspace
and workspace-Clippy source inputs are unchanged and their passes remain valid.
New results are pending; no failing sequence is relabeled as successful.

### U0 terminal verification and review

All required U0 gates are now green; the consolidated checker passes and records
12 exact source/lock inputs, logs and exit-receipt hashes in
[U0 proof](../evidence/user-facing-v4-u0-proof-20260913.json). This qualifies only
the bounded correction, not the complete user-facing v4 campaign.

| Final gate | Observed result |
| --- | --- |
| Linux workspace + both exact large-KV cases | 7,537 distinct passes; 347 workspace targets; three nested subprocess checks separate; seven existing ignores |
| Affected six targets, Linux/macOS/Windows | 224 passes on each platform |
| Final reference source, Linux/macOS/Windows | 150 tests and all 454 independent fixtures pass on each platform |
| Strict workspace Clippy | Pass, 20:10:05 UTC |
| Strict final reference Clippy | Pass, 20:18:07 UTC |
| Frozen contracts and debt | Pass, 20:19:43 UTC; 95 routes, 39 docs; eight reviewed entries, 164 retained matches |
| Debt self-test | Pass, 20:20:13 UTC |
| Changed-file formatting, Node syntax, diff hygiene, count-checker regressions | Pass |

The final reference/native rechecks retain all source hashes and lockfiles.
Native workers are closed, as is the final Linux sequence (exit 0, 20:20:13 UTC).
Final Data/home free bytes: 340,522,127,360 / 73,511,997,440. Implausibly small
systemd memory peaks are not treated as runtime RSS proof. All prior failed and
interrupted receipts remain preserved and named in the report.

Final review traced every complete-state branch: scope resolution and compaction
skip absent objects but preserve admission/cancellation/reservation lifetime;
native selected-field reads already report an explicitly missing definition;
runtime recovery already accepts an empty descriptor set; migration/transfer
classify completeness without inventing a catalog edge. Nonempty/content-only
bytes and error meanings are unchanged. No normal creation/default/backend,
production service, installed binary, retained database, or sealed evidence was
changed. U1 through U7 and the live normal-v4 default target remain outstanding.

## Current landing unit: U1 semantic production

Entry is the green U0 commit `94eb8f32`; direct owner, no delegated changes.
The bounded first slice writes canonical `ScopeDefinitionV1` bytes and identity
from already-canonical typed inputs. Owning config normalization remains a
later compiler responsibility; this codec must reject noncanonical requests,
not silently choose normalization or new scope semantics. Frozen Round 7
bytes, maximum combined length, ID domain and resolver behavior remain intact.

- [x] Prove the dedicated six-test `scope_definition_writer_spec` target red.
- [x] Implement preflighted canonical scope encoding; prove exact independent
  fixtures/IDs, all hash algorithms, malformed modes/paths/globs and boundaries.
- [ ] Continue all seven definition/projection families, canonical catalog COW,
  native conformance/availability and bounded compiler integration from the
  execution bridge; scope encoding alone does not complete U1.
- [ ] Run affected, reference, native, static and broad gates at the coherent
  writer landing boundary, then commit/push. Preserve all U0 regression guards.

The new test target is installed locally; production scope code is unchanged.
Run it on a fresh isolated desktop worktree at the green entry, using source-only
sync and the same-host debug build cache with two Cargo jobs. Preserve U0 source,
logs, lockfiles and all sealed historical artifacts. The class-6/7 catalog-key
question remains pending owner approval; no proposed key change is implemented.

Desktop entry uses new detached `source-u1` at 94eb8f32, with the old U0 source
untouched. Its first admission refused the absent root `Cargo.lock` before
Cargo ran; that file is untracked, unlike the reference lock. Copied the exact
frozen root lock (no regeneration) and verified the five U1 input hashes and
all three same-host plugin fixtures. The real red target is now running under
`aeordb-v4-u1-scope-red-ready-20260913`, two jobs, zero debug/incremental, normal
disk guard and 1,800-second host deadline. No scope production change yet.

Scope writer red closes at 20:28:18 UTC, exit 101: the new target cannot import
the missing `ScopeDefinitionWriteV1` and `encode_scope_definition` symbols.
No guard termination; source/lock inputs match. Added the bounded canonical
encoder after this failure, preserving the frozen envelope, validation and ID
domain. Refreshed only the three existing scope-reader audit line positions;
no review or suppression ceiling changed. Green execution is next. Each new
runner stage snapshots its own source-input manifest before executing, so
later implementation cannot change an earlier run's input evidence.

Scope writer green closes at 20:39:51 UTC: all six tests pass, no ignored or
filtered cases, source hashes unchanged, no guard termination. Data/home free
bytes: 340,515,901,440 / 73,455,755,264. This is narrow writer proof; full U1
and actual-v4 readiness remain outstanding. Prepared a second six-test target
for fixed-size invocation-policy writing: all six independent fixtures, each
resource-field offset, finite/nonzero and native/WASM context matrices, and
WASM32 memory alignment/address-space edges. Decoder, parser-plan/mapper
consumers and format hardening are the affected perimeter. The new policy
writer API is not implemented; run its red target next.

Invocation-policy red exits 101 at 20:41:59 UTC on its missing encoder symbol.
Implemented a fixed 128-byte stack encoder, reusing exact reader validation
before returning bytes, with no variable output allocation. The first affected
run exits 101 at 20:44:37: architecture 28 pass/1 fail solely because rustfmt
collapsed an existing conditional and moved four reviewed dependency-reader
locations by four lines. Other targets did not execute after that fail-fast
stop. Corrected those four inventory positions only; preserve the failed run.

Next independently falsifying target is `source_selector_writer_spec` (seven
tests): all 14 frozen selector fixtures, each metadata ID, ordered typed path
segments, malformed regex/key/mapper arguments, policy/profile/dependency
consistency and the combined 4 KiB cap. Owning configuration normalization,
corrected-vs-migration ingress and executor availability remain compiler/runtime
responsibilities; the codec cannot create catalog bindings or resolve the
pending dependency-key ruling. No selector production writer exists yet.

The first selector red run exits 101 at 20:47:33 UTC on the two missing writer
symbols plus a test integer inference error caused by the absent API type.
Made the test's metadata ID explicitly `u16`, reran unchanged production, and
`u1-selector-writer-red-typed` exits 101 at 20:48:52 on only the intended missing
symbols. Both receipts are retained. Then implemented the typed canonical
selector encoder: count/combined-length preflight precedes regex/argument
validation and output allocation; policy bytes reuse the fixed policy writer;
the caller's slice order, UTF-8 bytes and full u64 indices are preserved.

`u1-three-writers-affected` is now running 13 targets with `--no-fail-fast`, so
one failure cannot hide later regressions. It includes all 19 new writer
tests, exact fixture/hardening, catalog/root/semantic consumers, native parser
and parser-resource tests, and the corrected architecture inventory. Exact
ten-file source/lock manifest is snapshotted per stage. Results are pending;
no policy/selector green or U1 completion is claimed yet.

Terminal result: `u1-three-writers-affected` exits 0 at 20:52:20 UTC. All
**214 tests across 13 targets pass**, with no ignored/filtered/failing cases:
architecture 29, native parser resources 3, native parser 12, native semantic
source 6, native source 8, index semantic source 34, policy writer 6, scope
writer 6, semantic catalog reader 7, selector writer 7, format fixtures 77,
reader hardening 2, root migration 17. All ten inputs remain unchanged; no
guard termination, final Data/home bytes 340,498,968,576 / 73,455,050,752.
Filtered local `u1-evidence/` mirror contains logs/receipts/manifests/patches only,
not databases or target artifacts. Full U1 and broad/native/static writer
landing gates remain owed. Next dependency-safe slice: parser-plan encoding
against the eight frozen APRP fixtures and exact candidate/context/size rules.

Parser-plan red exits 101 at 20:59:30 UTC on only the absent
`encode_parser_resolution_plan` symbol. Its eight tests compile from typed
programs independent of production decoding: all eight fixtures, none/explicit
context, candidate policies/dependencies, MIME and registry order, native/raw
footer and family consistency, 512/513 registry entries, combined 128 KiB
boundary, and exact legacy match bytes. Added the bounded encoder after red;
factored the existing reader's plan/candidate checks into shared private
validators instead of introducing another interpretation. Decoder behavior and
wire bytes are intended unchanged. Writer preflights count/combined bytes,
validates all policies/program context, then allocates one output buffer.

`u1-four-writers-affected` now runs 14 targets (prior 13 plus parser writer)
against 12 exact source/lock inputs. Full log, terminal receipt and guard data
remain required before any green claim. No catalog key, native fingerprint,
production database, installed binary or ordinary creation default changed.

Parser-inclusive terminal result: `u1-four-writers-affected` exits 0 at
21:03:26 UTC; the full-log counter verifies **14 targets, 222 passes, zero
failures**, including all eight parser-writer cases. Input hashes unchanged;
no guard termination; Data/home free bytes 340,493,099,008 / 73,453,232,128.
Strict library all-target Clippy is now running on that same snapshot.
Broad/native/reference landing proof and the rest of U1 remain incomplete.

### U1 first four writers: frozen landing candidate

Strict library all-target Clippy passes at 21:09:31 UTC (zero warnings under
`-D warnings`). Fresh upstream comparison is 0/0 against `origin/development`
at 94eb8f32. Freeze the four writers and four new specs plus manifest/inventory
and both locks (12 checksummed inputs); no further production edits during
these gates. This is a coherent prerequisite landing within U1, not U1 closure.

Linux unit `aeordb-v4-u1-writers-gates-20260913` runs the whole workspace,
separate exact maximum-stage clamp/resize cases on Data, workspace Clippy,
reference tests/454 fixture verification, contracts and debt self-tests. The
two large-KV cases are relocated, not waived. Expected count from the last
green baseline plus 27 new tests is 7,564; only terminal receipts can establish
the actual count. Same-host debug cache, two Cargo jobs, debug/incremental zero,
16 GiB build guard/no swap, unchanged disk floors and host deadline remain in
force. At 21:16:53 the workspace was compiling with Data/home free bytes
340,492,275,712 / 73,418,354,688; no result count yet.

Both native hosts have new detached `source-u1` worktrees at 94eb8f32, with all
12 inputs verified and their existing frozen portal siblings reused. The exact
source-only archive SHA-256 is
`006458befb357033ae6bf0674e6310312e10c684b104c8da094f12f66f9b9245`;
it contains no target/database artifacts. macOS and Windows now each run the
14 affected targets, reference tests and 454 fixture verification with one
Cargo process/job per host. Their evidence is separate `u1-writers-evidence/`;
all U0 sources/receipts remain intact. Native and broad results are pending.

Native terminal proof is complete: macOS exits 0 for affected tests at
21:18:22 UTC, reference tests at 21:18:55 and fixtures at 21:19:25. Windows
exits 0 at 21:20:56, 21:21:19 and 21:21:31 respectively. Mechanical full-log
counts confirm **222 passes across 14 affected targets, 150 reference tests,
and all 454 fixtures on each platform**, no failures/ignores/filters. All
12 source inputs and before/after patches remain unchanged; no guard stops.
Final free bytes: Mac 68,102,377,472; Windows 19,175,534,592. Only small
receipt/log/patch evidence was mirrored locally. Linux full/static sequence
remains active; the 21:28:29 observation showed heartbeat tests progressing,
no failure summaries, Data/home free bytes 340,492,304,384 / 73,325,969,408.

The full Linux workspace exits 0 at **21:47:53 UTC**: **351 top-level targets,
7,562 passed, zero failed, seven existing ignores, two explicitly split large
KV tests**. Three nested subprocess checks pass and are counted separately;
the strict split-summary checker accepts the full mirrored log. No guard
termination, source inputs unchanged, final Data/home free bytes
340,491,935,744 / 73,324,806,144. The first exact large-KV case is now running
on Data; its sibling and final static/reference gates remain outstanding.
Changed-file rustfmt, diff hygiene and six count-checker regressions pass.
The consolidated writer-proof checker correctly refuses an incomplete suite
without creating a success report. Next-U1 drafts remain outside this frozen
candidate, not installed or passing evidence.

### Late writer-review finding: MIME name initials

Do not commit the four-writer unit yet, even if its currently frozen gates all
pass. Review found a pre-existing validator defect inherited by the new
parser-plan writer: production `parser_plan::is_canonical_mime_essence`, the
independent reference equivalent, and native `corrected_mime_essence` all
permit punctuation in the initial position. Round 9 adopts RFC 6838 restricted
names; [section 4.2](https://www.rfc-editor.org/rfc/rfc6838.html#section-4.2)
requires an ASCII letter or digit first. Names such as `!abc/plain` or
`text/+abc` must not be canonical corrected registry keys. Malformed stored
MIME becomes generic for corrected extension fallback; legacy exact matching
must remain unchanged.

Preserve the running candidate and finish its remaining unchanged gates as
baseline evidence. A separate three-test draft `mime_name_initial_spec.rs`
under the durable task cache covers the writer, independent reader mutations
at both widths, and actual native parser execution with legacy protection.
After the active sequence closes, install/run that target red, add the
independent reference regression, fix the whole three-site perimeter, and
qualify the final corrected source before the coherent writer commit. No
frozen fixture needs regeneration and no production database is involved.

The actual workspace formatting gate (`cargo fmt --all -- --check`) also
identified four current-unit files needing layout correction. Earlier explicit
changed-file checks incorrectly selected Rust edition 2024; the workspace is
edition 2021. This is not a green workspace-format result. After the frozen
sequence closes, apply the workspace formatter, review only the current-unit
diffs, and refresh any resulting audit line positions without changing review
ceilings. The late MIME correction and final source qualification will include
that layout correction. Upstream refresh still reports 0/0 at 94eb8f32.

Initial sequence closes at **21:59:54 UTC**, exit 0. The second large-KV
case passed at 21:56:54, workspace Clippy at 21:57:24, reference/fixture
checks and contracts/debt through 21:59:54. All **7,564 distinct workspace
tests** passed; final Data/home free bytes 340,393,828,352 / 73,314,529,280.
The consolidated [initial proof](../evidence/user-facing-v4-u1-writers-initial-proof-20260913.json)
passes its checker and explicitly says this unit is **not complete** until
the late correction is qualified. Original twelve-input manifest is archived.

Installed the three-test MIME target and the independent reference's separate
one-test module without changing production. New per-stage input manifest
contains 17 exact source/lock files. `u1-mime-library-red` is running on the
same isolated desktop source under the normal bounded runner. Preserve this
red evidence before correcting production; final actual workspace formatting
and behavioral gates remain required before the writer commit.

MIME red results are now preserved: the three library tests fail at 22:02:00
UTC (writer acceptance, reader acceptance, and wrong native fallback), and the
independent reference test fails at 22:03:15 on the same invalid-name acceptance.
Added the ASCII-alphanumeric initial check to the parser-plan canonical validator
and independent reader; native normalization now reuses the production validator
instead of maintaining a third restricted-name interpretation. Legacy exact
matching and stored metadata bytes remain unchanged. No fixtures regenerated.

Actual edition-2021 workspace formatting was applied to the four current-unit
files only, restoring the original four dependency inventory line positions.
Both `cargo fmt --all -- --check` and standalone reference formatting now pass.
The final affected sequence closes at 22:09:43: **225 passes in 15 library
targets**, **151 reference tests**, **454 independent fixtures**, strict release
reference Clippy; no failures or guard stops. Seventeen exact inputs remain
unchanged. Original red and initial-snapshot receipts are retained separately.

Final frozen qualification starts around 22:13 UTC: Linux unit
`aeordb-v4-u1-final-gates-20260913` runs the whole workspace, both exact Data
large-KV cases, workspace Clippy, reference/fixture and contracts/debt gates.
Native Mac/Windows each run the 15 affected targets and reference tests/fixtures
in separate `u1-final-evidence/` directories. The seventeen-file manifest SHA-256
is `27771c9b13a3caead7a1f1e27a59751169f33c7c6b81ea1e5a3d7a256ec0c055`;
source-only archive SHA-256 is
`f2ead71b5c9069c45df48d7f675c09c5f4d8fc5783930cb611b7181b7b46c54c`.
No source changes during qualification. Expected workspace total is 7,567
distinct passes, but final terminal/count verification is still owed. No writer
commit, U1 closure, default switch, deployment or production readiness claim yet.

Final native receipts now close green. Mac affected/reference/fixtures finish
at 22:15:03 / 22:15:34 / 22:16:06 UTC; Windows at 22:17:41 / 22:18:04 /
22:18:15. Full-log counters verify **225 passes/15 targets, 151 reference
tests, 454 fixtures on each host**, all seventeen source hashes checked again
afterward, unchanged before/after patches and no guard termination. Final free
bytes Mac 68,072,730,624 and Windows 19,144,691,712. Only evidence files were
mirrored. Linux full/static gates remain active; this is not unit completion.

Full current-unit source/test review after the MIME fix found no further
writer-layout or bounds issue. Next-slice draft review caught a vacuous direct
selector test: `parsed_value: None` already yields dependency-unavailable in the
old implementation. The draft now supplies a valid parsed map, so only the
missing fingerprint gate can explain the intended failure. Still not installed
or executed; no unearned red/green claim.

Final Linux main-suite receipt closes at **22:49:27 UTC**, exit 0, with
**352 top-level targets / 7,565 passes**, seven existing ignores, two separately
scheduled large-KV cases and three nested checks counted separately. The strict
full-log counter accepts these totals. The large-KV/static sequence remains
live under unit `aeordb-v4-u1-final-gates-20260913`; do not commit until all
terminal receipts pass. Data/home free bytes at main-suite exit are
340,346,564,608 / 73,325,395,968.

### Next-U1 failing-first proof without changing the frozen source

The idle Mac's exact already-qualified debug library was used for two
standalone `rustc --test` probes. Its SHA-256 is
`7f4ee6421538b2aff48e5c325ce0fbed814d80b59791936458cf5c24ab842716`;
dependency metadata identifies `source-u1`, and all seventeen source inputs
and before/after patches remain unchanged. Compilation has a 120-second
deadline; each tiny test run has a 30-second deadline. No additional Cargo
process, production edit, target transfer or database artifact transfer occurs.

- `native-public-boundary-red`, **22:49:07 UTC**, exit 101: three tests fail
  at the intended boundaries. The public native parser panics for a ten-byte
  GIF and for the first overflowing WAV byte rate. Exhaustive prefixes of
  eleven tiny media seeds find only `tiny.gif/10` in that prefix corpus.
  Test source SHA-256:
  `2d5428aafc0922ba3d595a8a828db9a10cb0e408d8bde59a5eaf8bcf4bd997df`.
- `dependency-availability-red`, **22:50:45 UTC**, exit 101: four target
  tests fail as intended (ADPT/AVST unknown-profile rejection, direct selector
  wrongly returning Missing, and shared evaluation doing parser work before
  refusing an unavailable selector). Two guards pass: known malformed records
  remain rejected and completed canonical values remain queryable.
  Test source SHA-256:
  `ab08dae687e440008275bc6a0f31c7bbca4e958066acfefa0250129dea317fc9`.

These are real red regressions for the next semantic-runtime slice, not fixes
or broad/native qualification of that slice. Their small logs, input hashes,
exit receipts and unchanged-source proofs are mirrored in same-named folders
under the local durable campaign cache. Test sources remain outside the frozen
writer candidate until its coherent landing. The corrected archive-MIME test
and all four native conformance bundles still require execution. The pending
dependency-catalog key ruling is not changed or implied by these results.

The third standalone Mac probe, `native-archive-mime-red`, closes at
**22:55:33 UTC**, exit 101: corrected output fails on
`docx/application/octet-stream` versus its detected canonical MIME; the legacy
four-format guard passes. Source SHA-256 is
`1ee68667ebfa010e1c4b962735a9c5f5273b337daaabf9a9f23c3194fcaae58b`.
It used the same unchanged qualified library with the existing local ZIP and
temporary-directory dependencies, and isolated temporary databases under the
Mac campaign cache. Source checks and unchanged-patch proof pass; only small
evidence was mirrored. The corrected test stops on its first reproduced
failure, so this red run does not claim all corrected format branches were
reached. All four native conformance bundles remain unqualified drafts.

### Canonical-writer landing proof complete

The final Linux sequence closes **23:02:58 UTC**, exit 0, last stage
`complete`; unified session 4769 is terminal. Both exact large-KV tests pass
(clamp 22:53:57, resize 22:58:27), workspace Clippy passes at 23:00:58,
reference tests/fixtures at 23:01:28 / 23:01:58, contracts at 23:02:28 and
debt self-tests at 23:02:58. Combined proof is **7,567 distinct workspace
passes**, seven existing ignores and three nested checks separately counted;
**151 reference tests / 454 fixtures**, **95 routes / 39 docs**, reviewed debt
**8 entries / 164 retained matches**. No source drift or guard termination.
Final Data/home free bytes: 340,346,249,216 / 73,324,855,296.

The final [writer proof](../evidence/user-facing-v4-u1-writers-proof-20260913.json)
was generated only after its fail-closed checker verified all required exit
receipts, strict whole-log counts, both exact relocated cases, native source
identity, formatting, unchanged seventeen-file inputs and disk guards. Native
disk logs were additionally checked across all six stages (thirteen samples,
all above their host floors). The Linux systemd process completed in 49m31.633s
with a reported 16.0 GiB rounded peak and zero swap. This is the **16 GiB build
guard**, not evidence for the final 8 GiB production-shaped runtime budget.

Full source/test diff review and actual edition-2021 formatting pass. Upstream
refresh remains 0/0 at 94eb8f32. This completes only the four-writer landing
unit; U1 semantic production, the default-v4 live target, native execution
corrections, service/cutover integration and the full readiness goal remain
open. The proof explicitly retains those limitations and original failures.

## Active U1 native semantics and execution availability

Entry is writer commit `6cd7f3aa0fe25af3f3bc8bd7dbe2b12055126b01`, pushed to
development with upstream 0/0. Direct owner only. Preserve qualified `source-u1`
and use a fresh detached desktop `source-u1-native`; reuse same-host targets
only. No production operations or dependency-key contract change.

- [x] Reproduce native framing/arithmetic, archive MIME, and availability
  defects against the unchanged qualified Mac library; preserve all three
  red receipts and the passing malformed/legacy/completed-value guards.
- [x] Install the eleven regression tests in normal Cargo targets and confirm
  the expected failing baseline on the desktop before production correction.
- [x] Correct GIF/WAV boundaries and corrected archive metadata; prove adjacent
  malformed prefixes and numeric boundaries without changing valid legacy data.
- [x] Execute and refine the four hand-authored native conformance bundles,
  freeze their reproducible identities, and replace placeholder identity checks.
- [x] Separate structural dependency retention from exact execution availability
  in library/reference readers, direct extraction and shared producer/query
  evaluation; completed canonical-value queries must remain usable.
- [x] Run affected, independent, static, full and native gates on the final
  unit source before a coherent landing. Then continue remaining definitions,
  catalog/COW/compiler; the catalog-key owner ruling remains a prerequisite.

Desktop normal-Cargo baseline `u1-native-baseline-red` closes at **23:10:34
UTC**, exit 101: availability 2 pass/4 fail, archive MIME 1 pass/1 fail,
public parser boundaries 0 pass/3 fail. All six inputs pass pre/post SHA
verification; no disk guard termination. Data/home free bytes are
340,298,936,320 / 73,262,759,936. Source is detached `source-u1-native` at
6cd7f3aa, not the preserved writer-qualified source. No Cargo remains running.
Upstream refresh at 23:17 UTC is 0/0. Next: extend the archive scalar-policy
perimeter before correction, then fix native boundaries and stored metadata.

### Native corrections and structural-retention focused proof

The first added archive-policy probe failed to compile (integer compared with
the typed candidate enum); this is **not** behavioral red evidence. After the
test-only correction, `u1-native-mime-policy-red2` fails at **23:22:35 UTC**:
the 32-byte stored-MIME policy case rejects DOCX incorrectly; the below-limit
guard passes. Independent reader tests both fail at **23:23:05 UTC**, preserving
separate `u1-native-reference-availability-red` receipts.

Corrections keep the GIF dimensions already present in ten bytes and guard the
optional packed byte; widen WAV arithmetic before multiplying; carry original
stored MIME through corrected Office/ODF builders; and check native metadata
scalar size before copying/claimed parser work. Legacy archive builders retain
their detected MIME. Library and independent reference retain unknown ABI and
executor profiles, while still rejecting known kind/role/ABI/profile conflicts.

Desktop `u1-native-corrections-green` closes **23:28:16 UTC**, exit 0:
**12 focused passes** (3 public boundaries, 4 archive/limits, 5 structural
availability guards including a 504-case permanent-ID matrix). The two still
unfixed direct/shared selector-execution tests were explicitly filtered from
this narrow run; this is **not** a green full unit. The independent reference
suite closes **23:28:46 UTC**, **153 passes**, exit 0. All seventeen staged
source hashes remain unchanged and no disk guard fires; final Data/home free
bytes 340,200,124,416 / 73,263,599,616. No commit or readiness claim.

Next, the four draft conformance bundles and three external private-owner
harnesses are being installed, not fingerprinted yet. Before their first run,
review corrected two draft transcription errors: WAV duration is floating
`0.0`, and two canonical i64 values consume **26**, not 18 bytes (each has
the frozen five-byte frame plus eight-byte payload). No previously frozen
fixture or qualified evidence is rewritten. Runtime availability and final
affected/full/native qualification remain open.

### Conformance catches corrected MIME parameter routing defect

`u1-native-conformance-initial` closes **23:36:09 UTC**, exit 101:
**11 of 12 tests pass**. The remaining failure is valid quoted-pair escaping
in a MIME parameter: `application/problem+json; q="a\"b"` incorrectly returns
no essence through `mime::Mime`. Full native family outputs/dispatch/prefixes,
raw-JSON independent bytes/failure distinctions, and both-width selector
order/quotas pass. The MIME test stops at its first failing vector, not a claim
that all remaining cases ran.

The expanded parameter grammar regression closes red at **23:41:50 UTC**;
the normal stored-file integration regression closes red at **23:45:31 UTC**,
proving the JSON claim is lost for the escaped-quote case. RFC 9110
[quoted strings](https://www.rfc-editor.org/rfc/rfc9110.html#section-5.6.4) and
[parameters](https://www.rfc-editor.org/rfc/rfc9110.html#section-5.6.6) confirm
the ratified grammar; whitespace around equals is forbidden while empty
semicolon-delimited slots are allowed. The 22 added parameter cases and
512 generated ASCII class checks are not yet green.

The corrected parser now uses a dedicated bounded `mime_router` owner: validates
parameter syntax without copying it, retains the original metadata, and copies
only the at-most-255-byte normalized essence. Legacy exact routing is untouched.
The integration test's native-text case uses `text/markdown`, because the old
definition fixture explicitly routes `text/plain` to a registry WASM parser.
That fixture's registry is preserved, not bypassed to force a native pass.
Fresh conformance/affected proof is next; fingerprints and selector execution
availability are still not completed.

Desktop `u1-native-mime-grammar-green` passes **13 tests** at **23:52:02 UTC**.
The subsequent integration failure was traced to a stale ordinary library:
its Cargo dep-info omitted the new `mime_router.rs`, and its rlib timestamp
was 23:45:21, newer than source timestamps preserved during the later rsync.
The separately compiled unit-test library did include the new module. A stored
MIME assertion and complete returned outcome confirmed the integration fixture
itself was correct. Diagnostic stage closes 23:55:09, exit 101.

Touching the two changed owning modules forced an ordinary-library rebuild
without changing any source bytes or deleting targets. Then
`u1-native-mime-affected-recompiled` passes **26 tests / five targets** at
**23:57:44 UTC**, including the stored-file JSON/native/legacy regression,
measured parser resource limits and all four archive formats. All 33 input
hashes remain unchanged; no guard stop. Final Data/home free bytes:
340,199,124,992 / 73,261,010,944. The earlier affected/diagnostic failures are
retained as stale-build evidence, not unresolved MIME behavior.

Future source synchronization uses checksum comparison and fresh destination
timestamps (`rsync -acR --no-times`, explicit source files only); never transfer
Cargo targets. Final-source broad gates must rebuild changed owners and retain
source/dep-info proof. The next test compares native parser identity against
the reviewed specification/fixture framing; the placeholder string hashes have
not yet been replaced. Selector availability remains open.

### Native identity and execution-support integration

The first identity probe failed to compile because the typed parser error has
Debug but no Display; it is not behavioral red evidence. After correcting that
test, `u1-native-identity-red2` closes **2026-09-14 00:01:50 UTC**, exit 101,
rejecting all three independently derived parser bundle fingerprints. The
adjacent mapper probe closes **00:02:20 UTC**, exit 101: a structurally retained
unknown ABI reaches the current mapper executor. These are preserved before
production correction.

Four literal native identities now derive from the reviewed two-file framing
documented in the conformance README. The parser checks one exact registry;
the selector runtime retains unknown definitions without compiling a substitute
and gates direct extraction and shared evaluation before parser work. Unknown
mapper ABI/profile is likewise refused before invoking the supplied executor;
actual WASM artifact installation/resolution remains U4 work. Original binary
format fixtures remain byte-identical; execution-test copies bind to current
native identities and recompute their dependent definition IDs.

`u1-native-execution-conformance-green` passes **16 tests** at **00:06:54 UTC**.
The availability run then passes six and fails two at **00:07:54 UTC**:
legacy unlimited output sentinels overflow shared setup before the new typed
unavailability result. The follow-up correction retains the decoded runtime but
does not compute execution allocations for an unavailable selector. This does
not clamp semantic limits or qualify known legacy unlimited execution.

Additional differential guards compare 1,024 generated integer canonical
encodings with independent bytes, 256 nested map/traversal cases with a
materialized ordering model, and 512 WAV rates with u128 arithmetic. These are
new proof, not frozen-fixture changes; they have not yet passed. The next
conformance/availability run uses 44 hashed inputs. The suppression inventory
refresh changes only ten source line locations, retaining all 1,503 reviewed
occurrences and their classifications; no baseline growth or new suppression.

The corrected setup passes **19 conformance tests** at **00:12:23 UTC** and
**all eight availability tests** at **00:13:23 UTC**, without filtering any
availability cases. All 44 hashes remain unchanged; no guard termination.

The 19-target affected run closes **00:16:01 UTC**, exit 101. Four targets
expose one execution-fixture binding mismatch: the original selector's stable
ID is `/org/aeordev/aeordb/native/aeor-regex-v1`, not the draft bundle's
`regex-selector-v1` suffix. The helper had silently left this dependency at its
placeholder identity, so real producer and selected-query paths correctly
reported it unavailable. The stable component ID is now preserved; the helper
requires every native dependency to bind, and the original .bin fixtures remain
unchanged. The conformance folder keeps its descriptive `regex-selector-v1`
name, distinct from the component's canonical ID.

The corrected selector two-file digest is
`eae50800c658f804c4bda0d0b20331399db77f078545759f2f20f52b88fe34d6`,
computed by a small same-host Rust/BLAKE3 utility against the exact framing.
The earlier `6aa293...` digest is an unlanded draft, not an accepted production
identity. No executable writer has published it. Both prepared native source
copies require the corrected snapshot before their first execution; no native
Cargo job was started against the draft. Fresh conformance and full affected
proof must pass before broader/native qualification.

The corrected-ID conformance run passes **19 tests** at **00:21:31 UTC**;
the full affected run passes **488 tests across 19 targets** at **00:23:31 UTC**,
with no failed, ignored or filtered affected cases. Static analysis stops at
**00:25:31 UTC** on one unnecessary clone in the new completed-value test.
The test now borrows the canonical value through `std::slice::from_ref`; no
production behavior or conformance input changed for this static correction.

Final-source input manifest (44 files):
`22ad44447abf46f61ba8c3ed6c5ed672aeb40c3a8e53751ae48299ebef0b4202`.
Source-only archive `u1-native-execution-source3.tar`:
`cdeb95844af18388544921d3e025e5f0a7b920f9761f9549a2efcfdb312f39f4`.
Linux availability/static recheck and the four-stage native Mac/Windows
qualification are running against that snapshot. The native gates have fresh
evidence directories; no draft selector identity was executed there.
Whole-workspace, release-boundary, reference and static-contract proof remains
required before landing. Upstream is still 6cd7f3aa, 0/0 after a fresh fetch.

Linux final availability recheck and whole-workspace Clippy pass, final static
receipt **00:29:06 UTC**. Native Mac closes all four gates at **00:31:22 UTC**;
Windows closes at **00:35:47 UTC**. Each passes **19 conformance tests**, the
**488-test / 19-target affected set**, **153 independent reference tests**, and
**454 independent format fixtures**. Conformance filters unrelated unit tests
(661 Mac, 658 Windows); no conformance case is skipped. Windows retains an
existing unused import warning in the Unix-only task-retention test helper;
this is not a native Clippy qualification claim.

Final free bytes: Mac **67,876,990,976**, Windows **18,948,304,896**. No native
disk guard or deadline fired, and pre/post source hashes and patches match.
Only small logs, receipts, manifests and patches are mirrored to the laptop.
The Linux full workspace is running under the owned
`aeordb-v4-u1-native-final-20260914` unit, with sequential exact large-KV,
reference, release-boundary and static-contract stages still pending. This
does not complete U1 or qualify an ordinary v4 database/service.

### Full-workspace gate catches one remaining execution-fixture binding

The source3 full-workspace run has **679 library passes and one failure**:
`json_regex_workspace_accounts_for_worst_case_escape_expansion` still loads the
old placeholder selector identity and expects executable-regex workspace. The
new availability gate correctly retains no executable segments, giving 4,200
bytes instead of the expected 25,170,024. This is not a green full run; its
`--no-fail-fast` execution continues unchanged to collect remaining failures.

The existing test is relocated to the external selector harness, preserves its
24 MiB escape-expansion assertion, binds its decoded test copy to the exact
current selector, and now covers both hash widths. An adjacent test checks that
an unavailable selector retains only definition memory, no compiled segments,
and reports typed unavailability. No production behavior or golden .bin bytes
change for this correction. Native final qualification will execute the whole
library test set, including this previously missed test, not only the narrower
`conformance_spec` filter. Prior receipts remain immutable historical evidence.

That full run closes at **01:07:20 UTC**, exit 101: **355 top-level targets,
7,599 passed, one failed, seven existing ignores and two intentionally split
large-KV cases**; three nested subprocess tests pass separately. The sole
failure is the accounting fixture above. No guard fires. Free Data/home bytes
are **340,196,892,672 / 73,182,568,448**; the driver does not run any later
stages after the failed workspace. The failed log/receipt/source snapshot is
also mirrored locally, before source synchronization resumes.

The corrected 44-file manifest is
`4b7e48b05ecd83df7f1be1a0cbefa2b5eb5e08604d50ee8e5e0eaf309cdfb3cb`;
source-only archive `u1-native-execution-source4.tar` is
`f71395b81646c94f7bfc318e66ad79013770fd7f5aca76113cbea0c0029c5bd5`.
Both actual Cargo format checks pass with unchanged hashes. Source4 is now
verified on all three hosts. Fresh stages use `u1-native-final2-*` / native
`u1-native-final2-evidence`, preserving all previous runs. Linux conformance
and static checks are active before the new whole-workspace qualification.

Source4's **21 conformance/accounting cases pass at 01:13:00 UTC** (660
unrelated library cases filtered in this narrow target), and strict workspace
Clippy passes at **01:15:30 UTC**. Full Linux and native final2 gates start at
**01:17 UTC**, with no source edits while they execute. Native library scope
is expanded to all unit tests; the affected integration/reference set is
unchanged. Final source4 format checks also pass. Full results remain pending.

Native final2 is now green: Mac closes **01:21:27 UTC** with **681 library
tests**, Windows closes **01:28:21 UTC** with **678 library tests**. Each
also passes **488 affected tests / 19 targets**, **153 independent reference
tests**, and **454 format fixtures**. Source hashes and pre/post patches match;
no timeout/disk stop. Final free bytes are Mac **67,859,484,672**, Windows
**18,946,170,880**. The library sets include both accounting regressions.
Linux full qualification remains active, no failure observed at 01:32 UTC.

### Next definition unit: independently reproduced contract corrections

These are outstanding U1 definition work, **not changes to the frozen native
qualification snapshot**. Read-only normative review resolves two reader/writer
disagreements without a new product choice:

- Round 8A section 1 explicitly preserves child caps **4/64/128/256 KiB**
  (field/selector/parser/dependencies). The production selector reader/writer,
  AVST child validator and reference currently cap selectors at **4 KiB**;
  the copied machine registry repeats that error. No later override was found.
- Round 9 section 1 explicitly makes canonical `none` the only parser plan
  valid for `always_missing_v0`, superseding the Round 8A earlier non-none
  statement. Round 8A section 6 says document input is zero for `none`.
  Current AVST/reference readers and the two old always-missing examples
  instead require a real legacy parser pipeline and nonzero document input.

Six standalone in-memory probes run against the unchanged, source4-qualified
Mac ordinary library; **one baseline guard passes and five intended contract
tests fail at 01:30:56 UTC**, exit 101. Failures cover the selector reader,
selector writer and AVST parent accepting >4 KiB through the exact 64 KiB
boundary, accepting canonical parser-free always-missing bytes, and rejecting
the superseded non-none example. Each test stops at its first failing case;
this is not a claim that all subsequent widths/boundaries executed.

Evidence is `semantic-definition-contract-red` under the durable campaign
cache. Library SHA-256 is
`b6389e94d0b727ad157ae86be0f442327916fa1a179d7c98edc59ced3fa43941`;
probe source SHA-256 is
`39245fd428f06f999b9d06c3f67fec843c48bc82100cce261ec2ebfd14576ad0`.
All 44 inputs and the ordinary library remain unchanged. The probe and runner
are retained alongside small logs/receipts locally; no test binary or database
is transferred. The native test-binary digest packet is separately preserved.

Before completing the remaining definitions, install these normal-Cargo targets,
expand malformed/boundary cases, reconcile independent reference/registry
expectations to the normative contracts, and preserve old binary artifacts as
historical evidence. Do not rewrite those artifacts merely to make a writer
round trip pass. This does not resolve or approve the separate class6/7
catalog-key owner question.

The next-unit cap territory also includes generated
`v4/contract_generated.rs` (`source-selector-v1` hard cap 4096), the
reference's selector fixture generator, and
`source_selector_writer_spec::selector_writer_preflights_combined_lengths_and_counts_before_regex_work`.
Preserve the existing 4,096-byte examples as valid historical cases, add the
true 65,536-byte boundary and one beyond, and prove the independent 1,024
segment-count boundary (the former byte cap prevented reaching that count).
The AVST writer draft now uses the normative 64 KiB child bound; it remains
outside the frozen candidate and has not been installed or executed.

### Native final2 Linux qualification in progress

The workspace closes **01:54:15 UTC**, exit 0. Both separately executed
large-KV tests pass: clamp **01:59:45 UTC**, resize **02:06:15 UTC**.
Independent reference passes **153 tests at 02:06:45 UTC** and verifies
**454 fixtures at 02:07:15 UTC**. Release-mode public parser/archive boundary
checks are compiling; contracts/debt remain queued. Full target/test counting
and same-host executable hashes still require the final evidence audit.

The next definition-unit draft now also contains seven converter/field writer
tests in the durable task cache, not the qualified source. They independently
parse the frozen Round 11 offsets, compare all 25 converters and corresponding
fields at both widths, check all five hash algorithms, and cover malformed
parameters/children, identity widths, limits and meaningfully changed inputs.
The proposed writer derives strategy names, masks and fingerprints from the
registry rather than accepting caller-authored semantic overrides. These APIs
do not exist yet and the draft is neither executed proof nor a production edit.

Additional cap-consumer inventory: `v4_format_fixture_spec` has an explicit
4,097-byte amplification assertion to move to the true 65,537-byte boundary.
Its complete fixture loops currently assume every selector/AVST example is
valid. Preserve both old always-missing binary artifacts as explicitly rejected
historical examples, add new canonical-none examples, and update independent
manifest/annotation/outcome metadata transparently; do not mutate the old bytes.

### Native landing unit: final verified evidence

Linux final2 closes **02:16:17 UTC**, exit 0, with the owned unit inactive and
MainPID 0. Release-boundary checks pass **seven tests at 02:15:16 UTC**;
contracts pass **02:15:46 UTC**, debt self-tests pass **02:16:17 UTC**.
Final Data/home free bytes are **337,891,184,640 / 73,165,402,112**.
The build unit peaks at **15.4 GiB with zero swap**, under its 16 GiB build
guard. This is explicitly **not** the final 8 GiB production-runtime proof.

The independent evidence checker passes: **355 top-level workspace targets,
7,601 passed, seven existing ignores, two explicitly split KV cases**; both
split cases separately pass, giving **7,603 distinct workspace tests**. The
three nested subprocess cases pass and are counted separately. It also checks
all narrow/static/native/reference/release receipts, guards and exact 44-file
source manifests, actual Cargo format receipts, and same-host hashes of
**23 Linux, 21 Mac and 21 Windows** executed test binaries. Only small evidence
files cross hosts; no binary or database transfer.

The first binary-hash audit stops before writing because it also selected the
CLI's `aeordb` test executable by basename. Cargo names both CLI and library
tests `aeordb`; selection now additionally requires the library's
`unittests src/lib.rs` header. The full workspace retains the CLI test results.
This evidence-tool correction neither changes production source nor excuses a
test failure. The retried 23-binary packet and complete evidence audit pass.

All original binary format fixtures remain byte-identical; the suppression
inventory still has the same 1,503 reviewed entries. Final source and test diffs,
all four native specifications/fixture bundles, direct/shared availability,
legacy routing and completed-value preservation have been reviewed. No new
native-unit defect remains open. The earlier failed runs remain preserved.

U1 semantic production and the full v4 goal are **not complete**. Remaining
definition/canonical compiler/catalog work, the pending owner catalog-key
ruling, actual WASM execution, legacy operational budgets, U2–U7 runtime,
migration-to-service/default integration and final qualification still stand.
No production database, service, install, release or default was changed.

## Active landing unit: remaining semantic definition writers

Entry/last green: `369cb19943fe1ca8b0595f180c9b3a05d7956d98`, pushed to
development with upstream 0/0. Direct integration owner. The prior native
source/receipts stay preserved; use a fresh detached desktop
`source-u1-definitions` and same-host build cache only.

Owned: dependency record/table, ValueStore, converter and field definition
writers; selector/AVST reader cap and always-missing context corrections;
independent reference, exact corresponding registry/fixture expectations,
shared source-evaluator correction, targeted consumer regressions, normal Cargo
targets and this ledger.
Forbidden in this writer unit: class6/7 catalog changes (the following Round 16
unit owns those approved corrections), physical/root publishers,
runtime/default/service activation, production and retained evidence databases.

Test/territory entry: all four existing definition decoders have independent
fixtures; no corresponding production encoders exist. Writers derive frozen
semantic fields from typed input and the permanent registry, validate complete
borrowed children before allocating output, preserve canonical dependency
ordinals/order, and return domain-separated IDs at the selected database hash.
No new alias/coercion or catalog-key policy is chosen here.

- [x] Install and execute the ten contract regressions before changing readers;
  install dependency (nine), ValueStore (five), converter/field (seven) writer
  tests and record missing-API red compilation separately from behavioral red.
- [x] Correct the ratified 64 KiB selector cap and parser-free always-missing
  context in production/reference/registry, preserving old binary artifacts as
  explicit historical rejections and adding independent canonical examples.
- [x] Implement bounded dependency/table, ValueStore, converter and field
  writers against independent exact bytes/IDs, malformed and boundary tests.
- [x] Expand parent/child closure, allocation preflight, ordering/duplicates,
  cross-family and changed-input identities; audit every affected consumer.
- [x] Run narrow, affected, static, independent, full and native gates against
  the final unit; record actual byte/hash/resource evidence in the coherent
  writer landing snapshot.
  Continue catalog/COW/compiler only within the ratified owner-key boundary.

The contract draft has four added adjacent cases beyond the original six-probe
red packet: independent 1,024-segment count, malformed framing at 64 KiB,
no-content-work/cancellation for always-missing, and invalid family/limits.
These additional cases have not yet run. All 100 named converter/field fixture
paths are checked present; that inventory is not execution proof.

Definition baseline completed **02:28:42 UTC** in the fresh detached checkout.
The ten behavioral regressions yielded one characterization pass and nine
intended failures. Each writer target separately failed compilation on its
missing public API (E0432); those are not behavioral passes. All 18 source/lock
inputs stayed unchanged and no resource guard fired. Receipts use prefix
`u1-definition-baseline-` under the existing desktop campaign evidence folder.

The adjacent dependency-flag audit added two more behavioral regressions:
known corrected ABI/profile records must reject migration-only identity flags;
a corrected AVST closure must also reject those flags when both executor IDs
are unknown. Both reproduce on unmodified production at **02:37:09 UTC**,
exit 101, zero passes/two failures, ten filtered. Explicit migration records
and standalone unknown-executor retention remain required positive controls.
Stage `u1-definition-flags-red`, no guard stop, all 18 inputs unchanged.

First production narrow pass: **02:44:31 UTC**, all **33 tests** across the
four new targets (dependency nine, converter/field seven, contract twelve,
ValueStore five). All 18 inputs unchanged, no guard termination. The four
writers now preflight lengths/closure before fallible output allocation;
ACNV/AFIX derive semantic fields and fingerprints from the permanent registry.

Independent reference red: **02:45:54 UTC**, all five separately written
contract regressions fail against the original reference readers. The corrected
reference now has six additional fixture cases planned: two canonical-none
AVST definitions and both-width selector examples at 65,536/65,537 bytes.
Old always-missing bytes are preserved as explicit archival rejections, not
rewritten to make them valid. Fixture generation/verification is still pending.

The initial full reference run (`u1-definition-reference-green`) was deliberately
terminated with scoped SIGTERM after **6m44s**, not passed. New 64 KiB fixtures
made its exhaustive per-byte copy/decode/hash loop quadratic. The successor
uses the scope oracle's established bounded large-fixture approach: retain
every byte mutation of all original ≤4 KiB examples, cover all added framing
and invocation-policy bytes, and sample large scalar interiors/end boundaries.
This changes only new-case mutation enumeration, not any acceptance assertion,
production limit or old fixture bytes. Full reference and generation must rerun.

Bounded reference rerun completes **03:00:04 UTC**, all **158 tests passing**
(13.82 seconds execution). Isolated generation then verifies **460 independent
cases at 03:00:36 UTC**, with all **455 pre-existing binary fixture files**
byte-identical. The extra binary beyond the 454 format cases is the separate
SystemFamily fixture. All 22 source/lock inputs remain unchanged; no disk guard
fires. Final Data/home free bytes: **337,746,141,184 / 73,604,513,792**.

Generated differences are restricted to six new cases and annotations, two
archival AVST annotation/outcome updates, manifest/result metadata, and generated
registry identity/selector-cap constants. These exact files are now installed
locally and in the disposable desktop checkout; no executable or DB was copied.
New allocation tests measure oversized-input preflight and inject one output
reservation failure in each encoder. ValueStore tests also cover each child's
identity, malformed/cross-family closure and parser-free migration-only output.
The expanded production suite is executing against 39 frozen inputs; no full
definition-unit, native, runtime/default or production-readiness claim yet.

Expanded production run closes **03:10:36 UTC**, **125 passed / one failed**
across eight targets, all 39 inputs unchanged, no guard stop. All 40 new
writer/contract/resource tests pass, as do seven selector-writer tests, all
77 format tests and the bounded mutation corpus. The sole failure is the
existing hardening test assuming identical reference/production error names
for a newly added rejection. Its adapter now maps the two new reference codes
explicitly and also requires the precise production error class. No production
acceptance or error code changes. The expanded suite must pass its fresh rerun.

A complete dependency flag matrix was also added (native zero-only; corrected
WASM artifact-only; explicit migration and unknown standalone profiles retain
their permitted identity flags). This raises the new-target total to 41 and
the eight-target narrow total to 127. A serialized narrow → workspace Clippy →
reference Clippy run is in flight before broader qualification.

The rerun closes **03:14:34 UTC**, all **127 tests across eight targets pass**.
Workspace Clippy passes **03:17:34 UTC**, independent reference Clippy passes
**03:18:04 UTC**, both with warnings denied. All 39 frozen inputs are unchanged,
no disk guard fires; final Data/home free bytes are **337,704,448,000 /
73,089,830,912**. Actual workspace/reference Cargo format checks also pass
locally without modifying those inputs. Live P0b/P0c reports now refresh current
hashes/counts with explicit prior values and exact new stage references;
historical native-unit proof remains untouched. Next: exact contract/debt gates,
affected consumers, final-source Linux and native qualification, then review
and land this definition unit. U1, default cutover and full readiness remain open.

Final-candidate-1 affected consumers pass **03:29:42 UTC**, **483 tests in 26
targets**; contracts pass **03:30:12 UTC**, debt self-tests **03:30:42 UTC**.
Mac candidate-1 passes 681 library tests, the same 483 affected tests, 158
reference tests and 460 fixtures by **03:33:41 UTC**. Its small evidence packet
and 28 same-host executable digests are mirrored. Windows SSH is refused; an
owner availability question is pending. These results do not waive Windows.

The final consumer review found an additional shared-runtime defect:
`AuthoritativeSourceEvaluatorV1` accounts hypothetical output for a canonical
always-missing selector. An independent three-test probe, linked against the
qualified Mac library without modifying its source, fails **03:40:20 UTC**:
4,144 bytes reported instead of zero, legacy unlimited-bound arithmetic
overflow, and a needless admission failure at memory capacity. All frozen
source/library checks remain identical. Probe and receipts are preserved in
the durable `always-missing-evaluator-red` folder; this is an executed defect,
not an unverified follow-up.

The owned Linux candidate-1 broad run was stopped with scoped SIGTERM at
**03:41:45 UTC** after 13m03s. It is **not a workspace pass**, despite systemd
reporting a successful supervisor exit. The explicit interruption report records
the TERM result, source manifest, earlier completed stages and resource samples;
no production service was involved. The full candidate must rerun.

Candidate 2 includes the narrow shared-evaluator correction in this same landing
unit. Always-missing has zero output/workspace and returns Missing after
validation/admission and cancellation checks, without parser or output
reservation. It does not alter legacy limits for real value-producing sources.
The permanent five-test target adds both-width producer/query coverage, legacy
unlimited values/counts, pressure, cancellation, memory release, metadata output
and pressure controls, invalid identities/framing and initial admission.
Source evaluator has no reviewed-suppression location changes. The frozen
manifest is now **43 inputs**, SHA-256
`b852ffeaa85efc290b681ec869ba36049a0adb468db7faee58f884026da65290`;
source2 archive SHA-256
`1c3b88fc66ac5f5bcece94a1cff761b8b3b81c7c6c23254eab30994bae7664f7`.
Actual format completed **03:43:44 UTC**. Candidate-2 narrow (488 tests/27
targets) and static checks are running before full/native requalification.

Candidate-2 focused/affected gate passes **03:46:41 UTC**, all **488 tests in
27 targets**, including all five shared-evaluator regressions. Workspace and
reference Clippy pass **03:49:12 / 03:49:42 UTC**; actual format checks pass
**03:48:53 / 03:48:54 UTC**. All 43 input hashes remain identical. No disk
guard fires; final Data/home free bytes **337,690,886,144 / 73,179,873,280**.
Full Linux and native Mac candidate-2 qualification are now running. Windows
still refuses SSH; no Windows worktree/build has been created for this unit.
The old fixture audit also independently confirms all 460 SHA-256/lengths,
452 untouched old manifest rows, exactly two explained archival outcomes, and
six added cases. No fixture bytes or definition identity rules changed in the
shared-evaluator correction.

Candidate-2 macOS qualification completes **03:55:29 UTC**: **681 library
tests, 488 affected tests across 27 targets, 158 independent reference tests,
and 460 fixtures**, all passing. The 43-input manifests, before/after source
patches, terminal receipts and exact counts are independently checked after
mirroring small evidence files. Same-host hashing records 29 executed test
binaries; none are transferred. Final free space is **67,715,596,288 bytes**.
Linux contracts/debt gates pass **03:52:27 / 03:52:57 UTC**. At **04:02:45 UTC**
the full workspace is progressing through storage tests, with zero swap and
Data/home free bytes **337,690,333,184 / 73,176,702,976**. Full Linux later
stages and Windows remain outstanding; native Mac success does not waive them.

Final review has inspected the current production writers/shared evaluator,
all six new test targets, affected adapters, independent reference changes,
registry/fixture audit and report updates against the ratified contracts. No
additional candidate defect was identified. A fresh upstream comparison is
**0/0 at `369cb199`**, and all 43 frozen inputs remain unchanged. At
**04:07:52 UTC** Linux continues through engine HTTP/fork tests. Windows SSH
still fails; the desktop's configured forwarding port 22220 has no listener.
No VM or service configuration was changed. Required Windows qualification and
the separate catalog-key owner ruling remain explicit outstanding gates.

Candidate-2 Linux full workspace completes **04:29:58 UTC**, exit 0. The
mirrored log is independently audited: **361 top-level targets, 7,647 passed,
zero failed, seven existing ignores, two explicitly split maximum-KV cases**.
Three nested subprocess tests also pass and are counted separately. The exact
43-input manifest matches. Final workspace Data/home free bytes are
**337,689,808,896 / 73,172,537,344**. The sequential driver remains active on
the separate large-KV stages, followed by release/reference gates. Those later
stages, same-host Linux executable digests and Windows qualification are still
required; this result alone does not complete the unit or full v4 goal.

### Candidate-2 Linux terminal evidence and outstanding prerequisites

The sequential Linux driver completes **04:42:29 UTC**, exit 0 and
`last_stage=complete`. Both separately executed maximum-KV cases pass at
**04:33:58 / 04:37:58 UTC**, giving **7,649 distinct workspace passes** in
total. Release execution passes **46 tests across six targets at 04:41:28 UTC**;
the independent reference passes **158 tests at 04:41:59 UTC** and verifies
**460 fixtures at 04:42:29 UTC**. All stage receipts report no guard termination
and preserve the identical 43-input manifest. Final Data/home free bytes are
**336,590,168,064 / 73,172,074,496**.

All small terminal receipts, source identities, logs and guards are mirrored
under the durable task cache's `u1-definition-evidence/` directory. The Linux
driver and same-host binary hasher have both exited successfully; no candidate
Cargo jobs remain. Local digest-packet verification confirms **35 Linux and
29 macOS executed test binaries**, each covering 29 distinct targets. No binary
or database was transferred. Packet SHA-256 values:

- `u1-definition-final2-linux-test-binaries.json`:
  `be048c111ea1cb87d9015955346d24b537780d78faa869cf67011eb243541bc7`.
- `u1-definition-final2-macos-test-binaries.json`:
  `f9137e63003aee5f1f0dbddca417f293bd2d447e50c4029196efc53274e459ad`.

The recorded **04:42:00 UTC** cgroup observation has a 16 GiB peak, zero swap,
335 `max` events and zero OOM/OOM-kill events. The exact snapshot is retained in
`u1-definition-final2-memory-observation.json`. This is bounded build/test
evidence, **not** the final 8 GiB actual-v4 production-runtime qualification.

The strict `verify-u1-definition-final2-evidence.mjs` checker successfully
audits Linux and macOS gate evidence, then exits 1 on the absent Windows
`u1-definition-final2-windows-evidence/library.result.json`. This is a missing
required platform run, not a Windows test failure and not a full checker pass.
No `u1-definition-final2-complete-proof.json` was written. Separately checked
all 43 current source hashes, both available binary packets and diff hygiene.
Windows SSH remains refused at **04:50 UTC**; its definition worktree/build
has not been created. No VM or service configuration was changed.

Required next actions remain Windows qualification of the frozen source2
candidate, the complete strict evidence check, then final staging review and
a coherent green commit/push. Catalog-key owner approval is independently
required before changing class6/7 keys. Neither missing prerequisite is waived;
the current unit, full U1, runtime/default integration and full v4 production
readiness remain incomplete. The source changes stay preserved and uncommitted
at base `369cb199`; no production, installation or real migration was performed.

Blocked audit, **04:52 UTC**: the previous goal turn completed Linux work and
evidence capture (progress); this continuation finds no additional safe action
after revalidating the same prerequisites. Windows SSH still refuses forwarding,
HEAD and all 43 source hashes remain unchanged, and no owner contract ruling
has arrived. These blockers have recurred across at least three consecutive
goal turns. Completed Linux/Mac qualification and bounded U2 inventories do not
waive either gate. The full goal is blocked, not complete; no test reruns, idle
monitors, source changes or production actions were started in this audit.
Resume from the Windows qualification or catalog ruling when the corresponding
prerequisite changes, retaining all remaining U1–U7 obligations.

## September 14 catalog identity entry evidence (next U1 unit)

Owner approval is recorded in Round 16; this inventory does not change the
currently qualifying writer source. `namespace.rs::decode_catalog_leaf` checks
database-derived `H` for classes 3–7 but does not require a class6/7 owner to
equal its semantic ID. `decode_definition` only checks the generic envelope,
not the class6/7 payload's canonical dependency bytes and domain-separated ID.
`SemanticCatalogReaderV1::with_definition` checks object/class/stored-ID closure
but does not validate the owner key when called directly. Existing scope/value/
field consumers validate their own identities through the shared helper; no
production complete dependency-catalog emitter or dependency ID-domain use was
found. Migration destination initialization still emits content-only state.

The independent reference `core.rs` likewise lacks dependency payload identity
and catalog owner-shape checks. Its fixture profiles cover Blake3-256 and
SHA-512; production v4 hashing supports all five registered algorithms through
`v4/hash.rs`, not legacy `HashAlgorithm::compute_hash`. Tests must cover all
five without making the raw artifact digest database-dependent.

A byte/SHA audit of all ten manifest semantic-object fixtures finds only class2
definitions/catalog bindings, no class6/7 binding to rewrite. This is a source
fixture/caller audit, not inspection of any user database. Preserve those
existing bytes and add explicit new independent fixtures for the corrected
dependency contract.

Draft `dependency_catalog_identity_spec.rs` in the durable task cache contains
eight failing-first/positive-control tests: all-algorithm keys, same-artifact
parser/mapper/runtime distinctions, same-width mismatch, raw-artifact key
rejection, wrong-width keys, stale/cross-domain IDs, malformed/wrong-class
dependency payloads, and direct resolver bypass. Draft SHA-256 is
`17aef88185e15022f96cb8816d5e58362a033105fa0674c9c2613ed23cba7507`.
It has been formatted but **not executed or installed**. First land the current
writer candidate after native Windows proof, then install and reproduce these
failures before implementation. Catalog COW/compiler and U2–U7 remain required.

A separate independent-reference draft has five tests covering positive keys,
owner/fingerprint disagreement, wrong widths, stale/cross-domain IDs and
malformed/wrong-class payloads at both fixture profiles. Durable cache file
`dependency_catalog_contract_spec.rs`, SHA-256
`0dc3d7bd1b468187a1abdab000bfb390f8d2564b4c061fe0d795476e8e4b1c7d`.
It also remains unexecuted/uninstalled pending the current landing boundary.

Windows execution resumed **05:57:25 UTC** after source-only transfer and
creation of a detached `source-u1-definitions` checkout at `369cb199`. All 43
source hashes pass; previous dirty native checkout remains intact. Library
stage exits 0 at **06:02:27 UTC**; affected integration tests are running
(Cargo PID 8576 observed at 06:02). Counts and complete native evidence still
require the final audit. Free space observed 19,447,435,264 bytes. The prior
native library already had the Windows-only unused retention-test import
warning; compare the current mirrored warning before recording it as unchanged.

### Windows completion and writer landing proof

Windows completes all four candidate-2 stages **06:08:28 UTC**: **678 library
tests**, **488 affected tests across 27 targets**, **158 independent reference
tests**, and **460 fixtures**, all passing with unchanged source/lock hashes,
identical before/after patches and no guard termination. Affected execution
completed 06:07:48; reference completed 06:08:13. Final free bytes are
**19,082,612,736**. The single Windows-only unused retention-test import warning
matches the prior native unit; no new warning or suppression was introduced.
The complete driver has exited 0 and no Cargo jobs remain.

All small native evidence is mirrored under the task cache's
`u1-definition-final2-windows-evidence/`. Same-host hashing records 29 executed
binaries; only its JSON packet was transferred. Packet SHA-256:
`246caf50c51fe29ff9c501fb32483344d526ec063fff9cf23c2ec2a510f89a0c`.

The full strict checker now **passes**, auditing all required Linux/Mac/Windows
receipts, exact test counts, guards, source identities, fixture history, format
checks and 35/29/29 executable digest records. It generated
`evidence/user-facing-v4-u1-definition-proof-20260914.json` only after those
gates passed. Final source/reference/fixture/report diffs and the unchanged
43-input candidate have been reviewed. This closes the writer qualification
unit, not all U1 or v4 runtime readiness. Round 16 approval is recorded in this
snapshot; catalog behavior changes and their new tests remain the next unit.

## Completed U1 landing unit: dependency catalog identity and dynamic width

Entry/last green: `c222ee29a0dc3dd6c22d36868f5042dc6ad4621e`, pushed to
development. Owner: direct integration agent. Owner ratification: Round 16.
Isolated desktop checkout: durable campaign cache `source-u1-catalog`;
previous writer/native checkouts and sealed evidence remain unchanged.

Owned: class6/7 dependency identity derivation/validation, semantic catalog
bindings and direct definition resolution, independent reference equivalents,
new exact fixtures/registry metadata and required generation, dedicated tests,
Cargo targets, reviewed error-location metadata and this ledger.
Forbidden: arbitrary artifact fingerprint changes, blanket acceptance of mixed
widths, silently reinterpreting existing ambiguous bindings, runtime/default
activation, separate mutable physical/KV owners, and production databases.
Catalog COW/compiler remain subsequent U1 work after this reader/identity gate.

- [x] Execute eight production and five reference contract tests against the
  unchanged implementation; record failures separately from positive controls.
- [x] Enforce complete class-domain dependency IDs and registry-derived width
  at low-level binding/definition and direct resolver boundaries.
- [x] Expand adjacent malformed/class/domain/cancellation and allocation paths;
  add independent both-width fixtures and all-five-algorithm production proof.
- [x] Run affected reader/root/index/source regressions, independent/static,
  full/release/native gates on frozen final inputs; preserve old fixture bytes.
- [x] Review and independently verify all exact-source evidence for this
  coherent correction. This snapshot is the catalog landing unit.
- [ ] Next: Windows path hardening, bounded catalog writers/COW and compilation,
  then U2–U7; no full v4 readiness claim.

Named narrow commands (guarded desktop runner, Cargo two jobs):
`cargo test --offline --locked -j 2 -p aeordb --test dependency_catalog_identity_spec`
and `cargo test --offline --locked -j 2 --manifest-path tools/v4-reference/Cargo.toml dependency_catalog_contract_spec`.
The reference filter intentionally selects only the new five red tests; full
reference/fixture qualification remains a later mandatory gate.

RED evidence: owned desktop unit `aeordb-v4-u1-catalog-identity-red-20260914`
completed at 06:19:48 UTC; unified 9318 closed 0. Production: **3 pass / 5
expected failures**, reference: **1 pass / 4 expected failures**; both Cargo
stages exited 101. The supervisor's exit 0 means both expected RED outcomes,
not passing tests. Failures reproduce wrong owner identity, raw artifact keys,
stale definition IDs, correctly hashed malformed/wrong-class dependency
payloads, direct resolver bypass, and reference width acceptance. Positive
controls retain complete identities and unknown executor metadata. Six frozen
source/lock inputs matched before/after; no guard stop. Small evidence mirrored
to campaign cache `u1-catalog-red-evidence`; old source manifests and receipts
are immutable historical inputs for this baseline. Next: add direct load-time
cancellation and adjacent boundary proofs before the smallest reader fix.

Adjacent RED: `u1-catalog-identity-adjacent-red`, unified 53281 closed 101,
finished 06:29:43 UTC. Twelve production tests: **5 pass / 7 expected failures**;
new failures prove load-time cancellation can reach the callback and correctly
hashed empty/truncated payloads are accepted. Passing controls cover direct
valid dynamic-width resolution, callback/source error propagation, missing
definitions and substituted class/semantic/object bindings. Six inputs unchanged,
no guard stop; these assertions were executed before their production fixes.
Namespace validation now reuses bounded canonical single-record decoding,
recomputes class6/7 semantic IDs, and requires matching owner keys. Direct
resolution repeats identity closure and checks cancellation after load and
before callback. Reference implementation remains independently constructed.

Candidate-1 focused GREEN: unified 46342 closed 0 at 06:39:46 UTC. Production
15/15, full reference 163/163, independent generated fixtures 472/472. All 14
frozen source/lock hashes matched before/after, manifest SHA
`2e499164d3aa01612170fefda5f0202d685cd350edb65d997b41c0a87d414f0a`.
Twelve added fixtures cover parser/mapper/native definition and binding pairs
at both widths; historical binary fixtures compare byte-for-byte equal.
Production proof covers all five algorithms, same-width cross-algorithm
rejection, ID-versus-object-ID confusion, all record truncations, bounded
borrowed component lengths, and cancellation before load/during load/before
inspection. Next add the independent fixture traversal/resolution test, refresh
only reviewed error line positions and generated metadata, then freeze the
final candidate for affected/static/full/release/native qualification.
Mac/Windows baseline catalog worktrees were created at c222ee29, no builds yet;
existing qualified worktrees remain untouched. All large work stays on desktop.

Preflight candidate adds real catalog traversal plus definition resolution over
all six independent dependency fixture pairs (16 identity tests total), imports
12 new binary/annotation pairs, and preserves all **461** previously tracked
binary files. The fixture registry's progress count is 472; final regeneration
changes only the two registry digest constants in `contract_generated.rs`.
The reviewed-error inventory changes exactly nine line positions, retaining
all 1,503 occurrence identities, patterns, reviews and its fixed ceiling.
Five-stage preflight (format/reference lint/three production targets/fixtures)
is running under unit `aeordb-v4-u1-catalog-preflight-20260914`, unified 18470,
42-input manifest `bd408bfbfe26a160f12f5ce31751f2b8304f8a8eaacbca983c077327a5c093e3`.
Final affected-reader coverage expands the previous 27 targets with this new
identity target, `migration_root_map_owner_spec`,
`v4_index_native_compaction_spec` and `v4_semantic_object_store_spec` (31 total).

Following U1 catalog-writer prerequisite: the independent generic reference
leaf decoder still lacks production's pre-existing control-owner canonical
path and class3–5 owner-width validation. This class6/7 ruling does not change
those classes' identity policy. Cover those existing rules with independent
malformed fixtures when landing the all-seven-class catalog codec/writer; do
not treat this dependency-only unit as completion of that broader codec gate.

Final candidate frozen 06:56 UTC: **44** source/lock/report inputs, manifest SHA
`6811f96ce094e5f5db8ca0eae04e658c11734028eeee8c30270151f6212ddff7`;
source-only archive SHA
`c7af7892216ae93966cff037b4477712b2cb7d54fac17bce404b6445b28eccc0`.
Preflight unified18470 closed0 at06:51:36: 16 identity /77 format /29 audit
tests pass, formatting and strict reference lint pass, 472 fixtures verify.
P0b/P0c live hashes/counts now link those executed receipts; previous refresh
history is retained. Only metadata changed after the 42-input preflight.
Native source import verified the same44inputs and exact44archive members on
both hosts; no target or executable was transferred. Qualified prior checkouts
remain unchanged. Linux reuses the three existing same-host U0 WASM targets
through symlinks, with their original content digests verified before tests.

Final qualification LIVE: Mac14548 and Windows55353 launched at06:58 UTC
(library +31 affected targets +163 reference +472 fixtures, one Cargo job);
desktop82742 under `aeordb-v4-u1-catalog-final-20260914`, invocation
`90ba64934c57462cb8f56df8e18f4e26`, launched06:58:56 UTC. Desktop runs31affected,
full static/contracts, complete workspace with two large KV cases isolated to
the Data disk, eight release targets, reference and fixtures. Two Cargo jobs,
16GiB/no-swap build guard, Data250GB/home64GiB floors remain enforced. This is
unit qualification, not final8GiB production runtime or full v4-default proof.

Catalog qualification update, September 14 07:13 UTC: Mac completes all four
stages (681 library, 533 affected across 31 targets, 163 reference, 472 fixtures),
with all 44 input hashes unchanged and 33 same-host executable digests captured.
Linux static/contracts/debt stages pass; full workspace is still running.
Windows stops at library: 674 pass / four fail, exit 101; no later stages ran.
Three mark-control tests fail with OS 123 in private regular-file permissions;
the spill failure test observes a stale manifest timestamp. Preserve those logs.

An eight-case Windows A/B run uses the **same executable** for all four tests:
each fails under the newly lengthened `catalog-temporary` scratch (89 characters),
and each passes under the previously qualified `temporary` scratch (81 characters).
Raw Win32 path calls in private workspace permissions and durable replacement
do not handle extended-length paths. This is a distinct required hardening unit,
not a catalog identity change. Restore the prior scratch setting for catalog
qualification, write new `u1-catalog-final2-evidence` receipts, and keep both the
failed first run and A/B observations. No test is skipped and no path-limit fix
or full v4 readiness is claimed. Following-unit proof must deliberately cross
the Win32 path boundary and cover workspace publication and manifest replacement.

Windows retry completes at 07:23:37 UTC, unified56288 exits0: library678,
532 tests across31 affected targets, reference163 and fixtures472 all pass. Final C free bytes
18,944,954,368; all44source hashes and stage patches unchanged, no guard stop.
The first failure and the same-binary A/B remain distinct evidence; the path
defect below is still unresolved. Linux full workspace remains in progress.
The same-host33executable digest packet is mirrored locally (SHA-256
`d1af34aea3cb32e38c55165499bf12d4a9315068db3933f57c81429f69a25b8f`);
the library executable hash matches all eight diagnostic A/B receipts.

Linux full workspace completes at07:41:02 UTC: independently counted
**7,663 top-level passes across362targets**, zero failures, seven existing ignores,
two deliberately split large-KV cases still pending. Three nested subprocess
passes are counted separately. Full-workspace source hashes/patches remain
unchanged. Driver82742 continues into the two Data-disk KV cases, then release,
reference and fixtures; do not treat those pending stages as complete.

Both isolated KV tests pass: clamp at07:45:33 UTC (240.87 seconds), resize at
07:50:04 UTC (245.82 seconds), one pass each and62other target tests filtered.
Together with the full workspace this is **7,665 distinct passes**. The Linux
driver continues into release compilation. Its memory peak reached the configured
16GiB build/test limit with zero swap; this is not8GiB production-runtime proof.

Final Linux qualification completes at07:54:35 UTC; driver82742 exits0 with
all13stages complete. Release69 tests across8targets, reference163 tests and
472independent fixtures pass. Final Data/home free bytes are336,186,961,920 /
72,756,748,288. The memory sample after both large KV passes records448limit
events but zero OOM events/kills and zero swap. All44inputs and per-stage
before/after patches remain unchanged. No executable or database crossed hosts.

After mirroring the92Linux terminal files, strict final verification checks
native receipts/counts,41Linux/33Mac/33Windows same-host executable hashes,
all44source/lock/report hashes, original failed Windows run and8A/B observations,
and unchanged historical fixtures/error-review identities. The proof above
records7665distinct Linux workspace passes, existing ignores/nested tests
separately, both native scopes, and the still-open Windows path defect and
U1–U7 work. A catalog reader correction is not completion of semantic production
or ordinary v4 service/default integration. No production service, database,
installation or default format was changed by this unit.

## Completed U1 prerequisite: Windows native path boundary

Entry and last green:9d3d4b83c6a3a39198af435eeac29dbb91ca2a04, catalog identity
correction committed and pushed to development. All prior qualification is
complete. Owner: direct agent. Scope: shared Windows path arguments only,
unchanged persisted bytes, permission policy, no-follow checks and durability
ordering. No system settings, production databases or service changes.

- [x] Run8new internal Windows regressions against unchanged raw API behavior:
  expect long-path and embedded-NUL failures; retain the invalid-path control.
- [x] Correct the shared raw API boundary and test encoding/normalization,
  malformed/native UTF-16 paths, and no-mutation error paths.
- [x] Reproduce all4original catalog-library failures under the longer scratch,
  then qualify all affected workspace/durability paths and native regressions.
- [x] Audit exact source/evidence for the coherent green landing; catalog
  codecs/COW/compiler and remaining campaign work are still required below.

Failing-first baseline launched08:04 UTC on Windows, unified10114. An isolated
`source-u1-windows-path` checkout at9d3d4b83 imports only the two test-related
changes and frozen root lock; manifestSHA
`36baf0c85ff4c5fb44e6ae0a24cf8f138b3812f99f9efcf719075ad317422ce6`.
No production path handling has changed. Eight targeted tests deliberately use
non-verbatim paths over320UTF-16 units, plus malformed-NUL source/destination
names and invalid-path controls. Normal Cargo execution, one job,20minute
deadline and8GB free-space floor; previous qualified worktrees/evidence retained.
Results are pending, not yet an executed failing-first proof.

Executed RED completes08:06:24 UTC, unified10114 exits101: **1control passes /
7required behavior assertions fail**,678existing library cases filtered out.
Compilation succeeded; no resource/deadline termination. Failures cover all five
long-path cases and both malformed-NUL namespace-preservation cases. The three
source inputs/patches remain unchanged. Minimum/final C free bytes18,780,319,744 /
18,868,432,896. Preserve full raw receipts before changing production behavior.

Candidate1 now runs16focused tests under unified78730: eight native I/O cases
and eight encoding tests. Shared `native_windows_path` bounds UTF-16 input/output,
rejects empty/NUL input, normalizes ordinary paths without filesystem access,
adds drive/UNC prefixes, and preserves native/verbatim names. Both raw API owners
call this helper; persisted path fingerprints and barriers are unchanged.
Candidate manifestSHA
`1ca155c3182087d5e7818d7a8fe95b102d0b743930bb329ac72e2327b03d5c52`
freezes seven inputs. Full failing-first logs are mirrored; the NUL failures
showed source-name disappearance, not merely a different error classification.
No green/final qualification or path-fix completion is claimed yet.

Entry inventory finds exactly four raw path API calls in library production:
`CreateDirectoryW` and `SetFileSecurityW` in `v4/private_workspace.rs`, plus
`ReplaceFileW` and `MoveFileExW` in `native_durability.rs`. Other `encode_wide`
uses encode persisted location fingerprints, not Win32 call arguments, and must
not change. Handle-based directory barriers, identities and permission validation
remain their existing owners. Do not alter Windows system settings.

Private-path consumers cover migration root-map/capture/run-manifest/cutover
journal, GC mark workspaces, index runtime workspace storage and query spools.
Durable replacement also serves emergency spill manifest updates and two backup
publication paths. Required failure-first tests: long private directory creation,
directory/file permission setup, replace-existing and move-to-absent, cross-parent
publication, embedded NUL rejection without namespace mutation, and missing/invalid
paths. Keep no-follow checks, lexical admission and uncertain-completion ordering.

Use a shared native path conversion only at the raw API boundary. It must
normalize relative/non-verbatim paths without resolving symlinks or requiring an
existing destination; preserve already-verbatim paths and native UTF-16; support
drive and UNC prefixes. Test normalization separately from actual long-path I/O.
Do not change persistent path identity/fingerprint serialization. Re-run the four
observed library failures and all affected Windows targets under the **longer**
scratch, plus Linux/macOS regression guards, before this defect is called fixed.
This unit follows catalog landing, not concurrent edits to its frozen candidate.

Windows path qualification update (September 14, 08:37 UTC): native RED executed
eight cases: seven failed behaviorally and the invalid/missing-path control
passed. Candidate 1 passes sixteen path cases; the final candidate expands this
to eighteen and passes the Windows narrow gate at 08:32:29. The same final
ten source/lock inputs are frozen on Linux, macOS and Windows; source manifest
SHA-256 `59129f82e0fa652dbe68f16149864e9cc9bfa44eccd8495d8bce4d295cecd653`.
Linux formatting and strict workspace Clippy pass; full library and 28 affected
targets are in progress. Windows has advanced to affected targets after the
library gate (counts not yet audited). macOS formatting passes; Clippy stopped
before compilation because the toolchain component was absent. Install that
user-owned component and resume under a separately named `clippy-component`
receipt; preserve the failed original gate. No source or assertion changes are
needed for this environmental prerequisite. Full qualification remains pending.

At 08:41, Linux finishes all six planned gates successfully; full library has
681 passing tests. Windows full library exits0 at08:35:28 under the longer
scratch; affected tests continue. macOS's missing Clippy component was installed
successfully, then `clippy-component` exits101 at08:38:32 on ten lint errors:
three descending-sort closures, five EXIF match guards, one spill-path match
guard, and the macOS identity helper's redundant `st_dev() as u64` cast.
All ten source expressions predate this unit; the path candidate adds none.
Keep both failed macOS receipts. A behavior-preserving native lint prerequisite
is required before landing, without allowances or changing toolchain versions.
Do not mutate the frozen ten inputs while Windows qualification is running.
Preserve completed first-candidate evidence; requalify changed final source.
Add directory/index/rule/parser regression targets for any corresponding edits;
existing platform, spill, format and library suites cover the other owners.

Windows affected gate exits101 at08:44:46: only
`backup_task_spec::test_backup_task_unwritable_dir_fails` fails. Its old fixture
uses `/proc/fake/deeply/nested/backup`, which is a valid writable Windows path;
the task correctly completes. Replace this nonportable fixture with a regular
file blocking a child destination inside its own TempDir, and prove the sentinel
survives unchanged. Do not disable the negative test or change backup behavior.
The accidental 70,528-byte test backup was moved recoverably from `C:\proc` to
the Windows campaign's `retained-unintended-backup/`; no ancestor directories
or unrelated data were removed. All original logs and the 696-pass Windows
library are retained. Separate non-strict native Clippy runs now inventory all
diagnostics; they are explicitly **not** qualification passes or warning waivers.

macOS diagnostic inventory closes0 at08:45:53: fourteen unique warnings, ten
production expressions plus two test `while let` opportunities, one explicit
test counter and one test sorting closure. Local corrections preserve stable
ordering, scalar types, match fallback behavior and test iteration bounds.
The source inventory grows by nine files (five production owners, four specs);
add `directory_ops_spec`, `index_store_spec`, `rules_spec`, `native_parsers_spec`,
`index_v1_cow_spec` and `index_producer_coordinator_spec` to the affected gate
(34 total). The original 28 already include migration-base clone and backup
tasks. All1,503 reviewed error entries remain identical except eight line
positions: four native-durability offsets and four EXIF offsets. IDs, patterns,
columns, review decisions and ceilings are unchanged.

Windows diagnostic initially stops before execution because Clippy is missing;
component installation succeeds and `clippy-inventory-component` now runs on
the preserved old ten-input remote candidate. Local corrections are not synced
into that running worktree. Original first-candidate archives/logs/digests remain
immutable evidence; the next qualification uses a new source manifest and new
`final2` receipt paths. No lint-inventory success will be labeled a strict pass.

Windows full inventory closes0 at08:59:30 (15 unique warnings). Its additional
findings are Windows-only expression simplifications in durability error
classification, spill native-byte length validation, memory observation, journal
lock classification, private-path separator parsing, build arguments and a
Unix-only test variable. These are corrected without warning allowances,
changing toolchain versions or disabling tests. The build argument borrow removal
changes one scanner fingerprint (`cc4cb823618f6ea36076` →
`0c86e9f2efd70ea3d1f4`) and shifts two columns. The expression still panics on
process-launch failure; classify its refreshed entry as `fatal-build-boundary`,
not the old inaccurate optional-data label. No default value or extra suppression
was added. Local scanner check passes all1,503 occurrences; fresh remote
architecture checks remain required. Persisted path encoding is unchanged.

Final2 launches09:05 UTC: Linux29333, Mac24089, Windows12478. Its25 exact
source/lock inputs have manifestSHA
`65d1722a34ecb2c36528a120edb6d69a16f322dc3a903b7f83cb8615033bf4fe`;
source-only archiveSHA
`4fa84a65eb0734a80f1b674d668019053eba99924d571db2634d7fd5318b53d7`.
All original first-candidate evidence is retained. Expand to36 affected targets
with RSS sampling and portal packaging checks; full library, strict workspace
Clippy and formatting run on native Unix, plus Linux contract/debt gates.
Windows runs18narrow cases, strict workspace Clippy, full library,36affected
targets and18release path cases. Same disk floors and one Cargo per host;
Linux buildcap16GiB/noSwap is not the final actual-v4 runtime8GiB gate.
Do not edit frozen source or call this unit complete before terminal evidence.

Final2 macOS completes09:12:12: formatting, strict workspace Clippy,
681library tests and983top-level integration tests across36targets all pass.
The three named index-store child-process reruns pass separately; do not add
them to the distinct count or treat their42filtered parent cases as skips.
All25inputs remain unchanged; minimum/final resource samples stay above the
30GB floor. Same-host37binary digests and raw evidence are mirrored locally.
Windows strict Clippy and696library cases pass by09:14:23; its36affected
targets andrelease cases remain in progress. Linux has reached the end of
affected tests; terminal contract/debt receipts remain outstanding. These
intermediate successes do not authorize landing or full-v4 readiness claims.

Final2 Linux completes09:16:51: formatting, strict workspace Clippy,
681library and984top-level integration tests across36targets, all472 reference
fixtures and debt self-tests pass. Three index-store child reruns are recorded
separately. Runtime11m33, CPU9m36, peak5,312,196,608bytes/noSwap;
final Data336,186,953,728 and home71,912,964,096bytes free. All raw logs,
driver receipt and37 same-host binary digests are mirrored. Windows affected
36targets complete0 at09:25:20; aggregate count awaits the final audit.
Its release gate started09:25:21 and is compiling at09:32. No source changes,
staging or landing while this last qualification gate remains outstanding.

Completed Windows log audit confirms968top-level passes across36targets, plus
three named index-store child reruns. Those children each filter41 tests, not
Unix's42: the existing disk-write-failure test is Unix-only, making the parent
target42 tests on Windows and43 on Unix. Corrected the evidence accounting to
require platform-exact child counts and names; no tests changed or skipped.
The first accounting assertion was a checker failure, not a failed native test.
The28 completed Windows log/receipt/manifest/patch files are mirrored; live
release output remains outside the final evidence packet until completion.

Final2 Windows release completes0 at09:45:40: all18 path regressions pass,
678 unrelated library cases filtered by the named narrow selector. Compilation
and execution took20m19; minimum sampled Cfree16,839,917,568bytes and final
16,836,980,736bytes remain above8GB. All25 source/lock inputs are unchanged.
All35 Windows evidence files and38 same-host executed binary digests are now
mirrored; no executable or database crossed hosts. The strict complete proof
check passes, including original7-failure RED, 16-case candidate, native
prerequisite failures, portable backup correction and unchanged audit ceiling.
Linux/macOS each contribute37 binary identities. Upstream development matches
the9d3 entry snapshot before landing. The path correction changes neither
persistent path bytes nor format/default/production-service behavior.

- [x] Reproduce the native long-path and NUL mutation failures before fixing.
- [x] Share bounded native path encoding across all four affected Win32 APIs.
- [x] Prove original/adjacent failures, native full libraries,36 affected
  targets, strict workspace linting, contract/debt gates and Windows release.
- [x] Preserve original failure evidence and audit exact final source/binaries.
- [ ] Continue U1 catalog codecs/COW/compiler and U2–U7 integration/qualification.

## Completed U1 prerequisite: independent catalog owner-key validation

Owner: direct execution. Last green3354ee91; upstream is synchronized. Owned:
reference `core.rs`, dedicated `spec/catalog_owner_contract_spec.rs`, this
ledger and narrow evidence. No production AeorDB codec, fixture bytes,
manifest/divergence entries, database or service changes. Round10 already
requires canonical control-owner paths and exact class3–5 hash widths;
Round16 retains exact dependency IDs for6/7. This is oracle completeness,
not a new persistent contract or permission to normalize malformed keys.

Production `namespace.rs::validate_catalog_owner_key` already enforces these
bounds. The independent checker only enforced6/7. Before any reader correction,
ran nine independently assembled byte tests in a separate9d3 checkout; the
current Windows candidate and main source stayed untouched. RED completes
09:41:32, exit101:4pass/5fail,163existing cases filtered, no ignored tests,
compile failures or resource termination. Failures are oversized control paths,
invalid UTF-8, absent kind/path, noncanonical relative paths and empty class3
owner keys. Raw evidence and test-binary identity are preserved in the campaign
cache's `u1-catalog-owner-red-linux-*` files. Four frozen input hashes have
manifestSHA `4be5602f2c759aab24047f69e7a0889b04955d0b0cba2098f5593bc68faca9dc`.

Correction checks control-kind/path byte bounds before path interpretation,
requires exact selected hash width for every class3–7 key, retains dependency
owner/semantic-ID agreement and lookup/order checks, and borrows previous keys
instead of allocating copies. Valid root, Unicode, internal-space and literal
backslash paths remain valid under the existing logical-path grammar. The
checker remains independent from AeorDB production code.

- [x] Reproduce the five missing owner-key checks with independent bytes.
- [x] Pass all nine new cases, all reference tests, strict reference Clippy,
  formatting, all472 existing independent fixtures and contract/debt gates.
- [x] Preserve production catalog/format regressions and audit unchanged
  fixtures, exact source/lock and executable evidence before the green landing.
- [ ] Continue all-class definition-object/catalog writers, bounded COW/compiler
  and the remaining U1–U7 runtime/qualification obligations.

The next catalog-writer entry must also examine reference envelope caps:
`decode_semantic_object` currently lacks the production1MiB hard cap. This
separate envelope concern is not silently treated as covered by owner-key tests.

Final source4-input manifestSHA
`8fa42fc3de81591c822654f504665d280c6edf31ec2687dd3e8d760a58080dff`,
source-only archiveSHA
`16fe5925905caf8fd2791c3ad3ff601ebae984663fffec0f628ea3887d39d107`.
The exact RED test bytes are unchanged in the final candidate. Desktop final
qualification completes10:00:40:9new cases, all172 reference tests, formatting,
strict reference Clippy,472 fixtures,16 production dependency-catalog and77
production format tests, contract evidence and normal debt policy all pass.
The93 production tests reuse exact same-host binaries from3354 qualification
after checking their hashes and all25 original source inputs; no production
source changed and no binary crossed hosts. This is not a new native release
or full-workspace run.

The final driver's `debt-selftest` label was inaccurate: its checker ignores
`--self-test` and only performs the normal policy scan. Preserve that receipt as
the policy-check result, never self-test proof. Run the real
`scripts/spec/check-v4-debt-spec.sh` under a separately named
`debt-selftest-corrected` receipt; it passes10:02:38. The strict audit requires
both the original actual command/output and this separate self-test. No source
or assertion changed for the harness correction.

All61 stage/driver evidence files plus both same-host reference binary digests
are mirrored. Strict complete proof passes10:08, preserving the4pass/5fail RED
and its binary identity. Final free bytes: Data336,155,258,880 and
home70,572,879,872. The user unit enforced4GiB/noSwap; its reported few-MiB peak
is not credible compiler RSS and is not used as actual-v4 resource proof.
Full U1 and U2–U7 remain required; no production service/default changed.

## Active U1 prerequisite: independent semantic envelope cap

Entry/last green7ee309c4; direct owner. Owned reference `core.rs`, dedicated
`spec/semantic_envelope_cap_spec.rs`, ledger and proof. Production namespace
decoding already rejects complete semantic objects larger than1,048,576bytes
before CRC/identity work. The independent reader omits that admission check,
and its final identity construction allocates a preimage from the whole input.
Round10 requires kind caps before allocation; this is an existing contract.

- [x] Execute independent RED: exact1MiB structural envelope stays accepted;
  larger correctly framed definitions and oversized malformed input must fail
  at size admission. These opaque payloads characterize only the structural
  envelope boundary, not valid compiled semantic projections.
- [x] Add the minimal pre-parse cap, pass all175 reference tests and472 frozen
  fixtures, strict reference lint/format and actual contract/debt gates.
- [x] Record unchanged fixtures, exact source/binaries, RED and final receipts;
  land the green prerequisite before catalog encoding/COW/compiler integration.

Three new tests are installed before any reader behavior change. Frozen RED
4-input manifestSHA
`c39bc39636fd9ba379b0992ab7f69cd073ef47011820e5a12a7b6886edfbf01e`;
archiveSHA `3531a5dfe2b456aaf9ec3232d34ac0ae7a3df994c8a09453fe5a2f6f7ada61d0`.
Use a new detached `source-u1-semantic-codecs` checkout at7ee; all prior source,
failure evidence and executed-binary identity records remain retained. Reusable
reference build-cache outputs may be rebuilt; pinned releases are untouched. No production
codec, fixture, default or database change is included in this prerequisite.

RED completed 2026-09-14T10:13:50Z: 1 passed, 2 behavioral failures,
172 filtered, no ignored tests or resource stop. The oversized valid envelope
was accepted, and oversized malformed magic reached framing validation instead
of size admission. The unchanged three tests now accompany a pre-parse 1MiB
cap; final qualification is next. RED logs, input manifest, archive, and
same-host binary identity are preserved under the campaign cache.

Final qualification completed2026-09-14T10:24:11Z: 3 narrow and175 full
reference tests, format, strict Clippy,472 fixtures, contracts and actual debt
self-test pass. The first SSH observer was mistakenly limited to30seconds;
its driver failed while Clippy's terminal wrapper was reporting/checking
source. Compilation itself succeeded, but that receipt is not a passing gate.
All four source hashes and before/after patches were verified unchanged;
Clippy and all remaining stages passed a separate correctly bounded driver.
Both failures and successes are retained in the strict proof. No test was
edited after RED and no frozen fixture or AeorDB production source changed.
Previous93 production regressions remain separate predecessor evidence.
Final manifestSHA39b6c13a102766bccaa42efe287ec45f283b09363323bffc48b05d665898873e;
archiveSHA5ded9f358d90c8ca7105879b929cd3c00be65426c27b60d87a5211caaad2935e.
Final free bytes Data336155209728/home70515556352; no resource stop.
All stage receipts and both same-host binary identities are mirrored locally.

## U2 read-only entry refinement: public append-writer guard

Audited against `369cb199` plus the unchanged 43-input U1 candidate while Linux
qualification runs. This is a bounded caller inventory, **not** completion of
U2's full backend/producer inventory and not permission to bypass U1 gates.
No runtime source changes accompany it.

Repository-wide Rust search for `writer_read_lock` finds **12 production call
sites**, its public definition, and one non-call entry in
`root_operation.rs::STORAGE_ENGINE_INTERNAL`. The test search finds **nine
actual calls** plus one architecture assertion containing the method spelling.
No `writer_write_lock` caller or public counterpart was found.

| Production caller at this revision | Exact dependency | Integration obligation |
| --- | --- | --- |
| `configuration_history.rs:53` | Captured v3 KV end/WAL end and file path; backward `EntryHeader` scan | Preserve bounded history/LKG recovery; changing only the lock return type cannot make the scanner understand v4 framing |
| `verify.rs:411` | Selected header sequence for durability verification | Capture format-correct selected authority and proven hard frontier |
| `verify.rs:484` | Preverified repair token: path, file metadata, header sequence, hot-tail offset | Preserve exact engine/frontier binding and reject intervening mutation; do not synthesize v3 hot-tail authority for v4 |
| `verify.rs:690` | Reporting current-WAL scanner with cancellation | Dispatch a bounded format-correct scanner; retain diagnostics and repair cancellation |
| `verify.rs:779` | Physical KV-end/WAL-end range | Preserve locator-range validation using actual selected layout |
| `verify.rs:906` | Live WAL end while checking separately decoded v3 hot-tail Voids | Keep v3 hot-tail logic in its versioned owner; v4 reclaim remains receipt/catalog authority |
| `durability_recovery.rs:92,100` | Before/after selected header sequence around explicit repair flush | Preserve stale-proof rejection and receipt ordering through one format-correct coordinator |
| `durability_recovery.rs:291` | Fallback selected sequence when seeding a persistent spill incident | Preserve catalog-before-latch publication and restart read-only state |
| CLI `probe.rs:426` | Reporting WAL scanner for path history | Preserve diagnostic partial/error reporting and bounded framing interpretation |
| CLI `probe.rs:885` | Current writer offset for growth statistics | Expose real physical frontier, not a guessed header projection |
| CLI `verify.rs:534` | Current KV byte length before offline expansion | Preserve capacity decision, clean close/reopen and preverified-report reuse |

The method itself currently returns `RwLockReadGuard<AppendWriter>` at
`storage_engine.rs:5222`; the engine owns concrete `RwLock<AppendWriter>` and
`Mutex<DiskKVStore>` fields at lines 863–864. U2 must replace that concrete
backend dependency without creating a second mutable file/KV/header owner.
Its public-method classification and callers must move together.

Named existing regression inputs for this boundary include
`configuration_history_internal_spec` (byte/candidate ceilings),
`config_resolver_spec` (invalid current configuration versus LKG/history),
`v3_transition_control_store_spec` (catalog-first crash, explicit repair proof,
intervening mutation, dropped repair and spill identity),
`verify_progress_internal_spec` (phase/cache progress and interrupted repair),
CLI `verify_progress_cli_spec` (read-only bytes/mtime, missing/malformed/locked
source, repair and logging admission), `probe_spec` (growth/checkpoint/tail
diagnostics), and `embedded_root_operation_spec` (method classification).
Nine direct test calls additionally inspect hot-tail offsets, KV stage/layout,
or writer offsets in resilience, KV, route-error, bulk-write and directory
specs. These are regression inputs to preserve and extend to genuine v4,
not already-executed v4 runtime proof.

### U2/U6 creation and open-admission refinement

The same source audit identifies six non-test production creation call sites
outside `StorageEngine` itself: CLI bootstrap `commands/start.rs:1006`, normal
server construction `server/mod.rs:874`, separate identity-file construction
`auth/provider.rs:178`, and three backup creators `backup.rs:218,343,1543`
(version export, full export, patch artifact). The three extra CLI-bin creators
belong to crash-soak, soak and GC-benchmark harnesses; inline `#[cfg(test)]`
calls and the crate documentation example are not additional production paths.
The factory search also checked renamed imports/type aliases; pointer casts and
server tuple aliases are not constructors.

All four create entry methods currently converge on `create_internal`:
`create`, `create_with_hot_dir`, `create_with_hot_dir_and_configuration_overrides`,
and crate-private `create_with_memory_coordinator`. The last one inherits the
source's memory owner for backup work. CLI bootstrap and server factories each
choose existing open versus new creation; identity-file bootstrap does so
independently. A normal-service-only format switch would miss backup and
identity creation.

Seven open entry methods converge on `open_internal`. Five public entry points
cover ordinary open, legacy hot-dir compatibility, progress, configuration
overrides, and import. Two crate-private paths add inherited-memory import and
offline migration inspection. Ordinary opens reject patch artifacts before
arming implicit shutdown; import deliberately accepts them. Offline inspection
passes `allow_mutating_recovery = false` and shares the strict non-mutating source path
used by public verify, migration preflight and migration execution. U2 must
preserve those distinctions, configuration validation before mutation, file
locking, inherited budgets and failed-constructor non-writing Drop behavior.
Migrating normal service does not authorize automatic in-place source upgrade,
nor conversion of a retained v3 migration source into a writable v4 engine.

Required later behavioral proof therefore includes ordinary CLI/server and
embedded creation, separate identity bootstrap, all three backup forms and
their import/standalone restrictions, strict read-only v3/v4 inspection, and
existing-source open without automatic migration. These are refreshed entry
requirements; the current U1 qualification does not prove their v4 behavior.
