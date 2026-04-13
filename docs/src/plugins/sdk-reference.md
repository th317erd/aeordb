# Plugin SDK Reference

Complete type reference for the `aeordb-plugin-sdk` crate. This covers every public struct, enum, trait, and method available to plugin authors.

## Macros

### `aeordb_parser!(fn_name)`

Generates WASM exports for a parser plugin. Your function must have the signature:

```rust
fn fn_name(input: ParserInput) -> Result<serde_json::Value, String>
```

Generated exports:
- `handle(ptr: i32, len: i32) -> i64` -- deserializes the parser envelope, calls your function, returns packed pointer+length to the serialized response

### `aeordb_query_plugin!(fn_name)`

Generates WASM exports for a query plugin. Your function must have the signature:

```rust
fn fn_name(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError>
```

Generated exports:
- `alloc(size: i32) -> i32` -- allocates guest memory for the host to write request data
- `handle(ptr: i32, len: i32) -> i64` -- deserializes the request, creates a `PluginContext`, calls your function, returns packed pointer+length to the serialized response

## Prelude

Import everything you need with:

```rust
use aeordb_plugin_sdk::prelude::*;
```

This re-exports: `PluginError`, `PluginRequest`, `PluginResponse`, `ParserInput`, `FileMeta`, `PluginContext`, `FileData`, `DirEntry`, `FileMetadata`, `QueryResult`, `AggregateResult`, `SortDirection`.

---

## `PluginRequest`

Request passed to a query plugin when it is invoked.

| Field | Type | Description |
|-------|------|-------------|
| `arguments` | `Vec<u8>` | Raw argument bytes (e.g., the HTTP request body forwarded to the plugin) |
| `metadata` | `HashMap<String, String>` | Key-value metadata about the invocation context |

Common metadata keys:

| Key | Description |
|-----|-------------|
| `function_name` | The function name from the invoke URL |

---

## `PluginResponse`

Response returned by a plugin after handling a request.

| Field | Type | Description |
|-------|------|-------------|
| `status_code` | `u16` | HTTP-style status code |
| `body` | `Vec<u8>` | Raw response body bytes |
| `content_type` | `Option<String>` | MIME content type of the body |
| `headers` | `HashMap<String, String>` | Additional response headers |

### Builder Methods

#### `PluginResponse::json(status_code: u16, body: &T) -> Result<Self, serde_json::Error>`

Serializes `body` (any `Serialize` type) to JSON. Sets `content_type` to `"application/json"`.

```rust
PluginResponse::json(200, &serde_json::json!({"ok": true}))
```

#### `PluginResponse::text(status_code: u16, body: impl Into<String>) -> Self`

Creates a plain text response. Sets `content_type` to `"text/plain"`.

```rust
PluginResponse::text(200, "Hello, world!")
```

#### `PluginResponse::error(status_code: u16, message: impl Into<String>) -> Self`

Creates a JSON error response: `{"error": "<message>"}`. Sets `content_type` to `"application/json"`.

```rust
PluginResponse::error(404, "not found")
```

---

## `PluginError`

Error enum for the plugin system.

| Variant | Description |
|---------|-------------|
| `NotFound(String)` | The plugin could not be found |
| `ExecutionFailed(String)` | The plugin failed during execution |
| `SerializationFailed(String)` | Request or response could not be serialized/deserialized |
| `ResourceLimitExceeded(String)` | Plugin exceeded memory, fuel, or other resource limits |
| `InvalidModule(String)` | An invalid or corrupt WASM module was provided |
| `Internal(String)` | A generic internal error |

All variants carry a `String` message. `PluginError` implements `Display`, `Debug`, and `Error`.

---

## `PluginContext`

Guest-side handle for calling AeorDB host functions from WASM. Created automatically by `aeordb_query_plugin!` and passed to the handler.

On non-WASM targets (native compilation), all methods return `PluginError::ExecutionFailed` -- this allows IDE support and unit testing of plugin logic without a WASM runtime.

### File Operations

#### `read_file(&self, path: &str) -> Result<FileData, PluginError>`

Read a file at the given path. Returns the decoded file bytes, content type, and size.

#### `write_file(&self, path: &str, data: &[u8], content_type: &str) -> Result<(), PluginError>`

Write (create or overwrite) a file. Data is base64-encoded on the wire automatically.

#### `delete_file(&self, path: &str) -> Result<(), PluginError>`

Delete a file at the given path.

#### `file_metadata(&self, path: &str) -> Result<FileMetadata, PluginError>`

Retrieve metadata for a file without reading its contents.

#### `list_directory(&self, path: &str) -> Result<Vec<DirEntry>, PluginError>`

List directory entries at the given path.

### Query and Aggregation

#### `query(&self, path: &str) -> QueryBuilder`

Start building a query against files at the given path. See [QueryBuilder](#querybuilder).

#### `aggregate(&self, path: &str) -> AggregateBuilder`

Start building an aggregation against files at the given path. See [AggregateBuilder](#aggregatebuilder).

---

## `FileData`

Raw file data returned by `read_file`.

| Field | Type | Description |
|-------|------|-------------|
| `data` | `Vec<u8>` | Decoded file bytes |
| `content_type` | `String` | MIME content type |
| `size` | `u64` | File size in bytes |

---

## `DirEntry`

A single directory entry returned by `list_directory`.

| Field | Type | Description |
|-------|------|-------------|
| `name` | `String` | Entry name (file or directory name, not the full path) |
| `entry_type` | `String` | `"file"` or `"directory"` |
| `size` | `u64` | Size in bytes (0 for directories, defaults to 0 if absent) |

---

## `FileMetadata`

Metadata about a stored file.

| Field | Type | Description |
|-------|------|-------------|
| `path` | `String` | Full storage path |
| `size` | `u64` | File size in bytes |
| `content_type` | `Option<String>` | MIME content type (if known) |
| `created_at` | `i64` | Creation timestamp (ms since epoch) |
| `updated_at` | `i64` | Last update timestamp (ms since epoch) |

---

## `ParserInput`

Input to a parser function.

| Field | Type | Description |
|-------|------|-------------|
| `data` | `Vec<u8>` | Raw file bytes (base64-decoded from the wire envelope) |
| `meta` | `FileMeta` | File metadata |

---

## `FileMeta`

Metadata about the file being parsed (available inside parser plugins).

| Field | Type | Description |
|-------|------|-------------|
| `filename` | `String` | File name only (e.g., `"report.pdf"`) |
| `path` | `String` | Full storage path (e.g., `"/docs/reports/report.pdf"`) |
| `content_type` | `String` | MIME type |
| `size` | `u64` | Raw file size in bytes |
| `hash` | `String` | Hex-encoded content hash (may be empty) |
| `hash_algorithm` | `String` | Hash algorithm (e.g., `"blake3_256"`, may be empty) |
| `created_at` | `i64` | Creation timestamp (ms since epoch, default 0) |
| `updated_at` | `i64` | Last update timestamp (ms since epoch, default 0) |

---

## `QueryBuilder`

Fluent builder for constructing AeorDB queries. Obtained via `PluginContext::query(path)` or `QueryBuilder::new(path)`.

### Field Conditions

Start with `.field("name")` to get a `FieldQueryBuilder`, then chain one operator:

#### Equality

| Method | Signature | Description |
|--------|-----------|-------------|
| `eq` | `(value: &[u8]) -> QueryBuilder` | Exact match on raw bytes |
| `eq_u64` | `(value: u64) -> QueryBuilder` | Exact match on u64 |
| `eq_i64` | `(value: i64) -> QueryBuilder` | Exact match on i64 |
| `eq_f64` | `(value: f64) -> QueryBuilder` | Exact match on f64 |
| `eq_str` | `(value: &str) -> QueryBuilder` | Exact match on string |
| `eq_bool` | `(value: bool) -> QueryBuilder` | Exact match on boolean |

#### Greater Than

| Method | Signature | Description |
|--------|-----------|-------------|
| `gt` | `(value: &[u8]) -> QueryBuilder` | Greater than on raw bytes |
| `gt_u64` | `(value: u64) -> QueryBuilder` | Greater than on u64 |
| `gt_str` | `(value: &str) -> QueryBuilder` | Greater than on string |
| `gt_f64` | `(value: f64) -> QueryBuilder` | Greater than on f64 |

#### Less Than

| Method | Signature | Description |
|--------|-----------|-------------|
| `lt` | `(value: &[u8]) -> QueryBuilder` | Less than on raw bytes |
| `lt_u64` | `(value: u64) -> QueryBuilder` | Less than on u64 |
| `lt_str` | `(value: &str) -> QueryBuilder` | Less than on string |
| `lt_f64` | `(value: f64) -> QueryBuilder` | Less than on f64 |

#### Range

| Method | Signature | Description |
|--------|-----------|-------------|
| `between` | `(min: &[u8], max: &[u8]) -> QueryBuilder` | Inclusive range on raw bytes |
| `between_u64` | `(min: u64, max: u64) -> QueryBuilder` | Inclusive range on u64 |
| `between_str` | `(min: &str, max: &str) -> QueryBuilder` | Inclusive range on strings |

#### Set Membership

| Method | Signature | Description |
|--------|-----------|-------------|
| `in_values` | `(values: &[&[u8]]) -> QueryBuilder` | Match any of the given byte values |
| `in_u64` | `(values: &[u64]) -> QueryBuilder` | Match any of the given u64 values |
| `in_str` | `(values: &[&str]) -> QueryBuilder` | Match any of the given strings |

#### Text Search

| Method | Signature | Description |
|--------|-----------|-------------|
| `contains` | `(text: &str) -> QueryBuilder` | Substring / trigram contains search |
| `similar` | `(text: &str, threshold: f64) -> QueryBuilder` | Trigram similarity search (0.0--1.0) |
| `phonetic` | `(text: &str) -> QueryBuilder` | Soundex/Metaphone phonetic search |
| `fuzzy` | `(text: &str) -> QueryBuilder` | Levenshtein distance fuzzy search |
| `match_query` | `(text: &str) -> QueryBuilder` | Full-text match query |

### Boolean Combinators

| Method | Signature | Description |
|--------|-----------|-------------|
| `and` | `(build_fn: FnOnce(QueryBuilder) -> QueryBuilder) -> Self` | AND group via closure |
| `or` | `(build_fn: FnOnce(QueryBuilder) -> QueryBuilder) -> Self` | OR group via closure |
| `not` | `(build_fn: FnOnce(QueryBuilder) -> QueryBuilder) -> Self` | Negate a condition via closure |

### Sorting and Pagination

| Method | Signature | Description |
|--------|-----------|-------------|
| `sort` | `(field: impl Into<String>, direction: SortDirection) -> Self` | Add a sort field |
| `limit` | `(count: usize) -> Self` | Limit result count |
| `offset` | `(count: usize) -> Self` | Skip the first N results |

### Execution

| Method | Signature | Description |
|--------|-----------|-------------|
| `execute` | `(self) -> Result<Vec<QueryResult>, PluginError>` | Execute the query via host FFI |
| `to_json` | `(&self) -> serde_json::Value` | Serialize builder state to JSON (for inspection/debugging) |

---

## `QueryResult`

A single query result returned by the host.

| Field | Type | Description |
|-------|------|-------------|
| `path` | `String` | Path of the matching file |
| `score` | `f64` | Relevance score (higher is better, default 0.0) |
| `matched_by` | `Vec<String>` | Names of the indexes/operations that matched (default empty) |

---

## `SortDirection`

Sort direction for query results.

| Variant | Description |
|---------|-------------|
| `Asc` | Ascending order |
| `Desc` | Descending order |

---

## `AggregateBuilder`

Fluent builder for constructing AeorDB aggregation queries. Obtained via `PluginContext::aggregate(path)` or `AggregateBuilder::new(path)`.

### Aggregation Operations

| Method | Signature | Description |
|--------|-----------|-------------|
| `count` | `(self) -> Self` | Request a count aggregation |
| `sum` | `(field: impl Into<String>) -> Self` | Request a sum on a field |
| `avg` | `(field: impl Into<String>) -> Self` | Request an average on a field |
| `min_val` | `(field: impl Into<String>) -> Self` | Request a minimum value on a field |
| `max_val` | `(field: impl Into<String>) -> Self` | Request a maximum value on a field |

### Grouping and Filtering

| Method | Signature | Description |
|--------|-----------|-------------|
| `group_by` | `(field: impl Into<String>) -> Self` | Group results by a field |
| `filter` | `(build_fn: FnOnce(QueryBuilder) -> QueryBuilder) -> Self` | Add a where condition via closure |
| `limit` | `(count: usize) -> Self` | Limit the number of groups returned |

### Execution

| Method | Signature | Description |
|--------|-----------|-------------|
| `execute` | `(self) -> Result<AggregateResult, PluginError>` | Execute the aggregation via host FFI |
| `to_json` | `(&self) -> serde_json::Value` | Serialize builder state to JSON |

---

## `AggregateResult`

Aggregation result returned by the host.

| Field | Type | Description |
|-------|------|-------------|
| `groups` | `Vec<serde_json::Value>` | Per-group aggregation results (default empty) |
| `total_count` | `Option<u64>` | Total count if `count` was requested without `group_by` |

---

## See Also

- [Parser Plugins](parsers.md) -- how to write and deploy parser plugins
- [Query Plugins](query-plugins.md) -- how to write and deploy query plugins
