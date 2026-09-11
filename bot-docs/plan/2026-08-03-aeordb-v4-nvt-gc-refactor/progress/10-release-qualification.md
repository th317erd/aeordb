# Current release qualification — 2026-09-10

## Authorization and entry gate

Owner: Codex, direct implementation under the `implement` workflow.
Original release entry: `3b48de7e42c2d2b6db895774eb8b046eff6d6fa7` on
`development`, matching origin after fetch. Production code is unchanged from
`0b20792b`; the successor fixes a native Windows test fixture and records proof.

Current follow-up (September 11): the checkpoint restart correction is qualified
against engine baseline `48baeefe0144e2a84458c6589aa0345afc223ff8`, documentation
entry `11fca469d59ff63e688bbeb65364dbf637adaac6`. Final-source full Linux, affected
native and static checks pass. The committed exact a8047327 native releases,
100 complete crash suites and all short soaks now pass; its first 12-hour stage
is active. At 48baeefe, media/live/resource/release-CLI
and 100 complete crash suites passed, but short S3 stopped on a malformed
checkpoint after cycle 8; no 12-hour stage started. Earlier build/qualification
passes below remain explicitly attributed predecessor evidence.

The owner explicitly authorized the next three items, then required a stop and
discussion before item 4. This ledger supplements Children 07/08 and the frozen
parent; it does not rewrite historical results or authorize production work.

- [x] 1. Renew capacity admission and exact native Linux/macOS/Windows release
  builds for checkpoint-corrected source a8047327.
- [ ] 2. Current-candidate disposable media migration, live HTTP/reopen/readback,
  bounded resource overlap, restart/crash matrix, and S1/S2/S3 12-hour stages.
- [ ] 3. Reconcile and seal the completion/DoD packet with current evidence,
  historical qualification clearly labeled, remaining operational gates explicit.
- [ ] 3a. Owner-requested cleanup after qualification: inventory the test databases
  under desktop `/media/Data/AeorDB/`, confirm exact disposable targets and no
  active openers, then remove unneeded test database files. Retain logs/source
  and still-needed failure evidence; record what was removed and bytes reclaimed.
- [ ] 4. **STOP AND DISCUSS WITH OWNER.** No production deployment, service change,
  canary, installation/public downloads update, cutover, first-write acceptance,
  destructive operational GC, or deletion/reuse of the retained corrupt database.

The owner additionally requested cleanup of test database files under
`/media/Data/AeorDB/` **when this work is done**, because the drive is nearly full.
This authorizes scoped disposable-test-data removal after qualification, not
deletion of unrelated databases, useful corruption evidence, or the retained
`FS-Server1` production-derived file. Resolve exact targets before deletion and
report whether they can be regenerated or recovered.

## Inputs and invariants

- Frozen, untracked Cargo.lock SHA-256:
  `06e6c7a8eb6dbccf52a0b97a4ee6edeece7297866305b0930314fd47b987faec`.
- Preserve the dirty original remote clones and all unrelated local untracked
  files, including user draft reports. Detached exact-commit sources only.
- Heavy Linux work stays on `wyatt-desktop:/media/Data/AeorDB/Tests/`.
  Never transfer a Cargo target tree between hosts; source rsync uses `--no-times`.
- Linux Cargo jobs: 2; native macOS/Windows: 1. Preserve the actual release
  profile (`debug=1`, `strip="none"`). Serialize heavy Linux workloads.
- Data free-space floor: 250,000,000,000 bytes. Private scratch/workspaces on
  desktop `/home/wyatt/.cache/codex/`, because Data exposes mode 0777.
- All long commands need host-side deadlines, disk monitoring, logs, and exit
  receipts. Long-soak model checks should be sparse (about 10 minutes), not busy
  polling. Unexpected failures retain evidence and block later qualification.
- Representative v3 input is a **new separately checksummed copy** of the sealed
  clean media rehearsal fixture, not the retained production-derived database.
  Its expected SHA-256 is
  `2be26ba43beb289bee0573355936b0ccd303540e39b13f815f1fedeeb504db74`.
- Existing current proof: 7,495 top-level full Linux tests at 0b20792b; native
  macOS 1,181 affected tests; native Windows 1,172 affected/CLI tests at corrected
  3b48de7e content; strict Clippy/contracts/debt gates. See ledger 09 for commands,
  receipts, limitations, nested child results, and retained failed attempts.
- Historical release/soak evidence at 535004f1 is useful harness/reference input,
  **not** current-candidate release proof. Production-scale repair was retired;
  this qualification must not imply that its migration succeeded.

## Resource admission and cache cleanup

Initial free bytes: desktop Data 256,884,199,424; desktop home 139,023,126,528;
Windows C: 22,365,851,648. macOS free space approximately 98.9 GB.
Desktop memory pressure warrants the conservative two-job cap.

Read-only inventory found an inactive old debug build cache at
`/media/Data/AeorDB/Tests/p9-exact-535004f1/target/debug` occupying
107,519,559,680 bytes. Its separate release directory is only 3,671,009,280 bytes
and must be retained with the old binaries and evidence. No matching Cargo,
Rust compiler, or test worker was running during inspection. The owner's standing
Cargo-clean authorization permits cleaning precisely that obsolete dev-profile
cache; no database, source tree, evidence log, or release artifact is a target.

Cleanup completed: `cargo clean --profile dev --target-dir
/media/Data/AeorDB/Tests/p9-exact-535004f1/target` removed 11,812 build-cache files
(102.0 GiB). Data free rose to 364,342,710,272 bytes. The old release executable
still exists; databases, source trees, old evidence and other caches were retained.
Clean log: new desktop campaign `evidence/obsolete-debug-clean.log`.

## Harness correction: stop on a failed soak cycle

The preflight found that S2 verification failures and S3 verification/checkpoint
failures incremented a counter but continued spawning workers on the suspect
database. Early worker exits already stopped correctly. This conflicts with the
standing requirement to preserve corruption evidence before further mutation.

New executable shell regressions reproduce four failures against the original
script: S2 corrupt/malformed verification started four workers each; S3 corrupt
verification/checkpoint loss started three each. The correction stops immediately
after any failed cycle, retaining the database, logs and S3 diagnostic copies.
Successful cycles continue normally; failure still returns nonzero.

- Target: `timeout 100s bash scripts/spec/soak-cycle-spec.sh`, now including the
  eight-case `soak-failure-spec.sh` fixture harness.
- Eight cases: S2 corrupt verify, malformed verify, early exit, successful cycles;
  S3 corrupt verify, checkpoint loss, early exit, successful cycles.
- Linux and native macOS Bash 3.2: combined old helper/new eight-case suite passes.
- Red-result transcript: `soak-failure-red.log`, SHA-256
  `50836dc0c45d4b0ddb4f89b0f9bcf7915ae3cce45dd2a68d232c370d62b64351`.
- Linux/macOS green outputs both have SHA-256
  `e277cdd314258835ca91638769e697c8b36f53e33bb99b8bd7825ba66456e766`.
- Logs/runners retained locally under
  `~/.cache/codex/aeordb-release-qualification-20260910/`; macOS test harness under
  `/Users/wyatt/.cache/codex/p9-release-harness-20260910/`.
- Shell syntax, `cargo fmt --all -- --check`, debt gate (8 entries/164 matches),
  and `git diff --check` pass. No Rust, embedded documentation or Cargo input was
  changed; existing Rust proof remains applicable. This is a harness-only fix,
  not a claim of a new full Rust-suite run.

Release binaries remain built from the exact 3b48de7e source. Long qualification
uses the corrected shell harness with only its repository-root path relocated to
that clean source; record both harness and executable hashes separately. Never
attribute the original continuation-on-failure behavior to the corrected harness.

### Adjacent S1 false-success correction

Extending the same fixture perimeter to S1 reproduced another inherited runner
defect: a worker exiting 7 was followed by `S1 complete` and runner exit 0. S1 now
captures and returns the worker's exact nonzero status after retiring its own
memory sampler, preserving database/log evidence. A normally completed worker
still returns zero. No workload or engine behavior changed.

The combined suite now contains ten scenario cases plus the existing helper
checks, and passes on Linux and native macOS Bash 3.2. Failing-first output
`soak-s1-red.log` has SHA-256
`0574f6536450fb34c8c243627b98f0f6ec8ffe42da0a2bc44517908e05c4eb39`;
both final green logs have SHA-256
`f2cfbaa9cf692e9541af1f6e430ee348acb769f0353b9a724aa1e9bf5a6d0373`.
Syntax and diff checks pass. Existing Rust qualification remains applicable:
Rust/Cargo/embedded-documentation inputs are unchanged from 3b48de7e.

## Current release results and locations

Desktop campaign: `/media/Data/AeorDB/Tests/p9-release-3b48de7e-20260910/`.
Native campaign: `~/.cache/codex/aeordb-tests/p9-release-3b48de7e-20260910/`.
Each contains `source/`, native-only `target/`, `evidence/`, and its runner.
The shared 53-file portal-input archive has SHA-256
`2c91a4dc3a6f3de6f4d03472886ff2b699d2dfdbee4d48ce04b58d36a51cab7b`.

- Linux release build passed in 8m05s (Rust/Cargo 1.94.0, two jobs), exit receipt
  at `2026-09-10T16:30:18Z`. Actual release profile retains debug information.
  CLI SHA-256 `1199bd1a4f1881b189af345a912b472f954bc32daf7412faaa013cc28b85f140`;
  soak worker `c9a780dba944c3e3b557e3b85442967c4883f8f52fc45278f12326139db6eaee`;
  crash worker `3e88b00b0f7b0e7075f90351fa3e5e48875b7910581ff0bc8baa30fb296df5a0`.
- Native macOS release build passed in 8m01s (Rust 1.95.0, one job), exit receipt
  at `2026-09-10T16:31:58Z`; macOS 26.6.2/25G83, Mach-O arm64, version 0.9.5.
  Binary digest is recorded in native `evidence/macos-release-binary.sha256`.
- Windows release build passed in 26m27s with native MSVC Rust 1.96.0, one job,
  at 16:47:39 UTC. SHA-256:
  `7ddb1b583e1dc24f54f77c087e6529c4c387a595c2366b6ac772fac06c5bbb87`.
  Minimum sampled C: free bytes 21,077,352,448; final 20,933,599,232, above the
  8 GB floor. Native receipt: `evidence/release-build.result.json`.
- Linux current-release live gate passed at `2026-09-10T16:32:17Z`: real docs
  routes, JSON/binary byte-exact readback, clean shutdown/reopen, deletion/missing
  cases, and offline `Status: OK`. Both disposable services exited cleanly.
- Unix guard preflights preserve success (0), command failure (1), and deadline
  (124) distinctly; host-side capacity samples and terminal receipts are present.

## Current action

The current-release 120-second 8 GiB/no-swap overlap gate passed at 16:35:58 UTC:
2,002,620,416-byte memory peak, zero swap, three completed KV expansions, no
functional failures. Health p50/p95/p99/max: 0.595/14.440/101.058/678.747 ms.
The test overlapped 1,151 writes, 2,928 reads, 894 blob commits, 310 searches,
44 reindex tasks, 45 dry-run GC operations and 60 cancellation probes.

The desktop's `run-linux-sequence.sh` stopped at 16:45:25 UTC, exit 1, during
media preparation. `sequence-launch.log`, `evidence/linux-sequence.tsv` and
`evidence/linux-sequence.exit` are authoritative. No migration, release CLI suite,
crash100 or soak stage started. Later gates remain pending, not running.
No force-unmount test is enabled (`AEORDB_CRASH_SOAK_TMPFS` is explicitly unset).
Cycle scheduling is seeded; the inherited worker remains wall-clock-seeded,
with actual checkpoint traces retained. Do not claim fully deterministic soaks.

Final packet reconciliation remains open. No production operation or current
long-soak success has been claimed.

## Qualification defect: plain verification mutates its source

The fresh 11,654,356,141-byte media copy matched the sealed original before
verification (`2be26ba43beb289bee0573355936b0ccd303540e39b13f815f1fedeeb504db74`).
Candidate `verify` exited 0/Status OK but changed its checksum to
`4e4954e398d9807b564d0df5c44eb53bb8dc43140f53d486d2c7e9b4f7accbfb`.
The copy and logs remain untouched at the release campaign `media/` and
`evidence/media/`. The sealed source was never opened by the candidate; its
recorded stat is unchanged. A bounded header comparison shows publication, not
proof of payload loss. Full-file byte-difference classification was not run.

The CLI used writable `StorageEngine::open` for both verification and repair;
normal open can perform recovery and normal close publishes durable headers.
Migration preflight already owns an OS-read-only, non-publishing open. The
correction reuses that path behind a report-only public verification facade;
the engine remains crate-private and cannot escape to callers. Explicit repair
keeps its existing workflow. The architecture inventory must retain that boundary.

Failing-first proof at 01172ee7 plus four new CLI tests: 4 passed, 3 failed.
Failures reproduce clean-file byte mutation, refusal of a read-only clean file,
and silent recovery of a truncated source. Stale-locator evidence preservation
already passes. Red logs/receipt: desktop
`/media/Data/AeorDB/Tests/p9-verify-read-only-20260910/evidence/verify-read-only-red.*`.

Next: qualify the narrow correction and adjacent malformed/lock/recovery paths;
make soak startup recovery explicit on diagnostic copies before read-only verify;
run affected/full/native proof; commit and rebuild the corrected candidate;
restart media qualification on a new copy, preserving this failed artifact.
All 3b48de7e native binaries/live/resource results above are baseline evidence,
not proof of the pending Rust correction. Items 1–3 remain open; item 4 is gated.

### Read-only correction: narrow and native progress

The initial 7-case CLI red target now passes all seven; the expanded target
passes 11/11 on Linux. Additional cases cover missing/malformed sources,
exclusive-lock refusal, and explicit normal startup recovery followed by a
byte-preserving strict check. The library recovery-control regression verifies
both active (refused) and completed (inspectable) persistent states without
republishing their bytes. Explicit repair/payload-readback remains green.

The reviewed suppression gate found only six stale source line numbers. Those
six locations were manually corrected, with unchanged occurrence identities,
patterns, review policies and the 1,503-entry ceiling. Its failed receipt is
retained as `verify-read-only-affected`; the corrected Linux seven-target run
passed at 17:16:56 UTC (`verify-read-only-affected-final`).

Native macOS passed 221 CLI tests (7 existing intentional ignores) and 118
affected library tests across 28 targets total, ending 17:17:24 UTC. Native
Windows CLI qualification is running with an 8 GB floor/one-hour deadline;
its affected library stage remains pending. Native proof lives under
`~/.cache/codex/aeordb-tests/p9-verify-read-only-20260910/evidence/`.

Soaks now explicitly exercise normal startup recovery with `probe --growth-stats`
on diagnostic copies, then run read-only `verify`. S2 now preserves its original
crash image just like S3. Copy/reopen failures prevent further verification or
workers; failed copies/logs are retained. No `--repair` was added to the soak.
The expanded 16-scenario harness passes on Linux and native macOS Bash 3.2,
covering worker exits, four copy failures, recovery-open failures, malformed and
bad verification, checkpoint loss, successful continuation, and original-byte
preservation. Final output SHA-256 on both hosts:
`5da9ac79caedeab49e6934c55b25eb8d10b1b87a5b837882414c2cc19fb084f4`.
The expanded red harness reproduces failed recovery ordering and failed success
cycles before the runner correction (`soak-read-only-red-expanded.log`).

The 11-file corrected source/spec/doc/harness manifest has SHA-256
`29dc41e1479c7c5ee89bd70f54cabb5948d4459df7b6cd42f97e009afe4679ae`
and matches laptop, desktop and macOS. Native Windows receives the seven-file
Rust/spec/doc patch, SHA-256
`9d3b6171b1496b747483fad1a8c8a16cd60248bde104305cdb9c3159c4d7dc7a`;
the Bash harness is not executed as a native Windows gate.

Linux `run-verify-followup-linux.sh` (initial PID 3403036) passed the guest-build
prerequisite at 17:19:31 UTC and started the full workspace/all-target suite.
It serializes full tests, strict Clippy, contract/debt checks and the final shell
harness, stopping on failure. Per-stage deadlines, disk guard logs and receipts
are under the diagnostic campaign `evidence/`; `followup.exit` records terminal
status. No release/media/crash/long-soak rerun starts before the fix is green.

The next media runner will require strict exit 0 for the sealed clean fixture
and capture its post-check checksum even if verification fails. The baseline
runner's unused acceptance of exit 1 was incorrect and is removed; preflight
still independently enforces source admissibility.

### Native lock-contention diagnostic correction

Windows CLI completed successfully at 17:20:33 UTC: 218 passed, 7 existing
intentional ignores, 21 targets. Its subsequent affected matrix reproduced an
inherited failure in `file_lock_spec`: the second open was correctly refused,
but Windows error 33 was not recognized as lock contention because the engine
tested only `ErrorKind::WouldBlock`. The native receipt is `affected.result.json`
(exit 101, 17:25:35 UTC). No lock exclusion or source-preservation failure occurred.

The one-line correction compares the raw error to the frozen fs2 dependency's
`lock_contended_error()` (Windows ERROR_LOCK_VIOLATION, Unix EWOULDBLOCK). The
original native failing assertion remains intact; CLI lock-refusal coverage now
requires the diagnostic, and a new negative test ensures failure to create a
lock file is not mislabeled as contention and cannot create a database.
No locking primitive, recovery authority or persistent format changed.

The in-progress Linux broad run was deliberately superseded, not counted as
passing: `workspace-final.exit` records 124/driver_signal and `followup.exit`
records 143 at 17:34:03 UTC. Its supervisor/guard exited. The corrected queue is
`run-verify-final-linux.sh`, initial PID 3431659; terminal receipt is
`evidence/followup-lock-final.exit`, and every gate uses the `*-lock-final` suffix.
It runs CLI/affected checks before a fresh full workspace/static/contracts/docs
matrix. Earlier logs and receipts are preserved. Native Mac/Windows likewise
rerun affected and CLI checks on the final correction with fresh receipt names.

The final 13-file source/spec/doc/harness manifest SHA-256 is
`55c351fbb64c95bb46c0b27763d4caa707c7f6fdcb2f42742d85ce5842d6d438`;
the nine-file native Rust/spec/doc subset is
`8bf1d888a4cfb95cfa6d52ebe79c1009b66af99395783f72fc41f61e14114648`.
These supersede the earlier 11-/7-file input sets without rewriting their proof.

The first lock-corrected affected runs also exposed an architecture assertion
that literally required the old Unix-only `WouldBlock` classifier. It now requires
the fs2 platform-specific comparison while retaining the guards for error-kind
and OS-message preservation. The product correction is unchanged. These failed
`affected-lock-final` attempts remain retained; they stopped before full-suite
execution, and the final Mac CLI launch was not admitted behind that failed gate.

Current queue (superseding the preceding paragraph's runner):
`run-verify-reviewed-linux.sh`, initial PID 3439813, with `*-lock-reviewed` stages
and `evidence/followup-lock-reviewed.exit`. Native affected reruns likewise use
`affected-lock-reviewed`; final native CLI still requires a passing affected gate.
Final source manifest now includes this architecture guard: 14 files, SHA-256
`122e6898a7420fcd97e574084990dbfac24252cdb35e325b7295e6862616738a`;
native ten-file subset SHA-256
`4bdae893c646dfe7a56b390f0610ab136d8b56dd1aa737e7f18ed96f62602246`.

Final native qualification is green: macOS 119 affected + 221 CLI tests (7
existing CLI ignores), ending 17:43:17 UTC; Windows 117 affected + 218 CLI tests
(7 existing CLI ignores), ending 17:52:34 UTC. Windows final receipts are
`affected-lock-reviewed-sync.result.json` and `cli-lock-reviewed-sync.result.json`.
The earlier Windows `affected-lock-reviewed` retry raced its source transfer and
ran the old compiled assertion; it is retained as invalid final-source proof.
The passing retries explicitly verify the final ten-file source manifest before
starting Cargo. Mac final receipts are `affected-lock-reviewed.exit` and
`cli-lock-reviewed.exit`. All applicable final source hashes match across hosts.
Native receipts/logs are mirrored into desktop diagnostic `evidence/macos/` and
`evidence/windows/`. No native candidate is installed and the Windows VM remains
booted. Linux's fresh full workspace is executing after compilation; final
Clippy/contracts/docs and release qualification remain pending.

### Broad-suite soak guard reconciliation

The full Linux `workspace-lock-reviewed` run found two stale assertions in
`gc_v4_qualification_harness_spec`: the old single-file scratch layout and exactly
two failure-count sites. The corrected runner has two owned diagnostic directories
and five explicit worker/copy/diagnostic failure sites. The test now checks those
counts, preservation of both original crash images, normal-open-before-verify
ordering, failed-open short-circuiting, and the absence of implicit `--repair`.
No production or runner behavior changed in this guard-only correction.

The revised nine-case target passes natively on Windows at 18:24:59 UTC and macOS
at 18:25:21 UTC (`harness-guards-final` receipts). Windows also received the Bash
source files because these Rust architecture checks inspect them; Bash itself is
not a native Windows execution gate. The final 15-file manifest is
`verify-source-final.sha256`, SHA-256
`35289a7d27ebb3f3af5910b0bfc867a05d383b4a73b6bd056a236d4d1d5cfbf5`.
It matches laptop/macOS/Windows. The desktop run remains on the preceding
14-file source until it finishes collecting all failures; do not edit its inputs
mid-run or relabel its expected nonzero result as passing. A fresh complete Linux
gate will follow the corrected narrow target, before any release rebuild.

The bounded continuation supervisor is `continue-after-reviewed-linux.sh`,
initial PID 3547345. It waits for the complete old run and requires exactly the
two named stale assertion failures (one failed target), then transfers the
staged test-only correction, checks all 15 source hashes, and runs
`run-verify-complete-linux.sh`. Any additional failure refuses the continuation.
Receipts are `continuation-complete.exit`, `followup-complete.exit`, and the
`*-complete` per-stage logs/guards. The fresh Linux run uses two Cargo jobs and
two test threads, with unchanged deadlines/disk floor; 32.28 GB memory was
available at admission. Native tests remain one job/thread. No current test
input is overwritten before its preceding run has a terminal receipt.

Cleanup inventory is read-only so far. Unrestricted recursive discovery and an
eight-level walk reached their 180-/120-second limits in old build trees; those
partial listings are not deletion manifests. A completed, source/build-excluding
four-level inventory found 40 database candidates (72,443,853,469 logical bytes,
72,410,009,600 allocated bytes), including retained failures and the still-needed
media inputs. Local evidence: `test-database-inventory-shallow.tsv` under the
durable release cache. Eligibility/openers must be rechecked after qualification;
no test database has been deleted by this inventory.

The original full Linux run finished at 18:49:14 UTC: 347 top-level targets,
7,503 passed, exactly the two stale guard assertions failed, 7 existing ignores;
all three nested index-store subprocess checks passed. No other target failed.
The staged correction was then admitted; `harness-complete` passed 9/9 at
18:50:12 UTC. The fresh `workspace-complete` full run started immediately with
the recorded two-job/two-test-thread environment and all 15 input hashes checked.

### Read-only correction qualified for landing

The fresh full Linux run passed at 19:22:13 UTC: **7,505 top-level tests across
347 targets**, zero failures, seven existing intentional ignores, plus three
passing nested index-store subprocess checks. Strict workspace/all-target Clippy
passed at 19:24:43. Contracts passed at 19:25:14 (454 independent fixtures,
95 routes, 39 documentation pages; debt unchanged at 8 entries/164 matches).
mdBook, the debt-checker self-test, and the 16-scenario soak harness all passed;
both completion supervisors exited zero at 19:26:46 UTC. Final Data free space
was 338,935,803,904 bytes. Formatting and diff hygiene passed locally.

Final affected/native proof is macOS **349 passed** (119 affected library,
221 CLI, 9 harness architecture) and Windows **344 passed** (117 affected
library, 218 CLI, 9 harness architecture), each with seven existing CLI ignores.
These are affected native matrices, not full native workspace runs. The native
Bash 3.2 and Linux executable soak fixtures pass all 16 scenarios. The original
failure tests were retained; no allowance, integrity threshold, or ignored-test
set was enlarged.

After all evidence writers exited, 152 files in the diagnostic campaign's
`evidence/` tree were sealed and independently rechecked. Its relative-path
`verify-evidence.sha256` manifest has SHA-256
`838fed3d3da935ff37288440d52113c637de661be6ffc43b38bed5633b2162c1`.
It includes retained failed/superseded attempts and final Linux/native proof,
without treating failed attempts as passes. The 15-file source manifest remains
`35289a7d27ebb3f3af5910b0bfc867a05d383b4a73b6bd056a236d4d1d5cfbf5`.
No database or build output is part of the source landing.

Next: commit this proven correction, prepare fresh exact-commit native release
campaigns, and renew live/resource/media/crash/duration gates. Baseline 3b48de7e
release receipts are historical only. Items 1–3 and the requested final test-data
cleanup remain open; step 4 remains an explicit stop-and-discuss boundary.

## Renewed exact release candidate — 33420bad

The qualified correction was committed and pushed as
`33420bad5b9b50a6478e871e34bb8de999c30b80`. All three hosts now have fresh detached
sources and fresh native-only release targets under `p9-release-33420bad-20260910`.
Linux is under `/media/Data/AeorDB/Tests/`; Mac/Windows are under
`~/.cache/codex/aeordb-tests/`. The final 15-file source manifest matches on every
host, with the unchanged frozen lock and 53-file portal archive. No target tree
was transferred and no original working clone was reset or switched.

Builds started at 19:29:36 UTC (Linux, jobs2), 19:30:14 (macOS, jobs1), and
19:30:19 (Windows MSVC, jobs1). Actual release profiles are preserved. Each has
a three-hour deadline, host free-space floor, log, guard and terminal receipt.
Build success is pending; the baseline binaries have not been relabeled.

Linux `continue-release-linux.sh` (initial PID 3615163) is queued behind the successful build receipt.
It seals all three Linux binaries, keeps same-host artifact copies, then runs
live HTTP/reopen/readback and the 120-second resource overlap before invoking
the media/crash/duration sequence. Its terminal receipt is
`evidence/release-continuation.exit`; the nested sequence records
`evidence/linux-sequence.exit` and per-stage receipts. The live runner now also
checks the served read-only verification documentation and byte/stat preservation
around its terminal offline check. Ports 20385/20386 were unoccupied at admission.

### Renewed build/live/resource results

- Linux release passed in 8m02s, receipt 19:38:07 UTC. Binary SHA-256:
  `27645c1c4a3d942ee2c2af5ed5a5263aff807cb3bef9956b32b395aa13ea1218`.
  Soak worker: `2e1d1731fd0e4d19d91cdb4526d0f4e188e80f8a109e42200da91c7c53206b05`;
  crash worker: `481ce790e46e3f338bcc492581cc95cc74341f0f03de4a579c2a9b5abc546be6`.
  Same-host artifact copies match; no installation occurred.
- Live HTTP/docs/readback/restart/delete/missing checks passed, including exact
  database checksum/stat preservation around final read-only verification.
  Receipt 19:39:01 UTC; both `aeordb-p9-33420bad-live-{a,b}.service` units are
  clean/inactive with successful results.
- The 120-second resource overlap passed at 19:41:32 UTC: 2,374,877,184-byte
  memory peak, zero swap, three completed KV expansions, no functional failures.
  Health p50/p95/p99/max: 0.565/10.132/90.645/409.039 ms; 4,918 samples.
  Workload: 1,328 writes, 3,054 reads, 1,117 blob commits, 355 searches,
  45 reindexes, 47 dry GC requests, 60 cancellation attempts. The test unit
  `aeordb-p4-8e-32359-3618958.service` is clean/inactive. Detailed evidence is
  `/home/wyatt/.cache/codex/p9-release-33420bad-20260910/resource-release/`.
- Native macOS release passed in 11m29s, receipt 19:41:48 UTC; binary SHA-256
  `a67dd11832c2b44a90a88d09675633b276afa885aff8f6b571a584ef8716ae5d`.
  Same-host artifact copy matches; native evidence is mirrored to desktop
  candidate `evidence/macos/` and the laptop durable release cache.
- Windows native release remains in progress. Media migration preparation
  started on Linux at 19:41:32 UTC; it has not yet passed. No crash/duration
  success is claimed for this candidate yet.

### Native release closure and capacity-qualified media retry

Windows MSVC release passed at 19:55:35 UTC in 25m14s; binary SHA-256
`088ebc655069730f8f936709eff794d0be163581640805039c22b6464e9be70a`.
The same-host artifact copy matches. Final C: free space was 19,301,474,304 bytes,
above its 8 GB floor. Native evidence is mirrored into desktop candidate
`evidence/windows/`. All three native release candidates now pass on exact
33420bad; none was installed or published.

The large source's strict verification passed exit 0 and preserved SHA-256
`2be26ba43beb289bee0573355936b0ccd303540e39b13f815f1fedeeb504db74`.
The first migration attempt then correctly refused `CapacityInsufficient` before
creating a destination or private workspace. Both old sequence/continuation
drivers exited 1 at 19:59:36 UTC; this is retained failed setup evidence, not a
migration pass or corruption event.

Cause: the harness omitted the previous rehearsal's explicit 1 GiB capture
limit. The registered defaults derive from the source's 6 TB volume and requested
64 GiB capture + 4 GiB root map + 128 GiB reserve on the separate `/home` workspace
volume: 210,453,397,504 bytes required versus 130,359,328,768 available.
Data still had 324,261,818,368 free bytes. The prior successful media manifest
explicitly used 1 GiB capture/1 GiB reserve; its bounds were not the defaults.

The new operational runner restores the tighter 1 GiB capture cap and keeps a
64 GiB home reserve (more conservative than that prior rehearsal), with the
unchanged 4 GiB root-map cap. The 250 GB Data floor is unchanged. A new versioned
guard, `run-unix-renewed.sh`, monitors both Data and the 64 GiB home reserve and
checks both at exit. No source-code change, new release build, or deletion is
needed. The unchanged source copy is freshly checksummed before reuse, avoiding
another 11.65 GB copy; all original/failed evidence remains intact.

Active queue is now `run-linux-renewed-sequence.sh`, initial PID 3637526. It writes
`evidence/linux-renewed-sequence.{launch.log,tsv,exit}`, begins with
`media-capacity-retry`, then runs the still-unexecuted CLI/crash/short/long stages.
New destination: `media-capacity-retry/shadow-v4.aeordb`; new private workspace:
`/home/wyatt/.cache/codex/p9-release-33420bad-20260910/media-capacity-retry-workspace`.
Per-migration proof is `evidence/media-capacity-retry/`. Every invocation, including
resume/retry, carries the same explicit captured configuration. The original
failed runner files/receipts are not overwritten, and both old owned PIDs were
confirmed absent before this retry. Step 4 remains closed.

### Renewed large-media qualification passed

`media-capacity-retry` passed at 20:56:23 UTC. Its planned interruption exited 137
at 20:21:07. The supervisor had observed `base_successor_published`; the last
milestone before the signal was already `destination_verification_running`.
This is a real interruption during destination verification after publication,
not proof that execution stopped exactly at the earlier base-only boundary.
The synthetic restart suite separately exercises the individual durable stages.

Resume and the completed-state retry both returned full verified completion:
15,354,506,282 copied/verified reachable content bytes, 50,151 distinct verified
entities, one verified root, and distinct source/destination physical identities.
Their versioned receipts are byte-identical. Source and destination SHA-256,
size, inode, mode and modification time are unchanged across the completed retry.

- Source: 11,654,356,141 bytes, SHA-256
  `2be26ba43beb289bee0573355936b0ccd303540e39b13f815f1fedeeb504db74`.
- Destination: 11,447,013,668 bytes, SHA-256
  `40cca2aa2c13149a77e5afb2540b280a3480de127cf690116184b500e8e3528e`.
- Resume: 828.76 seconds, 71,596 KiB maximum RSS, zero swaps.
- Completed retry: 822.13 seconds, 71,728 KiB maximum RSS, zero swaps.
- The sealed original's full checksum and stat remain unchanged; stat is
  `2081:2599017:11654356141:1788493099:777`.
- Data/home free at exit: 312,813,277,184 / 130,288,603,136 bytes.
- Closed media evidence was sealed and rechecked as
  `evidence/media-capacity-retry.sha256`, manifest SHA-256
  `cd094d966645c165962abd2f34e408eaf8c51f8b4ed0770245c4f684c0187cde`.

The same renewed sequence began release-mode CLI tests at 20:56:23 UTC. The
100-pass restart matrix and short/12-hour soaks remain queued, not yet passed.

Release-mode CLI qualification passed at 21:01:54 UTC: 221 tests across 21
targets, zero failures, seven existing intentional ignores. The same queue then
started `crash-100`, which requests **100 complete suite passes**, not 100
individual cases. Its force-unmount branch remains unconfigured/self-skipping;
no mount or forced-unmount operation is authorized or enabled. Short and long
soaks remain queued. Data/home free space was approximately 308.75/130.29 GB.

### Completion-packet review while duration qualification runs

The current canonical report/DoD/JSON still describe historical 535004f1.
Replace their current-facing claims only after the remaining gates close, while
preserving that older snapshot as explicitly historical evidence. Update the
parent's mutable execution banner, not its ratification-time header. In
particular, the final packet must distinguish these boundaries:

- Ordinary service opens, reads, writes, queries and GC still use the v3
  compatibility runtime. The v4 substrate is implemented and tested; the public
  `migrate-v4` command creates/verifies an offline shadow. A production-serving
  v4 activation/cutover command is not available. This is an implementation
  boundary, not merely a deployment permission waiting to be granted.
- The production-scale repair was deliberately retired, not successfully
  migrated; the retained immutable source is outside this task's cleanup scope.
- The renewed Linux full suite is 7,505 top-level tests plus three nested checks;
  the 349 macOS / 344 Windows results are affected native matrices, not fresh
  full native workspaces. Native release builds use exact 33420bad.
- Each crash-suite pass runs 23 planned process-interruption windows (10 writes,
  five mixed, four GC, four stress), plus bit-flip and truncation cases. Seven
  test functions report success, but one is the explicitly self-skipping
  force-unmount branch. Count complete suite passes rather than claiming 100
  individual interruption cases or a tested unmount operation.
- The current S1 wrapper performs terminal read-only verification and then
  hashes the database. It does not itself compare before/after verification
  hashes; byte invariance is separately proven by the CLI/library regressions,
  live release gate, and large-media gate. Do not overstate this wrapper's proof.
- Cleanup removes only selected inactive disposable database files, retaining
  checkpoint/metrics/logs and useful failure artifacts. Seal evidence before
  deletion, then record explicitly retired database manifest entries; an old
  manifest containing deleted databases cannot afterward be reported as fully
  reverified. The shallow inventory is not a deletion allowlist.

At 21:17:46 UTC the renewed driver was alive, 24 crash passes had completed and
pass 25 was active. No failure or capacity refusal; Data/home free space was
308.25/130.29 GB. No database cleanup, publication, installation, or production
operation has occurred.

### Exact executable identity correction before duration tests

The first `crash-100` completed all 100 suite passes at 22:05:26 UTC: 2,300
process-interruption windows, 100 bit-flip cases, 100 truncation cases, and
100 explicit unmount self-skips. Its elapsed loop time was 3,799 seconds.
The next S1 admission checksum correctly stopped the renewed sequence at
22:05:57 UTC, before creating a soak database.

Cause: `cargo test --release -p aeordb-cli --all-targets` unified the CLI's
Tokio `test-util` dev-dependency feature and replaced the top-level Cargo output
executables. The initial normal-release artifacts remained intact in the
separate `artifacts/` directory. Both Tokio fingerprints and both worker
fingerprints are preserved in `evidence/binary-identity-drift/`; source, lock,
release profile and qualified normal-release hashes are unchanged. Cargo's
later build/no-run invocations did not make the final top-level test-worker
path an exact match to the normal-release artifact.

The first 100 passes are retained as successful **test-feature-build** evidence,
not normal-release-worker qualification. Their worker SHA-256 is
`dae237a18bd1b0ae5ac2fd3a4b88f4a238de2110103331cb52a49d32914ce8a0`;
the pinned normal-release worker remains
`481ce790e46e3f338bcc492581cc95cc74341f0f03de4a579c2a9b5abc546be6`.
The 221-test release-profile CLI matrix remains valid source/test-build proof;
the existing live and media gates separately used the normal-release executable.

New same-host orchestration leaves source and Cargo output untouched:

- `run-crash-pinned.sh` invokes the existing integration-test executable directly
  from an isolated mirror whose adjacent worker resolves to the sealed release
  artifact. It repeats 100 complete passes without invoking Cargo. The test
  executable still has its normal test dependency graph; the child worker is
  now exactly the qualified normal-release executable.
- `soak-pinned.sh` changes only repository/binary path binding and replaces the
  build preamble with checksum admission. Workload, diagnostic-copy, recovery,
  verification, checkpoint and failure-stop logic match tracked 33420bad.
- All 16 existing shell scenario fixtures pass against the pinned variant;
  four additional admission cases reject missing manifest, missing binary,
  altered binary, and absent explicit directory before worker/database creation.
  Log hashes: `2dec3655567f6641c55861ed1e9c2b1639e6ac8febe27aded0783879d058663b`
  and `47079361b5c1ce165fcf518671407ae8e335ecd3f0abdf1cbe8f990558b6ec3c`.
  Pinned harness SHA-256:
  `b7ca2b8735bb039e5aa7cd2d4f3a52bbaae57eae193609377a4482a334f479f1`.
- `run-soak-pinned-stage.sh` pins every executable and additionally compares
  S1's complete database checksum and nanosecond stat around terminal read-only
  verification. Stage logs use `*-pinned`; data directories retain their original
  names because the failed admission created none.
- Preparation passed at 22:12:57 UTC. All source/artifact and copied test
  executable identities matched. Data/home free: 308,052,602,880 /
  130,103,095,296 bytes. The retained original failed sequence is not restarted.

The new `run-linux-pinned-sequence.sh` owns `evidence/linux-pinned-sequence.*`
and queues `crash-100-pinned`, the three short stages, then three sequential
12-hour stages. The unchanged two-volume guard and all original deadlines
remain active. No database deletion, installation, production access, or step 4
operation occurred. This is an executable-selection correction, not a Rust fix
or grounds to rebuild the already-qualified native releases.

Pinned driver PID 3719078 started at 22:13:57 UTC. At 22:19:59, nine passes
had completed and pass ten was active; Data/home remained above both floors.
This ledger checkpoint records completed build/live/resource/media gates and
the resolved executable-selection issue, not completion of pending long tests.

### Pinned crash and short-soak closure; long stages active

The exact-release-worker rerun passed all 100 suites at 23:17:59 UTC, with
3,828 seconds of loop execution, 2,300 process-interruption windows, and 100
explicit unmount self-skips. All sealed normal-release executable and copied
test-executable checksums still match. Log SHA-256:
`0904bd4603e9416d63a29cdd39156454ffbc8fe05afb4332c9ff20d9e1ab1b98`.

All short stages passed on the pinned normal-release binaries:

| Stage | Workload result | Terminal receipt UTC | Log SHA-256 |
| --- | --- | --- | --- |
| S1, 36 seconds | 339 writes, 136 reads, 53 deletes; strict verification and before/after database byte/stat checks pass | 23:18:59 | `1e533f773184ba3108d2aa28268de52246d3fd70686f04ee1043244bb760e4d6` |
| S2, 90-second loop window | Eight interruption/copy/reopen/verification cycles, no issue cycle | 23:21:00 | `e0f16bc2ffc76357aef4f5503480e02bb39b1e37eae9fc312fb55b9fdffb3af7` |
| S3, 90-second loop window | 13 interruption/copy/reopen/verification/checkpoint cycles, no issue cycle | 23:23:00 | `0a5d4478681ca04a60eeaa74f5d12aa5ac465af8599a6c23c6278f9eab9443fc` |

Closed short database sizes are 14,547,533 / 14,246,961 / 3,348,415 bytes;
all three complete-file checksum manifests were independently rechecked.

The same driver began `s1-12h-pinned` at 23:23:00 UTC, worker PID 3757938.
S2/S3 12-hour stages remain sequentially queued. Long-stage completion includes
review of terminal verification/checkpoints and retained resource metrics;
worker exit alone is not a claim that the memory-growth summary passed.
At 23:23, Data/home had 308.01/129.99 GB free. Owner-requested ten-minute model
monitoring and continuous host-side capacity/deadline guards remain in effect.
No test database cleanup or step 4 operation has occurred.

Closed prerequisite evidence was sealed and rechecked at 23:26:26 UTC:
`evidence/pinned-prerequisites.sha256`, 60 artifacts, SHA-256
`acf03564995858c99ff5274af3790e3e8a0504deaee736291bf2d98609b38ae5`.
The seal contains logs, receipts, metrics, checkpoint files and database checksum
records, not raw database payloads. Raw short database hashes were separately
verified at sealing. This distinction permits later explicit test-data retirement
without pretending the deleted database payloads remain available for checking.

S1's post-warmup throughput drops substantially after the first periodic GC;
the retained same-corpus historical run shows the same pattern, so this is not
established as a new regression. At 1,800/3,600 seconds, historical 9a71d4ce had
18,376/18,776 writes and current 33420bad had 19,486/19,899. First-GC void bytes
were 353,416,023 / 358,744,998 respectively. The current 3,600-second memory
baseline is 535,672 KiB RSS / 534,264 KiB VmData, with 13 open descriptors.
Do not equate a passing stability soak with sustained warmup-level throughput;
retain this characterization for later real-client operational discussion.

During S1, unrelated desktop home-volume activity reduced available space from
about 130 GB to 77.7 GB. The owned worker's open regular files were all on Data;
its private home scratch remained empty. A read-only I/O sample identified a
separate rsync writer. No private arguments/destination contents were inspected
and no unrelated process was changed. At 2026-09-11T10:28Z both observed rsync
PIDs had exited and home free space had stabilized. The 250 GB Data and 64 GiB
home guards never required relaxation; current S1 remains active, not passed.

### S1 terminal integrity failure — 2026-09-11

The worker completed 12 hours cleanly (27,281 writes / 13,851 reads / 4,536
deletes), but terminal read-only verification exited 2 with 20 missing KV
entries. All 20 reported physical headers are DirectoryIndex records. Corrupt
hash/header, stale KV, missing children, dangling records, B-tree issues,
unlisted files, invalid offsets/voids and broken snapshots were all zero.
The pinned sequence stopped at 11:23:47 UTC; S2/S3 did not start. This is a
failed integrity gate, not a capacity/deadline stop or a successful S1 result.

The 631,174,559-byte database remains preserved at `long/s1-12h/soak.aeordb`,
SHA-256 `74e1103abd16299fec96b9ee94c0941347453d0c14f0a8e8689fca5b9f9a9a65`.
Its checksum and nanosecond stat still match the pre-verification records.
No normal reopen, repair or cleanup has been performed on this failure artifact.
The separate resource summary passes: 721 rows, 12.00 hours logged, RSS growth
14.4%, VmData growth 17.3%, and maximum 14 descriptors. Final Data/home free
space was 307,382,878,208 / 77,735,796,736 bytes.

Current landing unit: investigate/reproduce the directory-key discrepancy,
then correct the proven cause and qualify the affected perimeter. Source entry
is 33420bad (documentation HEAD 2a5c74ed); fetched origin matches HEAD. Direct
owner is Codex. Owned territory is the GC/WAL/verification/rebuild interaction
and its regression evidence; production, frozen formats, activation, unrelated
user files and original failed database bytes remain forbidden. Two narrow GC
tests now target repeated physical directory records: clean verification after
sweep and no resurrection during KV rebuild. They are unrun target tests, not
proof of cause yet. Next command is the filtered desktop `gc_spec` baseline;
do not restart the duration sequence or relax verification to make it pass.

### S1 causal reproduction and bounded correction in progress

Follow-up evidence lives at desktop
`/media/Data/AeorDB/Tests/p9-s1-kv-followup-20260911/`. A separately copied
failed S1 stage is sealed by `frozen-s1-evidence.sha256` (17 files, manifest
SHA `2194d157ed80475998637f2a0257cbda8be452dcc47da41aa6ec9af4cf004904`).
The original and frozen copy are not repair or normal-open targets.

Independent read-only structural inspection proves all 20 missing keys have an
older unvoided record and a later void-covered record. One later timestamp is
at a lower physical offset, so chronology must retain `(timestamp, offset)`.
The shared verification/rebuild resolver previously excluded void-covered
records before chronology resolution, thereby selecting obsolete duplicates.
Rebuild can actually resurrect those keys; this is not only a report mismatch.

`duplicate-directory-red` was a compile error, not behavioral proof.
`duplicate-directory-red2` fails both targeted GC regressions on 33420bad;
`duplicate-directory-green` passes both with the initial correction.
`shared-resolution-green` passes 14 workspace and 8 scanner tests. Review then
found the initial correction could eclipse a legitimate empty rewrite after
retirement: `post-retirement-empty-red` fails the two added edge cases (4 pass,
2 fail, exit 101 at 12:05:28 UTC). All receipts are retained.

The next correction resolves the latest verified retirement cutoff first,
then applies the existing directory preference only to newer surviving values.
This is bounded external sorting with constant per-key resolution state, not a
KV placement redesign, GC activation, or frozen database-format change. Only
the disposable, non-resumable private rebuild-run version changes. Void-covered
chunk records are streamed/hash-checked before supplying retirement evidence;
ordinary historical chunks retain the existing metadata-only rebuild scan.
Strict verification still checks all payloads and does not suppress stale KV.

Remaining proof: new edge-case green, affected regressions, byte-identical S1
copy baseline/new read-only verification, accelerated GC exercise, broad/static/
native gates, new exact releases and release qualification. No S1 integrity
pass, S2/S3 run, test-database cleanup, deployment or step-4 action is claimed.

At 12:10:05 UTC, `retirement-resolution-green` passed the original edge cases
but failed the new independent exhaustive history model (18 pass / 1 fail).
The first mismatch was history 100: values at `(10,300)` and `(20,100)`, then
deletion at `(20,200)`. The preexisting private scratch codec wrote deletion
offset zero, losing its chronology tie-breaker on readback. The correction keeps
the deletion's physical offset in its private run record; no database layout
changes. The model enumerates 3,000 histories across directory/file types,
two chronology patterns, both hash widths, and three spill capacities, using a
separate complete-history oracle instead of the production resolver.

`retirement-resolution-green2` passed at 12:15:29 UTC: 20 workspace tests
(including all modeled histories and a named same-timestamp deletion/recreation
regression), 8 scanner tests and both original GC tests. Latest additional GC
tests require genuinely missing unretired keys and corrupt void-covered chunk
records to remain visible failures, with no partial KV publication.
`run-s1-diagnostic-sequence.sh` now runs affected storage tests, diagnostic-only
binaries, byte-identical copied-S1 baseline/corrected verification, a three-minute
accelerated GC/snapshot exercise, prospective audit inventory refresh and the
failed-S1 resource summary. Each stage has a deadline, two-volume guard and
terminal receipt; any failure stops subsequent stages. Release soaks stay stopped.

### Real failed-image proof and affected storage checks

`affected-storage` passed 281 tests across seven targets (12:18:03 UTC), including
the two added strict-failure guards. Diagnostic dev-profile artifacts are pinned
under `diagnostic-artifacts/`: CLI SHA
`162040949318d707826ef5a0b3350bd43b5e17861b98ef1f4d3749f7aaf0ff04`, worker SHA
`2bb86b6c6f990f7f4c9885c8aff81aa6a6cfa01b5b6b28f52b06e35ae1c9a441`.

`copied-s1-read-only` passed at 12:21:05 UTC on a fresh byte-identical 631 MB
copy, without repair or normal startup. Baseline release verification exited 2
with 20 missing keys; corrected diagnostic verification exited 0 with zero
integrity issues. SHA and nanosecond stat remain unchanged across both passes;
the original frozen evidence manifest rechecks. Baseline took 14.71 seconds /
62,352 KiB maximum RSS; corrected dev build took 56.66 seconds / 88,812 KiB.
Different build profiles mean these are not a performance comparison.

`accelerated-gc` passed at 12:24:36 UTC: 181.67 seconds, 958 writes / 484 reads /
160 deletes, configured snapshots every 5 seconds and GC every 10 seconds.
Terminal read-only verification reports 45 snapshots, 1,288 voids and no issues;
database SHA/stat remain unchanged around verification. Maximum worker RSS was
25,532 KiB. This is a short diagnostic stress exercise, not a replacement for
the required release-duration soaks.

The original failed S1's one-second memory log has 43,020 rows; peak RSS,
maximum current RSS and maximum high-water mark are all 612,544 KiB. Its resource
pass remains separate from the failed integrity result.

Native macOS initial checks passed 661 library and 281 affected-storage tests
by 12:24:38 UTC; Windows checks are underway. A Windows wrapper generation error
failed parsing before tests started; the failed wrapper is retained and the
corrected wrapper now receives an explicit parser admission check.

`audit-refresh` and `audit-pre-refresh-check` correctly refused a new syntactic
default-on-error occurrence (1503 to 1504, at `update_resolved_group`). This was
an `Option` predicate, not a swallowed engine error. It is now expressed with
`is_none_or`, preserving behavior without increasing the reviewed ceiling.
`audit-refresh2` checks a prospective inventory before any source adoption.
Initial diagnostic/native evidence predates that one-expression normalization;
final-source broad/native/release qualification remains required. No failing
receipt was overwritten and the original sequence stops at the audit refusal.

### Final-source ordinary qualification

The adopted inventory has exactly 1,503 entries with unchanged identities,
reviews and ceiling; only 30 line locations changed. `audit-refresh2` passes
(12:28:33 UTC). Final eight-file input manifest SHA is
`a311381db8a78562d55e42af4a8a19a2d0f7415154ab808d971e58e3523a93dd`;
native input archive SHA is
`3c865c61a7f9ffabedaa5b5edc78081d80efedc476989dc6f5c3938db8fd6f6b`.
Both include the new separate workspace spec and the updated audit inventory.

Linux `final-narrow` passes 20 reconstruction tests and 29 audit/architecture
tests at 12:35:10 UTC. Fresh one-job same-host WASM prerequisites pass at
12:37:10 UTC; `final-workspace` passes at 13:14:44 UTC: 7,521 top-level tests
across 347 targets, zero failures, seven existing ignores, plus three separately
counted nested index-store checks. It used two Cargo jobs, two test threads,
debug information disabled and incremental compilation disabled. Guard floors
remain 250 GB Data and 64 GiB home; no source changes occur during a gate.

Native macOS final-source qualification passes 1,192 tests: 661 library, 310
affected storage/audit across eight targets, and 221 CLI across 21 targets with
seven existing ignores. Last receipt is 12:38:43 UTC, 82,125,180,928 bytes free.
This is the relevant native matrix, not the full native workspace suite.
Native Windows final-source qualification passes 1,185 tests: 658 library,
309 affected storage/audit across eight targets, and 218 CLI across 21 targets
with seven existing ignores. Last receipt is 12:50:19 UTC; final C: free space
is 19,007,410,176 bytes. The platform-specific count difference is preserved,
not rounded to the macOS matrix. Its preexisting conditional-test unused-import
warning remains recorded; this is not a claim of native strict Clippy.
No new release candidate or successful duration-soak result is claimed yet.

The successor release harness is prepared under local durable cache
`aeordb-release-qualification-20260910/s1-release-templates/`. Its 16 pinned-soak
fixtures and four admission refusals pass; Bash and native PowerShell syntax
checks pass. Rendering refuses dirty source before creating a candidate.
The next build reuses only each host's inactive predecessor Cargo cache, not
transferred target artifacts, and pins normal-release executables before CLI
test-feature unification. Media capture is explicitly 1 GiB with a 64 GiB home
reserve. The duration sequence includes a mandatory S1 resource-summary gate.
No production operation is present in that sequence.

Final Linux static/architecture closure passes: strict workspace/all-target
Clippy at 13:17:15 UTC; contracts at 13:17:46 (454 independent fixtures,
95 routes, 39 documents, debt 8 reviewed entries/164 retained matches);
mdBook at 13:18:16; debt self-tests at 13:18:47; soak helper/16 scenario fixtures
at 13:19:18. The failed predecessor S1 resource summary is durably captured at
13:19:48 and still does not convert its failed integrity result into a pass.
The complete final-source sequence exits 0 at 13:19:48, with Data/home free
space 305,000,923,136 / 77,577,195,520 bytes. Formatting and diff checks pass.

The closed diagnostic/native evidence seal contains 236 files under the
follow-up campaign, including the frozen failure, the separate read-only proof
copy, diagnostic binaries, final Linux receipts, both native matrices and failed
attempts. `closed-followup-evidence.sha256` rechecks completely and has SHA-256
`2fb99032ebba73b85904a37ad594b4e2f31a4cf1887b020a0080749e838aca3e`.
This is the green correction landing boundary; it is not the successor release
or duration-soak evidence. Review confirms the only serialized change is the
private disposable rebuild-run version, not a public/frozen database contract.

### Successor release qualification — 48baeefe

The correction is committed/pushed as `48baeefe0144e2a84458c6589aa0345afc223ff8`
(nine scoped files; unrelated user work retained). Fresh detached sources on
all three hosts share that commit, the frozen lockfile and 53-file portal archive.
Desktop campaign: `/media/Data/AeorDB/Tests/p9-release-48baeefe-20260911/`;
native campaigns: `~/.cache/codex/aeordb-tests/p9-release-48baeefe-20260911/`.
Only inactive same-host predecessor Cargo targets are reused; no target artifacts
cross hosts. Normal release builds started at approximately 13:25 UTC.

The Linux sequence runs under a 200,000-second outer deadline (launcher 90873),
with per-stage deadlines, 30-second two-volume guards and durable terminal
receipts. `monitor-candidate.sh` reads the owned driver PID and current stage.
Its runner manifest SHA is
`c5433e20018b1abdf380128d4cd36df8e26d7d0d74cfcb8d031c40c6190b873d`.
Native builds have independent deadlines and capacity guards; their results
are recorded below. Duration soaks and cleanup remain pending.

Linux normal release passes at 13:29:01 UTC (3m56s, two jobs, optimized plus
debug information). Pinned hashes: CLI
`3f378464a60ce66014b7ec8c50a76cb64953878356c3ccc434e9e1de81c536a2`,
soak worker `32ae84ad9e06ec535409c868b486cd8469b0d26c39a68813b8746fa8e806d707`,
crash worker `f87491e9dbfe36041d3fc8984871ad545785659bb272826f9ef1230dcd48e64b`.
macOS arm64 normal release passes at 13:28:47 UTC (3m17s, one job), SHA
`005757daafe0b77e6f92dc9738eacf581605fe20913645126c91e19949bd66a2`.

The exact Linux release passes `copied-s1-release` at 13:29:41 UTC: the same
closed 631 MB failure copy now strictly verifies with zero missing entries and
zero other issues, while SHA and nanosecond stat stay unchanged. Verification
takes 17.23 seconds, maximum RSS 62,708 KiB, zero swap. No repair or normal open
is used, and this is not a controlled performance comparison. Both pinned
harness fixture gates pass, followed by live HTTP/docs/binary readback, clean
shutdown/restart, delete/missing cases and read-only terminal verification at
13:31:12 UTC.

Windows native MSVC Rust 1.96.0 release passes at 13:36:46 UTC (11m38s build, 698.75-second
controller elapsed, one job). Pinned executable SHA
`7fabb0a9bced42f4f9206f381f58315e039325dff94289ebc7665104e2f57def`;
minimum/final C: free space 18,749,120,512 / 18,900,402,176 bytes. All three
native release builds therefore pass for 48baeefe; no binary is installed or
published, and no production service is changed.

The 120-second resource overlap passes at 13:33:43 UTC: 2,190,843,904-byte
memory peak, zero swap, three completed KV expansions, no functional failures.
Health p50/p95/p99/maximum is 0.561/9.708/98.872/676.777 ms (4,900 samples).
The workload completed 1,124 writes, 2,961 reads, 961 blob commits, 304 searches,
43 reindex tasks, 45 dry-run GC operations and 60 cancellation probes.
The owned service exits cleanly. Media migration is running, with Data/home
free space 285,452,193,792 / 75,599,044,608 bytes at 13:39:44 UTC.

Capacity follow-up: S2 creates one recovery copy in private home scratch per
cycle (S3 creates two smaller copies). Home has about 6.9 GB above its 64 GiB
reserve, while historical S2 reached about 7.8 GB. An asynchronous owner question
requests permission to relocate only the completed 1.9 GB synthetic resource
database from home to the Data test area, retaining contents and logs. No file
has been moved or deleted; the question does not block current migration or S1.
Do not lower the reserve or inspect unrelated home data.

Media migration passes at 13:50:18 UTC. The planned first-run interruption
returns 137 at 13:41:07 after observing base-successor publication (not a claim
of an exact base-only interruption window). Resume and completed retry both
report full destination verification of 15,354,506,282 content bytes and 50,151
entities, distinct source/destination physical identities, and identical final
receipts. Source/original checksums and stats are unchanged; completed retry
also preserves destination checksum/stat.

Current media source SHA is
`2be26ba43beb289bee0573355936b0ccd303540e39b13f815f1fedeeb504db74`;
destination SHA is
`f6728854a5cd0a29816b8502720616a3e4c851632c88ff7489ed09cae18daa93`.
Resume takes 314.53 seconds / 129,048 KiB maximum RSS; completed retry takes
39.98 seconds / 73,160 KiB, both zero swap. These timings are not a controlled
performance comparison. Capture remains 1 GiB, home reserve
64 GiB; final Data/home free space is 281,223,258,112 / 75,597,398,016 bytes.
Release CLI qualification is now running; crash100 and all duration gates remain
pending, as does the unanswered home-artifact relocation question.

Release CLI qualification passes at 13:55:19 UTC: 221 tests across 21 targets,
zero failures, seven existing ignores. `pin-crash-test` passes at 13:55:49,
discovering the test executable from Cargo's JSON artifact messages and copying
it beside the exact normal-release worker. Pinned crash manifest SHA is
`6d5c34d1db5239f4758eba1ebbde5558098b43049fee1174198a2a864fe78d5c`.
The 100-complete-suite crash loop is running (pass 10 active at approximately
14:01 UTC), with unmount tests explicitly self-skipped. Later short and duration
stages have not yet run. Model checks now use ten-minute intervals; host guards
remain at 30 seconds.

### September 11: short S3 checkpoint restart failure

The 100 complete seven-function crash suites pass at 14:59:51 UTC (3,826 seconds,
2,300 interruption windows, 100 explicit unmount self-skips). Log SHA:
`cb5d582811adfc64a2d451586f634612f13e35239f8f5e9fa0fdd980d7762cc2`.
Short S1 and S2 pass at 15:00:52 and 15:02:52. Short S3 stops after cycle 8;
driver 90874 terminates at 15:04:23, exit 1, no capacity/deadline refusal.
No 12-hour stage started on 48baeefe.

The probe reports **malformed checkpoint**, not an established missing database
record. Line 1930 concatenates an interrupted prior path with the next worker's
startup comment: `/stress/batch-merge/doc-028.json# worker up mode=stress`.
The crash worker opens its checkpoint in append mode without removing the
nonterminated tail that read-only comparison correctly ignored during cycle 7.
The ordinary soak worker already truncates such a tail during checkpoint load.
The independently recovered verification copy reports Status OK/zero issues.

Preserve the original `long/s3-short/` under the current desktop release campaign
and `/home/wyatt/.cache/codex/p9-release-48baeefe-20260911/long/s3-short/diagnostics.RtcNg5/`.
Original DB SHA `537763019a5b58dda15dbc11a4ea0ae33b8dd0671abf9ac1a09a389a76fb320d`
(3,022,673 bytes); checkpoint SHA
`13aae8c8f19c187b58e51549e9dafc04514c742f1f0965265bb545b7cd890b5e`.
Do not alter or restart against these files.

Current bounded landing unit: preserve/hash failure closure; reproduce worker
restart against a deliberately incomplete checkpoint; cover malformed completed
records, truncation boundaries and failure handling; correct worker checkpoint
admission without weakening the read-only oracle; qualify narrow/broad/native,
then renew affected release/crash/soak proof. Owned hotspots are CLI checkpoint
reader, crash worker and their specs. Storage formats/engine, retained evidence,
production and step 4 remain outside this correction's scope. The `implement`
workflow requires failing-first proof and retains the failed gate as evidence.

The new desktop follow-up is
`/media/Data/AeorDB/Tests/p9-s3-checkpoint-followup-20260911/`, detached 48baeefe
plus the seven scoped source/test/script inputs. Failure preservation completes
at 15:25:53 UTC: the copied 21-file closure is checked under
`frozen-s3-evidence.sha256`, SHA
`a6d57beb4def31f2866373ef3724dde4cbc26506a5f0cd99c42c3f9d86d821ae`.
Original source bytes remain unchanged and no opener was present.

`checkpoint-restart-red` fails both independently authored worker restart tests
at 15:28:59: interrupted-line concatenation and continued writes after a malformed
completed record. Initial correction passes 22 tests at 15:30:51 (seven existing
crash ignores). Expanded `checkpoint-perimeter` passes at 15:35:51, including
exclusive database ownership refusal, every byte-cut boundary across all record
kinds/CRLF/multibyte text, idempotent admission, bounded oversized-record refusal,
read/truncate errors and injected barrier failure. The worker removes only the
incomplete tail, syncs truncation before its next append, and never changes the
complete prefix or a malformed completed checkpoint. The report-only reader
remains non-mutating. No engine/database format or ordinary soak-worker behavior
changes.

An adjacent shell-message regression fails on the old claim that any probe error
is data loss (`soak-message-red.log`, SHA
`9e2e3a73840aee8b193f0e742ab0cd7fb1f01d85e990d13b0475c63542ee86f2`).
The runner now says checkpoint comparison failed; it still stops and retains all
failure evidence. Combined shell helper/16 scenarios pass at 15:36:22. The audit
refresh at 15:36:52 reports the same 1,503 entries; the only generated-file
difference is its terminal newline, so no allowlist change is adopted.

Seven-input source manifest SHA:
`92e73c5aa85f444e80ff717285a2fe8aa808033a229c34347556da4b95a7e1e0`.
Native archive SHA:
`0b803cc0d95d2b2d81e5ebf14c8d14869e9f986489f396a4096f7ece3da44ac8`.
Native sources/evidence are under `~/.cache/codex/aeordb-tests/` with the same
follow-up basename. macOS CLI all-targets passes at 15:38:39; Windows CLI remains
running. Fresh full Linux/static and exact release/crash/soak gates are pending;
this correction is not yet committed or fully qualified.

Native Windows catches a portability defect in the initial correction: CLI
library test `append_admission_propagates_a_failed_barrier_after_truncation`
fails before reaching its injected barrier (23 pass/1 fail, 15:40:44 UTC).
A separate std-only handle probe confirms Windows returns access denied/code 5
for `set_len` on a read/append handle, preserving length 15; a read/write handle
successfully truncates it to 7. This is not waived as a fixture-only failure.
The worker now opens read/write with `truncate(false)`, and successful admission
explicitly seeks to EOF. The exclusive database ownership requirement protects
that single-writer append protocol. The same byte-boundary tests verify that no
prefix is overwritten; native results are rerun on the portable correction.

Final seven-input manifest SHA:
`b488f0a21705e06952cbe9583fb1c2010de4589ddce5b00fa2dcf913e8cb7326`;
final archive SHA:
`97c217e0399cbcfef53d989179cebe6124f1b3bef9514d09e7b554fa86ab6075`.
Linux portable perimeter passes at 15:45:05; fresh full/static sequence and
native portable CLI/architecture matrices are running. Initial native receipts
remain preserved. Fresh guest prerequisites passed at 15:42:43 using unchanged
guest/engine inputs, before the final CLI-only transfer.

Qualification scope for this checkpoint-only correction: rerun complete Linux,
affected native/CLI/static and exact release/crash/soak gates. Carry forward the
successful 48baeefe interrupted full-media migration, live and resource results
as explicitly attributed evidence of unchanged storage/migration/runtime code;
do not claim these were executed by a successor binary. Confirm the production
source delta mechanically and recheck the retained media evidence/input hashes
before final packet closure. This avoids an unnecessary extra 23 GB media copy.
All three 12-hour soaks still need to pass on the corrected checkpoint tooling.

Portable native qualification now passes: macOS CLI 230 tests/21 targets/seven
existing ignores at 15:45:34 and architecture 38 at 15:46:04; Windows CLI 227
tests/21 targets/seven existing ignores at 15:47:01 and architecture 38 at
15:49:24. Both native sequences are complete and their evidence is mirrored to
the laptop durable cache. The full Linux suite/static sequence remains running.

The prepared successor harness places large S2/S3 diagnostic database copies on
Data alongside their disposable test databases, while keeping private internal
workspaces in the guarded home TMPDIR. This avoids moving the completed resource
fixture; no answer to the earlier relocation question is needed for that path.
No resource file has been moved. Both volume floors remain unchanged. The
successor's 16 shell scenarios and four pinned-input admission cases pass. Its
media carry-forward gate checks source equivalence, prior receipts/artifact
hashes and a fresh successor CLI read-only verify against the already separate
clean test source, preserving source bytes/stat and explicit binary attribution.
It does not attempt cross-binary resume of a manifest pinned to the earlier
executable, and does not create another full media source/destination pair.

### September 11: full-suite capacity interruption

The portable Linux full run stops at 15:59:06 UTC with exit 124 and
`termination_reason=disk_floor`, not an assertion failure. Data remains
280,398,594,048 bytes free; home falls to 66,821,873,664 bytes, below its
68,719,476,736-byte floor. Inspection identifies an owned 8 GiB temporary KV
fixture, not an unrelated home writer. `disk_kv_store_spec` has two tests that
create a maximum-stage (8 GiB) block; available home headroom is only about
6.7 GB. The stopped run and fixture are preserved, not counted as a full pass.

Capacity correction does not change Rust source, assertions, or floors:
preserve/checksum the inactive large fixture on Data, verify it, remove only
its redundant home copy, then run both maximum-stage cases individually with
Data TMPDIR. The full workspace rerun excludes precisely those two already-run
cases, with combined evidence covering every original test. All remaining
tests retain private home TMPDIR. The two names are unique across the workspace;
the large-case executable is pinned from the stopped full run's actual logged
test binary. No test is waived, and aggregate counts must include those two
separate passes without double-counting them. New runner:
`run-s3-capacity-linux-sequence.sh`; old receipts remain untouched.

Relocation and lossless archival complete at 16:15:20 UTC. The original
8,589,933,656-byte fixture is retained as
`capacity-stop/maximum-stage-kv.aeordb.zst` (269,332 bytes), compressed SHA
`8cdb189af9df965338c2f7134639a8c04f074bdea180187a7e00797db0799377`.
Decompression reproduces raw SHA
`7f6c040f4b26f1dfe71fdccad87441fa306e6c2856a51c92f3aa75ea5bc35699`.
Only the verified redundant raw home/Data copies were removed; contents are
fully recoverable. Data/home free bytes are 280,398,249,984 / 75,409,653,760.
The split full-suite sequence has started; its terminal receipt remains pending.
The broader owner-requested test-database cleanup is still scheduled after
qualification, not complete.

Both unchanged maximum-stage cases pass on Data: clamp at 16:24:09 UTC
(246.39 seconds) and resize rejection at 16:28:40 (249.44 seconds). Each
reports one pass/62 sibling filters from the pinned 63-test target. Their
temporary databases are removed by normal test teardown. The remaining full
workspace run starts at 16:28:40 with only these two exact names excluded.
Its summary helper recognizes the legitimate two-filter parent result, while
counting the three index-store subprocess checks separately; six helper
regressions cover historical/split counts and refusal of unexplained filters,
failed results, truncated targets and missing child results.

### Checkpoint correction: final ordinary qualification

The full remaining workspace suite passes at 17:01:11 UTC: 7,528 top-level
tests across 347 Cargo targets, seven existing ignores, and three separately
counted nested index-store checks. Together with the two unchanged maximum-stage
passes on Data, this is **7,530 distinct top-level tests**, not a skipped-test
waiver. The prior capacity-stopped run remains failed historical evidence.

Strict workspace/all-target Clippy (`-D warnings`) passes at 17:04:12;
contracts at 17:04:42 (454 independent fixtures, 95 routes, 39 docs, debt
8 reviewed entries/164 retained matches); mdBook at 17:05:13; debt self-tests
at 17:05:43; soak helper/16 scenarios at 17:06:13. The complete sequence exits
0 at 17:06:14 with Data/home free bytes 280,368,791,552 / 75,457,970,176.
Fresh formatting, diff checks and the seven-input source manifest pass locally.
Native macOS/Windows source manifests match the same final seven-input digest;
their closed results and runners are collected in the follow-up evidence.

Review confirms the correction changes only qualification checkpoint handling
and diagnostic wording: no storage engine, public command, Cargo/dependency,
database-format or embedded-documentation input changes. Completed malformed
records still fail without mutation; only the incomplete tail is durably
removed under exclusive database ownership before the next worker append.

The closed follow-up seal contains 209 files, including frozen S3 failure
evidence, failed attempts, the lossless capacity archive, final Linux receipts,
both native matrices and exact native input manifests/runners. Its SHA-256 is
`6e25641e4d70228a8e622fed6e748742f7b44e8c96f766204d4d63641bc908a2`
(`closed-followup-evidence.sha256`); every listed digest rechecks successfully.
This closes the ordinary correction landing unit, not the new exact-release
or three 12-hour qualification gates. All step-4 operational boundaries remain
closed, and broad disposable-test-database cleanup is still pending.

### Renewed exact release — a8047327

The checkpoint correction is committed/pushed as
`a804732755b187b3e2bcdd109da37a2895dc9a80`. Clean detached sources and the same
frozen lockfile/portal inputs are prepared on all three hosts. Native builds
start around 17:12 UTC; Windows records Cargo PID 11036 and native MSVC Rust
1.96.0. No Cargo target files cross hosts; each host reuses only its inactive
predecessor release cache and pins normal binaries before any test-feature build.

Desktop campaign is `/media/Data/AeorDB/Tests/p9-release-a8047327-20260911/`;
native campaigns use the same basename under `~/.cache/codex/aeordb-tests/`.
Linux launcher 296910 starts at 17:12:28 with a 200,000-second outer deadline,
per-stage deadlines, 30-second capacity guards and durable terminal receipts.
Runner manifest SHA:
`0f60aa1bbc9cc5c521bfbb36c43db12b3707ef15f9289659d2842848083d6457`.

Sequence: exact release/pinning, byte-preserving copied-S1 verification, pinned
harness fixtures, current live HTTP/restart/readback, explicitly attributed
unchanged-runtime media/resource evidence with new-CLI read-only media verify,
release CLI, 100 complete crash suites, short S1/S2/S3, then three sequential
12-hour stages. Large diagnostic database copies now stay on Data; private
internal workspace TMPDIR and both capacity floors remain unchanged. No extra
full media database pair is created. New release and duration results remain
pending; completion packet and scoped cleanup follow them, then STOP at item 4.

Linux normal release passes at 17:16:59 UTC (4m24s, two jobs). Pinned hashes:
CLI `aef7b082c3b693838a8b8659ff2280d98589ac5cb917376e5ad43e9d2aff5fec`,
soak worker `061d0561b30b3a919c0cb7f8aa5b7dcdc813739bb9857ad7a543ef60ec330520`,
crash worker `59f6b286fe7f8043a74afe251397671f912c90136c1368a0dadbd7a636baa602`.
macOS normal release passes at 17:15:40 (3m04s, one job), pinned CLI
`3837136f5f62a9d637866415d5c4328012829939a7b9da901d3a03cafc350b14`.
Both retain the ordinary optimized/debug-information release profile. macOS
evidence is mirrored to the laptop cache; Windows remains building with C:
above 18.9 GB free at 17:19 UTC. The new Linux live scenario reports success
at 17:18:45 after HTTP/docs/binary payload, clean restart, deletion/missing and
offline verification; its owned units are disposable qualification services.

Windows native MSVC release passes at 17:25:44 UTC (13m29s build, 809.80-second
controller, one job), pinned SHA
`ccadd817214083a04f4cf61f0fa3c7294587002ea03908f09e8a53b932791d5d`.
Minimum sampled/final C: free bytes: 18,761,973,760 / 18,724,057,088. This clears
all three exact native builds for a8047327, without installation or publication.
The Linux unchanged-runtime/media gate is running; later release CLI, crash and
duration stages remain pending. The independent Linux driver is PID 296913.

The new Linux live gate has terminal success at 17:19:13 UTC. The
`unchanged-runtime-media-proof` gate passes at 17:28:15: the mechanical diff
contains only the reviewed qualification correction/progress entry, all prior
normal-release artifact hashes match, the source/destination media hashes match,
and prior full-migration receipts retain identical completion/parity evidence.
The a8047327 CLI strictly verifies the existing separate test source with
Status OK and unchanged checksum/nanosecond stat. Its verification takes
313.42 seconds, maximum RSS 58,576 KiB and zero swap; this is not a controlled
performance comparison. Full migration/resource results remain attributed to
48baeefe, not relabeled as a8047327 execution. Closed media-attribution evidence
is mirrored locally. Final Data/home free bytes at this gate are
279,670,247,424 / 75,187,335,168. Release CLI tests are now running.

Release CLI passes at 17:33:15 UTC: 230 tests/21 targets, zero failures and seven
existing ignores. Crash executable pinning passes at 17:33:46, manifest SHA
`aac9bf5743e1ab4f1bf302748815748034d2b40e4293e8d381624ee3f2008460`.
The complete-suite crash loop is active at 17:47:44; no current duration pass is
claimed. Model checks now use approximately ten-minute intervals. Exact normal
release/media evidence is committed/pushed in documentation checkpoint
`6a4006846cc2aa5fc2e4f460dedf726b3193e53f`; product candidate remains a8047327.

Packet reconciliation has started while the guarded run continues. The canonical
completion/DoD/machine reports now describe a8047327 and mark unfinished gates
pending. Original 535004f1 reports remain recoverable from Git at 6a400684; their
receipts/history are not rewritten. The mutable parent execution banner and
historical ledger-08 notice point to this active ledger. The packet explicitly
distinguishes a v3-compatible service and tested offline v4 shadow substrate
from unavailable public v4 activation/cutover; the remaining limitation is not
merely an ungranted deployment permission. Reports are still provisional until
all current gates, final audit and requested cleanup have terminal evidence.

The provisional packet passes JSON parsing, Markdown file-link resolution,
source-diff hygiene and the unchanged debt gate. A task-local checker compares
native binary identities to closed receipts, verifies test-count arithmetic and
predecessor attribution, and rejects false completion, wrong binary identity,
nested-count inflation, migration reattribution, unavailable activation claims,
unapproved step-4 authority and duration passes without elapsed evidence.
Checker and results are in the durable qualification cache as
`validate-release-packet.mjs` and `validate-release-packet-in-progress-final.log`.
No runtime/source/test/Cargo/embedded-doc bytes differ from a8047327. This is a
truthful in-progress documentation checkpoint, not the final audit/cleanup seal.

### Exact crash and short gates complete; first duration stage active

The a8047327 crash loop passes at 18:37:48 UTC: 100 complete seven-function
suites, 2,300 interruption windows and 100 explicit forced-unmount self-skips.
Forced unmount is not tested. Loop time is 3,830 seconds (3,842-second guarded
stage); terminal status is zero with no guard termination. Log SHA-256:
`4c51e815329f44a652ce5701301f366b8f3cca747d0d872a01d1fa5134b5b42a`.
Pinned normal/crash artifact hashes still match after the loop.

Short S1 passes 18:38:48, S2 18:40:49 and S3 18:42:49 UTC. S3 completes 12
cycles with successful verification and checkpoint comparison, including worker
restarts through the corrected append preparation. The S1 12-hour stage starts
at 18:42:49. At 18:48:01 its live sample records 300 elapsed seconds, 3,089
writes and 1,468 reads. Driver PID 296913 and the capacity guard remain active;
Data/home free bytes are 279,143,149,568 / 74,953,822,208, guard age 11 seconds.
No current 12-hour pass is claimed. All three durations, final audit and scoped
test-database cleanup remain required before the owner's step-4 discussion.
