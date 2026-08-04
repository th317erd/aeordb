# Child 03: Namespace, Semantic Roots, and System Families

**Parent:** [AeorDB V4, NVT, GC, and Migration Campaign](../../2026-08-03-aeordb-v4-nvt-gc-refactor.md)
**Landing units:** P2c, P2d, P2e, and P3b
**Status:** Starts after Children 01 and 02 are green
**Primary owner:** namespace/semantic authority owner
**Handoff:** selected-root and lifecycle reader APIs must be frozen before Children 04 and 06 activate

## 1. Outcome

Create one typed classification of every persisted family, one mutation and
locator-replacement path for every writer, one immutable v4 namespace/semantic
root authority, and one `ResolvedReadView` for every namespace read. Remove
parallel HTTP/SDK/plugin/task implementations without changing current v3 bytes
during the P2 waves.

This child owns what data is authoritative and how a root is admitted. Child 04
owns when a non-authoritative root retires and when physical incarnations may be
reclaimed.

## 2. Owned Territory

- `directory_ops.rs`, directory/B-tree namespace publication, and HEAD updates;
- system-family classification and protected traversal;
- file, directory, symlink, delete, copy, rename, merge, restore, promote, and
  repair namespace mutations;
- blob commit, embedded batch, sync, plugin, version, backup/import, and
  maintenance adapters into the same mutation service;
- locator replacement/retirement journal calls;
- root admission prepare/commit and immutable namespace/semantic roots;
- semantic definitions, parser/dependency selection, and content-only state;
- selected-root resolution and authorization context;
- backup/sync/migration closure classification; and
- operation ID, event, metric, and soft mutation emission after authority.

Forbidden without handoff:

- physical GC eligibility, sweep, and Void allocation;
- index page/NVT codecs or query planning;
- route response schema changes owned by Child 06;
- cutover/rename state machine owned by Child 07; and
- format registry edits owned by Child 01.

## 3. One SystemFamily Registry

The selected binary-owned registry replaces scattered protected-path folklore.
Every family descriptor freezes matcher, semantic role, GC policy, logical
backup policy, peer policy, data/client visibility, IndexPolicy, required
capability, transfer behavior, malformed behavior, and owner.

Required live corrections include:

| ID | Family | Required behavior |
| ---: | --- | --- |
| `0x0019` | exact and descendant `.aeordb-permissions` | user-visible and index-visible under normal path authorization; malformed state fails affected auth closed |
| `0x001A` | `/.aeordb-conflicts/` | included in logical backup/migration; omitted from peer replication and generic data APIs; exposed by typed conflict APIs |
| `0x0043` | `/.aeordb-system/controls/v1/` | protected, traversed, backed up/migrated by policy, never generically mutable |

`IndexPolicy` is exactly one of: not applicable, include under ordinary scope,
exclude from all indexes, or canonical projection only. Unmatched ordinary user
data is included. Permissions preserve ordinary index visibility. Operational
controls, credentials, GC, conflicts, and logs are excluded. Parser/index config
is exposed only through canonical projections.

P2 retains the old path predicate only as a named v0 migration evaluator. New
code asks the registry. P0's architecture gate must map every protected literal,
EntryType, KV domain, control kind, external workspace, and ordinary-user-data
case to exactly one policy.

## 4. P2c: Strict Traversal and Failure Boundaries

1. Introduce `SystemFamilyRegistry` behind current behavior and characterize
   every consumer before replacing lists.
2. Route backup, sync, peer, import, index scope, GC traversal, permissions,
   plugin host, repair, verify, and generic APIs through one classification.
3. Make unknown protected families typed failures. Generic traversal cannot
   reinterpret them as user data or silently skip them.
4. Apply family-specific malformed behavior: authorization fails the affected
   access closed; authoritative traversal creates repair evidence and blocks
   destructive conclusions; rebuildable derived state degrades/rebuilds.
5. Add strict B-tree/path traversal results that distinguish complete,
   diagnostically partial, and corrupt. Callers may not convert partial into an
   empty/complete collection.
6. Preserve existing behavior outside incorporated family contracts and record
   any intended divergence in the campaign ledger.

## 5. P2d: Shared Mutation and Locator Services

### `NamespaceMutationCoordinator`

Owns namespace serialization, dependency discovery, operation identity, root
construction, authority publication, root admission, current counters, and
post-commit fanout. Current global namespace serialization remains until a
separate lock-order proof supports sharding.

### `LocatorReplacementCoordinator`

Owns every stable-key replacement. It records old/new physical incarnation,
dependency order, authority publication, retirement evidence, and recovery
state. Absence from KV or a raw byte gap never proves an old incarnation free.

### Post-Commit Fanout

After durable authority commits, emit one typed soft mutation containing the
global publication sequence and exact source identities, then fan out cache
invalidation, authorized SSE, index wakeup, and diagnostics. Loss/gaps are
reconstructed by immutable root/SystemFamily diff or rebuild; user success
never waits for this soft append.

Every successful mutation has one operation ID and one acknowledgement event/
metric. Wrappers do not issue their own HEAD, counters, events, or index writes.

## 6. P2e: Producer Migration Waves

Each wave starts with operation-ledger characterization, routes through the
shared services, turns existing/new tests green, and deletes the bypass before
the next wave.

### Wave 1: Core DirectoryOps

- file and directory create/replace/delete;
- symlink create/delete and resolution-affecting updates;
- parent propagation, HEAD, path/content/identity locators; and
- FileRecord version migration and directory rebuild.

### Wave 2: Blob and Embedded Batch

- streamed PUT/finalize;
- existing-chunk blob commit;
- buffered batch small-file store and JSON merge batch; and
- copy/rename/merge and multi-file atomic publication semantics.

### Wave 3: Version, Backup, and Sync

- snapshot restore, fork promotion, version restore;
- backup import/promote and sync apply/conflict resolution; and
- whole-HEAD transitions via bounded reconciliation markers rather than an
  unbounded per-file transaction.

### Wave 4: System and Plugin

- users, groups, keys, permissions, shares, config, plugins, peers, conflicts,
  tasks, cron, and ControlStore adapters; and
- typed recursion suppression for ControlStore detail records, never a string
  path exception.

### Wave 5: Maintenance and Repair

- reindex metadata resave, repair, cleanup, GC follow-up, import, and migration
  source capture; and
- exact partial/corrupt outcomes rather than empty success.

### Wave Exit

Architecture tests prove no direct HEAD/root/control/path-list mutation, raw
stable-key replacement, or separate event/metric acknowledgement remains.

## 7. P3b: Immutable V4 Namespace and Semantic Authority

### Namespace Root

`NamespaceRootV1` selects one immutable namespace tree plus one semantic state,
registry/capability identity, and authority evidence. Internal directory/page
hashes cannot be submitted as namespace roots unless they have a valid committed
admission witness.

`DirectoryIndexV1` and directory entries preserve canonical child identity and
enough metadata for strict continuation. Namespace content remains authoritative;
derived indexes never enter this closure as answer authority.

### Semantic State

The root binds either:

- a complete immutable semantic state containing exact parser, plugin,
  definition, config, SystemFamily, and dependency identities; or
- a content-only state with a stable reason when historical semantics cannot be
  proven.

Historical reads never consult mutable current aliases. A content-only root may
serve exact content/metadata operations but returns the frozen typed semantic
unavailability for operations that require absent definitions.

### Root Admission

1. Build and durably publish every dependency.
2. Write immutable root admission prepare/evidence.
3. Under the root-state guard, recheck capability, authority, and lifecycle.
4. Hard-publish the authority selector and required first-admission commit.
5. Only then expose the root and append soft mutation/event/index work.

`RootAdmissionCommit` is one collectable immutable control per distinct admitted
root and remains rooted with that root closure until logical retirement and
physical quarantine. P0 measures its directory/write amplification.

### Root States

The common reader distinguishes current, retained, pending-delete,
logically-retired, physically-reclaimed, unknown/unadmitted, corrupt lifecycle,
and unavailable semantic state. Pending remains readable. Retired/reclaimed
returns deterministic `410`. Missing lifecycle authority never guesses.

## 8. One `ResolvedReadView`

Every namespace read resolves through one service that captures:

- logical database and selected physical instance;
- explicit `root_hash` or one captured current HEAD;
- admitted root and lifecycle state;
- semantic state and capability/SystemFamily identities;
- current authorization plus selected-root restrictive permissions;
- request/task pins and cancellation;
- concealment policy; and
- root response metadata including advisory expiry.

Current auth is checked before historical existence/state. Selected descendant
permissions may restrict current grants but never expand them. A normal share
credential cannot submit a non-live explicit root. Raw chunk/directory/internal
hash knowledge is not read authority.

Child 03 exposes the service API. Child 06 converts all routes and response
schemas to use it.

## 9. Transfer and Maintenance Closure

- Logical backup and migration include authoritative namespace, semantic,
  protected-family, snapshot/fork, symlink, and required historical closure.
- Peer policy follows the registry; conflicts do not replicate as foreign
  conflict authority.
- Derived indexes are omitted/rebuilt by default and never make logical backup
  incomplete.
- Verify and repair traverse strict typed closures and report family/root scope.
- GC receives admitted roots and exact typed edges; it does not rediscover
  protected paths independently.
- Migration maps only verified v3 roots; unknown external hashes return typed
  reset/unavailable rather than guessed mapping.

## 10. Landing Sequence

1. P2c-1 registry model and completeness architecture tests.
2. P2c-2 consumer conversion and duplicate-list deletion.
3. P2d-1 mutation/locator facade around current v3 behavior.
4. P2e waves 1 through 5, each green and pushed.
5. P3b-1 immutable root/semantic readers and reference model.
6. P3b-2 shadow root/admission writer through Child 01/02 services.
7. P3b-3 read-view resolver, lifecycle state, pins, transfer closure.

The hotspot owner records a handoff commit between every unit that changes
`directory_ops.rs`, `storage_engine.rs`, or common route/service wiring.

## 11. Verification

Required P3 campaign target:

```bash
timeout 15m cargo test -j 6 -p aeordb --test v4_root_migration_spec
```

Affected existing regressions include:

```bash
timeout 10m cargo test -j 6 -p aeordb --test directory_ops_spec
timeout 10m cargo test -j 6 -p aeordb --test sdk_bulk_write_spec
timeout 10m cargo test -j 6 -p aeordb --test version_access_spec
timeout 10m cargo test -j 6 -p aeordb --test backup_import_spec
timeout 10m cargo test -j 6 -p aeordb --test sync_apply_spec
timeout 10m cargo test -j 6 -p aeordb --test permissions_spec
timeout 10m cargo test -j 6 -p aeordb --test conflict_store_spec
timeout 10m cargo test -j 6 -p aeordb --test system_store_spec
timeout 10m cargo test -j 6 -p aeordb --test event_emission_spec
timeout 10m cargo test -j 6 -p aeordb --test tree_walker_spec
```

Model/fault tests interrupt every dependency, prepare, admission, selector,
read-view pin, locator replacement, and fanout boundary. Reopen must select old,
new, or typed recoverable state, never an orphan root or mixed namespace.

## 12. Real-World Proof

Run a real server and embedded client below `/tmp/codex` with concurrent file,
directory, symlink, blob, batch, merge, sync, plugin, restore, promote, repair,
and config operations. After seeded kills:

- strict verify reports no orphan admission, locator divergence, lost child, or
  mixed semantic state;
- current and historical authorized reads match the independent root model;
- unauthorized probes reveal no root existence/state;
- every operation has one event and metric acknowledgement; and
- soft mutation deletion/gaps do not lose acknowledged data or produce false
  complete index coverage.

## 13. Rollback

- P2 waves preserve v3 bytes and revert by landing unit, subject to Child 02's
  persistent safety-control downgrade gate.
- P3 shadow roots remain unselected until activation. Discard only the identified
  shadow destination/workspace; never alter source v3.
- Once a v4 capability/authority is selected, rollback requires a compatible v4
  reader and state-machine recovery, not an older writer.

## 14. Definition of Done

- [ ] One selected SystemFamily registry governs every consumer.
- [ ] Permissions, conflicts, and controls have the ratified policies.
- [ ] Unknown/malformed protected state fails at the approved scope.
- [ ] Every producer uses one namespace mutation and locator path.
- [ ] Every successful mutation has one operation/event/metric acknowledgement.
- [ ] Soft mutation loss reconstructs or rebuilds; it never falsifies authority.
- [ ] V4 roots bind immutable namespace and exact semantic/content-only state.
- [ ] Root admission has one crash-safe linearization point.
- [ ] Every namespace reader can consume one `ResolvedReadView`.
- [ ] Current authorization precedes historical observables.
- [ ] Transfer, GC, verify, and repair use typed closure policy.
- [ ] Real concurrent `/tmp` run and dirty reopen verify cleanly.
