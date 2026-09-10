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
- [ ] 4. **STOP AND DISCUSS WITH OWNER.** No production deployment, service change,
  canary, installation/public downloads update, cutover, first-write acceptance,
  destructive operational GC, or deletion/reuse of the retained corrupt database.

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
- Windows release build is still running with native MSVC Rust 1.96.0, one job,
  8 GB disk floor and three-hour deadline; no completion is claimed yet.
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

The desktop's `run-linux-sequence.sh` is active (initial PID 3367176), with
`sequence-launch.log`, `evidence/linux-sequence.tsv` and eventual
`evidence/linux-sequence.exit` as its authoritative progress/termination records.
It started media qualification at 16:38:50 UTC and queues release-mode CLI tests,
100 crash-suite passes, short soak preflights and three sequential 12-hour stages.
Every stage has a deadline/free-space guard; a failed stage stops the sequence.
No force-unmount test is enabled (`AEORDB_CRASH_SOAK_TMPFS` is explicitly unset).
Cycle scheduling is seeded; the inherited worker remains wall-clock-seeded,
with actual checkpoint traces retained. Do not claim fully deterministic soaks.

Final packet reconciliation remains open. No production operation or current
long-soak success has been claimed.
