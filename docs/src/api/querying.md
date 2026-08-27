# Query API

The query engine supports indexed field queries with boolean combinators, pagination, sorting, aggregations, projections, and an explain mode.

## Endpoint Summary

| Method | Path | Description | Auth | Status Codes |
|--------|------|-------------|------|-------------|
| POST | `/files/query` | Execute a query | Yes | 200, 400, 404, 410, 500, 503 |
| POST | `/files/search` | Global cross-directory search | Yes | 200, 400, 404, 410, 500, 503 |

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
  "include_matches": false,
  "max_matches_per_result": 5,
  "snippet_chars": 160,
  "match_context_lines": 2,
  "max_locator_scan_bytes": 268435456,
  "explain": false
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `path` | string | Yes | Directory path to query within |
| `where` | object/array | Yes | Query filter (see below) |
| `root_hash` | string | No | Exact lowercase namespace-root hash; mutually exclusive with `snapshot` and `version` |
| `snapshot` | string | No | Named snapshot selector; mutually exclusive with `root_hash` and `version` |
| `version` | string | No | Exact namespace-root hash through the legacy version alias; mutually exclusive with `root_hash` and `snapshot` |
| `limit` | integer | No | Max results to return (server default applies if omitted) |
| `page` | integer | No | One-based page origin; mutually exclusive with `offset`, `after`, and `before` |
| `offset` | integer | No | Skip this many results |
| `order_by` | array | No | Sort fields with direction |
| `after` | string | No | Canonical APOS cursor for forward pagination; requires explicit `root_hash` |
| `before` | string | No | Canonical APOS cursor for backward pagination; requires explicit `root_hash` |
| `include_total` | boolean | No | Include `total` in response (default: false) |
| `select` | array | No | Project specific fields in results |
| `aggregate` | object | No | Run aggregations instead of returning results |
| `include_matches` | boolean | No | Include request-time hit locators for returned results (default: false) |
| `max_matches_per_result` | integer | No | Maximum hit locators per result (default: 5, max: 50) |
| `snippet_chars` | integer | No | Maximum snippet characters per locator (default: 160, max: 4096) |
| `match_context_lines` | integer | No | Context line count for stored-file line fetch hints |
| `max_locator_scan_bytes` | integer | No | Caller cap for stored-file locator scans; server clamps to its hard cap |
| `explain` | string/boolean | No | `"plan"`, `"analyze"`, or `true` for query plan |

Hit locators are opt-in. They are generated only for the current page after authorization filtering, not during index lookup.

### Root Selection and Authorization

Query, search, aggregation, and EXPLAIN execute against one captured namespace
root. If no selector is supplied, AeorDB captures current HEAD once. A supplied
`root_hash`, `snapshot`, or `version` is exact and never falls back to a newer
HEAD when it is unavailable.

Successful responses include the selected root as:

```json
{
  "root": {
    "hash": "9f26...",
    "state": "retained",
    "expires_at": null
  }
}
```

Current path, key, user/group, share, and protected-family authorization is
evaluated before selected-root indexes, counts, pages, aggregates, EXPLAIN, or
locator bodies become observable. Historical permission documents may further
restrict current access but cannot expand it. Share-link credentials are
current-HEAD only.

Legacy-v3 persisted `.idx` files do not identify the namespace root they cover,
so exact-root query and search rebuild their working indexes from the selected
configuration, FileRecords, and bodies instead of trusting those files as
result authority. The v3 root-level parser registry is detached from namespace
HEAD; if a non-JSON content query would require proving that this registry did
not select a plugin, AeorDB fails closed with `503
HISTORICAL_VIEW_UNAVAILABLE` rather than guessing parser semantics or returning
stale results. Root-bound JSON and metadata fields are unaffected.

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

Combinators can be nested up to **32 levels deep**. Queries exceeding this
depth are rejected with a `400` error. A page can return at most **1,000
results**; larger requested limits are rejected instead of being silently
truncated. The default limit is 100.

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
  "root": {
    "hash": "9f26...",
    "state": "live",
    "expires_at": null
  },
  "items": [
    {
      "path": "/users/alice.json",
      "size": 256,
      "content_type": "application/json",
      "created_at": 1775968398000,
      "updated_at": 1775968398000,
      "score": 1.0,
      "matched_by": ["name"]
    }
  ],
  "has_more": true,
  "total": 150,
  "next_cursor": "QVBPUwE...",
  "prev_cursor": "QVBPUwE...",
  "limit": 20,
  "offset": 0
}
```

| Field | Type | Description |
|-------|------|-------------|
| `root` | object | Exact selected namespace root: `hash`, `state`, and nullable `expires_at` |
| `items` | array | Matching file metadata with scores |
| `has_more` | boolean | Whether more results exist beyond the current page |
| `total` | integer | Total matching results (only if `include_total: true`) |
| `next_cursor` | string | Canonical APOS cursor for the next page (if `has_more` is true) |
| `prev_cursor` | string | Canonical APOS cursor for the previous page |
| `limit` | integer | Effective page limit |
| `offset` | integer | Effective absolute offset for page/offset origins; omitted for APOS origins |
| `default_limit_hit` | boolean | Present and true when an omitted limit truncated the response |
| `default_limit` | integer | The server's default limit value (present with `default_limit_hit`) |

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
| `file_key` | string | Selected-root file identity, present when `include_matches` is true |
| `record_revision` | string | Exact selected-root FileRecord revision, present when `include_matches` is true |
| `content_hash` | string | Whole-file content hash, present when `include_matches` is true and the record has a hash |
| `matches` | array | Hit locators, present when `include_matches` is true |
| `matches_truncated` | boolean | True if more locators were available than returned |
| `locator_status` | string | `complete`, `partial`, or `unsupported` |

### Hit Locators

When `include_matches` is true, `/files/query` and `/files/search` add bounded hit locators to each returned result. This is designed for agents that need to search first and then fetch only relevant ranges.

For JSON files, matching indexed fields are reported as `field-value` locators with JSON Pointer fetch hints:

```json
{
  "path": "/users/alice.json",
  "file_key": "0f4a...",
  "record_revision": "8c21...",
  "content_hash": "b3c1...",
  "matches": [
    {
      "id": "m_0001",
      "query": "Alice",
      "matched_text": "Alice",
      "field": "name",
      "operator": "eq",
      "source": {
        "type": "field-value",
        "field": "name",
        "json_pointer": "/name",
        "value_type": "string"
      },
      "range": {
        "char": { "start": 0, "end": 5, "unit": "unicode-scalar", "basis": "field-value" }
      },
      "fetch": {
        "json_pointer": "/name",
        "preferred": "json_pointer"
      },
      "snippet": {
        "text": "Alice",
        "highlight": [{ "start": 0, "end": 5, "unit": "unicode-scalar" }],
        "truncated_before": false,
        "truncated_after": false
      },
      "confidence": "exact",
      "scan_status": "complete"
    }
  ],
  "matches_truncated": false,
  "locator_status": "complete"
}
```

Metadata matches such as `@filename` use `source.type = "metadata"`. Plain-text or unstructured UTF-8 file matches use `source.type = "stored-file"` and include byte, character, line, and column ranges plus `byte_range` and `line_range` fetch hints.

Use `POST /files/fetch` range mode to fetch the returned ranges. Copy the
query/search response's `root.hash` into the fetch request's `root_hash`, and
pass the result's `content_hash` as `if_content_hash`. The `file_key`,
`record_revision`, and `content_hash` identify the exact selected-root hit;
the content assertion detects a mismatched follow-up. Omitting `root_hash`
would fetch the same path from current HEAD instead of guaranteeing continuity.

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

`next_cursor` and `prev_cursor` are canonical unpadded base64url APOS v1
records. They are bound to the route, exact root, complete logical order,
FileKey, and RecordRevision; they are not legacy JSON cursors. Use `after` or
`before` with the exact `root.hash` returned by the previous response:

```json
{
  "root_hash": "9f26...",
  "limit": 20,
  "after": "QVBPUwE..."
}
```

Exactly one origin may be supplied: one-based `page`, zero-based `offset`,
`after`, or `before`. Any origin may be combined with `limit`. APOS root,
route, or order drift is rejected; AeorDB does not decode or fall back to the
old JSON/base64 cursor format.

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
| `@file_key` | `file_key` |
| `@record_revision` | `record_revision` |
| `@content_hash` | `content_hash` |
| `@matches` | `matches` |

Envelope fields (`root`, `has_more`, `next_cursor`, `total`, `limit`, and `offset`) are never stripped by projection.

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

The response shape depends on whether `group_by` is used. Aggregation results
are returned as a JSON object with the same exact `root` metadata. Group pages
use aggregate-group APOS tokens bound to the canonical group tuple rather than
to a fictitious file.

---

## Explain Mode

Inspect the authorization-filtered logical execution plan. Logical EXPLAIN
describes requested fields, operations, driver/coverage/work classes, and
exact-recheck requirements without exposing hidden paths, values, counts, or
physical offsets, pages, manifests, NVT state, or raw cardinality.

```json
{
  "path": "/users",
  "where": {"field": "age", "op": "gt", "value": 21},
  "explain": "plan"
}
```

| Value | Description |
|-------|-------------|
| `true` or `"plan"` | Show the logical plan without returning query results |
| `"analyze"` | Execute the authorized query and include its result envelope and bounded execution summary |

---

## Virtual Fields

Virtual fields let you query file metadata directly. Prefix a field name with `@` to query against built-in file attributes instead of indexed document fields.

AeorDB uses virtual-field indexes when they are configured. New databases bootstrap default virtual-field indexes at `/.aeordb-config/indexes.json`; older databases can add the same config and run a forced reindex to backfill them. If no index exists for a supported virtual field, AeorDB falls back to scanning FileRecord metadata for compatibility.

### Available Virtual Fields

| Field | Type | Description |
|-------|------|-------------|
| `@path` | string | Full file path (e.g., `/docs/report.pdf`) |
| `@filename` | string | Filename only -- the last segment of the path (e.g., `report.pdf`) |
| `@file_name` | string | Alias for `@filename`; queries use the canonical `@filename` index |
| `@extension` | string | File extension after the last `.` (e.g., `pdf`) |
| `@content_type` | string | MIME type (e.g., `application/pdf`) |
| `@hash` | string | Raw whole-file content hash (`blake3(file bytes)`) |
| `@size` | u64 | File size in bytes |
| `@created_at` | i64 | Creation timestamp in milliseconds |
| `@updated_at` | i64 | Last update timestamp in milliseconds |

### Supported Operators

**String virtual fields** (`@path`, `@filename`, `@file_name`, `@extension`, `@content_type`, `@hash`) support:

`eq`, `contains`, `in`, `gt`, `lt`, `similar` (trigram), `fuzzy` (edit distance), `phonetic` (soundex/metaphone), `match` (fused multi-strategy)

**Numeric virtual fields** (`@size`, `@created_at`, `@updated_at`) support:

`eq`, `gt`, `lt`, `between`, `in`

### How Virtual Fields Work

Virtual fields are derived from the FileRecord header and payload. Query execution is index-first:

- **Default indexes.** New databases index every built-in virtual field for exact lookup by default. `@filename` also gets trigram and phonetic indexes for `contains`, `similar`, `fuzzy`, `phonetic`, and `match`.
- **Ancestor glob indexes.** If a query is scoped below a glob-indexed directory, AeorDB can use the ancestor index and filter candidates back down to the requested path.
- **Scan fallback.** If a database does not have a matching virtual-field index, AeorDB scans FileRecords under the query path so legacy queries continue to work. This fallback is O(n) over files under the path.
- **Combinable with indexed fields.** You can mix virtual fields and indexed fields in the same `where` clause using boolean combinators (`and`, `or`, `not`).
- **Canonical aliases.** `@file_name` is accepted as an alias but is indexed and queried as `@filename`.

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

## Global Search

Search across all indexed directories in the database.

### POST /files/search

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `query` | string | No | Broad search — searched against all trigram, phonetic, soundex, and dmetaphone indexed fields |
| `where` | object | No | Structured query filter (same syntax as `/files/query`) |
| `path` | string | No | Scope search to a subtree (default: `/` = everything) |
| `root_hash` | string | No | Exact lowercase namespace-root hash; mutually exclusive with `snapshot` and `version` |
| `snapshot` | string | No | Named snapshot selector; mutually exclusive with `root_hash` and `version` |
| `version` | string | No | Exact namespace-root hash through the legacy version alias; mutually exclusive with `root_hash` and `snapshot` |
| `limit` | integer | No | Max results (default: 50, max: 1000) |
| `page` | integer | No | One-based page origin; mutually exclusive with `offset`, `after`, and `before` |
| `offset` | integer | No | Skip results |
| `after` | string | No | Canonical APOS cursor for forward pagination; requires explicit `root_hash` |
| `before` | string | No | Canonical APOS cursor for backward pagination; requires explicit `root_hash` |
| `include_matches` | boolean | No | Include request-time hit locators for returned results (default: false) |
| `max_matches_per_result` | integer | No | Maximum hit locators per result (default: 5, max: 50) |
| `snippet_chars` | integer | No | Maximum snippet characters per locator (default: 160, max: 4096) |
| `match_context_lines` | integer | No | Context line count for stored-file line fetch hints |
| `max_locator_scan_bytes` | integer | No | Caller cap for stored-file locator scans; server clamps to its hard cap |

At least one of `query` or `where` is required.

### Broad Search

Discovers all directories with fuzzy-capable indexes (trigram, phonetic, soundex, dmetaphone) and searches the term against every matching field:

```bash
curl -X POST http://localhost:6830/files/search \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"query": "alice", "limit": 20}'
```

### Structured Search

Same `where` clause syntax as `/files/query`, but searches across all directories that have the requested field indexed:

```bash
curl -X POST http://localhost:6830/files/search \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"where": {"field": "@size", "op": "gt", "value": 1048576}}'
```

### Combined

Broad search filtered by structured conditions:

```bash
curl -X POST http://localhost:6830/files/search \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"query": "report", "where": {"field": "@extension", "op": "eq", "value": "pdf"}}'
```

### Response

The search envelope uses `results` and always includes `total_count`, plus the
same exact `root`, page metadata, and canonical APOS cursors as query. Each
result has a `source` field indicating which directory's index matched. The
locator fields below are present only when `include_matches` is true:

```json
{
  "root": {
    "hash": "9f26...",
    "state": "live",
    "expires_at": null
  },
  "results": [
    {
      "path": "/users/alice.json",
      "score": 0.95,
      "matched_by": ["@filename"],
      "source": "/",
      "size": 256,
      "content_type": "application/json",
      "created_at": 1775968398000,
      "updated_at": 1775968398000,
      "file_key": "0f4a...",
      "record_revision": "8c21...",
      "content_hash": "b3c1...",
      "matches": [],
      "matches_truncated": false,
      "locator_status": "complete"
    }
  ],
  "has_more": false,
  "total_count": 1
}
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
