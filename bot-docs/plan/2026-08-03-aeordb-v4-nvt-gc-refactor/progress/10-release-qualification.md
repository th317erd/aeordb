# Current release qualification — 2026-09-10

## Authorization and entry gate

Owner: Codex, direct implementation under the `implement` workflow.
Entry/last-green source: `3b48de7e42c2d2b6db895774eb8b046eff6d6fa7` on
`development`, matching origin after fetch. Production code is unchanged from
`0b20792b`; the successor fixes a native Windows test fixture and records proof.

The owner explicitly authorized the next three items, then required a stop and
discussion before item 4. This ledger supplements Children 07/08 and the frozen
parent; it does not rewrite historical results or authorize production work.

- [x] 1. Capacity admission and exact native Linux/macOS/Windows release builds.
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
