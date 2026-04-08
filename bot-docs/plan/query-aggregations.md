# Query Engine: Aggregations — Spec

**Date:** 2026-04-07
**Status:** Draft
**Priority:** High — analytics use case
**Depends on:** Nothing (independent of sorting/pagination)

---

## 1. Overview

Add aggregation operations to the query engine: COUNT, SUM, AVG, MIN, MAX, and GROUP BY. Aggregations run on the FILTERED result set — the `where` clause narrows the data, aggregations compute statistics over what remains.

---

## 2. How it works with our architecture

The filter produces a `HashSet<Vec<u8>>` of matching file_hashes. Aggregations load field values from the index's `values` map (already stored for fuzzy recheck) and compute statistics.

- **COUNT** — `result_set.len()`. Trivial.
- **MIN/MAX** — for order-preserving indexes: first/last entry in the sorted entries array that's in the result set. O(entries) worst case, but fast because entries are sorted.
- **SUM/AVG** — load numeric values from `index.values` for each result, parse as number, accumulate. Requires the field to have a numeric index (u8-u64, i64, f64, timestamp).
- **GROUP BY** — load grouping field values from `index.values`, bucket results by value, aggregate per bucket.

### Type handling

Aggregation fields must be indexed. The index type determines what operations are valid:

| Index type | COUNT | MIN | MAX | SUM | AVG |
|-----------|-------|-----|-----|-----|-----|
| u8-u64, i64, f64 | Yes | Yes | Yes | Yes | Yes |
| timestamp | Yes | Yes | Yes | No | No |
| string | Yes | Yes (lexicographic) | Yes (lexicographic) | No | No |
| trigram | Yes | No | No | No | No |
| phonetic | Yes | No | No | No | No |

SUM/AVG on strings → error. MIN/MAX on strings → lexicographic ordering.

---

## 3. API

### Request

```json
{
  "path": "/people/",
  "where": {"field": "active", "op": "eq", "value": true},
  "aggregate": {
    "count": true,
    "sum": ["salary", "bonus"],
    "avg": ["age", "salary"],
    "min": ["age"],
    "max": ["salary"],
    "group_by": ["department"]
  }
}
```

All aggregate fields are optional. You can request just `count`, or just `min`/`max`, etc.

When `aggregate` is present, the response is an aggregation result, NOT a document list.

### Response (no GROUP BY)

```json
{
  "count": 150,
  "sum": {"salary": 12500000, "bonus": 750000},
  "avg": {"age": 34.5, "salary": 83333.33},
  "min": {"age": 22},
  "max": {"salary": 250000}
}
```

### Response (with GROUP BY)

```json
{
  "count": 150,
  "sum": {"salary": 12500000},
  "avg": {"age": 34.5},
  "groups": [
    {
      "key": {"department": "Engineering"},
      "count": 80,
      "sum": {"salary": 7200000},
      "avg": {"age": 32.1}
    },
    {
      "key": {"department": "Sales"},
      "count": 70,
      "sum": {"salary": 5300000},
      "avg": {"age": 37.2}
    }
  ]
}
```

Top-level aggregates are the totals (across all groups). `groups` contains per-group breakdowns.

### Multi-field GROUP BY

```json
{
  "aggregate": {
    "count": true,
    "group_by": ["department", "role"]
  }
}
```

Groups are by the combination of all group_by fields:
```json
{
  "groups": [
    {"key": {"department": "Engineering", "role": "senior"}, "count": 30},
    {"key": {"department": "Engineering", "role": "junior"}, "count": 50},
    {"key": {"department": "Sales", "role": "senior"}, "count": 25}
  ]
}
```

---

## 4. Group limiting

GROUP BY results respect the same `limit` field as regular queries. Default limit (20) applies. Users override with `"limit": N`. Same `default_limit_hit` behavior.

```json
{
  "aggregate": {
    "count": true,
    "group_by": ["department"]
  },
  "limit": 50
}
```

If the default limit is hit:
```json
{
  "groups": [...],
  "has_more": true,
  "default_limit_hit": true,
  "default_limit": 20
}
```

---

## 5. NULL handling

Documents without the aggregated field are not in that field's index. They are simply not counted/summed/averaged. This is correct — you can't aggregate what doesn't exist.

- `COUNT` with a `where` clause = count of matching documents (from the filter result set size)
- `SUM("salary")` = sum of salary values for documents that HAVE a salary index entry
- Documents without salary are invisible to the salary aggregation

This is not a problem to solve — it's the correct semantic.

---

## 6. Value deserialization

The `index.values` map stores raw bytes from `json_value_to_bytes`. To interpret them for aggregation, use the converter's `type_tag()`:

- `CONVERTER_TYPE_U8..U64` → parse as unsigned integer (big-endian)
- `CONVERTER_TYPE_I64` → parse as signed integer (big-endian)
- `CONVERTER_TYPE_F64` → parse as f64 (big-endian)
- `CONVERTER_TYPE_STRING` → UTF-8 string (for GROUP BY keys, MIN/MAX lexicographic)
- `CONVERTER_TYPE_TIMESTAMP` → parse as i64 millis (for MIN/MAX)

The converter type is always available on the `FieldIndex.converter` — no ambiguity.

---

## 7. Error cases

- Aggregate field not indexed → error: "No index found for aggregate field 'salary'"
- SUM/AVG on non-numeric field → error: "Cannot compute SUM on field 'name' (index type: string)"
- GROUP BY field not indexed → error: "No index found for group_by field 'department'"
- Empty result set → `count: 0`, all other aggregates are `null`

---

## 5. Implementation approach

### New query types

```rust
pub struct AggregateQuery {
    pub count: bool,
    pub sum: Vec<String>,
    pub avg: Vec<String>,
    pub min: Vec<String>,
    pub max: Vec<String>,
    pub group_by: Vec<String>,
}

pub struct AggregateResult {
    pub count: Option<u64>,
    pub sum: HashMap<String, f64>,
    pub avg: HashMap<String, f64>,
    pub min: HashMap<String, serde_json::Value>,
    pub max: HashMap<String, serde_json::Value>,
    pub groups: Option<Vec<GroupResult>>,
}

pub struct GroupResult {
    pub key: HashMap<String, serde_json::Value>,
    pub count: u64,
    pub sum: HashMap<String, f64>,
    pub avg: HashMap<String, f64>,
    pub min: HashMap<String, serde_json::Value>,
    pub max: HashMap<String, serde_json::Value>,
}
```

### Execution

1. Run the filter (existing) → `HashSet<file_hash>`
2. If `aggregate` is present:
   a. For each aggregate field, load the index
   b. Iterate the result set, look up values from `index.values`
   c. Parse values as appropriate type (numeric for SUM/AVG, any for MIN/MAX)
   d. If GROUP BY: bucket by group key, aggregate per bucket
   e. Return AggregateResult (not Vec<QueryResult>)

### Value parsing

The `index.values` map stores raw bytes (as produced by `json_value_to_bytes`):
- u64: 8 bytes big-endian → parse back to u64 → cast to f64 for SUM/AVG
- i64: 8 bytes big-endian → parse back to i64 → cast to f64
- f64: 8 bytes big-endian → parse back to f64
- string: UTF-8 bytes → for MIN/MAX (lexicographic), for GROUP BY key

Need a `parse_numeric_value(bytes, index_type) -> Option<f64>` helper.

---

## 6. Phases

1. COUNT (trivial — result set size)
2. MIN/MAX (walk sorted entries)
3. SUM/AVG (load values, accumulate)
4. GROUP BY with COUNT
5. GROUP BY with SUM/AVG/MIN/MAX
6. HTTP API + response format
