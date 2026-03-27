# Indexing Engine — Scalar Ratio Indexing

**Parent:** [Master Plan](./master-plan.md)
**Status:** In Design

---

## Core Concept: Unit Scalar Indexing

Every indexable value, regardless of type, is normalized to a scalar in the range `[0.0, 1.0]` via a mapping function `f(x)`. This scalar represents a *ratio* — a position within the known domain of that type. The scalar maps into an ordered offset table that points to data locations.

```
f(value) → [0.0, 1.0] → position in offset table → data location on storage
```

### The Raster Analogy

The offset table scales like a raster image. More data means more resolution (more "pixels" in the offset table). Less data means you can downsample. The structure is resolution-independent — it works at any scale, just with varying precision.

### Thesis

**Always good, even if not always perfect.** The index is always usable, always self-improving, and never requires manual rebuilding or human intervention.

---

## How It Works

### Basic Types (int, char, etc.)

All fixed-size types have known ranges. The mapping function is a simple normalization:

```
f(x) = (x - type_min) / (type_max - type_min)
```

- A `u8` value of 128 → `f(128) = 128 / 255 ≈ 0.502`
- A `u16` value of 32768 → `f(32768) = 32768 / 65535 ≈ 0.500`
- A `char` value of 'M' (77) → `f(77) = 77 / 255 ≈ 0.302`

### Negative Values

Negative values are indexed in a separate branch, growing toward `-Infinity`. This prevents signed types from polluting the positive index space and allows both positive and negative ranges to grow independently.

### Strings (Multi-Stage Decomposition)

Strings are variable-length with an effectively infinite domain. They are decomposed into a multi-dimensional vector of scalars:

1. **Stage 1:** Position between first and last values in the current dataset → scalar
2. **Stage 2:** Normalized length → scalar
3. **Stage 3:** Character sampling (e.g., every odd character) → hashed into scalar

This creates a binary tree of scalar fields — each level narrows the search space. Conceptually a multi-dimensional vector, structurally a tree of `[0.0, 1.0]` ranges.

### Precision Overflow → Dimensional Growth

When a single scalar runs out of resolution (e.g., a `u64` has more distinct values than a `f64` can represent), the index adds dimensions. The scalar becomes a vector of scalars, and tree depth grows as needed. Collisions become structurally impossible given sufficient dimensions.

---

## Offset Table and Block Structure

### Block-Based Organization

The offset table is organized into blocks on the storage engine. Blocks are analogous to "pixels" in the raster analogy. Data is distributed among peer blocks based on the indexing algorithm's calculations.

### Self-Correcting Reads (Lazy Healing)

When the offset table resizes (grows or shrinks), offsets may become slightly inaccurate. The correction mechanism:

1. A read uses the offset table to find an *approximate* data location
2. The reader scans forward/backward to find the *actual* position
3. The reader **writes the corrected offset back** to the offset table
4. The next reader at that location gets a 100% accurate hit

Properties of this approach:
- **No index rebuild operations.** Ever. The index is always usable, always improving.
- **Resize is non-blocking.** Resize the offset table, accept momentary degradation, and normal traffic heals it.
- **Write amplification is minimal.** Only offsets that are actually accessed get corrected.
- **Reads are self-optimizing.** The more a region is queried, the more accurate it becomes.
- **Cold storage gracefully degrades.** Untouched regions get fuzzy, but a single access snaps them back.

### Concurrency on Write-Back Corrections

Reads have a write side-effect (offset correction). Concurrent access considerations:

- Multiple readers finding the same stale offset may try to correct simultaneously
- Requires CAS (compare-and-swap) or similar atomic operation on offset cells
- "Last writer wins" is acceptable since all writers converge on the same correct value
- Broader concurrency strategy to be designed as part of [Concurrency & Transactions](./concurrency.md), potentially delegated to storage engine capabilities

---

## Operations Enabled

| Operation | How It Works |
|---|---|
| **Equality** (`= x`) | Compute `f(x)`, look up exact position in offset table |
| **Range** (`> x`, `< x`, `BETWEEN`) | Compute `f(x)`, slice the offset table from that position |
| **Negation** (`NOT x`) | Complement of `f(x)` position in the offset table |
| **Less than zero** | Separate negative branch, same scalar technique |
| **Pattern matching** (strings) | Multi-stage decomposition narrows candidates progressively |

---

## User-Defined Mapping Functions

Users can provide custom `f(x)` functions to optimize index resolution for their data distribution.

**Why:** The user knows their data. If 99% of timestamps are in the last 6 months, a custom mapping function gives that range higher resolution in the offset table.

**Default:** Linear mapping for numeric types, multi-stage decomposition for strings. Sane, works everywhere, but not optimal for skewed distributions.

**Custom example:**
```
# Logarithmic mapping for data with exponential distribution
f(x) = log(x - min) / log(max - min)
```

This makes the user the optimizer. The database provides the mechanism; the user provides the policy when they want to.

---

## Additional Index Types

The scalar ratio technique is the foundational approach. Additional specialized index types are planned but not yet documented. The scalar system is the *unifying framework* — specialized algorithms (hash, phonetic, geospatial, etc.) can be used where they are genuinely a better fit.

The indexing engine should be smart enough (or pluggable enough) to pick the right tool for the job, with scalar ratio as the universal default.

---

## Open Questions

- [ ] Exact block size strategy and tuning parameters
- [ ] Threshold for dimensional growth (when does a scalar become a vector?)
- [ ] Default mapping functions for each built-in type
- [ ] Plugin interface for custom index types beyond scalar ratio
- [ ] Integration with query engine — how does the planner reason about index accuracy?
- [ ] Metrics/observability for index health (what percentage of offsets are "stale"?)
- [ ] Additional clever index types (teased but not yet documented)

---

## Problems Addressed

From [Why Databases Suck](../docs/why-databases-suck.md):
- **#1 Storage engines stuck in the past** — Novel approach, not B+ tree or LSM
- **#8 Indexing is manual and static** — Self-healing, adaptive, user-customizable
- **#11 Compression/efficiency afterthought** — Compact scalar representation, resolution-adaptive
