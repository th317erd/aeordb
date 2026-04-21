# Query API

The query engine supports indexed field queries with boolean combinators, pagination, sorting, aggregations, projections, and an explain mode.

## Endpoint Summary

| Method | Path | Description | Auth | Status Codes |
|--------|------|-------------|------|-------------|
| POST | `/files/query` | Execute a query | Yes | 200, 400, 404, 500 |

---

## POST /files/query

Execute a query against indexed fields within a directory path.

### Request Body

```json
{
  "path": "/users",
  "where": {
    "field": "age",
    "op": "gt",
    "value": 21
  },
  "limit": 20,
  "offset": 0,
  "order_by": [{"field": "name", "direction": "asc"}],
  "after": null,
  "before": null,
  "include_total": true,
  "select": ["@path", "@score", "name"],
  "explain": false
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | Directory path to query within |
| `where` | object/array | Yes | Query filter (see below) |
| `limit` | integer | No | Max results to return (server default applies if omitted) |
| `offset` | integer | No | Skip this many results |
| `order_by` | array | No | Sort fields with direction |
| `after` | string | No | Cursor for forward pagination |
| `before` | string | No | Cursor for backward pagination |
| `include_total` | boolean | No | Include `total_count` in response (default: false) |
| `select` | array | No | Project specific fields in results |
| `aggregate` | object | No | Run aggregations instead of returning results |
| `explain` | string/boolean | No | `"plan"`, `"analyze"`, or `true` for query plan |

---

## Query Operators

Each field query is an object with `field`, `op`, and `value`:

```json
{"field": "age", "op": "gt", "value": 21}
```

### Comparison Operators

| Operator | Description | Value Type | Example |
|----------|-------------|------------|---------|
| `eq` | Exact match | any | `{"field": "status", "op": "eq", "value": "active"}` |
| `gt` | Greater than | number/string | `{"field": "age", "op": "gt", "value": 21}` |
| `lt` | Less than | number/string | `{"field": "age", "op": "lt", "value": 65}` |
| `between` | Inclusive range | number/string | `{"field": "age", "op": "between", "value": 21, "value2": 65}` |
| `in` | Match any value in a set | array | `{"field": "status", "op": "in", "value": ["active", "pending"]}` |

### Text Search Operators

These operators require the appropriate index type to be configured.

| Operator | Description | Index Required | Example |
|----------|-------------|---------------|---------|
| `contains` | Substring match | trigram | `{"field": "name", "op": "contains", "value": "alice"}` |
| `similar` | Fuzzy trigram match with threshold | trigram | `{"field": "name", "op": "similar", "value": "alice", "threshold": 0.3}` |
| `phonetic` | Sounds-like match | phonetic | `{"field": "name", "op": "phonetic", "value": "smith"}` |
| `fuzzy` | Configurable fuzzy match | trigram | See below |
| `match` | Multi-strategy combined match | trigram + phonetic | `{"field": "name", "op": "match", "value": "alice"}` |

### Fuzzy Operator Options

The `fuzzy` operator supports additional parameters:

```json
{
  "field": "name",
  "op": "fuzzy",
  "value": "alice",
  "fuzziness": "auto",
  "algorithm": "damerau_levenshtein"
}
```

| Parameter | Values | Default |
|-----------|--------|---------|
| `fuzziness` | `"auto"` or integer (edit distance) | `"auto"` |
| `algorithm` | `"damerau_levenshtein"`, `"jaro_winkler"` | `"damerau_levenshtein"` |

### Similar Operator Options

```json
{
  "field": "name",
  "op": "similar",
  "value": "alice",
  "threshold": 0.3
}
```

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `threshold` | float | 0.3 | Minimum similarity score (0.0 to 1.0) |

---

## Boolean Combinators

Combine multiple conditions using `and`, `or`, and `not`:

### AND

All conditions must match:

```json
{
  "where": {
    "and": [
      {"field": "age", "op": "gt", "value": 21},
      {"field": "status", "op": "eq", "value": "active"}
    ]
  }
}
```

### OR

At least one condition must match:

```json
{
  "where": {
    "or": [
      {"field": "status", "op": "eq", "value": "active"},
      {"field": "status", "op": "eq", "value": "pending"}
    ]
  }
}
```

### NOT

Invert a condition:

```json
{
  "where": {
    "not": {"field": "status", "op": "eq", "value": "deleted"}
  }
}
```

### Nested Boolean Logic

Combinators can be nested up to **32 levels deep**. Queries exceeding this depth are rejected with a `400` error. Additionally, a single query can return at most **100,000 results** -- requests that would exceed this ceiling are truncated.

```json
{
  "where": {
    "and": [
      {"field": "age", "op": "gt", "value": 21},
      {
        "or": [
          {"field": "role", "op": "eq", "value": "admin"},
          {"field": "role", "op": "eq", "value": "moderator"}
        ]
      }
    ]
  }
}
```

### Legacy Array Format

An array at the top level is sugar for AND:

```json
{
  "where": [
    {"field": "age", "op": "gt", "value": 21},
    {"field": "status", "op": "eq", "value": "active"}
  ]
}
```

---

## Response Format

### Standard Query Response

```json
{
  "results": [
    {
      "path": "/users/alice.json",
      "size": 256,
      "content_type": "application/json",
      "created_at": 1775968398000,
      "updated_at": 1775968398000,
      "score": 1.0,
      "matched_by": ["age"]
    }
  ],
  "has_more": true,
  "total_count": 150,
  "next_cursor": "eyJwYXRoIjoiL3VzZXJzL2JvYi5qc29uIn0=",
  "prev_cursor": "eyJwYXRoIjoiL3VzZXJzL2Fhcm9uLmpzb24ifQ==",
  "meta": {
    "reindexing": 0.67,
    "reindexing_eta": 1775968398803,
    "reindexing_indexed": 670,
    "reindexing_total": 1000,
    "reindexing_stale_since": 1775968300000
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `results` | array | Matching file metadata with scores |
| `has_more` | boolean | Whether more results exist beyond the current page |
| `total_count` | integer | Total matching results (only if `include_total: true`) |
| `next_cursor` | string | Cursor for the next page (if `has_more` is true) |
| `prev_cursor` | string | Cursor for the previous page |
| `default_limit_hit` | boolean | Present and true when the server's default limit was applied |
| `default_limit` | integer | The server's default limit value (present with `default_limit_hit`) |
| `meta` | object | Reindex progress metadata (present only during active reindex) |

### Result Fields

Each result object contains:

| Field | Type | Description |
|-------|------|-------------|
| `path` | string | Full path to the matched file |
| `size` | integer | File size in bytes |
| `content_type` | string | MIME type (nullable) |
| `created_at` | integer | Creation timestamp (ms) |
| `updated_at` | integer | Last update timestamp (ms) |
| `score` | float | Relevance score (1.0 = exact match) |
| `matched_by` | array | List of field names that matched |

---

## Sorting

Sort results by one or more fields:

```json
{
  "order_by": [
    {"field": "name", "direction": "asc"},
    {"field": "created_at", "direction": "desc"}
  ]
}
```

| Direction | Description |
|-----------|-------------|
| `asc` | Ascending (default) |
| `desc` | Descending |

---

## Pagination

### Offset-Based

```json
{
  "limit": 20,
  "offset": 40
}
```

### Cursor-Based

Use `after` or `before` with cursor values from a previous response:

```json
{
  "limit": 20,
  "after": "eyJwYXRoIjoiL3VzZXJzL2JvYi5qc29uIn0="
}
```

---

## Projection (select)

Return only specific fields in each result. Use `@`-prefixed names for built-in metadata fields:

```json
{
  "select": ["@path", "@score", "name", "email"]
}
```

| Virtual Field | Maps To |
|---------------|---------|
| `@path` | `path` |
| `@score` | `score` |
| `@size` | `size` |
| `@content_type` | `content_type` |
| `@created_at` | `created_at` |
| `@updated_at` | `updated_at` |
| `@matched_by` | `matched_by` |

Envelope fields (`has_more`, `next_cursor`, `total_count`, `meta`) are never stripped by projection.

---

## Aggregations

Run aggregate computations instead of returning individual results.

### Request

```json
{
  "path": "/orders",
  "where": {"field": "status", "op": "eq", "value": "complete"},
  "aggregate": {
    "count": true,
    "sum": ["total", "tax"],
    "avg": ["total"],
    "min": ["total"],
    "max": ["total"],
    "group_by": ["status"]
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `count` | boolean | Include a count of matching records |
| `sum` | array | Fields to sum |
| `avg` | array | Fields to average |
| `min` | array | Fields to find minimum |
| `max` | array | Fields to find maximum |
| `group_by` | array | Fields to group results by |

### Response

The response shape depends on whether `group_by` is used. Aggregation results are returned as a JSON object.

---

## Explain Mode

Inspect the query execution plan without running the full query. Useful for debugging index usage and performance.

```json
{
  "path": "/users",
  "where": {"field": "age", "op": "gt", "value": 21},
  "explain": "plan"
}
```

| Value | Description |
|-------|-------------|
| `true` or `"plan"` | Show the query plan |
| `"analyze"` | Execute the query and include timing information |

---

## Virtual Fields

Virtual fields let you query file metadata directly -- without defining any indexes. Prefix a field name with `@` to query against built-in file attributes instead of indexed document fields.

### Available Virtual Fields

| Field | Type | Description |
|-------|------|-------------|
| `@path` | string | Full file path (e.g., `/docs/report.pdf`) |
| `@filename` | string | Filename only -- the last segment of the path (e.g., `report.pdf`) |
| `@extension` | string | File extension after the last `.` (e.g., `pdf`) |
| `@content_type` | string | MIME type (e.g., `application/pdf`) |
| `@size` | u64 | File size in bytes |
| `@created_at` | i64 | Creation timestamp in milliseconds |
| `@updated_at` | i64 | Last update timestamp in milliseconds |

### Supported Operators

**String virtual fields** (`@path`, `@filename`, `@extension`, `@content_type`) support:

`eq`, `contains`, `in`, `gt`, `lt`, `similar` (trigram), `fuzzy` (edit distance), `phonetic` (soundex/metaphone), `match` (fused multi-strategy)

**Numeric virtual fields** (`@size`, `@created_at`, `@updated_at`) support:

`eq`, `gt`, `lt`, `between`, `in`

### How Virtual Fields Work

Virtual fields scan all files under the query path and evaluate each file's metadata against the filter conditions. This means:

- **No index configuration required.** Virtual field queries work immediately with zero setup.
- **Slower than indexed queries.** Because every file is scanned, virtual field queries are O(n) where n is the number of files under the path. For small-to-medium datasets this is perfectly fine. For large datasets with millions of files, prefer indexed fields for hot query paths.
- **Combinable with indexed fields.** You can mix virtual fields and indexed fields in the same `where` clause using boolean combinators (`and`, `or`, `not`).

### Virtual Field Examples

Find all PDFs by extension:

```bash
curl -X POST http://localhost:6830/files/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/",
    "where": {"field": "@extension", "op": "eq", "value": "pdf"}
  }'
```

Fuzzy filename search (handles typos):

```bash
curl -X POST http://localhost:6830/files/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/docs",
    "where": {"field": "@filename", "op": "similar", "value": "report", "threshold": 0.3}
  }'
```

Find large files over 10 MB:

```bash
curl -X POST http://localhost:6830/files/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/",
    "where": {"field": "@size", "op": "gt", "value": 10485760}
  }'
```

Find images by content type:

```bash
curl -X POST http://localhost:6830/files/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/",
    "where": {"field": "@content_type", "op": "contains", "value": "image/"}
  }'
```

Combine multiple virtual fields -- large PDFs:

```bash
curl -X POST http://localhost:6830/files/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/",
    "where": {
      "and": [
        {"field": "@extension", "op": "eq", "value": "pdf"},
        {"field": "@size", "op": "gt", "value": 1048576}
      ]
    }
  }'
```

---

## Examples

### Simple equality query

```bash
curl -X POST http://localhost:6830/files/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/users",
    "where": {"field": "status", "op": "eq", "value": "active"},
    "limit": 10
  }'
```

### Fuzzy name search with pagination

```bash
curl -X POST http://localhost:6830/files/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/users",
    "where": {"field": "name", "op": "similar", "value": "alice", "threshold": 0.4},
    "limit": 20,
    "order_by": [{"field": "name", "direction": "asc"}],
    "include_total": true
  }'
```

### Complex boolean query

```bash
curl -X POST http://localhost:6830/files/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/products",
    "where": {
      "and": [
        {"field": "price", "op": "between", "value": 10, "value2": 100},
        {
          "or": [
            {"field": "category", "op": "eq", "value": "electronics"},
            {"field": "category", "op": "eq", "value": "books"}
          ]
        },
        {"not": {"field": "status", "op": "eq", "value": "discontinued"}}
      ]
    },
    "order_by": [{"field": "price", "direction": "asc"}],
    "limit": 50
  }'
```

### Aggregation with grouping

```bash
curl -X POST http://localhost:6830/files/query \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "path": "/orders",
    "where": {"field": "year", "op": "eq", "value": 2026},
    "aggregate": {
      "count": true,
      "sum": ["total"],
      "avg": ["total"],
      "group_by": ["status"]
    }
  }'
```

### Error Responses

| Status | Condition |
|--------|-----------|
| 400 | Invalid query structure, missing field/op, unsupported operation, range query on non-range converter |
| 404 | Query path or index not found |
| 500 | Internal query execution failure |
