# Child 04: Physical Inventory, Root Lifecycle, GC, and Void

**Parent:** [AeorDB V4, NVT, GC, and Migration Campaign](../../2026-08-03-aeordb-v4-nvt-gc-refactor.md)
**Landing unit:** P4
**Status:** Starts after Child 03 root admission/lifecycle readers are frozen
**Primary owner:** physical-lifecycle/GC owner
**Activation rule:** destructive GC remains disabled until the complete model and resource gates pass

## 1. Outcome

Replace monolithic/unbounded marking and ambiguous Void reuse with a bounded,
resumable, incarnation-aware mark-and-sweep system. Keep logical root retirement
separate from physical reclamation, make incomplete work incapable of advancing
deletion state, and make reusable extents authoritative only through selected
receipt-backed catalogs and durable immutable claims.

The safety direction is always retained data or leaked space. No error,
corruption, cancellation, missing scratch, race, capacity pressure, or restart
may make an entity reclaimable earlier.

## 2. Owned Territory

- physical incarnation inventory and continuation metadata;
- replacement/retirement journal consumption;
- GC run context, task, cancellation, checkpoints, and workspace;
- root lifecycle candidate/retirement/expiry/reclaim-proof state;
- dense mark bitmap, frontier/path/mutation/candidate runs;
- quarantine, sweep proposal, receipts, recovery, and audit;
- Void catalog, immutable claims, settlement receipts, allocator integration;
- repair tickets/path latches created from GC traversal;
- GC CLI/API/task/health/metrics/SSE/Dashboard surfaces; and
- GC/void verify, repair, backup, and migration-reset behavior.

Forbidden without handoff:

- changing root admission/current authority;
- changing frozen format IDs/bodies;
- reclaiming through raw gaps or missing locators;
- loading all KV keys/entities/candidates in memory; and
- enabling production sweep before P8 operator gates.

## 3. Two Separate State Machines

### Logical Root Lifecycle

`live -> retained -> pending_delete -> logically_retired -> physically_reclaimed`

- Live roots are current authority or durably pinned.
- A valid admitted, non-authoritative root is retained until a complete mark
  publishes a candidate.
- Pending freezes time, grace, first generation, authority digest, and admission
  evidence. Its entire namespace/semantic/admission closure remains a mark root.
- A later complete mark may retire only after effective grace and final checks
  for current authority, pins, identity, and unchanged lifecycle evidence.
- Retirement hard-publishes immutable retirement evidence and lifecycle
  authority first. That instant changes future reads to `410`; no locator or
  byte is removed in the transaction.
- Physical marks later omit retired closure. Root-object reclaim proof advances
  optional post-reclaim evidence retention only after receipts plus a newer
  inventory prove no root-object incarnation remains.

Mandatory logically-retired evidence is not subject to the optional 30-day/
256 MiB expiry budget. The 1 GiB lifecycle hard cap blocks new candidate or
retirement publication in deterministic root-hash order; it never evicts
mandatory evidence. Blocked roots remain retained and physically marked.

Initial pending-delete grace is 86,400 seconds. Explicit zero removes only the
elapsed-time delay. Required complete marks is fixed at exactly two and is not
writable configuration. A candidate stores `grace_at_pending`; eligibility is
`pending_since + max(grace_at_pending, current configured grace)`. A later
increase extends existing candidates, while a decrease cannot move them earlier.

Logical retirement runs only while destructive GC and both lifecycle/physical
authority are healthy. Disabling GC leaves former roots retained. Lifecycle
capability activation first hard-publishes a valid empty lifecycle and only then
sets the monotonic capability. After activation, missing/corrupt lifecycle state
disables destructive GC and blocks non-live historical admission with a typed
failure; closure-valid current authority remains readable.

Request and authority admission acquire the shared root-state guard, establish
their pin/intended authority, and recheck lifecycle. Retirement uses the same
guard and final recheck. Whichever linearizes first wins; a retired root is never
pulled back from quarantine.

### Physical Incarnation Lifecycle

`active -> first-unreachable candidate -> confirmed candidate -> sweep proposed
-> locator removed -> Void selected with allocator blocked -> receipt
-> allocator-eligible Void -> optionally claimed`

Every incarnation independently requires two complete marks, frozen/effective
grace, final exact identity/reachability/pin checks, and hard receipt-backed
publication. Logical-key absence, an old offset, or one bad traversal is not a
physical proof.

## 4. Physical Inventory First

1. Enumerate every valid physical entity incarnation, stable locator, WAL
   extent, current/retired replacement relation, owner family, and bounds.
2. Stream and external-sort inventory; never materialize the database in a
   vector/map.
3. Reconcile the replacement/retirement journal and discover missing/ambiguous
   lineage conservatively.
4. Detect overlap, out-of-bounds, malformed headers, duplicate current
   incarnation, and locator identity mismatch as typed evidence.
5. Unknown lineage protects the affected extent/family and creates repair
   evidence. It cannot enter candidate or Void state.
6. Inventory publication is immutable and selected through exact A/B authority.

## 5. Bounded Mark Representation

At run start, flush the KV write buffer and capture stable KV layout generation,
root set, config/capability/SystemFamily fingerprint, physical inventory, and
durable publication boundary.

Physical liveness uses a dense bitmap addressed by captured KV bucket/slot.
The key in the slot remains identity authority. The bitmap reserves its full
memory before start. Expected size is roughly 11.3 MiB per 4 GiB KV layout and
scales with slots, not database payload bytes.

KV scanning, B-tree visitation, frontier, path-sensitive visits, mutation
catch-up, diagnostics, and candidate enumeration are streamed and bounded.
Buffers reserve first, spill into versioned checksummed runs, and compact
incrementally. A layout-generation change invalidates the initial
implementation's run and restarts it.

GC reserves 128 MiB preferred and 64 MiB minimum inside the global process
envelope. Failure to reserve waits or refuses with required/available evidence;
it never swaps implicitly or silently changes representation.

## 6. Online Mutation Convergence

- Writers use Child 03's shared publication guard and global publication
  sequence.
- GC records root/reference/incarnation changes after its captured boundary in
  a bounded spillable run.
- It drains/reconciles in bounded passes while writes continue.
- Final publication takes a short exclusive guard, captures the final boundary,
  drains remaining work, proves no gap, and then publishes.
- Missing/gapped soft acceleration triggers immutable authority diff/restart;
  it never means no changes.
- Convergence starvation publishes diagnostics and no candidate generation.

## 7. Durable Workspace and Resume

Default workspace:

```text
<database-parent>/.<database-filename>-gc-<database-id>-<run-id>/
```

An administrator may configure a different scratch root; database/run identity
subdirectories remain mandatory. Workspaces are private, no-follow, identity-
bound, preflighted for space, and constrained by free reserve and run cap.

Bulk bitmap/frontier/path/mutation/candidate/diagnostic objects live in the
workspace. Small selected checkpoint controls live in the protected database
ControlStore. Immutable bulk objects are synced before the inactive A/B control
slot selects them. The previous checkpoint remains valid until replacement is
verified.

Default checkpoint cadence is five minutes or 1 GiB newly processed logical
work, whichever occurs first, plus bounded graceful-shutdown checkpoint. Missing,
tampered, stale, incompatible, or incomplete scratch abandons the run and
preserves prior complete quarantine. Resumable mode never silently downgrades to
temporary nonresumable work.

Default GC scratch free reserve is:

```text
clamp(max(8 GiB, min(64 GiB, 2% filesystem capacity)),
      1 GiB,
      floor(filesystem capacity / 2))
```

On a filesystem below 2 GiB, the dependent GC operation is unavailable with a
typed capacity diagnostic rather than using an out-of-range default.

## 8. Authoritative and Derived Corruption

GC uses strict structural validation with diagnostic continuation across
independent branches. Any untraversable authoritative branch:

- creates/deduplicates a durable RepairTicket;
- applies the nearest-safe path-scoped read-only latch;
- marks the run incomplete; and
- blocks candidate publication/reclaim until repair and a fresh complete mark.

Healthy scopes remain available. Safe reads of damaged scopes carry explicit
partial/corrupt status.

Corrupt rebuildable derived index generations identify `(index_id, generation)`,
become `needs_rebuild`, and protect that owner family while authoritative
content GC continues if complete. Missing owner metadata conservatively protects
the whole derived type.

## 9. Sweep and Void Authority

### Sweep

1. Select only candidates from a complete later mark after effective grace.
2. Recheck current authority, lifecycle, exact incarnation, locator, pins,
   replacement lineage, and physical bounds under a short exclusive guard.
3. Hard-publish immutable proposal, execute bounded removals, and record per-
   incarnation outcomes.
4. Select the exact receipt-targeted Void catalog while allocator admission
   remains blocked, then hard-publish the commit receipt before any caller may
   allocate from the corresponding extents.
5. Crash recovery validates an unreceipted selected catalog and publishes one
   recovered receipt before allocator admission or another sweep; ambiguity
   retains/leaks the affected extents.

The frozen SweepProposal body formula is
`32 + 2H + count * (24 + 2H)`. P0 fixtures check both hash widths and reject any
reader/writer that uses a different fixed term.

### Void

- Selected Void catalog is allocator authority.
- `VoidClaim` is immutable reservation evidence; presence in selected catalog
  means outstanding.
- Settlement/abandonment omits the claim from a later selected catalog and
  writes an immutable settlement receipt.
- Claims are durably removed from available catalog before overwrite.
- Unused proven subranges may return. Unexplained bytes re-enter physical
  inventory/quarantine.
- Raw gap scans, hot tail, failed candidates, and stale locators are evidence
  only.

## 10. Cancellation, Startup, and One Execution Path

All loops observe the shared cancellation token at bounded intervals. Canceled,
failed, crashed, incomplete, or resource-exhausted runs publish diagnostics but
no new quarantine state.

Startup validates database/physical/run identity, layout generation, captured
roots, fingerprints, watermarks, manifests, and object checksums before resume.
Stale workspaces are inventoried by embedded identity, never filename alone.

CLI, HTTP, scheduled cadence, repair follow-up, and embedded APIs all construct
one `GcRunContext` and invoke one task/state machine. Wrappers may wait or stream
progress; they may not implement their own traversal, memory, cancellation,
checkpoint, or cleanup behavior.

## 11. Migration Interaction

- Source migration lease suspends source mutating GC and retention cleanup.
- Destination copies no mark run, workspace, checkpoint, candidate/grace,
  receipt, audit, Void claim, or corrupt-GC evidence.
- Destination starts `never_marked`, completes two non-destructive marks before
  cutover evidence, and enables no sweep until after production acceptance and
  the later complete-mark-plus-grace boundary.
- Source workspace remains tied to source physical identity for rollback,
  evidence, or later cleanup.

## 12. Landing Sequence

1. P4-1 format readers and physical inventory/reference model.
2. P4-2 replacement/retirement lineage and typed corruption evidence.
3. P4-3 bounded bitmap, streamed visitor, workspace, checkpoint, resume.
4. P4-4 logical lifecycle candidate/retirement/expiry state machine.
5. P4-5 physical quarantine and final guard.
6. P4-6 sweep proposal/receipt/recovery.
7. P4-7 Void catalog/claims/settlement and allocator integration.
8. P4-8 verify/repair/task/API/metrics/SSE/Dashboard and non-destructive soak.
9. P4-9 destructive activation gate remains disabled outside approved rehearsal.

Each unit is green, pushed, and independently revertable before the next starts.

## 13. Verification

Required campaign target:

```bash
timeout 20m cargo test -j 6 -p aeordb --test gc_v4_model_spec
```

Existing inputs:

```bash
timeout 10m cargo test -j 6 -p aeordb --test gc_spec
timeout 10m cargo test -j 6 -p aeordb --test void_manager_spec
timeout 10m cargo test -j 6 -p aeordb --test tree_walker_spec
timeout 10m cargo test -j 6 -p aeordb --test btree_spec
timeout 10m cargo test -j 6 -p aeordb --test corruption_hardening_spec
timeout 10m cargo test -j 6 -p aeordb --test header_repair_spec
timeout 10m cargo test -j 6 -p aeordb --test cleanup_spec
timeout 10m cargo test -j 6 -p aeordb --test gc_http_spec
```

The model interrupts every candidate, checkpoint, lifecycle, locator, proposal,
receipt, Void, claim, settlement, and expiry publication. Allowed outcomes are
readable pending, deterministic retired state, retained bytes, or leaked space.

## 14. Resource and Real-World Proof

- Run with swap disabled under an 8 GiB hard cgroup/job-object and concurrent
  reads, writes, indexing, upload, and maintenance.
- Force tiny spill thresholds, broad/deep/cyclic namespaces, snapshots/forks,
  links, permissions, large chunk populations, and mutation convergence.
- Delete/truncate/reorder/duplicate/tamper scratch objects and remove workspace
  during crash. No candidate state advances.
- Exhaust scratch/free reserve while normal writes continue; GC becomes typed
  incomplete without failing those writes.
- Inject every B-tree corruption class and verify path latch/repair/fresh-mark
  requirements.
- Complete two marks, quarantine, sweep, receipt, Void publication, claim,
  partial use, settlement, restart, and full byte verification on a real
  `/tmp/codex` database.

## 15. Rollback

Disable destructive GC and append allocation only. Retain and interpret already
selected lifecycle/quarantine/Void state with a compatible reader. Never run an
older writer lacking the stored capability. Missing/uncertain state restarts
mark or leaks space; it never rebuilds eligibility optimistically.

## 16. Definition of Done

- [ ] Logical retirement and physical reclaim are separate state machines.
- [ ] Pending roots remain readable and physically marked.
- [ ] Retirement returns deterministic `410` before byte removal.
- [ ] Mandatory retirement evidence cannot be capacity-evicted.
- [ ] Mark memory and scratch are bounded and fully attributed.
- [ ] Incomplete/canceled/corrupt runs advance no deletion state.
- [ ] Authoritative B-tree damage blocks publication and creates repair state.
- [ ] Every physical incarnation has explicit lineage and final guards.
- [ ] Void reuse requires selected receipt-backed authority and durable claim.
- [ ] Crash/corruption outcomes are retained data or leaked space.
- [ ] Destination migration starts `never_marked` with no copied GC state.
- [ ] Real mark-to-reuse cycle reopens and verifies under the hard memory limit.
