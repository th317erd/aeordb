# Completing the user-facing v4 runtime

Execution extension to the ratified [parent](../2026-08-03-aeordb-v4-nvt-gc-refactor.md).
Active TODO and evidence: [ledger 11](progress/11-user-facing-v4.md).
Entry revision: `9d04c76dc643f2de94fd389aac5c6a87889532fe`.
This is an implementation bridge, not replacement format or policy ratification.

## Outcome and evidence boundary

Ordinary creation must yield a genuine v4 database that the normal binary and
embedded API can reopen, read, mutate, query, maintain and verify. Existing v3
databases must have a supported, side-by-side path to that same runtime.
The previous shadow-only tooling and v3-compatible soaks do not prove this.

The target is the entire user-facing refactor. Deployment/publication and
mutations of real databases are not part of disposable qualification authority.
Keep the retained corrupt FS-Server1 database and sealed prior evidence intact.

Observed first live failure: normal a804 CLI passes HTTP CRUD/restart but writes
`AEOR` + version byte `03`, not `04`. The executable target is
`scripts/spec/v4-default-live-spec.mjs`; final DoD requires its genuine pass.

## Audited ownership boundaries

| Boundary | Existing owner / inspected limitation | Required integration |
| --- | --- | --- |
| Normal creation/open | CLI `start`, `server/mod.rs`, auth `provider`, backup creators call `StorageEngine`; it owns a concrete v3 `AppendWriter` and KV writer | One format-dispatched public engine; real v4 initialization, admission and lifecycle |
| V4 physical publication | `v4/first_authority.rs` owns file, KV mutex, header publisher and root guard | Reuse this authority path; do not attach a second independent mutable KV/header owner to the same file |
| Initialization | `v4/migration_destination.rs` composes v4 A/B headers, KV and first authority under migration preflight | Factor shared physical initialization; fresh creation cannot fabricate migration/source evidence |
| Semantics | `namespace.rs` writes root/state only; production state producers currently emit content-only legacy state | Canonical definition/catalog writers and bounded compiler/COW publication; known semantics for new roots |
| Reads | `read_view_native.rs` resolves v4 captured authority; server binds `legacy_v3_root_adapter.rs` | Native selected-root binding across the complete route matrix, with current authorization and request pins |
| Index runtime | `index_runtime_installation.rs` explicitly binds a v3 source and v4 shadow; source adapters use `StorageEngine` | Bind one native v4 authority, shared memory, soft mutation stream, coverage and descriptor recovery |
| Maintenance | public verify/probe/durability recovery still call `writer_read_lock()` returning v3 `AppendWriter` | Format-neutral observation/inspection boundaries and actual-v4 verify, repair, GC, backup and restore |
| Migration | offline run preserves bytes but templates roots as content-only; public CLI lacks service acceptance | Exact current semantics, retained-root policy, cutover journal, read-only service and durable acceptance |

The 136-line entry/import search is a **candidate inventory**, not 136 runtime
callers: it includes tests and comments. Likewise the 167 storage backend-field
references are a review workload, not independent publishers. Before each
consumer wave, classify exact call sites against the existing architecture and
SystemFamily fixtures. No comment/import-only hit counts as a verified edge.

Other confirmed constraints: a fresh complete semantic state cannot be labeled
legacy content-only. At entry, the writer/decoder and independent reference
incorrectly rejected the ratified complete-empty catalog; U0 corrects that
rejection against normative bytes, not mutual agreement alone.
The standalone shadow publisher also does not own runtime KV growth; pre-sizing
a migration destination cannot substitute for indefinite runtime capacity.

## Dependency-ordered landing units

The units below retain every child-plan obligation. Each starts with a refreshed
exact consumer/test inventory, target failure and named narrow command. Do not
claim later units ready merely because the first prerequisite is green.

1. **U0 — entry proof and contract corrections.** Preserve the live default red
   target, restore baseline/fix guards, and correct complete-empty semantic
   state through independent codec and consumer tests. Existing nonempty,
   content-only and malformed fixtures retain their meaning. Own only codec,
   reference, targeted consumers and their specs for that correction.
2. **U1 — complete semantic production.** Implement canonical definition and
   radix-catalog writers from frozen Round 10/11 bytes. Compile all seven
   definition/projection families through their owners, preserving effective
   scopes, parser/dependency identity and SystemFamily semantic fingerprint.
   Empty, default and changed configuration produce exact complete semantics;
   legacy roots remain content-only only where history truly cannot be proven.
   Build/update catalogs under bounded memory and COW, not whole-world reloads.
3. **U2 — one physical runtime owner.** Separate the version-gated legacy
   backend from the permanent `StorageEngine` API. Integrate existing v4
   publication, KV, captured readers and native barriers behind one owned
   backend. Add real create/open, lock/physical identity, capability admission,
   writer fencing, strict configuration, spill/latch discovery, memory budgets,
   counters, bounded shutdown and runtime KV growth/recovery. Failed opens may
   not write from Drop. No empty/missing backend fields or v3 header projections
   may masquerade as authoritative v4 state.
4. **U3 — all authoritative producers.** Follow Child 03's five waves:
   DirectoryOps; streamed/blob/batch/copy/rename; version/backup/restore/sync;
   system/auth/config/plugin/tasks; maintenance/repair. Each operation uses
   dependency-first, authority-last publication, exact root admission and
   locator replacement/retirement. Post-commit index/cache/SSE work stays soft.
   Preserve bounded metadata-only existing-chunk commit and current permission
   checks. Whole-HEAD replacement is not a sequence of visible partial roots.
5. **U4 — native read/index service.** Bind the shared read resolver, native
   parser/index coordinator, lazy stores, coverage, planners, APOS, locators,
   shares/plugins/downloads, SSE and embedded equivalents to U2/U3. Preserve the
   complete route-class matrix and coordinated client schemas. Supplied roots
   never fall through to HEAD; absent derived state never produces false exact
   results. Historical authorization remains current-authority intersection.
6. **U5 — maintenance and migration integration.** Complete actual-v4 physical
   inventory, lifecycle, mark/checkpoint/quarantine/sweep/Void and all public
   maintenance entry paths using the existing state machines. Integrate strict
   readonly verify, repair latches, logical transfer, copy adoption and backup.
   Finish public migration/cutover/recovery/read-only validation/acceptance
   commands around the existing journal and permits. Do not waive the retained
   v3 rollback boundary or enable destination sweep early. Online capture and
   persisted callers remain covered by Child 07, not silently removed.
7. **U6 — default cutover and retirement.** Only after U1–U5 behavior is present,
   make normal new databases v4 across CLI, embedded, identity and backup/clone
   creation. Existing files are detected, not rewritten in place. Preserve
   sanctioned v0/v3 compatibility owners during the ratified migration window;
   delete transitional inactive/shadow-only runtime bindings. Refresh capability
   advertisement only for fully integrated supported behavior, all clients,
   operational examples, API docs and Dashboard. Run the original live target.
8. **U7 — final actual-v4 qualification.** Full final-source Linux suite,
   native-platform gates, exact normal releases, real authenticated clients,
   media migration-to-service, crash matrix, no-swap resource overlap, required
   complete soaks, performance confirmation, and adversarial DoD review. Seal
   evidence with exact source/lock/binary identity and explicit skipped scope.
   Clean only verified unneeded disposable DBs; preserve failure specimens.

## Proof spine and recent-fix protection

U0 narrow target (desktop, matching lock, two jobs; wrappers enforce disk guard):

```bash
timeout 30m cargo test --offline --locked --release -j 2 -p aeordb \
  --test v4_root_migration_spec complete_empty_semantic
timeout 30m cargo test --offline --locked --release -j 2 -p aeordb \
  --test index_semantic_source_spec complete_empty_catalog
```

Then run the complete affected targets, native read-view/semantic catalog/root
authority regressions, independent reference verification, static contracts and
debt gates. New runtime/CLI integration targets must exercise public entry points
and ordinary release builds, not a separately implemented test service. Every
target added as an outstanding manual gate must pass by U7; no permanent
expected-failure allowance satisfies completion.

Each runtime wave retains and names guarding specs for checkpoint restart
(`a8047327`), retired-KV chronology (`48baeefe`), readonly verification
(`33420bad`), bounded repair/cache and single scan (`0b20792b`, `3ab02246`),
dirty authority recovery (`f8a656e6`), migration KV capacity (`08025b80`), and
legacy unsorted directories (`eb99f6ba`). Do not remove these protections during
format dispatch or reinterpret their prior v3-only proof as v4 proof.

Required failures include capability/registry mismatch, copied/stale physical
identity, torn/degraded headers, cancellation and shutdown races, state/lease
conflicts, malformed semantic/control/namespace bytes, zero/missing objects,
permissions and concealment, missing journals/indexes/NVT, disk/memory pressure,
locator replacement, partial migration/cutover and ambiguous durability. Use
independent byte and behavior models; sharing a production encoder is not an
independent oracle.

Use the parent's existing numerical performance/resource gates and confirmation
protocol. The final proof is against the exact integrated candidate, not a
module count or accumulation of predecessor passes. No production action occurs
as an accidental consequence of a readiness test.

## Landing and completion rules

One direct integration owner; no delegated edits. Preserve all unrelated WIP.
Fetch/compare upstream at each coherent boundary. Keep source changes and their
proof one revertable unit; format and review diffs, run affected then broad
gates, commit/push coherent green snapshots under standing authority. Never
push after an unexplained failing suite. Old evidence is immutable history.

No genuine owner-policy choice has been identified by the entry audit. Resolve
technical details from ratified contracts; escalate only a contradiction or
authority boundary that actually needs the owner. Completing U0 or any later
unit is not permission to mark the full goal complete.

## U1 entry inventory and proof refinement

Read-only source review during U0 verification confirms the following additional
boundaries; these are required implementation work, not newly chosen policy:

- `namespace.rs` has semantic state/root encoders and borrowed catalog/definition
  readers, but no catalog-leaf/internal or definition-record encoders. The
  independent core fixtures cover envelope shape; their parser projection
  payload (`canonical-parser-registry`) is a historical opaque example, **not**
  proof of a compiled canonical configuration. Preserve those reader fixtures
  while adding independently constructed valid compiler-output fixtures.
- `scope.rs`, `value_store.rs`, `field_definition.rs`, `source_selector.rs`,
  `parser_plan.rs` and `dependency.rs` decode the existing typed definitions.
  No production scope/value/field/selector/parser/dependency compiler writes
  those definitions. Writers must validate before allocation and keep exact
  class identity separate from the wrapping immutable object identity.
- `IndexConfigResolver` and `PathIndexConfig` currently select/deserialize v0
  controls. A new corrected configuration cannot reuse legacy converter aliases
  or source coercion silently. Round 11 section 10 fixes the corrected default
  field/strategy matrix and finite semantic bounds; v0 adapters remain explicit.
- Round 10 requires semantic mutations from **all** producer families to be
  staged and task-backed, including mixed ordinary/config batches. Compilation
  occurs without a long namespace lock; activation rechecks captured semantic
  inputs and atomically publishes the whole staged change. Codecs alone do not
  complete this producer integration (also tracked in U3).
- `index_native_parser.rs::native_fingerprint` currently hashes descriptive
  strings for three native components. Round 9 requires checked-in semantic
  specifications plus conformance manifests, including the fourth Regex
  selector component. Audit/freeze that evidence before a normal writer emits
  executable native dependencies; do not relabel string hashes as conformance
  proof or invalidate structurally retainable unknown dependencies.
- `V4FirstAuthorityPublisher::publish_immutable_semantic_objects` already
  publishes canonical protected FileRecords/chunks and checks repeat bytes.
  `load_semantic_object*` supports captured reads. Bind the compiler's bounded
  COW object sink to these owners; do not activate the disconnected legacy
  `V4SemanticObjectStore` as a second physical authority.

U1's falsifying tests must cover all seven classes, exact independent bytes and
IDs at both hash widths, invalid/oversized requests before allocation, sorted
and duplicate catalog keys, insertion-order-independent Patricia shape, bounded
single-path replacement/removal, restart/cancellation and memory admission.
Compiler tests additionally cover alias/default equivalence where approved,
every meaningful semantic change, irrelevant logging/formatting invariance,
nearer fieldless scopes, dependency availability, and atomic task activation.
An encoder/decoder round trip alone does not satisfy those tests.

### U1 dependency-key clarification pending owner review

The September 13 audit found a genuine persistent-contract ambiguity, not an
implementation preference. Round 9 assigns dependency fingerprints a fixed
32-byte raw-module or conformance-manifest digest, permits a module to declare
both parser and mapper roles, and includes role/ABI/profile in each canonical
dependency record. Round 10 names that fingerprint as the catalog owner key,
while the existing production reader requires database-hash width for classes
6/7. It separately defines H-wide, domain-separated dependency-definition IDs.

Concrete counterexample: two dependency records for the same module, one parser
and one mapper, share `(record_kind=6, raw-module fingerprint)` but have different
definition bytes/IDs. A catalog binding cannot store both under the same key;
this is an identical-input key conflict, not a cryptographic collision. With a
64-byte database hash, the reader also rejects the specified 32-byte owner.
No additional definition grouping/role-key rule was found in the ratified source.

Recommendation sent to the owner: explicitly key classes 6/7 by the already
defined complete dependency-definition ID, while keeping the raw 32-byte
fingerprint unchanged for artifact lookup and executor conformance. This would
resolve both issues but clarifies the frozen catalog contract, so it is **not
approved or implemented**. Await the owner's ruling before emitting those
bindings or altering their readers/fixtures. U0 verification and unrelated
U1 inventory remain safe to continue; no production database is involved.

### U1 native-execution availability perimeter

Source review also confirms a required regression beyond generating four
manifest hashes. `ValueStoreRuntimeV1::from_definition` compiles Regex segments
directly; its `extract` path does not check the selected native selector
fingerprint. `AuthoritativeSourceEvaluatorV1` routes producer and selected-query
evaluation through that owner. The definition decoder checks the selector
dependency's native role/profile structurally, not its exact available
fingerprint. Parser execution does check its own three native fingerprints.

Before activating new native definitions, add a falsifying test that changes
only the selector dependency fingerprint in a structurally valid ValueStore:
structural decoding/retention must remain possible, but source execution must
return typed dependency-unavailable before interpreting the selector. Exercise
both the shared evaluator and direct ValueStore runtime so neither bypasses
availability. Check current known manifest identity, unknown manifest, role/ABI
profile mismatch, cancellation, and memory release. Do not solve execution
availability by rejecting structurally retainable historical definitions.

Existing `index_native_parser_spec` has twelve tests covering exact historical
reads, MIME/JSON claims, limits, cancellation, explicit-WASM refusal, memory and
archive parsing. They are affected regressions, not the missing checked-in
four-component conformance manifests. The explicit/registry WASM refusal is
also an existing disconnected runtime boundary to complete during U4; an
encoder or default-native-only demonstration cannot qualify plugin execution.
