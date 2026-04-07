# Query Engine: Projections — Spec

**Date:** 2026-04-07
**Status:** Draft
**Priority:** Low — cosmetic convenience, not a query engine change
**Depends on:** Nothing

---

## 1. Overview

Projections are a response filter. The query engine runs normally, produces its standard JSON response, and then a post-processing step strips fields the user didn't ask for. No index awareness, no value loading, no new query execution paths.

This only applies to JSON responses. Non-JSON document queries (images, PDFs, etc.) are unaffected — if users need custom filtering for those, they write a query plugin.

---

## 2. How it works

```
Query Engine → Full JSON response → Projection filter → Trimmed JSON → Client
```

The projection filter runs in the HTTP layer, after serialization. It's `jq`-like field selection on the outgoing JSON.

---

## 3. API

```json
{
  "path": "/people/",
  "where": {"field": "age", "op": "gt", "value": 30},
  "select": ["path", "score", "content_type"]
}
```

### Before filter:
```json
[
  {"path": "/people/alice.json", "total_size": 1234, "content_type": "application/json", "created_at": 123, "updated_at": 456, "score": 1.0, "matched_by": []}
]
```

### After filter:
```json
[
  {"path": "/people/alice.json", "score": 1.0, "content_type": "application/json"}
]
```

### Works on any response shape

Pagination envelope:
```json
{
  "results": [
    {"path": "/people/alice.json", "score": 1.0}
  ],
  "has_more": true,
  "next_cursor": "..."
}
```

Aggregation response:
```json
{
  "count": 150,
  "avg": {"age": 34.5}
}
```

The filter applies recursively to result objects. Envelope fields (`has_more`, `next_cursor`, `total_count`) are never stripped — they're pagination metadata, not result data.

### No select = no filtering

Omitting `select` returns the full response (backward compatible).

---

## 4. Implementation

A single function in the HTTP response path:

```rust
fn apply_projection(response: serde_json::Value, select: &[String]) -> serde_json::Value
```

If the response is an array: filter each object. If the response is an object with a `results` array: filter each result, preserve envelope fields. Otherwise: return as-is.

Per-object filtering: keep only keys that are in the `select` list.

---

## 5. Phases

1. Parse `select` from query JSON
2. `apply_projection` helper function
3. Wire into HTTP query response path
4. Tests: array filtering, envelope filtering, no-select passthrough, empty select
