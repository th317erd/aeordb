# Query Plugins

Query plugins are WASM modules that can read, write, delete, query, and aggregate data inside AeorDB, then return custom HTTP responses. They are the extension mechanism for building custom API endpoints, computed views, data transformations, or any logic that needs to run server-side.

## How It Works

A query plugin receives a `PluginRequest` (containing the HTTP body and metadata) and a `PluginContext` that provides host functions for interacting with the database. The plugin performs whatever logic it needs -- querying data, writing files, aggregating results -- and returns a `PluginResponse` with a status code, body, and content type.

## Writing a Query Plugin: Step by Step

### 1. Create a Rust Crate

```bash
cargo new my-plugin --lib
cd my-plugin
```

Edit `Cargo.toml`:

```toml
[package]
name = "my-plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
aeordb-plugin-sdk = { path = "../aeordb-plugin-sdk" }
serde_json = "1"
```

### 2. Implement the Handler

Use the `aeordb_query_plugin!` macro and write a function that takes `(PluginContext, PluginRequest)` and returns `Result<PluginResponse, PluginError>`.

```rust
use aeordb_plugin_sdk::prelude::*;
use aeordb_plugin_sdk::aeordb_query_plugin;

aeordb_query_plugin!(handle);

fn handle(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError> {
    let results = ctx.query("/users")
        .field("name").contains("Alice")
        .field("age").gt_u64(21)
        .limit(10)
        .execute()?;

    PluginResponse::json(200, &serde_json::json!({
        "users": results,
        "count": results.len()
    })).map_err(|e| PluginError::SerializationFailed(e.to_string()))
}
```

The `aeordb_query_plugin!` macro generates:
- A global allocator for the WASM target
- An `alloc(size) -> ptr` export for host-to-guest memory allocation
- A `handle(ptr, len) -> i64` export that deserializes the request, creates a `PluginContext`, calls your function, and returns the serialized response

### 3. Build and Deploy

```bash
cargo build --target wasm32-unknown-unknown --release

curl -X PUT \
  http://localhost:3000/mydb/myschema/mytable/_deploy \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/wasm" \
  --data-binary @target/wasm32-unknown-unknown/release/my_plugin.wasm
```

### 4. Invoke the Plugin

```bash
curl -X POST \
  http://localhost:3000/mydb/myschema/mytable/_invoke/my-plugin \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"query": "Alice"}'
```

## The `PluginRequest` Struct

| Field | Type | Description |
|-------|------|-------------|
| `arguments` | `Vec<u8>` | Raw argument bytes (the HTTP request body forwarded to the plugin) |
| `metadata` | `HashMap<String, String>` | Key-value metadata about the invocation context |

The `metadata` map typically contains:

| Key | Description |
|-----|-------------|
| `function_name` | The function name from the invoke URL (e.g., `"echo"`, `"read"`) |

You can use `function_name` to multiplex a single plugin into multiple operations:

```rust
fn handle(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError> {
    let function = request.metadata
        .get("function_name")
        .map(|s| s.as_str())
        .unwrap_or("default");

    match function {
        "search" => handle_search(ctx, &request),
        "stats" => handle_stats(ctx, &request),
        _ => Ok(PluginResponse::error(404, &format!("Unknown function: {}", function))),
    }
}
```

## The `PluginContext`

`PluginContext` is your handle for calling AeorDB host functions from inside the WASM sandbox. It is created automatically by the macro and passed to your handler.

### File Operations

```rust
// Read a file -- returns FileData { data, content_type, size }
let file = ctx.read_file("/mydb/users/alice.json")?;

// Write a file (create or overwrite)
ctx.write_file("/mydb/output/result.json", b"{\"ok\":true}", "application/json")?;

// Delete a file
ctx.delete_file("/mydb/temp/scratch.json")?;

// Get file metadata -- returns FileMetadata { path, size, content_type, created_at, updated_at }
let meta = ctx.file_metadata("/mydb/users/alice.json")?;

// List directory entries -- returns Vec<DirEntry { name, entry_type, size }>
let entries = ctx.list_directory("/mydb/users/")?;
```

### Query

Use `ctx.query(path)` to get a `QueryBuilder` with a fluent API:

```rust
let results = ctx.query("/users")
    .field("name").contains("Alice")
    .field("age").gt_u64(21)
    .sort("name", SortDirection::Asc)
    .limit(10)
    .offset(0)
    .execute()?;

// results: Vec<QueryResult { path, score, matched_by }>
```

### Aggregate

Use `ctx.aggregate(path)` to get an `AggregateBuilder`:

```rust
let stats = ctx.aggregate("/orders")
    .count()
    .sum("total")
    .avg("total")
    .min_val("total")
    .max_val("total")
    .group_by("status")
    .limit(100)
    .execute()?;

// stats: AggregateResult { groups, total_count }
```

## The `QueryBuilder`

The `QueryBuilder` provides a fluent API for composing queries. Multiple conditions on the top level are implicitly ANDed.

### Field Operators

Start a field condition with `.field("name")`, then chain an operator:

**Equality:**
- `.eq(value: &[u8])` -- exact match on raw bytes
- `.eq_u64(value)` -- exact match on u64
- `.eq_i64(value)` -- exact match on i64
- `.eq_f64(value)` -- exact match on f64
- `.eq_str(value)` -- exact match on string
- `.eq_bool(value)` -- exact match on boolean

**Comparison:**
- `.gt(value: &[u8])`, `.gt_u64(value)`, `.gt_str(value)`, `.gt_f64(value)` -- greater than
- `.lt(value: &[u8])`, `.lt_u64(value)`, `.lt_str(value)`, `.lt_f64(value)` -- less than

**Range:**
- `.between(min: &[u8], max: &[u8])` -- inclusive range on raw bytes
- `.between_u64(min, max)` -- inclusive range on u64
- `.between_str(min, max)` -- inclusive range on strings

**Set Membership:**
- `.in_values(values: &[&[u8]])` -- match any of the given byte values
- `.in_u64(values: &[u64])` -- match any of the given u64 values
- `.in_str(values: &[&str])` -- match any of the given strings

**Text Search:**
- `.contains(text)` -- substring / trigram contains
- `.similar(text, threshold)` -- trigram similarity (threshold 0.0--1.0)
- `.phonetic(text)` -- Soundex/Metaphone phonetic match
- `.fuzzy(text)` -- Levenshtein distance fuzzy match
- `.match_query(text)` -- full-text match

### Boolean Combinators

```rust
// AND group
ctx.query("/users")
    .and(|q| q.field("name").contains("Alice").field("active").eq_bool(true))
    .limit(10)
    .execute()?;

// OR group
ctx.query("/users")
    .or(|q| q.field("role").eq_str("admin").field("role").eq_str("superadmin"))
    .execute()?;

// NOT
ctx.query("/users")
    .not(|q| q.field("status").eq_str("banned"))
    .execute()?;
```

### Sorting and Pagination

```rust
ctx.query("/users")
    .field("active").eq_bool(true)
    .sort("created_at", SortDirection::Desc)
    .sort("name", SortDirection::Asc)
    .limit(25)
    .offset(50)
    .execute()?;
```

## The `PluginResponse`

Three builder methods for constructing responses:

### `PluginResponse::json(status_code, &body)`

Serializes any `Serialize` type to JSON. Sets `Content-Type: application/json`.

```rust
PluginResponse::json(200, &serde_json::json!({"ok": true}))
    .map_err(|e| PluginError::SerializationFailed(e.to_string()))
```

### `PluginResponse::text(status_code, body)`

Returns a plain text response. Sets `Content-Type: text/plain`.

```rust
Ok(PluginResponse::text(201, "Created by plugin"))
```

### `PluginResponse::error(status_code, message)`

Returns a JSON error response in the form `{"error": "<message>"}`.

```rust
Ok(PluginResponse::error(404, "User not found"))
```

## Real-World Example: Echo Plugin

The built-in echo plugin (`aeordb-plugins/echo-plugin`) demonstrates multiplexing a single plugin across multiple operations:

```rust
use aeordb_plugin_sdk::prelude::*;
use aeordb_plugin_sdk::aeordb_query_plugin;

aeordb_query_plugin!(echo_handle);

fn echo_handle(ctx: PluginContext, request: PluginRequest) -> Result<PluginResponse, PluginError> {
    let function = request.metadata
        .get("function_name")
        .map(|s| s.as_str())
        .unwrap_or("echo");

    match function {
        "echo" => {
            PluginResponse::json(200, &serde_json::json!({
                "echo": true,
                "metadata": request.metadata,
                "body_len": request.arguments.len(),
            }))
            .map_err(|e| PluginError::SerializationFailed(e.to_string()))
        }
        "read" => {
            let path = std::str::from_utf8(&request.arguments)
                .map_err(|e| PluginError::ExecutionFailed(e.to_string()))?;
            match ctx.read_file(path) {
                Ok(file) => PluginResponse::json(200, &serde_json::json!({
                    "size": file.size,
                    "content_type": file.content_type,
                    "data_len": file.data.len(),
                }))
                .map_err(|e| PluginError::SerializationFailed(e.to_string())),
                Err(e) => Ok(PluginResponse::error(404, &e.to_string())),
            }
        }
        "write" => {
            match ctx.write_file("/plugin-output/result.json", b"{\"written\":true}", "application/json") {
                Ok(()) => PluginResponse::json(201, &serde_json::json!({"ok": true}))
                    .map_err(|e| PluginError::SerializationFailed(e.to_string())),
                Err(e) => Ok(PluginResponse::error(500, &e.to_string())),
            }
        }
        "delete" => {
            let path = std::str::from_utf8(&request.arguments)
                .map_err(|e| PluginError::ExecutionFailed(e.to_string()))?;
            match ctx.delete_file(path) {
                Ok(()) => PluginResponse::json(200, &serde_json::json!({"deleted": true}))
                    .map_err(|e| PluginError::SerializationFailed(e.to_string())),
                Err(e) => Ok(PluginResponse::error(500, &e.to_string())),
            }
        }
        "metadata" => {
            let path = std::str::from_utf8(&request.arguments)
                .map_err(|e| PluginError::ExecutionFailed(e.to_string()))?;
            match ctx.file_metadata(path) {
                Ok(meta) => PluginResponse::json(200, &serde_json::json!(meta))
                    .map_err(|e| PluginError::SerializationFailed(e.to_string())),
                Err(e) => Ok(PluginResponse::error(404, &e.to_string())),
            }
        }
        "list" => {
            let path = std::str::from_utf8(&request.arguments)
                .map_err(|e| PluginError::ExecutionFailed(e.to_string()))?;
            match ctx.list_directory(path) {
                Ok(entries) => PluginResponse::json(200, &serde_json::json!({"entries": entries}))
                    .map_err(|e| PluginError::SerializationFailed(e.to_string())),
                Err(e) => Ok(PluginResponse::error(500, &e.to_string())),
            }
        }
        "status" => Ok(PluginResponse::text(201, "Created by plugin")),
        _ => Ok(PluginResponse::error(404, &format!("Unknown function: {}", function))),
    }
}
```

## See Also

- [Parser Plugins](parsers.md) -- plugins that transform non-JSON files into queryable data
- [SDK Reference](sdk-reference.md) -- complete type reference for the plugin SDK
