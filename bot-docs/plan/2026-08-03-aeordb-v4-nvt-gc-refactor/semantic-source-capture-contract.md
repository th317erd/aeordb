# Durable protected semantic sources — U1 reader contract

September19 update: the complete captured union, guarded native sinks, physical
observation and read-only selected task graph passed
[combined Linux/macOS/Windows qualification](evidence/user-facing-v4-u1-captured-task-integration-proof-20260919.json).
Earlier isolated qualification notes below remain historical. Durable task
publication, restart/resume, GC retention and atomic activation are still open.

Status: structural readers, byte-only writers, native catalog readers, guarded
source staging and paired ordered catalog assembly qualified on Linux, macOS
and Windows, September17.
The [immutable reader proof](evidence/user-facing-v4-u1-semantic-source-capture-readers-proof-20260917.json)
does not qualify source-copy publication, complete closure, restart or activation.
The [guarded staging proof](evidence/user-facing-v4-u1-guarded-source-staging-proof-20260917.json)
qualifies process-local source copies only. Durable task/control publication
remains refused. Continue the
existing [task contract](semantic-mutation-task-contract.md) and
[ledger11](progress/11-user-facing-v4.md) under Round17's additive authority.
Entry091d0666 has landed the preceding native-directory unit on all platforms.

The [paired assembly proof](evidence/user-facing-v4-u1-source-catalog-build-proof-20260917.json)
qualifies deterministic byte construction from an already complete ordered input,
not discovery of that input or a durable task/GC closure permit.

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

### Captured-source reader entry3589b88a

The canonical-length prerequisite is committed/pushed. Reuse the existing
`NativeSemanticMutationInventoryV1` as the single captured physical view; add
`read_protected_source(path, bounds)` rather than capturing a second snapshot or
renaming the existing qualified task-inventory API. A non-Clone returned source
borrows that capture and retains its own memory reservation. Borrowed getters
expose exact original encoded FileRecord, decoded metadata, body, selected-H
`filec:` revision, entity version and flags. No owned-buffer extraction, resume
permission, publisher access or durable-retention claim is added.

`map_territory`: physical lookup/framing stays at `first_authority`; snapshot,
protection and read admission stay in `semantic_mutation_inventory`. A private
`semantic_source_native` child owns this source projection, reexported through
the existing observation/authority owner. Reuse `FileRecord::deserialize`, not
the migration-only borrowed parser or a third implementation. Existing task
visits and all publication refusals remain intact. Refresh the architecture
owner test with this deliberately read-only child when qualifying it.

`test_protocol`: the key hypothesis is that task/source reads share exactly one
captured boundary. Given replacement after capture, the old view returns old raw
bytes and a fresh view returns new bytes; captured absence cannot consult live
state. Given v0/v1 records with metadata, multiple chunks and actual inherited
flags, preserve the exact serialized record rather than canonicalizing it into
a control wrapper. Given bad framing, missing chunks, wrong roles/paths/hashes,
declared-length mismatch, cancellation or resource refusal, return an explicit
failure without writes, leaked reservations or fabricated absence.

Native file tests use independently built raw FileRecord bodies at both widths;
existing physical wrappers only assemble disposable fixtures. Add all-profile
identity, family/absence, malformed/read/work bounds, nested lifetime, allocator
refusal/retry and compressed-content bounds. One-source output is admitted at
its actual declared bounded size, not the64MiB module maximum for every source.
The existing stream decompressor's bounded output alone does not prove bounded
decoder workspace: inspect its actual implementation and qualify the selected
bounded path before claiming that property. Retain inherited allocation caveats.

`test_check`: four positive native tests run against an explicitly refusing
scaffold before implementation: `cargo test --offline --locked -j 2 -p aeordb
--lib protected_source_spec::`. Use the existing30-minute/6GiB/no-swap desktop
runner. A compile failure is not the required behavioral RED. Preserve raw
receipts and exact binaries before correcting the scaffold. Affected native
inventory/observation/source/compiler/migration/GC tests and final platform,
library/static/reference gates remain mandatory. Complete source-copy/catalog
closure, restart and activation are subsequent coupled U1 obligations.

Qualified September17 on Linux, macOS and native Windows in the
[native protected-source proof](evidence/user-facing-v4-u1-native-protected-source-proof-20260917.json).
The15 native cases preserve raw v0/v1 metadata/flags, old/fresh captures and
absence, all five hash profiles, bounded reads/allocation/decoder output, late
cancellation/pressure and the actual64MiB body. The operational physical entity
ceiling now includes8192bytes of framing headroom without enlarging the payload
limit. Fixed decoder workspace is separately charged; inherited FileRecord
allocations remain an explicitly documented limitation. No source-copy writer,
durable closure, resume or activation was enabled by this qualification.

### Following byte-writer boundary — September17 review

Prepare independent tests while native source qualification runs; executable
byte-writer behavior starts only after its green landing. A refusing scaffold
may run in a separate disposable checkout after the preceding Linux evidence
is sealed/copied; it cannot modify the active native candidate or enable a
publisher. The structural ASCM/ASCN prerequisite is already qualified on all
three platforms. This extends the assigned ASCM/ASCN
contract, not the capability mask or native publisher. No actual control may be
published until complete typed retention/recovery is integrated.

`map_territory`: `semantic_source_capture` owns borrowed manifest/leaf/child
views and validation; `system_control::encode_system_control_with_body` owns
fallible one-buffer framing, CRC and decode/readback. Reuse those owners via
three byte-only APIs: `encode_semantic_source_capture_v1`,
`encode_semantic_source_leaf_v1` and `encode_semantic_source_internal_v1`.
Manifest input is the existing borrowed capture view; nodes take database ID
and bounded borrowed row/child slices. Internal children keep their existing
first-no-separator representation. All envelopes use immutable sequence1.
No new physical file, envelope parser, persisted layout, writer permit, native
capture or public service route is needed. The compiler/task/native closure
consumers still follow; node bytes alone never prove full source-set coverage.

Validate identity/hash widths, absent-versus-present-zero, row/child shape,
canonical sorted paths, duplicate child IDs and checked payload capacity before
allocating output. Use the shared decoder for complete scalar/count/identity
validation after bounded fill. Keep original fixed fixtures, known/unadvertised
capabilities25/27 and every native/v3/control-store publication refusal intact.
Memory is one bounded output plus the existing24-byte manifest or H-byte node
identity, not a temporary body plus output or an allocation from declared graph
counts. Caller-owned row slices do not transfer their allocation responsibility.

`test_protocol`: hypothesis—these APIs reproduce independently hand-built
ASCM/ASCN bytes for every registered hash, without granting authority. Given
paired absent/present rows, encoding must preserve zero absence and exact H-wide
record identity. Given maximum fanout/long paths or bad ordering/width/shape,
bounded success or explicit refusal must occur without truncation or panic.
Given failure of output or identity allocation, preserve operational failure
and retry successfully with unchanged inputs. Self-roundtrips alone are not
proof: expected envelopes/CRC/identities come from the existing independent
test bodies and immutable reference fixtures.

Add actual positive RED against refusing scaffolds before implementation, then
malformed/count/length/path/duplicate/overflow matrices and measured allocation
refusal/retry. Unit/reference tests establish exact bytes; existing native
publication-refusal tests establish no new physical authority. Complete catalog
property tests and actual restart/GC/task E2E belong to following integration,
not a claim conferred by this byte-only API. Existing bounded30/50/90-minute
platform stages remain outer watchdogs; deterministic codec cases require no
network or blocking callbacks. Run narrow control/resource targets, all framing
consumers, libraries, architecture/static/reference and all native platforms
before landing. No production or VM reconfiguration is required by this slice.

Qualified September17,15:48UTC on Linux, macOS and native Windows in the
[immutable byte-writer proof](evidence/user-facing-v4-u1-source-capture-writers-proof-20260917.json).
All fourteen new functional/resource regressions and the final native/static/
reference gates passed. No physical source-copy publication, capability
advertisement, durable task retention or default-format change was enabled.

### Following native catalog observation — read-only design review

September17, during frozen byte-writer qualification. This records the next
coupled reader integration, not an enabled publisher or completed task closure.
Executable behavior begins after the byte-writer landing. Independent tests may
be drafted separately; no changes to the frozen candidate are permitted.
A refusing test scaffold may execute in a separate disposable checkout after
the preceding Linux evidence is sealed and copied; it enables no read behavior
or publisher and cannot replace the active platform candidate.

`map_territory` — what changes and who owns it? Extend the existing captured
inventory with retained `filec:` source reads and native ASCM/ASCN catalog
observation. Native sources still use `semantic_source_native` and the sole
FileRecord/chunk/whole-entity decoders. Canonical controls still use
`load_immutable_system_control_file` and its shared physical/key/body checks.
A private catalog child of the same inventory may project validated ASCN into
the existing accounted BTreeNode/ChildEntry view for `namespace_seek`.
That projection is temporary, never serialized as a DirectoryIndex or admitted
as an ordinary namespace. Absolute catalog paths use the direct shared seek,
not directory-component suffix ordering. Reuse shared inherited child bounds.

What other paths consume this? Existing current-path source reads, known-task
observation and captured inventory must keep their prior semantics and tests.
The inspected retained-read gap is real: the only current source method computes
`file:` from the path. There is no retained `filec:` read entry. Factor its
record/chunk body validation once; never duplicate its parser or call the live
current-path reader as a fallback. Both APIs preserve original v0/v1 metadata,
flags and admitted compression. A supplied retained revision must equal the
selected-H digest of the exact encoded FileRecord and name the expected path.
Missing retained records/chunks are errors, not explicit absent catalog rows.

Bounds belong to one complete operation, not each recursive/source call.
Share captured lookup and cumulative physical read/work admission across all
nodes and sources. Retained node frames, projections, bound strings, raw source
bodies and simultaneous base/request reads require separate coordinator charges.
The source reader's existing chunk scratch and operational body/entity caps
remain intact. The shared seek's inherited owned clones remain accounted;
do not claim universal allocator recovery from a fallible outer reservation.

Paired catalog validation needs two bounded ordered cursors. Retain ancestors
and the current leaf, visit each node once, and compare paths as rows emerge.
No whole-world map/set, repeated root seek per row, or declared-count allocation.
Check inherited half-open ranges, ordering, depth, cycles, actual node/path
counts and paired path equality. Point lookup remains a bounded root-to-leaf
operation and is explicitly weaker than complete paired traversal. Preserve
three outcomes: unlisted path, listed explicit absence, and listed present
immutable source. A missing dependency never becomes either absence outcome.

Read ASCM at the requested task/checkpoint identity and bind the complete ASMC
envelope through the existing binding decoder. This diagnostic observation does
not select the current ASMT, acquire ownership, or validate all task roots.
Stored capability declarations and database identity must match the known
control requirements. Do not reject historical retained bytes merely because
their captured physical ID, fence or semantic generation differs from today's:
the existing observer deliberately preserves those bytes for retention/takeover
inspection. Resume and activation must perform their separate current-owner checks.

Actual paired path/count validation is not proof that the compiler captured
every relevant input. The source fingerprint also includes namespace-resident
configuration, so protected-only traversal cannot recompute it partially and
claim equality. Full base/request union capture, namespace closure, compiled
objects, native publication, durable task retention/recovery and activation
remain following coupled obligations. No new read result grants those permits.

`test_protocol` — what is already tested? Structural ASCM/ASCN binding, captured
raw current sources, task selection/inventory, native directory ordering and
shared seek bounds have independent proofs. None proves retained revision lookup
or the complete native protected-source pair. Hypothesis: exact old inputs remain
readable by content identity after replacement/reopen, with linear complete
traversal and no latest-state substitution.

Three initial falsifiers, before implementation: given an archived v0/v1 source
and a newer current path, retained lookup returns the exact old record/body;
given a same-path current file but missing archived revision, it refuses;
given differently shaped base/request catalogs with equal paths, paired traversal
visits those paths in order, while a missing/different path refuses completion.
Use actual bounded native files and independent expected revision/body values.

How can tests give false confidence? A mocked current alias, matching declared
counts, re-encoding the expected FileRecord, or a self-generated malformed graph
with an already-invalid hash would not prove the intended edge. Separate actual
native identity/corruption tests from defensive graph-model cycle/range tests.
Cover all five hashes, empty/absent/missing/duplicate rows, malformed framing,
database/identity mismatch, depth/work/read limits, per-node read counts, allocator
and memory pressure, late cancellation/visitor failure, nested calls and retry.

Unit/property tests compare seeks and ordered paired rows against an independent
small ordered map. Native integration replaces paths and reopens real files;
resource tests measure charges and releases at callbacks, including the final
callback. Service E2E/GC/resume permissions cannot be claimed by these reader
tests. Keep deterministic fixtures small; every process has the established
30/50/90-minute platform watchdog, with bounded coordination waits in race tests.
Run actual RED first, then retained/source/catalog tests, all affected native
consumers and final platform/static/reference gates. Review the finalized API
and exact consumer/target inventory before writing its production behavior.

#### Reader API and budget refinement

The same captured inventory owns three operations: retained source read by
original path plus revision; point lookup by task/checkpoint, base-or-requested
catalog and path; and paired ordered visitation by task/checkpoint. Point
lookup returns a typed unlisted/explicit-absence/present result. Present results
borrow the capture and retain source accounting. The paired callback receives
one path and borrowed optional base/requested sources; returning false yields an
explicit incomplete summary, never closure success. Successful full traversal
reports actual paths and each catalog's actual node count. It additionally
requires the root index and parser paths; this is still not compiler-union proof.

Catalog bounds name maximum depth, total work, total entity-read bytes and the
existing per-source body/chunk limits. Depth is operationally capped at256, not
a persisted-format limit. Each operation creates one shared lookup/budget for
ASCM, ASMC, all ASCN nodes and retained FileRecords/chunks. Work charges physical
entity reads, loaded catalog nodes and yielded rows; byte accounting measures
entity reads, not underlying KV-page traffic. No nested per-source budget reset.
Standalone current/retained source methods keep their own existing bounded read
operation. Factor their common implementation over the existing lookup trait.

Point-result layout refinement after the actual strict-Clippy failure: use a
private-field result struct with `disposition()` and borrowed `source()` getters.
The disposition remains the typed Unlisted/Absent/Present enum; only Present
has a source. This preserves capture/accounting ownership without boxing the
large source or suppressing the enum-size check. It changes no persisted bytes
or existing service API. Native tests assert all three outcomes, absence of a
source in the first two, and retained memory until the point result is dropped.

Node loading reserves shared-control decode scratch, then retains separately
charged bounded BTreeNode projections. Point lookup uses the shared direct seek;
paired cursors retain one internal ancestor per level plus one leaf each and
reuse shared child bounds. No catalog-sized map, repeated root seek or claim
that inherited owned-clone allocation is universally recoverable is allowed.
Capability25 and27 declarations are required for native capture controls but
remain unadvertised by ordinary binaries. Missing requested ASCM/ASMC is an
error; stale captured physical/fence/generation fields remain diagnostic data.

`test_check`: the three retained-source RED failures are real and preserved.
Add a paired-catalog falsifier against a refusing scaffold before implementing
catalog behavior. Independently expect ordered paths and old/new bodies from
differently shaped native trees; then mismatch the last path or declared count.
Property cases compare point and paired traversal with an independent small
ordered map. Resource cases exhaust cumulative reads/work across individually
admissible sources, and check final-callback cancellation, visitor failure,
nested visits, retained charges and retries. Existing native source, inventory,
observation and selected-directory tests remain regression gates. The native
loader/compiler/control/migration/GC consumer inventory and architecture owner
test must be refreshed together before final platform qualification. The same
bounded native runners apply; no service/network dependency enters these tests.

### Following guarded source staging — implementation-entry review

`map_territory`: the next physical dependency is preserving a validated captured
source as its exact immutable `filec:` record, sharing its existing typed chunks.
The producer is `NativeProtectedSemanticSourceV1`; its private fields retain the
originating captured inventory and staging guard. The existing sole physical
owner supplies the immutable batch transaction, collision readback, publication
observer and committed-error receipt. Do not create another file/KV writer,
serialize the decoded FileRecord again, or weaken generic SYSTEM refusal.

The source reader currently discards chunk representation evidence after
decompression. A future staging call cannot merely trust the old successful
read and assume the live chunk locators still denote those bytes. Preserve a
bounded in-memory digest of the ordered original chunk versions, flags,
compression, keys and stored bytes during successful source validation.
Physical offsets, publication timestamps and write sequences are excluded:
relocation may change them without changing the retained representation.
This is ephemeral comparison evidence, not a new wire identity or graph edge.

Before staging, validate the live typed chunk closure against that evidence
through a fresh settled snapshot of the SAME physical owner. Enforce cumulative
read/work bounds, cancellation and memory admission. Compare database, algorithm,
physical identity and writer fence with the source's originating capture.
Do the potentially long chunk reads outside the root/KV mutex. Under the short
publication guard, recheck the exact fresh header sequence; concurrent change
refuses for bounded retry, not unbounded internal retry or trusting stale reads.
Then use the existing dependency transaction for one original-version/flags/body
FileRecord with no mutable path alias. Exact-repeat publication is idempotent;
different representation at the same identity is a collision. Preserve committed
receipts if publication or readback reports a post-commit failure.

This is staging under a live process-local guard, analogous to the existing
native semantic-catalog staging adapter. It cannot release that guard, select an
ASMT/ASMC/ASCM, advertise capabilities, create a durable task pin or admit a root.
The earlier durable-operation gate still requires full task retention/recovery
and reclamation integration. Direct task/control publication remains disabled.
Complete compiler source-union capture, staged namespace closure, selected-task
GC traversal and activation remain the coupled following integration—not
consequences of a successful immutable source copy.

`test_protocol`: existing native source tests prove decoding and process-local
ownership; immutable authority tests prove transaction boundaries; staging tests
exercise the four final reclamation barriers. None yet proves this composed
source-staging path. The hypothesis is that a captured old record can be staged
after its current alias changes, then reopened at its exact revision, without
changing HEAD/current alias/chunk representation or bypassing resource limits.

Falsifiers before implementation:

1. Given original v0/v1 ordinary/SYSTEM sources with compressed and plain chunks,
   capture, replace the current alias, stage, reopen, and compare exact original
   bytes/body/revision. A second stage is byte-stable idempotence. Both widths
   and all registered algorithms need coverage.
2. Given missing, wrong-role, changed-representation or corrupt live chunks,
   staging refuses before copying the record. Repaired framing/CRC must not hide
   changed content. Wrong physical identity/fence and header-change races also
   refuse; no current-path fallback is permitted.
3. Given cancellation, memory/read/work limits, allocation failure or injected
   publication failure, preserve the exact no-commit/committed distinction,
   release accounting, retain staging protection and allow a bounded retry.

Unit checks cover digest framing and representation distinctions; native
integration covers real file publication/reopen/current-alias preservation;
property cases span hash profiles and original versions/flags; fault tests use
existing transaction observers and bounded coordination. A copied test fixture
or only checking that a key exists would give false confidence. No network or
production dependency enters these tests. Established native stage watchdogs
apply, with two-second coordination deadlines and no joins while holding a
guard needed by the worker. Run the composed behavioral REDs before changing
production behavior; qualify the full affected/static/native suite before any
staging-enable landing. Refresh this entry map and exact API at that boundary.

Observed staging refinements (September17): exact-repeat byte stability requires
readback before entering the generic publisher's KV baseline flush. Keep the
existing generic flush/transaction semantics intact; under the existing root
guard, use its shared exact-entity reader and fallible one-entry receipt on the
no-write branch. Only a missing identity enters the physical transaction.
Fresh slot sequence and write high-water must also be monotonic relative to the
originating capture, in addition to the same owner/fence and final full-header
comparison. Actual failing regressions establish both requirements.

Retry has two distinct boundaries: resource/cancellation/concurrent-change
refusals have no commit, while a hard dependency failure preserves the old
authority but leaves the existing durability coordinator failed. That latter
case requires reopen/recovery before another write; never clear its latch as
part of source staging. A post-commit observer failure instead preserves its
committed receipt and permits exact idempotent readback. The composed tests must
assert these existing producer contracts rather than assuming all failures can
retry in the same process.
## Following catalog assembly: bounded ordered construction

The next construction boundary consumes an already ordered, exactly counted
stream of paired base/request revisions. It does not discover that stream or
claim its compiler dependency union is complete. In particular, namespace
configuration rows also belong to the existing source fingerprint, whereas
these catalogs contain only protected non-HEAD sources. Do not substitute a
protected-only digest for that existing fingerprint.

Territory searched: the ASCN encoders/borrowed decoders, native paired traversal
and point seek, source fingerprint, semantic catalog compiler/native adapter,
legacy B-tree writer, KV rebuild sorting, query ordering runs and migration
root-map sorting. The latter three own specialized record formats and lifecycle
rules; none is a ready-made canonical path-union iterator. The legacy B-tree
writer publishes DirectoryIndex bytes through StorageEngine. Neither is a
drop-in ASCN writer. Avoid adding an external workspace owner just to assemble
an already ordered stream. Complete dependency discovery/sorting remains a
separate explicit obligation, not an inferred property of this constructor.

Use the qualified leaf/internal encoders and their shared control identity.
Consume one row at a time; form leaves at the existing 256-row or 1 MiB body
boundary. Fold completed leaves through a bounded binary carry forest. Each
internal node has exactly two children, uses the right subtree's first path as
separator, and is emitted only after both children. At end, fold the retained
forest from right to left. This gives deterministic, bounded-depth construction
without unary nodes, a world-sized map, or reads of all earlier nodes. The
existing format permits this shape; no persistent layout or maximum changes.
Base/request catalogs use the identical row partition and path set, including
explicit absences. One synchronous node callback hands immutable encoded bytes
to the caller's existing staging owner. No new file, KV, header, transaction,
task/control publisher, capability advertisement or activation route belongs
in this construction helper.

Account for the leaf window, both output buffers, row/child projections,
previous/current paths, hash state, forest minima/identities and returned roots
under the existing memory coordinator. Caller-owned iterator backing and sink
storage remain their own explicitly documented admission obligations. Reject
oversized owned capacities as well as oversized logical rows. Enforce exact
count, strict path ordering, selected-H nonzero identities, path validity,
checked node counts, and caller work/output/workspace limits. Check cancellation
and memory pressure around input and sink callbacks, including the final EOF
and final node emission. A failure returns no completed catalog; earlier
unselected emissions do not grant retention, resume, or visibility authority.

`map_territory` checkpoint: this limited assembler's producers are the counted
row iterator and existing encoders; consumers are the staging callback and a
private-field result containing roots/counts. Native task selection, source
union discovery, GC and activation are still unimplemented consumers, not
implicitly qualified by the assembler. The public publisher's semantic-task
refusal must remain unchanged throughout this unit.

`test_protocol` for this boundary:

- Existing byte writer and native reader tests remain unchanged; add assembler
  tests alongside the semantic source capture specs. Independent hand-built
  control bytes/digests and a test-only ordered map provide the oracle.
- Given two paired paths with a base/request replacement and explicit absence,
  constructing both catalogs must match independently encoded leaf bytes and
  selected-H identities for every registered hash algorithm.
- Given more than 256 paths, odd leaf counts and maximum-length paths, traverse
  every emitted root independently: exact rows, range separators, no missing or
  repeated edges, correct counts, bounded height, and children emitted first.
- Given malformed/unsorted/duplicate rows, count mismatch, an iterator error,
  a sink error, exhausted bounds, cancellation or allocation refusal, return
  no completed result and release the helper's memory. Explicitly test failures
  at final EOF and after the final sink callback.
- Unit tests prove deterministic bytes and limits; integration combines the
  assembler with the existing readers against actual emitted objects; property
  tests compare varied partitions and both catalog sides to an independent map.
  A native service E2E test does not yet apply to this byte-only helper. Later
  task/restart/GC and ordinary-service gates remain owed, not replaced by mocks.
- False confidence: encoder/decoder agreement alone, a sink that secretly
  stores the whole stream in production, retained per-row capacities, or a
  result called a closure permit. No network or waits are needed; small
  deterministic tests target ten seconds, with existing bounded native stage
  runners as the outer deadline. Large stress cases need explicit resource and
  elapsed-time bounds rather than an unbounded test loop.

Write the independent two-path, split/ordering and failed-callback tests against
a refusing scaffold first. Preserve the observed RED, then implement and expand
adversarial/resource coverage before final native qualification. This design
does not relax the coupled durable capture/publication gate above.

## Following source-union entry: shared alias discovery

The complete source-union map still has three distinct obligations: discover
references from both configuration sets, resolve exact aliases/modules under
the captured view, and produce an ordered deduplicated path stream including
namespace configuration revisions. None is supplied by the catalog assembler.
The first bounded step is alias discovery through the existing schema owners.

`map_territory`: `parser_registry_compiler` owns strict registry parsing, MIME
normalization, duplicate checks and the512-entry bound. Its compiler resolves
one alias per normalized MIME entry. `index_configuration_source` owns the
strict corrected index schema; `index_configuration_compiler` resolves an
explicit parser only when at least one non-metadata field exists, and resolves
mapper aliases from selectors. Its `unused_parser_does_not_create_dependencies`
regression deliberately permits a missing unused parser. Both compiler traits
return borrowed dependency records. Actual native loading therefore needs an
owned, bounded prepared snapshot before those borrows, not live resolver calls
or an unbounded global map. That adapter remains the next dependent obligation.

Add a byte-only alias visitor beneath `semantic_source_capture`, with explicit
source kind (parser registry or index configuration), proven optional source,
source/workspace/alias-occurrence ceilings, role-tagged borrowed alias callbacks,
shared memory accounting and cancellation. None means a proven absent source,
not a read error or permission to infer absence. Return the completed occurrence
count only on success. This visitor is not a compiled-schema validity token,
captured snapshot, complete union, artifact/executor proof or publication permit.

Extract the registry's existing parse and workspace calculation for its compiler
and this visitor to share; preserve exact schema errors and its allocation-error
classification. Reuse `index_configuration_source::parse` and factor only the
existing used-parser predicate for both consumers. Do not add another JSON
parser, change compiler bytes or resolve dependencies during discovery. Some
semantic checks (scope normalization, field compatibility and compilation) still
belong to the compiler, so successful discovery must not imply those passed.

Registry occurrence order follows its existing normalized MIME order. Index
discovery visits its used explicit parser, then mapper occurrences in source
row order. Repeated aliases, including one alias in different roles, remain
occurrences; the later source-union owner must sort/deduplicate paths and retain
both role requirements. No process-wide alias set is introduced. Admit bounded
AST memory before parsing and check cancellation/admission around every callback,
including the final one and an empty result. Enforce occurrence limits before
handing each alias to the caller. Callback errors propagate without a completed
result; earlier notifications cannot confer closure authority.

`test_protocol`: existing registry/config compiler specs are the consumer guards.
Independent expected aliases/roles must cover normalized MIME order, repeated
aliases, mixed parser/mapper selectors, missing/default/empty sources and the
unused-parser rule. Compare discovery's role/name set with actual compiler
resolver calls for valid configurations, without using the visitor to build its
own oracle. Malformed/duplicate schemas, legacy versions, invalid aliases,
unknown members/policies and bounds must refuse. Callback failures, final
cancellation/pressure and retry must release memory; source-bound allocation
probes preserve the parsers' explicitly inherited allocation limitations.
Tests are deterministic and small; no network, module execution or database
writes belong to this visitor. Later native alias/module and full-union/restart
tests are still required. Begin with three positive tests against a refusing
scaffold in an isolated worktree; preserve actual RED before implementation.

The wider workspace audit found no drop-in path-union sorter: directory repair
sorts depth/ChildEntry records with its own lifecycle; native query ordering
sorts fixed-hash references into its specialized row spool. Reuse shared private
path/capacity primitives and bounded run-tier patterns if external union sorting
is required, but do not reuse those record formats or stale-cleanup authority.
Exact captured namespace traversal and complete-union ownership remain an entry
gate before selecting that next implementation, not settled by this inspection.

### Next native dependency edge: one captured alias/module pair

September17 territory review traced both compiler snapshot traits, all their
implementations, APAL/APWM readers, raw-artifact inspection, dependency-record
encoding, protected-source loading and native task inventory. Production does
not yet implement either compiler snapshot trait. Existing implementations are
test fixtures. The legacy `WasmPluginRuntime` implements the old `handle` ABI
and host imports, not corrected pure profile2. Do not use it as availability
proof for a newly compiled corrected dependency.

The next bounded native prerequisite reads one alias and its referenced raw
module from the same `NativeSemanticMutationInventoryV1`. It must reuse the
existing captured lookup and source loader, with one cumulative read-byte
budget across both records and their chunks. It may not consult live aliases,
recapture between reads, publish a file/control, or create another physical
owner. The result retains the two existing accounted protected-source values,
their exact original revisions and the capture/staging lifetime.

Centralize canonical path construction beside the existing identity readers:
alias paths use BLAKE3 of exact alias UTF-8, module paths use the recorded raw
module fingerprint. Preserve existing path checks and error meanings. Reject
invalid/oversized alias names before lookup and use fallible bounded path
allocation. A missing alias is proven absence in this capture; a present alias
whose module is missing, malformed or inconsistent is an error, never absence.

Use the shared APAL/APWM/raw-module inspector before returning the pair. Cache
at most two bounded canonical dependency records, one per declared parser/mapper
role, through the existing dependency encoder. Their profile2 fields describe
the exact required executor; they do not prove that the module has valid core
bytecode, correct exports/imports or an available executor. Unknown requested
roles cannot manufacture a dependency. The result exposes borrowed decoded
records for later prepared compiler snapshots, plus its original source values
for guarded staging and union accounting. No process-wide alias map or module
cache is added. Input bodies, short path/record workspace and temporary identity
inspection all retain explicit shared memory admission and cancellation checks.

The later bounded prepared snapshot must copy only necessary alias/record
metadata, releasing module bodies between references, and preserve exact
base/request source selection. Neither this pair nor alias discovery completes
that snapshot, namespace configuration capture, sorted union, fingerprint,
durable task closure, GC or activation. Resume through existing paired catalogs
and executor admission remain explicit downstream obligations.

`test_protocol`: preserve three actual behavioral REDs before implementation:
exact native alias/module capture with independently framed role records at32/
64-byte database widths; an old capture surviving alias replacement while a
fresh capture sees the new module; and proven old absence after a later alias
addition. Compare database bytes before/after every read-only exercise. Extend
with missing/corrupt aliases/modules, metadata/digest/role disagreements, exact
read/workspace/body bounds, cancellation/pressure at final checks, actual
allocation failures and retry/accounting release. Keep the identity-versus-core
bytecode distinction explicit in a regression. Existing artifact identity,
protected-source, compiler and native library suites remain consumer guards.
Use actual bounded native files, not a mocked mutable alias lookup. These tests
perform no network calls or module execution; native stage deadlines and small
fixtures bound them. Later service/plugin lifecycle and durable restart/GC
tests remain required rather than being inferred from this local read edge.

September17 candidate refinement: the native pair reserves32KiB for at most two
bounded dependency records, short canonical paths and metadata, plus the existing
identity inspector's16KiB temporary workspace. Thus its workspace admission is
48KiB; source bodies/physical decoding remain separately charged through the
existing reader. The module cap must be1..64MiB even for absent aliases. Chunk
ceilings are per source, while the physical read-byte budget is cumulative across
both records and every referenced chunk. An independently summed locator test
must pass at the exact total and refuse one byte below it.

The observed three REDs are retained. Candidate1 adds six native tests covering
all admission bounds/Unicode names, malformed or missing identity/dependencies,
exact paired read/workspace/body limits, final cancellation/pressure for both
presence and absence, actual path/body/record allocation failures and retry, and
identity versus invalid core bytecode. Each uses an actual disposable native file
and compares database bytes before/after read-only work. Existing protected-source
tests cover physical corruption, compressed chunks and captured-reader bounds;
artifact tests retain APAL/APWM framing and metadata checks. Those component
passes do not replace the candidate's actual paired integration tests.

Test-protocol review: unit tests are useful for pure framing, but native captured
integration is the decisive proof of this boundary. Independent role bytes and
physical locator sums avoid writer/reader self-agreement. Existing all-hash
compiler parity and malformed-fixture suites supply bounded property coverage;
manual inspection alone is insufficient. No network, stdin or external service
exists in these cases; small fixtures should finish in seconds under the native
stage deadline. Full service E2E remains owed at runtime integration, rather than
fabricated here. Candidate files and RED evidence are isolated from the frozen
alias-discovery qualification; no production or retained database is involved.

### Following native compiler boundary: prepared per-source alias snapshot

September17 territory review: the registry and configuration compilers return
borrowed dependency records through `ParserAliasSnapshotV1` and
`IndexConfigurationAliasSnapshotV1`. Their only implementations remain test
fixtures. The native pair now supplies verified captured metadata, but retains
the full module body and does not implement either trait. Copying every module
into a global compiler map would violate the bounded-source contract.

Add a prepared snapshot for the aliases referenced by one registry or index
configuration source. It borrows the same native capture/staging lifetime and
owns only sorted exact alias names, requested-role markers and bounded encoded
dependency records. Preparation reuses shared alias discovery; compilation uses
binary search and existing record decoding, with no subsequent physical lookup.
Missing prepared aliases/roles are explicit captured absence/unavailability;
asking for an alias or role outside the prepared set is an operational misuse,
not proof of absence. Repeated alias occurrences resolve one physical pair, and
one alias requested in both roles preserves both records. Unused parsers remain
unused. Release each pair's module/source buffers before reading the next alias.

The first native entry explicitly resolves this capture's current protected
sources. It does not represent requested replacements or retained catalog sides.
Those adapters must select their own exact source side before later task
integration; callers may not relabel this snapshot as a complete base/request
union. A supplied configuration is caller-owned immutable bytes, not itself a
namespace capture token. Artifact/bytecode/executor admission remains separate.

Use the existing pair implementation with an injected captured lookup so every
alias shares one cumulative physical-read budget. Preserve the public pair's
behavior. Two deterministic discovery passes permit exact occurrence/name
admission before allocation, without quadratic vector insertion or a whole-world
map: count and sum bytes, admit/fallibly collect, sort and merge requested roles,
then load each unique alias. Retained rows/names/records and parsing/pair scratch
remain separately charged to the same coordinator; an explicit snapshot-byte
ceiling bounds the retained table. Check cancellation/admission before work,
between references, after final completion and on trait lookup.

`test_protocol`: existing discovery, plugin-pair, registry/configuration/parser
context and native captured-reader tests are regression guards. Three initial
behavioral REDs must use actual native files: compile registry and mixed-role
configuration with independently framed dependency records at32/64-byte widths;
preserve an old prepared snapshot across alias replacement while a fresh capture
changes; distinguish captured missing aliases, unused parsers and out-of-set
lookup. Compare database bytes around read-only preparation/compilation.
Then cover malformed sources, missing modules/roles, cumulative multi-alias read
bounds, duplicate-load avoidance, table/name/record allocation failure, retained
body release, callback-final/lookup cancellation and pressure, and retries.
Unit/property cases help bound lookup/accounting; actual native compiler
integration is decisive, not a mock resolver or encoder/decoder self-oracle.
No network, module execution or service startup exists in these tests. Use small
fixtures and the bounded native stage runner; service and durable task E2E remain
owed. Keep this candidate isolated until the frozen plugin-pair final run lands.

The namespace audit remains a separate owed edge: selected namespace reads use
captured-header high-water checks with live publisher locators, whereas source
inventory owns a settled KV snapshot. Shared directory validation lives in
`read_view_native.rs`; shared bounded ordering/navigation lives in
`namespace_seek.rs`. Future captured namespace traversal must reuse those rules
without fabricating a user authorization/read-view object or consulting live KV.

### Namespace-reader prerequisite: bound directory decoding before allocation

September17 source review traced all three selected-directory decoder consumers:
selected scanning, selected point/permission lookup and descendant authorization.
They share `decode_validated_selected_directory_node` in`read_view_native.rs`.
Its legacy B-tree decoder allocates the declared internal-key vector before the
selected owner checks canonical fanout; flat and B-tree leaf decoding likewise
materialize all children before checking their selected limits. A three-byte
internal header can therefore request65,535String slots before rejection. This
is a source-review finding until the isolated allocation tests run, not an
observed production incident or a reason to change valid persistent bytes.

Before wiring another bounded captured namespace reader, enforce those existing
selected limits before materialization through the shared directory/B-tree
owners. Keep legacy decoding behavior in its existing adapter; do not add an
independent format parser. Preserve canonical round-trip validation, exact
ordering, inherited ranges, flat-root/B-tree-child rules and selected errors.
No authorization, physical lookup, mutable owner or staging permit is added.

`test_protocol`: three independent byte fixtures must first demonstrate actual
excess allocation/child decoding in the existing selected path: a three-byte
maximum-count internal header,257flat children and41actual B-tree leaf children
behind a40-count header. Use the thread-local allocator's non-failing occurrence
measurement; assert bounded peak allocation or name-decode count, not merely
that malformed data is rejected eventually. Then preserve valid flat/B-tree
cases at both widths, malformed/version/framing/count boundaries, old generic
decoder behavior and actual selected native-reader integration. Unit allocation
measurements establish timing of refusal; existing native selected-reader tests
establish integration, and final runtime service E2E remains owed. Small fixtures
have no network/stdin; use the bounded native test runner. Keep the RED and
candidate separate from the frozen prepared-snapshot qualification. Include the
adjacent prepared-snapshot table/name allocation probes in this follow-up before
fullU1readiness; they are not claimed by the earlier record-copy probes.

### Following namespace integration: shared values, distinct source results

September18 read-only map confirms that the protected source result is also the
receiver of`stage_retained_copy`. Merely admitting descendant configuration
family0x0002in`validate_source_path` would therefore change that publication
boundary. Do not do so. The existing protected families1/3/31/32and their
result type remain distinct from a namespace configuration read result.

The next reader must use the inventory's settled KV snapshot and captured
header, not the selected service reader's current locator lookup. Reuse the
existing physical read, whole-entity, FileRecord/chunk, SystemFamily and
directory validation owners. A private shared decoded-source value can support
both public result types; only the protected wrapper retains the staging API.
Preserve original path/family, exact content revision, version/flags, bounded
chunk decoding and cumulative physical-read accounting. A namespace revision
is read by its supplied immutable content key, never by the current path key.
The supplied tree/revision alone is not root admission or user read permission.

For namespace discovery, reuse`namespace_seek`for ordered child navigation and
the selected directory rules for fanout, canonical bytes, names and inherited
ranges. Keep directory identity validation shared as well. An iterative bounded
path/depth traversal emits descendant configuration paths/revisions in complete
path-byte order, including directory-prefix ordering; callbacks are provisional
until successful completion. One lookup/work counter spans every visited node
and configuration body. Retain only bounded ancestors and the current source;
no whole-tree map or independent format parser. Final cancellation/admission
checks apply to empty, complete and early-stop outcomes.

Entry tests must use real small native files with independently specified
source bytes:32/64-byte trees, flat/B-tree nesting and prefix-order cases,
old/fresh captures across changes, exact revisions despite current-path changes,
missing/malformed referenced entities, irrelevant ordinary files, explicit
namespace/protected-result separation, exact work/read/body limits, partial
visits, allocation failures, cancellation and release/retry. Preserve all
protected staging, selected-reader and compiler regressions. This is the next
integration design, not implemented functionality or a full-union claim.

The selected namespace reader also requires unflagged ordinary FileRecord and
chunk representations. Its configuration adapter must preserve that rule;
the protected reader's support for SYSTEM source/chunk forms cannot silently
broaden namespace reads. Keep the existing protected return signatures and
accessors, with an internal source-kind parameter at the shared decode boundary
and a private shared value. Do not expose a conversion from the namespace
result to the protected staging result. Existing protected-read validation
order and error details are compatibility checks in this extraction.

Traversal must also preserve the selected FileRecord-to-directory comparison:
path, total size, content type, creation time and update time all agree before
emitting a configuration. The standalone exact-revision read does not assert
that additional directory relationship. Keep the comparison shared, and test
each mismatch rather than treating a valid content hash alone as sufficient.

September19 qualification update: the directory prerequisite is now supported
by the original three measured failures, nine new regression cases and final
Linux/macOS/Windows results against the same frozen source. The bounded table
collector and B-tree entry enforce existing limits before excessive collection
allocation; legacy decoder behavior and canonical formats are unchanged.
Existing child-string, internal-key and canonical serialization allocations
remain inherited limitations, not a claim of universal allocation recovery.
The namespace adapter described above is still the next implementation unit.

### Namespace adapter implementation and review — September19

The three native-file entry failures were observed before implementation.
The adapter now shares the private decoded-source value while preserving the
protected result and its family validator. Directory identity and all five
directory-to-FileRecord metadata comparisons are shared with selected readers;
the existing bounded directory decoder and ordered seek remain their owners.
Current namespace discovery retains the original captured header/KV snapshot,
one cumulative physical-read/work counter, bounded ancestor frames and only
the current source. Ordinary file bodies are outside this enumeration.

Candidate2 passed1,182tests, including fourteen namespace cases; formatting,
strict Clippy and the unchanged1,501reviewed audit identities passed. Review
then added a work-budget sweep and entry/callback cancellation/pressure checks.
That sweep reproduced a FileRecord-read work refusal as the wrong error variant
at budget13. A namespace-only result mapping corrects it without changing the
protected decoder; candidate4 is under final preflight. The16namespace cases
and native final gates must all pass before this adapter is considered qualified.
Original failures and every subsequent source packet/raw result are retained.

### Complete-union entry audit — not yet an implementation design

The protected catalog and full fingerprint have different path sets. ASCN
archives protected non-HEAD sources only; the fingerprint also includes
namespace-resident configurations. A future union owner must not put namespace
rows into the protected reader merely because the byte-only node encoder accepts
canonical paths. Complete ordered traversal of both trees establishes namespace
presence/absence; failed or partial reads cannot establish absence.

Current plugin-pair and prepared-alias adapters resolve current sources within
one capture. They do not apply requested protected replacements or resolve a
retained catalog side. The next integration must reuse their identity/schema
owners while selecting exact base/request revisions explicitly. Include aliases
and modules referenced by either side, even when the requested configuration
removes the old reference; explicit absence still needs a completed lookup.

Staging protection prevents reclamation, not concurrent publication. A root
published after a KV capture is not readable through that older capture. The
request's complete staged tree and protected source revisions need an explicit
capture/publication order plus base/generation rechecks; substituting current
locators or calling a second capture silently is not an acceptable workaround.

The inspected scratch sorters are domain-specific: directory repair sorts
ChildEntry/depth records and cleans stale prefixes; KV rebuild sorts fixed hashes
with replacement chronology; native query ordering sorts FileKeys with query
record payloads; migration root maps sort fixed root identities. None directly
implements canonical variable-length source-path union. Reuse the shared
`v4/private_workspace.rs` path/file/capacity primitives. Any new source-union
spool must bound row bytes, sort windows, run metadata, merge fan-in, disk use and
all I/O/work, and must not load the entire source set into a map. A spool is
disposable scratch, not a new persisted database format or restart authority.
Exact API, ownership and failing-first union tests remain the next entry gate.

### Next bounded prerequisite: disposable source-path ordering

The source-set owner needs a canonical, deduplicated path stream before it can
resolve every base/request identity and feed the existing fingerprint/catalog
builders. Alias occurrences arrive in schema order, may repeat across many
configurations and may refer to different modules on the two sides. Namespace
traversal order alone therefore cannot order the complete source set.

Implement a private source-path workspace beneath the capture module, using
the existing private-directory/file/capacity helpers. This is a permanent
domain helper for disposable scratch, not a second database owner, portable
format, durable task checkpoint, compiler snapshot or complete-union token.
Do not change any existing repair/query/migration workspace representation.

Its builder accepts canonical absolute UTF-8 paths under explicit input-count,
path-byte, sort-memory, stored-byte, cumulative-I/O and minimum-free-space
limits. Own one small sort window, bounded merge fan-in and logarithmically
bounded run descriptors; never retain all input paths or all initial runs.
Fallible allocations and memory admission precede their work. Sort by complete
path bytes and deduplicate without losing any unique path. Empty input is valid
for this helper, but cannot stand in for a complete capture's mandatory globals.
The captured-source owner, not the sorter, establishes membership and absence.

Use a private versioned, counted, checksummed scratch frame with bounded path
length. No unchecked count may drive allocation. A run must validate its complete
framing, order, count and end before declaring success. Merge output is admitted
before writing while charging simultaneous input/output storage; retire only
exact owned run files after their replacement completes. Unique private task
directories avoid stale-prefix cleanup and collision with unrelated work.
I/O/cancellation/pressure failure poisons unfinished work; no failed build may
return a finished set. Finished cursors borrow their workspace lifetime, retain
bounded accounted scratch and preserve errors rather than shortening the stream.
The workspace is not resumable; higher-level durable capture owns restart.

`test_protocol`: first observe failures for (1) unsorted duplicate paths split
across tiny runs yielding one exact independent sorted set, (2) prefix/Unicode
ordering and independent repeated cursors, and (3) empty/boundary input with
memory release and a preserved unrelated sibling. Then cover every truncated
frame/header, checksum/count/order/trailing-byte mismatch, canonical path limits,
input/sort/storage/I/O/free-space limits, real allocation refusal, cancellation,
pressure, partial writes and unusable-after-failure behavior. Compare varied
partitions and merge fan-in against a test-only ordered set, and assert peak
retained descriptors, open inputs and admitted bytes. Native temporary-file
tests use existing bounded runners; each small deterministic case should finish
within ten seconds. No external service or production fixture is needed.

After that prerequisite, finish the actual source owner: discover both sides
through the shared schema readers, resolve exact protected replacements and
retained source selections, merge namespace identities separately, and bind the
complete result to the existing paired catalogs and source fingerprint. Those
integration APIs and capture ordering still require their own concrete tests;
the path workspace alone does not close U1 or enable task publication.

### Composition refinement: preserve existing namespace ordering

September19 review of the actual traversal finds a useful next boundary:
`visit_namespace_configuration_sources` already owns a bounded ancestor stack,
one cumulative lookup/work budget, exact metadata checks and complete-path
ordering. The protected catalog has a separate ordered cursor. Do not copy
namespace identities into a second unordered whole-world map, discard their
revisions into a path-only sort, or repeatedly rescan the tree for each path.

The path workspace can order the protected globals, aliases and module paths.
The two namespace streams are already ordered. Expose a pausable cursor through
the existing namespace traversal owner, then merge base namespace, requested
namespace and protected-path streams with one head from each. Equal namespace
paths pair their exact revisions; a path appearing on only one side establishes
absence on the other only after that ordered cursor advances past it or finishes.
Failed reads are errors, never absences. This preserves the different protected
catalog and full-fingerprint membership contracts without another disk format.

Keep the existing visitor as an adapter over that same cursor, preserving its
callbacks, early-stop result and final cancellation/pressure checks. Returned
source rows retain the captured inventory and their own memory; cursor state
retains only ancestors and its cumulative budget. A failed cursor is terminal,
and even a successful cursor is not root admission or complete source-set proof.
Verify interleaved independent cursors, old captures after publication, retained
rows, empty/end/failure lifecycle, prefix order and exact read/work bounds, plus
all existing namespace visitor regressions. This extraction remains part of the
source-union integration milestone, not a separate user-facing completion claim.

The remaining entry decisions are explicit requested-source selection, immutable
staged-tree publication-before-capture ordering, and exact base/generation
rechecks. Reuse the current plugin-pair identity/schema owners for both sides;
do not silently resolve a requested replacement through the current alias. The
eventual capture result must feed the existing paired catalogs and fingerprint
before durable task/GC integration can rely on it.

### Selected plugin inputs within the source-union owner

The current plugin-pair reader and prepared alias snapshot already share the
artifact inspector and dependency encoder. Preserve that ownership when adding
requested/retained selection: extract an internal pair reader which asks its
caller for the exact alias source, then the artifact source named by that alias.
The caller supplies already-accounted source observations from the same capture;
this is not a public callback that grants capture or publication authority.
Reject another capture or a mismatched path. Never replace an explicit absence
or selection error with a current-path lookup. Reuse the existing inspector,
source bounds, result lifetime and final cancellation/admission checks.

The ordinary current-source adapter supplies its existing cumulative lookup.
The full source-union owner must supply its own cumulative lookup and explicit
base/request policy; a callback does not itself prove membership, absence,
global work limits, or complete capture. Likewise the retained catalog adapter
must distinguish an unlisted path from its explicitly absent row.

Falsifying cases precede extraction: retained alias/module A must produce A's
exact dependency records while the same capture's current alias is B; explicit
selected alias absence must not consult current B; a selected artifact read
error must survive unchanged, release reservations, and permit a fresh retry.
Run these with real native files, both identity widths and independent expected
dependency bytes. Extend with wrong-capture/path, late cancellation/pressure,
allocation and cumulative-budget cases before integration. Existing current
pair and prepared-snapshot tests stay unchanged. Unit/property tests cover the
selection and bounds; native files cover the actual read path. Public service
behavior remains U2–U7 work, not a claim from this internal extraction. All
small cases use bounded fixture work and the established timed desktop runner.

### Source-union composition: execution outline and remaining entry proof

The next composition must join the qualified pieces rather than introduce
another schema or scratch-record format. This outline is not an implemented
capture API or permission to publish tasks.

1. Validate a bounded immutable request: expected base NamespaceRoot, staged
   DirectoryIndex identity and a strictly path-ordered list of protected source
   replacements/deletions. The replacement list is request-sized, not a map of
   the database. Present replacements name immutable FileRecord revisions which
   must already exist in the same capture; omitted entries use captured current
   state, while an explicit deletion means absence. Do not load future staged
   objects through an older capture. Include changed protected paths as request
   inputs, without enumerating every unrelated installed alias/module.
2. Bind the base to the captured header's HEAD and its actual namespace closure,
   and read the exact semantic generation through captured A/B controls. Reuse
   the existing root/state/admission decoders and control readers.
   The current `load_namespace_authority_at_captured_header` method consults live
   KV under a lock, so it is not a substitute for the settled inventory lookup.
   Missing generation is an error, not invented generation zero. The initial
   source-union result must remain distinct from a durable task/resume permit.
3. Keep one operation's read/work counters across discovery and output passes.
   Separate each namespace traversal's ancestor stack from that common operation
   so two ordered trees can advance independently without resetting quotas.
   Resolve both mandatory globals and both sides of namespace configurations
   with the existing schema/alias visitor. For a discovered alias, resolve both
   base and requested selections through the shared plugin-pair reader; append
   its canonical alias path and each selected module path to the bounded path
   workspace. Do not lose removed dependencies or substitute current inputs.
4. Count namespace union paths by merging the two already-ordered streams, with
   at most one retained row per tree. Do not put namespace identities into the
   path-only sorter. Finish the protected-path workspace and use its unique count
   for paired catalog assembly. Read base/request sources for each protected path
   through the same cumulative lookup and pass their exact revisions/absence to
   the existing catalog builder. A source callback may stage retained copies
   under the held staging guard, but partial callbacks never mean completion.
5. Feed the existing fingerprint builder a strict ordered merge of protected
   paths and both namespace streams, using only BASE revisions or explicit
   absence. The count is protected paths plus unique namespace paths. Validate
   every iterator's successful EOF; retain concrete source errors through any
   iterator adapter instead of turning them into absence/count mismatches.
   Preserve the requested identities separately in the paired catalogs/tree.
6. Return accounted roots/fingerprint and the captured binding only after all
   checks complete. Guard lifetime, later durable source/catalog retention,
   checkpoint/task selection, restart/GC proof and activation's exact base and
   generation recheck remain mandatory following integration. This preparation
   cannot authorize an early HEAD change or expose staged data in listing/SSE.

Entry tests should compare the complete emitted pair set and fingerprint against
an independently constructed small map/preimage: empty trees with both absent
globals; overlapping/added/removed namespace configurations; changed protected
configuration and alias/module revisions on both sides; duplicated aliases across
tiny sort runs; and an alias removed by the requested configuration. Then test
wrong base/generation, unknown staged revisions, deletion versus failed read,
callback failure, shared-budget exhaustion across both trees/passes, final EOF
cancellation, allocator refusal, and private scratch cleanup. Existing retained
catalog readers must read emitted nodes and preserved source revisions; later
durable task tests must exercise actual restart and GC. No global collection,
second mutable database owner or per-path namespace rescan is acceptable.

Further source inspection narrows step2: `decode_immutable_namespace_authority`
calls `decode_namespace_tree_root_v0`, which still deserializes/reserializes an
entire root node through the older directory API. Do not reintroduce that path
into bounded source discovery. The existing selected-semantic-authority loader
already validates the fixed root, semantic state and HEAD admission without
materializing the tree. Factor its lookup-independent body over the existing
entity-lookup trait, keeping its live caller's current guard and validation
unchanged; the source owner can supply its settled lookup and accounting. Check
the directory itself through the qualified bounded namespace traversal. Add
captured-base and malformed-root/state/admission tests around that shared seam.
This refinement follows executable callees, not an assertion that the legacy
whole-root decoder has been globally replaced or qualified for this new path.

Captured-base entry detail: retain the exact selected generation control, not
only its integer, so the later task/activation owner can compare the established
slot/digest expectation. The internal base loader accepts the composition's one
lookup rather than creating a fresh quota. Its result retains capture and memory
but is not a public task or activation token. Keep the ordinary loader's healthy
header check before locking KV, with the shared validation body also checking
its supplied observation; do not weaken the live failure ordering to share code.

Account the actual canonical-control loader caps:64KiB bounds a FileRecord
entity, not the control payload. Root-admission and generation framing currently
admit up to1MiB before typed validation. Reserve the shared loaders' overlapping
slot bodies/copies and root/state scratch before reading, then retain only a
conservative bounded metadata charge plus the actual selected control capacity.
This inherits the shared loaders' existing allocator limitations; it is not a
new claim that all transitive allocations can recover from host OOM. Regressions
must cover the actual fallible root-entity allocation and post-load refusal.

Composition implementation uses `prepare_semantic_source_union`: its owned
result retains the original inventory, selected base/generation, requested tree,
paired catalog roots and fingerprint. The captured header sequence is the
inventory header sequence, not the root's older admission sequence. A paired
namespace cursor reuses the existing state machine with one shared lookup and a
separately accounted second traversal stack. All internal source/base/tree reads
and both passes consume that one read/work budget; alias occurrences also have
a global ceiling, including missing aliases whose lookup reads no entity.
Caller callbacks are arbitrary explicit operations: their retained sink storage,
additional I/O/publication and work must be budgeted by their owner. Preparation
quotas do not purport to meter external callback effects. Callback output remains
provisional, with its original concrete errors retained by iterator adapters.
No ASCM/task control, durable retention or activation is granted by this result.

### Guarded node persistence and the source-union sink — September19

The following integration is isolated while the preceding source-union native
qualification finishes. It is part of durable task integration, not a separate
claim that task selection, recovery or retention is ready.

`NativeSemanticMutationInventoryV1::stage_semantic_source_nodes` accepts one
node or one paired emission. It requires the original capture's live protection,
healthy selected HEAD, unchanged logical/physical identity and writer fence,
non-regressing physical frontier, and captured/current reader/writer declarations
for both capabilities25/27. It never sets those bits or advertises support.

Decode both inputs before writes, require ASCN for the captured database, bound
their encoded size and workspace, and deduplicate an exactly equal pair. Keep
the existing root guard across canonical existing-body readback and the sole
locked immutable entity publisher; release the KV guard before calling that
publisher. The shared system-file preparer and receipt translator remain the
owners of wrapper bytes and physical outcomes. No independent transaction,
framing parser or general semantic-control permission is introduced.

An ASCN identity can recur across captures with a different requested timestamp.
Reusing it must preserve its original FileRecord timestamps and compare the
complete canonical stored representation, not regenerate a conflicting wrapper.
A fully existing exact repeat returns before the generic publisher's KV flush,
preserving physical bytes. Mixed old/new pairs preserve old identities while
publishing only the missing entities through the same transaction owner.

Cancellation, accounting and pressure are rechecked at entry and immediately
before publication. Once a transaction commits, its success or committed-error
receipt is preserved; later cancellation cannot imply rollback. The source-union
error has a distinct control-publication variant so its iterator/callback bridge
does not erase that outcome. Whole-task cumulative publication/read/work quotas
remain the enclosing task owner's responsibility, not a per-pair sink claim.

Falsifying evidence: three actual positive-target failures precede implementation;
candidate1 passes939library cases. Candidate2 adds eight refusal/resource/fault
cases and passes947library plus420affected cases without runtime changes.
Candidate3's additional real union-sink/reopen, committed-error, exact collision
and concurrent-capture cases are under qualification. Ordinary semantic task
publishers remain refused. Complete durable typed retention and actual task
checkpoint selection/resume/activation must be qualified before that changes.

### Owned staging and physical-entry observation — September19 integration

`prepare_and_stage_semantic_source_union` owns both native sinks and returns a
privately constructed result that borrows the original capture/protection. It
bounds cumulative source-copy attempts, logical payload attempts and actual live
chunk-validation read bytes, plus per-pair node workspace. Equal base/request
revisions share one copy attempt per path; retries still consume their actual
read budget. Discovery retains its separate cumulative budget. These counters
do not claim total physical KV-flush or wrapper I/O accounting.

The retained-copy implementation is factored through a private metered entry;
the existing API, representation checks, transaction owner and committed errors
are preserved. Failures can leave unselected immutable dependencies, never a
selected task or admitted root. Interruptions after a dependency commit refuse
whole-operation completion without claiming rollback. Exact-bound, cold partial
write, larger-plugin, pressure/cancellation and zero-chunk tests accompany the
original reopen targets. Linux qualification passes1,384tests plus strict static
and audit gates; no all-platform or durable-task readiness follows from that.

The following `visit_captured_source_physical_entries` observation exposes the
same paired reader's physical reads: companion/checkpoint wrappers and chunks,
both catalog-node closures, exact retained FileRecords and their actual chunks.
It has no second parser, file owner or whole-set dedup table. Callbacks may repeat
and are provisional until the existing complete paired validation succeeds.
Original callback errors and immediate cancellation/admission failures remain
typed; late corruption or count disagreement invalidates all prior visits.
This is one source branch only, not selected task, namespace, resume, activation
or complete GC authority. Its two positive targets failed before implementation;
nine focused cases now pass, including exact cumulative budgets, repeated shared
references, first/final callback cancellation and pressure, actual allocation
failure, late corruption, late count mismatch and later catalog replacement.
The isolated Linux run passes1,393library/affected tests plus formatting, strict
Clippy and the unchanged1,501-entry audit. A private callback type alias resolves
the observed Clippy complexity diagnostic without suppressing it. Broader native
and enclosing task qualification remain outstanding. Ordinary large-file
retention must use bounded typed metadata traversal, not this materializing
semantic-source reader.
