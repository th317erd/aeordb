# Child 05: Index Definitions, Immutable Pages, and Sparse NVT

**Parent:** [AeorDB V4, NVT, GC, and Migration Campaign](../../2026-08-03-aeordb-v4-nvt-gc-refactor.md)
**Landing unit:** P5
**Status:** Starts after Child 01 freezes registries and Child 03 provides immutable shadow roots
**Primary owner:** field-index storage/semantics owner
**Activation:** shadow derived artifacts only; Child 06 owns runtime activation

## 1. Outcome

Replace whole-index blobs with immutable, copy-on-write, page-addressable
derived artifacts. Freeze definition-owned canonical semantics, scope-local
document identity, ordered posting/value pages, and a sparse fixed-point NVT
that lands readers near relevant pages without becoming answer authority.

No operation in this child may load, clone, sort, or serialize a complete index.
Deleting every NVT tile must preserve exact posting-page results.

## 2. Owned Territory

- converter/strategy/source/definition contracts and built-in conformance;
- scope catalog, document ordinals, FileKey reverse map, and document state;
- value store and exact canonical source values;
- immutable artifact directories, posting/value/state pages, and manifests;
- sparse NVT tiles and hint repair/rebuild;
- mutation-journal/checkpoint codecs consumed later by Child 06;
- page split/merge/compaction and validation;
- v0 codec/read compatibility modules; and
- independent model/property/fixture tests.

Forbidden without handoff:

- namespace/root authority and hard commit waiters;
- query/API activation;
- GC sweep/Void policy;
- v3 source mutation during migration; and
- changing KV ordering or page layout.

## 3. Refactor Boundary

Freeze the existing shared NVT behavior into explicit compatibility owners:

- `LegacyNvtV1`: byte-identical legacy codec/core;
- `KvNvt`: current KV wrapper, behavior unchanged;
- `FieldIndexV0Nvt`: v0 field-index compatibility wrapper; and
- `FieldNvt`: independent sparse v1 hint implementation.

No v1 field-index code uses the legacy/KV NVT API. KV format, stage size,
normalized bucket mapping, key resolution, flush, expansion, startup, and
snapshot behavior remain outside this refactor.

Preserve current public facades during migration, but all new storage lives in
focused versioned modules for v0 compatibility, definitions, manifests,
directories, pages, NVT, checkpoints, and validation.

## 4. Definition-Owned Semantics

### Converter Contract

Each immutable converter definition owns:

- accepted source types and canonicalization;
- typed equality and comparison;
- fixed `u64` normalized coordinate in `[0, u64::MAX]`;
- expansion/token behavior and strict limits;
- order-preserving capability;
- complete serialized configuration and fingerprint; and
- exact query-literal compilation.

Persisted numeric canonical bytes remain little-endian. Typed comparators decode
them; generic lexicographic byte order is not numeric order. Floating-point
ratios are conceptual only and are never persisted or used as an authority.

Scalar collisions are expected. Posting keys and complete canonical source
values are exact-rechecked before accepting equality or a range boundary.
`typed_exact_blake3_v1` is candidate routing only; injected digest collisions
must not produce a false match.

### Definition Identity

An index ID fingerprints complete semantics: scope/glob, field/source selector,
converter/strategy config, Unicode/collation/tokenizer/multi-value rules,
parser/plugin identity/version/checksum/limits, canonical value schema, and
index format. A semantic change creates a new shadow generation; it never
reinterprets existing coordinates/postings.

Dependencies remain pinned for a build. A plugin/config change pauses or
invalidates the build rather than mixing versions.

## 5. Scope Catalog and Document Identity

All fields/strategies under one effective index scope share:

- nonreused scope-local `u64` document ordinals;
- ordinal-to-current FileKey/path/revision records;
- live FileKey-to-ordinal reverse records;
- exact source NamespaceRoot and semantic/definition identities;
- active definitions and high-water marks; and
- tombstones preserving incarnation/non-reuse history.

Cross-field set operations use ordinals only within one ScopeId. Queries that
cross child config/glob overrides evaluate each effective scope independently
and merge by FileKey. Identical ordinal numbers in different scopes are never
compared or combined.

Recreating a deleted path creates a new document incarnation and ordinal.
Routine compaction never renumbers ordinals.

## 6. Canonical Value and State Stores

- Value pages store the complete canonical source value per document/source
  ordinal and current RecordRevision identity.
- Multi-value ordinals preserve source order and are contiguous for live rows.
- Document state pages record only deterministic unindexable outcomes using
  stable structured reason/evidence. Operational parser/plugin failures degrade
  the generation and do not persist as a negative match.
- Exact equality always rechecks complete canonical values.
- Missing, typed null, malformed, over-limit, and multi-value behavior follows
  the frozen definition and independent evaluator.

## 7. Ordered Posting Pages

Posting order is coordinate first, then complete converter-owned posting key,
document ordinal, source-value ordinal, and expansion ordinal. The coordinate
preserves NVT range geometry; exact fields disambiguate collisions.

Requirements:

1. ArtifactDirectory fences/ranks are the correctness path.
2. Bidirectional page links support sequential scans and must agree exactly.
3. Stable PageIds never reuse or wrap. Split retains left ID and allocates a
   fresh right ID; merge retains lower ID and retires upper forever.
4. Copy-on-write rewrites only affected pages, required neighbor links, and the
   directory path to a new manifest.
5. Birth generation permits same-owner structural sharing across manifests.
6. Physical offset/length/write-sequence spans are hints and are used for
   coalescing only after current locator validation.
7. Missing/corrupt pages invalidate the derived closure and force fallback or
   rebuild. Skipping a page cannot return a complete query.
8. V1 pages are uncompressed. Any future codec requires a new capability and
   evidence.

Initial normal target is 64 KiB, split above 96 KiB, merge below 16 KiB when
the combined page fits 64 KiB, and hard cap 4 MiB for a dedicated legal large
record. P0 benchmarking may tune manifest values without changing format.

Directory `logical_bytes` is the exact sum of live record bytes. A nonempty
tombstone-only page or subtree therefore has zero logical bytes; directory
descriptors require `logical_bytes == 0` exactly when `live_count == 0`.

## 8. Sparse NVT

NVT is a sparse tiled map from normalized coordinate cells to nearby posting
PageIds and cardinality hints.

### Insert/Update

- Converter output already determines logical placement; no global NVT sort is
  performed.
- A new immutable tile generation records only populated cells.
- Resolution changes add/discard hint precision without moving postings.
- Hints may overwrite, have gaps, lose resolution, or disappear entirely.

### Lookup

1. Compile point/range bound through the exact converter definition.
2. Compute target cell/tile.
3. Use the exact cell or scan backward through sparse entries/previous nonempty
   tiles for a predecessor anchor.
4. If none exists, begin at the posting manifest's first page.
5. Resolve hinted PageId through the pinned posting directory and validate its
   fences/generation.
6. Fall back to exact directory predecessor search when stale/missing.
7. Scan posting pages and exact-recheck complete values until the requested
   boundary/result is proven.

NVT never stores an ArtifactHash to pin a posting generation. Corrupt/missing
NVT is discarded. Healing is best-effort and cannot block or change the current
query.

Initial tile cell count is 1,024; benchmark 256, 1,024, and 4,096. Resolution
and tile size are tuning fields in manifests, not authority.

## 9. Virtual Fields and Built-In Profiles

Every supported virtual field has at least an exact-equality strategy when its
value is defined. The built-in profile must preserve:

- `@filename`: exact plus approved trigram, fuzzy, and phonetic strategies;
- `@path`: existing approved behavior, unchanged by this campaign;
- `@hash`: exact full-file content hash only, with no trigram index;
- `@content_type`: exact only, with no trigram index;
- `@extension`, times, and size: exact/typed ordering as appropriate; and
- canonical `@file_name` alias handling at one ingress adapter only.

Whole-file content hash remains stored on FileRecord write/migration. Indexing
does not stream file content to synthesize it.

## 10. Immutable Mutation and Checkpoint Codecs

Implement exact v1 codecs for:

- typed per-path mutation records and batches;
- journal segment chain/reset and source-root boundaries;
- task checkpoint, resume key, attached unpublished roots, and external run
  descriptor;
- page/manifests and generation publication inputs; and
- validation reports and compaction evidence.

These codecs are recoverable derived infrastructure. Child 06 decides runtime
soft-journal and coverage publication. A missing journal can require diff or
rebuild; it cannot make partial artifacts authoritative.

## 11. Split, Merge, Compaction, and Reclamation

- Build dependencies, pages, directories, manifest, then inactive active
  pointer in that order.
- Full verify recomputes owner identity, hashes, fences, ranks, links, counts,
  definitions, values, and source-root coverage.
- Tombstones drop only when coverage/journal/pin boundaries prove safety.
- Compaction is shadow COW and bounded by memory/scratch reservations.
- Obsolete artifacts enter Child 04's physical inventory/quarantine; index code
  never deletes/reuses WAL extents itself.
- Backup/peer logical transfer omits derived pages by default and restores
  registry state as `needs_rebuild` unless a separately compatible closed
  generation is explicitly transferred.

## 12. Landing Sequence

1. P5-1 legacy/KV NVT separation and byte-identical v0/KV fixtures.
2. P5-2 converter/strategy/source/definition registries and independent model.
3. P5-3 scope catalog, value store, state, identity, and manifest codecs.
4. P5-4 artifact directories and ordered page readers/writers.
5. P5-5 COW insert/delete/split/merge/compaction.
6. P5-6 sparse NVT tiles, lookup fallback, and healing.
7. P5-7 mutation/checkpoint codecs and unpublished attachment rooting.
8. P5-8 shadow index build, restart, corruption, memory, and full validation.

Each unit is independently green and pushed. Nothing activates a production
v1 index pointer.

## 13. Verification

Required campaign target:

```bash
timeout 15m cargo test -j 6 -p aeordb --test index_v1_reference_spec
```

Existing inputs:

```bash
timeout 10m cargo test -j 6 -p aeordb --test converter_spec
timeout 10m cargo test -j 6 -p aeordb --test index_store_spec
timeout 10m cargo test -j 6 -p aeordb --test index_values_spec
timeout 10m cargo test -j 6 -p aeordb --test multi_index_spec
timeout 10m cargo test -j 6 -p aeordb --test nvt_spec
timeout 10m cargo test -j 6 -p aeordb --test nvt_ops_spec
timeout 10m cargo test -j 6 -p aeordb --test trigram_spec
timeout 10m cargo test -j 6 -p aeordb --test phonetic_spec
timeout 10m cargo test -j 6 -p aeordb --test content_hash_spec
```

Property/model tests cover deterministic cross-platform coordinates,
comparator laws, all-values-one-coordinate collisions, random mutations,
definition changes, multi-scope identity, splits/merges/tombstones, absent/
stale/corrupt NVT, shuffled build order, restart, and compaction.

## 14. Resource and Real-World Proof

- Build/query indexes larger than memory with small forced page/tile caches.
- Add many inactive indexes and prove resident posting/value bytes do not scale.
- Update one file and prove no complete index read/write occurs.
- Compare point/range reads with and without all NVT tiles.
- Validate bounded physical coalescing on contiguous WAL spans and fallback on
  stale/short/relocated spans.
- Build shadow indexes for real JSON/text/media metadata in `/tmp/codex`, reopen,
  full verify, corrupt selected hints/pages, and compare the independent model.
- Record page reads, bytes, amplification, RSS, latency, and cache behavior.

## 15. Rollback

All v1 artifacts remain shadow-derived. Remove their unselected registry roots
or quarantine them through Child 04. Namespace authority and user bytes remain
untouched. Keep v0 readers; never route old v0 mutation code into v1 pages.

## 16. Definition of Done

- [ ] Legacy KV/v0 NVT bytes and behavior are unchanged.
- [ ] Converter/query literals share exact canonical semantics.
- [ ] Digest/coordinate collisions cannot create false matches.
- [ ] Scope ordinals never cross scope identity or reuse.
- [ ] Every page/directory operation is bounded and COW.
- [ ] No index operation loads or serializes a complete index.
- [ ] NVT is sparse, approximate, disposable, and independently rebuildable.
- [ ] Empty/stale/corrupt NVT returns the exact reference result.
- [ ] Virtual field profiles match ratified strategy policy.
- [ ] Shadow build/restart/compaction reproduces fixtures and reference output.
- [ ] Memory and physical read amplification evidence meet parent gates.
