# V3-to-V4 Migration and Cutover

AeorDB contains a qualified v4 storage, root, GC, native-index, query, and
side-by-side migration substrate. The currently shipped service still selects
the v3 compatibility runtime for ordinary database opens, reads, writes,
queries, and garbage collection.

> **There is currently no public `aeordb migrate`, `aeordb cutover`, HTTP
> migration route, or automatic v3-to-v4 upgrade.** Do not call internal Rust
> migration modules, edit migration controls, rename database artifacts by
> hand, or treat a normal server restart as a v4 migration.

This page documents the release boundary and the operator protocol that the
staged migration implementation enforces. It is not authorization to run a
copied-production rehearsal, canary, installation, deployment, operational
cutover, first v4 write, or destructive v4 garbage collection.

## Status at a Glance

| Surface | Current status |
| --- | --- |
| Existing `.aeordb` service authority | V3 compatibility runtime |
| V3 readers and recovery | Required and retained |
| V4 formats, bounded readers/writers, roots, lifecycle, GC, native indexes, and query implementation | Implemented and independently tested |
| Side-by-side clone, capture, reconciliation, verification, cutover journal, rollback, and crash recovery | Implemented as an internal/rehearsal substrate |
| Public migration CLI or HTTP API | Not available |
| Automatic v3-to-v4 migration on startup | Disabled |
| Copied-production rehearsal and canary | Require explicit environment/data authorization |
| Operator acceptance and first v4 write | Not performed by ordinary startup or deployment |
| Destructive v4 GC | Not authorized by migration preparation or read-only validation |

The inactive v4 index-runtime state shown by
[`GET /system/stats`](./observability.md#index-runtime) is expected for an
ordinary v3 service. It is not evidence that a migration failed, and it does
not mean the server is silently serving v4 indexes.

## Three Different Kinds of “Migration”

Do not confuse these independent operations:

1. **Legacy single-file layout compatibility.** Current startup can recognize
   older v3 KV layouts and converge them to the current in-file v3 layout. This
   is not a v4 format cutover.
2. **FileRecord payload backfill.** A forced reindex can rewrite older live
   FileRecord payloads through the current writer and populate fields such as
   whole-file `content_hash`. See [Reindexing](./reindex.md). This also is not a
   v4 database migration.
3. **V3-to-v4 side-by-side migration.** The staged campaign builds a separate
   destination, captures concurrent source changes, reconciles exact authority,
   verifies closure, and changes the service-path identity only through a
   journaled cutover. This operation is not currently exposed to operators.

`aeordb deployment-check` is likewise a binary-replacement safety probe. Its
`aeordb.v3-transition-recovery.v1` capability means the candidate understands
v3 durability/recovery transition state; it does not activate or accept v4.

## Safety Invariants

Any future rehearsal or operational cutover must preserve all of these
invariants:

- Preserve the source and evidence originals. Develop, repair, and rehearse
  only against a separately identified copy.
- Record the source database checksum, stable file identity, size, selected
  root, effective configuration fingerprint, SystemFamily registry
  fingerprint, binary digest, and toolchain before work begins.
- Put source, destination, backup, and cutover journal in one absolute,
  same-parent filesystem layout. Cross-filesystem rename/copy is not accepted
  as an atomic cutover substitute.
- Keep clone, capture, reconciliation, and cutover workspaces private and
  bounded. Workspace files are resumable evidence, not service authority.
- Keep the source readable and preserve its exact bytes until operator
  acceptance. Migration must never “repair” its source while cloning it.
- Suspend source GC only through the migration-owned interlock. Do not hold a
  multi-terabyte KV snapshot or infer completeness from process memory.
- Validate every destination object, root mapping, protected SystemFamily,
  capability, and physical identity before namespace installation.
- Enter read-only validation before acceptance. The first accepted v4 write is
  a distinct, recorded boundary.
- Crash recovery must move only to an explicitly allowed predecessor or
  successor state. A valid but unapproved journal successor fails closed.
- Delay destructive v4 GC until the cutover has been accepted, monitored, and
  separately authorized. Retained v3 backup evidence is not garbage.

## Preparing an Authorized Rehearsal

The public CLI cannot start the migration. When an approved orchestration tool
is supplied, prepare its inputs as follows:

1. Stop or snapshot through a supported mechanism before making a byte copy.
   Do not assume that copying a live, mutating database file produces a
   coherent rehearsal source.
2. Keep the original untouched. Compute and retain a checksum for both the
   original and the rehearsal copy, and record how the copy was produced.
3. Run read-only verification against the copy and retain the complete report.
   Do not use `--repair` merely to make preflight pass.
4. Record the exact candidate binary SHA-256 and confirm the candidate was
   built from the intended commit with complete embedded documentation.
5. Confirm storage for the destination, source backup, cutover journal,
   capture/reconciliation workspaces, logs, and verification artifacts. A
   destination is not allowed to consume the only preserved source copy.
6. Freeze the effective runtime/lifecycle configuration and all registered
   migration limits. Configuration drift requires a new preflight.
7. Define the explicit cancellation, rollback, read-only validation,
   acceptance, monitoring, and destructive-GC owners before execution.

Use [Backup & Restore](./backup.md) for logical backup workflows and
[`aeordb verify`](../cli/commands.md#aeordb-verify) for read-only integrity
evidence. Neither command replaces the side-by-side migration protocol.

## Staged Cutover Sequence

The internal state machine is designed around these stop-capable phases:

1. **Preflight:** bind the source path/identity/root, destination path, hash
   profile, capability set, configuration, registry, workspace limits, and
   cancellation authority before mutation.
2. **Base clone:** stream source authority into a separate v4 destination with
   bounded traversal and destination batches. The source remains the service.
3. **Capture:** durably record post-boundary source mutations. Losing optional
   capture forces full reconciliation; it never converts an acknowledged
   source write into a false failure.
4. **Final reconciliation:** freeze the exact final source root, replay and
   reconcile capture, publish the final root map, and verify the complete
   destination against the frozen source authority.
5. **Read-only cutover validation:** journal each same-filesystem namespace
   transition, preserve the exact source at the backup identity, install the
   verified destination at the service identity, reopen read-only, and run
   real clients plus integrity checks.
6. **Operator acceptance:** record an explicit acceptance decision only after
   the read-only window and evidence review. Ordinary health/readiness is not
   acceptance.
7. **First v4 write:** cross a separate recorded barrier. After this point an
   old v3-only binary is not a valid state rollback mechanism.
8. **Monitoring and retirement:** retain the v3 backup and migration evidence
   through the approved monitoring window. Destructive v4 GC and backup
   retirement remain separate actions.

## Rollback Boundaries

### Before Operator Acceptance

The cutover journal can recover backward to the exact v3 service bytes or
forward to the verified v4 destination only when the recorded identities and
checksums agree. A pre-acceptance rollback restores v3 at the service path and
preserves the v4 destination for diagnosis. It does not rewrite either format.

### After Acceptance but Before the First V4 Write

Stop and inspect the exact acceptance, journal, service identity, and
destination verification evidence. Do not infer that binary rollback is safe
from a healthy process exit. The operational runbook must explicitly authorize
the selected state transition.

### After the First V4 Write

Treat v4 as persisted authority. Replacing the process with a v3-only binary
cannot interpret the new state and is not rollback. Recovery must use a
v4-capable binary and the recorded v4 state machine, or restore the separately
preserved v3 backup as a data rollback under its own approval and loss window.

## Evidence Required for Each Gate

Retain, outside the repository and outside the database being exercised:

- source commit, Cargo lock digest, candidate binary digest, OS/filesystem,
  toolchain, and effective configuration sources;
- original/copy/source/destination/backup checksums and stable file identities;
- preflight, clone, capture, reconciliation, verification, journal, reopen,
  client, load, cancellation, and crash-recovery logs;
- exact start/end timestamps, command deadlines, seeds, peak memory, disk use,
  and swap evidence;
- the operator who approved the copy, canary, cutover, acceptance, first write,
  monitoring completion, and destructive GC boundaries; and
- every rejected attempt or divergence. Never delete a truthful failing report
  merely because a later retry passed.

## What Operators Can Safely Do Today

- Use checked binary replacement and read-only database inspection as described
  in [Deployment Safety](./deployment-safety.md).
- Capture bounded live state with `aeordb status --json` and preserve incident
  evidence before repair or restart.
- Create logical exports and independently checksummed offline copies.
- Run `aeordb verify` read-only against a disposable or authorized copy.
- Run current v3 GC only under the current [GC runbook](./gc.md); do not treat
  it as v4 lifecycle retirement.
- Wait for a versioned, documented orchestration command before attempting a
  v3-to-v4 migration.

## See Also

- [Deployment Safety](./deployment-safety.md) — checked binary replacement and downgrade refusal
- [Backup & Restore](./backup.md) — logical copies, import, and promotion
- [Observability](./observability.md) — runtime, durability, recovery, and inactive v4 index state
- [Garbage Collection](./gc.md) — current service GC and destructive boundaries
- [Storage Engine](../concepts/storage-engine.md) — active v3 storage and the staged v4 target
- [Indexing & Queries](../concepts/indexing.md) — active whole-index compatibility and v4 native artifacts
