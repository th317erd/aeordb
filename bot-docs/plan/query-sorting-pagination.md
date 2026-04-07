# Query Engine: Sorting + Pagination — Spec

**Date:** 2026-04-07
**Status:** Draft
**Priority:** High — foundational for all query use cases
**Depends on:** Nothing (builds on existing query engine)

---

## 1. Overview

Add ORDER BY sorting and two pagination styles (offset + cursor) to the query engine. Currently results are unsorted (exact queries) or implicitly sorted by score (fuzzy). Only `limit` exists.

---

## 2. Sorting

### How it works

Sort happens AFTER filtering. The filter produces a set of file_hashes, then we sort that set by looking up each file_hash's value in the sort field's index.

```
Filter → HashSet<file_hash> → Load sort values from index → Sort → Paginate → Return
```

For order-preserving indexes (numeric, timestamp, string), the index entries are already sorted by scalar. We can exploit this for single-field sorts: walk the sorted entries and emit only those in the result set. This avoids loading all values into memory.

For non-order-preserving indexes (hash, trigram, phonetic), sorting by that field is meaningless. The sort field must have an order-preserving index.

### Multi-field sorting

ORDER BY age ASC, name DESC — load the primary sort value for each result, then the secondary for tie-breaking. In-memory sort with a multi-key comparator.

### API

```json
{
  "path": "/people/",
  "where": {"field": "active", "op": "eq", "value": true},
  "order_by": [
    {"field": "age", "direction": "asc"},
    {"field": "name", "direction": "desc"}
  ],
  "limit": 20
}
```

- `order_by` is an array (ordered by priority)
- `direction`: `"asc"` (default) or `"desc"`
- If `order_by` is omitted, results are unordered (existing behavior)
- Fuzzy queries: if `order_by` is omitted, sort by `@score` descending (existing behavior). If `order_by` is provided, it takes priority.
- Virtual fields (prefixed with `@`) are valid sort fields: `@score`, `@path`, `@size`, `@created_at`, `@updated_at`. These are computed/metadata values, not indexed fields.
- `@score` is always available for fuzzy queries. For exact queries, `@score` is 1.0 for all results (sorting by it is a no-op).

### Error cases

- Sort field has no index → error: "No index found for sort field 'x'"
- Sort field's index is not order-preserving → error: "Cannot sort by non-order-preserving field 'x' (index type: trigram)"

---

## 3. Pagination

### Offset-based

Simple stateless pagination. Client specifies `offset` (number of results to skip) and `limit`.

```json
{
  "where": {"field": "age", "op": "gt", "value": 18},
  "order_by": [{"field": "age", "direction": "asc"}],
  "limit": 20,
  "offset": 40
}
```

Page 1: offset=0, limit=20. Page 2: offset=20. Page 3: offset=40.

**Trade-off:** O(offset+limit) work per request — the engine still evaluates and sorts all results up to offset+limit, then discards the first `offset`. Fine for small datasets, degrades for large offsets.

### Cursor-based

More efficient for large datasets. The cursor encodes the last result's sort key AND the version hash, so the next page operates on the same snapshot of data — no skips or duplicates from concurrent mutations.

```json
{
  "where": {"field": "age", "op": "gt", "value": 18},
  "order_by": [{"field": "age", "direction": "asc"}],
  "limit": 20,
  "after": "eyJhZ2UiOjM1LCJfaGFzaCI6ImFiYzEyMyIsIl92ZXJzaW9uIjoiZGVhZGJlZWYifQ"
}
```

The `after` token is an opaque base64-encoded JSON containing:
```json
{"age": 35, "_hash": "abc123...", "_version": "deadbeef..."}
```

- Sort key for seeking
- File hash for tie-breaking
- Version hash: locks the query to the same tree state as page 1. Subsequent pages use this version, not HEAD. This guarantees cursor stability — data mutations between pages don't affect the result set.

The engine uses the cursor to seek directly into the sorted index, skipping everything before it. O(limit) per page regardless of position.

`before` cursor for backward pagination:
```json
{
  "order_by": [{"field": "age", "direction": "asc"}],
  "limit": 20,
  "before": "cursor_token"
}
```

### Response metadata

```json
{
  "results": [...],
  "total_count": 150,
  "has_more": true,
  "next_cursor": "eyJhZ2UiOjU1LCJfaGFzaCI6ImRlZjQ1NiJ9",
  "prev_cursor": "eyJhZ2UiOjM1LCJfaGFzaCI6ImFiYzEyMyJ9"
}
```

`total_count` is the total matching results (before pagination). Computing this requires evaluating the full filter — it can be expensive. Make it opt-in:

```json
{"include_total": true}
```

Default: `total_count` is omitted (saves a full scan for paginated queries).

---

## 4. Implementation approach

### Query struct changes

```rust
pub struct Query {
    pub path: String,
    pub field_queries: Vec<FieldQuery>,
    pub node: Option<QueryNode>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,           // NEW
    pub order_by: Vec<SortField>,        // NEW
    pub after: Option<String>,           // NEW: cursor token
    pub before: Option<String>,          // NEW: cursor token
    pub include_total: bool,             // NEW
    pub strategy: QueryStrategy,
}

pub struct SortField {
    pub field: String,
    pub direction: SortDirection,
}

pub enum SortDirection {
    Asc,
    Desc,
}
```

### QueryResult changes

Wrap results in a paginated response:

```rust
pub struct PaginatedResult {
    pub results: Vec<QueryResult>,
    pub total_count: Option<u64>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
}
```

### Execution changes

After filtering (existing), before returning:
1. If `order_by` is set: load sort values from indexes, sort results
2. If `after`/`before` cursor: decode cursor, seek into sorted results
3. If `offset`: skip first N results
4. Apply `limit`
5. If `include_total`: count total (before pagination)
6. Build cursors from first/last result sort keys

### HTTP API changes

The `/query` POST body accepts new fields: `order_by`, `offset`, `after`, `before`, `include_total`.

Response wraps in pagination envelope when `order_by` or pagination fields are present:
```json
{
  "results": [{...}, {...}],
  "has_more": true,
  "next_cursor": "...",
  "total_count": 150
}
```

When no pagination fields → flat array (backward compatible).

---

## 5. Default limit

ALL queries that return multiple items have a default limit of 20. This prevents accidental full-table dumps of entire documents. The response indicates when the default was applied:

```json
{
  "results": [...],
  "default_limit_hit": true,
  "default_limit": 20,
  "has_more": true
}
```

Users can override with an explicit `"limit": N`. The engine may impose a hard cap in the future (forcing cursor-based pagination for large result sets).

When `limit` is explicitly provided, `default_limit_hit` is omitted.

This applies globally — not just to sorted/paginated queries. Every query path (exact, fuzzy, aggregation groups) respects the default limit.

---

## 6. Edge cases

- Empty result set: `has_more: false`, `next_cursor: null`
- Single result: works normally
- Cursor with deleted data: cursor points to a key that no longer exists — seek to the next valid entry
- Multi-field cursor: encodes all sort field values for precise positioning
- Score + sort: if both fuzzy score and order_by exist, order_by takes priority
- Sort by non-indexed field: error (we don't load files to sort)

---

## 6. Phases

1. Single-field ORDER BY (asc/desc) + offset/limit pagination
2. Multi-field ORDER BY
3. Cursor-based pagination (after/before)
4. include_total opt-in count
5. HTTP response envelope with pagination metadata
