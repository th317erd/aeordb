# Current release qualification — 2026-09-10

## Authorization and entry gate

Owner: Codex, direct implementation under the `implement` workflow.
Entry/last-green source: `3b48de7e42c2d2b6db895774eb8b046eff6d6fa7` on
`development`, matching origin after fetch. Production code is unchanged from
`0b20792b`; the successor fixes a native Windows test fixture and records proof.

The owner explicitly authorized the next three items, then required a stop and
discussion before item 4. This ledger supplements Children 07/08 and the frozen
parent; it does not rewrite historical results or authorize production work.

- [ ] 1. Capacity admission and exact native Linux/macOS/Windows release builds.
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
