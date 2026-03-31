# AeorDB — Indexing Unification Design

## The Goal

Unify the NVT (engine KVS lookup) and the scalar ratio indexing module (user-facing indexes) under one pluggable conversion trait. One concept, one interface, infinite index types.

---

## Round 1: The Unified Converter Trait

### What We're Unifying

| Currently | Module | Purpose |
|---|---|---|
| `hash_to_scalar()` in nvt.rs | engine/ | Converts BLAKE3 hashes to [0.0, 1.0] for KVS bucket lookup |
| `ScalarMapping` trait in indexing/ | indexing/ | Converts typed values (u8, u64, string, etc.) to [0.0, 1.0] for index lookup |

These do the same thing: `value → scalar`. The trait should be shared.

### Proposed Trait

```rust
/// Converts any value to a normalized scalar in [0.0, 1.0].
/// The conversion determines how values are distributed across the index.
/// Order-preserving converters enable range queries.
pub trait ScalarConverter: Send + Sync {
  /// Convert raw bytes to a scalar in [0.0, 1.0].
  fn to_scalar(&self, value: &[u8]) -> f64;

  /// Is this converter order-preserving?
  /// If true: to_scalar(a) < to_scalar(b) implies a < b (for the type's natural ordering).
  /// Required for range queries (gt, lt, between).
  fn is_order_preserving(&self) -> bool;

  /// Human-readable name for this converter.
  fn name(&self) -> &str;
}
```

### Built-in Converters

| Converter | Order-Preserving | Use Case |
|---|---|---|
| `HashConverter` | No | KVS hash lookups. BLAKE3 hashes → uniform scalar. |
| `U8Converter` | Yes | u8 fields. value / 255.0 |
| `U16Converter` | Yes | u16 fields. |
| `U32Converter` | Yes | u32 fields. |
| `U64Converter` | Yes | u64 fields. |
| `I64Converter` | Yes | Signed integers. Shifted to [0.0, 1.0]. |
| `F64Converter { min, max }` | Yes | Floats. Normalized within configurable range. |
| `StringConverter` | Partially | Strings. Multi-stage: first byte weighted + length. Preserves rough lexicographic order but not exact. |
| `TimestampConverter { min, max }` | Yes | UTC millisecond timestamps. |
| `WasmConverter { plugin }` | User-defined | Custom WASM plugin does the conversion. |

<!-- 
Love it!
 -->

---

## Concerns and Open Questions

### Concern 1: Non-Uniform Distribution Kills Uniform Buckets

**The problem:** The NVT uses uniform-width buckets. This is perfect for hashes (uniform distribution) but terrible for user data.

Example: 1 million age values. Ages cluster 20-50. `U64Converter` maps linearly: age 30 → scalar 0.000000014. ALL million values end up in bucket 0. The other 1023 buckets are empty. Every lookup scans the entire dataset.

**Question for Wyatt:** The original scalar ratio design had self-correcting offset tables that ADAPT to the distribution. The NVT has fixed uniform buckets. Should we:

a) Use **uniform NVT buckets** for everything (simple, but bad for skewed data)?
b) Use **adaptive offset tables** (your original design) for user indexes, NVT for hash lookups?
c) Use **adaptive NVT** — start uniform, then rebalance buckets based on actual data distribution?
d) Let the **converter handle it** — a smart converter maps values to uniform scalars even if the source data is skewed? (e.g., `AgeConverter` knows ages cluster 20-50 and spreads that range across [0.0, 1.0] more evenly)

<!-- WYATT: This is the core architectural question. Option (d) is the most elegant — the converter IS the distribution equalizer. But it requires the converter to know the data distribution ahead of time, which isn't always possible. -->

### Concern 2: Range Queries Need Order Preservation

**The problem:** "Find all ages > 30" requires `to_scalar(30) < to_scalar(31)`. Hash converters don't preserve order (by design — that's what makes hashes good for uniform distribution). String converters are only approximately order-preserving.

**Question for Wyatt:** Should we:

a) Enforce that all user-facing index converters MUST be order-preserving? (Prevents using hash-based converters for user indexes)
b) Allow non-order-preserving converters but disable range queries on those indexes? (More flexible, queries return an error if you try a range query on a hash-indexed field)
c) Something else?

The `is_order_preserving()` method on the trait enables option (b) — the query engine checks at runtime and refuses range queries on non-order-preserving indexes.

<!-- WYATT: I'm leaning (b). Maximum flexibility. If a user wants to index a field by hash (fast exact lookup, no range queries), let them. The engine just refuses range queries on that index and tells them why. -->

### Concern 3: Multi-Dimensional Data

**The problem:** Geospatial data is 2D (lat/lon). The NVT is 1D. Flattening 2D to 1D (hilbert curve, z-order curve) works for proximity queries but is lossy for "find all points within a circle."

**Question for Wyatt:** Is the NVT the right structure for geospatial, or do we need a separate index type (R-tree, quad-tree) for spatial data?

Your original plan mentioned "multi-dimensional vectors" for complex types. The scalar approach can work for 1D queries (find by latitude OR longitude) but not for 2D queries (find by latitude AND longitude simultaneously).

Options:
a) Flatten to 1D (hilbert curve converter) — approximate, works for many cases
b) Multiple 1D indexes (one for lat, one for lon) — exact per dimension, combine at query time
c) Separate spatial index type — purpose-built for 2D/3D queries (future)

<!-- WYATT: I suspect the answer is (b) for now, (c) eventually. Your NDNVT concept might naturally extend to this — each dimension gets its own NVT, and the query engine intersects results. -->

### Concern 4: Memory Footprint at Scale

**The problem:** The KVS NVT is always in memory. If every user index has its own NVT, and there are hundreds of indexes, memory adds up.

**Question for Wyatt:** Should index NVTs:

a) Always be in memory (fast, but scales with number of indexes)?
b) On-demand loading with LRU eviction (like directories)?
c) Only the KVS NVT is always in memory, user index NVTs are on-disk with caching?

<!-- WYATT: I'm thinking (c). The KVS is critical-path (every operation uses it). User indexes are query-path (only used when querying that specific field). Different access patterns, different memory strategies. -->

### Concern 5: WASM Converter Performance

**The problem:** A WASM converter plugin called per-value during bulk indexing. Millions of host↔WASM boundary crossings.

**Question for Wyatt:** Options:

a) Accept the overhead — WASM is ~5x slower, but conversion is tiny (a few math ops)
b) Batch API — pass a batch of values to the WASM plugin, get a batch of scalars back (amortize the boundary crossing cost)
c) Only allow native (Rust) converters for built-in types, WASM for exotic types

<!-- WYATT: Batch API (b) is probably the right answer. Send 1000 values, get 1000 scalars back. One boundary crossing instead of 1000. -->

### Concern 6: The Lookup Structure Question

**The problem:** We have two lookup structures:
- **NVT buckets** (uniform width, static, good for uniform data)
- **Offset tables** (variable, self-correcting, good for skewed data)

Both use scalars. Both do the same job (scalar → data location). But they're optimized for different distributions.

**Question for Wyatt:** Should the unified index allow BOTH lookup structures, chosen per-index based on the converter type?

```rust
enum IndexStructure {
  NVTBuckets(NormalizedVectorTable),   // for uniform data (hashes)
  OffsetTable(OffsetTable),             // for skewed data (user values, self-correcting)
}
```

Or should we make one structure that handles both cases (adaptive)?

<!-- WYATT: Your original "always good, not always perfect" design was the offset table with self-correction. The NVT is essentially a simpler, non-self-correcting version. Can the offset table replace the NVT entirely? It would self-correct for ANY distribution, whether uniform (hashes) or skewed (user data). The NVT becomes just an optimization hint for the initial bucket layout. -->

### Concern 7: Index Storage in the Engine

**The problem:** Where does index data live in the append-only engine?

Options:
a) Each index is a FileRecord at `.indexes/{field_name}` — the index data IS a file
b) Index entries are their own entity type (add IndexEntry to the entity types)
c) Index data is embedded in the KV block alongside hash→offset entries

<!-- WYATT: I think (a) aligns with "everything is a file." The index is a file whose content is the NVT + sorted entries. When the index is updated, a new version of the file is written (append-only). Old versions are preserved for old snapshots. -->

### Concern 8: Write Pipeline Integration

**The problem:** When a file is stored, the pipeline must:
1. Store chunks
2. Store FileRecord
3. Update directory tree
4. **Parse fields** (via parser plugins)
5. **Update indexes** (via converters + NVT/offset tables)

Steps 4-5 are NEW. They happen after the file is stored but before the operation is "complete." If the server crashes between step 3 and step 5, the file exists but isn't indexed. Is that acceptable?

<!-- WYATT: I think yes — the file is stored and recoverable. The index is rebuildable (re-run parsers on all files at that path). Index lag is acceptable; data loss is not. -->

---

## Proposed Unified Architecture

```
ScalarConverter (trait)
  ├── HashConverter (engine internal, for KVS)
  ├── U64Converter, I64Converter, StringConverter, etc.
  ├── TimestampConverter
  ├── GeoHilbertConverter (future)
  └── WasmConverter (user plugin)

IndexStructure
  ├── For KVS: NVT buckets → sorted KV block (always in memory)
  └── For user indexes: offset table/NVT → sorted index entries (on-demand, LRU cached)

Write pipeline:
  store_file → write chunks + FileRecord + directories
            → run parsers (extract fields)
            → for each indexed field: convert → update index structure

Query pipeline:
  query request → identify target index
               → load index NVT/offset table (cache)
               → converter: query value → scalar
               → lookup: scalar → candidate entries
               → filter/verify candidates
               → return results
```

---

## Resolved Questions

| # | Question | Resolution |
|---|---|---|
| 1 | Distribution handling | **Converter handles it.** The converter tracks its own observed min/max range and maps values to [0.0, 1.0] within that range. Uniform buckets work because the converter's job is to MAKE the distribution uniform. Type authors can bake in domain knowledge (e.g., age distribution) for even better results. This is exactly what Wyatt built and tested in 2012. |
| 2 | Range queries on non-order-preserving indexes | **Refuse with error (option b).** `is_order_preserving()` on the trait. Engine refuses range queries on non-order-preserving indexes and tells the user why. Maximum flexibility. |
| 3 | Multi-dimensional | **Multiple 1D indexes for now (option b).** One NVT per dimension, query engine intersects results. Full spatial index types (R-tree, quad-tree) deferred to future. |
| 4 | Memory | **KVS NVT always in memory, user index NVTs on-demand with LRU (option c).** Different access patterns, different memory strategies. |
| 5 | WASM performance | **Batch API (option b).** Send N values, get N scalars back. One boundary crossing instead of N. |
| 6 | Lookup structure | **Uniform buckets work** because the converter makes the distribution uniform. No need for two structures. The self-correcting property comes from the converter expanding its range as new data is seen — existing scalars are "always good" approximations that improve on access. |
| 7 | Index storage | **Indexes are files at `.indexes/` (option a).** Everything is a file. Index data is a FileRecord whose content is the NVT + sorted entries. Updated via append-only (new version written). Old versions preserved for snapshots. |
| 8 | Crash between store and index | **Acceptable.** File is stored and recoverable. Index is rebuildable by re-running parsers. Index lag is acceptable; data loss is not. |

---

## Additional Resolved Decisions (from further discussion)

| Decision | Resolution |
|---|---|
| Converter trait name | `ScalarConverter` with `to_scalar(&self, value: &[u8]) -> f64` and `is_order_preserving() -> bool` |
| Converter range tracking | Converters track `observed_min` / `observed_max` internally. Range expands as new data is seen. |
| NVT and indexing unification | One `ScalarConverter` trait shared by both engine KVS (HashConverter) and user indexes (U64Converter, StringConverter, etc.) |
| Functions as endpoints | Published functions are callable HTTP endpoints with typed arguments. Data and code versioned together. |
| Server-side compilation | Raw Rust source pushed to DB, compiled to WASM on first invocation, cached. SDK lives in the DB at `/.system/sdk/`. |
| Schema-as-code | `#[aeordb_schema]` proc macro generates parser, converters, index config, and typed query builder from struct definitions. |
| Query builder | Chainable builder (inspired by Mythix ORM). Operations accumulate until terminal method (`.all()`, `.first()`, `.cursor()`). |

---

*All indexing questions resolved. Ready for implementation.*
