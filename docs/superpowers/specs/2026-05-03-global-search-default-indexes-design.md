# Global Search Endpoint + Default Indexes

## Goal

A global search endpoint (`POST /files/search`) that searches across all indexed directories, plus built-in default indexes on every file's metadata (filename, hash, timestamps, size, content type) so the entire database is searchable out of the box.

## 1. Default Global Indexes

### Built-In Config

On first boot (or upgrade), the engine writes a default `.config/indexes.json` at `/` if one doesn't exist:

```json
{
  "glob": "**/*",
  "indexes": [
    {"name": "@filename", "type": ["string", "trigram", "phonetic", "dmetaphone"]},
    {"name": "@hash", "type": "trigram"},
    {"name": "@created_at", "type": "timestamp"},
    {"name": "@updated_at", "type": "timestamp"},
    {"name": "@size", "type": "u64"},
    {"name": "@content_type", "type": "string"}
  ]
}
```

This triggers a full background reindex of every file in the database.

### `@`-Prefixed Field Sources

The indexing pipeline recognizes `@`-prefixed field names as metadata fields extracted from the FileRecord, not from the file's parsed content:

| Field | Source | Type |
|-------|--------|------|
| `@filename` | Last segment of `FileRecord.path` | string + trigram + phonetic + dmetaphone |
| `@hash` | Hex-encoded content hash from FileRecord's first chunk hash (or identity hash) | trigram |
| `@created_at` | `FileRecord.created_at` | timestamp |
| `@updated_at` | `FileRecord.updated_at` | timestamp |
| `@size` | `FileRecord.total_size` | u64 |
| `@content_type` | `FileRecord.content_type` | string |

No parser is invoked for `@` fields — the values come directly from the FileRecord metadata. This means even binary files (images, PDFs, videos) get indexed by filename, hash, size, timestamps, and content type without needing a parser.

### Storage

All default indexes live at `/.indexes/`. Every file in the database has entries in these indexes.

### Bootstrap Behavior

1. On server start, check if `/.config/indexes.json` exists.
2. If missing: write the default config. This triggers an automatic reindex task for `/` with `glob: "**/*"`.
3. If present: do nothing (config already exists from a previous boot). The user may have customized it.
4. The reindex runs as a background task — the server is immediately available for requests.

### User Indexes Are Additive

User-created index configs at specific directories (e.g., `/users/.config/indexes.json`) create separate indexes at `/users/.indexes/`. The global defaults at `/.indexes/` are independent. Both sets of indexes are searchable.

## 2. `@hash` Virtual Field

Add `@hash` to the existing virtual field system in the query engine:

| Field | Type | Description |
|-------|------|-------------|
| `@hash` | string | Hex-encoded content hash of the file |

Supports: `eq`, `contains`, `similar`, `fuzzy`, `in`

## 3. Global Search Endpoint

### `POST /files/search`

Searches across all indexed directories. Accepts the same `where` clause syntax as `/files/query`.

### Request Body

```json
{
  "query": "alice",
  "where": { ... },
  "path": "/",
  "limit": 50,
  "offset": 0,
  "order_by": [{"field": "@score", "direction": "desc"}],
  "include_total": false,
  "select": ["@path", "@score", "@matched_by"]
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `query` | string | No | Broad search term — searched against ALL trigram, phonetic, soundex, and dmetaphone indexed fields |
| `where` | object/array | No | Structured query filter (same syntax as `/files/query`) |
| `path` | string | No | Scope search to a subtree (default: `/` = everything) |
| `limit` | integer | No | Max results (default: 50) |
| `offset` | integer | No | Skip results |
| `order_by` | array | No | Sort fields |
| `include_total` | boolean | No | Include total count |
| `select` | array | No | Project specific fields |

At least one of `query` or `where` is required.

### Three Modes

**Broad search** — `query` only:
```json
{"query": "alice", "limit": 20}
```
Discovers all directories with trigram/phonetic/soundex/dmetaphone indexes, runs the term against every fuzzy-capable field, merges by score.

**Structured search** — `where` only:
```json
{"where": {"field": "age", "op": "gt", "value": 23}}
```
Discovers all directories that have an index for the specified field(s), runs the where clause, merges results.

**Combined** — `query` + `where`:
```json
{"query": "alice", "where": {"field": "@extension", "op": "eq", "value": "json"}}
```
Broad search results filtered by the structured where clause.

### Response Format

Same as `/files/query`, plus `source` per result:

```json
{
  "results": [
    {
      "path": "/users/alice.json",
      "score": 0.95,
      "matched_by": ["@filename", "name"],
      "source": "/users/",
      "size": 256,
      "content_type": "application/json",
      "created_at": 1775968398000,
      "updated_at": 1775968398000
    }
  ],
  "has_more": true,
  "total_count": 150
}
```

### Implementation

1. **Discover indexed directories:** Scan for all directories containing `.indexes/` subdirectories. Cache this list (invalidate when new index configs are stored).
2. **Broad search fan-out:** For each indexed directory, load all field indexes of type trigram, phonetic, soundex, or dmetaphone. Run the `query` term against each using `similar` operator. Collect results with scores.
3. **Structured search fan-out:** For each indexed directory, check if it has the requested field index. If yes, run the `where` clause. If not, skip.
4. **Merge:** Combine results from all directories. Deduplicate by file path (keep highest score). Sort by score descending (or user-specified `order_by`). Apply pagination.
5. **Path scoping:** When `path` is specified, only search directories under that path.

### Performance Considerations

- The global default indexes at `/.indexes/` cover every file. A broad search against just `/.indexes/` may be sufficient for most queries — no need to fan out to per-directory indexes unless the user has custom fields.
- For the first implementation, searching only `/.indexes/` (the defaults) + any per-directory indexes that have the requested field is a reasonable optimization.
- The index directory cache prevents repeated directory scans on every search.

## 4. Future: Admin Configuration

A `/.config/search.json` (or Settings page UI) to control:
- Which fields are included in broad search
- Excluded directories
- Default result limits
- Broad search scoring weights

Not in this implementation — noted for future work.

## Files Affected

| File | Change |
|------|--------|
| `aeordb-lib/src/engine/indexing_pipeline.rs` | Handle `@`-prefixed fields by extracting from FileRecord metadata instead of parsed JSON |
| `aeordb-lib/src/engine/query_engine.rs` | Add `@hash` virtual field |
| `aeordb-lib/src/server/engine_routes.rs` | New `POST /files/search` endpoint |
| `aeordb-lib/src/server/mod.rs` | Register `/files/search` route |
| `aeordb-lib/src/engine/search.rs` (new) | Global search logic: discover indexes, fan-out, merge, deduplicate |
| `aeordb-lib/src/engine/mod.rs` | Add `pub mod search` |
| `aeordb-cli/src/commands/start.rs` | Bootstrap: write default index config on first boot |
| `docs/src/api/querying.md` | Document `/files/search` endpoint |
| `docs/src/concepts/indexing.md` | Document default indexes and `@` field indexing |
