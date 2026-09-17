# Durable protected semantic sources — U1 reader contract

Status: structural readers qualified on Linux, macOS and Windows, September17.
The [immutable reader proof](evidence/user-facing-v4-u1-semantic-source-capture-readers-proof-20260917.json)
does not qualify source-copy publication, complete closure, restart or activation.
Writer enablement remains refused. Continue the
existing [task contract](semantic-mutation-task-contract.md) and
[ledger11](progress/11-user-facing-v4.md) under Round17's additive authority.
Entry091d0666 has landed the preceding native-directory unit on all platforms.

## Proven missing edge

Round9 keeps global config, plugin aliases and module archives outside HEAD.
Round10 requires exact raw input capture, restart and activation checks while
excluding raw formatting/metadata from compiled semantic identity. ASMC's base
and staged namespace trees therefore cannot preserve every raw source. Its
source fingerprint cannot enumerate an old alias or reconstruct its bytes.
The current captured KV snapshot solves process-local observation only.

Do not alter ASMT/ASMC/ASMG layouts, source-fingerprint preimages, TaskPin kinds,
derived IndexTask kinds, SemanticState identity or ordinary namespace contents.
Do not make an in-memory capture into a restart permit. This draft adds typed
companion capture authority rather than reinterpreting an old hash slot.

## Proposed representation

Use two immutable SystemControl families at the existing physical owner:

- A task/checkpoint companion manifest binds base/request protected-source
  catalogs to the existing checkpoint's logical, physical and capture context.
- A bounded source-catalog node maps canonical original paths to existing
  immutable FileRecord content identities, or zero for explicit absence.
  Leaf and internal nodes are content addressed. Preserve exact original
  FileRecord bytes/version under their existing `filec:` content key and share
  the original typed chunks; no additional archive-body format is needed.

This is not an IndexArtifact, ordinary DirectoryIndex or admitted NamespaceRoot.
Use the shared SystemControl envelope, canonical system FileRecord/chunk loader
and sole publisher. Reuse the existing bounded B-tree search/bounds algorithm
through an in-memory node projection; do not reinterpret persisted directory
child fields or add another file/KV owner.

Namespace-resident configuration already remains reachable through the admitted
base and complete staged namespace trees. Archive only protected non-HEAD input
records here. A catalog includes explicit absences, so its base/request catalogs
have the same complete relevant path set even for additions and removals.

Reader-slice assignments, preserving the inspected registry/fixture meanings:

| Kind | Magic | Slug | Identity |
| --- | --- | --- | --- |
| 0x0048 | ASCM | semantic-source-capture | task ID16 + checkpoint sequence u64LE |
| 0x0049 | ASCN | semantic-source-node | selected-H node content identity |

All use immutable envelope sequence1. Preserve 0x0047 as unknown: the independent
reference's existing rejection test uses it. ASCP is already ScopeDefinition's
magic and is not available. Capability27, `SemanticSourceCaptureV1`, distinguishes these
retention obligations; preserve unknown24 and26. Existing capability25 remains
known but unadvertised. Assignment does not enable any runtime writer.

## Proposed body contracts

All integers are little endian; H is the selected registered database digest
width. Common framing, exact length, reserve and CRC checks remain unchanged.
Every body begins with its nonzero logical database ID. No native pointer,
absolute host filename or physical offset becomes portable content identity.

### ASCM companion manifest

Body length is exactly112+6H:

| Offset | Bytes | Field |
| --- | --- | --- |
| 0 | 16 | database ID |
| 16 | 16 | task ID |
| 32 | 8 | checkpoint sequence |
| 40 | 16 | captured physical instance ID |
| 56 | 8 | captured writer fence epoch |
| 64 | 8 | captured semantic generation |
| 72 | 8 | captured header sequence |
| 80 | 8 | captured timestamp, nonnegative i64 |
| 88 | 8 | protected path count |
| 96 | 8 | base catalog node count |
| 104 | 8 | requested catalog node count |
| 112 | H each | base NamespaceRoot, staged DirectoryIndex, base source catalog, requested source catalog, source fingerprint, complete checkpoint payload digest |

All IDs, sequences, counts and hashes are nonzero. Each node count is at most
2*protected_path_count-1 with checked arithmetic. Counts and root hashes are
declared obligations, never proof of complete traversal. The paired catalogs
must enumerate exactly the same paths and match these actual counts.

Resolve the companion by the selected checkpoint's canonical task/sequence path,
not through mutable latest state. Compare every duplicated identity/capture field,
base/staged root and source fingerprint against ASMC. Also bind the complete ASMC
envelope with the same selected-H digest already required by ASMT. This covers
compiler/registry fingerprints and all phase/cursor/output fields without
duplicating them or introducing a dependency cycle: ASMC bytes stay unchanged.
Publication is immutable and byte-for-byte collision checked. A companion belongs to one checkpoint;
later checkpoints may reuse its immutable catalogs and records but publish their
own correctly bound companion before selecting the checkpoint.

### Existing immutable FileRecord source copies

Present rows use the existing selected-H HASH(`filec:` + exact original serialized
FileRecord body), not its mutable `file:` path key, `fileid:` projection or physical
incarnation. A missing row uses all-zero H bytes, with no invented absence entity.
The original FileRecord entity version0 or1, original path, metadata, content
hash and chunk list remain byte-for-byte unchanged. A metadata-only or timestamp
replacement therefore changes raw revision even when compiled meaning does not.
This domain already exists in DirectoryOps, migration publication and selected
namespace readers; independent native replacement/metadata proof is still owed.

Use a narrowly guarded captured-source operation at the existing native physical
owner. Preserve existing physical flags and admitted chunk representation rather
than marking every source SYSTEM or rewriting chunk hashes. Canonical control
wrappers use SYSTEM chunks keyed with `system::`; ordinary/migrated source files use
unflagged `chunk:` entities. Verify actual original representations and identities
through their respective existing decoders. Do not apply empty-metadata/one-chunk
control-wrapper rules to arbitrary configuration or module FileRecords. A source
copy has no current-path KV alias and grants no namespace/read-view admission.
The generic immutable publisher's SYSTEM refusal remains unchanged; the private
capture permit cannot be fabricated from an arbitrary entity descriptor.

Validate the actual FileRecord path/family, version, framing, exact content key,
declared length/hash and every chunk before granting a retained-source result.
Keep decoder policy separate from executor availability. FileRecord decode still
uses the existing owner; do not introduce a second parser. Missing or malformed
referenced records/chunks are incomplete/corrupt, never explicit absence. A
bounded read refusal is operational Resource, never absence or proof of malformed
data. The existing4MiB native entity ceiling supplies the initial operational
bound, not a new restriction on historical FileRecord wire bytes.

An already-present content identity is accepted only after exact version, flags,
compression and body readback agrees; a collision/mismatch refuses publication.
Retain only this immutable record and its own chunks, not the current mutable
path incarnation merely because the old record embeds the same original path.
The catalog's explicit typed edge owns this retention, not a scan that roots
every historical FileRecord forever. This operation remains disabled until
actual native closure/restart/reclamation tests qualify it.

### ASCN source catalog node

Body starts with database ID16, node kind u16 (1 leaf,2 internal), reserved u16
zero, item count u32, payload length u32 and reserved u32 zero:32 bytes total.
Payload must consume the remaining body exactly. Body cap remains1MiB.

- Leaf payload: repeated `(path_length u32, path bytes, FileRecord_content_ID[H])`.
  Count1..256; paths are canonical absolute UTF-8, strictly byte-ordered and
  unique; all-zero ID means explicitly absent, every present ID is nonzero.
  Each loaded FileRecord must name the identical original path. Database binding
  belongs to the catalog/control/native header, not an invented FileRecord field.
- Internal payload: first child node ID[H], then repeated
  `(separator_length u32, separator bytes, next_child_node_ID[H])`.
  Count1..128 separators; all child IDs are nonzero and pairwise distinct.
  Separators are canonical absolute paths, strictly byte-ordered and unique.
  Inherited inclusive-lower/exclusive-upper ranges apply to all visited keys.

Node identity is selected-H HASH of ASCII
`aeordb.semantic-source-node.v1\0` followed by its exact body. Both node kinds
are borrowed bounded framing, not an unbounded decoded collection. Complete
closure proof checks cycles, depth, counts, path sets and every actual edge.
Point lookup cannot scan all earlier records; complete ordered traversal must
visit each edge once under explicit read/work/memory limits. Source catalogs
are nonempty because capture always includes global index and parser config
paths, using explicit absence where appropriate.

## Capture, retention and activation obligations

Capture one settled native header/KV view under the existing short guard and
hold staging protection through publication of the first durable task capture.
Reuse the current captured lookup; do not silently fall back to live locators.
Enumerate base/request namespace configuration paths and every referenced
protected registry/alias/module path. Capture the complete union, not just the
new request's aliases. Apply the requested protected replacements to a separate
requested catalog; both catalogs preserve explicit absence rows.

The existing source fingerprint enumerates original paths in strict order using
their captured BASE FileRecord revision or explicit absence. Its path set is the
complete relevant base/request union. Requested replacement bytes are separately
bound by the companion's immutable requested catalog and staged namespace tree.
Raw formatting changes still advance semantic generation; canonical semantic
identity remains unchanged where compiled meaning is equivalent. Recheck exact
base identities and generation at activation, including change-and-change-back.

Retain companion FileRecords/chunks, both catalog-node closures, exact immutable
source FileRecords and their actual typed chunk edges. Ordinary
base/staged namespace closure is still retained through ASMC. Superseded original
system FileRecord locators may be retired only after the immutable source copy
and its required chunks have durable retention. Never replace a current source
with the archive copy or expose capture records in user HEAD/listing/SSE.

Publish dependencies before manifest/checkpoint/task selection. Source copies
and nodes are immutable collision checked. Keep the old selected capture live
until its replacement is durable and selected. A missing companion, incomplete
catalog, source mismatch or failed read blocks resume/activation and makes mark
incomplete. Terminal scheduler status alone does not release the closure.

Old codec-only ASMT/ASMC observations retain their meanings. They do not acquire
a resume permit without the newly required complete source capture. New task
publication requires both capabilities25 and27 and qualified capture/GC/recovery
integration. Logical transfer omits these node-local in-flight controls through
the existing protected-control family; physical-copy adoption validates the
captured physical identity/fences and performs explicit takeover. No production
migration or service operation follows merely from codec qualification.

## Required entry audit and falsifying proof

Before freezing this draft, audit all common control kind/magic/identity/capability
registries, production/reference dispatch, admission, v3/native refusal, strict
verification, transfer policy, GC discovery, task observation/selection, shared
native file loaders, B-tree projections and recent allocation regressions.
Check the raw FileRecord revision domain and canonical protected source rules
against their actual writers, not this proposed spelling alone. Identify every
changed caller and preserve historical fixtures and immutable qualification.

`test_protocol`:

- Current coverage: common control framing/A-B/capabilities; source fingerprints;
  captured header/KV inventory; selected directory ordering/ranges/resources;
  APAL/APWM/module identity; compiler equivalence and staging/GC barriers. None
  yet proves this durable companion closure.
- Hypothesis: exact base and requested protected inputs survive alias replacement,
  process restart and GC without entering HEAD or changing semantic identity.
- Given aliasA, capture then replace withB/reopen: resume readsA from its capture
  and activation refuses the stale base, never quietly readsB.
- Given raw whitespace-only config replacement: canonical definitions remain
  equal, while source revision/generation conflict prevents stale activation.
- Given a mixed ordinary/config/plugin batch: no part becomes visible before one
  successful activation; cancellation/restart preserves old authority and roots.
- Unit/reference proof: independent hand-built envelopes for all five hashes,
  repaired-CRC malformed cases, every truncation, reserve/count/path/identity
  mismatch, overflow, borrowed views and actual allocator refusal/retry.
- Integration proof: actual native files at32/64-byte widths; native captured
  reads, exact original-body retention, old-locator replacement, complete typed
  closure, missing/corrupt dependencies, paused callbacks/races and accounting.
- Property proof: sorted paired catalog enumeration versus an independent map;
  bounded point reads and one-pass traversal, adversarial separators/cycles and
  cancellation at final callbacks. No world-sized production maps or rescans.
- E2E proof: durable task/checkpoint selection and process restart, concurrent
  ordinary updates/rebase, changed semantic generation, copy adoption and GC.
  Actual service/client activation remains the later ordinary-runtime gate.
- False confidence to avoid: self-generated goldens, mocked latest alias lookup,
  only matching source fingerprints, treating declared counts as closure, or
  keeping a capture alive only through process-local guards.
- Timeouts: small deterministic codec/fixture cases with existing watchdogs;
  bounded coordination waits that release gates before assertions; established
  Linux30-minute, Mac50-minute and Windows90-minute outer stage deadlines.
  No unbounded network dependency is needed for the native fixture proof.

Test order is independent RED readers -> exact byte/readers and reference ->
bounded source/capture publication -> restart/GC/activation integration -> full
final-source platform/static/reference gates. No writer enablement before its
actual retention/recovery proof. Review this draft's implicit companion binding,
source-set completeness and allocation/GC behavior again before declaring the
contract settled. The native-directory prerequisite has landed; the reader
landing below does not implement the later durable source-copy closure.

### First draft review findings

The actual native authority uses stable HASH(`file:`+path) system record keys,
while ordinary immutable FileRecords use HASH(`filec:`+serialized body). A legacy
`system_file_identity_hash` helper has a different `sysfileid:` projection that
omits raw metadata; the production symbol search found only its definition and
reexport. It is not evidence for a captured raw-record revision and must not be
substituted for one. The proposed revision domain still requires an independent
same-path replacement/metadata test before use.

The native canonical system-control loader currently requires empty metadata,
one system chunk and a64KiB outer FileRecord. That proves control-wrapper shape,
not every global config/plugin source writer. The draft therefore preserves
original record bytes/chunk lists under the separate4MiB bound and leaves the
family-specific source acceptance inventory open. Do not freeze reader fixtures
that assume these two representations are identical.

The native consumer audit favors exact existing FileRecords over the initial
ASCR archive proposal; remove that proposed format/domain/assignment entirely.
`semantic_mutation_inventory::inspect_entry` validates each FileRecord and its
family but discovers controls only under the canonical control path/content/key
combination. The relevant config/alias/module source copies are not controls.
`gc_state` and `gc_retirement` classify exact physical incarnations; their typed
models do not make every embedded original path current authority. Actual native
source-closure traversal is still an integration obligation, not an existing
GC behavior proved by these helpers. The public `gc.rs` implementation is still
legacy-backed and cannot be used as the v4 capture proof.

Legacy `mark_live_path_file_records` explicitly distinguishes current `file:`
keys from old content aliases. Its recursive system traversal additionally marks
mutable path keys, which must not be copied into the captured-only edge walker.
The new native closure must follow only the exact immutable source and its chunks;
separate current-source roots retain current authority. Logical transfer derives
current sources from its admitted namespace/protected current-path inventory,
never every historical KV FileRecord with a matching semantic family. Captured
in-flight tasks remain node-local under the unchanged family0x0043 policy.

No fixed source version/flag assumption is frozen by this design. Before native
writer enablement, tests must cover both supported original FileRecord versions,
ordinary versus canonical SYSTEM chunk representations, exact current-path
replacement and old content retention, and release after the last task drops.
The next independently qualified reader slice only assigns/decodes ASCM/ASCN,
binds a companion to the complete ASMC payload, and preserves all publisher
refusals. It does not claim this later source-copy/closure work is implemented.

## Reader landing unit — entry091d0666

Direct execution, no delegated edits. Existing qualified source and both locked
dependency manifests are the baseline. No upstream drift at entry. New format
readers do not enable byte writers, source-copy publication or a resume permit.

`map_territory`: common control kind/magic/path/immutability/body dispatch is
`v4/system_control.rs`; new body/borrowed-view/paired-checkpoint readers belong
in `v4/semantic_source_capture.rs`, exported through existing `v4/mod.rs`.
The independent owner is `tools/v4-reference/src/system_control.rs` with its own
new body parser and fixtures. Capability truth is
`spec/fixtures/v4/format-contract-registry.json` -> reference `contract_gen.rs`
-> generated constants -> production header/admission/namespace/index readers.
The independent header/root/index readers consume their own known-capability
predicate; audit that predicate's exact call sites before changing it.

Preserve unknown24/26 and SystemControl0x0047; expand the all-256-bit property
expectation only by assigned27. `BinaryCapabilityProfileV1::current` remains
byte-identical. Extend `is_semantic_mutation` to cover both new reader-only kinds,
so native generic publishers and both ControlStore adapters still refuse them.
Extend actual native immutable-publication refusal and all-kind adapter tests.
The existing task observer/inventory may inspect old codec-only task summaries;
they still grant no resume/complete-GC capability and are not silently rewritten
to demand a companion from old fixtures. New capture acceptance later requires
capabilities25/27 and complete typed closure, not these old observations alone.

Receiver inventories to extend together: common all-kind fixture count and
kind-specific malformed-body matrix; reference ALL/kind/magic/slug/immutable/
build/validate dispatch; generated registry consistency; contract script's exact
magic/body-count/immutable list; generic hardening's existing SystemControl route;
SystemFamily/transfer classification of both new canonical paths; allocator
resource tests. Historical fixture binary/hex bytes and error meanings stay
unchanged. Add independently generated fixture rows rather than reinterpret old
rows. Audit metadata may move locations but must not grow its1501-entry ceiling.

`test_check`: the first test must demonstrate that independently constructed
valid ASCM/ASCN bytes are rejected by the current common reader. Four positive
checks (manifest, leaf, internal, capability27) plus a preserved-unknown check
run as a new child of `semantic_mutation_control_spec`; no production change
before preserving that actual RED. Command on desktop:
`cargo test --offline --locked -j 2 -p aeordb --test semantic_mutation_control_spec
source_capture::`, under the existing30-minute/6GiB/no-test-swap/disk-floor runner.

Then extend borrowed API tests before implementation: all five hashes, exact
body/envelope lengths, every truncation, repaired CRC, database/task/physical/
checkpoint/hash zero/width/mismatch, all copied ASMC fields plus full-payload
digest binding, timestamps and checked2N-1 arithmetic. Node cases include both
kinds, absent/present FileRecords, canonical absolute path/UTF-8/order/duplicates,
every reserve/count/payload bound, maximum path/node size, nonzero/distinct child
IDs and outer immutable sequence. Bounded identity allocation/refusal/retry is
measured separately; borrowed node views do not allocate per row or claimed count.
Complete tree validation/lookup/retention is a following runtime unit, not a
property conferred by a locally valid node or manifest count.

Narrow green precedes affected common control/admission/namespace/native authority,
task observation/inventory, transfer, compiler/migration and GC regression gates.
Finish all native library/static/reference/fixture gates on one frozen candidate,
seal/copy exact receipts and executable hashes, review, then commit/push one
revertable reader unit. Keep service/default/writer activation disabled. Next
units are immutable source-copy/native capture, complete durable typed closure,
checkpoint/resume and atomic activation under the existing full-U1 obligations.

Reader API refinement: `decode_semantic_source_capture_v1` returns borrowed
manifest fields; `decode_semantic_source_node_v1` returns a borrowed node with
leaf-entry or internal-child iterators (paths/hashes borrow the immutable body).
The first internal child has no separator; later children carry their preceding
separator. Parsing failures remain explicit iterator results, never silently
shorten a stream. No vector allocation scales with row count. Common envelope
identity ownership is a fallible24-byte manifest key or selected-H node key;
reuse `hash::try_digest_parts` for the latter, not an infallible digest allocation.
`decode_semantic_source_capture_binding_v1` validates the complete original ASMC
envelope digest plus every duplicated scalar/root/fingerprint field and returns
the paired borrowed views, not an opaque execution/resume/retention permission.

The independent capability predicate is `tools/v4-reference/src/core.rs`, called
by main header validation, core namespace/root decoding, `index.rs` and
`index_tasks.rs`. Preserve its32-byte/existing-bit rules while recognizing27.
Production `database_header::capabilities_are_known` already uses the generated
sparse mask; header/admission/namespace/index consumers share that owner.
No hard-coded contiguous-prefix rewrite or runtime support-mask change is needed.

## Following native-reader entry: declared-length allocation boundary

Prepare only while the reader candidate is frozen; start executable changes
after its all-platform landing. The existing canonical system loader reserves
the FileRecord's declared body length, then extends from the decoded chunk,
and only afterwards compares actual length. Both inputs remain outer-bounded,
but the extension may exceed its initial fallible reservation. This is a source
review finding until the following actual test demonstrates it.

`map_territory`: the single helper is
`first_authority::load_canonical_system_file_at_path`. Direct consumers include
canonical control slots, captured task inventory, current/captured semantic
objects, immutable semantic publication readback and selected/successor semantic
state validation. Their outer observers and compiler/error adapters remain
unchanged. Preserve the one-chunk/version1/SYSTEM/empty-metadata wrapper policy;
arbitrary protected source FileRecords still need their separate qualified path.

`test_check` / `test_protocol`: the falsifying case uses actual disposable native
files at both hash widths and a large independent ASMC payload. Rewrite only
the fixture FileRecord's declared size, maintaining valid entity framing and
integrity. Measure allocation attempts for zero, smaller and larger claims;
each must return the existing `first_authority_system_file_content` error before
allocating an output body. Count attempts without injecting an abort into the
old infallible-growth path. Preserve the actual failure before any correction.
Then add allocation-refusal, valid-empty/exact-body, unchanged bytes/header,
memory release and successful retry coverage through the shared native loader.

Narrow command: `cargo test --offline --locked -j 2 -p aeordb --lib
canonical_system_file_length_mismatch`, under the existing30-minute desktop
resource guard. This slice owns only the shared loader, its existing native
test child, mechanically relocated audit metadata and dated evidence. Rerun
the complete native observer/inventory, semantic compiler/store, current control,
root/publication and migration consumers plus final platform/static/reference
gates before landing. No encoded bytes, error policy, writer capability or
production database changes follow from this allocation-order correction.

Qualified September17,13:29UTC on Linux, macOS and Windows in the
[canonical-length proof](evidence/user-facing-v4-u1-canonical-system-length-proof-20260917.json).
The actual two failing-first allocation assertions and four final native
regressions are preserved. Native protected-source integration may now begin;
this loader correction does not implement that capture or enable its writers.

### Native protected-source integration: refreshed read-only perimeter

September17, during frozen loader qualification; no new source implementation
or qualified capture API is implied by these findings.

The concrete source families are root index config0x0001, parser config0x0003,
plugin aliases0x0031 and plugin artifacts0x0032. The registry distinguishes
descendant index config0x0002 (namespace-resident) and legacy plugins0x0030
(semantic roleNone, quarantine/manual migration); do not silently admit either
as a corrected non-HEAD source. Missing module or alias inputs may be represented
in the capture union but cannot satisfy required dependency closure/execution.

`semantic_mutation_inventory` owns the existing settled snapshot and bounded
lookup under `NativeStagingProtection`. Share that capture owner for source
reads rather than independently capturing task controls and protected inputs.
Callbacks must remain outside root/KV locks, with separate scratch admission
for nested or simultaneous operations. No live path lookup may substitute for
the captured locator. Inspect exact current-path records and retained `filec:`
records through the same typed FileRecord/chunk validation, not separate parsers.

Actual representation producers checked: `append_system_file` emits version0
SYSTEM chunks under `system::` and version1 SYSTEM FileRecords; legacy buffered
DirectoryOps computes `chunk:` identities even for protected flags. Migration
`copy_chunk` admits its explicit legacy domain, then normalizes output to ordinary
`chunk:` bytes. That ingress normalization is not permission to rewrite an
already-native captured source. FileRecord v0 has no stored whole-content hash;
v1 does. Preserve original version, metadata, path, flags and exact serialized
body when deriving/reusing `filec:`. A same-ID flag/version/body disagreement
must still refuse through the existing exact immutable readback owner.

Native capture must not add a third FileRecord parser. The ordinary decoder is
`file_record.rs`; the existing migration-only borrowed reader additionally lives
in `migration_base_clone_execution.rs`. Reusing/factoring a borrowed projection
requires parity/malformed tests and retained migration error behavior; a new
parallel decoder is not an acceptable shortcut. Large raw modules can reach
64MiB while individual FileRecord entities retain the4MiB operational bound.
Do not reserve every source at the module maximum or assemble whole catalogs.
Use bounded chunks and the existing all-profile `IncrementalDigestV1` for content
validation; its fallible finalizer is available. Any decompression scratch must
be admitted/bounded too, without inferring universal allocation recovery from
the current `decompress_bounded` helper's use of infallible Vec growth.

The private `publish_immutable_entity_batch_with_validation_locked` already
provides dependency/header ordering and exact collision checks to specialized
owners. A future captured-source entry must validate an unforgeable same-owner
capture before entering that path. Keep public GenericContent SYSTEM refusal;
do not turn a caller-supplied descriptor or reader view into a copy/GC permit.
Actual source replacement, restart, typed closure and release tests remain owed.
