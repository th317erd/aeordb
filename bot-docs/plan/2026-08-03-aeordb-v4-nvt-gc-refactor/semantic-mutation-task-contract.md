# Durable semantic mutation — additive Round 17 contract

Entry: `716cf3d6`. Implementation owner: Codex, direct execution. These are new
SystemControlV1 kinds, not extensions of TaskPinV1 or IndexTaskCheckpointV1.
Existing kind IDs, bytes, fixture meanings and capability assignments survive.
Reader/fixture qualification precedes writers; runtime advertisement remains
disabled until capture, GC, recovery and activation consumers are integrated.

## Territory and landing gates

The shared envelope/path/A-B selector is `v4/system_control.rs`; the native
physical publisher is `v4/first_authority.rs`. `control_store.rs` owns the old
v3 transition and disconnected v4 adapters. New kinds must be refused by the
v3 transition adapter. `task_queue.rs` is scheduler presentation, not task
authority. `semantic_catalog_native.rs` borrows the native physical owner and
does not itself create pins. The compiler's opaque in-memory result is not
persisted admission. Existing `gc_mark_runtime`/`gc_mark_convergence` consume
captured reachability through their owners; a search finds no direct TaskPin
decoder there. Connecting new roots to actual mark/retirement/sweep remains a
runtime integration gate, not an inferred consequence of decoding a control.

Registry/code-generation, production and independent-reference envelope
readers, header capability masks, common control selection, protected-family
classification, transfer policy, strict verification and architecture tests are
reader-wave consumers. Activation/root admission, source snapshots, task queue,
compiler resume, pin reconciliation, migration/adoption and physical backup are
following runtime consumers. No new service writer is enabled in reader wave.
Logical transfer must not resurrect node-local in-flight tasks; physical-copy
resume requires explicit adoption and new fencing. Inspected registry family
0x0043 covers the entire protected control prefix: physical copy is required,
logical backup/export/join/client sync omit, peer/import are node-local, and
verification is strict-required. Add executable classification checks for all
three canonical new paths; no family-registry bytes need changing.

## Envelope, assignments and admission

All integer fields are little endian. H is the selected registered database
hash width, including all five currently registered algorithms. Common 32-byte
SystemControlV1 header, exact lengths, CRC and canonical protected paths apply.

| Kind | Magic | Identity | Mutability | Body bytes |
| --- | --- | --- | --- | --- |
| 0x0044 SemanticMutationTask | ASMT | task_id[16] | A/B | 112 + H |
| 0x0045 SemanticMutationCheckpoint | ASMC | task_id[16] + checkpoint_sequence u64 | immutable, envelope sequence 1 | 168 + 9H + C |
| 0x0046 SemanticMutationGeneration | ASMG | singleton | A/B | 16 |

Capability bit 25 is `SemanticMutationTaskV1`. Bit24 remains unassigned: frozen
unknown-capability fixtures use it, and their byte-level negative meaning is
preserved. The registry is intentionally no longer contiguous. A database using any of these
controls requires it for both readers and writers. Assignment is not runtime
advertisement: `BinaryCapabilityProfileV1::current` retains its previous mask
until the entire runtime integration is qualified. Old readers therefore fail
admission before writes. No extra entry type, KV owner or physical file is added.

### SemanticMutationTask

| Offset | Bytes | Field |
| --- | --- | --- |
| 0 | 16 | nonzero database_id |
| 16 | 16 | nonzero task_id |
| 32 | 16 | nonzero physical_instance_id |
| 48 | 16 | nonzero holder_boot_id |
| 64 | 8 | nonzero task fencing_token |
| 72 | 8 | nonzero writer_fence_epoch |
| 80 | 8 | created_at_ms, nonnegative i64 |
| 88 | 8 | updated_at_ms, i64 >= created_at_ms |
| 96 | 2 | state |
| 98 | 2 | flags: bit 0 pins_released, all others zero |
| 100 | 8 | nonzero selected checkpoint_sequence |
| 108 | 4 | reserved zero |
| 112 | H | nonzero selected checkpoint payload digest |

States are 1 queued, 2 capturing, 3 compiling, 4 ready_to_activate,
5 activating, 6 completed, 7 failed, 8 cancelled, 9 superseded. Pins may be
released only for terminal states 6..9, after the retention owner has proved
safe release. Neither elapsed time nor a terminal scheduler row releases pins.
Digest is selected-algorithm HASH of the complete ASMC envelope, including CRC;
it is a binding check, not an untyped KV reference. Resolve the checkpoint by
its canonical kind/task/sequence path, then compare complete payload identity.

### SemanticMutationCheckpoint

| Offset | Bytes | Field |
| --- | --- | --- |
| 0 | 16 | nonzero database_id |
| 16 | 16 | nonzero task_id |
| 32 | 8 | nonzero checkpoint_sequence |
| 40 | 16 | nonzero captured physical_instance_id |
| 56 | 8 | nonzero captured writer_fence_epoch |
| 64 | 8 | nonzero captured semantic generation |
| 72 | 8 | nonzero captured header sequence |
| 80 | 8 | captured_at_ms, nonnegative i64 |
| 88 | 2 | phase: 1 captured, 2 compiling, 3 pruning, 4 ready, 5 activated |
| 90 | 2 | cursor_kind: 0 none, 1 canonical config owner path, 2 dependency ID |
| 92 | 4 | C, cursor byte length, <= 65,535 |
| 96 | 8 | expected final configuration count |
| 104 | 8 | current catalog configuration count |
| 112 | 8 | current catalog record count |
| 120 | 8 | current catalog node count |
| 128 | 8 | current catalog dependency count |
| 136 | 8 | nonzero accepted logical mutation count |
| 144 | 8 | activation generation; zero before activated |
| 152 | 8 | dependency-pruning candidate catalog record count |
| 160 | 8 | dependency-pruning candidate catalog node count |
| 168 | 9H | the nine hashes below in exactly this order |
| 168 + 9H | C | cursor, no padding or trailing bytes |

Hash slots: base NamespaceRoot; staged DirectoryIndex tree; current semantic
catalog; dependency-pruning candidate catalog; compiled SemanticState;
candidate NamespaceRoot; compiler fingerprint; semantic registry fingerprint;
captured semantic source identity fingerprint. Base, staged tree and the last
three fingerprints are required nonzero. Optional hashes use all-zero absence.
No fixed 32-byte substitute is permitted for H-wide fields.

Catalog absence requires zero records/nodes/dependencies/configurations.
Presence requires positive records/nodes, nodes <= 2*records-1 (checked), and
dependencies/configurations <= records. The pruning catalog has the same presence/count rule.
The compiler must separately prove actual counts and typed closure; plausible
numbers in a checkpoint do not grant an opaque compiled-catalog capability.

Captured phase has no catalogs, output state/root, cursor or activation.
Compiling/pruning have no output state/root or activation. A config-owner cursor
is legal only while compiling; a selected-H nonzero dependency-ID cursor only
while pruning. Empty cursor requires kind 0; nonempty paths use the shared
canonical absolute path validator. Ready/activated require a catalog, exact
final configuration count, output state and candidate root, no pruning catalog
and no cursor. Activated generation is exactly captured generation + 1 with
checked overflow; every earlier phase has zero activation generation.

The staged tree is the complete immutable requested overlay on the captured
base tree, including ordinary files in mixed batches. It is NOT an admitted
NamespaceRoot and grants no read-view authority. Differences against the base
describe the request; activation rebases those differences on current HEAD
under normal conflict/authorization rules. Inputs and module FileRecord
identities remain available through the pinned base and staged trees.

The source fingerprint is HASH of `aeordb.semantic-mutation-sources.v1\0`
followed by sorted, unique records `(path_length u32, canonical UTF-8 path,
FileRecord hash H)`. Zero hash means an explicitly captured absence. The source
owner enumerates the complete relevant union of base/request controls, aliases
and referenced modules. This digest is a comparison aid, not an authoritative
replacement for enumerating and checking exact captured identities at resume
and activation. Its fixture and source-owner proof precede any writer use.

### SemanticMutationGeneration

Body is exactly nonzero database_id[16]. The selected A/B envelope sequence IS
the semantic generation, initially 1. Every committed semantic-input mutation,
including a semantic-equivalent raw rewrite, advances it once in the same
atomic transaction as task activation and HEAD. Ordinary changes do not.
Generation exhaustion fails before mutation; it never wraps or resets. This
prevents an input-changing-and-changing-back race from escaping capture checks.

## Selection, recovery and typed reachability

Task A/B selection uses the existing highest-valid-sequence rule and fails on
equal-sequence disagreement. Validate the selected checkpoint's identity,
payload digest, logical and physical identity, phases and fences before resume.
Takeover advances the task fence and uses the current writer epoch; it does
not rewrite the immutable captured checkpoint or trust old node ownership.

An unreleased task protects its selected checkpoint's FileRecord/chunks, the
admitted base root closure, the staged DirectoryIndex/FileRecord/chunk closure,
both semantic catalogs and their typed definition/module edges, any output
SemanticState, and any candidate root as STAGED bytes (not invented admission).
Fingerprints, sequence numbers and payload digest are comparisons, not graph
edges. An incomplete/unknown task closure makes mark incomplete and prevents
sweep; it must never be treated as an empty task. Intermediate publication
requires the existing publication/root guard plus staging protection until the
next checkpoint is durable and selected. Releasing the old checkpoint before
selecting its replacement is forbidden. This wiring is a mandatory later gate.

Completed requires an activated checkpoint and selected authority read-back.
Ready/activating require a ready checkpoint. Queued/capturing require captured;
compiling permits compiling/pruning. Failed/cancelled/superseded cannot refer
to activated. A torn post-HEAD presentation/status update cannot undo a commit:
recover completion from the atomically selected task activation/generation/root
authorities. Cancellation loses after the activation commit begins.

## Falsifying verification

Independent hand-built envelopes precede production readers. Test every hash,
old layout stability, all states/phases, truncation at every offset, repaired-CRC
illegal enums/reserves/zero IDs/counts/presence/cursor/cross-record mismatches,
digest binding, immutable sequence, ambiguous A/B selection and cap checks.
Payload fields and cursors remain borrowed; no allocation scales with cursor
length or claimed catalog/source counts. The shared envelope retains only a
fallibly allocated 16-byte task or 24-byte checkpoint identity; selection adds
one fallibly allocated H-byte digest. Measure those exact bounds and injected
allocation failures rather than claiming the existing envelope is allocation-free.
Capability tests distinguish known-but-unadvertised bit25 from unknown bits24/26
and test all 256 positions, including the partial final known byte.

Before runtime writers: independently generate fixed fixtures/reference readers,
audit protected transfer policy and v3 refusal, then qualify byte writers. Next
prove each actual file publication boundary, restart/takeover, GC retention,
semantic races, ordinary rebase, mixed-batch invisibility and commit-wins-cancel.
Full Linux and native-platform gates remain required; codec green is not U1 done.

## Byte-writer qualification slice — September16

Entry candidate `d19109f7`; production writer behavior starts after the
outstanding native Windows reader/probe gate, not on its launch. Independent
test-only drafts may be prepared while qualification runs. This slice only
produces bytes.
Use the existing borrowed `SemanticMutationTaskV1` and
`SemanticMutationCheckpointV1` as typed inputs; generation takes the database
identity and explicit nonzero sequence. Checkpoint envelope sequence is always1.
Do not add a scheduler, second control owner, task publication route or capability
advertisement. Native `first_authority` and both `control_store` adapters must
continue refusing these controls before I/O.

The existing SystemControl framing encoder has twelve call sites across
`system_control`, `root_authority`, `migration_control`, `migration_root_map`,
`migration_cutover_control` and `index_operation_control`. Add fallible exact
output allocation at that owner and a bounded body-fill entry used by the new
typed writers; retain the existing slice encoder as its delegating adapter.
This shares the framing/CRC/round-trip owner instead of duplicating its bytes.
Validate identifier/hash widths and cursor caps before copying; reject a present
optional hash containing only zero bytes rather than silently encoding absence.
No allocation may scale with claimed catalog/source counts. Encoding does not
prove closure, current ownership or task/checkpoint selection.

The new byte APIs preserve `FormatError::is_allocation_failure`; this is not a
claim that every legacy engine adapter preserves operational classifications.
Inspection found the existing configuration/durability `format_error` adapters
collapse format failures into `EngineError::InvalidInput`, and index recovery
uses a generic `native_index_format` code. First-authority/root-map wrappers
retain the nested `FormatError`. Preserve fail-closed behavior and carry the
legacy adapter classification review into U2/U5; do not label those existing
runtime adapters resource-qualified merely because the byte encoder is.

Falsifying test order:

1. Add callable refusing scaffolds and independent positive targets; preserve
   the actual failing run before implementing behavior. Compare output to frozen
   reference fixtures and independent hand-built envelopes, not self-generated
   goldens. Exercise all five algorithms, nine task states, five checkpoint
   phases, both cursor types, all optional hash slots and nontrivial counters.
2. Require typed rejection for every short/long/zero identity, wrong hash width,
   absent-vs-present-zero confusion, zero sequences/fences/counts, timestamp and
   phase mismatch, excessive/invalid cursor, overflow and partial ready state.
   Recheck A/B selection and paired checkpoint digest/state binding on output.
3. Measure exact output and bounded identity allocations, inject their failure
   independently, and retry successfully. Maximum cursor must require only one
   output buffer plus the decoder's bounded24-byte identity, not a second body
   allocation. Failed encoding must not publish or change any source input.
4. Run existing control, admission, root publication, migration and configuration
   regressions (all shared framing callers), resource specs, independent reference
   fixtures, static contracts, debt audit, full library and strict Clippy. Qualify
   final writer source on Linux, macOS and native Windows with bounded runners and
   exact source/lock/executable evidence. Existing reader/publisher refusal tests
   remain mandatory. This byte-only API has no live service surface to exercise;
   native file publication/reopen remains owed by the following runtime slice.

## Runtime entry: coherent native observation — September17

This is the first runtime prerequisite after the qualified byte writers, not
permission to enable task publication independently of retention/activation.
The existing first-authority loaders each take `root_state`, observe the header
and lock the same KV owner. Calling the task/generation/checkpoint loaders in
sequence does not retain that common boundary across calls. Introduce one
bounded native observation at the existing physical owner, reusing its canonical
SystemControl FileRecord/chunk loaders and A/B selector under one guard.
Do not add another file/KV owner or bypass a publication refusal.

The observation is deliberately **not** a resume permit, pin, admitted root or
compiled-catalog proof. It reports the captured header, selected generation,
task and—only while pins remain held—the bound immutable checkpoint. Enforce
logical database/kind/path/identity, complete envelope digest, selected phase
and existing fence/timestamp relationships through the shared readers.
Current physical identity, writer epoch, generation, exact input identities,
closure retention and executor availability still require the subsequent
fenced runtime owner's decisions. In particular, a completed historical task
need not match today's semantic generation, and an adopted physical copy must
not silently grant old task ownership.

Released terminal tasks no longer promise retention of their checkpoints.
Their observation must remain a released terminal summary, not manufacture
checkpoint absence into an empty active task or declare a legitimately collected
checkpoint corrupt. Conversely, an unreleased selected task without its exact
checkpoint/generation is an incomplete authority observation and must fail;
mark must never treat that failure as no protected roots. A missing requested
task can be reported only after checked A/B lookup; malformed slot/body/I/O
errors retain the selector's existing fail-closed distinctions.

Keep memory and work bounded independently of catalog/source counts. Reserve
scratch before any file-body allocation and retain the result's charge while
owned bytes remain alive. Check cancellation before admission and between
bounded reads; preserve operational/resource failures rather than classifying
them as missing/corrupt data. Avoid recursive acquisition of `root_state` when
composing the existing loaders. Runtime publication and capability25 remain
disabled throughout this observational slice.

Falsifying proof before implementation: callable refusal plus native disposable
file fixtures containing independently frozen task/checkpoint/generation bytes.
Fixtures must use test-only physical assembly, not enable production publishers.
Prove reopen/read-only bytes and physical length stability, both 32/64-byte
widths, absent task, generation absence, malformed slots, equal-sequence
disagreement, selected digest/phase/identity mismatch, released terminal summary
after checkpoint absence, physical-copy/history observations without ownership,
pre-cancellation, memory refusal/retry and exact retained accounting. A controlled
selection-change test must demonstrate one boundary rather than three unlocked
lookups. Reuse existing native file/entity validation instead of a mock-only
store; fault tests may supplement that actual file path.

Before coding, complete the exact helper/error/fixture consumer inventory and
freeze the bounded request/result types. The follow-up inventory below supplies
that entry check for this observation only; capture/retention ownership remains
outstanding.
The September17 helper audit found an additional prerequisite: shared
`select_system_control_pair` currently classifies **every** decode error as a
bad slot, although the new semantic task identity decoder can fail allocation.
Add real allocator-injection RED for failure in either slot (including the
newer slot), and resource failure beside a genuinely corrupt peer. Preserve
genuine torn-slot fallback, equal-sequence ambiguity and identity checks while
propagating operational allocation errors. Qualify that selector correction
before using it for native task observations. Existing native slot loading also
contains body clones/owned FileRecord decoding: bounded admission is not proof
that every inner allocation is fallible. Inventory and test that boundary before
claiming native observation's resource behavior.
The following dependent slices still owe complete source capture (including
sorted, non-rescanning enumeration), durable task-root discovery/GC protection,
checkpoint publication/resume and guarded HEAD/generation/task activation.

### Native observation entry inventory and API

The September17 follow-up inspected the executable call chain, not just imports:
`load_mutable_system_control{,_selected_pair}` and
`load_immutable_system_control` each acquire `root_state`, observe one header,
lock the same KV owner and check layout alignment. The private pair loader
uses `load_system_file_slot` -> `load_canonical_system_file_at_path` ->
`read_entity_bounded`/`decode_whole_entity`/`FileRecord::deserialize`.
`discover_mutable_control` supplies the existing identity/sequence policy.
The immutable loader verifies kind/database/path identity. The already-qualified
`decode_semantic_mutation_selection` supplies exact checkpoint digest, task,
physical identity, captured fence, lifetime and phase checks. Reuse these owners;
do not copy their policies into another native file reader.

Freeze the next API as `V4FirstAuthorityPublisher::observe_semantic_mutation_task`
with a borrowed request containing database ID, task ID, cancellation and the
shared memory coordinator. Return a non-Clone, privately constructed observation
containing the captured header, selected task/generation metadata and owned
checkpoint bytes when retained. Expose borrowed typed readers and explicit
`Absent`, `ReleasedTerminal`, or `CheckpointHeld` disposition. The result owns
its memory reservation; no `into_bytes` escape may discard that charge while
returning owned buffers. Neither disposition nor a parsed checkpoint implements
any resume, root-admission or retention permit trait.

One `root_state` guard and one checked header/KV boundary cover all reads. Do
not compose the separately locking public loaders. A missing task returns an
absent observation only after checking its slots; an existing task requires a
selected generation. Read and bind its checkpoint only when pins are not
released. Current generation/physical instance/writer ownership are observations,
not implicit adoption checks: historical completion may legitimately differ.
The later fenced owner decides whether current authority permits work.

Memory inventory: canonical FileRecord entities are capped at64KiB and control
bodies at their registered encoded caps. The pair loader temporarily retains
both FileRecords/bodies plus body clones and the selected owned copy; sequential
task/generation reads retain prior outputs. Reserve a conservative envelope of
`8 * (largest_control_encoded_cap + 64KiB) + 64KiB` before header/body loading,
under `MemoryOwner::Task`, and retain that charge with the result. Prove measured
peak ownership fits this bound. Existing KV cache/page allocations retain their
existing owner, not a second task charge. The inspected native fixture uses the
KV bootstrap coordinator, separately from the request's task coordinator;
configured bounded pages can later attach the runtime coordinator through
`DiskKVStore::activate_bounded_pages`. This slice does not prove that the whole
runtime shares one coordinator: that binding remains U2's obligation.
Cancellation is checked before
admission, after acquiring the guard and between bounded reads.

The transitive audit also found small infallible path/header/hash allocations,
owned FileRecord fields, and pair body clones. Reservation is **not** universal
host-OOM recovery. Native proof must distinguish coordinator refusal, currently
fallible entity/body-buffer failures, and these inherited allocations. Preserve
original error sources and resource distinctions; do not advertise universal
allocator recovery or silently call an I/O/resource failure absence. Allocation
fault qualification may require further shared-helper corrections before this
observational unit is ready; no duplicate parser is authorized as a shortcut.

Test entry is a child of the existing native first-authority internal harness,
using `create_environment_for_algorithm_at_kv_stage` and test-only physical
FileRecord/chunk assembly. Frozen ASMT/ASMC/ASMG bytes remain independent of the
production semantic writers. Reopen actual disposable files at32/64-byte widths;
compare bytes/length/header before and after observation. Exercise absent and
released tasks, missing generation/checkpoint, corrupt slots and ambiguous ties,
all existing binding failures, physical/history differences without ownership,
cancellation, budget refusal/retry and reservation lifetime. A controlled writer
attempt during observation must remain blocked until the complete observation
has been assembled. No production publisher refusal is relaxed for fixtures.

The implementation is a private child of `first_authority`, with public types
reexported by that existing owner. Its source must be included in
`v4_first_authority_spec`'s exact reviewed-owner inventory, with additional
read-only/no-publication checks. The broader native Mac gate exposed the missing
inventory entry after the thirteen new native cases passed. Keep that failure
and rerun all platform gates on the corrected test inventory; this is not an
exception allowing another physical writer.

September17 qualification is complete in the source-bound
[observation proof](evidence/user-facing-v4-u1-semantic-observation-proof-20260917.json).
All thirteen native observation cases pass on Linux, macOS and Windows, with
affected suites, library, static and independent-reference gates. The Windows
memory-pressure failure and identical-executable isolated recovery are retained.
The operation satisfies this read-only slice; complete source enumeration,
durable closure retention, checkpoint publication/resume and atomic activation
remain separate required runtime work.

### Next prerequisite: ordered native namespace seeks

Entry82574387. Source capture requires an ordered, bounded namespace stream;
the selected-root reader currently restarts DFS on every page. Its public
`scan_files` consumer is `query_native_source`'s full partition builder. The
existing `index_native_source` maintenance scanner already has the required
full-path ordering and lower-bound/successor algorithm, including punctuation
before the directory separator. Share that algorithm instead of adding another
independent walk or routing v4 through a synthesized v3 header.

The intended permanent internal owner is `v4/namespace_seek.rs`: ordered child
selection and bounded B-tree lower-bound/successor traversal over caller-supplied
validated directory nodes. It owns no file, KV, header, task, authorization or
GC authority. Each loaded node retains its decoder's memory reservation through
use; bounded traversal scratch is admitted by the existing calling operation.
Physical adapters remain in `index_native_source` and `read_view_native` and
preserve their own framing, content identity, cancellation and error policies.
V0 flat directories retain their explicit sorting adapter; v4 flat directories
remain strictly canonical. Inherited separator ranges must be validated before
a skipped subtree can justify a successor/absence result. Keep legacy revision
point lookup and its parser consumers outside this migration unless a necessary
shared validation change is separately proved.

Correct `scan_files` to return full-path byte order with direct resume seeks.
Its captured header/root/semantic identity, authorization scope, request pin,
page accounting, missing-resume failure, complete/incomplete reporting and
symlink non-following behavior remain. It must never rescan all preceding
documents merely to reach a later page. An explicit absent or non-file resume
still fails before claiming completion. This is an in-process traversal API,
not a newly persisted cursor, staged-root admission or complete source capture.

Falsifying test first: the existing native fixture with two40-entry B-tree
leaves must return76/77 after75 with64work steps, then78/79 and complete, at
both hash widths and with byte-identical file contents. Add prefix-directory
punctuation order versus an independently sorted path list; page concatenation
must exactly match that list. Cover multi-level successor/empty leaves, unknown
roles, malformed ranges/order, missing/native-read failures, authorization,
cancel/retry and accounting release. Preserve all `index_native_scan_spec`
cases (including late600-file seek, legacy unsorted flat input and historical
root behavior), `index_native_source_spec`, native read-view/query partition
consumers, and source-fingerprint tests. Measure seek work/read bounds instead
of relying on elapsed-time claims. Existing bounded desktop/native-platform
qualification and exact-source evidence gates apply before landing.

This prerequisite is qualified on Linux, macOS and native Windows in the
[September17 namespace seek proof](evidence/user-facing-v4-u1-namespace-seek-proof-20260917.json).
The proof retains actual failing-first ordering/work tests, intermediate test
and harness failures, final native suites and exact source/binary identity.
It does not grant staged-root admission or complete capture/retention authority.

### Runtime staging protection — September17 entry38ef8edd

The next integration prerequisite closes the in-process interval between
immutable staging and durable checkpoint selection. Add a non-Clone, privately
constructed `NativeStagingProtectionV1` borrowing the existing
`V4FirstAuthorityPublisher`. Acquisition/release update bounded state under
that owner's existing `root_state` mutex; the returned guard does **not** hold
the mutex during compilation. Account its lifetime with the shared task-memory
coordinator. Cancellation/admission/poisoning/count exhaustion fail closed.
The guard grants no namespace admission, task ownership or restart permission.
This is a permanent publication-gap barrier, not a new persisted format,
second physical writer or replacement for durable task-root discovery.
Protection belongs to the borrowed publisher, not a cross-process file lock;
ordinary-engine enforcement of the single-owner boundary remains U2 work.

Exact current exclusion consumers are `publish_physical_quarantine_excluded`,
`publish_root_retirement_excluded`, `publish_root_reclaim_excluded` and
`execute_sweep_locator_removals`' guarded callback. Under the same root mutex,
each must refuse final publication/removal while staging protection is active.
Request-pin coordinators remain unchanged. Ordinary immutable staging, reads,
and successor publication must remain possible; predecessor-only GC evidence
must not bypass the final barrier. Guard acquisition after a reclaim cannot
restore missing bytes: subsequent source/closure validation is still required.
Keep receipt reconciliation and existing receipt-backed Void settlement intact;
the guard does not retroactively invalidate completed removals or durable uses.

Bind `NativeSemanticCatalogStagingStoreV1` to a borrowed protection guard,
not an unchecked promise from its caller. The repository has six constructor
call sites, all in `migration_execution_spec`; there is no production runtime
caller yet. Its sole physical publisher remains borrowed through the guard.
Update that complete fixture inventory and architecture-owner assertions with
the binding. Do not enable semantic task/control writers or capability25.

Falsifying sequence: add explicit refusing acquisition plus native tests before
implementation; retain that RED. Prove acquire/drop and nested lifetimes at
both hash widths, no long-held mutex, memory refusal/retry, cancellation and
poisoned accounting. Actual native retirement, reclaim, quarantine and sweep
fixtures must refuse at the final boundary, preserve selected state/locators,
and succeed after the final protection drops. Controlled races exercise both
linearization orders with bounded waits. Catalog staging/readback must work
while protection is held, and its borrow must prevent premature release.
Run native authority, lifecycle/quarantine/sweep/Void, compiler/migration,
request-pin and allocation regressions; full platform/static/reference gates
apply. No result here proves durable selected-checkpoint closure across restart;
global discovery, typed GC edges and checkpoint/activation integration remain
mandatory before ordinary service or semantic publication can be enabled.

Candidate1 exposed a necessary receiver correction before runtime binding:
the quarantine, root-retirement and root-reclaim public/observer wrappers take
`&mut self` solely because they pass `self` to `RetirementJournalOwnerV1::flush`.
Their actual final publication already uses the internal authority/KV mutexes.
Use the existing `SharedFirstAuthorityRetirementSinkV1` (already used by mutable
controls and index publication) for all six pre/post flushes, and accept `&self`
in those six wrappers. Keep the exact ordering, receipts and post-commit lineage
failure behavior; do not create another sink or detach the staging lifetime from
its publisher. Existing mutable callers remain valid. The failed candidate is
retained, and the complete native fault/race/lineage suites must qualify this
coupled receiver change before landing.
Existing pre-barrier journal flushes remain allowed. Refusal prevents final
reclamation authority/removal, not every possible journal append; byte-identical
refusal fixtures deliberately begin with drained journals.

Candidate2 exercised743 library cases:742 passed and one new fixture failed.
It reused the initial empty namespace/semantic contents under a new transaction,
so content identity correctly entered the retry path and rejected its missing
publication witness. Candidate3 uses a distinct parent directory referencing
the already-published empty child, asserts a non-idempotent successor, and keeps
that witness validation unchanged. Seven newly unnecessary mutable test bindings
were removed following the receiver correction. Preserve both failed candidates.
Final qualification also includes controlled cancellation/hard-memory-pressure
arrival while acquisition waits for the root mutex, with no retained count or
reservation after refusal, then successful retry. The expanded affected inventory
contains all36 existing `gc_v4_*` targets; native-library tests own the actual
first-authority boundaries, not an invented second reclamation implementation.

This slice is qualified on Linux, macOS and native Windows in the
[September17 staging-protection proof](evidence/user-facing-v4-u1-staging-protection-proof-20260917.json).
All eight new native cases, the augmented GC boundary/race cases, full library,
88 affected targets and platform/static/reference gates passed. Failed scaffolds,
candidates and preflight are retained; the proof does not grant durable retention,
cross-process locking, task activation or ordinary-v4 service readiness.

### Following integration: captured native task inventory

Start only after the staging-protection source is qualified and landed. This
extends the same U1 contract, not the persisted format or service permissions.
The current known-task observation is not global discovery. Canonical controls
are stable-path-keyed FileRecords in KV, and may be absent from the namespace.
The existing captured-header readers still use live KV locators: they are not
a frozen view of mutable control incarnations.

Capture the selected header and a retained `Arc<ReadSnapshot>` under the existing
short root/KV exclusion, borrowing live staging protection from the same owner.
Validate exact database/physical/layout/hash/frontier alignment; release both
mutexes before enumeration or caller callbacks. No implicit flush, new file/KV
owner, root admission or resume permission may be hidden in this read operation.
Keep the snapshot's existing page-generation lifetime/accounting and admit all
additional bounded task scratch. Hold the protection through use; a captured
locator by itself does not prevent physical reuse.

Share the canonical physical read chain (`read_entity_bounded`,
`load_canonical_system_file_at_path`, control-slot loading) with an explicit
captured lookup input. Do not copy the FileRecord/chunk/content/key checks into
another physical decoder or substitute live locators when a capture misses.
Any factoring must preserve every existing live caller and error classification.
Enumerate page-by-page with checked work/read limits, cancellation and explicit
completion. No complete-key/task vector, world-sized dedup set or repeated
prefix scan is acceptable. `visit_captured_slots` already requires flushed
stable slots; `visit_all` additionally understands frozen buffered overrides.
Choose the matching existing snapshot contract deliberately, and test it.

The namespace reader currently bounds an individual FileRecord entity to4MiB.
Control payload caps do not justify skipping larger ordinary FileRecords during
discovery. Bound each read/decode before allocation; crossing the admitted record,
work or memory bound produces typed incomplete/resource evidence, never a
successful empty inventory. Reuse existing v4 `IncrementalDigestV1` if streaming
integrity becomes necessary; the legacy `HashAlgorithm::incremental_hasher`
supports only BLAKE3 and is not a v4 all-profile replacement. Do not introduce a
second framing parser merely to optimize this scan.

Pathnames contain a fixed BLAKE3 digest of kind/identity, not the task ID itself.
Discover a complete canonical A/B pair, derive the selected identity through the
shared selector, then prove its exact path/database/kind binding. Reuse the
existing torn-payload fallback and allocation-error refusal policy; physical
FileRecord/chunk corruption must not turn into fallback or absence. Emit a task
once, for its selected physical slot, rather than retaining a global dedup map.
Released terminal controls may summarize without a checkpoint as the existing
observer does. Unreleased tasks require generation and exact selected checkpoint
identity/digest/state binding before visiting their six declared typed roots.
Declared edges remain obligations, not proof that the complete graph exists or
that a candidate NamespaceRoot has been admitted.

Tests before implementation: a native fixture with two task controls outside the
namespace must enumerate both; a snapshot captured before A/B replacement must
retain the old selected incarnation while a fresh capture sees the new one;
and callback-triggered publication must complete without a held scan mutex.
Use independent control payload fixtures, actual positioned file/KV reads, both
hash widths, and byte comparisons while the publisher remains alive. Extend
coverage to A-only/B-only/equal-sequence/torn/ambiguous pairs, wrong role/path or
database, missing generation/checkpoint, bad binding, unknown protected inputs,
buffer overrides, bounded page generations, early stop/cancellation, read and
allocation failures, oversized records, and retry after pressure release.
Partial callbacks cannot grant a complete retention result. Controlled races
need bounded waits and must release their setup gates before assertions.

Preserve the thirteen known-task observation regressions, snapshot/mark-slot
visitors, selector allocation-failure regressions, native publication/recovery,
all affected GC and compiler/migration targets. Apply the established final-source
native-platform/static/reference gates. Full durable typed-closure traversal,
checkpoint selection/resume and atomic activation remain subsequent coupled
runtime obligations; no semantic writer or capability advertisement is enabled
by a read-only inventory.

#### Captured entry visitor prerequisite — entry31460881

Use buffered captured entries, not fabricated stable mark slots. Inspection of
`DiskKVStore::publish_atomic_visibility_after_authority` confirms that successful
authority publication exposes its delta through `publish_buffer_only`. Therefore
the captured reader must support frozen buffered overrides without a hidden
flush. Extend `ReadSnapshot` with a separately named strict entry visitor; retain
ordinary visitor and flushed-slot contracts unchanged. The new visitor checks
NVT/layout, page CRC/structure, bucket placement/duplicates, buffered map-key
identity and exact effective live count. It reserves no whole-key collection,
charges a caller work limit for each page and raw entry (including deleted or
overridden entries), and checks cancellation before work and after callbacks.
Early stop is explicitly incomplete; late corruption cannot become success.
Its fixed one-page decode scratch must be included in the native inventory's
memory admission. Snapshot lease retention is not physical-entity retention.

Falsifying tests first in the existing `kv_snapshot_spec` target: wrong live
count, cancellation on the final callback and mismatched buffered key must not
return complete. Then cover both widths, page/buffer overrides and tombstones,
wrong bucket/duplicate/NVT layout, callback errors/stops, empty cancellation,
page/raw-entry budget exhaustion, bounded-provider read failure and retained
old page generations while writes continue. Individual fixtures are small and
bounded; existing desktop30-minute stage/resource wrappers remain the outer
timeout. This prerequisite and its native task consumer qualify together;
it does not complete global task discovery by itself.

The first native inventory must not filter by unverified KV role tags and then
claim global task absence. Validate current entity identity/integrity/role in a
single bounded pass, sharing the existing whole-entity decoder and canonical
system-file chain. Caller limits bound each entity and all read bytes (including
canonical dependency rereads); an oversized ordinary entity refuses completion.
This is a current-KV task inventory, not an inventory of every stale physical
incarnation or proof of efficient multi-terabyte startup. Full physical inventory
integration/performance and typed GC closure remain required downstream. Native
lookup factoring must preserve current live-call error classification. Share
optional A/B selection before binding its discovered identity to its canonical
path; do not duplicate or weaken torn-control/allocation failure handling.

Review boundaries: matching scalar header/KV counts do not prove settled
visibility. Authority-bound capture must reject an active atomic visibility
batch or transaction owner without changing the ordinary readers' right to
use their previously published snapshot. A callback's original typed failure
must survive cancellation arriving at the same callback boundary.

Separate retained capture memory from per-visit scratch. The public captured
view can be visited again, including recursively from a callback; simultaneous
visits cannot share one scratch reservation. Each visit admits and releases its
own bounded entity/control/page scratch through the existing memory coordinator,
without a long-held publication mutex or another physical owner. Preserve
retries and retained snapshot accounting on all exits. The dedicated nested
visit test measures the extra charge independently of per-task observation
memory; a compile failure is not its required falsifying result.

### Protected source retention addendum — September17 entry091d0666

The earlier sentence that inputs remain reachable through base/staged trees
does not cover Round9's protected non-HEAD global configuration, aliases and
module archives. Existing ASMC bytes and source fingerprint preimages remain
unchanged; the [durable source capture contract](semantic-source-capture-contract.md)
supplies the missing explicitly enumerable companion closure. It selects two
immutable reader-first controls, ASCM0x0048 and ASCN0x0049, plus capability27,
while preserving unknown24/26 and the existing unknown-kind0x0047 fixture.
The initial ASCR wrapper proposal was discarded in favor of retaining exact
existing content-keyed FileRecords and sharing their actual typed chunks.

The ASCM companion is addressed by task/checkpoint identity and additionally
binds the entire original ASMC envelope digest. Neither decoded paired records
nor a matching digest grants resume, executor, namespace or GC admission. New
publication remains refused until bounded source capture, durable complete typed
closure, checkpoint/resume and atomic activation are independently qualified.
Old codec-only task observations remain inspectable with their original semantics;
no missing companion is turned into a fabricated empty source set or live permit.

### Captured graph integration boundary — September19

The source union, actual guard-bound node/copy staging and selected-task graph
passed [one final-source three-platform qualification](evidence/user-facing-v4-u1-captured-task-integration-proof-20260919.json)
on September19. This does not relax the
remaining task-writer gate. The existing physical owner, captured KV boundary,
canonical control readers, directory validators and semantic catalog codecs
remain the sole owners of their respective representations.

`visit_captured_semantic_task_physical_entries` is a bounded deep inspection:
it binds the selected task/checkpoint/source pair, historical admitted base,
staged namespace, current/pruning catalogs, output state and staged candidate.
It validates ordinary file chunks and whole-file content without retaining a
whole media body. Unknown supported dependency shapes remain retainable without
inventing executor availability; archived module identity uses its existing
content-fingerprint path, not the current alias. A staged candidate does not
acquire invented RootAdmissionCommit evidence.

`visit_captured_semantic_task_metadata_entries` shares that traversal but treats
ordinary chunk payloads as opaque leaves, matching the ratified bounded-mark
decision. Verified FileRecord references resolve through the same captured KV
snapshot. Check key, role, minimum locator length and captured data-region
geometry before presenting the reference. Missing or invalid locators refuse
completion; nonempty files cannot omit every chunk reference. This operation
does not read/decompress/hash ordinary chunk payloads or certify their health.
`physical_reads`/`read_bytes` exclude these leaves; separate
`opaque_chunk_references`/`opaque_chunk_bytes` describe them. Repeated edges
remain repeated work, not an unbounded in-memory deduplication set.

Both operations retain cancellation, cumulative read/work limits, bounded
namespace ancestors/depth/path/chunk workspace and original callback failures.
Callbacks run without the root/KV mutexes and are provisional until successful
completion. Cancellation or pressure at the last callback still refuses a
summary. Neither a successful summary nor a caller's accepted callback grants
a durable GC mark, compiler resume/admission, namespace admission or activation
permission. The capture must retain its existing staging protection.

Source/control/catalog bodies still require their exact bounded readers. The
metadata operation is consequently not a cheap commit-time validator, a global
startup inventory optimization or the final spillable/checkpointed GC frontier.
Those owners must preserve zero-content-read existing-chunk commits and the
full bounded-mark contract during U2/U3/U5 integration. Resource qualification
does not claim universal recovery from inherited inner allocation failures.
Actual durable checkpoint publication, retention discovery across restart,
compiler-owned cursor restoration and atomic task/generation/HEAD activation
remain mandatory before ordinary task publication or capability advertisement.

### Compiler continuation prerequisite: bounded first binding — September19

`SemanticCatalogReaderV1::with_first_record` selects the first binding in a
previously admitted nonempty catalog using the existing exact/ordinal descent.
Order is catalog lookup digest then full key, not raw dependency ID; no ordinal
becomes persisted identity. It reads at most H+1nodes and retains one body plus
H-sized path metadata. The caller admits scratch/callback output and retains
physical protection. Untouched subtrees still require prior complete admission.

The [three-platform proof](evidence/user-facing-v4-u1-catalog-first-record-proof-20260919.json)
preserves original lookup tests and the initial failing targets; it covers all
registered hashes, independent Patricia ordering/COW progression, source and
allocation refusals, cancellation/error priority, and old/updated catalog reads
after actual native close/reopen. It does not implement durable pruning or
resume. A later remaining-candidate-root strategy may use cursor-kind0 during
Pruning, already allowed by frozen ASMC bytes, but must select both updated
catalog roots only after a complete bounded step and prove their typed closure.

Partial catalog admission must remain separate from Complete state admission.
Current configuration count can differ from final expected count while compiling;
ASMC mutation_count is an accepted logical-operation count, not a processed-
configuration cursor. Candidate dependencies must match exact main bindings;
Pruning candidates must additionally be unused. Neither those graph checks nor
plausible counters prove source-position correctness or grant task ownership.
Source/cursor binding, fences, retention and atomic activation remain mandatory
in the enclosing continuation owner.

### Compiler continuation prerequisite: partial catalog admission

`admit_semantic_catalog_progress_v1` accepts an exact ASMC envelope, the captured
compiled parser registry, compilation request and bounded object source. It
uses the existing checkpoint decoder and checks supported compiler/registry
profiles and expected final configuration count. Only nonempty post-registry
Compiling and Pruning snapshots are admitted here; Pruning additionally requires
the actual configuration count to equal the final count.

The operation shares Complete admission's full tree, actual count, typed
definition/ownership and reachability proof. Candidate records must be dependency
classes6/7 with exact matching main-catalog bindings; every otherwise unreachable
record must be accounted for. Compiling may still have live candidate dependencies;
Pruning candidates must be unused. Complete admission has no candidate allowance
and remains strict. No Complete semantic state is synthesized for unfinished work.

The non-Clone `AdmittedSemanticCatalogProgressV1` owns charged metadata and exposes
borrowed main/candidate snapshots, phase and counts. It does not grant source-
position correctness, task ownership, fencing, executor availability, durable
retention, namespace admission or publication. Physical protection and bounded
reads remain the caller's responsibility. Captured, Ready and Activated require
their own enclosing runtime decisions, not this partial-catalog operation.

Independent unit fixtures construct ASMC and Patricia bytes. Native composition
tests separately stage unused dependency catalogs through the existing guarded
physical owner, close/reopen and check read-only admission/refusal/retry at all
five hashes. Their in-memory checkpoint identities are not a selected durable
task. Actual allocator tests target decoder identity, both retained root copies
and the reachability bitmap; inherited infallible digest allocations are not a
claim of universal host-OOM recovery. Full task continuation remains unfinished.

The [September19 three-platform proof](evidence/user-facing-v4-u1-catalog-progress-proof-20260919.json)
qualifies these exact catalog-only inputs on Linux, macOS and native Windows,
including the unchanged strict Complete admission regressions. This closes the
partial-catalog prerequisite, not the enclosing continuation/retention owner.

### Compiler continuation prerequisite: exclusive namespace source seek

`open_namespace_configuration_cursor_after` is an additive constructor on the
existing captured inventory reader. Its exclusive lower bound is the canonical
configuration **file** path, not its owner directory. The bound may be absent,
including an absent/non-directory ancestor; its lexical suffix is still defined.
Global protected configuration is not a namespace input. ASMC's configuration-
owner cursor must be converted to the full file path by the enclosing compiler
owner; owner ordering alone differs around punctuation before `/`.

The constructor seeds only matching directory ancestors using the existing
validated seek/load chain. The existing successor cursor then reads the suffix
with one cumulative work/read budget, bounded stack and charged path copies.
Canonical/family/byte/depth limits, inherited B-tree ranges, captured-only
locators, cancellation and pressure checks remain enforced. Original from-start
and paired-source-union traversal are unchanged. A cursor error is terminal;
retry requires a fresh cursor and cannot reuse partially advanced state.

Seeking deliberately does not read or validate skipped configuration bodies.
A successful suffix therefore grants no processed-prefix equivalence, catalog
progress, task ownership, retention or activation permission. The captured
inventory and staging-protection lifetimes remain mandatory. This introduces
neither a persisted cursor encoding nor a second physical or traversal owner.

Qualification uses native disposable files, independently sorted full paths,
present/deleted/absent bounds, punctuation/UTF8, malformed branches, missing or
corrupt successors, cancellation/pressure and shared budgets. A late seek must
finish within768work units while enumeration from the beginning actually fails
that same limit. A missing skipped body is not reread or falsely certified.
Root-copy allocation injection explicitly initializes existing registry
singletons first; the isolated cold-registry failure is retained and does not
become a universal or cold-start allocation-recovery claim. The
[September19 exact-source proof](evidence/user-facing-v4-u1-namespace-cursor-seek-proof-20260919.json)
qualifies this primitive on Linux, macOS and native Windows. All twelve new
cases also pass individual ten-second Linux deadlines (slowest656ms). Catalog
continuation, source-prefix binding and durable task integration remain open.

### Catalog-only compiler continuation — September19

`SemanticCatalogContinuationV1` owns the existing compiler and auxiliary COW
candidate tree; it is not a second semantic or physical writer. Fresh start
stages the registry. Incremental start uses the existing opaque Complete-base
inheritance and exact registry check. Restart consumes catalog-only partial
admission, checks the same hash/final-count/capability request and exact registry,
and admits a fresh32MiB lease to the supplied memory coordinator. Both the old
admission lease and new lease are charged during rebinding; the old lease is
released on success or refusal, never silently moved between budgets.

Configuration application, phase transition and single-dependency pruning
consume the continuation. A failure cannot return reusable half-updated work.
The preceding immutable checkpoint remains the retry point, and the enclosing
owner must retain its objects and select the main/candidate root pair together
only after the complete step succeeds. Snapshot getters grant no durable
selection, physical retention, task fence, source position or HEAD authority.

The old ordered bulk updater and the continuation share configuration mutation,
base inheritance and live-dependency exclusion. Mutation-count and final-count
checks remain in the bulk API. ASMC `mutation_count` still counts accepted
logical operations, not configurations processed by this catalog primitive.
A changed parser registry requires fresh composition; source-owner decisions
about changed aliases or other protected inputs remain outside this primitive.

`finish_configurations` checks the final configuration count, completes the
existing live-candidate exclusion pass, then enters Pruning. Do not select an
intermediate exclusion result. With no nominated candidates it performs no
catalog scan. `prune_one` finds one remaining binding through bounded tree paths,
checks its exact main binding, removes both entries and returns only after both
updates succeed. Removing the final dependency can reuse the registry leaf and
empty the candidate root without publishing any new object. `finish` requires
Pruning with no candidates and emits an unselected Complete semantic state.

Qualification must include independent expected bindings/Patricia bytes,
fresh-composition parity, Complete admission, drop/admit/continue at each phase,
shared live dependencies, every observed read/publication fault in six step
fixtures, sampled cancellation boundaries, per-publication memory pressure,
numeric no-rescan bounds and native close/reopen at all registered hashes.
Targeted allocation probes cover both partial-root copies after registry
initialization, not universal host-OOM recovery. Native fixtures keep HEAD
unchanged and hold ASMC bytes in the harness; they do not prove selected durable
task recovery. Source-prefix equivalence, task publication/retention discovery,
fencing and atomic activation remain required enclosing runtime work.

The [exact-source continuation proof](evidence/user-facing-v4-u1-catalog-continuation-proof-20260919.json)
qualifies this catalog-only slice on Linux, macOS and native Windows. All21new
cases passed individual10second Linux deadlines, slowest2019ms. Full affected
and library gates, strict formatting/Clippy,185reference tests and502independent
fixtures passed with all raw evidence local. Earlier fixture, duplicate-module
and formatting failures are retained as history, not waived. This closes the
catalog stepper prerequisite, not the enclosing source/task integration.

### Retained-side prepared compiler inputs — September19

`prepare_captured_semantic_alias_snapshot` prepares the existing parser/mapper
borrow interfaces from one explicitly selected Base or Requested source catalog.
It validates the ASCM/ASMC pair even for empty or unused inputs, then uses one
catalog operation for the whole preparation. An unlisted required alias or
artifact is an error; explicitly captured alias absence remains absence. Neither
case consults the current path. Present revisions require their exact retained
FileRecords, canonical paths, chunks and plugin identity checks.

Current and retained preparation share the original two-pass alias discovery,
role deduplication, bounded table construction, fallible dependency copies and
borrowed resolution. Only names and dependency records survive preparation, not
module bodies or catalog buffers. The existing staging/capture lifetime remains
required; cancellation or memory pressure refuses preparation and later borrows.

The minimum catalog/plugin read-byte ceiling covers the companion/checkpoint,
catalog traversal and every unique alias/module pair cumulatively. Catalog work
also spans the whole operation. Body/chunk/count ceilings are intersected with
the selected source owner's limits. Existing point/pair readers keep their
original bounds and error classification; a raw source-read limit does not
become a catalog-read limit merely because selection used a catalog.

This adapter accepts caller-owned configuration bytes. It does not bind those
bytes to a source revision, prove whole source-union completeness or processed-
prefix equivalence, select a task, retain objects across restart, fence a writer
or activate HEAD. Those remain explicit obligations of the enclosing source/task
owner. The internal selected-reader callback grants no additional authority.

Qualification includes real registry/configuration compilation and native reopen
at all five hashes; independent expected dependency bytes and physical budget
costs; absence/unlisted/missing/misbound/malformed input; physical integrity;
large-module release, actual dependency-copy allocation refusals, operational
bounds, cancellation, pressure and retry. Allocation injection targets the two
final dependency copies, not universal inherited allocation recovery. Existing
current-snapshot late-completion tests exercise the unchanged shared checks.

The [exact-source retained-alias proof](evidence/user-facing-v4-u1-retained-alias-proof-20260919.json)
qualifies this adapter on Linux, macOS and native Windows. All ten new cases
passed individual ten-second Linux deadlines, slowest400ms. Linux passed1617
library/affected cases; Mac passed1032library and501affected; Windows passed1047
library and501affected. All three passed185reference tests,502independent
fixtures and their required format/static gates. Every raw artifact is local;
the original RED, two incorrect new-test error expectations and new-fixture
Clippy failure remain recorded. No runtime relaxation was used to resolve them.
This closes retained alias preparation only, not source-prefix admission,
durable task selection/recovery, activation or ordinary-v4 service readiness.

### Retained source-union validation — implementation entry18111638

Before restoring compiler progress, validate its declared retained source set
through the captured native inventory. Add
`validate_captured_semantic_source_union(task_id, checkpoint_sequence, bounds)`
as a read-only observation, not a task-selection, compiler-prefix or GC permit.
Its summary reports protected and namespace union paths, actual base/requested
configuration counts, total physical read bytes and separate cumulative catalog
and namespace work. No source bodies survive in the summary. Physical protection
and the exact captured view remain the enclosing caller's responsibility.

The validation bounds contain the existing catalog and namespace bounds plus
module/workspace, aggregate alias-occurrence and fingerprint-workspace ceilings.
The catalog read-byte budget covers controls, root metadata, catalog/source reads
and both namespace passes together. Namespace reads also obey their existing
local ceiling; logical catalog and namespace work keep separately named limits.
Do not reset a quota for each alias, configuration, tree or fingerprint pass.

Bind one immutable ASCM/ASMC pair and completely validate both protected catalogs
using their existing ordered reader. Resolve the historical admitted base through
root/state/admission metadata validation, without fabricating current HEAD or
loading an unbounded directory representation. Share the old metadata rules and
preserve the existing full-reader and live-HEAD validation/error ordering; use
the bounded namespace reader for both base and staged DirectoryIndex trees.

Use the existing schema alias visitor and selected plugin pair reader to require
every referenced alias and module on both retained sides. Explicit alias absence
is preserved; Unlisted is an error, never current fallback. Validate every
declared protected row. Additional declared paths may represent explicitly named
unchanged or unused replacements; do not infer or discard the original accepted
logical operation list from the minimum set of configuration references.

Merge the already ordered protected catalog and paired namespace streams to
reconstruct the frozen BASE source fingerprint, including explicit absence for
requested-only namespace paths. No new scratch format, world-sized set or
per-path namespace restart is needed. Count actual requested configurations,
including the protected global index source separately, and compare ASMC's
expected final configuration count. Namespace union count is not that count.
Absent existing global configuration remains absent, not a new-database default.

The initial failing-first native targets cover empty/absent inputs and changed
base/requested configurations after actual close/reopen and current authority
advance. Independent ordered-map/preimage expectations precede implementation.
Required additions include all five hashes, retained plugin revision differences,
omitted required aliases/modules, incorrect namespace-inclusive fingerprint,
wrong final count, malformed/missing root admission and source/namespace data,
late cancellation, memory/budget refusal, retry and complete reservation release.
Retain original current-source, namespace, graph, catalog and compiler fixtures.
Use individual ten-second native-fixture deadlines and final exact-source
Linux/macOS/Windows, static/reference/architecture qualification before landing.

This slice does not choose fresh/incremental compilation, certify a saved
configuration cursor, select a durable checkpoint, advance a task fence or
publish HEAD. Those remain coupled following source-prefix/task work. All
generic semantic-control publication refusals and capability advertisement stay
unchanged. No production database or service operation is part of this unit.

The [three-platform qualification](evidence/user-facing-v4-u1-source-union-validation-proof-20260919.json)
now covers fifteen native validation cases, independent source preimages and
physical-read/work ceilings, real reopen/current-authority advancement, both-side
plugin membership, metadata/source failures and cancellation/resource release.
The exact105-input candidate preserves the existing full-root and source-reader
regressions and1501audit reviews. This completes only the retained source-set
observation prerequisite; source-prefix and durable task authority remain open.

### Retained compiler-prefix admission — entry28248ec6

Before the enclosing task owner restores Compiling or Pruning, compose retained
source validation with existing catalog-progress admission and exact class1
source-projection comparisons. Add
`admit_captured_semantic_compiler_progress(task_id, checkpoint_sequence, bounds)`.
Its opaque non-Clone output owns the requested compiled registry, admitted
catalog progress, derived request and completed source statistics while borrowing
the captured inventory. Consuming catalog parts strips that wrapper and supplies
only the existing catalog primitives. No task selection, durable retention,
writer fence or activation is granted. Other phases use their own owners.

One ASCM/ASMC pair, historical base metadata and cumulative source/namespace
operations cover the whole proof. Semantic object reads share the same captured
physical-byte/work meters and canonical kind/identity reader. Compilers, retained
alias snapshots and semantic decode scratch receive separately named workspace
ceilings on the same coordinator. No full-map load, per-source budget reset or
COW write replay is permitted. Revalidate once per restart, not after every
in-process compiler step.

Runtime construction order is the global configuration slot first, followed by
namespace configuration full-file paths in byte order. A Compiling None cursor
means before the global slot; owner `/` means that slot was processed even when
absent. A nonroot owner must name a member of the BASE/REQUEST namespace union.
Processed owners must equal requested compiled projections; unprocessed owners
must equal BASE projections for incremental work, or be absent for fresh work.
Compare IDs and canonical definition bytes, and independently count expected
bindings to exclude extra class1 entries. Pruning requires the entire requested
configuration set plus existing exact unused-candidate closure. Its remaining
candidate root determines forward progress; a codec-valid dependency-ID cursor
is informational, not proof of any historical deletion or ordinal. New writers
use the previously qualified None-cursor pruning strategy.

The following task writer and this admission owner share one deterministic
mode rule without new ASMC bytes. Nonempty Complete BASE catalogs must first
pass existing strict Complete admission with retained BASE registry/counts;
unsupported producer profiles or malformed catalogs cannot fall back to fresh.
The exact unchanged compiled registry permits incremental work; registry change
requires fresh construction. Content-only and valid Complete-empty bases use
fresh construction. Complete-empty with actual BASE configurations is invalid.
Request hash comes from capture, final count from ASMC and required capabilities
from the admitted BASE state; old catalog/compiler capability checks remain.

Failing-first proof must admit a correct global-first prefix after native
close/reopen and unrelated current-input change, while refusing a same-count
wrong projection that the old independent source/catalog admissions accept.
Cover all hashes, cursor/order/deletion/alias/mode cases, malformed and missing
inputs, exact cumulative quotas, actual allocation refusal/retry, cancellation,
pressure and reservation release. At least one admitted continuation must
finish and reopen with an independently expected final catalog. Ten-second
individual native deadlines and final Linux/macOS/Windows/static/reference gates
precede landing. This is an implementation contract, not a completed proof.

Qualification September20: the exact110-input C9 snapshot passed the combined
[three-platform compiler-prefix proof](evidence/user-facing-v4-u1-compiler-prefix-proof-20260920.json).
All23native cases pass on Linux, Mac and Windows; Linux additionally enforces
individual10second deadlines (slowest4566ms). The real continuation/finish/reopen
case covers all five hash algorithms and independent final owner keys. Tests
also cover changed aliases/registry, extra bindings, missing/damaged catalogs,
unsupported profiles, source positions, shared exact quotas and operational
refusal/retry. Actual allocation injection is qualified at control-body loading,
not every possible inherited allocation site. Original behavioral RED bodies,
canonical object read/check order and existing source-validation checks remain.
Linux passed1673library/affected; Mac1070library; Windows1085library. All native
platform gates,185reference tests,502independent fixtures and1501unchanged audit
reviews are source/binary-bound in the proof. This closes read-only compiler
prefix admission only. Durable task selection, retention, fencing and atomic
activation remain required; no generic writer or runtime capability was enabled.

### Native initial checkpoint dependencies — entry8e8543be

The following bounded integration derives and stages ASMC/ASCM from an opaque
`NativeStagedSemanticSourceUnionV1`. It is immutable unselected work, not a
selected task, durable retention grant or activation. The enclosing task owner
still owes selection, replacement, fencing, GC retention and atomic activation.
The only caller inputs are task ID, accepted mutation count, capture/publication
times and workspace ceiling. Derive original physical/header/generation/base,
requested tree, paired source catalogs/counts and source identity from that
qualified union. Derive requested configuration count during its existing
bounded global/namespace walk, and use the frozen current compiler/registry
profiles. Sequence1/Captured has no compiler output or cursor.

Share the guard-bound native source-control staging owner for canonical wrappers,
exact idempotency, dependency batching and original committed-error receipts.
No generic semantic-control publication bypass, second physical owner or runtime
capability is permitted. Same exact pair at a later publication time is a
byte-stable retry; the same immutable identity with different bytes must refuse.
The capture's protection remains necessary after staging; reopening unselected
dependencies never manufactures a resumable task.

First execute native failing-first stage/readback/reopen tests against a callable
refusal. Independently construct the control envelopes and test all hashes,
source counts/history, invalid requests, resource/cancellation and physical
ownership failures, interruption and exact retry. Keep existing source-node and
generic writer refusal regressions and final three-platform qualification.
This entry is a contract, not a completed native checkpoint-staging claim.

Qualification September20: the exact113-input C4 snapshot passed the
[three-platform checkpoint proof](evidence/user-facing-v4-u1-captured-checkpoint-proof-20260920.json).
The13native cases cover independent control envelopes across all five hashes,
requested configuration counts, retained historical inputs, exact and partial
retries, concurrent publication, invalid requests, allocation/cancellation/
pressure refusal, owner drift, committed receipts and actual close/reopen.
Linux passed1686library/affected tests and13individual ten-second deadlines
(slowest391ms); Mac1083library and Windows1098library. All platforms passed
185reference tests,502independent fixtures and required static/format gates.
The proof retains the initial test-helper compile failure, actual behavioral
RED and C3's single architecture-assertion failure on all three platforms.
C4 corrects that assertion to permit only the private derived dependency pair;
runtime remains identical to C1, original behavioral RED cases unchanged.
No persisted layout, generic writer or capability advertisement changed.
This closes initial dependency staging, not selected-task retention, fencing,
resumption or activation. Those and U2–U7 remain required.

### Bounded metadata-only native task discovery — entry0342eac4

The existing `visit` remains deep physical inspection. Add separately named
`visit_metadata` with the same provisional task-count/completion result; neither
entry grants retention, resumption or task selection. Completion means the
captured KV inventory was exhausted, not integrity verification of opaque
non-FileRecord payloads. Existing callers and error behavior remain unchanged.

Factor WholeEntity header validation into its current decoder owner, preserving
full-decoder validation order and diagnostics. The metadata physical read remains
under first-authority/captured lookup ownership: fixed bounded header/key reads,
CRC and component/total lengths, kind/version/hash/codec/reserved bytes, sequence,
captured physical extent and exact locator/key/role binding must be checked before
classifying the entry. Header integrity alone never certifies payload integrity.
Fully read all FileRecords and all canonical control dependencies through existing
owners, including ordinary large FileRecords under their current refusal policy.
Use one snapshot and cumulative actual physical-byte/work budgets, separately
admitted simultaneous scan scratch; no unchecked KV-tag filtering, live fallback,
complete-key map, hidden flush or second framing parser.

Retain the reproduced deep-read characterization and first execute a callable
metadata RED: one small task fits64KiB, then adding an ordinary256KiB chunk must
not prevent exact metadata discovery under that budget. Exercise all five hashes.
Unrelated payload damage may remain opaque for metadata but must fail deep
inspection; malformed headers, FileRecords and task dependencies must still fail.
Add independent framing/diagnostic fixtures, exact quota boundaries, truncation/
EOF/extent/identity/role failures, history/concurrent replacement, callback error/
cancellation precedence, early completion, nested memory, allocation refusal and
retry. Preserve original deep-inventory regressions. Include first-authority
architecture in preflight, then final affected/static/reference/native-platform
gates. This is discovery infrastructure, not the following graph-to-mark owner.

Qualification September20: the exact120-input C6 snapshot passed the
[three-platform metadata-discovery proof](evidence/user-facing-v4-u1-task-metadata-proof-20260920.json).
All12native cases pass on Linux/macOS/Windows; Linux additionally enforces
individual ten-second deadlines (slowest1309ms). Linux passed1825library/affected
tests, Mac1096library and Windows1111library; both native platforms passed645
affected and257narrow tests. Every platform passed185reference tests,
502independent fixtures and required static/format gates. The audit preserves
1501reviewed occurrences; three existing integer-conversion mappings changed
scanner identity after moving into the shared header decoder, with exact old
patterns and review rationale retained. Original deep-reader order/diagnostics,
integrity checks and behavioral RED bodies are mechanically preserved.
The proof retains the initial fixture compilation failures and the later typed-
cancellation assertion correction. No runtime changed after passing C3.
Metadata completion still says nothing about opaque payload integrity, global
mark closure, durable task ownership or activation. Those and U2–U7 remain open.

### Captured all-task retention traversal — entry452d8dc5

Compose the qualified metadata discovery and known-task metadata graph through
one separately named read-only operation on NativeSemanticMutationInventoryV1:
`visit_captured_semantic_task_retention_entries`. Stream selected task graphs
against that same captured header/KV history without collecting task identities.
Physical callbacks may repeat; every callback remains provisional until complete
success. Return task count and consumed work/read-byte statistics, not a durable
mark, task-selection, release, resume, namespace or activation permit. Released
terminal tasks keep their existing control-only interpretation; unreleased tasks
require the complete existing source/namespace/catalog/output graph.

NativeSemanticTaskRetentionBoundsV1 supplies total maximum_work and
maximum_read_bytes plus the existing per-graph bounds. The enclosing limits are
intersected with the capture's limits; local per-body/depth/workspace bounds
remain enforced. Physical bytes count discovery prefixes, every full read and
all graph/source reads, including repeats. Work is the sum of original inventory
page/raw-entry units, graph steps and source-catalog steps, including tombstones,
overrides and source steps that do not read payloads. A source read contributes
to its existing graph and catalog work categories, but physical bytes are charged
exactly once. Admission precedes actual work/I/O; no per-task reset or post-hoc
ceiling enforcement is allowed.

Preserve the existing public single-operation entries, diagnostics and quotas.
Use private optional admission around their existing readers and charging sites,
not new parsers, an allocating shared counter, or another captured lookup owner.
The KV entry visitor's existing page/raw-entry charging order and cancellation
checks must survive factoring. Callback errors retain precedence over simultaneous
cancellation/pressure; incomplete/error returns grant no usable partial result.
Nested operations separately admit simultaneous scratch. No implicit KV flush,
live fallback, world-sized set, bitmap, control write or runtime capability change.

The test-only characterization proves two individually valid selected graphs
cover26independently enumerated locators and exceed one per-task byte limit when
combined. Its first attempt refused nested scratch at64MiBsoft/96MiBhard;
using256KiB entity scratch for the small fixture passed without raising memory
limits. Keep that refusal as history and qualify insufficient-workspace behavior.

First execute callable REDs for two-task discovery/graph composition and exact
combined ceilings. Cross-check read statistics against the independently qualified
discovery threshold plus both individual graph counts. Add all-hash, empty,
released/A-B/history, later-branch failure, header/opaque-payload, page/tombstone,
callback/concurrency, cancellation, memory/allocation/retry and cumulative-source
work cases. Preserve captured KV, inventory, graph, source catalog and architecture
regressions; preflight their exact targets before final native-platform gates.
Individual native deadlines and final Linux/macOS/Windows/static/reference proof
precede landing. The global flushed-slot mark owner, durable retention/recovery,
task selection/fencing and atomic activation remain following obligations.

#### September20 discovered live-count dependency

Retention C3's later-task failure test successfully refuses the missing companion
after the first complete graph, but retry after restoring its locator fails:
28effective live entries disagree with the snapshot's recorded27. The unchanged
DiskKVStore::insert counts key presence rather than live/deleted transitions.
Source tracing finds the same distinction in atomic staging, bulk insertion and
flag deletion. Its separate unpublished buffer-only rebuild path also counts
new tombstone rows as live. This is a physical KV bookkeeping dependency, not a
reason to weaken captured-inventory count validation or discard the retry case.

Extend this landing to a shared checked live-count transition and direct
failing-first regressions. Preserve ordinary publication, atomic invisibility/
abort, bulk's current-layout reads/deferred publication, and buffer-only's
no-page-read rebuild contract (including the corrupt-page preservation test).
For missing/live/deleted prior entries and live/deleted replacements, only
effective live membership changes the count. Repeated updates must be idempotent;
old snapshots retain their prior counts/content; reopen must agree with the
published view. Overflow/underflow must refuse before changing that entry.
Keep original count/deletion/snapshot/migration regressions, add disk-KV and
concurrency/resize targets to preflight and all native final gates. This corrects
bookkeeping only: no new task writer, automatic repair or production operation.

#### Qualification — September20

The frozen131-input C7 candidate and its Linux/macOS/Windows evidence are bound
by `evidence/user-facing-v4-u1-task-retention-proof-20260920.json`. All24new
native cases pass, including the original two composition REDs, five direct KV
transition REDs and later-task restoration retry. Linux passed1988library/
affected and659older-consumer cases; macOS passed1120library/784affected/
659older consumers; Windows passed1135library/784affected/658older consumers.
Native narrow,185reference,502fixture and format/static gates also pass.
The24individual Linux deadlines peaked at477ms under the unchanged resource
floors. All raw evidence is retained locally; failed scratch admission, original
count failures, formatting and malformed audit-packet attempts remain history.
The final audit keeps all1501reviewed occurrence identities and changes only
their line metadata. This qualifies read-only composed traversal and the scoped
KV bookkeeping correction, not durable task publication or global GC completion.

### Captured task bitmap contribution — following retention integration

`NativeSemanticMutationInventoryV1::mark_captured_semantic_tasks` composes its
qualified all-task visitor with the existing captured-slot validator and dense
bitmap. The opaque result borrows the same inventory and staging-protection
lifetime. It is only this capture's task contribution: no global completion,
task selection, resume, release, publication, activation or reclaim permit.
Public result access is read-only summary/bitmap bytes/full-locator membership.

Reject every nonempty captured KV buffer, even with zero tasks. Never flush,
recapture current state or create another physical owner. The enclosing global
run-start owner still must explicitly flush and capture its complete frontier.
Each callback resolves its key in the retained snapshot and compares flags,
hash, offset and length before marking; a same-key replacement is not the same
incarnation. Duplicate references are charged but mark idempotently. Any failure
drops the provisional bitmap rather than returning partial completion.

Bounds retain the existing cumulative discovery/graph/source work and physical
read admission, adding positive maximum_slot_lookups and maximum_slot_page_bytes.
Each resolution charges one full selected-algorithm page before lookup, including
cache hits and repeated references. Report these as logical page bytes, not
measured disk I/O. Reserve the entire bitmap and checked four-page decode scratch
through the same coordinator before traversal. Check cancellation/pressure at
each lookup and at successful completion; retain only bitmap memory afterwards.
Membership queries separately admit bounded page scratch, preserve build counters,
and return false for absent/deleted/different locators. Corrupt layout, malformed
hash width and interruptions remain typed errors. Preserve original graph,
bitmap, observation and integer-conversion causes across callback unwinding.

Characterization independently proves26expected physical locators and the
same-key/same-slot/different-offset hazard. Callable REDs precede implementation;
bitmap bytes are compared with a separately enumerated captured-slot oracle,
not only internal counters. Coverage includes exact combined limits, all hashes,
empty/released tasks, buffered refusal, historical replacement, native reopen,
later-task failure/retry, allocation refusal, simultaneous reservations and
deterministic first/final interruption. A scoped test-only observer invokes the
same build operation; the public entry supplies no callback. The bitmap's
zero-production-caller guard is handed off only to this named adapter; forbidden
service/control/reclaim ownership remains checked. Original reader/KV/bitmap
tests survive. Individual deadlines and final native/static/reference proof
precede landing.

This does not persist a bitmap or qualify durable task discovery after process
restart, GC mutation convergence, task fencing/checkpoint selection or atomic
task/generation/HEAD activation. Those enclosing integrations remain required.

Mark qualification:137-input C4 passed the final native gates, including14new
cases, Linux2002library/affected, macOS1134library/784affected, Windows1149library/
784affected,185reference and502independent fixtures. Individual Linux deadlines
peaked348ms. Windows completed September20,07:53:27UTC; all raw evidence is local
and checked. The [combined proof](evidence/user-facing-v4-u1-task-mark-proof-20260920.json)
records the executed characterization, REDs, corrected audit failure, immutable
source/binary identities, bounds and retained parent-only KV consumer evidence.
This remains a read-only task contribution, not durable final reclamation
enforcement or user-facing production readiness.

### Native root-task exclusion — following captured task marking

Two executed native REDs demonstrate the gap: a reopened unreleased ASMT marks
its base root correctly, but the existing final root retirement accepted only a
supplied external absence claim; task-capable physical reclaim likewise lacked
native task evidence. Process-local staging protection is not durable task
absence. This unit extends both existing final root boundaries, not the task
writer or ordinary runtime.

`V4FirstAuthorityPublisher::qualify_semantic_task_root_exclusion` qualifies one
nonzero database-width root against a completed captured task contribution. Its
opaque result borrows the independently supplied publisher, retains the captured
A/B header region, dynamic root identity, cancellation and a small accounted
reservation. It retains no bitmap or snapshot and may outlive capture/protection,
but cannot outlive or cross the publisher. Resolve the target in that capture's
validated slot; refuse a marked root or a key with another physical role.
An absent key is permitted by this task-only predicate, not proof of root
admission or completed reclaim. Every existing lifecycle/retention check survives.

Both native root final operations require this evidence if either header mask
declares capability25. Old headers without that declaration retain their prior
no-proof contract; any supplied proof is always validated. Under the existing
root lock, check its cancellation/reservation, exact publisher, target and A/B
region before any lifecycle authority change, including exact retries. A later
publication invalidates it even if the target locator or logical HEAD stayed
the same. Do not recapture under the final lock, detach bits, invent another
publisher, silently accept an external absence boolean, or block unrelated roots
merely because some task exists.

The existing pre-final retirement-lineage flush can invalidate prior evidence;
settle pending lineage before capture or capture again. After a completed
publication, exact retry likewise needs fresh evidence while retaining normal
idempotency. Original typed mark/observation/memory causes remain nested in both
root-publication errors and their shared conversion; pre-commit refusals have
no committed receipt. Existing post-commit outcomes are unchanged.

The target cases retain independent task/checkpoint envelopes and real native
publication/reopen. They cover protected, unrelated, absent and released task
states; both root final methods; copied identities with different publisher
owners; changed targets/frontiers; new task selection after an empty capture;
both capability masks; all registered hash widths; malformed identifiers/roles,
allocation refusal/retry, bounded retained memory and separate proof interruption.
Full prior native root/GC and captured-reader regressions, per-case deadlines,
static/audit and final three-platform gates remain required before landing.

Limitations remain explicit: fixture task controls are seeded because there is
no admitted task writer yet. These low-level root tests are not the complete
service GC pipeline. Quarantine/sweep target qualification, unselected checkpoint
graph validation and coupled task/generation/HEAD publication remain mandatory
following work; no capability advertisement or production action is enabled.

This conservative per-target frontier is not a bulk-GC throughput claim. Each
intervening publication invalidates it; a service loop must not silently rebuild
the whole task graph for every root and call that scalable convergence. U5 must
qualify the batching/convergence path and its measured work bounds. Do not relax
the frontier or reuse detached marks to obtain that performance.

Qualification September20: all141frozen C6 inputs passed the final Linux,
macOS and Windows gates. Linux passed2049library/affected cases and16individual
ten-second deadlines (maximum261ms); macOS1150library/815affected/257narrow;
Windows1165library/815affected/257narrow. All platforms passed185reference tests,
502independent fixtures and their required static/format checks. The error audit
retains1501reviewed identities; C6 changes only13line locations from green C5.
Windows completed09:42:07UTC and its sole six-minute collector completed09:44:19.
All raw evidence is local and checked by the
[combined proof](evidence/user-facing-v4-u1-task-root-exclusion-proof-20260920.json).
It retains the executed REDs and intermediate fixture, architecture and format
failures. No task selector, quarantine/sweep completion or service readiness is
implied by this independently qualified root-retention milestone.

### Native physical task exclusion — following root exclusion

This unit extends the existing quarantine-selection and sweep-removal final
boundaries. Like the landed root proof, its opaque evidence retains the exact
publisher, captured A/B region, artifact kind/key, cancellation and a small
accounted reservation, not the captured bitmap or snapshot. It is a task-only
predicate; all existing complete-mark, grace, lifecycle, pin, incarnation,
durability, receipt and removal-authority checks remain mandatory.

Quarantine qualification validates its complete effective candidate stream:
ordered base pages plus ordered immutable Set/Clear deltas, latest operation
winning for an identical physical incarnation. Reuse the existing closure
validator and support loader/postorder walker. A bounded heap of borrowed delta
cursors merges candidates without a whole-candidate map or repeated full-delta
scan per candidate. Charge input rows and identity comparisons, cap immutable
delta bodies/counts at their frozen bounds, and check cancellation and memory
throughout. Any malformed closure or callback failure invalidates the result.

Sweep qualification visits every candidate of the exact decoded proposal. Both
paths check the actual physical prefix at the proposed offset before consulting
captured-slot task membership: key, type, typed version, write sequence,
integrity field and extent must agree. Factor the existing checked metadata
reader; do not replace an older proposed incarnation with today's live locator.
These prefix checks are not a full payload-integrity verification. Legacy
physical framing and gap candidate classes not admitted by that shared reader
remain refused; their enclosing U5 maintenance integration is still required.

Use dynamic registered digest width throughout. A typed version0 body may have
a nonzero v4 write sequence, as the independent frozen WholeEntity fixtures
already demonstrate. Preserve the legacy zero-sequence/nonzero-version refusal;
do not change fixture bytes, hash domains or row widths to reconcile consumers.

At each final native boundary, require evidence when either capability mask
declares bit25; always validate supplied evidence. Under the existing root lock,
check owner, artifact kind/key, exact frontier, cancellation and memory before
external callbacks and before an exact retry can return. Intervening publication
invalidates prior evidence. Fresh evidence may restore a legitimate idempotent
retry; it cannot bypass prior quarantine or sweep authority.

Verification must retain the failing-first missing-guard and version/framing
regressions, independent merge oracle, protected versus unrelated candidates,
both fixture widths/all registered hashes, older/current incarnations after
reopen, malformed identities/graphs, exact resource/work limits, cancellation,
allocator refusal and complete release. Independently bound every added case.
Preserve callback errors and typed task/merge causes. Static ownership checks
must explicitly admit only this reader projection and guard both final sites.
Final-source Linux, macOS and Windows, reference, fixture and error-audit gates
precede landing. A zero-test run is not evidence; archive transfer must refresh
Cargo source timestamps as well as verify bytes before reusing build output.

September20 execution remains isolated and unqualified: native C6 passed12cases
but failed its architecture registry; RED5 reproduced four process-terminating
allocation refusals in the inherited validator constructor. C9 includes the
reviewed reader boundary, fallible/accounted constructor scratch and retry
regressions. Results and final qualification belong to ledger11, not this
contract. No task writer, capability advertisement or service is enabled.

Qualification September20 supersedes the earlier isolated-candidate status:
all158 C16 inputs are integrated byte-for-byte and pass the complete Linux,
macOS and Windows gates. Linux passed2302library/affected cases,186reference
tests,502independent fixtures and39individual ten-second deadlines (maximum
611ms). macOS passed1162library/1140affected/257narrow plus the separate targeted
allocation gate. Windows passed1177library/1138affected/257narrow; its final
driver completed13:59:14UTC and the six-minute collector finished14:00:41.
Required format/static checks passed, including audit1501 and strict Clippy
where prescribed. Windows's two-case affected-count difference is the existing
Unix/Windows workspace-test inventory, not skipped regressions.

The [combined proof](evidence/user-facing-v4-u1-task-physical-exclusion-proof-20260920.json)
binds source/lock/tool/binary identities, all raw gates, original behavioral
failures and the Mac allocation-target correction. That correction initializes
the fixture cancellation mutex before measuring named GC buffers; all ten
failure injections remain required. It does not claim arbitrary allocator
recovery. The positive sweep fixture records callback outcomes without actual
physical removal. Service-level GC execution, batching/convergence, unselected
checkpoint validation and coupled task/generation/HEAD publication remain
required. No task writer, runtime capability or production service is enabled.

### Unselected immutable checkpoint graph — entry ad5358a8

Initial task selection cannot use a selected-task reader's Absent result as
proof of its staged graph. Add explicit captured checkpoint metadata traversal
by nonzero task ID and checkpoint sequence. Missing ASCM/ASMC is an error, never
an empty successful task observation. Return a distinct checkpoint summary;
preserve the existing selected-task result shape, dispositions and counters.

Reuse the captured catalog owner's canonical ASCM/ASMC loading/binding and
paired-source traversal. Factor the existing checkpoint graph body, retaining
admitted-base namespace/state, staged namespace, partial/pruning catalogs,
definition/module edges, output state and unadmitted candidate/rebase trees.
Never fabricate ASMT to invoke the selected reader or fall back to current KV,
aliases or HEAD. Ordinary chunk payloads remain opaque references with checked
captured roles/extents, not a claim of verified content integrity.

All callbacks are provisional until complete success and may repeat. Preserve
original callback failures, cancellation, cumulative physical read/work bounds,
separate source bounds, bounded namespace workspace/depth and reservation
release. Tests must distinguish absent task from a valid unselected graph,
check independently enumerated edges across hashes/reopen, reject late missing
dependencies and binding disagreement, preserve selected-reader regressions,
and individually bound the new cases. This observation alone grants no task
selection, fencing, resume, pin release, root admission or global GC authority.

Qualification September21: the final161-input C3 snapshot matches MAIN exactly
and passes Linux, macOS and native Windows. Linux passed2314library/affected,
186reference,502independent fixtures and12individual ten-second cases (maximum
501ms); macOS passed1174library/1140affected/257narrow; Windows passed1189library/
1138affected/257narrow. Both native platforms also passed186reference/502fixtures
and their prescribed formatting/static gates. Windows finished15:48:30UTC;
its six-minute collector sealed and verified all local raw evidence by15:50:25.

The [combined proof](evidence/user-facing-v4-u1-unselected-checkpoint-graph-proof-20260921.json)
binds exact source, lock, tool and executable identities, both retained REDs,
the test-module ordering correction and unchanged1501audit identities with13
line-only metadata changes. Shared selected-task graph, retention and physical
exclusion cases remain required and passed on all three platforms. The real
initial-staging path and close/reopen are exercised; later-phase fixtures seed
controls through the existing test owner. No service or bulk-throughput claim,
ordinary-content integrity check, universal allocator recovery, task writer,
capability advertisement or production action is implied. Durable selection,
resume/fencing, checkpoint replacement and coupled activation remain U1 work.

### Initial durable task selection — entry 430e7be0

Select the initial Queued ASMT only from the opaque staged source union and its
already-published, exactly derived ASCM/ASMC pair. The caller supplies task/boot
identity, the original capture request, physical publication clocks and explicit
read/work/memory bounds, not arbitrary serialized task/control bodies. Reuse the
initial-pair derivation and the qualified captured graph reader; never silently
stage a missing pair or accept absent-task discovery as complete graph evidence.

Initial task fencing token, control sequence and checkpoint sequence are1;
pins remain held.
Use the immutable capture time for both initial logical creation/update times,
separate from the physical attempt timestamp, so an exact retry remains byte
stable. Compare exact selected generation with the original captured selection;
ordinary HEAD advancement is allowed only with unchanged semantic inputs and
fresh current graph/frontier admission. A generation advance, even an equivalent
raw rewrite or change-and-back, requires recapture. An existing advanced,
different-owner or released task cannot be reset by the initial selector.

All native publication stays with first_authority's existing physical owner.
Keep the generic semantic-control refusal; expose no boolean/serialized-body
override. A private typed admission binds the retained source owner, derived
pair/task bytes, current physical instance/writer fence, generation, capabilities,
healthy staging protection, cancellation, accounted scratch and exact fresh
captured frontier. Recheck before exact retry and writes under the same root
guard that remains held through publication. Flush pending retirement before
capture or safely refuse a later frontier change; never weaken its chronology.
Preserve existing generic-family retries and original committed error receipts.

Initial exact retry and conflicting-current-task refusal must also precede the
shared physical writer's KV-buffer flush. Under the retained root guard, read
the selected task through the canonical pair loader and compare exact derived
bytes; project an idempotent receipt only after all final admission checks.
Existing generic family flush/retry ordering remains unchanged. Pending
retirement still settles before graph capture as specified above.

The callable refusing RED precedes implementation. Require independent literal
task bytes, real stage/select/close/reopen/retention, conflicting/advanced retries,
owner/epoch/generation races, ordinary rebase versus late-frontier refusal,
missing pair/graph, named allocation failure, resource/cancellation bounds and
pre/postcommit recovery. Preserve all shared mutable-control, migration,
index-operation, receipt, selected-reader and GC regressions. Individual narrow
deadlines, strict static/format/reference gates and final-source native platform
qualification precede landing. Selection does not complete takeover, resumed
compilation, replacement checkpoint selection, safe pin release, atomic
task/generation/HEAD activation or any service/capability integration.

Qualification September21: all164 C5 inputs match MAIN and passed the complete
Linux, macOS and native Windows gates. Linux passed2427library/affected cases,
19narrow cases,19individual ten-second deadlines (maximum292ms),186reference
tests and502independent fixtures, plus audit1501/strict Clippy/format/debt gates.
macOS passed1193library/1234affected/257narrow; Windows passed1208library/
1231affected/257narrow. Both also passed186reference/502fixtures and their
prescribed static/format gates. Windows completed17:47:13UTC; the sole collector
sealed and verified its raw records17:49:48UTC.

The [combined proof](evidence/user-facing-v4-u1-initial-task-selection-proof-20260921.json)
binds source, locks, tools and binaries; original behavioral RED and corrections
are retained. The C4 retirement caller-registry failure occurred on all three
platforms; C5 adds the reviewed borrowed caller and source assertions without
changing runtime bytes. The shared physical writer body is mechanically
unchanged. Named allocation refusal is not universal allocation recovery;
ordinary content retention is not payload-integrity verification. Pending
retirement may settle before a read-only task retry. Restart observation is
qualified, but fenced resumed work and subsequent checkpoint/activation/service
integration remain required. No capability or production service is enabled.

### Fenced Captured work and first compiler checkpoint — entry 112c965a

Extend the same native owner from an opaque selected-task observation, never
caller-supplied task bytes. The first scope accepts only held Queued/Capturing
tasks with a Captured checkpoint. Other phases remain explicit refusals until
their continuation is implemented; no timer-based expiry, automatic copy
adoption, task reset, pin release, generation change or HEAD activation is added.

Reserve the next immutable checkpoint identity by selecting a fresh task fence
`max(old_fence, selected_checkpoint_sequence) + 1`, with checked overflow.
Preflight room for both task control advances before the reservation write.
Advance the control sequence once while preserving its old checkpoint, phase,
pins and creation time; select the explicit holder boot and current writer epoch.
The work handle owns this one reservation. Final checkpoint selection advances
control again without changing its fence. Every fresh acquisition, including
the same boot, advances the fence; an exact retry from the same opaque prior
observation derives the same target. An abandoned reservation is never reused
by a fresh operation, and an unknown immutable collision refuses rather than
overwriting or scanning for an unused ID. Independent control/fence/checkpoint
counters remain legal, including checkpoint sequence larger than task control.

The observation must belong to the current physical instance and writer epoch;
a newer writer may explicitly observe and take over an older task in that same
physical database. Its captured immutable pair retains the original capture
epoch. Current and observed reader/writer capabilities, exact captured semantic
generation, task bytes, healthy staging protection, cancellation, accounted
memory and freshly validated graph/frontier are required. Repeat these checks
under the existing root guard through publication; exact retry/conflict precedes
the legacy KV flush. Settle pending retirement before graph capture. Borrow the
existing retirement owner; restart callers reconstruct/resume its existing chain.

Consume the opaque work handle to start the existing semantic catalog
continuation. Share retained-source preparation, parser-registry compilation and
base construction-mode admission with the current prefix reader. A nonempty
Complete base is strictly admitted even if its registry changes; corruption or
unsupported semantics cannot fall back to fresh compilation. Matching registries
use `from_complete`; changed registry, content-only or valid Complete-empty use
`start`. Complete-empty with retained configurations refuses. The progress reader
continues to reject Captured; do not weaken it to manufacture a continuation.

Derive the Compiling/None-cursor checkpoint from actual continuation roots and
counts and unchanged immutable capture fields. Derive its own source companion
with the reserved sequence and new payload digest. Use the existing immutable
pair publisher with a private exact task/generation guard at its final root-lock
boundary. Catalog staging may leave unselected immutable objects on interruption;
it cannot confer task authority. Freshly validate the staged graph and exact
retained compiler prefix, then recheck all guards and select through the already
qualified mutable physical owner. Preserve typed committed task receipts and
distinguish committed immutable staging from selected task commitment. Generation
and ordinary HEAD remain unchanged by this unit.

Verification starts with callable refusing REDs before runtime behavior. Require
the real all-hash initial selection/reopen/acquisition/first compiler selection/
reopen/prefix/retention path, literal persisted-field expectations, exact retries,
fresh same/new-owner fencing, valid high checkpoint counters, exhaustion,
terminal/released/missing/malformed inputs, owner/generation/capability races,
ordinary versus late HEAD changes, cancellation/pressure/allocation release,
pre/postcommit outcomes, orphan-pair interruption and restart with old retention.
Exercise fresh/incremental/changed-registry and Complete-empty/nonempty source
modes. Preserve the shared physical/migration/index/GC caller registries and
regressions. Ten-second new-case Linux deadlines and matching final source on
Linux/macOS/Windows precede landing. General stepping/pruning/resume, activation,
pin release and service/capability integration remain following U1-U7 work.

Qualification September21: C15's170inputs match MAIN and passed Linux, macOS
and native Windows final gates. Linux passed2461broad cases in73targets, all34
new cases and34individual ten-second deadlines (maximum811ms),186reference
tests and502independent fixtures, plus audit1501, strict Clippy, format and debt.
macOS passed1227library/1234affected/257narrow; Windows passed1242library/
1231affected/257narrow. Both also passed186reference/502fixtures and their
prescribed static/format gates. Windows completed21:01:06UTC and its sole
collector sealed, collected and strictly verified the raw evidence21:03:26UTC.

The [combined proof](evidence/user-facing-v4-u1-fenced-task-work-proof-20260921.json)
retains all failed starts, including the real cross-publisher private-pair gap
and its regression, both explicit architecture-inventory corrections, C13's
single Clippy correction and C14's launcher count error. C15 is byte-identical
to C14; no test or runtime code changed for that launcher retry. The existing
generic/admitted mutable writer body is mechanically identical to entry112c965a.
Late cancellation of shared retirement preserves its original committed error
and task receipt; immutable pair commitment alone is not task selection.
This qualification does not complete resumed configuration/pruning/Ready work,
atomic activation, terminal pin release, public runtime or production readiness.
