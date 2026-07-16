# NVT Field-Index Refactor and Production Migration Plan

**Date:** 2026-07-16
**Status:** Draft for architectural review
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

## Scope Boundary: Keep the KV Stable

The current `DiskKVStore` continues using its existing fixed bucket pages and
current `NormalizedVectorTable` behavior. This project must not change:

- KV block format or version;
- KV stage sizes;
- fixed KV bucket-page placement;
- KV lookup, flush, expansion, or recovery semantics; or
- KV startup and snapshot behavior.

The shared `NormalizedVectorTable` abstraction is currently a coupling hazard.
The first refactor separates it into:

- `KvNvt`: the current implementation, preserved for the KV without semantic changes; and
- `FieldNvt`: the new sparse, tiled, hint-oriented implementation for field indexes.

The future KV concern about logical page continuation should be recorded
separately. It is not a prerequisite for this field-index refactor because the
current KV maps normalized coordinates directly to fixed physical bucket pages.

## Source Ownership and Refactor Boundaries

The current implementation concentrates storage, caching, mutation, v0
serialization, and query behavior in a few large modules. Preserve public
facades during migration, but move new behavior behind explicit boundaries:

| Current area | Refactoring direction |
| --- | --- |
| `engine/nvt.rs`, `nvt_ops.rs` | Preserve bytes/semantics as `kv_nvt`; build `field_index/nvt.rs` independently. |
| `engine/index_store.rs` | Retain the v0 codec/reader in `field_index/v0`; split v1 manifest, artifacts, pages, cache, memtable, and registry into focused modules. |
| `engine/scalar_converter.rs` | Keep the v0 trait/codec; add converter v2 and adapters without reinterpreting serialized v0 converter state. |
| `engine/indexing_pipeline.rs` | Produce one typed `IndexMutationIntent`; format-specific writers consume it. Parsing/extraction must not write indexes directly. |
| `engine/query_engine.rs` | Split predicate compilation, costing, page scans, document-set composition, sorting, aggregation, and execution into planner/executor modules. |
| `engine/entry_type.rs`, `entry_header.rs`, `entry_scanner.rs`, `storage_engine.rs` | Add the versioned `IndexArtifact` entry and direct append/read primitives. |
| `engine/gc.rs`, `backup.rs`, `verify.rs`, repair modules | Teach maintenance code artifact reachability and derived-data repair policy before v1 can be activated. |
| server task/routes and CLI | Add migration control, validation, rollback, finalization, and diagnostics without overloading ordinary reindex. |

Compatibility rules:

- Existing public query, SDK, and HTTP request schemas remain accepted.
- `IndexManager` becomes a registry-backed facade; callers do not choose v0/v1.
- `IndexingPipeline` emits errors and mutation intents through one result path.
  Logging may supplement an error but must never replace or suppress it.
- New modules may read v0 state, but v0 modules must not depend on v1 modules.
- No transitional adapter may deserialize and clone a whole v0 index merely to
  satisfy a v1 page-level interface.

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
  documents: PageDirectory<DocumentOrdinal, FileKey>,
  active_indexes: Vec<IndexId>,
}
```

Properties:

- A `FileKey` is the stable path-derived file identity already used by indexes.
- A `DocumentOrdinal` is allocated within a scope and shared across its fields.
- New documents receive monotonically increasing ordinals.
- Deleted ordinals are tombstoned and are never reused within that `ScopeId`.
- Routine compaction must not renumber ordinals. Renumbering would require an
  explicit whole-scope migration that republishes every dependent index as one
  coordinated operation, and is outside this refactor.
- Postings store ordinals instead of cloning 32- or 64-byte hashes repeatedly.
- Cross-field candidate sets use document ordinals, enabling Roaring-style
  bitmaps now and GPU-compatible membership textures later.
- Query results resolve ordinals back to FileKeys through the scope catalog.

This creates two deliberately separate bitmap dimensions:

- field-local scalar/NVT regions for page routing and cost estimation; and
- scope-wide document membership for correct cross-field intersections.

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

- `NormalizedScalar` preserves the ratio semantics of `[0.0, 1.0]`.
- Page-local ordering is `(coordinate, canonical typed value, document ordinal)`.
- Equality never relies solely on floating-point equality.
- Signed integers use signed numeric comparison, not raw-byte comparison.
- Floating-point values use a documented total ordering and explicit NaN policy.
- Timestamp parsing failures return errors rather than silently becoming epoch zero.
- Multi-valued fields retain every canonical source value, not only the last one.
- Numeric configuration bounds remain typed; `u64` and `i64` limits are not routed through `f64` configuration fields.
- Converter state is immutable within a mapping generation.

For order-preserving converters, mapping-bound changes preserve logical typed
order but invalidate hint precision. They trigger a new FieldNvt generation,
not a rewrite of every posting page.

### 3. Index Artifact Storage

Recommendation: introduce one new derived entry type, `IndexArtifact`, with one
available KV type tag. Its payload identifies the artifact subtype.

```rust
enum IndexArtifactKind {
  ActivePointer,
  Manifest,
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

All artifacts except active pointers are immutable and content-addressed. A page
rewrite or NVT-tile correction creates a new artifact; it never changes the
artifact visible through an older manifest. The active pointer is the sole
stable, mutable key in the publication protocol.

Artifact key domains must be distinct:

```text
index-active:{index_id}
index-manifest:{index_id}:{generation}:{artifact_hash}
index-directory:{owner_id}:{generation}:{artifact_hash}
index-page:{index_id}:{generation}:{page_id}:{artifact_hash}
index-values:{scope_id}:{generation}:{page_id}:{artifact_hash}
index-nvt:{index_id}:{generation}:{tile_id}:{artifact_hash}
index-scope:{scope_id}:{generation}:{page_id}:{artifact_hash}
index-migration:{task_id}:{artifact_kind}
```

All payloads carry magic, schema version, index/scope identity, generation,
length, and checksum. A key/payload identity mismatch is corruption.

Artifact directories should be immutable, fixed-fanout radix nodes keyed by
logical page/tile ID, not another mutable B-tree. Each node contains a populated
child bitmap, child artifact hashes, level/prefix metadata, and a checksum. This
provides bounded-depth exact lookup, copy-on-write updates, and predecessor
navigation through occupancy bitmaps without depending on physical KV order.
Corrupt directory nodes degrade only the derived index and can be rebuilt.

### 4. Manifest and Atomic Publication

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
  nvt_directory_root: ArtifactHash,
  value_store_manifest: ArtifactHash,
  scope_catalog_manifest: ArtifactHash,
  first_page_id: Option<PageId>,
  last_page_id: Option<PageId>,
  page_count: u64,
  posting_count: u64,
  document_count: u64,
  nvt_resolution: u64,
  nvt_tile_cells: u32,
  source_head: Vec<u8>,
  applied_mutation_sequence: u64,
  previous_manifest: Option<Vec<u8>>,
  logical_checksum: Vec<u8>,
}
```

Publication order:

1. Write immutable new page/value/tile artifacts.
2. Copy-on-write the affected artifact-directory paths so the new directory
   roots resolve stable logical IDs to exact immutable artifact hashes.
3. Sync derived artifacts and directory nodes when a durable publish is required.
4. Write a content-addressed manifest containing the exact directory roots.
5. Validate that every referenced artifact is readable and checksummed.
6. Atomically update the stable active pointer to the manifest hash.
7. Keep the previous manifest reachable for reader retry and rollback.

If a crash occurs before step 6, new artifacts are unreachable and disposable.
If an active manifest or referenced page is unreadable, the reader may inspect
the previous manifest for recovery. It may serve from that manifest only when a
complete retained mutation overlay bridges it to the query's visibility epoch.
Otherwise it marks the index degraded and uses an authoritative scan or returns
an explicit error. It must never return stale/partial index results as complete.

Readers pin a manifest and resolve every logical page/tile ID through that
manifest's immutable directory roots. This prevents an old reader from seeing a
new page revision during a split. Stable mutable page keys are forbidden because
they would break manifest-level atomic visibility.

### 5. Posting Pages

Pages have a target payload size, not a fixed physical offset.

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
  value_ordinal: u32,
  flags: PostingFlags,
}
```

Page rules:

- Pages are logically ordered through stable page IDs and next/previous links.
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
- Readers deduplicate by `(document, value_ordinal)` across page revisions and overlays.

Initial target sizes and split thresholds must be benchmarked. Start testing at
64 KiB target pages with split near 90% and compaction below 30%, but do not
hard-code these values into the format.

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
4. If the page begins after the target, follow `previous_page` until it is a safe anchor.
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

NVT scaling behavior:

- Increasing resolution creates a new sparse generation with more logical cells.
- Existing page links and pages remain valid.
- Old hints may be remapped approximately or omitted.
- Missing cells heal through reads, writes, and background sampling.
- Decreasing resolution retains a conservative subset of anchors.
- NVT tiles are cached by bytes under the global memory budget.
- Complete NVT rebuild is optional maintenance, not a query prerequisite.

### 8. Bounded Mutation Overlay

Replace `HashMap<BufferedIndexKey, FieldIndex>` with byte-accounted memtables of
additions, removals, and canonical-value updates.

```rust
struct IndexMutationBatch {
  sequence: u64,
  scope_id: ScopeId,
  document: DocumentOrdinal,
  authoritative_head: Vec<u8>,
  mutations: Vec<IndexMutation>,
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
- One scope coordinator atomically admits a complete mutation batch before a
  successful write response can advertise indexed visibility.
- Queries pin a scope visibility sequence. Every selected index must cover that
  sequence through its manifest plus retained overlays, or be treated as degraded.
- A batch remains retained until every affected active format has published it
  or a degraded marker forces queries onto an authoritative fallback.
- Active and frozen memtables so flushing does not stop incoming writes.
- Query readers merge active/frozen mutations over disk pages.
- Flush groups mutations by target page and rewrites only affected pages.
- A page cache entry can be cleanly evicted after publication.
- Dirty state consists only of bounded mutation records, never complete indexes.
- Shutdown and emergency spill serialize mutation records, not entire indexes.
- After restart, an index whose `source_head` trails the authoritative namespace
  HEAD is reconciled by Merkle diff before it is advertised as current.
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
- Compaction publishes a new generation and retires old artifacts only after
  readers release their pins.
- Query execution never deep-clones an index through serialization.

## Query Execution Refactor

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

### 5. QueryStrategy Compatibility

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
- Migration progress and checkpoints are durable IndexArtifacts.
- Cancellation, crash, shutdown, and restart leave either v0 active or a complete
  published v1 generation.

### Binary Compatibility Gate

`IndexArtifact` is a new entry type. A binary that predates it may reject the
database during entry scanning even when every query still uses v0. Therefore,
software compatibility and per-index data cutover are separate gates:

1. Deploy a compatibility release that understands and safely ignores inactive
   v1 artifacts while continuing to read/write v0.
2. Verify clean start, dirty recovery, backup, GC, verify, repair, and normal
   traffic with v1 creation disabled.
3. Before the first v1 artifact append, persist a minimum-reader capability and
   make deployment tooling reject binaries lacking `index_artifact_v1` support.
4. Only then permit migration preflight to create a lease or v1 artifact.
5. Cut indexes over independently after their validation succeeds.
6. Finalize and later remove v0 only through explicit operator actions.

After step 3, rollback during migration means using the compatibility release to
switch an index back to v0. It does not mean reinstalling an arbitrary older
binary. A full binary rollback past this boundary requires restoring a pre-v1
backup or a deliberately written artifact-stripping offline migration.

### Format Selection

Introduce an index registry that resolves each index to:

```text
v0_active
v1_building_with_v0_active
v1_active_dual_write
v1_active
degraded
```

Resolution order is explicit. The presence of v1 artifacts alone never causes a
cutover. The stable active-pointer payload is also the per-index registry record,
so format state and active manifest cannot disagree after a crash.

### Stable Source Snapshot

Migration must not rely on timestamps.

1. Register migration and begin recording v1 mutation overlays for the scope.
2. Capture the current namespace HEAD hash as `H0`.
3. Pin `H0` as a temporary GC root owned by the migration lease.
4. Traverse the immutable directory/FileRecord graph rooted at `H0`.
5. Build v1 through its normal bounded page writer.

The migration pin is removed only after cutover or cancellation cleanup. GC must
recognize active migration roots.

### Live-Write Catch-Up

While the base is built:

- normal writes continue updating v0;
- the v1 migration overlay receives idempotent document/value mutations;
- deletes, renames, copies, restores, merges, batch writes, blob commits, and
  embedded SDK writes use the same mutation-intent path; and
- migration progress stores its last source cursor and applied mutation sequence.

Before cutover:

1. Capture a newer HEAD `H1`.
2. Merkle-diff `H0` to `H1` to detect any mutation-intent gaps.
3. Reconcile changed, added, deleted, and renamed paths into v1.
4. Apply overlay mutations through a captured sequence cutoff.
5. Repeat until caught up.
6. Under a short scope publication barrier, capture `Hfinal` and final cutoff.
7. While the barrier is held, apply the small final Merkle diff and mutation
   overlay through that cutoff.
8. Validate the final delta and sequence invariants, then publish the v1 active pointer with
   `source_head = Hfinal` and `applied_mutation_sequence = cutoff`.
9. Change the registry state to `v1_active_dual_write` as part of that same
   active-pointer publication, then release the barrier.
10. Subsequent writes dual-write to the already-active v0 and v1 paths.

The publication barrier has a strict time/work budget. If the final delta is too
large, release it without changing the active pointer, process another catch-up
round, and retry. Full page-chain and shadow validation happen before the
barrier; only bounded final-delta validation is permitted while writes are held.

Mutations are idempotent by document identity and sequence, so overlap between
Merkle diff and the mutation overlay is safe.

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
- source HEAD and mutation cutoff accounting;
- bounded-memory validation; and
- representative v0/v1 shadow-query comparison.

For production rollout, support a configurable shadow-read sample rate. The
server executes selected queries against both formats, compares document IDs,
records mismatches, and returns the currently active format's answer.

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
2. Require v1 to be caught up to the current mutation sequence.
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
- parser/plugin availability;
- estimated temporary and final disk use;
- configured and expected peak memory;
- v0 indexes with known stale/degraded status;
- current active tasks and conflicting maintenance;
- required minimum binary capability after finalization; and
- an estimated migration order.

Default order should prioritize exact/small indexes before large trigram and
phonetic indexes, limiting concurrent migrations by bytes rather than count.

## GC, Backup, Verify, and Repair

### GC

- Treat active manifests, previous rollback manifests, migration leases, and
  grace-period v0/v1 roots as derived GC roots.
- Traverse manifest-to-page/tile/value/catalog references.
- Reclaim orphaned build artifacts only when no active task/manifest references them.
- Never infer user-content liveness from index artifacts.

### Backup and Restore

- Index configuration and format registry are included by default.
- Derived v1 pages may be omitted by default and rebuilt after restore.
- An optional `include_indexes` mode includes active manifests and artifacts.
- Restore validates capability before activating included indexes.
- A restore without pages marks indexes `needs_rebuild`; it does not silently
  advertise them as ready.

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

## Observability

Expose per-index and aggregate metrics:

- format and active generation;
- migration state and source HEAD;
- page, posting, live-document, tombstone, and value counts;
- NVT resolution, populated cells, tile reads, backward-anchor distance, and hint hit rate;
- pages scanned per point/range query;
- boundary pages and typed rechecks;
- page-cache/NVT-tile/value-cache hits, misses, bytes, and pins;
- active/frozen memtable bytes and oldest mutation age;
- page splits, compactions, bytes written, and write amplification;
- planner estimates versus actual pages/postings;
- shadow-query comparisons and mismatches;
- degraded indexes and fallback scans; and
- v0 dual-write time and cost.

Dashboard diagnostics should answer:

- Which indexes own memory?
- Which indexes are dirty, pinned, migrating, degraded, or over budget?
- How effective are their NVT hints?
- Which queries scan more pages than planned?
- Which indexes are still v0 and why?

`EXPLAIN ANALYZE` should include the selected driver, scalar constraints, NVT
anchor, estimated/actual pages, membership operation, recheck count, cache hits,
and fallback reason.

## Implementation Phases

### Phase 0: Freeze Semantics and Establish Baselines

- [ ] Record this plan's invariants as architecture tests.
- [ ] Capture v0 compatibility fixtures for every converter and strategy.
- [ ] Add production-shaped benchmarks for exact, range, fuzzy, boolean, sort,
      aggregate, global search, sustained writes, and reindex.
- [ ] Record RSS, peak RSS, allocation count, page reads, bytes written, and latency.
- [ ] Add failure fixtures for missing NVT, corrupt NVT, corrupt index file, and stale postings.
- [ ] Confirm no phase changes KV bytes or behavior.

Exit: reproducible baseline and characterization suite.

### Phase 1: Decouple KV NVT from Field NVT

- [ ] Rename/wrap the current implementation as `KvNvt` without format changes.
- [ ] Add `FieldNvt` types in a separate module.
- [ ] Move field-index code off the shared KV NVT API.
- [ ] Add compile-time/module boundaries preventing accidental cross-use.
- [ ] Verify existing KV fixtures byte-for-byte.

Exit: field-index work can proceed without changing KV behavior.

### Phase 2: Converter v2 and Scope Catalog

- [ ] Implement canonicalization, typed comparison, fingerprints, and mapping generations.
- [ ] Fix typed configuration bounds.
- [ ] Add signed, floating, timestamp, collision, malformed-value, and multi-value property tests.
- [ ] Implement scope document ordinals and paged ordinal/FileKey mapping.
- [ ] Add document membership bitmap abstraction.

Exit: stable typed semantics and shared cross-field identity.

### Phase 3: IndexArtifact and v1 Page Format

- [ ] Add versioned `IndexArtifact` entry and KV tag.
- [ ] Update scanner, verify, repair, GC, backup, counters, and diagnostics.
- [ ] Implement manifests, active pointers, posting pages, value pages, and scope pages.
- [ ] Implement checksums, identity validation, generation pinning, and publication ordering.
- [ ] Add fault injection at every write/publish boundary.

Exit: crash-safe, directly addressable derived artifacts exist independently of v0.

### Phase 4: Sparse FieldNvt and Page Scanner

- [ ] Implement deterministic tile addressing and bounded tile cache.
- [ ] Implement empty-cell backward search and page-link backtracking.
- [ ] Implement conservative point and range scans with actual typed recheck.
- [ ] Implement non-blocking hint healing.
- [ ] Implement online resolution changes without moving pages.
- [ ] Test complete NVT loss, random gaps, stale/wrong hints, resize, and tile corruption.

Exit: v1 point/range reads are correct with no NVT and faster as hints heal.

### Phase 5: Bounded v1 Mutation Path

- [ ] Implement byte-bounded active/frozen memtables.
- [ ] Route all indexing entry points through one mutation-intent API.
- [ ] Cover file store, streaming finalize, blob batch commit, embedded batch write,
      merge, copy, rename, restore, delete, and reindex.
- [ ] Implement page-local inserts, replacements, tombstones, and splits.
- [ ] Implement overlay-visible reads and emergency spill.
- [ ] Enforce hard global memory limits under many-index write stress.

Exit: sustained writes do not load or dirty complete indexes.

### Phase 6: v1 Query Leaves and Strategy Selection

- [ ] Implement exact, IN, range, trigram, and phonetic page probes.
- [ ] Read only metadata when selecting an index strategy.
- [ ] Recheck canonical values lazily.
- [ ] Implement cost estimates and `EXPLAIN` details.
- [ ] Add limit-aware streaming for simple queries.

Exit: single-field v1 queries are bounded and match authoritative results.

### Phase 7: Boolean Planner and Membership Composition

- [ ] Implement cost-based AND driver selection.
- [ ] Implement scope membership bitmap AND/OR/NOT/XOR/DIFFERENCE.
- [ ] Restrict scalar-region composition to compatible same-field coordinate spaces.
- [ ] Replace unsafe/no-op strided and progressive execution.
- [ ] Add estimate-versus-actual planner feedback.

Exit: complex queries are bounded, correct, and strategy options have real behavior.

### Phase 8: Sorting, Pagination, Aggregation, and Global Search

- [ ] Push limits/cursors into ordered scans.
- [ ] Stream indexed sorts and bounded top-K sorts.
- [ ] Stream aggregate/group calculations from value pages.
- [ ] Replace global `usize::MAX` fan-out with bounded top-K merging.
- [ ] Add backpressure and cancellation to long query scans.

Exit: secondary query surfaces no longer require whole indexes or all results in memory.

### Phase 9: Mixed-Format Registry and Migration

- [ ] Implement v0/v1 format registry and per-index resolution.
- [ ] Add migration lease, HEAD pinning, source walker, checkpoints, and resume.
- [ ] Add dual-write v1 overlay and Merkle-diff catch-up.
- [ ] Add validation, shadow reads, cutover, grace rollback, and explicit finalization.
- [ ] Add dry-run, API, CLI, progress, cancellation, and disk/memory controls.
- [ ] Test interruption at every migration state.

Exit: copied v0 production databases migrate online with concurrent writes and safe rollback.

### Phase 10: Rollout, Documentation, and v0 Retirement

- [ ] Document v1 architecture, operations, migration, rollback, repair, and metrics.
- [ ] Update API, SDK, CLI, admin, query, indexing, backup, and deployment docs.
- [ ] Run complete unit/integration/property/fault suites.
- [ ] Run a real server against a `/tmp` database with mixed files and live HTTP queries.
- [ ] Run crash/restart soak with migration, writes, deletes, and queries concurrently.
- [ ] Migrate a copy of the FS-Server1 database and compare memory/performance/results.
- [ ] Deploy compatibility binary with v1 disabled by default.
- [ ] Enable one canary index, validate, then expand incrementally.
- [ ] Finalize v0 only after an explicit production review.

Exit: v1 is the default for new indexes; existing production indexes have an operator-controlled migration path.

## Required Test Matrix

### Correctness and Property Tests

- Every converter output is finite and within `[0.0, 1.0]` or rejects input.
- Typed comparator is reflexive, antisymmetric, and transitive.
- Order-preserving converters preserve typed ordering despite scalar collisions.
- Point/range results match an authoritative scan for randomized data.
- Multi-value documents match every indexed value.
- Missing or corrupt NVT cells never cause false negatives.
- Random hint corruption either heals or triggers a conservative fallback.
- Resolution growth/shrink does not change results.
- Random page splits, deletes, replacements, and compactions preserve results.
- Cross-field bitmap results match set-based reference evaluation.

### Migration Tests

- Open untouched v0 database with compatibility binary.
- Mixed v0/v1 fields in one scope.
- Mixed v0/v1 strategies on one field.
- Migrate while creating, overwriting, deleting, renaming, copying, restoring,
  merging, and blob-committing files.
- Crash/restart during base scan, page flush, manifest write, pointer publish,
  catch-up, validation, cutover, grace, finalization, and cleanup.
- Pause/resume and cancel/restart at every state.
- Parser unavailable, plugin version changed, disk full, memory pressure, and shutdown.
- Corrupt v1 page before and after cutover.
- Roll back during grace and confirm post-cutover writes exist in v0.
- Refuse unsafe old-binary deployment after finalization.
- Restore backup with and without derived indexes.
- GC while migration lease and old manifest readers are active.

### Resource and Performance Tests

- Peak index-owned memory remains below configured budget plus documented query pins.
- Adding inactive indexes does not increase resident postings memory.
- Sustained writes to many indexes do not make memory scale with total index corpus.
- One-file updates do not read or write complete indexes.
- Point queries read a bounded number of tiles/pages after warm-up.
- Range scans read pages proportional to matched region plus boundaries.
- Migration memory remains bounded from small fixtures through production copies.
- Write and query latency are measured under compaction and migration load.

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
- `docs/src/api/querying.md`: planner, strategy, limits, and `EXPLAIN` behavior.
- `docs/src/api/admin.md`: migration/status/validation/finalization endpoints and metrics.
- `docs/src/operations/reindex.md`: distinction between reindex and format migration.
- New `docs/src/operations/index-migration.md`: preflight, rollout, rollback, and recovery.
- CLI help and examples for all migration commands.
- `docs/SKILL.md`: bot-oriented safe migration and diagnostic workflow.
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

## Definition of Done

The refactor is complete only when:

- the KV remains compatible and unchanged;
- no field-index operation requires loading, cloning, sorting, or serializing a complete index;
- memory remains within a configured hard budget under many-index sustained writes;
- NVT deletion and random gaps preserve query correctness;
- point, range, fuzzy, boolean, sort, aggregate, pagination, and global-search
  results match authoritative reference execution;
- query planning uses conservative scalar regions and document-identity composition;
- migration from v0 is online, resumable, observable, validated, and reversible during grace;
- copied production databases pass migration, crash/restart, verify, and result comparison;
- operational and bot documentation is complete; and
- v0 retirement requires an explicit operator action.
