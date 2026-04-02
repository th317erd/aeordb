# Unified Indexing — ScalarConverter + NVT + Bitmap Compositing

**Parent:** [Master Plan](./master-plan.md)
**Status:** Designed, test-planned, ready for implementation
**Supersedes:** [indexing-engine.md](./indexing-engine.md) (Sprint 1 prototype)

---

## Core Principle

One trait (`ScalarConverter`). One lookup structure (NVT). Bitmap compositing for complex queries. Two execution tiers. Memory bounded at all scales.

---

## ScalarConverter Trait

```rust
pub trait ScalarConverter: Send + Sync {
  fn to_scalar(&self, value: &[u8]) -> f64;    // always [0.0, 1.0]
  fn is_order_preserving(&self) -> bool;
  fn name(&self) -> &str;
}
```

### Built-in Converters

| Converter | Order-Preserving | Notes |
|---|---|---|
| `HashConverter` | No | For KVS hash lookups. Uniform distribution. |
| `U8/U16/U32/U64Converter` | Yes | Range-tracking (observed min/max). |
| `I64Converter` | Yes | Signed, shifted to [0.0, 1.0]. |
| `F64Converter` | Yes | Clamping, NaN/Inf handling. |
| `StringConverter` | Partially | Multi-stage: first byte weighted + length. |
| `TimestampConverter` | Yes | UTC milliseconds, range-tracking. |
| `WasmConverter` | User-defined | Batch API: N values → N scalars. |

### Range Tracking (Self-Adapting Distribution)

Numeric converters track `observed_min` / `observed_max`. The converter MAKES the distribution uniform — NVT buckets stay uniform width.

```
First value: 100. Range 100-100. scalar = 0.5.
Value 500: Range expands to 100-500. Scalars redistribute.
Value 25: Range expands to 25-500. Existing scalars are approximately correct.
Self-correcting: exact positions fixed on next access.
```

If `observed_min == observed_max`: return 0.5 (avoid division by zero).

---

## NVT (Normalized Vector Table)

One NVT per field index. Bucket-based lookup into sorted entries.

```rust
struct NormalizedVectorTable {
  converter: Box<dyn ScalarConverter>,
  buckets: Vec<NVTBucket>,
}
```

### Concurrent Access: Reader-Correction + Coordinator Swap

```
Readers (concurrent):
  1. Use current NVT (immutable Arc reference)
  2. Perform lookup
  3. If scan was far from expected → push correction to lock-free stack
     Correction = (scalar, correct_offset)

Coordinator (background, threshold-triggered):
  1. Drain correction stack (when > N corrections accumulated)
  2. Aggregate corrections
  3. Generate new NVT incorporating corrections
  4. Atomic swap: replace Arc pointer
  5. Old NVT dropped when all readers release their Arc
```

Double-buffering. Readers never block. NVT heals through normal access.

### One NVT Per Field

A single NVT handles eq, gt, lt, between — the scalar mapping inherently preserves order. Multiple NVTs per field only for genuinely different index TYPES (e.g., string equality + fuzzy + phonetic on the same column).

---

## Two-Tier Query Execution

### Tier 1: Direct Scalar Lookups (Simple Queries)

For `WHERE age > 30 AND name = 'Bob'`:
- `converter.to_scalar(30)` → jump to NVT offset. Everything after = candidates. O(1).
- `converter.to_scalar("Bob")` → jump to bucket. One comparison. O(1).
- Intersection: trivially computed from scalar positions.

No bitmaps. No compositing. Just math. Handles most queries.

### Tier 2: NVT Bitmap Compositing (Complex Queries)

For OR, NOT, IN, joins — build NVTMasks and composite.

---

## NVT Bitmap Compositing

### NVTMask

```rust
struct NVTMask {
  bucket_count: usize,
  bits: Vec<u64>,  // packed bitset, 64 buckets per u64
}
```

Each bucket = one bit. "On" = candidates in this region. "Off" = eliminated.

### Fixed Memory Regardless of Data Size

```
1,024 buckets   →   128 bytes
1,048,576 buckets → 128 KB
16,777,216 buckets → 2 MB
```

A 5-table join = 5 masks × 2 MB = 10 MB total. Whether each table has 100 rows or 100 billion.

### Logical Operations

```
AND:        result.bits[i] = a.bits[i] & b.bits[i]
OR:         result.bits[i] = a.bits[i] | b.bits[i]
NOT:        result.bits[i] = !a.bits[i]
XOR:        result.bits[i] = a.bits[i] ^ b.bits[i]
DIFFERENCE: result.bits[i] = a.bits[i] & !b.bits[i]
```

O(bucket_count / 64) operations. Nanoseconds for 1024 buckets.

### Strided Access (Zero-Cost Resolution Scaling)

Don't copy. Don't resize. Change the step size.

```
Full:       bucket[0], bucket[1], bucket[2], ...
Stride 2:   bucket[0],            bucket[2], ...
Stride 64:  bucket[0], ..., bucket[64], ...
```

### Progressive Refinement

```
Pass 1 (stride 64):  250K comparisons → 50 regions survive (95%+ eliminated)
Pass 2 (stride 1):   50 × 64 = 3,200 comparisons on survivors
Total:               253K instead of 16M
```

### Range Masks

```
age > 30:
  scalar = converter.to_scalar(30) → 0.42
  All buckets after 0.42 → bits set. Direct jump, not scan.

name = 'Bob':
  scalar = converter.to_scalar("Bob") → 0.67
  Single bucket at 0.67 → one bit set.
```

---

## Boolean Query Logic

### QueryNode Tree

```rust
enum QueryNode {
  Field(FieldQuery),          // leaf: single field operation
  And(Vec<QueryNode>),        // all children must match
  Or(Vec<QueryNode>),         // any child matches
  Not(Box<QueryNode>),        // invert child
}
```

Execution walks bottom-up:
1. Leaf → NVTMask from field's NVT + operation
2. AND → bitwise AND child masks
3. OR → bitwise OR child masks
4. NOT → bitwise NOT child mask
5. Final mask → scan surviving buckets → load results

### Rust Query Builder

```rust
QueryBuilder::new(&engine, "/users/")
  .field("age").gt_u64(30)
  .field("city").eq_str("NYC")
  .not(|q| q.field("role").eq_str("banned"))
  .limit(100)
  .all()
```

Typed convenience methods — no `.to_be_bytes()`.

### JSON Query API

```json
{
  "path": "/users/",
  "where": {
    "and": [
      { "field": "age", "op": "gt", "value": 30 },
      { "field": "city", "op": "eq", "value": "NYC" },
      { "not": { "field": "role", "op": "eq", "value": "banned" } }
    ]
  },
  "limit": 100
}
```

Backward compatible: flat array `"where": [...]` = sugar for `"where": { "and": [...] }`.

---

## Memory-Bounded Joins

### IN Queries with Static Set

```
WHERE color IN ('red', 'blue', 'green')
→ Compute scalar for each value, set bits in mask, AND with query mask.
```

### Cross-Path Joins (Streaming Mask Construction)

```
Paints.where.color.IN(Palettes.where.primaryColor)

1. Walk Palettes.primaryColor NVT bucket by bucket (streaming)
2. Non-empty bucket → set corresponding bit in target mask
3. Mask complete: 2 MB regardless of subquery result size
4. AND with Paints.color mask
5. Scan surviving buckets for actual value matches
```

O(buckets), not O(entries). Memory = mask size, not data size.

### NVT-to-NVT Compositing

If both fields use compatible converters (same type, same scalar mapping):
- Composite the NVTs directly — no value materialization
- Works for most real-world joins (strings-to-strings, numbers-to-numbers)

---

## Index Storage

Indexes are files at `.indexes/{field_name}.idx` under each path. Each index file contains:

```
[Converter state (type tag + observed min/max + config)]
[NVT serialization (version + buckets)]
[Sorted entries (scalar: f64, file_hash: [u8; N])]
```

Updated via append-only. Old versions preserved for snapshots.

### Memory Strategy

| Index | In Memory | Strategy |
|---|---|---|
| KVS NVT | Always | Critical path — every operation |
| User index NVTs | On-demand | LRU cached, evicted under memory pressure |

---

## Write Pipeline

```
1. Store chunks + FileRecord + directory tree         ← data safe (fsync)
2. Parse fields via parser plugins                     ← extract values
3. For each indexed field:
   a. converter.to_scalar(value) → scalar
   b. Insert into FieldIndex sorted entries
   c. NVT heals via reader-correction (no immediate rebuild)
4. Save updated index file                             ← index updated
```

Crash between step 1 and step 4: data exists, index is stale. Rebuild by re-running parsers. Index lag is acceptable; data loss is not.

---

## Query Strategy

```rust
enum QueryStrategy {
  Full,                        // scan all buckets
  Strided(usize),              // skip every N buckets
  Progressive { stride, threshold },  // rough pass + refine
  Auto,                        // engine picks based on index sizes
}
```

Auto thresholds:
- < 10K entries: Full
- \> 100K entries: Progressive stride 64
- \> 1M entries: Progressive stride 256

---

## Implementation Tasks

```
Task 1:  FieldIndex backed by NVT + reader-correction + coordinator swap
Task 2:  NVTMask bitmap operations (packed u64, GPU-compatible)
Task 3:  Direct scalar jumps for simple queries (Tier 1)
Task 4:  Typed convenience methods (gt_u64, eq_str, etc.)
Task 5:  QueryNode tree with boolean logic (AND, OR, NOT)
Task 6:  Two-tier query execution engine
Task 7:  Memory-bounded joins (streaming mask construction)
Task 8:  HTTP query API with boolean logic + backward-compatible sugar
Task 9:  Strided / progressive execution
Task 10: Index serialization with NVT
```

---

## Test Plan

~30 tests across 3 spec files:

**nvt_ops_spec.rs:** mask construction, AND/OR/NOT/XOR, popcount, surviving buckets, strided, progressive, cross-resolution, empty/full masks

**query_boolean_spec.rs:** AND/OR/NOT queries, nested boolean, QueryNode tree execution, JSON formats, backward compatibility

**query_strategy_spec.rs:** strided correctness, progressive correctness, auto strategy selection, performance benchmarks
