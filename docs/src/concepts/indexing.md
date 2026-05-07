# Indexing & Queries

AeorDB indexes are opt-in and configured per-directory. Nothing is indexed by default -- you control exactly which fields are indexed, with which strategies, and for which file types. This keeps the engine lean and predictable.

## Index Configuration

Create a `.config/indexes.json` file in any directory to define indexes for files in that directory:

```bash
curl -X PUT http://localhost:6830/files/users/.config/indexes.json \
  -H "Content-Type: application/json" \
  -d '{
    "indexes": [
      {"name": "name", "type": ["string", "trigram"]},
      {"name": "age", "type": "u64"},
      {"name": "city", "type": "string"},
      {"name": "email", "type": "trigram"},
      {"name": "created_at", "type": "timestamp"}
    ]
  }'
```

When this file is created or updated, the engine automatically triggers a background reindex of all existing files in the directory.

### Subdirectory Indexing with Glob

By default, an index config only indexes direct children of its directory. To index files across subdirectories, add a `glob` field:

```bash
curl -X PUT http://localhost:6830/files/sessions/.config/indexes.json \
  -H "Content-Type: application/json" \
  -d '{
    "glob": "*/session.json",
    "indexes": [
      {"name": "patient_name", "type": ["string", "trigram"]},
      {"name": "notes", "type": "trigram", "source": ["comments", "", "text"]}
    ]
  }'
```

This config at `/sessions/` indexes all `session.json` files in immediate subdirectories (e.g., `/sessions/s1/session.json`, `/sessions/s2/session.json`).

**Glob patterns:**
- `*` — matches one directory level (`*/file.json` matches `subdir/file.json`)
- `**` — matches any depth (`**/*.json` matches `a/b/c/file.json`)
- `?` — matches a single character

When a file is stored, the engine checks ancestor directories for glob configs. The nearest matching ancestor's config is used. Indexes are stored at the config owner's directory, so querying `/sessions/` finds results from all matching subdirectories.

Reindexing with a glob config recursively scans all subdirectories and filters by the glob pattern.

## Index Types

| Type | Order-Preserving | Description |
|------|-----------------|-------------|
| `u64` | Yes | Unsigned 64-bit integer. Range-tracking with observed min/max. |
| `i64` | Yes | Signed 64-bit integer. Shifted to [0.0, 1.0] for NVT storage. |
| `f64` | Yes | 64-bit floating point. Clamping for NaN/Inf handling. |
| `string` | Partially | Exact string matching. Multi-stage scalar: first byte weighted + length. |
| `timestamp` | Yes | UTC millisecond timestamps. Range-tracking. |
| `trigram` | No | Trigram-based fuzzy text matching. Tolerates typos, supports substring search. |
| `phonetic` | No | General phonetic matching (Soundex algorithm). |
| `soundex` | No | Soundex encoding for English names. |
| `dmetaphone` | No | Double Metaphone for multi-cultural phonetic matching. |

## Multi-Strategy Indexes

A single field can be indexed with multiple strategies by passing `type` as an array:

```json
{"name": "title", "type": ["string", "trigram", "phonetic"]}
```

This creates three separate index files for the same field:
- `title.string.idx` -- exact match queries
- `title.trigram.idx` -- fuzzy/substring queries
- `title.phonetic.idx` -- phonetic queries

Use the appropriate query operator to target the desired index.

## How Indexes Work

AeorDB uses a Normalized Vector Table (NVT) for index lookups. Each indexed field gets its own NVT.

### The NVT Approach

1. A `ScalarConverter` maps each field value to a scalar in [0.0, 1.0]
2. The scalar maps to a bucket in the NVT
3. The bucket points to the matching entries

For numeric types (u64, i64, f64, timestamp), the converter tracks the observed min/max and distributes values uniformly across the [0.0, 1.0] range. This means range queries (`gt`, `lt`, `between`) are efficient -- they resolve to a contiguous range of buckets.

For a query like `WHERE age > 30`:
1. `converter.to_scalar(30)` computes where 30 falls in the bucket range
2. All buckets after that point are candidates
3. Only those buckets are scanned

This is O(1) for the bucket lookup, with a small linear scan within the bucket.

### Two-Tier Execution

Simple queries (single field, direct comparison) use direct scalar lookups -- no bitmaps, no compositing. Most queries fall into this tier.

Complex queries (OR, NOT, multi-field boolean logic) build NVT bitmaps and composite them:
- Each field condition produces a bitmask over the NVT buckets
- `AND` = bitwise AND of masks
- `OR` = bitwise OR of masks
- `NOT` = bitwise NOT of a mask
- The final mask identifies which buckets contain results

Memory usage is bounded: a bitmask for 1M buckets is only 128KB, regardless of how many entries exist.

## Source Resolution

By default, the `name` field in an index definition is used as the JSON key to extract the value:

```json
{"name": "age", "type": "u64"}
```

This extracts the `age` field from `{"name": "Alice", "age": 30}`.

### Nested Fields

For nested JSON or parser output, use the `source` array to specify the path:

```json
{"name": "author", "source": ["metadata", "author"], "type": "string"}
```

This extracts `metadata.author` from a JSON structure like:
```json
{"metadata": {"author": "Jane Smith", "title": "Report"}}
```

The `source` array supports:
- String segments for object key lookup: `["metadata", "author"]`
- Integer segments for array index access: `["items", 0, "name"]`

### Array Fan-Out

To index every element in an array, use an empty string `""` as a source segment. This "fans out" — creating one index entry per array element:

```json
{"name": "tag", "type": "trigram", "source": ["tags", ""]}
```

For `{"tags": ["rust", "database", "aeordb"]}`, this creates **three** index entries for the same file. A query for `tag = "rust"` will find this file.

Fan-out works on objects too — `""` iterates all values:

```json
{"name": "value", "type": "string", "source": [""]}
```

For `{"color": "red", "size": "large"}`, this creates entries for both `"red"` and `"large"`.

Fan-out can be chained for nested structures:

```json
{"name": "comment_text", "type": "trigram", "source": ["comments", "", "text"]}
```

For `{"comments": [{"text": "hello"}, {"text": "world"}]}`, this creates entries for `"hello"` and `"world"`.

### Regex Filtering

Use a regex segment `/pattern/flags` to filter which keys (on objects) or elements (on arrays) are included:

```json
{"name": "tag_value", "type": "string", "source": ["/^tag_/"]}
```

For `{"tag_color": "red", "tag_size": "large", "name": "foo"}`, this creates entries for `"red"` and `"large"` (keys matching `/^tag_/`), but not `"foo"`.

Supported flags:
- `i` — case-insensitive matching

```json
{"name": "name_field", "type": "string", "source": ["/^name$/i"]}
```

Matches keys `name`, `Name`, `NAME`, etc.

### Plugin Mapper

For complex extraction logic, delegate to a WASM plugin:

```json
{
  "name": "summary",
  "source": {"plugin": "my-mapper", "args": {"mode": "summary", "max_length": 500}},
  "type": "trigram"
}
```

The plugin receives the parsed JSON and the `args` object, and returns the extracted field value.

## Parser Integration

For non-JSON files (PDFs, images, XML, etc.), a parser converts raw bytes into a JSON object that the indexing pipeline can work with.

### Native Parsers (Built-In)

AeorDB ships with 8 native parsers that handle common formats automatically -- no deployment required. During indexing, the engine tries native parsers first based on content type (with extension-based fallback for `application/octet-stream`). Supported formats include text, HTML/XML, PDF, images (JPEG/PNG/GIF/BMP/WebP/TIFF/SVG), audio (MP3/WAV/OGG), video (MP4/AVI/WebM/MKV/FLV), MS Office (DOCX/XLSX), and ODF (ODT/ODS). See [Plugin Endpoints -- Native Parsers](../api/plugins.md#native-parsers) for the full list.

If no native parser handles the content type, the engine falls through to the WASM plugin system.

### WASM Parser Plugins

For custom or proprietary formats not covered by the built-in parsers, deploy a WASM parser plugin. WASM parsers receive the same input envelope and return the same JSON structure as native parsers.

### Configuration

```json
{
  "parser": "pdf-extractor",
  "parser_memory_limit": "256mb",
  "indexes": [
    {"name": "title", "source": ["metadata", "title"], "type": ["string", "trigram"]},
    {"name": "author", "source": ["metadata", "author"], "type": "phonetic"},
    {"name": "content", "source": ["text"], "type": "trigram"},
    {"name": "page_count", "source": ["metadata", "page_count"], "type": "u64"}
  ]
}
```

The parser receives a JSON envelope with the file data (base64-encoded) and metadata:

```json
{
  "data": "<base64-encoded file bytes>",
  "meta": {
    "filename": "report.pdf",
    "path": "/docs/reports/report.pdf",
    "content_type": "application/pdf",
    "size": 1048576
  }
}
```

The parser returns a JSON object (like `{"text": "...", "metadata": {"title": "...", ...}}`), and the `source` paths in each index definition walk this JSON to extract field values.

### Global Parser Registry

You can also register parsers globally by content type at `/.config/parsers.json`:

```json
{
  "application/pdf": "pdf-extractor",
  "image/jpeg": "image-metadata",
  "image/png": "image-metadata"
}
```

When a file is stored and no parser is configured in the directory's index config, the engine checks this registry using the file's content type.

### Failure Handling

Parser and indexing failures never prevent file storage. The file is always stored regardless of parse/index errors. If logging is enabled in the index config (`"logging": true`), errors are written to `.logs/` under the directory.

## Default Indexes

On first server start, AeorDB bootstraps a default index configuration at `/.config/indexes.json` with `glob: "**/*"`. This automatically indexes every file's metadata across the entire database:

| Field | Index Types | Description |
|-------|------------|-------------|
| `@filename` | string, trigram, phonetic, dmetaphone | File name (last path segment). Supports exact match, fuzzy search, and phonetic matching. |
| `@hash` | trigram | Content hash. Supports substring and similarity search. |
| `@created_at` | timestamp | Creation time. Supports range queries. |
| `@updated_at` | timestamp | Last update time. Supports range queries. |
| `@size` | u64 | File size in bytes. Supports range queries. |
| `@content_type` | string | MIME type. Supports exact match. |

These indexes are stored at `/.indexes/` and cover every file in the database. Because the bootstrap config uses `glob: "**/*"`, the global search endpoint ([`POST /files/search`](../api/files.md#global-search)) works out of the box with no additional configuration.

### @-Field Source Resolution

When the indexing pipeline encounters a field name starting with `@`, it extracts the value from the file's metadata (FileRecord) instead of parsing the file content. This means even binary files (images, videos, PDFs) are indexed by filename, hash, timestamps, size, and content type without needing a parser.

### Customization

The default config at `/.config/indexes.json` is only written on first boot. You can modify it to add or remove default fields. Changes trigger an automatic reindex.

## Automatic Reindexing

When you store or update a `.config/indexes.json` file, the engine automatically enqueues a background reindex task for that directory. The task:

1. Reads the current index config
2. Lists all files in the directory
3. Re-runs the indexing pipeline for each file (in batches of 50, yielding between batches)
4. Reports progress via `GET /system/tasks`

During reindexing, queries still work but may return incomplete results. The query response includes a `meta.reindexing` field with the current progress:

```json
{
  "results": [...],
  "meta": {
    "reindexing": 0.67,
    "reindexing_eta": 1775968398803,
    "reindexing_indexed": 6700,
    "reindexing_total": 10000
  }
}
```

## Query API

Queries are submitted as `POST /files/query` with a JSON body:

```json
{
  "path": "/users/",
  "where": {
    "and": [
      {"field": "age", "op": "gt", "value": 30},
      {"field": "city", "op": "eq", "value": "Portland"},
      {"not": {"field": "role", "op": "eq", "value": "banned"}}
    ]
  },
  "sort": {"field": "age", "order": "desc"},
  "limit": 50,
  "offset": 0
}
```

### Boolean Logic

The `where` clause supports full boolean logic:

```json
{
  "where": {
    "or": [
      {"field": "city", "op": "eq", "value": "Portland"},
      {
        "and": [
          {"field": "age", "op": "gt", "value": 25},
          {"field": "city", "op": "eq", "value": "Seattle"}
        ]
      }
    ]
  }
}
```

For backward compatibility, a flat array in `where` is treated as an implicit `and`:

```json
{
  "where": [
    {"field": "age", "op": "gt", "value": 30},
    {"field": "city", "op": "eq", "value": "Portland"}
  ]
}
```

### Query Operators

| Operator | Description | Value Type |
|----------|-------------|-----------|
| `eq` | Equals | any |
| `gt` | Greater than | numeric, timestamp |
| `gte` | Greater than or equal | numeric, timestamp |
| `lt` | Less than | numeric, timestamp |
| `lte` | Less than or equal | numeric, timestamp |
| `between` | Inclusive range | `[min, max]` |
| `fuzzy` | Trigram fuzzy match | string (requires trigram index) |
| `phonetic` | Phonetic match | string (requires phonetic/soundex/dmetaphone index) |

## Next Steps

- [Quick Start](../getting-started/quick-start.md) -- hands-on tutorial with query examples
- [Configuration](../getting-started/configuration.md) -- full index config reference
- [Storage Engine](./storage-engine.md) -- how indexed data is stored
