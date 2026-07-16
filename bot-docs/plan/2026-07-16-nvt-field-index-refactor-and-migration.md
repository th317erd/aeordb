# NVT Field-Index Refactor and Production Migration Plan

**Date:** 2026-07-16
**Status:** Revised after engine-level critical review; explicit decisions remain before Phase 3
**Scope:** User and virtual field indexes, field-index query execution, and migration from `FieldIndex` v0
**Explicit non-goal:** Redesigning the current disk KV page layout or its lookup behavior

## Outcome

Replace the current whole-index `FieldIndex` implementation with a disk-resident,
page-addressable index whose Normalized Vector Table (NVT) is a sparse,
non-authoritative navigation and planning aid.

The completed system must:

- keep memory bounded independently of total index size and configured index count;
- route values and query bounds through converter-produced normalized coordinates;
- use sparse NVT hints to land near relevant pages, then scan and recheck for correctness;
- tolerate missing, stale, resized, partially populated, or corrupt NVT data without returning incorrect results;
- update and split only the pages affected by a mutation;
- compile query predicates into conservative scalar spans or points before reading postings;
- choose query drivers using estimated work, not expression order or scalar width alone;
- intersect different fields by document identity, not by coincident NVT coordinates;
- preserve live-write visibility with a bounded mutation overlay;
- migrate existing v0 indexes online, incrementally, and resumably;
- retain an explicit rollback path until the operator finalizes migration; and
- leave authoritative files and FileRecords untouched by index-format migration.

## Architectural Invariants

These are acceptance requirements, not implementation suggestions.

1. FileRecords, file content, and the current namespace remain authoritative.
2. Index pages plus pending index mutations are derived accelerators.
3. The NVT is never authoritative for membership, ordering correctness, or existence.
4. Losing the complete NVT can make a query slower but cannot change its result.
5. A converter maps every accepted value to a normalized coordinate in `[0.0, 1.0]`.
6. A converter also defines canonical equality and, where supported, typed ordering.
7. Scalar collisions are expected. Actual typed values are rechecked before accepting or rejecting a result.
8. Physical page placement is unrelated to logical scalar order.
9. Every page carries enough metadata to continue a logical scan in either direction.
10. A stale hint is validated against page metadata before it is trusted as a starting point.
11. Empty NVT cells are valid. Readers search backward for an earlier anchor and scan forward.
12. Resolution changes create or discard hints; they do not require moving all index entries.
13. Every approximate pruning operation is conservative and cannot introduce false negatives.
14. Cross-field AND/OR/NOT operations use a shared document-identity dimension.
15. Memory limits account for mutation buffers, NVT tiles, page cache, query pins, and temporary compaction state.
16. Index migration never overwrites or deletes v0 indexes before successful cutover and validation.
17. Mixed v0 and v1 indexes are supported at field/strategy granularity during rollout.
18. No startup path silently performs a production-wide migration.
19. Persisted normalized coordinates use a deterministic fixed-point encoding;
    floating-point ratios are a conceptual/API representation only.
20. Index definition changes never reinterpret existing posting coordinates.
21. Query authorization and path restriction happen before counts, aggregates,
    pagination, scoring, or result materialization.
22. A query may span multiple effective index scopes, but document ordinals are
    never compared or combined across scope IDs.
23. Whole-namespace HEAD transitions and index-definition changes use bounded
    reconciliation/rebuild workflows, not unbounded per-file mutation batches.
24. Index artifact publication relies only on ordered, prefix-safe WAL appends;
    it does not assume rollback-capable engine transactions.
25. Startup loads registry and manifest roots only. Posting pages, value pages,
    and NVT tiles remain lazy and byte-bounded.
26. Obsolete index artifacts must have a bounded reclamation path; v1 may not
    exchange unbounded resident memory for unbounded WAL/KV growth.
27. Page IDs and document ordinals are never reused within their identity
    lineage; stale hints/references must fail validation, never alias new data.
28. Logical page order is independent from physical WAL placement, but range
    scans batch/coalesce bounded physical reads instead of issuing one random
    read per logical page.

## Scope Boundary: Keep the KV Stable

The current `DiskKVStore` continues using its existing fixed bucket pages and
current `NormalizedVectorTable` behavior. This project must not change:

- KV block format or version;
- KV stage sizes;
- fixed KV bucket-page placement;
- KV lookup, flush, expansion, or recovery semantics; or
- KV startup and snapshot behavior.

The shared `NormalizedVectorTable` abstraction is currently a coupling hazard,
but its bytes are also embedded in v0 `FieldIndex` files. The first refactor
therefore separates it into:

- `LegacyNvtV1`: byte-identical codec/core retained only for compatibility;
- `KvNvt`: a current-behavior wrapper over that legacy core;
- `FieldIndexV0Nvt`: a v0 reader/writer wrapper over the same legacy codec; and
- `FieldNvt`: the new sparse, tiled, hint-oriented implementation used only by v1 indexes.

The v0 and KV wrappers may share the frozen codec but not evolve semantics
together. No v1 field-index code uses the legacy/KV NVT API.

The future KV concern about logical page continuation should be recorded
separately. It is not a prerequisite for this field-index refactor because the
current KV maps normalized coordinates directly to fixed physical bucket pages.

GPU bitmap/texture execution remains a future optimization. This refactor keeps
fixed-point coordinates, document-membership dimensions, and page metadata
compatible with that direction, but does not introduce GPU runtime, positional
indexes, distributed index transfer, or a KV redesign.

## Source Ownership and Refactor Boundaries

The current implementation concentrates storage, caching, mutation, v0
serialization, and query behavior in a few large modules. Preserve public
facades during migration, but move new behavior behind explicit boundaries:

| Current area | Refactoring direction |
| --- | --- |
| `engine/nvt.rs`, `nvt_ops.rs` | Freeze a legacy codec for KV/v0 compatibility, wrap it as `kv_nvt` and `field_index/v0/nvt`, and build v1 `field_index/nvt.rs` independently. |
| `engine/index_store.rs` | Retain the v0 codec/reader in `field_index/v0`; split v1 manifest, artifacts, pages, cache, memtable, and registry into focused modules. |
| `engine/scalar_converter.rs` | Keep the v0 trait/codec; add converter v2 and adapters without reinterpreting serialized v0 converter state. |
| `engine/indexing_pipeline.rs` | Produce one typed `IndexMutationIntent`; format-specific writers consume it. Parsing/extraction must not write indexes directly. |
| `engine/query_engine.rs` | Split predicate compilation, costing, page scans, document-set composition, sorting, aggregation, and execution into planner/executor modules. |
| `engine/file_header.rs`, `entry_type.rs`, `entry_header.rs`, `entry_scanner.rs`, `storage_engine.rs` | Add a reader-capability header version, versioned `IndexArtifact`, and direct append/read primitives. |
| `engine/gc.rs`, `backup.rs`, `verify.rs`, repair modules | Teach maintenance code artifact reachability and derived-data repair policy before v1 can be activated. |
| server task/routes and CLI | Add migration control, validation, rollback, finalization, and diagnostics without overloading ordinary reindex. |

Compatibility rules:

- Existing public query, SDK, and HTTP request schemas remain accepted.
- Existing public `NormalizedVectorTable` exports remain a deprecated legacy
  compatibility alias for the documented transition; they never expose `FieldNvt`.
- `IndexManager` becomes a registry-backed facade; callers do not choose v0/v1.
- `IndexingPipeline` emits errors and mutation intents through one result path.
  Logging may supplement an error but must never replace or suppress it.
- New modules may read v0 state, but v0 modules must not depend on v1 modules.
- No transitional adapter may deserialize and clone a whole v0 index merely to
  satisfy a v1 page-level interface.

## Critical Review Findings and Risk Register

This table records the second-pass conflicts found against the current engine.
Items marked resolved are design constraints in this plan; they still require
implementation and tests.

| Risk/conflict | Consequence if missed | Resolution/gate |
| --- | --- | --- |
| Current engine transactions group durability but cannot roll back. | A partially failed batch could be incorrectly described or handled as atomic. | Prefix-safe immutable publication, pointer last, validation fallback, explicit hard-barrier commit errors. |
| One mutable active-pointer key is a metadata single point of failure. | A torn/corrupt latest pointer can hide an otherwise valid prior generation. | Independent checksummed A/B pointer slots; choose highest fully valid sequence. |
| Current KV pages persist only fixed hash-length keys. | Literal artifact strings would truncate/pad and collide. | Domain-separated canonical preimages hashed to fixed-width keys; payload identity recheck. |
| A new EntryType is unknown to old scanners. | Old binaries can fail as soon as the first v1 artifact exists, before cutover. | Compatibility binary and minimum-reader gate before any artifact append. |
| A system "capability marker" is invisible to old clean-start code. | An old binary might open existing KV pages without scanning the unknown EntryType. | Explicit A/B file-header version/capability upgrade that old readers already reject. |
| Current dirty scanner reads/hashes every non-chunk value. | Millions of page artifacts would make recovery scan all index bytes. | Lazy artifact payload verification during rebuild; verify on demand/scrub. |
| Current void reuse permits only chunks. | Immutable page churn could grow WAL and KV state without bound. | Artifact-generation cleanup, disk reserve, bounded mark state, and tested reuse/compaction policy before rollout. |
| Publishing NVT healing through the posting manifest over-couples disposable hints. | Hint correction would amplify manifest/pointer writes and pin generations. | Independent best-effort NVT manifest/pointer; every hint validated against pinned posting pages. |
| Converter bounds/canonicalization affect stored page coordinates. | Treating a mapping change as NVT-only would misroute or miss postings. | Immutable definition fingerprint; semantic changes require shadow rebuild/cutover. |
| Persisted `f64` coordinates are nondeterministic/awkward at boundaries. | Cross-platform bytes, NaN, endpoint, and bucket behavior become fragile. | Persist fixed-point `u64` normalized coordinates with checked integer cell mapping. |
| A process-local `u64` mutation sequence is not durable across restart. | Resume/cutoff logic could compare unrelated epochs. | Durable SourceVersion plus boot-scoped visibility token; idempotency by content-derived mutation/source revision. |
| One query can span child config/glob overrides. | Combining identical ordinal numbers from different scopes returns wrong documents. | Partition by effective scope, compose locally, merge globally by FileKey. |
| Rename/move may cross effective scopes. | Source removal or destination addition can become visible alone. | Multi-scope mutation transaction or degrade/reconcile every affected scope. |
| Current query routes may post-filter permissions. | Hidden rows can affect totals, aggregates, pagination, scoring, and EXPLAIN. | Authorization is a planner/executor constraint before every observable result. |
| Snapshot restore/fork promotion replaces HEAD directly. | Building one mutation per changed file can exhaust memory; indexes become stale instantly. | NamespaceTransition state and bounded Merkle reconciliation before indexes report current. |
| Existing sync diff materializes complete old/new trees. | Reusing it for a production catch-up can consume unbounded memory even for a small delta. | New checkpointable hash-pruning Merkle walker emits bounded changed-path batches. |
| Parser/plugin/config changes alter field meaning. | One index generation could mix incompatible values. | Full definition/dependency fingerprint and pinned shadow generation. |
| v0 itself may contain stale postings. | v0/v1 equality can reject a correct authoritative rebuild or bless two wrong indexes. | Authoritative source evaluation is the oracle; v0 comparison is diagnostic. |
| Existing v0 numeric config passed integer bounds through `f64`. | "Fixing" bounds during migration silently changes results. | Preserve effective v0 converter semantics; typed config is an explicit new-definition upgrade. |
| Immutable page revisions scatter physically in the WAL. | Logical range scans become one random HDD read per page. | Page descriptors plus bounded physical-offset prefetch/coalescing and optional locality compaction. |
| GC can race readers of retired manifests. | A long query/cursor may read artifacts after sweep. | Generation leases are GC roots; cursor leases are TTL/count/byte bounded. |
| One value/query can expand into unbounded tokens/postings. | User data can bypass cache budgets and exhaust memory/disk. | Definition and query complexity limits with explicit strict/lenient failure semantics. |
| Logical backup/peer sync does not naturally carry internal artifacts. | Restored active pointers can reference absent pages or foreign ordinals. | Distinguish physical copy from logical transfer; omitted indexes restore as `needs_rebuild`. |
| Current namespace mutation lock is global. | Premature per-scope locking introduces deadlocks during cutover. | Use existing global namespace barrier initially; shard only after lock-order proof. |
| Current indexing pipeline can log and suppress field failures. | A successful write can leave an index silently incomplete. | One mutation-result path; logging supplements errors/degraded state and never replaces it. |

## Target Components

### 1. Index Scope Catalog and Document Ordinals

All field/strategy indexes owned by one index configuration directory share a
scope catalog.

```rust
struct ScopeIndexCatalog {
  schema_version: u8,
  scope_id: ScopeId,
  owner_path: String,
  generation: u64,
  next_document_ordinal: u64,
  documents_by_ordinal: PageDirectory<DocumentOrdinal, DocumentDescriptor>,
  ordinals_by_file: PageDirectory<FileKey, DocumentOrdinal>,
  active_indexes: Vec<IndexId>,
}
```

Properties:

- A `FileKey` is the stable path-derived file identity already used by indexes.
- A `DocumentOrdinal` is allocated within a scope and shared across its fields.
- `DocumentDescriptor` records the FileKey, normalized path, current source
  FileRecord revision, and live/tombstoned state.
- Forward and reverse catalog maps are published together and verify as a bijection.
- New documents receive monotonically increasing ordinals.
- Deleted ordinals are tombstoned and are never reused within that `ScopeId`.
- Recreating the same path/FileKey after deletion creates a new document
  incarnation and ordinal; the reverse map points to the current incarnation.
- Routine compaction must not renumber ordinals. Renumbering would require an
  explicit whole-scope migration that republishes every dependent index as one
  coordinated operation, and is outside this refactor.
- Postings store ordinals instead of cloning 32- or 64-byte hashes repeatedly.
- Cross-field candidate sets use document ordinals, enabling Roaring-style
  bitmaps now and GPU-compatible membership textures later.
- Query results resolve ordinals back to FileKeys through the scope catalog.
- The catalog universe includes every live file matched by the effective config
  scope/glob, including documents missing a queried field. This is required for
  correct `NOT`, `exists`, and `missing` semantics.

This creates two deliberately separate bitmap dimensions:

- field-local scalar/NVT regions for page routing and cost estimation; and
- scope-wide document membership for correct cross-field intersections.

The config resolver selects at most one effective index configuration per file,
but a query path can contain several disjoint effective scopes because child
configs/globs override ancestors. The planner evaluates each scope independently,
converts scope-local ordinals to FileKeys, then merges results across scopes.
It never intersects ordinal numbers from different `ScopeId` values. Query-path
restriction is compiled as a mandatory predicate, preferably using `@path`, so
a narrow folder query does not scan the entire owning config scope.

### Index Definition Identity

An `IndexId` must identify semantics, not only `(path, field, strategy)`. Its
definition fingerprint includes:

- normalized owning scope and glob behavior;
- field name and source/extractor configuration;
- strategy and converter type/configuration;
- Unicode, collation, tokenizer, and multi-value semantics;
- parser/plugin ID, version, checksum, and relevant limits; and
- index-format and canonical-value schema versions.

Changing any component builds a new index generation while the old definition
remains active. Dependencies are pinned for the duration of build/migration; a
plugin/config change pauses or invalidates the build rather than mixing semantic
versions. Removal retires the definition through the same grace/GC process.

### 2. Converter Contract v2

The existing `ScalarConverter` is retained for v0 compatibility. New indexes use
an expanded contract:

```rust
trait FieldIndexConverter: Send + Sync {
  fn coordinate(&self, value: &[u8]) -> NormalizedScalar;
  fn canonicalize(&self, value: &[u8]) -> EngineResult<Vec<u8>>;
  fn compare(&self, left: &[u8], right: &[u8]) -> EngineResult<Ordering>;
  fn is_order_preserving(&self) -> bool;
  fn expand_value(&self, value: &[u8]) -> EngineResult<Vec<Vec<u8>>>;
  fn fingerprint(&self) -> ConverterFingerprint;
  fn serialize(&self) -> Vec<u8>;
}
```

Requirements:

- `NormalizedScalar` is persisted as a `u64` fixed-point coordinate over the
  conceptual ratio `[0.0, 1.0]`; diagnostics may render it as a ratio.
- Cell mapping uses checked integer arithmetic (for example a `u128`
  multiply-high operation), including exact handling of both endpoints.
- Page-local ordering is `(coordinate, canonical typed value, document ordinal)`.
- Equality never relies solely on coordinate equality.
- Signed integers use signed numeric comparison, not raw-byte comparison.
- Floating-point values use a documented total ordering and explicit NaN policy.
- Timestamp parsing failures return errors rather than silently becoming epoch zero.
- Multi-valued fields retain every canonical source value, not only the last one.
- Numeric configuration bounds remain typed; `u64` and `i64` limits are not routed through `f64` configuration fields.
- Converter state and the complete index-definition fingerprint are immutable
  within an index generation.
- String canonicalization pins Unicode normalization, case folding, collation,
  and tokenizer versions so Linux, macOS, and Windows produce identical bytes.

Any mapping-bound, canonicalization, collation, tokenizer, parser, plugin, or
field-source change creates a new index definition and requires an
authoritative shadow build plus cutover. Even when typed order is preserved,
existing pages contain old coordinates and bounds and cannot be reinterpreted.
Only NVT resolution changes are independent of posting-page generation.

Migration of an existing v0 definition preserves its effective serialized
converter behavior, including historical numeric-bound casting, so migration
does not silently change results. Correct typed integer bounds arrive through a
new versioned config schema and therefore a new definition fingerprint/shadow
build. Preflight reports lossy or ambiguous v0 bounds and offers an explicit
separate config-upgrade workflow.

### 3. Index Artifact Storage

Recommendation: introduce one new derived entry type, `IndexArtifact`, with one
available KV type tag. Its payload identifies the artifact subtype.

```rust
enum IndexArtifactKind {
  IndexActivePointer,
  NvtActivePointer,
  ScopeActivePointer,
  Manifest,
  NvtManifest,
  ArtifactDirectoryNode,
  PostingPage,
  ValuePage,
  NvtTile,
  ScopeCatalogPage,
  MigrationLease,
  MigrationCheckpoint,
}
```

Reasons to avoid ordinary `.aeordb-indexes/*.idx` FileRecords for v1 artifacts:

- page updates should not mutate user-visible directory trees;
- derived indexes should not inflate namespace snapshots;
- page payloads should not be chunked and reassembled as user files;
- millions of pages should not become children in a directory B-tree;
- direct artifact keys allow one KV lookup per page or tile; and
- GC, verify, backup, and repair can apply index-specific policy.

All IndexArtifacts except active-pointer slots are immutable and content-addressed. A page
rewrite or NVT-tile correction creates a new artifact; it never changes the
artifact visible through an older manifest. Posting and NVT active pointers each
use two stable mutable A/B slots; shared scope catalogs do the same. No other
IndexArtifact key is overwritten. Existing system task records remain the
mutable control plane and point to immutable migration checkpoints.

Artifact key domains must be distinct:

```text
index-active:{index_id}:{slot_a_or_b}
index-nvt-active:{index_id}:{slot_a_or_b}
index-scope-active:{scope_id}:{slot_a_or_b}
index-manifest:{index_id}:{generation}:{artifact_hash}
index-directory:{owner_id}:{generation}:{artifact_hash}
index-page:{index_id}:{generation}:{page_id}:{artifact_hash}
index-values:{scope_id}:{generation}:{page_id}:{artifact_hash}
index-nvt:{index_id}:{generation}:{tile_id}:{artifact_hash}
index-scope:{scope_id}:{generation}:{page_id}:{artifact_hash}
index-migration:{task_id}:{artifact_kind}:{artifact_hash}
```

These strings are domain-separated logical key preimages, not literal KV keys.
The disk KV format stores exactly `hash_algo.hash_length()` bytes; every
artifact key is therefore `H(domain || canonical identity bytes)`. Payloads
repeat the full logical identity, and reads reject a hash/identity mismatch.
No caller may pass a variable-length textual key into the fixed-width KV index.

All payloads carry magic, schema version, index/scope identity, generation,
length, and checksum. A key/payload identity mismatch is corruption.

On-disk artifact encoding is canonical, explicitly little-endian, and uses
fixed-width integer types plus checked length prefixes. It must not serialize
`usize`, Rust enum layout, or unstable `serde`/`bincode` representations.
Decoders enforce page, collection, recursion, and allocation limits before
allocating. Golden byte fixtures must round-trip identically on all supported
platforms, and every decoder is fuzzed.

Artifact directories should be immutable, fixed-fanout radix nodes keyed by
logical page/tile ID, not another mutable B-tree. Each node contains a populated
child bitmap, child artifact hashes, level/prefix metadata, and a checksum. This
provides bounded-depth exact lookup, copy-on-write updates, and predecessor
navigation through occupancy bitmaps without depending on physical KV order.
Corrupt directory nodes degrade only the derived index and can be rebuilt.

IndexArtifact payloads are lazy-verified during ordinary dirty-startup KV
reconstruction: the scanner validates the entry header/length and records its
fixed-width key without reading every page payload. Full hash/checksum validation
happens on page read, explicit verify, migration validation, and sampled scrub.
This prevents derived pages from turning every dirty start into a full index read.

### 4. Manifests and Crash-Safe Publication

```rust
struct SourceVersion {
  head_hash: Vec<u8>,
}

struct RuntimeVisibilityToken {
  boot_id: u128,
  sequence: u64,
}
```

`SourceVersion` is the content-addressed namespace HEAD. The existing file-header
sequence is not suitable because non-namespace maintenance can advance it while
HEAD is unchanged. The runtime visibility token orders in-memory overlays only
within one engine boot and is never compared across boot IDs. After restart,
manifests are reconciled from their source HEAD to current HEAD before being
called current. Mutation idempotency uses a content-derived `MutationId`
containing index definition, document, operation, and source revision; it never
depends on a process-local counter.

```rust
struct FieldIndexManifestV1 {
  schema_version: u8,
  index_id: IndexId,
  scope_id: ScopeId,
  owner_path: String,
  field_name: String,
  strategy: String,
  converter: Vec<u8>,
  converter_fingerprint: ConverterFingerprint,
  generation: u64,
  posting_directory_root: ArtifactHash,
  value_store_manifest: ArtifactHash,
  scope_catalog_manifest: ArtifactHash,
  first_page_id: Option<PageId>,
  last_page_id: Option<PageId>,
  next_page_id: u64,
  page_count: u64,
  posting_count: u64,
  document_count: u64,
  source_version: SourceVersion,
  applied_visibility: RuntimeVisibilityToken,
  previous_manifest: Option<Vec<u8>>,
  logical_checksum: Vec<u8>,
}
```

```rust
struct FieldNvtManifestV1 {
  schema_version: u8,
  index_id: IndexId,
  converter_fingerprint: ConverterFingerprint,
  hint_generation: u64,
  tile_directory_root: ArtifactHash,
  resolution: u64,
  tile_cells: u32,
  approximate_page_generation: u64,
}
```

Posting/value publication and NVT publication are intentionally independent.
The posting manifest is correctness-bearing. The NVT manifest is best-effort;
readers may use any converter-compatible hint generation, validate every hinted
PageId against their pinned posting directory, and discard it on any mismatch.
Hint healing and NVT resizing never republish posting pages or their active pointer.

Each active-pointer slot stores index identity, monotonically increasing pointer
sequence, registry/format state, current and previous manifest hashes, source
version/visibility, and checksum. Readers load both slots and select the highest
sequence whose payload and referenced roots validate. Publication writes the
older/inactive slot last. A torn/corrupt/newer-but-incomplete slot therefore
falls back directly to the other slot without scanning WAL history.

Publication order:

1. Write immutable new posting/value artifacts.
2. Copy-on-write the affected artifact-directory paths so the new directory
   roots resolve stable logical IDs to exact immutable artifact hashes.
3. Apply the configured derived-index durability barrier. A publication may be
   soft-durable because indexes are rebuildable, but recovery must then validate
   and fall back rather than trusting reordered/lost writes.
4. Write a content-addressed manifest containing the exact directory roots.
5. Validate that every referenced artifact is readable and checksummed.
6. Append the inactive A/B active-pointer slot revision last in the same ordered
   batch and insert its KV mapping last.
7. Keep the previous manifest reachable for reader retry and rollback.

AeorDB transactions do not provide rollback. Safety comes from prefix ordering:
the new pointer slot is last, and recovery accepts it only when its manifest and
roots validate. If a write fails before the pointer KV insert, the old pointer
remains active and new artifacts are orphaned. If a crash persists a pointer but
not all dependencies, recovery rejects that generation and reconciles from the
previous valid manifest. The implementation must not describe this as a general
atomic transaction.

Implement a dedicated byte-bounded artifact publication batch rather than
assuming the current generic `WriteBatch` supplies compression, versioning,
durability, or rollback semantics. Hard-barrier operations such as migration
cutover/finalization use an explicit commit method whose sync/header errors are
returned and durability-latched. They must not rely on `Drop`-only transaction
completion that can only log an error.

If a crash occurs before step 6, new artifacts are unreachable and disposable.
If an active manifest or referenced page is unreadable, the reader may inspect
the previous manifest for recovery. It may serve from that manifest only when a
complete retained mutation overlay bridges it to the query's visibility epoch.
Otherwise it marks the index degraded and uses an authoritative scan or returns
an explicit error. It must never return stale/partial index results as complete.

Readers pin a posting manifest and resolve every logical page ID through that
manifest's immutable directory root. This prevents an old reader from seeing a
new page revision during a split. Stable mutable page keys are forbidden because
they would break manifest-level visibility.

### 5. Posting Pages

Pages have a target payload size, not a fixed physical offset.

```rust
struct PageDescriptor {
  page_id: PageId,
  artifact_hash: ArtifactHash,
  min_coordinate: NormalizedScalar,
  max_coordinate: NormalizedScalar,
  min_canonical_value: Vec<u8>,
  max_canonical_value: Vec<u8>,
  previous_page: Option<PageId>,
  next_page: Option<PageId>,
  posting_count: u32,
}
```

Posting-directory leaves map PageIds to descriptors, not hashes alone. The page
header repeats descriptor identity/bounds/links and readers reject disagreement.
This lets a scanner enumerate a bounded logical run from compact directory
metadata, resolve artifact hashes to KV offsets, sort a prefetch window by
physical offset, coalesce adjacent reads, then process pages in logical order.

```rust
struct PostingPageHeader {
  magic: u32,
  schema_version: u8,
  index_id: IndexId,
  generation: u64,
  page_id: PageId,
  converter_fingerprint: ConverterFingerprint,
  min_coordinate: NormalizedScalar,
  max_coordinate: NormalizedScalar,
  min_canonical_value: Vec<u8>,
  max_canonical_value: Vec<u8>,
  previous_page: Option<PageId>,
  next_page: Option<PageId>,
  posting_count: u32,
  live_count: u32,
  checksum: u32,
}
```

Posting entries contain at least:

```rust
struct Posting {
  coordinate: NormalizedScalar,
  value_ref: ValueRef,
  document: DocumentOrdinal,
  source_record_hash: RecordRevisionHash,
  value_ordinal: u32,
  flags: PostingFlags,
}
```

Page rules:

- Pages are logically ordered through stable page IDs and next/previous links.
- Page IDs are monotonically allocated from the manifest and never reused in an
  index lineage, including after merge/deletion.
- A manifest's persistent page directory maps each stable page ID to one exact,
  immutable page artifact.
- Physical WAL offsets may be arbitrary and may change between revisions.
- Readers validate page identity, generation, checksum, converter fingerprint,
  and scalar/value bounds.
- Inserts use a hint, backtrack if necessary, scan, then mutate one page.
- Full pages split locally. New left/right artifacts and the copy-on-write page
  directory are published behind one new manifest, so readers see either the
  complete old chain or the complete new chain.
- Deletes create page-local tombstones or mutation-overlay removals.
- Page merging is deferred to compaction; it must not require cascading link rewrites.
- Compaction may repack a bounded logical run for physical locality, but remains
  copy-on-write and cannot make locality a correctness requirement.
- Readers deduplicate by `(document, value_ordinal)` across page revisions and overlays.

Initial target sizes and split thresholds must be benchmarked. Start testing at
64 KiB target pages with split near 90% and compaction below 30%, but do not
hard-code these values into the format.

Benchmark per-page compression independently by artifact kind. Compression must
remain bounded and page-local; checksums/content identity cover a documented
canonical byte representation, and decompression limits are validated before
allocation. A flush batch also has a byte cap so many dirty pages cannot become
one unbounded `WriteBatch` allocation.

### 6. Canonical Value Store

Raw field values are stored once per scope, field, document, and value ordinal.
Strategy postings refer to these values rather than duplicating them in string,
trigram, Soundex, and Metaphone indexes.

The value store must support:

- multiple values per field/document;
- exact recheck;
- typed range recheck;
- fuzzy scoring;
- sort key retrieval;
- aggregation and grouping; and
- page-local/range reads without loading the complete field.

Definitions set explicit limits for canonical bytes per value/document,
multi-value count, expanded token count, and postings per document. Trigram and
phonetic expansion deduplicate identical tokens before admission. Exceeding a
limit records deterministic strict/lenient `unindexable` state or rejects the
write according to policy; values are never silently truncated into false query
semantics.

Virtual metadata may be read directly from FileRecords when that is cheaper, but
the planner needs an explicit cost model rather than an implicit fallback.

### 7. Sparse, Tiled FieldNvt

The FieldNvt is a logical array of optional hints divided into independently
addressable tiles.

```rust
struct FieldNvtCell {
  page_id: PageId,
  anchor_coordinate: NormalizedScalar,
  approximate_live_count: u32,
}
```

```rust
struct FieldNvtTile {
  schema_version: u8,
  index_id: IndexId,
  generation: u64,
  tile_id: u64,
  populated_cells: BitSet,
  cells: Vec<Option<FieldNvtCell>>,
  checksum: u32,
}
```

Lookup behavior:

1. Convert the target value or bound to a normalized coordinate.
2. Compute its logical NVT cell and tile.
3. If the cell is populated, load and validate the referenced page.
4. Follow `previous_page` until composite `(coordinate, canonical value)` bounds
   prove no earlier page can contain the target. Equal-coordinate collision runs
   may span many pages and require backtracking even when the hinted page starts
   at the target coordinate.
5. If the cell is empty or invalid, use the tile occupancy bitmap to find the
   preceding populated cell.
6. If the current tile has no predecessor, use predecessor lookup in the sparse,
   ordered tile directory to jump directly to the preceding populated tile.
7. If no prior hint exists, start at the manifest's first page.
8. Scan pages forward until the exact value is found or page bounds prove it has been passed.
9. Recheck canonical values for correctness.
10. Submit a non-blocking hint correction for useful missing/stale cells.

The tile directory is sparse and supports exact and floor/predecessor lookup. A
large empty scalar region therefore costs a bounded directory traversal rather
than one read per absent tile.

Range behavior:

1. Convert lower and upper query bounds to coordinates.
2. Form a conservative NVT interval.
3. Start from a validated anchor at or before the lower interval.
4. Include boundary pages before/after the computed interval when precision or
   page metadata makes them potentially relevant.
5. Stop only when typed page/value bounds prove the upper bound has been passed.
6. Recheck every accepted posting using converter comparison.

Both point and range termination use composite typed bounds, not scalar bounds
alone. A converter that maps every test value to one coordinate must remain
correct through arbitrary page splits; it merely loses NVT selectivity.

NVT scaling behavior:

- Increasing resolution creates a new sparse generation with more logical cells.
- Existing page links and pages remain valid.
- Old hints may be remapped approximately or omitted.
- Missing cells heal through reads, writes, and background sampling.
- Hint corrections enter a bounded, deduplicating, rate-limited low-priority
  queue; they are dropped under memory/I/O pressure rather than delaying queries.
- Decreasing resolution retains a conservative subset of anchors.
- NVT tiles are cached by bytes under the global memory budget.
- Complete NVT rebuild is optional maintenance, not a query prerequisite.

### 8. Bounded Mutation Overlay

Replace `HashMap<BufferedIndexKey, FieldIndex>` with byte-accounted memtables of
additions, removals, and canonical-value updates.

```rust
struct IndexMutationTransaction {
  visibility: RuntimeVisibilityToken,
  source_version: SourceVersion,
  mutation_id: MutationId,
  scopes: Vec<ScopeMutationBatch>,
}

struct ScopeMutationBatch {
  scope_id: ScopeId,
  documents: Vec<DocumentMutation>,
}

struct DocumentMutation {
  document: DocumentOrdinal,
  file_key: FileKey,
  old_source_record_hash: Option<RecordRevisionHash>,
  new_source_record_hash: Option<RecordRevisionHash>,
  indexes: Vec<IndexMutation>,
}

struct IndexMutation {
  index_id: IndexId,
  operation: Add | Remove | Replace,
  values: Vec<CanonicalValue>,
}
```

Requirements:

- Global hard cap by allocated bytes, not mutation count.
- Per-index soft quotas to prevent one index monopolizing memory.
- A coordinator admits every scope affected by one namespace operation as one
  in-memory transaction. Cross-scope rename/move cannot update only one side.
- The complete transaction is admitted before a successful write response can
  advertise indexed visibility; if capacity prevents admission, every affected
  index is marked for authoritative reconciliation before the transaction may be dropped.
- Queries pin a boot-scoped visibility token. Every selected index must cover
  that token through its manifest plus retained overlays, or be treated as degraded.
- A batch remains retained until every affected active format has published it
  or a degraded marker forces queries onto an authoritative fallback.
- Active and frozen memtables so flushing does not stop incoming writes.
- Query readers merge active/frozen mutations over disk pages.
- Flush groups mutations by target page and rewrites only affected pages.
- A page cache entry can be cleanly evicted after publication.
- Dirty state consists only of bounded mutation records, never complete indexes.
- Shutdown and emergency spill serialize mutation records, not entire indexes.
- Long-running build/migration deltas use bounded, segmented disk-backed
  IndexArtifact journals after the in-memory threshold. Journals are accelerators;
  authoritative HEAD diff remains capable of reconstructing a lost segment.
- After restart, an index whose `source_version` differs from the authoritative
  namespace version is reconciled by Merkle diff before it is advertised as current.
- Whole-HEAD transitions (`snapshot_restore`, fork promotion, promoted backup)
  enqueue a `NamespaceTransition` marker, immediately mark affected indexes
  reconciling, and run a bounded Merkle diff. They never construct one enormous
  in-memory per-file mutation transaction.
- Config, glob, parser/plugin, or converter-definition changes create a shadow
  definition build. They do not mutate an existing definition in place.
- A failed derived-index flush marks the index degraded. A broad storage sync
  failure still enters the database durability-failure policy.

The default budget must be configurable and visible. The system must remain
within the configured budget even when every configured index receives writes.

### 9. Cache and Concurrency Model

Use separate bounded caches for:

- manifests and active pointers;
- scope catalog pages;
- NVT tiles;
- posting pages;
- canonical value pages; and
- active/frozen mutation tables.

All are charged to one global index-memory coordinator. Metrics report logical
payload bytes, allocator-estimated bytes, pinned bytes, dirty bytes, and budget
pressure.

Concurrency rules:

- No global mutex surrounds all indexes.
- Per-index publication locks serialize page splits and manifest swaps.
- Read pages and tiles are immutable `Arc` values.
- Page cache misses coalesce concurrent loads for the same artifact.
- Readers pin one manifest generation for the duration of a scan.
- A generation-lease registry exposes all in-flight reader, migration, compaction,
  and consistent-cursor roots to GC. Compaction retires old artifacts only after
  those leases expire/release.
- Cache admission reserves bytes before loading. Query pinning that would exceed
  the hard budget is backpressured/rejected rather than silently oversubscribing.
- Cancellation and deadlines release pins promptly; temporary sort/bitmap/parser
  memory is charged to the same coordinator or a documented sibling budget.
- Query execution never deep-clones an index through serialization.

## Query Execution Refactor

### 0. Query Context, Scope, and Authorization

Every execution receives a `QueryExecutionContext` describing:

- normalized requested path and effective config-scope partitions;
- unrestricted embedded/system access or the caller's authorized path/rule set;
- namespace `SourceVersion` and runtime visibility token;
- consistency/cursor mode, deadline, cancellation token, and memory budget; and
- whether privileged planner/physical diagnostics may be returned.

Authorization is part of candidate evaluation. Unauthorized documents are
removed before counts, groups, aggregates, ranking, limits, cursors, and
`EXPLAIN ANALYZE` statistics. Normal API keys and user/group grants cannot infer
hidden document counts or field distributions from query metadata. Embedded
callers must opt explicitly into unrestricted context; it is never inferred
from the absence of HTTP middleware.

For SDK compatibility, existing trusted embedded query methods may remain as a
time-bounded deprecated wrapper that constructs an explicitly named
`TrustedEmbedded` context. Server and plugin routes are forbidden from using
that wrapper and must pass caller authorization. The unrestricted behavior and
deprecation are documented rather than silently changing existing local callers.

Each effective scope is planned independently in its own ordinal space. A
mandatory path/access constraint participates in driver selection. Scope-local
results are converted to FileKeys and merged with deterministic global
tie-breaking. Permission generation/cache invalidation remains independent from
derived index generation.

Cross-scope ordered merge, grouping, min/max, and fuzzy top-K require compatible
typed-comparator/collation/scoring fingerprints. The planner groups compatible
definitions and either applies a documented API-level coercion with authoritative
recheck or rejects an incompatible cross-scope operation. It never compares raw
coordinates or uncalibrated scores from semantically different definitions.

### Semantic Contract Before Optimization

Phase 0 must freeze existing intended behavior for missing fields, JSON null,
empty arrays, duplicate multi-values, malformed typed values, NaN/infinity,
`NOT`, `exists`/`missing`, comparison coercion, string normalization, fuzzy
scoring, and multi-value sorting/aggregation. The scope catalog's live universe
and value store must be able to implement that contract. Optimization may not
silently substitute SQL three-valued logic or a new collation.

### 1. Compile Predicates into Scan Constraints

Each indexed leaf becomes one of:

```rust
enum ScanConstraint {
  ScalarPoint { coordinate, canonical_value },
  ScalarPoints(Vec<...>),
  ScalarRange { lower, upper, inclusivity },
  TokenPoints(Vec<...>),
  FullIndexWithRecheck,
}
```

Compilation returns:

- candidate index strategy;
- NVT cells/tiles likely involved;
- conservative boundary expansion;
- estimated pages and postings;
- estimated value-page/recheck cost;
- overlay mutation count; and
- whether the constraint supports ordered streaming.

Compilation enforces request complexity limits: boolean nodes/depth, IN values,
expanded tokens, sort/group fields, aggregate groups, requested result window,
and estimated memory/pin budget. Rejection occurs before page loads or large
temporary allocations.

### 2. Cost-Based Driver Selection

For `AND`, choose the cheapest selective leaf as the driving scan. Cost is based
on estimated page reads, postings, mutation overlay work, and recheck cost.
Scalar interval width is only a fallback estimate.

Other predicates consume candidate document ordinals through one of:

- point membership probes;
- a document-membership bitmap;
- merge intersection of ordered posting streams; or
- final authoritative recheck.

Planner ordering must be observable through `EXPLAIN`.

### 3. Correct Boolean Composition

- Same-field scalar constraints may be combined in that field's coordinate space.
- Different fields never intersect raw NVT bucket numbers.
- Cross-field AND/OR/XOR/DIFFERENCE uses scope document ordinals.
- NOT operates against the scope's live-document membership bitmap or a bounded
  anti-join, not a `HashSet` of every hash from every index.
- Approximate NVT masks identify page regions only; candidate membership masks
  identify documents.

### 4. Streaming Results

- Push `limit`, cursor, and offset into scans when ordering permits.
- Avoid loading every FileRecord before applying a default limit.
- Explicit total counts may require broader work and must report that cost.
- Sorting by an indexed field follows ordered page streams where possible.
- Aggregations process value pages incrementally with bounded state.
- Global search maintains a bounded top-K heap per request and does not request
  `usize::MAX` results from every field and directory.
- Fuzzy queries probe only token pages required by the query and fetch canonical
  values only for candidates that survive posting intersection/union.
- Keyset cursors contain the query/definition fingerprint, complete sort tuple,
  unique FileKey tie-breaker, source version, and consistency mode. They are
  authenticated or strictly validated; callers cannot inject arbitrary planner state.
- Default live cursors document their concurrent-write behavior. A strict
  consistent mode creates a TTL-bounded query lease pinning source and manifest
  roots; otherwise a stale source/definition cursor is rejected explicitly.
- Artifact retention and GC account for unexpired consistent-cursor leases.

### 5. Degraded and Fallback Behavior

Fallback is capability-specific, never a euphemism for partial results:

- virtual metadata predicates may scan FileRecords with bounded streaming;
- content fields may use authoritative parser/plugin re-evaluation only when
  the exact pinned definition dependency is available and resource limits permit;
- otherwise the query returns an explicit index-unavailable/reconciling error;
- per-document parser failures are recorded as versioned `unindexable` state,
  with strict versus lenient policy defined by configuration; and
- no stale manifest, incomplete overlay, or parser failure may be presented as
  a complete negative match.

### 6. QueryStrategy Compatibility

Do not continue exposing no-op behavior.

- `Full`: exact conservative page plan with no sampling.
- `Progressive`: conservative coarse-region planning followed by exact refinement.
- `Auto`: cost-based choice with a correctness-preserving fallback.
- `Strided`: either redefine as conservative region sampling with no false
  negatives or deprecate it. The present skip-and-sample semantics are unsafe.

No strategy may change query results.

## Migration Architecture

### Migration Principles

- Migration is per `(scope, field, strategy)` index.
- v0 and v1 may coexist in one database indefinitely.
- New code reads both formats before any migration begins.
- Existing v0 `.idx` files remain untouched through build, catch-up, validation,
  cutover, and rollback grace.
- v1 is built from authoritative current files, not trusted v0 postings by default.
- A fast v0-copy mode may be considered later but must be explicitly unsafe with
  respect to inherited stale entries and is not the production default.
- Index migration does not rewrite user FileRecords or content chunks.
- Migration checkpoints are immutable IndexArtifacts referenced by the existing
  durable task record; lease/root state is discoverable by GC from that record.
- Cancellation, crash, shutdown, and restart leave either v0 active or a complete
  published v1 generation.

### Binary Compatibility Gate

`IndexArtifact` is a new entry type. A binary that predates it may reject the
database during entry scanning even when every query still uses v0. Therefore,
software compatibility and per-index data cutover are separate gates:

1. Deploy a compatibility release that understands and safely ignores inactive
   v1 artifacts, reads both current and capability-aware file headers, and
   continues to read/write v0.
2. Verify clean start, dirty recovery, backup, GC, verify, repair, and normal
   traffic with v1 creation disabled.
3. Through an explicit operator action under the exclusive database lock,
   perform a crash-resumable A/B transition to a new header version carrying
   `min_reader_capabilities = index_artifact_v1`. Write/sync one v4 slot, then
   replace/sync the remaining v3 slot, and verify both slots.
4. A mixed v3/v4 state is a resumable pre-capability state in which v1 artifact
   creation is forbidden. Only after both slots are v4 does the old-reader fence
   become enforceable rather than advisory.
5. Verify restart on the upgraded header before writing any artifacts and make
   deployment tooling reject binaries lacking `index_artifact_v1` support.
6. Only then permit migration preflight to create a lease or v1 artifact.
7. Cut indexes over independently after their validation succeeds.
8. Finalize and later remove v0 only through explicit operator actions.

After step 4 completes, rollback during migration means using the compatibility release to
switch an index back to v0. It does not mean reinstalling an arbitrary older
binary. A full binary rollback past this boundary requires restoring a pre-v1
backup or a deliberately written artifact-stripping plus header-downgrade
offline migration. Header readers/writers must have real v3/v4 dispatch and
golden fixtures; the new binary may not reinterpret v3 bytes as v4.

### Format Selection

Introduce an index registry that resolves each index to:

```text
v0_active
v1_building_with_v0_active
v1_active_dual_write
v1_active
v1_reconciling
needs_rebuild
degraded
```

Resolution order is explicit. The presence of v1 artifacts alone never causes a
cutover. The stable active-pointer payload is also the per-index registry record,
so format state and active manifest cannot disagree after a crash.

### Stable Source Snapshot

Migration must not rely on timestamps.

1. Register migration and begin recording v1 mutation overlays for the scope.
2. Capture the current durable `SourceVersion` as `V0` with HEAD `H0`.
3. Pin `H0` as a temporary GC root owned by the migration lease.
4. Pin the exact index definition and parser/plugin dependencies.
5. Traverse the immutable directory/FileRecord graph rooted at `H0`.
6. For metadata-only definitions, never read file chunks. Content parsing obeys
   configured memory, file-size, execution-time, and cancellation limits.
7. Build v1 through its normal bounded page writer.

The source walker applies the same canonical internal/system-path exclusions as
normal indexing and never indexes v0 `.aeordb-indexes` files, task records,
logs, configs, or v1 artifacts as user documents.

The migration pin is removed only after cutover or cancellation cleanup. GC must
recognize active migration roots.

Preflight first verifies the authoritative namespace: HEAD traversal, mutable
path-key records, and directory membership must agree. Path-key-only or
HEAD-only files are reported and migration is blocked until repair policy is
chosen; the build must never silently omit disputed live data.

### Live-Write Catch-Up

While the base is built:

- normal writes continue updating v0;
- the v1 migration overlay receives idempotent document/value mutations and
  rolls them into bounded disk-backed delta-journal segments;
- deletes, renames, copies, restores, merges, batch writes, blob commits, and
  embedded SDK writes use the same mutation-intent path; and
- migration progress stores its last deterministic source cursor, build roots,
  boot-scoped visibility cutoff, and latest reconciled `SourceVersion`.

The base walker indexes only pinned `H0`. It never lets an older H0 value
overwrite a newer journaled source revision. Journal segments are checkpointed,
checksummed, byte-bounded, and pruned after application. A missing/corrupt/full
journal records a catch-up gap and falls back to HEAD Merkle diff; it can delay
cutover but cannot permit one without authoritative reconciliation.

Before cutover:

1. Capture a newer durable source version `V1` with HEAD `H1`.
2. Merkle-diff `H0` to `H1` to detect any mutation-intent gaps.
3. Reconcile changed, added, deleted, and renamed paths into v1.
4. Apply overlay mutations through a captured runtime visibility cutoff.
5. Repeat until caught up.
6. Under a short namespace publication barrier, capture `Vfinal`/`Hfinal` and
   the final boot-scoped visibility cutoff. The first implementation uses the
   existing global `namespace_write_guard`; scope-sharded barriers are a later
   optimization only after lock ordering is proven.
7. While the barrier is held, apply the small final Merkle diff and mutation
   overlay through that cutoff.
8. Validate the final delta and visibility invariants, then publish the v1
   active pointer with `source_version = Vfinal` and the applied cutoff.
9. Change the registry state to `v1_active_dual_write` as part of that same
   active-pointer publication, then release the barrier.
10. Subsequent writes dual-write to the already-active v0 and v1 paths.

The publication barrier has a strict time/work budget. If the final delta is too
large, release it without changing the active pointer, process another catch-up
round, and retry. Full page-chain and shadow validation happen before the
barrier; only bounded final-delta validation is permitted while writes are held.

Mutations are idempotent by content-derived MutationId and source FileRecord
revision, so overlap between Merkle diff and the mutation overlay is safe. If
the process restarts, boot-scoped cutoffs are discarded and catch-up resumes by
authoritative source-version diff.

The migration walker must not call the current sync path that materializes two
complete `VersionTree` maps. It recursively compares content-addressed directory
hashes, skips equal subtrees, traverses changed B-tree/directory nodes in stable
order, emits byte-bounded path batches, and persists a resumable traversal stack.

### Validation Before Cutover

Every migrated index must pass:

- artifact checksum and identity verification;
- complete forward and backward page-chain traversal;
- no page-link cycles or unreachable active pages;
- converter fingerprint consistency;
- monotonic typed page bounds for order-preserving strategies;
- posting/value-reference integrity;
- document ordinal resolution;
- tombstone and duplicate resolution;
- manifest counts matching a full page traversal;
- NVT hint validation, with invalid hints discarded rather than fatal;
- source-version and visibility-cutoff accounting;
- bounded-memory validation;
- representative v0/v1 shadow-query comparison; and
- authoritative source evaluation for deterministic samples and every
  mismatch class.

For production rollout, support a configurable shadow-read sample rate. The
server executes selected queries against both formats, compares document IDs,
records mismatches, and returns the currently active format's answer. Because v0
may already be stale, authoritative evaluation decides whether v0, v1, or both
are wrong.

Any mismatch blocks automatic cutover/finalization and identifies the index,
query shape, v0-only documents, and v1-only documents.

### Cutover and Grace Period

Cutover is one active-pointer update per index.

After cutover:

- v1 serves reads;
- writes continue updating both v0 and v1 during a configurable/manual grace period;
- shadow comparison continues;
- v0 remains immediately available for pointer rollback; and
- migration status reports dual-write overhead and grace age.

Because v0 dual-writing retains its existing memory and rewrite costs, migrate
indexes incrementally and keep the grace period deliberate rather than endless.

### Finalization and Old-Binary Safety

Finalization is explicit and irreversible without rebuilding v0:

1. Require zero unresolved validation/shadow mismatches.
2. Require v1 to cover the current source version and runtime visibility cutoff.
3. Stop v0 writes for the index.
4. Rename/mark the v0 index as retired so an older binary fails to find it rather
   than serving a stale index silently.
5. Advance the capability marker to record that active indexes require v1, in
   addition to the earlier `index_artifact_v1` reader requirement.
6. Retain retired v0 artifacts for a configurable cleanup window.
7. Allow GC to reclaim v0 only after that window and an explicit cleanup action.

Deployment tooling must inspect the capability marker and refuse to deploy a
binary that cannot read every active index format. An arbitrary historical
binary cannot be made safe after v0 retirement; rollback must use a compatibility
binary with v1 support.

### Rollback

- Before cutover: discard the unpublished v1 generation and keep v0 active.
- During grace: atomically point reads back to v0 and stop v1 publication while
  retaining v1 artifacts for diagnosis.
- After finalization but before v0 cleanup: rebuild/catch up v0 before switching.
- After v0 cleanup: rollback means running a bounded authoritative v0 rebuild or
  restoring a compatible backup; direct pointer rollback is unavailable.

Rollback never modifies authoritative user content.

### Migration Task and CLI/API Surface

Add a dedicated task instead of overloading `force` reindex semantics:

```json
POST /system/tasks/index-migrate
{
  "path": "/",
  "target_format": "v1",
  "mode": "authoritative",
  "dry_run": false,
  "shadow_read_rate": 0.01,
  "max_memory_bytes": 1073741824,
  "max_concurrency": 2
}
```

Required operations:

- preflight/dry-run;
- start selected indexes;
- pause and resume;
- cancel safely;
- status and per-index progress;
- validate or revalidate;
- cut over manually;
- roll back during grace;
- finalize; and
- clean retired artifacts.

All online migration/finalization/cleanup operations require root-level
administrative authority, reject share/scoped keys, use idempotency/task IDs,
and acquire maintenance leases that conflict explicitly with GC, restore,
capability downgrade, and overlapping definition migration. Status/progress may
stream through the existing task/SSE machinery without exposing indexed values
or unauthorized paths.

Offline CLI equivalents:

```text
aeordb index status <database>
aeordb index migrate <database> --path / --target-format v1 --dry-run
aeordb index validate <database> --format v1
aeordb index finalize <database> --path / --yes
aeordb index cleanup <database> --retired-before <timestamp> --yes
```

The online API is required for live production databases. The offline CLI must
respect the existing exclusive database lock.

### Migration Preflight

Dry-run reports:

- indexes by format, strategy, estimated postings, and estimated source files;
- exact definition fingerprints and pinned parser/plugin availability;
- authoritative HEAD/path-key/directory consistency;
- estimated temporary/final disk use, KV-entry growth, and reclaimable orphan bytes;
- current free space, mandatory safety reserve, and worst-case cancellation cleanup space;
- configured and expected peak memory;
- v0 indexes with known stale/degraded status;
- current active tasks and conflicting maintenance;
- required minimum reader capability before creating the first artifact;
- dirty-startup/recovery compatibility and latest verified backup; and
- an estimated migration order.

Default order should prioritize exact/small indexes before large trigram and
phonetic indexes, limiting concurrent migrations by bytes rather than count.

## GC, Backup, Verify, and Repair

### GC

- Treat both valid A/B pointer slots, their active/previous manifests, migration
  leases, and grace-period v0/v1 roots as derived GC roots.
- Include in-flight query/consistent-cursor/compaction generation leases captured
  at the GC mark boundary; do not sweep an artifact merely because a newer
  manifest became active during the cycle.
- Traverse manifest-to-page/tile/value/catalog references.
- Treat NVT manifests/tiles as a separate disposable root family; loss never
  degrades posting correctness.
- Reclaim orphaned builds, superseded immutable revisions, expired NVT
  generations, and canceled migration artifacts only when no lease references them.
- Bound artifact marking memory through partitioned/streamed mark state rather
  than adding every artifact hash to the existing monolithic live `HashSet`.
- Trigger derived cleanup by orphan bytes/KV-key count and free-space pressure,
  not only wall-clock age. Migration pauses before breaching its disk reserve.
- Add a crash-safe reuse or compaction policy for reclaimed artifact-sized voids.
  Current chunk-only void reuse is insufficient for sustained immutable-index churn.
- Any random void reuse must preserve old leased generations and obey a
  write/sync-before-pointer durability order; otherwise append and compact later.
- Never infer user-content liveness from index artifacts.

### Backup and Restore

- Distinguish physical database copies from logical export/patch/peer sync.
  Physical copies retain artifacts and capabilities; logical transfer includes
  index definitions but omits derived artifacts by default.
- A logical restore that omits pages rewrites registry state to `needs_rebuild`;
  it must not copy an active pointer that references absent artifacts.
- An optional `include_indexes` mode includes a closed, validated set of active
  manifests, directories, pages, catalogs, NVT hints, and capability metadata.
- `include_indexes` is root-only because canonical value/catalog pages contain
  paths and field values independent of ordinary export path filtering.
- `include_indexes` pins a generation and exports it only when its source version
  matches the exported namespace and no omitted overlay is required; otherwise
  it omits that index and records `needs_rebuild` in the export manifest.
- Restore validates capability before activating included indexes.
- A restore without pages marks indexes `needs_rebuild`; it does not silently
  advertise them as ready.
- Snapshot restore, fork promotion, and promoted import emit a namespace
  transition and reconcile indexes from old HEAD to new HEAD.
- Patch/peer synchronization never assumes source ordinals/artifact hashes are
  valid locally; targets rebuild or maintain their own derived indexes unless a
  future protocol explicitly negotiates identical definitions and format support.

### Startup and Background Scheduling

- Startup reads capability, registry, active pointers, and shallow manifest roots
  only; it never eagerly materializes posting/value/NVT pages.
- Invalid active roots select a complete previous generation only when overlays
  bridge freshness; otherwise mark `needs_rebuild`/`degraded` and start normally
  with explicit query fallback/error behavior.
- Pending migrations resume after the server becomes ready, from durable
  checkpoints and authoritative source diff. They do not block health readiness.
- Migration, reconciliation, compaction, derived GC, backup, and reindex share an
  I/O/memory task scheduler with foreground-latency feedback, cancellation, and
  HDD-friendly concurrency defaults.
- Graceful shutdown freezes admission, checkpoints tasks, spills bounded overlays,
  releases pins, and reports any index state that will require reconciliation.

### Verify and Repair

Add index-specific modes:

```text
aeordb verify <db> --indexes
aeordb repair <db> --indexes --rebuild-hints
aeordb repair <db> --indexes --rebuild-pages --yes
```

Verification distinguishes:

- authoritative database corruption;
- derived posting/value page corruption;
- stale or missing NVT hints; and
- migration/checkpoint inconsistencies.

Repairing NVT hints must never require rewriting authoritative files. Corrupt
posting pages trigger index degradation and authoritative reindex of the affected
field/strategy, not whole-database failure.

If both active-pointer slots are invalid, repair may enumerate immutable manifest
candidates and produce a ranked recovery report, but it activates none until all
referenced roots validate and authoritative source-version checks pass. Ambiguous
candidates require explicit operator choice or authoritative rebuild.

## Observability

Expose per-index and aggregate metrics:

- format and active generation;
- migration state, durable source version, and runtime overlay visibility;
- page, posting, live-document, tombstone, and value counts;
- NVT resolution, populated cells, tile reads, backward-anchor distance, and hint hit rate;
- pages scanned per point/range query;
- boundary pages and typed rechecks;
- page-cache/NVT-tile/value-cache hits, misses, bytes, and pins;
- active/frozen memtable bytes and oldest mutation age;
- page splits, compactions, bytes written, and write amplification;
- planner estimates versus actual pages/postings;
- shadow-query comparisons and mismatches;
- degraded indexes and fallback scans;
- v0 dual-write time and cost;
- artifact live/orphan/retired bytes, KV-key count, reclamation lag, and disk reserve;
- per-task I/O throughput/throttle time and foreground-latency backoff;
- parser failures/unindexable document counts by definition without exposing values; and
- migration/reconciliation phase, documents/bytes/pages completed and total,
  checkpoint age, throughput, and ETA with an explicit unknown state when the
  remaining work cannot be estimated reliably.

Dashboard diagnostics should answer:

- Which indexes own memory?
- Which indexes are dirty, pinned, migrating, degraded, or over budget?
- How effective are their NVT hints?
- Which queries scan more pages than planned?
- Which indexes are still v0 and why?

`EXPLAIN ANALYZE` should include the selected driver, scalar constraints, NVT
anchor, estimated/actual pages, membership operation, recheck count, cache hits,
and fallback reason.

Physical paths, hidden document counts, field distributions, and index values
are privileged diagnostics. Non-root query `EXPLAIN` is authorization-filtered
and redacted before returning any cardinality or planner detail.

## Implementation Phases

### Phase 0: Freeze Semantics and Establish Baselines

- [ ] Record this plan's invariants as architecture tests.
- [ ] Capture v0 compatibility fixtures for every converter and strategy.
- [ ] Add production-shaped benchmarks for exact, range, fuzzy, boolean, sort,
      aggregate, global search, sustained writes, and reindex.
- [ ] Record RSS, peak RSS, allocation count, page reads, bytes written, and latency.
- [ ] Add failure fixtures for missing NVT, corrupt NVT, corrupt index file, and stale postings.
- [ ] Freeze missing/null/multi-value/NOT/coercion/collation/fuzzy semantics.
- [ ] Characterize authorization-before-pagination/count/aggregate behavior and cursor consistency.
- [ ] Characterize ordered-batch, HEAD `SourceVersion`, dirty-startup, GC, and void-reuse behavior.
- [ ] Confirm no phase changes KV bytes or behavior.

Exit: reproducible baseline and characterization suite.

### Phase 1: Decouple KV NVT from Field NVT

- [ ] Freeze/extract the byte-identical legacy NVT codec used by KV and v0 indexes.
- [ ] Wrap the legacy core separately as `KvNvt` and `FieldIndexV0Nvt` without format changes.
- [ ] Add `FieldNvt` types in a separate module.
- [ ] Ensure only v1 field-index code moves off the legacy/KV NVT API.
- [ ] Add compile-time/module boundaries preventing accidental cross-use.
- [ ] Verify existing KV fixtures byte-for-byte.

Exit: field-index work can proceed without changing KV behavior.

### Phase 2: Converter v2, Definition Identity, and Scope Catalog

- [ ] Implement fixed-point coordinates, canonicalization, typed comparison, and fingerprints.
- [ ] Add typed/versioned configuration bounds without reinterpreting v0 serialized state.
- [ ] Implement complete definition fingerprints including parser/plugin/tokenizer dependencies.
- [ ] Add signed, floating, timestamp, collision, malformed-value, and multi-value property tests.
- [ ] Implement the storage-independent model/codecs for non-reused scope
      ordinals and bidirectional paged ordinal/FileKey catalogs; persistence lands in Phase 3.
- [ ] Model disjoint effective scopes, config/glob overrides, and definition replacement.
- [ ] Add document membership bitmap abstraction.

Exit: stable typed semantics and shared cross-field identity.

### Phase 3: IndexArtifact and v1 Page Format

- [ ] Add explicit v3/v4 file-header readers/writers and minimum-reader capability bits.
- [ ] Add versioned `IndexArtifact` entry and KV tag.
- [ ] Implement fixed-width hashed KV keys and canonical bounded cross-platform codecs.
- [ ] Update scanner for lazy artifact payload verification and update verify, counters, and diagnostics.
- [ ] Implement posting/NVT manifests, active pointers, immutable radix directories,
      posting/value/scope pages, and generation leases.
- [ ] Implement checksums, identity validation, prefix-safe publication ordering,
      previous-generation fallback, and explicit soft/hard durability behavior.
- [ ] Add an artifact reclamation skeleton before any v1 write path can be enabled.
- [ ] Add fault injection at every write/publish boundary.

Exit: crash-safe, directly addressable derived artifacts exist independently of v0.

### Phase 4: Sparse FieldNvt and Page Scanner

- [ ] Implement deterministic tile addressing and bounded tile cache.
- [ ] Implement empty-cell backward search and page-link backtracking.
- [ ] Implement conservative point and range scans with actual typed recheck.
- [ ] Implement non-blocking hint healing.
- [ ] Publish NVT hints through their independent best-effort manifest/pointer.
- [ ] Implement online resolution changes without moving pages.
- [ ] Test complete NVT loss, random gaps, stale/wrong hints, resize, and tile corruption.

Exit: v1 point/range reads are correct with no NVT and faster as hints heal.

### Phase 5: Bounded v1 Mutation Path

- [ ] Implement byte-bounded active/frozen memtables.
- [ ] Route all indexing entry points through one mutation-intent API.
- [ ] Cover file store, streaming finalize, blob batch commit, embedded batch write,
      merge, copy, rename, restore, delete, and reindex.
- [ ] Implement multi-scope mutation transactions and content-derived idempotency IDs.
- [ ] Implement whole-HEAD transition markers and bounded source-version reconciliation.
- [ ] Implement definition-change shadow builds and per-document unindexable state.
- [ ] Implement page-local inserts, replacements, tombstones, and splits.
- [ ] Implement overlay-visible reads and emergency spill.
- [ ] Enforce hard global memory limits under many-index write stress.

Exit: sustained writes do not load or dirty complete indexes.

### Phase 6: v1 Query Leaves and Strategy Selection

- [ ] Implement exact, IN, range, trigram, and phonetic page probes.
- [ ] Require `QueryExecutionContext` and apply authorization/path constraints before observables.
- [ ] Read only metadata when selecting an index strategy.
- [ ] Recheck canonical values lazily.
- [ ] Implement the virtual-metadata/content-parser fallback capability matrix.
- [ ] Implement cost estimates and `EXPLAIN` details.
- [ ] Add limit-aware streaming for simple queries.

Exit: single-field v1 queries are bounded and match authoritative results.

### Phase 7: Boolean Planner and Membership Composition

- [ ] Implement cost-based AND driver selection.
- [ ] Implement scope membership bitmap AND/OR/NOT/XOR/DIFFERENCE.
- [ ] Plan disjoint effective scopes independently and merge by FileKey.
- [ ] Restrict scalar-region composition to compatible same-field coordinate spaces.
- [ ] Replace unsafe/no-op strided and progressive execution.
- [ ] Add estimate-versus-actual planner feedback.

Exit: complex queries are bounded, correct, and strategy options have real behavior.

### Phase 8: Sorting, Pagination, Aggregation, and Global Search

- [ ] Push limits/cursors into ordered scans.
- [ ] Implement validated keyset cursors plus TTL-bounded consistent query leases.
- [ ] Stream indexed sorts and bounded top-K sorts.
- [ ] Stream aggregate/group calculations from value pages.
- [ ] Replace global `usize::MAX` fan-out with bounded top-K merging.
- [ ] Add backpressure and cancellation to long query scans.

Exit: secondary query surfaces no longer require whole indexes or all results in memory.

### Phase 9: Mixed-Format Registry and Migration

- [ ] Implement v0/v1 format registry and per-index resolution.
- [ ] Add capability gate before first artifact, migration lease, SourceVersion/HEAD
      pinning, dependency pinning, source walker, checkpoints, and resume.
- [ ] Add dual-write v1 overlay and Merkle-diff catch-up.
- [ ] Implement the bounded hash-pruning Merkle diff; do not reuse full-tree sync diff.
- [ ] Add authoritative validation, diagnostic v0/v1 shadow reads, cutover,
      grace rollback, and explicit finalization.
- [ ] Add dry-run, API, CLI, progress, cancellation, and disk/memory controls.
- [ ] Refuse migration on authoritative namespace inconsistency or inadequate disk reserve.
- [ ] Test interruption at every migration state.

Exit: copied v0 production databases migrate online with concurrent writes and safe rollback.

### Phase 10: Rollout, Documentation, and v0 Retirement

- [ ] Document v1 architecture, operations, migration, rollback, repair, and metrics.
- [ ] Update API, SDK, CLI, admin, query, indexing, backup, and deployment docs.
- [ ] Run complete unit/integration/property/fault suites.
- [ ] Run a real server against a `/tmp` database with mixed files and live HTTP queries.
- [ ] Run crash/restart soak with migration, writes, deletes, and queries concurrently.
- [ ] Verify lazy startup, bounded derived GC, canceled-build cleanup, backup/restore,
      snapshot/fork transitions, and peer/patch behavior.
- [ ] Verify root-only migration controls and authorization-safe query/EXPLAIN behavior.
- [ ] Migrate a copy of the FS-Server1 database and compare memory/performance/results.
- [ ] Deploy compatibility binary with v1 disabled by default.
- [ ] Enable one canary index, validate, then expand incrementally.
- [ ] Finalize v0 only after an explicit production review.

Exit: v1 is the default for new indexes; existing production indexes have an operator-controlled migration path.

## Required Test Matrix

### Correctness and Property Tests

- Every accepted converter value produces a deterministic fixed-point coordinate;
  equivalent inputs match across Linux, macOS, and Windows.
- Typed comparator is reflexive, antisymmetric, and transitive.
- Order-preserving converters preserve typed ordering despite scalar collisions.
- All-values-same-coordinate fixtures remain correct across multi-page collision
  runs, splits, point lookups, and range boundaries.
- Point/range results match an authoritative scan for randomized data.
- Multi-value documents match every indexed value.
- Missing or corrupt NVT cells never cause false negatives.
- Random hint corruption either heals or triggers a conservative fallback.
- Resolution growth/shrink does not change results.
- Random page splits, deletes, replacements, and compactions preserve results.
- Cross-field bitmap results match set-based reference evaluation.
- Cross-scope query results match a FileKey-based reference evaluator.
- Incompatible cross-scope converter/collation/scoring definitions never enter
  an unordered sort/group/top-K merge.
- Missing/null/NOT/multi-value semantics match the frozen v0/reference contract.
- Unauthorized documents never affect counts, groups, scores, cursors, or explain metrics.
- Golden artifact bytes and malformed-input rejection match on every platform.

### Migration Tests

- Open untouched v0 database with compatibility binary.
- Mixed v0/v1 fields in one scope.
- Mixed v0/v1 strategies on one field.
- Migrate while creating, overwriting, deleting, renaming, copying, restoring,
  merging, and blob-committing files.
- Migrate across child config/glob overrides and cross-scope moves.
- Change config/parser/plugin definitions during build and verify safe pause/restart.
- Restore snapshot/promote fork during build and verify bounded reconciliation.
- Crash/restart during base scan, page flush, manifest write, pointer publish,
  catch-up, validation, cutover, grace, finalization, and cleanup.
- Corrupt/torn newest A/B slot, then both slots, and verify deterministic fallback/degradation.
- Pause/resume and cancel/restart at every state.
- Parser unavailable, plugin version changed, disk full, memory pressure, and shutdown.
- Corrupt v1 page before and after cutover.
- Roll back during grace and confirm post-cutover writes exist in v0.
- Refuse unsafe old-binary deployment after finalization.
- Verify old binaries refuse both clean and dirty starts immediately after the
  explicit header capability upgrade, before and after artifacts exist.
- Restore backup with and without derived indexes.
- GC while migration lease and old manifest readers are active.
- Expire/revoke consistent-cursor leases and reclaim only after their roots release.

### Resource and Performance Tests

- Peak index-owned memory remains below configured budget plus documented query pins.
- Adding inactive indexes does not increase resident postings memory.
- Sustained writes to many indexes do not make memory scale with total index corpus.
- One-file updates do not read or write complete indexes.
- Point queries read a bounded number of tiles/pages after warm-up.
- Range scans read pages proportional to matched region plus boundaries.
- Migration memory remains bounded from small fixtures through production copies.
- Write and query latency are measured under compaction and migration load.
- Dirty-startup work is proportional to registry/hot-tail reconstruction, not
  total IndexArtifact payload bytes.
- Repeated updates plus cleanup keep artifact WAL bytes and KV-key count within
  documented amplification bounds.
- Disk-reserve pressure pauses migration before ENOSPC and cancellation cleanup
  remains possible within the reserve.
- Oversized content files and parser/plugin failures cannot exceed configured
  memory/time budgets or stall shutdown indefinitely.
- Background migration/reconciliation/GC throttles under foreground latency and
  resumes without losing checkpoints.

### Real-World Validation

For every significant phase:

1. Build the CLI/server with constrained Cargo parallelism.
2. Start a database under `/tmp`.
3. Upload JSON, text, binary, multi-valued, numeric, timestamp, and fuzzy-search files.
4. Exercise writes, updates, deletes, queries, pagination, sorting, and restart.
5. Run `verify` before and after forced crash recovery.
6. Record memory, page reads, bytes written, and query plans.

Before production migration, repeat against a copy of the live database. Never
develop, benchmark, or test migration by opening the production database directly.

## Documentation Deliverables

- `docs/src/concepts/indexing.md`: authoritative NVT/page/value/catalog architecture.
- `docs/src/concepts/storage-engine.md`: `IndexArtifact` and derived-data durability.
- `docs/src/api/querying.md`: planner, scope partitioning, authorization,
  cursor consistency, fallback, strategy, limits, and `EXPLAIN` behavior.
- `docs/src/api/admin.md`: migration/status/validation/finalization endpoints and metrics.
- `docs/src/operations/reindex.md`: distinction between reindex and format migration.
- New `docs/src/operations/index-migration.md`: preflight, rollout, rollback, and recovery.
- `docs/src/SUMMARY.md`: link the new operations page.
- Backup/restore/peer-sync docs: physical versus logical artifact behavior.
- CLI help and examples for all migration commands.
- `docs/src/SKILL.md` and served/generated `docs/SKILL.md`: bot-oriented safe
  migration and diagnostic workflow.
- Release notes with old-binary compatibility boundary.

## Decisions to Confirm Before Phase 3

1. **Artifact representation:** approve dedicated `IndexArtifact` entries rather
   than v1 pages represented as internal FileRecords. Recommendation: approve.
2. **Document ordinal width:** use `u64` logically; choose compressed bitmap
   implementation after benchmark. Recommendation: `u64` API with segmented
   32-bit bitmap containers.
3. **Initial page target:** benchmark 16, 64, and 256 KiB. Recommendation: begin
   with 64 KiB and encode limits in manifest, not schema constants.
4. **NVT tile size:** benchmark 256, 1,024, and 4,096 cells per artifact.
5. **Freshness policy:** decide whether an index mutation-intent failure rejects
   the user write or acknowledges it while marking the index degraded. Recommendation:
   preserve authoritative write success unless the failure indicates a broader
   durability failure, then force authoritative query fallback.
6. **Grace finalization:** manual only versus policy-assisted. Recommendation:
   manual finalization for the first production migrations.
7. **Backup policy:** omit derived pages by default versus include active pages.
   Recommendation: omit by default after restore/reindex tooling is proven.
8. **v0 cleanup delay:** choose minimum retention after finalization. Recommendation:
   retain until at least one verified backup and one clean restart on v1.
9. **Derived publication durability:** choose sync thresholds and whether an
   operator may request hard-durable index generations. Recommendation: batch
   soft-durable publications aggressively, always validate on recovery, and use
   explicit barriers for migration cutover/finalization rather than per mutation.
10. **Cursor consistency:** default live keyset behavior versus strict leases.
    Recommendation: default documented live behavior; offer explicit consistent
    leases with a bounded TTL and return a stale-cursor error after expiry.
11. **Artifact reclamation:** random void reuse versus append plus compaction.
    Recommendation: implement generation cleanup first; enable artifact void
    reuse only after power-cut tests prove write/sync-before-pointer ordering.
12. **Page compression:** none versus page-local compression by artifact kind.
    Recommendation: benchmark current zstd and uncompressed pages on HDD/SSD;
    persist the choice per manifest and never span compression across pages.
13. **Parser failure policy:** strict index degradation versus versioned
    per-document exclusion. Recommendation: explicit config with strict default
    for identity/range fields and observable lenient mode for search corpora.
14. **Consistent-query lease capacity:** maximum TTL, count, and pinned bytes.
    Recommendation: enforce all three and reject admission before exceeding the
    global index memory/artifact-retention budgets.
15. **File-header capability format:** approve a new explicitly dispatched header
    version with minimum-reader bits before introducing IndexArtifact. Recommendation:
    approve; a system-record-only marker cannot safely fence old clean-start binaries.

## Definition of Done

The refactor is complete only when:

- the KV remains compatible and unchanged;
- no field-index operation requires loading, cloning, sorting, or serializing a complete index;
- memory remains within a configured hard budget under many-index sustained writes;
- dirty startup remains lazy with respect to total derived payload bytes;
- immutable artifact churn has bounded WAL/KV amplification and a tested reclamation path;
- NVT deletion and random gaps preserve query correctness;
- point, range, fuzzy, boolean, sort, aggregate, pagination, and global-search
  results match authoritative reference execution;
- query planning uses conservative scalar regions and document-identity composition;
- path/config scope and authorization are applied before every observable query result;
- config/plugin and whole-HEAD transitions reconcile without partial index visibility;
- migration from v0 is online, resumable, observable, validated, and reversible during grace;
- copied production databases pass migration, crash/restart, verify, and result comparison;
- operational and bot documentation is complete; and
- v0 retirement requires an explicit operator action.
