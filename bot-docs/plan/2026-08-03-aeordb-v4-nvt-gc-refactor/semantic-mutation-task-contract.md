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
