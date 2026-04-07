# Query Engine: Projections — Spec

**Date:** 2026-04-07
**Status:** Draft
**Priority:** Medium — bandwidth optimization, developer ergonomics
**Depends on:** Nothing (but benefits from sorting/pagination for full utility)

---

## 1. Overview

Return specific field values instead of full FileRecord metadata. Currently every query result includes path, total_size, content_type, created_at, updated_at, score, matched_by — but not the actual document field values (the data the user queried for). Projections fix this.

---

## 2. The problem

Today: you query for people with age > 30, you get back a list of file paths. To see the actual name and age, you need a second request per file to `GET /engine/{path}`. That's N+1 queries — the exact pattern databases exist to prevent.

With projections: you query and get the field values inline:

```json
[
  {"_path": "/people/alice.json", "name": "Alice", "age": 35},
  {"_path": "/people/bob.json", "name": "Bob", "age": 42}
]
```

---

## 3. How it works

Field values are stored in `FieldIndex.values` — the `HashMap<file_hash, Vec<u8>>` we added for fuzzy recheck. For any indexed field, we can retrieve the raw value without loading the file.

For projected fields that are NOT indexed, we'd need to load the file content and parse it. This is expensive and should be:
1. Attempted from the `.parsed/` cache or file content
2. Flagged in EXPLAIN as a "full scan projection"

For v1: only indexed fields are projectable. Non-indexed field → error.

---

## 4. API

### Request

```json
{
  "path": "/people/",
  "where": {"field": "age", "op": "gt", "value": 30},
  "select": ["name", "age", "email"]
}
```

`select` is an array of field names to include in results.

### Response

When `select` is present, the response shape changes from FileRecord-based to field-value-based:

```json
{
  "results": [
    {
      "_path": "/people/alice.json",
      "_score": 1.0,
      "_matched_by": [],
      "name": "Alice",
      "age": 35,
      "email": "alice@example.com"
    }
  ]
}
```

System fields (prefixed with `_`) are always included:
- `_path` — file path
- `_score` — relevance score
- `_matched_by` — matching strategies (for fuzzy queries)
- `_size` — file size (optional, included if no select or if explicitly selected)
- `_content_type` — MIME type (optional)
- `_created_at` — creation timestamp (optional)
- `_updated_at` — update timestamp (optional)

### No select = existing behavior

When `select` is absent, results use the current FileRecord format (backward compatible).

### Select system fields explicitly

```json
{"select": ["name", "_created_at", "_size"]}
```

This includes only `name` plus the specified system fields. `_path` and `_score` are always included.

---

## 5. Value deserialization

The `index.values` map stores raw bytes (from `json_value_to_bytes`):
- Strings → UTF-8 bytes → return as JSON string
- u64 → 8 bytes big-endian → return as JSON number
- i64 → 8 bytes big-endian → return as JSON number
- f64 → 8 bytes big-endian → return as JSON number
- bool → 1 byte → return as JSON boolean

Need a `bytes_to_json_value(bytes, index_type) -> serde_json::Value` helper that reverses `json_value_to_bytes`.

---

## 6. Error cases

- Projected field not indexed → error: "Cannot project field 'phone' — no index found. Only indexed fields can be projected."
- Field indexed but no values stored → return null for that field
- Empty select array → error

---

## 7. Implementation approach

### Query struct changes

```rust
pub struct Query {
    // ... existing fields ...
    pub select: Option<Vec<String>>,  // NEW: projected field names
}
```

### Execution changes

After filtering and sorting (if applicable):
1. If `select` is present: for each result, for each selected field:
   a. Find the field's index
   b. Look up file_hash in `index.values`
   c. Deserialize bytes to JSON value using `bytes_to_json_value`
   d. Build a JSON object with the projected fields + system fields
2. Return the projected results

### Response type

When projections are active, `QueryResult` becomes a JSON object instead of a fixed struct. The HTTP response serialization handles this differently.

---

## 8. Phases

1. `select` parameter parsing in Query and HTTP API
2. `bytes_to_json_value` helper (reverse of `json_value_to_bytes`)
3. Projection execution: load values from index, build JSON response
4. System field handling (`_path`, `_score`, etc.)
5. Combine with sorting/pagination
