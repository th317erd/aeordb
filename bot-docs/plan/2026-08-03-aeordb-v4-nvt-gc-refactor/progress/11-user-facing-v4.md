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

## Current landing unit: U0 entry proof and empty semantic state

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

Next action: land the verified U0 correction, then begin U1 with the canonical
scope writer's failing-first target. The dependency-key clarification is
pending owner review and does not block that independent writer. The original
normal-creation target remains red until actual runtime/default integration.

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
