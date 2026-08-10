# Child 07: Side-by-Side Migration, Cutover, and Rollout

**Parent:** [AeorDB V4, NVT, GC, and Migration Campaign](../../2026-08-03-aeordb-v4-nvt-gc-refactor.md)
**Landing units:** P3c and P8
**Status:** P3c starts after Child 03 shadow roots; P8 requires Children 04 and 06
**Primary owner:** migration/cutover/operations owner
**Production rule:** no FS-Server1 mutation before explicit P8 operator approval

## 1. Outcome

Migrate a live v3 database into a separate v4 physical file while preserving
the v3 source byte-for-byte, capturing or reconciling every authoritative
producer, validating behavior against independent oracles, and cutting over
through a crash-safe same-host journal with an explicit operator-controlled v4
write boundary.

Before the first acknowledged v4 write, rollback restores service to untouched
v3. After that boundary, v3 is evidence/backup rather than current authority;
recovery moves v4 forward or latches it read-only. No reverse journal is implied.

## 2. Owned Territory

- migration preflight, lease/fence/progress/control state;
- source evidence identity and destination physical identity;
- shadow clone, bounded capture, checkpoints, final reconciliation;
- legacy v3-root to v4-root map and explicit unavailable/reset cases;
- cutover control and external `cutover.acut` A/B journal;
- source/destination rename, reopen, verify, and read-only validation;
- operator acceptance and write-boundary announcement;
- copied-production rehearsal, canary, production monitoring, rollback/recovery;
- release build/install/deploy/package orchestration for Linux/macOS/Windows; and
- migration CLI/API/SDK/Dashboard/health/status operation ledger.

Forbidden without handoff:

- altering v3 source bytes during copy/rehearsal;
- repairing the source in place;
- guessing mappings for unknown v3 roots;
- bypassing capability/SystemFamily/durability/memory/GC gates;
- production cutover from an uncommitted or unverified binary; and
- storing evidence databases or secrets in Git.

## 3. Identity and Rollback Boundaries

- Preserve `logical_database_id` across migration.
- Destination v4 receives a new random nonzero `physical_instance_id` in both
  selected header slots.
- V3 source receives a random migration evidence ID stored only in migration/
  cutover controls; v3 is not modified to add one.
- Stable same-host file identity combines platform file identity, selected
  header bytes, format, logical/physical identity, expected size, and durable
  sequence. Unsupported stable identity blocks online cutover.
- The complete 56-byte platform descriptor is retained as exact evidence. Its
  same-physical-file comparison is platform-specific: Unix schema 1 includes
  available birth evidence to resist inode reuse; Windows schema 1 follows the
  documented volume-serial plus 128-bit file-ID key because `ReplaceFileW`
  preserves the old destination creation time while retaining the replacement
  file ID. Recompute and record the complete descriptor after every reopen,
  rename, and replacement boundary.
- Byte-for-byte copies retain physical identity and are diagnostic/read-only
  until explicit adoption publishes a new physical ID and writer fence.
- V3 rollback is lossless only before any v4 write acknowledgement. The exact
  first acknowledgement is durably recorded and prominently exposed.

## 4. P3c: Shadow Migration Substrate

### Preflight

Capture and verify:

- source path, checksum, size, selected header, filesystem/file identity;
- source strict verify and unresolved repair/latch/spill/path-latch state;
- source protected families, modules, snapshots/forks, symlinks, history, peers,
  sync state, tasks, plugins, and root provenance;
- destination/workspace/backup/capture/free-reserve capacity separately;
- platform durability/cutover capabilities;
- source and destination memory budgets; and
- exact binary commit/digest and capability registry.

Any authoritative corruption, unsupported required durability, insufficient
reserve, active incompatible repair, or ambiguous identity stops before source
mutation or destination activation.

### Lease and Source GC

Acquire a durable fenced migration lease. Suspend source mutating GC and
retention cleanup for the lease duration. Lease success/cancel/verified rollback
releases suspension. A stale lease is recovered explicitly; restart does not
silently clear it.

### Base Clone

Stream v3 authoritative state into a separate v4 file under bounded memory and
disk buffers. Preserve exact content/chunks, namespace history required by
retained roots/snapshots/forks, symlinks, semantic/config closure, and protected
families according to SystemFamily policy. Build v4 physical identity and
controls through Children 01-03 services.

### Capture and Reconciliation

Every authoritative producer participates: files, directories, symlinks,
deletes, auth/system/config/semantic state, snapshots/forks, sync bases/peer
state by policy, tasks/pins, conflicts, plugins, restore/promote, repair/import,
and maintenance controls.

Capture is bounded disk state with defaults:

- maximum 64 GiB;
- free reserve `clamp(max(16 GiB, min(128 GiB, 5% capacity)), 1 GiB,
  floor(capacity/2))`; and
- checkpoint every 300 seconds.

Capture exhaustion never fails or rolls back an acknowledged source write. It
hard-records `needs_full_reconcile`, stops optional capture, and requires a
complete authority/SystemFamily diff during final freeze or aborts the shadow
migration. Critical destination/workspace pressure cleans only identified
shadow work and preserves source authority/evidence.

### Legacy Root Map

Map every verified current/former v3 root that migration preserves to its exact
v4 root. Root map pages are bounded, checksummed, linked, and selected by a
control. Unknown externally held v3 hashes receive typed reset/unavailable;
migration never guesses or treats an arbitrary directory hash as `/`.

### P3c Exit

Destination is shadow-only and refuses service write admission/cutover. It
passes full verify and read-model comparison for copied state. Source checksum,
size, header, and current service remain unchanged.

## 5. Migration State Machine

Permanent phases:

```text
preflight -> copy -> reconcile -> final_freeze -> destination_verify ->
cutover -> read_only_validation -> operator_acceptance
```

Every transition is fenced, monotonic, A/B selected, and records source/
destination header sequences, capture/reconciliation watermarks, family/root
digests, effective config, progress, ETA, and stable error evidence.

Crash recovery validates lease, physical identities, external cutover journal,
database controls, paths, selected headers, sizes, and durable sequences before
choosing a step. Ambiguous state latches and prompts an exact recovery command;
it never guesses which file is current.

## 6. P8 Pre-Cutover Rehearsal

### Copied-Production Proof

1. Take/receive a separately checksummed copy; never open production as the
   migration target.
2. Run strict source verify and preserve findings.
3. Clone to v4 while replaying representative concurrent mutation traces.
4. Build v1 indexes against captured roots and reconcile all families.
5. Complete two non-destructive v4 marks; keep sweep disabled.
6. Compare P0 behavior/operation ledgers, root maps, content/chunk digests,
   query/list/search/fetch/range output, auth, backup, sync, and metrics.
7. Crash at every lease/capture/checkpoint/header/control/rename/reopen boundary.
8. Rehearse pre-write rollback and post-boundary read-only forward recovery.
9. Record time, peak RSS, scratch/capture/destination/backup capacity, I/O,
   health latency, and expected production window.

Any unexplained mismatch or resource breach stops the campaign.

### Canary

Run the same committed native release candidate with real clients and load on a
non-production/canary copy or separately approved canary instance. Verify client
APOS/root schemas, upload/blob latency, async coverage, health responsiveness,
dirty restart, and operator recovery commands.

## 7. Cutover Algorithm

1. Confirm backup, destination full verify, root/family reconciliation, native
   release matrix, capacity, maintenance approval, and no active source repair.
2. Acquire final source write freeze and drain admitted writes.
3. Capture final source authority/header/publication sequence.
4. Reconcile exact namespace, semantic, protected-family, snapshot/fork, and
   approved persisted-root state; a journal gap cannot be an empty delta.
5. Full-verify destination and write destination-verified state to database
   control and external A/B cutover journal.
6. Hard-sync source/destination files and parent directory as required.
7. Rename source to the uniquely identified v3 backup and install destination
   at service path, updating both journals after each durable boundary.
8. Reopen with the exact candidate binary and verify header physical identity,
   capabilities, roots, controls, and storage.
9. Serve a bounded read-only validation window. Run real clients and comparison
   probes while v3 remains lossless rollback.
10. Obtain explicit operator acceptance.
11. Enable v4 writes, hard-record and announce the rollback boundary.
12. Monitor continuously. Do not attempt v3 rename-back after this point.

The external `cutover.acut` has two 1,024-byte CRC-protected slots and mirrors
the selected database cutover body. Database and external evidence are both
validated; disagreement is a recovery stop, not a precedence guess.

## 8. Stop and Recovery Conditions

Before v4 write acceptance, any condition below restores service to untouched
v3 after verified rollback:

- source/destination identity mismatch;
- unexplained behavior/data/root/family divergence;
- failed durability barrier/read-back/rename;
- corrupt/ambiguous header, control, tree, or cutover journal;
- missing required module/family/capability;
- memory/disk reserve breach or health starvation;
- incomplete final reconciliation; or
- client/API incompatibility.

After acknowledged v4 writes, the same conditions latch v4 read-only and invoke
forward repair/recovery. V3 remains evidence and is never silently promoted as
current. A separately proven reverse journal is future scope.

## 9. Post-Cutover Operation

Monitor:

- durability frontier/latch/spill and shutdown drain;
- RSS/owner budgets/evictions/unaccounted memory;
- health latency and readiness;
- upload throughput, staged chunks, blob queue/commit latency;
- soft mutation lag, coverage, index workers/cache;
- query/root/APOS correctness and fallback;
- root lifecycle, physical inventory, mark/checkpoint/quarantine;
- Void catalog/claims/reusable bytes and disk amplification;
- repair/path latches and B-tree corruption; and
- migration/cutover state and backup identity.

Only after stable accepted v4 operation run the first production complete mark.
Wait for a later complete mark plus effective grace before any production sweep.
Destructive activation is an independent operator action.

## 10. Native Release, Install, and Deploy

- Build/test Linux with `-j 6` or less.
- Build/test native macOS on `wyatt-mac` and native Windows on `win11vm` from the
  exact committed source and lockfile.
- All platforms run fixture, capability, durability, and cutover suites.
- Record binary hashes, versions, source commit, platform, commands, and logs.
- Install the Linux candidate to `~/.local/bin/aeordb` through the checked local
  install script whenever deployment is requested.
- Deploy through the checked service script that backs up binary/config,
  validates transition controls, drains/restarts safely, waits for full ready,
  and records health/identity.
- Copy public release artifacts to the approved downloads location only after
  the complete release gate.

## 11. Landing Sequence

1. P3c-1 preflight, identity, lease, source-GC suspension, and progress controls.
2. P3c-2 bounded base clone, capture, checkpoint, and final-reconcile model.
3. P3c-3 root map, shadow verification, cancel/resume, and source invariance.
4. P8-1 synthetic and copied-production fault rehearsal.
5. P8-2 native release qualification and canary under real clients.
6. P8-3 production preflight and read-only cutover validation.
7. P8-4 explicit operator acceptance and first-v4-write boundary.
8. P8-5 post-cutover monitoring and delayed GC activation evidence.

Each unit is green, pushed, and separately recoverable before the next begins.
The production units require explicit operator authorization in addition to
their technical start gates.

## 12. Verification

Required campaign target:

```bash
timeout 30m cargo test -j 6 -p aeordb-cli --test cutover_fault_spec
```

Existing inputs include:

```bash
timeout 10m cargo test -j 6 -p aeordb --test backup_export_spec
timeout 10m cargo test -j 6 -p aeordb --test backup_import_spec
timeout 10m cargo test -j 6 -p aeordb --test backup_diff_spec
timeout 10m cargo test -j 6 -p aeordb --test versioning_spec
timeout 10m cargo test -j 6 -p aeordb --test cross_restart_spec
timeout 10m cargo test -j 6 -p aeordb --test cluster_join_spec
timeout 10m cargo test -j 6 -p aeordb --test sync_engine_spec
timeout 10m cargo test -j 6 -p aeordb --test resilience_features_spec
timeout 10m cargo test -j 6 -p aeordb-cli --test crash_inject_spec
timeout 10m cargo test -j 6 -p aeordb-cli --test probe_spec
```

The fault matrix interrupts every preflight, lease, base copy, capture append,
checkpoint, reconciliation, root-map, destination verify, journal slot, sync,
rename, reopen, validation, acceptance, and first-write boundary.

## 13. Evidence Packet

P8 cannot proceed without:

- source and backup identity/checksum report;
- capacity/free-reserve calculation;
- mutation-producer reconciliation ledger;
- root/family/content/chunk comparison;
- behavior and intended-divergence report;
- native platform qualification report;
- cutover crash-state report;
- canary report;
- exact rollback/forward-recovery commands; and
- signed/recorded operator acceptance and first-v4-write boundary.

Evidence databases and credentials remain outside Git.

## 14. Definition of Done

- [ ] V3 source is unchanged before cutover and retained as identified backup.
- [ ] Destination has distinct physical identity and preserved logical identity.
- [ ] Every authoritative producer is captured or final-reconciled.
- [ ] Capture exhaustion never fails source writes or hides a gap.
- [ ] Source GC suspension is durable and released correctly.
- [ ] Unknown v3 roots are reset/unavailable, never guessed.
- [ ] Copied-production migration and all crash states pass.
- [ ] Native Linux/macOS/Windows candidates share fixtures/capabilities.
- [ ] Read-only validation and explicit operator acceptance precede v4 writes.
- [ ] V3 rollback boundary is durably recorded and honored.
- [ ] Health/resource/query/index/GC/void monitoring remains stable after cutover.
- [ ] No production sweep runs before later complete mark plus grace.
