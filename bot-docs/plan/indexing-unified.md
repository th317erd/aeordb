# Unified Indexing — ScalarConverter + NVT

**Parent:** [Master Plan](./master-plan.md)
**Status:** Designed, test-planned, ready for implementation
**Supersedes:** [indexing-engine.md](./indexing-engine.md) (Sprint 1 prototype)

---

## Core Principle

One trait. One lookup structure. Infinite index types.

The `ScalarConverter` converts any value to [0.0, 1.0]. The NVT uses that scalar for bucket-based lookup. The converter handles distribution normalization. The NVT stays uniform.

---

## ScalarConverter Trait

```rust
pub trait ScalarConverter: Send + Sync {
  /// Convert raw bytes to a scalar in [0.0, 1.0].
  fn to_scalar(&self, value: &[u8]) -> f64;

  /// Is this converter order-preserving?
  /// Required for range queries (gt, lt, between).
  fn is_order_preserving(&self) -> bool;

  /// Human-readable name.
  fn name(&self) -> &str;
}
```

### Built-in Converters

| Converter | Order-Preserving | How It Works |
|---|---|---|
| `HashConverter` | No | First 8 bytes as u64 / u64::MAX. Uniform distribution. For KVS hash lookups. |
| `U8Converter` | Yes | value / 255.0. Tracks observed min/max. |
| `U16Converter` | Yes | (value - min) / (max - min). Tracks observed min/max. |
| `U32Converter` | Yes | Same pattern. |
| `U64Converter` | Yes | Same pattern. |
| `I64Converter` | Yes | Shifted to unsigned range, then normalized. |
| `F64Converter` | Yes | Normalized within configurable/observed min/max. Clamps outliers. |
| `StringConverter` | Partially | Multi-stage: first byte weighted + length. Rough lexicographic order. |
| `TimestampConverter` | Yes | UTC milliseconds normalized within observed range. |
| `WasmConverter` | User-defined | Custom WASM plugin. Batch API: N values in, N scalars out. |

### Range Tracking (Self-Adapting Distribution)

Numeric converters track `observed_min` and `observed_max` as data is indexed:

```rust
struct U64Converter {
  observed_min: u64,   // smallest value ever indexed
  observed_max: u64,   // largest value ever indexed
}

impl ScalarConverter for U64Converter {
  fn to_scalar(&self, value: &[u8]) -> f64 {
    let v = u64::from_be_bytes(value.try_into().unwrap());
    if self.observed_min == self.observed_max {
      return 0.5; // all same values — center of range
    }
    (v.saturating_sub(self.observed_min)) as f64
      / (self.observed_max - self.observed_min) as f64
  }
}
```

When the range expands (new min or max observed):
- Existing scalars are approximately correct (in the right neighborhood)
- The "always good, not always perfect" property — lookups still work, just scan a bit more
- Self-correcting on access: when a value is found, its exact position is updated

Type authors with domain knowledge can bake in better distributions (e.g., an `AgeConverter` that knows ages cluster 20-50 and spreads that range more evenly).

---

## Unified NVT

The NVT is the same structure for both the engine KVS and user indexes. The only difference is the converter:

```rust
struct NVT {
  converter: Box<dyn ScalarConverter>,
  buckets: Vec<NVTBucket>,
  version: u8,
}
```

- **For KVS:** `NVT::new(HashConverter, 1024)` — hash lookups into the KV block
- **For user age index:** `NVT::new(U64Converter::new(), 1024)` — age lookups
- **For user name index:** `NVT::new(StringConverter::new(), 1024)` — string lookups

Same bucket structure. Same lookup algorithm. Same resize logic. Different converter.

---

## Range Queries

Order-preserving converters enable range queries:

```
"Find all ages > 30":
  1. to_scalar(30) → 0.42
  2. All buckets with scalar > 0.42 contain candidates
  3. Scan those buckets, filter exact matches
```

Non-order-preserving converters (HashConverter) refuse range queries:

```
"Find all hashes > X":
  → Error: "Index 'hash_index' uses HashConverter which does not support range queries"
```

The `is_order_preserving()` method enables the engine to check at query time.

---

## Index Storage

Indexes are files at `.indexes/` under each path. Everything is a file.

```
/myapp/users/
  .indexes/
    email_string.idx     ← FileRecord: NVT + sorted entries for email field
    age_u64.idx          ← FileRecord: NVT + sorted entries for age field
    name_fuzzy.idx       ← FileRecord: NVT + sorted entries for name field
  alice.json
  bob.json
```

Each index file contains:
- Converter state (type + observed min/max + config)
- NVT buckets
- Sorted index entries (scalar → file path hash)

Updated via append-only: new version of the index file written on change. Old versions preserved for snapshots.

---

## Memory Strategy

| Index | In Memory | Strategy |
|---|---|---|
| KVS NVT (HashConverter) | Always | Critical path — every operation uses it |
| User index NVTs | On-demand | Loaded when queried, LRU cached, evicted under memory pressure |

The KVS is always hot. User indexes are warm on access, cold otherwise.

---

## Write Pipeline

When a file is stored at a path with configured parsers and indexes:

```
1. Store chunks                          ← data is safe
2. Store FileRecord                      ← file is safe
3. Update directory tree                 ← filesystem is consistent
4. Run parser plugins → extract fields   ← fields extracted from raw bytes
5. For each indexed field:
   a. Converter: field value → scalar
   b. Update NVT + sorted entries
   c. Write new index file version       ← index updated
```

Steps 1-3 are durable (fsync'd). Steps 4-5 are derived (rebuildable if lost).

If crash between step 3 and step 5: file exists, index is stale. Rebuilding: re-run parsers on all files at the path, reconstruct index.

---

## Query Pipeline

```
1. Query request arrives (exact, range, fuzzy, etc.)
2. Identify target path + field + operation
3. Load index NVT for that field (cache hit or read from .indexes/)
4. Converter: query value → scalar
5. NVT: scalar → bucket → scan sorted entries → candidate file hashes
6. For multi-field queries: intersect/union candidate sets
7. Load matching FileRecords
8. Apply any post-filters
9. Return results (all / first / cursor / count)
```

---

## WASM Converter Batch API

For custom WASM converters, batch to amortize host↔WASM boundary cost:

```rust
trait WasmBatchConverter {
  /// Convert N values to N scalars in one WASM call.
  fn to_scalars_batch(&self, values: &[&[u8]]) -> Vec<f64>;
}
```

One boundary crossing for 1000 values instead of 1000 crossings.

---

## NVT Bitmap Compositing — The Query Execution Engine

The NVT is not just a lookup table. It IS the query execution engine. Each NVT is a bitmap — buckets are pixels, populated buckets are "on." Complex queries are bitmap compositing operations.

### Logical Operations on NVTs

```
NVT_age (age > 30):
  [____████████████████]    ← buckets with entries for age > 30

NVT_name (name = "Bob"):
  [█__█____█___________]    ← buckets with entries for name = "Bob"

AND (intersection):  [________█___________]  ← on in BOTH
OR  (union):         [█__█████████████████]  ← on in EITHER
NOT (difference):    [____████_███████████]  ← first minus second
XOR (symmetric diff):[█__█████_███████████]  ← on in one but not both
```

The operation is **O(bucket_count)**, not O(entries). With 1024 buckets, it's 1024 comparisons regardless of whether you have 10 entries or 10 billion.

The result is a NEW composited mask — the "on" buckets tell you exactly where to scan for actual entries.

### Strided Access (Zero-Cost Resolution Scaling)

Don't downsample. Don't copy. Just change the stride.

```
Full resolution:     bucket[0], bucket[1], bucket[2], bucket[3], ...
Stride 2 (half):     bucket[0],            bucket[2],            ...
Stride 4 (quarter):  bucket[0],                        bucket[4], ...
Stride 64 (1/64th):  bucket[0], ..., bucket[64], ..., bucket[128], ...
```

Zero allocation. Zero copying. One loop with a step size.

```rust
fn composite_and(nvt_a: &NVT, nvt_b: &NVT, stride: usize) -> Vec<usize> {
  let mut surviving = Vec::new();
  let mut i = 0;
  while i < nvt_a.bucket_count() {
    if nvt_a.buckets[i].entry_count > 0 && nvt_b.buckets[i].entry_count > 0 {
      surviving.push(i);
    }
    i += stride;
  }
  surviving
}
```

### Progressive Refinement

Like progressive JPEG — start rough, refine only where needed:

```
Pass 1 (stride 64):  16M buckets checked in 250K comparisons → 50 regions survive
Pass 2 (stride 1):   scan those 50 regions × 64 buckets = 3,200 comparisons
Total:               253,200 comparisons instead of 16,000,000

95%+ of the index space eliminated in the first pass.
```

### Complex Query Composition

```
result = (NVT_age_gt_30 AND NVT_city_eq_NYC) OR NVT_role_eq_admin

Step 1: AND(age, city) → intermediate mask
Step 2: OR(intermediate, role) → final mask
Step 3: Scan entries in surviving buckets only
```

Three bitmap operations. Microseconds. Then only scan the handful of surviving bucket regions.

### Cross-Resolution Compositing

If NVT_age has 1024 buckets and NVT_name has 4096 buckets:
- Each bucket in NVT_age maps to 4 buckets in NVT_name
- Composite at the lower resolution (1024), then refine at the higher resolution for surviving regions
- OR: use stride 4 on NVT_name to match NVT_age's resolution

No resampling needed — just adjust the stride.

### GPU Offloading (Future)

NVT compositing IS image processing. GPUs are built for exactly this:

```
CPU: 16M bucket composite = 16M sequential comparisons
GPU: 16M bucket composite = one compute shader kernel, massively parallel, microseconds
```

Upload two NVT arrays to GPU memory. Run AND/OR/NOT shader. Download result mask. Database queries at framerate speeds — real-time analytics on billions of records.

---

## Multi-Dimensional Queries (Future)

For 2D+ data (geospatial): one NVT per dimension. Query engine intersects results.

```
"Find all points within 5km of (lat, lon)":
  1. LatConverter: lat range → scalar range → NVT bucket range → candidate set A
  2. LonConverter: lon range → scalar range → NVT bucket range → candidate set B
  3. Intersect A ∩ B → candidates
  4. Post-filter: exact distance check on candidates
```

Purpose-built spatial index types (R-tree, quad-tree) deferred to future.

---

## Implementation Tasks

```
Task 1: ScalarConverter trait + built-in converters (unit tests)
Task 2: Refactor NVT to use ScalarConverter (replace hardcoded hash_to_scalar)
Task 3: Remove old src/indexing/ module (replaced by unified design)
Task 4: Index file storage (.indexes/ as FileRecords)
Task 5: Write pipeline integration (store → parse → index)
Task 6: Query pipeline (query → index → results)
Task 7: Wire to HTTP query endpoints
Task 8: WASM converter support + batch API
```

---

## Test Plan

Six priority levels, ~60 tests. See `.claude/conversation.md` for detailed test list.

| Priority | Category | Tests |
|---|---|---|
| P1 | Converter correctness | ~20 (unit tests, edge cases) |
| P2 | NVT + converter integration | ~9 |
| P3 | Index lifecycle | ~14 |
| P4 | Write pipeline | ~8 |
| P5 | Query pipeline | ~9 |
| P6 | Performance benchmarks | ~4 |
