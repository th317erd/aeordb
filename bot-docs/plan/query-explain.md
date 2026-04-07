# Query Engine: EXPLAIN — Spec

**Date:** 2026-04-07
**Status:** Draft
**Priority:** Medium — debugging and optimization
**Depends on:** Better after sorting/aggregations exist (more to explain)

---

## 1. Overview

Show the query execution plan: which indexes are used, what strategy is chosen, estimated vs actual costs. Like SQL's EXPLAIN / EXPLAIN ANALYZE.

---

## 2. Two modes

### EXPLAIN (plan only)

Shows what the engine WOULD do, without executing the query. Fast — no data scanned.

```json
{
  "path": "/people/",
  "where": {"field": "name", "op": "similar", "value": "Smith"},
  "explain": true
}
```

Response:
```json
{
  "plan": {
    "type": "fuzzy_recheck",
    "field": "name",
    "operation": "similar",
    "index": "name.trigram.idx",
    "index_type": "trigram",
    "candidate_strategy": "trigram_or",
    "recheck": true,
    "threshold": 0.3,
    "index_entries": 4500,
    "estimated_candidates": "~50-200"
  }
}
```

### EXPLAIN ANALYZE (plan + execution)

Executes the query AND reports timing/counts:

```json
{
  "path": "/people/",
  "where": {"field": "name", "op": "similar", "value": "Smith"},
  "explain": "analyze"
}
```

Response:
```json
{
  "plan": {
    "type": "fuzzy_recheck",
    "field": "name",
    "operation": "similar",
    "index": "name.trigram.idx",
    "index_type": "trigram",
    "candidate_strategy": "trigram_or",
    "recheck": true,
    "threshold": 0.3
  },
  "execution": {
    "total_duration_ms": 3.5,
    "filter_duration_ms": 1.2,
    "recheck_duration_ms": 2.1,
    "sort_duration_ms": 0.2,
    "candidates_generated": 47,
    "after_recheck": 12,
    "results_returned": 12,
    "indexes_loaded": ["name.trigram.idx"],
    "index_entries_scanned": 4500,
    "files_loaded_for_recheck": 0
  },
  "results": [{...}]
}
```

EXPLAIN ANALYZE returns both the plan AND the actual results (like PostgreSQL's EXPLAIN ANALYZE).

---

## 3. Plan information per query type

### Exact/comparison queries (Tier 1)

```json
{
  "plan": {
    "type": "tier1_scalar_lookup",
    "nodes": [
      {
        "field": "age",
        "operation": "gt",
        "index": "age.u64.idx",
        "index_type": "u64",
        "order_preserving": true,
        "index_entries": 1000
      }
    ],
    "boolean_logic": "and",
    "bitmap_compositing": false
  }
}
```

### Boolean queries (Tier 2)

```json
{
  "plan": {
    "type": "tier2_bitmap_compositing",
    "tree": {
      "or": [
        {"field": "city", "op": "eq", "index": "city.string.idx"},
        {"field": "state", "op": "eq", "index": "state.string.idx"}
      ]
    },
    "bitmap_compositing": true,
    "bucket_count": 1024
  }
}
```

### Fuzzy queries

```json
{
  "plan": {
    "type": "fuzzy_recheck",
    "field": "name",
    "operation": "match",
    "indexes_checked": [
      {"index": "name.trigram.idx", "strategy": "trigram_or"},
      {"index": "name.soundex.idx", "strategy": "phonetic_lookup"},
      {"index": "name.dmetaphone.idx", "strategy": "phonetic_lookup"},
      {"index": "name.string.idx", "strategy": "exact_lookup"}
    ],
    "recheck": true,
    "scoring": "max_across_strategies"
  }
}
```

---

## 4. Implementation approach

### Query struct changes

```rust
pub struct Query {
    // ... existing fields ...
    pub explain: ExplainMode,  // NEW
}

pub enum ExplainMode {
    Off,        // normal execution
    Plan,       // plan only, no execution
    Analyze,    // plan + execution + results
}
```

### Execution changes

Wrap the existing execution in timing instrumentation:
1. Before filter: record start time
2. After filter: record candidate count, filter duration
3. After recheck: record recheck count, recheck duration
4. After sort: record sort duration
5. After limit: record results returned
6. Build ExplainResult from collected metrics

For `Plan` mode: analyze the query structure, determine which indexes would be used, estimate costs, return without executing.

For `Analyze` mode: execute normally but collect all the metrics above, return both plan + metrics + results.

### Response type

```rust
pub struct ExplainResult {
    pub plan: serde_json::Value,
    pub execution: Option<serde_json::Value>,  // None for Plan mode
    pub results: Option<Vec<QueryResult>>,      // None for Plan mode
}
```

---

## 5. Phases

1. ExplainMode enum + Query field
2. Plan-only mode (inspect query, report indexes/strategies)
3. Analyze mode (timing instrumentation, candidate/result counts)
4. HTTP API (`"explain": true` or `"explain": "analyze"`)
5. Plan detail for boolean trees (show the tree structure)
