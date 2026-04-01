# AeorDB — NVT Bitmap Compositing Query Engine

Planning the implementation of NVT bitmap compositing for query execution.

---

## What We Have Now

The current query engine (`query_engine.rs`) works but is naive:
1. For each field query, loads the `FieldIndex`, runs a lookup → candidate file hashes
2. Collects all candidate sets into `Vec<HashSet>`
3. Intersects them (AND only) by retaining entries present in all sets
4. Loads FileRecords for survivors

**Problems:**
- Only supports AND (intersection). No OR, NOT, XOR.
- Loads ALL candidates into memory as HashSets — O(entries) memory
- No progressive refinement — scans entire indexes even if 99% will be eliminated
- FieldIndex uses sorted Vec binary search, not NVT buckets
- The NVT we built (unified with ScalarConverter) isn't used for field indexes at all

## What We Want

NVT bitmap compositing:
- Every field index is backed by an NVT (not just a sorted Vec)
<!-- At _least one_, yes... (probably mode than one) -->
- Queries composite NVT bitmaps — AND, OR, NOT, XOR
- Strided access for fast rough passes
- Progressive refinement: rough pass eliminates 95%+, precise scan on survivors
- The query builder supports full boolean logic

---

## Round 1: Implementation Plan

### Task 1: FieldIndex Backed by NVT

Currently `FieldIndex` in `index_store.rs` stores:
```rust
pub entries: Vec<IndexEntry>,  // sorted by scalar
```

Change to also maintain an NVT over the entries:

```rust
pub struct FieldIndex {
  pub field_name: String,
  pub converter: Box<dyn ScalarConverter>,
  pub entries: Vec<IndexEntry>,        // sorted by scalar
  pub nvt: NormalizedVectorTable,      // bucket index into entries
}
```

When entries are inserted/removed, the NVT is rebuilt (or updated incrementally). The NVT's buckets point into ranges of the sorted entries Vec — exactly like the KVS NVT points into the KV block.

**Key question:** Should we rebuild the NVT on every insert, or batch rebuilds? For bulk indexing (store 1000 files), rebuilding per-insert is wasteful. Better: dirty flag + rebuild on query.

<!-- WYATT: Thoughts on lazy NVT rebuild vs eager? -->

<!-- 
🤔 So, I think part of the benefit of this structure is that it can heal on read. It is the reader that holds the offset, the scan, and the correct offset. It is the reader that will know they had to scan hundreds of entries, possibly a full bucket before they found what they were looking for.

We also just want to be able to scale the NVT... period. Like a raster. Just double it (or whatever), and wham... new higher resolution NVT. It still has low resolution data, like a scaled raster, but that poor resolution can heal on read.

Now readers must be concurrent. So that raises the question: How do you have a good concurrent writer that can manage (and consolidate) multiple simultaneous changes? I am personally thinking that we store a "change list" on a stack with each reader... when the reader finishes its operation (reporting some desired updates to the index), then the coordinator will generate a new higher resolution NVT, and in an instant (mutex), swap it out with the old one. All readers then happily proceed with finding their data, reporting desired updates, having all those updates go onto a stack, have those updates mathematically optimized (probably just the average, or mean). Walla! You keep inserting things, including new buckets into the index, the readers/writers request changes (caused by low efficiency), and a coordinator aggregates, generates a new NVT, and swaps it out with the old one (notice how the buckets themselves never has to change).

Note: Notice how I said "differences". We wouldn't want to store a full copy of the NVT (which could be hundreds of megabytes). Instead, we want to have readers report changes (i.e. request 0.45434 be updated to offset 12345623456). We then aggregate these altogether (which are likely going to all be the same number/offset), and update the underlying NVT by "swapping the buffer", as they say in Arcade Game programming.
 -->

### Task 2: NVT Bitmap Operations

Create `aeordb-lib/src/engine/nvt_ops.rs`:

```rust
/// Result of a bitmap composite operation.
/// Contains the surviving bucket indices.
pub struct NVTMask {
  bucket_count: usize,
  // Bitset: one bit per bucket. 1 = surviving, 0 = eliminated.
  bits: Vec<u64>,  // packed bits, 64 buckets per u64
}
```

Operations:
```rust
impl NVTMask {
  /// Create mask from an NVT: bucket is "on" if entry_count > 0
  fn from_nvt(nvt: &NormalizedVectorTable) -> Self;

  /// Create mask from NVT with a scalar range filter
  /// Only buckets whose scalar range overlaps [min_scalar, max_scalar] are "on"
  fn from_nvt_range(nvt: &NormalizedVectorTable, min_scalar: f64, max_scalar: f64) -> Self;

  /// Logical AND — surviving buckets must be on in BOTH masks
  fn and(&self, other: &NVTMask) -> NVTMask;

  /// Logical OR — surviving buckets on in EITHER mask
  fn or(&self, other: &NVTMask) -> NVTMask;

  /// Logical NOT — invert the mask
  fn not(&self) -> NVTMask;

  /// Logical XOR
  fn xor(&self, other: &NVTMask) -> NVTMask;

  /// Difference: self AND NOT other
  fn difference(&self, other: &NVTMask) -> NVTMask;

  /// Count surviving (on) buckets
  fn popcount(&self) -> usize;

  /// Iterate surviving bucket indices
  fn surviving_buckets(&self) -> Vec<usize>;

  /// Strided version: only check every Nth bucket
  fn and_strided(&self, other: &NVTMask, stride: usize) -> NVTMask;

  /// Progressive: rough pass at stride, then precise on survivors
  fn and_progressive(&self, other: &NVTMask, initial_stride: usize) -> NVTMask;
}
```

Using a packed bitset (Vec<u64>) means:
- 1024 buckets = 16 u64s = 128 bytes
- AND/OR/NOT = bitwise ops on 16 u64s = nanoseconds
- 16M buckets = 256K u64s = 2MB — still fast for bitwise ops

<!-- WYATT: Should NVTMask be its own struct, or integrated into the NVT? I'm leaning separate — the mask is a query-time artifact, the NVT is a persistent structure. -->

<!-- 
I agree... but let's make sure however we are desiging this that it is compatible with a GPU in future please.

Also, keep in mind that the mapping function itself can be used on the input.

f(x) -> NVT -> offset
If you want all values that are >= 0.6, then you:
f(0.6) -> offset into NVT

So, you ALWAYS have the correct starting point. You can also plot a range with `f(max) - f(min)`.

This means that any value that is part of the query has a boundary that is already known. If query you like:
`WHERE a.name = 'derp' and a.age > 50`, then you know the age index is `f(age + 1)` and everything following, and the name index is `f(name)`, which is a single bucket. Where those two indexes overlap is where you have your data.
 -->

### Task 3: Range Masks from Converters

For range queries (gt, lt, between), we need to create a mask that marks which NVT buckets fall within a scalar range:

```
Query: age > 30
  1. converter.to_scalar(30) → 0.42
  2. All buckets with scalar > 0.42 → mask bits set
  3. AND with other query masks
```

For exact queries:
```
Query: name = "Bob"
  1. converter.to_scalar("Bob") → 0.67
  2. Only the bucket containing 0.67 → single bit set
  3. AND with other masks → still just that one bucket (if it survived)
  4. Scan entries in that bucket for exact match
```

The `from_nvt_range` constructor handles this.

<!-- WYATT: For exact queries, should we mark just one bucket (tight) or the bucket ± neighbors (fuzzy, accounts for converter imprecision)? I'm leaning tight + post-filter. -->

<!-- 
Whichever way we go about this, we won't have to really worry I don't think. The "search" method will find the start of the bucket it is looking for, regardless of if it has to search backwards, or forwards.
 -->

### Task 4: Query Builder with Boolean Logic

Extend `QueryBuilder` to support OR, NOT, and grouping:

<!-- 
Is there anyway to just have numbers, instead of this `&30_u64.to_be_bytes()` business?
 -->
 
```rust
// AND (current):
QueryBuilder::new(&engine, "/users/")
  .field("age").gt(&30_u64.to_be_bytes())
  .field("city").eq(b"NYC")
  .all()

// OR (new):
QueryBuilder::new(&engine, "/users/")
  .or(|q| {
    q.field("age").gt(&30_u64.to_be_bytes())
     .field("city").eq(b"NYC")
  })
  .or(|q| {
    q.field("role").eq(b"admin")
  })
  .all()

// NOT (new):
QueryBuilder::new(&engine, "/users/")
  .field("age").gt(&30_u64.to_be_bytes())
  .not(|q| {
    q.field("role").eq(b"admin")
  })
  .all()

// Complex:
QueryBuilder::new(&engine, "/users/")
  .and(|q| {
    q.field("age").gt(&30_u64.to_be_bytes())
     .field("active").eq(&[1])
  })
  .not(|q| {
    q.field("role").eq(b"banned")
  })
  .limit(100)
  .all()
```

Internally, this builds a tree of operations:

```rust
enum QueryNode {
  Field(FieldQuery),                    // leaf: single field operation
  And(Vec<QueryNode>),                  // all children must match
  Or(Vec<QueryNode>),                   // any child matches
  Not(Box<QueryNode>),                  // invert child
}
```

The query executor walks the tree bottom-up:
1. Leaf nodes → create NVTMask from the field's NVT + the query operation
2. AND nodes → bitwise AND all child masks
3. OR nodes → bitwise OR all child masks
4. NOT nodes → bitwise NOT the child mask
5. Final mask → surviving buckets → scan entries → load FileRecords

<!-- WYATT: The closure-based API is Rusty and ergonomic. But it's also compile-time — you can't build a query tree from JSON at runtime with closures. We need BOTH: closures for the Rust SDK, and a QueryNode tree for the HTTP/JSON API. -->

<!-- 
You are very correct about this!
 -->

### Task 5: HTTP Query API with Boolean Logic

Update `POST /query` to support boolean logic in JSON:

```json
{
  "path": "/users/",
  "where": {
    "and": [
      { "field": "age", "op": "gt", "value": 30 },
      { "field": "city", "op": "eq", "value": "NYC" },
      {
        "not": {
          "field": "role", "op": "eq", "value": "banned"
        }
      }
    ]
  },
  "limit": 100
}
```

Or with OR:

```json
{
  "path": "/users/",
  "where": {
    "or": [
      {
        "and": [
          { "field": "age", "op": "gt", "value": 30 },
          { "field": "city", "op": "eq", "value": "NYC" }
        ]
      },
      { "field": "role", "op": "eq", "value": "admin" }
    ]
  }
}
```

The JSON structure maps directly to the `QueryNode` tree.

<!-- WYATT: Should we also support a flat array format for simple AND-only queries (backward compatible with current format)? i.e. the current `"where": [...]` is sugar for `"where": { "and": [...] }` -->

<!-- 
Sure! I am a fan of sugar. :)
 -->

### Task 6: Strided / Progressive Execution

The query executor gets a `strategy` option:

```rust
pub enum QueryStrategy {
  Full,                      // scan all buckets (current behavior)
  Strided(usize),            // skip every N buckets
  Progressive {              // rough pass, then refine
    initial_stride: usize,
    refinement_threshold: usize,  // max surviving buckets before refining
  },
  Auto,                      // engine picks based on index sizes
}
```

For `Auto`:
- If all indexes have < 10K entries: Full (fast enough)
- If any index has > 100K entries: Progressive with stride 64
- If any index has > 1M entries: Progressive with stride 256

<!-- WYATT: Auto is the right default. Users shouldn't have to think about query strategy. But exposing it is good for benchmarking and tuning. -->

<!-- 
Agreed.
 -->

### Task 7: Update Index Serialization

FieldIndex serialization must now include the NVT data. The index file at `.indexes/{field}.idx` contains:

```
[Converter state]
[NVT serialization (buckets)]
[Sorted entries (scalar + file_hash)]
```

This is already close to what we have — we just add the NVT to the serialization.

---

## Implementation Order

```
Task 1 (FieldIndex + NVT)        ← foundation
Task 2 (NVTMask bitmap ops)      ← core compositing
Task 3 (Range masks)             ← query-to-mask conversion
Task 4 (QueryBuilder boolean)    ← Rust API
Task 5 (HTTP boolean queries)    ← JSON API
Task 6 (Strided/Progressive)     ← optimization
Task 7 (Serialization update)    ← persistence
```

Tasks 1-3 are the foundation. Task 4+5 are the API. Task 6 is optimization. Task 7 is persistence.

---

## Test Plan

```
nvt_ops_spec.rs:
  - test_mask_from_nvt
  - test_mask_from_nvt_range
  - test_mask_and
  - test_mask_or
  - test_mask_not
  - test_mask_xor
  - test_mask_difference
  - test_mask_popcount
  - test_mask_surviving_buckets
  - test_mask_and_strided
  - test_mask_progressive_refinement
  - test_mask_cross_resolution (different bucket counts)
  - test_mask_empty
  - test_mask_full (all buckets on)

query_boolean_spec.rs:
  - test_query_and (current behavior, regression)
  - test_query_or
  - test_query_not
  - test_query_complex_nested
  - test_query_node_tree_execution
  - test_query_json_and_format
  - test_query_json_or_format
  - test_query_json_not_format
  - test_query_json_backward_compatible (flat array = AND)

query_strategy_spec.rs:
  - test_strided_query_produces_correct_results
  - test_progressive_query_produces_correct_results
  - test_auto_strategy_selects_appropriately
  - test_strided_is_faster_than_full (benchmark)
  - test_progressive_eliminates_most_buckets
```

---

## Questions for Wyatt

1. **Lazy vs eager NVT rebuild** — dirty flag + rebuild on query, or rebuild on every insert?
2. **NVTMask as separate struct** — keep it separate from NVT (query artifact vs persistent structure)?
3. **Exact query bucket width** — tight (one bucket) or fuzzy (±neighbors)?
4. **Backward compatible JSON** — flat array `"where": [...]` = sugar for `"where": { "and": [...] }`?
5. **Auto strategy thresholds** — 10K/100K/1M reasonable?
6. **Ready to build?**

---
---

## Round 2: Refined Design After Discussion

### Key Insight: Two Tiers of Query Execution

**Tier 1 — Direct scalar lookups (simple queries, no compositing needed):**

For `WHERE age > 30 AND name = 'Bob'`:
- `converter.to_scalar(30)` → jump straight to that NVT offset. Everything after is the age range. One computation.
- `converter.to_scalar("Bob")` → jump to that bucket. One computation.
- The intersection is trivially computed from the two scalar positions — no bitmap needed.

Most queries live here. It's O(1) per field. No bitmaps. No compositing. Just math.

**Tier 2 — Bitmap compositing (complex queries):**

Needed when:
- **OR queries** — union of non-overlapping regions
- **NOT queries** — inversion
- **IN queries with dynamic sets** — `color IN (subquery results)`
- **Cross-path joins** — `Paints.where.color.IN(Palettes.where.primaryColor)`

This is where NVTMask compositing earns its keep.

### Multiple NVTs Per Field (Not Multiple Resolutions)

A field might have MULTIPLE NVTs optimized for different operation types:

```
/myapp/users/.indexes/age/
  eq.nvt        ← optimized for equality lookups (tight buckets around common values)
  range.nvt     ← optimized for gt/lt/between (uniform distribution across full range)
```

Different NVTs for the same data, with different bucket distributions. The query engine picks the right NVT based on the operation. The skipping/striding feature gives us resolution scaling for free — no separate resolution NVTs needed.

<!-- 
I wonder if this is really needed ... if we are mapping values, and we inherantly know if a value is greater or lesser, then do we really need different NVTs for each index? Or is a single one enough for most operations?

I know that we want multiple indexes in some cases though. For example, we a string `content` column, we might want a fuzzy index, a phonetic index, etc...
 -->

### NVT Concurrent Access: Reader Reports + Coordinator Swap

```
Readers (concurrent):
  1. Use the current NVT (immutable reference)
  2. Perform their lookup
  3. If they had to scan far from the expected offset → report correction
     Report = (scalar: 0.4543, correct_offset: 12345623456)
  4. Reports pushed onto a lock-free stack

Coordinator (single thread, periodic):
  1. Drain the correction stack
  2. Aggregate corrections (average/mean for conflicting updates)
  3. Generate a new NVT incorporating corrections
  4. Atomic swap: replace the old NVT pointer with the new one
  5. Old NVT is dropped when all readers release their references (Arc)
```

Double-buffering. Readers never block. The NVT heals over time through normal access patterns. The coordinator is a background task, not a per-query cost.

### Typed Convenience Methods (No More Bytes)

```rust
// Before (ugly):
.field("age").gt(&30_u64.to_be_bytes())

// After (clean):
.field("age").gt_u64(30)
.field("name").eq_str("Bob")
.field("score").gt_f64(95.5)
.field("active").eq_bool(true)
.field("created").gt_timestamp(1711234567000)
```

Conversion to bytes happens inside. The user never sees `to_be_bytes()`.

### Resolved Questions from Round 1

| # | Question | Resolution |
|---|---|---|
| 1 | Lazy vs eager NVT rebuild | **Reader-corrected + coordinator swap.** NVT heals through normal reads. Coordinator aggregates corrections and swaps periodically. No explicit rebuild. |
| 2 | NVTMask separate or integrated | **Separate.** Mask is a query-time artifact. NVT is persistent. Keep separate. GPU-compatible packed u64 bitset. |
| 3 | Exact query bucket width | **Direct scalar jump.** `f(value)` → bucket. Search from there. The NVT self-corrects on access. No fuzz needed. |
| 4 | Backward compatible JSON | **Yes.** Flat array = AND sugar. |
| 5 | Auto strategy thresholds | **Agreed.** 10K/100K/1M. Tune via stress testing. |
| 6 | Ready to build? | **Almost.** Need to discuss IN/join queries and wildcard first. |

---

## Round 2: IN Queries, Joins, and Wildcards

### IN Queries with Static Set

```
WHERE color IN ('red', 'blue', 'green')
```

**Small set (< 1000 values):** Compute scalar for each value. Mark those NVT buckets. Create a sparse mask. AND with other field masks if needed. Fast.

**Large set (> 1000 values):** Same approach but the mask becomes denser. Still O(set_size) to build the mask, O(bucket_count) to AND. Acceptable.

### IN Queries with Subquery (Cross-Path Joins)

```
Paints.where.color.IN(CustomerPalettes.where.primaryColor)
```

**The subquery produces a dynamic set of values.** Two approaches:

**Approach A — Materialized set:**
1. Execute the subquery: load all `primaryColor` values from `CustomerPalettes`
2. For each value: compute scalar, mark NVT bucket
3. Result: a mask of which buckets have matching colors
4. AND with the outer query's mask
5. Scan surviving buckets for actual matches

**Approach B — Streaming mask construction:**
1. Stream subquery results (don't load all into memory)
2. For each streamed value: compute scalar, set bit in mask
3. When stream is exhausted: mask is complete
4. AND with outer query

Approach B is better for large subquery results — bounded memory (just the mask, not the full value set).

**Approach C — NVT-to-NVT compositing:**
If both `Paints.color` and `Palettes.primaryColor` have NVTs:
1. Get the `primaryColor` NVT (already exists as an index)
2. Composite directly: AND the two NVTs
3. No value materialization at all

This only works if both fields use the SAME converter (same scalar mapping). If they do — the NVTs are directly comparable. If they don't (different types, different ranges) — fall back to Approach A or B.

<!-- WYATT: Approach C is the dream — pure NVT compositing for joins. But it requires same-converter compatibility. For different types (e.g., joining an integer color code to a string color name), you need value-level comparison. Thoughts? -->

<!-- 
My thoughts are that they likely _will_ be the same (or similar) types. Think about it: you are going to compare strings to strings, numbers to numbers, and bytes to bytes. More often than not, we will probably have the same type.

Also, I've been thinking about it, and we probably don't need as many specific NVTs as we think we do... especially if we are smart about things. This reduces the likelihood that we will have NVT comparison issues.
 -->

### Wildcard / Pattern Matching

`WHERE name LIKE '%smith%'`

The NVT CAN'T help with mid-string matching. The scalar converter maps the BEGINNING of the string (first byte + length). "Smith" and "Blacksmith" map to completely different scalars. The `%` prefix kills NVT-based lookup.

**Options:**

**A) Full scan with NVT pre-filter:**
If the query also has an NVT-friendly clause (`WHERE age > 30 AND name LIKE '%smith%'`), use the NVT for `age > 30` to narrow candidates, THEN full-scan those candidates for the wildcard match. The NVT reduces the scan set, even if it can't handle the wildcard directly.

**B) Trigram index (future):**
Split strings into 3-character grams: "smith" → ["smi", "mit", "ith"]. Index each trigram. `%smith%` → find entries containing all three trigrams → post-filter for exact match. This is how PostgreSQL's `pg_trgm` works.

**C) Inverted index / full-text (future):**
Tokenize strings into words. Index each word. Good for `CONTAINS 'smith'` but not arbitrary `LIKE` patterns.

<!-- 
Trigrams are good for "contains" too.
 -->

**For now:** Option A (NVT pre-filter + full scan on candidates). Options B and C are future index types that plug into the same ScalarConverter + NVT architecture — just with different converters.

<!-- WYATT: Trigram indexing is a great future addition. The converter would be a `TrigramConverter` that maps trigrams to scalars. Each string produces MULTIPLE index entries (one per trigram). The NVT then supports containment queries natively. -->

<!-- 
I think it would be an interesting idea to try and understand _how_ we would do this, being as we have a scalar indexing system.

I wonder if we could assume a tri-gram is assign to any given chunk: for example, ann -> maps to 0.12342, zebra -> maps to 0.99956 something. Then we repeat chunks often with a weight, a "distance from start" for each matching letter. I bet we could do some interesting things with this.
 -->

---

## Revised Task List

```
Task 1: FieldIndex backed by NVT (with reader-correction + coordinator swap)
Task 2: NVTMask bitmap operations (packed u64 bitset, GPU-compatible)
Task 3: Range masks from converters (direct scalar jumps, not scans)
Task 4: Typed convenience methods on QueryBuilder (gt_u64, eq_str, etc.)
Task 5: QueryNode tree with boolean logic (AND, OR, NOT)
Task 6: Two-tier query execution (Tier 1: direct scalars, Tier 2: bitmap compositing)
Task 7: IN queries (static set + subquery/streaming)
Task 8: HTTP query API with boolean logic + backward-compatible sugar
Task 9: Strided / progressive execution
Task 10: Update index serialization (NVT included)
```

---

## Questions for Wyatt

1. **Multiple NVTs per field** — is eq.nvt + range.nvt the right split, or something different?
2. **Cross-path joins with NVT compositing (Approach C)** — worth pursuing now, or defer? Requires same-converter compatibility.
3. **Wildcard handling** — NVT pre-filter + full scan for now, trigram index as future?
4. **Coordinator swap frequency** — every N queries? Every N seconds? On-demand when correction stack exceeds a threshold?
5. **Ready to build?**

---
---

## Round 3: Memory-Bounded Joins + Resolved Questions

### Resolved from Round 2

| # | Question | Resolution |
|---|---|---|
| 1 | Multiple NVTs per field | **One NVT per field for most cases.** A single NVT handles eq, gt, lt, between naturally via scalar position. Multiple NVTs only for genuinely different index TYPES on the same data (e.g., string equality + fuzzy + phonetic on a `content` column). |
| 2 | Cross-path joins | **Viable — same types are likely.** Strings compare to strings, numbers to numbers. NVT-to-NVT compositing works for most real-world joins. |
| 3 | Wildcard handling | **NVT pre-filter + scan for now. Trigram index future.** Trigrams can be scalar-mapped (each trigram → scalar, weighted by position). Keeps the NVT paradigm intact. |
| 4 | Coordinator swap frequency | **Threshold-based.** When the correction stack exceeds N entries (e.g., 1000), trigger a swap. Not time-based — activity-driven. |

### The Memory Problem with Joins

Wyatt's concern: "We can't load hundreds of megabytes of indexes into memory to compare."

The key question for any join: `does value V exist in set S?` — an IN operation. For GT/LT, it's `is V on the right/left side?` For NOT, it's the inversion. All reduce to membership testing.

### The Answer: Masks Are Fixed Size

The NVT mask is **one bit per bucket**. The mask size is determined by bucket count, NOT by data size:

```
1,024 buckets  →   128 bytes mask
16,384 buckets  →  2 KB mask
1,048,576 buckets → 128 KB mask
16,777,216 buckets → 2 MB mask
```

A 2 MB mask represents 16 million buckets. Whether each bucket contains 1 entry or 1 million entries, the mask is still 2 MB. The mask says "this region has matches" — you never load actual values until you scan the surviving buckets.

### Memory-Bounded Join Flow

```
Join: Paints.where.color.IN(Palettes.where.primaryColor)

Step 1: Build the subquery mask (streaming, fixed memory)
  - Walk Palettes.primaryColor NVT bucket by bucket
  - For each non-empty bucket: set the corresponding bit in a mask
  - Memory: one NVTMask (2 MB max regardless of data size)
  - Time: O(bucket_count), NOT O(entry_count)

Step 2: Composite masks (nanoseconds)
  - AND the subquery mask with the outer query's mask
  - Pure bitwise operations on the packed u64 arrays
  - Memory: another NVTMask (2 MB)

Step 3: Scan survivors (proportional to results, not total data)
  - Only the surviving buckets are scanned
  - For each surviving bucket: load entries, verify actual value match
  - Memory: proportional to result set size, bounded by limit
```

**Total memory for a multi-table join:** `N masks × 2 MB each`. For a 5-table join: 10 MB. Fixed. Regardless of whether each table has 100 rows or 100 billion.

### Multi-Index Compositing (3+ Fields)

```
WHERE age > 30 AND city = 'NYC' AND NOT role = 'banned'

Step 1: age_mask = mask from age NVT (everything > f(30))     — 2 MB
Step 2: city_mask = mask from city NVT (bucket for f('NYC'))  — 2 MB
Step 3: role_mask = mask from role NVT (bucket for f('banned')) — 2 MB
Step 4: result = age_mask AND city_mask AND NOT(role_mask)     — bitwise, nanoseconds
Step 5: scan surviving buckets                                 — proportional to results
```

Memory: 3 masks × 2 MB = 6 MB. Constant. The data can be petabytes.

### Streaming NVT Walk for Subqueries

For subqueries that produce dynamic value sets, we don't even need to walk the entire NVT. We stream:

```rust
fn build_mask_from_subquery(
  subquery_nvt: &NVT,
  target_nvt: &NVT,
  converter: &dyn ScalarConverter,
) -> NVTMask {
  let mut mask = NVTMask::empty(target_nvt.bucket_count());

  // Stream through the subquery NVT bucket by bucket
  for bucket_index in 0..subquery_nvt.bucket_count() {
    if subquery_nvt.buckets[bucket_index].entry_count == 0 {
      continue; // skip empty buckets
    }

    // This bucket has entries — mark the corresponding region in the target mask
    // (bucket indices map to scalar ranges, which map to target NVT buckets)
    let scalar_start = bucket_index as f64 / subquery_nvt.bucket_count() as f64;
    let scalar_end = (bucket_index + 1) as f64 / subquery_nvt.bucket_count() as f64;

    let target_start = (scalar_start * target_nvt.bucket_count() as f64) as usize;
    let target_end = (scalar_end * target_nvt.bucket_count() as f64) as usize;

    for i in target_start..=target_end.min(target_nvt.bucket_count() - 1) {
      mask.set_bit(i);
    }
  }

  mask
}
```

No values loaded. No entries scanned. Just NVT structure → mask. O(bucket_count).

### Trigram Indexing via Scalars (Conceptual)

Each trigram maps to a scalar:
```
"smith" → trigrams: ["smi", "mit", "ith"]
  smi → f("smi") → 0.723
  mit → f("mit") → 0.534
  ith → f("ith") → 0.389
```

Each trigram becomes an index entry. A string with 10 characters produces 8 trigram entries. The NVT indexes ALL trigrams for ALL strings in the field.

`WHERE name CONTAINS 'smith'`:
1. Compute trigrams for 'smith': [smi, mit, ith]
2. For each trigram: find the NVT bucket → mask
3. AND all trigram masks → candidates contain ALL trigrams
4. Post-filter candidates for actual substring match

This keeps the scalar paradigm. The converter maps trigrams to scalars. The NVT handles the rest.

Weight by position (Wyatt's idea): trigrams near the start of the string get scalars closer to the string's overall scalar. Trigrams later in the string are spread further. This means prefix searches are tighter (fewer buckets) and suffix searches are wider (more scanning). Natural optimization for the common case.

---

## Final Revised Task List

```
Task 1:  FieldIndex backed by NVT + reader-correction + coordinator swap
Task 2:  NVTMask bitmap operations (packed u64, GPU-compatible)
Task 3:  Direct scalar jumps for simple queries (Tier 1)
Task 4:  Typed convenience methods (gt_u64, eq_str, etc.)
Task 5:  QueryNode tree with boolean logic (AND, OR, NOT)
Task 6:  Two-tier query execution engine
Task 7:  Memory-bounded joins (streaming mask construction)
Task 8:  HTTP query API with boolean logic + sugar
Task 9:  Strided / progressive execution
Task 10: Index serialization with NVT
```

Trigram indexing, full-text, and GPU offloading → future tasks.

---

## Questions for Wyatt

1. **Coordinator swap threshold** — 1000 corrections before swap? Higher? Lower?
2. **Trigram indexing** — capture as future task, or think deeper now?
3. **Ready to build?**

---

*Waiting for Wyatt's feedback on Round 3...*